# S7 step 5 (signed updates) — Windows, 2026-11-06

Branch: `codex/windows-environment` (local only). ADR 0016. Release binaries built with
the local dev/APM wrapper. No input injection, focus acquisition, screen capture or real
audio: every window runs on an isolated invisible desktop.

## What was built

* `crates/update`: Ed25519-signed manifest (envelope over the exact bytes), strict
  format/policy checks (product, platform, expiry, clock skew, newer-only, bad list,
  ≤ 32 files / 256 MiB, safe relative paths, no Windows device names), install layout
  with atomic `state.json`, staged HTTPS download with size limit and SHA-256, one-shot
  trial with confirmation and rollback. Crypto from `aws-lc-rs` (already used for TLS).
* `apps/launcher` (`turbo-pancake.exe`): starts the current version, trials a pending
  one (60 s for confirmation), rolls back on exit/timeout or a repeated start.
* `tools/update-tool` (`turbo-pancake-update`): `keygen`, `sign` (hashes a build
  directory, writes `manifest.json` + `files/<version>/`), `verify`.
* Server: `GET /v1/update/manifest`, `GET /v1/update/files/{version}/{*path}`
  (`--update-dir`, streamed, ≤ 8 concurrent downloads, no key on the server).
* Client: keys pinned at build time (`TURBO_PANCAKE_UPDATE_PUBKEYS`), check at start
  and every 6 h, settings-panel status and "Установить и перезапустить", green badge on
  ⚙, restart through the launcher, self-confirmation 4 s after start. A build without
  keys shows "Обновления не настроены в этой сборке" and installs nothing.

## Trial (`benchmarks/scripts/s7-update-trial.ps1`)

A fresh trial key; the client built as 0.1.0 and 0.1.1 with that key; install layout
with the launcher; real TLS server with `--update-dir`.

| Phase | Software | OpenGL (femtovg) |
|---|---|---|
| A 0.1.0 → 0.1.1: check, download + verify (24.7 MB), restart via launcher, trial, confirmation; state current=0.1.1 previous=0.1.0; both clients exit 0 | PASS (staged 0.78 s, confirmed 5.7 s after launch) | PASS (0.91 s, 5.8 s) |
| B broken 0.1.2 (an executable that exits with code 2): trial fails, marked bad, 0.1.1 starts again, 0.1.2 not offered again | PASS | PASS |
| C1 available, not accepted by the user: nothing installed (settings snapshot reviewed) | PASS | PASS |
| C2 truncated file / C3 one byte changed: "загруженный файл повреждён", state unchanged | PASS | PASS |
| C4 manifest edited after signing: "подпись обновления не прошла проверку" | PASS | PASS |
| C5 correctly signed older release (downgrade/replay): up to date, nothing installed | PASS | PASS |
| Foreground samples of trial processes | 0 | 0 |

The confirmation time is dominated by the deliberate 4 s "has been running" wait.

## Headless tests

`crates/update` 16 tests (signature/forgery/foreign key/unknown id, policy, file rules,
state machine, interrupted state write, staged download with truncated, oversized,
wrong Content-Length and tampered bodies against a local HTTP server);
`apps/app-server/tests/update.rs` (served release verified and staged; traversal,
backslash and bad versions → 404; no release → 404); desktop `updater` status tests.

## Found on the way

* Building the APM C++ code in a longer target directory (`t/update-trial-target`)
  failed in the linker: object paths exceeded Windows MAX_PATH (248 characters in the
  normal `t/release`). The keyed builds use `t/u`.

## Not covered / owner decisions

* The **release key** must be created by the owner (`turbo-pancake-update keygen`) and
  kept off the repository and server; official builds then set
  `TURBO_PANCAKE_UPDATE_PUBKEYS`. Until then release builds report updates as not
  configured.
* Authenticode signing of the launcher/client (owner's certificate) and updating the
  launcher itself: installer, step 6.
* Real network conditions (slow or interrupted downloads over the internet): the
  interruption cases are covered by the truncated-body tests only.
