#!/bin/bash
# install-cloud-server.sh — app-driven installer for botwork-cloud-server
#
# State machine (phases):
#   0 deploy companion scripts (boot-services.sh / watchdog.sh -> /root/bot-work)
#   1 dependency check   (curl/bash/tar/gzip/sha256sum; md5sum optional)
#   2 detect             (--version + dual /health + env hash -> short-circuit)
#   3 download           (arch detect -> tar.gz + .sha256 -> verify -> install .bak)
#   4 service            (env validation -> level-aware start + persistence)
#   5 autostart          (L1/L2 idempotent re-check; L3 rc.local/.bashrc hooks)
#   6 watchdog           (kill old instances -> start new one)
#   7 verify             (dual /health x3 -> one restart round -> rollback on upgrade)
#
# Output contract: exactly one line  BOTWORK_INSTALL_RESULT=ok
#                  or                BOTWORK_INSTALL_RESULT=fail detail=<enum>
#   enum: dependency_missing | download_failed | sha256_mismatch |
#         install_failed | verify_failed | rollback_failed
#
# Self-contained: does NOT source common.sh. The Rust caller uploads this file
# together with boot-services.sh and watchdog.sh into /tmp/botwork-install/,
# and writes /root/bot-work/cloud-server.env (600) BEFORE invoking this script.
#
# Usage:
#   bash install-cloud-server.sh <min_version> [adapter_port] [schedule_port] [upstream_url]
# Env override: BOTWORK_RELEASE_BASE_URL (default: GitHub releases of
#   github.com/lai-tan/botwork-cloud-server-dist — public dist repo, source repo stays private)

set -uo pipefail

MIN_VERSION="${1:-0.1.0}"
ADAPTER_PORT="${2:-8082}"
SCHEDULE_PORT="${3:-8090}"
UPSTREAM_URL="${4:-https://apihub.agnes-ai.com/v1}"
ADAPTER_WS_PORT=8081

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
WORK_DIR=/root/bot-work
BIN=/usr/local/bin/botwork-cloud-server
ENV_FILE="$WORK_DIR/cloud-server.env"
ENV_HASH_FILE="$WORK_DIR/.env.md5"
INSTALL_DIR=/tmp/botwork-install
ASSETS_DIR="$INSTALL_DIR/assets"
BACKUP="$BIN.bak"
BOOT="$WORK_DIR/boot-services.sh"

BASE_URL="${BOTWORK_RELEASE_BASE_URL:-https://github.com/lai-tan/botwork-cloud-server-dist/releases/download}"

RESULT_EMITTED=0

fail() {
    detail="$1"
    msg="${2:-}"
    if [ "$RESULT_EMITTED" -eq 0 ]; then
        echo "BOTWORK_INSTALL_RESULT=fail detail=$detail"
        RESULT_EMITTED=1
    fi
    [ -n "$msg" ] && echo "[install] $msg"
    exit 1
}

finish_ok() {
    echo "BOTWORK_INSTALL_RESULT=ok"
    RESULT_EMITTED=1
    exit 0
}

# --- health helpers ----------------------------------------------------------
adapter_health() {
    curl -fs -m 3 "http://127.0.0.1:$ADAPTER_PORT/health" >/dev/null 2>&1
}

schedule_health() {
    curl -fs -m 3 "http://127.0.0.1:$SCHEDULE_PORT/health" >/dev/null 2>&1
}

# --- version helpers (no sort -V: alpine/busybox lacks it) -------------------
version_ge() {
    printf '%s\n%s\n' "$1" "$2" \
        | sort -t. -k1,1n -k2,2n -k3,3n | head -n 1 | grep -qx "$2"
}

hash_env() {
    if [ "$HASH_TOOL" = "md5sum" ]; then
        md5sum "$ENV_FILE" 2>/dev/null | awk '{print $1}'
    else
        sha256sum "$ENV_FILE" 2>/dev/null | awk '{print $1}'
    fi
}

