# Project State
Spec version: 1.0 (`TECHNICAL_SPEC_MESSENGER_RU.md`, owner copy outside the repo)
Current stage: S1 — end-to-end 1:1 voice
Stage status: NOT_STARTED (S0 = DONE on 2026-11-02, evidence below)
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
* **Drive C: is ~98 % full.** Build directory is `D:/dsmini-target` via the gitignored
  `.cargo/config.toml`; on a fresh checkout of this machine recreate it:
  `[build]` / `target-dir = "D:/dsmini-target"`.
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
| Protocol v1 schema + Rust types + validation + limits | `proto/dsmini/v1/signaling.proto`, `crates/protocol` | 2 unit + 7 contract tests |
| Signaling server: `/healthz`, `/readyz`, `/v1/version`, `/v1/signal` (hello, rooms, SDP/ICE relay, heartbeat eviction, bounded queues, connection limit, 64 KiB frames, 8 KiB WS buffers) | `apps/app-server` | 13 integration tests over real TCP/WS |
| Desktop native window (Slint 1.18.1, winit, FemtoVG/OpenGL + software), Discord-like empty shell, dark/light toggle, smoke mode, `--window-pos`, `--no-activate` | `apps/desktop` | Windows: bench + `window-check.ps1` + visual screenshot; Linux/macOS: CI smoke |
| str0m WebRTC spike, **aws-lc-rs** crypto: ICE+DTLS+SRTP+data channel over real loopback UDP, SDP via protobuf | `crates/media-transport` | `loopback_spike` on 3 OS (Windows: handshake 27 ms, 100/100 RTP) |
| Benchmark harness (startup + memory, signaling latency + idle connections), child stderr/exit diagnostics | `benchmarks/harness` | JSON reports in `benchmarks/results/`, CI artifacts `bench-<os>` |
| Audio device probe (cpal) | `tools/media-probe` | Windows report in `benchmarks/results/` |
| ADRs 0001–0005, API, ARCHITECTURE, TESTING, STAGES | `docs/` | — |

## Acceptance checks — S0 (DONE)

| Criterion | Command / procedure | Result | Evidence |
|---|---|---|---|
| Compiles on supported CI targets | CI matrix `cargo clippy/test/build --release --locked` | **PASS** Windows, Ubuntu, macOS arm64 | https://github.com/slvx666/turbo-pancake/actions/runs/41364362270 |
| fmt / clippy clean | `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets --locked -- -D warnings` | **PASS** (3 OS) | same run |
| Tests | `cargo test --workspace --locked` | **PASS** 23/23 on each OS | same run + local Windows |
| No known vulnerable dependencies | `rustsec/audit-check` | **PASS** (5 *unmaintained* warnings: bincode, ttf-parser, zbus_names, zvariant, zvariant_derive — transitive via Slint/accesskit, informational) | same run, job `audit` |
| App opens an empty but real native window | Windows: `window-check.ps1`; CI: `dsmini-bench desktop-startup` | **PASS** Windows (OpenGL + software), Linux X11/Xvfb (software), macOS runner (OpenGL) | local log; CI logs `desktop smoke` steps |
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

## Design decisions / ADR links

* [0001 Workspace & toolchain](ADR/0001-workspace-and-toolchain.md) — accepted
* [0002 UI: Slint](ADR/0002-ui-slint.md) — provisional (license, GPU texture, frame times open)
* [0003 WebRTC: str0m](ADR/0003-webrtc-str0m.md) — provisional; crypto backend switched to
  aws-lc-rs after `cargo audit` flagged RUSTSEC-2026-0387 in the rust-crypto path
* [0004 Signaling protocol](ADR/0004-signaling-protocol.md) — accepted
* [0005 Media/OS research](ADR/0005-media-platform-research.md) — research

## Session log 2026-11-02

Commits: `4217f64` (S0 skeleton), `5cbd68b` (state), `9dd3118` (aws-lc-rs, audit
permissions, harness diagnostics), `12bbbef` (libxkbcommon-x11 in CI), `d45e850` (macOS
window smoke), `910a23d` (macOS RSS via `ps`). CI runs: 41360886590 (audit + Linux smoke
failed), 41362565545 (Linux smoke failed: missing libxkbcommon-x11), 41363307819 (green),
41363967899 (macOS memory sampling failed), **41364362270 (all green)**.

Local commands (Windows): `cargo fmt --all -- --check`, `cargo clippy --workspace
--all-targets --locked -- -D warnings`, `cargo test --workspace --locked` (23 passed),
release build, `dsmini-bench desktop-startup` (femtovg/software, second monitor,
`--no-activate`), `dsmini-bench signaling --messages 2000 --idle-connections 500`,
`dsmini-media-probe`, `benchmarks/scripts/window-check.ps1`.

## Exact next task — start S1 (plan in `docs/STAGES.md` → "S1 plan")

1. Create `crates/media-audio`:
   * capture/playback via cpal on the RT callbacks, lock-free SPSC rings
     (e.g. `rtrb`), no allocation/locks in callbacks;
   * resampler device rate → 48 kHz (dev mic is 192 kHz mono; handle 44.1/48/96/192 kHz,
     mono/stereo);
   * Opus 48 kHz, 20 ms frames, FEC + DTX configurable (choose binding per ADR 0005 —
     write ADR 0006 with the choice and license check);
   * adaptive jitter buffer with PLC; unit tests with synthetic sine/speech-like signals,
     loss/jitter injection and continuity metrics (concealed %, underruns).
2. Benchmark the DTLS backends (aws-lc-rs vs wincrypto) in the spike and record in ADR 0003.
3. Then wire audio ↔ `UdpPeer` ↔ signaling client in the desktop app (call by room id until
   accounts exist in S5) and run two clients on this machine (second monitor, inactive).
