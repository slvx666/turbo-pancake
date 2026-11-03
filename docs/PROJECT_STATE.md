# Project State
Spec version: 1.0 (`TECHNICAL_SPEC_MESSENGER_RU.md`, owner copy outside the repo)
Current stage: S3 — screen share 1080p60 + system audio (IN_PROGRESS since 2026-11-03: step 1 capture + encode probe on Windows done)
Stage status: S0 = DONE; **S1 = BLOCKED** (2026-11-02: everything works on Windows incl. a call between two homes, but the spec's real-device trials on macOS/Linux need hardware the owner does not have — formal blocker); **S2 = DONE** (2026-11-03, every criterion PASS, see the S2 table; per spec §16 work continues while S1 stays blocked)
Last verified commit: 6596928 (branch `main`, CI run 41654959430: Windows, Ubuntu, audit green; macOS arm64 job never got a runner — cancelled with 0 steps, as in runs 41641056166 and 41647867149 ×3; last macOS run that executed tests: 41588139375, before the SFU burst fix — see known issue 26)
Platform matrix (S0 scope: build + tests + window smoke): Windows = PASS / macOS = PASS (CI runner, arm64) / Linux = PASS (CI, X11 via Xvfb; Wayland UNVERIFIED)
Last session: 2026-11-03

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
| **Screen capture + encode probe** (S3 step 1): `list` (monitors with refresh rate, hardware H.264/HEVC/AV1 encoder/decoder MFTs, adapter); `run` — Windows.Graphics.Capture monitor item → own D3D11 copy (newest frame wins) → D3D11 video processor BGRA→NV12 + scale → hardware H.264 MFT (async, low latency, CBR, GOP), fixed-rate pacing; JSON with capture/encoded FPS, latency percentiles, bitrate, CPU; `--pattern` draws a moving test pattern in a window that never activates | `tools/screen-probe` | Windows reports in `benchmarks/results/screen-probe-windows-*.json`; foreground sampling 0 of 112; macOS/Linux print UNVERIFIED |
| **Voice pipeline** (S1): cpal capture/playback on lock-free `rtrb` rings with xrun/failure counters, device listing/selection by id, rubato FFT resampler (any rate ↔ 48 kHz), Opus 48 kHz mono 20 ms (libopus 1.6.1 static, FEC/DTX/complexity/bitrate at runtime), adaptive jitter buffer with FEC→PLC, `VoiceSender` (mute = encoded silence, peak level) / `VoiceReceiver` (per-peer volume, stats) | `crates/media-audio` | 11 unit + 2 impairment tests (3 OS in CI); `examples/device_check` on real Windows devices; `examples/codec_cost` |
| **Call driver** (S1): one thread per call, `UdpPeer` (non-blocking socket, ≤ 1 ms idle sleep), RTP-mode Opus track, extended seq/SRTP ROC, SDP offer/answer + candidate commands, states incl. Reconnecting (ICE + 3 s silence), stats (RTT, loss, loop gap), test wire loss | `crates/media-transport/src/{lib,call}.rs` | 1 unit + 3 UDP tests (`tests/call.rs`) + 2-client test (`tests/voice_call.rs`) |
| **Voice DSP** (S1): WebRTC APM 2.1.0 (vendored patched sys crate), NS + AEC3 (+HPF, optional AGC2) on the capture path, mix as render reference, live config; feature `apm` | `crates/media-audio/src/dsp.rs`, `third_party/webrtc-audio-processing-sys/` | 3 unit + 2 engine tests; CI job `apm` |
| **Audio engine** (S1): worker thread owning both streams, mic → `VoiceSender` → bounded outgoing channel; non-blocking `PacketSink` → per-peer `VoiceReceiver` (≤ 100 peers) → 48 kHz mix → output resample, speaker-paced playout with 40 ms prefill (wall clock without speaker), bounded command channel (mute, device switch, per-peer volume, remove peer, encoder config), status snapshot every 50 ms, failed stream → reopen selected → default → retry 1 s; `SimIo` simulated devices | `crates/media-audio/src/{engine,sim}.rs` | 5 integration tests `tests/engine.rs`; `examples/engine_check` on real Windows devices |
| **Desktop session + voice room** (S1): `Session` thread (Tokio current-thread) — `/v1/signal` hello/join/leave/relay/ping, reconnect with backoff 0.5→5 s and rejoin; engine (APM on by default) started on join, `CallDriver` per remote (later joiner offers), restart after `Failed`, `PeerLeft` ends the call, leave releases everything; Slint panel (room id, join/leave, mute, NS/AEC, input/output pickers, remote volume, mic level, state + RTT/loss/jitter stats); CLI `--server --name --room --mute --remote-volume --call-log --test-send-loss` | `apps/desktop/src/{lib,session,view,main}.rs`, `ui/main.slint` | 1 unit + 2 integration tests (`tests/session.rs`, real app-server + WS + UDP, simulated devices); `benchmarks/scripts/voice-trial.ps1` with real devices |
| **Per-member audio** (S2): SFU assigns an SSRC per member session and forwards every member to every other member (no transcoding, no renegotiation); `TrackPublished`/`TrackUnpublished`; client `CallDriver::expect_source`, engine peer per SSRC | `crates/media-sfu`, `proto/…/signaling.proto`, `crates/media-transport/src/call.rs`, `apps/desktop/src/session.rs` | `tests/session.rs::three_members_each_hear_both_others`, contract test, ADR 0008 |
| **Group call client** (S2 step 3): member list (peer id → SSRC → engine stream) with a speaking indicator and a volume slider per member (`Command::SetMemberVolume`, on top of the call volume); voice activation with an adjustable threshold (`Command::SetVoiceGate`, dBFS, 300 ms hangover; the sequence number pauses, the RTP timestamp runs on); own speaking indicator and a dB microphone meter with the threshold mark; jitter buffer measures transit by RTP timestamp and starts a talkspurt after a pause instead of discarding it as late | `crates/media-audio/src/{pipeline,jitter,engine}.rs`, `apps/desktop/src/{session,view,main}.rs`, `apps/desktop/ui/main.slint` | unit tests `pipeline::tests::*`, `jitter::tests::{talkspurt_*,short_pause_*}`, `tests/engine.rs::voice_activation_pauses_sending_and_drives_the_indicators`, `tests/session.rs::speaking_indicators_member_volume_and_voice_activation` |
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

## Acceptance checks — S2 (DONE 2026-11-03)

| Criterion | Command / procedure | Result | Evidence |
|---|---|---|---|
| ≥ 3 real clients | `powershell -File benchmarks/scripts/group-trial.ps1 -Seconds 60` — app-server + three release desktop clients (real default microphone and speakers, windows on the 2nd monitor, inactive), C with voice activation at −45 dBFS | **PASS (Windows, one machine, real devices, 2026-11-03)**: all three joined and stayed connected over UDP, each saw the two others; A↔B ≈ 2 200 frames each way in 45 s, 0 concealed, 0 late, 0 decode errors; C received both streams (2 215 / 2 216 frames, 0 concealed) and, its microphone being below the threshold, sent 0 frames (2 222 gated); all exited cleanly (exit 0). The room was quiet (mic ≈ −80 dBFS) and the call volume 0, so **by-ear quality with three people is UNVERIFIED** (by ear: two homes in S1); three separate machines **UNVERIFIED**. Headless 3 sessions: **PASS** | `benchmarks/results/group-trial-windows-20261103.log`, `cargo test -p turbo-pancake-desktop --test session three_members` |
| Automated 20-participant scenario | 20 desktop `Session`s (simulated devices, real UDP) through one in-process app-server + SFU: 3 talk, 17 listen with voice activation and a silent mic. Everyone hears every talker (listeners 3 streams, talkers 2), speaking indicators match, listeners send 0 frames; a talker leaves + a listener switches rooms → the 17 left hear 2 talkers; both return → all hear 3; server outage → all 20 reconnect and hear 3 again | **PASS (headless, Windows debug build, 3 runs)**: all heard after ≈ 1.97 s, recovered after outage ≈ 2.4 s, worst concealed 0.0 %, test process CPU 9.8–15.2 s over 6.0–7.3 s wall (≈ 1.6 of 12 logical CPUs for 20 clients + server) | `cargo test -p turbo-pancake-desktop --test session twenty -- --nocapture` (prints `TURBO_PANCAKE_TWENTY …`) |
| Join / leave | 3-member test: leave → others keep hearing each other, rejoin → all hear all | **PASS (headless)** | same test |
| Reconnection | signaling outage: 2 members, and 3 members (server restart → all rejoin, each hears both others as exactly 2 fresh streams) | **PASS (headless)**; real network roaming **UNVERIFIED** | `signaling_outage_ends_media_and_reconnects`, `group_call_reconnects_after_a_server_outage` |
| New call closes the previous one | server: join of another room → `RoomLeft{old}`, old members `PeerLeft{SWITCHED}`, SFU session ends; repeated join/leave idempotent; full room refuses without ending the current call. Client: 3 sessions, Алиса switches from Боб's room to Вера's — Боб loses her stream at once, Алиса and Вера hear each other, repeated join changes nothing | **PASS (headless, per connection)**; per *user* across devices **UNVERIFIED** until accounts (S5) | `apps/app-server/tests/signaling.rs::{joining_another_room_closes_the_previous_call,switch_to_a_full_room_keeps_the_current_call}`, `apps/desktop/tests/session.rs::joining_another_room_closes_the_previous_call` |
| No per-viewer transcoding | SFU forwards RTP payloads untouched (code + ADR 0008); load measured with `turbo-pancake-bench sfu` (release server process, 20 real call drivers) | **PASS**: no transcoding by design; 20 members, 1 / 3 / 10 / 20 talking → server 6.9 / 6.2 / 14.4 / 25.4 % of one core, 951 – 18 991 packets/s out, 99–100 % delivered, egress ≈ 1.3 – 25.2 Mbit/s (re-measured after the burst fix) | `crates/media-sfu/src/lib.rs`, `benchmarks/results/sfu-20m-windows-1793711275.json`, `sfu-50m-windows-1793711325.json` |
| Per-member volume, speaking indicator, voice activation, AEC/NS toggle | 3 headless members through the real server: speaking indicators (own + per member), voice activation stops sending in silence (0 frames sent, ≥ 25 gated) and the next talkspurt plays (late ≤ 5), member volume 0 silences only that member, call volume scales all | **PASS (headless)**; UI rendered on Windows (window-check screenshot, not driven by input); by-ear check with real devices **UNVERIFIED**; AEC/NS toggle from S1 | `cargo test -p turbo-pancake-desktop --test session speaking_indicators`, `cargo test -p turbo-pancake-media-audio --test engine voice_activation` |

## Acceptance checks — S3 (IN_PROGRESS)

| Criterion | Command / procedure | Result | Evidence |
|---|---|---|---|
| Real share with measured frames, latency, bitrate | step 1 probe only: `turbo-pancake-screen-probe run --monitor 1 --seconds 10 --pattern` (secondary monitor, 1920×1080 @ 60 Hz, RTX 2060 SUPER, NVIDIA H.264 Encoder MFT, 8 Mbit/s CBR requested) | **UNVERIFIED** (no transport/viewer yet). Probe: capture 54.9 fps, encoded 57.8 fps, capture → encoded p50 18.4 / p95 30.3 / p99 43.0 ms, submit → encoded p50 6.7 / p95 16.3 ms, 983 kbit/s on the simple pattern (encoder does not pad CBR), 10 keyframes / 10 s, process CPU 5.6 % of one core. Static screen: capture 0.3 fps, encoder repeats the last frame at 57 fps | `benchmarks/results/screen-probe-windows-1080p60-pattern-2026-11-03.json`, `…-1080p60-2026-11-03.json` |
| System audio without echo, own sound excluded | — | **UNVERIFIED** (step 6) | — |
| Stop / switch monitor | — | **UNVERIFIED** (step 5) | — |
| Platform limits stated | macOS/Linux probe prints UNVERIFIED; no hardware for trials (same as S1) | **UNVERIFIED** | — |

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
    forwarded streams; speaker cap deferred with measurements (ADR 0008, issue 25).
20. `apps/desktop/tests/session.rs` runs its tests one at a time (shared mutex): several
    real-time simulated clients in parallel debug builds starved each other (≈ 4 %
    concealed frames, failing the < 3 % bound); serial runs: 0 concealed. One serial run in
    ~10 still failed while the dev machine was busy with other work.
    **Fixed 2026-11-03:** the macOS failures (CI 41559716784 attempt 1, 41575409729,
    41584876386, 41588139375: ≈ 35 % of RTP missing, also over the WebSocket tunnel) were
    not CPU starvation but an SFU bug: str0m in RTP mode keeps one received packet until
    `poll_output`, and the SFU handed a whole batch of datagrams in before polling, so a
    member's burst lost all packets but the last (SFU counters: 403 RTP out of str0m for
    ≈ 645 sent). macOS clients send in bursts (loop gaps up to 56 ms); Windows/Linux
    rarely. The SFU now polls after every datagram (`crates/media-sfu/tests/burst.rs`:
    20 → 100 of 100 forwarded).