# =============================================================================
# phase 0: deploy companion scripts
# =============================================================================
echo "[install] phase 0/7: deploy companion scripts"
mkdir -p "$WORK_DIR" "$INSTALL_DIR" "$ASSETS_DIR" || fail install_failed "cannot create $WORK_DIR / $INSTALL_DIR"
for f in boot-services.sh watchdog.sh; do
    if [ -f "$SCRIPT_DIR/$f" ]; then
        cp "$SCRIPT_DIR/$f" "$WORK_DIR/$f" || fail install_failed "cannot copy $f to $WORK_DIR"
        chmod +x "$WORK_DIR/$f" 2>/dev/null
        echo "[install] deployed $WORK_DIR/$f"
    else
        fail install_failed "companion script $f missing next to install-cloud-server.sh in $SCRIPT_DIR; re-run the agent creation to re-upload assets"
    fi
done

# =============================================================================
# phase 1: dependency check (must precede detection: health probes need curl)
# =============================================================================
echo "[install] phase 1/7: dependency check"
MISSING_DEPS=""
for dep in curl bash tar gzip sha256sum; do
    command -v "$dep" >/dev/null 2>&1 || MISSING_DEPS="$MISSING_DEPS $dep"
done
if [ -n "$MISSING_DEPS" ]; then
    fail dependency_missing "missing dependencies:$MISSING_DEPS; install them via the device package manager (apt/apk/opkg) and retry"
fi
if command -v md5sum >/dev/null 2>&1; then
    HASH_TOOL=md5sum
else
    HASH_TOOL=sha256sum
    echo "[install] md5sum not found, using sha256sum for env hash"
fi
echo "[install] dependencies ok"

# =============================================================================
# phase 2: detect current state
# =============================================================================
echo "[install] phase 2/7: detect current state"
NEED_DOWNLOAD=0
NEED_RESTART=0
DID_INSTALL=0

CURRENT_VERSION=""
if [ -x "$BIN" ]; then
    VERSION_OUTPUT="$("$BIN" --version 2>/dev/null || true)"
    CURRENT_VERSION="$(echo "$VERSION_OUTPUT" | awk '{print $2}')"
fi

if [ ! -x "$BIN" ] || [ -z "$CURRENT_VERSION" ]; then
    echo "[install] binary missing or --version failed (corrupt?) -> download"
    NEED_DOWNLOAD=1
elif ! version_ge "$CURRENT_VERSION" "$MIN_VERSION"; then
    echo "[install] installed version $CURRENT_VERSION < expected $MIN_VERSION -> upgrade"
    if ! (adapter_health && schedule_health); then
        echo "[install] old services unhealthy, starting them before upgrade (healthy baseline for rollback judgement)"
        sh "$BOOT" start adapter  >/dev/null 2>&1 || true
        sh "$BOOT" start schedule >/dev/null 2>&1 || true
        sleep 2
    fi
    NEED_DOWNLOAD=1
else
    if adapter_health && schedule_health && [ -f "$ENV_FILE" ]; then
        CURRENT_HASH="$(hash_env || echo none)"
        RECORDED_HASH="$(cat "$ENV_HASH_FILE" 2>/dev/null || echo none)"
        if [ "$CURRENT_HASH" = "$RECORDED_HASH" ]; then
            echo "[install] binary $CURRENT_VERSION healthy, env unchanged -> idempotent short-circuit"
            finish_ok
        else
            echo "[install] env hash changed -> restart services to apply new config"
            NEED_RESTART=1
        fi
    else
        echo "[install] binary $CURRENT_VERSION present but service unhealthy or env missing -> restart"
        NEED_RESTART=1
    fi
fi

