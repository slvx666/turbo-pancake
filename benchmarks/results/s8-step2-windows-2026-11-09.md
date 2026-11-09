# S8 step 2 — groups of 100 under load (Windows dev machine, loopback)

Date: 2026-11-09. Tool: `turbo-pancake-bench group-load`
(`benchmarks/harness/src/group_load.rs`). Builds: release server and bench from `6c8d3e2`
plus this step's changes (the JSON `git_commit` field shows the base). Raw reports:
`group-load-2x100-windows-1794228692.json`, `group-load-5x100-windows-1794228706.json`;
step 1 re-run with the corrected CPU probe: `messenger-500u-windows-1794228777.json`.

## What runs

* The release server as its own process on `127.0.0.1:0`, `--no-media-udp`,
  `--load-test-limits` (rate limits off, loopback only), real PostgreSQL from
  `crates/pg-test` (**`fsync=off`, `synchronous_commit=off`**: writes are cheaper than on a
  durable server; `--database-url` runs the same against a real database).
* Accounts through the register API (8 in parallel), groups of 100 (member 0 creates),
  one signed-in WebSocket per account with the client heartbeat.
* **Load**, every group at once: 20 members post 50 messages each over HTTP with 20 ms
  between their sends; every tenth send is issued twice concurrently with the same
  `client_nonce`. At the same time the creator removes 10 other members and re-adds them,
  5 rounds (100 ms out per round). While removed, each member tries `POST …/messages` and
  `GET …/messages`.
* **Live check**: each connection's `MessageCreated` events against the membership
  windows. A member must get every message committed while it was a member and none
  committed while it was out. Each change is bracketed by sequences the bench saw: `lo` =
  highest sequence of sends that had *completed* before the change was requested, `hi` =
  `last_sequence` in the change's response. Messages inside a bracket may go either way
  ("boundary"); outside, the rule is strict.
* **Reconnect storm**: all WebSockets close at once; 50 messages per group are posted
  while everybody is offline; all accounts reconnect at once and resync with
  `GET /changes?after=<newest change_seq the connection saw>`; then one more message per
  group must reach every member live.
* **History**: each group's full history paged with `GET /messages`: sequences 1..=N, no
  gaps or duplicates, N = distinct accepted sends, every accepted send at the sequence
  (and message id) it was given, every sender's sequences increasing.
* DB pool: `GET /v1/server-stats` (new, aggregate operator metrics) sampled every 25 ms.
* Loopback, bench and server on one machine (20 logical CPUs): the numbers are the
  server's own cost here, not network latency.

## Results

| Metric | 2 groups × 100 (200 users) | 5 groups × 100 (500 users) |
|---|---|---|
| Overall | **PASS** | **PASS** |
| Accepted sends / load phase | 2000 in 2.25 s (890/s) | 5000 in 4.39 s (1139/s) |
| Same-nonce concurrent pairs, one stored message | 200 of 200 | 500 of 500 |
| Per-sender order violations | 0 | 0 |
| HTTP send p50 / p95 / p99 | 11.2 / 27.5 / 50.5 ms | 39.2 / 54.2 / 70.2 ms |
| Live delivery (send start → member's event) p50 / p95 / p99 | 11.2 / 26.6 / 52.6 ms (188 365 events) | 40.1 / 53.4 / 71.0 ms (470 333 events) |
| Missed live deliveries (member at commit time) | 0 | 0 |
| Delivered while removed | 0 | 0 |
| Boundary messages delivered (either way allowed) | 651 of 1126 | 1632 of 2757 |
| Duplicate events / events of a foreign conversation | 0 / 0 | 0 / 0 |
| Removals + re-adds | 100 | 250 |
| Remove p50 / p99 | 15.7 / 58.0 ms | 8.1 / 126.9 ms |
| Re-add p50 / p99 | 5.7 / 43.4 ms | 6.5 / 114.9 ms |
| Removed member's send / history refused (404) | 200 of 200 | 500 of 500 |
| `membership_revision` grows on every re-add | yes | yes |
| Server CPU during load | 7.0 cores | 10.2 cores |
| DB pool (16) in use during load p50; samples at 16 | 15; 40 of 90 | 16; 124 of 175 |
| Storm: messages posted while offline | 100 | 250 |
| Storm: all reconnected in / connect p99 | 0.085 s / 27.3 ms | 0.22 s / 33.1 ms |
| Storm: all resynced in / per client p99 | 0.028 s / 11.2 ms | 0.061 s / 9.7 ms |
| Storm: missing after resync | 0 | 0 |
| After the storm: live message to every member p99 | 1.9 ms, 0 missed | 2.2 ms, 0 missed |
| History: messages = accepted, gaps, duplicates, mismatches | 2102 = 2102, 0, 0, 0 | 5255 = 5255, 0, 0, 0 |
| Server working set before / after load | 21.1 / 25.8 MiB | 30.8 / 40.6 MiB |

A light run (1 group of 100, 5 senders, 50 ms between sends: 78 messages/s) used 0.61–0.68
of a core; the same with 10 members 0.12 of a core. Delivering one event costs the server
≈ 75 µs of CPU here, ≈ 8 ms per message to 100 members.

## Findings

* **Bench CPU probe was wrong on Windows** (fixed): sysinfo's accumulated process time is
  derived from sampled usage there; the same light run read 0.5 %–83 % of a core. The
  bench now reads `GetProcessTimes` (user + kernel); repeats of the light run agree
  within ≈ 10 % (0.61 / 0.68 and 0.125 / 0.115 of a core). Step 1 re-run with the fixed probe: 500 sessions, 30 s hold **0.72 %** of a
  core (step 1 reported 0.8 %; conclusion unchanged), PASS.
* **The DB pool (16) is the first limit for write bursts.** At ~900–1100 messages/s into
  100-member groups the pool is full most of the time and send latency grows with the
  queue (p50 11 → 39 ms from 2 to 5 groups); nothing is lost or reordered. The burst is far
  above a 500-user chat load (step 1 hold: 0.7 % of a core); pool size and the cost per
  message (≈ 9 statements incl. two member-list reads) are inputs for step 4 limits and
  the target-server run.
* **Each event is a separate WebSocket write + flush** (`signaling.rs` writer). This is
  most of the ≈ 75 µs per delivery on Windows loopback; batching queued frames before one
  flush would cut it under bursts. Not changed in this step (no failure; measured first on
  the target Linux server, owner-assisted).
* Removed members get `404` (no access at all, API.md), not `403`; the earlier next-task
  note said 403. The bench checks the documented 404.

## Not covered here (UNVERIFIED / owner-assisted)

* The run on the target server with its real, durable database (`--database-url`).
* Network effects: loopback only.
