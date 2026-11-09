#!/usr/bin/env bash
# Publishes a signed client release (ADR 0016) to the server's update directory
# (docs/ADMIN.md, "Publishing an update"). Run as root on the server.
#
#   publish-update.sh <signed dir>
#
# <signed dir> is the `update/` directory of a release (deploy/release.ps1 -UpdateKey) or
# the --out of `turbo-pancake-update sign`: manifest.json + files/<version>/... .
# Files go in first, the manifest last with an atomic rename, so a client never sees a
# manifest whose files are missing. The previous version's files are kept (clients that
# are downloading it finish); older ones are removed.
#   TURBO_PANCAKE_UPDATE_DIR  default /var/lib/turbo-pancake/update
set -euo pipefail

IN="${1:?usage: publish-update.sh <signed dir>}"
DEST="${TURBO_PANCAKE_UPDATE_DIR:-/var/lib/turbo-pancake/update}"

[ -f "$IN/manifest.json" ] || { echo "publish: $IN/manifest.json not found" >&2; exit 2; }
versions=()
for d in "$IN"/files/*/; do
    [ -d "$d" ] && versions+=("$(basename "$d")")
done
[ "${#versions[@]}" -eq 1 ] || { echo "publish: expected exactly one version under $IN/files, found ${#versions[@]}" >&2; exit 2; }
VERSION="${versions[0]}"
case "$VERSION" in
    *[!0-9.]* | "") echo "publish: bad version directory $VERSION" >&2; exit 2 ;;
esac
size="$(stat -c %s "$IN/manifest.json")"
[ "$size" -le 65536 ] || { echo "publish: manifest.json is $size bytes (> 64 KiB)" >&2; exit 2; }

install -d -m 755 "$DEST" "$DEST/files"
previous=""
if [ -f "$DEST/manifest.json" ]; then
    previous="$(ls -1 "$DEST/files" 2>/dev/null | grep -v '^\.' | sort -V | tail -n 1 || true)"
fi

# 1. files (copied next to the target, then renamed into place)
tmp="$DEST/files/.incoming-$VERSION"
rm -rf "$tmp"
cp -R "$IN/files/$VERSION" "$tmp"
chmod -R a+rX,go-w "$tmp"
rm -rf "$DEST/files/$VERSION"
mv "$tmp" "$DEST/files/$VERSION"

# 2. manifest (atomic replace)
cp "$IN/manifest.json" "$DEST/.manifest.json.tmp"
chmod 644 "$DEST/.manifest.json.tmp"
sync -f "$DEST/.manifest.json.tmp" 2>/dev/null || sync
mv -f "$DEST/.manifest.json.tmp" "$DEST/manifest.json"

# 3. keep this and the previously published version only
for d in "$DEST"/files/*/; do
    v="$(basename "$d")"
    if [ "$v" != "$VERSION" ] && [ "$v" != "$previous" ]; then
        rm -rf "$DEST/files/$v"
    fi
done
echo "published $VERSION (kept: ${previous:-none}) in $DEST"