# =============================================================================
# phase 3: download + verify + install binary
# =============================================================================
if [ "$NEED_DOWNLOAD" -eq 1 ]; then
    echo "[install] phase 3/7: download and install binary"
    ARCH="$(uname -m)"
    case "$ARCH" in
        aarch64|arm64) TARGET_ARCH=aarch64 ;;
        x86_64|amd64)  TARGET_ARCH=x86_64 ;;
        *)
            fail install_failed "unsupported architecture: $ARCH (supported: aarch64/x86_64, glibc)"
            ;;
    esac
    TAG="cloud-server-v$MIN_VERSION"
    PKG_NAME="botwork-cloud-server-${TARGET_ARCH}-unknown-linux-gnu.tar.gz"
    DL_URL="$BASE_URL/$TAG/$PKG_NAME"
    echo "[install] architecture $ARCH, downloading $DL_URL"

    DL_FAIL=1
    SHA_FAIL=0
    try_download() {
        DL_FAIL=1
        SHA_FAIL=0
        echo "[install] downloading package..."
        curl -fL --progress-bar -m 120 -o "$ASSETS_DIR/$PKG_NAME" "$DL_URL" || return 1
        DL_FAIL=0
        echo "[install] downloading checksum..."
        curl -fsSL -m 60 -o "$ASSETS_DIR/$PKG_NAME.sha256" "$DL_URL.sha256" || return 1
        DL_FAIL=0
        echo "[install] verifying sha256..."
        if (cd "$ASSETS_DIR" && sha256sum -c "$PKG_NAME.sha256" >/dev/null 2>&1); then
            return 0
        fi
        SHA_FAIL=1
        return 1
    }

    if ! try_download; then
        echo "[install] first attempt failed (download=$DL_FAIL sha=$SHA_FAIL), retrying once"
        if ! try_download; then
            if [ "$SHA_FAIL" -eq 1 ]; then
                fail sha256_mismatch "package integrity check failed after retry; the download may be corrupted or tampered"
            fi
            fail download_failed "download failed after retry: $DL_URL ; if GitHub is unreachable, set BOTWORK_RELEASE_BASE_URL to a reachable mirror and retry"
        fi
    fi

    echo "[install] extracting package"
    tar -xzf "$ASSETS_DIR/$PKG_NAME" -C "$ASSETS_DIR" || fail install_failed "failed to extract $PKG_NAME"
    NEW_BIN="$(find "$ASSETS_DIR" -maxdepth 3 -type f ! -name '*.sha256' ! -name '*.tar.gz' ! -name '*.sh' | head -n 1)"
    [ -n "$NEW_BIN" ] || fail install_failed "no binary found inside $PKG_NAME"
    chmod +x "$NEW_BIN" 2>/dev/null
    if ! "$NEW_BIN" --version >/dev/null 2>&1; then
        fail install_failed "extracted binary is not runnable on this device (glibc/arch mismatch?)"
    fi
    echo "[install] extracted binary ok: $NEW_BIN"

    if [ -f "$BIN" ]; then
        echo "[install] backing up existing binary to $BACKUP"
        cp "$BIN" "$BIN.tmp.bak" && mv -f "$BIN.tmp.bak" "$BACKUP" \
            || echo "[install] WARN: backup failed, rollback will be unavailable"
    fi
    cp "$NEW_BIN" "$BIN.new" && mv -f "$BIN.new" "$BIN" && chmod 755 "$BIN" \
        || fail install_failed "failed to install binary to $BIN"
    "$BIN" --version >/dev/null 2>&1 || fail install_failed "installed binary still not runnable at $BIN"
    DID_INSTALL=1
    NEED_RESTART=1
    echo "[install] binary installed: $( "$BIN" --version 2>/dev/null || echo "$BIN" )"
else
    echo "[install] phase 3/7: skipped (binary up to date)"
fi

# =============================================================================
# phase 4: service configuration (level-aware start + persistence, v1.9)
# =============================================================================
echo "[install] phase 4/7: service configuration"

if [ ! -f "$ENV_FILE" ]; then
    fail install_failed "env file missing: $ENV_FILE (must be written before running this script)"
fi
if ! grep -Eq '^OPENAI_API_KEY=.+$' "$ENV_FILE"; then
    fail install_failed "env file invalid: OPENAI_API_KEY line missing or empty in $ENV_FILE"
fi

LEVEL=3
if pidof systemd >/dev/null 2>&1 || [ -d /run/systemd/system ]; then
    LEVEL=1
elif [ -d /etc/init.d ] && command -v service >/dev/null 2>&1; then
    LEVEL=2
fi
echo "[install] detected init level: L$LEVEL"

