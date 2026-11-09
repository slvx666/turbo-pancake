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
| GET | `/v1/version` | implemented | `{"server_version":"x.y.z","protocol_version":1,"messenger":true}` |
| GET | `/v1/server-stats` | implemented (S8) | operator metrics, aggregate counts only: `{"connections","max_connections","db_pool":{"max","size","idle","in_use"}}` (`db_pool` is `null` without the messenger) |
| GET | `/v1/signal` | implemented | WebSocket upgrade (below); `503` when connection limit reached |
| POST | `/v1/auth/register` | implemented (S5) | `RegisterRequest` → `201 AuthResponse`; `409` username taken |
| POST | `/v1/auth/login` | implemented (S5) | `LoginRequest` → `AuthResponse`; `401` for unknown user *and* wrong password (same body, same cost) |
| POST | `/v1/auth/refresh` | implemented (S5) | → `AuthResponse` with a new token; the old token stops working |
| POST | `/v1/auth/logout` | implemented (S5) | → `Empty`; revokes this session, its WebSocket gets `SessionRevoked` and closes |
| GET / PATCH | `/v1/me` | implemented (S5) | → `User`; PATCH `UpdateProfileRequest{display_name}` |
| POST | `/v1/me/password` | implemented (S5) | `ChangePasswordRequest` → `AuthResponse` (this session, new token); every other session revoked; `403` wrong current password |
| GET | `/v1/sessions` | implemented (S5) | → `SessionList` (active sessions, `current` marks the caller) |
| DELETE | `/v1/sessions/{id}` | implemented (S5) | → `Empty`; `404` not the caller's |
| GET | `/v1/users/lookup?username=` | implemented (S5) | → `User`; exact, case-insensitive match; `404` none |
| GET | `/v1/blocks` | implemented (S5) | → `UserList` (users the caller blocked) |
| PUT / DELETE | `/v1/blocks/{user_id}` | implemented (S5) | → `Empty`, idempotent |
| GET | `/v1/conversations` | implemented (S5) | → `ConversationList` |
| POST | `/v1/conversations/direct` | implemented (S5) | `CreateDirectRequest{user_id}` → `Conversation` (one per pair, idempotent) |
| GET | `/v1/conversations/{id}/messages?before=&limit=` | implemented (S5) | → `MessagePage`, newest first below `before` (≤ 100) |
| POST | `/v1/conversations/{id}/messages` | implemented (S5) | `SendMessageRequest` → `201 ChatMessage`; `200` with the stored message for a repeated `client_nonce`; `403` block |
| PATCH / DELETE | `/v1/conversations/{id}/messages/{message_id}` | implemented (S5) | `EditMessageRequest` → `ChatMessage`; DELETE → tombstone; author only (`403`) |
| GET | `/v1/conversations/{id}/changes?after=&limit=` | implemented (S5) | → `MessagePage` of changes after a `change_seq` (≤ 500), tombstones included |
| POST | `/v1/conversations/{id}/read` | implemented (S5) | `MarkReadRequest` → `Receipt` (only goes up, clamped to the newest message) |
| POST | `/v1/conversations/{id}/members` | planned (S6) | groups |
| POST | `/v1/calls`, `/v1/calls/{id}/join`, `/v1/calls/{id}/leave` | planned (S1/S2/S6) | |
| GET | `/v1/update/manifest` | implemented (S7) | signed envelope `{"key_id","manifest":base64,"signature":base64}` (JSON, ≤ 64 KiB, `no-cache`); `404` none published; no authentication (ADR 0016) |
| GET | `/v1/update/files/{version}/{*path}` | implemented (S7) | release file, `application/octet-stream` with `Content-Length`; `404` for paths outside the rules (`x.y.z`, `[A-Za-z0-9._-]` segments); `503` + `Retry-After` above 8 concurrent downloads |

### Updates (S7, ADR 0016)

The manifest (format 1) is JSON: `format`, `product` (`turbo-pancake-desktop`),
`platform` (`windows-x86_64`), `version`, `published_at_ms`, `expires_at_ms`, optional
`notes`, `files[{path,size,sha256}]` (must include `turbo-pancake-desktop.exe`); unknown
fields are rejected. The Ed25519 signature covers the exact manifest bytes; clients pin
the public keys at build time, accept only newer, unexpired, not-rolled-back versions and
download files only from this server with size and SHA-256 checks. The server never
holds the signing key (`turbo-pancake-update sign` runs offline; `--update-dir` serves
its output).

### Transport security (ADR 0014)

