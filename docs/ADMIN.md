# Administration guide (S8 step 7)

How to build a release, install and run the server, publish client updates, keep
backups, watch the service and know its limits. Windows is the only client platform
until S9 (ADR 0012); the server runs on Debian/Ubuntu with systemd. Never put addresses,
keys or passwords of a real installation into the repository.

Related documents: `docs/DEPLOY-TLS.md` (certificates), `docs/BACKUP.md` (backups and the
retention cycle of deleted messages), ADR 0014 (TLS), 0016 (signed updates), 0017
(installer), `docs/API.md` (endpoints), `docs/TESTING.md` (test and load commands).

## 1. Release build (Windows build machine)

```powershell
powershell -NoProfile -File deploy/release.ps1 -Version 0.1.2 `
    -DefaultServer wss://<IP>:7878/v1/signal -DefaultServerCa <path>\ca.pem `
    -UpdatePubkeys "<id>:<base64>" -UpdateKey <path>\release.pk8 -UpdateKeyId <id> `
    -Notes "what changed"
```

All options are optional; without them you get a client that asks for no particular
server and has updates switched off. The script

* refuses uncommitted changes in tracked files (`-AllowDirty` builds them and records
  `"dirty": true`), takes the build inputs only from its parameters (stray
  `TURBO_PANCAKE_*` variables of the shell are cleared for the build);
* builds launcher, client and signing tool with the version baked in
  (`TURBO_PANCAKE_BUILD_VERSION`), packs them (`turbo-pancake-update pack`), builds the
  per-user installer around the package and checks it is ≤ 30 MB;
* makes the server bundle: `server/install-server.sh` and `server/src.tar.gz`
  (`git archive` of the same commit; the server is compiled on the host);
* with `-UpdateKey`, signs the client as an update into `update/` and verifies the
  manifest with the pinned keys (the key id must be among `-UpdatePubkeys`);
* writes `release.json` (version, commit, rustc, what was built in, sizes — no
  addresses) and `SHA256SUMS`, and checks them.

Output (default `t/dist/turbo-pancake-<version>`; an existing non-empty directory is
refused):

```text
windows/turbo-pancake-setup-<v>.exe        give this to users
windows/client/turbo-pancake.exe           package contents; the client alone is the update
windows/client/turbo-pancake-desktop.exe
tools/turbo-pancake-update.exe             signing tool: stays on the owner's machine
server/install-server.sh, server/src.tar.gz
update/manifest.json, update/files/<v>/…   with -UpdateKey only
release.json, SHA256SUMS
```

Check a copied release anywhere:

```powershell
powershell -NoProfile -File deploy/release.ps1 -Verify -Out <release dir>
```

```bash
cd <release dir> && sha256sum -c SHA256SUMS
```

Version numbers: the client version is `-Version`; the server reports the workspace
version (`[workspace.package] version` in `Cargo.toml`, `/v1/version`). Bump the
workspace version when the server changes, so both match.

Owner tasks (not automated, see S7): Authenticode signing of the installer and the two
executables with the owner's certificate (sign `windows/client/*.exe` before `sign` /
packing and the installer afterwards, then run the script's checks again); the Slint
license choice before public distribution (ADR 0002); creating and keeping the update key
(section 5).

## 2. Server installation and upgrade

Requirements: Debian/Ubuntu with systemd, root, TCP **and** UDP 7878 reachable (forward
both on the router behind NAT; without UDP, media falls back to the WebSocket with more
latency). PostgreSQL and Rust are installed by the script; a release build needs ~2 GB
of memory (the script adds swap on small hosts).

1. Certificates once (DEPLOY-TLS §1): `gen-cert` → copy `server.pem`, `server.key` and
   the public `ca.pem` into `server/tls/` next to `install-server.sh` (never `ca.key`).
