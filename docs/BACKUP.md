# Backups, restore and recovery (S8 step 5)

What the Linux server keeps, how to restore it, and how long deleted messages can
survive in backups (spec §7, §12). Scripts: `deploy/linux/backup.sh`,
`deploy/linux/restore.sh`; `install-server.sh` installs both and the timer.

## What is backed up

The PostgreSQL database of the messenger: accounts (Argon2id hashes, never passwords),
session token hashes, conversations, members and receipts, messages (deleted ones as
tombstones without text), blocks. Not in the database and therefore not in a backup:
live calls (ephemeral by design, ADR 0015), presence, media. Keep separately and offline:
`/etc/turbo-pancake` (server certificate and key, `server.env`, database password) and
the CA key (never on the server, docs/DEPLOY-TLS.md).

## Schedule and files

* `turbo-pancake-backup.timer` runs `turbo-pancake-backup.service` daily at 03:30 (± 15
  min; a missed run is made up after boot, `Persistent=true`) as user `postgres` (peer
  authentication: no password on any command line), at low CPU/IO priority.
* Output: `/var/backups/turbo-pancake/turbo-pancake-<UTC time>.dump` (mode 600, directory
  700, owner `postgres`), `pg_dump` custom format, compressed. Written as `.partial`,
  checked with `pg_restore --list`, then renamed: a `.dump` file that exists is complete.
* Space check before each run: twice the newest dump must be free, else the run fails
  (exit 2) and nothing is deleted.
* Retention: dumps older than `TURBO_PANCAKE_BACKUP_KEEP_DAYS` (14) are deleted, but the
  3 newest are always kept (`TURBO_PANCAKE_BACKUP_KEEP_MIN`).
* Status: `systemctl list-timers turbo-pancake-backup.timer`, `journalctl -u
  turbo-pancake-backup` (one line per run: file, size, time, removed dumps).
* Off-site copy (administrator's choice, spec §12): copy the newest dump to a separate
  medium or encrypted external storage, e.g. `age`/`gpg`-encrypted before upload. The
  same retention must apply there (below).

## Retention cycle of deleted messages

Deleting a message for everyone clears its text in the live database at once (the row
stays as a tombstone so replies and sync keep working). Where the text can still exist:

| Place | Until |
|---|---|
| Live database table files (old row version) | the next vacuum of `messages` (autovacuum, usually minutes to hours); not readable through the server |
| Write-ahead log on the server | WAL segments are recycled after checkpoints (minutes); not part of the dumps |
| Daily dumps taken before the deletion | **at most 15 days**: the newest dump without the text is taken within a day, older ones are deleted after 14 days |
| Off-site copies | the administrator's retention; must not exceed the 14 days above to keep this promise |
| Clients that showed or copied it | cannot be guaranteed (spec §7) |

So a message deleted for everyone is gone from the server's own backups at most 15
days after its deletion, provided the daily backup keeps running. If backups stop,
`KEEP_MIN` keeps the 3 newest dumps however old they are; fix the backup (and delete
stale dumps by hand) to stay within the cycle. Verified by
`apps/app-server/tests/backup_restore.rs`: a message deleted before a backup is not in
its dump, one deleted after it still is.

## Restore drill (do it after setup and then regularly)

```bash
sudo -u postgres /usr/local/lib/turbo-pancake/restore.sh /var/backups/turbo-pancake/<file>.dump tp_drill
sudo -u postgres dropdb tp_drill
```

`restore.sh` creates a **new** database (it refuses an existing name), restores the dump
in one transaction (`--exit-on-error`), then runs `turbo-pancake-app-server check-db`
on it. `check-db` prints a JSON report and exits 1 if an invariant is broken: message
sequences gap-free per conversation, counters match the messages, unique change
positions, receipts within the history, direct conversations with their two members,
one creator per group, ≤ 100 active members, replies inside their conversation, authors
who were members; plus row counts and a fingerprint per table (two copies of the same
data have equal fingerprints). `check-db` only reads; it can run on the live database too:

```bash
sudo -u turbo-pancake sh -c 'set -a; . /etc/turbo-pancake/server.env; exec turbo-pancake-app-server check-db'
```

## Recovery

1. Stop the server: `systemctl stop turbo-pancake` (graceful: in-flight requests finish).
2. Restore the newest good dump into a new database owned by the service role:
   `sudo -u postgres TURBO_PANCAKE_RESTORE_OWNER=turbo_pancake /usr/local/lib/turbo-pancake/restore.sh <dump> turbo_pancake_restored`
   and check that `check-db` says `"ok": true`.
3. Either point `TURBO_PANCAKE_DATABASE_URL` in `/etc/turbo-pancake/server.env` at the new
   database, or rename: `sudo -u postgres psql -c 'ALTER DATABASE turbo_pancake RENAME TO turbo_pancake_broken'`
   then `... RENAME TO turbo_pancake` for the restored one.
4. `systemctl start turbo-pancake`. Migrations newer than the dump are applied at start.
   Sessions in the dump stay valid (clients reconnect without signing in again); messages
   sent after the dump are lost on the server and the clients' caches resync to the
   server's history (pending sends of a client are resent with their nonces).

## After a reboot

The unit starts after `postgresql.service`; if PostgreSQL is not accepting connections
yet, the server keeps retrying for 120 s (then exits and systemd restarts it; the unit
never stops restarting, `StartLimitIntervalSec=0`). Clients reconnect with backoff and
resync from their last `change_seq`; live calls end with a restart (ADR 0015) and are
started again by the users.

## What is verified where

| Item | Where | Status |
|---|---|---|
| `backup.sh` / `restore.sh` on a real PostgreSQL, retention, archive check, refusal to overwrite, `check-db` ok and catches a broken copy, equal fingerprints, a server on the copy serves the same sessions and history | `apps/app-server/tests/backup_restore.rs` (Git's bash on Windows, PostgreSQL 17 of `crates/pg-test`) | PASS (dev machine) |
| Deleted-message retention cycle (before / after a backup) | same test | PASS |
| Server start while the database is down: bounded wait, then up when it returns | `apps/app-server/tests/db_outage.rs` | PASS |
| Restart: clients reconnect and resync, no duplicates; sessions and history survive | `crates/messenger-client/tests/sync.rs`, `apps/app-server/tests/messenger.rs`, native `s7-reconnect-trial.ps1` | PASS (earlier steps) |
| systemd timer and service, peer authentication, the socket URL of `check-db`, a real reboot of the host, dump size/time on real data | the owner's Linux server | UNVERIFIED (owner-assisted) |
