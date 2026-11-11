#!/bin/bash
# common.sh — 部署脚本共享函数库
# 被 deploy-ubuntu.sh / deploy-centos.sh source

set -euo pipefail

# ============ 颜色输出 ============
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

log_info()  { echo -e "${GREEN}[INFO]${NC}  $*"; }
log_warn()  { echo -e "${YELLOW}[WARN]${NC}  $*"; }
log_error() { echo -e "${RED}[ERROR]${NC} $*"; }
log_step()  { echo -e "\n${BLUE}========== $* ==========${NC}"; }

# ============ 默认配置 ============
# key 不再内置默认值（历史明文 key 已移除），缺失时在 start_cloud_server_service 内 fail-fast
AGNES_API_KEY="${AGNES_API_KEY:-}"
AGNES_MODEL="${AGNES_MODEL:-agnes-3.0-flash}"
NPM_REGISTRY="${NPM_REGISTRY:-https://registry.npmmirror.com}"
NODE_MAJOR="${NODE_MAJOR:-24}"
ADAPTER_PORT="${ADAPTER_PORT:-8082}"
INSTALL_CLOUD_SERVER="${INSTALL_CLOUD_SERVER:-yes}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# ============ 通用检测函数 ============
check_cmd() {
    command -v "$1" >/dev/null 2>&1
}

ensure_cmd() {
    if ! check_cmd "$1"; then
        log_error "命令 '$1' 未找到，请先安装"
        return 1
    fi
}

# ============ 安装 Node.js ============
install_nodejs() {
    if check_cmd node && check_cmd npm; then
        local ver
        ver=$(node --version)
        log_info "Node.js 已安装: $ver"
        return 0
    fi
    log_step "安装 Node.js v${NODE_MAJOR}"
    install_nodejs_distro
    log_info "Node.js 安装完成: $(node --version)"
}

# ============ 安装 Rust ============
install_rust() {
    if check_cmd rustc; then
        log_info "Rust 已安装: $(rustc --version)"
        return 0
    fi
    log_step "安装 Rust (rustup)"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    source "$HOME/.cargo/env"
    log_info "Rust 安装完成: $(rustc --version)"
}

# ============ 安装 npm 全局工具 ============
install_npm_tools() {
    log_step "安装 npm 全局工具 (openclaw, codex, claude code)"
    npm config set registry "$NPM_REGISTRY"

    log_info "安装 openclaw..."
    npm install -g openclaw@latest 2>/dev/null || npm install -g openclaw
    log_info "openclaw: $(openclaw --version 2>/dev/null || echo 'unknown')"

    log_info "安装 codex..."
    npm install -g @openai/codex@latest 2>/dev/null || npm install -g @openai/codex
    log_info "codex: $(codex --version 2>/dev/null || echo 'unknown')"

    log_info "安装 claude code..."
    npm install -g @anthropic-ai/claude-code@latest 2>/dev/null || npm install -g @anthropic-ai/claude-code
    log_info "claude: $(claude --version 2>/dev/null || echo 'unknown')"
}

# ============ 配置 codex ============
configure_codex() {
    log_step "配置 codex (Agnes provider via adapter)"
    mkdir -p "$HOME/.codex"
    cat > "$HOME/.codex/config.toml" << CODEX_EOF
model = "agnes-3.0-flash"
model_provider = "agnes"

[model_providers.agnes]
name = "Agnes"
base_url = "http://127.0.0.1:${ADAPTER_PORT}/v1"
wire_api = "responses"
env_key = "AGNES_API_KEY"
CODEX_EOF
    log_info "codex 配置已写入 $HOME/.codex/config.toml"
    log_info "codex base_url → http://127.0.0.1:${ADAPTER_PORT}/v1 (adapter)"
}

# ============ 配置 claude code ============
configure_claude_code() {
    log_step "配置 claude code (via adapter)"
    log_info "claude code ANTHROPIC_BASE_URL → http://127.0.0.1:${ADAPTER_PORT}"
    log_info "claude code ANTHROPIC_API_KEY → \${AGNES_API_KEY}"
    log_info "注意: bot-work-windows 端通过 SSH 环境变量传递 ANTHROPIC_BASE_URL 和 ANTHROPIC_API_KEY"
}

