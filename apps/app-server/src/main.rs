use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(version, about = "turbo-pancake application server")]
struct Args {
    #[command(subcommand)]
    command: Option<Cmd>,
    /// Listen address. Defaults to loopback.
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
    /// PostgreSQL URL of the messenger (accounts, messages). Without it the server runs
    /// calls only. Keep credentials in the environment, not on the command line.
    #[arg(long, env = "TURBO_PANCAKE_DATABASE_URL", hide_env_values = true)]
    database_url: Option<String>,
    /// Allow joining calls without an account. Default: only on a loopback address.
    #[arg(long, env = "TURBO_PANCAKE_ANONYMOUS_CALLS")]
    anonymous_calls: Option<bool>,
    /// Server certificate chain (PEM, server certificate first) for HTTPS/WSS on the
    /// listen port. See `gen-cert` and docs/DEPLOY-TLS.md.
    #[arg(long, env = "TURBO_PANCAKE_TLS_CERT", requires = "tls_key")]
    tls_cert: Option<PathBuf>,
    /// Private key (PEM) of `--tls-cert`.
    #[arg(long, env = "TURBO_PANCAKE_TLS_KEY", requires = "tls_cert")]
    tls_key: Option<PathBuf>,
    /// Serve the messenger over plain HTTP on a non-loopback address (LAN trials only:
    /// passwords and tokens cross the network unencrypted).
    #[arg(long, env = "TURBO_PANCAKE_INSECURE_ALLOW_PLAINTEXT")]
    insecure_allow_plaintext: bool,
    /// Directory with a signed release from `turbo-pancake-update sign`
    /// (`manifest.json`, `files/<version>/…`), served at `/v1/update/…` (ADR 0016).
    #[arg(long, env = "TURBO_PANCAKE_UPDATE_DIR")]
    update_dir: Option<PathBuf>,
    /// Rate limits for load tests (`turbo-pancake-bench messenger`: hundreds of accounts
    /// from one address). Refused unless the server listens on a loopback address.
    #[arg(long, env = "TURBO_PANCAKE_LOAD_TEST_LIMITS")]
    load_test_limits: bool,
    /// Database connections in the pool (docs/ADMIN.md: size it with PostgreSQL's
    /// `max_connections`).
    #[arg(long, env = "TURBO_PANCAKE_DATABASE_POOL", default_value_t = 16)]
    database_pool: u32,
    /// Argon2id hashes at once (19 MiB each). Default: half the CPUs, 1..=4.
    #[arg(long, env = "TURBO_PANCAKE_HASH_CONCURRENCY")]
    hash_concurrency: Option<usize>,
    /// Logins/registrations that may wait for a hash slot; beyond it they get 503.
    #[arg(long, env = "TURBO_PANCAKE_HASH_QUEUE", default_value_t = 64)]
    hash_queue: usize,
    /// Open TCP/TLS connections at once (keep well below the file descriptor limit).
    #[arg(
        long,
        env = "TURBO_PANCAKE_MAX_TCP_CONNECTIONS",
        default_value_t = 4096
    )]
    max_tcp_connections: usize,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Check the messenger database (`TURBO_PANCAKE_DATABASE_URL`) for consistency, e.g.
    /// after a restore: prints a JSON report, exit code 1 when an invariant is broken.
    /// Read-only; does not run migrations.
    CheckDb,
    /// Create a private CA and a server certificate for these IP addresses / host names
    /// (writes ca.pem, ca.key, server.pem, server.key into --out). Give ca.pem to the
    /// clients (`--server-ca`), keep ca.key offline.
    GenCert {
        /// IP address or DNS name the clients dial (repeatable).
        #[arg(long = "name", required = true)]
        names: Vec<String>,
        #[arg(long, default_value = ".")]
        out: PathBuf,
        /// Validity of the server certificate in days (the CA gets 10 years).
        #[arg(long, default_value_t = 825)]
        days: i64,
    },
    /// Issue a new server certificate from the existing CA in --dir (rotation: clients
    /// keep their ca.pem).
    RenewCert {
        #[arg(long = "name", required = true)]
        names: Vec<String>,
        #[arg(long, default_value = ".")]
        dir: PathBuf,
        #[arg(long, default_value_t = 825)]
        days: i64,
    },
}

fn write_secret(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    // The key files are created new; restrict access with the OS tools of the server
    // (e.g. `icacls` / `chmod 600`) — see docs/DEPLOY-TLS.md.
    std::fs::write(path, text)
}

