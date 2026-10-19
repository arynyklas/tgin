//! `SessionStore`: one account's MTProto session in `{dir}/session.redb`,
//! owned by one process through `{dir}/lock`.
//!
//! Every write is one redb transaction, committed before the call returns.
//! The table layout is the M3 spike's (`spike/m3`, `spikes/m3-grammers`), so
//! the spike's live authorization imports with a plain copy.
//!
//! # Permissions
//!
//! On Unix the directory is set to mode 0700 and `session.redb` and `lock`
//! to 0600 on every open: the file holds the account's auth keys. On
//! Windows tgin sets nothing; the directory's ACL is the operator's, who
//! must keep it readable by tgin's account alone.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition, WriteTransaction};

use crate::SessionError;
use crate::lock::{DirLock, lock_dir};
use crate::updates::{StateDelta, StoredState};

/// DC id → `ipv4(4) port(2) ipv6(16) port(2)`, ports little-endian.
const DC_OPTIONS: TableDefinition<i32, &[u8]> = TableDefinition::new("dc_options");
/// DC id → the 256-byte permanent auth key for that DC.
const AUTH_KEYS: TableDefinition<i32, &[u8]> = TableDefinition::new("auth_keys");
/// Single row `"home"` → home DC id.
const HOME_DC: TableDefinition<&str, i32> = TableDefinition::new("home_dc");
/// Bot API dialog id → encoded [`Peer`] (see `encode_peer`).
const PEERS: TableDefinition<i64, &[u8]> = TableDefinition::new("peers");
/// `"pts" | "qts" | "seq" | "date"` → value. The spike's name, kept.
const UPDATE_STATE: TableDefinition<&str, i32> = TableDefinition::new("update_state");
/// Bare channel id → channel pts.
const CHANNEL_PTS: TableDefinition<i64, i32> = TableDefinition::new("channel_pts");
/// Bare channel id → nothing: a batch moved the channel ahead of its
/// stored pts and it has not caught up since, so it catches up at start.
const CHANNELS_BEHIND: TableDefinition<i64, ()> = TableDefinition::new("channels_behind");
/// `schema`, `created_unix`, `self_user_id`, `login_unix`, `update_id`;
/// integers are little-endian `i64`.
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");

const SCHEMA_VERSION: &[u8] = b"1";
const HOME_KEY: &str = "home";
const SELF_USER_ID: &str = "self_user_id";
const LOGIN_UNIX: &str = "login_unix";
const UPDATE_ID: &str = "update_id";

/// A DC's permanent auth key.
#[derive(Clone, PartialEq, Eq)]
pub struct AuthKey(pub [u8; 256]);

impl std::fmt::Debug for AuthKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AuthKey(..)")
    }
}

/// Where a DC listens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DcAddr {
    pub ipv4: SocketAddrV4,
    pub ipv6: SocketAddrV6,
}

/// A channel's flavour, as Telegram reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelKind {
    Broadcast,
    Megagroup,
    Gigagroup,
    Community,
}

/// What a peer is. `None` flags are not yet known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeerKind {
    User {
        bot: Option<bool>,
        is_self: Option<bool>,
    },
    Chat,
    Channel {
        kind: Option<ChannelKind>,
    },
}

/// A cached peer: its bare id, kind and access hash.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Peer {
    pub id: i64,
    pub kind: PeerKind,
    pub access_hash: Option<i64>,
}

impl Peer {
    /// The Bot API dialog id the `peers` table is keyed by.
    #[must_use]
    pub fn dialog_id(&self) -> i64 {
        match self.kind {
            PeerKind::User { .. } => self.id,
            PeerKind::Chat => -self.id,
            PeerKind::Channel { .. } => -1_000_000_000_000 - self.id,
        }
    }
}

/// The common update state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UpdateState {
    pub pts: i32,
    pub qts: i32,
    pub seq: i32,
    pub date: i32,
}

/// The logged-in user, written once per login in one transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Login {
    pub self_user_id: i64,
    pub login_unix: i64,
}

/// One account's session, exclusively owned by this process while it lives.
#[derive(Debug)]
pub struct SessionStore {
    db: Database,
    // Dropped after `db` (field order), so the file is closed before another
    // process can win the lock.
    _lock: DirLock,
    /// Test-only: commits allowed before the injected crash.
    #[cfg(test)]
    commits_left: std::sync::Mutex<Option<u32>>,
}

