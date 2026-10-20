//! The message layer: msg_ids and the clock, seqnos, containers, acks,
//! salts, resends and the answers matched to requests.

use super::*;

/// msg_ids from the local clock plus Telegram's offset, strictly rising.
pub(super) struct Clock {
    pub(super) offset: i32,
    pub(super) last: i64,
}

pub(super) fn now_unix() -> (i64, u32) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    (
        i64::try_from(now.as_secs()).unwrap_or(0),
        now.subsec_nanos(),
    )
}

impl Clock {
    fn next(&mut self) -> i64 {
        let (secs, nanos) = now_unix();
        let mut id = ((secs + i64::from(self.offset)) << 32) | (i64::from(nanos) << 2);
        if id <= self.last {
            id = self.last + 4;
        }
        self.last = id;
        id
    }

    /// Takes Telegram's time from one of its msg_ids. A new offset also
    /// drops the floor: ids made with a wrong offset must not hold the next
    /// ones in the future (Telethon's `update_time_offset`).
    fn correct(&mut self, server_msg_id: i64) {
        let (secs, _) = now_unix();
        let offset = i32::try_from((server_msg_id >> 32) - secs).unwrap_or(self.offset);
        if offset != self.offset {
            self.offset = offset;
            self.last = 0;
        }
    }
}

impl Conn {
    pub(super) fn new(ping_interval: Duration) -> Self {
        Self {
            session_id: rand::random(),
            seq: 0,
            init: Init::Needed,
            acks: Vec::new(),
            sent_acks: VecDeque::new(),
            pending: Vec::new(),
            packet: Vec::new(),
            picks: Vec::new(),
            next_ping: Instant::now() + ping_interval,
            pong_deadline: None,
        }
    }

    fn push_pending(&mut self, what: What) {
        self.pending.push(Pending {
            what,
            slot: Slot::Queued,
            resent: false,
        });
    }

    pub(super) fn enqueue_request(&mut self, command: Command) {
        self.push_pending(What::Request {
            body: command.body,
            reply: command.reply,
        });
    }

    /// grammers `Encrypted::get_seq_no`.
    fn seq_no(&mut self, content_related: bool) -> i32 {
        if content_related {
            self.seq += 2;
            self.seq - 1
        } else {
            self.seq
        }
    }

