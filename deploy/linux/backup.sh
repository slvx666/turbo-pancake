#!/usr/bin/env bash
# Backup of the turbo-pancake messenger database (S8 step 5, docs/BACKUP.md).
#
#   backup.sh                      # run by turbo-pancake-backup.timer as user postgres
#
# Writes <dir>/turbo-pancake-<UTC time>.dump (pg_dump custom format, compressed), first
# as .partial, checked with pg_restore --list, then renamed: a dump file that exists is
# complete. Afterwards deletes dumps older than KEEP_DAYS, keeping at least KEEP_MIN of
# the newest. Connection: libpq environment / peer authentication (no password on the
# command line). Settings (environment):
#   TURBO_PANCAKE_BACKUP_DIR       default /var/backups/turbo-pancake
#   TURBO_PANCAKE_BACKUP_DB        default turbo_pancake
#   TURBO_PANCAKE_BACKUP_KEEP_DAYS default 14   (retention cycle, docs/BACKUP.md)
#   TURBO_PANCAKE_BACKUP_KEEP_MIN  default 3
#   PG_DUMP, PG_RESTORE            tool paths (default: from PATH)
set -euo pipefail

DIR="${TURBO_PANCAKE_BACKUP_DIR:-/var/backups/turbo-pancake}"
DB="${TURBO_PANCAKE_BACKUP_DB:-turbo_pancake}"
KEEP_DAYS="${TURBO_PANCAKE_BACKUP_KEEP_DAYS:-14}"
KEEP_MIN="${TURBO_PANCAKE_BACKUP_KEEP_MIN:-3}"
PG_DUMP="${PG_DUMP:-pg_dump}"
PG_RESTORE="${PG_RESTORE:-pg_restore}"

umask 077
mkdir -p "$DIR"

stamp="$(date -u +%Y%m%dT%H%M%SZ)"
out="$DIR/turbo-pancake-$stamp.dump"
tmp="$out.partial"
trap 'rm -f "$tmp"' EXIT

# Leftovers of an interrupted run.
find "$DIR" -maxdepth 1 -name 'turbo-pancake-*.dump.partial' -mmin +60 -delete

# Space: at least twice the newest dump must be free (a first run needs 64 MiB).
newest="$(ls -1 "$DIR"/turbo-pancake-*.dump 2>/dev/null | sort | tail -n 1 || true)"
need_kib=65536
if [ -n "$newest" ]; then
    need_kib=$(( $(du -k "$newest" | cut -f1) * 2 + 1024 ))
fi
free_kib="$(df -Pk "$DIR" | awk 'NR == 2 {print $4}')"
if [ "$free_kib" -lt "$need_kib" ]; then
    echo "backup: not enough space in $DIR (${free_kib} KiB free, ${need_kib} KiB needed)" >&2
    exit 2
fi

start=$(date +%s)
"$PG_DUMP" --format=custom --compress=6 --no-owner --no-privileges --file="$tmp" "$DB"
# The archive must be readable (table of contents) before it counts as a backup.
"$PG_RESTORE" --list "$tmp" >/dev/null
mv "$tmp" "$out"
trap - EXIT

# Retention: dumps older than KEEP_DAYS, but never the KEEP_MIN newest.
mapfile -t dumps < <(ls -1 "$DIR"/turbo-pancake-*.dump 2>/dev/null | sort -r)
removed=0
for i in "${!dumps[@]}"; do
    [ "$i" -lt "$KEEP_MIN" ] && continue
    if [ -n "$(find "${dumps[$i]}" -maxdepth 0 -mtime +"$KEEP_DAYS")" ]; then
        rm -f "${dumps[$i]}"
        removed=$((removed + 1))
    fi
done

size="$(du -k "$out" | cut -f1)"
echo "backup: $out ${size} KiB in $(( $(date +%s) - start )) s; ${#dumps[@]} dumps before retention, $removed removed"
