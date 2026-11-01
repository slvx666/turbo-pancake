# DsMini API

Canonical schemas: [`proto/dsmini/v1/`](../proto/dsmini/v1). This document describes
transport, semantics and error handling. Contract tests:
`crates/protocol/tests/contract.rs`, behaviour tests: `apps/app-server/tests/signaling.rs`.

Status legend: **implemented** (code + tests exist), **planned** (stage in brackets).

## HTTP endpoints

| Method | Path | Status | Response |
|---|---|---|---|
| GET | `/healthz` | implemented | `200 ok` while the process serves requests |
| GET | `/readyz` | implemented | `200 ready`; `503` when the WebSocket connection limit is reached |
| GET | `/v1/version` | implemented | `{"server_version":"x.y.z","protocol_version":1}` |
| GET | `/v1/signal` | implemented | WebSocket upgrade (below); `503` when connection limit reached |
| POST | `/v1/auth/register`, `/v1/auth/login`, `/v1/auth/refresh`, `/v1/auth/logout`, `/v1/me` | planned (S5) | |
| GET | `/v1/users/lookup?username=` | planned (S5) | exact match only |
| POST/DELETE | `/v1/blocks/{user_id}` | planned (S5) | |
| GET/POST | `/v1/conversations`, `/v1/conversations/{id}/members` | planned (S5/S6) | |
| GET/POST/PATCH/DELETE | `/v1/conversations/{id}/messages` | planned (S5) | cursor pagination, `client_nonce` idempotency |
| POST | `/v1/conversations/{id}/read` | planned (S5) | |
| POST | `/v1/calls`, `/v1/calls/{id}/join`, `/v1/calls/{id}/leave` | planned (S1/S2/S6) | |
| GET | `/v1/updates/check` | planned (S7) | signed manifest |

## WebSocket `/v1/signal` (protocol v1)

* One **binary** frame = one protobuf envelope. Client sends `ClientEnvelope`, server
  sends `ServerEnvelope`. Text frames are answered with `ERROR_CODE_BAD_REQUEST`.
* Maximum frame: **64 KiB** (`MAX_FRAME_BYTES`). Larger frames close the connection.
* `protocol_version` must be `1` in every envelope. Any other value → `Error{UNSUPPORTED_VERSION}`
  followed by close.
* `request_id` (client-chosen, non-zero when a reply is wanted) is echoed in the direct
  reply or error. Unsolicited events carry `request_id = 0`.
* `event_id` is a per-connection, gap-free counter starting at 1 for every server frame.
* Unknown fields are ignored (forward compatibility within v1, tested).

### Session lifecycle

1. Client → `Hello{client_version, display_name}` (must be first; anything else →
   `HELLO_REQUIRED`). Display name: 1–64 printable chars, no leading/trailing spaces.
2. Server → `Welcome{peer_id, server_version, heartbeat_interval_ms, max_frame_bytes}`.
3. The client must send *some* frame (e.g. `Ping`) at least every
   `heartbeat_interval_ms`; after 30 s of silence (default) the server drops the
   connection and announces `PeerLeft{reason=TIMEOUT}`.

S0 has **no authentication**; the server binds to `127.0.0.1` by default. S5 adds a
token-authenticated handshake (Hello field 3 is reserved for it).

### Rooms and relay

| Client request | Success reply | Errors |
|---|---|---|
| `JoinRoom{room_id}` | `RoomJoined{room_id, peers[]}` (existing members); others get `PeerJoined` | `BAD_REQUEST` (invalid id), `ALREADY_IN_ROOM`, `ROOM_FULL` (default 100) |
| `LeaveRoom{room_id}` | `RoomLeft{room_id}`; others get `PeerLeft{reason=LEFT}` | `NOT_IN_ROOM` |
| `Signal{room_id, to_peer_id, description \| ice_candidate}` | none; target gets `SignalRelay{from_peer_id, ...}` | `NOT_IN_ROOM`, `PEER_NOT_FOUND`, `BAD_REQUEST` (self-target, missing payload, SDP > 32 KiB, untyped SDP, candidate > 1 KiB) |
| `Ping{client_time_us}` | `Pong{client_time_us}` | — |

* A connection is in at most one room at a time (spec: one active call per user).
* Room id: 1–64 chars of `[A-Za-z0-9_-]`.
* Abrupt disconnects announce `PeerLeft{reason=DISCONNECTED}`.
* Ordering guarantee: `RoomJoined` is always delivered to the joiner before any
  `SignalRelay` from members who reacted to its `PeerJoined`.
* Slow consumers: each connection has a bounded outbound queue (256 frames); a peer
  whose queue overflows is disconnected rather than buffering without limit.
* The server never logs SDP/ICE contents.

### Error codes

`UNSUPPORTED_VERSION(1)`, `BAD_REQUEST(2)`, `PAYLOAD_TOO_LARGE(3)`, `HELLO_REQUIRED(4)`,
`ALREADY_IN_ROOM(5)`, `NOT_IN_ROOM(6)`, `PEER_NOT_FOUND(7)`, `ROOM_FULL(8)`, `INTERNAL(9)`.
`Error.message` is human-readable and never contains secrets or user content.

## Compatibility policy

* Field numbers are never reused; removed fields become `reserved`.
* Adding fields / oneof variants is allowed within v1; clients must ignore unknown ones.
* Breaking changes → new package `dsmini.v2`, new `PROTOCOL_VERSION`, server supports
  both for one release cycle.
* Binary encodings of representative messages are pinned in contract tests.
