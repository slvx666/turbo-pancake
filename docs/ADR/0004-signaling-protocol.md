# ADR 0004 — Realtime signaling: WebSocket + protobuf envelopes

* Status: accepted (S0, 2026-11-02)
* Spec: §6, §11

## Decision

* One WebSocket per client at `/v1/signal`; each binary frame carries one protobuf
  `ClientEnvelope` / `ServerEnvelope` (`proto/turbo_pancake/v1/signaling.proto`).
* Envelopes carry `protocol_version`, `request_id` (correlation) and server `event_id`
  (per-connection, gap-free) as required by §11. Later stages add domain events
  (`message.*`, `presence.*`, `call.*`, `media.*`) as new oneof variants in v1.
* Limits enforced server-side: 64 KiB frame, 32 KiB SDP, 1 KiB ICE candidate, room
  id/display name validation, 100 peers per room, 2048 connections, bounded per-connection
  outbound queue (256) with disconnect of slow consumers, heartbeat timeout 30 s.
* WebSocket read/write buffers are 8 KiB (default 128 KiB cost ~140 KB per idle connection;
  tuned value ~18 KB, measured in `benchmarks/results/signaling-*`).
* Rooms in S0 are ad-hoc and unauthenticated (dev only, loopback default). From S1/S5
  rooms become server-authoritative call sessions bound to conversations; joining requires
  an authenticated session and membership check.

## Alternatives considered

* JSON over WebSocket — simpler debugging, but the spec asks for strictly typed versioned
  contracts and the same types are shared with Rust clients anyway.
* gRPC/HTTP2 streaming — heavier dependency tree, no benefit for a single native client.

## Consequences

* Contract tests pin the byte encoding of representative messages.
* Any breaking change → `turbo_pancake.v2` package (see docs/API.md compatibility policy).
