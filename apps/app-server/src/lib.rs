//! DsMini application server.
//!
//! S0 provides health/readiness/version endpoints and a development signaling
//! relay (`/v1/signal`). Authentication, PostgreSQL and the messenger domain are
//! added in S5; until then the server binds to loopback by default and must not
//! be exposed publicly.

pub mod signaling;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Debug)]
pub struct Config {
    /// A connection that sends no frame for this long is dropped.
    pub heartbeat_timeout: Duration,
    /// Interval advertised to clients in `Welcome` (must be < heartbeat_timeout).
    pub heartbeat_interval: Duration,
    /// Maximum peers in one room (group chat limit from the spec).
    pub max_room_peers: usize,
    /// Bounded per-connection outbound queue; a consumer that falls this far
    /// behind is disconnected instead of growing memory without bound.
    pub outbound_queue: usize,
    /// Maximum simultaneous WebSocket connections.
    pub max_connections: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            heartbeat_timeout: Duration::from_secs(30),
            heartbeat_interval: Duration::from_secs(10),
            max_room_peers: 100,
            outbound_queue: 256,
            max_connections: 2048,
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub hub: Arc<signaling::Hub>,
    pub connections: Arc<Semaphore>,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        let connections = Arc::new(Semaphore::new(config.max_connections));
        Self {
            config: Arc::new(config),
            hub: Arc::new(signaling::Hub::default()),
            connections,
        }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        .route("/v1/version", get(version))
        .route("/v1/signal", get(signaling::ws_handler))
        .with_state(state)
}

async fn readyz(State(state): State<AppState>) -> impl IntoResponse {
    if state.connections.available_permits() > 0 {
        (StatusCode::OK, "ready")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "connection limit reached")
    }
}

async fn version() -> impl IntoResponse {
    axum::Json(serde_json::json!({
        "server_version": SERVER_VERSION,
        "protocol_version": dsmini_protocol::PROTOCOL_VERSION,
    }))
}

/// Serves until `shutdown` resolves; returns after in-flight connections end.
pub async fn serve(
    listener: TcpListener,
    config: Config,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let app = router(AppState::new(config));
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await
}
