# S8 step 1 — messenger sessions and presence under load (Windows dev machine, loopback)

Date: 2026-11-06. Tool: `turbo-pancake-bench messenger` (`benchmarks/harness/src/messenger.rs`).
Builds: release server and bench from `86fbb7a` plus this step's changes (the commit that
adds the scenario; the JSON `git_commit` field shows the base). Raw reports:
`messenger-500u-windows-1793986102.json`, `messenger-1000u-windows-1793986170.json`.

## What runs

* The release server as its own process on `127.0.0.1:0` with `--no-media-udp` and
  `--load-test-limits` (rate limits off; the server refuses the flag on a non-loopback
  bind — checked: `--bind 0.0.0.0:0 --load-test-limits` exits 1 with that message).
* Real PostgreSQL: the throw-away cluster of `crates/pg-test` (**`fsync=off`,
  `synchronous_commit=off`** — database writes are cheaper than on a durable server).
  `--database-url` points the same run at a real database (target-server run).
* N accounts through the real `POST /v1/auth/register` (8 in parallel, Argon2id on the
  server), groups of 100 created by their first member through
  `POST /v1/conversations/group`, then N signed-in WebSockets (`Hello.auth_token` →
  `Welcome` with the user) sending the client heartbeat every 10 s.
* Phases: presence churn (20 members disconnect, then reconnect: every online contact must
  get `PresenceChanged`), 20 messages to a 100-member group (all 99 other members must get
  `MessageCreated`), 30 s hold, 10 "silent" clients (stop sending, socket left open — a
  frozen or crashed client), then an HTTP snapshot (`GET /v1/conversations`) of every live
  member compared with the truth.
* Loopback: the numbers are the server's own cost on this machine, not network latency.
  Bench and server share the machine.

## Results

| Metric | 500 users (5 × 100) | 1000 users (10 × 100) |
|---|---|---|
| Overall | **PASS** | **PASS** |
| Register (Argon2id) p50 / p95 / p99 | 30.7 / 39.5 / 72.8 ms; all in 2.0 s | 30.6 / 38.6 / 46.9 ms; 4.0 s |
| Create group of 100 p50 / max | 16.6 / 22.5 ms | 15.5 / 22.3 ms |
| Connect + authenticated `Welcome` p50 / p95 / p99 | 5.5 / 9.9 / 49.1 ms; all in 0.25 s | 6.1 / 10.1 / 11.9 ms; 0.50 s |
| Presence offline → last of 99 contacts p50 / p95 | 1.23 / 1.47 ms | 1.22 / 1.51 ms |
| Presence online → last of 99 contacts p50 / p95 | 1.79 / 2.14 ms | 1.85 / 2.27 ms |
| Presence deliveries missed | 0 of 3960 | 0 of 3960 |
| Group message: HTTP send p50 | 4.3 ms | 4.3 ms |
| Group message → last of 99 receivers p50 / p99 | 4.5 / 6.2 ms | 4.4 / 8.2 ms |
| Group message deliveries missed | 0 of 1980 | 0 of 1980 |
| Hold 30 s: server CPU | 0.8 % of one core | 2.8 % of one core |
| Hold: server working set / private | 30.3 / 22.9 MiB | 45.3 / 38.4 MiB |
| Working set per signed-in session | ≈ 33 KiB | ≈ 31 KiB |
| Hold: ping RTT p50 / max | 0.22 / 0.28 ms | 0.23 / 0.49 ms |
| Connections dropped during hold | 0 | 0 |
| Silent clients marked offline at every contact | 978 of 978 after 27.3–27.5 s | 988 of 988 after 27.0–27.5 s |
| Silent sockets closed by the server | 10 of 10 | 10 of 10 |
| HTTP snapshot presence wrong | 0 of 49 000 | 0 of 99 000 |
| Server working set start / all connected / after all left | 11.7 / 29.3 / 19.8 MiB | 11.7 / 44.0 / 23.6 MiB |

Silent detection: the server drops a connection that sent nothing for its heartbeat
timeout (30 s); the clock in the table starts when the clients stopped, a few seconds
after their last heartbeat, so ≈ 27 s is the expected value. No stale "online" remained:
every contact got the offline event and the HTTP snapshots agree.

## Findings

* **Argon2id concurrency is unbounded** (S8 step 4): every register/login/password check
  runs on Tokio's blocking pool (up to 512 threads) with 19 MiB per hash, so a burst of
  parallel logins can take ≈ 9.5 GiB. The bench uses 8 parallel registrations; a
  semaphore on the server is step 4 work (PROJECT_STATE issue 45).
* Memory returns after the sessions leave (working set 29.3 → 19.8 MiB for 500); the
  remainder is allocator and database-pool state, not per-session leaks (1000 users:
  23.6 MiB after leaving).

## Not covered here (owner-assisted / later steps)

* The same tool on the owner's target server against its real PostgreSQL
  (`--database-url`, bench on the server host: the tool starts the server on loopback).
  A run from another machine over the real network needs a mode against an already
  running server with its own limits (not built yet).
* Group writes under concurrent senders and membership changes, reconnect storm (step 2).
