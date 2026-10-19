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
//! back. [`updates`] is tgin's own update-state handling and gap recovery
//! over those bytes.
//!
//! [`login`] logs the account in over raw TL (QR, code, 2FA) and out.
//!
//! A [`Sender`] holds one connection to one DC and gives every request an
//! honest outcome: answered, [`SenderError::NotSent`] (never written, one
//! resend allowed) or [`SenderError::NoAnswer`] (written, never resent).

mod dc;
pub mod decode;
pub mod envelope;
mod error;
mod lock;
pub mod login;
pub mod media;
pub mod rpc;
pub mod schema;
mod sender;
mod session;
mod sink;
#[cfg(test)]
mod testgen;
#[cfg(any(test, feature = "testkit"))]
pub mod testkit;
pub mod tl_json;
mod transport;
pub mod updates;

pub use dc::DcOption;
pub use error::SessionError;
pub use schema::TlError;
pub use sender::{Connected, InitParams, MAX_REQUEST, Pushes, RpcOutcome, Sender, SenderError};
pub use session::{AuthKey, ChannelKind, DcAddr, Login, Peer, PeerKind, SessionStore, UpdateState};
pub use sink::{Admission, BatchSink, ID_BLOCK, IdsError, UpdateIds};
pub use transport::{
    GeneratedKey, Opened, PacketReader, PacketWriter, TcpTransport, Transport, TransportError,
};
pub use updates::{Batch, Dropped, Fetch, StateDelta, StoredState, UpdateBytes, UpdateManager};