    /// Builds the next packet into `self.packet`: owed acks, then queued
    /// messages in order, the init call first on a fresh connection.
    /// `false` when there is nothing to send.
    pub(super) fn build(&mut self, st: &mut Shared) -> bool {
        // A caller that gave up before its request was written never has
        // it written.
        self.pending.retain(|p| {
            !matches!((&p.slot, &p.what), (Slot::Queued, What::Request { reply, .. }) if reply.is_closed())
        });
        if self.init == Init::Needed {
            self.pending.insert(
                0,
                Pending {
                    what: What::Init,
                    slot: Slot::Queued,
                    resent: false,
                },
            );
            self.init = Init::Pending {
                deadline: Instant::now() + st.tuning.init_timeout,
            };
        }
        let ack_count = self.acks.len().min(MAX_ACKS_PER_MESSAGE);
        let ack_len = 4 + 4 + 4 + 8 * ack_count;
        let mut count = usize::from(ack_count > 0);
        let mut size = if ack_count > 0 {
            MESSAGE_HEADER + ack_len
        } else {
            0
        };
        self.picks.clear();
        for (i, p) in self.pending.iter().enumerate() {
            if p.slot != Slot::Queued {
                continue;
            }
            let len = match &p.what {
                What::Ping { body } => body.len(),
                What::Init => st.init_call.len(),
                // Requests wait for the init call's answer.
                What::Request { .. } if self.init != Init::Done => continue,
                What::Request { body, .. } => st.wrapper_len() + body.len(),
            };
            if count == MAX_CONTAINER_MESSAGES
                || size + MESSAGE_HEADER + len > MAX_CONTAINER_PAYLOAD
            {
                break;
            }
            count += 1;
            size += MESSAGE_HEADER + len;
            self.picks.push(i);
        }
        if count == 0 {
            return false;
        }

        let mut packet = std::mem::take(&mut self.packet);
        packet.clear();
        packet.extend_from_slice(&st.salt.to_le_bytes());
        packet.extend_from_slice(&self.session_id.to_le_bytes());
        let container_at = packet.len();
        if count > 1 {
            packet.extend_from_slice(&[0; CONTAINER_HEADER]);
        }
        let mut acked = None;
        if ack_count > 0 {
            let msg_id = st.clock.next();
            let seq = self.seq_no(false);
            let ids: Vec<i64> = self.acks.drain(..ack_count).collect();
            write_header(&mut packet, msg_id, seq, ack_len);
            packet.extend_from_slice(&MSGS_ACK.to_le_bytes());
            packet.extend_from_slice(&VECTOR.to_le_bytes());
            packet.extend_from_slice(&len_i32(ack_count).to_le_bytes());
            for id in &ids {
                packet.extend_from_slice(&id.to_le_bytes());
            }
            acked = Some((msg_id, ids));
        }
        let picks = std::mem::take(&mut self.picks);
        for &i in &picks {
            let msg_id = st.clock.next();
            let seq = self.seq_no(true);
            let p = &mut self.pending[i];
            match &p.what {
                What::Ping { body } => {
                    write_header(&mut packet, msg_id, seq, body.len());
                    packet.extend_from_slice(body);
                }
                What::Init => {
                    write_header(&mut packet, msg_id, seq, st.init_call.len());
                    packet.extend_from_slice(&st.init_call);
                }
                What::Request { body, .. } => {
                    write_header(&mut packet, msg_id, seq, st.wrapper_len() + body.len());
                    if st.without_updates {
                        packet.extend_from_slice(&WITHOUT_UPDATES);
                    }
                    packet.extend_from_slice(body);
                }
            }
            p.slot = Slot::InFlight {
                msg_id,
                container: msg_id,
            };
        }
        let mut container = acked.as_ref().map_or(0, |(id, _)| *id);
        if count > 1 {
            container = st.clock.next();
            let seq = self.seq_no(false);
            let inner = packet.len() - container_at - MESSAGE_HEADER;
            let header = &mut packet[container_at..container_at + CONTAINER_HEADER];
            header[..8].copy_from_slice(&container.to_le_bytes());
            header[8..12].copy_from_slice(&seq.to_le_bytes());
            header[12..16].copy_from_slice(&len_i32(inner).to_le_bytes());
            header[16..20].copy_from_slice(&MSG_CONTAINER.to_le_bytes());
            header[20..].copy_from_slice(&len_i32(count).to_le_bytes());
            for &i in &picks {
                if let Slot::InFlight { msg_id, .. } = self.pending[i].slot {
                    self.pending[i].slot = Slot::InFlight { msg_id, container };
                }
            }
        }
        if let Some((msg_id, ids)) = acked {
            if self.sent_acks.len() == SENT_ACKS_KEPT {
                self.sent_acks.pop_front();
            }
            self.sent_acks.push_back((msg_id, container, ids));
        }
        self.packet = packet;
        self.picks = picks;
        true
    }

    pub(super) fn mark_written(&mut self) {
        for p in &mut self.pending {
            if let Slot::InFlight { msg_id, container } = p.slot {
                p.slot = Slot::Written { msg_id, container };
            }
        }
    }

    /// An incoming packet's plaintext.
    pub(super) fn on_packet(&mut self, st: &mut Shared, packet: Bytes) -> Result<(), Cause> {
        let session_id = i64_at(&packet, 8).ok_or(Cause::Protocol("short packet"))?;
        if session_id != self.session_id {
            return Err(Cause::Protocol("another session's packet"));
        }
        let (msg_id, seqno, body, _) =
            message_at(&packet, PACKET_HEADER).ok_or(Cause::Protocol("message header"))?;
        self.on_message(st, msg_id, seqno, body, 0)
    }

