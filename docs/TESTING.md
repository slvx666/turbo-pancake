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

Messenger tests need PostgreSQL binaries (`initdb`, `postgres`, `createdb`, any version ≥ 15):
set `TURBO_PANCAKE_PG_BIN` to their directory (a portable zip is enough, no service; on the dev
machine it is set in the untracked `.cargo/config.toml`). `TURBO_PANCAKE_SKIP_PG=1` skips them
with a message instead of failing.

| Suite | Location | What it proves |
|---|---|---|
| protocol unit | `crates/protocol/src/lib.rs` | room id / display name validation |
| protocol contract | `crates/protocol/tests/contract.rs` | pinned binary encoding, version check, oversize/garbage rejection, forward-compat unknown fields, signal validation |
| messenger integration + security (S5) | `apps/app-server/tests/messenger.rs`, `src/ratelimit.rs` (unit) | real server, HTTP + WS, real PostgreSQL: register/login/refresh/logout, same 401 for unknown user and wrong password, session list/revoke, password change revokes other sessions (their WS gets `SessionRevoked` and closes); lookup, direct conversation idempotent from both sides; send with Unicode/emoji, live event without the nonce, nonce retry returns the stored message (no event, no sequence), reply, edit/delete author-only (403), non-members 404, tombstones in history and change feed, receipts clamped and monotonic with `ReadUpdated`; blocks: both sides 403 with the same text, presence hidden and restored; 300 missed messages + edit + delete recovered page by page exactly once; 80 concurrent sends (each sent twice at once) → sequences 1..80 gap-free; presence over two devices and after a silent heartbeat timeout; missing/garbage/overlong token on every endpoint, wrong content type, malformed and 70 KiB bodies, bad ids/queries; 429 on register/login/messages; history, sessions and nonces survive a server restart, nobody online after it; 503 without a database |
| test PostgreSQL | `crates/pg-test` | one throw-away cluster per test process (`TURBO_PANCAKE_PG_BIN`, or `PGBIN` on CI), killed with the process via a job object, one database per test |
| signaling integration | `apps/app-server/tests/signaling.rs` | real TCP+WS: hello/welcome, join/leave, offer/answer/ICE relay, error codes, heartbeat eviction, room capacity, connection limit, oversize close, 20-peer join/relay/leave, room switch (`RoomLeft` + `PeerLeft{SWITCHED}`, full room keeps the current call), idempotent join/leave, gap-free `event_id`, health/ready/version |
| call driver | `crates/media-transport/tests/call.rs`, `src/call.rs` (unit) | real UDP: offer/answer relay, ICE+DTLS-SRTP, 150 frames each way at 20 ms across the u16 sequence wrap (payload/timestamp intact), 10 % wire loss loses exactly those packets, peer loss → Reconnecting → Failed; sequence extension across wrap/reorder |
| two headless clients | `crates/media-transport/tests/voice_call.rs` | two `AudioEngine`s (simulated devices) + two `CallDriver`s over UDP, 5 % loss: ≥ 300 frames played, FEC repairs, concealment < 3 %, ≤ 3 late, 0 decode errors, tone reaches the far speaker, silent mic stays silent; leaving releases devices, device threads and socket; the other side goes Reconnecting |
| desktop sessions + SFU | `apps/desktop/tests/session.rs`, `src/view.rs` (unit) | the client's own `Session` (signaling + engine + media, simulated devices) ×2 through an in-process `app-server` with its SFU: UDP call with 5 % loss (≥ 200 frames, concealment < 3 %, 0 decode errors), mute, leave releases devices/threads and the other member sees it in < 2 s, rejoin; **all UDP dropped on both clients (VPN without UDP) → automatic WebSocket tunnel, audio flows**; server without UDP → WebSocket at once; server outage → reconnect, rejoin, audio again; **3 members** each hear both others as separate streams, leave/rejoin; speaking indicators (own + per member), voice activation stops sending in silence and the next talkspurt plays, member volume 0 silences only that member, call volume scales all; joining another room ends the previous call for both sides; 3-member reconnect after a server outage; **20 members** (3 talkers, 17 listeners with voice activation) hear every talker through leave, room switch, rejoin and a server outage (`-- --nocapture` prints `TURBO_PANCAKE_TWENTY` timings) |
| STUN / srflx | `crates/media-transport/tests/stun.rs`, `src/stun.rs` (unit) | RFC 5769 XOR-MAPPED-ADDRESS vector, MAPPED-ADDRESS fallback, truncated/foreign responses rejected; fake STUN server on loopback → `typ srflx` with the mapped address in offer and answer, `reflexive_addr` in stats, call connects; silent server + unresolvable name → offer only delayed, call connects |
| WebRTC spike | `crates/media-transport/tests/loopback_spike.rs` | str0m ICE+DTLS+SRTP over real loopback UDP, SDP via protobuf, 100/100 RTP packets, data channel |
| audio unit | `crates/media-audio/src/{resample,codec,jitter,pipeline}.rs` | 16/44.1/48/96/192 kHz ↔ 48 kHz (tone + level preserved), Opus round trip level/bitrate, FEC beats PLC on lost frames, DTX, jitter buffer order/reorder/dup/late/FEC/PLC/wrap-around/adaptation/stall+burst, talkspurt after a pause (no late drops, delay unchanged, short pause keeps buffered frames); voice gate: hangover, contiguous seq + timestamp jump, mute silences the indicator; receiver speaking flag over two talkspurts |
| audio engine | `crates/media-audio/tests/engine.rs` (simulated devices, `sim.rs`) | start/stop and drop release every device stream and device thread; mic → Opus → loop back as a peer → jitter buffer → mix → speaker (peak kept), peer volume 0 and mute silence the output, muted frames still sent; input + output switch with different rates keeps seq/timestamp continuous; unplugged device → fallback to default; no speaker → wall-clock ticks and retry, recovery when it returns; unknown ids at start fall back; voice activation: own and peer speaking, nothing sent in silence, next talkspurt plays without late frames, gate off sends silence again |
| voice DSP (`--features apm`) | `crates/media-audio/src/dsp.rs`, `tests/engine.rs` | pass-through when off/unavailable; AEC3 removes a 30 ms −6 dB echo by > 25 dB; NS lowers the noise floor in pauses by > 6 dB keeping speech within 3 dB; engine audio loop: remote voice through a simulated speaker→mic room comes back > 25 dB down with AEC (≈ −7 dB without; that baseline test runs in every build). Windows: `powershell -File benchmarks/scripts/apm-env.ps1 cargo test -p turbo-pancake-media-audio --features apm` |
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
# S1 trial: server + two real desktop clients (second monitor, no focus, default devices),
# one TURBO_PANCAKE_CALL status line per second per client
powershell -File benchmarks/scripts/voice-trial.ps1 -Seconds 30 -Loss 0.05 -Out benchmarks/results/voice-trial-windows-<ts>.log
# WebRTC APM evaluation (stand-alone crate; Windows needs the steps in tools/apm-eval/README.md)
(cd tools/apm-eval && cargo run --release)
```

Windows note: libopus is built by CMake. Without an explicit `CMAKE_GENERATOR` the
`cmake` crate drops `/O2` (≈ 3.5× slower encoder). Set
`CMAKE_GENERATOR = "Visual Studio 17 2022"` under `[env]` in `.cargo/config.toml`
(CI derives it via `vswhere`) and `cargo clean -p opusic-sys` once.

## Screen share S4 (Windows, dev machine)

```bash
# layers, previews, adaptation through a real SFU (headless, synthetic H.264-shaped payloads)
cargo test -p turbo-pancake-media-sfu --test screen layers -- --nocapture
# 1440p120 / 1080p60 encode → decode and 50 preview decodes on one thread
cargo test -p turbo-pancake-media-video --release --test loopback -- --nocapture --test-threads=1 hardware fifty
# load: 50 publishers + 2 viewers, 1 → 50 viewers, 50 × 49 previews (release server process)
turbo-pancake-bench sfu-screen --server-exe <target>/release/turbo-pancake-app-server --scenario grid --n 50
# GUI trial: server + 3 clients on the 2nd monitor (A shares it, C a generated picture, B watches A)
powershell -File benchmarks/scripts/s4-trial.ps1 -Seconds 30 [-WatchLayer low] [-BRecvLoss 0.15] [-ALoss 0.15] [-Budget 1] [-ShareWindow [-Fullscreen]]
```

## Groups S6 (Windows, headless)

Run through the configured local dev/APM wrapper:
`cargo test -p turbo-pancake-app-server --test messenger --locked`.
S6 group tests require PostgreSQL explicitly; skipping it cannot pass these tests.

* `group_100_members_fanout_capacity_permissions_and_restart`: 100 real accounts and
  authenticated WebSockets receive a message; all 100 recover the same ordered history
  and acknowledge reads. Tests creator-only mutations, direct-block semantics in a
  group, removal, concurrent additions at capacity, durable snapshots after restart
  and re-add with the same history/receipts.
* `group_validation_authentication_and_atomic_creation`: invalid/duplicate/self/missing
  usernames, malformed titles, missing/bad tokens and rejected management of direct
  conversations; failed creation leaves no partial conversation.
* `group_removal_wins_against_a_message_waiting_for_the_row_lock`: holds the actual
  PostgreSQL conversation lock, observes a blocked HTTP send via `pg_stat_activity`,
  removes membership and releases the lock. The send must fail without advancing
  either counter. No sleep-based assumption about request scheduling.

These group-only tests do not by themselves certify native UI or account-bound calls;
that coverage is listed below. They use no real audio devices, capture, focus or input injection.

Client integration (`cargo test -p turbo-pancake-messenger-client --test sync --locked`):

* `group_services_sync_members_rights_removal_and_offline_rename`: three real Services
  with separate SQLite files and authenticated WebSockets. Create/add/rename, rejected
  non-creator rename, offline metadata/history recovery, open-chat removal and re-add
  with exactly the original messages and no stale draft.
* `group_offline_removal_purges_pending_and_stale_events_cannot_restore_access`:
  real HTTP/server/PostgreSQL plus a client engine. Missed removal invalidation,
  pending/draft/history/member purge, stale message event, re-add. Also remove + re-add
  entirely while offline: own membership revision must expose the gap and purge the
  old pending sends before flushing them. Existing history reloads exactly once.
* Cache unit `s5_cache_migrates_without_losing_history_drafts_or_pending` upgrades a
  populated S5 schema to S6 and checks retained data/cursors and idempotent reopen.

Authenticated call coverage:

* Server `conversation_calls_enforce_account_ownership_blocks_and_removal` and
  `calls_concurrent_devices_stale_leave_decline_busy_and_restart`: account/session
  ownership, three concurrent devices, stale leave/attach, anonymous namespace
  isolation, blocks, member removal, busy, decline, restart and unchanged messages.
* `call_http_authentication_invitation_timeout_and_revoked_reservation` uses the real
  supervisor (35-second upper bound) to prove unanswered invitation expiry; also
  verifies authentication on every call endpoint and revoked unattached participation.
* Desktop `authenticated_conversation_media_switch_reconnect_revoke_and_history`:
  real authenticated signaling/SFU and simulated bidirectional audio, second-device
  takeover, old-device resource release, same-lease reconnect, revoke, leave/restart
  and preserved chat history. `pg-test` is required, never skipped for S6.
* Client Service `call_service_invite_join_switch_leave_and_chat_continuity` checks
  invitation/accept, group switch, leave and the saved-draft snapshot after moving
  away from the chat. Group history sync also verifies removed authors' names in a
  fresh new member's cache (protobuf field 12, additive SQLite migration 3).

Native release acceptance, without disturbing the user's desktop:

```powershell
# Build via the configured dev/APM wrapper, then supply the local PostgreSQL bin dir.
powershell -NoProfile -File benchmarks/scripts/s6-trial.ps1 -PgBin <postgres-bin> -Out <local-log>
# Only after the software run passes:
powershell -NoProfile -File benchmarks/scripts/s6-trial.ps1 -PgBin <postgres-bin> -Backend winit-femtovg -Out <local-log>
```

The script creates its own temporary PostgreSQL cluster and private CA, starts the
TLS server and three native release clients on an isolated **invisible** Windows
desktop (never switches desktops), samples their native windows and foreground
ownership, and exercises normal UI callbacks without mouse/keyboard input. Audio is
simulated; no microphone, playback device or screen capture is opened. Group creation,
three-member audio, two messages, chat/call draft switches, leave, direct invitation/
accept/leave and explicit decline must pass. Every client must exit cleanly. Renderer
snapshots contain only the app's own pixels, not the user's desktop. Software and
OpenGL snapshots are visually checked separately. Profiles are unique and <=32 chars;
test credentials are removed at exit. Logs, snapshots, TLS keys and test DB stay in
the ignored build directory; publish only sanitized result summaries.

The script's options use the same native callbacks as the visible controls:
`--group-title`, `--group-members`, `--open-group`, `--call`, `--auto-join-call`,
`--auto-decline-call`, `--leave-call-after-ms`, `--trial-chat-switch`,
`--simulate-audio`, `--snapshot` (PPM), `--snapshot-after-ms`.
Real audio/by-ear and two-machine checks remain separate owner-assisted trials.

## Window lifecycle and tray S7 (Windows)

```powershell
# settings round trip / damaged file / atomic replace; tray click and menu routing
# through a hidden window (no icon is added)
cargo test -p turbo-pancake-desktop --lib
# real notification-area icon: add + remove, no window, focus or input (touches the
# user's taskbar for a moment, therefore ignored by default)
cargo test -p turbo-pancake-desktop --lib real_icon -- --ignored --nocapture
# release client on an isolated invisible desktop: close → hidden, restore through the
# tray window's message path, exit through its menu command; both renderers
powershell -NoProfile -File benchmarks/scripts/s7-tray-trial.ps1 -Out <local-log>
powershell -NoProfile -File benchmarks/scripts/s7-tray-trial.ps1 -Backend winit-femtovg -Out <local-log>
```

The trial samples the client's app windows (winit class `Window Class`; winit's own
message window `Winit Thread Event Target` is always `WS_VISIBLE` and is ignored) and
the foreground window every 100 ms. It passes only with the visibility trace
shown → hidden → shown, markers `closed visible=false` / `restored visible=true`, a
clean exit marker, exit code 0 and no foreground sample. The isolated desktop has no
shell, so there the icon is not shown (`icon=false`); the real icon is the ignored test.
Settings live in `%LOCALAPPDATA%/turbo-pancake/settings/<profile>.conf`.

One client per profile (step 2):

```powershell
# lock per profile, give-up wait, signal finds only its own profile's tray window
cargo test -p turbo-pancake-desktop --lib instance tray
# four release clients on one invisible desktop: A hides to the tray, B (same profile)
# hands over and exits without a window, A comes back; a scripted smoke client of the
# same profile and a guarded client of another profile both start normally
powershell -NoProfile -File benchmarks/scripts/s7-instance-trial.ps1 -Out <local-log>
powershell -NoProfile -File benchmarks/scripts/s7-instance-trial.ps1 -Backend winit-femtovg -Out <local-log>
```

Layout and scale (step 3):

```powershell
# ten own-window snapshots at several physical sizes and scale factors (SLINT_SCALE_FACTOR
# or the profile's ui_scale setting): size, factor, clean exit checked; PNGs for review
powershell -NoProfile -File benchmarks/scripts/s7-layout-trial.ps1 -Out <local-log>
powershell -NoProfile -File benchmarks/scripts/s7-layout-trial.ps1 -Backend winit-femtovg -Out <local-log>
```

Review the PNGs in the printed work directory by eye (clipping, overlap); they show
the local account name and stay in the ignored build directory. Snapshot-only flags:
`--window-size WxH`, `--open-settings`, `--sidebar-open`, `--call-tab 0|1`.

Connection banner and unsent input across a server outage (step 4):

```powershell
# real TLS server + PostgreSQL, three release clients on an invisible desktop: one with a
# chat open and unsent text, two in a direct call; the server is killed for -OutageMs
# (default 8000) and restarted on the same port and data. Checks the offline/restored
# banner of every client, the call link shown as reconnecting, input_kept=true, no
# history duplicates, no banner before the outage; own-window PNG per banner state
powershell -NoProfile -File benchmarks/scripts/s7-reconnect-trial.ps1 -PgBin <postgres-bin> -Out <local-log>
powershell -NoProfile -File benchmarks/scripts/s7-reconnect-trial.ps1 -PgBin <postgres-bin> -Backend winit-femtovg -Out <local-log>
```

Flags: `--snapshot-banner-dir <dir>` (one snapshot per banner kind), `--trial-input
<text>` (typed into the message and room fields once the scripted chat is open); with
`--chat-log` each banner change prints `TURBO_PANCAKE_BANNER kind=… messenger=… call=…`.
A server restart ends live calls (ADR 0015), so the call's "Звонок восстановлен" path is
covered by `banner::tests` and the same-lease reconnect in `apps/desktop/tests/session.rs`,
not by this trial.

Signed updates (step 5, ADR 0016):

```powershell
# generates a trial key, builds the client as 0.1.0 and 0.1.1 with that key pinned (own
# target dir t/u, short for MAX_PATH), lays out an install (launcher + versions/ +
# state.json) and runs on an invisible desktop against a TLS server with --update-dir:
# A update + restart + confirmation, B broken 0.1.2 rolled back and not re-offered,
# C available-but-unconfirmed / truncated file / tampered file / tampered manifest /
# signed downgrade change nothing. -CargoWrapper: the script that runs cargo with the
# APM environment (default benchmarks/scripts/apm-env.ps1)
powershell -NoProfile -File benchmarks/scripts/s7-update-trial.ps1 -Out <local-log>
powershell -NoProfile -File benchmarks/scripts/s7-update-trial.ps1 -Backend winit-femtovg -SkipBuild -Out <local-log>
```

Headless coverage: `crates/update` (signature, policy, file rules, install state machine,
interrupted state write, staged download against a local HTTP server with truncated,
oversized and tampered bodies) and `apps/app-server/tests/update.rs` (served release
verified and staged; traversal and missing releases → 404).

Installer (step 6, ADR 0017):

```powershell
# packs launcher + client as 0.1.0 and 0.1.1, builds one installer per package and runs
# fresh install (client started), shortcut start, upgrade, repair, older installer,
# repair while running (refused), uninstall (data kept) and uninstall --purge with
# LOCALAPPDATA/APPDATA/TEMP inside the work directory and a trial-suffixed HKCU key
powershell -NoProfile -File benchmarks/scripts/s7-install-trial.ps1 -Out <local-log>
powershell -NoProfile -File benchmarks/scripts/s7-install-trial.ps1 -Backend winit-femtovg -SkipBuild -Out <local-log>
```

Release installer by hand: `turbo-pancake-update pack --version <v> --input <dir with
turbo-pancake.exe and turbo-pancake-desktop.exe> --out <pkg>`, then build
`turbo-pancake-installer` with `TURBO_PANCAKE_SETUP_PAYLOAD=<pkg>`.

Profiles and Windows E2E (step 7):

```powershell
# idle RAM (working set, private, GPU dedicated/shared), idle CPU and startup
# (first frame, chat shown) of the signed-in client with the chat open, per renderer
powershell -NoProfile -File benchmarks/scripts/s7-profile-trial.ps1 -PgBin <postgres-bin> -Out <local-log> -Json <report.json>
# viewer cost per renderer while watching a generated 1440p120 / 1080p60 share
powershell -NoProfile -File benchmarks/scripts/s7-renderer-video-trial.ps1 -Out <local-log>
# install -> register/sign in -> chat -> call -> signed update -> rollback -> uninstall --purge
# (cargo needs the tool paths: pass the local launcher as -CargoWrapper on the dev machine)
powershell -NoProfile -File benchmarks/scripts/s7-e2e-trial.ps1 -PgBin <postgres-bin> -Out <local-log>
powershell -NoProfile -File benchmarks/scripts/s7-e2e-trial.ps1 -PgBin <postgres-bin> -Backend winit-femtovg -SkipBuild -Out <local-log>
```

The client uses the software renderer unless `SLINT_BACKEND` is set (ADR 0002).

Smoke/trial runs (`--smoke-exit-after-ms`) are not guarded unless `--single-instance`
is given, so the older multi-client trials keep working; `--multi-instance` disables
the guard for any run.

## Messenger S5 (Windows, dev machine)

```bash
# server + PostgreSQL: accounts, sessions, messages, blocks, sync, security negatives
cargo test -p turbo-pancake-app-server --test messenger
# two real client services (engine + SQLite cache + WebSocket) against it
cargo test -p turbo-pancake-messenger-client --test sync
# GUI trial: server + PostgreSQL + two release clients on the 2nd monitor, 4 phases
# (exchange, block, client restart from the stored session, server restart)
powershell -File benchmarks/scripts/s5-trial.ps1 -Out benchmarks/results/s5-trial-windows.log
# the same over HTTPS + WSS (gen-cert private CA, clients with --server-ca)
powershell -File benchmarks/scripts/s5-trial.ps1 -Tls
# TLS: private CA accepted; web PKI only, wrong CA, wrong address, plain HTTP refused
cargo test -p turbo-pancake-messenger-client --test tls
```

The desktop client signs in from the UI, or for trials with `--profile <p> --login <user>
[--register]` (password in `TURBO_PANCAKE_PASSWORD`), `--chat-with <user>`, `--send <text>`,
`--block`, `--chat-log`. Sessions are stored in the Windows Credential Manager
(`turbo-pancake/<profile>/<server>`), caches under `%LOCALAPPDATA%/turbo-pancake/cache/<profile>`.

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
   Discord-style window with empty DM list; the ☀/☾ button toggles light/dark theme.
   Since S7 closing the window hides it to the tray icon (setting under ⚙, default on);
   the icon's menu "Выйти" or "Выйти из приложения" ends the process.

## Not yet covered (tracked in PROJECT_STATE.md)

OS-level netem (in-process impairment exists for audio), real-device/two-machine
owner-assisted trials and macOS/Linux hardware (S9). PostgreSQL, auth/security and
native messenger/call scenarios are covered above; target-server load is S8.
