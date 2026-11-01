# ADR 0005 — Media and OS API research (S0 findings and plan)

* Status: research (S0, 2026-11-02). Items marked UNVERIFIED have no runnable proof yet.
* Spec: §4, §5, §14

## Audio I/O

* **cpal 0.18** (WASAPI / CoreAudio / ALSA+PulseAudio/PipeWire-ALSA). Verified on the dev
  machine with `tools/media-probe` (`benchmarks/results/media-probe-windows-*.json`):
  3 inputs / 3 outputs enumerated with correct Unicode names; default input and output
  opened and driven for 1 s (≈ 96–100 callbacks/s).
* Finding: the default microphone here runs at **192 kHz mono** (shared mode), output at
  192 kHz. The audio pipeline must resample to 48 kHz for Opus (S1), never assume 48 kHz.
* Finding: WASAPI reported buffer under/overrun on stream start for the output stream
  (transient). S1 must measure glitches in steady state.
* macOS/Linux: UNVERIFIED.

## Opus

Candidates for S1: `opus` crate (libopus via `audiopus_sys`, needs cmake — available on
this machine through Visual Studio) or a static libopus build of our own; pure-Rust ports
must be evaluated for quality/CPU before use. No custom codec (spec §3). UNVERIFIED.

## Voice processing (AEC / NS / AGC / VAD)

Preferred: WebRTC AudioProcessing (APM) via a Rust binding (e.g. `webrtc-audio-processing`
crate, builds the C++ library) — measure CPU, double-talk and echo per §4. NS on by default,
toggleable; never applied to screen-share system audio. UNVERIFIED (S1/S2).

## Screen capture (§5.2)

| OS | API | Notes |
|---|---|---|
| Windows | Windows.Graphics.Capture (monitor item), D3D11 frames | zero-copy into NVENC/MF encoder possible via shared D3D11 texture |
| macOS | ScreenCaptureKit (`SCDisplay`), IOSurface/CVPixelBuffer | screen-recording permission UX on M4 (§14.2) |
| Linux | XDG Desktop Portal ScreenCast + PipeWire (Wayland), X11 fallback (XShm/XComposite) | DMA-BUF import where supported |

All UNVERIFIED; first runnable spike is part of S3.

## System audio without the messenger's own sound (§5.1, §14.1)

| OS | Mechanism | Expected exactness |
|---|---|---|
| Windows 10 20348+/11 | WASAPI **process loopback** (`ActivateAudioInterfaceAsync` with `AUDIOCLIENT_PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE` on our PID) | exact exclusion of our process tree, by OS design |
| macOS 13+ | ScreenCaptureKit audio with `excludesCurrentProcessAudio = true` | exact for our process |
| Linux | PipeWire: capture sink monitor while routing our own playback to a separate node, or per-app links | depends on audio graph/session manager; must be validated, may require fallback |

Per spec: the feature must not be declared implemented where exclusion is not exact;
Linux fallback must be disclosed to the user. All UNVERIFIED (S3).

## Hardware video codecs (§5.2, §14.3)

* Windows: NVENC/NVDEC (NVIDIA Video Codec SDK), Media Foundation (vendor-neutral H.264/
  AV1 MFTs), AMF/QSV via MF. Dev machine: RTX 2060 SUPER (Turing: H.264/HEVC encode, **no
  AV1 encode**, no AV1 decode). Target RTX 4070 Super supports AV1 encode/decode.
* macOS: VideoToolbox (H.264 HW; AV1 decode on M3+/M4 only, no AV1 encode).
* Linux: VA-API / NVENC.
* Licensing (§14.8): prefer OS/vendor APIs over bundling FFmpeg; if FFmpeg is used, LGPL
  dynamic linking only and H.264 patent position documented. Owner decision before S3.

All UNVERIFIED; capability probing tool and 1080p60 encode spike belong to S3.

## Network/TLS trust for IP-only server (§2, §14.5)

Not addressed in S0. Plan: pin the server's public key/certificate fingerprint in the
signed installer (no global verification bypass), or use a CA that issues IP certificates.
Decision required before S5 exposes the server publicly.
