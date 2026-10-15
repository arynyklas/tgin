//! A consumer that posts each update to HTTP endpoints, with retries,
//! balancing and per-endpoint circuit breakers.
//!
//! One dispatcher task leases updates as the single holder `push` and
//! delivers up to `max_in_flight` of them at once. A delivery keeps its lease
//! through every retry, so under per-chat ordering a chat's next update waits
//! until the current one is delivered or dead-lettered. Push leases never
//! expire: the dispatcher acks every lease it settles and releases the rest
//! when it stops, so a new dispatcher resumes from the queue alone.

use std::fmt;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use arc_swap::ArcSwap;
use futures_util::stream::{FuturesUnordered, StreamExt};
use reqwest::StatusCode;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, RETRY_AFTER};
use smallvec::SmallVec;
use tgin_runtime::backoff::{Backoff, honor_retry_after};
use tgin_runtime::breaker::{Breaker, BreakerState};
use tgin_runtime::http::describe_error;
use tgin_runtime::queue::{HolderId, LeaseQueue, QueueConfig};
use tgin_runtime::secret::Secret;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, sleep_until};
use tokio_util::sync::CancellationToken;

pub use crate::config::{Balance, Retry as RetryPolicy};
use crate::envelope::Envelope;
use crate::metrics::{ConsumerDropReason, ConsumerMetrics, DeliveryOutcome, EndpointMetrics};

/// Lease timeout of a push queue (see [`crate::config::Push::queue`]): long
/// enough never to pass, since the push queue is never expired.
pub const LEASE_TIMEOUT: Duration = Duration::from_secs(365 * 24 * 60 * 60);

/// The one holder the dispatcher leases as.
const HOLDER: &str = "push";

const SECRET_HEADER: &str = "x-telegram-bot-api-secret-token";
const SOURCE_HEADER: &str = "x-tgin-source";
const UPDATE_TYPE_HEADER: &str = "x-tgin-update-type";
const ATTEMPT_HEADER: &str = "x-tgin-attempt";

/// One downstream URL. Its breaker is per endpoint and lives as long as the
/// endpoint set it was given with.
pub struct Endpoint {
    /// May carry a secret: it is never logged or formatted.
    pub url: reqwest::Url,
    pub secret_token: Option<Secret<String>>,
    /// Bound on one request, from connecting to the end of the response.
    pub timeout: Duration,
    pub breaker: Breaker,
}

#[derive(Clone, Debug)]
pub struct PushOptions {
    /// Most deliveries at once, counting those waiting to retry.
    pub max_in_flight: usize,
    pub balance: Balance,
    pub retry: RetryPolicy,
    /// Source names by [`crate::ids::SourceId`], for `X-Tgin-Source` and logs.
    pub source_names: Arc<[Arc<str>]>,
}

pub struct PushConsumer {
    name: Arc<str>,
    holder: HolderId,
    queue: LeaseQueue<Envelope>,
    targets: ArcSwap<Targets>,
    client: reqwest::Client,
    metrics: ConsumerMetrics,
    /// Round-robin position, shared by every endpoint set.
    cursor: AtomicUsize,
    /// Deliveries the dispatcher holds, sending or waiting to retry.
    in_flight: AtomicUsize,
    draining: AtomicBool,
    /// Wakes deliveries waiting for a breaker after an attempt's outcome, and
    /// the dispatcher after [`PushConsumer::set_endpoints`].
    changed: Notify,
}

