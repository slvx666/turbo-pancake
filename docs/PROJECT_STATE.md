# Project State
Spec version: 1.0 (`TECHNICAL_SPEC_MESSENGER_RU.md`, owner copy outside the repo)
Current stage: S1 — end-to-end 1:1 voice
Stage status: IN_PROGRESS (S0 = DONE on 2026-11-02, evidence below; S1 step 1 — audio pipeline crate — done)
Last verified commit: 910a23d (branch `main`, https://github.com/slvx666/turbo-pancake, CI run 41364362270 green)
Platform matrix (S0 scope: build + tests + window smoke): Windows = PASS / macOS = PASS (CI runner, arm64) / Linux = PASS (CI, X11 via Xvfb; Wayland UNVERIFIED)
Last session: 2026-11-02

## Environment (dev machine, Windows)

* Windows 11 Pro 10.0.28000, AMD Ryzen 5 5500 (12 threads), 32 GB RAM,
  **NVIDIA RTX 2060 SUPER** (driver 617.14) — *not* the spec's RTX 4070 Super test machine.
* Monitors: DISPLAY1 primary 1920×1080 at (0,0); DISPLAY2 1920×1080 at (1920,0).
* Toolchain: **rustc/cargo 1.99.1** (pinned in `rust-toolchain.toml`, installed by rustup into
  `%USERPROFILE%\.cargo`; PATH `$HOME/.cargo/bin`), MSVC 14.44 (VS 2022 Community),
  Windows SDK 10.0.28000. No system protoc (protox), no cmake/NASM needed (aws-lc-rs builds).
* **Drive C: is ~98 % full.** Build directory is `D:/turbo-pancake-target` via the gitignored
  `.cargo/config.toml`; on a fresh checkout of this machine recreate it:
  `[build]` / `target-dir = "D:/turbo-pancake-target"`.
* Docker Desktop engine does not start here (likely the full C: drive); Linux/macOS are
  verified through GitHub Actions instead.
* GitHub: remote `origin` = `https://github.com/slvx666/turbo-pancake` (public). `gh` is
  logged in as `slvx666` (scopes repo, workflow). Owner authorised pushing to this repo
  (2026-11-02). Check CI with `gh run list -R slvx666/turbo-pancake`.

## Owner instruction for testing (2026-11-02)

The owner keeps working on this machine during tests: **test GUI on the second monitor,
never take mouse or focus.** Launch every client with `--window-pos 2020,100 --no-activate`
and use `benchmarks/scripts/window-check.ps1` for smoke checks. Never move the mouse or
send input; screenshots only of our own window region (and do not commit them if other
apps are visible at the edges).

## Implemented and verified

| Component | Path | Verified by |
|---|---|---|
| Cargo workspace, profiles, lints | `Cargo.toml`, `rust-toolchain.toml` | fmt/clippy/test clean on 3 OS |
| CI: Windows / Ubuntu / macOS arm64 — fmt, clippy -D warnings, tests, release build, desktop window smoke (macOS GUI session, Linux Xvfb), signaling bench smoke, `cargo audit` | `.github/workflows/ci.yml` | run 41364362270: all jobs success |
| Protocol v1 schema + Rust types + validation + limits | `proto/turbo_pancake/v1/signaling.proto`, `crates/protocol` | 2 unit + 7 contract tests |
| Signaling server: `/healthz`, `/readyz`, `/v1/version`, `/v1/signal` (hello, rooms, SDP/ICE relay, heartbeat eviction, bounded queues, connection limit, 64 KiB frames, 8 KiB WS buffers) | `apps/app-server` | 13 integration tests over real TCP/WS |
| Desktop native window (Slint 1.18.1, winit, FemtoVG/OpenGL + software), Discord-like empty shell, dark/light toggle, smoke mode, `--window-pos`, `--no-activate` | `apps/desktop` | Windows: bench + `window-check.ps1` + visual screenshot; Linux/macOS: CI smoke |
| str0m WebRTC spike, **aws-lc-rs** crypto: ICE+DTLS+SRTP+data channel over real loopback UDP, SDP via protobuf | `crates/media-transport` | `loopback_spike` on 3 OS (Windows: handshake 27 ms, 100/100 RTP) |
| Benchmark harness (startup + memory, signaling latency + idle connections), child stderr/exit diagnostics | `benchmarks/harness` | JSON reports in `benchmarks/results/`, CI artifacts `bench-<os>` |
| Audio device probe (cpal) | `tools/media-probe` | Windows report in `benchmarks/results/` |
| **Voice pipeline** (S1): cpal capture/playback on lock-free `rtrb` rings with xrun/failure counters, device listing/selection by id, rubato FFT resampler (any rate ↔ 48 kHz), Opus 48 kHz mono 20 ms (libopus 1.6.1 static, FEC/DTX/complexity/bitrate at runtime), adaptive jitter buffer with FEC→PLC, `VoiceSender` (mute = encoded silence, peak level) / `VoiceReceiver` (per-peer volume, stats) | `crates/media-audio` | 11 unit + 2 impairment tests (3 OS in CI); `examples/device_check` on real Windows devices; `examples/codec_cost` |
| ADRs 0001–0005, API, ARCHITECTURE, TESTING, STAGES | `docs/` | — |

## Acceptance checks — S0 (DONE)

| Criterion | Command / procedure | Result | Evidence |
|---|---|---|---|
| Compiles on supported CI targets | CI matrix `cargo clippy/test/build --release --locked` | **PASS** Windows, Ubuntu, macOS arm64 | https://github.com/slvx666/turbo-pancake/actions/runs/41364362270 |
| fmt / clippy clean | `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets --locked -- -D warnings` | **PASS** (3 OS) | same run |
| Tests | `cargo test --workspace --locked` | **PASS** 23/23 on each OS | same run + local Windows |
| No known vulnerable dependencies | `rustsec/audit-check` | **PASS** (5 *unmaintained* warnings: bincode, ttf-parser, zbus_names, zvariant, zvariant_derive — transitive via Slint/accesskit, informational) | same run, job `audit` |
| App opens an empty but real native window | Windows: `window-check.ps1`; CI: `turbo-pancake-bench desktop-startup` | **PASS** Windows (OpenGL + software), Linux X11/Xvfb (software), macOS runner (OpenGL) | local log; CI logs `desktop smoke` steps |
| Toolchain/versions recorded | this file, `rust-toolchain.toml`, `Cargo.lock` | **PASS** | |
| Reproducible benchmark harness | `benchmarks/README.md` | **PASS** | `benchmarks/results/*.json`, CI artifacts |
| Research + ADRs (UI, HW codecs, GPU textures, str0m, capture, audio) | `docs/ADR/0002`, `0003`, `0005` | **PASS** as research records; platform-specific items stay **UNVERIFIED** and are scheduled in S1/S3 | |
| `PROJECT_STATE.md` filled | this file | **PASS** | |

### Measured numbers

| Metric | Windows dev machine | Linux CI (Xvfb, software) | macOS CI runner (VM, OpenGL) | Spec target |
|---|---|---|---|---|
| Idle RAM WS / private | OpenGL 71.5 / 76.7 MiB; software 21.8 / 6.2 MiB | 19.8 / 13.5 MiB | 105.5 MiB RSS (run 1) | ≤ 50 MiB |
| Spawn → first frame (warm) | OpenGL 398 ms; software 73 ms | 44 ms | 1005 ms (run 1), 3518 ms (run 0) | < 2 s |
| Desktop exe (unsigned) | 9.1 MiB | — | — | installer ≤ 30 MB |
| Server bytes per idle WS | 17.9 KB | 15.4 KB | 17.2 KB | 500 sessions |
| Signaling relay latency (loopback) | p50 72 µs, p99 153 µs | in CI artifact | in CI artifact | — |

CI runners are shared VMs without real GPUs; their numbers are smoke evidence, not
performance results for the target hardware (RTX 4070 Super / M4).

## Acceptance checks — S1 (IN_PROGRESS)

| Criterion | Command / procedure | Result | Evidence |
|---|---|---|---|
| Two real desktop clients talk through the server (signaling + WebRTC) | two `turbo-pancake-desktop` instances + `app-server` | **UNVERIFIED** — audio ↔ transport ↔ desktop wiring not implemented yet | — |
| Speech continuity under moderate packet loss | `cargo test -p turbo-pancake-media-audio --test impairment` (in-process network model) | **PASS (pipeline level)**: loss ≤ 10 % × jitter ≤ 50 ms → 0 decode errors, 0 late, PLC-only ≤ 1.4 % of frames, FEC repairs ≈ 90 % of losses, level ratio ≥ 0.91; end-to-end over real UDP between clients still **UNVERIFIED** | `benchmarks/results/audio-impairment-windows-20261102.json` |
| No audio loop | AEC + mute; two clients on one machine | **UNVERIFIED** — no AEC yet (APM pending) | — |
| Resources released after leave | drop of `Capture`/`Playback` stops streams; call-level check pending | **UNVERIFIED** | — |
| Opus in/out, mute, device selection | unit tests + `examples/device_check` | **PASS (Windows, library level)**: 192 kHz mic → Opus → 192 kHz stereo out, 0 overflows, ≈ 50 frames/s, 0 concealed; mute path unit-covered; UI device pickers not implemented | `benchmarks/results/audio-device-check-windows-20261102.json` |
| Basic DSP (NS/AEC) | — | **UNVERIFIED** (not started) | — |
| Trials recorded | this file + `benchmarks/results/audio-*.json` | **PASS** for the pipeline part | |
| Planned platforms | CI on Windows/Ubuntu/macOS (unit + impairment tests) | build/tests in CI; device I/O on macOS/Linux **UNVERIFIED** | CI run (see session log) |

## Known issues and blockers

1. **Owner decision: Slint license** (GPL-3.0 vs Royalty-free 2.0 with attribution) — ADR 0002.
   Not blocking S1, blocks any public distribution.
2. GPU-renderer idle RAM exceeds 50 MiB (Windows OpenGL +~50 MiB driver context; macOS VM
   105 MiB). Tracked for S7 (ADR 0002 options). Software renderer fits (≈ 20–22 MiB).
3. macOS memory sampling: run 0 reported RSS 0.0 MiB (sampling glitch, `ps` while the first
   run was still starting on a slow VM); private bytes not sampled on macOS; GPU memory
   not sampled on any OS.
4. Software-renderer CPU cost under animation/HiDPI and frame-time percentiles unmeasured.
5. Default microphone on the dev machine runs at 192 kHz mono → S1 must resample to 48 kHz.
6. Linux runtime dependency found: winit dlopens `libxkbcommon-x11.so` (package
   `libxkbcommon-x11-0`); must be declared in the Linux package (S7). Wayland not tested.
7. S0 signaling is unauthenticated (loopback default, warning on non-loopback bind).
8. Dev GPU (RTX 2060 SUPER) lacks AV1 encode; target-hardware results must come from the
   owner's RTX 4070 Super and M4 machines.
9. CI warns that macOS arm64 runners may queue longer; a full CI run takes ~15 min.
10. **Windows libopus build pitfall:** the `cmake` crate with the autodetected Visual Studio
    generator overrides `CMAKE_C_FLAGS_RELEASE` without `/O2` → unoptimised libopus
    (845 vs 243 µs/frame). Fixed by `CMAKE_GENERATOR` in the local `.cargo/config.toml`
    `[env]` (also `CMAKE` = VS-bundled cmake.exe, not on PATH here) and a CI step using
    `vswhere`. A fresh checkout on this machine must recreate both entries.
11. Dev microphone delivered peak level 0.000 during `device_check` (muted in Windows or a
    silent room); audible voice quality on real hardware is UNVERIFIED.
12. Output startup underrun ≈ 22 ms before the first decoded frame (expected; prefill
    silence when wiring the engine). Device hot-unplug rebuild and sleep/wake not
    implemented (errors only flag `StreamHealth::failed`).

## Design decisions / ADR links

* [0001 Workspace & toolchain](ADR/0001-workspace-and-toolchain.md) — accepted
* [0002 UI: Slint](ADR/0002-ui-slint.md) — provisional (license, GPU texture, frame times open)
* [0003 WebRTC: str0m](ADR/0003-webrtc-str0m.md) — provisional; crypto backend switched to
  aws-lc-rs after `cargo audit` flagged RUSTSEC-2026-0387 in the rust-crypto path
* [0004 Signaling protocol](ADR/0004-signaling-protocol.md) — accepted
* [0005 Media/OS research](ADR/0005-media-platform-research.md) — research
* [0006 Voice pipeline](ADR/0006-voice-pipeline.md) — accepted for S1 (cpal, rtrb, rubato,
  libopus 1.6.1 via `opus` 0.4, own jitter buffer; APM open)

## Session log 2026-11-02

**Rename (owner request):** project, crates, binaries and protocol were renamed from the
working title "DsMini" to **turbo-pancake**: crates/binaries `turbo-pancake-*`, Rust crates
`turbo_pancake_*`, protobuf package `turbo_pancake.v1` (`proto/turbo_pancake/v1/`; wire
encoding unchanged, contract tests pass), stdout markers/env vars `TURBO_PANCAKE_*`
(e.g. `TURBO_PANCAKE_BIND`), build dir `D:/turbo-pancake-target`, project folder
`C:\Users\burger\Desktop\clavdiy\turbo-pancake`. JSON reports in `benchmarks/results/`
created before the rename still mention `dsmini-*` executables (historical evidence, left
as is).

Commits: `4217f64` (S0 skeleton), `5cbd68b` (state), `9dd3118` (aws-lc-rs, audit
permissions, harness diagnostics), `12bbbef` (libxkbcommon-x11 in CI), `d45e850` (macOS
window smoke), `910a23d` (macOS RSS via `ps`). CI runs: 41360886590 (audit + Linux smoke
failed), 41362565545 (Linux smoke failed: missing libxkbcommon-x11), 41363307819 (green),
41363967899 (macOS memory sampling failed), **41364362270 (all green)**.

Local commands (Windows): `cargo fmt --all -- --check`, `cargo clippy --workspace
--all-targets --locked -- -D warnings`, `cargo test --workspace --locked` (23 passed),
release build, `turbo-pancake-bench desktop-startup` (femtovg/software, second monitor,
`--no-activate`), `turbo-pancake-bench signaling --messages 2000 --idle-connections 500`,
`turbo-pancake-media-probe`, `benchmarks/scripts/window-check.ps1`.

## Session log 2026-11-02 (S1, step 1)

Files: `crates/media-audio/**` (new: `lib.rs`, `device.rs`, `resample.rs`, `codec.rs`,
`jitter.rs`, `pipeline.rs`, `testsignal.rs`, `tests/impairment.rs`,
`examples/{codec_cost,device_check}.rs`), `Cargo.toml` (member + dev opt-level for DSP
crates), `Cargo.lock`, `.github/workflows/ci.yml` (Windows `CMAKE_GENERATOR`, codec cost
step), `docs/ADR/0006-voice-pipeline.md`, `docs/{ARCHITECTURE,TESTING,STAGES}.md`,
`benchmarks/results/audio-*-windows-20261102.json`.

Local (Windows): `cargo fmt --all -- --check` OK; `cargo clippy --workspace --all-targets
--locked -- -D warnings` OK; `cargo test --workspace --locked` 36 passed; codec cost:
encode 243 µs/frame (complexity 9), decode 29 µs, resample 192→48 kHz 22 µs per 20 ms frame.

## Exact next task — S1 step 2/3 (plan in `docs/STAGES.md` → "S1 plan")

1. `crates/media-audio`: an `AudioEngine` worker thread that owns `Capture`/`Playback`,
   moves samples ring → `VoiceSender` and `VoiceReceiver` → ring (output-paced, prefill
   ~40 ms), exposes mute/volume/device switch through a bounded command channel, rebuilds
   a stream when `StreamHealth::failed` (fallback to default device), and reports levels/
   stats. Test: engine start/stop releases devices (no thread left), device switch.
2. Evaluate WebRTC APM (`webrtc-audio-processing` 2.x): build on 3 OS in CI, NS default on
   + toggle, AEC with playback reference, CPU per 10 ms frame; record in ADR 0006.
3. `crates/media-transport`: async call driver — `UdpPeer` on its own thread, Opus audio
   track (str0m `Media` API, PT 111, 48 kHz), RTP seq/timestamp from `EncodedFrame`,
   bounded channels to/from the audio engine; loopback test: two drivers exchange encoded
   speech over real UDP with in-process loss, continuity checked as in `impairment.rs`.
4. Desktop: signaling client (Tokio thread, `/v1/signal`), "join room by id" call UI,
   mute, device pickers, connection state; then two clients on this machine (second
   monitor, `--no-activate`) through `app-server`, results recorded here.
5. DTLS backend benchmark (aws-lc-rs vs wincrypto) in the spike → ADR 0003.
