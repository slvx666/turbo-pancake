# Testing

All commands run from the repository root. On the primary dev machine the build directory
is `D:/dsmini-target` (set in the gitignored `.cargo/config.toml`, because drive C: is
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

## Benchmarks (`benchmarks/README.md`)

```bash
cargo build --release -p dsmini-desktop -p dsmini-app-server -p dsmini-bench -p dsmini-media-probe
$T/release/dsmini-bench desktop-startup --exe $T/release/dsmini-desktop --runs 7 --settle-ms 3000 --label femtovg \
  --client-arg=--window-pos --client-arg=2020,100 --client-arg=--no-activate
$T/release/dsmini-bench signaling --server-exe $T/release/dsmini-app-server --messages 2000 --idle-connections 500
$T/release/dsmini-media-probe > benchmarks/results/media-probe-<os>-<ts>.json
```

## Desktop window smoke without disturbing the user (Windows)

The owner works on the same machine during tests. Rules: windows under test open on the
**secondary monitor** and **inactive** (`--window-pos 2020,100 --no-activate`); no test
moves the mouse or sends keyboard input.

```powershell
powershell -File benchmarks/scripts/window-check.ps1 -Exe D:/dsmini-target/release/dsmini-desktop.exe [-Backend winit-software] [-Screenshot out.png]
```

Checks: window appears at the requested position, foreground window never belongs to the
client (sampled every 20 ms), clean exit. Monitor layout of the dev machine: DISPLAY1
primary 1920×1080 at (0,0), DISPLAY2 1920×1080 at (1920,0).

## Manual acceptance (S0)

1. `cargo run --release -p dsmini-app-server` → prints `DSMINI_LISTENING 127.0.0.1:7878`;
   `curl http://127.0.0.1:7878/v1/version` returns protocol_version 1.
2. `cargo run --release -p dsmini-desktop -- --window-pos 2020,100 --no-activate` → a dark
   Discord-style window with empty DM list; the ☀/☾ button toggles light/dark theme;
   closing the window ends the process.

## Not yet covered (tracked in PROJECT_STATE.md)

netem/impairment tests, E2E between two desktop clients, PostgreSQL integration, security
negative tests for auth (no auth yet), cross-platform smoke on macOS/Linux hardware.
