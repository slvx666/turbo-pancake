//! turbo-pancake application server.
//!
//! S0 provides health/readiness/version endpoints and a development signaling
//! relay (`/v1/signal`); S1 adds the media SFU (`turbo-pancake-media-sfu`): room members
//! send audio to the server over UDP (same port number as the WebSocket listener) or
//! tunneled through their signaling connection. S5 adds the messenger (ADR 0013):
//! accounts and sessions (`auth`), direct conversations and messages (`chat`) in
//! PostgreSQL, events and presence over the same WebSocket (`events`).

pub mod api;
pub mod auth;
pub mod chat;
pub mod events;
pub mod ratelimit;
pub mod signaling;
pub mod tls;

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post, put};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use turbo_pancake_media_sfu::{Sfu, SfuConfig};

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
    /// UDP media socket. `false` = media only through the WebSocket tunnel.
    pub media_udp: bool,
    /// UDP media port; `None` = the WebSocket listener's port number.
    pub media_udp_port: Option<u16>,
    /// Extra addresses announced as UDP candidates (e.g. the public IP behind a port
    /// forward when clients dial a host name). The address a client dialed and the
    /// default-route address are always announced.
    pub public_ips: Vec<IpAddr>,
    /// High-layer screen share budget to viewers, kbit/s (`None` = unlimited).
    pub max_high_egress_kbps: Option<u32>,
    /// PostgreSQL URL of the messenger; `None` = calls only, messenger API answers 503.
    pub database_url: Option<String>,
    /// Database connections in the pool.
    pub database_pool: u32,
    /// `Hello` without a token may join rooms (development: loopback servers, S1–S4
    /// tests). Off for servers reachable from the network.
    pub anonymous_calls: bool,
    pub limits: ratelimit::Limits,
    /// HTTPS + WSS on the listening port (ADR 0014); `None` = plain HTTP.
    pub tls: Option<Arc<turbo_pancake_tls::rustls::ServerConfig>>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            heartbeat_timeout: Duration::from_secs(30),
            heartbeat_interval: Duration::from_secs(10),
            max_room_peers: 100,
            outbound_queue: 256,
            max_connections: 2048,
            media_udp: true,
            media_udp_port: None,
            public_ips: Vec::new(),
            max_high_egress_kbps: None,
            database_url: None,
            database_pool: 16,
            anonymous_calls: true,
            limits: ratelimit::Limits::default(),
            tls: None,
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub hub: Arc<signaling::Hub>,
    pub connections: Arc<Semaphore>,
    /// Media forwarding; `None` only if it failed to start.
    pub sfu: Option<Arc<Sfu>>,
    /// Default-route address of this machine, announced as a UDP candidate.
    pub lan_ip: Option<IpAddr>,
    /// Messenger database (`None` = messenger disabled).
    pub db: Option<sqlx::PgPool>,
    pub registry: Arc<events::Registry>,
    pub limiters: Arc<ratelimit::Limiters>,
    /// Becomes `true` when the server shuts down: WebSocket connections end.
    pub shutdown: tokio::sync::watch::Receiver<bool>,
    shutdown_tx: Arc<tokio::sync::watch::Sender<bool>>,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        let connections = Arc::new(Semaphore::new(config.max_connections));
        let limiters = Arc::new(ratelimit::Limiters::new(&config.limits));
        let (shutdown_tx, shutdown) = tokio::sync::watch::channel(false);
        Self {
            shutdown,
            shutdown_tx: Arc::new(shutdown_tx),
            config: Arc::new(config),
            hub: Arc::new(signaling::Hub::default()),
            connections,
            sfu: None,
            lan_ip: None,
            db: None,
            registry: Arc::new(events::Registry::default()),
            limiters,
        }
    }

    /// Connects to the messenger database and applies pending migrations.
    pub async fn with_database(mut self) -> Result<Self, sqlx::Error> {
        if let Some(url) = &self.config.database_url {
            let pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(self.config.database_pool)
                .acquire_timeout(Duration::from_secs(5))
                .connect(url)
                .await?;
            MIGRATOR.run(&pool).await?;
            tracing::info!("messenger database ready");
            self.db = Some(pool);
        }
        Ok(self)
    }

    /// Starts the SFU: UDP on `port` when enabled; a UDP bind failure leaves the tunnel.
    pub fn with_media(mut self, ip: IpAddr, port: u16) -> Self {
        let udp_port = self.config.media_udp_port.unwrap_or(port);
        let udp_ip = if ip.is_ipv4() {
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        } else {
            IpAddr::V6(Ipv6Addr::UNSPECIFIED)
        };
        let mut cfg = SfuConfig {
            udp_bind: self
                .config
                .media_udp
                .then_some(SocketAddr::new(udp_ip, udp_port)),
            max_high_egress_kbps: self.config.max_high_egress_kbps,
        };
        let sfu = Sfu::start(cfg.clone()).or_else(|e| {
            tracing::warn!(port = udp_port, error = %e, "UDP media unavailable, tunnel only");
            cfg.udp_bind = None;
            Sfu::start(cfg)
        });
        match sfu {
            Ok(sfu) => {
                tracing::info!(udp_port = ?sfu.udp_port(), "media SFU started");
                self.sfu = Some(Arc::new(sfu));
            }
            Err(e) => tracing::error!(error = %e, "media SFU failed to start"),
        }
        self.lan_ip = turbo_pancake_media_transport::default_route_ip(ip.is_ipv6()).ok();
        self
    }
}

