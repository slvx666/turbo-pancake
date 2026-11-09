//! The HTTP connection loop (S8 step 4). `axum::serve` sets no timer, so hyper never
//! times out a client that sends its request headers slowly or not at all (checked: a
//! partial request stayed open > 45 s); with the connection cap such clients could hold
//! every slot. This loop is axum's (HTTP/1.1 with upgrades for WebSockets, graceful
//! shutdown that lets in-flight requests finish) plus `header_read_timeout`: a connection
//! whose request headers are not complete in time is closed.

use std::future::Future;
use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum::extract::ConnectInfo;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::sync::watch;

pub async fn serve<L>(
    mut listener: L,
    app: Router,
    shutdown: impl Future<Output = ()>,
    header_read_timeout: Duration,
) where
    L: axum::serve::Listener<Addr = SocketAddr>,
    L::Io: Send + Unpin + 'static,
{
    // Dropped sender = "shut down"; every connection task holds a receiver of `done`, so
    // `done_tx.closed()` resolves when the last one ended.
    let (stop_tx, stop_rx) = watch::channel(());
    let (done_tx, done_rx) = watch::channel(());
    let mut shutdown = std::pin::pin!(shutdown);
    loop {
        let (io, addr) = tokio::select! {
            c = listener.accept() => c,
            () = &mut shutdown => break,
        };
        let app = app.clone();
        let service = hyper::service::service_fn(move |mut req: hyper::Request<Incoming>| {
            // The caller's address for rate limits and the security log (`ClientIp`).
            req.extensions_mut().insert(ConnectInfo(addr));
            let mut app = app.clone();
            // A `Router` is always ready, so `call` without `poll_ready` is fine.
            async move { tower_service::Service::call(&mut app, req).await }
        });
        let mut stop = stop_rx.clone();
        let done = done_rx.clone();
        tokio::spawn(async move {
            let conn = http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(header_read_timeout)
                .serve_connection(TokioIo::new(io), service)
                .with_upgrades();
            let mut conn = std::pin::pin!(conn);
            let mut stopping = false;
            loop {
                tokio::select! {
                    r = conn.as_mut() => {
                        if let Err(e) = r {
                            tracing::debug!(%addr, error = %e, "connection ended with an error");
                        }
                        break;
                    }
                    _ = stop.changed(), if !stopping => {
                        stopping = true;
                        conn.as_mut().graceful_shutdown();
                    }
                }
            }
            drop(done);
        });
    }
    // Stop accepting, ask every connection to finish its request and close, then wait.
    drop(listener);
    drop(stop_rx);
    drop(stop_tx);
    drop(done_rx);
    done_tx.closed().await;
}
