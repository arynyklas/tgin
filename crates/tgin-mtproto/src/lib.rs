//! tgin's user-account (MTProto) engine. This is the only workspace crate
//! that may depend on a grammers crate (`cargo xtask grammers-confinement`).
//!
//! A session directory belongs to one process: [`SessionStore::open`] holds
//! the OS file lock on `{dir}/lock` for the store's lifetime. The lock is
//! per OS instance, so never share a session directory across hosts, bind
//! mounts or network filesystems.
//!
//! The account serves one layer, 230 ([`schema::API`]). [`decode`] reads the
//! routing and update-state facts of Telegram's bytes straight from that
//! schema; [`tl_json`] renders the same bytes as TL-JSON (design §10.5) and
//! back.

pub mod decode;
mod error;
mod lock;
pub mod schema;
mod session;
#[cfg(test)]
mod testgen;
pub mod tl_json;

pub use error::SessionError;
pub use schema::TlError;
pub use session::{AuthKey, ChannelKind, DcAddr, Login, Peer, PeerKind, SessionStore, UpdateState};