With `--tls-cert/--tls-key` every endpoint (HTTP API and `/v1/signal`) is served over
HTTPS / WSS on the same port. Clients always verify the certificate: against the private
CA given with `--server-ca`, otherwise against the public web PKI; there is no option to
skip verification. Setup and rotation: `docs/DEPLOY-TLS.md`.

### Messenger API conventions (S5, ADR 0013)

* Bodies are protobuf (`proto/turbo_pancake/v1/api.proto`), `Content-Type:
  application/x-protobuf`, ≤ 64 KiB (`413` above). Authenticated calls send
  `Authorization: Bearer <token>`; a missing, unknown, expired or revoked token is `401`.
* Every error is an HTTP status plus `ApiError{code, message}`: `BAD_REQUEST` 400,
  `UNAUTHENTICATED` 401, `FORBIDDEN` 403, `NOT_FOUND` 404, `CONFLICT` 409,
  `PAYLOAD_TOO_LARGE` 413, `RATE_LIMITED` 429, `UNAVAILABLE` 503 (server without a
  database), `INTERNAL` 500. A conversation the caller is not in, and a malformed id, are
  `404` (nothing about its existence leaks).
* IDs are UUID strings; times are Unix milliseconds.
* Usernames: 3–32 chars of `[a-z0-9_]`, case-insensitive, start with a letter, no trailing or
  doubled `_`, reserved service names refused. Passwords: 8–128 characters. Messages: 1–4000
  characters, not blank. Device names ≤ 64 characters.
* Rate limits (per address: register, login; per user: lookup, conversation creation,
  message writes) answer `429`.
* Order and sync: `server_sequence` orders messages inside a conversation (gap-free);
  `change_seq` grows with every create/edit/delete. A client keeps the highest applied
  `change_seq` per conversation, drops events at or below it and fetches `/changes?after=`
  when an event skips ahead or after any reconnect.
* Blocks: in a direct conversation with a block in either direction, sending and editing
  are `403 "you cannot message this user"` (same text whoever blocked), presence is hidden
  both ways, deleting your own messages still works.

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

1. Client → `Hello{client_version, display_name, auth_token}` (must be first; anything
   else → `HELLO_REQUIRED`). With a token the connection is signed in (the display name
   comes from the account); a bad token → `UNAUTHENTICATED` and close. Without a token the
   display name (1–64 printable chars, no leading/trailing spaces) is required and the
   connection may only use calls — and only when the server allows anonymous calls
   (default on loopback, `--anonymous-calls`); otherwise `UNAUTHENTICATED` and close.
2. Server → `Welcome{peer_id, server_version, heartbeat_interval_ms, max_frame_bytes, user}`.
3. The client must send *some* frame (e.g. `Ping`) at least every
   `heartbeat_interval_ms`; after 30 s of silence (default) the server drops the
   connection and announces `PeerLeft{reason=TIMEOUT}`.

Messenger events on signed-in connections (`request_id = 0`, at-least-once):

| Event | When |
|---|---|
| `MessageCreated` / `MessageEdited` / `MessageDeleted{message}` | to every member of the conversation (the nonce only to the author) |
| `ReadUpdated{Receipt}` | a member's delivered/read position went up |
| `PresenceChanged{user_id, online}` | a contact's first connection opened / last one closed or timed out; also after a block change |
| `ConversationUpdated{conversation_id}` | a conversation appeared, or its members, names or block state changed: refetch it |
| `SessionRevoked{session_id}` | logout, revoke or password change elsewhere; the server closes the connection |

Presence = at least one live signed-in connection; a silent connection is dropped after the
heartbeat timeout, so a crashed client turns offline within 30 s. Server shutdown closes all
connections.

### Group conversations (S6)

All group endpoints require a valid account token. Bodies and snapshots use protobuf.

| Method and path | Request | Response |
|---|---|---|
| `POST /v1/conversations/group` | `CreateGroupRequest{title, usernames[]}` | `Conversation` |
| `PATCH /v1/conversations/{id}` | `RenameGroupRequest{title}` | `Conversation` |
| `POST /v1/conversations/{id}/members` | `AddGroupMemberRequest{username}` | `Conversation` |
| `DELETE /v1/conversations/{id}/members/{user_id}` | none | `Conversation` |

Creation includes the caller as creator and resolves 1–99 distinct other accounts by
exact case-insensitive username; invalid/missing accounts roll back the whole creation.
Titles contain 1–100 printable Unicode characters, cannot be blank, and are trimmed.
Only the creator can rename or change membership (403 for other active members, 404
for outsiders); the creator cannot be removed. These endpoints cannot manage direct
conversations. Creation and management use the per-user conversation rate limit.

