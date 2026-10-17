//! Per-token state (update queue, webhook, files) and the injected faults.

use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use bytes::Bytes;
use parking_lot::Mutex;
use serde::Deserialize;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::{journal::Journal, webhook};

/// The bot id of a token shaped `^[0-9]+:[A-Za-z0-9_-]+$`; `None` for any
/// other token (the server's "invalid token specified").
pub(crate) fn bot_id(token: &str) -> Option<i64> {
    let (id, secret) = token.split_once(':')?;
    let id_ok = !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit());
    let secret_ok = !secret.is_empty()
        && secret
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if !(id_ok && secret_ok) {
        return None;
    }
    id.parse().ok().filter(|id| *id > 0)
}

pub(crate) struct Shared {
    bots: Mutex<HashMap<String, Arc<Bot>>>,
    faults: Mutex<Vec<Fault>>,
    pub journal: Journal,
    pub client: reqwest::Client,
    pub cancel: CancellationToken,
    next_file: AtomicU64,
}

impl Shared {
    pub fn new(cancel: CancellationToken) -> reqwest::Result<Self> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            bots: Mutex::new(HashMap::new()),
            faults: Mutex::new(Vec::new()),
            journal: Journal::default(),
            client,
            cancel,
            next_file: AtomicU64::new(1),
        })
    }

    /// The bot of a valid token, created (with its webhook delivery task) on
    /// first use.
    pub fn bot(self: &Arc<Self>, token: &str, id: i64) -> Arc<Bot> {
        let mut bots = self.bots.lock();
        if let Some(bot) = bots.get(token) {
            return Arc::clone(bot);
        }
        let bot = Arc::new(Bot::new(token, id));
        bots.insert(token.to_owned(), Arc::clone(&bot));
        tokio::spawn(webhook::deliver(Arc::clone(self), Arc::clone(&bot)));
        bot
    }

    pub fn next_file(&self) -> u64 {
        self.next_file.fetch_add(1, Ordering::Relaxed)
    }

    pub fn add_fault(&self, fault: Fault) {
        self.faults.lock().push(fault);
    }

    /// Consume one use of the first fault matching `token` and `method`.
    pub fn take_fault(&self, token: &str, method: &str) -> Option<Fault> {
        let mut faults = self.faults.lock();
        let index = faults
            .iter()
            .position(|f| f.token == token && f.method.eq_ignore_ascii_case(method))?;
        let fault = faults[index].clone();
        faults[index].count -= 1;
        if faults[index].count == 0 {
            faults.remove(index);
        }
        Some(fault)
    }
}

/// `POST /fake/faults`: the next `count` calls of `method` by `token` answer
/// `status`, after `delay_ms`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Fault {
    pub token: String,
    pub method: String,
    pub status: u16,
    pub retry_after: Option<u64>,
    pub count: u32,
    pub delay_ms: Option<u64>,
    /// Overrides the default description of `status`.
    pub description: Option<String>,
}

impl Fault {
    pub fn validate(&self) -> Result<(), &'static str> {
        if bot_id(&self.token).is_none() {
            return Err("invalid token");
        }
        if self.method.is_empty() {
            return Err("empty method");
        }
        if !(400..=599).contains(&self.status) {
            return Err("status must be 400..=599");
        }
        if self.count == 0 {
            return Err("count must be at least 1");
        }
        Ok(())
    }
}

pub(crate) struct Queued {
    pub update_id: i64,
    pub payload: Bytes,
}

pub(crate) struct Webhook {
    pub url: String,
    pub secret_token: Option<String>,
    pub max_connections: i64,
    pub allowed_updates: Option<serde_json::Value>,
}

pub(crate) struct StoredFile {
    pub file_id: String,
    pub file_unique_id: String,
    pub file_path: String,
    pub file_name: String,
    pub mime_type: Option<String>,
    pub data: Bytes,
}

#[derive(Default)]
pub(crate) struct BotState {
    pub queue: VecDeque<Queued>,
    pub next_update_id: i64,
    pub webhook: Option<Webhook>,
    /// The latest `getUpdates` call; an older pending one ends with 409.
    pub poll_seq: u64,
    pub next_message_id: i64,
    /// By `file_id`.
    pub files: HashMap<String, StoredFile>,
    pub last_error: Option<(i64, String)>,
}

impl BotState {
    /// Confirm by `offset` like `getUpdates`: a positive offset drops every
    /// update below it, a negative one keeps only the last `-offset`.
    pub fn confirm(&mut self, offset: i64) {
        if offset > 0 {
            while self.queue.front().is_some_and(|u| u.update_id < offset) {
                self.queue.pop_front();
            }
        } else if offset < 0 {
            let keep = usize::try_from(offset.unsigned_abs()).unwrap_or(usize::MAX);
            let forget = self.queue.len().saturating_sub(keep);
            self.queue.drain(..forget);
        }
    }

    pub fn file_by_path(&self, path: &str) -> Option<&StoredFile> {
        self.files.values().find(|file| file.file_path == path)
    }
}

pub(crate) struct Bot {
    pub token: String,
    pub id: i64,
    pub state: Mutex<BotState>,
    /// Updates, polls and webhook changes: wakes parked polls and the
    /// delivery task.
    pub changed: Notify,
}

impl Bot {
    fn new(token: &str, id: i64) -> Self {
        Self {
            token: token.to_owned(),
            id,
            state: Mutex::new(BotState {
                next_update_id: 1,
                next_message_id: 1,
                ..BotState::default()
            }),
            changed: Notify::new(),
        }
    }

    pub fn user(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "is_bot": true,
            "first_name": format!("Fake {}", self.id),
            "username": format!("fake_{}_bot", self.id),
        })
    }

    /// Queue an update, assigning `update_id` when it has none. `None` if the
    /// payload is not a JSON object.
    pub fn inject(&self, payload: &[u8]) -> Option<i64> {
        let value: serde_json::Value = serde_json::from_slice(payload).ok()?;
        let fields = value.as_object()?;
        let given = fields.get("update_id").and_then(serde_json::Value::as_i64);
        let mut state = self.state.lock();
        let update_id = given.unwrap_or(state.next_update_id);
        let payload = match given {
            Some(_) => Bytes::copy_from_slice(payload),
            None => with_update_id(payload, update_id)?,
        };
        state.next_update_id = state.next_update_id.max(update_id.saturating_add(1));
        state.queue.push_back(Queued { update_id, payload });
        drop(state);
        self.changed.notify_waiters();
        Some(update_id)
    }
}

/// Insert `"update_id":N` as the first key, keeping the rest verbatim.
fn with_update_id(payload: &[u8], update_id: i64) -> Option<Bytes> {
    let start = payload.iter().position(|b| *b == b'{')?;
    let rest = &payload[start + 1..];
    let empty = rest
        .iter()
        .find(|b| !b.is_ascii_whitespace())
        .is_some_and(|b| *b == b'}');
    let mut out = Vec::with_capacity(payload.len() + 32);
    out.extend_from_slice(format!("{{\"update_id\":{update_id}").as_bytes());
    if !empty {
        out.push(b',');
    }
    out.extend_from_slice(rest);
    Some(Bytes::from(out))
}
