#!/bin/sh
# watchdog.sh — botwork-cloud-schedule 调度守护进程看门狗（SysV/nohup 部署级配套）
# 设计依据：AUTOMATION-DESIGN.md §5.2 部署三级回退 + §5.2.1 卡死检测
#
# 机制：
# - 每 30s 轮询 curl -sf http://127.0.0.1:$PORT/health（比 kill -0 强：
#   /health 含调度循环心跳，能检测"进程活着但调度循环卡死"——心跳超 90s 返回 503）
# - 连续 3 次失败（90s）→ kill -TERM 优雅停机（等待 ≤35s，超时 KILL）→ 重新拉起
# - 拉起命令按部署级别选择（/root/bot-work/deploy-level：sysv|nohup）：
#     sysv  → /etc/init.d/botwork-cloud-schedule start
#     nohup → /root/bot-work/start-schedule.sh
# - 防重复：启动时检查 /run/botwork-watchdog.pid，已有存活实例直接退出
#   （init start 分支每次都会拉起 watchdog，靠此检查避免叠多个）
#
# 日志追加写 /root/bot-work/watchdog.log（量级约 200KB/天，定期手动清理或配 logrotate）
# 生命周期：service start 时顺带拉起；service stop 时先被杀（否则 stop 会被视为崩溃而重新拉起）

ENV_FILE=/root/bot-work/schedule-daemon.env
LEVEL_FILE=/root/bot-work/deploy-level
PIDFILE=/run/botwork-watchdog.pid
SVC_PIDFILE=/run/botwork-cloud-schedule.pid
LOG=/root/bot-work/watchdog.log

log() { echo "$(date '+%F %T') $*" >> "$LOG" 2>/dev/null; }

# --- 防重复：已有存活实例则退出 ---
if [ -f "$PIDFILE" ]; then
    OLD=$(cat "$PIDFILE" 2>/dev/null)
    if [ -n "$OLD" ] && kill -0 "$OLD" 2>/dev/null; then
        exit 0
    fi
fi
echo $$ > "$PIDFILE"

# --- TERM/INT 时清理自身 pidfile（配合 init 脚本 stop 的先杀 watchdog） ---
trap 'rm -f "$PIDFILE"; exit 0' TERM INT

[ -f "$ENV_FILE" ] && . "$ENV_FILE"
PORT="${SCHEDULE_HEALTH_PORT:-8081}"
LEVEL=$(cat "$LEVEL_FILE" 2>/dev/null || echo sysv)

restart_service() {
    if [ "$LEVEL" = "nohup" ]; then
        /root/bot-work/start-schedule.sh
    else
        /etc/init.d/botwork-cloud-schedule start
    fi
}

log "watchdog started (pid $$, port $PORT, level $LEVEL)"

FAILS=0
while true; do
    if curl -sf -m 5 "http://127.0.0.1:${PORT}/health" >/dev/null 2>&1; then
        FAILS=0
    else
        FAILS=$((FAILS + 1))
        log "health check failed (${FAILS}/3)"
        if [ "$FAILS" -ge 3 ]; then
            PID=$(cat "$SVC_PIDFILE" 2>/dev/null)
            log "3 consecutive failures (process dead or loop stuck), restarting (old pid=${PID:-unknown})"
            if [ -n "$PID" ]; then
                kill -TERM "$PID" 2>/dev/null
                i=0
                while [ "$i" -lt 35 ] && kill -0 "$PID" 2>/dev/null; do
                    sleep 1
                    i=$((i + 1))
                done
                kill -0 "$PID" 2>/dev/null && kill -KILL "$PID" 2>/dev/null
            fi
            rm -f "$SVC_PIDFILE"
            restart_service
            FAILS=0
        fi
    fi
    sleep 30
done