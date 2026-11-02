# Architecture

Source of requirements: `TECHNICAL_SPEC_MESSENGER_RU.md` v1.0 (kept outside the repo by the
owner; sections referenced as §N). This file describes what exists **now** and where
the planned pieces will go.

## Workspace layout (actual)

```text
turbo-pancake/
├─ Cargo.toml                 workspace, shared deps, release profile
├─ rust-toolchain.toml        pinned toolchain (1.99.1)
├─ apps/
│  ├─ desktop/                Slint native client: window, voice room panel; lib: session (signaling + engine + call), view
│  └─ app-server/             Axum server (S0: health/version + signaling relay)
├─ crates/
│  ├─ protocol/               prost types generated from proto/ + validation + limits
│  ├─ media-transport/        str0m: UdpPeer + CallDriver (1:1 call thread, RTP-mode Opus, state machine)
│  └─ media-audio/            voice pipeline: cpal I/O + SPSC rings, rubato, Opus, jitter buffer, AudioEngine thread
├─ proto/turbo_pancake/v1/           protobuf schemas (source of truth for the wire format)
├─ benchmarks/
│  ├─ harness/                turbo-pancake-bench: startup/memory/signaling scenarios → JSON
│  └─ results/                committed reports
├─ tools/media-probe/         audio host/device enumeration + open test
├─ docs/                      this file, API, TESTING, STAGES, PROJECT_STATE, ADR/
└─ .github/workflows/ci.yml   fmt + clippy -D warnings + tests on Windows/Linux/macOS
```

Planned additions (created when the owning stage starts, not before, to avoid empty
crates): `apps/media-sfu` (S2), `crates/media-capture` and
`crates/media-codec` (S3), `crates/storage`, `crates/auth`, `crates/chat`, `migrations/`
(S5), `deploy/` contents (S5/S8).

## Runtime components

```text
 desktop (Slint UI thread)                     app-server (Tokio)
 ├─ UI event loop  ── never blocks ──┐         ├─ /healthz /readyz /v1/version
 ├─ tp-session (Tokio current-thread)  ◄─WS/protobuf─►├─ /v1/signal  (rooms, SDP/ICE relay)
 │    commands in (bounded), snapshots out (invoke_from_event_loop)
 ├─ tp-audio (AudioEngine) + cpal RT callbacks └─ (S5) auth, chat, PostgreSQL
 │    └─ tp-audio-pump ─▶ current call (bounded, drop when full)
 └─ tp-call (CallDriver, str0m) ◄──UDP/DTLS-SRTP──► peer (S1) / media-sfu (S2+)
      └─ tp-call-events ─▶ tp-session
```

S1 room model (`apps/desktop/src/session.rs`): the first member of a room waits and
answers, the member who joins later offers. Joining starts the audio engine, leaving
stops call and engine (devices, threads and socket released). A failed call is restarted
while the peer is still in the room; a lost signaling connection ends the call and is
retried with backoff (0.5 → 5 s), then the room is rejoined.

Concurrency rules (§3): UI thread only renders and dispatches; network on Tokio; audio
callbacks are real-time and lock-free (bounded SPSC rings); capture/encode/decode on
dedicated threads with bounded queues that drop the oldest frame. Every queue in the
server is bounded (see signaling outbound queue).

## Wire protocol

WebSocket + protobuf envelopes (`ClientEnvelope`/`ServerEnvelope`), versioned by package
(`turbo_pancake.v1`) and `protocol_version`. See [API.md](API.md) and
[ADR 0004](ADR/0004-signaling-protocol.md).

## Decisions

See `docs/ADR/`. Summary:

| ADR | Decision | Status |
|---|---|---|
| 0001 | Rust workspace, pinned toolchain, prost+protox (no system protoc) | accepted |
| 0002 | Slint (winit + FemtoVG/OpenGL, software fallback) as UI | provisional — license + GPU-texture checks pending |
| 0003 | str0m for WebRTC in client and SFU; aws-lc-rs crypto backend | provisional — backend benchmark in S1 |
| 0004 | Signaling over WebSocket + protobuf binary frames | accepted |
| 0005 | Media/OS API research plan (audio, capture, codecs, system-audio exclusion) | research, most items UNVERIFIED |
| 0006 | Voice pipeline: cpal + rtrb + rubato + libopus 1.6.1 (static), own jitter buffer | accepted for S1; APM open |
