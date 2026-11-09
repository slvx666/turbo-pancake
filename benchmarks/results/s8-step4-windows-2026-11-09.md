# S8 step 4 — hardening (Windows dev machine)

Date: 2026-11-09. Server changes in `apps/app-server/src/{hashing,limits,http,api,lib,main}.rs`,
`crates/pg-test` (`stop` / `restart`), `deploy/linux/install-server.sh` (unit limits).
Tests: `apps/app-server/tests/{hardening,db_outage}.rs`, `hashing::tests`. Bench:
`turbo-pancake-bench login-burst`. Raw reports:
`login-burst-200x-c4-q64-windows-1794230855.json`,
`login-burst-200x-c4-q1000-windows-1794230859.json`,
`login-burst-200x-c256-q1000-windows-1794230862.json`.

## What changed and how it is checked

| Area | Before | Now | Evidence |
|---|---|---|---|
| Password hashing (issue 45) | one Argon2id (19 MiB) per request on the blocking pool, up to 512 at once; a failed blocking task answered "wrong password" | at most `--hash-concurrency` (default half the CPUs, 1..=4) at once, `--hash-queue` (64) may wait ≤ 10 s, beyond: `503 server busy` + `Retry-After`; never `401` for a correct password because of load | `hashing::tests` (bound, refusal count, waiter timeout); `password_hash_burst_is_bounded_and_refused_with_503_not_wrong_password` (40 parallel logins, 2 slots + 4 queue: peak 2, every refusal 503 + Retry-After); `login-burst` below |
| Database pool exhausted | `500 internal error` after 5 s | `503 database busy` after `database_acquire_timeout` (3 s) + `Retry-After` | `pool_exhaustion_and_slow_requests_answer_503_then_recover` (pool 1 held by a request blocked on a row lock: another request 503 in < 2 s; readiness 503; liveness 200; recovery after the lock goes) |
| Slow handler | no limit | `request_timeout` (30 s) → `503 request timed out`; the dropped transaction rolls back | same test: the blocked send answers 503 after 1 s, its message is not stored, the same nonce sends normally afterwards |
| Database outage | `500` | `503 database unavailable` + `Retry-After`; login `503` (not `401`); `/readyz` 503 `database unavailable`; `/healthz` 200; no crash; recovers by itself | `db_outage.rs`: PostgreSQL stopped (`pg_ctl stop -m fast`) and restarted on the same port while the server runs |
| Disk full / out of resources | `500` | SQLSTATE class 53 (53100 disk_full, 53200, 53300, 53400) and 57P/08 → `503` with a clear message | `insufficient_resource_errors_map_to_unavailable`: a real 53400 (temp_file_limit) from PostgreSQL maps to `UNAVAILABLE`, a syntax error stays `INTERNAL`. A really full disk was not produced (owner-assisted on the target server) |
| Readiness | WebSocket limit only | + database `SELECT 1` within 2 s | tests above |
| Connection / fd limit | unbounded HTTP connections | `--max-tcp-connections` (4096): `accept` waits for a free slot, further clients wait in the OS backlog | `tcp_connection_cap_makes_extra_clients_wait` (cap 2: a third client waits, served once a slot frees) |
| Slow request headers (slowloris) | **a half-sent request stayed open > 45 s** (axum::serve sets no hyper timer) | own HTTP/1.1 loop (`http.rs`, axum's plus a timer): headers must be complete within `header_read_timeout` (10 s), WebSocket upgrades unchanged | `slow_request_headers_are_cut_off` (500 ms in the test); release binary probe: closed after **10 s**; all WebSocket, TLS (HTTPS + WSS, `messenger-client/tests/tls.rs`) and messenger tests green |
| Graceful shutdown | Ctrl+C only — **systemd's SIGTERM killed the server without a graceful stop**; no bound on waiting | SIGTERM handled on Unix; stop accepting, in-flight requests finish within `shutdown_grace` (10 s), then the process exits; pool closed | `graceful_shutdown_finishes_in_flight_requests` (a request in flight at the stop is answered 201, no new connections meanwhile), `shutdown_grace_bounds_a_stuck_request` (serve returns within the grace) |
| systemd unit | `LimitNOFILE=65536`, `Restart=always` | + `KillSignal=SIGTERM`, `TimeoutStopSec=20`, `TasksMax=1024`, `MemoryHigh=1536M`, `MemoryMax=2G` | text review only (Linux host: owner-assisted) |

New operator flags: `--database-pool` (16), `--hash-concurrency`, `--hash-queue`,
`--max-tcp-connections`. `/v1/server-stats` shows the hashing counters.
Dependencies: `hyper` / `hyper-util` / `tower-service` became direct dependencies of the
server (all already in the graph through axum; `Cargo.lock` +3 dependency lines, no new
crate, no version change; hyper's `http2` is not enabled — it would add `h2`).

## Login burst: memory with and without the bound (release, loopback, 200 parallel logins)

| Server setting | OK | 503 | Other | OK latency p50 / p99 | Hashes at once (peak) | Server private bytes above idle (peak) |
|---|---|---|---|---|---|---|
| `--hash-concurrency 4 --hash-queue 64` (default-like) | 83 | 117 | 0 | 226 / 423 ms | 4 | **+87 MiB** |
| `--hash-concurrency 4 --hash-queue 1000` | 200 | 0 | 0 | 516 / 980 ms | 4 | **+89 MiB** |
| `--hash-concurrency 256 --hash-queue 1000` (≈ old behaviour) | 200 | 0 | 0 | 419 / 679 ms | 184 | **+874 MiB** |

The bound keeps the memory of a burst at ≈ concurrency × 19 MiB whatever the burst size;
without it memory grows with the burst (≈ 4.7 MiB per login here, 184 at once). The
default queue sheds the excess at once instead of making 200 clients wait ~1 s; clients
retry after `Retry-After` (the per-address login rate limit applies first in production;
the bench runs with load-test limits).

## Not covered here (UNVERIFIED / owner-assisted)

* SIGTERM from systemd and the unit limits on the Linux server (code path is `cfg(unix)`,
  not compiled on this Windows machine; CI is Windows-only until S9).
* A real disk-full on the database volume (mapping verified with a class-53 error).
* The desktop client's handling of `503` + `Retry-After` beyond its existing retry /
  reconnect logic was not re-tested in this step.