impl PushConsumer {
    /// `cfg` should come from [`crate::config::Push::queue`]: the push queue
    /// is never expired, so its lease timeout only has to be long. `client`
    /// must not follow redirects (as [`tgin_runtime::http::client`] by
    /// default): any 3xx dead-letters the update.
    pub fn new(
        name: Arc<str>,
        cfg: QueueConfig,
        endpoints: Vec<Endpoint>,
        opts: PushOptions,
        client: reqwest::Client,
        metrics: ConsumerMetrics,
    ) -> Self {
        let targets = Targets::new(&name, endpoints, opts, &metrics);
        Self {
            name,
            holder: HolderId(Arc::from(HOLDER)),
            queue: LeaseQueue::new(cfg),
            targets: ArcSwap::from_pointee(targets),
            client,
            metrics,
            cursor: AtomicUsize::new(0),
            in_flight: AtomicUsize::new(0),
            draining: AtomicBool::new(false),
            changed: Notify::new(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn queue(&self) -> &LeaseQueue<Envelope> {
        &self.queue
    }

    /// Runs the dispatcher until `cancel` or `force`. After `cancel` it
    /// leases nothing new and starts no new attempt: deliveries waiting to
    /// retry or for a breaker return their leases to the queue at once, and
    /// requests already sent finish (bounded by their request timeouts),
    /// then the task returns. `force` aborts those requests too and returns
    /// their leases, as does dropping or aborting the task, so a dispatcher
    /// spawned later retries every update not delivered or dead-lettered.
    /// Run at most one dispatcher per consumer at a time.
    pub fn spawn(
        self: &Arc<Self>,
        cancel: CancellationToken,
        force: CancellationToken,
    ) -> JoinHandle<()> {
        let this = Arc::clone(self);
        tokio::spawn(async move { this.dispatch(cancel, force).await })
    }

    /// Replaces the endpoints and options. Deliveries in flight finish on the
    /// endpoint they were sent to; every later choice, retries included, uses
    /// the new set. Breaker state comes with the new endpoints: an endpoint
    /// given again starts from its new breaker.
    pub fn set_endpoints(&self, endpoints: Vec<Endpoint>, opts: PushOptions) {
        let targets = Targets::new(&self.name, endpoints, opts, &self.metrics);
        self.targets.store(Arc::new(targets));
        self.changed.notify_waiters();
    }

    /// Deliveries in flight, counting those waiting to retry.
    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::SeqCst)
    }

    /// Updates not yet delivered or dead-lettered: queued and in flight.
    pub fn pending(&self) -> usize {
        let stats = self.queue.stats();
        stats.available + stats.leased
    }

    /// Marks the consumer as draining. Dispatching goes on unchanged until
    /// the queue is empty or the dispatcher is cancelled.
    pub fn begin_drain(&self) {
        self.draining.store(true, Ordering::SeqCst);
    }

    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::SeqCst)
    }

    async fn dispatch(&self, cancel: CancellationToken, force: CancellationToken) {
        let mut deliveries = FuturesUnordered::new();
        loop {
            // Enabled before looking at the queue, so no wake-up is lost.
            let mut queued = pin!(self.queue.notified());
            queued.as_mut().enable();
            let mut changed = pin!(self.changed.notified());
            changed.as_mut().enable();

            let stopping = cancel.is_cancelled();
            if stopping {
                if deliveries.is_empty() {
                    break;
                }
            } else {
                let max = self.targets.load().opts.max_in_flight;
                let room = max.saturating_sub(deliveries.len());
                if room > 0 {
                    let leases = self.queue.lease(&self.holder, room, Instant::now());
                    if !leases.is_empty() {
                        for lease in leases {
                            let held = Held::new(self, lease.seq);
                            deliveries.push(self.deliver(held, lease.item, &cancel));
                        }
                        self.update_depth();
                    }
                }
            }
            tokio::select! {
                biased;
                () = force.cancelled() => break,
                Some(()) = deliveries.next(), if !deliveries.is_empty() => {}
                () = &mut queued, if !stopping => {}
                () = &mut changed, if !stopping => {}
                () = cancel.cancelled(), if !stopping => {}
            }
        }
        // Dropping the deliveries aborts their requests and backoffs and
        // returns their leases to the queue.
        drop(deliveries);
    }

    /// Delivers one update, retrying on the endpoints of the current set,
    /// until it is delivered or dead-lettered. Once `cancel` fires it starts
    /// no new attempt and returns, leaving the update to the queue. A request
    /// already sent when `cancel` fires still settles the update: if it fails
    /// transiently and the update has spent `max_attempts` or `max_age`, the
    /// update is dead-lettered, not released.
    async fn deliver(&self, held: Held<'_>, envelope: Envelope, cancel: &CancellationToken) {
        let mut backoff: Option<Backoff> = None;
        let mut attempt: u32 = 0;
        let mut last: Option<(usize, Cause)> = None;
        loop {
            let mut sent = match self.choose(&envelope, cancel).await {
                Choice::Send(sent) => sent,
                // Dropping `held` returns the update to its position.
                Choice::Cancelled => return,
                Choice::Expired => {
                    // `max_age` passed before an endpoint could take it.
                    let (endpoint, cause) = match last {
                        Some((endpoint, cause)) => (Some(endpoint), cause),
                        None => (None, Cause::Expired),
                    };
                    self.dead_letter(held, &envelope, endpoint, &cause, "max_age");
                    return;
                }
            };
            attempt = attempt.saturating_add(1);
            let result = self.send(&sent, &envelope, attempt).await;
            let now = Instant::now();
            match result {
                Ok(()) => {
                    sent.report(true, now);
                    held.ack();
                    sent.slot().metrics.attempt(DeliveryOutcome::Delivered);
                    let latency = now.saturating_duration_since(envelope.received_at);
                    self.metrics.latency(latency.as_secs_f64());
                    return;
                }
                Err(Failure::Permanent(cause)) => {
                    // The endpoint answered: it is up, only the update fails.
                    sent.report(true, now);
                    sent.slot().metrics.attempt(DeliveryOutcome::DeadLetter);
                    let reason = cause.permanent_reason();
                    self.dead_letter(held, &envelope, Some(sent.index), &cause, reason);
                    return;
                }
                Err(Failure::Transient { cause, retry_after }) => {
                    sent.report(false, now);
                    let slot = sent.slot();
                    let retry = sent.targets.opts.retry;
                    let backoff = backoff.get_or_insert_with(|| {
                        Backoff::new(retry.initial, retry.max, Duration::ZERO)
                    });
                    let computed = backoff.next_delay(&mut rand::rng());
                    let delay = honor_retry_after(computed, retry_after, retry.max);
                    let too_old = match (
                        envelope.received_at.checked_add(retry.max_age),
                        now.checked_add(delay),
                    ) {
                        (Some(deadline), Some(retry_at)) => retry_at >= deadline,
                        (None, _) => false,
                        (Some(_), None) => true,
                    };
                    let exhausted = attempt >= retry.max_attempts;
                    if exhausted || too_old {
                        slot.metrics.attempt(DeliveryOutcome::DeadLetter);
                        let reason = if exhausted { "max_attempts" } else { "max_age" };
                        self.dead_letter(held, &envelope, Some(sent.index), &cause, reason);
                        return;
                    }
                    slot.metrics.attempt(DeliveryOutcome::Retry);
                    last = Some((sent.index, cause));
                    drop(sent);
                    tokio::select! {
                        biased;
                        // Dropping `held` returns the update to its position.
                        () = cancel.cancelled() => return,
                        () = sleep(delay) => {}
                    }
                }
            }
        }
    }

    /// Picks an endpoint of the current set whose breaker admits a request,
    /// waiting while every breaker is open, until the update is older than
    /// `max_age` or `cancel` fires.
    async fn choose(&self, envelope: &Envelope, cancel: &CancellationToken) -> Choice<'_> {
        loop {
            if cancel.is_cancelled() {
                return Choice::Cancelled;
            }
            let mut changed = pin!(self.changed.notified());
            changed.as_mut().enable();
            let targets = self.targets.load_full();
            let now = Instant::now();
            let deadline = envelope.received_at.checked_add(targets.opts.retry.max_age);
            if deadline.is_some_and(|deadline| now >= deadline) {
                return Choice::Expired;
            }
            let wait = match self.pick(&targets, envelope.key, now) {
                Ok(index) => return Choice::Send(Sending::new(targets, index, &self.changed)),
                Err(wait) => wait,
            };
            // A half-open breaker waits for its probe's outcome, which
            // notifies `changed`; so does a new endpoint set.
            let reopen = wait.and_then(|wait| now.checked_add(wait));
            let until = match (reopen, deadline) {
                (Some(reopen), Some(deadline)) => Some(reopen.min(deadline)),
                (reopen, deadline) => reopen.or(deadline),
            };
            let sleep = async {
                match until {
                    Some(until) => sleep_until(until).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                () = cancel.cancelled() => return Choice::Cancelled,
                () = &mut changed => {}
                () = sleep => {}
            }
        }
    }

    /// The endpoint to send to, its in-flight count already taken, or how
    /// long until an open breaker may admit a probe (`None`: no breaker
    /// reopens by itself).
    fn pick(
        &self,
        targets: &Targets,
        key: Option<u64>,
        now: Instant,
    ) -> Result<usize, Option<Duration>> {
        let now = now.into_std();
        loop {
            let mut healthy: SmallVec<[usize; 8]> = SmallVec::new();
            let mut wait: Option<Duration> = None;
            for (index, slot) in targets.slots.iter().enumerate() {
                match slot.breaker.ready_in(now) {
                    Some(Duration::ZERO) => healthy.push(index),
                    Some(left) => wait = Some(wait.map_or(left, |w| w.min(left))),
                    None => {}
                }
            }
            if healthy.is_empty() {
                return Err(wait);
            }
            let index = match (targets.opts.balance, key) {
                (Balance::HashChat, Some(key)) => {
                    let n = u64::try_from(healthy.len()).unwrap_or(u64::MAX);
                    healthy[usize::try_from(key % n).unwrap_or(0)]
                }
                (Balance::LeastInFlight, _) => healthy
                    .iter()
                    .copied()
                    .min_by_key(|&i| targets.slots[i].in_flight.load(Ordering::SeqCst))
                    .unwrap_or(healthy[0]),
                (Balance::RoundRobin | Balance::HashChat, _) => {
                    healthy[self.cursor.fetch_add(1, Ordering::Relaxed) % healthy.len()]
                }
            };
            let slot = &targets.slots[index];
            // Another delivery may have taken the one probe of a reopening
            // breaker since `ready_in`; then look again.
            if slot.breaker.allow(now) {
                slot.in_flight.fetch_add(1, Ordering::SeqCst);
                return Ok(index);
            }
        }
    }

    async fn send(
        &self,
        sent: &Sending<'_>,
        envelope: &Envelope,
        attempt: u32,
    ) -> Result<(), Failure> {
        let slot = sent.slot();
        let mut headers = HeaderMap::with_capacity(5);
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            HeaderName::from_static(ATTEMPT_HEADER),
            HeaderValue::from(attempt),
        );
        match &slot.secret {
            SecretHeader::None => {}
            SecretHeader::Valid(value) => {
                headers.insert(HeaderName::from_static(SECRET_HEADER), value.clone());
            }
            SecretHeader::Invalid => return Err(Failure::Permanent(Cause::InvalidSecret)),
        }
        if let Some(Some(source)) = sent
            .targets
            .source_headers
            .get(usize::from(envelope.source.0))
        {
            headers.insert(HeaderName::from_static(SOURCE_HEADER), source.clone());
        }
        if let Ok(update_type) = HeaderValue::from_maybe_shared(envelope.update_type.clone()) {
            headers.insert(HeaderName::from_static(UPDATE_TYPE_HEADER), update_type);
        }
        let response = self
            .client
            .post(slot.url.clone())
            .timeout(slot.timeout)
            .headers(headers)
            .body(envelope.payload.clone())
            .send()
            .await;
        match response {
            Ok(response) => {
                let status = response.status();
                if status.is_success() {
                    Ok(())
                } else if status == StatusCode::REQUEST_TIMEOUT
                    || status == StatusCode::TOO_MANY_REQUESTS
                    || status.is_server_error()
                {
                    Err(Failure::Transient {
                        cause: Cause::Status(status.as_u16()),
                        retry_after: retry_after(response.headers()),
                    })
                } else {
                    // Other 4xx, and any 3xx the client did not follow.
                    Err(Failure::Permanent(Cause::Status(status.as_u16())))
                }
            }
            // The request could not even be built: sending it again is
            // no different.
            Err(err) if err.is_builder() => {
                Err(Failure::Permanent(Cause::Error(describe_error(&err))))
            }
            Err(err) => Err(Failure::Transient {
                cause: Cause::Error(describe_error(&err)),
                retry_after: None,
            }),
        }
    }

    /// Drops the update for good: acks it, counts it and logs it, without its
    /// payload or the endpoint's URL.
    fn dead_letter(
        &self,
        held: Held<'_>,
        envelope: &Envelope,
        endpoint: Option<usize>,
        cause: &Cause,
        reason: &'static str,
    ) {
        held.ack();
        self.metrics.dropped(ConsumerDropReason::DeadLetter, 1);
        let targets = self.targets.load();
        let source = targets
            .opts
            .source_names
            .get(usize::from(envelope.source.0))
            .map_or("?", |name| &**name);
        tracing::warn!(
            consumer = %self.name,
            source = %source,
            update_id = envelope.update_id,
            update_type = %std::str::from_utf8(&envelope.update_type).unwrap_or("?"),
            endpoint = endpoint.and_then(|e| u64::try_from(e).ok()),
            reason = %reason,
            last = %cause,
            "update dead-lettered"
        );
    }

    fn update_depth(&self) {
        let stats = self.queue.stats();
        let gauge = |n: usize| i64::try_from(n).unwrap_or(i64::MAX);
        self.metrics.set_depth(
            gauge(stats.available),
            gauge(stats.leased),
            gauge(self.in_flight()),
        );
    }
}

