//! Everything the fake saw, for tests to assert on through `GET /fake/calls`.

use std::{
    collections::BTreeMap,
    time::{SystemTime, UNIX_EPOCH},
};

use parking_lot::Mutex;
use serde::Serialize;

use crate::params::Params;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A Bot API method call.
    Call,
    /// A `/file/bot…` download.
    File,
    /// An update POSTed to the bot's webhook.
    Webhook,
}

/// Where a call came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Via {
    Http,
    /// Built from a webhook response, as Telegram does.
    WebhookReply,
}

/// What became of a webhook response that named a method.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplyOutcome {
    Executed,
    Refused,
}

#[derive(Clone, Debug, Serialize)]
pub struct FileEntry {
    pub name: String,
    pub filename: String,
    pub size: usize,
    /// [`fnv1a64`] of the part's bytes, so tests can compare contents.
    pub fnv1a64: String,
}

/// The 64-bit FNV-1a hash of `data` as 16 lowercase hex digits: a content
/// check for tests, not a cryptographic digest.
pub fn fnv1a64(data: &[u8]) -> String {
    let hash = data.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    format!("{hash:016x}")
}

/// One journal line. Calls are recorded when they arrive, so `status` is
/// `null` while a call (a long poll) is still pending, and
/// [`ABANDONED`] (499) once the client went away before the answer.
#[derive(Clone, Debug, Serialize)]
pub struct Entry {
    pub seq: u64,
    /// Arrival, Unix milliseconds.
    pub time_ms: u64,
    pub bot_id: i64,
    pub kind: Kind,
    pub via: Option<Via>,
    /// The method as called; empty for downloads and webhook deliveries.
    pub method: String,
    pub http_method: String,
    /// Of the request, or for a webhook delivery of the bot's response.
    pub content_type: Option<String>,
    pub transfer_encoding: Option<String>,
    pub args: BTreeMap<String, String>,
    pub files: Vec<FileEntry>,
    /// What the fake answered, or for a delivery what the bot answered.
    pub status: Option<u16>,
    pub update_id: Option<i64>,
    pub reply_method: Option<String>,
    pub reply: Option<ReplyOutcome>,
    pub error: Option<String>,
    #[serde(skip)]
    pub token: String,
}

impl Entry {
    pub fn new(token: &str, bot_id: i64, kind: Kind, http_method: &str) -> Self {
        Self {
            seq: 0,
            time_ms: 0,
            bot_id,
            kind,
            via: None,
            method: String::new(),
            http_method: http_method.to_owned(),
            content_type: None,
            transfer_encoding: None,
            args: BTreeMap::new(),
            files: Vec::new(),
            status: None,
            update_id: None,
            reply_method: None,
            reply: None,
            error: None,
            token: token.to_owned(),
        }
    }

    pub fn with_params(mut self, params: &Params) -> Self {
        self.args.clone_from(&params.args);
        self.files = params
            .files
            .iter()
            .map(|part| FileEntry {
                name: part.name.clone(),
                filename: part.filename.clone(),
                size: part.data.len(),
                fnv1a64: fnv1a64(&part.data),
            })
            .collect();
        self
    }
}

/// Which entries `GET /fake/calls` returns.
#[derive(Debug, Default, serde::Deserialize)]
pub struct Filter {
    pub token: Option<String>,
    pub method: Option<String>,
    pub kind: Option<String>,
}

impl Filter {
    fn matches(&self, entry: &Entry) -> bool {
        let kind = match entry.kind {
            Kind::Call => "call",
            Kind::File => "file",
            Kind::Webhook => "webhook",
        };
        // `?token=&method=getMe` filters by method only.
        fn wanted(value: &Option<String>) -> Option<&str> {
            value.as_deref().filter(|value| !value.is_empty())
        }
        wanted(&self.token).is_none_or(|token| token == entry.token)
            && wanted(&self.method).is_none_or(|method| method.eq_ignore_ascii_case(&entry.method))
            && wanted(&self.kind).is_none_or(|wanted| wanted == kind)
    }
}

/// The status of a call whose client disconnected before it was answered
/// (nginx's "client closed request").
pub const ABANDONED: u16 = 499;

#[derive(Default)]
pub struct Journal {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    next_seq: u64,
    entries: Vec<Entry>,
}

impl Journal {
    /// Record an entry; returns its sequence number for [`Journal::finish`].
    pub fn record(&self, mut entry: Entry) -> u64 {
        let mut inner = self.inner.lock();
        inner.next_seq += 1;
        entry.seq = inner.next_seq;
        entry.time_ms = unix_millis();
        let seq = entry.seq;
        inner.entries.push(entry);
        seq
    }

    /// Record a call that is answered later: dropping the guard before
    /// [`Pending::finish`] (hyper drops the handler when the client
    /// disconnects) marks it [`ABANDONED`].
    pub fn pending(&self, entry: Entry) -> Pending<'_> {
        Pending {
            journal: self,
            seq: self.record(entry),
            finished: false,
        }
    }

    /// Set the status of a recorded entry (a no-op once cleared).
    pub fn finish(&self, seq: u64, status: u16) {
        let mut inner = self.inner.lock();
        if let Some(entry) = inner.entries.iter_mut().rev().find(|e| e.seq == seq) {
            entry.status = Some(status);
        }
    }

    pub fn list(&self, filter: &Filter) -> Vec<Entry> {
        let inner = self.inner.lock();
        inner
            .entries
            .iter()
            .filter(|entry| filter.matches(entry))
            .cloned()
            .collect()
    }

    pub fn clear(&self) {
        self.inner.lock().entries.clear();
    }
}

/// A recorded call still waiting for its answer.
pub struct Pending<'a> {
    journal: &'a Journal,
    seq: u64,
    finished: bool,
}

impl Pending<'_> {
    pub fn finish(mut self, status: u16) {
        self.finished = true;
        self.journal.finish(self.seq, status);
    }
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.journal.finish(self.seq, ABANDONED);
        }
    }
}

pub fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

pub fn unix_seconds() -> i64 {
    i64::try_from(unix_millis() / 1000).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::fnv1a64;

    /// The published FNV-1a 64 vectors, and the ones the tests use.
    #[test]
    fn fnv1a64_vectors() {
        assert_eq!(fnv1a64(b""), "cbf29ce484222325");
        assert_eq!(fnv1a64(b"a"), "af63dc4c8601ec8c");
        assert_eq!(fnv1a64(b"hello"), "a430d84680aabd0b");
        assert_eq!(fnv1a64(b"abc"), "e71fa2190541574b");
    }
}
