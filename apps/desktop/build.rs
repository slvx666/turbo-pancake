fn main() {
    // Pinned update keys and the release version are baked in (ADR 0016).
    println!("cargo:rerun-if-env-changed=TURBO_PANCAKE_UPDATE_PUBKEYS");
    println!("cargo:rerun-if-env-changed=TURBO_PANCAKE_BUILD_VERSION");
    slint_build::compile("ui/main.slint").expect("failed to compile Slint UI");
}
