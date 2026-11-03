# Stages (from spec §17) and status

Status is maintained in `docs/PROJECT_STATE.md`; this file lists acceptance criteria
verbatim-in-meaning so each session can check them.

| Stage | Scope | Acceptance (short) | Status |
|---|---|---|---|
| S0 | Audit, skeleton, media feasibility | compiles on CI targets or blockers recorded; real native window; toolchain recorded; reproducible benchmark harness; PROJECT_STATE filled; ADRs for UI/codecs/GPU textures/str0m/capture/audio | DONE (2026-11-02, CI run 41364362270) |
| S1 | 1:1 voice end-to-end | two real clients talk via server; continuity under moderate loss; no audio loop; resources freed after leave; trials recorded | BLOCKED (2026-11-02): Windows done, real-device trials on macOS/Linux need owner hardware (none available) |
| S2 | Group audio + SFU | ≥3 real clients + automated 20-participant scenario; join/leave/reconnect; new call closes previous; no per-viewer transcoding | IN_PROGRESS (started 2026-11-02 under the S1 platform blocker, spec §16) |
| S3 | Screen share 1080p60 + system audio | real share with fps/latency/bitrate measured; audio w/o echo; stop/switch monitor; platform limits stated | NOT_STARTED |
| S4 | Adaptive quality, 1440p120, previews | manual+auto quality; no stale subscriptions; FPS/latency proven on RTX & M4, Linux separately; 50 publishers / 50 previews controlled load | NOT_STARTED |
| S5 | Accounts + direct messages | real message exchange; server-side block; correct after restart/reconnect; no duplicates; security tests | NOT_STARTED |
| S6 | Groups + calls in messenger | 100-member group integration test; start/join/leave call without breaking history/permissions | NOT_STARTED |
| S7 | Desktop polish + delivery | E2E on Win/macOS/Linux, signed updates, RAM/startup/size profiles | NOT_STARTED |
| S8 | Load, hardening, release | reports on target server: 500 WS, group 100, 50 publishers/previews, 1→50 viewers; backup/restore; release package | NOT_STARTED |

## S2 plan (in progress since 2026-11-02)

Base already in place from S1 (ADR 0007): every member has one media session with the
server's SFU (UDP or WebSocket tunnel), RTP is forwarded without transcoding, the engine
mixes up to 100 peers with per-peer volume and jitter buffers.

1. ~~**Per-member audio in the SFU**~~ — done 2026-11-02: option (c), ADR 0008; 3-member
   headless test. Was: today a member hears only one other member. Options to decide by a small spike: (a) server-initiated renegotiation adding one
   recvonly m-line per member; (b) a fixed number of receive slots (last-N loudest speakers)
   with a slot → member mapping event; (c) one m-line with one SSRC per member (str0m RTP
   mode). Criteria: 100 members, join/leave without glitches, CPU on the server.
2. ~~Protocol~~ — done: `TrackPublished`/`TrackUnpublished` (fields 20/21, contract test).
   Was: speaker/slot mapping (`media.track_published/unpublished`, spec §11) and,
   for (a), server offers; contract tests.
3. ~~Client~~ — done 2026-11-03: member list with a speaking indicator and a volume
   slider per member, voice activation with an adjustable threshold (sequence number
   pauses, RTP timestamp runs; jitter buffer starts a talkspurt), own indicator.
4. ~~Call lifecycle~~ — done 2026-11-03: joining another room closes the previous call
   (`PeerLeft{SWITCHED}` to its members, `RoomLeft` to the joiner), idempotent join/leave,
   group reconnect test. Per user across devices waits for accounts (S5).
5. ~~Tests~~ — automated 20-member scenario done 2026-11-03 (3 talkers + 17 listeners with
   voice activation; leave, switch, rejoin, server outage). Still open: 3 real clients by
   hand.
6. Server never transcodes; measure SFU CPU/bandwidth for 20 members.

## S1 plan (Windows done; macOS/Linux trials blocked on hardware; step 1 done 2026-11-02 except APM; AudioEngine with device rebuild done)

1. `crates/media-audio`: cpal capture/playback on dedicated RT callbacks with lock-free
   SPSC rings; resampler to 48 kHz (device here is 192 kHz); Opus 48 kHz 20 ms frames;
   adaptive jitter buffer with PLC/FEC; mute; device selection + hot-unplug handling.
2. APM (AEC + NS default on, toggle) — integrated behind feature `apm` (vendored patched sys crate); CI job `apm` on 3 OS; default-on after green CI.
3. (done 2026-11-02) Desktop: signaling client (Tokio thread) using `/v1/signal`, call UI (join room by id
   for the S1 milestone since accounts are S5), device pickers, mute, connection state.
4. Transport: `UdpPeer` → driver per call (done: `CallDriver`, host candidates, loss
   detection/reconnecting state); server-reflexive (STUN) later.
5. Tests: two headless clients with file/synthetic audio over real UDP through the
   server signaling; netem-like impairment in-process (loss 1/3/5/10 %, jitter 20/50 ms);
   PESQ-free continuity metrics (concealed frames %, underruns); resource release check.
6. Manual: two machines (or two processes on the dev machine on the second monitor)
   talking; record results.