if [ "$NEED_RESTART" -eq 1 ]; then
    echo "[install] stopping services (install/restart/config-change)"
    sh "$BOOT" stop adapter  >/dev/null 2>&1 || true
    sh "$BOOT" stop schedule >/dev/null 2>&1 || true
fi

if [ "$LEVEL" -eq 1 ]; then
    echo "[install] writing systemd units"
    cat > /etc/systemd/system/botwork-cloud-server.service <<UNIT_EOF
[Unit]
Description=botwork-cloud-server adapter
After=network.target

[Service]
Type=simple
Environment=HOME=/root
EnvironmentFile=$ENV_FILE
ExecStart=$BIN --listen ws://127.0.0.1:$ADAPTER_WS_PORT --adapter-port $ADAPTER_PORT --base-url $UPSTREAM_URL
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
UNIT_EOF

    cat > /etc/systemd/system/botwork-cloud-server-schedule.service <<UNIT_EOF
[Unit]
Description=botwork-cloud-server schedule
After=network.target

[Service]
Type=simple
Environment=HOME=/root
ExecStart=$BIN --schedule --health-port $SCHEDULE_PORT
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
UNIT_EOF

    systemctl daemon-reload 2>/dev/null || true
    systemctl enable botwork-cloud-server.service botwork-cloud-server-schedule.service >/dev/null 2>&1 || true
    echo "[install] starting services via systemctl"
    systemctl start botwork-cloud-server.service botwork-cloud-server-schedule.service 2>&1 | sed 's/^/[install] systemctl: /' || true

elif [ "$LEVEL" -eq 2 ]; then
    echo "[install] writing SysV init scripts"
    cat > /etc/init.d/botwork-cloud-server <<'INIT_EOF'
#!/bin/sh
### BEGIN INIT INFO
# Provides:          botwork-cloud-server
# Required-Start:    $network
# Required-Stop:     $network
# Default-Start:     2 3 4 5
# Default-Stop:      0 1 6
# Short-Description: botwork-cloud-server adapter
### END INIT INFO

export HOME=/root
export PATH=/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin
ENV_FILE=/root/bot-work/cloud-server.env
BIN=/usr/local/bin/botwork-cloud-server
PIDFILE=/root/bot-work/adapter.pid
LOGFILE=/root/bot-work/adapter.log
DAEMON_ARGS="--listen ws://127.0.0.1:8081 --adapter-port 8082 --base-url https://apihub.agnes-ai.com/v1"

is_running() {
    curl -fs -m 3 http://127.0.0.1:8082/health >/dev/null 2>&1 && return 0
    pgrep -f "adapter-port" >/dev/null 2>&1
}

case "$1" in
    start)
        if is_running; then
            echo "botwork-cloud-server already running"
            exit 0
        fi
        if [ ! -f "$ENV_FILE" ]; then
            echo "env file missing: $ENV_FILE"
            exit 1
        fi
        . "$ENV_FILE"
        setsid nohup "$BIN" $DAEMON_ARGS >> "$LOGFILE" 2>&1 &
        echo $! > "$PIDFILE"
        echo "botwork-cloud-server started"
        ;;
    stop)
        if [ -f "$PIDFILE" ]; then
            kill "$(cat "$PIDFILE")" 2>/dev/null
            rm -f "$PIDFILE"
        fi
        pkill -f "adapter-port" 2>/dev/null
        echo "botwork-cloud-server stopped"
        ;;
    restart)
        "$0" stop
        sleep 1
        "$0" start
        ;;
    status)
        if is_running; then
            echo "botwork-cloud-server is running"
            exit 0
        fi
        echo "botwork-cloud-server is stopped"
        exit 3
        ;;
    *)
        echo "Usage: $0 {start|stop|restart|status}"
        exit 1
        ;;
esac
exit 0
INIT_EOF
    chmod +x /etc/init.d/botwork-cloud-server

    cat > /etc/init.d/botwork-cloud-server-schedule <<'INIT_EOF'
