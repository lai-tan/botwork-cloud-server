#!/bin/bash
# install-schedule-daemon.sh — botwork-cloud-schedule 调度守护进程统一安装器
# 设计依据：AUTOMATION-DESIGN.md §5.2 部署三级回退 + §9 步骤 23b
#
# 三级回退（逐级检测）：
#   优先级 1：systemd  （command -v systemctl 且 /run/systemd/system 存在）
#   优先级 2：SysV init + service（无 systemd 但 service 可用——目标 chroot 环境即此级别）
#   优先级 3：nohup + setsid + watchdog（service 也不可用）
#
# 前置条件：
#   - botwork-cloud-server（含 --schedule 支持）已编译安装到 /usr/local/bin
#   - 本目录（deploy/schedule/）与 watchdog.sh、botwork-cloud-schedule.init 在一起
#
# 用法：
#   SCHEDULE_API_KEY=sk-... ./install-schedule-daemon.sh
#   其余配置可用环境变量覆盖（见下方默认值），或先安装后编辑 /root/bot-work/schedule-daemon.env
#   （env 文件改动后须重启服务生效）

set -euo pipefail

# ===== 配置（环境变量可覆盖）=====
API_KEY="${SCHEDULE_API_KEY:?SCHEDULE_API_KEY 未设置（用法：SCHEDULE_API_KEY=sk-... $0）}"
BASE_URL="${SCHEDULE_BASE_URL:-https://apihub.agnes-ai.com/v1}"
HEALTH_PORT="${SCHEDULE_HEALTH_PORT:-8081}"
CONFIG_PATH="${SCHEDULE_CONFIG:-/root/bot-work/schedules.json}"
BOT_WEB_URL="${SCHEDULE_BOT_WEB_URL:-https://bot.xqx.us.kg/api/v1}"
MAX_CONCURRENT="${SCHEDULE_MAX_CONCURRENT:-5}"
HANDOFF_SLOTS="${SCHEDULE_HANDOFF_SLOTS:-2}"
MAX_EXECUTION_TIME="${SCHEDULE_MAX_EXECUTION_TIME:-1800}"
API_TOKEN="${SCHEDULE_API_TOKEN:-}"

BIN=/usr/local/bin/botwork-cloud-server
WORKDIR=/root/bot-work
ENV_FILE="$WORKDIR/schedule-daemon.env"
LEVEL_FILE="$WORKDIR/deploy-level"
INIT_NAME=botwork-cloud-schedule
INIT_FILE="/etc/init.d/$INIT_NAME"
UNIT_FILE="/etc/systemd/system/${INIT_NAME}.service"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

info() { echo -e "\033[0;32m[INFO]\033[0m $*"; }
warn() { echo -e "\033[1;33m[WARN]\033[0m $*"; }
die()  { echo -e "\033[0;31m[ERR ]\033[0m $*" >&2; exit 1; }

# ===== 0. 前置检查 =====
[ "$(id -u)" -eq 0 ] || die "须 root 运行"
[ -x "$BIN" ] || die "$BIN 不存在（先编译安装：cd /root/botwork-cloud-server && cargo build --release && cp target/release/botwork-cloud-server /usr/local/bin/）"
"$BIN" --help 2>&1 | grep -q -- '--schedule' || die "$BIN 不支持 --schedule（旧版二进制，须用含 scheduler 模块的源码重新编译）"
command -v curl >/dev/null 2>&1 || die "curl 未安装"
[ -f "$SCRIPT_DIR/watchdog.sh" ] || die "缺少 $SCRIPT_DIR/watchdog.sh（部署文件不完整）"

