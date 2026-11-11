#!/bin/sh
# botwork-cloud-server watchdog v2 — dual-guard /health polling
#
# Polls adapter (default 8082) and schedule (default 8090) /health every 30s.
# Each service has an independent failure counter; 3 consecutive failures of a
# service trigger a restart of that service only (stop + start via
# boot-services.sh, the single point of service command definitions).
#
# Notes:
# - Logs to /root/bot-work/watchdog.log; never prints key material.
# - Single instance: pid file + kill -0 guard; trap TERM/INT removes pid file.
# - Exits immediately when curl is missing (otherwise every health check would
#   fail and the loop would restart services forever).
# - Each round trims adapter.log/schedule.log/watchdog.log over 10MB (chroot
#   environments usually have no logrotate).

WORK_DIR=/root/bot-work
BOOT_SERVICES="$WORK_DIR/boot-services.sh"
ADAPTER_PORT="${BOTWORK_ADAPTER_PORT:-8082}"
SCHEDULE_PORT="${BOTWORK_SCHEDULE_PORT:-8090}"
INTERVAL=30
FAIL_THRESHOLD=3
LOG_MAX_BYTES=$((10 * 1024 * 1024))

PID_FILE="$WORK_DIR/watchdog.pid"
LOG_FILE="$WORK_DIR/watchdog.log"

ADAPTER_FAILS=0
SCHEDULE_FAILS=0

log() {
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] $*" >> "$LOG_FILE" 2>/dev/null
}

health_ok() {
    curl -fs -m 3 "http://127.0.0.1:$1/health" >/dev/null 2>&1
}

trim_log() {
    f="$1"
    [ -f "$f" ] || return 0
    size=$(wc -c < "$f" 2>/dev/null || echo 0)
    if [ "${size:-0}" -gt "$LOG_MAX_BYTES" ]; then
        tail -c 1048576 "$f" > "$f.tmp" 2>/dev/null \
            && cat "$f.tmp" > "$f" 2>/dev/null \
            && rm -f "$f.tmp" 2>/dev/null
    fi
}

restart_service() {
    # stop first: boot-services start skips live processes (health/pgrep
    # heuristic), so a hung process would survive a bare start.
    sh "$BOOT_SERVICES" stop "$1" >/dev/null 2>&1
    sh "$BOOT_SERVICES" start "$1" >/dev/null 2>&1
}

main_loop() {
    while :; do
        if health_ok "$ADAPTER_PORT"; then
            ADAPTER_FAILS=0
        else
            ADAPTER_FAILS=$((ADAPTER_FAILS + 1))
            log "adapter health failed ($ADAPTER_FAILS/$FAIL_THRESHOLD)"
            if [ "$ADAPTER_FAILS" -ge "$FAIL_THRESHOLD" ]; then
                log "restarting adapter"
                restart_service adapter
                ADAPTER_FAILS=0
            fi
        fi

        if health_ok "$SCHEDULE_PORT"; then
            SCHEDULE_FAILS=0
        else
            SCHEDULE_FAILS=$((SCHEDULE_FAILS + 1))
            log "schedule health failed ($SCHEDULE_FAILS/$FAIL_THRESHOLD)"
            if [ "$SCHEDULE_FAILS" -ge "$FAIL_THRESHOLD" ]; then
                log "restarting schedule"
                restart_service schedule
                SCHEDULE_FAILS=0
            fi
        fi

        trim_log "$WORK_DIR/adapter.log"
        trim_log "$WORK_DIR/schedule.log"
        trim_log "$LOG_FILE"

        sleep "$INTERVAL"
    done
}

# --- single-instance guard ---------------------------------------------------
if [ -f "$PID_FILE" ]; then
    OLD_PID=$(cat "$PID_FILE" 2>/dev/null)
    if [ -n "$OLD_PID" ] && kill -0 "$OLD_PID" 2>/dev/null; then
        log "watchdog already running (pid $OLD_PID), exiting"
        exit 0
    fi
fi

# --- mandatory dependencies --------------------------------------------------
if ! command -v curl >/dev/null 2>&1; then
    log "curl not found, watchdog cannot run (install curl first)"
    exit 1
fi
if [ ! -f "$BOOT_SERVICES" ]; then
    log "boot-services.sh missing at $BOOT_SERVICES, watchdog cannot run"
    exit 1
fi

mkdir -p "$WORK_DIR" 2>/dev/null
echo $$ > "$PID_FILE" 2>/dev/null
trap 'rm -f "$PID_FILE" 2>/dev/null; exit 0' TERM INT

log "watchdog started (pid $$, adapter=$ADAPTER_PORT schedule=$SCHEDULE_PORT)"
main_loop