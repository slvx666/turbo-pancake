# ADR 0009 — H.264: operating-system and vendor codecs only

* Status: **accepted** (2026-11-03, owner decision)
* Spec: §3 (no own codecs), §5.2 (H.264 mandatory, hardware first), §14.8 (codec licensing),
  §17 S3.

## Context

Screen share needs H.264 on every platform. Shipping an H.264 implementation ourselves
brings patent-pool and licence obligations (§14.8). The S3 probe (ADR 0005) showed that the
OS route works on Windows: Windows.Graphics.Capture → D3D11 → NVIDIA H.264 Encoder MFT
through Media Foundation, all on the GPU.

Options presented to the owner:

1. **OS / vendor codecs only** — Media Foundation (NVENC/AMF/QSV MFTs and the Microsoft
   decoder) on Windows, VideoToolbox on macOS, VA-API on Linux. We ship no codec code.
2. Option 1 plus the OpenH264 binary downloaded from Cisco (BSD, Cisco covers the patent
   pool) as a CPU fallback.
3. FFmpeg (LGPL, dynamic) — broad support, large DLLs (hurts the 30 MB installer target),
   LGPL obligations, unclear H.264 patent position.

## Decision

Option 1. The client uses only codecs provided by the operating system or the GPU vendor's
system API. No H.264 encoder or decoder is bundled.

## Consequences

* Where no suitable encoder is available (no hardware encoder, a Windows edition without
  Media Foundation, a Linux system without VA-API H.264), screen sharing is unavailable with
  a clear message (§2: "high modes may be unavailable with a clear notice"). Watching may
  still work when a decoder exists.
* A CPU encoder on Windows is still possible through the Microsoft H.264 encoder MFT (part
  of the OS), to be evaluated for low modes; it is not a bundled codec.
* Linux coverage depends on the distribution's VA-API drivers (and their H.264 support,
  which some distributions strip); this is a stated platform limitation, re-checked when a
  Linux trial is possible.
* Revisit only by owner decision (e.g. if OpenH264 becomes necessary for Linux coverage).
