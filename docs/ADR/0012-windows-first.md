# ADR 0012 — Windows first: other systems after the full Windows product

* Status: **accepted** (owner decision, 2026-11-04)
* Spec: §2 (target platforms), §16, §17 (stage acceptance), §0.7 (requirement changes need
  an ADR). Changes how stages S1–S8 are accepted; does not drop macOS/Linux from the product.

## Context

S1, S3 and S4 were complete and measured on Windows but stayed BLOCKED because their
acceptance includes real-device trials on macOS (M4) and Linux, which need capture/codec
code for those systems and hardware the owner does not have. Every later stage (S5–S8)
would block the same way.

## Decision

1. **Development and acceptance target Windows only** until the whole product (S0–S8) is
   done on Windows. macOS and Linux work is not started and not planned per stage.
2. A stage is **DONE (Windows)** when every criterion passes on Windows. Platform parts of a
   criterion ("on macOS/M4", "Linux separately", "installers for three OSes", "E2E on
   Windows/macOS/Linux") move to the port stage below, not to the stage itself.
3. Items that need the owner's hardware or presence on Windows (spec test machine RTX 4070
   Super, a 1440p ≥ 120 Hz monitor, two homes, a camera for screen-to-screen latency, by-ear
   checks) are recorded as **owner-assisted, pending** in PROJECT_STATE; they do not block
   the next stage. A stage whose Windows criterion cannot be shown at all without them stays
   open for that criterion.
4. **New stage S9 — macOS and Linux port** (after S8): ScreenCaptureKit + VideoToolbox +
   `excludesCurrentProcessAudio` (macOS), xdg-desktop-portal + PipeWire + VA-API and system
   audio exclusion validation (Linux), audio devices, tray, installers, signing, updates;
   acceptance = the platform parts moved out of S1–S8 by point 2.
5. **CI runs Windows only** (plus the dependency audit). Code keeps compiling on other
   systems where it already does (cfg stubs stay), but this is not checked per commit until
   S9, which brings the Linux/macOS jobs back.

## Consequences

* S1, S3 and S4 become DONE (Windows) with their owner-assisted items listed; S5 starts.
* Platform-neutral code (protocol, server, SFU, session) still must not use Windows-only
  APIs outside `cfg(windows)` modules, so S9 does not need a rewrite.
* The server is a Linux machine in the spec (§2): the app-server and SFU are platform-neutral
  Rust and are tested on Windows; their deployment on Linux (systemd/Docker, spec §12) is S8
  work and is verified there, because it is the server, not a client platform.
