#!/bin/sh
# boot-services.sh — single point of start/stop/status for botwork-cloud-server
# services (adapter + schedule) and the watchdog.
#
# Dispatches by init level:
#   L1 systemd   -> systemctl start/stop
#   L2 SysV      -> service <name> start/stop (init scripts in /etc/init.d)
#   L3 fallback  -> source env + setsid nohup with explicit command line,
#                   pid recorded in /root/bot-work/{adapter,schedule}.pid
#
# start is idempotent: skips when /health is ok or the process signature is
# already present (prevents duplicate starts / port conflicts when heal or a
# full pull-up runs while services are already healthy).
#
# Usage:
#   boot-services.sh start [adapter|schedule|watchdog|all]   (no arg = start all)
#   boot-services.sh stop  [adapter|schedule|all]            (no arg = stop both)
#   boot-services.sh status
#
# Constraints (must stay in sync with systemd unit ExecStart / init scripts):
# - adapter:   --listen ws://127.0.0.1:8081 --adapter-port 8082 --base-url <upstream>
#              API key is read from OPENAI_API_KEY via clap env attribute after
#              sourcing cloud-server.env (never passed as --api-key).
# - schedule:  --schedule --health-port 8090
#              MUST NOT take --api-key/--base-url (a complete pair spawns an
#              in-process adapter that steals port 8082) and MUST NOT take a
#              non-default --listen (mutually exclusive with --schedule).
#              Schedule needs no key, so its env file is NOT sourced.

export PATH=/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin
export HOME=/root

BIN=/usr/local/bin/botwork-cloud-server
WORK_DIR=/root/bot-work
ENV_FILE="$WORK_DIR/cloud-server.env"

ADAPTER_WS_PORT=8081
ADAPTER_PORT=8082
SCHEDULE_PORT=8090
UPSTREAM_URL=https://apihub.agnes-ai.com/v1

ADAPTER_PID_FILE="$WORK_DIR/adapter.pid"
SCHEDULE_PID_FILE="$WORK_DIR/schedule.pid"
WATCHDOG_PID_FILE="$WORK_DIR/watchdog.pid"
WATCHDOG_SCRIPT="$WORK_DIR/watchdog.sh"

log() {
    echo "[boot-services] $*"
}

health_ok() {
    curl -fs -m 3 "http://127.0.0.1:$1/health" >/dev/null 2>&1
}

detect_level() {
    if pidof systemd >/dev/null 2>&1 || [ -d /run/systemd/system ]; then
        echo 1
    elif [ -d /etc/init.d ] && command -v service >/dev/null 2>&1; then
        echo 2
    else
        echo 3
    fi
}

adapter_running() {
    health_ok "$ADAPTER_PORT" && return 0
    pgrep -f "adapter-port" >/dev/null 2>&1
}

schedule_running() {
    health_ok "$SCHEDULE_PORT" && return 0
    pgrep -f "botwork-cloud-server --schedule" >/dev/null 2>&1
}

start_adapter() {
    if adapter_running; then
        log "adapter already running, skip"
        return 0
    fi
    case "$LEVEL" in
        1)
            systemctl start botwork-cloud-server.service
            ;;
        2)
            service botwork-cloud-server start
            ;;
        3)
            if [ ! -f "$ENV_FILE" ]; then
                log "env file missing: $ENV_FILE, cannot start adapter"
                return 1
            fi
            . "$ENV_FILE"
            setsid nohup "$BIN" --listen "ws://127.0.0.1:$ADAPTER_WS_PORT" \
                --adapter-port "$ADAPTER_PORT" --base-url "$UPSTREAM_URL" \
                >> "$WORK_DIR/adapter.log" 2>&1 &
            echo $! > "$ADAPTER_PID_FILE" 2>/dev/null
            log "adapter started (pid $(cat "$ADAPTER_PID_FILE" 2>/dev/null))"
            ;;
    esac
}

start_schedule() {
    if schedule_running; then
        log "schedule already running, skip"
        return 0
    fi
    case "$LEVEL" in
        1)
            systemctl start botwork-cloud-server-schedule.service
            ;;
        2)
            service botwork-cloud-server-schedule start
            ;;
        3)
            setsid nohup "$BIN" --schedule --health-port "$SCHEDULE_PORT" \
                >> "$WORK_DIR/schedule.log" 2>&1 &
            echo $! > "$SCHEDULE_PID_FILE" 2>/dev/null
            log "schedule started (pid $(cat "$SCHEDULE_PID_FILE" 2>/dev/null))"
            ;;
    esac
}

