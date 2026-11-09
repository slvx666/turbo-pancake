//! Request and connection limits (S8 step 4).
//!
//! * [`request_timeout`]: a handler that runs longer than the configured time is
//!   cancelled and answered `503 Unavailable` + `Retry-After` (its database work rolls
//!   back with the dropped transaction). WebSocket upgrades answer at once, so sessions
//!   are not affected; neither is a response body that is still streaming.
//! * [`Limited`]: at most `max` accepted connections at once. The next `accept` waits
//!   for a free slot, so further clients queue in the OS backlog instead of taking file
//!   descriptors from the process (which also needs them for the database and media).

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::api::{ApiError, Code};

pub async fn request_timeout(State(limit): State<Duration>, req: Request, next: Next) -> Response {
    let path = req.uri().path().to_owned();
    match tokio::time::timeout(limit, next.run(req)).await {
        Ok(resp) => resp,
        Err(_) => {
            tracing::warn!(%path, ?limit, "request timed out");
            ApiError::new(Code::Unavailable, "request timed out, try again").into_response()
        }
    }
}

/// A listener that holds one semaphore permit per open connection.
pub struct Limited<L> {
    inner: L,
    slots: Arc<Semaphore>,
}

impl<L> Limited<L> {
    pub fn new(inner: L, max: usize) -> Self {
        Self {
            inner,
            slots: Arc::new(Semaphore::new(max.max(1))),
        }
    }
}

impl<L> axum::serve::Listener for Limited<L>
where
    L: axum::serve::Listener<Addr = SocketAddr>,
    L::Io: Unpin,
{
    type Io = Counted<L::Io>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let permit = self
            .slots
            .clone()
            .acquire_owned()
            .await
            .expect("connection slots never close");
        let (io, addr) = self.inner.accept().await;
        (
            Counted {
                io,
                _permit: permit,
            },
            addr,
        )
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

/// A connection that frees its slot when dropped.
pub struct Counted<T> {
    io: T,
    _permit: OwnedSemaphorePermit,
}

impl<T: AsyncRead + Unpin> AsyncRead for Counted<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for Counted<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.io).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.io).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }
}