/// Schema migrations (`migrations/`), embedded in the binary.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        .route("/v1/version", get(version))
        .route("/v1/media-stats", get(media_stats))
        .route("/v1/signal", get(signaling::ws_handler))
        // Messenger API (ADR 0013, docs/API.md).
        .route("/v1/auth/register", post(auth::register))
        .route("/v1/auth/login", post(auth::login))
        .route("/v1/auth/refresh", post(auth::refresh))
        .route("/v1/auth/logout", post(auth::logout))
        .route("/v1/me", get(auth::me).patch(auth::update_me))
        .route("/v1/me/password", post(auth::change_password))
        .route("/v1/sessions", get(auth::list_sessions))
        .route("/v1/sessions/{id}", delete(auth::revoke_session))
        .route("/v1/users/lookup", get(chat::lookup))
        .route("/v1/blocks", get(chat::list_blocks))
        .route("/v1/blocks/{id}", put(chat::block).delete(chat::unblock))
        .route("/v1/conversations", get(chat::list_conversations))
        .route("/v1/conversations/direct", post(chat::create_direct))
        .route(
            "/v1/conversations/{id}/messages",
            get(chat::history).post(chat::send_message),
        )
        .route("/v1/conversations/{id}/changes", get(chat::changes))
        .route(
            "/v1/conversations/{id}/messages/{message_id}",
            axum::routing::patch(chat::edit_message).delete(chat::delete_message),
        )
        .route("/v1/conversations/{id}/read", post(chat::mark_read))
        .with_state(state)
}

async fn readyz(State(state): State<AppState>) -> impl IntoResponse {
    if state.connections.available_permits() > 0 {
        (StatusCode::OK, "ready")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "connection limit reached")
    }
}

/// SFU counters (numbers only, no member data) for diagnostics and load tests.
async fn media_stats(State(state): State<AppState>) -> impl IntoResponse {
    let Some(sfu) = &state.sfu else {
        return (StatusCode::SERVICE_UNAVAILABLE, "media unavailable").into_response();
    };
    let s = sfu.stats();
    axum::Json(serde_json::json!({
        "peers": s.peers,
        "tunnel_peers": s.tunnel_peers,
        "connected_peers": s.connected_peers,
        "rtp_in": s.rtp_in,
        "rtp_out": s.rtp_out,
        "streams_out": s.streams_out,
        "udp_in": s.udp_in,
        "udp_out": s.udp_out,
        "udp_send_errors": s.udp_send_errors,
        "tunnel_in": s.tunnel_in,
        "tunnel_out": s.tunnel_out,
        "dropped_commands": s.dropped_commands,
    }))
    .into_response()
}

async fn version(State(state): State<AppState>) -> impl IntoResponse {
    axum::Json(serde_json::json!({
        "server_version": SERVER_VERSION,
        "protocol_version": turbo_pancake_protocol::PROTOCOL_VERSION,
        "messenger": state.db.is_some(),
    }))
}

/// Serves until `shutdown` resolves; returns after in-flight connections end.
pub async fn serve(
    listener: TcpListener,
    config: Config,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let local = listener.local_addr()?;
    let tls = config.tls.clone();
    let state = AppState::new(config)
        .with_database()
        .await
        .map_err(|e| std::io::Error::other(format!("messenger database: {e}")))?
        .with_media(local.ip(), local.port());
    let sfu = state.sfu.clone();
    let stop_connections = state.shutdown_tx.clone();
    let app = router(state).into_make_service_with_connect_info::<SocketAddr>();
    let stop = async move {
        shutdown.await;
        // Long-lived WebSocket connections would hold the graceful shutdown forever.
        let _ = stop_connections.send(true);
    };
    match tls {
        Some(cfg) => {
            use axum::serve::ListenerExt;
            // `tap_io` gives any listener axum's `ConnectInfo<SocketAddr>` support.
            let listener = tls::TlsListener::new(listener, cfg)?.tap_io(|_| {});
            axum::serve(listener, app)
                .with_graceful_shutdown(stop)
                .await?
        }
        None => {
            axum::serve(listener, app)
                .with_graceful_shutdown(stop)
                .await?
        }
    }
    if let Some(sfu) = sfu {
        sfu.stop();
    }
    Ok(())
}