start_watchdog() {
    if [ -f "$WATCHDOG_PID_FILE" ]; then
        WPID=$(cat "$WATCHDOG_PID_FILE" 2>/dev/null)
        if [ -n "$WPID" ] && kill -0 "$WPID" 2>/dev/null; then
            log "watchdog already running (pid $WPID), skip"
            return 0
        fi
    fi
    PS_OUT=$(ps -ef 2>/dev/null || ps ax 2>/dev/null || ps 2>/dev/null)
    if echo "$PS_OUT" | grep '[w]atchdog\.sh' >/dev/null 2>&1; then
        log "watchdog process detected, skip"
        return 0
    fi
    if [ ! -f "$WATCHDOG_SCRIPT" ]; then
        log "watchdog.sh missing: $WATCHDOG_SCRIPT"
        return 1
    fi
    setsid nohup sh "$WATCHDOG_SCRIPT" >> "$WORK_DIR/watchdog.log" 2>&1 &
    echo $! > "$WATCHDOG_PID_FILE" 2>/dev/null
    log "watchdog started (pid $(cat "$WATCHDOG_PID_FILE" 2>/dev/null))"
}

stop_adapter() {
    case "$LEVEL" in
        1)
            systemctl stop botwork-cloud-server.service 2>/dev/null
            ;;
        2)
            service botwork-cloud-server stop 2>/dev/null
            ;;
        3)
            if [ -f "$ADAPTER_PID_FILE" ]; then
                APID=$(cat "$ADAPTER_PID_FILE" 2>/dev/null)
                if [ -n "$APID" ]; then
                    kill "$APID" 2>/dev/null
                    i=0
                    while [ "$i" -lt 5 ] && kill -0 "$APID" 2>/dev/null; do
                        sleep 1
                        i=$((i + 1))
                    done
                    kill -0 "$APID" 2>/dev/null && kill -9 "$APID" 2>/dev/null
                fi
                rm -f "$ADAPTER_PID_FILE"
            fi
            pkill -f "adapter-port" 2>/dev/null
            log "adapter stopped"
            ;;
    esac
}

stop_schedule() {
    case "$LEVEL" in
        1)
            systemctl stop botwork-cloud-server-schedule.service 2>/dev/null
            ;;
        2)
            service botwork-cloud-server-schedule stop 2>/dev/null
            ;;
        3)
            if [ -f "$SCHEDULE_PID_FILE" ]; then
                SPID=$(cat "$SCHEDULE_PID_FILE" 2>/dev/null)
                if [ -n "$SPID" ]; then
                    kill "$SPID" 2>/dev/null
                    i=0
                    while [ "$i" -lt 5 ] && kill -0 "$SPID" 2>/dev/null; do
                        sleep 1
                        i=$((i + 1))
                    done
                    kill -0 "$SPID" 2>/dev/null && kill -9 "$SPID" 2>/dev/null
                fi
                rm -f "$SCHEDULE_PID_FILE"
            fi
            pkill -f "botwork-cloud-server --schedule" 2>/dev/null
            log "schedule stopped"
            ;;
    esac
}

do_status() {
    echo "init level: L$LEVEL"
    if adapter_running; then
        echo "adapter:   running (port $ADAPTER_PORT)"
    else
        echo "adapter:   stopped"
    fi
    if schedule_running; then
        echo "schedule:  running (port $SCHEDULE_PORT)"
    else
        echo "schedule:  stopped"
    fi
    WPID=""
    [ -f "$WATCHDOG_PID_FILE" ] && WPID=$(cat "$WATCHDOG_PID_FILE" 2>/dev/null)
    if [ -n "$WPID" ] && kill -0 "$WPID" 2>/dev/null; then
        echo "watchdog:  running (pid $WPID)"
    else
        echo "watchdog:  stopped"
    fi
}

usage() {
    echo "Usage: $0 {start [adapter|schedule|watchdog|all] | stop [adapter|schedule|all] | status}"
}

LEVEL=$(detect_level)

CMD="${1:-start}"
ACTION="${2:-all}"

case "$CMD" in
    start)
        case "$ACTION" in
            adapter)  start_adapter ;;
            schedule) start_schedule ;;
            watchdog) start_watchdog ;;
            all)      start_adapter; start_schedule; start_watchdog ;;
            *) usage; exit 1 ;;
        esac
        ;;
    stop)
        case "$ACTION" in
            adapter)  stop_adapter ;;
            schedule) stop_schedule ;;
            all)      stop_adapter; stop_schedule ;;
            *) usage; exit 1 ;;
        esac
        ;;
    status)
        do_status
        ;;
    *)
        usage
        exit 1
        ;;
esac