21. **Fixed 2026-11-03 — `window-check.ps1` saw the window in the foreground** (109 of 110
    samples) although it was created with `--no-activate`: `Start-Process` goes through
    ShellExecute, whose STARTUPINFO show mode (SW_SHOWNORMAL) Windows applies to a
    process's first `ShowWindow` instead of winit's SW_SHOWNOACTIVATE, and a process
    started by the foreground app may take the foreground. The script now launches through
    CreateProcess (`UseShellExecute = false`): 0 of 110. Launchers with redirected output
    (voice-trial, group-trial, the bench harness) already used CreateProcess. Any new GUI
    launcher must do the same.
22. Per-member volumes are keyed by the connection's peer id and reset on reconnect or
    rejoin (accounts with stable ids arrive in S5).
23. With voice activation the receiver conceals ~200 ms after each talkspurt (counted as
    concealed/underrun ticks) before it rebuffers; Opus PLC fades out, so it is quiet, but
    `concealed_ratio` overstates loss for gated senders. A comfort-noise / end-of-talkspurt
    marker would remove it.
24. One active call is enforced per connection; the same person on two devices can be in
    two rooms until accounts (S5) give a user identity to enforce it per user.
25. **The SFU is one thread:** ≈ 11–14 µs per forwarded packet, saturates one core at
    ≈ 72–78 000 packets/s out (≈ 1 500 concurrent streams); 50 members all talking
    delivered 59–64 % (dev machine shared with other work). No speaker cap yet (ADR 0008:
    needs the audio-level extension from clients) and no sharding across cores. Fine for
    S2 (20 members ≤ 26 % of a core); revisit for 100-member rooms with many simultaneous
    talkers or many busy rooms (S7 load tests).
