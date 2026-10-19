//! Hand-built TL bytes and a scripted [`Dial`], for tests that drive a
//! login without Telegram: this crate's and the admin API's (feature
//! `testkit`). Every reply is written byte by byte, independent of
//! grammers' serializers.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use bytes::Bytes;
use grammers_tl_types::{self as tl, Identifiable};
use tokio::sync::mpsc;

use crate::login::{Dial, Link};
use crate::{AuthKey, Pushes, RpcOutcome, SenderError};

/// A TL byte writer.
#[derive(Default)]
pub struct Tl(Vec<u8>);

impl Tl {
    #[must_use]
    pub fn ctor(id: u32) -> Self {
        Self(id.to_le_bytes().to_vec())
    }
    #[must_use]
    pub fn int(mut self, v: i32) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    #[must_use]
    pub fn uint(mut self, v: u32) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    #[must_use]
    pub fn long(mut self, v: i64) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    /// TL `bytes`: a 1- or 4-byte length, the data, padding to 4.
    #[must_use]
    pub fn bytes(mut self, b: &[u8]) -> Self {
        let header = if b.len() < 254 {
            self.0.push(u8::try_from(b.len()).unwrap_or(0));
            1
        } else {
            self.0.push(254);
            let len = u32::try_from(b.len()).unwrap_or(0).to_le_bytes();
            self.0.extend_from_slice(&len[..3]);
            4
        };
        self.0.extend_from_slice(b);
        let pad = (4 - (header + b.len()) % 4) % 4;
        self.0.extend(std::iter::repeat_n(0u8, pad));
        self
    }
    #[must_use]
    pub fn string(self, s: &str) -> Self {
        self.bytes(s.as_bytes())
    }
    #[must_use]
    pub fn raw(mut self, other: &[u8]) -> Self {
        self.0.extend_from_slice(other);
        self
    }
    #[must_use]
    pub fn done(self) -> Vec<u8> {
        self.0
    }
}

/// `user#b1b8cc83` with `self`, `access_hash` and, when given, `phone`.
#[must_use]
pub fn user(id: i64, phone: Option<&str>) -> Vec<u8> {
    let mut flags = (1u32 << 10) | 1;
    if phone.is_some() {
        flags |= 1 << 4;
    }
    let t = Tl::ctor(0xb1b8_cc83).uint(flags).uint(0).long(id).long(-77);
    match phone {
        Some(p) => t.string(p).done(),
        None => t.done(),
    }
}

/// `auth.authorization#2ea2c0d4` with no optional field.
#[must_use]
pub fn authorization(user_id: i64, phone: Option<&str>) -> Vec<u8> {
    Tl::ctor(0x2ea2_c0d4)
        .uint(0)
        .raw(&user(user_id, phone))
        .done()
}

/// `auth.authorizationSignUpRequired#44747e9a`.
#[must_use]
pub fn sign_up_required() -> Vec<u8> {
    Tl::ctor(0x4474_7e9a).uint(0).done()
}

/// `auth.loginToken#629f1980`.
#[must_use]
pub fn login_token(token: &[u8], expires: i32) -> Vec<u8> {
    Tl::ctor(0x629f_1980).int(expires).bytes(token).done()
}

/// `auth.loginTokenMigrateTo#68e9916`.
#[must_use]
pub fn login_token_migrate(dc: i32, token: &[u8]) -> Vec<u8> {
    Tl::ctor(0x068e_9916).int(dc).bytes(token).done()
}

/// `auth.loginTokenSuccess#390d5c5e`.
#[must_use]
pub fn login_token_success(user_id: i64, phone: Option<&str>) -> Vec<u8> {
    Tl::ctor(0x390d_5c5e)
        .raw(&authorization(user_id, phone))
        .done()
}

/// `auth.loggedOut#c3a2835f` with no field.
#[must_use]
pub fn logged_out() -> Vec<u8> {
    Tl::ctor(0xc3a2_835f).uint(0).done()
}

