fn main() {
    // Pinned update keys and the release version are baked in (ADR 0016).
    println!("cargo:rerun-if-env-changed=TURBO_PANCAKE_UPDATE_PUBKEYS");
    println!("cargo:rerun-if-env-changed=TURBO_PANCAKE_BUILD_VERSION");
    // A release for one server: its default address and private CA (docs/DEPLOY-TLS.md).
    // Set only at build time, never committed.
    println!("cargo:rerun-if-env-changed=TURBO_PANCAKE_DEFAULT_SERVER");
    println!("cargo:rerun-if-env-changed=TURBO_PANCAKE_DEFAULT_SERVER_CA");
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let dest = out.join("default-server-ca.pem");
    match std::env::var_os("TURBO_PANCAKE_DEFAULT_SERVER_CA") {
        Some(p) => {
            println!(
                "cargo:rerun-if-changed={}",
                std::path::Path::new(&p).display()
            );
            std::fs::copy(&p, &dest).expect("TURBO_PANCAKE_DEFAULT_SERVER_CA: cannot read the CA");
        }
        None => std::fs::write(&dest, b"").expect("CA placeholder"),
    }
    slint_build::compile("ui/main.slint").expect("failed to compile Slint UI");
}