26. **macOS CI after the SFU burst fix is UNVERIFIED:** from 2026-11-03 13:00 UTC GitHub did
    not assign a macOS arm64 runner (runs 41641056166, 41647867149 ×3, 41654959430: cancelled after
    15 min queued, 0 steps). The failures there are explained by SFU counters (RTP
    overwritten in str0m before forwarding) and the fix is covered by a deterministic test
    that fails on the old code (`crates/media-sfu/tests/burst.rs`), green on Windows and
    Ubuntu. First thing next session: check the newest CI run's macOS job (session suite
    green ⇒ close this issue).
27. **H.264 licensing — decided 2026-11-03 (ADR 0009):** OS / vendor codecs only, nothing
    bundled; no encoder ⇒ screen share unavailable with a clear message.
28. S3 probe findings: `MFTEnumEx(HARDWARE)` lists no H.264 *decoder* on Windows — the
    Microsoft H.264 decoder MFT is a software MFT that uses DXVA when given a D3D manager, so
    decode acceleration must be checked by enabling D3D on it (step 3). The probe pattern
    window paces with `DwmFlush`, which follows the primary monitor's refresh (320 Hz here),
    so it changes content faster than the 60 Hz captured monitor — fine for FPS, but the
    end-to-end latency marker (step 7) must be timestamp-based. CBR on NVENC does not pad:
    simple content gives ≈ 1 Mbit/s of 8 requested.
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
* [0009 H.264: OS/vendor codecs only](ADR/0009-h264-os-encoders-only.md) — accepted (owner, 2026-11-03)
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

