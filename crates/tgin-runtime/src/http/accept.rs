//! Accepting connections for `axum_server`.
//!
//! A tower concurrency limit on the router only bounds in-flight requests, so
//! idle keep-alive connections would still pile up. [`ConnLimit`] takes a
//! semaphore permit per accepted connection and keeps it inside the stream, so
//! the permit is released only when the connection is dropped. It also turns
//! Nagle's algorithm off: a response written in several pieces (a pull batch
//! is one write per update) would otherwise wait for the client's delayed ACK
//! before its later pieces go out.

use std::future::{Ready, ready};
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum_server::accept::Accept;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Admits a connection only while a permit is free; otherwise the connection
/// is closed as soon as it is accepted. Admitted connections have
/// `TCP_NODELAY` set; if setting it fails, the connection is refused, as
/// `axum_server`'s own acceptors do (on Linux it cannot fail on a live TCP
/// socket).
#[derive(Clone)]
pub(crate) struct ConnLimit {
    permits: Arc<Semaphore>,
}

impl ConnLimit {
    pub(crate) fn new(permits: Arc<Semaphore>) -> Self {
        Self { permits }
    }
}

impl<S> Accept<TcpStream, S> for ConnLimit {
    type Stream = Permitted<TcpStream>;
    type Service = S;
    type Future = Ready<io::Result<(Self::Stream, Self::Service)>>;

    fn accept(&self, stream: TcpStream, service: S) -> Self::Future {
        let Ok(permit) = Arc::clone(&self.permits).try_acquire_owned() else {
            return ready(Err(io::Error::other("connection limit reached")));
        };
        ready(stream.set_nodelay(true).map(|()| {
            (
                Permitted {
                    inner: stream,
                    _permit: permit,
                },
                service,
            )
        }))
    }
}

/// A stream that holds a connection permit for as long as it lives.
pub(crate) struct Permitted<S> {
    inner: S,
    _permit: OwnedSemaphorePermit,
}

impl<S: AsyncRead + Unpin> AsyncRead for Permitted<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Permitted<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

#[cfg(test)]
mod tests {
    use tokio::net::{TcpListener, TcpStream};

    use super::*;

    async fn accepted() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (client, server) = tokio::join!(TcpStream::connect(addr), listener.accept());
        (client.expect("connect"), server.expect("accept").0)
    }

    /// Responses go out as several writes (the pull path writes each update
    /// as it is); with Nagle on, a write after the first waits for the
    /// client's delayed ACK, up to 40 ms on Linux, per response.
    #[tokio::test]
    async fn accepted_connections_have_nagle_off() {
        let (_client, server) = accepted().await;
        assert!(
            !server.nodelay().expect("nodelay"),
            "the OS default is Nagle on"
        );
        let limit = ConnLimit::new(Arc::new(Semaphore::new(1)));
        let (stream, ()) = limit.accept(server, ()).await.expect("admitted");
        assert!(stream.inner.nodelay().expect("nodelay"));
    }

    #[tokio::test]
    async fn connections_beyond_the_limit_are_refused() {
        let limit = ConnLimit::new(Arc::new(Semaphore::new(1)));
        let (_c1, s1) = accepted().await;
        let (_c2, s2) = accepted().await;
        let kept = limit.accept(s1, ()).await.expect("first admitted");
        assert!(limit.accept(s2, ()).await.is_err());
        drop(kept);
        let (_c3, s3) = accepted().await;
        assert!(limit.accept(s3, ()).await.is_ok());
    }
}