/// One endpoint set with the options it was given with.
struct Targets {
    slots: Box<[Slot]>,
    opts: PushOptions,
    /// `X-Tgin-Source` values by source; `None` for a name that is no valid
    /// header value.
    source_headers: Box<[Option<HeaderValue>]>,
}

impl Targets {
    fn new(
        consumer: &str,
        endpoints: Vec<Endpoint>,
        opts: PushOptions,
        metrics: &ConsumerMetrics,
    ) -> Self {
        let slots = endpoints
            .into_iter()
            .enumerate()
            .map(|(index, endpoint)| {
                let secret = match &endpoint.secret_token {
                    None => SecretHeader::None,
                    Some(secret) => match HeaderValue::from_str(secret.expose()) {
                        Ok(mut value) => {
                            value.set_sensitive(true);
                            SecretHeader::Valid(value)
                        }
                        Err(_) => {
                            tracing::warn!(
                                consumer = %consumer,
                                endpoint = u64::try_from(index).ok(),
                                "secret_token is not a valid header value; \
                                 updates for this endpoint are dead-lettered"
                            );
                            SecretHeader::Invalid
                        }
                    },
                };
                Slot {
                    url: endpoint.url,
                    secret,
                    timeout: endpoint.timeout,
                    breaker: endpoint.breaker,
                    in_flight: AtomicUsize::new(0),
                    metrics: metrics.endpoint(index),
                }
            })
            .collect();
        let source_headers = opts
            .source_names
            .iter()
            .map(|name| HeaderValue::from_str(name).ok())
            .collect();
        Self {
            slots,
            opts,
            source_headers,
        }
    }
}

