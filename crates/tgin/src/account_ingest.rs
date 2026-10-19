//! An account's update batches into the engine: each update routed by the
//! account's rules on its facts, as envelope v1, a batch whole or not at
//! all.
//!
//! The account's ingest task calls [`AccountIngest`] through
//! [`BatchSink`]; [`tgin_mtproto::sink`] states its duties (batch order,
//! `refused` on a refusal, persisting what `committed` returns).

use std::borrow::Cow;
use std::sync::Arc;
use std::time::SystemTime;

use bytes::Bytes;
use tgin_core::engine::Engine;
use tgin_core::facts::Facts;
use tgin_core::sink::{Admit, BatchUpdate};
use tgin_mtproto::decode::PeerId;
use tgin_mtproto::envelope::{self, Meta};
use tgin_mtproto::schema::API;
use tgin_mtproto::{Admission, Batch, BatchSink, SessionStore, UpdateBytes, UpdateIds};

/// The most text the rules see, in bytes, as for bot updates.
const TEXT_CAP: usize = 4096;
/// The Bot API's offset of a channel id (`-100…`).
const CHANNEL_OFFSET: i64 = 1_000_000_000_000;

/// The engine's ingest of one account.
pub struct AccountIngest {
    engine: Arc<Engine>,
    account: Arc<str>,
}

impl AccountIngest {
    /// The ingest of the account `account` of the engine's config.
    pub fn new(engine: Arc<Engine>, account: &str) -> Self {
        Self {
            engine,
            account: Arc::from(account),
        }
    }
}

impl BatchSink for AccountIngest {
    fn admit(&self, batch: &Batch, ids: &mut UpdateIds, store: &SessionStore) -> Admission {
        if batch.updates.is_empty() {
            return Admission::Admitted;
        }
        let Some(source) = self.engine.account_source(&self.account) else {
            tracing::warn!(
                account = %self.account,
                "the account is not in the current config; its updates are refused"
            );
            return Admission::Refused;
        };
        let first = match ids.take(batch.updates.len(), store) {
            Ok(first) => first,
            Err(err) => {
                tracing::error!(
                    account = %self.account,
                    error = %err,
                    "no update ids for a batch; it is refused"
                );
                return Admission::Refused;
            }
        };
        let received_at = SystemTime::now();
        let mut items: Vec<Item<'_>> = batch
            .updates
            .iter()
            .zip(first..)
            .map(|(update, update_id)| Item {
                update,
                meta: Meta {
                    account: &self.account,
                    layer: API.layer,
                    update_id,
                    received_at,
                },
                users: &batch.users,
                chats: &batch.chats,
                payload: None,
                rendered: None,
            })
            .collect();
        match self.engine.admit_batch(source, &mut items) {
            Admit::Admitted | Admit::Unroutable => Admission::Admitted,
            Admit::Backpressure => Admission::Refused,
        }
    }
}

/// One update of a batch on its way into the engine.
struct Item<'a> {
    update: &'a UpdateBytes,
    meta: Meta<'a>,
    users: &'a Bytes,
    chats: &'a Bytes,
    payload: Option<Bytes>,
    rendered: Option<Bytes>,
}

impl BatchUpdate for Item<'_> {
    fn facts(&self) -> Facts<'_> {
        let f = self.update.facts();
        Facts {
            update_type: f.constructor,
            chat_id: f.peer.map(bot_api_id),
            chat_type: None,
            user_id: f.sender,
            text: f.text.map(|t| Cow::Borrowed(capped(t))),
            callback_data: None,
            // An account has no username for a `@bot` suffix to name: the
            // command matches by its name alone.
            command: f
                .command
                .map(|c| (c.split_once('@').map_or(c, |(name, _)| name), None)),
        }
    }

    fn update_id(&self) -> i64 {
        self.meta.update_id
    }

    fn update_type(&self) -> &'static str {
        self.update.facts().constructor
    }

    fn payload(&mut self) -> Bytes {
        self.payload
            .get_or_insert_with(|| envelope::raw(&self.meta, self.update, self.users, self.chats))
            .clone()
    }

    fn rendered(&mut self) -> Bytes {
        if let Some(rendered) = &self.rendered {
            return rendered.clone();
        }
        let rendered = match envelope::rendered(&self.meta, self.update, self.users, self.chats) {
            Ok(rendered) => rendered,
            Err(err) => {
                // The decoder took these bytes, so the codec should too.
                tracing::warn!(
                    account = %self.meta.account,
                    update_id = self.meta.update_id,
                    constructor = self.update.facts().constructor,
                    error = %err,
                    "the update has no TL-JSON; delivered without `update`"
                );
                self.payload()
            }
        };
        self.rendered = Some(rendered.clone());
        rendered
    }
}

/// A peer as the Bot API numbers chats: users as they are, basic groups
/// negated, channels and supergroups as `-100…`.
fn bot_api_id(peer: PeerId) -> i64 {
    match peer {
        PeerId::User(id) => id,
        PeerId::Chat(id) => id.saturating_neg(),
        PeerId::Channel(id) => CHANNEL_OFFSET.saturating_add(id).saturating_neg(),
    }
}

/// `text` cut to [`TEXT_CAP`] bytes on a char boundary.
fn capped(text: &str) -> &str {
    if text.len() <= TEXT_CAP {
        return text;
    }
    let mut end = TEXT_CAP;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
