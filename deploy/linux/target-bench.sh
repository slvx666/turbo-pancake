#!/usr/bin/env bash
# S8 on the target server (docs/ADMIN.md, "Load and recovery checks on the server").
# Runs the S8 step 1-5 checks on the host itself and collects reports in one directory.
# Run as root on the server after install-server.sh, at a quiet time:
#
#   target-bench.sh <results dir> [steps]      steps: comma-separated, default 1,2,3,4,5
#
#   1  messenger: 500 and 1000 signed-in WebSocket sessions, presence, group fan-out
#   2  group-load: 2 x 100 and 5 x 100 members under writes, removals, reconnect storm
#   3  media: 50 publishers, 50 x 49 previews, 1 -> 50 viewers (also at 8 Mbit/s and with
#      a 40 Mbit/s budget), 50 voice members; all with accounts
#   4  hardening: login burst (bounded Argon2id), the unit's limits, graceful stop on
#      SIGTERM and time to ready again (restarts the service: needs ALLOW_RESTART=1)
#   5  backup: one backup run now, restore drill into a scratch database, check-db on
#      it and on the live database
#   r  after a reboot: service and timer state, time since boot until ready, database
#      wait messages in the journal
#
# Load runs (1-4) start their own server process on 127.0.0.1 with a dedicated, freshly
# created database `turbo_pancake_bench`; the production service and its database are
# not touched (except the restart in step 4 when allowed). A loopback run measures the
# server's own CPU/memory on this hardware, not the network or the uplink.
#   SRC          source tree (default /opt/turbo-pancake/src, from install-server.sh)
#   SERVICE_URL  https://<public IP>:7878 for health probes (default from server.env)
set -uo pipefail

RESULTS="${1:?usage: target-bench.sh <results dir> [steps]}"
STEPS=",${2:-1,2,3,4,5},"
SRC="${SRC:-/opt/turbo-pancake/src}"
ETC=/etc/turbo-pancake
LIB=/usr/local/lib/turbo-pancake
export PATH="$HOME/.cargo/bin:$PATH"

[ "$(id -u)" -eq 0 ] || { echo "run as root"; exit 1; }
# A login shell allows 1024 descriptors; 1000 sessions need ~1000 on each side (the
# bench and its server inherit this). The service itself has LimitNOFILE=65536.
ulimit -n 65536 2>/dev/null || ulimit -n "$(ulimit -Hn)"
# Each run gets a deadline, so one stuck scenario cannot block the rest.
RUN_TIMEOUT="${RUN_TIMEOUT:-20m}"
# Same allocator setting as the service unit (install-server.sh), so the bench's server
# measures what production runs.
export MALLOC_MMAP_THRESHOLD_=1048576
mkdir -p "$RESULTS"
RESULTS="$(cd "$RESULTS" && pwd)"
SUMMARY="$RESULTS/summary.txt"
FAILED=0
note() { echo "$*" | tee -a "$SUMMARY"; }
want() { case "$STEPS" in *",$1,"*) return 0 ;; *) return 1 ;; esac; }

note "== target-bench $(date -u +%Y-%m-%dT%H:%M:%SZ) steps=${STEPS#,}"
note "host: $(nproc) CPUs, $(awk '/MemTotal/ {print int($2/1024)}' /proc/meminfo) MiB RAM, kernel $(uname -r)"
note "postgres: $(sudo -u postgres psql -tAc 'SHOW server_version' 2>/dev/null || echo unknown)"
note "udp buffers: rmem_max=$(sysctl -n net.core.rmem_max) wmem_max=$(sysctl -n net.core.wmem_max)"
note "source: $(cat "$SRC/.git-commit" 2>/dev/null || git -C "$SRC" rev-parse --short HEAD 2>/dev/null || echo 'src.tar.gz (commit in the release.json of the release)')"

# ---- probes of the production service ----
if [ -z "${SERVICE_URL:-}" ] && [ -f "$ETC/server.env" ]; then
    ip="$(sed -n 's/^TURBO_PANCAKE_PUBLIC_IP=//p' "$ETC/server.env" | cut -d, -f1)"
    port="$(sed -n 's/^TURBO_PANCAKE_BIND=.*:\([0-9]*\)$/\1/p' "$ETC/server.env")"
    [ -n "$ip" ] && SERVICE_URL="https://$ip:${port:-7878}"
