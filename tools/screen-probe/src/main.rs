//! Screen capture + hardware encode probe (S3 step 1, spec §5, §14.3).
//!
//! `list` prints monitors and hardware video encoders/decoders; `run` captures one monitor,
//! converts and scales it on the GPU and encodes it with the hardware H.264 encoder at a
//! fixed frame rate, then prints FPS, latency and bitrate as JSON. No pixels are written
//! anywhere. Windows only for now; other systems report UNVERIFIED.

use clap::{Parser, Subcommand};

#[cfg(windows)]
mod pattern;
#[cfg(windows)]
mod win;

#[derive(Parser)]
#[command(about = "Screen capture + hardware encode probe")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List monitors and hardware video codecs.
    List,
    /// Capture and encode a monitor for a while, print measurements.
    Run(RunArgs),
}

#[derive(clap::Args, Debug, Clone)]
pub struct RunArgs {
    /// Monitor index from `list`.
    #[arg(long)]
    pub monitor: usize,
    #[arg(long, default_value_t = 10)]
    pub seconds: u64,
    #[arg(long, default_value_t = 1920)]
    pub width: u32,
    #[arg(long, default_value_t = 1080)]
    pub height: u32,
    #[arg(long, default_value_t = 60)]
    pub fps: u32,
    #[arg(long, default_value_t = 8000)]
    pub bitrate_kbps: u32,
    /// Keyframe interval in seconds.
    #[arg(long, default_value_t = 2)]
    pub gop_seconds: u32,
    /// Draw a moving test pattern (a window that never takes focus) on the captured monitor,
    /// so the capture sees new content every refresh.
    #[arg(long)]
    pub pattern: bool,
    /// Also write the JSON report to this file.
    #[arg(long)]
    pub out: Option<std::path::PathBuf>,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let report = run(cli.command)?;
    let text = serde_json::to_string_pretty(&report)?;
    println!("{text}");
    Ok(())
}

#[cfg(windows)]
fn run(command: Command) -> anyhow::Result<serde_json::Value> {
    match command {
        Command::List => win::list(),
        Command::Run(args) => {
            let report = win::run(&args)?;
            if let Some(path) = &args.out {
                std::fs::write(path, serde_json::to_string_pretty(&report)?)?;
            }
            Ok(report)
        }
    }
}

#[cfg(not(windows))]
fn run(_command: Command) -> anyhow::Result<serde_json::Value> {
    Ok(serde_json::json!({
        "os": std::env::consts::OS,
        "status": "UNVERIFIED",
        "note": "capture/encode probe is implemented for Windows only (S3 step 1); \
                 macOS (ScreenCaptureKit + VideoToolbox) and Linux (PipeWire portal + VA-API) follow",
    }))
}