/// `vector<User>` with one user: the answer to `users.getUsers`.
#[must_use]
pub fn users(id: i64, phone: Option<&str>) -> Vec<u8> {
    Tl::ctor(0x1cb5_c415).int(1).raw(&user(id, phone)).done()
}

/// `auth.sentCode#5e002502`, `timeout` 60, hash `"hash"`.
#[must_use]
pub fn sent_code(code_type: &[u8]) -> Vec<u8> {
    Tl::ctor(0x5e00_2502)
        .uint(1 << 2)
        .raw(code_type)
        .string("hash")
        .int(60)
        .done()
}

/// `auth.sentCodeTypeSms#c000bba2`, length 5.
#[must_use]
pub fn sms() -> Vec<u8> {
    Tl::ctor(0xc000_bba2).int(5).done()
}

/// `account.password#957b50fb`: has_password, hint `pet`, and a `p` that
/// fails grammers' safe-prime check.
#[must_use]
pub fn account_password() -> Vec<u8> {
    account_password_with(&[0xff; 256], &[5; 256], &[6; 32])
}

/// Telegram's 2048-bit SRP prime (g = 3).
const SRP_P: &str = "c71caeb9c6b1c9048e6c522f70f13f73980d40238e3e21c14934d037563d930f\
    48198a0aa7c14058229493d22530f4dbfa336f6e0ac925139543aed44cce7c37\
    20fd51f69458705ac68cd4fe6b6b13abdc9746512969328454f18faf8c595f64\
    2477fe96bb2a941d5bcd1d4ac8cc49880708fa9b378e3c4f3a9060bee67cf9a4\
    a4a695811051907e162753b56b0f6b410dba74d8a84b2a14b3144e0ef1284754\
    fd17ed950d5965b4b9dd46582db1178d169c6bc465b0d6ff9ca3928fef5b9ae4\
    e418fc15e83ebea0f87fa9ff5eed70050ded2849f47bf959d956850ce929851f\
    0d8115f635b105ee2e4e15d04b2454bf6f4fadf034b10403119cd8e3b92fcc5b";

/// Telegram's 2048-bit SRP prime, big-endian.
#[must_use]
pub fn srp_prime() -> Vec<u8> {
    let hex: Vec<u8> = SRP_P.bytes().filter(u8::is_ascii_hexdigit).collect();
    hex.chunks(2)
        .map(|pair| {
            let digit = |c: u8| {
                char::from(c)
                    .to_digit(16)
                    .and_then(|d| u8::try_from(d).ok())
            };
            digit(pair[0]).unwrap_or(0) * 16 + digit(pair[1]).unwrap_or(0)
        })
        .collect()
}

/// As [`account_password`], with Telegram's real SRP prime, so SRP runs
/// end to end.
#[must_use]
pub fn account_password_srp() -> Vec<u8> {
    account_password_with(&srp_prime(), &[5; 256], &[6; 32])
}

/// As [`account_password_srp`], with the given `srp_B` and `secure_random`.
#[must_use]
pub fn account_password_sized(srp_b: &[u8], secure_random: &[u8]) -> Vec<u8> {
    account_password_with(&srp_prime(), srp_b, secure_random)
}

fn account_password_with(p: &[u8], srp_b: &[u8], secure_random: &[u8]) -> Vec<u8> {
    let algo = Tl::ctor(0x3a91_2d4a)
        .bytes(&[1; 8])
        .bytes(&[2; 16])
        .int(3)
        .bytes(p)
        .done();
    Tl::ctor(0x957b_50fb)
        .uint((1 << 2) | (1 << 3))
        .raw(&algo)
        .bytes(srp_b)
        .long(42)
        .string("pet")
        .raw(&Tl::ctor(0xd45a_b096).done())
        .raw(&Tl::ctor(0x004a_8537).done())
        .bytes(secure_random)
        .done()
}