struct Slot {
    url: reqwest::Url,
    secret: SecretHeader,
    timeout: Duration,
    breaker: Breaker,
    /// Requests on the wire to this endpoint.
    in_flight: AtomicUsize,
    metrics: EndpointMetrics,
}

enum SecretHeader {
    None,
    Valid(HeaderValue),
    /// Not sendable as a header: sending without it would skip the
    /// endpoint's authentication, so no request is sent.
    Invalid,
}

/// A request's claim on one endpoint: its in-flight count, and its breaker's
/// probe if it took one. Dropping it wakes deliveries waiting for a breaker;
/// dropped before [`Sending::report`] (the dispatcher was forced), it reopens
/// a half-open breaker so the probe is not lost.
struct Sending<'a> {
    targets: Arc<Targets>,
    index: usize,
    changed: &'a Notify,
    reported: bool,
}

impl<'a> Sending<'a> {
    /// Takes over the in-flight count [`PushConsumer::pick`] took.
    fn new(targets: Arc<Targets>, index: usize, changed: &'a Notify) -> Self {
        Self {
            targets,
            index,
            changed,
            reported: false,
        }
    }

    fn slot(&self) -> &Slot {
        &self.targets.slots[self.index]
    }

    /// Tells the endpoint's breaker whether the endpoint is up.
    fn report(&mut self, up: bool, now: Instant) {
        let breaker = &self.slot().breaker;
        if up {
            breaker.on_success();
        } else {
            breaker.on_failure(now.into_std());
        }
        self.reported = true;
    }
}

