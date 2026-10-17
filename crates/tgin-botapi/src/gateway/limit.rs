//! One bot's rate limiter: Telegram's flood limits for the bot as a whole,
//! per chat, and per group (a negative `chat_id` or an `@username`). Per
//! chat and per group are keyed GCRA quotas that book their slot ahead; the
//! bot-wide quota is a slot calendar, so a request booked ahead never takes
//! the bot's earlier slots.

use std::time::Duration;

use tgin_core::config::{self, RateMode};
use tgin_runtime::limit::{Admit, Booking, KeyedLimiter, Quota, QuotaError, SlotCalendar};
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
    /// Bot-wide: a calendar, so a request delayed by its chat takes one
    /// later slot and leaves the earlier ones to the bot's other chats.
    global: SlotCalendar,
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
            global: SlotCalendar::new(cfg.global.count, cfg.global.per, cfg.global.burst)?,
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
    /// group's when the chat is a group, each for the instant the request
    /// really leaves. A first pass books every quota from `now`; the
    /// longest delay `D` is when the request leaves, so every quota booked
    /// for an earlier slot is booked again from `now + D` (cancelled, then
    /// booked), until `D` stops growing. The global quota is a calendar: the
    /// request takes the earliest free bot-wide slot at or after `D`, and
    /// the free slots before it stay free for the bot's other chats. All or
    /// none: a quota that refuses undoes every booking made.
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
        let mut start = Duration::ZERO;
        loop {
            for (index, bucket) in buckets.into_iter().enumerate() {
                let Some(bucket) = bucket else {
                    continue;
                };
                if booked[index].is_some_and(|b| b.delay >= start) {
                    continue;
                }
                if let Some(early) = booked[index].take() {
                    self.unbook(early);
                }
                match self.book(bucket, now, start) {
                    Ok(booking) => {
                        admitted.raise(bucket.scope(), booking.delay);
                        booked[index] = Some(booking);
                    }
                    Err(limited) => {
                        self.undo(&booked);
                        return Err(limited);
                    }
                }
            }
            if admitted.delay == start {
                return Ok(admitted);
            }
            start = admitted.delay;
        }
    }

    /// Books one slot of `bucket` at `from` after `now` or later; the
    /// booking's delay counts from `now`.
    fn book<'a>(
        &self,
        bucket: Bucket<'a>,
        now: Instant,
        from: Duration,
    ) -> Result<Booked<'a>, Limited> {
        let refused = |retry_after: Duration| Limited {
            scope: bucket.scope(),
            retry_after,
        };
        let (limiter, key, hold): (_, _, fn(&'a ChatKey, Admit) -> Hold<'a>) = match bucket {
            Bucket::Global => {
                return match self.global.acquire(now, from, self.max_delay) {
                    Ok(booking) => Ok(Booked {
                        delay: booking.delay,
                        hold: Hold::Global(booking),
                    }),
                    Err(denied) => Err(refused(denied.retry_after)),
                };
            }
            Bucket::Chat(key) => (&self.chat, key, Hold::Chat),
            Bucket::Group(key) => (&self.group, key, Hold::Group),
        };
        let at = now
            .checked_add(from)
            .ok_or_else(|| refused(Duration::MAX))?;
        match limiter.acquire(key, at, self.max_delay.saturating_sub(from)) {
            Ok(admit) => Ok(Booked {
                delay: from.saturating_add(admit.delay),
                hold: hold(key, admit),
            }),
            Err(denied) => Err(refused(from.saturating_add(denied.retry_after))),
        }
    }

    /// Cancels `booked`.
    fn unbook(&self, booked: Booked<'_>) {
        match booked.hold {
            Hold::Global(booking) => self.global.cancel(booking),
            Hold::Chat(key, admit) => self.chat.cancel(key, admit),
            Hold::Group(key, admit) => self.group.cancel(key, admit),
        }
    }

    /// Cancels every booking of `booked`, the latest first.
    fn undo(&self, booked: &[Option<Booked<'_>>]) {
        for booked in booked.iter().rev().flatten() {
            self.unbook(*booked);
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
    hold: Hold<'a>,
    /// When the booked slot starts, from the request's `now`.
    delay: Duration,
}

/// What undoes a booking.
#[derive(Clone, Copy)]
enum Hold<'a> {
    Global(Booking),
    Chat(&'a ChatKey, Admit),
    Group(&'a ChatKey, Admit),
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
        // Another chat is not held back by the second one's wait: the
        // calendar's second slot at once is still free.
        let third = limiter.acquire(Some(&ChatKey::Id(8)), now).unwrap();
        assert_eq!(
            third,
            Admitted {
                delay: Duration::ZERO,
                scope: Scope::Global
            }
        );
        // Now two are booked at once: a fourth waits for the window.
        let fourth = limiter.acquire(Some(&ChatKey::Id(9)), now).unwrap();
        assert_eq!(
            fourth,
            Admitted {
                delay: Duration::from_secs(2),
                scope: Scope::Global
            }
        );
    }

    /// The defaults: 30/s burst 30, per chat 1/s burst 3, per group 20/min
    /// burst 20.
    fn defaults() -> BotLimiter {
        limiter(rate(30, 1, 30), rate(1, 1, 3), rate(20, 60, 20))
    }

    /// The longest delay `fresh` requests got while `busy` ones were made
    /// alongside: `schedule` gives each one's offset and chat.
    fn fresh_delay(schedule: &[(u64, ChatKey, bool)]) -> Duration {
        let limiter = defaults();
        let t0 = Instant::now();
        schedule
            .iter()
            .filter_map(|(ms, chat, fresh)| {
                let admitted = limiter.acquire(Some(chat), t0 + Duration::from_millis(*ms));
                fresh.then(|| admitted.expect("a fresh chat is admitted").delay)
            })
            .max()
            .unwrap_or_default()
    }

    /// After 8 sends to one chat, a send to another at the same instant
    /// goes at once (it waited 4.067 s when global was one TAT).
    #[tokio::test(start_paused = true)]
    async fn busy_chat_does_not_delay_another() {
        let mut schedule: Vec<_> = (0..8).map(|_| (0, ChatKey::Id(7), false)).collect();
        schedule.push((0, ChatKey::Id(8), true));
        assert_eq!(fresh_delay(&schedule), Duration::ZERO);
    }

    /// 10/s to one chat plus 10/s to fresh chats: the fresh ones never wait
    /// (max 4.07 s before).
    #[tokio::test(start_paused = true)]
    async fn busy_chat_does_not_delay_fresh_chats() {
        let schedule: Vec<_> = (0..100u64)
            .flat_map(|tick| {
                [
                    (tick * 100, ChatKey::Id(7), false),
                    (
                        tick * 100,
                        ChatKey::Id(10_000 + i64::try_from(tick).unwrap()),
                        true,
                    ),
                ]
            })
            .collect();
        assert_eq!(fresh_delay(&schedule), Duration::ZERO);
    }

    /// 1/s to one busy group plus 5/s to private chats: the private chats
    /// never wait (max 4.07 s before).
    #[tokio::test(start_paused = true)]
    async fn busy_group_does_not_delay_private_chats() {
        let mut schedule = Vec::new();
        for tick in 0..250u64 {
            if tick % 5 == 0 {
                schedule.push((tick * 200, ChatKey::Id(-100), false));
            }
            schedule.push((
                tick * 200,
                ChatKey::Id(10_000 + i64::try_from(tick).unwrap()),
                true,
            ));
        }
        assert_eq!(fresh_delay(&schedule), Duration::ZERO);
    }

    /// A 5-message answer to a new user each second plus 10 single messages
    /// a second: the single ones never wait (mean 2.38 s before).
    #[tokio::test(start_paused = true)]
    async fn answers_to_new_users_do_not_delay_single_messages() {
        let mut schedule = Vec::new();
        for second in 0..20u64 {
            let user = ChatKey::Id(20_000 + i64::try_from(second).unwrap());
            for _ in 0..5 {
                schedule.push((second * 1_000, user.clone(), false));
            }
            for k in 0..10u64 {
                let single = 30_000 + i64::try_from(second * 10 + k).unwrap();
                schedule.push((second * 1_000 + k * 100, ChatKey::Id(single), true));
            }
        }
        assert_eq!(fresh_delay(&schedule), Duration::ZERO);
    }

    proptest::proptest! {
        /// Whatever the schedule, the start times of the admitted requests
        /// keep every quota: at most 5 bot-wide in any 500 ms (10/s burst
        /// 5), and per chat (2/s burst 2) and per group (1/s burst 2) GCRA
        /// conformance, `s_j − s_i ≥ (j − i − (burst − 1)) × T`.
        #[test]
        fn every_quota_holds_on_the_start_times(
            requests in proptest::collection::vec((0u64..150, 0usize..6), 1..200),
        ) {
            let limiter = limiter(rate(10, 1, 5), rate(2, 1, 2), rate(1, 1, 2));
            let chats = [1, 2, 3, -10, -11, -12].map(ChatKey::Id);
            let t0 = Instant::now();
            let mut at = t0;
            let mut starts: Vec<(Instant, usize)> = Vec::new();
            for (gap, chat) in requests {
                at += Duration::from_millis(gap);
                if let Ok(admitted) = limiter.acquire(Some(&chats[chat]), at) {
                    starts.push((at + admitted.delay, chat));
                }
            }
            starts.sort();
            let all: Vec<Instant> = starts.iter().map(|(s, _)| *s).collect();
            for run in all.windows(6) {
                proptest::prop_assert!(run[5] - run[0] >= Duration::from_millis(500), "{all:?}");
            }
            let gcra = |of: &[Instant], interval: Duration, burst: u32| {
                (0..of.len()).all(|i| {
                    (i + 1..of.len()).all(|j| {
                        let n = u32::try_from(j - i).unwrap_or(u32::MAX);
                        n < burst || of[j] - of[i] >= interval * (n - (burst - 1))
                    })
                })
            };
            for (chat, key) in chats.iter().enumerate() {
                let of: Vec<Instant> =
                    starts.iter().filter(|(_, c)| *c == chat).map(|(s, _)| *s).collect();
                proptest::prop_assert!(gcra(&of, Duration::from_millis(500), 2), "chat {chat}: {of:?}");
                if key.is_group() {
                    proptest::prop_assert!(gcra(&of, Duration::from_secs(1), 2), "group {chat}: {of:?}");
                }
            }
        }
    }

    /// Under a global backlog every quota is still charged when the request
    /// leaves: one chat gets at most 3 sends, then 1 per second, as
    /// `per_chat`'s defaults allow, however long global delays them.
    #[tokio::test(start_paused = true)]
    async fn chat_quota_holds_under_global_backlog() {
        // The defaults: 30/s burst 30, per chat 1/s burst 3, per group 20/min burst 20.
        let limiter = limiter(rate(30, 1, 30), rate(1, 1, 3), rate(20, 60, 20));
        let now = Instant::now();
        // 110 other chats: three full bot-wide windows, and 20 in the fourth.
        for chat in 0..110 {
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
        assert!(limiter.global.acquire(now, at_once, at_once).is_ok());
        assert!(limiter.global.acquire(now, at_once, at_once).is_err());
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
