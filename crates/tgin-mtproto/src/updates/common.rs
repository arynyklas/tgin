//! The common box: pts, qts and seq, the pushes held for a gap, and the
//! `getState` / `getDifference` chain.

use super::*;

impl UpdateManager {
    /// The answer to [`Fetch::GetState`]. Held common and qts updates are
    /// delivered as they are, then the state is taken whole.
    pub fn feed_state(&mut self, bytes: Bytes) -> Vec<Batch> {
        let CommonFetch::State { epoch } = self.fetch else {
            return Vec::new();
        };
        self.backoff = Duration::ZERO;
        if epoch != self.epoch {
            self.fetch = self.resync();
            return Vec::new();
        }
        let s = match decode::state(&bytes) {
            Ok(s) => s,
            Err(e) => {
                // Never retried in a loop: the idle difference comes next.
                self.undecodable("state", &bytes, &e);
                self.fetch = CommonFetch::Idle;
                self.chain_end();
                return Vec::new();
            }
        };
        let mut out = Out::new(self.empty_group());
        let mut held = std::mem::take(&mut self.held_pts);
        held.append(&mut self.held_qts);
        held.sort_by_key(|h| h.group.origin);
        for h in held {
            if let Item::Update(ub) = h.item {
                self.deliver(&mut out, &h.group, ub);
            }
        }
        self.pts = s.pts;
        self.qts = s.qts;
        self.seq = s.seq;
        self.date = s.date;
        self.seq_seen.clear();
        self.gap_deadline = None;
        self.fetch = CommonFetch::Idle;
        self.idle_at = Instant::now() + IDLE_DIFFERENCE;
        self.touch_common_all(&mut out);
        self.chain_end();
        self.finish(out)
    }

    /// The answer to [`Fetch::GetDifference`].
    pub fn feed_difference(&mut self, bytes: Bytes) -> Vec<Batch> {
        let CommonFetch::Difference { epoch } = self.fetch else {
            return Vec::new();
        };
        self.backoff = Duration::ZERO;
        if epoch != self.epoch {
            // Asked before a refused batch rolled the state back.
            self.fetch = self.resync();
            return Vec::new();
        }
        self.idle_at = Instant::now() + IDLE_DIFFERENCE;
        let d = match decode::difference(&bytes) {
            Ok(d) if d.constructor.starts_with("updates.difference") => d,
            Ok(_) => {
                self.undecodable(
                    "difference",
                    &bytes,
                    &TlError::WrongType {
                        want: "updates.Difference",
                        got: "updates.ChannelDifference",
                    },
                );
                self.fetch = CommonFetch::WantState;
                return Vec::new();
            }
            Err(e) => {
                // Skip past it: resync from `updates.getState`, so the same
                // difference is never fetched again.
                self.undecodable("difference", &bytes, &e);
                self.fetch = CommonFetch::WantState;
                return Vec::new();
            }
        };
        let home = self.group(&bytes, d.users, d.chats);
        let mut out = Out::new(home.clone());
        match d.constructor {
            "updates.differenceEmpty" => {
                self.date = d.date.unwrap_or(self.date);
                self.seq = d.seq.unwrap_or(self.seq);
                out.open.delta.date = Some(self.date);
                out.open.delta.seq = Some(self.seq);
                out.open.common = true;
                self.fetch = CommonFetch::Idle;
            }
            "updates.differenceTooLong" => match d.pts {
                Some(p) if p > self.pts => {
                    // The box jumps to `p`: no difference from there brings
                    // back the held pushes it covers, so they go as they
                    // are (as `feed_state` does). Those already committed
                    // go: cut at the committed pts, not the in-memory one,
                    // so a push applied by a pending batch survives that
                    // batch's refusal (duplicates are allowed, losses not).
                    let local = self.committed.pts;
                    let (covered, rest): (Vec<Held>, Vec<Held>) =
                        std::mem::take(&mut self.held_pts)
                            .into_iter()
                            .partition(|h| h.pts <= p);
                    let mut covered: Vec<Held> =
                        covered.into_iter().filter(|h| h.pts > local).collect();
                    self.held_pts = rest;
                    covered.sort_by_key(|h| h.group.origin);
                    for h in covered {
                        if let Item::Update(ub) = h.item {
                            self.deliver(&mut out, &h.group, ub);
                        }
                    }
                    self.switch(&mut out, &home);
                    self.pts = p;
                    out.open.delta.pts = Some(p);
                    out.open.common = true;
                    self.fetch = CommonFetch::WantDifference;
                }
                _ => self.fetch = CommonFetch::WantState,
            },
            _ => {
                let slice = d.constructor == "updates.differenceSlice";
                let pts = d.pts.unwrap_or(self.pts);
                let qts = d.qts.unwrap_or(self.qts);
                for m in &d.new_messages {
                    self.compose(&mut out, UPDATE_NEW_MESSAGE, m, &[pts, 0]);
                }
                for m in &d.new_encrypted_messages {
                    self.compose(&mut out, UPDATE_NEW_ENCRYPTED_MESSAGE, m, &[qts]);
                }
                for u in &d.other_updates {
                    match UpdateBytes::new(bytes.slice_ref(u)) {
                        Ok(ub) => self.offer(ub, Mode::Difference, &mut out),
                        Err(e) => self.undecodable("update", u, &e),
                    }
                }
                let progressed = (pts, qts) != (self.pts, self.qts);
                self.pts = pts;
                self.qts = qts;
                self.seq = d.seq.unwrap_or(self.seq);
                self.date = d.date.unwrap_or(self.date);
                // The whole difference is one commit: its state goes
                // on its last batch.
                self.switch(&mut out, &home);
                self.touch_common_all(&mut out);
                self.fetch = if slice && progressed {
                    CommonFetch::WantDifference
                } else {
                    CommonFetch::Idle
                };
            }
        }
        if self.fetch == CommonFetch::Idle {
            self.seq_seen.clear();
            // With a follow-up chain due, an uncached peer's short update
            // waits for it; otherwise it goes as it is.
            let check = self.again;
            self.release_common(Common::Pts, check, &mut out);
            self.release_common(Common::Qts, check, &mut out);
        }
        self.chain_end();
        self.rearm_gap();
        self.finish(out)
    }

