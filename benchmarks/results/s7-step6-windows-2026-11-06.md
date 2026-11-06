# S7 step 6 (per-user installer) — Windows, 2026-11-06

Branch: `codex/windows-environment` (local only). ADR 0017. Release binaries built with
the local dev/APM wrapper. No input injection, focus acquisition or screen capture:
every client window runs on an isolated invisible desktop.

## What was built

* `turbo-pancake-update pack`: the `TPPK` package (JSON header with SHA-256 per file +
  raw deflate level 10 via `miniz_oxide`, already in the dependency graph).
* `apps/installer` → `turbo-pancake-setup.exe`: package embedded at compile time
  (`TURBO_PANCAKE_SETUP_PAYLOAD`, inside the PE image, so Authenticode covers it);
  installs into `%LOCALAPPDATA%\Programs\turbo-pancake` without admin rights, creates the
  Start-menu shortcut and the HKCU uninstall entry, starts the client. Fresh / upgrade /
  repair / older-package (keeps the newer version) rules; refuses when the version's
  files are in use.
* `crates/setup`: layout (only own entries are written or deleted; a directory without
  `state.json` + launcher is never deleted), Windows integration (registry, `IShellLinkW`,
  message boxes, Credential Manager purge).
* Launcher `--uninstall`: runs from a temporary copy, removes files, shortcut and entry;
  profile data (cache, settings) and stored sessions only on "yes"/`--purge`; the copy
  deletes itself afterwards.

## Trial (`benchmarks/scripts/s7-install-trial.ps1`)

`LOCALAPPDATA`, `APPDATA`, `TEMP` pointed into the work directory (the default install
paths, isolated) and a trial id suffixing the HKCU key; the client and launcher are
packed as 0.1.0 and 0.1.1.

| Phase | Software | OpenGL (femtovg) |
|---|---|---|
| 1 Fresh install (0.44–0.50 s), installer starts the client (native window), HKCU entry, shortcut to the launcher, `state.json` current=0.1.0 | PASS | PASS |
| 2 Shortcut target starts the client (first frame, clean exit) | PASS | PASS |
| 3 Upgrade 0.1.0 → 0.1.1 (previous=0.1.0, entry version 0.1.1); repair of a deleted client; older installer keeps 0.1.1 | PASS | PASS |
| 4 Repair while the client runs: refused with exit 1, install intact | PASS | PASS |
| 5 Uninstall: install directory, shortcut, entry removed; profile data kept; temporary uninstaller gone | PASS | PASS |
| 6 Reinstall + uninstall `--purge`: profile data removed | PASS | PASS |
| Foreground samples | 0 | 0 |

Afterwards no `turbo-pancake*` uninstall key, Start-menu entry or
`%LOCALAPPDATA%\Programs\turbo-pancake` existed on the real profile.

## Sizes (spec §13: package ≤ 30 MB)

| Item | Bytes |
|---|---|
| Client + launcher, uncompressed | 25 075 200 |
| **Installer `turbo-pancake-setup.exe`** | **13 900 288 (13.3 MiB) — PASS** |
| Installed (one version) | 25 075 276 |

Deflate level 10; the ADR names LZMA/zstd as the next step if the package grows.
Authenticode is not applied (owner's certificate) and will add a few KB.

## Found on the way

* Building the installer alone failed in `aws-lc-sys` (NASM required): rustls enables
  `prebuilt-nasm` only when it is part of the build. The workspace dependency now enables
  `prebuilt-nasm` itself; `Cargo.lock` is unchanged by that.

## Not covered / owner-assisted

* The interactive message boxes (uninstall confirmation, errors) were not clicked: no
  input injection; the trial uses `--quiet`. Owner check: install and uninstall once
  through "Apps & features".
* Authenticode signing; SmartScreen behaviour of an unsigned installer.
