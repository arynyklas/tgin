//! Channels: each channel's pts, its pushes held for a gap, the fetch
//! queue and `getChannelDifference`, and channels dropped or forgotten.

use super::*;

impl UpdateManager {
    /// Stops tracking a channel the account can no longer read (e.g.
    /// `CHANNEL_PRIVATE`): its state and held updates are dropped, and the
    /// next committed delta tells the caller to delete its stored rows
    /// ([`StateDelta::forgotten`]), so it is not loaded at every start.
    pub fn forget_channel(&mut self, channel: i64) {
        if let Some(ch) = self.channels.remove(&channel)
            && matches!(ch.fetch, ChannelFetch::InFlight { .. })
        {
            self.in_flight = self.in_flight.saturating_sub(1);
        }
        self.gapped.remove(&channel);
        self.retrying.remove(&channel);
        self.drop_stored(channel);
    }

    /// Forgets a channel's committed state and its behind mark, here and,
    /// through the next committed delta, in the store.
    fn drop_stored(&mut self, channel: i64) {
        self.committed_channels.remove(&channel);
        self.startup.retain(|&c| c != channel);
        self.ahead.remove(&channel);
        self.ahead_new.retain(|&c| c != channel);
        self.caught_up.retain(|&(c, _)| c != channel);
        // Every mark so far is a lower batch id; a later one is not.
        let bound = self.next_id;
        match self.forgotten.iter_mut().find(|(c, _)| *c == channel) {
            Some(row) => row.1 = bound,
            None => self.forgotten.push((channel, bound)),
        }
    }

    /// Drops a channel's state, so no difference is fetched from it again
    /// (an undecodable or permanently failed fetch, its call no longer in
    /// flight, e.g. no access hash to ask with): its next update starts it
    /// again, and its held updates are delivered from there. Its stored
    /// rows go too, so a quiet channel is not left behind for good.
    pub(super) fn abandon_channel(&mut self, channel: i64) -> Vec<Batch> {
        self.drop_stored(channel);
        if let Some(ch) = self.channels.get_mut(&channel) {
            // Pending batches of the dropped state no longer commit it.
            ch.epoch += 1;
            ch.pts = None;
            ch.fetch = ChannelFetch::Idle;
            ch.again = false;
            ch.retry_at = None;
            ch.backoff = Duration::ZERO;
            ch.refused_until = None;
        }
        self.retrying.remove(&channel);
        let mut out = Out::new(self.empty_group());
        self.release_channel(channel, &mut out);
        self.finish(out)
    }