impl Drop for Sending<'_> {
    fn drop(&mut self) {
        let slot = &self.targets.slots[self.index];
        slot.in_flight.fetch_sub(1, Ordering::SeqCst);
        if !self.reported && slot.breaker.state() == BreakerState::HalfOpen {
            slot.breaker.on_failure(Instant::now().into_std());
        }
        self.changed.notify_waiters();
    }
}

/// A lease the dispatcher holds. Unless acked, dropping it returns the update
/// to the queue.
struct Held<'a> {
    consumer: &'a PushConsumer,
    seq: u64,
    acked: bool,
}

impl<'a> Held<'a> {
    fn new(consumer: &'a PushConsumer, seq: u64) -> Self {
        consumer.in_flight.fetch_add(1, Ordering::SeqCst);
        Self {
            consumer,
            seq,
            acked: false,
        }
    }

    fn ack(mut self) {
        self.consumer.queue.ack_seq(&self.consumer.holder, self.seq);
        self.acked = true;
    }
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        let consumer = self.consumer;
        if !self.acked {
            consumer.queue.release_seq(&consumer.holder, self.seq);
        }
        consumer.in_flight.fetch_sub(1, Ordering::SeqCst);
        consumer.update_depth();
    }
}

/// What [`PushConsumer::choose`] found.
enum Choice<'a> {
    Send(Sending<'a>),
    /// The update reached `max_age`.
    Expired,
    Cancelled,
}

