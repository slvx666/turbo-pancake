//! turbo-pancake application server.
//!
//! S0 provides health/readiness/version endpoints and a development signaling
//! relay (`/v1/signal`); S1 adds the media SFU (`turbo-pancake-media-sfu`): room members
//! send audio to the server over UDP (same port number as the WebSocket listener) or
//! tunneled through their signaling connection. Authentication, PostgreSQL and the messenger domain are
//! added in S5; until then the server binds to loopback by default and must not
//! be exposed publicly.

pub mod signaling;

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
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
}

impl AppState {
    pub fn new(config: Config) -> Self {
        let connections = Arc::new(Semaphore::new(config.max_connections));
        Self {
            config: Arc::new(config),
            hub: Arc::new(signaling::Hub::default()),
            connections,
            sfu: None,
            lan_ip: None,
        }
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

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        .route("/v1/version", get(version))
        .route("/v1/media-stats", get(media_stats))
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

async fn version() -> impl IntoResponse {
    axum::Json(serde_json::json!({
        "server_version": SERVER_VERSION,
        "protocol_version": turbo_pancake_protocol::PROTOCOL_VERSION,
    }))
}

/// Serves until `shutdown` resolves; returns after in-flight connections end.
pub async fn serve(
    listener: TcpListener,
    config: Config,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let local = listener.local_addr()?;
    let state = AppState::new(config).with_media(local.ip(), local.port());
    let sfu = state.sfu.clone();
    let app = router(state);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await?;
    if let Some(sfu) = sfu {
        sfu.stop();
    }
    Ok(())
}
