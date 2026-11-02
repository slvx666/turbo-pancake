# ADR 0006 — Voice pipeline: cpal + rtrb + rubato + libopus, own jitter buffer

* Status: **accepted for S1** (2026-11-02); voice processing (AEC/NS/AGC) still open → S1/S2
* Spec: §3 (no blocking between RT audio, network, UI), §4, §13 (network profiles), §14.8

## Context

S1 needs Opus 48 kHz voice with loss resilience and an adaptive jitter buffer on Windows,
macOS and Linux. The dev machine's microphone and speaker run at **192 kHz** (ADR 0005),
so resampling is mandatory. The spec forbids writing our own codec (§3).

## Decision

| Concern | Choice | License | Why |
|---|---|---|---|
| Device I/O | `cpal` 0.18 | Apache-2.0 | WASAPI / CoreAudio / ALSA (PipeWire/Pulse via ALSA); already verified by `media-probe` |
| RT ↔ worker hand-off | `rtrb` 0.4 SPSC rings | MIT/Apache-2.0 | wait-free, no allocation in the callback |
| Resampling | `rubato` 5 `Fft` (synchronous, fixed ratio) | MIT/Apache-2.0 | high quality, ≈ 22 µs per 20 ms frame 192→48 kHz |
| Codec | `opus` 0.4 → `opusic-sys` 0.7.6, **bundled libopus 1.6.1**, static | MIT/Apache-2.0 (binding), BSD-3-Clause (libopus) | reference implementation; FEC/DTX/PLC; no runtime DLL |
| Jitter buffer, FEC/PLC scheduling | own (`media-audio/src/jitter.rs`) | — | small, testable, matches our 20 ms framing; not codec/crypto code |

Rejected: pure-Rust Opus ports (`opus-rs`, `opus-pure`, `opus1`) — young, no proven
quality/CPU parity with libopus (§3 allows proven native libs); `audiopus_sys` — older
libopus, needs separate static/dynamic configuration; `opus-head-sys` — vendors libopus
too, but `opus` has the more complete safe API (FEC decode, DTX, CTLs).

Pipeline parameters (defaults, all adjustable at runtime through `EncoderConfig`):
mono, 20 ms frames, 32 kbit/s VBR, `Application::Voip`, `Signal::Voice`, in-band FEC on,
expected loss 5 %, DTX off, complexity 9. Muted microphone → encoded digital silence is
still sent (keeps the receiver clock and jitter estimate alive; nothing from the mic leaves).

Jitter buffer: slots keyed by extended RTP sequence; target delay = p95 − min of transit
times over the last 100 packets + one frame + 10 ms margin, clamped to 20–400 ms; grows at
once, shrinks one frame per packet; a missing frame is rebuilt from the next packet's FEC,
otherwise PLC; nothing ahead → PLC and the clock keeps running, after 200 ms rebuffer;
more than target + 2 frames buffered → drop one frame per tick (also absorbs clock drift).

## Findings / evidence (Windows dev machine, 2026-11-02)

* Stationary test tones get **no FEC**: SILK's VAD classifies them as noise and does not
  encode LBRR. Tests and benchmarks use `testsignal::speech_like` (gliding pitch, moving
  formants, syllable envelope, pauses) instead.
* **Build pitfall:** with the Visual Studio generator autodetected, the `cmake` crate
  overrides `CMAKE_C_FLAGS_RELEASE` without `/O2`, so libopus was built **unoptimised**
  (845 µs/frame at complexity 9). Setting `CMAKE_GENERATOR` explicitly (local
  `.cargo/config.toml`, CI step on Windows) restores CMake's Release flags: **243 µs/frame**
  (≈ 1.2 % of one core), decode 29 µs/frame. `benchmarks/results/audio-codec-cost-windows-20261102.json`.
* Network matrix (`tests/impairment.rs`, 20 s, loss 0/1/3/5/10 % × jitter 0/20/50 ms):
  no decode errors, no late packets, level preserved (ratio 0.91–0.95); at 10 % random
  loss FEC repairs ≈ 90 % of lost frames, PLC-only ≤ 1.4 % of frames; target delay
  40 / 60 / 80 ms for 0 / 20 / 50 ms jitter. 2 s outage: concealment then rebuffer, delay
  back to 60 ms. `benchmarks/results/audio-impairment-windows-20261102.json`.
* Real devices (`examples/device_check.rs`): fifine USB mic 192 kHz mono and Realtek
  output 192 kHz stereo driven through the full pipeline for 5 s: 0 capture overflows,
  ≈ 50 frames/s sent and played, 0 concealed; startup underrun of ≈ 22 ms on the output
  before the first decoded frame. Microphone level was 0 (muted at OS level or silent
  room), so audible quality is **UNVERIFIED**.
  `benchmarks/results/audio-device-check-windows-20261102.json`.

* **Audio engine** (`src/engine.rs`, step 2, 2026-11-02): one worker thread owns both
  streams (cpal streams are created and dropped on it, so `!Send` streams never cross
  threads), drains the mic ring into `VoiceSender`, mixes per-peer `VoiceReceiver`s at
  48 kHz and resamples once to the output rate. Playout is paced by the speaker ring
  (40 ms prefill/target); without an output device it ticks on the wall clock. Engine ↔
  transport and UI ↔ engine channels are bounded and `try_send` on the engine side.
  Fatal stream errors → reopen selected device → default → retry every 1 s.
* **cpal error kinds:** the first engine run on real devices reopened the microphone
  213 times in 10 s: WASAPI reports a capture *data discontinuity* as `ErrorKind::Xrun`
  through the error callback and the handler treated every kind except `DeviceChanged`
  as fatal. `Xrun` is now counted (`StreamHealth::backend_xruns`) and `RealtimeDenied`
  flagged; only the remaining kinds mark the stream failed. After the fix
  (`examples/engine_check.rs`, 20 s, live mic + speaker switch by id halfway): 999 frames
  sent/played, 0 concealed, 0 underruns, 2 reopens (the switches), 2 backend xruns,
  start 55 ms, stop 15 ms, devices reopen after stop.
  `benchmarks/results/audio-engine-check-windows-20261102.json`.

## Open / UNVERIFIED

* AEC / NS / AGC / VAD: WebRTC APM binding (`webrtc-audio-processing` 2.x) to be evaluated
  next; it must run at 48 kHz on 10 ms frames between resampler and encoder.
* Device hot-unplug: the engine reopens failed streams with fallback (tested on simulated
  devices, `tests/engine.rs`); a physical unplug and sleep/wake are untested.
* Engine thread priority is the OS default (no real-time promotion yet).
* macOS / Linux device paths: CI builds and unit tests only.
* Bitrate/FEC adaptation from RTCP receiver reports (str0m) — S1/S2.