fi
# Verified TLS: the private CA (ca.pem) and a connection to loopback for the public name.
probe() {
    local path="$1"
    [ -f "$ETC/tls/ca.pem" ] && [ -n "${SERVICE_URL:-}" ] || { echo "no-ca"; return; }
    local hostport="${SERVICE_URL#https://}"
    curl -s -o /dev/null -w '%{http_code}' --max-time 3 --cacert "$ETC/tls/ca.pem" \
        --connect-to "$hostport:127.0.0.1:${hostport##*:}" "$SERVICE_URL$path" || echo "000"
}

# ---- build ----
if want 1 || want 2 || want 3 || want 4; then
    note "== build (bench + server, release)"
    if ! (cd "$SRC" && cargo build --release --locked -p turbo-pancake-bench -p turbo-pancake-app-server) \
            > "$RESULTS/build.log" 2>&1; then
        note "FAIL build, see build.log"; exit 1
    fi
    BENCH="$SRC/target/release/turbo-pancake-bench"
    SERVER="$SRC/target/release/turbo-pancake-app-server"

    # Dedicated role and database; recreated empty before every run.
    if [ ! -f "$ETC/bench-db-password" ]; then
        (umask 077; head -c 24 /dev/urandom | base64 | tr -d '/+=' > "$ETC/bench-db-password")
    fi
    BENCH_PASS="$(cat "$ETC/bench-db-password")"
    if ! sudo -u postgres psql -tAc "SELECT 1 FROM pg_roles WHERE rolname='turbo_pancake_bench'" | grep -q 1; then
        sudo -u postgres psql -q -c "CREATE ROLE turbo_pancake_bench LOGIN PASSWORD '$BENCH_PASS'"
    fi
    export TURBO_PANCAKE_BENCH_DATABASE_URL="postgres://turbo_pancake_bench:$BENCH_PASS@127.0.0.1/turbo_pancake_bench"
fi

fresh_db() {
    sudo -u postgres dropdb --if-exists turbo_pancake_bench
    sudo -u postgres createdb -O turbo_pancake_bench turbo_pancake_bench
}

# run <name> <bench args...>: fresh database, own log, the report path and verdict.
run() {
    local name="$1"; shift
    fresh_db
    local t0=$SECONDS
    timeout --kill-after=30s "$RUN_TIMEOUT" "$BENCH" --out-dir "$RESULTS" "$@" --server-exe "$SERVER" > "$RESULTS/$name.log" 2>&1
    local code=$?
    # A run can finish (exit 0) with a failed verdict in its report.
    local report
    report="$(sed -n 's/^report written to //p' "$RESULTS/$name.log" | tail -n 1)"
    if [ "$code" -eq 0 ] && [ -n "$report" ] && grep -q '"pass": false' "$report"; then
        code=3
        note "  $name: report verdict pass=false"
    fi
    if [ "$code" -eq 0 ]; then
        note "PASS $name ($((SECONDS - t0)) s) $(grep -o 'report written to .*' "$RESULTS/$name.log" | tail -n 1)"
    else
        FAILED=$((FAILED + 1))
        [ "$code" -eq 124 ] || [ "$code" -eq 137 ] && note "  $name hit the $RUN_TIMEOUT deadline"
        note "FAIL $name ($((SECONDS - t0)) s, exit $code), see $name.log"
        # The bench's server may outlive a killed bench.
        pkill -f -- "$SERVER --bind 127.0.0.1:0" 2>/dev/null || true
    fi
}

if want 1; then
    note "== step 1: messenger sessions and presence"
    run messenger-500u messenger --users 500
    run messenger-1000u messenger --users 1000
fi

if want 2; then
    note "== step 2: groups of 100 under load"
    run group-load-2x100 group-load --groups 2
    run group-load-5x100 group-load --groups 5
fi

if want 3; then
    note "== step 3: media load with accounts"
    run screen-publishers-50 sfu-screen --scenario publishers --n 50 --talkers 3 --accounts
    run screen-grid-50 sfu-screen --scenario grid --n 50 --talkers 3 --accounts
    run screen-fanout-50 sfu-screen --scenario fanout --n 50 --talkers 3 --accounts
    run screen-fanout-50-8m sfu-screen --scenario fanout --n 50 --talkers 3 --accounts --high-kbps 8000 --high-fps 60
    run screen-fanout-50-budget40 sfu-screen --scenario fanout --n 50 --talkers 3 --accounts --max-high-egress-mbps 40
    run voice-50m sfu --members 50 --talkers 1,10,50 --accounts
