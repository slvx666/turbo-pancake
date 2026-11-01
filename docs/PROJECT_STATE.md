# Project State
Spec version: 1.0 (`TECHNICAL_SPEC_MESSENGER_RU.md`, owner copy outside the repo)
Current stage: S0 — audit, skeleton, media feasibility
Stage status: IN_PROGRESS (all local criteria PASS; blocked only on CI verification of Linux/macOS, see blockers)
Last verified commit: 4217f64 (branch `main`, local repository, no remote yet)
Platform matrix: Windows = PASS (build/tests/window/benchmarks) / macOS = UNVERIFIED / Linux = UNVERIFIED
Last session: 2026-11-02

## Environment (dev machine, Windows)

* Windows 11 Pro 10.0.28000, AMD Ryzen 5 5500 (12 threads), 32 GB RAM,
  **NVIDIA RTX 2060 SUPER** (driver 617.14) — *not* the spec's RTX 4070 Super test machine.
* Monitors: DISPLAY1 primary 1920×1080 at (0,0); DISPLAY2 1920×1080 at (1920,0).
* Toolchain: rustup 1.x, **rustc/cargo 1.99.1** (pinned in `rust-toolchain.toml`), MSVC 14.44
  (VS 2022 Community), Windows SDK 10.0.28000. No system protoc needed (protox).
* Rust was installed by this session to `%USERPROFILE%\.cargo` (PATH: `$HOME/.cargo/bin`).
* **Drive C: is ~98 % full.** Build directory is `D:/dsmini-target` via the gitignored
  `.cargo/config.toml`; recreate it on a fresh checkout of this machine:
  `[build]\ntarget-dir = "D:/dsmini-target"`.
* Docker Desktop is installed but its engine did not start (likely the full C: drive), so
  no Linux container build was possible.

## Owner instruction for testing (2026-11-02)

The owner keeps working on this machine during tests: **test GUI on the second monitor,
never take mouse or focus.** Use `--window-pos 2020,100 --no-activate` for every client
launch and `benchmarks/scripts/window-check.ps1` for smoke checks. Never move the mouse
or send input; screenshots only of our own window region.

## Implemented and verified

| Component | Path | Verified by |
|---|---|---|
| Cargo workspace, release/dev profiles, lints | `Cargo.toml`, `rust-toolchain.toml` | fmt/clippy/test clean |
| CI workflow (Win/Ubuntu/macOS arm64: fmt, clippy -D warnings, tests, release build, Xvfb desktop smoke, signaling bench, cargo audit) | `.github/workflows/ci.yml` | **not run yet** (no remote) |
| Protocol v1 schema + Rust types + validation + limits | `proto/dsmini/v1/signaling.proto`, `crates/protocol` | 2 unit + 7 contract tests |
| Signaling server: `/healthz`, `/readyz`, `/v1/version`, `/v1/signal` WS (hello, rooms, SDP/ICE relay, heartbeat eviction, bounded queues, connection limit, 64 KiB frames, 8 KiB WS buffers) | `apps/app-server` | 13 integration tests over real TCP/WS |
| Desktop native window (Slint 1.18.1, winit, FemtoVG/OpenGL + software), Discord-like empty shell, dark/light toggle, keyboard-focusable button, smoke mode, `--window-pos`, `--no-activate` | `apps/desktop` | benchmark runs + `window-check.ps1` (position OK, never foreground, clean exit) + screenshot visually checked |
| str0m WebRTC spike: ICE+DTLS+SRTP+data channel over real loopback UDP, SDP via protobuf | `crates/media-transport` | `loopback_spike` test: handshake 17 ms, 100/100 RTP |
| Benchmark harness (desktop startup + memory, signaling latency + idle connections) | `benchmarks/harness` | JSON reports in `benchmarks/results/` |
| Audio device probe (cpal) | `tools/media-probe` | report in `benchmarks/results/` |
| ADRs 0001–0005, API, ARCHITECTURE, TESTING, STAGES | `docs/` | — |

## Acceptance checks — S0

| Criterion | Command / procedure | Result | Evidence |
|---|---|---|---|
| Compiles on supported CI targets (or exact platform blockers recorded) | `cargo build --workspace --all-targets` | Windows **PASS**; Linux **UNVERIFIED**; macOS **UNVERIFIED** | local build; CI defined but never executed (no GitHub remote, Docker engine down, no Mac in session) |
| fmt / clippy clean | `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets --locked -- -D warnings` | **PASS** (Windows) | session log 2026-11-02 |
| Tests | `cargo test --workspace --locked` | **PASS** 23/23 (Windows) | 2 unit, 7 contract, 13 signaling, 1 WebRTC spike |
| App opens an empty but real native window | `dsmini-desktop --window-pos 2020,100 --no-activate`; `window-check.ps1` | **PASS** (Windows, OpenGL and software renderers) | window at (2020,100) 1116×739, foreground never ours (≈110 samples), exit 0; screenshot reviewed (not committed: edges showed other apps) |
| Toolchain/versions recorded | this file + `rust-toolchain.toml` + `Cargo.lock` | **PASS** | |
| Reproducible benchmark harness exists | `dsmini-bench desktop-startup …`, `dsmini-bench signaling …` (see `benchmarks/README.md`) | **PASS** | `benchmarks/results/*.json` |
| Research + ADRs: UI, HW codecs, GPU textures, str0m, capture APIs, audio | `docs/ADR/0002`, `0003`, `0005` | **PASS** as research records; most platform items explicitly **UNVERIFIED** (planned spikes in S1/S3) | |
| `PROJECT_STATE.md` filled | this file | **PASS** | |

