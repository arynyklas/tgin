//! The ingest: pushes and fetches fed to the update manager, batches
//! admitted and committed, and the state published each turn.

use super::*;

/// Refusals of a channel's difference that say the account can no longer
/// read the channel.
const CHANNEL_GONE: [&str; 3] = [
    "CHANNEL_PRIVATE",
    "CHANNEL_INVALID",
    "CHANNEL_PUBLIC_GROUP_NA",
];

type Fetched = (Fetch, Result<RpcOutcome, SenderError>);

/// One run's ingest: the manager, its fetches and the batches it gives.
pub(super) struct Ingest<'a> {
    pub(super) ctx: &'a Ctx,
    pub(super) store: &'a SessionStore,
    pub(super) session: Arc<dyn Session>,
    pub(super) manager: UpdateManager,
    pub(super) ids: UpdateIds,
    pub(super) fetches: JoinSet<Fetched>,
    /// What [`UpdateManager::dropped`] said last.
    pub(super) dropped: Dropped,
    /// What [`UpdateManager::flood_wait`] said last.
    pub(super) flood: Option<(Instant, SystemTime)>,
    /// Whether this turn's catch-up answers reached Telegram, for
    /// [`publish`](Self::publish).
    pub(super) reached: Option<bool>,
    /// The account's frozen flag as [`publish`](Self::publish) last saw it.
    pub(super) frozen: bool,
}

impl Ingest<'_> {
    pub(super) async fn run(
        &mut self,
        mut pushes: Pushes,
        affected: &mut mpsc::Receiver<(BoxKind, i32, i32)>,
    ) -> End {
        loop {
            if let Err(end) = self.issue() {
                return end;
            }
            self.publish();
            let deadline = self.manager.deadline();
            let shared = &self.ctx.shared;
            let handled = tokio::select! {
                biased;
                () = self.ctx.ingest.cancelled() => return End::Stopped,
                () = shared.signal.notified() => {
                    if shared.key_gone.swap(false, Ordering::SeqCst) {
                        return self.ctx.needs_login();
                    }
                    Ok(())
                }
                push = pushes.recv() => match push {
                    Some(push) => {
                        let batches = self.manager.feed_push(push);
                        self.handle(&batches)
                    }
                    None => {
                        tracing::warn!(account = %shared.name, "the account's main session ended");
                        return End::Retry;
                    }
                },
                Some((box_kind, pts, count)) = affected.recv() => {
                    let batches = self.manager.feed_affected(box_kind, pts, count);
                    self.handle(&batches)
                }
                Some(done) = self.fetches.join_next(), if !self.fetches.is_empty() => {
                    match done {
                        Ok(fetched) => self.answer(fetched),
                        // A fetch lost its task: the manager would wait for
                        // it for ever, so this run ends.
                        Err(_) => Err(End::Retry),
                    }
                }
                () = tokio::time::sleep_until(deadline) => Ok(()),
            };
            if let Err(end) = handled {
                return end;
            }
        }
    }

    /// Runs every fetch the manager wants now.
    fn issue(&mut self) -> Result<(), End> {
        while let Some(fetch) = self.manager.next_action() {
            match fetch_body(fetch, self.store) {
                Ok(Some(body)) => {
                    let session = Arc::clone(&self.session);
                    self.fetches
                        .spawn(async move { (fetch, session.invoke(body).await) });
                }
                Ok(None) => {
                    tracing::debug!(account = %self.ctx.shared.name, "no access hash for a channel to catch up; its state is dropped");
                    let batches = self.manager.fetch_failed_permanently(fetch);
                    self.handle(&batches)?;
                }
                Err(e) => return Err(self.ctx.storage_failed(&e)),
            }
        }
        Ok(())
    }

    /// A fetch's outcome, back to the manager.
    fn answer(&mut self, (fetch, outcome): Fetched) -> Result<(), End> {
        let name = &self.ctx.shared.name;
        match outcome {
            Ok(RpcOutcome::Result(bytes)) => {
                self.reached = Some(true);
                let batches = match fetch {
                    Fetch::GetState => self.manager.feed_state(bytes),
                    Fetch::GetDifference { .. } => self.manager.feed_difference(bytes),
                    Fetch::GetChannelDifference { channel, .. } => {
                        self.manager.feed_channel_difference(channel, bytes)
                    }
                };
                self.handle(&batches)
            }
            Ok(RpcOutcome::Error { code, message }) => {
                self.reached = Some(true);
                tracing::debug!(account = %name, fetch = fetch_name(fetch), code, error = %message, "a catch-up call was refused");
                if lost_authorization(code, &message) {
                    return Err(self.ctx.needs_login());
                }
                match (classify(code, &message), fetch) {
                    (ErrorClass::Frozen, _) => {
                        self.ctx.shared.mark_frozen();
                        self.manager.fetch_failed(fetch);
                    }
                    (ErrorClass::FloodWait(x) | ErrorClass::SlowMode(x), _) => {
                        tracing::warn!(account = %name, fetch = fetch_name(fetch), seconds = x, "Telegram asks the account to wait before it catches up again");
                        self.manager
                            .fetch_flood_wait(fetch, Duration::from_secs(u64::from(x)));
                    }
                    (_, Fetch::GetChannelDifference { channel, .. })
                        if CHANNEL_GONE.contains(&message.as_str()) =>
                    {
                        self.manager.forget_channel(channel);
                    }
                    _ if (400..500).contains(&code) => {
                        let batches = self.manager.fetch_failed_permanently(fetch);
                        self.handle(&batches)?;
                    }
                    _ => self.manager.fetch_failed(fetch),
                }
                Ok(())
            }
            Err(e) => match e {
                SenderError::KeyRevoked | SenderError::KeyUnknown => Err(self.ctx.needs_login()),
                SenderError::InitRefused { code, message } if (400..500).contains(&code) => {
                    Err(self.ctx.init_refused(code, &message))
                }
                SenderError::BadRequest => {
                    let batches = self.manager.fetch_failed_permanently(fetch);
                    self.handle(&batches)
                }
                e => {
                    tracing::debug!(account = %name, fetch = fetch_name(fetch), error = %e, "a catch-up call failed; it is asked again");
                    self.reached = Some(false);
                    self.manager.fetch_failed(fetch);
                    Ok(())
                }
            },
        }
    }

    /// One manager call's batches: their marks and peers in one write,
    /// then each admitted or refused, in order, and the state that gives
    /// persisted.
    fn handle(&mut self, batches: &[Batch]) -> Result<(), End> {
        if batches.is_empty() {
            return Ok(());
        }
        let ctx = self.ctx;
        let stored = |r: Result<(), SessionError>| r.map_err(|e| ctx.storage_failed(&e));
        stored(self.store.mark_channels_behind(batches))?;
        for batch in batches {
            let admission = match ctx.sink.admit(batch, &mut self.ids, self.store) {
                Ok(admission) => admission,
                Err(e) => {
                    tracing::error!(account = %ctx.shared.name, error = %e, "the account's update ids could not be persisted");
                    ctx.shared.set_state(AccountState::Failed(Failure::Storage));
                    return Err(End::Retry);
                }
            };
            let delta = match admission {
                Admission::Admitted => {
                    let n = u64::try_from(batch.updates.len()).unwrap_or(u64::MAX);
                    ctx.shared.updates.fetch_add(n, Ordering::Relaxed);
                    self.manager.committed(batch.id)
                }
                Admission::Refused => self.manager.refused(batch.id),
            };
            if let Some(delta) = delta {
                stored(self.store.commit_state(&delta))?;
            }
        }
        Ok(())
    }

    /// The status the manager gives: boxes catching up, drops.
    fn publish(&mut self) {
        let shared = &self.ctx.shared;
        let catching = self.manager.catching_up();
        shared.common.store(catching.common, Ordering::Relaxed);
        shared.channels.store(catching.channels, Ordering::Relaxed);
        let dropped = self.manager.dropped();
        shared.undecodable.fetch_add(
            dropped.undecodable.saturating_sub(self.dropped.undecodable),
            Ordering::Relaxed,
        );
        shared.full.fetch_add(
            dropped.full.saturating_sub(self.dropped.full),
            Ordering::Relaxed,
        );
        self.dropped = dropped;
        // The state, decided once per turn: the common box waiting out
        // Telegram's flood wait shows it until then, whatever answered
        // meanwhile (a channel's fetch, an RPC call); otherwise whether the
        // last catch-up call reached Telegram.
        let flood = self.manager.flood_wait();
        if flood != self.flood.map(|(at, _)| at) {
            self.flood = flood.map(|at| {
                let until = SystemTime::now() + at.saturating_duration_since(Instant::now());
                (at, until)
            });
            if flood.is_none() && self.reached.is_none() {
                self.reached = Some(true);
            }
        }
        let frozen = shared.frozen.load(Ordering::Relaxed);
        let newly_frozen = frozen && !std::mem::replace(&mut self.frozen, frozen);
        match (self.flood, self.reached.take()) {
            (Some((_, until)), _) => shared.set_state(AccountState::FloodWait { until }),
            (None, Some(true)) => shared.connected(),
            (None, Some(false)) => shared.set_state(AccountState::Connecting),
            // An RPC answer reported the account frozen: Telegram answered.
            (None, None) if newly_frozen => shared.connected(),
            (None, None) => {}
        }
    }
}

