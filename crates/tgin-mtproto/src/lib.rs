//! tgin's user-account (MTProto) engine. This is the only workspace crate
//! that may depend on a grammers crate (`cargo xtask grammers-confinement`).
//!
//! A session directory belongs to one process: [`SessionStore::open`] holds
//! the OS file lock on `{dir}/lock` for the store's lifetime. The lock is
//! per OS instance, so never share a session directory across hosts, bind
//! mounts or network filesystems.

mod error;
mod lock;
mod session;

pub use error::SessionError;
pub use session::{AuthKey, ChannelKind, DcAddr, Login, Peer, PeerKind, SessionStore, UpdateState};
