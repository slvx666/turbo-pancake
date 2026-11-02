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

    turbo_pancake_app_server::serve(listener, Default::default(), async {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("shutdown requested");
    })
    .await?;
    Ok(())
}
