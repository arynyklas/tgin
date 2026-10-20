//! The connection loop: connecting and reconnecting, reading and
//! writing, pings, and handing answers out.

use super::*;

impl<T: Transport> Actor<T> {
    pub(super) async fn run(mut self, reader: T::Reader, writer: T::Writer) {
        let mut conn = Some((reader, writer, None));
        loop {
            let (reader, writer, first) = match conn.take() {
                Some(c) => c,
                None => match self.idle().await {
                    Some(c) => c,
                    None => return,
                },
            };
            if self.serve(reader, writer, first).await {
                return;
            }
        }
    }

    /// Without a connection: hands out pushes, and opens a fresh connection
    /// for the next request. `None` once the [`Sender`] is dropped.
    async fn idle(&mut self) -> Option<(T::Reader, T::Writer, Option<Command>)> {
        loop {
            tokio::select! {
                command = self.commands.recv() => {
                    let command = command?;
                    if let Some(error) = self.st.refusal(Instant::now()) {
                        let _ = command.reply.send(Err(error));
                        continue;
                    }
                    if let Some(gone) = self.st.dead {
                        let _ = command.reply.send(Err(gone.error()));
                        continue;
                    }
                    if let Some(at) = self.st.retry_at.filter(|at| *at > Instant::now()) {
                        // Backing off after a −429: pushes still go out.
                        loop {
                            tokio::select! {
                                () = sleep_until(at) => break,
                                permit = self.pushes.reserve(), if !self.st.backlog.is_empty() => {
                                    hand_out(permit, &mut self.st.backlog);
                                }
                            }
                        }
                    }
                    if command.reply.is_closed() {
                        continue;
                    }
                    match self.transport.open(&self.dc, Some(&self.key)).await {
                        Ok(opened) => return Some((opened.reader, opened.writer, Some(command))),
                        Err(e) => {
                            tracing::debug!(dc = self.st.dc_id, error = %e, "mtproto connect failed");
                            self.st.back_off(Instant::now());
                            let _ = command.reply.send(Err(SenderError::NotSent));
                            while let Ok(queued) = self.commands.try_recv() {
                                let _ = queued.reply.send(Err(SenderError::NotSent));
                            }
                        }
                    }
                }
                permit = self.pushes.reserve(), if !self.st.backlog.is_empty() => {
                    hand_out(permit, &mut self.st.backlog);
                }
            }
        }
    }

    /// Runs one connection until it ends. `true` once the [`Sender`] is
    /// dropped.
    async fn serve(
        &mut self,
        mut reader: T::Reader,
        mut writer: T::Writer,
        first: Option<Command>,
    ) -> bool {
        let st = &mut self.st;
        let mut conn = Conn::new(st.tuning.ping_interval);
        if let Some(command) = first {
            conn.enqueue_request(command);
        }
        let cause = loop {
            if writer.is_flushed() && conn.build(st) {
                writer.queue(&conn.packet);
            }
            let reading = st.backlog.is_empty();
            let mut timer = conn
                .pong_deadline
                .map_or(conn.next_ping, |d| d.min(conn.next_ping));
            if let Init::Pending { deadline } = conn.init {
                timer = timer.min(deadline);
            }
            tokio::select! {
                packet = reader.recv(), if reading => match packet {
                    Ok(packet) => {
                        if let Err(cause) = conn.on_packet(st, packet) {
                            break cause;
                        }
                    }
                    Err(e) => break Cause::Transport(e),
                },
                flushed = writer.flush(), if !writer.is_flushed() => match flushed {
                    Ok(()) => conn.mark_written(),
                    Err(e) => break Cause::Transport(e),
                },
                permit = self.pushes.reserve(), if !reading => {
                    hand_out(permit, &mut st.backlog);
                }
                command = self.commands.recv() => match command {
                    Some(command) => conn.enqueue_request(command),
                    None => break Cause::Shutdown,
                },
                () = sleep_until(timer) => {
                    if let Err(cause) = conn.on_timer(st, Instant::now()) {
                        break cause;
                    }
                }
            }
        };
        let gone = match &cause {
            Cause::Transport(TransportError::Status(404)) => {
                Some(if st.logged_in.load(Ordering::Relaxed) {
                    KeyGone::Revoked
                } else {
                    KeyGone::Unknown
                })
            }
            _ => None,
        };
        match gone {
            Some(_) => tracing::warn!(
                dc = st.dc_id,
                "mtproto transport -404: the auth key is unknown"
            ),
            None => tracing::debug!(dc = st.dc_id, cause = %cause, "mtproto connection ended"),
        }
        st.dead = st.dead.or(gone);
        let init_done = conn.init == Init::Done;
        st.learn(&cause, init_done, Instant::now());
        // A connection that never finished its init call never had pushes
        // to lose; a new session's own `new_session_created` signals it.
        if gone.is_none() && init_done && !matches!(cause, Cause::Shutdown) {
            st.connection_ended();
        }
        conn.end(writer.is_flushed(), gone, &cause);
        matches!(cause, Cause::Shutdown)
    }
}

/// Moves one backlog entry into the pushes channel; drops the backlog when
/// nobody receives pushes.
fn hand_out(
    permit: Result<mpsc::Permit<'_, Bytes>, mpsc::error::SendError<()>>,
    backlog: &mut VecDeque<Bytes>,
) {
    match permit {
        Ok(permit) => {
            if let Some(push) = backlog.pop_front() {
                permit.send(push);
            }
        }
        Err(_) => backlog.clear(),
    }
}