# ============ 启动 botwork-cloud-server 服务 ============
start_cloud_server_service() {
    log_step "启动 botwork-cloud-server 服务 (adapter port ${ADAPTER_PORT})"

    # key fail-fast：无默认值兜底，缺失即报错退出（防静默以空 key 启动）
    if [ -z "${AGNES_API_KEY:-}" ]; then
        log_error "AGNES_API_KEY 未设置，无法启动 cloud-server 服务；请 export AGNES_API_KEY=<your-key> 后重试"
        return 1
    fi

    local server_bin
    server_bin=$(command -v botwork-cloud-server 2>/dev/null || echo "/usr/local/bin/botwork-cloud-server")
    if [ ! -x "$server_bin" ]; then
        log_error "botwork-cloud-server 未找到: $server_bin"
        return 1
    fi

    local upstream_url="https://apihub.agnes-ai.com/v1"
    local server_port="${CLOUD_SERVER_PORT:-8081}"
    local schedule_port="${SCHEDULE_HEALTH_PORT:-8090}"

    # key 唯一落盘位置：/root/bot-work/cloud-server.env（600）
    # systemd unit 用 EnvironmentFile 引用、init/nohup 用 source 读取，明文不进 unit/init/日志
    mkdir -p /root/bot-work
    ( umask 077; cat > /root/bot-work/cloud-server.env << ENVEOF
OPENAI_API_KEY=${AGNES_API_KEY}
ENVEOF
    )
    chmod 600 /root/bot-work/cloud-server.env
    log_info "API key 已写入 /root/bot-work/cloud-server.env (600)"

    # 检测 systemd 是否可用
    if pidof systemd >/dev/null 2>&1; then
        cat > /etc/systemd/system/botwork-cloud-server.service << EOF
[Unit]
Description=BotWork Cloud Server (adapter port ${ADAPTER_PORT})
After=network.target

[Service]
Type=simple
Environment=HOME=/root
EnvironmentFile=/root/bot-work/cloud-server.env
ExecStart=${server_bin} --listen ws://127.0.0.1:${server_port} --base-url ${upstream_url} --adapter-port ${ADAPTER_PORT}
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
EOF

        cat > /etc/systemd/system/botwork-cloud-server-schedule.service << EOF
[Unit]
Description=BotWork Cloud Server schedule (health port ${schedule_port})
After=network.target

[Service]
Type=simple
Environment=HOME=/root
ExecStart=${server_bin} --schedule --health-port ${schedule_port}
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
EOF

        systemctl daemon-reload
        systemctl enable botwork-cloud-server botwork-cloud-server-schedule
        systemctl restart botwork-cloud-server
        systemctl restart botwork-cloud-server-schedule
        sleep 2

        if systemctl is-active --quiet botwork-cloud-server; then
            log_info "botwork-cloud-server 服务已启动 (systemd, ws port ${server_port}, adapter port ${ADAPTER_PORT})"
        else
            log_warn "botwork-cloud-server systemd 服务启动失败"
        fi
        if systemctl is-active --quiet botwork-cloud-server-schedule; then
            log_info "botwork-cloud-server-schedule 服务已启动 (systemd, health port ${schedule_port})"
        else
            log_warn "botwork-cloud-server-schedule systemd 服务启动失败"
        fi
    elif command -v service >/dev/null 2>&1 && [ -d /etc/init.d ]; then
        log_info "systemd 不可用，使用 service 命令启动"
        cat > /etc/init.d/botwork-cloud-server << 'INITEOF'
#!/bin/sh
### BEGIN INIT INFO
# Provides:          botwork-cloud-server
# Required-Start:    $network $local_fs
# Required-Stop:     $network $local_fs
# Default-Start:     2 3 4 5
# Default-Stop:      0 1 6
# Short-Description: botwork-cloud-server adapter
# Description:       Anthropic-to-OpenAI adapter for codex/claude-code CLI
### END INIT INFO

DAEMON=/usr/local/bin/botwork-cloud-server
NAME=botwork-cloud-server
PIDFILE=/var/run/${NAME}.pid
LOGFILE=/var/log/${NAME}.log

export HOME=/root

LISTEN_ADDR="ws://127.0.0.1:8081"
ADAPTER_PORT=8082
ENV_FILE=/root/bot-work/cloud-server.env
BASE_URL="https://apihub.agnes-ai.com/v1"

DAEMON_ARGS="--listen ${LISTEN_ADDR} --base-url ${BASE_URL} --adapter-port ${ADAPTER_PORT}"

. /lib/lsb/init-functions

is_running() {
    if [ -f "$PIDFILE" ]; then
        PID=$(cat "$PIDFILE" 2>/dev/null)
        if [ -n "$PID" ] && kill -0 "$PID" 2>/dev/null; then
            return 0
        fi
    fi
    pgrep -x "$NAME" > /dev/null 2>&1
}

get_pid() {
    if [ -f "$PIDFILE" ]; then
        PID=$(cat "$PIDFILE" 2>/dev/null)
        if [ -n "$PID" ] && kill -0 "$PID" 2>/dev/null; then
            echo "$PID"
            return
        fi
    fi
    pgrep -x "$NAME" 2>/dev/null | head -1
}

case "$1" in
    start)
        if is_running; then
            log_success_msg "$NAME already running (PID $(get_pid))"
            exit 0
        fi
        if [ -f "$ENV_FILE" ]; then
            . "$ENV_FILE"
        fi
        log_daemon_msg "Starting $NAME"
        if [ -x /usr/sbin/start-stop-daemon ]; then
            /usr/sbin/start-stop-daemon --start --background --make-pidfile --pidfile "$PIDFILE" \
                --exec "$DAEMON" -- $DAEMON_ARGS
        else
            setsid sh -c "exec '$DAEMON' $DAEMON_ARGS >> '$LOGFILE' 2>&1" &
            echo $! > "$PIDFILE"
        fi
        if [ $? -eq 0 ]; then
            log_end_msg 0
        else
            log_end_msg 1
        fi
        ;;
    stop)
        if ! is_running; then
            log_success_msg "$NAME not running"
            rm -f "$PIDFILE"
            exit 0
        fi
        log_daemon_msg "Stopping $NAME"
        PID=$(get_pid)
        if [ -n "$PID" ]; then
            kill "$PID" 2>/dev/null
            for i in 1 2 3 4 5 6 7 8 9 10; do
                if ! kill -0 "$PID" 2>/dev/null; then
                    break
                fi
                sleep 0.5
            done
            if kill -0 "$PID" 2>/dev/null; then
                kill -9 "$PID" 2>/dev/null
            fi
        fi
        rm -f "$PIDFILE"
        log_end_msg 0
        ;;
    restart|force-reload)
        $0 stop
        sleep 1
        $0 start
        ;;
    status)
        if is_running; then
            log_success_msg "$NAME is running (PID $(get_pid))"
            exit 0
        else
            log_failure_msg "$NAME is not running"
            exit 3
        fi
        ;;
    *)
        echo "Usage: $0 {start|stop|restart|status}"
        exit 1
        ;;
