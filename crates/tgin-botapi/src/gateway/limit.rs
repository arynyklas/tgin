//! One bot's rate limiter: Telegram's flood limits for the bot as a whole,
//! per chat, and per group (a negative `chat_id` or an `@username`), each a
//! keyed GCRA quota that books its slot ahead.

use std::time::Duration;

use tgin_core::config::{self, RateMode};
use tgin_runtime::limit::{Admit, Denied, KeyedLimiter, Quota, QuotaError};
use tokio::time::Instant;

use super::Gateway;
use super::chat_key::ChatKey;
use super::methods::MethodSet;
pub use super::metrics::RateScope as Scope;

/// The longest `retry_after` tgin answers, in seconds. A denial can be
/// `Duration::MAX` away; a client is better served by an hour.
pub const MAX_RETRY_AFTER_SECS: u64 = 3_600;

/// A request the limits refuse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limited {
    /// The quota that refused it.
    pub scope: Scope,
    /// How long until that quota has a slot: `Duration::MAX` when its next
    /// slot lies past any representable instant.
    pub retry_after: Duration,
}

impl Limited {
    fn by(scope: Scope, denied: Denied) -> Limited {
        Limited {
            scope,
            retry_after: denied.retry_after,
        }
    }

    /// `retry_after` as a 429 carries it: see [`retry_after_secs`].
    pub fn retry_after_secs(&self) -> u64 {
        retry_after_secs(self.retry_after)
    }
}

/// `wait` in whole seconds, rounded up without overflowing, and at most
/// [`MAX_RETRY_AFTER_SECS`].
pub fn retry_after_secs(wait: Duration) -> u64 {
    wait.as_secs()
        .saturating_add(u64::from(wait.subsec_nanos() > 0))
        .min(MAX_RETRY_AFTER_SECS)
}

/// A request the limits admit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Admitted {
    /// How long the request waits before it goes: the longest of the
    /// delays its bookings got, each at most `max_delay`.
    pub delay: Duration,
    /// The quota whose booking set `delay`; [`Scope::Global`] when nothing
    /// waits.
    pub scope: Scope,
}

/// One bot's limits: which methods they cover, how a request beyond them
/// is treated, and the three quotas.
pub struct BotLimiter {
    methods: MethodSet,
    mode: RateMode,
    /// Zero in `reject` mode.
    max_delay: Duration,
    global: KeyedLimiter<()>,
    chat: KeyedLimiter<ChatKey>,
    group: KeyedLimiter<ChatKey>,
}

impl BotLimiter {
    /// The limiter `cfg` describes.
    ///
    /// # Errors
    ///
    /// A rate with a zero period (validation rejects it).
    pub fn new(cfg: &config::RateLimit) -> Result<BotLimiter, QuotaError> {
        let quota = |rate: &config::Rate| Quota::new(rate.count, rate.per, rate.burst);
        Ok(BotLimiter {
            methods: MethodSet::new(&cfg.methods),
            mode: cfg.mode,
            max_delay: match cfg.mode {
                RateMode::Shape => cfg.max_delay,
                RateMode::Reject | RateMode::Off => Duration::ZERO,
            },
            global: KeyedLimiter::new(quota(&cfg.global)?),
            chat: KeyedLimiter::new(quota(&cfg.per_chat)?),
            group: KeyedLimiter::new(quota(&cfg.per_group)?),
        })
    }

    /// Whether `method` is one of `rate_limit.methods` (ignoring ASCII
    /// case), whatever the mode: the send-class methods whose small bodies
    /// the gateway buffers.
    pub fn covers(&self, method: &str) -> bool {
        self.methods.matches(method)
    }

    /// Whether the limits apply to `method`: it is covered and the mode is
    /// not `off`.
    pub fn applies(&self, method: &str) -> bool {
        self.mode != RateMode::Off && self.covers(method)
    }