fi

if want 4; then
    note "== step 4: hardening"
    run login-burst-200 login-burst --logins 200
    note "unit: $(systemctl show turbo-pancake -p KillSignal -p TimeoutStopUSec -p LimitNOFILE -p TasksMax -p MemoryHigh -p MemoryMax -p StartLimitIntervalUSec | tr '\n' ' ')"
    if [ "${ALLOW_RESTART:-0}" = "1" ]; then
        since="$(date '+%Y-%m-%d %H:%M:%S')"
        t0=$(date +%s%N)
        systemctl restart turbo-pancake
        code=000
        for _ in $(seq 1 300); do
            code="$(probe /readyz)"
            [ "$code" = "200" ] || [ "$code" = "no-ca" ] && break
            sleep 0.1
        done
        ms=$((($(date +%s%N) - t0) / 1000000))
        if journalctl -u turbo-pancake --since "$since" --no-pager -o cat | grep -q 'shutdown requested'; then
            graceful=yes
        else
            graceful=no; FAILED=$((FAILED + 1))
        fi
        [ "$code" = "200" ] || [ "$code" = "no-ca" ] || FAILED=$((FAILED + 1))
        note "restart: graceful stop on SIGTERM=$graceful, ready again after ${ms} ms (readyz $code)"
        [ "$code" = "no-ca" ] && note "  readiness not probed: copy the public ca.pem to $ETC/tls/ca.pem"
    else
        note "restart: skipped (set ALLOW_RESTART=1; it interrupts live calls)"
    fi
fi

if want 5; then
    note "== step 5: backup and restore drill"
    t0=$SECONDS
    if systemctl start turbo-pancake-backup.service; then
        dump="$(ls -1t /var/backups/turbo-pancake/*.dump 2>/dev/null | head -n 1)"
        note "backup: $(basename "$dump") $(stat -c %s "$dump") bytes in $((SECONDS - t0)) s"
        sudo -u postgres dropdb --if-exists tp_drill
        t0=$SECONDS
        if sudo -u postgres "$LIB/restore.sh" "$dump" tp_drill > "$RESULTS/restore-drill.log" 2>&1; then
            note "PASS restore drill ($((SECONDS - t0)) s): $(grep -o '"ok": *[a-z]*' "$RESULTS/restore-drill.log" | head -n 1)"
        else
            FAILED=$((FAILED + 1)); note "FAIL restore drill, see restore-drill.log"
        fi
        sudo -u postgres dropdb --if-exists tp_drill
    else
        FAILED=$((FAILED + 1)); note "FAIL backup run: journalctl -u turbo-pancake-backup"
    fi
    if sudo -u turbo-pancake sh -c "set -a; . $ETC/server.env; exec turbo-pancake-app-server check-db" > "$RESULTS/check-db-live.json" 2>&1; then
        note "PASS check-db on the live database"
    else
        FAILED=$((FAILED + 1)); note "FAIL check-db on the live database, see check-db-live.json"
    fi
    note "timer: $(systemctl list-timers turbo-pancake-backup.timer --no-pager --no-legend | head -n 1)"
fi

if want r; then
    note "== after reboot"
    note "boot: $(uptime -s), service active since $(systemctl show turbo-pancake -p ActiveEnterTimestamp --value)"
    note "service: $(systemctl is-active turbo-pancake), timer: $(systemctl is-active turbo-pancake-backup.timer), readyz: $(probe /readyz)"
    waits="$(journalctl -u turbo-pancake -b --no-pager -o cat | grep -c -i 'database' || true)"
    note "journal lines about the database since boot: $waits (see journal-boot.log)"
    journalctl -u turbo-pancake -b --no-pager -o short-iso | head -n 200 > "$RESULTS/journal-boot.log"
    [ "$(systemctl is-active turbo-pancake)" = "active" ] || FAILED=$((FAILED + 1))
fi

if [ "$FAILED" -eq 0 ]; then note "VERDICT PASS"; else note "VERDICT FAIL ($FAILED)"; fi
note "Reports in $RESULTS. Remove addresses and host names before sharing or committing them."
[ "$FAILED" -eq 0 ]
