# DsMini

Native desktop messenger with high-quality voice and screen sharing (Rust, Slint, str0m).
Requirements: `TECHNICAL_SPEC_MESSENGER_RU.md` v1.0 (owner's spec). Current status:
[docs/PROJECT_STATE.md](docs/PROJECT_STATE.md).

## Quick start

Prerequisites: `rustup` (toolchain pinned by `rust-toolchain.toml`), platform C toolchain
(MSVC Build Tools on Windows). Linux packages: see `.github/workflows/ci.yml`.

```bash
cargo test --workspace
cargo run --release -p dsmini-app-server            # dev signaling server on 127.0.0.1:7878
cargo run --release -p dsmini-desktop               # native window (S0 shell)
```

Docs: [Architecture](docs/ARCHITECTURE.md) · [API](docs/API.md) · [Testing](docs/TESTING.md) ·
[Stages](docs/STAGES.md) · [ADR](docs/ADR) · [Benchmarks](benchmarks/README.md)

Third-party: UI built with [Slint](https://slint.dev) (license choice pending, see ADR 0002).
