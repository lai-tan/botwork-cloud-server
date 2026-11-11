// Copyright (c) 2026 Jehu Shaw. All rights reserved.

//! 任务隔离与统一取消链（AUTOMATION-DESIGN.md §5.2.1 v2.16 定稿）：
//! - 外层 wrapper：注册表占位 → scopeguard 护栏 → 排队 acquire（响应取消）
//!   → TOCTOU 重校验（status + 生效区间）→ run 占位记录 + last_status=
//!   'running'（运行中状态通道，崩溃恢复输入）→ 内层 spawn → 外层 select
//!   超时 → finalize 单点收尾；
//! - cancel_chain（DELETE/内存熔断/停机共用）：先写 reason（状态先于
//!   信号）→ token.cancel → 等待注册表条目移除（grace 10s）→ AbortHandle
//!   兜底强杀；超时路径由 wrapper 持 JoinHandle 自行 grace/abort；
//! - finalize：唯一写 run 记录 + last_* 字段级 merge 的位置。

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio_util::sync::CancellationToken;

use super::codex_runtime::CodexRuntime;
use super::codex_runtime::RoutedEvent;
use super::config::Schedule;
use super::registry::CancelReason;
use super::runs::RunRecord;
use super::runs::RunsStore;
use super::runs::TokenUsage;
use super::trigger;
use super::DaemonState;

const OUTPUT_SUMMARY_MAX_CHARS: usize = 4000;
const HANDOFF_MARKER: &str = "@handoff_local";
const HANDOFF_QUEUE_WARN_AFTER: Duration = Duration::from_secs(600);
const CANCEL_GRACE_PERIOD: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
pub struct TaskOutcome {
    pub status: String,
    pub output_summary: String,
    pub error: Option<String>,
    pub token_usage: Option<TokenUsage>,
    /// codex thread rollout 落盘路径（§5.2 v2.43：thread_start/
    /// thread_resume 响应 thread.path，随 run 记录落盘——反向交接
    /// 下载源；finalize 全量覆盖 run 记录，各 outcome 路径均携带）
    pub rollout_path: Option<String>,
    /// execute_inner 事件循环 AgentMessage 分支全文检测 @handoff_local
    /// 置位（§5.6.4 方式 3 v2.44：finalize 仅见截断摘要无法回扫全文，
    /// 以标志位传递；消费与否由 finalize 按 is_handoff 判定）
    pub handoff_marker: bool,
    /// §4.9 执行证据（v2.52 F2）：execute_inner 收集——AgentMessage →
    /// assistant_output；CommandExecution/McpToolCall/WebSearch →
    /// external_action；FileChange → local_file_mutation。3 变体 JSON
    /// 口径与本地调度 Evidence 枚举序列化一致（tag=type snake_case），
    /// 经 RunRecord.evidence 落盘、同步协议原样镜像到本地
    pub evidence: Vec<serde_json::Value>,
    /// §4.9 产物清单（v2.52 F2）：FileChange 变更文件 {name,path} 去重
    /// 收集，口径对齐本地 push_artifact
    pub artifacts: Vec<serde_json::Value>,
}

impl TaskOutcome {
    fn failed(error: String) -> Self {
        Self {
            status: "failed".to_string(),
            output_summary: String::new(),
            error: Some(error),
            token_usage: None,
            rollout_path: None,
            handoff_marker: false,
            evidence: Vec::new(),
            artifacts: Vec::new(),
        }
    }

    fn from_turn(status: &str, output: String) -> Self {
        let (status, error) = match status {
            "Completed" => ("success", None),
            "Failed" => ("failed", Some("turn 执行失败".to_string())),
            _ => ("interrupted", Some("turn 被中断".to_string())),
        };
        Self {
            status: status.to_string(),
            output_summary: truncate_summary(output),
            error,
            token_usage: None,
            rollout_path: None,
            handoff_marker: false,
            evidence: Vec::new(),
            artifacts: Vec::new(),
        }
    }
}

fn truncate_summary(mut output: String) -> String {
    if output.chars().count() > OUTPUT_SUMMARY_MAX_CHARS {
        output = output.chars().take(OUTPUT_SUMMARY_MAX_CHARS).collect();
        output.push_str("…(截断)");
    }
    output
}

/// 触发入口：占位失败（同 schedule 已有实例）走 missed_pending，
/// 其余进入完整的 wrapper 流程。
pub fn spawn_task(state: Arc<DaemonState>, schedule: Schedule, trigger_type: String) {
    tokio::spawn(async move {
        wrapper(state, schedule, trigger_type).await;
    });
}