    /// One message; `depth` counts the containers and `gzip_packed` around
    /// it (MTProto nests neither, so more than two is refused).
    fn on_message(
        &mut self,
        st: &mut Shared,
        msg_id: i64,
        seqno: i32,
        body: Bytes,
        depth: u8,
    ) -> Result<(), Cause> {
        if depth > 2 {
            return Err(Cause::Protocol("nested containers"));
        }
        if seqno & 1 == 1 {
            self.acks.push(msg_id);
        }
        let bad = || Cause::Protocol("service message");
        match u32_at(&body, 0).ok_or(bad())? {
            RPC_RESULT => {
                let req_msg_id = i64_at(&body, 4).ok_or(bad())?;
                let outcome = rpc_outcome(body.slice(12..), true);
                let updates = match &outcome {
                    Ok(RpcOutcome::Result(result))
                        if u32_at(result, 0).is_some_and(|id| UPDATES.contains(&id)) =>
                    {
                        Some(result.clone())
                    }
                    _ => None,
                };
                // A request's own updates also reach the update manager,
                // in wire order with the pushes.
                if self.resolve(req_msg_id, outcome)?
                    && let Some(updates) = updates
                {
                    st.push(updates);
                }
            }
            MSG_CONTAINER => {
                let count = u32_at(&body, 4).ok_or(bad())?;
                let mut at = 8;
                for _ in 0..count {
                    let (id, seq, inner, next) = message_at(&body, at).ok_or(bad())?;
                    self.on_message(st, id, seq, inner, depth + 1)?;
                    at = next;
                }
            }
            GZIP_PACKED => {
                let inner = gunzip(&body[4..]).ok_or(Cause::Protocol("gzip_packed"))?;
                // Already acknowledged above, as the outer message.
                self.on_message(st, msg_id, 0, inner, depth + 1)?;
            }
            BAD_SERVER_SALT => {
                let bad_msg_id = i64_at(&body, 4).ok_or(bad())?;
                let code = i32_at(&body, 16).ok_or(bad())?;
                let salt = i64_at(&body, 20).ok_or(bad())?;
                st.salt = salt;
                self.on_bad_msg(st, msg_id, bad_msg_id, code)?;
            }
            BAD_MSG_NOTIFICATION => {
                let bad_msg_id = i64_at(&body, 4).ok_or(bad())?;
                let code = i32_at(&body, 16).ok_or(bad())?;
                self.on_bad_msg(st, msg_id, bad_msg_id, code)?;
            }
            NEW_SESSION_CREATED => {
                let first_msg_id = i64_at(&body, 4).ok_or(bad())?;
                st.salt = i64_at(&body, 20).ok_or(bad())?;
                self.orphan_before(first_msg_id);
                st.session_lost();
            }
            PONG => {
                let ping_msg_id = i64_at(&body, 4).ok_or(bad())?;
                self.resolve(ping_msg_id, Err(SenderError::NoAnswer))?;
            }
            // grammers `handle_detailed_info`: acknowledge the answer.
            MSG_DETAILED_INFO => self.acks.push(i64_at(&body, 12).ok_or(bad())?),
            MSG_NEW_DETAILED_INFO => self.acks.push(i64_at(&body, 4).ok_or(bad())?),
            MSGS_ACK | FUTURE_SALTS | MSGS_STATE_REQ | MSGS_STATE_INFO | MSGS_ALL_INFO
            | MSG_RESEND_REQ | MSG_RESEND_ANS_REQ | DESTROY_SESSION_OK | DESTROY_SESSION_NONE
            | HTTP_WAIT | MSG_COPY => {}
            _ => st.backlog.push_back(body),
        }
        Ok(())
    }

