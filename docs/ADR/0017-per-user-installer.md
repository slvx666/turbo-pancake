# ADR 0017 — Per-user installer (Windows)

Status: accepted (2026-11-06, S7 step 6). Spec §9 (installers, signed updates), §13
(package ≤ 30 MB), §1 (no autostart). Builds on ADR 0016 (install layout).

## Context

The client needs an installer that works without administrator rights, creates the
ADR 0016 layout the updater relies on, appears in "Apps & features" with a working
uninstall, and fits the 30 MB package target. External installer tools (WiX, Inno
Setup, NSIS) would add a non-Rust toolchain to the release build and their own
runtime/size; the layout and the upgrade rules already live in `crates/update`.

## Decision

**Own Rust installer** `turbo-pancake-setup.exe` (`apps/installer`), GUI subsystem, no
UI toolkit: it installs without questions, reports errors in a message box and starts
the client (unless `--no-launch`). `--quiet` suppresses message boxes (scripted
installs, trials).

**Payload embedded at compile time.** `turbo-pancake-update pack` writes a package
(`TPPK` format: JSON header with version, path, size, SHA-256 per file, then one raw
deflate stream per file); the installer's build script embeds it from
`TURBO_PANCAKE_SETUP_PAYLOAD` (`include_bytes!`), so the package is part of the PE image
and an Authenticode signature added afterwards covers it with no overlay tricks. A
build without the variable embeds an empty package and refuses to install. Every file
is checked against its SHA-256 while extracting. Compression: `miniz_oxide` (deflate,
level 10), already in the dependency graph through image decoding — no new crate.

**Layout and system integration** (all per user, HKCU / user profile):

* `%LOCALAPPDATA%\Programs\turbo-pancake\` — `turbo-pancake.exe` (launcher),
  `versions/<v>/turbo-pancake-desktop.exe`, `state.json` (ADR 0016).
* Start-menu shortcut `%APPDATA%\Microsoft\Windows\Start Menu\Programs\turbo-pancake.lnk`
  to the launcher (COM `IShellLinkW`).
* Uninstall entry `HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\turbo-pancake`
  (DisplayName/Version, Publisher, InstallLocation, DisplayIcon, EstimatedSize,
  NoModify/NoRepair, `UninstallString` = `"<root>\turbo-pancake.exe" --uninstall`).
* No autostart entry (out of scope, spec §1).

**Upgrade rules.** A fresh install writes `state.json` with the package version. Over an
existing install: a newer package becomes `current` (the old one `previous`; pending is
cleared and the version removed from the bad list — the user chose it explicitly); the
same version is repaired (files rewritten); an older package keeps the newer installed
version and only refreshes the launcher and the system entries. The running launcher is
replaced by renaming it aside first (Windows allows renaming a running executable).
Files of the version being written must not be in use: the installer reports "закройте
turbo-pancake" instead of leaving a half-written version.

**Uninstall** is a launcher mode: `turbo-pancake.exe --uninstall` copies itself to the
temporary directory and runs the copy, which removes the shortcut, the registry entry
and the install directory, then deletes itself through a delayed `cmd /c del`. Profile
data (`%LOCALAPPDATA%\turbo-pancake`: cache, settings) and stored sessions in Credential
Manager (`turbo-pancake/*`) are kept unless the user answers "yes" to removing them, or
`--purge` is given.

**Isolation for tests.** Hidden options `--install-dir` and `--trial-id <id>` place the
install elsewhere and suffix the registry key and shortcut name, so trials never touch
a real installation; the trial removes everything it created.

## Consequences

* Release flow: build client + launcher → `turbo-pancake-update pack` → build the
  installer with `TURBO_PANCAKE_SETUP_PAYLOAD` → Authenticode (owner) → publish; updates
  then go through ADR 0016.
* The launcher is updated by running a newer installer (not by the in-app updater).
* Deflate is weaker than LZMA; if the package approaches 30 MB, an LZMA/zstd crate is the
  next step (ADR update with licence check).
