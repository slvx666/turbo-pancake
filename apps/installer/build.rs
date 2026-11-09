//! Embeds the release package named by `TURBO_PANCAKE_SETUP_PAYLOAD` (from
//! `turbo-pancake-update pack`); without it the installer carries an empty package and
//! refuses to install (ADR 0017).

fn main() {
    turbo_pancake_win_icon::embed(std::path::Path::new("../../assets/icon/turbo-pancake.ico"));
    println!("cargo:rerun-if-env-changed=TURBO_PANCAKE_SETUP_PAYLOAD");
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let dest = out.join("payload.bin");
    match std::env::var_os("TURBO_PANCAKE_SETUP_PAYLOAD") {
        Some(p) => {
            println!(
                "cargo:rerun-if-changed={}",
                std::path::Path::new(&p).display()
            );
            std::fs::copy(&p, &dest).expect("TURBO_PANCAKE_SETUP_PAYLOAD: cannot read the package");
        }
        None => std::fs::write(&dest, b"").expect("payload placeholder"),
    }
}