    /// An answer for the message `msg_id`. `true` when it was a caller's
    /// request.
    fn resolve(
        &mut self,
        msg_id: i64,
        outcome: Result<RpcOutcome, SenderError>,
    ) -> Result<bool, Cause> {
        let Some(i) = self
            .pending
            .iter()
            .position(|p| p.slot.msg_id() == Some(msg_id))
        else {
            return Ok(false);
        };
        let p = self.pending.remove(i);
        match p.what {
            What::Ping { .. } => self.pong_deadline = None,
            What::Init => match outcome {
                Ok(RpcOutcome::Result(_)) => self.init = Init::Done,
                Ok(RpcOutcome::Error { code, message }) => {
                    return Err(Cause::InitRefused { code, message });
                }
                Err(_) => return Err(Cause::Protocol("initConnection answer")),
            },
            What::Request { reply, .. } => {
                let _ = reply.send(outcome);
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Telegram ignored the message `bad_msg_id` (a request or a container).
    fn on_bad_msg(
        &mut self,
        st: &mut Shared,
        server_msg_id: i64,
        bad_msg_id: i64,
        code: i32,
    ) -> Result<(), Cause> {
        // grammers `handle_bad_notification` for the clock. Codes 32 and 33
        // (msg_seqno too low or too high) are not guessed at as grammers
        // does: the named request fails, and the connection ends, so the
        // next one is a fresh session with seqno from 0.
        if matches!(code, 16 | 17) {
            st.clock.correct(server_msg_id);
        }
        let retry = matches!(code, 16 | 17 | 48);
        let mut kept = VecDeque::with_capacity(self.sent_acks.len());
        for (ack_id, container, ids) in self.sent_acks.drain(..) {
            if ack_id == bad_msg_id || container == bad_msg_id {
                self.acks.extend(ids);
            } else {
                kept.push_back((ack_id, container, ids));
            }
        }
        self.sent_acks = kept;
        let mut i = 0;
        while i < self.pending.len() {
            let p = &mut self.pending[i];
            if !p.slot.matches(bad_msg_id) {
                i += 1;
                continue;
            }
            if retry && !p.resent {
                p.slot = Slot::Queued;
                p.resent = true;
                i += 1;
                continue;
            }
            let p = self.pending.remove(i);
            match p.what {
                What::Ping { .. } => self.pong_deadline = None,
                What::Init => return Err(Cause::Protocol("initConnection ignored")),
                What::Request { reply, .. } => {
                    let _ = reply.send(Err(SenderError::BadMessage { code }));
                }
            }
        }
        if matches!(code, 32 | 33) {
            return Err(Cause::Protocol("msg_seqno refused"));
        }
        Ok(())
    }

    /// Telegram's new session starts at the client's message
    /// `first_msg_id`: whatever went out in an earlier packet reached the
    /// old session, whose answers are lost (TDLib's
    /// `on_new_session_created`). Those requests get
    /// [`SenderError::NoAnswer`], an orphaned init call is made again, and
    /// orphaned pings are dropped.
    fn orphan_before(&mut self, first_msg_id: i64) {
        let floor = self
            .pending
            .iter()
            .find(|p| p.slot.msg_id() == Some(first_msg_id))
            .and_then(|p| p.slot.container())
            .unwrap_or(first_msg_id);
        let mut i = 0;
        while i < self.pending.len() {
            if self.pending[i].slot.container().is_none_or(|c| c >= floor) {
                i += 1;
                continue;
            }
            match self.pending.remove(i).what {
                What::Ping { .. } => {}
                What::Init => self.init = Init::Needed,
                What::Request { reply, .. } => {
                    let _ = reply.send(Err(SenderError::NoAnswer));
                }
            }
        }
        if !self
            .pending
            .iter()
            .any(|p| matches!(p.what, What::Ping { .. }))
        {
            self.pong_deadline = None;
        }
    }

    pub(super) fn on_timer(&mut self, st: &Shared, now: Instant) -> Result<(), Cause> {
        if let Init::Pending { deadline } = self.init
            && now >= deadline
        {
            // As with the pong: the answer may sit behind frames the full
            // pushes channel keeps unread.
            if st.backlog.is_empty() {
                return Err(Cause::InitTimeout);
            }
            self.init = Init::Pending {
                deadline: now + st.tuning.init_timeout,
            };
        }
        if let Some(deadline) = self.pong_deadline
            && now >= deadline
        {
            // The pong may sit behind frames the full pushes channel keeps
            // unread.
            if st.backlog.is_empty() {
                return Err(Cause::PongTimeout);
            }
            self.pong_deadline = Some(now + st.tuning.pong_timeout);
        }
        if now >= self.next_ping {
            let ping = tl::functions::PingDelayDisconnect {
                ping_id: rand::random(),
                disconnect_delay: NO_PING_DISCONNECT,
            };
            self.push_pending(What::Ping {
                body: ping.to_bytes().into(),
            });
            self.next_ping = now + st.tuning.ping_interval;
            self.pong_deadline
                .get_or_insert(now + st.tuning.pong_timeout);
        }
        Ok(())
    }

    /// The connection is gone: every request still here gets its outcome.
    pub(super) fn end(self, flushed: bool, gone: Option<KeyGone>, cause: &Cause) {
        for p in self.pending {
            let What::Request { reply, .. } = p.what else {
                continue;
            };
            let error = match (gone, p.slot, cause) {
                (Some(gone), _, _) => gone.error(),
                (None, Slot::Queued, Cause::InitRefused { code, message }) => {
                    SenderError::InitRefused {
                        code: *code,
                        message: message.clone(),
                    }
                }
                (None, Slot::Queued, _) => SenderError::NotSent,
                // Telegram cannot have a frame the connection never took whole.
                (None, Slot::InFlight { .. }, _) if !flushed => SenderError::NotSent,
                (None, Slot::InFlight { .. } | Slot::Written { .. }, _) => SenderError::NoAnswer,
            };
            let _ = reply.send(Err(error));
        }
    }
}

/// An `rpc_result`'s `result`: Telegram's error, or its bytes.
fn rpc_outcome(result: Bytes, unpack: bool) -> Result<RpcOutcome, SenderError> {
    match u32_at(&result, 0) {
        Some(RPC_ERROR) => {
            let code = i32_at(&result, 4).ok_or(SenderError::NoAnswer)?;
            let message = tl_bytes(&result[8..]).ok_or(SenderError::NoAnswer)?;
            Ok(RpcOutcome::Error {
                code,
                message: String::from_utf8_lossy(message).into_owned(),
            })
        }
        Some(GZIP_PACKED) if unpack => {
            let inner = gunzip(&result[4..]).ok_or(SenderError::NoAnswer)?;
            rpc_outcome(inner, false)
        }
        _ => Ok(RpcOutcome::Result(result)),
    }
}

fn write_header(packet: &mut Vec<u8>, msg_id: i64, seqno: i32, len: usize) {
    packet.extend_from_slice(&msg_id.to_le_bytes());
    packet.extend_from_slice(&seqno.to_le_bytes());
    packet.extend_from_slice(&len_i32(len).to_le_bytes());
}

/// Lengths here are bounded by [`MAX_CONTAINER_PAYLOAD`].
fn len_i32(len: usize) -> i32 {
    i32::try_from(len).unwrap_or(i32::MAX)
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn i32_at(b: &[u8], at: usize) -> Option<i32> {
    Some(i32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn i64_at(b: &[u8], at: usize) -> Option<i64> {
    Some(i64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// The message at `at`: `(msg_id, seqno, body, end)`.
fn message_at(b: &Bytes, at: usize) -> Option<(i64, i32, Bytes, usize)> {
    let msg_id = i64_at(b, at)?;
    let seqno = i32_at(b, at + 8)?;
    let len = usize::try_from(i32_at(b, at + 12)?).ok()?;
    let start = at + MESSAGE_HEADER;
    let end = start.checked_add(len).filter(|end| *end <= b.len())?;
    Some((msg_id, seqno, b.slice(start..end), end))
}

/// A TL `bytes`/`string` value's content.
fn tl_bytes(b: &[u8]) -> Option<&[u8]> {
    let (len, start) = match *b.first()? {
        254 => (
            usize::from(*b.get(1)?) | usize::from(*b.get(2)?) << 8 | usize::from(*b.get(3)?) << 16,
            4,
        ),
        255 => return None,
        short => (usize::from(short), 1),
    };
    b.get(start..start + len)
}

/// `gzip_packed`'s `packed_data`, unpacked.
fn gunzip(packed: &[u8]) -> Option<Bytes> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(tl_bytes(packed)?)
        .take(MAX_UNPACKED)
        .read_to_end(&mut out)
        .ok()?;
    (u64::try_from(out.len()).ok()? < MAX_UNPACKED).then(|| out.into())
}
