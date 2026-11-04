# Stages (from spec §17) and status

Status is maintained in `docs/PROJECT_STATE.md`; this file lists acceptance criteria
verbatim-in-meaning so each session can check them.

**Scope rule (owner, 2026-11-04, ADR 0012): Windows only.** Every stage S1–S8 is planned,
built and accepted on Windows. Platform parts of the criteria (macOS/M4, Linux, three-OS
installers and E2E) belong to **S9** after S8. Items that need the owner's hardware or
presence are recorded as owner-assisted and do not block the next stage.

| Stage | Scope | Acceptance (short) | Status |
|---|---|---|---|
| S0 | Audit, skeleton, media feasibility | compiles on CI targets or blockers recorded; real native window; toolchain recorded; reproducible benchmark harness; PROJECT_STATE filled; ADRs for UI/codecs/GPU textures/str0m/capture/audio | DONE (2026-11-02, CI run 41364362270) |
| S1 | 1:1 voice end-to-end | two real clients talk via server; continuity under moderate loss; no audio loop; resources freed after leave; trials recorded | DONE (Windows, ADR 0012); owner-assisted: measured two-home log, by-ear AEC; macOS/Linux → S9 |
| S2 | Group audio + SFU | ≥3 real clients + automated 20-participant scenario; join/leave/reconnect; new call closes previous; no per-viewer transcoding | DONE (2026-11-03; CI 41647867149 Windows/Ubuntu; macOS re-check pending, PROJECT_STATE issue 26) |
| S3 | Screen share 1080p60 + system audio | real share with fps/latency/bitrate measured; audio w/o echo; stop/switch monitor; platform limits stated | DONE (Windows) 2026-11-04; owner-assisted: camera latency, two machines; macOS/Linux → S9 |
| S4 | Adaptive quality, 1440p120, previews | manual+auto quality; no stale subscriptions; FPS/latency proven on RTX & M4, Linux separately; 50 publishers / 50 previews controlled load | DONE (Windows) 2026-11-04; owner-assisted: RTX 4070 Super, 1440p120 capture, camera latency; macOS/M4, Linux → S9 |
| S5 | Accounts + direct messages | real message exchange; server-side block; correct after restart/reconnect; no duplicates; security tests | IN_PROGRESS (plan below, ADR 0013) |
| S6 | Groups + calls in messenger | 100-member group integration test; start/join/leave call without breaking history/permissions | NOT_STARTED |
| S7 | Desktop polish + delivery | E2E on Windows (macOS/Linux → S9), signed updates, RAM/startup/size profiles | NOT_STARTED |
| S8 | Load, hardening, release | reports on target server: 500 WS, group 100, 50 publishers/previews, 1→50 viewers; backup/restore; release package | NOT_STARTED |
| S9 | macOS + Linux port (ADR 0012) | ScreenCaptureKit/VideoToolbox/`excludesCurrentProcessAudio`; portal/PipeWire/VA-API + audio exclusion validation; devices, tray, installers, signing, updates; the macOS/Linux parts of S1–S8 criteria; CI jobs back | NOT_STARTED (after S8) |

## S5 plan (2026-11-04, Windows only, ADR 0013)

Scope (spec §7, §8, §10, §11, §12, §17 S5): accounts, sessions, direct conversations,
messages with replies/edit/delete, delivery/read receipts, presence, blocks, client cache
and sync. Groups (S6) reuse the same tables and feed.

1. ~~**Protocol**~~ (done 2026-11-04) — `proto/turbo_pancake/v1/api.proto` (users, auth, sessions, blocks,
   conversations, messages, receipts, errors), `Hello.auth_token` (field 3), events
   `MessageCreated/Edited/Deleted`, `ReadUpdated`, `PresenceChanged`,
   `ConversationUpdated`, `SessionRevoked`; validation (username, password, body); contract
   tests; `docs/API.md`.
2. ~~**Test PostgreSQL**~~ (done) — `crates/pg-test`: one throw-away cluster per test process
   (portable binaries: `TURBO_PANCAKE_PG_BIN`, or `PGBIN` on CI),
   killed with the process (Windows job object), one database per test.
3. ~~**Server**~~ (done) — migrations, Argon2id, tokens, rate limits, HTTP API, authenticated
   WebSocket, event fan-out, presence, blocks; integration + security tests (authn/authz
   negatives, blocked user, revocation, enumeration-equal answers, oversized payloads,
   idempotent send, ordering under concurrent senders, resync after missed events).
4. ~~**Client core**~~ (done, `crates/messenger-client`) — HTTP client, SQLite cache with migrations and corrupted-cache
   recovery, pending operations (optimistic send pending/sent/failed), sync on connect
   (change feed per conversation), dedup/gap handling of live events, receipts, token in
   the Credential Manager; headless tests against a real server.
5. ~~**Desktop UI**~~ (done 2026-11-04) — sign in / register, conversation list with presence and unread,
   find by username → direct chat, Discord-like message list (name, time, reply quote,
   edited mark, own message state, delivery/read), reply / edit / delete / copy, drafts,
   block/unblock, sessions and password change, connection state; voice room stays.
6. ~~**Acceptance**~~ (done, `s5-trial.ps1`) — two real desktop clients on this machine (`s5-trial.ps1`, second
   monitor): exchange, block, restart and reconnect without duplicates or gaps; restart
   of the server; results in PROJECT_STATE.
7. **TLS** — server TLS (rustls) with an IP certificate or pinned key, client verification
   (never disabled), documented trust setup.