    /// Books one request: on the global quota, on `chat`'s, and on its
    /// group's when the chat is a group, each at the instant the request
    /// really leaves. A first pass books every quota at `now`; the longest
    /// delay `D` is when the request leaves, so every quota booked for an
    /// earlier slot is booked again at `now + D` (cancelled, then booked
    /// there), until `D` stops growing. A quota charged for a slot before
    /// the request leaves would let its key's later requests go too soon.
    /// All or none: a quota that refuses undoes every booking made.
    ///
    /// # Errors
    ///
    /// A quota cannot start the request within `max_delay`.
    pub fn acquire(&self, chat: Option<&ChatKey>, now: Instant) -> Result<Admitted, Limited> {
        let buckets = [
            Some(Bucket::Global),
            chat.map(Bucket::Chat),
            chat.filter(|chat| chat.is_group()).map(Bucket::Group),
        ];
        let mut booked: [Option<Booked<'_>>; 3] = [None; 3];
        let mut admitted = Admitted {
            delay: Duration::ZERO,
            scope: Scope::Global,
        };
        for (index, bucket) in buckets.into_iter().enumerate() {
            let Some(bucket) = bucket else {
                continue;
            };
            match self.book(bucket, now, self.max_delay) {
                Ok(admit) => {
                    admitted.raise(bucket.scope(), admit.delay);
                    booked[index] = Some(Booked {
                        bucket,
                        admit,
                        delay: admit.delay,
                    });
                }
                Err(denied) => {
                    self.undo(&booked);
                    return Err(Limited::by(bucket.scope(), denied));
                }
            }
        }
        loop {
            let start = admitted.delay;
            // `start` is at most `max_delay`, which validation bounds.
            let Some(at) = now.checked_add(start) else {
                return Ok(admitted);
            };
            for index in 0..booked.len() {
                let Some(early) = booked[index].filter(|b| b.delay < start) else {
                    continue;
                };
                self.unbook(early.bucket, early.admit);
                booked[index] = None;
                match self.book(early.bucket, at, self.max_delay.saturating_sub(start)) {
                    Ok(admit) => {
                        let delay = start.saturating_add(admit.delay);
                        admitted.raise(early.bucket.scope(), delay);
                        booked[index] = Some(Booked {
                            bucket: early.bucket,
                            admit,
                            delay,
                        });
                    }
                    Err(denied) => {
                        self.undo(&booked);
                        return Err(Limited {
                            scope: early.bucket.scope(),
                            retry_after: start.saturating_add(denied.retry_after),
                        });
                    }
                }
            }
            if admitted.delay == start {
                return Ok(admitted);
            }
        }
    }

    /// Books one slot of `bucket` at `at`.
    fn book(&self, bucket: Bucket<'_>, at: Instant, max_delay: Duration) -> Result<Admit, Denied> {
        match bucket {
            Bucket::Global => self.global.acquire(&(), at, max_delay),
            Bucket::Chat(chat) => self.chat.acquire(chat, at, max_delay),
            Bucket::Group(chat) => self.group.acquire(chat, at, max_delay),
        }
    }

    /// Cancels `admit`, a booking of `bucket`.
    fn unbook(&self, bucket: Bucket<'_>, admit: Admit) {
        match bucket {
            Bucket::Global => self.global.cancel(&(), admit),
            Bucket::Chat(chat) => self.chat.cancel(chat, admit),
            Bucket::Group(chat) => self.group.cancel(chat, admit),
        }
    }

    /// Cancels every booking of `booked`, the latest first.
    fn undo(&self, booked: &[Option<Booked<'_>>]) {
        for booked in booked.iter().rev().flatten() {
            self.unbook(booked.bucket, booked.admit);
        }
    }

    /// Forgets the keys whose quota is back to a fresh key's at `now`.
    pub fn sweep(&self, now: Instant) {
        self.global.retain_recent(now);
        self.chat.retain_recent(now);
        self.group.retain_recent(now);
    }
}

impl Admitted {
    /// Takes `delay` of `scope` when it is longer than the current one.
    fn raise(&mut self, scope: Scope, delay: Duration) {
        if delay > self.delay {
            self.delay = delay;
            self.scope = scope;
        }
    }
}

/// One quota a request books, with its key.
#[derive(Clone, Copy)]
enum Bucket<'a> {
    Global,
    Chat(&'a ChatKey),
    Group(&'a ChatKey),
}

impl Bucket<'_> {
    fn scope(self) -> Scope {
        match self {
            Bucket::Global => Scope::Global,
            Bucket::Chat(_) => Scope::Chat,
            Bucket::Group(_) => Scope::Group,
        }
    }
}

/// A request's booking on one quota.
#[derive(Clone, Copy)]
struct Booked<'a> {
    bucket: Bucket<'a>,
    admit: Admit,
    /// When the booked slot starts, from the request's `now`.
    delay: Duration,
}

impl Gateway {
    /// Sweeps every bot's limiter (see [`BotLimiter::sweep`]).
    pub fn sweep_limiters(&self, now: Instant) {
        for bot in &self.table.load().bots {
            bot.limiter.sweep(now);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::*;

    fn rate(count: u32, per: u64, burst: u32) -> config::Rate {
        config::Rate {
            count: NonZeroU32::new(count).unwrap(),
            per: Duration::from_secs(per),
            burst: NonZeroU32::new(burst).unwrap(),
        }
    }

    /// Shape mode, `send*`, with the given quotas.
    fn limiter(global: config::Rate, chat: config::Rate, group: config::Rate) -> BotLimiter {
        BotLimiter::new(&config::RateLimit {
            mode: RateMode::Shape,
            max_delay: Duration::from_secs(5),
            methods: vec!["send*".to_owned()],
            global,
            per_chat: chat,
            per_group: group,
        })
        .unwrap()
    }

    fn roomy() -> config::Rate {
        rate(1_000, 1, 1_000)
    }

    #[test]
    fn applies_by_method_and_mode() {
        let mut cfg = config::RateLimit {
            mode: RateMode::Reject,
            max_delay: Duration::from_secs(5),
            methods: vec!["send*".to_owned(), "copymessage".to_owned()],
            global: roomy(),
            per_chat: roomy(),
            per_group: roomy(),
        };
        let reject = BotLimiter::new(&cfg).unwrap();
        assert!(reject.applies("sendMessage"));
        assert!(reject.applies("SENDPHOTO"));
        assert!(reject.applies("copyMessage"));
        assert!(!reject.applies("copyMessages"));
        assert!(!reject.applies("getChat"));
        assert_eq!(reject.max_delay, Duration::ZERO);
        cfg.mode = RateMode::Off;
        let off = BotLimiter::new(&cfg).unwrap();
        assert!(!off.applies("sendMessage"));
        assert!(
            off.covers("sendMessage"),
            "off mode still covers its methods"
        );
        assert!(!off.covers("getChat"));
    }

    #[tokio::test(start_paused = true)]
    async fn largest_delay_wins_with_its_scope() {
        // Global 1/s after 2, chat 1 per 2 s after 1.
        let limiter = limiter(rate(1, 1, 2), rate(1, 2, 1), roomy());
        let now = Instant::now();
        let chat = ChatKey::Id(7);
        let first = limiter.acquire(Some(&chat), now).unwrap();
        assert_eq!(first.delay, Duration::ZERO);
        assert_eq!(first.scope, Scope::Global);
        // Global would start it at once; the chat in 2 s.
        let second = limiter.acquire(Some(&chat), now).unwrap();
        assert_eq!(
            second,
            Admitted {
                delay: Duration::from_secs(2),
                scope: Scope::Chat
            }
        );
        // Another chat: global booked the second request at 2 s, when it
        // leaves, so global's burst has room again only there.
        let third = limiter.acquire(Some(&ChatKey::Id(8)), now).unwrap();
        assert_eq!(
            third,
            Admitted {
                delay: Duration::from_secs(2),
                scope: Scope::Global
            }
        );
    }

    /// Under a global backlog every quota is still charged when the request
    /// leaves: one chat gets at most 3 sends, then 1 per second, as
    /// `per_chat`'s defaults allow, however long global delays them.
    #[tokio::test(start_paused = true)]
    async fn chat_quota_holds_under_global_backlog() {
        // The defaults: 30/s burst 30, per chat 1/s burst 3, per group 20/min burst 20.
        let limiter = limiter(rate(30, 1, 30), rate(1, 1, 3), rate(20, 60, 20));
        let now = Instant::now();
        for chat in 0..146 {
            limiter
                .acquire(Some(&ChatKey::Id(1_000 + chat)), now)
                .expect("within max_delay");
        }
        let starts: Vec<Duration> = (0..8)
            .filter_map(|_| limiter.acquire(Some(&ChatKey::Id(7)), now).ok())
            .map(|admitted| admitted.delay)
            .collect();
        assert!(starts.len() >= 4, "{starts:?}");
        // GCRA with burst 3 and 1 s: sends i < j are at least j - i - 2 s apart.
        for (i, a) in starts.iter().enumerate() {
            for (j, b) in starts.iter().enumerate().skip(i + 1) {
                let gap = Duration::from_secs(u64::try_from(j - i).unwrap().saturating_sub(2));
                assert!(
                    b.saturating_sub(*a) >= gap,
                    "sends {i} and {j} at {a:?} and {b:?}: {starts:?}"
                );
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn refusal_undoes_earlier_bookings() {
        // Group: one per hour; global and chat roomy but countable.
        let limiter = limiter(rate(1, 60, 2), rate(1, 60, 2), rate(1, 3_600, 1));
        let now = Instant::now();
        let group = ChatKey::Username("chan".into());
        assert!(limiter.acquire(Some(&group), now).is_ok());
        let refused = limiter.acquire(Some(&group), now).unwrap_err();
        assert_eq!(refused.scope, Scope::Group);
        assert_eq!(refused.retry_after, Duration::from_secs(3_600));
        // The refused request's global and chat bookings were undone: each
        // quota has exactly one slot left at `now`.
        let at_once = Duration::ZERO;
        assert!(limiter.chat.acquire(&group, now, at_once).is_ok());
        assert!(limiter.chat.acquire(&group, now, at_once).is_err());
        assert!(limiter.global.acquire(&(), now, at_once).is_ok());
        assert!(limiter.global.acquire(&(), now, at_once).is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn chat_refusal_undoes_global_booking() {
        let limiter = limiter(rate(1, 60, 2), rate(1, 60, 1), roomy());
        let now = Instant::now();
        let chat = ChatKey::Id(5);
        assert!(limiter.acquire(Some(&chat), now).is_ok());
        let refused = limiter.acquire(Some(&chat), now).unwrap_err();
        assert_eq!(refused.scope, Scope::Chat);
        // Global had two slots; the refused request gave its own back.
        assert!(limiter.acquire(Some(&ChatKey::Id(6)), now).is_ok());
        assert_eq!(limiter.acquire(None, now).unwrap_err().scope, Scope::Global);
    }

    #[tokio::test(start_paused = true)]
    async fn group_quota_only_for_groups() {
        let limiter = limiter(roomy(), roomy(), rate(1, 60, 1));
        let now = Instant::now();
        for _ in 0..3 {
            assert!(limiter.acquire(Some(&ChatKey::Id(9)), now).is_ok());
        }
        assert!(limiter.acquire(Some(&ChatKey::Id(-9)), now).is_ok());
        assert_eq!(
            limiter
                .acquire(Some(&ChatKey::Id(-9)), now)
                .unwrap_err()
                .scope,
            Scope::Group
        );
    }

    #[tokio::test(start_paused = true)]
    async fn sweep_forgets_only_fresh_keys() {
        let limiter = limiter(rate(1, 1, 1), rate(1, 10, 1), rate(1, 10, 1));
        let now = Instant::now();
        limiter.acquire(Some(&ChatKey::Id(-1)), now).unwrap();
        limiter.sweep(now + Duration::from_secs(5));
        assert!(limiter.global.is_empty());
        assert_eq!(limiter.chat.len(), 1);
        assert_eq!(limiter.group.len(), 1);
        limiter.sweep(now + Duration::from_secs(10));
        assert!(limiter.chat.is_empty() && limiter.group.is_empty());
    }

    #[test]
    fn retry_after_rounds_up_saturates_and_caps() {
        assert_eq!(retry_after_secs(Duration::from_millis(1)), 1);
        assert_eq!(retry_after_secs(Duration::from_secs(1)), 1);
        assert_eq!(retry_after_secs(Duration::from_millis(1_001)), 2);
        assert_eq!(retry_after_secs(Duration::from_secs(3_600)), 3_600);
        assert_eq!(retry_after_secs(Duration::from_secs(3_601)), 3_600);
        assert_eq!(retry_after_secs(Duration::MAX), MAX_RETRY_AFTER_SECS);
        let limited = Limited {
            scope: Scope::Chat,
            retry_after: Duration::MAX,
        };
        assert_eq!(limited.retry_after_secs(), 3_600);
    }
}
