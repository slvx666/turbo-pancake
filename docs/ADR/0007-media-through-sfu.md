# ADR 0007 — Call media through the server (SFU), UDP with a WebSocket fallback

* Status: **accepted** (2026-11-02)
* Spec: §3 (`media-sfu`, star topology), §6 (WebRTC, TURN must not inflate traffic
  silently), §12, §17 S1/S2. Supersedes the peer-to-peer media path of S1 step 4.

## Context

The first real call between two homes (owner ↔ a remote tester, server on the tester's
public IP, both on Windows) failed with peer-to-peer media: signaling worked, but every attempt
ended in `connect timeout`. The owner's machine routes traffic through a VPN client in TUN mode; the STUN-reported
"public" address was the proxy's exit, and with the VPN off it still failed. The owner's
requirement: calls must work **reliably with and without a VPN** ("everybody uses one").

Peer-to-peer through NATs (host + STUN candidates) cannot meet that: VPNs/proxies and
symmetric NATs break hole punching, some VPNs carry no UDP at all, and a TURN relay is
both a separate service and still UDP-first.

## Decision

1. **Every member talks only to the server.** After joining a room the client offers one
   WebRTC session (ICE + DTLS-SRTP, Opus, RTP mode) to the server's SFU
   (`crates/media-sfu`, run inside `app-server` on its own thread); the server is ICE-lite
   and forwards RTP between members without transcoding. The client always sends first,
   so any NAT or VPN that lets the client reach the server lets media through.
2. **UDP to the server** (same port number as the WebSocket, default 7878) is the primary
   path. The server announces as candidates the IP the client dialed (from the client's
   `MediaOffer.dialed_ip`; behind a port forward that is the public IP), configured
   `--public-ip`s and its LAN address; it needs no STUN.
3. **WebSocket tunnel fallback:** the same encrypted datagrams (STUN/DTLS/SRTP) travel in
   `MediaDatagram` frames of the signaling connection (`MEDIA_TRANSPORT_WEBSOCKET`), with
   fixed virtual addresses on both ends for ICE. Wherever signaling works, media works —
   only TCP 7878 must be reachable. The client (`TransportPref::Auto`) tries UDP for 3 s,
   then the tunnel, and alternates after failures; a server without UDP rejects the UDP
   offer at once. Encryption is unchanged (DTLS-SRTP end to end between client and SFU).
4. Media datagrams for a member are **dropped, never queued without bound**, when its
   connection is full (like packet loss); they never get the member disconnected.

## Consequences

* Traffic goes through the server: ~40 kbit/s per member per direction for Opus voice —
  negligible for S1/S2 audio; the 1 Gbit/s budget of §5.3 is about video (S3+).
* No E2EE: the SFU decrypts SRTP to forward it (as any SFU; spec §8 FUTURE item).
* Latency over the tunnel is TCP: under loss, head-of-line blocking adds jitter; the
  adaptive jitter buffer absorbs it, UDP stays preferred.
* S1 scope of the SFU: one forwarded stream per member; in rooms of more than two each
  member hears the earliest other member. Per-member tracks, subscriptions and mixing
  rules are S2 work.
* The P2P path (`CallDriver` between two clients, STUN candidates) remains in
  `media-transport` and its tests; the desktop no longer uses it.
* Protocol v1 gains `MediaOffer`, `MediaAnswer`, `MediaDatagram` (new oneof variants,
  compatible; field numbers pinned in contract tests).

## Evidence

`apps/desktop/tests/session.rs` (real app-server + SFU, real UDP / WebSocket):
UDP call with 5 % loss (0 decode errors, concealment < 3 %), mute, leave/rejoin (found and
fixed: a member's new session restarts its sequence space — listeners now restart their
numbering), **every UDP datagram dropped on both clients → automatic switch to the
WebSocket, audio flows**, server without UDP → WebSocket at once, signaling outage →
reconnect and talk again.
