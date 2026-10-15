//! HTTP/1.1 server built on `axum_server`, hardened for public ingress.
//!
//! `axum_server` sets no hyper timer, so hyper silently skips its header-read
//! timeout; [`serve`] installs [`TokioTimer`] and an explicit timeout. The
//! server is HTTP/1 only because hyper-util's auto (h1 + h2) builder sniffs
//! the protocol before the h1 timeout starts, so a client that never sends a
//! byte would hold its connection forever.
//!
//! Handlers can read the TCP peer's address with
//! [`ConnectInfo<SocketAddr>`](axum::extract::ConnectInfo).

use std::io;
use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum_server::tls_rustls::{RustlsAcceptor, RustlsConfig};
use axum_server::{Handle, Server};
use hyper_util::rt::TokioTimer;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

use super::accept::ConnLimit;

/// PEM files for the server certificate chain and its private key.
#[derive(Clone, Debug)]
pub struct TlsFiles {
    pub cert: PathBuf,
    pub key: PathBuf,
}

/// Listener and hardening settings for [`serve`].
#[derive(Clone, Debug)]
pub struct ServerOptions {
    /// Addresses to listen on; at least one is required.
    pub listen: Vec<SocketAddr>,
    /// Serve HTTPS with these files instead of plain HTTP.
    pub tls: Option<TlsFiles>,
    /// Open connections allowed across all listeners; further connections
    /// are closed as soon as they are accepted.
    pub max_connections: usize,
    /// How long a client may take to send a complete request head.
    pub header_read_timeout: Duration,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            listen: Vec::new(),
            tls: None,
            max_connections: 10_000,
            header_read_timeout: Duration::from_secs(10),
        }
    }
}

/// Servers started by [`serve`], one per listen address.
///
/// Dropping it leaves the listeners serving; stop them with
/// [`graceful_shutdown`](Self::graceful_shutdown) and [`wait`](Self::wait).
pub struct RunningServer {
    /// Bound addresses, in the order of [`ServerOptions::listen`].
    pub local_addrs: Vec<SocketAddr>,
    tls: Option<ServerTls>,
    permits: Arc<Semaphore>,
    max_connections: usize,
    /// One per listener. A graceful-shutdown signal latches in its `Handle`,
    /// so it takes effect even on a server that has not been polled yet.
    handles: Vec<Handle<SocketAddr>>,
    tasks: Vec<JoinHandle<()>>,
}

/// The certificate of a TLS server, shared by every listener's acceptor; a
/// reload swaps it for all of them. Cloning shares it.
#[derive(Clone)]
pub struct ServerTls(RustlsConfig);

impl ServerTls {
    /// Loads the certificate and key from `files`, which may differ from the
    /// files the server started with. New handshakes on every listener
    /// present the new certificate; open connections keep the old one.
    ///
    /// # Errors
    ///
    /// The files cannot be loaded; the previous certificate then stays in
    /// use.
    pub async fn reload(&self, files: &TlsFiles) -> io::Result<()> {
        let reloaded = load_tls(files).await?;
        self.0.reload_from_config(reloaded.get_inner());
        Ok(())
    }
}