2. Copy the release's `server/` directory to the host, check `sha256sum -c` (or compare
   the two lines of `SHA256SUMS`), then:

   ```bash
   sudo bash install-server.sh <public IP>
   ```

   It installs packages, builds `turbo-pancake-app-server` from `src.tar.gz`, creates the
   `turbo-pancake` system account, database role and database (password in
   `/etc/turbo-pancake/db-password`), `/etc/turbo-pancake/server.env`, the service unit,
   the backup timer and the helper scripts in `/usr/local/lib/turbo-pancake/`, opens the
   port only when ufw is already active, and starts the service.
   It also raises the kernel's UDP buffer maxima (`/etc/sysctl.d/90-turbo-pancake.conf`,
   `rmem_max`/`wmem_max` = 8 MiB; with the usual 208 KiB the kernel dropped voice under
   load on the target server) and runs the service with `MALLOC_MMAP_THRESHOLD_=1048576`
   (without it glibc kept ~440 MiB of freed password-hashing memory). If the journal says
   "UDP media buffers capped by the OS", check the sysctl file.
3. Check: `systemctl status turbo-pancake`, `journalctl -u turbo-pancake -n 50`, and from
   a client machine `curl --cacert ca.pem https://<IP>:7878/v1/version`.

Boot time: the unit is ordered after `network-online.target`. If the host's
`systemd-networkd-wait-online` times out (an interface that never comes up), every such
service starts only after its 2-minute timeout; fix the host's network configuration
(e.g. `--any` in an override of that unit) if that matters.

**Upgrade:** run the same command with the new release's `server/` directory. The
database (migrations run at start), certificates, passwords and `server.local.env` are
kept; the service restarts gracefully (in-flight requests finish within 10 s; live calls
end and are restarted by the users, ADR 0015).

**Rollback of the server:** take a backup first (`systemctl start
turbo-pancake-backup.service`), then run `install-server.sh` with the previous release's
`server/` directory. Migrations are additive (new columns/tables only), so an older
server works on a newer schema unless the release notes say otherwise; if they do,
restore the backup taken before the upgrade (BACKUP.md, "Recovery").

## 3. Configuration

The service reads `/etc/turbo-pancake/server.env` (written by the installer, rewritten on
every run) and then `/etc/turbo-pancake/server.local.env` (yours, never rewritten; its
values win). After a change: `systemctl restart turbo-pancake`. Every flag has an
environment variable; `turbo-pancake-app-server --help` lists them.

| Variable (flag) | Default | Notes |
|---|---|---|
| `TURBO_PANCAKE_BIND` (`--bind`) | `127.0.0.1:7878` | installer: `0.0.0.0:7878` |
| `TURBO_PANCAKE_PUBLIC_IP` (`--public-ip`) | — | announced for UDP media; installer sets it |
| `TURBO_PANCAKE_MEDIA_PORT` / `_NO_MEDIA_UDP` | listen port / off | UDP media port; WebSocket-only media |
| `TURBO_PANCAKE_TLS_CERT` / `_TLS_KEY` | — | required off loopback for accounts (ADR 0014) |
| `TURBO_PANCAKE_DATABASE_URL` | — | without it only anonymous calls (dev) |
| `TURBO_PANCAKE_DATABASE_POOL` | 16 | keep below PostgreSQL `max_connections` (100 by default) minus backups/admin |
| `TURBO_PANCAKE_HASH_CONCURRENCY` | half the CPUs, 1..=4 | Argon2id hashes at once, 19 MiB each |
| `TURBO_PANCAKE_HASH_QUEUE` | 64 | logins waiting for a hash slot (≤ 10 s); beyond: `503` + `Retry-After` |
| `TURBO_PANCAKE_MAX_TCP_CONNECTIONS` | 4096 | open TCP/TLS connections; the unit allows 65536 descriptors |
| `TURBO_PANCAKE_MAX_HIGH_EGRESS_MBPS` | unlimited | budget for watched full-quality shares (spec §5.3); set it below the measured uplink minus voice and previews |
| `TURBO_PANCAKE_UPDATE_DIR` | installer: `/var/lib/turbo-pancake/update` | signed client updates (section 5) |
| `TURBO_PANCAKE_ANONYMOUS_CALLS` | only on loopback | keep off on a public server |
| `TURBO_PANCAKE_LOG_JSON` | installer: true | JSON lines for journald |
| `TURBO_PANCAKE_INSECURE_ALLOW_PLAINTEXT` | off | LAN trials only; never on a public address |
| `TURBO_PANCAKE_LOAD_TEST_LIMITS` | off | refused off loopback; load tools only |