async fn wrapper(state: Arc<DaemonState>, schedule: Schedule, trigger_type: String) {
    // 1. 注册表占位（entry 原子匹配；DashMap 引用立即 drop）。
    //    依赖触发占位失败走置 pending 分流（§5.7 重入细节：合并补发
    //    语义 ≠ 跳过，不写 missed_pending 噪音 run 记录）——覆盖
    //    dispatch 预查与占位之间的 TOCTOU 窗口
    let Some(cancel_token) = state.registry.try_register(&schedule.id, &trigger_type) else {
        if trigger_type == "dependency" {
            return set_pending_dependency(&state, &schedule.id).await;
        }
        return mark_missed_pending(&state, &schedule, &trigger_type).await;
    };
    // 1'. panic 安全护栏（scopeguard）：wrapper 任意退出路径（正常/超时/panic
    //     unwinding）drop 时保证注册表条目被移除；同步清理 pending_deletes——
    //     排队中被 DELETE 取消的任务静默退出不经 finalize，DashSet 条目若
    //     残留会导致同 id 重建后的首次 finalize 误跳 last_* merge（remove 幂等，
    //     与 finalize 内的清理共存）
    let registry_for_guard = state.registry.clone();
    let pending_for_guard = state.pending_deletes.clone();
    let guard_id = schedule.id.clone();
    let _slot_guard = scopeguard::guard(guard_id, |id| {
        registry_for_guard.remove(&id);
        pending_for_guard.remove(&id);
    });

    // 2. 排队获取槽位；排队期间同样响应取消信号（停机/删除对排队任务生效）
    let is_handoff = trigger_type == "handoff" || schedule.is_handoff();
    let queued_since = std::time::Instant::now();
    let pool = if is_handoff {
        state.handoff_pool.clone()
    } else {
        state.normal_pool.clone()
    };
    let _permit = tokio::select! {
        permit = pool.acquire() => match permit {
            Ok(permit) => permit,
            Err(_) => {
                tracing::error!(schedule.id = %schedule.id, "并发信号量已关闭，放弃本次触发");
                return;
            }
        },
        _ = cancel_token.cancelled() => {
            tracing::info!(schedule.id = %schedule.id, "排队中被取消（停机/删除）：静默退出，重启后按 auto_catch_up 处理");
            return;
        }
    };
    if is_handoff && queued_since.elapsed() > HANDOFF_QUEUE_WARN_AFTER {
        tracing::warn!(
            schedule.id = %schedule.id,
            queued_secs = queued_since.elapsed().as_secs(),
            "交接任务排队超过 10 分钟（handoff 槽位被长任务占用）"
        );
    }
    // 资源熔断期间停止接受新任务（内存/磁盘双标志独立判定；已排队的不打断，
    // 醒来后放弃本次触发）
    let memory_ok = state.memory_ok.load(std::sync::atomic::Ordering::Relaxed);
    let disk_ok = state.disk_ok.load(std::sync::atomic::Ordering::Relaxed);
    if !memory_ok || !disk_ok {
        tracing::warn!(schedule.id = %schedule.id, "资源熔断中（内存/磁盘超限），放弃本次触发");
        return;
    }

    // 3. TOCTOU 重校验：排队期间可能已被 DELETE/pause/出区间
    let now = Utc::now();
    let Some(schedule) = state
        .store
        .find(&schedule.id)
        .await
        .filter(|s| s.status == "active" && trigger::in_effective_range(s, now))
    else {
        tracing::info!(schedule.id = %schedule.id, "schedule 已删除/暂停/出生效区间，放弃排队中的触发");
        return;
    };

    // 4. 运行中状态通道（§5.2/§5.2.1）：TOCTOU 重校验通过、即将真正执行——
    //    生成 run_id 写 run 占位记录（status='running'，finished_at=null）并
    //    更新 last_status='running'。这是崩溃恢复两通道的输入（runs 扫描无
    //    finished_at + schedules.json 扫描 last_status='running'），也是前端
    //    "执行中"状态的来源。排队中被取消/熔断放弃的路径未走到此处，不写占位
    //    （未真正开始执行）；finalize 以同一 run_id 覆盖回填
    let started_at = Utc::now();
    let run_id = uuid::Uuid::new_v4().to_string();
    let placeholder = RunRecord {
        id: run_id.clone(),
        schedule_id: schedule.id.clone(),
        trigger_type: trigger_type.clone(),
        started_at: started_at.to_rfc3339(),
        finished_at: None,
        status: "running".to_string(),
        output_summary: None,
        error: None,
        rollout_path: None,
        token_usage: None,
        evidence: Vec::new(),
        artifacts: Vec::new(),
    };
    if let Err(err) = state.runs.write(&placeholder).await {
        tracing::warn!(schedule.id = %schedule.id, "run 占位记录写入失败（继续执行）: {err}");
    }
    let _ = state
        .store
        .update(&schedule.id, |m| {
            m.last_status = Some("running".to_string());
            m.last_run_at = Some(started_at.to_rfc3339());
        })
        .await;

    // 5. 内层 task：真正执行（deadline 从此刻起算，排队不计入执行超时）
    let deadline =
        trigger::effective_max_execution_time(&schedule, state.opts.max_execution_time);
    let mut inner = tokio::spawn(execute_inner(
        state.codex.clone(),
        schedule.clone(),
        cancel_token.clone(),
        state.runs.clone(),
        run_id.clone(),
        trigger_type.clone(),
        started_at,
    ));
    state.registry.set_abort_handle(&schedule.id, inner.abort_handle());

    // 6. 外层 select：等内层完成 or 超时；超时分支发取消信号后
    //    grace（10s）→ 兜底 abort（同步 IO 段无法中断）→ 回收
    let joined: Result<TaskOutcome, tokio::task::JoinError> = tokio::select! {
        res = &mut inner => res,
        _ = tokio::time::sleep(deadline) => {
            // 状态先于信号（同 cancel_chain ① ②）：先写 reason 再 cancel
            state.registry.set_cancel_reason(&schedule.id, CancelReason::Timeout);
            cancel_token.cancel();
            match tokio::time::timeout(CANCEL_GRACE_PERIOD, &mut inner).await {
                Ok(res) => res,
                Err(_) => {
                    tracing::warn!(schedule.id = %schedule.id, "超时 grace 用尽，强制 abort");
                    inner.abort();
                    inner.await
                }
            }
        }
    };
    let outcome = match joined {
        Ok(o) => o,
        Err(join_err) => {
            let msg = if join_err.is_panic() {
                "任务 panic（已由护栏回收注册表条目）".to_string()
            } else {
                "任务被取消".to_string()
            };
            TaskOutcome::failed(msg)
        }
    };

    // 7. 收尾（单点）：以同一 run_id 覆盖回填占位记录 + 字段级 merge last_*；
    finalize(&state, &schedule, &trigger_type, &run_id, started_at, outcome).await;

    // 8. 检查点②（§4.13 语义③合并补发）：本条目运行期间上游 success
    //    置位的 pending 在此兑现——须先显式释放注册表占位（guard 的
    //    RAII drop 在函数末尾，不提前释放则补发实例的 try_register 必
    //    失败退化为再置 pending，事件延迟到下一次任意 run）。guard
    //    drop 幂等且此后无 panic 风险点，提前释放安全
    drop(_slot_guard);
    if let Some(fresh) = state.store.find(&schedule.id).await {
        if fresh.pending_dependency_trigger {
            let still_active = fresh.status == "active";
            let _ = state
                .store
                .update(&schedule.id, |m| {
                    m.pending_dependency_trigger = false;
                })
                .await;
            if still_active {
                tracing::info!(schedule.id = %schedule.id, "补发依赖触发（运行期间上游 success，pending 兑现）");
                spawn_task(state.clone(), fresh, "dependency".to_string());
            } else {
                tracing::info!(schedule.id = %schedule.id, "pending 清零（条目非 active，不补发）");
            }
        }
    }
}