# ===== 1. 管理 API 端口冲突检测与既有 ws 服务迁移 =====
# 既有 botwork-cloud-server --listen ws://127.0.0.1:$HEALTH_PORT 常驻服务占用管理 API 端口。
# 迁移策略：LISTEN 改 off 释放端口。调度进程自带进程内 adapter（--base-url，随机端口），
# 不依赖既有服务的 8082 adapter。
migrate_legacy_ws() {
    local ws_pid
    ws_pid=$(pgrep -f "botwork-cloud-server --listen ws://127.0.0.1:${HEALTH_PORT}" | head -1 || true)
    [ -z "$ws_pid" ] && return 0
    warn "检测到既有 ws 服务 (pid $ws_pid) 占用 ${HEALTH_PORT}，迁移为 --listen off（adapter 保留，codex 不受影响）"
    if [ -f "$UNIT_FILE" ] && grep -q "botwork-cloud-server --listen ws" "$UNIT_FILE"; then
        sed -i "s#--listen ws://127.0.0.1:${HEALTH_PORT}#--listen off#g" /etc/systemd/system/botwork-cloud-server.service
        systemctl daemon-reload
        systemctl restart botwork-cloud-server
    elif [ -f /etc/init.d/botwork-cloud-server ] && grep -q 'LISTEN_ADDR="ws://' /etc/init.d/botwork-cloud-server; then
        sed -i "s#^LISTEN_ADDR=.*#LISTEN_ADDR=\"off\"#" /etc/init.d/botwork-cloud-server
        if command -v service >/dev/null 2>&1; then
            service botwork-cloud-server restart || /etc/init.d/botwork-cloud-server restart
        else
            /etc/init.d/botwork-cloud-server restart
        fi
    else
        # 裸 nohup 进程（无 init/unit 配置）：直接停止，后续如需 adapter 手动以 --listen off 拉起
        kill "$ws_pid" 2>/dev/null || true
        warn "既有 ws 服务为裸进程（无自启配置），已停止；如仍需 adapter，请以 --listen off 手动拉起"
    fi
    sleep 2
    if pgrep -f "botwork-cloud-server --listen ws://127.0.0.1:${HEALTH_PORT}" >/dev/null 2>&1; then
        die "迁移后仍有进程监听 ws://127.0.0.1:${HEALTH_PORT}，请手动处理后重试"
    fi
    info "端口 ${HEALTH_PORT} 已释放"
}
migrate_legacy_ws

# 端口已有其他健康服务响应（疑似重复安装）？
if curl -sf -m 2 "http://127.0.0.1:${HEALTH_PORT}/health" >/dev/null 2>&1; then
    die "端口 ${HEALTH_PORT} 已有健康服务响应（疑似旧实例仍在运行），请先停止后重装"
fi

# ===== 2. 工作目录与配置文件 =====
mkdir -p "$WORKDIR" "$WORKDIR/logs"

cat > "$ENV_FILE" << EOF
# botwork-cloud-server --schedule 调度守护进程配置（install-schedule-daemon.sh 生成）
SCHEDULE_API_KEY=${API_KEY}
SCHEDULE_BASE_URL=${BASE_URL}
SCHEDULE_HEALTH_PORT=${HEALTH_PORT}
SCHEDULE_CONFIG=${CONFIG_PATH}
SCHEDULE_BOT_WEB_URL=${BOT_WEB_URL}
SCHEDULE_MAX_CONCURRENT=${MAX_CONCURRENT}
SCHEDULE_HANDOFF_SLOTS=${HANDOFF_SLOTS}
SCHEDULE_MAX_EXECUTION_TIME=${MAX_EXECUTION_TIME}
EOF
[ -n "$API_TOKEN" ] && echo "SCHEDULE_API_TOKEN=${API_TOKEN}" >> "$ENV_FILE"
chmod 600 "$ENV_FILE"
info "配置已写入 $ENV_FILE（600）"

# watchdog 就位（SysV/nohup 级共用）
install -m 755 "$SCRIPT_DIR/watchdog.sh" "$WORKDIR/watchdog.sh"

TOKEN_ARGS=""
[ -n "$API_TOKEN" ] && TOKEN_ARGS=" --api-token ${API_TOKEN}"

# ===== 3. 三级检测与安装 =====
if command -v systemctl >/dev/null 2>&1 && [ -d /run/systemd/system ]; then
    # ---- 优先级 1：systemd ----
    info "部署级别: systemd"
    cat > "$UNIT_FILE" << EOF
[Unit]
Description=BotWork Cloud Schedule Daemon
After=network.target

[Service]
Type=simple
Environment=HOME=/root
EnvironmentFile=${ENV_FILE}
    ExecStart=${BIN} --schedule --schedule-config \${SCHEDULE_CONFIG} --health-port \${SCHEDULE_HEALTH_PORT} --max-concurrent \${SCHEDULE_MAX_CONCURRENT} --handoff-slots \${SCHEDULE_HANDOFF_SLOTS} --max-execution-time \${SCHEDULE_MAX_EXECUTION_TIME} --api-key \${SCHEDULE_API_KEY} --base-url \${SCHEDULE_BASE_URL} --bot-web-server-url \${SCHEDULE_BOT_WEB_URL}${TOKEN_ARGS}
Restart=always
RestartSec=5
TimeoutStopSec=35

[Install]
WantedBy=multi-user.target
EOF
    echo "systemd" > "$LEVEL_FILE"
    systemctl daemon-reload
    systemctl enable "$INIT_NAME"
    systemctl restart "$INIT_NAME"