impl SessionStore {
    /// Opens (or creates) the session in `dir`, holding `{dir}/lock` for as
    /// long as the returned value lives. The lock is taken before
    /// `session.redb` is created or opened; another holder gives
    /// [`SessionError::Held`].
    pub fn open(dir: &Path) -> Result<Self, SessionError> {
        std::fs::create_dir_all(dir)?;
        restrict(dir, 0o700)?;
        let lock = lock_dir(dir)?;
        restrict(&dir.join("lock"), 0o600)?;

        let path = dir.join("session.redb");
        precreate_private(&path)?;
        let db = Database::create(&path)?;
        restrict(&path, 0o600)?;

        let txn = db.begin_write()?;
        {
            txn.open_table(DC_OPTIONS)?;
            txn.open_table(AUTH_KEYS)?;
            txn.open_table(HOME_DC)?;
            txn.open_table(PEERS)?;
            txn.open_table(UPDATE_STATE)?;
            txn.open_table(CHANNEL_PTS)?;
            txn.open_table(CHANNELS_BEHIND)?;
            let mut meta = txn.open_table(META)?;
            if meta.get("schema")?.is_none() {
                meta.insert("schema", SCHEMA_VERSION)?;
                meta.insert("created_unix", now_unix().to_le_bytes().as_slice())?;
            }
        }
        txn.commit()?;

        Ok(Self {
            db,
            _lock: lock,
            #[cfg(test)]
            commits_left: std::sync::Mutex::new(None),
        })
    }

    fn write<T>(
        &self,
        f: impl FnOnce(&WriteTransaction) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        let txn = self.db.begin_write()?;
        let out = f(&txn)?;
        #[cfg(test)]
        {
            let mut left = self.commits_left.lock().expect("test hook");
            match left.as_mut() {
                Some(0) => return Err(SessionError::Crashed),
                Some(n) => *n -= 1,
                None => {}
            }
        }
        txn.commit()?;
        Ok(out)
    }

    /// Every stored DC address, by DC id.
    pub fn dc_options(&self) -> Result<Vec<(i32, DcAddr)>, SessionError> {
        let txn = self.db.begin_read()?;
        let mut out = Vec::new();
        for row in txn.open_table(DC_OPTIONS)?.iter()? {
            let (id, addr) = row?;
            let addr = decode_addr(addr.value()).ok_or(SessionError::Corrupt("dc option"))?;
            out.push((id.value(), addr));
        }
        Ok(out)
    }

    pub fn set_dc_option(&self, dc: i32, addr: DcAddr) -> Result<(), SessionError> {
        self.write(|txn| {
            txn.open_table(DC_OPTIONS)?
                .insert(dc, encode_addr(&addr).as_slice())?;
            Ok(())
        })
    }

    /// The home DC, once one is set.
    pub fn home_dc(&self) -> Result<Option<i32>, SessionError> {
        let txn = self.db.begin_read()?;
        Ok(txn.open_table(HOME_DC)?.get(HOME_KEY)?.map(|v| v.value()))
    }

    /// The permanent auth key for `dc`.
    pub fn auth_key(&self, dc: i32) -> Result<Option<AuthKey>, SessionError> {
        let txn = self.db.begin_read()?;
        match txn.open_table(AUTH_KEYS)?.get(dc)? {
            Some(k) => Ok(Some(AuthKey(
                <[u8; 256]>::try_from(k.value()).map_err(|_| SessionError::Corrupt("auth key"))?,
            ))),
            None => Ok(None),
        }
    }

    /// Makes `dc` the home DC with `key`, in one transaction. Once a self
    /// user is recorded the home authorization is the login, so this
    /// refuses with [`SessionError::KeyReplaceRefused`] instead of
    /// replacing it, or moving home to another DC [C: S4 F8].
    pub fn set_home_key(&self, dc: i32, key: &AuthKey) -> Result<(), SessionError> {
        self.write(|txn| {
            if txn.open_table(META)?.get(SELF_USER_ID)?.is_some() {
                return Err(SessionError::KeyReplaceRefused { dc });
            }
            txn.open_table(HOME_DC)?.insert(HOME_KEY, dc)?;
            txn.open_table(AUTH_KEYS)?.insert(dc, key.0.as_slice())?;
            Ok(())
        })
    }