/// 内层执行：thread/start（交接记录分流 thread/resume）→ turn/start →
/// 事件循环（响应取消 → turn/interrupt；AgentMessage 分支全文检测
/// @handoff_local 置 handoff_marker——§5.6.4 方式 3 v2.44 落点）。
async fn execute_inner(
    codex: CodexRuntime,
    schedule: Schedule,
    cancel_token: CancellationToken,
    runs: RunsStore,
    run_id: String,
    trigger_type: String,
    started_at: chrono::DateTime<Utc>,
) -> TaskOutcome {
    // §5.6.1 步骤 3（v2.43）：交接记录（is_handoff 且配置 rollout 路径）
    // 分流 thread/resume（path 指定，协议 experimental thread/resume.path），
    // 其余走 thread/start；rollout 文件缺失由服务端报错 → run failed。
    // 两响应类型不同，取共同消费字段（thread.id / thread.path）统一元组
    let (thread_id, thread_path) = if schedule.is_handoff()
        && schedule.handoff_rollout_path.is_some()
    {
        match codex.thread_resume(&schedule).await {
            Ok(r) => (r.thread.id, r.thread.path),
            Err(e) => return TaskOutcome::failed(format!("thread/resume 失败: {e}")),
        }
    } else {
        match codex.thread_start(&schedule).await {
            Ok(r) => (r.thread.id, r.thread.path),
            Err(e) => return TaskOutcome::failed(format!("thread/start 失败: {e}")),
        }
    };
    // rollout 落盘路径（§5.2 v2.43）：启动时即回填占位记录（同 run_id
    // 覆盖，run 仍 running）——反向交接下载源，运行中即可见
    let rollout_path = thread_path
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned());
    if let Some(rollout) = rollout_path.as_ref() {
        let updated = RunRecord {
            id: run_id.clone(),
            schedule_id: schedule.id.clone(),
            trigger_type: trigger_type.clone(),
            started_at: started_at.to_rfc3339(),
            finished_at: None,
            status: "running".to_string(),
            output_summary: None,
            error: None,
            rollout_path: Some(rollout.clone()),
            token_usage: None,
            evidence: Vec::new(),
            artifacts: Vec::new(),
        };
        if let Err(err) = runs.write(&updated).await {
            tracing::warn!(schedule.id = %schedule.id, "rollout_path 占位回填失败（继续执行）: {err}");
        }
    }
    let (tx, mut rx) = tokio::sync::mpsc::channel::<RoutedEvent>(1024);
    codex.register_thread(&thread_id, tx);
    // panic 护栏：execute_inner panic 时 unregister（正常路径末尾的显式
    // unregister 保留，remove 幂等）——否则 routes map 残留 entry（泄漏）
    let unreg_codex = codex.clone();
    let unreg_thread_id = thread_id.clone();
    let _routes_guard =
        scopeguard::guard((), move |_| unreg_codex.unregister_thread(&unreg_thread_id));

    // 模型覆盖（§9.2 批次 5 远程 headless）：schedule.model_override JSON 复合
    // 结构解析出 model+effort 透传 turn/start（providerIndex 远程无效——多上游
    // 路由由 adapter 按模型名承担 §9.3 层 2）；解析失败静默走默认（无人值守）
    let (ov_model, ov_effort) = schedule
        .model_override
        .as_deref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .map(|v| {
            let model = v
                .get("model")
                .and_then(|m| m.as_str())
                .filter(|m| !m.is_empty())
                .map(String::from);
            let effort = v
                .get("effort")
                .and_then(|e| e.as_str())
                .map(String::from);
            (model, effort)
        })
        .unwrap_or((None, None));

    let turn = match codex
        .turn_start(
            &thread_id,
            &schedule.prompt,
            ov_model.as_deref(),
            ov_effort.as_deref(),
        )
        .await
    {
        Ok(r) => r,
        Err(e) => {
            let mut o = TaskOutcome::failed(format!("turn/start 失败: {e}"));
            o.rollout_path = rollout_path;
            codex.unregister_thread(&thread_id);
            return o;
        }
    };
    let mut active_turn_id = turn.turn.id;
    let mut output = String::new();
    let mut handoff_marker = false;
    // §4.9 证据收集（v2.52 F2）：AgentMessage 逐条 assistant_output；
    // 四类实质动作项经 collect_item_evidence 映射（见函数注释）
    let mut evidence: Vec<serde_json::Value> = Vec::new();
    let mut artifacts: Vec<serde_json::Value> = Vec::new();
    let mut artifact_paths: std::collections::HashSet<String> = std::collections::HashSet::new();

    let mut outcome = loop {
        tokio::select! {
            ev = rx.recv() => {
                match ev {
                    Some(RoutedEvent::TurnStarted { turn_id, .. }) => {
                        active_turn_id = turn_id;
                    }
                    Some(RoutedEvent::AgentMessage { text, .. }) => {
                        output.push_str(&text);
                        // §5.6.4 方式 3（v2.44）：AgentMessage 分支全文检测
                        // （非截断摘要——标记可能出现在截断线之后）；消费与否
                        // 由 finalize 按 is_handoff 判定（独立任务忽略记 warning）
                        if !handoff_marker && text.contains(HANDOFF_MARKER) {
                            handoff_marker = true;
                            tracing::info!(
                                schedule.id = %schedule.id,
                                "检测到 @handoff_local 标记（待 finalize 按 is_handoff 消费）"
                            );
                        }
                        evidence.push(serde_json::json!({
                            "type": "assistant_output",
                            "content": text,
                        }));
                    }
                    Some(RoutedEvent::ItemCompleted { item, .. }) => {
                        collect_item_evidence(
                            &item,
                            &mut evidence,
                            &mut artifacts,
                            &mut artifact_paths,
                        );
                    }
                    Some(RoutedEvent::TurnCompleted { turn_id, status, turn_json, .. }) => {
                        if turn_id != active_turn_id {
                            // 防陈旧完成事件（interrupt 后旧 turn 的补发通知）
                            continue;
                        }
                        let mut o = TaskOutcome::from_turn(&status, output);
                        o.token_usage = turn_json
                            .get("usage")
                            .and_then(|u| serde_json::from_value(u.clone()).ok());
                        break o;
                    }
                    None => break TaskOutcome::failed("事件流意外关闭".to_string()),
                }
            }
            _ = cancel_token.cancelled() => {
                let _ = codex.turn_interrupt(&thread_id, &active_turn_id).await;
                break TaskOutcome::from_turn("Interrupted", output);
            }
        }
    };
    codex.unregister_thread(&thread_id);
    // §5.2/§5.6.4：各 outcome 路径统一携带（finalize 全量覆盖 run 记录，
    // 缺失会把占位记录中的 rollout_path 冲掉）；evidence/artifacts 同理
    outcome.rollout_path = rollout_path;
    outcome.handoff_marker = handoff_marker;
    outcome.evidence = evidence;
    outcome.artifacts = artifacts;
    outcome
}

