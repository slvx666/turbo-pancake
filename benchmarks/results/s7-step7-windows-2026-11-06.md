# S7 step 7 (profiles and Windows E2E acceptance) — Windows, 2026-11-06

Branch: `codex/windows-environment` (local only). Release binaries built with the local
dev/APM wrapper. No input injection, focus acquisition or screen capture: every client
window runs on an isolated invisible desktop; audio is simulated. Host: Windows 11
(build 26200) with the spec's Windows test GPU; full host line (CPU, driver) in the
local logs only.

## 1. Idle profile of the signed-in client (`s7-profile-trial.ps1`)

Real TLS server + PostgreSQL, two accounts with a two-message direct history. The
first account's release client starts 6× per renderer with its stored session and the
direct chat open (default window size, 100 % scale); after 10 s idle: memory of the
process, then CPU over 5 s. Raw samples: `s7-profile-windows-2026-11-06.json`.

| Metric (p50 of 6 runs) | Software renderer | OpenGL (femtovg) | Spec §13 target |
|---|---|---|---|
| Working set | **33.8 MiB** (33.7–44.1) | 72.3 MiB (72.2–72.5) | ≤ 50 MiB |
| Private bytes | **11.5 MiB** | 98.3 MiB | — (reported) |
| GPU memory (dedicated / shared, WDDM counters) | 0 / 0 MiB | 30.1 / 10.7 MiB | reported separately |
| Idle CPU | 0 % (≤ 0.6 %) | 0 % (≤ 0.6 %) | — |
| Spawn → first frame, first run after build | 128 ms | 275 ms | < 2 s |
| Spawn → first frame, warm p50 | 115 ms | 266 ms | < 2 s |
| Spawn → chat history shown (interactive), warm p50 | 116 ms | 267 ms | < 2 s |

* The client is one process (no child processes); the launcher exits after starting it.
* Two software runs showed 44.1 MiB working set (same private bytes): the OS trims or
  keeps shared pages differently between runs; the maximum is still below 50 MiB.
* "First run after build" is the closest automated approximation of a cold start; a
  true cold start (after reboot / flushed file cache) is **UNVERIFIED** (manual).
* The chat is shown from the local SQLite cache right after the first frame; the
  server sync follows.

## 2. Renderer cost while watching a share (`s7-renderer-video-trial.ps1`)

Anonymous loopback server; A shares a generated picture at a fixed quality, B watches the
high layer in a 1600×900 window; B measured over 10 s of the steady part.

| Quality | Renderer | Shown fps p50 | Decode p50 | Viewer CPU (% of one core) | WS / private |
|---|---|---|---|---|---|
| 2560×1440 @120 | software | 59.7 | 6.2 ms | **130 %** | **132 / 145 MiB** |
| 2560×1440 @120 | OpenGL | 60.1 | 4.9 ms | 183 % | 220 / 332 MiB |
| 1920×1080 @60 | software | 59.9 | 3.4 ms | **70 %** | **113 / 128 MiB** |
| 1920×1080 @60 | OpenGL | 59.9 | 2.7 ms | 120 % | 175 / 265 MiB |

Both renderers show frames at the refresh of the invisible desktop (≈ 60 Hz), so shown
fps above 60 is not measured here: **120 fps display UNVERIFIED** (needs a ≥ 120 Hz
monitor, owner-assisted). Decoded frames reach the UI as CPU images in both renderers
(no texture import yet, ADR 0002 item 2), which is why OpenGL costs more here.

## 3. Renderer decision (ADR 0002)

The software renderer is now the default (`apps/desktop/src/main.rs`): it is the only
one within the idle RAM target, starts faster, and costs less CPU and RAM while watching
a share. `SLINT_BACKEND=winit-femtovg` still selects OpenGL. Verified: background smoke
without `SLINT_BACKEND` prints `FIRST_FRAME … software`; with `winit-femtovg` it prints
`opengl`. Frame-time p95/p99 during UI animation and scrolling remain **UNVERIFIED**.

## 4. Windows E2E (`s7-e2e-trial.ps1`)

One chain with real release binaries, isolated user folders (LOCALAPPDATA, APPDATA,
TEMP), a trial id suffixing the HKCU key and prefixing profiles; client built as 0.1.0
and 0.1.1 with a fresh trial update key pinned; installer embeds 0.1.0.

| Phase | Software | OpenGL |
|---|---|---|
| 1 install 0.1.0 (installer 13.3 MiB, installed 25.1 MB; HKCU entry, Start-menu shortcut) | PASS (488 ms) | PASS (404 ms) |
| 2 register + sign in via the installed launcher (TLS/WSS, PostgreSQL) | PASS | PASS |
| 3 direct chat: sender shows "отправлено", failed=0; live receiver gets the same body | PASS | PASS |
| 4 direct call, media over TCP both ways (played ≈ 940–975 frames each), clean exits | PASS | PASS |
| 5 signed update 0.1.0 → 0.1.1 through the server; confirmed ≈ 5.3 s after launch; 0.1.1 signed in with the stored session and shows the same history | PASS | PASS |
| 6 broken 0.1.2: marked bad, 0.1.1 restarted with session and history, not re-offered | PASS | PASS |
| 7 uninstall `--purge`: install dir, shortcut, HKCU entry, profile data, 2 stored sessions removed | PASS | PASS |
| foreground samples | 0 | 0 |

After the runs the real profile has no trial credentials, uninstall entries or
shortcuts.

## Not covered here (owner-assisted / UNVERIFIED)

Keyboard tab/focus order; clicking the real tray icon/menu and the installer message
boxes; Authenticode signing and SmartScreen; the release update key; true cold start;
120 Hz display and UI frame-time percentiles; real network loss and sleep/wake; Slint
license choice (ADR 0002).