/// The push Telegram sends when the QR is scanned:
/// `updateShort { updateLoginToken }`.
#[must_use]
pub fn update_login_token() -> Vec<u8> {
    Tl::ctor(0x78d4_dec1)
        .raw(&Tl::ctor(0x564f_e691).done())
        .int(1_800_000_000)
        .done()
}

/// The sender's synthetic `updatesTooLong`.
#[must_use]
pub fn updates_too_long() -> Vec<u8> {
    Tl::ctor(0xe317_af7e).done()
}

/// Constructor ids of the requests a login or an account sends.
pub mod ids {
    use super::{Identifiable, tl};

    pub const EXPORT_LOGIN_TOKEN: u32 = tl::functions::auth::ExportLoginToken::CONSTRUCTOR_ID;
    pub const IMPORT_LOGIN_TOKEN: u32 = tl::functions::auth::ImportLoginToken::CONSTRUCTOR_ID;
    pub const GET_PASSWORD: u32 = tl::functions::account::GetPassword::CONSTRUCTOR_ID;
    pub const CHECK_PASSWORD: u32 = tl::functions::auth::CheckPassword::CONSTRUCTOR_ID;
    pub const SEND_CODE: u32 = tl::functions::auth::SendCode::CONSTRUCTOR_ID;
    pub const SIGN_IN: u32 = tl::functions::auth::SignIn::CONSTRUCTOR_ID;
    pub const LOG_OUT: u32 = tl::functions::auth::LogOut::CONSTRUCTOR_ID;
    pub const GET_USERS: u32 = tl::functions::users::GetUsers::CONSTRUCTOR_ID;
    pub const GET_STATE: u32 = tl::functions::updates::GetState::CONSTRUCTOR_ID;
    pub const GET_DIFFERENCE: u32 = tl::functions::updates::GetDifference::CONSTRUCTOR_ID;
    pub const GET_CHANNEL_DIFFERENCE: u32 =
        tl::functions::updates::GetChannelDifference::CONSTRUCTOR_ID;
    pub const EXPORT_AUTHORIZATION: u32 = tl::functions::auth::ExportAuthorization::CONSTRUCTOR_ID;
    pub const IMPORT_AUTHORIZATION: u32 = tl::functions::auth::ImportAuthorization::CONSTRUCTOR_ID;
}

/// TL bytes from TL-JSON (layer 230); panics on a bad object.
#[must_use]
pub fn tl_bytes(json: &str) -> Vec<u8> {
    let mut out = Vec::new();
    if let Err(e) = crate::tl_json::from_json(json, &mut out) {
        panic!("{e}: {json}");
    }
    out
}

/// `updates.state`, no unread count.
#[must_use]
pub fn state(pts: i32, qts: i32, date: i32, seq: i32) -> Vec<u8> {
    tl_bytes(&format!(
        r#"{{"_":"updates.state","pts":{pts},"qts":{qts},"date":{date},"seq":{seq},"unread_count":0}}"#
    ))
}

/// `updates.differenceEmpty`.
#[must_use]
pub fn difference_empty(date: i32, seq: i32) -> Vec<u8> {
    tl_bytes(&format!(
        r#"{{"_":"updates.differenceEmpty","date":{date},"seq":{seq}}}"#
    ))
}

/// A text `message` from user `from` in the private chat with `from`.
#[must_use]
pub fn private_message(id: i32, from: i64, text: &str) -> String {
    let text = serde_json::to_string(text).unwrap_or_default();
    format!(
        r#"{{"_":"message","id":{id},"peer_id":{{"_":"peerUser","user_id":"{from}"}},"date":1,"message":{text}}}"#
    )
}

/// `updates.difference` with `messages` (TL-JSON `message` objects), no
/// other update, ending at `pts`.
#[must_use]
pub fn difference(messages: &[String], pts: i32, date: i32, seq: i32) -> Vec<u8> {
    tl_bytes(&format!(
        r#"{{"_":"updates.difference","new_messages":[{}],"new_encrypted_messages":[],"other_updates":[],"chats":[],"users":[],"state":{{"_":"updates.state","pts":{pts},"qts":0,"date":{date},"seq":{seq},"unread_count":0}}}}"#,
        messages.join(",")
    ))
}