    pub(super) fn offer_common(&mut self, which: Common, h: Held, out: &mut Out) {
        if self.fetch.busy() {
            if h.check_peer {
                // Its covering difference may be one that started earlier.
                self.want_difference();
            }
            self.hold_common(which, h);
            return;
        }
        let local = i64::from(self.local(which));
        match h.before().cmp(&local) {
            std::cmp::Ordering::Equal if h.check_peer => {
                // Fetch the difference that covers it: it brings the users
                // and chats. The update stays held until then.
                self.hold_common(which, h);
                self.want_difference();
            }
            std::cmp::Ordering::Equal => {
                self.apply_common(which, h, out);
                self.release_common(which, true, out);
            }
            std::cmp::Ordering::Less => {} // already applied
            std::cmp::Ordering::Greater => {
                self.hold_common(which, h);
                self.arm_gap();
            }
        }
    }

    fn local(&self, which: Common) -> i32 {
        match which {
            Common::Pts => self.pts,
            Common::Qts => self.qts,
        }
    }

    fn held_mut(&mut self, which: Common) -> &mut Vec<Held> {
        match which {
            Common::Pts => &mut self.held_pts,
            Common::Qts => &mut self.held_qts,
        }
    }

    fn hold_common(&mut self, which: Common, h: Held) {
        let held = self.held_mut(which);
        if held.len() < MAX_HELD {
            held.push(h);
        }
    }

    fn apply_common(&mut self, which: Common, h: Held, out: &mut Out) {
        if let Item::Update(ub) = h.item {
            self.deliver(out, &h.group, ub);
        }
        match which {
            Common::Pts => {
                self.pts = h.pts;
                out.open.delta.pts = Some(h.pts);
            }
            Common::Qts => {
                self.qts = h.pts;
                out.open.delta.qts = Some(h.pts);
            }
        }
        out.open.common = true;
    }