#!/bin/sh
### BEGIN INIT INFO
# Provides:          botwork-cloud-server-schedule
# Required-Start:    $network
# Required-Stop:     $network
# Default-Start:     2 3 4 5
# Default-Stop:      0 1 6
# Short-Description: botwork-cloud-server schedule
### END INIT INFO

export HOME=/root
export PATH=/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin
BIN=/usr/local/bin/botwork-cloud-server
PIDFILE=/root/bot-work/schedule.pid
LOGFILE=/root/bot-work/schedule.log
DAEMON_ARGS="--schedule --health-port 8090"

is_running() {
    curl -fs -m 3 http://127.0.0.1:8090/health >/dev/null 2>&1 && return 0
    pgrep -f "botwork-cloud-server --schedule" >/dev/null 2>&1
}

case "$1" in
    start)
        if is_running; then
            echo "botwork-cloud-server-schedule already running"
            exit 0
        fi
        setsid nohup "$BIN" $DAEMON_ARGS >> "$LOGFILE" 2>&1 &
        echo $! > "$PIDFILE"
        echo "botwork-cloud-server-schedule started"
        ;;
    stop)
        if [ -f "$PIDFILE" ]; then
            kill "$(cat "$PIDFILE")" 2>/dev/null
            rm -f "$PIDFILE"
        fi
        pkill -f "botwork-cloud-server --schedule" 2>/dev/null
        echo "botwork-cloud-server-schedule stopped"
        ;;
    restart)
        "$0" stop
        sleep 1
        "$0" start
        ;;
    status)
        if is_running; then
            echo "botwork-cloud-server-schedule is running"
            exit 0
        fi
        echo "botwork-cloud-server-schedule is stopped"
        exit 3
        ;;
    *)
        echo "Usage: $0 {start|stop|restart|status}"
        exit 1
        ;;
esac
exit 0
INIT_EOF
    chmod +x /etc/init.d/botwork-cloud-server-schedule

    if command -v update-rc.d >/dev/null 2>&1; then
        update-rc.d botwork-cloud-server defaults >/dev/null 2>&1 || true
        update-rc.d botwork-cloud-server-schedule defaults >/dev/null 2>&1 || true
    elif command -v chkconfig >/dev/null 2>&1; then
        chkconfig botwork-cloud-server on >/dev/null 2>&1 || true
        chkconfig botwork-cloud-server-schedule on >/dev/null 2>&1 || true
    fi
    echo "[install] starting services via service(8)"
    service botwork-cloud-server start 2>&1 | sed 's/^/[install] service: /' || true
    service botwork-cloud-server-schedule start 2>&1 | sed 's/^/[install] service: /' || true

else
    echo "[install] starting services via boot-services.sh (L3)"
    sh "$BOOT" start adapter  2>&1 | sed 's/^/[install] /' || true
    sh "$BOOT" start schedule 2>&1 | sed 's/^/[install] /' || true
fi

# record env hash AFTER services (re)started: the detect phase compares this
# file to skip restarts when config is unchanged
hash_env > "$ENV_HASH_FILE" 2>/dev/null || true

# =============================================================================
# phase 5: autostart persistence
# =============================================================================
echo "[install] phase 5/7: autostart persistence"
if [ "$LEVEL" -eq 1 ]; then
    systemctl is-enabled botwork-cloud-server.service >/dev/null 2>&1 \
        || systemctl enable botwork-cloud-server.service >/dev/null 2>&1 || true
    systemctl is-enabled botwork-cloud-server-schedule.service >/dev/null 2>&1 \
        || systemctl enable botwork-cloud-server-schedule.service >/dev/null 2>&1 || true
    echo "[install] L1: systemd units enabled (start=persist unified in phase 4)"
elif [ "$LEVEL" -eq 2 ]; then
    if [ ! -f /etc/init.d/botwork-cloud-server ] || [ ! -f /etc/init.d/botwork-cloud-server-schedule ]; then
        echo "[install] WARN: init scripts missing despite L2 (unexpected)"
    fi
    echo "[install] L2: init scripts registered (start=persist unified in phase 4)"
