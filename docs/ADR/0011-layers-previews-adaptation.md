# ADR 0011 — Two layers per share, layer subscriptions, previews and adaptation

* Status: **accepted** (2026-11-04)
* Spec: §5.1 (per-viewer quality, auto + manual, FPS before resolution, 1440p120, previews
  up to 50, one large picture, separate window), §5.3 (two versions of a source, preview
  160–320 px / 5–15 fps, decode limits, admission control, priorities), §11
  (`subscription.updated`, layer subscriptions), §17 S4. Extends ADR 0010.

## Context

S3 sent one H.264 stream per share to whoever watched it. S4 needs a cheap preview of
every share for the grid, the full stream only for the selected share, automatic and
manual quality, and fair use of the server uplink (50 viewers × 20 Mbit/s would fill the
1 Gbit/s link alone). The SFU must stay a forwarder (no transcoding, spec §5.3).

## Decision

1. **Two encodings, two m-lines.** The sharer encodes the captured picture twice on the
   GPU (D3D11 video processor scales, two encoder sessions): the high layer (the chosen
   preset, fitted to the monitor: no upscaling, fps ≤ refresh rate) and a preview layer
   (longest side 320 px, 10 fps, ≈ 200 kbit/s) with the same capture timestamps. The
   preview goes on a fourth m-line (second video m-line), so the SFU tells the layers
   apart by m-line, the way it already separates voice from share audio — no simulcast
   `rid` extension (not negotiated, ADR 0008) and no SDP change when a share starts.
   Rejected: simulcast with `rid` (str0m needs the mid/rid header extensions the SFU does
   not negotiate, ADR 0008); SVC (no OS H.264 encoder offers temporal/spatial SVC we
   can rely on on all three systems); a third, middle layer (one more encoder session
   per share; revisit if viewers on poor links need more than the preview).
2. **Previews** are subscribed per viewer with `SubscribePreviews` (≤ 50). The client
   subscribes only the visible page of the grid (`max_previews`, default 24) and none when
   previews are hidden; the watched share is not previewed. The client decodes all
   previews on one thread with the OS decoder in software (50 hardware decoder sessions
   cost more than 50 small CPU decodes: 50 × 320×180 × 10 fps = 14 % of one core here).
3. **The watched stream carries either layer.** `SubscribeScreen.layer`: AUTO (default,
   = high for S3 clients), HIGH, LOW. The SFU forwards the chosen layer into the viewer's
   one main stream (the source's video SSRC on its first video m-line); a switch waits for
   a keyframe of the new layer (the SFU asks the sharer for it) and continues the outgoing
   sequence numbers, so the viewer's assembler and decoder see one stream that changes
   size. Both layers use the capture clock as RTP timestamps, so timestamps stay monotonic.
4. **Automatic layer per viewer** (AUTO): the viewer's receiver reports on its main stream
   (str0m `MediaEgressStats` per stream, 1 s) feed `NetworkPolicy`: ≥ 8 % loss in two
   reports or ≥ 20 % in one → low; back to high after 8 clean (≤ 2 %) reports once a hold
   of 8 s has passed, the hold doubling (to 64 s) when the probe fails within 10 s.
5. **Admission control** (spec §5.3): `--max-high-egress-mbps` caps the sum of the high
   streams' announced bitrates. Subscriptions are served in order; one that does not fit
   gets the low layer with reason ADMISSION (explicit HIGH requests included) and moves up
   when the budget frees. Priorities follow: audio is never limited, high streams by
   budget, previews by the client's page limit.
6. **Sender adaptation** (spec §5.1 "FPS before resolution"): `adapt::Adapter` on the
   sharer once a second, fed by the server's receiver reports for our main video, the
   share of packets the server NACKed (receiver reports count repaired packets as
   received) and pacer drops. ≥ 10 % → −30 % bitrate, 2–10 % hold, < 2 % for 3 s → +8 %
   up to the preset. The picture follows the bitrate down a ladder at the preset frame
   rate (preset size → 1080p → 720p → 540p) and lowers the frame rate (½ steps, ≥ 15 fps)
   only at the smallest size; down at most every 2 s, up one rung per 10 s. A size change
   rebuilds converter + encoder (keyframe); the server is told the new size and bitrate.
   Pacing follows the bitrate (1.6 × target, ≥ 12 Mbit/s).
7. **1440p120** is a preset like the others, fitted to the monitor. The dev monitors are
   1080p (60 Hz secondary), so the capture side of 1440p120 is not verifiable here; the
   encoder and decoder are measured at 1440p120 with generated pictures.

## Consequences

* A viewer on a bad downlink falls back to the preview layer (320 px / 10 fps): the only
  per-viewer degradation without transcoding or a third layer. FPS-before-resolution
  applies to the sender's uplink adaptation, which affects every viewer.
* Loss upstream of the SFU (the sharer's uplink) also shows in viewers' receiver reports,
  so AUTO viewers may drop to the low layer for a problem the sharer's adaptation is
  already handling (seen in the trial with 15 % uplink loss); the server cannot tell the
  two apart from receiver reports alone.
* The SFU stays single-threaded: 50 × 49 previews (≈ 49 000 packets/s out) use ≈ 92 % of
  one core on the dev machine (PROJECT_STATE issue 25); sharding across cores is S8 work.
* str0m's 100-packet repair window (issue 31) still limits repair of high-bitrate layers
  on long RTTs; pacing at 1.6 × bitrate keeps keyframe bursts inside it on a LAN.
* GPU texture import into Slint is not done: the viewer still converts NV12 → RGBA on the
  CPU and Slint uploads it (spec §5.1 SHOULD, "minimal copies"; issue 32).