Fixed limits (code): API request bodies up to `MAX_API_BODY_BYTES` (`413` beyond) and
bounded WebSocket frames (`docs/API.md`), request header read timeout 10 s, handler timeout 30 s (`503 request timed out`), database
acquire timeout 3 s and SQLSTATE 53/57P/08 → `503` + `Retry-After`, start-up wait for
the database 120 s, rate limits for registration and login (per address) and for
username lookup, conversation creation and message writes (per account). The unit sets `KillSignal=SIGTERM`,
`TimeoutStopSec=20`, `LimitNOFILE=65536`, `TasksMax=1024`, `MemoryHigh=1536M`,
`MemoryMax=2G`, `Restart=always` without a start limit.

## 4. TLS rotation

DEPLOY-TLS §4: on a trusted machine with `ca.key`, `renew-cert --dir tls/ --name <IP>`,
copy the new `server.pem`/`server.key` to `/etc/turbo-pancake/tls/` (mode 644 / 640,
group `turbo-pancake`) — or into `server/tls/` and re-run the installer — then
`systemctl restart turbo-pancake`. Clients keep their `ca.pem`. A leaked `ca.key` means a
new CA and a new client build (`-DefaultServerCa`) for everyone. The server
certificate lasts 825 days by default; note its expiry (`openssl x509 -enddate -noout
-in /etc/turbo-pancake/tls/server.pem`).

## 5. Publishing a signed update (ADR 0016)

Once, on the owner's machine (offline storage for the key, never on the server or in
the repository):

```powershell
turbo-pancake-update keygen --id <id> --out <safe place>\release.pk8
# prints TURBO_PANCAKE_UPDATE_PUBKEYS=<id>:<base64> — pass it as -UpdatePubkeys
```

Clients only trust keys built into them, so the **first** release with updates must be
installed with the installer; later ones arrive as updates. Per release:

