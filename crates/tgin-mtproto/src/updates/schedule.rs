//! When to fetch: the next action, the deadline, the failure and refusal
//! backoffs and Telegram's flood waits.

use super::*;

/// The later of two pending waits, if any.
pub(super) fn wait_for(a: Option<Instant>, b: Option<Instant>) -> Option<Instant> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

/// The later of a pending retry time and `at`.
pub(super) fn later(pending: Option<Instant>, at: Instant) -> Instant {
    pending.map_or(at, |p| p.max(at))
}

impl UpdateManager {
    /// The next call to run, if one is due. Call again after each answer
    /// and by [`deadline`](Self::deadline).
    pub fn next_action(&mut self) -> Option<Fetch> {
        let now = Instant::now();
        if self.fetch == CommonFetch::Idle
            && (self.gap_deadline.is_some_and(|d| d <= now) || self.idle_at <= now)
        {
            self.fetch = CommonFetch::WantDifference;
            self.gap_deadline = None;
        }
        if self.idle_at <= now {
            self.idle_at = now + IDLE_DIFFERENCE;
        }
        let retry_due = wait_for(self.retry_at, self.refused_until).is_none_or(|d| d <= now)
            && self.common_flood.is_none_or(|d| d <= now);
        match self.fetch {
            CommonFetch::WantState if retry_due => {
                self.fetch = CommonFetch::State { epoch: self.epoch };
                self.again = false;
                self.retry_at = None;
                self.refused_until = None;
                return Some(Fetch::GetState);
            }
            CommonFetch::WantDifference if retry_due => {
                self.fetch = CommonFetch::Difference { epoch: self.epoch };
                // This call postdates every signal so far.
                self.again = false;
                self.retry_at = None;
                self.refused_until = None;
                return Some(Fetch::GetDifference {
                    pts: self.pts,
                    qts: self.qts,
                    date: self.date,
                });
            }
            _ => {}
        }
        if !self.retrying.is_empty() {
            let due: Vec<i64> = self
                .retrying
                .iter()
                .copied()
                .filter(|c| {
                    self.channels
                        .get(c)
                        .and_then(Channel::ready_at)
                        .is_none_or(|d| d <= now)
                })
                .collect();
            for c in due {
                self.retrying.remove(&c);
                if let Some(ch) = self.channels.get_mut(&c) {
                    ch.retry_at = None;
                    ch.refused_until = None;
                    if ch.fetch == ChannelFetch::Wanted {
                        self.queue.push_back(c);
                    }
                }
            }
        }
        if !self.gapped.is_empty() {
            let due: Vec<i64> = self
                .gapped
                .iter()
                .copied()
                .filter(|c| {
                    self.channels
                        .get(c)
                        .and_then(|ch| ch.deadline)
                        .is_none_or(|d| d <= now)
                })
                .collect();
            for c in due {
                self.gapped.remove(&c);
                if let Some(ch) = self.channels.get_mut(&c) {
                    ch.deadline = None;
                    if ch.fetch == ChannelFetch::Idle && ch.pts.is_some() {
                        ch.fetch = ChannelFetch::Wanted;
                        self.queue.push_back(c);
                    }
                }
            }
        }
        if self.channel_flood.is_some_and(|d| d > now) {
            return None;
        }
        while self.in_flight < MAX_CHANNEL_FETCHES {
            let c = self.queue.pop_front()?;
            let Some(ch) = self.channels.get_mut(&c) else {
                continue;
            };
            if ch.fetch != ChannelFetch::Wanted {
                continue;
            }
            if ch.ready_at().is_some_and(|d| d > now) {
                // Backing off: queued again when the wait ends.
                self.retrying.insert(c);
                continue;
            }
            let Some(pts) = ch.pts else {
                ch.fetch = ChannelFetch::Idle;
                continue;
            };
            ch.fetch = ChannelFetch::InFlight { epoch: ch.epoch };
            ch.refused_until = None;
            // This call postdates every signal so far.
            ch.again = false;
            self.in_flight += 1;
            return Some(Fetch::GetChannelDifference { channel: c, pts });
        }
        None
    }

    /// When to call [`next_action`](Self::next_action) next: now when a
    /// fetch is due (a pending common fetch past its retry time, or a
    /// queued channel with a free slot), else the earliest of a gap's
    /// hold, a box's next fetch (after both a failed fetch's retry and a
    /// refusal's wait) and the idle difference. A caller may
    /// sleep until it after every call that feeds or reports anything.
    #[must_use]
    pub fn deadline(&self) -> Instant {
        let now = Instant::now();
        let channel_flood = self.channel_flood.filter(|&d| d > now);
        let mut at = self.idle_at.min(self.common_deadline(now));
        if self.fetch == CommonFetch::Idle
            && let Some(d) = self.gap_deadline
        {
            at = at.min(d);
        }
        for c in self.gapped.iter().chain(&self.retrying) {
            if let Some(ch) = self.channels.get(c) {
                for d in [ch.deadline, ch.ready_at()].into_iter().flatten() {
                    at = at.min(channel_flood.map_or(d, |f| d.max(f)));
                }
            }
        }
        // A queued channel waits only for a free slot and the method's
        // flood wait.
        if !self.queue.is_empty() && self.in_flight < MAX_CHANNEL_FETCHES {
            at = at.min(channel_flood.unwrap_or(now));
        }
        at
    }