enum Failure {
    /// Worth retrying: 408, 429, 5xx or a transport error.
    Transient {
        cause: Cause,
        retry_after: Option<Duration>,
    },
    Permanent(Cause),
}

/// What made the last attempt fail, for the dead-letter log.
enum Cause {
    Status(u16),
    /// [`describe_error`]'s URL-free rendering.
    Error(String),
    InvalidSecret,
    /// No attempt was made before `max_age`.
    Expired,
}

impl Cause {
    /// The dead-letter reason of a permanent failure. A redirect is never
    /// followed: it would turn the POST into a GET and could carry the
    /// secret header to another host.
    fn permanent_reason(&self) -> &'static str {
        match self {
            Self::Status(status) if (300..400).contains(status) => "redirect",
            _ => "rejected",
        }
    }
}

impl fmt::Display for Cause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Status(status) => write!(f, "status {status}"),
            Self::Error(err) => f.write_str(err),
            Self::InvalidSecret => f.write_str("secret_token is not a valid header value"),
            Self::Expired => f.write_str("no attempt before max_age"),
        }
    }
}

/// `Retry-After` in seconds; the HTTP-date form is ignored.
fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let value = headers.get(RETRY_AFTER)?.to_str().ok()?;
    value.trim().parse().ok().map(Duration::from_secs)
}