fn fetch_name(fetch: Fetch) -> &'static str {
    match fetch {
        Fetch::GetState => "updates.getState",
        Fetch::GetDifference { .. } => "updates.getDifference",
        Fetch::GetChannelDifference { .. } => "updates.getChannelDifference",
    }
}

/// The request for `fetch`; `None` for a channel whose access hash is not
/// cached.
fn fetch_body(fetch: Fetch, store: &SessionStore) -> Result<Option<Bytes>, SessionError> {
    let body = match fetch {
        Fetch::GetState => tl::functions::updates::GetState {}.to_bytes(),
        Fetch::GetDifference { pts, qts, date } => tl::functions::updates::GetDifference {
            pts,
            pts_limit: None,
            pts_total_limit: None,
            date,
            qts,
            qts_limit: None,
        }
        .to_bytes(),
        Fetch::GetChannelDifference { channel, pts } => {
            let Some(access_hash) = store
                .peer(crate::decode::channel_dialog_id(channel))?
                .and_then(|peer| peer.access_hash)
            else {
                return Ok(None);
            };
            tl::functions::updates::GetChannelDifference {
                force: false,
                channel: tl::enums::InputChannel::Channel(tl::types::InputChannel {
                    channel_id: channel,
                    access_hash,
                }),
                filter: tl::enums::ChannelMessagesFilter::Empty,
                pts,
                limit: CHANNEL_DIFFERENCE_LIMIT,
            }
            .to_bytes()
        }
    };
    Ok(Some(Bytes::from(body)))
}
