#!/usr/bin/env bash
# Restores a turbo-pancake dump into a NEW database and checks it (S8 step 5,
# docs/BACKUP.md). It never writes into an existing database: restore drills use a
# scratch name; a real recovery restores into a new name and then points the server at
# it (or renames it while the server is stopped).
#
#   restore.sh <dump file> <new database name>
#
# Connection: libpq environment / peer authentication (run as user postgres).
#   TURBO_PANCAKE_SERVER_BIN  default /usr/local/bin/turbo-pancake-app-server (check-db)
#   TURBO_PANCAKE_CHECK_URL   database URL for check-db; default: the new database over
#                             the local socket (postgres:///<name>?host=/var/run/postgresql)
#   TURBO_PANCAKE_RESTORE_OWNER  owner of the new database and its objects (a real
#                             recovery: turbo_pancake); default: the connecting user
#   PG_RESTORE, CREATEDB, PSQL  tool paths (default: from PATH)
set -euo pipefail

DUMP="${1:?usage: restore.sh <dump file> <new database name>}"
TARGET="${2:?usage: restore.sh <dump file> <new database name>}"
SERVER_BIN="${TURBO_PANCAKE_SERVER_BIN:-/usr/local/bin/turbo-pancake-app-server}"
PG_RESTORE="${PG_RESTORE:-pg_restore}"
CREATEDB="${CREATEDB:-createdb}"
PSQL="${PSQL:-psql}"

case "$TARGET" in
    *[!a-z0-9_]* | "") echo "restore: database name must be [a-z0-9_]" >&2; exit 2 ;;
esac
[ -f "$DUMP" ] || { echo "restore: $DUMP not found" >&2; exit 2; }
exists="$("$PSQL" -d postgres -tAc "SELECT 1 FROM pg_database WHERE datname = '$TARGET'")"
if [ "$exists" = "1" ]; then
    echo "restore: database $TARGET exists; refusing to overwrite it" >&2
    exit 2
fi

OWNER="${TURBO_PANCAKE_RESTORE_OWNER:-}"
owner_args=()
role_args=()
if [ -n "$OWNER" ]; then
    owner_args=(--owner="$OWNER")
    role_args=(--role="$OWNER")
fi

start=$(date +%s)
"$CREATEDB" "${owner_args[@]}" "$TARGET"
"$PG_RESTORE" --exit-on-error --no-owner --no-privileges --single-transaction \
    "${role_args[@]}" --dbname="$TARGET" "$DUMP"
echo "restore: $DUMP -> $TARGET in $(( $(date +%s) - start )) s; checking"

URL="${TURBO_PANCAKE_CHECK_URL:-postgres:///$TARGET?host=/var/run/postgresql}"
TURBO_PANCAKE_DATABASE_URL="$URL" "$SERVER_BIN" check-db
