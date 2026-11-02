# Testing

All commands run from the repository root. On the primary dev machine the build directory
is `D:/turbo-pancake-target` (set in the gitignored `.cargo/config.toml`, because drive C: is
nearly full); elsewhere it is `target/`. Below `$T` means that directory.

## Automated (CI: `.github/workflows/ci.yml`, Windows / Ubuntu / macOS arm64)

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

| Suite | Location | What it proves |
|---|---|---|
| protocol unit | `crates/protocol/src/lib.rs` | room id / display name validation |
| protocol contract | `crates/protocol/tests/contract.rs` | pinned binary encoding, version check, oversize/garbage rejection, forward-compat unknown fields, signal validation |
| signaling integration | `apps/app-server/tests/signaling.rs` | real TCP+WS: hello/welcome, join/leave, offer/answer/ICE relay, error codes, heartbeat eviction, room capacity, connection limit, oversize close, 20-peer join/relay/leave, gap-free `event_id`, health/ready/version |
| WebRTC spike | `crates/media-transport/tests/loopback_spike.rs` | str0m ICE+DTLS+SRTP over real loopback UDP, SDP via protobuf, 100/100 RTP packets, data channel |
| audio unit | `crates/media-audio/src/{resample,codec,jitter}.rs` | 16/44.1/48/96/192 kHz ↔ 48 kHz (tone + level preserved), Opus round trip level/bitrate, FEC beats PLC on lost frames, DTX, jitter buffer order/reorder/dup/late/FEC/PLC/wrap-around/adaptation/stall+burst |
| audio engine | `crates/media-audio/tests/engine.rs` (simulated devices, `sim.rs`) | start/stop and drop release every device stream and device thread; mic → Opus → loop back as a peer → jitter buffer → mix → speaker (peak kept), peer volume 0 and mute silence the output, muted frames still sent; input + output switch with different rates keeps seq/timestamp continuous; unplugged device → fallback to default; no speaker → wall-clock ticks and retry, recovery when it returns; unknown ids at start fall back |
| audio impairment | `crates/media-audio/tests/impairment.rs` | 192 kHz mic → Opus → simulated network (loss 0/1/3/5/10 % × jitter 0/20/50 ms, 2 s outage) → 44.1 kHz output: no decode errors, bounded late/underruns, concealment ≤ loss, loudness kept, bounded delay. `TURBO_PANCAKE_REPORT_DIR=<dir>` writes the table as JSON |

## Benchmarks (`benchmarks/README.md`)

```bash
cargo build --release -p turbo-pancake-desktop -p turbo-pancake-app-server -p turbo-pancake-bench -p turbo-pancake-media-probe
$T/release/turbo-pancake-bench desktop-startup --exe $T/release/turbo-pancake-desktop --runs 7 --settle-ms 3000 --label femtovg \
  --client-arg=--window-pos --client-arg=2020,100 --client-arg=--no-activate
$T/release/turbo-pancake-bench signaling --server-exe $T/release/turbo-pancake-app-server --messages 2000 --idle-connections 500
$T/release/turbo-pancake-media-probe > benchmarks/results/media-probe-<os>-<ts>.json
# voice pipeline CPU cost and real-device run (receiver volume 0: the speaker plays silence)
cargo run --release -p turbo-pancake-media-audio --example codec_cost
cargo run --release -p turbo-pancake-media-audio --example device_check -- 5
cargo run --release -p turbo-pancake-media-audio --example engine_check -- 20
# WebRTC APM evaluation (stand-alone crate; Windows needs the steps in tools/apm-eval/README.md)
(cd tools/apm-eval && cargo run --release)
```

Windows note: libopus is built by CMake. Without an explicit `CMAKE_GENERATOR` the
`cmake` crate drops `/O2` (≈ 3.5× slower encoder). Set
`CMAKE_GENERATOR = "Visual Studio 17 2022"` under `[env]` in `.cargo/config.toml`
(CI derives it via `vswhere`) and `cargo clean -p opusic-sys` once.

## Desktop window smoke without disturbing the user (Windows)

The owner works on the same machine during tests. Rules: windows under test open on the
**secondary monitor** and **inactive** (`--window-pos 2020,100 --no-activate`); no test
moves the mouse or sends keyboard input.

```powershell
powershell -File benchmarks/scripts/window-check.ps1 -Exe D:/turbo-pancake-target/release/turbo-pancake-desktop.exe [-Backend winit-software] [-Screenshot out.png]
```

Checks: window appears at the requested position, foreground window never belongs to the
client (sampled every 20 ms), clean exit. Monitor layout of the dev machine: DISPLAY1
primary 1920×1080 at (0,0), DISPLAY2 1920×1080 at (1920,0).

## Manual acceptance (S0)

1. `cargo run --release -p turbo-pancake-app-server` → prints `TURBO_PANCAKE_LISTENING 127.0.0.1:7878`;
   `curl http://127.0.0.1:7878/v1/version` returns protocol_version 1.
2. `cargo run --release -p turbo-pancake-desktop -- --window-pos 2020,100 --no-activate` → a dark
   Discord-style window with empty DM list; the ☀/☾ button toggles light/dark theme;
   closing the window ends the process.

## Not yet covered (tracked in PROJECT_STATE.md)

OS-level netem (in-process impairment exists for audio), E2E between two desktop clients, PostgreSQL integration, security
negative tests for auth (no auth yet), cross-platform smoke on macOS/Linux hardware.
