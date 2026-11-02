# Stages (from spec §17) and status

Status is maintained in `docs/PROJECT_STATE.md`; this file lists acceptance criteria
verbatim-in-meaning so each session can check them.

| Stage | Scope | Acceptance (short) | Status |
|---|---|---|---|
| S0 | Audit, skeleton, media feasibility | compiles on CI targets or blockers recorded; real native window; toolchain recorded; reproducible benchmark harness; PROJECT_STATE filled; ADRs for UI/codecs/GPU textures/str0m/capture/audio | DONE (2026-11-02, CI run 41364362270) |
| S1 | 1:1 voice end-to-end | two real clients talk via server; continuity under moderate loss; no audio loop; resources freed after leave; trials recorded | IN_PROGRESS |
| S2 | Group audio + SFU | ≥3 real clients + automated 20-participant scenario; join/leave/reconnect; new call closes previous; no per-viewer transcoding | NOT_STARTED |
| S3 | Screen share 1080p60 + system audio | real share with fps/latency/bitrate measured; audio w/o echo; stop/switch monitor; platform limits stated | NOT_STARTED |
| S4 | Adaptive quality, 1440p120, previews | manual+auto quality; no stale subscriptions; FPS/latency proven on RTX & M4, Linux separately; 50 publishers / 50 previews controlled load | NOT_STARTED |
| S5 | Accounts + direct messages | real message exchange; server-side block; correct after restart/reconnect; no duplicates; security tests | NOT_STARTED |
| S6 | Groups + calls in messenger | 100-member group integration test; start/join/leave call without breaking history/permissions | NOT_STARTED |
| S7 | Desktop polish + delivery | E2E on Win/macOS/Linux, signed updates, RAM/startup/size profiles | NOT_STARTED |
| S8 | Load, hardening, release | reports on target server: 500 WS, group 100, 50 publishers/previews, 1→50 viewers; backup/restore; release package | NOT_STARTED |

## S1 plan (in progress; step 1 done 2026-11-02 except APM; AudioEngine with device rebuild done)

1. `crates/media-audio`: cpal capture/playback on dedicated RT callbacks with lock-free
   SPSC rings; resampler to 48 kHz (device here is 192 kHz); Opus 48 kHz 20 ms frames;
   adaptive jitter buffer with PLC/FEC; mute; device selection + hot-unplug handling.
2. APM (AEC + NS default on, toggle) — integrated behind feature `apm` (vendored patched sys crate); CI job `apm` on 3 OS; default-on after green CI.
3. Desktop: signaling client (Tokio thread) using `/v1/signal`, call UI (join room by id
   for the S1 milestone since accounts are S5), device pickers, mute, connection state.
4. Transport: `UdpPeer` → driver per call (done: `CallDriver`, host candidates, loss
   detection/reconnecting state); server-reflexive (STUN) later.
5. Tests: two headless clients with file/synthetic audio over real UDP through the
   server signaling; netem-like impairment in-process (loss 1/3/5/10 %, jitter 20/50 ms);
   PESQ-free continuity metrics (concealed frames %, underruns); resource release check.
6. Manual: two machines (or two processes on the dev machine on the second monitor)
   talking; record results.