/// §4.9 证据映射（v2.52 F2，对齐本地调度 3 变体口径）：
/// - CommandExecution → external_action "{command} → {aggregated_output}"
///   （本地 CommandBlock 同构：command → output）
/// - McpToolCall → external_action "mcp:{server}:{tool}({arguments}) →
///   {result|error}"（本地 ToolCall 同构）
/// - WebSearch → external_action "web_search({query}) → {results}"
/// - FileChange → 逐变更 local_file_mutation {path, action}（kind 细分
///   added/deleted/modified，本地 DiffBlock 仅 modified 的超集）+ artifacts
///   {name,path} 去重
/// AgentMessage 不经此函数（专用变体：output 聚积 + assistant_output）。
fn collect_item_evidence(
    item: &codex_app_server_protocol::ThreadItem,
    evidence: &mut Vec<serde_json::Value>,
    artifacts: &mut Vec<serde_json::Value>,
    artifact_paths: &mut std::collections::HashSet<String>,
) {
    use codex_app_server_protocol::PatchChangeKind;
    use codex_app_server_protocol::ThreadItem;
    match item {
        ThreadItem::CommandExecution {
            command,
            aggregated_output,
            ..
        } => {
            let output = aggregated_output.clone().unwrap_or_default();
            if command.is_empty() && output.is_empty() {
                return;
            }
            evidence.push(serde_json::json!({
                "type": "external_action",
                "description": format!("{command} → {output}"),
            }));
        }
        ThreadItem::FileChange { changes, .. } => {
            for change in changes {
                let action = match &change.kind {
                    PatchChangeKind::Add => "added",
                    PatchChangeKind::Delete => "deleted",
                    PatchChangeKind::Update { .. } => "modified",
                };
                evidence.push(serde_json::json!({
                    "type": "local_file_mutation",
                    "path": change.path,
                    "action": action,
                }));
                push_artifact(&change.path, artifacts, artifact_paths);
            }
        }
        ThreadItem::McpToolCall {
            server,
            tool,
            arguments,
            result,
            error,
            ..
        } => {
            let result_text = if let Some(err) = error {
                err.message.clone()
            } else if let Some(res) = result {
                serde_json::to_string(&res.content).unwrap_or_default()
            } else {
                String::new()
            };
            evidence.push(serde_json::json!({
                "type": "external_action",
                "description": format!("mcp:{server}:{tool}({arguments}) → {result_text}"),
            }));
        }
        ThreadItem::WebSearch(w) => {
            let result_text = w
                .results
                .as_ref()
                .map(|r| serde_json::to_string(r).unwrap_or_default())
                .unwrap_or_default();
            evidence.push(serde_json::json!({
                "type": "external_action",
                "description": format!("web_search({}) → {result_text}", w.query),
            }));
        }
        _ => {}
    }
}