The limit is 100 active members including the creator (409 for a new member when
full). Add/remove are idempotent; re-adding a removed member restores access to the
existing history and keeps receipt cursors. Removal immediately denies server history,
message writes and receipts. Membership mutations and message writes serialize on the
conversation row, with rights rechecked after acquiring the lock. Direct blocks do not
prevent shared-group messages; presence privacy continues to apply.

`ConversationUpdated` goes to affected accounts, including a removed account. Clients
refetch the authoritative conversation list and discard entries no longer listed;
reconnect performs the same refresh to repair missed invalidations. Message change
cursors remain dedicated to message changes; metadata never consumes their sequence.
`Member.membership_revision` (field 5, starts at 1) increments on re-add, so a client
can detect removal followed by re-add even when both invalidations were missed.
On a change to its own revision the client discards the previous membership's pending
operations, drafts and message cursor, closes the old composer, and reloads history.
History messages carry `author_display_name` (ChatMessage field 12), so removing an
author from the active member list does not erase the author label in old messages
or replies. SQLite migration 3 refreshes existing group cursors to fill that field;
the cached message bodies and pending operations remain intact.

### Rooms and relay

Account calls (S6, ADR 0015): `GET /v1/calls -> CallList`,
`POST /v1/calls (CreateCallRequest) -> CallParticipation`,
`POST /v1/calls/{id}/join -> CallParticipation`,
`POST /v1/calls/{id}/leave (LeaveCallRequest) -> Empty`,
`POST /v1/calls/{id}/decline -> Empty`. All require account authentication and current
conversation membership; direct create/join refuses blocks. Creating a direct call to
a participant already busy elsewhere returns 409. Accept/join switches the caller's
old participation across all devices; a retry by the owning session keeps its lease.
Only that session and participation id can leave; stale leave is a no-op.

Media attaches with `JoinCallMedia{call_id, participation_id}` on an authenticated
WebSocket. Regular `JoinRoom` is anonymous development only and cannot use `call_`
room ids. A removed/blocked/revoked/switched participant loses the hub/SFU session;
stale media offers cannot restore it. `CallStarted`, `CallParticipantJoined`,
`CallParticipantLeft`, `CallEnded` carry a full `ConversationCall` snapshot with a
monotonic revision. List snapshots repair missed events. The live call state is
ephemeral: restart ends calls, never restores media from old ids. Unattached leases
and unanswered invitations expire in 30 seconds; disconnect grace is 10 seconds.

| Client request | Success reply | Errors |
|---|---|---|
| `JoinRoom{room_id}` | `RoomJoined{room_id, peers[]}` (existing members); others get `PeerJoined`. While in another room: first `RoomLeft{old room}` (event, `request_id` 0), the old room's members get `PeerLeft{reason=SWITCHED}` and the media session ends; then `RoomJoined`. Same room again: `RoomJoined` with the current members, nobody else is told | `BAD_REQUEST` (invalid id), `ROOM_FULL` (default 100; checked before leaving, so the current call goes on) |
| `LeaveRoom{room_id}` | `RoomLeft{room_id}`; others get `PeerLeft{reason=LEFT}`. Not in that room: `RoomLeft` all the same, nothing changes (idempotent) | — |
| `Signal{room_id, to_peer_id, description \| ice_candidate}` | none; target gets `SignalRelay{from_peer_id, ...}` | `NOT_IN_ROOM`, `PEER_NOT_FOUND`, `BAD_REQUEST` (self-target, missing payload, SDP > 32 KiB, untyped SDP, candidate > 1 KiB) |
| `Ping{client_time_us}` | `Pong{client_time_us}` | — |

* An anonymous development connection is in at most one room at a time: a new join
  closes the previous room. `ALREADY_IN_ROOM` stays reserved. Authenticated S6 calls
  enforce one participation per account across devices through the lease described
  above, and cannot use this anonymous `JoinRoom` path.
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
| `TrackPublished{room_id, peer_id, ssrc, kind, rtx_ssrc}` | The member's stream arrives on this connection's media session as the RTP stream `ssrc`: `kind` `AUDIO` (0, voice; what S2 servers sent without the field), `SCREEN` (H.264 video, with its RTX stream `rtx_ssrc`) or `SCREEN_AUDIO` (the share's system audio). Sent for every member already present when the client's media session is accepted, then whenever another member's session starts. |
| `TrackUnpublished{room_id, peer_id, ssrc, kind}` | That stream ended (member left, its session was replaced or died). |

* Every other member is its own SSRC on the one audio m-line; no renegotiation. A
  member's new session gets a new SSRC (its `TrackUnpublished` for the old one comes
  first). The server answers without the RTP `mid` header extension, so streams are told
  apart by SSRC only; packets of an SSRC not announced yet are dropped by the client.