fn certificates(cmd: Cmd) -> anyhow::Result<()> {
    match cmd {
        Cmd::GenCert { names, out, days } => {
            std::fs::create_dir_all(&out)?;
            for f in ["ca.key", "server.key"] {
                anyhow::ensure!(
                    !out.join(f).exists(),
                    "{} exists: refusing to overwrite keys (use renew-cert)",
                    out.join(f).display()
                );
            }
            let pki = turbo_pancake_tls::generate(&names, days)?;
            std::fs::write(out.join("ca.pem"), &pki.ca_cert_pem)?;
            write_secret(&out.join("ca.key"), &pki.ca_key_pem)?;
            std::fs::write(out.join("server.pem"), &pki.server_chain_pem)?;
            write_secret(&out.join("server.key"), &pki.server_key_pem)?;
            println!(
                "wrote ca.pem (give to clients), ca.key (keep offline), server.pem, server.key to {}",
                out.display()
            );
        }
        Cmd::CheckDb => unreachable!("handled in main"),
        Cmd::RenewCert { names, dir, days } => {
            let ca = std::fs::read_to_string(dir.join("ca.pem"))?;
            let ca_key = std::fs::read_to_string(dir.join("ca.key"))?;
            let (chain, key) = turbo_pancake_tls::issue_server(&ca, &ca_key, &names, days)?;
            std::fs::write(dir.join("server.pem"), chain)?;
            write_secret(&dir.join("server.key"), &key)?;
            println!("wrote a new server.pem and server.key to {}", dir.display());
        }
    }
    Ok(())
}

async fn check_db(url: Option<&str>) -> anyhow::Result<()> {
    let url = url.ok_or_else(|| anyhow::anyhow!("set TURBO_PANCAKE_DATABASE_URL"))?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(url)
        .await?;
    let report = turbo_pancake_app_server::integrity::check(&pool).await?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    pool.close().await;
    if report["ok"] != true {
        std::process::exit(1);
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    match args.command {
        Some(Cmd::CheckDb) => return check_db(args.database_url.as_deref()).await,
        Some(cmd) => return certificates(cmd),
        None => {}
    }
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    if args.log_json {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(filter)
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }

    let tls = match (&args.tls_cert, &args.tls_key) {
        (Some(cert), Some(key)) => Some(turbo_pancake_tls::server_config(
            &std::fs::read(cert)?,
            &std::fs::read(key)?,
        )?),
        _ => None,
    };
    let loopback = args.bind.ip().is_loopback();
    if !loopback && tls.is_none() && args.database_url.is_some() && !args.insecure_allow_plaintext {
        anyhow::bail!(
            "refusing to serve accounts over plain HTTP on {}: use --tls-cert/--tls-key, bind \
             to loopback, or pass --insecure-allow-plaintext for a LAN trial",
            args.bind
        );
    }
    if args.load_test_limits && !loopback {
        anyhow::bail!(
            "--load-test-limits turns rate limits off: only allowed on a loopback address, not {}",
            args.bind
        );
    }
    let limits = if args.load_test_limits {
        tracing::warn!("load-test limits: rate limits are off (loopback only)");
        turbo_pancake_app_server::ratelimit::Limits::relaxed()
    } else {
        turbo_pancake_app_server::ratelimit::Limits::default()
    };
    let anonymous_calls = args.anonymous_calls.unwrap_or(loopback);
    if !loopback && anonymous_calls {
        tracing::warn!(bind = %args.bind, "anonymous calls are open to the network");
    }

    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    // Machine-readable line used by the benchmark harness and tests.
    println!("TURBO_PANCAKE_LISTENING {}", listener.local_addr()?);
    tracing::info!(
        addr = %listener.local_addr()?,
        tls = tls.is_some(),
        version = turbo_pancake_app_server::SERVER_VERSION,
        "listening"
    );

    let config = turbo_pancake_app_server::Config {
        media_udp: !args.no_media_udp,
        media_udp_port: args.media_port,
        public_ips: args.public_ips,
        max_high_egress_kbps: args.max_high_egress_mbps.map(|m| m.saturating_mul(1000)),
        database_url: args.database_url,
        anonymous_calls,
        tls,
        update_dir: args.update_dir,
        limits,
        database_pool: args.database_pool.max(1),
        hash_concurrency: args
            .hash_concurrency
            .unwrap_or_else(turbo_pancake_app_server::hashing::default_concurrency),
        hash_queue: args.hash_queue,
        max_tcp_connections: args.max_tcp_connections,
        ..Default::default()
    };
    turbo_pancake_app_server::serve(listener, config, shutdown_signal()).await?;
    tracing::info!("stopped");
    Ok(())
}

/// Ctrl+C everywhere; on Unix also SIGTERM, which systemd sends on `stop`/`restart`
/// (without it the service would be killed without a graceful shutdown).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "SIGTERM handler unavailable");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
    tracing::info!("shutdown requested");
}