1. `deploy/release.ps1 -Version <new> -UpdatePubkeys <list> -UpdateKey <key> -UpdateKeyId <id> …`
   (the version must be greater than the users' version).
2. Copy the release's `update/` directory to the server and publish it:

   ```bash
   sudo /usr/local/lib/turbo-pancake/publish-update.sh <copied update dir>
   ```

   Files are placed first, the manifest last with an atomic rename; this and the
   previous version's files are kept. No restart needed: the server reads the directory
   per request (`GET /v1/update/manifest`).
3. Clients see the update in their settings, download it over HTTPS with size and SHA-256
   checks, and switch on restart; a version that does not start is rolled back and never
   offered again.

A manifest expires (default 90 days, `-UpdateExpiresDays`): re-sign and re-publish the
current release before that, or clients stop seeing it (protection against an old
manifest being replayed). Key rotation: build a release with both the old and the new
id in `-UpdatePubkeys`, sign it with the old key, let users update, then sign with the
new key only. A lost key: users must reinstall with the installer of a build that pins a
new key.

## 6. Backups

See `docs/BACKUP.md`: daily `pg_dump` at 03:30 by `turbo-pancake-backup.timer`, 14 days
(at least 3 dumps kept), restore drill with `restore.sh` + `check-db`, recovery, and the
**retention cycle of deleted messages: gone from the server's own backups at most 15
days after deletion**. Off-site copies are the administrator's choice and must follow
the same 14 days. Keep `/etc/turbo-pancake` (certificate, key, passwords) in a separate,
encrypted place.

## 7. Monitoring

| What | How |
|---|---|
| Liveness | `GET /healthz` (200 while the process serves) |
| Readiness | `GET /readyz` (200 when the database answers `SELECT 1` within 2 s; 503 otherwise) |
| Operator metrics | `GET /v1/server-stats` (WebSocket connections, database pool max/size/idle/in use), `GET /v1/media-stats` (SFU sessions, packets, send errors); aggregate numbers only, no user data |
| Version | `GET /v1/version` |
| Logs | `journalctl -u turbo-pancake` (JSON lines; never message text, passwords, tokens or media) |
| Backups | `systemctl list-timers turbo-pancake-backup.timer`, `journalctl -u turbo-pancake-backup` |
| Database consistency | `check-db` (BACKUP.md) |

A probe from the server itself, with verified TLS (the public `ca.pem` in
`/etc/turbo-pancake/tls/`):

```bash
curl --cacert /etc/turbo-pancake/tls/ca.pem --connect-to <IP>:7878:127.0.0.1:7878 https://<IP>:7878/readyz
```

Worth alerting on: `/readyz` not 200 for more than a minute; the service restarting
repeatedly (`systemctl show turbo-pancake -p NRestarts`); database pool `in_use` equal to
`max` for long periods; free disk below twice the newest dump (the backup then fails with
exit 2); certificate expiry within 30 days; no backup newer than 2 days.

## 8. Limits and capacity

Measured on the Windows development machine over loopback (server's own cost, not the
network). The target-server run (`benchmarks/results/s8-target-server-2026-11-09.md`)
gave the same verdicts on Linux: 500 sessions 0.5 % of a core / 22.9 MiB, 1000 sessions
36 MiB, 5 × 100 groups 967 sends/s, grid 50 × 49 previews 403 Mbit/s at 76 % of a core,
50 talkers 94 %, login burst +80 MiB.

| Load | Result | Source |
|---|---|---|
| 500 signed-in sessions, groups of 100, presence churn | 0 missed presence/fan-out events; hold 0.72 % of a core; ≈ 33 KiB per session (WS 30.3 MiB); group message to 99 members p99 6.2 ms | S8 step 1 |
| 1000 sessions | PASS, 2.8 % of a core, WS 45.3 MiB | S8 step 1 |
| 5 groups × 100 under concurrent writes, removals, reconnect storm | 5000 sends in 4.4 s, 0 gaps/duplicates, removed members refused; the pool (16) saturates at ≈ 900–1100 sends/s (p50 11 → 39 ms, no loss) | S8 step 2 |
| 50 screen publishers + viewers, 50 × 49 previews, 1 → 50 high viewers (accounts, voice) | 27 % / 66 % / 31 % of a core; 50 × 12 Mbit/s = 647 Mbit/s egress, server not saturated; voice ≥ 99.8 % | S8 step 3 |
| High-egress budget 40 Mbit/s with 50 viewers | exactly 20 high, 30 preview with reason `ADMISSION`, voice 100 % | S8 step 3 |
| 50 voice members, all talking | 92 % of a core (one SFU thread); 10 talkers 26 % | S2 step 6, S8 step 3 |
| 200 parallel logins | +87 MiB with the default hash bound (+874 MiB unbounded) | S8 step 4 |

What limits a real installation first: the **uplink** (a 1 Gbit/s line carries at most
≈ 50 × 20 Mbit/s of full-quality shares before overhead, spec §5.3 — set
`MAX_HIGH_EGRESS_MBPS`), then one SFU worker thread for many simultaneous talkers, then
the database pool on write bursts. One share is capped at 12 Mbit/s by the client pacer.

## 9. Load and recovery checks on the server (S8, owner-assisted)

`/usr/local/lib/turbo-pancake/target-bench.sh` (installed by `install-server.sh`) runs
the S8 step 1–5 checks on the host and writes all reports and a `summary.txt` with a
verdict into one directory. Run it at a quiet time as root:

```bash
sudo /usr/local/lib/turbo-pancake/target-bench.sh /root/s8-target 1,2,3        # load, ~15 min
sudo ALLOW_RESTART=1 /usr/local/lib/turbo-pancake/target-bench.sh /root/s8-target 4   # restarts the service
sudo /usr/local/lib/turbo-pancake/target-bench.sh /root/s8-target 5            # backup + restore drill
sudo reboot
# after the reboot:
sudo /usr/local/lib/turbo-pancake/target-bench.sh /root/s8-target r
```

* Steps 1–4 build `turbo-pancake-bench` from the installed source and run their own
  server process on `127.0.0.1` with a separate, freshly created database
  `turbo_pancake_bench` (password in `/etc/turbo-pancake/bench-db-password`); the
  production service and its database are not touched, except the restart in step 4.
  Loopback numbers measure this host's CPU and memory, not its network.
* The **real uplink** needs clients elsewhere: e.g. two or three real clients on other
  networks watching one share at 1440p while `GET /v1/media-stats` and the router show
  the egress, or a UDP throughput test (`iperf3 -u`) to a machine outside. Record the
  result and set `MAX_HIGH_EGRESS_MBPS` from it.
* Step 5 runs one backup now, restores it into `tp_drill`, checks it and drops it, and
  runs `check-db` on the live database.
* `r` after a reboot: service and timer active, `/readyz`, the start-up messages of the
  database wait in the journal.

Before sharing or committing results, remove addresses, host names and anything about
the network; describe the host generically ("target server").