/// artifacts 追加（{name,path}，路径去重；空路径跳过）——口径对齐本地
/// RunCollect::push_artifact。
fn push_artifact(
    path: &str,
    artifacts: &mut Vec<serde_json::Value>,
    artifact_paths: &mut std::collections::HashSet<String>,
) {
    if path.is_empty() || !artifact_paths.insert(path.to_string()) {
        return;
    }
    let name = std::path::Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string());
    artifacts.push(serde_json::json!({ "name": name, "path": path }));
}

/// 统一取消链（外部调用方：DELETE / 内存熔断 / 优雅停机）：
/// 先写 reason（状态先于信号）→ token.cancel → 等待注册表条目移除
/// （任务退出且 finalize 落盘完成，grace 10s）→ 超时经 AbortHandle 强杀。
/// 不写任何状态——收尾单点由外层 finalize 执行；
/// 回收（await）由 wrapper 外层 select 完成。
/// 超时路径不经过此函数（wrapper 持有 JoinHandle 自行 grace/abort）。
pub async fn cancel_chain(state: &DaemonState, schedule_id: &str, reason: CancelReason) {
    let Some((token, abort_handle, _existing_reason)) = state.registry.slot_snapshot(schedule_id)
    else {
        return;
    };
    // ① 先写 reason（状态先于信号：保证 finalize 读取时已就位）
    state.registry.set_cancel_reason(schedule_id, reason);
    // ② 再发取消信号：内层 select 响应 → interrupt codex turn；
    //    排队中 wrapper 的 acquire select 同样响应 → 静默退出
    token.cancel();
    let Some(abort_handle) = abort_handle else {
        // 排队中任务（内层尚未 spawn）：token 已使 wrapper 静默退出
        return;
    };
    // ③ grace：等待注册表条目移除（任务退出 + finalize 完成），上限 10s
    let deadline = tokio::time::Instant::now() + CANCEL_GRACE_PERIOD;
    while state.registry.contains(schedule_id) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // ④ 兜底强杀（同步 IO 段无法中断）；abort 作用于快照中的句柄，
    //    同 id 新实例不会被误杀（至多多等一个 grace 周期）
    if state.registry.contains(schedule_id) {
        tracing::warn!(schedule.id = %schedule_id, "取消 grace 用尽，强制 abort");
        abort_handle.abort();
    }
}

