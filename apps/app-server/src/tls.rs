//! HTTPS / WSS on the listening port (ADR 0014): an axum `Listener` whose TLS handshakes
//! run in their own tasks with a timeout, so a slow or silent client never stalls the
//! accept loop; at most `MAX_HANDSHAKES` run at once.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, mpsc};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;
use turbo_pancake_tls::rustls::ServerConfig;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_HANDSHAKES: usize = 256;

pub struct TlsListener {
    rx: mpsc::Receiver<(TlsStream<TcpStream>, SocketAddr)>,
    local: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl TlsListener {
    pub fn new(tcp: TcpListener, config: Arc<ServerConfig>) -> std::io::Result<Self> {
        let local = tcp.local_addr()?;
        let (tx, rx) = mpsc::channel(64);
        let acceptor = TlsAcceptor::from(config);
        let slots = Arc::new(Semaphore::new(MAX_HANDSHAKES));
        let task = tokio::spawn(async move {
            loop {
                let (stream, addr) = match tcp.accept().await {
                    Ok(c) => c,
                    Err(e) => {
                        // Out of descriptors and the like: back off instead of spinning.
                        tracing::warn!(error = %e, "accept failed");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                };
                if tx.is_closed() {
                    break;
                }
                let Ok(permit) = slots.clone().try_acquire_owned() else {
                    // Handshake flood: drop the connection.
                    continue;
                };
                let (acceptor, tx) = (acceptor.clone(), tx.clone());
                tokio::spawn(async move {
                    let _permit = permit;
                    match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
                        Ok(Ok(tls)) => {
                            let _ = tx.send((tls, addr)).await;
                        }
                        Ok(Err(e)) => tracing::debug!(%addr, error = %e, "TLS handshake failed"),
                        Err(_) => tracing::debug!(%addr, "TLS handshake timed out"),
                    }
                });
            }
        });
        Ok(Self { rx, local, task })
    }
}

impl Drop for TlsListener {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.rx.recv().await {
            Some(c) => c,
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(self.local)
    }
}