else
    MARK="# BOTWORK-AUTOSTART"
    HOOK_LINE="$WORK_DIR/boot-services.sh start all >/dev/null 2>&1"
    if [ -f /etc/rc.local ]; then
        if grep -q "$MARK" /etc/rc.local 2>/dev/null; then
            echo "[install] rc.local hook already present"
        elif grep -q '^exit 0' /etc/rc.local 2>/dev/null; then
            sed -i "/^exit 0/i $HOOK_LINE" /etc/rc.local
            sed -i "/^exit 0/i $MARK" /etc/rc.local
        else
            { echo "$MARK"; echo "$HOOK_LINE"; } >> /etc/rc.local
        fi
    else
        printf '#!/bin/sh\n%s\n%s\nexit 0\n' "$MARK" "$HOOK_LINE" > /etc/rc.local
        chmod +x /etc/rc.local
    fi
    if grep -q "$MARK" /root/.bashrc 2>/dev/null; then
        echo "[install] .bashrc hook already present"
    else
        { echo "$MARK"; echo "$HOOK_LINE"; } >> /root/.bashrc
    fi
    echo "[install] L3: rc.local/.bashrc hooks configured (marker-deduped)"
fi

# =============================================================================
# phase 6: watchdog
# =============================================================================
echo "[install] phase 6/7: watchdog"
if [ -f "$WORK_DIR/watchdog.pid" ]; then
    WPID="$(cat "$WORK_DIR/watchdog.pid" 2>/dev/null)"
    [ -n "$WPID" ] && kill "$WPID" 2>/dev/null
    rm -f "$WORK_DIR/watchdog.pid"
fi
OLD_W_PIDS="$(pgrep -f '[w]atchdog\.sh' 2>/dev/null || true)"
for wp in $OLD_W_PIDS; do
    kill "$wp" 2>/dev/null || true
done
sleep 1
for wp in $OLD_W_PIDS; do
    kill -9 "$wp" 2>/dev/null || true
done
sh "$BOOT" start watchdog 2>&1 | sed 's/^/[install] /' || true

# =============================================================================
# phase 7: verify
# =============================================================================
echo "[install] phase 7/7: verify"

do_verify() {
    i=1
    while [ "$i" -le 3 ]; do
        if adapter_health && schedule_health; then
            return 0
        fi
        echo "[install] health check attempt $i/3 failed, waiting 2s"
        sleep 2
        i=$((i + 1))
    done
    return 1
}

restart_both() {
    sh "$BOOT" stop adapter  >/dev/null 2>&1 || true
    sh "$BOOT" stop schedule >/dev/null 2>&1 || true
    sleep 1
    sh "$BOOT" start adapter  >/dev/null 2>&1 || true
    sh "$BOOT" start schedule >/dev/null 2>&1 || true
}

if do_verify; then
    echo "[install] verify passed"
    finish_ok
fi

echo "[install] verify failed, restarting services and re-verifying once"
restart_both
sleep 2
if do_verify; then
    echo "[install] verify passed after restart"
    finish_ok
fi

echo "[install] verify still failing; last adapter.log lines:"
tail -n 5 "$WORK_DIR/adapter.log" 2>/dev/null || true
echo "[install] last schedule.log lines:"
tail -n 5 "$WORK_DIR/schedule.log" 2>/dev/null || true

if [ "$DID_INSTALL" -eq 1 ] && [ -f "$BACKUP" ]; then
    echo "[install] rolling back to previous binary"
    cp "$BACKUP" "$BIN.rollback" && mv -f "$BIN.rollback" "$BIN" && chmod 755 "$BIN" \
        || fail rollback_failed "rollback copy failed; manual fix: cp $BACKUP $BIN then restart services"
    restart_both
    sleep 2
    if do_verify; then
        fail verify_failed "new version failed verification; rolled back to previous version and service is healthy again (expected version NOT satisfied, re-run installation after fixing)"
    fi
    fail rollback_failed "rollback did not restore health; check $WORK_DIR/adapter.log and $WORK_DIR/schedule.log"
fi

fail verify_failed "service failed to become healthy; check $WORK_DIR/adapter.log and $WORK_DIR/schedule.log (viewable via the app file manager)"