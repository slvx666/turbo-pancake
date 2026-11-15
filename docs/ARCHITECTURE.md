# Architecture

Source of requirements: `TECHNICAL_SPEC_MESSENGER_RU.md` v1.0 (kept outside the repo by the
owner; sections referenced as §N). This file describes what exists **now** (Windows, S0–S8
done; macOS and Linux are stage S9, ADR 0012). Status and evidence: `PROJECT_STATE.md`.

## Workspace layout

```text
turbo-pancake/
├─ Cargo.toml                 workspace, shared deps, lints (unsafe denied), profiles
├─ rust-toolchain.toml        pinned toolchain
├─ apps/
│  ├─ desktop/                Slint client: windows, tray, settings, updater glue;
│  │                          lib: session (signaling + audio + call + share), view, banner
│  ├─ app-server/             Axum server: accounts, chats, groups, calls, signaling,
│  │                          updates, limits; runs the SFU as one thread
│  ├─ launcher/               `turbo-pancake.exe`: starts the installed version, rollback
│  └─ installer/              per-user installer with the package embedded (ADR 0017)
├─ crates/
│  ├─ protocol/               prost types from proto/ + validation + limits + contract tests
│  ├─ media-audio/            cpal I/O, SPSC rings, resampling, Opus, jitter buffer, WebRTC
│  │                          APM, AudioEngine thread, system-audio capture, rt scheduling
│  ├─ media-transport/        str0m: UdpPeer + CallDriver (call thread, RTP mode, pacing)
│  ├─ media-video/            Windows capture → GPU convert → H.264 MFT; decoder; adaptation
│  ├─ media-sfu/              SFU: ICE-lite session per member, RTP forwarding, layers
│  ├─ messenger-client/       HTTP API client, SQLite cache, sync engine, service thread
│  ├─ tls/                    rustls configuration, private CA (ADR 0014)
│  ├─ update/                 signed manifest, staged install, rollback (ADR 0016)
│  ├─ setup/                  install layout, shortcuts, uninstall entry
│  ├─ win-icon/               `.ico` → `.res` in build scripts
│  └─ pg-test/                throw-away PostgreSQL cluster for tests
├─ proto/turbo_pancake/v1/    protobuf schemas (source of truth for the wire format)
├─ migrations/                PostgreSQL schema (SQLx)
├─ benchmarks/                harness (load and media scenarios), scripts, results
├─ deploy/                    release script, Linux install / backup / restore / bench
├─ tools/                     media-probe, screen-probe, update-tool, apm-eval
├─ third_party/               patched `webrtc-audio-processing-sys` (MSVC build)
└─ docs/                      this file, API, TESTING, STAGES, ADMIN, BACKUP, ADR/
```

Differences from the spec's example layout (§15): the SFU is a library, run either as a
thread of `app-server` or — with `--media-process`, the deployed default — as a worker
process of the same binary that the server supervises and restarts (ADR 0020), not a
separate `apps/media-sfu` program; `domain` / `auth` / `chat`
/ `storage` are modules of `app-server` and `messenger-client`, not crates of their own.

## Runtime components

```text
 desktop process                                   app-server process (Tokio)
 ├─ UI thread (Slint, software renderer)           ├─ HTTPS /v1/… (accounts, chats, calls)
 │    callbacks → bounded command channels         ├─ WSS /v1/signal (events + signaling)
 │    snapshots ← invoke_from_event_loop           ├─ PostgreSQL pool (SQLx)
 ├─ tp-session (Tokio current-thread): signaling,  ├─ bounded Argon2 hashing
 │    call control, share control, adaptation      └─ tp-sfu (one thread, all rooms):
 ├─ messenger thread: HTTP + SQLite cache               str0m session per member,
 ├─ tp-audio (AudioEngine)  ◄─ cpal RT callbacks        RTP forward, layer choice
 │    └─ tp-audio-pump ─▶ current call
 ├─ tp-call (CallDriver, str0m) ◄─ UDP, or the WebSocket tunnel, DTLS-SRTP ─► tp-sfu
 ├─ tp-screen-share (capture → convert → encode), tp-system-audio
 └─ viewer / preview decoder threads ─▶ newest picture ─▶ UI timer
```

