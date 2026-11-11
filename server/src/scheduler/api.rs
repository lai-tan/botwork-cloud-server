// Copyright (c) 2026 Jehu Shaw. All rights reserved.

//! 本地管理 API（AUTOMATION-DESIGN.md §5.2.1 健康检查与管理 API）：
//! - 复用 `--health-port`，仅绑定 127.0.0.1（SSH exec curl 即在本机）；
//! - /health 检测进程存活 + 调度循环心跳（last_tick 超 90s 未更新返回 503）；
//! - POST body 仅配置字段（服务端剥离运行时字段），按 id upsert，上限 100；
//! - DELETE 对运行中/排队中任务走取消链并返回 202；
//! - `--api-token` 启用后管理端点要求 Bearer 认证（健康端点免认证）；
//! - SIGTERM 停机窗口内返回 503。

use std::sync::Arc;

use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::middleware::from_fn_with_state;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;

use axum::Json;
use axum::Router;
use tokio::net::TcpListener;

use super::config::MAX_SCHEDULES;
use super::config::Schedule;
use super::DaemonState;

pub async fn serve(state: Arc<DaemonState>) -> anyhow::Result<()> {
    let port = state.opts.health_port;
    let app = Router::new()
        .route("/health", get(health))
        .route("/health/ready", get(health_ready))
        .route("/metrics", get(metrics))
        .route("/schedules", get(list_schedules).post(create_schedule))
        .route("/schedules/{id}", axum::routing::delete(delete_schedule))
        .route(
            "/schedules/{id}/trigger",
            axum::routing::post(trigger_schedule),
        )
        .route("/runs", get(list_runs))
        .layer(from_fn_with_state(state.clone(), auth_and_gating))
        .with_state(state.clone());
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    tracing::info!("管理 API 监听 127.0.0.1:{port}");
    let shutdown = state.shutdown.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown.cancelled().await;
        })
        .await?;
    Ok(())
}

async fn auth_and_gating(
    State(state): State<Arc<DaemonState>>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let path = request.uri().path();
    let is_health = path == "/health" || path == "/health/ready";
    // 停机窗口内所有端点返回 503
    if state.shutdown.is_cancelled() {
        return service_unavailable("正在停机");
    }
    // 管理端点（写操作）在 --api-token 启用时要求 Bearer 认证；健康端点免认证
    let needs_auth = state.opts.api_token.is_some()
        && !is_health
        && (request.method() == axum::http::Method::POST
            || request.method() == axum::http::Method::DELETE);
    if needs_auth {
        let expected = format!(
            "Bearer {}",
            state.opts.api_token.as_deref().unwrap_or_default()
        );
        let provided = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if provided != expected {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "unauthorized"})),
            )
                .into_response();
        }
    }
    next.run(request).await
}

fn service_unavailable(message: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": message})),
    )
        .into_response()
}

async fn health(State(state): State<Arc<DaemonState>>) -> Response {
    // 进程存活 + 调度循环运行中（§5.2.1）：循环每轮更新 last_tick 心跳，
    // 超 90s（3 个 30s 周期）未更新视为调度循环卡死，返回 503 供 watchdog 重启
    let last_tick = state.last_tick.load(std::sync::atomic::Ordering::Relaxed);
    let stalled = chrono::Utc::now().timestamp() - last_tick > 90;
    if stalled {
        service_unavailable("scheduling loop stalled")
    } else {
        (StatusCode::OK, "ok").into_response()
    }
}

async fn health_ready(State(state): State<Arc<DaemonState>>) -> Response {
    if state.ready.load(std::sync::atomic::Ordering::Relaxed) {
        (StatusCode::OK, "ready").into_response()
    } else {
        service_unavailable("starting")
    }
}