esac

exit 0
INITEOF
        chmod +x /etc/init.d/botwork-cloud-server

        cat > /etc/init.d/botwork-cloud-server-schedule << 'SCHEDINITEOF'
#!/bin/sh
### BEGIN INIT INFO
# Provides:          botwork-cloud-server-schedule
# Required-Start:    $network $local_fs
# Required-Stop:     $network $local_fs
# Default-Start:     2 3 4 5
# Default-Stop:      0 1 6
# Short-Description: botwork-cloud-server schedule
### END INIT INFO

DAEMON=/usr/local/bin/botwork-cloud-server
NAME=botwork-cloud-server-schedule
PIDFILE=/var/run/${NAME}.pid
LOGFILE=/var/log/${NAME}.log

export HOME=/root

HEALTH_PORT=8090

is_running() {
    curl -fs -m 3 http://127.0.0.1:8090/health >/dev/null 2>&1 && return 0
    pgrep -f "botwork-cloud-server --schedule" > /dev/null 2>&1
}

get_pid() {
    if [ -f "$PIDFILE" ]; then
        PID=$(cat "$PIDFILE" 2>/dev/null)
        if [ -n "$PID" ] && kill -0 "$PID" 2>/dev/null; then
            echo "$PID"
            return
        fi
    fi
    pgrep -f "botwork-cloud-server --schedule" 2>/dev/null | head -1
}

case "$1" in
    start)
        if is_running; then
            log_success_msg "$NAME already running (PID $(get_pid))"
            exit 0
        fi
        log_daemon_msg "Starting $NAME"
        setsid nohup "$DAEMON" --schedule --health-port "$HEALTH_PORT" >> "$LOGFILE" 2>&1 &
        echo $! > "$PIDFILE"
        log_end_msg 0
        ;;
    stop)
        if ! is_running; then
            log_success_msg "$NAME not running"
            rm -f "$PIDFILE"
            exit 0
        fi
        log_daemon_msg "Stopping $NAME"
        PID=$(get_pid)
        if [ -n "$PID" ]; then
            kill "$PID" 2>/dev/null
            for i in 1 2 3 4 5 6 7 8 9 10; do
                if ! kill -0 "$PID" 2>/dev/null; then
                    break
                fi
                sleep 0.5
            done
            if kill -0 "$PID" 2>/dev/null; then
                kill -9 "$PID" 2>/dev/null
            fi
        fi
        rm -f "$PIDFILE"
        log_end_msg 0
        ;;
    restart|force-reload)
        $0 stop
        sleep 1
        $0 start
        ;;
    status)
        if is_running; then
            log_success_msg "$NAME is running (PID $(get_pid))"
            exit 0
        else
            log_failure_msg "$NAME is not running"
            exit 3
        fi
        ;;
    *)
        echo "Usage: $0 {start|stop|restart|status}"
        exit 1
        ;;
esac