Concurrency rules (§3): the UI thread only renders and dispatches; network on Tokio; audio
callbacks are real-time and lock-free (bounded SPSC rings); capture, encode and decode run
on dedicated threads with bounded queues that drop the oldest item. Every queue in the
server is bounded. The threads that carry voice (`tp-audio`, `tp-audio-pump`, `tp-call`,
`tp-system-audio`) register for audio-class scheduling (`media_audio::rt`, MMCSS "Pro
Audio" on Windows) so a game that saturates the CPU does not starve them.

## Voice path

```text
mic → ring → resample 48 kHz → APM (AEC3, NS) → mute/gate → Opus (48 kbit/s, in-band FEC)
    → RTP → SFU (forward, one SSRC per member) →
    jitter buffer (adaptive target, FEC / PLC, delay correction) → decode → volume
    → mix → limiter → AEC reference → resample → ring → speaker
```

* The encoder's loss resilience follows the measured path loss (`LossAdapter`: expected
  loss 10–30 %, more bits for the FEC copy), evaluated once a second in the session.
* The jitter buffer removes excess delay one frame per tick: a hole is skipped, otherwise
  two consecutive frames play in one tick, blended (no cut in the waveform).
* The mix of several voices goes through a limiter instead of being clipped.
* Audio is not retransmitted (video is, by NACK/RTX); there is no TURN: without UDP the
  media goes through the signaling WebSocket.

## Screen share path

```text
Windows.Graphics.Capture → D3D11 video processor (scale, NV12) → H.264 MFT (hardware, low
latency, CBR) → pacer → RTP → SFU → (high layer to the viewer of the share, low layer as
previews) → frame assembler (NACK repairs, keyframe requests) → H.264 decoder (DXVA) →
NV12 → RGBA on the CPU → newest picture → Slint image (main window)
      or → D3D11 video processor → swap chain of the share window (ADR 0019)
```

Two layers per share (ADR 0011); the high layer adapts bitrate, then size, then rate. A
share whose pipeline stops on its own (resolution change, device lost) is restarted up to
three times a minute. System audio: WASAPI process loopback excluding our own process
tree, Opus stereo 96 kbit/s on its own stream.

In the share window every decoded picture is drawn from the GPU; the view embedded in
the main window keeps the CPU path (≈ 60 pictures a second at most). System audio plays
in stereo on stereo devices (mid/side rings next to the mono voice path).

## Windows

The client creates its windows opaque and keeps them so (`apps/desktop/src/opaque.rs`):
Slint's winit backend otherwise asks for a transparent window, which lets the desktop
show through whenever the surface is not fully painted. After a show, a focus change, a
display change or a resume from sleep the window repaints in full (the software renderer
otherwise repaints only what changed).

## Wire protocol

WebSocket + protobuf envelopes (`ClientEnvelope` / `ServerEnvelope`), versioned by package
(`turbo_pancake.v1`) and `protocol_version`; HTTPS + protobuf for requests. See
[API.md](API.md) and [ADR 0004](ADR/0004-signaling-protocol.md).

## Decisions

| ADR | Decision |
|---|---|
| 0001 | Rust workspace, pinned toolchain, prost + protox (no system protoc) |
| 0002 | Slint (winit); software renderer by default, FemtoVG/OpenGL selectable; license open |
| 0003 | str0m for WebRTC in client and SFU; aws-lc-rs crypto backend |
| 0004 | Signaling over WebSocket + protobuf binary frames |
| 0005 | Media / OS API research (audio, capture, codecs, system-audio exclusion) |
| 0006 | Voice pipeline: cpal + rtrb + rubato + libopus (static), own jitter buffer, APM |
| 0007 | Media through the server SFU, UDP + WebSocket tunnel fallback |
| 0008 | Per-member audio: one m-line, one SSRC per member |
| 0009 | H.264 through operating-system and vendor codecs only |
| 0010 | Screen share transport, subscriptions, loss repair, system audio |
| 0011 | Two layers per share, layer subscriptions, previews, adaptation |
| 0012 | Windows first; macOS and Linux after the full Windows product (S9) |
| 0013 | Accounts, sessions, direct messages and sync |
| 0014 | TLS for an IP-only server: a private CA trusted by the clients |
| 0015 | Conversation calls and account ownership |
| 0016 | Signed updates with staged install and rollback |
| 0017 | Per-user installer |
| 0018 | Opaque windows, audio-class scheduling, adaptive voice resilience |
| 0019 | The share window shows pictures from the GPU (own swap chain, no CPU copy) |
| 0020 | The SFU can run as a supervised worker process of the server |
