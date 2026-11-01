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

* offer (audio m-line + data channel) → serialized into the real `dsmini.v1.SessionDescription`
  protobuf → answer, same path back;
* ICE + DTLS + SRTP over **real loopback UDP sockets** on Windows: connected in **17 ms**;
* 100 RTP packets (Opus payload type, 20 ms cadence, opaque 120-byte payload) → **100/100**
  received, data channel message (UTF-8) received; `disconnect()` releases the session.

This proves transport feasibility only. No encoded speech, no network impairment, no SFU.

## Decision

* Use **str0m 0.24** for both the desktop client and `media-sfu`. No libwebrtc
  dependency (keeps binary size/RAM budget achievable; no Chromium build).
* Crypto backend for now: **`rust-crypto`** (pure Rust, builds everywhere without cmake/
  NASM/OpenSSL). Alternatives to benchmark in S1 (DTLS handshake time, SRTP throughput at
  20 Mbit/s, binary size): `aws-lc-rs` (str0m default, needs cmake on Windows),
  `wincrypto` (Windows CNG), `apple-crypto` (CryptoKit). A per-platform backend is allowed.

## Consequences / work we own

* Client: jitter buffer (adaptive), Opus encode/decode, APM (AEC/NS/AGC), pacing of
  video, keyframe request policy, reconnection (ICE restart) — S1/S2/S3.
* SFU: track routing, subscription/layer selection, RTCP termination/forwarding,
  per-subscriber BWE and admission control (§5.3), TURN interplay — S2/S4.
* Interop with browsers is not required (no web client), which removes most SDP
  compatibility risk; still we keep standard SDP for debuggability.

## Open items

* Netem tests (loss/jitter/RTT profiles of §13) on top of str0m + our jitter buffer: S1.
* Crypto backend benchmark and choice: S1.
* str0m simulcast vs. two independent encodings (§5.3) decision: S4 spike.
