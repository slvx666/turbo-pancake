#!/usr/bin/env bash
# Installs or updates the turbo-pancake server on a Debian/Ubuntu host (run as root).
#
#   bash install-server.sh <public IP> [branch]
#
# Before the first run, put server.pem and server.key (from `gen-cert`, docs/DEPLOY-TLS.md)
# into `tls/` next to this script. With `src.tar.gz` next to it (`git archive`) that source
# is built; otherwise the branch is fetched from GitHub. Re-running updates, rebuilds and
# restarts the service; the database, certificates and the administrator's own settings in
# /etc/turbo-pancake/server.local.env are kept (docs/ADMIN.md). Firewall rules are added
# only when ufw is already active (the script never turns a firewall on).
# Behind NAT, forward TCP and UDP 7878 to this host.
set -euo pipefail

PUBLIC_IP="${1:?usage: install-server.sh <public IP> [branch]}"
BRANCH="${2:-main}"
REPO="${TURBO_PANCAKE_REPO:-https://github.com/slvx666/turbo-pancake.git}"
PORT=7878
SRC=/opt/turbo-pancake/src
ETC=/etc/turbo-pancake
HERE="$(cd "$(dirname "$0")" && pwd)"
TLS_IN="$HERE/tls"

[ "$(id -u)" -eq 0 ] || { echo "run as root"; exit 1; }

echo "== packages"
export DEBIAN_FRONTEND=noninteractive
apt-get update -q
apt-get install -y -q git curl ca-certificates build-essential pkg-config cmake clang \
    postgresql

# A release build of the server needs ~2 GB of memory; add swap on small hosts.
if [ "$(awk '/MemTotal/ {print int($2/1024)}' /proc/meminfo)" -lt 3000 ] && ! swapon --show | grep -q .; then
    echo "== swap (2 GB)"
    fallocate -l 2G /swapfile && chmod 600 /swapfile && mkswap /swapfile && swapon /swapfile
    grep -q '^/swapfile' /etc/fstab || echo '/swapfile none swap sw 0 0' >> /etc/fstab
fi

echo "== rust"
if ! command -v cargo >/dev/null && [ ! -x "$HOME/.cargo/bin/cargo" ]; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none
fi
export PATH="$HOME/.cargo/bin:$PATH"

if [ -f "$HERE/src.tar.gz" ]; then
    echo "== source (src.tar.gz)"
    mkdir -p "$SRC"
    find "$SRC" -mindepth 1 -maxdepth 1 ! -name target -exec rm -rf {} +
    tar -xzf "$HERE/src.tar.gz" -C "$SRC"
    # The commit of the archive (git archive records it), for target-bench.sh reports.
    gzip -dc "$HERE/src.tar.gz" | git get-tar-commit-id > "$SRC/.git-commit" || true
elif [ -d "$SRC/.git" ]; then
    echo "== source ($BRANCH)"
    git -C "$SRC" fetch --depth 1 origin "$BRANCH"
    git -C "$SRC" checkout -q -B "$BRANCH" FETCH_HEAD
else
    echo "== source ($BRANCH)"
    rm -rf "$SRC" && mkdir -p "$(dirname "$SRC")"
    git clone --depth 1 --branch "$BRANCH" "$REPO" "$SRC"
fi

echo "== build"
(cd "$SRC" && cargo build --release --locked -p turbo-pancake-app-server)
install -m 755 "$SRC/target/release/turbo-pancake-app-server" /usr/local/bin/turbo-pancake-app-server

echo "== kernel limits for UDP media"
# The SFU asks for 4 MiB socket buffers; Linux caps them at rmem_max/wmem_max (often
# 208 KiB), and the kernel then drops bursts (measured on a target server: 65 000
# datagrams lost per 50 x 49 preview run, voice 72 %). Raising the maxima changes no
# default of other programs.
cat > /etc/sysctl.d/90-turbo-pancake.conf <<'SYSCTL'
net.core.rmem_max = 8388608
net.core.wmem_max = 8388608
SYSCTL
sysctl -q -p /etc/sysctl.d/90-turbo-pancake.conf

echo "== service account"
id turbo-pancake >/dev/null 2>&1 || useradd --system --home /var/lib/turbo-pancake --shell /usr/sbin/nologin turbo-pancake
mkdir -p "$ETC/tls" /var/lib/turbo-pancake
chown turbo-pancake: /var/lib/turbo-pancake

echo "== certificates"
if [ -f "$TLS_IN/server.pem" ]; then
    install -m 644 -o root -g turbo-pancake "$TLS_IN/server.pem" "$ETC/tls/server.pem"
    install -m 640 -o root -g turbo-pancake "$TLS_IN/server.key" "$ETC/tls/server.key"
fi
[ -f "$ETC/tls/server.pem" ] || { echo "missing $TLS_IN/server.pem and server.key"; exit 1; }
# ca.pem is public (the clients' trust anchor); kept for local health probes.
[ ! -f "$TLS_IN/ca.pem" ] || install -m 644 "$TLS_IN/ca.pem" "$ETC/tls/ca.pem"
[ ! -f "$TLS_IN/ca.key" ] || echo "WARNING: ca.key does not belong on the server; delete $TLS_IN/ca.key"
rm -f "$TLS_IN/server.key"

echo "== database"
systemctl enable --now postgresql
if [ ! -f "$ETC/db-password" ]; then
    (umask 077; head -c 24 /dev/urandom | base64 | tr -d '/+=' > "$ETC/db-password")