    /// Stores a media DC's key. The home DC's key goes through
    /// [`set_home_key`](Self::set_home_key) only.
    pub fn set_dc_key(&self, dc: i32, key: &AuthKey) -> Result<(), SessionError> {
        self.write(|txn| {
            if txn.open_table(HOME_DC)?.get(HOME_KEY)?.map(|v| v.value()) == Some(dc) {
                return Err(SessionError::HomeDc { dc });
            }
            txn.open_table(AUTH_KEYS)?.insert(dc, key.0.as_slice())?;
            Ok(())
        })
    }

    /// The recorded login, if any.
    pub fn login(&self) -> Result<Option<Login>, SessionError> {
        let txn = self.db.begin_read()?;
        let meta = txn.open_table(META)?;
        let self_user_id = read_i64(&meta, SELF_USER_ID)?;
        let login_unix = read_i64(&meta, LOGIN_UNIX)?;
        match (self_user_id, login_unix) {
            (Some(self_user_id), Some(login_unix)) => Ok(Some(Login {
                self_user_id,
                login_unix,
            })),
            (None, None) => Ok(None),
            _ => Err(SessionError::Corrupt("login marker")),
        }
    }

    /// Records a verified login in one transaction [C: S3 F7]: the self
    /// user's peer row (`is_self`), `meta.self_user_id` and
    /// `meta.login_unix`. A crash leaves either all three or none.
    pub fn record_login(&self, me: Peer) -> Result<Login, SessionError> {
        let PeerKind::User { bot, .. } = me.kind else {
            return Err(SessionError::Corrupt("self peer that is not a user"));
        };
        let login = Login {
            self_user_id: me.id,
            login_unix: now_unix(),
        };
        let me = Peer {
            kind: PeerKind::User {
                bot,
                is_self: Some(true),
            },
            ..me
        };
        self.write(|txn| {
            txn.open_table(PEERS)?
                .insert(me.dialog_id(), encode_peer(&me).as_slice())?;
            let mut meta = txn.open_table(META)?;
            meta.insert(SELF_USER_ID, login.self_user_id.to_le_bytes().as_slice())?;
            meta.insert(LOGIN_UNIX, login.login_unix.to_le_bytes().as_slice())?;
            Ok(())
        })?;
        Ok(login)
    }

    /// Clears the authorization for a new login, in one transaction and
    /// under the lock: every auth key, the home DC, the login marker, the
    /// cached peers and the update state (common and channels). Kept: the
    /// DC addresses and the `update_id` high-water mark, so that ids handed
    /// out downstream never repeat. A login after `needs_login`, a failed
    /// attempt and a logout go through here; deleting the files instead
    /// would let another process take the directory between the delete and
    /// the next open.
    pub fn reset_for_login(&self) -> Result<(), SessionError> {
        self.write(|txn| {
            txn.delete_table(AUTH_KEYS)?;
            txn.delete_table(HOME_DC)?;
            txn.delete_table(PEERS)?;
            txn.delete_table(UPDATE_STATE)?;
            txn.delete_table(CHANNEL_PTS)?;
            txn.delete_table(CHANNELS_BEHIND)?;
            txn.open_table(AUTH_KEYS)?;
            txn.open_table(HOME_DC)?;
            txn.open_table(PEERS)?;
            txn.open_table(UPDATE_STATE)?;
            txn.open_table(CHANNEL_PTS)?;
            txn.open_table(CHANNELS_BEHIND)?;
            let mut meta = txn.open_table(META)?;
            meta.remove(SELF_USER_ID)?;
            meta.remove(LOGIN_UNIX)?;
            Ok(())
        })
    }

    /// The cached peer with this Bot API dialog id.
    pub fn peer(&self, dialog_id: i64) -> Result<Option<Peer>, SessionError> {
        let txn = self.db.begin_read()?;
        match txn.open_table(PEERS)?.get(dialog_id)? {
            Some(v) => Ok(Some(
                decode_peer(v.value()).ok_or(SessionError::Corrupt("peer"))?,
            )),
            None => Ok(None),
        }
    }

    /// Caches `peers` in one transaction, merged into the rows already
    /// there: a peer without an access hash keeps the cached one, and a
    /// flag the new row does not know (`None`) keeps the cached value.
    pub fn put_peers(&self, peers: &[Peer]) -> Result<(), SessionError> {
        self.write(|txn| {
            let mut table = txn.open_table(PEERS)?;
            for peer in peers {
                let key = peer.dialog_id();
                let old = match table.get(key)? {
                    Some(v) => decode_peer(v.value()),
                    None => None,
                };
                let merged = old.map_or(*peer, |old| merge_peer(old, *peer));
                table.insert(key, encode_peer(&merged).as_slice())?;
            }
            Ok(())
        })
    }