/// 收尾单点：以同一 run_id 覆盖回填占位记录 + 唯一写 last_* 字段级 merge 的位置。
async fn finalize(
    state: &Arc<DaemonState>,
    schedule: &Schedule,
    trigger_type: &str,
    run_id: &str,
    started_at: chrono::DateTime<Utc>,
    outcome: TaskOutcome,
) {
    // cancel_reason 在 token.cancel 前写入，此处读取必已就位；
    // 但任务在取消生效前真实完成时以真实结果为准（幂等防御）
    let (status, error) = match state.registry.cancel_reason(&schedule.id) {
        Some(reason) => {
            let (rs, re) = reason.terminal_status();
            if rs == "timeout" && outcome.status == "success" {
                (outcome.status.clone(), outcome.error.clone())
            } else {
                (rs.to_string(), Some(re.to_string()))
            }
        }
        None => (outcome.status.clone(), outcome.error.clone()),
    };

    let finished_at = Utc::now();
    let run = RunRecord {
        id: run_id.to_string(),
        schedule_id: schedule.id.clone(),
        trigger_type: trigger_type.to_string(),
        started_at: started_at.to_rfc3339(),
        finished_at: Some(finished_at.to_rfc3339()),
        status: status.clone(),
        output_summary: Some(outcome.output_summary.clone()),
        error: error.clone(),
        rollout_path: outcome.rollout_path.clone(),
        token_usage: outcome.token_usage,
        // §4.9（v2.52 F2）：execute_inner 收集的执行证据/产物随 run 落盘
        evidence: outcome.evidence,
        artifacts: outcome.artifacts,
    };
    if let Err(err) = state.runs.write(&run).await {
        tracing::error!(schedule.id = %schedule.id, "run 记录落盘失败: {err}");
    }
    state.metrics.record(&status);
    state
        .metrics
        .record_duration((finished_at - started_at).to_std().unwrap_or_default());

    // 条目已被 DELETE：仅写 run 记录（interrupted + 任务被删除），不写 last_*
    if state.pending_deletes.contains(&schedule.id) {
        state.pending_deletes.remove(&schedule.id);
        tracing::info!(schedule.id = %schedule.id, "任务已被删除，跳过 last_* merge");
        return;
    }

    // once 任务执行后自动过期（§4.1/§4.3）：**任意终态**（success/failed/
    // timeout/interrupted）执行过即 expired——不自动重试（§5.2.1 资源监控
    // "不自动重试"）；若仅 success 置 expired，失败/超时的 once 任务保持
    // active 且 next_run_at 已清，但历史版本 next_run_at 未清时会每 30s
    // 重复触发（P1 级重跑环路，此处为防御性终态收口）
    if schedule.frequency_type == "once" {
        let _ = state
            .store
            .update(&schedule.id, |m| {
                m.status = "expired".to_string();
            })
            .await;
    }

    // 字段级 merge：仅更新运行时字段，不整条写回快照
    let _ = state
        .store
        .update(&schedule.id, |m| {
            m.last_status = Some(status.clone());
            m.last_error = error;
            m.last_run_at = Some(started_at.to_rfc3339());
        })
        .await;

    // §5.6.4 反向交接检测（v2.44 写序：常规收尾之后执行，handed_off
    // 覆盖 once-expired——反序则 expired 吃掉待接管标记）：
    // - is_handoff 记录（original_schedule_id 非空）任意终态 → 回流本地；
    // - 配置 handoff_to_agent_id 的独立记录任意终态 → 按配置回流；
    // - 独立记录的 @handoff_local 标记忽略并记 warning（远程不识本地
    //   agent 清单，§5.6.4 方式 3）
    if schedule.is_handoff() || schedule.handoff_to_agent_id.is_some() {
        tracing::info!(
            schedule.id = %schedule.id,
            status = %status,
            "交接检测命中，标记 handed_off（待本地接管，§5.6.4）"
        );
        let _ = state
            .store
            .update(&schedule.id, |m| {
                m.status = "handed_off".to_string();
            })
            .await;
    } else if outcome.handoff_marker {
        tracing::warn!(
            schedule.id = %schedule.id,
            "独立远程任务输出含 @handoff_local 标记，忽略（仅 is_handoff 记录生效，§5.6.4）"
        );
    }

    // 检查点①（§4.13 语义①④）：success 后按水位触发下游（事件旁路，
    // 与常规频率调度并存；trigger_type='dependency'）
    if status == "success" {
        dispatch_dependents(state, &schedule.id).await;
    }

    // §5.4 推送：push_to_bot=true 时经 --bot-web-server-url 落通知
    // （推送与 run 记录两条独立路径，通知丢失不补发）
    push_notification(state, schedule, &status, &outcome.output_summary, finished_at).await;
}

