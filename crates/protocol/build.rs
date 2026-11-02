//! Compiles `proto/` with the pure-Rust `protox` compiler so that building the
//! workspace never requires a system `protoc` installation.

use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../proto");
    let files = ["turbo_pancake/v1/signaling.proto"];
    for f in &files {
        println!("cargo:rerun-if-changed={}", proto_root.join(f).display());
    }
    let fds = protox::compile(files, [&proto_root])?;
    prost_build::Config::new().compile_fds(fds)?;
    Ok(())
}
