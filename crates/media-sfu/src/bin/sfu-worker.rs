//! The SFU as a process of its own, driven over its standard input and output
//! (`turbo_pancake_media_sfu::remote`). The server normally runs the same worker through
//! its own `sfu-worker` subcommand; this binary exists so the media crate can test the
//! process boundary without the server.
//!
//!   sfu-worker [--udp-bind <addr:port>] [--max-high-egress-kbps <n>]
//!
//! Nothing but protocol frames may be written to standard output.

use std::process::ExitCode;

use turbo_pancake_media_sfu::SfuConfig;
use turbo_pancake_media_sfu::remote::run_worker;

fn main() -> ExitCode {
    let mut config = SfuConfig::default();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--udp-bind" => config.udp_bind = args.next().and_then(|v| v.parse().ok()),
            "--max-high-egress-kbps" => {
                config.max_high_egress_kbps = args.next().and_then(|v| v.parse().ok())
            }
            _ => {}
        }
    }
    match run_worker(config, std::io::stdin().lock(), std::io::stdout()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sfu-worker: {e}");
            ExitCode::FAILURE
        }
    }
}
