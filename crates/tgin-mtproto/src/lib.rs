//! tgin's user-account (MTProto) engine. This is the only workspace crate
//! that may depend on a grammers crate (`cargo xtask grammers-confinement`).
//!
//! A session directory belongs to one process: [`SessionStore::open`] holds
//! the OS file lock on `{dir}/lock` for the store's lifetime. The lock is
//! per OS instance, so never share a session directory across hosts, bind
//! mounts or network filesystems.
//!
//! A [`Sender`] holds one connection to one DC and gives every request an
//! honest outcome: answered, [`SenderError::NotSent`] (never written, one
//! resend allowed) or [`SenderError::NoAnswer`] (written, never resent).

mod dc;
mod error;
mod lock;
mod sender;
mod session;
mod transport;

pub use dc::DcOption;
pub use error::SessionError;
pub use sender::{InitParams, RpcOutcome, Sender, SenderError};
pub use session::{AuthKey, ChannelKind, DcAddr, Login, Peer, PeerKind, SessionStore, UpdateState};
pub use transport::{
    GeneratedKey, Opened, PacketReader, PacketWriter, TcpTransport, Transport, TransportError,
};
