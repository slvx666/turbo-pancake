# turbo-pancake API

Canonical schemas: [`proto/turbo_pancake/v1/`](../proto/turbo_pancake/v1). This document describes
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
| `JoinRoom{room_id}` | `RoomJoined{room_id, peers[]}` (existing members); others get `PeerJoined`. While in another room: first `RoomLeft{old room}` (event, `request_id` 0), the old room's members get `PeerLeft{reason=SWITCHED}` and the media session ends; then `RoomJoined`. Same room again: `RoomJoined` with the current members, nobody else is told | `BAD_REQUEST` (invalid id), `ROOM_FULL` (default 100; checked before leaving, so the current call goes on) |
| `LeaveRoom{room_id}` | `RoomLeft{room_id}`; others get `PeerLeft{reason=LEFT}`. Not in that room: `RoomLeft` all the same, nothing changes (idempotent) | — |
| `Signal{room_id, to_peer_id, description \| ice_candidate}` | none; target gets `SignalRelay{from_peer_id, ...}` | `NOT_IN_ROOM`, `PEER_NOT_FOUND`, `BAD_REQUEST` (self-target, missing payload, SDP > 32 KiB, untyped SDP, candidate > 1 KiB) |
| `Ping{client_time_us}` | `Pong{client_time_us}` | — |

* A connection is in at most one room at a time (spec: one active call per user): a new
  join closes the previous call. `ALREADY_IN_ROOM` is no longer sent (kept reserved). Until
  accounts (S5) "user" means connection: two connections of one person are two members.
* A `RoomLeft` for the room the client is in, without its own leave, means the server
  ended the membership; the desktop client ends the call and does not rejoin by itself.
* Room id: 1–64 chars of `[A-Za-z0-9_-]`.
* Abrupt disconnects announce `PeerLeft{reason=DISCONNECTED}`.
* Ordering guarantee: `RoomJoined` is always delivered to the joiner before any
  `SignalRelay` from members who reacted to its `PeerJoined`.
* Slow consumers: each connection has a bounded outbound queue (256 frames); a peer
  whose queue overflows is disconnected rather than buffering without limit.
* The server never logs SDP/ICE contents or media datagrams.

### Media through the server (SFU, ADR 0007)

| Client request | Success reply | Errors |
|---|---|---|
| `MediaOffer{room_id, sdp, transport, dialed_ip}` (in the joined room) | `MediaAnswer{room_id, sdp, transport}` with the same `request_id` | `NOT_IN_ROOM`, `BAD_REQUEST` (no transport, empty SDP or > 32 KiB, offer rejected e.g. UDP disabled), `INTERNAL` (media server busy/unavailable) |
| `MediaDatagram{data}` (transport `WEBSOCKET` only) | none; the server sends `MediaDatagram` back | dropped silently when invalid (empty, > 2048 bytes) or without a media session |

* The client offers one sendrecv Opus m-line; the server is ICE-lite, its candidates are in
  the answer: for `UDP` the IP the client dialed, configured public IPs and its LAN
  address, all with the media port (default: the WebSocket port number, UDP); for
  `WEBSOCKET` a virtual address — datagrams then travel inside `MediaDatagram` frames.
* A new `MediaOffer` replaces the connection's media session; `LeaveRoom` and a closed
  connection end it.
* Media datagrams to a slow connection are dropped (like packet loss), never queued
  without bound, and never disconnect it.

Server events (ADR 0008, spec §11 `media.track_published/unpublished`):

| Event | Meaning |
|---|---|
| `TrackPublished{room_id, peer_id, ssrc}` | The member's audio arrives on this connection's media session as the RTP stream `ssrc`. Sent for every member already sending when the client's media session is accepted, then whenever another member's session starts. |
| `TrackUnpublished{room_id, peer_id, ssrc}` | That stream ended (member left, its session was replaced or died). |

* Every other member is its own SSRC on the one audio m-line; no renegotiation. A
  member's new session gets a new SSRC (its `TrackUnpublished` for the old one comes
  first). The server answers without the RTP `mid` header extension, so streams are told
  apart by SSRC only; packets of an SSRC not announced yet are dropped by the client.

### Error codes

`UNSUPPORTED_VERSION(1)`, `BAD_REQUEST(2)`, `PAYLOAD_TOO_LARGE(3)`, `HELLO_REQUIRED(4)`,
`ALREADY_IN_ROOM(5)`, `NOT_IN_ROOM(6)`, `PEER_NOT_FOUND(7)`, `ROOM_FULL(8)`, `INTERNAL(9)`.
`Error.message` is human-readable and never contains secrets or user content.

## Compatibility policy

* Field numbers are never reused; removed fields become `reserved`.
* Adding fields / oneof variants is allowed within v1; clients must ignore unknown ones.
* Breaking changes → new package `turbo_pancake.v2`, new `PROTOCOL_VERSION`, server supports
  both for one release cycle.
* Binary encodings of representative messages are pinned in contract tests.