async fn metrics(State(state): State<Arc<DaemonState>>) -> impl IntoResponse {
    let m = &state.metrics;
    let running = state.registry.ids().len();
    let rel = |a: &std::sync::atomic::AtomicU64| a.load(std::sync::atomic::Ordering::Relaxed);
    let le60 = rel(&m.duration_le_60);
    let le300 = rel(&m.duration_le_300);
    let le1800 = rel(&m.duration_le_1800);
    let le3600 = rel(&m.duration_le_3600);
    let le_inf = rel(&m.duration_le_inf);
    let duration_sum_secs = rel(&m.duration_sum_ms) as f64 / 1000.0;
    let body = format!(
        "# TYPE botwork_running gauge\nbotwork_running {running}\n\
         # TYPE botwork_completed_total counter\nbotwork_completed_total {}\n\
         # TYPE botwork_failed_total counter\nbotwork_failed_total {}\n\
         # TYPE botwork_timeout_total counter\nbotwork_timeout_total {}\n\
         # TYPE botwork_interrupted_total counter\nbotwork_interrupted_total {}\n\
         # TYPE botwork_missed_pending_total counter\nbotwork_missed_pending_total {}\n\
         # TYPE botwork_task_duration_seconds histogram\n\
         botwork_task_duration_seconds_bucket{{le=\"60\"}} {le60}\n\
         botwork_task_duration_seconds_bucket{{le=\"300\"}} {le300}\n\
         botwork_task_duration_seconds_bucket{{le=\"1800\"}} {le1800}\n\
         botwork_task_duration_seconds_bucket{{le=\"3600\"}} {le3600}\n\
         botwork_task_duration_seconds_bucket{{le=\"+Inf\"}} {le_inf}\n\
         botwork_task_duration_seconds_sum {duration_sum_secs}\n\
         botwork_task_duration_seconds_count {le_inf}\n",
        rel(&m.completed),
        rel(&m.failed),
        rel(&m.timeout),
        rel(&m.interrupted),
        rel(&m.missed_pending),
    );
    (
        StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4")],
        body,
    )
}

/// §3.6 字段约束矩阵校验（后端创建时校验，非法返回 422）。
fn validate_frequency_fields(s: &Schedule) -> Result<(), String> {
    fn non_empty(v: &Option<String>) -> Option<&str> {
        v.as_deref().map(str::trim).filter(|s| !s.is_empty())
    }
    match s.frequency_type.as_str() {
        "periodic" => {
            let mode = s.periodic_mode.as_deref().unwrap_or("");
            match mode {
                "daily" | "weekly" | "biweekly" | "monthly" | "yearly" | "cron" => {}
                other => {
                    return Err(format!(
                        "periodic_mode 非法: {other:?}（须为 daily/weekly/biweekly/monthly/yearly/cron）"
                    ))
                }
            }
            match mode {
                "weekly" | "biweekly" => {
                    if non_empty(&s.byday).is_none() {
                        return Err(format!("{mode} 要求 byday 非空（如 \"MO,WE,FR\"）"));
                    }
                }
                "monthly" => {
                    if non_empty(&s.bymonthday).is_none() {
                        return Err("monthly 要求 bymonthday 非空（如 \"1,15\"）".to_string());
                    }
                }
                "yearly" => {
                    if s.bymonth.is_none() || s.bymonthday.is_none() {
                        return Err("yearly 要求 bymonth 与 bymonthday 非空".to_string());
                    }
                }
                "cron" => {
                    // 远程暂未启用 cron 模式（cron crate 未 vendored，v2.17 偏差），
                    // 创建时直接 422 比落库后静默跳过（告警仅服务端日志）更诚实
                    return Err(
                        "cron 模式远程暂未启用（避免引入新 crates.io 依赖，见 §10）".to_string(),
                    );
                }
                _ => {}
            }
        }
        "interval" => match s.interval_seconds {
            Some(v) if v >= 60 => {}
            Some(v) => return Err(format!("interval_seconds 须 ≥ 60（当前 {v}）")),
            None => return Err("interval 要求 interval_seconds 非空".to_string()),
        },
        "once" => {}
        other => {
            return Err(format!(
                "frequency_type 非法: {other:?}（须为 periodic/interval/once）"
            ))
        }
    }
    Ok(())
}

async fn list_schedules(State(state): State<Arc<DaemonState>>) -> impl IntoResponse {
    let schedules = state.store.snapshot().await;
    Json(serde_json::json!({ "schedules": schedules }))
}

#[derive(serde::Deserialize)]
struct RunsQuery {
    schedule_id: Option<String>,
    limit: Option<usize>,
}

/// GET /runs（§5.2.1，Phase 3 步骤 22 同步通道）：schedule_id 省略时每个
/// schedule 各返回最近 limit 条（同步语义）；limit 默认 100、上限 1000；
/// GET 免认证（与 GET /schedules 一致）；停机窗口 503 由 auth_and_gating 统一处理。
async fn list_runs(
    State(state): State<Arc<DaemonState>>,
    Query(query): Query<RunsQuery>,
) -> Response {
    let limit = query.limit.unwrap_or(100).clamp(1, 1000);
    match state.runs.list(query.schedule_id.as_deref(), limit).await {
        Ok(runs) => (StatusCode::OK, Json(serde_json::json!({ "runs": runs }))).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": err.to_string()})),
        )
            .into_response(),
    }
}