/// `updates` holding `updates` (TL-JSON `Update` objects), no users or
/// chats, `seq` 0 (unordered).
#[must_use]
pub fn updates(updates: &[String]) -> Vec<u8> {
    tl_bytes(&format!(
        r#"{{"_":"updates","updates":[{}],"users":[],"chats":[],"date":1,"seq":0}}"#,
        updates.join(",")
    ))
}

/// One scripted answer.
#[derive(Clone, Debug)]
pub enum Reply {
    Ok(Vec<u8>),
    Rpc(i32, &'static str),
    /// Written, no answer ([`SenderError::NoAnswer`]).
    NoAnswer,
    /// Never written ([`SenderError::NotSent`]).
    NotSent,
    /// Written, and no answer ever comes.
    Hang,
    /// The reply, after a while (Tokio time).
    Later(std::time::Duration, Box<Reply>),
    /// Transport −404 on a logged-in key.
    KeyRevoked,
    /// Transport −404 on a key without a self user.
    KeyUnknown,
    /// Telegram refused the connection's init call.
    InitRefused(i32, &'static str),
}

impl Reply {
    /// What the request gets: waits out [`Reply::Later`], and for ever on
    /// [`Reply::Hang`].
    pub async fn outcome(self) -> Result<RpcOutcome, SenderError> {
        let mut reply = self;
        while let Reply::Later(wait, inner) = reply {
            tokio::time::sleep(wait).await;
            reply = *inner;
        }
        match reply {
            Reply::Ok(bytes) => Ok(RpcOutcome::Result(Bytes::from(bytes))),
            Reply::Rpc(code, message) => Ok(RpcOutcome::Error {
                code,
                message: message.to_owned(),
            }),
            Reply::NoAnswer => Err(SenderError::NoAnswer),
            Reply::NotSent | Reply::Later(..) => Err(SenderError::NotSent),
            Reply::Hang => std::future::pending().await,
            Reply::KeyRevoked => Err(SenderError::KeyRevoked),
            Reply::KeyUnknown => Err(SenderError::KeyUnknown),
            Reply::InitRefused(code, message) => Err(SenderError::InitRefused {
                code,
                message: message.to_owned(),
            }),
        }
    }
}

#[derive(Default)]
struct Inner {
    /// `(expected request constructor, reply)`, in order.
    script: Mutex<VecDeque<(u32, Reply)>>,
    /// `(dc, request body)` of every request.
    calls: Mutex<Vec<(i32, Vec<u8>)>>,
    /// `(dc, had a key)` of every dial.
    dials: Mutex<Vec<(i32, bool)>>,
    /// The latest connection's push channel.
    pushes: Mutex<Option<mpsc::Sender<Bytes>>>,
    marked: AtomicBool,
    keys: AtomicU8,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A [`Dial`] whose links answer from one script, shared by every
/// connection; a dial without a key makes a new key `[n; 256]`.
#[derive(Clone, Default)]
pub struct ScriptedDial(Arc<Inner>);

impl ScriptedDial {
    #[must_use]
    pub fn new(script: Vec<(u32, Reply)>) -> Self {
        let dial = Self::default();
        dial.extend(script);
        dial
    }

    /// Appends to the script.
    pub fn extend(&self, script: Vec<(u32, Reply)>) {
        lock(&self.0.script).extend(script);
    }

    /// Pushes `body` on the latest connection; false without one.
    pub fn push(&self, body: Vec<u8>) -> bool {
        lock(&self.0.pushes)
            .as_ref()
            .is_some_and(|tx| tx.try_send(Bytes::from(body)).is_ok())
    }

    /// `(dc, request constructor)` of every request so far.
    #[must_use]
    pub fn calls(&self) -> Vec<(i32, u32)> {
        lock(&self.0.calls)
            .iter()
            .map(|(dc, body)| (*dc, ctor(body)))
            .collect()
    }

    /// Every request body so far.
    #[must_use]
    pub fn bodies(&self) -> Vec<Vec<u8>> {
        lock(&self.0.calls).iter().map(|(_, b)| b.clone()).collect()
    }

    /// `(dc, had a key)` of every dial so far.
    #[must_use]
    pub fn dials(&self) -> Vec<(i32, bool)> {
        lock(&self.0.dials).clone()
    }

    /// The script is used up.
    #[must_use]
    pub fn done(&self) -> bool {
        lock(&self.0.script).is_empty()
    }

    /// A link called `mark_logged_in`.
    #[must_use]
    pub fn marked_logged_in(&self) -> bool {
        self.0.marked.load(Ordering::SeqCst)
    }
}

fn ctor(body: &[u8]) -> u32 {
    body.get(..4)
        .and_then(|b| <[u8; 4]>::try_from(b).ok())
        .map_or(0, u32::from_le_bytes)
}

impl Dial for ScriptedDial {
    type Link = ScriptedLink;

    async fn dial(
        &self,
        dc: i32,
        key: Option<AuthKey>,
        _logged_in: bool,
    ) -> Result<(ScriptedLink, Pushes, Option<AuthKey>), SenderError> {
        lock(&self.0.dials).push((dc, key.is_some()));
        let (tx, rx) = mpsc::channel(16);
        *lock(&self.0.pushes) = Some(tx);
        let new_key = match key {
            Some(_) => None,
            None => Some(AuthKey([self.0.keys.fetch_add(1, Ordering::SeqCst); 256])),
        };
        Ok((
            ScriptedLink {
                dc,
                inner: Arc::clone(&self.0),
            },
            Pushes::from_receiver(rx),
            new_key,
        ))
    }
}

/// One scripted connection.
pub struct ScriptedLink {
    dc: i32,
    inner: Arc<Inner>,
}

impl Link for ScriptedLink {
    async fn invoke(&self, body: Bytes) -> Result<RpcOutcome, SenderError> {
        let id = ctor(&body);
        lock(&self.inner.calls).push((self.dc, body.to_vec()));
        let next = lock(&self.inner.script).pop_front();
        match next {
            Some((expected, reply)) if expected == id => reply.outcome().await,
            // Off script: the request "was never sent".
            _ => Err(SenderError::NotSent),
        }
    }

    fn mark_logged_in(&self) {
        self.inner.marked.store(true, Ordering::SeqCst);
    }
}

/// One session [`FakeNetwork`] opened.
#[derive(Clone, Debug)]
pub struct Dialed {
    pub dc: i32,
    pub media: bool,
    pub had_key: bool,
    /// Tokio time.
    pub at: tokio::time::Instant,
}

/// One request a [`FakeNetwork`] session got.
#[derive(Clone, Debug)]
pub struct NetCall {
    pub dc: i32,
    pub media: bool,
    pub body: Vec<u8>,
}

impl NetCall {
    /// The request's constructor id.
    #[must_use]
    pub fn ctor(&self) -> u32 {
        ctor(&self.body)
    }
}

type Answer = Box<dyn FnMut(&NetCall) -> Reply + Send>;

#[derive(Default)]
struct Net {
    /// What the next connects give, in order: `Reply::Ok` connects,
    /// `KeyRevoked`/`KeyUnknown`/`InitRefused` fail with that error, and
    /// any other reply fails to connect. Empty: connects succeed.
    connects: Mutex<VecDeque<Reply>>,
    answer: Mutex<Option<Answer>>,
    /// The latest main session's push channel.
    pushes: Mutex<Option<mpsc::Sender<Bytes>>>,
    dials: Mutex<Vec<Dialed>>,
    calls: Mutex<Vec<NetCall>>,
}

/// A scripted Telegram for an account
/// ([`Network`](crate::account::Network)). Every session answers through
/// one handler; without one, `updates.getState` gets `state(1, 0, 1, 1)`,
/// `updates.getDifference` gets `differenceEmpty(1, 1)`, and anything
/// else `400 NOT_SCRIPTED`.
#[derive(Clone, Default)]
pub struct FakeNetwork(Arc<Net>);

impl FakeNetwork {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Answers every request with `answer`.
    pub fn answer(&self, answer: impl FnMut(&NetCall) -> Reply + Send + 'static) {
        *lock(&self.0.answer) = Some(Box::new(answer));
    }

    /// Scripts the next connects (see [`Net::connects`]).
    pub fn connects(&self, replies: Vec<Reply>) {
        lock(&self.0.connects).extend(replies);
    }

    /// Pushes `body` on the latest main session; false without one.
    pub fn push(&self, body: Vec<u8>) -> bool {
        lock(&self.0.pushes)
            .as_ref()
            .is_some_and(|tx| tx.try_send(Bytes::from(body)).is_ok())
    }

    /// Ends the latest main session's pushes, as a dead connection would.
    pub fn drop_pushes(&self) {
        lock(&self.0.pushes).take();
    }

    #[must_use]
    pub fn dials(&self) -> Vec<Dialed> {
        lock(&self.0.dials).clone()
    }

    #[must_use]
    pub fn calls(&self) -> Vec<NetCall> {
        lock(&self.0.calls).clone()
    }

    /// The constructors of the requests so far.
    #[must_use]
    pub fn ctors(&self) -> Vec<u32> {
        lock(&self.0.calls).iter().map(NetCall::ctor).collect()
    }
}

impl crate::account::Network for FakeNetwork {
    fn connect(
        &self,
        dc: crate::DcOption,
        key: Option<AuthKey>,
        media: bool,
    ) -> crate::rpc::BoxFuture<'_, Result<crate::account::Connection, SenderError>> {
        Box::pin(async move {
            lock(&self.0.dials).push(Dialed {
                dc: dc.id,
                media,
                had_key: key.is_some(),
                at: tokio::time::Instant::now(),
            });
            let scripted = lock(&self.0.connects).pop_front();
            match scripted {
                None | Some(Reply::Ok(_)) => {}
                Some(reply @ (Reply::KeyRevoked | Reply::KeyUnknown | Reply::InitRefused(..))) => {
                    return reply
                        .outcome()
                        .await
                        .map(|_| unreachable!("an error reply"));
                }
                Some(_) => return Err(SenderError::Connect(crate::TransportError::Closed)),
            }
            let (tx, rx) = mpsc::channel(64);
            if !media {
                *lock(&self.0.pushes) = Some(tx);
            }
            let new_key = key
                .is_none()
                .then(|| AuthKey([u8::try_from(dc.id).unwrap_or(0); 256]));
            Ok(crate::account::Connection {
                session: Arc::new(FakeSession {
                    net: Arc::clone(&self.0),
                    dc: dc.id,
                    media,
                }),
                pushes: Pushes::from_receiver(rx),
                new_key,
            })
        })
    }
}

/// One session of a [`FakeNetwork`].
struct FakeSession {
    net: Arc<Net>,
    dc: i32,
    media: bool,
}

impl crate::media::Session for FakeSession {
    fn invoke(&self, body: Bytes) -> crate::rpc::BoxFuture<'_, Result<RpcOutcome, SenderError>> {
        Box::pin(async move {
            let call = NetCall {
                dc: self.dc,
                media: self.media,
                body: body.to_vec(),
            };
            lock(&self.net.calls).push(call.clone());
            let reply = match lock(&self.net.answer).as_mut() {
                Some(answer) => answer(&call),
                None => default_answer(&call),
            };
            reply.outcome().await
        })
    }
}

fn default_answer(call: &NetCall) -> Reply {
    match call.ctor() {
        ids::GET_STATE => Reply::Ok(state(1, 0, 1, 1)),
        ids::GET_DIFFERENCE => Reply::Ok(difference_empty(1, 1)),
        _ => Reply::Rpc(400, "NOT_SCRIPTED"),
    }
}
