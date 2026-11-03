# ADR 0008 — Per-member audio: one m-line, one SSRC per member

* Status: **accepted** (2026-11-02)
* Spec: §4 (group calls, per-member volume), §6 (SFU: publisher/subscriber, track
  negotiation), §11 (`media.track_published/unpublished`), §17 S2. Extends ADR 0007.

## Context

ADR 0007 put every member on one WebRTC session with the server's SFU, but forwarded only
one stream per member: in rooms of more than two, each member heard the earliest other
one. S2 needs everyone to hear everyone, each member as a separate stream on the client
(own jitter buffer, own volume, speaking indicator), for up to 100 members, with joins and
leaves that do not disturb the others.

Options from the S2 plan:

* **(a) Renegotiation, one recvonly m-line per member.** Every join/leave triggers an SDP
  round trip with every other member (99 at 100 members); the SDP grows by one m-line per
  member (~40 KiB at 100, close to the 64 KiB frame limit); offers can collide with the
  member's own transport restarts. Most moving parts.
* **(b) Fixed last-N receive slots** with a slot → member mapping event. Bounded
  bandwidth, but a slot changing owner restarts a jitter buffer mid-speech, the mapping
  event races the RTP it describes, and per-member volume follows slots, not people.
* **(c) One m-line, one SSRC per member** (what Discord-style voice servers do). No
  renegotiation; the mapping SSRC → member travels once per member session over
  signaling.

## Decision

Option **(c)**.

1. The SFU assigns every member session an SSRC (unique among live sessions, a new one
   for every new session of the same member, so a rejoin starts a fresh sequence space).
   A member's audio leaves the SFU on that SSRC in every other session of the room. RTP is
   forwarded, never transcoded (spec §17 S2).
2. Signaling: `TrackPublished{room_id, peer_id, ssrc}` / `TrackUnpublished{…}`
   (`ServerEnvelope` fields 20/21). When a member's media session is accepted it gets one
   `TrackPublished` per member already sending, and every other member gets one for it;
   when a session ends (leave, replaced, ICE dead) the others get `TrackUnpublished`.
3. The client registers each announced SSRC on its call (`CallDriver::expect_source`);
   each SSRC is its own engine peer (peer id = SSRC) with its own jitter buffer and volume.
   Packets of an SSRC not (yet) announced are dropped — the first few ms of a member's
   audio at most, since the event leaves the server before the first forwarded packet.
4. Sequence numbers: each outgoing stream subtracts the source's rollover at the first
   forwarded packet, so every stream starts with rollover counter 0 (the receiver's SRTP
   assumption) however long the source has been talking.

### str0m details found on the way

* **No `mid` header extension.** With several SSRCs on one m-line, str0m's receiver maps
  an unknown SSRC that carries `mid` onto the m-line's existing stream (it treats it as an
  SSRC change), which would steal another member's stream. The SFU answers without the
  `mid` extension (audio level, abs-send-time and transport-cc stay); the client maps
  streams by SSRC only (`a=ssrc` from the SDP and the announced ones). Client → SFU
  keeps working through the client's `a=ssrc`.
* **One rid per outgoing stream.** str0m's pacer queues by (mid, rid), not by SSRC: a
  second stream on the same mid without a rid is never sent. Each forwarded stream is
  declared with its own rid (the SSRC in hex); the rid extension is not negotiated, so it
  never goes on the wire. str0m keeps the rid list of a session (8 bytes per source ever
  forwarded on it); bounded by the members a session sees.

## Consequences

* Bandwidth: everyone who sends is forwarded to everyone: N·(N−1) streams. Opus voice
  ≈ 40 kbit/s → 20 members talking at once ≈ 15 Mbit/s server egress, 100 ≈ 400 Mbit/s.
  Members are silent most of the time: voice activation (S2 step 3) stops sending, and
  DTX makes silence cheap. A cap on concurrently forwarded speakers (loudest/most recent
  N, using the negotiated audio-level extension) is the planned guard for large rooms;
  measured in S2 step 6.
* **Measured (S2 step 6, `turbo-pancake-bench sfu`, Windows dev machine, release, loopback):**
  20 members all talking = 380 forwarded streams, 19 000 packets/s out, 18.7 % of one
  core, 100 % delivered, ≈ 25 Mbit/s egress including headers. The SFU thread costs
  ≈ 11 µs per forwarded packet under load and saturates one core at ≈ 78 000 packets/s
  out (50 members all talking: 64 % delivered — it drops, it does not fall over). One SFU
  thread therefore carries about 1 500 concurrently forwarded streams: a 100-member room
  with up to ~15 people talking at once, or many small rooms.
* **Speaker cap: not implemented in S2.** 20 members (the S2 target) need < 20 % of a
  core in the worst case. For 100-member rooms a cap is needed only when more than ~15
  members send at once; it must rank *speech*, not traffic (a member without voice
  activation sends silence continuously), so it needs the audio level in every packet:
  the client sets the negotiated RFC 6464 audio-level extension from the encoder's frame
  level, the SFU forwards the N loudest recently active sources per room. Alternative or
  complement: one SFU worker per CPU core, rooms sharded across them. Decided when a
  stage needs larger rooms (or S7 load tests).
* The client decodes every forwarded stream: ≈ 30 µs per 20 ms frame per stream on the dev
  machine — 20 talking members ≈ 3 % of one core.
* Per-member state on the client keys on SSRC, and the UI maps member → SSRC through
  `TrackPublished`; volume and speaking indicator per member follow from it (S2 step 3).
* Voice activation (S2 step 3) pauses a member's stream: the sender stops sending, its
  sequence number pauses and the RTP timestamp keeps running (RTP practice), so the SFU
  forwards no traffic and the receivers' loss statistics stay clean; the jitter buffer
  uses the timestamp to tell a pause from loss or jitter.
* Video (S3+) can reuse the scheme (more SSRCs, a video m-line) or move to explicit
  subscriptions; decided with S3/S4.

## Evidence

`apps/desktop/tests/session.rs::three_members_each_hear_both_others` (real app-server +
SFU, real UDP, simulated devices): three members talk, each hears both others as two
streams (≥ 100 frames each, 0 decode errors, < 3 % concealed); one leaves → the others
keep one stream each and keep hearing each other; she rejoins with a new session →
everyone hears both others again. The two-member scenarios of ADR 0007 (UDP with 5 % loss,
mute, leave/rejoin, WebSocket fallback, server without UDP, signaling outage) pass
unchanged. Contract test `track_field_numbers_are_pinned`.