    /// Applies the held updates that now follow on. With `check`, a live
    /// short update from an uncached peer still waits for a difference;
    /// after a difference it goes as it is, so the check never loops.
    fn release_common(&mut self, which: Common, check: bool, out: &mut Out) {
        if self.fetch.busy() || self.held_mut(which).is_empty() {
            return;
        }
        let mut held = std::mem::take(self.held_mut(which));
        held.sort_by_key(Held::before);
        let mut keep = Vec::new();
        for h in held {
            let local = i64::from(self.local(which));
            if !keep.is_empty() || h.before() > local {
                keep.push(h);
            } else if h.before() == local {
                if check && h.check_peer && self.unknown_peer(&facts_of(&h)) {
                    keep.push(h);
                    self.want_difference();
                } else {
                    self.apply_common(which, h, out);
                }
            }
        }
        *self.held_mut(which) = keep;
    }

    /// The catch-up after a rollback: a difference from the committed
    /// state, or `updates.getState` when none was ever committed (a
    /// difference from pts 0 is refused: PERSISTENT_TIMESTAMP_EMPTY).
    pub(super) fn resync(&self) -> CommonFetch {
        if self.committed.pts == 0 {
            CommonFetch::WantState
        } else {
            CommonFetch::WantDifference
        }
    }

    /// `updates.getDifference` as soon as possible (a gap's hold is over,
    /// `updatesTooLong`, or an uncached peer). While one is only pending,
    /// the signal joins it. While a call is in flight, that call may
    /// predate the signal, so exactly one more difference follows the
    /// chain's end: the signal is never lost, and a burst runs one chain.
    pub(super) fn want_difference(&mut self) {
        match self.fetch {
            CommonFetch::Idle => self.fetch = CommonFetch::WantDifference,
            CommonFetch::State { .. } | CommonFetch::Difference { .. } => self.again = true,
            CommonFetch::WantState | CommonFetch::WantDifference => {}
        }
        self.gap_deadline = None;
    }

    /// The common chain is over: one more difference if a signal came
    /// while a call of it was in flight.
    fn chain_end(&mut self) {
        if self.fetch == CommonFetch::Idle && std::mem::take(&mut self.again) {
            self.fetch = CommonFetch::WantDifference;
        }
        if self.fetch == CommonFetch::Idle {
            // The start-up difference is over: the channels left behind by
            // the last run catch up now, through the fetch queue.
            for c in std::mem::take(&mut self.startup) {
                if let Some(ch) = self.channels.get_mut(&c)
                    && ch.fetch == ChannelFetch::Idle
                    && ch.pts.is_some()
                {
                    ch.fetch = ChannelFetch::Wanted;
                    self.queue.push_back(c);
                }
            }
        }
    }

    pub(super) fn arm_gap(&mut self) {
        self.gap_deadline
            .get_or_insert_with(|| Instant::now() + GAP_HOLD);
    }

    /// Keeps the common gap deadline only while a gap is open.
    pub(super) fn rearm_gap(&mut self) {
        let open =
            !(self.held_pts.is_empty() && self.held_qts.is_empty() && self.seq_seen.is_empty());
        if open && !self.fetch.busy() {
            self.arm_gap();
        } else {
            self.gap_deadline = None;
        }
    }

    /// After a seq is applied, the containers already handled that now
    /// follow on close the seq gap.
    pub(super) fn advance_seq(&mut self, out: &mut Out) {
        loop {
            let next = i64::from(self.seq) + 1;
            self.seq_seen.retain(|&(s, _)| i64::from(s) >= next);
            let Some(i) = self
                .seq_seen
                .iter()
                .position(|&(s, _)| i64::from(s) == next)
            else {
                break;
            };
            let (_, end) = self.seq_seen.swap_remove(i);
            self.seq = end;
            out.open.delta.seq = Some(end);
        }
    }

    fn touch_common_all(&self, out: &mut Out) {
        out.open.delta.pts = Some(self.pts);
        out.open.delta.qts = Some(self.qts);
        out.open.delta.seq = Some(self.seq);
        out.open.delta.date = Some(self.date);
        out.open.common = true;
    }
}