/// Binds every address in `opts.listen`, then serves `router` on each, with
/// each request's TCP peer address as `ConnectInfo<SocketAddr>`.
///
/// All addresses are bound before any is served, so on error nothing is left
/// running.
///
/// # Errors
///
/// Fails if `opts` lists no address, `max_connections` is zero or above
/// [`Semaphore::MAX_PERMITS`], the TLS files cannot be loaded, or an address
/// cannot be bound.
pub async fn serve(router: Router, opts: &ServerOptions) -> io::Result<RunningServer> {
    if opts.listen.is_empty() {
        return Err(invalid_input("at least one listen address is required"));
    }
    if opts.max_connections == 0 || opts.max_connections > Semaphore::MAX_PERMITS {
        return Err(invalid_input("max_connections is out of range"));
    }

    let tls = match &opts.tls {
        Some(files) => Some(ServerTls(load_tls(files).await?)),
        None => None,
    };

    let listeners = opts
        .listen
        .iter()
        .map(|addr| {
            let listener = TcpListener::bind(addr)?;
            listener.set_nonblocking(true)?;
            Ok(listener)
        })
        .collect::<io::Result<Vec<_>>>()?;
    let local_addrs = listeners
        .iter()
        .map(TcpListener::local_addr)
        .collect::<io::Result<Vec<_>>>()?;
    let servers = listeners
        .into_iter()
        .map(axum_server::from_tcp)
        .collect::<io::Result<Vec<_>>>()?;

    let permits = Arc::new(Semaphore::new(opts.max_connections));
    let handles: Vec<Handle<SocketAddr>> = servers.iter().map(|_| Handle::new()).collect();
    let tasks = servers
        .into_iter()
        .zip(&handles)
        .map(|(server, handle)| {
            let server = server.handle(handle.clone());
            let limit = ConnLimit::new(Arc::clone(&permits));
            let make_service = router
                .clone()
                .into_make_service_with_connect_info::<SocketAddr>();
            // With a pre-bound listener and an axum make-service, `serve`
            // cannot fail.
            match &tls {
                Some(tls) => {
                    let acceptor = RustlsAcceptor::new(tls.0.clone()).acceptor(limit);
                    let server = harden(server.acceptor(acceptor), opts.header_read_timeout);
                    tokio::spawn(async move {
                        let _ = server.serve(make_service).await;
                    })
                }
                None => {
                    let server = harden(server.acceptor(limit), opts.header_read_timeout);
                    tokio::spawn(async move {
                        let _ = server.serve(make_service).await;
                    })
                }
            }
        })
        .collect();

    Ok(RunningServer {
        local_addrs,
        tls,
        permits,
        max_connections: opts.max_connections,
        handles,
        tasks,
    })
}

impl RunningServer {
    /// The server's certificate, to reload; `None` without TLS.
    pub fn tls(&self) -> Option<ServerTls> {
        self.tls.clone()
    }

    /// Stops accepting on every listener and lets open connections finish;
    /// connections still open after `grace` are closed.
    pub fn graceful_shutdown(&self, grace: Duration) {
        for handle in &self.handles {
            handle.graceful_shutdown(Some(grace));
        }
    }

    /// Waits until every listener has stopped.
    pub async fn wait(self) {
        for task in self.tasks {
            if let Err(err) = task.await
                && err.is_panic()
            {
                std::panic::resume_unwind(err.into_panic());
            }
        }
    }

    /// Connections currently admitted across all listeners.
    pub fn connection_count(&self) -> usize {
        self.max_connections - self.permits.available_permits()
    }
}

/// HTTP/1 only, with a timer so the header-read timeout actually applies.
///
/// Must run after `Server::acceptor`, which resets the HTTP/1-only flag and
/// would bring back protocol sniffing.
fn harden<A>(
    server: Server<SocketAddr, A>,
    header_read_timeout: Duration,
) -> Server<SocketAddr, A> {
    let mut server = server.http1_only();
    server
        .http_builder()
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(header_read_timeout);
    server
}

/// Loads the PEM files into a config that advertises only `http/1.1` over
/// ALPN. `axum_server`'s PEM loaders also offer `h2`, which this HTTP/1-only
/// server would accept and then fail to speak.
async fn load_tls(files: &TlsFiles) -> io::Result<RustlsConfig> {
    let loaded = RustlsConfig::from_pem_file(&files.cert, &files.key).await?;
    let mut config = (*loaded.get_inner()).clone();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    loaded.reload_from_config(Arc::new(config));
    Ok(loaded)
}

fn invalid_input(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loopback() -> Vec<SocketAddr> {
        vec![SocketAddr::from(([127, 0, 0, 1], 0))]
    }

    async fn serve_err(opts: ServerOptions) -> io::Error {
        match serve(Router::new(), &opts).await {
            Ok(_) => panic!("serve accepted {opts:?}"),
            Err(err) => err,
        }
    }

    #[tokio::test]
    async fn serve_rejects_empty_listen() {
        let err = serve_err(ServerOptions::default()).await;
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[tokio::test]
    async fn serve_rejects_out_of_range_max_connections() {
        for max_connections in [0, Semaphore::MAX_PERMITS + 1] {
            let err = serve_err(ServerOptions {
                listen: loopback(),
                max_connections,
                ..ServerOptions::default()
            })
            .await;
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        }
    }

    #[tokio::test]
    async fn plain_server_has_no_tls_to_reload() {
        let server = serve(
            Router::new(),
            &ServerOptions {
                listen: loopback(),
                ..ServerOptions::default()
            },
        )
        .await
        .unwrap();
        assert!(server.tls().is_none());
    }
}
