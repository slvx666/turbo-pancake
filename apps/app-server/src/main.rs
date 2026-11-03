use std::net::SocketAddr;

use clap::Parser;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(version, about = "turbo-pancake application server")]
struct Args {
    /// Listen address. Defaults to loopback: S0 has no authentication.
    #[arg(long, env = "TURBO_PANCAKE_BIND", default_value = "127.0.0.1:7878")]
    bind: SocketAddr,
    /// Emit logs as JSON lines (for systemd/journald collection).
    #[arg(long, env = "TURBO_PANCAKE_LOG_JSON")]
    log_json: bool,
    /// UDP port for call media (default: same number as the listen port). Forward it on
    /// the router together with the TCP port for the lowest latency; without it media
    /// falls back to the WebSocket.
    #[arg(long, env = "TURBO_PANCAKE_MEDIA_PORT")]
    media_port: Option<u16>,
    /// Media only through the WebSocket (no UDP socket).
    #[arg(long, env = "TURBO_PANCAKE_NO_MEDIA_UDP")]
    no_media_udp: bool,
    /// Public address to announce for UDP media (repeatable or comma-separated), e.g.
    /// behind a port forward when clients connect by host name.
    #[arg(
        long = "public-ip",
        env = "TURBO_PANCAKE_PUBLIC_IP",
        value_delimiter = ','
    )]
    public_ips: Vec<std::net::IpAddr>,
    /// Budget for high-quality screen share streams to viewers, Mbit/s (sum of the
    /// watched shares' bitrates, spec §5.3 admission control). Viewers over it get the
    /// preview layer and are told why. Default: unlimited.
    #[arg(long, env = "TURBO_PANCAKE_MAX_HIGH_EGRESS_MBPS")]
    max_high_egress_mbps: Option<u32>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    if args.log_json {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(filter)
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }

    if !args.bind.ip().is_loopback() {
        tracing::warn!(
            bind = %args.bind,
            "binding to a non-loopback address; S0 signaling is unauthenticated"
        );
    }

    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    // Machine-readable line used by the benchmark harness and tests.
    println!("TURBO_PANCAKE_LISTENING {}", listener.local_addr()?);
    tracing::info!(addr = %listener.local_addr()?, version = turbo_pancake_app_server::SERVER_VERSION, "listening");

    let config = turbo_pancake_app_server::Config {
        media_udp: !args.no_media_udp,
        media_udp_port: args.media_port,
        public_ips: args.public_ips,
        max_high_egress_kbps: args.max_high_egress_mbps.map(|m| m.saturating_mul(1000)),
        ..Default::default()
    };
    turbo_pancake_app_server::serve(listener, config, async {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("shutdown requested");
    })
    .await?;
    Ok(())
}