async fn create_schedule(
    State(state): State<Arc<DaemonState>>,
    Json(body): Json<Schedule>,
) -> Response {
    if body.id.trim().is_empty() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({"error": "id 不能为空（由调用方生成 UUID）"})),
        )
            .into_response();
    }
    // once 任务要求 run_at 为合法的未来时间（§3.6 字段约束矩阵）
    if body.frequency_type == "once" {
        let is_future = body
            .run_at
            .as_deref()
            .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
            .map(|d| d.with_timezone(&chrono::Utc) > chrono::Utc::now());
        if is_future != Some(true) {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(serde_json::json!({
                    "error": "frequency_type='once' 要求 run_at 为合法的未来时间（ISO8601）"
                })),
            )
                .into_response();
        }
    }
    // §3.6 字段约束矩阵其余行（periodic 必填字段 / interval ≥60 / cron 未启用）
    if let Err(msg) = validate_frequency_fields(&body) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({"error": msg})),
        )
            .into_response();
    }
    // body 仅配置字段：剥离调用方可能携带的运行时字段（服务端忽略）
    let config_only = body.config_fields_only();
    let exists = state.store.find(&config_only.id).await.is_some();
    if !exists && state.store.len().await >= MAX_SCHEDULES {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({"error": format!("schedules 数量已达上限 {MAX_SCHEDULES}")})),
        )
            .into_response();
    }
    // 依赖校验临界区（§5.7 v2.39）：持锁"快照→校验（存在/同 agent/环）→
    // upsert"原子，违规 422（与 cron/once 校验同通道）
    match state.store.upsert_with_dependency_check(config_only).await {
        Ok(Ok(merged)) => (StatusCode::OK, Json(serde_json::to_value(merged).unwrap_or_default())).into_response(),
        Ok(Err(msg)) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({"error": msg})),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": err.to_string()})),
        )
            .into_response(),
    }
}

/// POST /schedules/{id}/trigger（§5.6.1 决策 C：专用触发端点，v2.43）：
/// 任何 active 记录可触发；trigger_type 按 is_handoff() 分流
/// handoff/manual_test（与本地 test_trigger 对齐）；不污染 POST
/// /schedules 的幂等 upsert 语义；Bearer 认证同 POST（auth_and_gating
/// 已按 method=POST 覆盖）。
async fn trigger_schedule(
    State(state): State<Arc<DaemonState>>,
    Path(id): Path<String>,
) -> Response {
    let Some(schedule) = state.store.find(&id).await else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "schedule 不存在"})),
        )
            .into_response();
    };
    if schedule.status != "active" {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": format!("仅 active 记录可触发（当前 status={}）", schedule.status)
            })),
        )
            .into_response();
    }
    let trigger_type = if schedule.is_handoff() {
        "handoff"
    } else {
        "manual_test"
    };
    super::executor::spawn_task(state.clone(), schedule, trigger_type.to_string());
    (
        StatusCode::OK,
        Json(serde_json::json!({"status": "triggered", "trigger_type": trigger_type})),
    )
        .into_response()
}

async fn delete_schedule(
    State(state): State<Arc<DaemonState>>,
    Path(id): Path<String>,
) -> Response {
    let occupied = state.registry.slot_snapshot(&id).is_some();
    // 被依赖拒绝（§5.7 v2.39：持锁扫描，引用非空 422——先解除依赖
    // 再删除，与本地 delete_schedule 同规则，不级联删引用）
    let removal = match state.store.remove_if_unreferenced(&id).await {
        Ok(r) => r,
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": err.to_string()})),
            )
                .into_response()
        }
    };
    match removal {
        Err(referrers) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": format!("该任务被 [{}] 依赖，须先解除依赖再删除", referrers.join("、"))
            })),
        )
            .into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "schedule 不存在"})),
        )
            .into_response(),
        Ok(Some(_)) => {
            if occupied {
                // 运行中/排队中：记入 pending_deletes 并走取消链，返回 202（取消中）
                state.pending_deletes.insert(id.clone());
                let state_for_cancel = state.clone();
                tokio::spawn(async move {
                    super::executor::cancel_chain(
                        &state_for_cancel,
                        &id,
                        super::registry::CancelReason::Deleted,
                    )
                    .await;
                });
                return (
                    StatusCode::ACCEPTED,
                    Json(serde_json::json!({"status": "deleting", "detail": "任务取消中"})),
                )
                    .into_response();
            }
            (
                StatusCode::OK,
                Json(serde_json::json!({"status": "deleted"})),
            )
                .into_response()
        }

    }
}