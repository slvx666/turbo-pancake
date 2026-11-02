# ADR 0003 — WebRTC stack: str0m in client and SFU (provisional)

* Status: **provisional** (S0, 2026-11-02)
* Spec: §3, §6, §14.7

## Context

Media must use WebRTC (ICE, DTLS-SRTP, RTP/RTCP). The spec names `str0m` as the candidate
for SFU and client. str0m is a Sans-I/O library: it provides ICE, DTLS, SRTP, SDP,
RTP/RTCP, NACK/RTX, TWCC + bandwidth estimation, simulcast primitives and data channels,
but **no** sockets, threads, jitter buffer, audio processing, codecs or SFU routing.

## Spike (implemented, passing)

`crates/media-transport` (`UdpPeer`: one `UdpSocket` + one `Rtc`, poll loop) and
`crates/media-transport/tests/loopback_spike.rs`:

* offer (audio m-line + data channel) → serialized into the real `turbo_pancake.v1.SessionDescription`
  protobuf → answer, same path back;
* ICE + DTLS + SRTP over **real loopback UDP sockets** on Windows: connected in **17 ms**
  (rust-crypto), **27 ms** (aws-lc-rs, current);
* 100 RTP packets (Opus payload type, 20 ms cadence, opaque 120-byte payload) → **100/100**
  received, data channel message (UTF-8) received; `disconnect()` releases the session.

This proves transport feasibility only. No encoded speech, no network impairment, no SFU.

## Decision

* Use **str0m 0.24** for both the desktop client and `media-sfu`. No libwebrtc
  dependency (keeps binary size/RAM budget achievable; no Chromium build).
* Crypto backend: **`aws-lc-rs`** (str0m default). Builds on Windows/MSVC without cmake
  or NASM on PATH (verified 2026-11-02), and in CI on Ubuntu/macOS.
  * Revision 2026-11-02: the initial `rust-crypto` backend was dropped after the first CI
    `cargo audit` run: it pulls `dimpl[rust-crypto] → p256/p384/ecdsa 0.13 →
    crypto-bigint 0.5.5`, affected by **RUSTSEC-2026-0387** (secret-dependent timing in
    constant-time ops, fixed only in 0.7.6 which the 0.13 curve crates cannot use).
  * Still to benchmark in S1 (DTLS handshake, SRTP throughput at 20 Mbit/s, binary size):
    `wincrypto` (Windows CNG), `apple-crypto` (CryptoKit). Do not re-enable `rust-crypto`
    until its curve crates move to a fixed crypto-bigint.

## Consequences / work we own

* Client: jitter buffer (adaptive), Opus encode/decode, APM (AEC/NS/AGC), pacing of
  video, keyframe request policy, reconnection (ICE restart) — S1/S2/S3.
* SFU: track routing, subscription/layer selection, RTCP termination/forwarding,
  per-subscriber BWE and admission control (§5.3), TURN interplay — S2/S4.
* Interop with browsers is not required (no web client), which removes most SDP
  compatibility risk; still we keep standard SDP for debuggability.

## S1: call driver (`crates/media-transport/src/call.rs`, 2026-11-02)

* One thread per call owns a `UdpPeer`; engine ↔ call and signaling ↔ call channels are
  bounded and never block the call thread. States: Negotiating → Connecting → Connected
  ↔ Reconnecting → Closed / Failed(reason).
* **RTP mode** (`set_rtp_mode(true)`), Opus only (`clear_codecs().enable_opus(true, false)`):
  str0m's frame mode holds up to 15 audio packets (300 ms) waiting for a gap to fill;
  in RTP mode every packet is handed over at once with its sequence number and our
  adaptive jitter buffer (ADR 0006) reorders/conceals. Outgoing packets carry the
  engine's sequence numbers and timestamps.
* **Extended sequence numbers:** `RtpWrite` wants 64-bit sequence numbers whose upper bits
  are the SRTP rollover counter. Passing the raw u16 lost every packet after the wrap;
  starting the extension at 65 536 made the receiver (ROC 0) reject everything. The
  driver extends relative to the previous number starting at the raw value
  (unit-tested across the wrap and reordering).
* **Windows timer resolution:** a 1–2 ms socket read timeout really waited ≈ 16 ms (loop
  gap 16.7 ms measured), delaying outgoing audio and reading one datagram per iteration;
  under parallel test load ~9 % of datagrams were lost on loopback. `UdpPeer` now uses a
  non-blocking socket, drains every queued datagram per step (polling str0m after each,
  RTP mode keeps one pending packet) and sleeps ≤ 1 ms with `thread::sleep`: loop gap
  2–6 ms, 0 loss, handshake 8 ms (was 17–27 ms). Cost: ≤ 1000 wake-ups/s per call.
* **Peer loss:** ICE consent on the controlled (answering) side did not notice a
  vanished offerer within 20 s. The driver also treats 3 s without any datagram as lost
  (Reconnecting; back to Connected on the next RTP packet; Failed after
  `reconnect_timeout`, default 10 s).
* Candidates: host candidate of the bind address, or of the default-route IP when bound
  to 0.0.0.0 (no packet sent to find it). STUN/TURN: later (S2/S8).
* Evidence: `tests/call.rs` (both directions 150/150 across the u16 wrap, 10 % wire loss →
  exactly the dropped packets missing, peer loss → Reconnecting → Failed) and
  `tests/voice_call.rs` (two `AudioEngine`s on simulated devices through two drivers over
  UDP, 5 % loss: 301 frames played, 17 FEC-repaired, 1 concealed, 0 late, 0 decode
  errors, speaker peak 0.305 for a 0.3 tone, leave releases devices/threads/socket).

## Open items

* Netem over real UDP: loss done (`tests/call.rs`, `tests/voice_call.rs`); jitter/RTT
  profiles run in-process (`media-audio/tests/impairment.rs`); a delay/jitter shim on the
  socket is still missing.
* Crypto backend benchmark and choice: S1.
* str0m simulcast vs. two independent encodings (§5.3) decision: S4 spike.