fi
DB_PASS="$(cat "$ETC/db-password")"
if ! sudo -u postgres psql -tAc "SELECT 1 FROM pg_roles WHERE rolname='turbo_pancake'" | grep -q 1; then
    sudo -u postgres psql -q -c "CREATE ROLE turbo_pancake LOGIN PASSWORD '$DB_PASS'"
fi
if ! sudo -u postgres psql -tAc "SELECT 1 FROM pg_database WHERE datname='turbo_pancake'" | grep -q 1; then
    sudo -u postgres createdb -O turbo_pancake turbo_pancake
fi

echo "== configuration"
(umask 027; cat > "$ETC/server.env" <<EOF
TURBO_PANCAKE_BIND=0.0.0.0:$PORT
TURBO_PANCAKE_PUBLIC_IP=$PUBLIC_IP
TURBO_PANCAKE_TLS_CERT=$ETC/tls/server.pem
TURBO_PANCAKE_TLS_KEY=$ETC/tls/server.key
TURBO_PANCAKE_DATABASE_URL=postgres://turbo_pancake:$DB_PASS@127.0.0.1/turbo_pancake
TURBO_PANCAKE_LOG_JSON=true
TURBO_PANCAKE_UPDATE_DIR=/var/lib/turbo-pancake/update
EOF
)
chgrp turbo-pancake "$ETC/server.env"
install -d -m 755 /var/lib/turbo-pancake/update
# The administrator's settings (limits, budgets): read after server.env, never rewritten.
if [ ! -f "$ETC/server.local.env" ]; then
    (umask 027; cat > "$ETC/server.local.env" <<'EOF'
# Own settings of this server (docs/ADMIN.md, "Configuration"); install-server.sh never
# rewrites this file. Values here override server.env. Examples:
# TURBO_PANCAKE_MAX_HIGH_EGRESS_MBPS=600
# TURBO_PANCAKE_DATABASE_POOL=16
# TURBO_PANCAKE_HASH_CONCURRENCY=2
EOF
)
    chgrp turbo-pancake "$ETC/server.local.env"
fi

cat > /etc/systemd/system/turbo-pancake.service <<EOF
[Unit]
Description=turbo-pancake messenger server
After=network-online.target postgresql.service
Wants=network-online.target
# Keep restarting (the server also waits up to 120 s for PostgreSQL after a reboot).
StartLimitIntervalSec=0

[Service]
User=turbo-pancake
Group=turbo-pancake
EnvironmentFile=$ETC/server.env
EnvironmentFile=-$ETC/server.local.env
# glibc raises its mmap threshold after the first freed Argon2 block (19 MiB), so later
# blocks stay in per-thread heaps: +440 MiB after 70 logins instead of +80 MiB. A fixed
# threshold returns them to the OS (measured on a target server, docs/ADMIN.md).
Environment=MALLOC_MMAP_THRESHOLD_=1048576
ExecStart=/usr/local/bin/turbo-pancake-app-server
Restart=always
RestartSec=2
# Graceful stop: SIGTERM, the server lets in-flight requests finish (10 s grace), then
# systemd kills it after TimeoutStopSec.
KillSignal=SIGTERM
TimeoutStopSec=20
# Resource limits (S8 step 4): descriptors well above --max-tcp-connections (4096) plus
# the database pool and media; a memory ceiling so a runaway cannot take the host.
LimitNOFILE=65536
TasksMax=1024
MemoryHigh=1536M
MemoryMax=2G
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
ReadWritePaths=/var/lib/turbo-pancake
WorkingDirectory=/var/lib/turbo-pancake

[Install]
WantedBy=multi-user.target
EOF

if command -v ufw >/dev/null && ufw status | grep -q "Status: active"; then
    echo "== firewall (ufw is active)"
    ufw allow "$PORT/tcp" >/dev/null
    ufw allow "$PORT/udp" >/dev/null
fi

echo "== backups (docs/BACKUP.md)"
install -d -m 755 /usr/local/lib/turbo-pancake
install -m 755 "$SRC/deploy/linux/backup.sh" /usr/local/lib/turbo-pancake/backup.sh
install -m 755 "$SRC/deploy/linux/restore.sh" /usr/local/lib/turbo-pancake/restore.sh
install -m 755 "$SRC/deploy/linux/publish-update.sh" /usr/local/lib/turbo-pancake/publish-update.sh
install -m 755 "$SRC/deploy/linux/target-bench.sh" /usr/local/lib/turbo-pancake/target-bench.sh
install -d -m 700 -o postgres -g postgres /var/backups/turbo-pancake
cat > /etc/systemd/system/turbo-pancake-backup.service <<EOF
[Unit]
Description=turbo-pancake database backup
After=postgresql.service

[Service]
Type=oneshot
User=postgres
Environment=TURBO_PANCAKE_BACKUP_DIR=/var/backups/turbo-pancake
Environment=TURBO_PANCAKE_BACKUP_KEEP_DAYS=14
ExecStart=/usr/local/lib/turbo-pancake/backup.sh
Nice=10
IOSchedulingClass=idle
EOF
cat > /etc/systemd/system/turbo-pancake-backup.timer <<EOF
[Unit]
Description=Daily turbo-pancake database backup

[Timer]
OnCalendar=*-*-* 03:30:00
RandomizedDelaySec=15min
Persistent=true

[Install]
WantedBy=timers.target
EOF

systemctl daemon-reload
systemctl enable turbo-pancake >/dev/null
systemctl enable --now turbo-pancake-backup.timer >/dev/null
systemctl restart turbo-pancake
sleep 2
systemctl --no-pager --lines 15 status turbo-pancake