    /// What the update manager starts from: the common state, every
    /// channel's pts, the users whose access hash is cached, and the
    /// channels that may be behind their stored pts.
    pub fn stored_state(&self) -> Result<StoredState, SessionError> {
        let mut known_users = Vec::new();
        {
            let txn = self.db.begin_read()?;
            for row in txn.open_table(PEERS)?.iter()? {
                let (_, v) = row?;
                let peer = decode_peer(v.value()).ok_or(SessionError::Corrupt("peer"))?;
                if matches!(peer.kind, PeerKind::User { .. }) && peer.access_hash.is_some() {
                    known_users.push(peer.id);
                }
            }
        }
        let mut behind = Vec::new();
        {
            let txn = self.db.begin_read()?;
            for row in txn.open_table(CHANNELS_BEHIND)?.iter()? {
                behind.push(row?.0.value());
            }
        }
        Ok(StoredState {
            common: self.update_state()?,
            channels: self.channel_pts()?,
            known_users,
            behind,
        })
    }

    /// Persists what [`UpdateManager::committed`](crate::UpdateManager::committed)
    /// returned, in one transaction: a crash keeps all of it or none.
    pub fn commit_state(&self, delta: &StateDelta) -> Result<(), SessionError> {
        self.write(|txn| {
            let mut table = txn.open_table(UPDATE_STATE)?;
            for (k, v) in [
                ("pts", delta.pts),
                ("qts", delta.qts),
                ("seq", delta.seq),
                ("date", delta.date),
            ] {
                if let Some(v) = v {
                    table.insert(k, v)?;
                }
            }
            let mut channels = txn.open_table(CHANNEL_PTS)?;
            for &(channel, pts) in &delta.channels {
                channels.insert(channel, pts)?;
            }
            let mut behind = txn.open_table(CHANNELS_BEHIND)?;
            for channel in &delta.caught_up {
                behind.remove(channel)?;
            }
            Ok(())
        })
    }

    /// Records a batch's [`ahead`](crate::Batch::ahead) channels, before
    /// the batch is admitted: after a crash, they catch up at start.
    pub fn mark_channels_behind(&self, channels: &[i64]) -> Result<(), SessionError> {
        if channels.is_empty() {
            return Ok(());
        }
        self.write(|txn| {
            let mut behind = txn.open_table(CHANNELS_BEHIND)?;
            for &channel in channels {
                behind.insert(channel, ())?;
            }
            Ok(())
        })
    }

    pub fn update_state(&self) -> Result<UpdateState, SessionError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(UPDATE_STATE)?;
        let get =
            |k: &str| -> Result<i32, SessionError> { Ok(table.get(k)?.map_or(0, |v| v.value())) };
        Ok(UpdateState {
            pts: get("pts")?,
            qts: get("qts")?,
            seq: get("seq")?,
            date: get("date")?,
        })
    }

    pub fn set_update_state(&self, state: UpdateState) -> Result<(), SessionError> {
        self.write(|txn| {
            let mut table = txn.open_table(UPDATE_STATE)?;
            table.insert("pts", state.pts)?;
            table.insert("qts", state.qts)?;
            table.insert("seq", state.seq)?;
            table.insert("date", state.date)?;
            Ok(())
        })
    }

    /// Every channel's pts, by bare channel id.
    pub fn channel_pts(&self) -> Result<Vec<(i64, i32)>, SessionError> {
        let txn = self.db.begin_read()?;
        let mut out = Vec::new();
        for row in txn.open_table(CHANNEL_PTS)?.iter()? {
            let (id, pts) = row?;
            out.push((id.value(), pts.value()));
        }
        Ok(out)
    }

    pub fn set_channel_pts(&self, channel: i64, pts: i32) -> Result<(), SessionError> {
        self.write(|txn| {
            txn.open_table(CHANNEL_PTS)?.insert(channel, pts)?;
            Ok(())
        })
    }

    /// The highest `update_id` handed out, if any.
    pub fn update_id(&self) -> Result<Option<i64>, SessionError> {
        let txn = self.db.begin_read()?;
        read_i64(&txn.open_table(META)?, UPDATE_ID)
    }

    pub fn set_update_id(&self, high_water: i64) -> Result<(), SessionError> {
        self.write(|txn| {
            txn.open_table(META)?
                .insert(UPDATE_ID, high_water.to_le_bytes().as_slice())?;
            Ok(())
        })
    }
}

