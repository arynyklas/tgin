//! Hardened HTTP server and client.
//!
//! [`serve`] runs an axum [`Router`](axum::Router) on one or more listeners
//! with a header-read timeout, a connection cap and optional hot-reloadable
//! TLS. [`client`] builds the outbound `reqwest::Client`, and
//! [`describe_error`] renders its errors without the request URL, which can
//! carry a bot token.

mod accept;
mod client;
mod server;

pub use client::{ClientOptions, client, describe_error};
pub use server::{RunningServer, ServerOptions, ServerTls, TlsFiles, serve};