/// §5.4 推送：push_to_bot=true 且推送三参数齐全时，fire-and-forget
/// 经 bot-web-server POST /notifications/push 落通知。
/// 终态映射：success→success，其余（failed/timeout/interrupted）→failed；
/// 推送失败仅记日志，不影响 run 记录（通知丢失不补发）。
async fn push_notification(
    state: &Arc<DaemonState>,
    schedule: &Schedule,
    status: &str,
    summary: &str,
    finished_at: chrono::DateTime<Utc>,
) {
    if !schedule.push_to_bot {
        return;
    }
    let Some(base_url) = state.opts.bot_web_server_url.as_ref() else {
        tracing::warn!(
            schedule.id = %schedule.id,
            "push_to_bot=true 但未配置 --bot-web-server-url，跳过推送"
        );
        return;
    };
    let Some(username) = state.opts.bot_web_server_username.as_ref() else {
        tracing::warn!(
            schedule.id = %schedule.id,
            "push_to_bot=true 但未配置 --bot-web-server-username，跳过推送"
        );
        return;
    };
    let Some(push_token) = state.opts.bot_web_server_push_token.as_ref() else {
        tracing::warn!(
            schedule.id = %schedule.id,
            "push_to_bot=true 但未配置 --bot-web-server-push-token，跳过推送"
        );
        return;
    };

    let url = format!(
        "{}/notifications/push",
        base_url.trim_end_matches('/')
    );
    let push_status = if status == "success" { "success" } else { "failed" };
    // 端点 taskName min_length=1：空名回退 schedule id
    let task_name = if schedule.name.is_empty() {
        schedule.id.as_str()
    } else {
        schedule.name.as_str()
    };
    // 摘要截 500 字符，空串转 null
    let summary_value: serde_json::Value = if summary.is_empty() {
        serde_json::Value::Null
    } else {
        let mut chars = summary.chars();
        let truncated: String = chars.by_ref().take(500).collect();
        let suffix = if chars.next().is_some() { "…" } else { "" };
        serde_json::Value::String(format!("{truncated}{suffix}"))
    };
    let body = serde_json::json!({
        "username": username,
        "taskName": task_name,
        "status": push_status,
        "summary": summary_value,
        "occurredAt": finished_at.timestamp_millis(),
    });

    // fire-and-forget：curl 子进程（不引 HTTP client 依赖），finalize 不等待
    let body_str = body.to_string();
    let push_token = push_token.clone();
    let schedule_id = schedule.id.clone();
    tokio::spawn(async move {
        let attempt = tokio::process::Command::new("curl")
            .arg("-sS")
            .arg("-X")
            .arg("POST")
            .arg(&url)
            .arg("-H")
            .arg("Content-Type: application/json")
            .arg("-H")
            .arg(format!("X-Push-Token: {push_token}"))
            .arg("--max-time")
            .arg("10")
            .arg("--data-binary")
            .arg(&body_str)
            .output()
            .await;
        match attempt {
            Ok(out) if out.status.success() => {
                tracing::info!(schedule.id = %schedule_id, "§5.4 推送通知成功");
            }
            Ok(out) => {
                tracing::warn!(
                    schedule.id = %schedule_id,
                    "§5.4 推送通知失败: curl exit={}, stderr={}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr)
                );
            }
            Err(err) => {
                tracing::warn!(schedule.id = %schedule_id, "§5.4 推送通知失败: {err}");
            }
        }
    });
}