### Measured numbers (Windows dev machine, release build)

| Metric | Value | Spec target | Verdict |
|---|---|---|---|
| Idle RAM, OpenGL renderer (WS / private) | 71.5 / 76.7 MiB p50 | ≤ 50 MiB | **FAIL for GPU path** (driver context); recorded, decision in S7 (ADR 0002) |
| Idle RAM, software renderer (WS / private) | 21.8 / 6.2 MiB p50 | ≤ 50 MiB | PASS (S0 shell only) |
| Spawn → first frame (warm) | OpenGL 398 ms p50; software 73 ms p50 | < 2 s | PASS (cold start not measured) |
| Desktop exe size (uncompressed, unsigned) | 9.1 MiB | installer ≤ 30 MB | on track; no installer yet |
| Signaling relay latency (loopback) | p50 72 µs, p99 153 µs | — | baseline |
| Server memory per idle WS | 17.9 KB (was 140 KB before buffer tuning) | 500 sessions | 500 idle → 14.3 MiB total |

## Known issues and blockers

1. **BLOCKER (environment): Linux/macOS compilation unverified.** Needs either a GitHub
   remote (owner creates repo, then `git remote add origin … && git push -u origin main`
   to trigger `.github/workflows/ci.yml`) or access to Linux/macOS machines. Docker
   engine failed to start (C: full) — freeing space on C: would also allow a Linux build
   in Docker (`rust:1.99` image + packages from ci.yml).
2. **Owner decision: Slint license** (GPL-3.0 vs Royalty-free 2.0 with attribution) — ADR 0002.
3. GPU renderer idle RAM exceeds 50 MiB target (NVIDIA OpenGL context ≈ +50 MiB). Not a
   S0 criterion; tracked for S7 (try software renderer for chat UI, Skia/D3D, femtovg-wgpu).
4. Software-renderer CPU cost under animation/HiDPI unmeasured; frame-time percentiles
   not yet collected; GPU memory not sampled by the harness.
5. Default microphone on the dev machine is 192 kHz mono → S1 needs resampling to 48 kHz.
6. CI workflow untested; expect possible fixes for Linux packages / macOS memory probe
   (`sysinfo` path) on first run.
7. S0 signaling is unauthenticated (loopback default, warning on non-loopback bind).
8. Dev test machine GPU (RTX 2060 SUPER) lacks AV1 encode; target-hardware results
   (RTX 4070 Super, M4) must come from the owner's machines.

## Design decisions / ADR links

* [0001 Workspace & toolchain](ADR/0001-workspace-and-toolchain.md) — accepted
* [0002 UI: Slint](ADR/0002-ui-slint.md) — provisional (license, GPU texture, frame times open)
* [0003 WebRTC: str0m](ADR/0003-webrtc-str0m.md) — provisional (crypto backend benchmark in S1)
* [0004 Signaling protocol](ADR/0004-signaling-protocol.md) — accepted
* [0005 Media/OS research](ADR/0005-media-platform-research.md) — research

## Files changed this session

Everything (new repository): `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`,
`rustfmt.toml`, `.gitignore`, `.gitattributes`, `README.md`, `.github/workflows/ci.yml`,
`proto/dsmini/v1/signaling.proto`, `crates/protocol/**`, `crates/media-transport/**`,
`apps/app-server/**`, `apps/desktop/**`, `benchmarks/**`, `tools/media-probe/**`, `docs/**`.
Machine-local, not committed: `.cargo/config.toml` (target dir on D:).

## Commands run this session (all on Windows)

```text
cargo build --workspace --all-targets                       OK
cargo fmt --all -- --check                                  OK
cargo clippy --workspace --all-targets --locked -- -D warnings  OK
cargo test --workspace --locked                             OK (23 passed)
cargo build --release -p dsmini-desktop -p dsmini-app-server -p dsmini-bench -p dsmini-media-probe  OK
dsmini-bench desktop-startup … --label femtovg|software --client-arg=--window-pos --client-arg=2020,100 --client-arg=--no-activate
dsmini-bench signaling --messages 2000 --idle-connections 500
dsmini-media-probe
benchmarks/scripts/window-check.ps1 (OpenGL + software)    OK
```

## Exact next task

1. **Close S0 blocker**: if a GitHub remote is available (ask the owner for the repo URL;
   pushing requires their OK), push `main`, let CI run on Windows/Ubuntu/macOS, fix any
   failures, record run URLs here and set S0 = DONE. Alternatively build in Docker if C:
   has been freed. If neither is possible, S0 stays with the formal environment blocker
   above and work may proceed to S1 (spec §16.3 allows continuing past a stage with an
   explicitly documented blocker).
2. **Start S1** following `docs/STAGES.md` → "S1 plan": begin with `crates/media-audio`
   (cpal capture/playback → resample 192k/44.1k→48k → Opus 20 ms → adaptive jitter buffer
   → playback), unit-tested with synthetic signals, then wire it to `UdpPeer` and the
   signaling client for a two-process call on this machine (both windows on the second
   monitor, `--no-activate`).
