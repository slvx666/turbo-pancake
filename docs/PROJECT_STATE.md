# Project State
Spec version: 1.0 (`TECHNICAL_SPEC_MESSENGER_RU.md`, owner copy outside the repo)
Current stage: S2 — group audio + SFU
Stage status: IN_PROGRESS (S0 = DONE; **S1 = BLOCKED** on 2026-11-02: everything works on Windows incl. a call between two homes, but the spec's real-device trials on macOS/Linux need hardware the owner does not have — formal blocker, so S2 starts per spec §16)
Last verified commit: 9af5491 (branch `main`, CI run 41503469373: Windows, Ubuntu, audit green; macOS job cancelled — no macOS arm64 runner acquired in 15 min (0 steps), as in runs 41480272959/41500008299; last green macOS: run 41457062573)
Platform matrix (S0 scope: build + tests + window smoke): Windows = PASS / macOS = PASS (CI runner, arm64) / Linux = PASS (CI, X11 via Xvfb; Wayland UNVERIFIED)
Last session: 2026-11-02

## Environment (dev machine, Windows)

* Windows 11 Pro 10.0.28000, AMD Ryzen 5 5500 (12 threads), 32 GB RAM,
  **NVIDIA RTX 2060 SUPER** (driver 617.14) — *not* the spec's RTX 4070 Super test machine.
* Toolchain: **rustc/cargo 1.99.1** (pinned in `rust-toolchain.toml`, installed by rustup into
  `%USERPROFILE%\.cargo`; PATH `$HOME/.cargo/bin`), MSVC 14.44 (VS 2022 Community),
  Windows SDK 10.0.28000. No system protoc (protox), no cmake/NASM needed (aws-lc-rs builds).
* Machine-local setup (build directory, test display, container engine, GitHub access) is
  kept in the untracked `CLAUDE.local.md`; Linux/macOS are verified through GitHub Actions.

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
| **Call driver** (S1): one thread per call, `UdpPeer` (non-blocking socket, ≤ 1 ms idle sleep), RTP-mode Opus track, extended seq/SRTP ROC, SDP offer/answer + candidate commands, states incl. Reconnecting (ICE + 3 s silence), stats (RTT, loss, loop gap), test wire loss | `crates/media-transport/src/{lib,call}.rs` | 1 unit + 3 UDP tests (`tests/call.rs`) + 2-client test (`tests/voice_call.rs`) |
| **Voice DSP** (S1): WebRTC APM 2.1.0 (vendored patched sys crate), NS + AEC3 (+HPF, optional AGC2) on the capture path, mix as render reference, live config; feature `apm` | `crates/media-audio/src/dsp.rs`, `third_party/webrtc-audio-processing-sys/` | 3 unit + 2 engine tests; CI job `apm` |
| **Audio engine** (S1): worker thread owning both streams, mic → `VoiceSender` → bounded outgoing channel; non-blocking `PacketSink` → per-peer `VoiceReceiver` (≤ 100 peers) → 48 kHz mix → output resample, speaker-paced playout with 40 ms prefill (wall clock without speaker), bounded command channel (mute, device switch, per-peer volume, remove peer, encoder config), status snapshot every 50 ms, failed stream → reopen selected → default → retry 1 s; `SimIo` simulated devices | `crates/media-audio/src/{engine,sim}.rs` | 5 integration tests `tests/engine.rs`; `examples/engine_check` on real Windows devices |
| **Desktop session + voice room** (S1): `Session` thread (Tokio current-thread) — `/v1/signal` hello/join/leave/relay/ping, reconnect with backoff 0.5→5 s and rejoin; engine (APM on by default) started on join, `CallDriver` per remote (later joiner offers), restart after `Failed`, `PeerLeft` ends the call, leave releases everything; Slint panel (room id, join/leave, mute, NS/AEC, input/output pickers, remote volume, mic level, state + RTT/loss/jitter stats); CLI `--server --name --room --mute --remote-volume --call-log --test-send-loss` | `apps/desktop/src/{lib,session,view,main}.rs`, `ui/main.slint` | 1 unit + 2 integration tests (`tests/session.rs`, real app-server + WS + UDP, simulated devices); `benchmarks/scripts/voice-trial.ps1` with real devices |
| **Per-member audio** (S2): SFU assigns an SSRC per member session and forwards every member to every other member (no transcoding, no renegotiation); `TrackPublished`/`TrackUnpublished`; client `CallDriver::expect_source`, engine peer per SSRC | `crates/media-sfu`, `proto/…/signaling.proto`, `crates/media-transport/src/call.rs`, `apps/desktop/src/session.rs` | `tests/session.rs::three_members_each_hear_both_others`, contract test, ADR 0008 |
| ADRs 0001–0008, API, ARCHITECTURE, TESTING, STAGES | `docs/` | — |

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
| Two real desktop clients talk through the server (signaling + WebRTC) | `powershell -File benchmarks/scripts/voice-trial.ps1 -Seconds 30 -Loss 0.05` (server + two release clients, real default devices, 2nd monitor, `--no-activate`) | **PASS (Windows, two homes, by ear, 2026-11-02)**: owner and a remote tester through the SFU build — both heard each other clearly (no metrics captured). **PASS (Windows, one machine, signaling over loopback, media over the LAN host candidate)**: joined, call connected 7–8 ms after the driver started, 30 s, A→B 1395 sent / 1282 received (5 % test loss), B played 1331 frames, FEC 57, concealed 0, late 0, decode errors 0, RTT 5–16 ms, 0 xruns, both exit cleanly and A sees B leave at once. Speakers ran at volume 0 (owner at work), so **audibility is UNVERIFIED**; two physical machines, macOS and Linux devices **UNVERIFIED**. Headless: `apps/desktop/tests/session.rs` PASS | `benchmarks/results/voice-trial-windows-20261102.log` |
| Speech continuity under moderate packet loss | `cargo test -p turbo-pancake-media-audio --test impairment` (in-process network model) | **PASS (pipeline level)**: loss ≤ 10 % × jitter ≤ 50 ms → 0 decode errors, 0 late, PLC-only ≤ 1.4 % of frames, FEC repairs ≈ 90 % of losses, level ratio ≥ 0.91; **PASS (headless, real UDP)**: two engines + call drivers, 5 % wire loss → 301 played, 17 FEC, 1 concealed, 0 late, 0 decode errors (`cargo test -p turbo-pancake-media-transport --test voice_call`); **PASS (two real desktop clients, Windows)**: 5 % loss → 57 FEC repairs, 0 concealed, 0 late (voice-trial log) | `benchmarks/results/audio-impairment-windows-20261102.json` |
| No audio loop | `tests/engine.rs::echo_canceller_breaks_the_audio_loop` (`--features apm`, simulated speaker→mic coupling) | **PASS (engine level, 3 OS)**: remote voice comes back with AEC3 + NS at −67.6 dB (Windows CI), −67.8 dB (Ubuntu), −44.1 dB (macOS VM) vs ≈ −7 dB without (CI run 41386258757); two real clients **UNVERIFIED** | ADR 0006 |
| Resources released after leave | `AudioEngine::stop`/drop: `tests/engine.rs` (simulated: 0 open streams, 0 device threads, engine thread joined); `examples/engine_check` (real Windows devices reopen right after stop, stop 15 ms) | **PASS (engine + call level, headless)**: `tests/voice_call.rs` hang-up → 0 streams, 0 device threads, call thread joined, socket closed; **PASS (desktop session, headless)**: `apps/desktop/tests/session.rs` leave/quit → 0 streams, 0 device threads, peer told in < 2 s; real clients exit cleanly after leaving (voice-trial) | `benchmarks/results/audio-engine-check-windows-20261102.json` |
| Opus in/out, mute, device selection | unit tests + `examples/device_check` + `tests/engine.rs` + `examples/engine_check` | **PASS (Windows, engine level)**: 192 kHz mic → Opus → 192 kHz stereo out, 0 overflows, ≈ 50 frames/s, 0 concealed; live input/output switch by id without sequence gaps (simulated + real), unplug fallback (simulated); UI device pickers implemented (switch through the session; not exercised by hand yet) | `benchmarks/results/audio-device-check-windows-20261102.json` |
| Basic DSP (NS/AEC) | `src/dsp.rs` unit tests + engine loop test (`--features apm`); `tools/apm-eval` | **PASS (Windows, engine level)**: NS default on + live toggle, AEC3 with the mix as reference, echo and NS tests pass; ≈ 60 µs/10 ms; builds and tests pass on Windows/Ubuntu/macOS (CI job `apm`, run 41386258757); on by default in the desktop build (feature `apm`, CI main job); real-device AEC with audible playback **UNVERIFIED** | ADR 0006, `benchmarks/results/audio-apm-eval-windows-20261102.json` |
| Trials recorded | this file + `benchmarks/results/audio-*.json` | **PASS** for the pipeline part | |
| Planned platforms | CI on Windows/Ubuntu/macOS (unit + impairment tests) | build/tests in CI; device I/O on macOS/Linux **UNVERIFIED** | CI run (see session log) |

## Acceptance checks — S2 (IN_PROGRESS)

| Criterion | Command / procedure | Result | Evidence |
|---|---|---|---|
| ≥ 3 real clients | three desktop clients on real devices (by hand) | **UNVERIFIED** (not run yet); headless 3 sessions: **PASS** | `cargo test -p turbo-pancake-desktop --test session three_members` |
| Automated 20-participant scenario | headless sessions + real server | **UNVERIFIED** (not written yet, S2 step 5) | |
| Join / leave | 3-member test: leave → others keep hearing each other, rejoin → all hear all | **PASS (headless)** | same test |
| Reconnection | signaling outage test (2 members) | **PASS (2 members, headless)**; group reconnect **UNVERIFIED** | `signaling_outage_ends_media_and_reconnects` |
| New call closes the previous one | — | **UNVERIFIED** (S2 step 4) | |
| No per-viewer transcoding | SFU forwards RTP payloads untouched (code + ADR 0008) | **PASS (by design)**; CPU/bandwidth for 20 members **UNVERIFIED** (S2 step 6) | `crates/media-sfu/src/lib.rs` |
| Per-member volume, speaking indicator, voice activation, AEC/NS toggle | — | volume per stream in the engine; UI per member, VAD **UNVERIFIED** (S2 step 3); AEC/NS toggle from S1 | |

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
12. Output startup underrun ≈ 22 ms before the first decoded frame in `device_check`; the
    engine now prefills 40 ms of silence, remaining output xruns ≈ 7 ms per stream open.
13. **cpal reports non-fatal events through the error callback** (WASAPI capture discontinuity → `ErrorKind::Xrun`). Treating them as fatal caused a reopen storm (213 reopens / 10 s). Fixed in `device.rs`; only device/stream-level kinds mark a stream failed.
14. Physical unplug and sleep/wake are not tested on real hardware (only simulated devices); engine thread runs at default OS priority.
15. Desktop release exe grew 9.1 → 17.9 MiB (APM, str0m/aws-lc, Tokio, Slint std-widgets); installer ≤ 30 MB target to be measured in S7.
16. Media: host + server-reflexive (STUN) candidates since 2026-11-02; no TURN, so symmetric NAT / CGNAT on both sides cannot connect. In the trial `rx_peak` stayed 0.000 with remote volume 0 although B's mic peak was 0.05 — the peak is measured after the volume; audible quality needs an owner-assisted trial.
17. The S1 room is joined by id without authentication (accounts in S5); a third member in a room is ignored (1:1 only).
19. **Media goes through the server since 2026-11-02 (ADR 0007):** UDP to the server or a
    WebSocket tunnel. Since S2 step 1 every member hears every other member as its own
    SSRC (ADR 0008). No E2EE (the SFU decrypts SRTP). No speaker cap yet: N·(N−1)
    forwarded streams (planned guard: last-N speakers, S2 step 6).
20. `apps/desktop/tests/session.rs` runs its tests one at a time (shared mutex): several
    real-time simulated clients in parallel debug builds starved each other (≈ 4 %
    concealed frames, failing the < 3 % bound); serial runs: 0 concealed. One serial run in
    ~10 still failed while the dev machine was busy with other work.
18. **Windows MAX_PATH:** the bundled APM (meson/abseil) release build fails with LNK1181 when the
    cargo target dir is long (`D:	urbo-pancake	urbo-pancake	arget`, CI run 41454787623;
    debug passed). CI uses `CARGO_TARGET_DIR=D:/t` on Windows; locally `D:/turbo-pancake-target`
    works. A packaging/build-from-source note is needed for S7.

## Design decisions / ADR links

* [0001 Workspace & toolchain](ADR/0001-workspace-and-toolchain.md) — accepted
* [0002 UI: Slint](ADR/0002-ui-slint.md) — provisional (license, GPU texture, frame times open)
* [0003 WebRTC: str0m](ADR/0003-webrtc-str0m.md) — provisional; crypto backend switched to
  aws-lc-rs after `cargo audit` flagged RUSTSEC-2026-0387 in the rust-crypto path
* [0004 Signaling protocol](ADR/0004-signaling-protocol.md) — accepted
* [0005 Media/OS research](ADR/0005-media-platform-research.md) — research
* [0007 Media through the SFU, UDP + WebSocket fallback](ADR/0007-media-through-sfu.md) — accepted
* [0008 Per-member audio: one m-line, one SSRC per member](ADR/0008-per-member-audio-streams.md) — accepted
* [0006 Voice pipeline](ADR/0006-voice-pipeline.md) — accepted for S1 (cpal, rtrb, rubato,
  libopus 1.6.1 via `opus` 0.4, own jitter buffer; APM open)

## Session log 2026-11-02

**Rename (owner request):** project, crates, binaries and protocol were renamed from the
working title "DsMini" to **turbo-pancake**: crates/binaries `turbo-pancake-*`, Rust crates
`turbo_pancake_*`, protobuf package `turbo_pancake.v1` (`proto/turbo_pancake/v1/`; wire
encoding unchanged, contract tests pass), stdout markers/env vars `TURBO_PANCAKE_*`
(e.g. `TURBO_PANCAKE_BIND`), build dir `D:/turbo-pancake-target`, project folder
`%USERPROFILE%\Desktop\...\turbo-pancake`. JSON reports in `benchmarks/results/`
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

CI run 41369109162 (commit e00ff86): **all jobs success** (Windows, Ubuntu, macOS, audit);
media-audio unit + impairment tests pass on all three. Codec cost on runners (complexity 9,
µs/frame encode / decode): Linux 176 / 20, macOS 278 / 31, Windows 308 / 38. Windows
runner has VS 18 (2026); the generator step printed a wrong year ("Visual Studio 18 18"),
fixed in the next commit with an explicit version→year map.

## Session log 2026-11-02 (S1, step 2.1 — AudioEngine)

Files: `crates/media-audio/src/{engine.rs,sim.rs}` (new), `src/{lib,device,pipeline}.rs`
(`VoiceSender::set_input_rate`, cpal `Xrun`/`RealtimeDenied` non-fatal,
`StreamHealth::{backend_xruns,realtime_denied}`, `AudioError::Engine`),
`tests/engine.rs`, `examples/engine_check.rs`, `docs/{ADR/0006,TESTING,ARCHITECTURE,STAGES,PROJECT_STATE}.md`,
`benchmarks/results/audio-engine-check-windows-20261102.json`.

Local (Windows): fmt OK; clippy `-D warnings` OK; `cargo test --workspace --locked`
41 passed (engine suite 5/5, repeated 5× without flakes, 1.4 s). `engine_check -- 20` on
fifine 192 kHz mic + Realtek 192 kHz output: 999 frames sent/looped/played, 0 concealed,
0 underruns, 0 dropped, 2 reopens (deliberate switches), output xruns 4224 samples
(≈ 22 ms over 3 output opens: startup/switch), start 55 ms, stop 15 ms, reopen after stop OK.

## Session log 2026-11-02 (S1, step 2.2 — APM evaluation)

CI run 41378979777 (commit 23ce9dd, AudioEngine): **all jobs success** (Windows, Ubuntu,
macOS, audit); engine tests pass on all three with simulated devices.

APM (`webrtc-audio-processing` 2.1.0 `bundled`) built on Windows only after: meson+ninja
(venv `D:/turbo-pancake-tools/venv`), clang-cl, `nm` (= `llvm-nm`, `D:/turbo-pancake-tools/bin`),
Git `cp`, `rustup component add llvm-tools` (**+383 MB on C:**, remove with
`rustup component remove llvm-tools` if C: runs out), `vcvars64.bat`, and the patch in
`third_party/webrtc-audio-processing-sys/`. Numbers in ADR 0006. Files: `tools/apm-eval/**`,
`third_party/webrtc-audio-processing-sys/2.1.0-windows.patch`,
`benchmarks/results/audio-apm-eval-windows-20261102.json`, ADR 0006, this file.

## Session log 2026-11-02 (S1, step 2.3 — APM in the engine)

Owner left the choice to us → vendored the patched sys crate (ADR 0006 decision).
Files: `third_party/webrtc-audio-processing-sys/**`, `Cargo.toml` (`exclude`,
`[patch.crates-io]`), `Cargo.lock`, `crates/media-audio/{Cargo.toml,src/dsp.rs,
src/engine.rs,src/pipeline.rs,src/sim.rs,src/lib.rs,tests/engine.rs,examples/engine_check.rs}`,
`benchmarks/scripts/apm-env.ps1` (Windows build environment), `.github/workflows/ci.yml`
(job `apm`), docs. Local: default fmt/clippy/test OK (43 tests); `--features apm`
clippy OK and tests OK (media-audio 14 unit + 7 engine + 2 impairment).

CI fixes on the way (commits 5541d70, 5e9d61a, bf85c04): `apm-env.ps1` must be run with
`-File` (direct invocation drops `--`); on the windows-latest image (VS 18) a separate LLVM
is on PATH whose clang-cl/libclang do not match the VS clang headers that vcvars puts on
INCLUDE → CC/CXX by full path and `LIBCLANG_PATH` to the VS LLVM; macOS VM reached only
−24.6 dB once in the simulated room (device-thread jitter) → loop test 10 s, bound −20 dB.

## Session log 2026-11-02 (S1, step 3 — call driver)

Files: `crates/media-transport/{Cargo.toml,src/lib.rs,src/call.rs,tests/call.rs,
tests/voice_call.rs}`, `Cargo.lock`, docs (ADR 0003, ARCHITECTURE, TESTING, STAGES, this
file). Found and fixed on the way (ADR 0003): extended sequence numbers / SRTP ROC,
Windows 16 ms socket-timeout granularity (loss under load, +16 ms send latency), answerer
not noticing a vanished offerer. Local: fmt/clippy OK, `cargo test --workspace --locked`
48 passed; transport suite repeated 3× without failures.

## Session log 2026-11-02 (S1, step 4 — desktop session + voice room UI)

Files: `apps/desktop/{Cargo.toml,src/lib.rs,src/session.rs,src/view.rs,src/main.rs,ui/main.slint,tests/session.rs}`,
`crates/media-audio/src/sim.rs` (`SimIo::with_state`), `.github/workflows/ci.yml` (APM tools in
the main job, job `apm` removed — the workspace build enables `media-audio/apm` through the
desktop), `benchmarks/scripts/voice-trial.ps1`, `benchmarks/results/voice-trial-windows-20261102.log`,
`docs/{ARCHITECTURE,TESTING,PROJECT_STATE}.md`. Local (Windows, through `apm-env.ps1`): fmt OK,
clippy `-D warnings` OK, `cargo test --workspace --locked` 56 passed (session suite 3× without
failures); release build OK; voice trial as in the S1 table. Commits f258e45, ea298db, c28fd19.
CI run 41454787623: macOS/Ubuntu green, Windows clippy + all tests green but the APM release
build failed (MAX_PATH, issue 18); fixed by a short target dir → **CI run 41457062573 all green**.

## Session log 2026-11-02 (S1 — first call between two homes, STUN)

The owner and a remote tester (Windows) tried a call over the internet: the tester ran
`turbo-pancake-app-server --bind 0.0.0.0:7878` on a public IP (TCP 7878 reachable,
`/v1/version` answered), the owner's client joined room `test` and saw the tester
(`others=1`), but every call attempt ended in `connect timeout` (33 attempts in 5 min):
only private host candidates were offered. Added STUN (server-reflexive candidates, ADR
0003): `crates/media-transport/src/{stun.rs,lib.rs,call.rs}`, `tests/stun.rs`,
`apps/desktop` (`SessionConfig::stun_servers`, `--stun`, `--no-stun`, public address and
remote address in the stats line and `--call-log`). The two-homes call itself is
**UNVERIFIED** until the tester runs the new client.

## Session log 2026-11-02 (S1 — media through the server, works behind VPNs)

The STUN build still failed between the two homes (owner behind a VPN client in TUN mode; with VPNs
off too). Owner requirement: calls must work reliably with and without a VPN. Decision
(ADR 0007): media through the server — new `crates/media-sfu` (ICE-lite str0m per member,
RTP forwarding, one UDP socket on the WebSocket port number, WebSocket tunnel via
`MediaDatagram`), protocol v1 `MediaOffer`/`MediaAnswer`/`MediaDatagram` (+ contract and
validation tests), `app-server` (`--media-port`, `--no-media-udp`, `--public-ip`),
`media-transport` tunnel link (`UdpPeer::tunnel`, `MediaLink`, `TUNNEL_CLIENT/SERVER`),
desktop session rewritten (media session with the SFU after join, `TransportPref::Auto`
UDP 3 s → tunnel, alternating; members list; `--media auto|udp|tcp`, `--test-block-udp`).
Found and fixed in the SFU: a member's new session after rejoin restarts its sequence
numbers; listeners kept extending the old ones and SRTP dropped the "replayed" packets.
`apps/desktop/tests/session.rs`: 4 scenarios incl. **all UDP dropped → WebSocket, audio
flows** and server without UDP — 3 runs without failures. Release clients on this machine
(`voice-trial.ps1 -Seconds 20 -Loss 0.05`, real devices, volume 0): **UDP via SFU** — B
played 824, FEC 43, concealed 0, late 0, decode errors 0, RTT 8–22 ms; **`--test-block-udp`
on both** — switched to the WebSocket (2nd media session), B played 711, FEC 37, concealed
0, decode errors 0, RTT ≈ 15 ms (`benchmarks/results/voice-trial-sfu-*-windows-20261102.log`).
Local: clippy OK, `cargo test --workspace --locked` 62 passed. The real two-homes call is
**UNVERIFIED** until the owner and the tester run the new server and client.

## Session log 2026-11-02 (S1 — first call between two homes works)

Owner and a remote tester (both Windows, different home networks, server on the tester's
machine) ran the SFU build: **the call worked, both heard each other clearly** (owner's
report: "всё слышно и вроде всё хорошо"). Transport used (UDP or WebSocket), VPN on/off,
RTT/loss and echo behaviour were **not recorded** — no `--call-log` was captured.
CI run 41472046421 (commit 6edbda7): Windows green; Ubuntu failed in `tests/stun.rs` —
Linux returns EINVAL for a datagram from a loopback-bound socket to a public srflx
address and `UdpPeer` treated it as fatal; now EINVAL/EADDRNOTAVAIL/EPERM on one send are
ignored like unreachable errors (ICE consent decides). macOS jobs in runs 41459625029 and
41472046421: runner not acquired / cancelled (GitHub capacity), not a test failure.

## S1 blocker (2026-11-02)

macOS and Linux real-device voice trials (spec §17 S1: planned platforms must pass before
DONE) cannot run: the owner has no Mac or Linux machine. Builds and all tests pass in CI on
Ubuntu and macOS arm64 (simulated devices). Unblock: any Apple Silicon Mac / Linux box →
run `voice-trial` (or two clients against one server) and record. Still open on Windows
and doable any time: a measured two-homes run with `--call-log` (with/without VPN), the
DTLS backend benchmark (ADR 0003).

## Session log 2026-11-02 (S2, step 1 — per-member audio)

Decision ADR 0008: one m-line, one SSRC per member, mapping by `TrackPublished`. Found in
str0m 0.24.1 on the way: (1) with several SSRCs on one m-line the receiver re-maps an
unknown SSRC carrying the `mid` extension onto the existing stream → the SFU no longer
negotiates `mid`; (2) the pacer queues by (mid, rid), a second stream without a rid was
never sent (first test run: clients received 0 packets) → one rid per forwarded stream.
Files: `proto/turbo_pancake/v1/signaling.proto`, `crates/protocol/tests/contract.rs`,
`crates/media-sfu/src/lib.rs`, `apps/app-server/src/signaling.rs`,
`crates/media-transport/src/call.rs`, `apps/desktop/src/session.rs`,
`apps/desktop/tests/session.rs`, `docs/ADR/0008-per-member-audio-streams.md`, ADR 0007,
`docs/{API,ARCHITECTURE,STAGES,PROJECT_STATE}.md`.
Local (Windows, through `apm-env.ps1`): fmt OK, clippy `-D warnings` OK,
`cargo test --workspace --locked` 64 passed; session suite 5× in a row green (serial).
CI run 41500008299 (commit 87294fc): Windows + audit green, macOS cancelled (runner),
Ubuntu failed `signaling_outage_ends_media_and_reconnects` ("audio after reconnect"): the
old stream's packets still queued in the engine re-created its peer after `remove_peer`
(a silent ghost stream next to the new one). Fixed: `RemovePeer` drains the incoming
queue first, and the session drops packets of SSRCs it no longer accepts (checked on the
call thread); the test now asserts exactly one stream after reconnect. Local: 64 passed,
session suite 4× green.
CI run 41503469373 (commit 9af5491): Windows, Ubuntu (incl. the 3-member and outage tests),
audit green; macOS runner not acquired (cancelled, 0 steps).

## Exact next task — S2 step 3 (plan in `docs/STAGES.md` → "S2 plan")

Client side of group calls: `Snapshot` lists members with their stream (peer id → SSRC →
engine `PeerStatus`), Slint member list with a volume slider and a speaking indicator per
member (`Command::SetMemberVolume(peer_id, v)`), and voice activation with an adjustable
threshold in the engine (stop sending below the threshold with a hangover; indicator for
the own mic). Then step 5: automated 20-member headless scenario.

## Previous next-task list — S1 (plan in `docs/STAGES.md` → "S1 plan")

1. ~~AudioEngine~~ — done (see session log above).
2. ~~APM~~ — done, green on 3 OS. When the desktop starts using `media-audio` (item 4),
   depend on it with `features = ["apm"]` and move the meson/ninja/llvm-tools/clang-cl
   setup from job `apm` into the main CI job.
3. ~~Call driver~~ — done (`CallDriver`, see session log).
4. ~~Desktop signaling client + call UI + two clients on this machine~~ — done (see log).
5. ~~Call between two homes~~ — works by ear (owner + tester, 2026-11-02); a measured run
   with `--call-log` on both sides (with and without VPN) is still to be recorded.
   Was: the call between two homes with the SFU build (the tester runs the new
   `turbo-pancake-app-server --bind 0.0.0.0:7878`, TCP 7878 forwarded, UDP 7878 optional;
   both run the new client, with and without VPN; record transport, RTT, loss, played/FEC/
   concealed from `--call-log`), then the owner-assisted audible trial — two machines on one LAN (or headphones on one
   machine): `turbo-pancake-app-server --bind 0.0.0.0:7878` on one, clients with
   `--server ws://<ip>:7878/v1/signal --room test --call-log`, talk, toggle NS/AEC/mute,
   switch devices in the UI; record speech quality, echo, device switching. Needs the owner.
6. macOS and Linux real-device runs of `voice-trial` (owner's M4; a Linux box with X11 and
   Wayland) — required before S1 DONE (spec §17 S1).
7. DTLS backend benchmark (aws-lc-rs vs wincrypto) in the spike → ADR 0003.
8. ~~Server-reflexive (STUN) candidates~~ — done 2026-11-02 (see log); verify with the remote
   tester (two homes, server on the tester's public IP): superseded by ADR 0007.