    /// The answer to [`Fetch::GetChannelDifference`] for `channel`.
    pub fn feed_channel_difference(&mut self, channel: i64, bytes: Bytes) -> Vec<Batch> {
        let Some(ch) = self.channels.get_mut(&channel) else {
            return Vec::new();
        };
        let ChannelFetch::InFlight { epoch } = ch.fetch else {
            return Vec::new();
        };
        self.in_flight = self.in_flight.saturating_sub(1);
        ch.backoff = Duration::ZERO;
        if epoch != ch.epoch {
            ch.fetch = ChannelFetch::Wanted;
            self.queue.push_back(channel);
            return Vec::new();
        }
        let old = ch.pts;
        let d = match decode::difference(&bytes) {
            Ok(d) if d.constructor.starts_with("updates.channelDifference") => Ok(d),
            Ok(_) => Err(TlError::WrongType {
                want: "updates.ChannelDifference",
                got: "updates.Difference",
            }),
            Err(e) => Err(e),
        };
        let d = match d {
            Ok(d) => d,
            Err(e) => {
                self.undecodable("channel difference", &bytes, &e);
                return self.abandon_channel(channel);
            }
        };
        let home = self.group(&bytes, d.users, d.chats);
        let mut out = Out::new(home.clone());
        let pts = d.pts.or(old).unwrap_or(0);
        if d.constructor == "updates.channelDifferenceTooLong" {
            // The channel jumps to `pts`, past updates no difference brings
            // back: the held pushes it covers go as they are (as for
            // `differenceTooLong`); its `messages` are only the latest.
            if let Some(ch) = self.channels.get_mut(&channel) {
                // Cut at the committed pts, not the in-memory one: a held
                // push applied by a pending batch must survive that batch's
                // refusal (duplicates are allowed, losses are not).
                let local = self.committed_channels.get(&channel).copied().unwrap_or(0);
                let (covered, rest): (Vec<Held>, Vec<Held>) = std::mem::take(&mut ch.held)
                    .into_iter()
                    .partition(|h| h.pts <= pts);
                // Those already committed go.
                let mut covered: Vec<Held> =
                    covered.into_iter().filter(|h| h.pts > local).collect();
                ch.held = rest;
                covered.sort_by_key(|h| h.group.origin);
                for h in covered {
                    if let Item::Update(ub) = h.item {
                        self.deliver(&mut out, &h.group, ub);
                    }
                }
            }
            self.switch(&mut out, &home);
        }
        for m in &d.new_messages {
            self.compose(&mut out, UPDATE_NEW_CHANNEL_MESSAGE, m, &[pts, 0]);
        }
        for u in &d.other_updates {
            match UpdateBytes::new(bytes.slice_ref(u)) {
                Ok(ub) => self.offer(ub, Mode::ChannelDifference(channel), &mut out),
                Err(e) => self.undecodable("update", u, &e),
            }
        }
        self.switch(&mut out, &home);
        let mut more = !d.is_final && d.pts.is_some_and(|p| old.is_none_or(|o| p > o));
        if let Some(ch) = self.channels.get_mut(&channel) {
            more |= std::mem::take(&mut ch.again);
            ch.pts = d.pts;
            if let Some(p) = d.pts {
                out.open.touch_channel(channel, p);
            }
            if more {
                ch.fetch = ChannelFetch::Wanted;
                self.queue.push_back(channel);
            } else {
                ch.fetch = ChannelFetch::Idle;
            }
        }
        self.mark_ahead(channel);
        self.release_channel(channel, &mut out);
        self.finish(out)
    }

    pub(super) fn offer_channel(&mut self, c: i64, h: Held, out: &mut Out) {
        let ch = self.channels.entry(c).or_insert_with(|| Channel::new(None));
        if ch.fetch != ChannelFetch::Idle {
            if ch.held.len() < MAX_HELD {
                ch.held.push(h);
            }
            self.mark_ahead(c);
            return;
        }
        let Some(local) = ch.pts else {
            self.adopt(c, &h, out);
            self.apply_channel(c, h, out);
            self.release_channel(c, out);
            return;
        };
        match h.before().cmp(&i64::from(local)) {
            std::cmp::Ordering::Equal => {
                self.apply_channel(c, h, out);
                self.release_channel(c, out);
            }
            std::cmp::Ordering::Less => {} // already applied
            std::cmp::Ordering::Greater => {
                if ch.held.len() < MAX_HELD {
                    ch.held.push(h);
                }
                ch.deadline.get_or_insert_with(|| Instant::now() + GAP_HOLD);
                self.gapped.insert(c);
                self.mark_ahead(c);
            }
        }
    }

    /// An unknown channel's state starts just before its first update, in
    /// a batch of its own: once that batch is committed, a crash before
    /// the update's own commit still leaves a state to catch up from.
    fn adopt(&mut self, c: i64, h: &Held, out: &mut Out) {
        let base = i32::try_from(h.before()).unwrap_or(h.pts);
        self.set_baseline(c, base, out);
    }

    fn set_baseline(&mut self, c: i64, pts: i32, out: &mut Out) {
        let group = out.open.group.clone();
        self.flush(out);
        self.mark_ahead(c);
        if let Some(ch) = self.channels.get_mut(&c) {
            ch.pts = Some(pts);
        }
        out.open.touch_channel(c, pts);
        self.flush(out);
        out.open.group = group;
    }

