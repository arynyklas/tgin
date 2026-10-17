//! Hardened HTTP server and client.
//!
//! [`serve`] runs an axum [`Router`](axum::Router) on one or more listeners
//! with a header-read timeout, a connection cap and optional hot-reloadable
//! TLS. [`client`] builds the outbound `reqwest::Client`, and
//! [`describe_error`] renders its errors without the request URL, which can
//! carry a bot token. [`bearer_matches`] checks an `Authorization: Bearer`
//! header in constant time; [`Via`] detects requests that loop back.

mod accept;
mod auth;
mod client;
mod server;
mod via;

pub use auth::bearer_matches;
pub use client::{ClientOptions, client, describe_error};
pub use server::{RunningServer, ServerOptions, ServerTls, TlsFiles, serve};
pub use via::{Via, refuse_loops};