elif command -v service >/dev/null 2>&1 && [ -d /etc/init.d ]; then
    # ---- 优先级 2：SysV init + service（目标 chroot 环境即此级别）----
    info "部署级别: SysV init + service"
    install -m 755 "$SCRIPT_DIR/botwork-cloud-schedule.init" "$INIT_FILE"
    echo "sysv" > "$LEVEL_FILE"
    # 先直接运行 init 脚本验证脚本本身（service 包装层可能有问题——部署历史教训）
    "$INIT_FILE" start
    sleep 2
    PID=$(cat /run/botwork-cloud-schedule.pid 2>/dev/null || echo "")
    if [ -n "$PID" ] && kill -0 "$PID" 2>/dev/null; then
        info "init 脚本直接启动成功 (pid $PID)"
    else
        die "init 脚本启动失败（查 $WORKDIR/logs/ 与 $INIT_FILE）"
    fi
    # 再验证 service 包装层
    if service "$INIT_NAME" status >/dev/null 2>&1; then
        info "service 包装层验证通过"
    else
        warn "service 包装层异常（init 脚本本身正常）——用 $INIT_FILE start/stop 管理"
    fi
    # 开机自启注册（chroot 环境可能无此机制，尽力而为）
    if command -v update-rc.d >/dev/null 2>&1; then
        update-rc.d "$INIT_NAME" defaults || true
    fi
else
    # ---- 优先级 3：nohup + watchdog ----
    info "部署级别: nohup + watchdog"
    cat > "$WORKDIR/start-schedule.sh" << 'EOF'
#!/bin/sh
# nohup 级启动脚本（install-schedule-daemon.sh 生成；watchdog.sh 重启时也调此脚本）
export HOME=/root
. /root/bot-work/schedule-daemon.env
mkdir -p /root/bot-work/logs
setsid nohup /usr/local/bin/botwork-cloud-server --schedule \
    --schedule-config "$SCHEDULE_CONFIG" \
    --health-port "$SCHEDULE_HEALTH_PORT" \
    --max-concurrent "$SCHEDULE_MAX_CONCURRENT" \
    --handoff-slots "$SCHEDULE_HANDOFF_SLOTS" \
    --max-execution-time "$SCHEDULE_MAX_EXECUTION_TIME" \
    --api-key "$SCHEDULE_API_KEY" \
    --base-url "$SCHEDULE_BASE_URL" \
    --bot-web-server-url "$SCHEDULE_BOT_WEB_URL" \
    ${SCHEDULE_API_TOKEN:+--api-token "$SCHEDULE_API_TOKEN"} \
    >> /root/bot-work/logs/stderr.log 2>&1 &
echo $! > /run/botwork-cloud-schedule.pid
EOF
    chmod +x "$WORKDIR/start-schedule.sh"
    echo "nohup" > "$LEVEL_FILE"
    "$WORKDIR/start-schedule.sh"
fi

# ===== 4. 拉 watchdog（仅 SysV/nohup 级；systemd 级由 Restart=always 承担）=====
if [ "$(cat "$LEVEL_FILE")" != "systemd" ]; then
    setsid nohup "$WORKDIR/watchdog.sh" >/dev/null 2>&1 &
    info "watchdog 已拉起（/root/bot-work/watchdog.log）"
fi

# ===== 5. 安装验证 =====
info "等待服务就绪..."
sleep 3
READY=0
for i in $(seq 1 10); do
    if curl -sf -m 3 "http://127.0.0.1:${HEALTH_PORT}/health" >/dev/null 2>&1; then
        READY=1
        break
    fi
    sleep 2
done
[ "$READY" -eq 1 ] || die "服务健康检查失败（查 $WORKDIR/logs/ 最新日志）"
info "[OK] /health"

curl -sf -m 3 "http://127.0.0.1:${HEALTH_PORT}/health/ready" >/dev/null 2>&1 && info "[OK] /health/ready" || warn "/health/ready 未就绪（schedules.json 加载异常？）"
curl -sf -m 3 "http://127.0.0.1:${HEALTH_PORT}/metrics" >/dev/null 2>&1 && info "[OK] /metrics" || warn "/metrics 异常"
curl -sf -m 3 "http://127.0.0.1:${HEALTH_PORT}/schedules" >/dev/null 2>&1 && info "[OK] GET /schedules" || warn "GET /schedules 异常"

echo ""
info "安装完成。管理命令："
case "$(cat "$LEVEL_FILE")" in
    systemd)
        echo "  systemctl status|restart|stop ${INIT_NAME}"
        echo "  日志: journalctl -u ${INIT_NAME} + $WORKDIR/logs/schedule-*.log"
        ;;
    sysv)
        echo "  service ${INIT_NAME} status|restart|stop   （或 $INIT_FILE）"
        echo "  日志: $WORKDIR/logs/schedule-*.log + $WORKDIR/watchdog.log"
        ;;
    nohup)
        echo "  启动: $WORKDIR/start-schedule.sh"
        echo "  停止: kill \$(cat /run/botwork-cloud-schedule.pid)"
        echo "  日志: $WORKDIR/logs/"
        ;;
esac
echo "  端到端验证: $SCRIPT_DIR/verify-schedule-daemon.sh"