    fn apply_channel(&mut self, c: i64, h: Held, out: &mut Out) {
        if let Item::Update(ub) = h.item {
            self.deliver(out, &h.group, ub);
        }
        if let Some(ch) = self.channels.get_mut(&c) {
            ch.pts = Some(h.pts);
        }
        out.open.touch_channel(c, h.pts);
        self.mark_ahead(c);
    }

    fn release_channel(&mut self, c: i64, out: &mut Out) {
        let Some(ch) = self.channels.get_mut(&c) else {
            return;
        };
        if ch.fetch != ChannelFetch::Idle || ch.held.is_empty() {
            if ch.held.is_empty() {
                ch.deadline = None;
                self.gapped.remove(&c);
            }
            return;
        }
        let mut held = std::mem::take(&mut ch.held);
        held.sort_by_key(Held::before);
        let mut keep = Vec::new();
        for h in held {
            let local = self.channels.get(&c).and_then(|ch| ch.pts);
            match local {
                None => {
                    self.adopt(c, &h, out);
                    self.apply_channel(c, h, out);
                }
                Some(_) if !keep.is_empty() => keep.push(h),
                Some(l) => match h.before().cmp(&i64::from(l)) {
                    std::cmp::Ordering::Equal => self.apply_channel(c, h, out),
                    std::cmp::Ordering::Less => {}
                    std::cmp::Ordering::Greater => keep.push(h),
                },
            }
        }
        if let Some(ch) = self.channels.get_mut(&c) {
            if keep.is_empty() {
                ch.deadline = None;
                self.gapped.remove(&c);
            } else {
                ch.deadline.get_or_insert_with(|| Instant::now() + GAP_HOLD);
                self.gapped.insert(c);
            }
            ch.held = keep;
        }
    }

    /// `updateChannelTooLong`: a catch-up signal for the channel, with or
    /// without a pts. An unknown channel has nothing to catch up from; its
    /// pts, when given, becomes its state.
    pub(super) fn channel_too_long(&mut self, c: i64, pts: Option<i32>, out: &mut Out) {
        let ch = self.channels.entry(c).or_insert_with(|| Channel::new(None));
        match ch.pts {
            None => {
                if let Some(p) = pts.filter(|&p| p != 0)
                    && ch.fetch == ChannelFetch::Idle
                {
                    self.set_baseline(c, p, out);
                    self.release_channel(c, out);
                }
            }
            Some(_) => match ch.fetch {
                ChannelFetch::Idle => {
                    ch.fetch = ChannelFetch::Wanted;
                    ch.deadline = None;
                    self.gapped.remove(&c);
                    self.queue.push_back(c);
                }
                // The call in flight may predate the signal: one more
                // fetch follows it, even after a `final` answer.
                ChannelFetch::InFlight { .. } => ch.again = true,
                // Not yet asked: the signal joins it. A pending retry is
                // cut short: the server just showed it is reachable.
                ChannelFetch::Wanted => {
                    if ch.retry_at.take().is_some() {
                        ch.backoff = Duration::ZERO;
                        self.retrying.remove(&c);
                        self.queue.push_back(c);
                    }
                }
            },
        }
        if self.channels.get(&c).is_some_and(|ch| ch.pts.is_some()) {
            self.mark_ahead(c);
        }
    }

    /// `c` is ahead of its committed state: the next batch names it, unless
    /// it is already persisted as behind.
    fn mark_ahead(&mut self, c: i64) {
        if let std::collections::btree_map::Entry::Vacant(e) = self.ahead.entry(c) {
            e.insert(MARK_PENDING);
            self.ahead_new.push(c);
        }
    }

    /// Whether `c` is where its committed state says: nothing held,
    /// nothing to fetch, its pts committed.
    pub(super) fn caught_up_with_commit(&self, c: i64) -> bool {
        // A start-up channel has not caught up yet: its fetch comes after
        // the start-up difference.
        !self.startup.contains(&c)
            && self.channels.get(&c).is_none_or(|ch| {
                ch.fetch == ChannelFetch::Idle
                    && ch.held.is_empty()
                    && ch.ready_at().is_none()
                    && ch.pts == self.committed_channels.get(&c).copied()
            })
    }
}
