#!/usr/bin/env bash
# Benchmark HAProxy (with the SPOA IP-reputation agent) in Docker over loopback HTTP.
#
# Workflow:
#   1. `docker compose up -d` (containers are NOT stopped afterwards).
#   2. Wait until HAProxy answers on 127.0.0.1:8080.
#   3. Temporarily patch the SPOE config so the agent receives the random IP sent in the
#      `ip` URL parameter (`args ip=src` -> `args ip=urlp(ip)`), and shorten the stick-table
#      expiry so the cache churns (`expire 10s` -> `expire 1s`). The original files are backed
#      up to temporary files.
#   4. Restart the haproxy container so it re-reads the mounted, patched config.
#   5. Run `wrk` for BENCH_DOCKER_DURATION seconds with BENCH_DOCKER_CONNECTIONS concurrent
#      connections, hitting `/?ip=<random IPv4>` on every request, and print the summary.
#   6. On success, error or SIGINT/SIGTERM a trap restores the two config files and restarts
#      haproxy so it reverts to the original configuration; no files are left behind.

set -euo pipefail

COMPOSE=(docker compose)
URL_BASE="${BENCH_DOCKER_URL:-http://127.0.0.1:8080}"
# Readiness probe. Port 8080 cannot be used: the source IP seen by HAProxy (the Docker bridge
# gateway) is itself in the blocklist, so requests get `silent-drop` (no HTTP reply). The
# exporter frontend on 8404 answers 200 regardless of IP reputation and proves HAProxy is up.
READY_URL="${BENCH_DOCKER_READY_URL:-http://127.0.0.1:8404/metrics}"
DURATION="${BENCH_DOCKER_DURATION:-60}"
CONNECTIONS="${BENCH_DOCKER_CONNECTIONS:-300}"
THREADS="${BENCH_DOCKER_THREADS:-4}"
WRK_BIN="${WRK:-wrk}"

HAPROXY_CFG="haproxy/haproxy.cfg"
SPOE_CFG="haproxy/haproxy-spoa-ip-reputation-firehol.cfg"

HAPROXY_BAK="$(mktemp)"
SPOE_BAK="$(mktemp)"
LUA_TMP="$(mktemp)"

patched=0
# Set to 1 only after the originals have been copied to the backups; guards the trap against
# restoring from an empty backup if we fail before the copy (mktemp pre-creates empty files).
backup_taken=0

log() { printf '[bench-docker] %s\n' "$*"; }

die() {
    printf '[bench-docker] ERROR: %s\n' "$*" >&2
    exit 1
}

wait_ready() {
    local deadline=$(( $(date +%s) + 60 ))
    until curl -fsS -o /dev/null --max-time 2 "$READY_URL" 2>/dev/null; do
        if (( $(date +%s) >= deadline )); then
            die "HAProxy did not become ready on ${READY_URL} (is '${COMPOSE[*]} up -d' running?)"
        fi
        sleep 1
    done
    log "HAProxy is ready (${READY_URL} answered OK)"
}

restore_configs() {
    # Idempotent: only touch a file when the originals were backed up AND a backup actually
    # differs from the current content.
    [[ "$backup_taken" -eq 1 ]] || return 0
    local changed=0
    if [[ -f "$HAPROXY_BAK" ]] && ! cmp -s "$HAPROXY_BAK" "$HAPROXY_CFG"; then
        mv -f "$HAPROXY_BAK" "$HAPROXY_CFG"
        log "Restored $HAPROXY_CFG"
        changed=1
    fi
    if [[ -f "$SPOE_BAK" ]] && ! cmp -s "$SPOE_BAK" "$SPOE_CFG"; then
        mv -f "$SPOE_BAK" "$SPOE_CFG"
        log "Restored $SPOE_CFG"
        changed=1
    fi
    rm -f "$HAPROXY_BAK" "$SPOE_BAK" "$LUA_TMP"
    if [[ "$changed" -eq 1 && "$patched" -eq 1 ]]; then
        log "Restarting haproxy to revert to the original configuration"
        "${COMPOSE[@]}" restart haproxy >/dev/null
        wait_ready
    fi
}

# Always restore (normal exit, or `set -e` failure)...
trap restore_configs EXIT
# ...and react to cancellation: restore, then propagate the signal's exit status.
trap 'trap - INT TERM; restore_configs; exit 130' INT
trap 'trap - INT TERM; restore_configs; exit 143' TERM

patch_file() {
    local file="$1" needle="$2" replacement="$3"
    if ! grep -qF -- "$needle" "$file"; then
        die "pattern not found in $file: '$needle'"
    fi
    sed -i "s|$(printf '%s' "$needle" | sed 's/[&|\\]/\\&/g')|$(printf '%s' "$replacement" | sed 's/[&|\\]/\\&/g')|" "$file"
}

log "Starting docker compose services"
"${COMPOSE[@]}" up -d
wait_ready

# Backup the originals before any modification.
cp -p "$HAPROXY_CFG" "$HAPROXY_BAK"
cp -p "$SPOE_CFG" "$SPOE_BAK"
backup_taken=1

# Point the SPOA agent at the random IP sent in the `ip` URL parameter.
log "Patching $SPOE_CFG: 'args ip=src' -> 'args ip=urlp(ip)'"
patch_file "$SPOE_CFG" "args ip=src" "args ip=urlp(ip)"

# Shorten the stick-table expiry so the (random) IP cache churns during the load.
log "Patching $HAPROXY_CFG: 'expire 10s' -> 'expire 1s'"
patch_file "$HAPROXY_CFG" "stick-table type ip size 1m expire 10s store gpt0" \
    "stick-table type ip size 1m expire 1s store gpt0"

patched=1
log "Restarting haproxy to apply the patched configuration"
"${COMPOSE[@]}" restart haproxy >/dev/null
wait_ready

# wrk Lua script: build a unique URL carrying a random IPv4 on every request.
cat > "$LUA_TMP" <<'LUA'
function request()
    local a = math.random(1, 255)
    local b = math.random(0, 255)
    local c = math.random(0, 255)
    local d = math.random(1, 255)
    return wrk.format("GET", "/?ip=" .. a .. "." .. b .. "." .. c .. "." .. d)
end
LUA

log "Running wrk for ${DURATION}s with ${CONNECTIONS} concurrent connections against ${URL_BASE}/?ip=<random>"
log "Command: $WRK_BIN -t$THREADS -c$CONNECTIONS -d${DURATION}s -s $LUA_TMP \"$URL_BASE\""
echo
"$WRK_BIN" -t"$THREADS" -c"$CONNECTIONS" -d"${DURATION}s" -s "$LUA_TMP" "$URL_BASE" ||
    die "wrk benchmark failed."
echo
log "Benchmark finished; containers left running. Config files were restored."
