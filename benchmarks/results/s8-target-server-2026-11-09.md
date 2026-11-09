# S8 on the target server (2026-11-09)

The owner's Linux target server (Debian/Ubuntu family, systemd, the distribution's
PostgreSQL; hardware and network not described here). Tool:
`deploy/linux/target-bench.sh` (docs/ADMIN.md §9), run over SSH by the developer with the
owner's permission. Load runs start their own server on 127.0.0.1 with a scratch
database: they measure the host's CPU/memory, **not** the network or the uplink.

## Run 1 — release 0.1.2 server (768be4b): defects found

| Finding | Evidence | Fix (874c6eb) |
|---|---|---|
| 1000 sessions hung for 30 min | bench and its server at 1011 / 1024 open files (login shell limit) | `target-bench.sh` raises `ulimit -n`, deadline per run |
| 50 × 49 preview grid: **voice 72 %** (`pass: false`), reproduced 3× | `UdpRcvbufErrors` +65 000 per run, send errors 0; the 4 MiB socket buffers are capped at `net.core.rmem_max` (208 KiB) | `install-server.sh` sets `rmem_max`/`wmem_max` = 8 MiB (sysctl.d); the server warns when the OS grants less |
| Server memory ≈ 800 MiB after registrations; login burst **+440 MiB** (Windows: +87) | `MALLOC_ARENA_MAX=2`: +857 MiB; `MALLOC_MMAP_THRESHOLD_=1048576`: +79 MiB | unit sets `MALLOC_MMAP_THRESHOLD_=1048576` (glibc's dynamic threshold kept freed 19 MiB Argon2 blocks in thread heaps) |
| `target-bench.sh` printed PASS for the failed grid run | bench exit 0 with `"pass": false` in the report | the report verdict is checked |

## Run 2 — fixed server (874c6eb): all PASS

| Step | Scenario | Result |
|---|---|---|
| 1 | 500 sessions, 5 groups of 100 | 0 missed presence (churn) / 0 of 1980 fan-out; fan-out p99 9.8 ms; 0 of 49 000 stale presences; hold 0.5 % of a core, server WS 22.9 MiB |
| 1 | 1000 sessions | 0 missed; p99 7.8 ms; 0 of 99 000 stale; 0.8 % of a core, WS 36.0 MiB |
| 2 | 2 × 100 under writes, removals, storm | 723 sends/s, p50/p99 17 / 59 ms; 0 missed, gaps, duplicates; storm: 100 offline messages, 0 missing, reconnect all 0.05 s |
| 2 | 5 × 100 | 967 sends/s, p50/p99 52 / 97 ms (pool of 16 saturated, no loss); 0 missed/gaps/dups; storm 250 offline, 0 missing |
| 3 | 50 publishers + 2 viewers | voice 100 %, main 99.9 %, previews 99.8 %; 26.7 % of a core |
| 3 | grid 50 × 49 previews | **voice 99.99 %**, previews 99.8 %; 403 Mbit/s; 76 % of a core; 0 kernel drops |
| 3 | 1 → 50 high viewers (2 Mbit/s) | voice 100 %, main 99.7 %; 116 Mbit/s; 20 % |
| 3 | 1 → 50 at 8 Mbit/s 60 fps | 436 Mbit/s, main 99.8 %; 44 % |
| 3 | 1 → 50 with a 40 Mbit/s budget | exactly 20 high, 30 preview with `ADMISSION`; voice 100 %; 57 Mbit/s |
| 3 | 50 voice members, 1 / 10 / 50 talkers | 5.8 / 23 / 94 % of a core, 100 % delivered, 163 Mbit/s at 50 |
| 4 | 200 parallel logins | 69 ok, 131 × 503 (queue 64), no other status; **+80 MiB** peak |
| 4 | unit and restart | `KillSignal=15`, `TimeoutStopSec=20s`, `LimitNOFILE=65536`, `TasksMax=1024`, `MemoryHigh/Max` 1.5 / 2 GiB, no start limit; restart: graceful stop on SIGTERM, ready again after 155 ms |
| 5 | backup + restore drill | dump in < 1 s, restored into a scratch database, `check-db` ok; `check-db` on the live database: 0 violations; timer scheduled |
| r | real reboot | service active ~2 min after boot, `/readyz` 200 (also from outside with the private CA), backup timer active, every other enabled service of the host back |

Note on the reboot: the host's `systemd-networkd-wait-online` times out after 2 min on
every boot (host configuration, present before this work), which delays everything
ordered after `network-online.target`, this server included. Fixing it is the host
owner's choice (ADMIN.md §2).

## Not covered

* Real uplink and remote clients (loopback only); target-server CPU limit for 50
  simultaneous talkers is ~94 % of one SFU thread.
* Load from a real client population (only synthetic bench clients).