    /// When a pending common fetch may run: after a failed fetch's retry,
    /// a refusal's wait and Telegram's flood wait; far off when none is
    /// pending.
    fn common_deadline(&self, now: Instant) -> Instant {
        match self.fetch {
            CommonFetch::WantState | CommonFetch::WantDifference => {
                let at = wait_for(self.retry_at, self.refused_until).unwrap_or(now);
                self.common_flood.map_or(at, |f| at.max(f))
            }
            _ => self.idle_at,
        }
    }

    /// Telegram answered `fetch` with `FLOOD_WAIT_X` (or `SLOWMODE_WAIT_X`):
    /// it is asked again, not before `wait` from now. The wait is apart
    /// from the failure backoff and a refusal's wait, and no signal cuts it
    /// short. For `updates.getChannelDifference` it holds every channel's
    /// fetch, since Telegram limits the method.
    pub fn fetch_flood_wait(&mut self, fetch: Fetch, wait: Duration) {
        let until = Instant::now() + wait;
        let slot = match fetch {
            Fetch::GetState | Fetch::GetDifference { .. } => &mut self.common_flood,
            Fetch::GetChannelDifference { .. } => &mut self.channel_flood,
        };
        *slot = Some(slot.map_or(until, |d| d.max(until)));
        self.fetch_failed(fetch);
    }

    /// Until when the common box waits out Telegram's flood wait, while it
    /// does.
    #[must_use]
    pub fn flood_wait(&self) -> Option<Instant> {
        self.common_flood.filter(|&d| d > Instant::now())
    }

    /// A fetch from [`next_action`](Self::next_action) got no answer (a
    /// connection error or an RPC error). It is asked again after a
    /// backoff, per box: [`RETRY_MIN`], doubling up to [`RETRY_MAX`], reset
    /// by the box's next answer. Its updates wait meanwhile.
    ///
    /// A call made stale by a rollback (a batch refused while it was in
    /// flight) is replaced at once by the catch-up from the committed
    /// state, as its answer would have been.
    pub fn fetch_failed(&mut self, fetch: Fetch) {
        let now = Instant::now();
        match fetch {
            Fetch::GetState | Fetch::GetDifference { .. } => {
                let (want, epoch) = match (fetch, self.fetch) {
                    (Fetch::GetState, CommonFetch::State { epoch }) => {
                        (CommonFetch::WantState, epoch)
                    }
                    (Fetch::GetDifference { .. }, CommonFetch::Difference { epoch }) => {
                        (CommonFetch::WantDifference, epoch)
                    }
                    _ => return,
                };
                if epoch != self.epoch {
                    self.fetch = self.resync();
                    return;
                }
                self.fetch = want;
                self.retry_at = Some(now + self.backoff.next());
            }
            Fetch::GetChannelDifference { channel, .. } => {
                if let Some(ch) = self.channels.get_mut(&channel)
                    && let ChannelFetch::InFlight { epoch } = ch.fetch
                {
                    self.in_flight = self.in_flight.saturating_sub(1);
                    ch.fetch = ChannelFetch::Wanted;
                    if epoch != ch.epoch {
                        self.queue.push_back(channel);
                        return;
                    }
                    ch.retry_at = Some(now + ch.backoff.next());
                    self.retrying.insert(channel);
                }
            }
        }
    }

    /// A fetch failed in a way asking again cannot fix (for example
    /// PERSISTENT_TIMESTAMP_INVALID, CHANNEL_INVALID, or a channel whose
    /// access hash the caller lacks). A failed difference resyncs with
    /// `updates.getState`; getState itself is asked again as by
    /// [`fetch_failed`](Self::fetch_failed). A channel's state is dropped,
    /// as for an undecodable answer: its next update starts it again, and
    /// its held updates are delivered from there.
    ///
    /// A call made stale by a rollback failed about a state that is gone:
    /// it is replaced by the catch-up from the committed state instead, so
    /// the refused updates are fetched again, not skipped.
    pub fn fetch_failed_permanently(&mut self, fetch: Fetch) -> Vec<Batch> {
        match fetch {
            Fetch::GetState => self.fetch_failed(fetch),
            Fetch::GetDifference { .. } => {
                if let CommonFetch::Difference { epoch } = self.fetch {
                    self.fetch = if epoch == self.epoch {
                        CommonFetch::WantState
                    } else {
                        self.resync()
                    };
                }
            }
            Fetch::GetChannelDifference { channel, .. } => {
                let Some(ch) = self.channels.get_mut(&channel) else {
                    return Vec::new();
                };
                let ChannelFetch::InFlight { epoch } = ch.fetch else {
                    return Vec::new();
                };
                self.in_flight = self.in_flight.saturating_sub(1);
                if epoch != ch.epoch {
                    ch.fetch = ChannelFetch::Wanted;
                    self.queue.push_back(channel);
                    return Vec::new();
                }
                return self.abandon_channel(channel);
            }
        }
        Vec::new()
    }
}