/// 同 schedule 重入被跳过：写一条 missed_pending run 记录 + 更新 last_status。
async fn mark_missed_pending(state: &Arc<DaemonState>, schedule: &Schedule, trigger_type: &str) {
    let now = Utc::now();
    let run = RunRecord {
        id: uuid::Uuid::new_v4().to_string(),
        schedule_id: schedule.id.clone(),
        trigger_type: trigger_type.to_string(),
        started_at: now.to_rfc3339(),
        finished_at: Some(now.to_rfc3339()),
        status: "missed_pending".to_string(),
        output_summary: None,
        error: Some("上一实例仍在执行（运行中/排队中），跳过本次触发".to_string()),
        rollout_path: None,
        token_usage: None,
        evidence: Vec::new(),
        artifacts: Vec::new(),
    };
    if let Err(err) = state.runs.write(&run).await {
        tracing::error!(schedule.id = %schedule.id, "missed_pending 记录落盘失败: {err}");
    }
    state.metrics.record_missed();
    let _ = state
        .store
        .update(&schedule.id, |m| {
            m.last_status = Some("missed_pending".to_string());
        })
        .await;
}
/// 置依赖合并补发标志（B running 中上游 success → B finalize 检查点②兑现）。
async fn set_pending_dependency(state: &Arc<DaemonState>, schedule_id: &str) {
    let _ = state
        .store
        .update(schedule_id, |m| {
            m.pending_dependency_trigger = true;
        })
        .await;
    tracing::info!(schedule.id = %schedule_id, "依赖触发时任务运行中，置 pending 合并补发");
}

/// 水位检查（§4.13 语义②，快照口径）：B 的全部上游 d 须
/// last_status=='success' 且 d.last_run_at > B.last_run_at（B.last_run_at
/// NULL 恒满足；上游字段缺失 → 不满足）。DateTime 解析比较——RFC3339
/// 纳秒精度变长，字典序不可靠。
async fn dependency_ready(state: &DaemonState, b: &Schedule) -> bool {
    let Some(deps) = b.depends_on.as_ref() else {
        return false;
    };
    if deps.is_empty() {
        return false;
    }
    let snapshot = state.store.snapshot().await;
    let b_last = b
        .last_run_at
        .as_deref()
        .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
        .map(|d| d.with_timezone(&Utc));
    for dep_id in deps {
        let Some(d) = snapshot.iter().find(|s| &s.id == dep_id) else {
            return false;
        };
        if d.last_status.as_deref() != Some("success") {
            return false;
        }
        let Some(d_last) = d
            .last_run_at
            .as_deref()
            .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
            .map(|dt| dt.with_timezone(&Utc))
        else {
            return false;
        };
        if let Some(blt) = b_last {
            if d_last <= blt {
                return false;
            }
        }
    }
    true
}

/// 检查点①（§4.13 语义①②③）：上游 success 后扫描下游，水位满足即触发。
/// running/排队中（registry 占位）→ 置 pending 合并补发；否则 spawn
/// （先查 contains 再决定，§5.7 重入细节——TOCTOU 窗口由 wrapper 的
/// dependency 分流兜底）。
async fn dispatch_dependents(state: &Arc<DaemonState>, upstream_id: &str) {
    let snapshot = state.store.snapshot().await;
    for b in snapshot {
        let Some(deps) = b.depends_on.as_ref() else { continue };
        if !deps.iter().any(|d| d == upstream_id) {
            continue;
        }
        if b.status != "active" {
            continue;
        }
        if !dependency_ready(state, &b).await {
            continue;
        }
        if state.registry.contains(&b.id) {
            set_pending_dependency(state, &b.id).await;
        } else {
            tracing::info!(
                schedule.id = %b.id,
                upstream.id = %upstream_id,
                "依赖触发（上游 success 且水位满足）"
            );
            spawn_task(state.clone(), b, "dependency".to_string());
        }
    }
}

/// 启动恢复（§5.7 检查点③远程对应，recover_running_flags 同期）：扫描
/// pending=true 且水位满足且 idle 的条目补发——进程停机期间上游 success
/// 的事件不丢失。
pub async fn recover_pending_dependencies(state: &Arc<DaemonState>) {
    for b in state.store.snapshot().await {
        if !b.pending_dependency_trigger || b.status != "active" {
            continue;
        }
        if state.registry.contains(&b.id) {
            continue;
        }
        if !dependency_ready(state, &b).await {
            // 水位不满足：清零（上游未全部成功，事件语义上未成立）
            let _ = state
                .store
                .update(&b.id, |m| {
                    m.pending_dependency_trigger = false;
                })
                .await;
            continue;
        }
        let _ = state
            .store
            .update(&b.id, |m| {
                m.pending_dependency_trigger = false;
            })
            .await;
        tracing::info!(schedule.id = %b.id, "启动恢复：补发依赖触发（pending 且水位满足）");
        spawn_task(state.clone(), b, "dependency".to_string());
    }
}