exit 0
SCHEDINITEOF
        chmod +x /etc/init.d/botwork-cloud-server-schedule

        if command -v update-rc.d >/dev/null 2>&1; then
            update-rc.d botwork-cloud-server defaults
            update-rc.d botwork-cloud-server-schedule defaults
        fi
        service botwork-cloud-server restart
        service botwork-cloud-server-schedule restart
        sleep 3

        if service botwork-cloud-server status >/dev/null 2>&1; then
            log_info "botwork-cloud-server 服务已启动 (service, ws port ${server_port}, adapter port ${ADAPTER_PORT})"
            log_info "停止: service botwork-cloud-server stop"
        else
            log_warn "botwork-cloud-server service 启动失败，回退 nohup"
            pkill -f "adapter-port" 2>/dev/null || true
            sleep 1
            ( . /root/bot-work/cloud-server.env && setsid nohup "$server_bin" \
                --listen "ws://127.0.0.1:${server_port}" \
                --base-url "$upstream_url" \
                --adapter-port "${ADAPTER_PORT}" \
                >> /tmp/botwork-cloud-server.log 2>&1 & )
            sleep 2
            if pgrep -f "adapter-port" >/dev/null 2>&1; then
                log_info "botwork-cloud-server 已启动 (nohup, PID $(pgrep -f 'adapter-port'))"
            else
                log_error "botwork-cloud-server 启动失败"
            fi
        fi
    else
        log_warn "systemd 和 service 均不可用，使用 nohup 启动 (L3 兜底)"
        pkill -f "adapter-port" 2>/dev/null || true
        pkill -f "botwork-cloud-server --schedule" 2>/dev/null || true
        sleep 1
        ( . /root/bot-work/cloud-server.env && setsid nohup "$server_bin" \
            --listen "ws://127.0.0.1:${server_port}" \
            --base-url "$upstream_url" \
            --adapter-port "${ADAPTER_PORT}" \
            >> /tmp/botwork-cloud-server.log 2>&1 & )
        setsid nohup "$server_bin" --schedule --health-port "${schedule_port}" \
            >> /tmp/botwork-cloud-server-schedule.log 2>&1 &
        sleep 2

        if pgrep -f "adapter-port" >/dev/null 2>&1; then
            log_info "botwork-cloud-server 已启动 (nohup, PID $(pgrep -f 'adapter-port'))"
            log_info "日志: /tmp/botwork-cloud-server.log"
            log_info "停止: pkill -f adapter-port"
        else
            log_error "botwork-cloud-server 启动失败，请检查: /tmp/botwork-cloud-server.log"
        fi
        if pgrep -f "botwork-cloud-server --schedule" >/dev/null 2>&1; then
            log_info "botwork-cloud-server-schedule 已启动 (nohup, health port ${schedule_port})"
        else
            log_warn "botwork-cloud-server-schedule 启动失败，请检查: /tmp/botwork-cloud-server-schedule.log"
        fi
    fi
}

# ============ 验证安装 ============
verify_install() {
    log_step "验证安装"
    local pass=0 fail=0

    check_cmd openclaw && log_info "openclaw: $(openclaw --version 2>/dev/null)" || { log_error "openclaw 未安装"; ((fail++)); }
    check_cmd codex    && log_info "codex: $(codex --version 2>/dev/null)"       || { log_error "codex 未安装"; ((fail++)); }
    check_cmd claude   && log_info "claude: $(claude --version 2>/dev/null)"     || { log_error "claude 未安装"; ((fail++)); }
    check_cmd node     && log_info "node: $(node --version)"                     || { log_error "node 未安装"; ((fail++)); }
    check_cmd python3  && log_info "python3: $(python3 --version)"               || { log_error "python3 未安装"; ((fail++)); }

    if [ -f "$HOME/.codex/config.toml" ]; then
        log_info "codex 配置: OK"
    else
        log_error "codex 配置缺失"; ((fail++))
    fi

    if systemctl is-active --quiet botwork-cloud-server 2>/dev/null; then
        log_info "botwork-cloud-server 服务: 运行中 (systemd)"
    elif service botwork-cloud-server status >/dev/null 2>&1; then
        log_info "botwork-cloud-server 服务: 运行中 (service, PID $(pgrep -f 'botwork-cloud-server'))"
    elif pgrep -f "botwork-cloud-server" >/dev/null 2>&1; then
        log_info "botwork-cloud-server 服务: 运行中 (nohup, PID $(pgrep -f 'botwork-cloud-server'))"
    else
        log_warn "botwork-cloud-server 服务: 未运行"
    fi

    if check_cmd botwork-cloud-server; then
        log_info "botwork-cloud-server: $(botwork-cloud-server --version 2>/dev/null || echo 'installed')"
    else
        log_warn "botwork-cloud-server: 未安装"
    fi

    echo ""
    if [ "$fail" -eq 0 ]; then
        log_info "所有核心组件安装成功!"
    else
        log_error "$fail 个组件安装失败"
    fi
    return "$fail"
}