### Screen share (S3, ADR 0010)

The client's offer has three m-lines (four since S4, see below): voice (Opus), screen video (H.264, packetization
mode 1, RTX) and the share's system audio (Opus). Requests need a media session in the
joined room.

| Client request | Effect | Errors |
|---|---|---|
| `ScreenShare{room_id, active, width, height, fps}` | Starts/updates (`active`, size 1..=4096, fps 1..=240) or stops this member's share; every other member gets `ScreenShareState`. Send it again after every new `MediaOffer` (a new media session forgets the share). | `BAD_REQUEST` (validation), `NOT_IN_ROOM` (no room or no media session), `INTERNAL` (media server busy) |
| `SubscribeScreen{room_id, peer_id}` | Watch that member's share (`""` = none); replaces the previous subscription. The server forwards the share's video and system audio only to its subscribers and asks the sharer for a keyframe. | as above |

| Event | Meaning |
|---|---|
| `ScreenShareState{room_id, peer_id, active, width, height, fps}` | A member started, changed or stopped sharing. Sent for every active share when the client's media session is accepted, then on every change; a member who leaves or loses its media session stops sharing. |

* Loss repair: NACK/RTX on each leg (client ↔ server), keyframe requests (PLI/FIR) from a
  viewer reach the sharer. The sending client paces video (12 Mbit/s cap).

### Layers and previews (S4, ADR 0011)

A client that can carry previews offers a fourth m-line (second H.264 video m-line) on
which it sends the low preview layer of its share (≈ 320 px longest side, ≈ 10 fps, same
RTP timestamps as the high layer). The server announces every member's preview stream as
`TrackPublished{kind: TRACK_KIND_SCREEN_PREVIEW(3), ssrc, rtx_ssrc}` — only to sessions
with four m-lines (an S3 client would read an unknown kind as audio).

| Client request | Effect | Errors |
|---|---|---|
| `ScreenShare{…, bitrate_kbps, preview}` | As in S3, plus the high layer's target bitrate (≤ 100 000, used for the server's high-stream budget) and whether a preview layer is sent. Sent again when adaptation changes size, rate or bitrate. | as above |
| `SubscribeScreen{room_id, peer_id, layer}` | `layer`: `SCREEN_LAYER_AUTO(0)` (default: high unless the viewer's downlink loses packets or the budget is used up), `HIGH(1)`, `LOW(2)`. The same peer with another layer switches the running stream at the next keyframe of the new layer (sequence numbers continue). Unknown layer → `BAD_REQUEST`. | as above |
| `SubscribePreviews{room_id, peer_ids}` | The shares shown as previews (≤ `MAX_PREVIEWS` = 50 distinct non-empty ids), replacing the previous list; `[]` = none. Previews of members not sharing wait until they do. | `BAD_REQUEST`, `NOT_IN_ROOM`, `INTERNAL` |

| Event | Meaning |
|---|---|
| `ScreenShareState{…, bitrate_kbps, preview}` | `preview`: a preview layer can be received for this share. |
| `ScreenSubscription{room_id, peer_id, requested, layer, reason}` | spec §11 `subscription.updated`: the layer (`HIGH`/`LOW`) the watched share flows with now or after the pending switch, and why: `REQUESTED(1)`, `NETWORK(2)` (auto, ≥ 8 % loss in two receiver reports or ≥ 20 % in one; back up after 8 clean reports and a hold that doubles after a failed probe, 8–64 s), `ADMISSION(3)` (server budget `--max-high-egress-mbps` used up; earlier subscriptions keep theirs), `SINGLE_LAYER(4)` (the sharer sends no preview). Sent after every `SubscribeScreen` and on every change. |

### Error codes

`UNSUPPORTED_VERSION(1)`, `BAD_REQUEST(2)`, `PAYLOAD_TOO_LARGE(3)`, `HELLO_REQUIRED(4)`,
`ALREADY_IN_ROOM(5)`, `NOT_IN_ROOM(6)`, `PEER_NOT_FOUND(7)`, `ROOM_FULL(8)`, `INTERNAL(9)`,
`UNAUTHENTICATED(10)`.
`Error.message` is human-readable and never contains secrets or user content.

## Compatibility policy

* Field numbers are never reused; removed fields become `reserved`.
* Adding fields / oneof variants is allowed within v1; clients must ignore unknown ones.
* Breaking changes → new package `turbo_pancake.v2`, new `PROTOCOL_VERSION`, server supports
  both for one release cycle.
* Binary encodings of representative messages are pinned in contract tests.