## S4 plan (2026-11-04; Windows done, macOS/Linux blocked)

Scope (spec §5.1, §5.3, §13, §17 S4, ADR 0011): two layers per share, per-viewer layer
subscriptions, automatic + manual quality, previews with decode limits, separate window
and full screen, 1440p120 where supported, controlled load scenarios.

1. ~~Protocol~~ — `SubscribeScreen.layer`, `SubscribePreviews`, `ScreenSubscription`,
   `TRACK_KIND_SCREEN_PREVIEW`, bitrate/preview in `ScreenShare(State)`; contract tests.
2. ~~SFU~~ — preview m-line, preview forwarding per subscription, main stream carries
   either layer (switch at keyframe, continuous sequence numbers), `NetworkPolicy` from
   receiver reports, high-stream budget; `tests/screen.rs::layers_*`.
3. ~~Sender~~ — preview encoder, runtime reconfiguration, `adapt::Adapter` (bitrate →
   size → fps), pacing follows bitrate, presets fitted to the monitor, 1440p120 preset.
4. ~~Client~~ — preview decoder thread, visible page subscribed, watch layer, UI grid with
   size slider and pages, quality picker, separate window, full screen.
5. ~~Load~~ — `turbo-pancake-bench sfu-screen --scenario publishers|fanout|grid`.
6. ~~Trials~~ — `benchmarks/scripts/s4-trial.ps1` (auto/low/downlink loss/uplink loss/
   budget/window/full screen), 1440p120 encode/decode, 50 preview decodes.
7. Blocked: macOS (ScreenCaptureKit + VideoToolbox) and Linux (portal/PipeWire + VA-API)
   capture/codecs from S3; measurements on the spec's RTX 4070 Super / M4 machines and a
   1440p ≥ 120 Hz monitor need the owner.

## S3 plan (2026-11-03; Windows done 2026-11-04, macOS/Linux blocked)

Scope (spec §5.1, §5.2, §14.1–14.3, §17 S3): one sharer per member, whole selected monitor,
1080p60 H.264 with hardware encode/decode, system audio without our own sound, one viewer.
Windows first (dev machine); macOS/Linux pieces are written behind `cfg` and stay
UNVERIFIED until they run on real hardware (owner has none — same blocker shape as S1).

1. ~~**Capture + encode probe**~~ — done 2026-11-03 on Windows (PROJECT_STATE S3 table) (`tools/screen-probe`): list monitors and hardware encoders;
   capture a monitor with Windows.Graphics.Capture (D3D11 texture, no CPU copy), scale and
   convert BGRA → NV12 on the GPU (D3D11 video processor), encode H.264 with the hardware
   Media Foundation MFT (async, low-latency CBR); report captured / encoded FPS, capture →
   encoded latency p50/p95/p99, bitrate, keyframes as JSON in `benchmarks/results/`.
   No pixels are written anywhere. Runs on the secondary monitor only.
2. ~~**Owner decision (ADR 0009): H.264 licensing.**~~ — decided 2026-11-03: (a), OS/vendor codecs only. Options: (a) OS/vendor encoders only
   (Media Foundation, VideoToolbox, VA-API) — no codec code shipped by us; (b) bundle
   OpenH264 (Cisco binary, BSD, Cisco pays MPEG LA when downloaded from Cisco) as a CPU
   fallback; (c) FFmpeg LGPL dynamic. Until decided the code uses (a) only.
3. ~~`crates/media-video`~~ — done 2026-11-04 (capture, converter, `ScreenShare` thread, decoder): capture thread → GPU convert → encoder thread with a
   depth-1 "latest frame wins" slot (no queues that grow), keyframe on request, bitrate
   change at runtime; decoder (MF H.264 hardware MFT, D3D11 output) → NV12 → BGRA texture.
4. ~~Transport~~ — done 2026-11-04 (ADR 0010): a video m-line (H.264, packetization-mode 1) on the existing str0m
   session to the SFU; SFU forwards the sharer's RTP to subscribed viewers only, relays
   PLI/FIR as keyframe requests; protocol: `ScreenSharePublished/Unpublished`,
   `SubscribeScreen{peer}`; contract tests.
5. ~~Viewer~~ — done 2026-11-04: the desktop shows the selected share in the call window (Slint image from a
   CPU copy first; GPU texture import is S4 work), stop/switch monitor from the share menu.
6. ~~System audio~~ — done on Windows 2026-11-04 (tone test): WASAPI process loopback excluding our process tree (Windows 10 2004+
   / 11), Opus stereo 48 kHz as a second audio track without NS/AEC (spec §4); on macOS
   ScreenCaptureKit `excludesCurrentProcessAudio`, Linux disabled with a stated limitation
   until the PipeWire routing is validated.
7. ~~Acceptance runs~~ — done on Windows 2026-11-04 (`screen-trial.ps1`): real share between two clients on this machine (sharer captures
   the secondary monitor) with frame counts, end-to-end latency via a timestamp marker
   drawn in a test pattern window, bitrate; system audio with the client's own playback
   playing and verified absent from the captured stream; stop / monitor switch tests.

## S2 plan (done 2026-11-03)

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
   voice activation; leave, switch, rejoin, server outage); three real clients on real
   devices (`benchmarks/scripts/group-trial.ps1`, one machine) done 2026-11-03.
6. ~~Server never transcodes; measure SFU CPU/bandwidth for 20 members~~ — done
   2026-11-03 (`turbo-pancake-bench sfu`): 20 members all talking = 18.7 % of one core;
   speaker cap deferred (ADR 0008).

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
