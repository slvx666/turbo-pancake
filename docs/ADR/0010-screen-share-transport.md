# ADR 0010 — Screen share: transport, subscriptions, loss repair and system audio

* Status: **accepted** (2026-11-04)
* Spec: §5.1–§5.3, §6 (SFU: publisher/subscriber, keyframe requests, loss recovery), §11
  (`media.track_published`, `subscription.updated`), §17 S3. Extends ADR 0007/0008/0009.

## Context

S3 needs one member's whole monitor (1080p60 H.264, hardware codecs, ADR 0009) and the
computer's sound without the messenger's own sound to reach a viewer through the SFU,
with 100–200 ms capture → display latency in good conditions, while voice keeps working
as in S2. S4 adds two qualities, previews and 50 publishers; S3 must not block that.

## Decision

1. **One media session, three m-lines.** Every client offers audio (voice, ADR 0008),
   H.264 video and a second audio m-line (the share's system audio). No renegotiation when
   shares start or stop. The SFU assigns every member session three SSRCs (voice, screen
   + its RTX SSRC, screen audio) and announces them with `TrackPublished{kind}` (new
   `TrackKind`: `AUDIO` = 0 so S2 messages keep their meaning, `SCREEN`, `SCREEN_AUDIO`).
2. **Share state and subscriptions are server-side.** `ScreenShare{active, size, fps}`
   (client → server, after every media offer so a restarted session announces again) →
   `ScreenShareState` to the room (and to a member whose session starts later).
   `SubscribeScreen{peer}` selects the one share a member watches (`""` = none). The SFU
   forwards screen video and screen audio only to that share's subscribers and only while
   it is announced — nothing is sent for unwatched shares (spec §5.1: no unneeded
   high-quality subscriptions). A new subscription gets a fresh outgoing stream and a
   keyframe request to the sharer.
3. **Loss repair hop by hop.** Video RTP is written `nackable`; str0m answers NACKs from
   its send buffer over RTX on each leg (client → SFU, SFU → viewer), keyframe requests
   (PLI/FIR) from a viewer go to the sharer (the forwarded stream's rid carries the source
   SSRC, so the SFU finds it). The receiver's `FrameAssembler` waits ≤ 250 ms for repairs,
   then drops to the next keyframe and asks for one.
4. **Pacing.** str0m NACKs, and accepts repairs, only within 100 sequence numbers of the
   newest packet, every 33 ms. A 1080p keyframe sent as one burst (≈ 140 packets) lost its
   early packets for good (37 % of frames delivered at 3 % loss). The client paces video
   (token bucket, `CallConfig::video_pacing_bps`, 12 Mbit/s): two repair rounds fit in the
   window on a LAN, 91–100 % delivered at 3 % loss both ways. More than 0.5 s queued →
   the frame is dropped and the encoder makes a keyframe (the encoder outran the pace).
5. **Newest frame wins everywhere.** Capture keeps only the latest frame; the encoder
   takes a frame per tick only when it has input capacity; the viewer decodes every frame
   (references) but converts and shows only the newest of each round.
6. **System audio** (spec §5.1): Windows WASAPI process loopback excluding our process
   tree — exact by construction, verified with tones (own −51.6 dB, a child process −23 dB,
   a process outside the tree captured at full level). Opus stereo 96 kbit/s,
   `Application::Audio`, no NS/AEC/AGC. Nothing is sent in silence (the RTP timestamp runs
   on, like voice activation). A viewer plays it as one more engine stream while watching
   (so it is part of the echo canceller's reference). macOS/Linux: not implemented — the
   share runs without sound and says so; no mixed capture is offered as a fallback.

## Consequences

* The SFU stays a forwarder: no transcoding, one subscription per member (S4 adds layers).
* Pacing caps a share at 12 Mbit/s and adds up to ≈ 100 ms to a keyframe (not to other
  frames); with long RTTs the 100-packet str0m window still limits repair — S4 must either
  raise it (str0m change) or add our own NACK/RTX handling for high bitrates.
* Same-machine trials measure capture → decoded picture directly (RTP timestamps are
  capture times on the performance-counter clock); display presentation (UI timer ≤ 16 ms
  + vsync) is not included and needs an instrumental (camera) measurement.
* The viewer converts NV12 → RGBA on the CPU (≈ 4 ms per 1080p frame on 4 threads) and
  Slint uploads it; GPU texture import is S4 work (spec §5.1 SHOULD: minimal copies).