fn read_i64(
    meta: &impl ReadableTable<&'static str, &'static [u8]>,
    key: &'static str,
) -> Result<Option<i64>, SessionError> {
    match meta.get(key)? {
        Some(v) => Ok(Some(i64::from_le_bytes(
            v.value()
                .try_into()
                .map_err(|_| SessionError::Corrupt(key))?,
        ))),
        None => Ok(None),
    }
}

/// Creates `path` owner-only before redb opens it, so the auth keys are
/// never readable by others, not even until the first `restrict`.
#[cfg(unix)]
fn precreate_private(path: &Path) -> Result<(), SessionError> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(path)?;
    Ok(())
}

#[cfg(not(unix))]
fn precreate_private(_path: &Path) -> Result<(), SessionError> {
    Ok(())
}

#[cfg(unix)]
fn restrict(path: &Path, mode: u32) -> Result<(), SessionError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

/// Windows: the ACL is the operator's (see the module docs).
#[cfg(not(unix))]
fn restrict(_path: &Path, _mode: u32) -> Result<(), SessionError> {
    Ok(())
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

fn encode_addr(a: &DcAddr) -> [u8; 24] {
    let mut out = [0u8; 24];
    out[0..4].copy_from_slice(&a.ipv4.ip().octets());
    out[4..6].copy_from_slice(&a.ipv4.port().to_le_bytes());
    out[6..22].copy_from_slice(&a.ipv6.ip().octets());
    out[22..24].copy_from_slice(&a.ipv6.port().to_le_bytes());
    out
}

fn decode_addr(b: &[u8]) -> Option<DcAddr> {
    let b: &[u8; 24] = b.try_into().ok()?;
    let v4: [u8; 4] = b[0..4].try_into().ok()?;
    let p4 = u16::from_le_bytes(b[4..6].try_into().ok()?);
    let v6: [u8; 16] = b[6..22].try_into().ok()?;
    let p6 = u16::from_le_bytes(b[22..24].try_into().ok()?);
    Some(DcAddr {
        ipv4: SocketAddrV4::new(Ipv4Addr::from(v4), p4),
        ipv6: SocketAddrV6::new(Ipv6Addr::from(v6), p6, 0, 0),
    })
}

fn tri(v: Option<bool>) -> u8 {
    match v {
        None => 0,
        Some(false) => 1,
        Some(true) => 2,
    }
}

fn untri(b: u8) -> Option<Option<bool>> {
    match b {
        0 => Some(None),
        1 => Some(Some(false)),
        2 => Some(Some(true)),
        _ => None,
    }
}

/// `new` over `old` for one dialog: unknown fields of `new` keep `old`'s.
fn merge_peer(old: Peer, new: Peer) -> Peer {
    let kind = match (old.kind, new.kind) {
        (
            PeerKind::User {
                bot: ob,
                is_self: os,
            },
            PeerKind::User { bot, is_self },
        ) => PeerKind::User {
            bot: bot.or(ob),
            is_self: is_self.or(os),
        },
        (PeerKind::Channel { kind: ok }, PeerKind::Channel { kind }) => {
            PeerKind::Channel { kind: kind.or(ok) }
        }
        (_, kind) => kind,
    };
    Peer {
        id: new.id,
        kind,
        access_hash: new.access_hash.or(old.access_hash),
    }
}

/// `tag(1) id(8) has_auth(1) auth(8) bot(1) is_self(1) kind(1)`,
/// little-endian; the spike's encoding.
fn encode_peer(p: &Peer) -> [u8; 21] {
    let (tag, bot, is_self, kind) = match p.kind {
        PeerKind::User { bot, is_self } => (0u8, bot, is_self, 0u8),
        PeerKind::Chat => (1, None, None, 0),
        PeerKind::Channel { kind } => (
            2,
            None,
            None,
            match kind {
                None => 0,
                Some(ChannelKind::Broadcast) => 1,
                Some(ChannelKind::Megagroup) => 2,
                Some(ChannelKind::Gigagroup) => 3,
                Some(ChannelKind::Community) => 4,
            },
        ),
    };
    let mut out = [0u8; 21];
    out[0] = tag;
    out[1..9].copy_from_slice(&p.id.to_le_bytes());
    out[9] = u8::from(p.access_hash.is_some());
    out[10..18].copy_from_slice(&p.access_hash.unwrap_or(0).to_le_bytes());
    out[18] = tri(bot);
    out[19] = tri(is_self);
    out[20] = kind;
    out
}

fn decode_peer(b: &[u8]) -> Option<Peer> {
    let b: &[u8; 21] = b.try_into().ok()?;
    let id = i64::from_le_bytes(b[1..9].try_into().ok()?);
    let access_hash = match b[9] {
        0 => None,
        1 => Some(i64::from_le_bytes(b[10..18].try_into().ok()?)),
        _ => return None,
    };
    let kind = match b[0] {
        0 => PeerKind::User {
            bot: untri(b[18])?,
            is_self: untri(b[19])?,
        },
        1 => PeerKind::Chat,
        2 => PeerKind::Channel {
            kind: match b[20] {
                0 => None,
                1 => Some(ChannelKind::Broadcast),
                2 => Some(ChannelKind::Megagroup),
                3 => Some(ChannelKind::Gigagroup),
                4 => Some(ChannelKind::Community),
                _ => return None,
            },
        },
        _ => return None,
    };
    Some(Peer {
        id,
        kind,
        access_hash,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn me(id: i64) -> Peer {
        Peer {
            id,
            kind: PeerKind::User {
                bot: Some(false),
                is_self: None,
            },
            access_hash: Some(-9),
        }
    }

    #[test]
    fn second_open_is_held() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("alice");
        let first = SessionStore::open(&dir).expect("first open");
        match SessionStore::open(&dir) {
            Err(SessionError::Held { account }) => assert_eq!(account, "alice"),
            Err(e) => panic!("expected Held, got {e}"),
            Ok(_) => panic!("second open succeeded while held"),
        }
        let held = SessionStore::open(&dir).expect_err("still held");
        assert_eq!(
            held.for_account("main").to_string(),
            "account `main` is held by another process"
        );
        drop(first);
        SessionStore::open(&dir).expect("reopen after release");
    }

    #[test]
    fn home_key_not_replaced_after_login() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let s = SessionStore::open(tmp.path()).expect("open");
        // Before a login the key may be replaced (a fresh handshake).
        s.set_home_key(2, &AuthKey([1; 256])).expect("first key");
        s.set_home_key(2, &AuthKey([2; 256]))
            .expect("replace before login");
        s.record_login(me(42)).expect("login");
        for dc in [2, 4] {
            match s.set_home_key(dc, &AuthKey([3; 256])) {
                Err(SessionError::KeyReplaceRefused { dc: d }) => assert_eq!(d, dc),
                other => panic!("expected KeyReplaceRefused, got {other:?}"),
            }
        }
        // The media path cannot reach the home key either.
        assert!(matches!(
            s.set_dc_key(2, &AuthKey([3; 256])),
            Err(SessionError::HomeDc { dc: 2 })
        ));
        drop(s);
        let s = SessionStore::open(tmp.path()).expect("reopen");
        assert_eq!(s.home_dc().expect("home"), Some(2));
        assert_eq!(s.auth_key(2).expect("key"), Some(AuthKey([2; 256])));
        assert!(matches!(
            s.set_home_key(2, &AuthKey([3; 256])),
            Err(SessionError::KeyReplaceRefused { dc: 2 })
        ));
    }

    /// A crash at any commit of `record_login` leaves the whole marker or
    /// none of it: there is one transaction, so no kill lands between the
    /// self user and `login_unix`.
    #[test]
    fn login_marker_is_atomic() {
        for commits in 0..3 {
            let tmp = tempfile::tempdir().expect("tempdir");
            {
                let s = SessionStore::open(tmp.path()).expect("open");
                *s.commits_left.lock().expect("hook") = Some(commits);
                let _ = s.record_login(me(42));
            }
            let s = SessionStore::open(tmp.path()).expect("reopen");
            let txn = s.db.begin_read().expect("read");
            let meta = txn.open_table(META).expect("meta");
            let user = read_i64(&meta, SELF_USER_ID).expect("self");
            let at = read_i64(&meta, LOGIN_UNIX).expect("login_unix");
            let row = s.peer(42).expect("peer");
            let parts = [user.is_some(), at.is_some(), row.is_some()];
            assert!(
                parts == [false; 3] || parts == [true; 3],
                "crash after {commits} commits left a partial marker: {parts:?}"
            );
            assert_eq!(s.login().expect("login").is_some(), parts[0]);
        }
    }

    /// A re-login after `needs_login`: the reset clears the keys, the home
    /// DC, the login marker, peers and update state in place, under the
    /// lock (no second open succeeds meanwhile), and keeps the DC
    /// addresses and the `update_id` high-water mark.
    #[test]
    fn reset_for_login_clears_the_authorization_under_the_lock() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let s = SessionStore::open(tmp.path()).expect("open");
        let addr = DcAddr {
            ipv4: SocketAddrV4::new(Ipv4Addr::new(149, 154, 167, 51), 443),
            ipv6: SocketAddrV6::new(Ipv6Addr::LOCALHOST, 443, 0, 0),
        };
        s.set_dc_option(2, addr).expect("dc");
        s.set_home_key(2, &AuthKey([1; 256])).expect("home");
        s.set_dc_key(4, &AuthKey([4; 256])).expect("media");
        s.record_login(me(42)).expect("login");
        s.set_update_state(UpdateState {
            pts: 5,
            qts: 6,
            seq: 7,
            date: 8,
        })
        .expect("state");
        s.set_channel_pts(9, 10).expect("channel");
        s.mark_channels_behind(&[9]).expect("behind");
        s.set_update_id(77).expect("update_id");

        s.reset_for_login().expect("reset");
        assert!(matches!(
            SessionStore::open(tmp.path()),
            Err(SessionError::Held { .. })
        ));
        assert_eq!(s.login().expect("login"), None);
        assert_eq!(s.home_dc().expect("home"), None);
        assert_eq!(s.auth_key(2).expect("key"), None);
        assert_eq!(s.auth_key(4).expect("key"), None);
        assert_eq!(s.peer(42).expect("peer"), None);
        assert_eq!(s.update_state().expect("state"), UpdateState::default());
        assert!(s.channel_pts().expect("channels").is_empty());
        assert!(s.stored_state().expect("stored").behind.is_empty());
        assert_eq!(s.dc_options().expect("dcs"), vec![(2, addr)]);
        assert_eq!(s.update_id().expect("update_id"), Some(77));
        // A new key may now be stored: the marker is gone.
        s.set_home_key(4, &AuthKey([5; 256])).expect("new home");
        drop(s);
        let s = SessionStore::open(tmp.path()).expect("reopen");
        assert_eq!(s.login().expect("login"), None);
        assert_eq!(s.home_dc().expect("home"), Some(4));
    }

    #[test]
    fn state_roundtrip_all_tables() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let addr = |n: u8| DcAddr {
            ipv4: SocketAddrV4::new(Ipv4Addr::new(149, 154, 167, n), 443),
            ipv6: SocketAddrV6::new(
                Ipv6Addr::new(0x2001, 0x67c, 0x4e8, 0xf002, 0, 0, 0, n.into()),
                443,
                0,
                0,
            ),
        };
        let peers = [
            Peer {
                id: 7,
                kind: PeerKind::User {
                    bot: Some(true),
                    is_self: None,
                },
                access_hash: Some(i64::MIN),
            },
            Peer {
                id: 7,
                kind: PeerKind::Chat,
                access_hash: None,
            },
            Peer {
                id: 99,
                kind: PeerKind::Channel {
                    kind: Some(ChannelKind::Megagroup),
                },
                access_hash: Some(5),
            },
        ];
        let state = UpdateState {
            pts: 10,
            qts: 11,
            seq: 12,
            date: 13,
        };
        let login = {
            let s = SessionStore::open(tmp.path()).expect("open");
            assert_eq!(s.home_dc().expect("home"), None);
            assert_eq!(s.login().expect("login"), None);
            assert_eq!(s.update_id().expect("update_id"), None);
            assert_eq!(s.update_state().expect("state"), UpdateState::default());
            s.set_dc_option(2, addr(2)).expect("dc 2");
            s.set_dc_option(4, addr(4)).expect("dc 4");
            s.set_home_key(2, &AuthKey([2; 256])).expect("home key");
            s.set_dc_key(4, &AuthKey([4; 256])).expect("dc 4 key");
            s.put_peers(&peers).expect("peers");
            s.set_update_state(state).expect("state");
            s.set_channel_pts(99, 5).expect("chan");
            s.set_channel_pts(99, 6).expect("chan again");
            s.set_channel_pts(100, 1).expect("chan 100");
            s.set_update_id(1234).expect("update_id");
            s.record_login(me(42)).expect("login")
        };
        let s = SessionStore::open(tmp.path()).expect("reopen");
        assert_eq!(
            s.dc_options().expect("dcs"),
            vec![(2, addr(2)), (4, addr(4))]
        );
        assert_eq!(s.home_dc().expect("home"), Some(2));
        assert_eq!(s.auth_key(2).expect("key 2"), Some(AuthKey([2; 256])));
        assert_eq!(s.auth_key(4).expect("key 4"), Some(AuthKey([4; 256])));
        assert_eq!(s.auth_key(5).expect("key 5"), None);
        for p in &peers {
            assert_eq!(s.peer(p.dialog_id()).expect("peer").as_ref(), Some(p));
        }
        assert_eq!(
            s.peer(42).expect("self"),
            Some(Peer {
                kind: PeerKind::User {
                    bot: Some(false),
                    is_self: Some(true)
                },
                ..me(42)
            })
        );
        assert_eq!(s.update_state().expect("state"), state);
        assert_eq!(s.channel_pts().expect("chans"), vec![(99, 6), (100, 1)]);
        assert_eq!(s.update_id().expect("update_id"), Some(1234));
        assert_eq!(s.login().expect("login"), Some(login));
        assert!(login.login_unix > 1_700_000_000);
    }

    /// A peer row without an access hash (or with an unknown flag) never
    /// erases what the cache holds; a new hash replaces the old one.
    #[test]
    fn put_peers_merges_and_keeps_cached_hash() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let s = SessionStore::open(tmp.path()).expect("open");
        let user = |bot, is_self, access_hash| Peer {
            id: 7,
            kind: PeerKind::User { bot, is_self },
            access_hash,
        };
        let chan = |kind, access_hash| Peer {
            id: 99,
            kind: PeerKind::Channel { kind },
            access_hash,
        };
        s.put_peers(&[
            user(Some(true), Some(false), Some(11)),
            chan(Some(ChannelKind::Broadcast), Some(5)),
        ])
        .expect("first");
        s.put_peers(&[user(None, None, None), chan(None, None)])
            .expect("hashless rows");
        assert_eq!(
            s.peer(7).expect("user"),
            Some(user(Some(true), Some(false), Some(11)))
        );
        assert_eq!(
            s.peer(-1_000_000_000_099).expect("channel"),
            Some(chan(Some(ChannelKind::Broadcast), Some(5)))
        );
        s.put_peers(&[user(Some(false), None, Some(12))])
            .expect("new hash");
        assert_eq!(
            s.peer(7).expect("user"),
            Some(user(Some(false), Some(false), Some(12)))
        );
        let st = s.stored_state().expect("stored");
        assert_eq!(st.known_users, vec![7]);
    }

    /// `commit_state` writes only the fields a delta names, plus its
    /// channel rows, and clears the behind rows it reports caught up;
    /// `stored_state` reads them back.
    #[test]
    fn commit_state_applies_delta() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let s = SessionStore::open(tmp.path()).expect("open");
        s.set_update_state(UpdateState {
            pts: 1,
            qts: 2,
            seq: 3,
            date: 4,
        })
        .expect("state");
        s.mark_channels_behind(&[5, 6, 7]).expect("behind");
        s.mark_channels_behind(&[]).expect("none");
        s.commit_state(&StateDelta {
            pts: Some(10),
            seq: Some(30),
            channels: vec![(5, 50), (6, 60)],
            caught_up: vec![5, 7],
            ..StateDelta::default()
        })
        .expect("commit");
        drop(s);
        let s = SessionStore::open(tmp.path()).expect("reopen");
        let st = s.stored_state().expect("stored");
        assert_eq!(
            st.common,
            UpdateState {
                pts: 10,
                qts: 2,
                seq: 30,
                date: 4
            }
        );
        assert_eq!(st.channels, vec![(5, 50), (6, 60)]);
        assert_eq!(st.behind, vec![6]);
        assert!(st.known_users.is_empty());
    }

    /// A directory and files that already exist with wider modes are
    /// narrowed by `open`, not only created narrow.
    #[cfg(unix)]
    #[test]
    fn files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("acct");
        let set = |p: &Path, mode: u32| {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).expect("chmod");
        };
        let mode = |p: &Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
        std::fs::create_dir(&dir).expect("mkdir");
        set(&dir, 0o755);
        for file in ["lock", "session.redb"] {
            std::fs::File::create(dir.join(file)).expect("create");
            set(&dir.join(file), 0o644);
            assert_eq!(mode(&dir.join(file)), 0o644);
        }
        assert_eq!(mode(&dir), 0o755);
        let _s = SessionStore::open(&dir).expect("open");
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join("session.redb")), 0o600);
        assert_eq!(mode(&dir.join("lock")), 0o600);
    }
}
