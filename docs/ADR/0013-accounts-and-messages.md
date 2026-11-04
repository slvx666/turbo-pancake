# ADR 0013 — Accounts, sessions, direct messages and sync

* Status: **accepted** (2026-11-04)
* Spec: §7 (messages, order, idempotency, receipts, delete for everyone, pagination,
  resync), §8 (username + password, Argon2id, sessions with revocation, presence, blocks),
  §10 (logical schema, client SQLite cache), §11 (HTTPS API, WebSocket events), §12
  (limits, rate limits, no secrets or message bodies in logs), §17 S5.

## Decision

1. **Storage.** PostgreSQL ≥ 15 through SQLx (runtime-checked queries, no compile-time
   database), migrations in `migrations/` embedded with `sqlx::migrate!` and applied at
   start. IDs are UUIDv4 (immutable, not enumerable). Without `--database-url` the server
   still runs calls (S1–S4 tests, benches) and answers the messenger API with `503`.
2. **Ordering and sync.** Each conversation row holds two counters bumped in the same
   transaction that writes a message (the row lock serialises writers):
   `message_seq` → `messages.server_sequence` (display order, gap-free over created
   messages) and `change_seq` → `messages.change_seq` (every create/edit/delete gets a new
   value). Clients keep the highest applied `change_seq` per conversation; live events
   carry it, so a duplicate (≤ applied) is dropped and a gap (> applied + 1) triggers
   `GET …/changes?after=` — the same call used after a reconnect. Deleted messages become
   tombstones (body cleared, `deleted = true`) so replies keep their reference and the
   delete reaches every cache through the same change feed.
3. **Idempotent send.** `(author_id, client_nonce)` is unique; a retried send returns the
   stored message. The client stores pending sends in SQLite and retries them after a
   reconnect or restart.
4. **Receipts.** Per member and conversation `delivered_sequence` / `read_sequence`, only
   ever raised (`GREATEST`), shared by all devices of a user; `read.updated` goes to every
   member's connections.
5. **Auth.** Argon2id (RFC 9106 second recommended set scaled down: m = 19 MiB, t = 2,
   p = 1, per-hash salt, PHC string), hashing on the blocking pool. A session token is 32
   random bytes (base64url), only its SHA-256 is stored; sessions expire after 30 days of
   no use, `refresh` rotates the token, `logout` / session revoke / password change revoke
   in the database and close the session's WebSocket connections. Unknown user and wrong
   password give the same answer and cost (a dummy hash is verified). The WebSocket
   `Hello` carries the token (field 3, reserved for it since S0).
6. **Usernames.** 3–32 chars `[a-z0-9_]`, starting with a letter, case-insensitive (key is
   lowercase, display keeps the typed case); ASCII only, which rules out look-alike
   letters from other scripts; reserved service names are refused. Display names are
   free Unicode (1–64 printable chars).
7. **Blocks.** Either side of a block cannot send new direct messages in their direct
   conversation (`FORBIDDEN`, the same message whoever blocked) or start a direct call
   (S6 enforces it at call start); presence is hidden both ways. Existing history stays.
8. **Presence** is in memory: a user is online while at least one authenticated
   WebSocket is alive (heartbeat timeout 30 s), so a crash or a cut cable turns into
   offline within the timeout; a server restart starts everyone offline.
9. **API shape.** HTTP `/v1/...` with protobuf bodies (`application/x-protobuf`, types in
   `proto/turbo_pancake/v1/api.proto`), errors as `ApiError{code, message}` with a matching
   HTTP status; events are new `ServerEnvelope` variants on `/v1/signal`.
10. **Rate limits** in memory (token buckets per IP and per user): register, login,
    lookup, conversation creation, message writes. Security events go to the `security`
    log target with ids only.
11. **Client cache.** SQLite (rusqlite, bundled) per server and user: conversations,
    messages, drafts, read cursors, pending operations, settings; schema version in
    `user_version`; a cache that fails to open or migrate is moved aside and rebuilt from
    the server. The token lives in the Windows Credential Manager, not in SQLite.
12. **TLS.** S5 runs plain HTTP/WS on loopback for tests and trials. TLS with an
    IP-address certificate or a pinned server key (spec §2, spike 5) is implemented as the
    last S5 step; until then the server refuses to bind a non-loopback address with
    accounts enabled unless `--insecure-allow-plaintext` is given (for LAN trials only).

## Consequences

* Two counters per conversation instead of one global sequence: no cross-conversation
  ordering (not needed), no hot global row.
* At-least-once events plus the change feed mean the client never needs a full re-download
  after a reconnect; it does need a full snapshot when its cache is rebuilt.
* Presence and rate limits do not survive a restart and do not span several server
  processes (one server in the spec, §2).
