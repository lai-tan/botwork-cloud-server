#!/bin/bash
# verify-schedule-daemon.sh — 调度守护进程端到端验证（§9 步骤 23b 远程验证）
#
# 验证链路：健康检查三端点 → POST /schedules 创建 once 测试任务（90s 后触发）
#   → GET /schedules 确认 → 轮询 GET /runs 等待真实执行完成（触发调度循环 → codex
#   thread → Agnes API → run 记录落盘 runs/）→ run 字段抽查 → DELETE 清理
#
# 注意：会真实调用一次 Agnes API（prompt 极短，agnes-3.0-flash 成本可忽略）
# 前置：install-schedule-daemon.sh 已完成且服务运行中

set -euo pipefail

ENV_FILE=/root/bot-work/schedule-daemon.env
[ -f "$ENV_FILE" ] && . "$ENV_FILE"
PORT="${SCHEDULE_HEALTH_PORT:-8081}"
BASE="http://127.0.0.1:${PORT}"

PASS=0; FAIL=0
ok() { echo "[PASS] $*"; PASS=$((PASS + 1)); }
ng() { echo "[FAIL] $*"; FAIL=$((FAIL + 1)); }

# ===== 1. 健康检查 =====
curl -sf -m 5 "$BASE/health" >/dev/null 2>&1 && ok "/health 200" || ng "/health 不可达"
curl -sf -m 5 "$BASE/health/ready" >/dev/null 2>&1 && ok "/health/ready 200" || ng "/health/ready 未就绪"
curl -sf -m 5 "$BASE/metrics" >/dev/null 2>&1 && ok "/metrics 200" || ng "/metrics 异常"

# ===== 2. 创建 once 测试任务（run_at = 当前 UTC + 90s）=====
# body 按 §5.2 schema 配置字段（远端结构体 #[serde(default)]，其余字段走默认值）
SCHED_ID=$(cat /proc/sys/kernel/random/uuid)
AGENT_ID=$(cat /proc/sys/kernel/random/uuid)
RUN_AT=$(date -u -d '+90 seconds' +%Y-%m-%dT%H:%M:%SZ)
BODY=$(cat << EOF
{"id":"$SCHED_ID","agent_id":"$AGENT_ID","name":"e2e-verify-once","prompt":"Reply with exactly: OK","work_dir":"/root/bot-work","frequency_type":"once","run_at":"$RUN_AT","status":"active"}
EOF
)
HTTP=$(curl -s -o /tmp/verify-post.out -w '%{http_code}' -X POST \
    -H 'Content-Type: application/json' -d "$BODY" "$BASE/schedules" || echo 000)
case "$HTTP" in
    200|201|204) ok "POST /schedules ($HTTP)" ;;
    *) ng "POST /schedules ($HTTP): $(head -c 300 /tmp/verify-post.out)" ;;
esac

# ===== 3. GET 确认已创建（含服务端管理字段 created_at/updated_at）=====
GET_JSON=$(curl -sf -m 5 "$BASE/schedules" || echo "")
echo "$GET_JSON" | grep -q "$SCHED_ID" && ok "GET /schedules 含测试任务" || ng "GET /schedules 未找到测试任务"
echo "$GET_JSON" | grep -q '"created_at"' && ok "GET /schedules 返回含 created_at（服务端管理字段）" || ng "GET /schedules 缺 created_at"

# ===== 4. 等待触发与执行完成 =====
echo "等待任务触发（run_at=$RUN_AT，调度循环 30s 周期 + codex 执行时间，上限 5 分钟）..."
FINAL=""
LAST_SHOWN=""
DEADLINE=$(( $(date +%s) + 300 ))
while [ "$(date +%s)" -lt "$DEADLINE" ]; do
    STATUS=$(curl -sf -m 5 "$BASE/runs?schedule_id=$SCHED_ID&limit=1" 2>/dev/null \
        | grep -o '"status":"[^"]*"' | head -1 | cut -d'"' -f4 || echo "")
    if [ -n "$STATUS" ] && [ "$STATUS" != "$LAST_SHOWN" ]; then
        echo "  run status: $STATUS"
        LAST_SHOWN="$STATUS"
    fi
    case "$STATUS" in
        success|failed|interrupted|timeout|expired)
            FINAL="$STATUS"
            break
            ;;
    esac
    sleep 10
done
if [ -n "$FINAL" ]; then
    ok "任务执行到终态（status=$FINAL）"
else
    ng "5 分钟内未见终态 run 记录（调度循环/执行链路异常，查 /root/bot-work/logs/）"
fi
[ "$FINAL" = "success" ] && ok "执行成功（调度→codex→Agnes API 全链路通）" || true

# ===== 5. run 记录字段抽查（§5.2 run schema）=====
RUN_JSON=$(curl -sf -m 5 "$BASE/runs?schedule_id=$SCHED_ID&limit=1" 2>/dev/null || echo "")
echo "$RUN_JSON" | grep -q '"schedule_id"' && ok "run 记录含 schedule_id" || ng "run 记录缺 schedule_id"
echo "$RUN_JSON" | grep -q '"trigger_type"' && ok "run 记录含 trigger_type" || ng "run 记录缺 trigger_type"
echo "$RUN_JSON" | grep -q '"started_at"' && ok "run 记录含 started_at" || ng "run 记录缺 started_at"
echo "$RUN_JSON" | grep -q '"finished_at"' && ok "run 记录含 finished_at" || ng "run 记录缺 finished_at"

# ===== 6. 清理 =====
HTTP=$(curl -s -o /dev/null -w '%{http_code}' -X DELETE "$BASE/schedules/$SCHED_ID" || echo 000)
case "$HTTP" in
    200|202|204) ok "DELETE /schedules/$SCHED_ID ($HTTP)" ;;
    *) ng "DELETE /schedules/$SCHED_ID ($HTTP)" ;;
esac
sleep 1
if curl -sf -m 5 "$BASE/schedules" | grep -q "$SCHED_ID"; then
    ng "删除后任务仍存在"
else
    ok "删除确认（GET /schedules 已无测试任务）"
fi

# ===== 结果汇总 =====
echo ""
echo "===== 端到端验证结果: PASS=$PASS FAIL=$FAIL ====="
[ "$FAIL" -eq 0 ] && echo "全部通过" || { echo "存在失败项（run 记录保留在 /root/bot-work/runs/ 供排查）"; exit 1; }