//! Connection cap for `axum_server`.
//!
//! A tower concurrency limit on the router only bounds in-flight requests, so
//! idle keep-alive connections would still pile up. [`ConnLimit`] takes a
//! semaphore permit per accepted connection and keeps it inside the stream, so
//! the permit is released only when the connection is dropped.

use std::future::{Ready, ready};
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum_server::accept::Accept;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Admits a connection only while a permit is free; otherwise the connection
/// is closed as soon as it is accepted.
#[derive(Clone)]
pub(crate) struct ConnLimit {
    permits: Arc<Semaphore>,
}

impl ConnLimit {
    pub(crate) fn new(permits: Arc<Semaphore>) -> Self {
        Self { permits }
    }
}

impl<I, S> Accept<I, S> for ConnLimit {
    type Stream = Permitted<I>;
    type Service = S;
    type Future = Ready<io::Result<(Self::Stream, Self::Service)>>;

    fn accept(&self, stream: I, service: S) -> Self::Future {
        ready(match Arc::clone(&self.permits).try_acquire_owned() {
            Ok(permit) => Ok((
                Permitted {
                    inner: stream,
                    _permit: permit,
                },
                service,
            )),
            Err(_) => Err(io::Error::other("connection limit reached")),
        })
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