## Session log 2026-11-03 (S2 completion — macOS loss, focus, three real clients)

* macOS CI failures traced with new counters (`CallStats.send_errors`,
  `SfuStats.udp_send_errors`, `GET /v1/media-stats`, session tests print both sides'
  engine/call counters and the SFU's on failure) to an SFU bug: RTP overwritten in
  str0m's single pending-packet slot when a member's datagrams were handed in as a batch.
  Fixed (poll after every datagram), regression test `crates/media-sfu/tests/burst.rs`
  (old code 20/100, new 100/100). SFU bench re-run: numbers unchanged within noise
  (20 members all talking 25.4 % of a core, 100 % delivered; 50 talking saturates).
* Known issue 21 fixed: `window-check.ps1` launches through CreateProcess → 0/110
  foreground samples (was 109/110).
* Three real clients: `benchmarks/scripts/group-trial.ps1` (new), 60 s, see the S2 table;
  `--call-log` now prints per-member `members=name:played:concealed%:speaking`, device
  names are scrubbed from the committed log.

## Session log 2026-11-03 (S2, step 6 — SFU load)

New bench scenario `turbo-pancake-bench sfu` (`benchmarks/harness/src/sfu.rs`): release
server in its own process, N members with real signaling + `CallDriver` (no audio engine),
synthetic 100-byte frames every 20 ms from the first k members, server CPU from process CPU
time. Results (Windows dev machine, loopback): 20 members, talkers 1/3/10/20 → 6.7 / 6.9 /
13.9 / 18.7 % of one core, 19 014 packets/s out at 20 talkers, 100 % delivered; 50 members,
talkers 3/10/25/50 → 11 / 26 / 67 / 100 %, delivered 100 / 100 / 100 / 64 %. Decision
(ADR 0008): no speaker cap in S2; a cap needs RFC 6464 audio level from clients, recorded
with the alternative of one SFU worker per core. Client decode of 19 streams ≈ 19 × 29 µs
per 20 ms ≈ 2.8 % of a core (decode cost from the S1 step 1 benchmark).

## Session log 2026-11-03 (S2, step 5 — 20-member scenario)

New test `twenty_members_hear_every_talker_through_leave_switch_and_reconnect` in
`apps/desktop/tests/session.rs` (full client sessions, no lighter harness needed: with only
three senders each client decodes at most three streams). Local runs (Windows, debug):
`TURBO_PANCAKE_TWENTY members=20 talkers=3 all_heard_after_ms=1964..1976
recovered_after_ms=2379..2423 worst_concealed=0.0000 total_ms≈5.7 s`; test process CPU
measured from outside (`TotalProcessorTime`): 15.16 / 9.91 / 9.75 s for 7.3 / 6.0 / 6.0 s
wall. Full workspace: fmt OK, clippy OK, 77 tests passed twice in a row.
Not covered: all 20 talking at once (decode load N·(N−1)) — measured in step 6 together
with SFU CPU/egress.
CI run 41559716784 (commit 7de3154): attempt 1 — Windows, Ubuntu green; macOS passed the
20-member test but the two tests that ran right after it failed the < 3 % concealment
bound (`two_sessions_call_through_the_server` 10 %: 200 played, 113 by FEC, 20 concealed;
`vpn_without_udp_falls_back_to_the_websocket` ≈ 18 %), i.e. packets arrived in bursts.
Not reproduced locally with the whole session suite pinned to 3 logical CPUs (2 runs,
9/9). Attempt 2 (failed jobs re-run): all green incl. macOS. Recorded under issue 20.

## Session log 2026-11-03 (S2, step 4 — call lifecycle)

Server (`apps/app-server/src/signaling.rs`): `JoinRoom` while in another room leaves it
first (`PeerLeft{reason=SWITCHED}` to its members, SFU session ended, `RoomLeft{old}` to
the joiner before `RoomJoined`); a full target room is refused before leaving; repeated
`JoinRoom` of the current room re-sends `RoomJoined`; `LeaveRoom` of a room we are not in
just confirms. Proto: `LEAVE_REASON_SWITCHED = 4` (contract test), `ALREADY_IN_ROOM`
reserved. Client: a server-initiated `RoomLeft` for the current room ends the call.
Tests: 2 server tests, 2 session tests (room switch with 3 members, group reconnect).
Local (Windows, `apm-env.ps1`): fmt OK, clippy `-D warnings` OK, `cargo test --workspace
--locked` 76 passed.
CI run 41551356643 (commit ecb505e): Windows, Ubuntu, macOS arm64 (first green macOS since
run 41457062573, so the S2 code incl. steps 1–3 now passes on macOS too) and audit green.

## Session log 2026-11-03 (S2, step 3 — group call client)

Engine: `VoiceSender::set_voice_gate` (dBFS threshold, 15-frame hangover; below it frames
are not encoded or sent, the sequence number pauses and the RTP timestamp keeps running);
own `speaking`, `mic_db`, `frames_gated` in `EngineStatus`; `ReceiverStats::speaking`
(decoded speech above −45 dBFS before the local volume, 300 ms hangover). Jitter buffer:
transit by extended RTP timestamp (a pause is not jitter) and talkspurt detection
(timestamp − seq·frame grew) — found while designing: with contiguous sequence numbers the
old buffer would have dropped the next talkspurt as late and raised the target delay by the
pause length. `IncomingPacket` carries the RTP timestamp. Session: `Snapshot.members` is
`Vec<Member>` (peer id, name, SSRC, own volume, engine stream), `Command::SetMemberVolume`,
`Command::SetVoiceGate`; engine gain = call volume × member volume. UI: member list with a
speaking ring and a volume slider per member (updated in place so a dragged slider is not
rebuilt), voice activation checkbox + threshold slider, dB meter with the threshold mark;
`--voice-gate <dB>`; `--call-log` gains `streams`, `speaking`, `vad_db`, `frames_gated`,
`speaking_members`.
Local (Windows, through `apm-env.ps1`): fmt OK, clippy `-D warnings` OK,
`cargo test --workspace --locked` 71 passed (new: 3 pipeline, 2 jitter, 1 engine, 1 session).
GUI: `window-check.ps1` — window at the requested position, exit 0, UI renders the new
controls; but the window was foreground (known issue 21), so no further GUI runs.
CI run 41540292365 (commit 7c36c8d): Windows, Ubuntu (incl. the new engine and session
tests), audit green; macOS runner not acquired (cancelled, 0 steps).

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

## Session log 2026-11-03 (S3, step 1 — capture + encode probe)

* CI 41661382739: Windows, Ubuntu, audit green; macOS arm64 again got no runner (issue 26 open).
* Reviewed and committed `tools/screen-probe` (clippy fixes, back in the workspace), added
  `--pattern` (non-activating moving test window on the captured monitor). Results in the S3
  table above; foreground sampled every 50 ms during a 6 s run: 0 of 112 samples.

## Exact next task — S3 step 2/3

1. ~~Owner decision on H.264 licensing~~ — done: ADR 0009, OS/vendor codecs only.
2. S3 step 3 (`docs/STAGES.md`): `crates/media-video` — move the probe's capture → convert →
   encode pipeline into a library with a capture thread, a depth-1 latest-frame slot,
   keyframe request and runtime bitrate; add the decoder (MS H.264 MFT with D3D11 manager,
   verify DXVA is used) and a loopback test encode → decode on Windows (frame count, size,
   latency). Then step 4 (H.264 video track through the SFU).

## Previous next-task list — start S3

0. ~~Check known issue 26; commit `tools/screen-probe`~~ — done 2026-11-03 (issue 26 still open).
1. ~~Write the S3 plan~~ (in `docs/STAGES.md`). Was: write the S3 plan in `docs/STAGES.md` from spec §5.1/§5.2, §14, §17 S3 and ADR 0005
   (capture, system audio without our own sound, hardware codecs — all UNVERIFIED there):
   capture spike per OS (Windows: Windows.Graphics.Capture monitor item → D3D11 texture;
   macOS: ScreenCaptureKit; Linux: xdg-desktop-portal + PipeWire), hardware encoder probe
   and a 1080p60 encode spike (NVENC / Media Foundation on Windows, VideoToolbox on macOS,
   VA-API on Linux), system-audio capture excluding the messenger (Windows process
   loopback exclude, macOS `excludesCurrentProcessAudio`, Linux PipeWire routing).
2. Owner decision recorded in ADR 0005 before S3: H.264 licensing / dynamic linking of
   codecs. Raise it with the owner with options; do not pick silently.
3. First runnable piece: a `tools/` probe that lists monitors and hardware encoders and
   captures + encodes 10 s of the primary monitor at 1080p60 on Windows, reporting real
   FPS, encode latency and bitrate as JSON in `benchmarks/results/` (GUI/capture runs:
   secondary monitor only, launched through CreateProcess, never take focus).

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
