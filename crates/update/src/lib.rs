//! Signed updates (S7 step 5, ADR 0016): manifest signing and verification
//! ([`manifest`]), the per-user install layout with atomic state, staged downloads,
//! switch and rollback ([`install`]). Platform-neutral; the launcher and the desktop
//! client use it on Windows.

pub mod install;
pub mod manifest;
pub mod package;

pub use install::{Install, LaunchPlan, State};
pub use manifest::{
    Envelope, FileEntry, Manifest, TrustedKey, UpdateError, Version, generate_key, sign, verify,
};

/// The client executable every manifest must contain.
pub const CLIENT_EXE: &str = "turbo-pancake-desktop.exe";
/// The launcher in the install root.
pub const LAUNCHER_EXE: &str = "turbo-pancake.exe";
/// Platform id of this build's packages.
pub const PLATFORM: &str = "windows-x86_64";
pub const PRODUCT: &str = "turbo-pancake-desktop";

/// Lower-case hex of bytes (SHA-256 digests in manifests).
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// SHA-256 of a whole file (signing tool, tests).
pub fn sha256_file(path: &std::path::Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut ctx = aws_lc_rs::digest::Context::new(&aws_lc_rs::digest::SHA256);
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        ctx.update(&buf[..n]);
    }
    Ok(hex(ctx.finish().as_ref()))
}
