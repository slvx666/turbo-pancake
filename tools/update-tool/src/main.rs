//! `turbo-pancake-update` — offline release signing (ADR 0016).
//!
//! * `keygen --id <id> --out <key.pk8>` — a new Ed25519 key; prints the public key line
//!   for `TURBO_PANCAKE_UPDATE_PUBKEYS`. Keep the private key off the repo and server.
//! * `sign --key <key.pk8> --id <id> --version <x.y.z> --input <dir> --out <update-dir>`
//!   — hashes every file of the build directory, copies them to
//!   `<update-dir>/files/<version>/` and writes the signed `<update-dir>/manifest.json`
//!   (the directory the server's `--update-dir` serves).
//! * `verify --manifest <file> --pubkeys <list> [--current <x.y.z>]`.
//! * `pack --version <x.y.z> --input <dir> --out <package>` — the installer package
//!   (ADR 0017) of a directory holding `turbo-pancake.exe` and
//!   `turbo-pancake-desktop.exe`; build the installer with
//!   `TURBO_PANCAKE_SETUP_PAYLOAD=<package>`.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use clap::{Parser, Subcommand};
use turbo_pancake_update::manifest::{Expect, FORMAT, valid_path};
use turbo_pancake_update::{FileEntry, Manifest, TrustedKey, Version, sha256_file};

#[derive(Parser)]
#[command(
    name = "turbo-pancake-update",
    about = "Sign and verify update manifests"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    Keygen {
        #[arg(long)]
        id: String,
        #[arg(long)]
        out: PathBuf,
    },
    Sign {
        #[arg(long)]
        key: PathBuf,
        #[arg(long)]
        id: String,
        #[arg(long)]
        version: String,
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value_t = 90)]
        expires_days: i64,
        #[arg(long, default_value = "")]
        notes: String,
    },
    Pack {
        #[arg(long)]
        version: String,
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    Verify {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        pubkeys: String,
        #[arg(long, default_value = "0.0.0")]
        current: String,
    },
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn files_of(dir: &Path, prefix: &str, out: &mut Vec<String>) -> Result<(), String> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .flatten()
        .collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e
            .file_name()
            .into_string()
            .map_err(|_| "non-UTF-8 file name".to_string())?;
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        if e.path().is_dir() {
            files_of(&e.path(), &rel, out)?;
        } else {
            out.push(rel);
        }
    }
    Ok(())
}

fn run(cli: Cli) -> Result<(), String> {
    match cli.cmd {
        Cmd::Keygen { id, out } => {
            if out.exists() {
                return Err(format!("{} exists; not overwritten", out.display()));
            }
            let (pk8, public) = turbo_pancake_update::generate_key().map_err(|e| e.to_string())?;
            std::fs::write(&out, pk8).map_err(|e| e.to_string())?;
            let line = format!("{id}:{}", B64.encode(public));
            TrustedKey::parse_list(&line)?;
            println!(
                "private key: {} (keep it off the repository and the server)",
                out.display()
            );
            println!("TURBO_PANCAKE_UPDATE_PUBKEYS={line}");
        }
        Cmd::Sign {
            key,
            id,
            version,
            input,
            out,
            expires_days,
            notes,
        } => {
            if Version::parse(&version).is_none() {
                return Err(format!("version {version}: expected major.minor.patch"));
            }
            let pk8 = std::fs::read(&key).map_err(|e| format!("{}: {e}", key.display()))?;
            let mut paths = Vec::new();
            files_of(&input, "", &mut paths)?;
            let dest = out.join("files").join(&version);
            std::fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
            let mut files = Vec::new();
            for p in paths {
                if !valid_path(&p) {
                    return Err(format!("file name not allowed in updates: {p}"));
                }
                let src = input.join(&p);
                let target = dest.join(&p);
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                std::fs::copy(&src, &target).map_err(|e| format!("{p}: {e}"))?;
                files.push(FileEntry {
                    size: std::fs::metadata(&target).map_err(|e| e.to_string())?.len(),
                    sha256: sha256_file(&target).map_err(|e| e.to_string())?,
                    path: p,
                });
            }
            let now = now_ms();
            let manifest = Manifest {
                format: FORMAT,
                product: turbo_pancake_update::PRODUCT.into(),
                platform: turbo_pancake_update::PLATFORM.into(),
                version: version.clone(),
                published_at_ms: now,
                expires_at_ms: now + expires_days * 86_400_000,
                notes,
                files,
            };
            let env =
                turbo_pancake_update::sign(&manifest, &pk8, &id).map_err(|e| e.to_string())?;
            // Self-check with the key's own public half.
            let public = aws_lc_rs::signature::Ed25519KeyPair::from_pkcs8(&pk8)
                .map(|k| {
                    use aws_lc_rs::signature::KeyPair;
                    k.public_key().as_ref().to_vec()
                })
                .map_err(|_| "not an Ed25519 key".to_string())?;
            turbo_pancake_update::verify(
                &env,
                &[TrustedKey { id, public }],
                &Expect {
                    product: turbo_pancake_update::PRODUCT,
                    platform: turbo_pancake_update::PLATFORM,
                    current: Version(0, 0, 0),
                    bad: &[],
                    now_ms: now,
                },
            )
            .map_err(|e| format!("self-check: {e}"))?;
            std::fs::write(out.join("manifest.json"), env).map_err(|e| e.to_string())?;
            println!(
                "signed {version}: {} files, manifest {}",
                manifest.files.len(),
                out.join("manifest.json").display()
            );
        }
        Cmd::Pack {
            version,
            input,
            out,
        } => {
            let mut paths = Vec::new();
            files_of(&input, "", &mut paths)?;
            let mut files = Vec::new();
            for p in paths {
                let data = std::fs::read(input.join(&p)).map_err(|e| format!("{p}: {e}"))?;
                files.push((p, data));
            }
            let raw: usize = files.iter().map(|f| f.1.len()).sum();
            let pkg =
                turbo_pancake_update::package::pack(&version, &files).map_err(|e| e.to_string())?;
            // The installer's own checks, before anything is shipped.
            let (h, _) =
                turbo_pancake_update::package::read_header(&pkg).map_err(|e| e.to_string())?;
            for need in [
                turbo_pancake_update::LAUNCHER_EXE,
                turbo_pancake_update::CLIENT_EXE,
            ] {
                if !h.files.iter().any(|f| f.path == need) {
                    return Err(format!("{need} missing in {}", input.display()));
                }
            }
            std::fs::write(&out, &pkg).map_err(|e| e.to_string())?;
            println!(
                "packed {version}: {} files, {raw} bytes -> {} bytes ({:.1} %)",
                h.files.len(),
                pkg.len(),
                pkg.len() as f64 * 100.0 / raw.max(1) as f64
            );
        }
        Cmd::Verify {
            manifest,
            pubkeys,
            current,
        } => {
            let keys = TrustedKey::parse_list(&pubkeys)?;
            let current = Version::parse(&current).ok_or("bad --current")?;
            let env = std::fs::read(&manifest).map_err(|e| e.to_string())?;
            let m = turbo_pancake_update::verify(
                &env,
                &keys,
                &Expect {
                    product: turbo_pancake_update::PRODUCT,
                    platform: turbo_pancake_update::PLATFORM,
                    current,
                    bad: &[],
                    now_ms: now_ms(),
                },
            )
            .map_err(|e| e.to_string())?;
            println!("valid: version {} with {} files", m.version, m.files.len());
        }
    }
    Ok(())
}

fn main() {
    if let Err(e) = run(Cli::parse()) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
