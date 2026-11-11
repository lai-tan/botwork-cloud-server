// Copyright (c) 2026 Jehu Shaw. All rights reserved.

//! `botwork-cloud-server --schedule` 调度守护进程
//! （AUTOMATION-DESIGN.md §5.2 / §5.2.1 v2.16 定稿）：
//! - 集成内置 API 适配器 + codex app-server（in_process）+ 调度器；
//! - 启动顺序：日志轮转 → 配置加载（首次创建）→ 崩溃恢复 → codex 运行时
//!   → 管理 API → 资源监控 → 启动补跑扫描 → 调度主循环；
//! - SIGTERM 优雅停机：停止新触发 → 管理 API 503 → 对注册表全部条目
//!   （含排队）并发取消链 → 等待注册表清空（上限 30s）→ 落盘退出。

pub mod api;
pub mod codex_runtime;
pub mod config;
pub mod executor;
pub mod monitor;
pub mod registry;
pub mod runs;
pub mod trigger;

use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use chrono::DateTime;
use chrono::Utc;
use clap::Args as ClapArgs;
use dashmap::DashSet;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use adapter_shared::adapter::ApiAdapter;
use codex_arg0::Arg0DispatchPaths;
use codex_config::LoaderOverrides;
use codex_protocol::protocol::SessionSource;
use codex_utils_cli::CliConfigOverrides;

use codex_runtime::CodexRuntime;
use config::Schedule;
use config::ScheduleStore;
use registry::CancelReason;
use registry::TaskRegistry;
use runs::RunsStore;

/// `--schedule` 参数组（与现有 CLI 参数平级，见 §5.2 启动参数）。
#[derive(Debug, Clone, ClapArgs)]
pub struct ScheduleArgs {
    /// 进入调度守护进程模式（与 --listen 互斥，触发时进程内调用 codex）
    #[arg(long = "schedule", default_value_t = false)]
    pub schedule: bool,

    /// schedules.json 路径（缺省 $HOME/bot-work/schedules.json）。
    /// 注意：不能用 `--config`——顶层 CliConfigOverrides 的 `-c/--config key=value`
    /// 是 global 参数，同名会被其抢先消费并按 override 解析失败。
    #[arg(long = "schedule-config", value_name = "FILE")]
    pub config_path: Option<PathBuf>,

    /// 总并发上限（静态拆分为普通池 + 交接池）
    #[arg(long = "max-concurrent", default_value_t = 5)]
    pub max_concurrent: usize,

    /// 交接任务专属槽位数（不被普通任务侵占）
    #[arg(long = "handoff-slots", default_value_t = 2)]
    pub handoff_slots: usize,

    /// 普通任务全局默认执行超时（秒；per-schedule 可覆盖，handoff 默认 24h）
    #[arg(long = "max-execution-time", default_value_t = 1800)]
    pub max_execution_time: u64,

    /// 健康检查与管理 API 端口（仅绑定 127.0.0.1）
    #[arg(long = "health-port", default_value_t = 8081)]
    pub health_port: u16,

    /// 管理 API Bearer 认证令牌（默认无认证；同机有不可信服务时启用）
    #[arg(long = "api-token", value_name = "TOKEN")]
    pub api_token: Option<String>,

    /// 推送通知的 bot web server 地址（Phase 4 启用）
    #[arg(long = "bot-web-server-url", value_name = "URL")]
    pub bot_web_server_url: Option<String>,

    /// 推送通知目标用户名（bot-web-server 按用户名落通知，与 --bot-web-server-url 配对）
    #[arg(long = "bot-web-server-username", value_name = "USERNAME")]
    pub bot_web_server_username: Option<String>,

    /// 推送通知 service token（bot-web-server X-Push-Token 认证）
    #[arg(long = "bot-web-server-push-token", value_name = "TOKEN")]
    pub bot_web_server_push_token: Option<String>,
}

#[derive(Default)]
pub struct Metrics {
    pub completed: AtomicU64,
    pub failed: AtomicU64,
    pub timeout: AtomicU64,
    pub interrupted: AtomicU64,
    pub missed_pending: AtomicU64,
    // 执行时长 histogram（秒，§5.2.1 /metrics）：手写桶计数，不引 prometheus crate
    pub duration_le_60: AtomicU64,
    pub duration_le_300: AtomicU64,
    pub duration_le_1800: AtomicU64,
    pub duration_le_3600: AtomicU64,
    pub duration_le_inf: AtomicU64,
    pub duration_sum_ms: AtomicU64,
}

impl Metrics {
    pub fn record(&self, status: &str) {
        match status {
            "success" => {
                self.completed.fetch_add(1, Ordering::Relaxed);
            }
            "failed" => {
                self.failed.fetch_add(1, Ordering::Relaxed);
            }
            "timeout" => {
                self.timeout.fetch_add(1, Ordering::Relaxed);
            }
            "interrupted" => {
                self.interrupted.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
    }

    pub fn record_missed(&self) {
        self.missed_pending.fetch_add(1, Ordering::Relaxed);
    }

    /// 执行时长 histogram（cumulative buckets + sum + count，Prometheus 口径）。
    pub fn record_duration(&self, duration: Duration) {
        let secs = duration.as_secs_f64();
        if secs <= 60.0 {
            self.duration_le_60.fetch_add(1, Ordering::Relaxed);
        }
        if secs <= 300.0 {
            self.duration_le_300.fetch_add(1, Ordering::Relaxed);
        }
        if secs <= 1800.0 {
            self.duration_le_1800.fetch_add(1, Ordering::Relaxed);
        }
        if secs <= 3600.0 {
            self.duration_le_3600.fetch_add(1, Ordering::Relaxed);
        }
        self.duration_le_inf.fetch_add(1, Ordering::Relaxed);
        self.duration_sum_ms
            .fetch_add(duration.as_millis() as u64, Ordering::Relaxed);
    }
}

pub struct DaemonState {
    pub opts: ScheduleArgs,
    pub store: ScheduleStore,
    pub runs: RunsStore,
    pub registry: TaskRegistry,
    pub normal_pool: Arc<Semaphore>,
    pub handoff_pool: Arc<Semaphore>,
    pub codex: CodexRuntime,
    pub pending_deletes: DashSet<String>,
    pub shutdown: CancellationToken,
    pub ready: AtomicBool,
    /// 资源熔断双标志（内存/磁盘独立判定，accepting = memory_ok && disk_ok；
    /// 拆分原因：单标志下磁盘 >90% 的 false 会被内存 <80% 分支的恢复写覆盖，
    /// 磁盘保护每次仅存活 <30s）
    pub memory_ok: AtomicBool,
    pub disk_ok: AtomicBool,
    /// 调度循环心跳（epoch 秒，每轮 loop 更新）：/health 检测循环卡死用
    pub last_tick: AtomicI64,
    pub metrics: Metrics,
}

pub struct ScheduleDaemonOptions {
    pub arg0_paths: Arg0DispatchPaths,
    pub cli_config_overrides: CliConfigOverrides,
    pub loader_overrides: LoaderOverrides,
    pub strict_config: bool,
    pub session_source: SessionSource,
    pub schedule_args: ScheduleArgs,
    pub adapter: Option<ApiAdapter>,
}

pub async fn run_schedule_daemon(opts: ScheduleDaemonOptions) -> Result<()> {
    let sargs = opts.schedule_args.clone();
    let config_path = match sargs.config_path.clone() {
        Some(p) => p,
        None => default_config_path()?,
    };
    let base_dir = config_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let _log_guard = init_tracing(&base_dir.join("logs"))?;
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        config = %config_path.display(),
        "botwork-cloud-server --schedule 启动"
    );

    if sargs.handoff_slots == 0 || sargs.handoff_slots >= sargs.max_concurrent {
        bail!(
            "--handoff-slots ({}) 必须在 1..--max-concurrent ({}) 范围内",
            sargs.handoff_slots,
            sargs.max_concurrent
        );
    }
    let normal_slots = sargs.max_concurrent - sargs.handoff_slots;
    let handoff_slots = sargs.handoff_slots;

    let store = ScheduleStore::open(config_path).await?;
    let runs = RunsStore::open(base_dir.join("runs")).await?;
    config::recover_running_flags(&store).await?;
    runs.scan_and_recover().await?;

    let shutdown = CancellationToken::new();
    let codex = CodexRuntime::start(
        opts.arg0_paths,
        opts.cli_config_overrides,
        opts.loader_overrides,
        opts.strict_config,
        opts.session_source,
        shutdown.clone(),
    )
    .await?;

    let state = Arc::new(DaemonState {
        opts: sargs,
        store,
        runs,
        registry: TaskRegistry::new(),
        normal_pool: Arc::new(Semaphore::new(normal_slots)),
        handoff_pool: Arc::new(Semaphore::new(handoff_slots)),
        codex,
        pending_deletes: DashSet::new(),
        shutdown: shutdown.clone(),

        ready: AtomicBool::new(true),
        memory_ok: AtomicBool::new(true),
        disk_ok: AtomicBool::new(true),
        last_tick: AtomicI64::new(Utc::now().timestamp()),
        metrics: Metrics::default(),
    });

    startup_catch_up(&state).await;
    executor::recover_pending_dependencies(&state).await;

    let api_state = state.clone();
    let api_task = tokio::spawn(async move {
        if let Err(err) = api::serve(api_state).await {
            tracing::error!("管理 API 异常退出: {err}");
        }
    });
    let monitor_state = state.clone();
    let monitor_task = tokio::spawn(async move {
        monitor::run_monitor(monitor_state).await;
    });
    let loop_state = state.clone();
    let loop_task = tokio::spawn(async move {
        scheduling_loop(loop_state).await;
    });

    wait_for_shutdown_signal().await;
    tracing::info!("收到停机信号，开始优雅停机（上限 30s）");
    shutdown.cancel();

    // 对注册表中所有条目（含运行中与排队中）并发调用取消链
    let cancel_ids = state.registry.ids();
    for id in &cancel_ids {
        if let Some((trigger_type, started_at)) = state.registry.trigger_info(id) {
            tracing::info!(
                schedule.id = %id,
                trigger_type = %trigger_type,
                started_at = %started_at.to_rfc3339(),
                "停机取消：注册表残留任务"
            );
        }
    }
    let mut cancel_tasks = Vec::with_capacity(cancel_ids.len());
    for id in cancel_ids {
        let st = state.clone();
        cancel_tasks.push(tokio::spawn(async move {
            executor::cancel_chain(&st, &id, CancelReason::Shutdown).await;
        }));
    }

    // 等待注册表清空（含 finalize 落盘），总上限 30 秒
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !state.registry.is_empty() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    // 各 cancel_chain 已含 10s grace；不再无限等待其返回（极端场景总停机
    // 时长会超 init 脚本 35s KILL 线），3s 上限后交由下方 abort 兜底
    for t in cancel_tasks {
        let _ = tokio::time::timeout(Duration::from_secs(3), t).await;
    }
    // 超时未清空的条目直接 abort，依赖下次启动的崩溃恢复标记
    for (id, _) in state.registry.oldest_first() {
        if let Some((_, Some(handle), _)) = state.registry.slot_snapshot(&id) {
            tracing::warn!(schedule.id = %id, "停机等待超时，强制 abort");
            handle.abort();
        }
    }

    let _ = tokio::time::timeout(Duration::from_secs(3), loop_task).await;
    let _ = tokio::time::timeout(Duration::from_secs(3), api_task).await;
    let _ = tokio::time::timeout(Duration::from_secs(3), monitor_task).await;
    state.codex.request_shutdown();
    if let Some(adapter) = opts.adapter {
        adapter.stop().await;
    }
    tracing::info!("优雅停机完成");
    Ok(())
}

impl ScheduleArgs {
    pub fn is_enabled(&self) -> bool {
        self.schedule
    }
}

fn default_config_path() -> Result<PathBuf> {
    let home = std::env::var("HOME")
        .context("HOME 未设置（chroot 环境请 export HOME=/root，见 §5.2 部署）")?;
    Ok(PathBuf::from(home).join("bot-work").join("schedules.json"))
}

/// tracing 初始化：stderr + 按天滚动文件（保留 30 天，§5.2 2b 日志与轮转）。
fn init_tracing(log_dir: &Path) -> Result<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;

    std::fs::create_dir_all(log_dir)
        .with_context(|| format!("创建日志目录失败: {}", log_dir.display()))?;
    let appender = tracing_appender::rolling::Builder::new()
        .filename_prefix("schedule")
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .max_log_files(30)
        .build(log_dir)
        .with_context(|| format!("初始化日志滚动失败: {}", log_dir.display()))?;
    let (non_blocking, guard) = tracing_appender::non_blocking(appender);
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_ansi(false))
        .with(tracing_subscriber::fmt::layer().with_ansi(false).with_writer(non_blocking))
        .init();
    Ok(guard)
}

/// 启动补跑扫描（§4.3）：next_run_at 落在过去的条目——
/// once 直接 expired；auto_catch_up=true 补跑最近 1 次；否则标 missed_pending。
async fn startup_catch_up(state: &Arc<DaemonState>) {
    let now = Utc::now();
    for s in state.store.snapshot().await {
        let Some(nra) = s
            .next_run_at
            .as_deref()
            .and_then(|v| DateTime::parse_from_rfc3339(v).ok())
            .map(|d| d.with_timezone(&Utc))
        else {
            continue;
        };
        if nra >= now || s.status != "active" || !trigger::in_effective_range(&s, now) {
            continue;
        }
        let trigger_type = trigger_type_of(&s);
        match s.frequency_type.as_str() {
            "once" => {
                let _ = state
                    .store
                    .update(&s.id, |m| {
                        m.status = "expired".to_string();
                        m.next_run_at = None;
                    })
                    .await;
                tracing::info!(schedule.id = %s.id, "once 任务错过触发点，标记 expired（不补跑）");
            }
            _ if s.auto_catch_up => {
                advance_next_run_at(state, &s, nra).await;
                tracing::info!(schedule.id = %s.id, "补跑最近 1 次错过的触发（catch_up）");
                executor::spawn_task(state.clone(), s, "catch_up".to_string());
            }
            _ => {
                advance_next_run_at(state, &s, nra).await;
                let run = runs::RunRecord {
                    id: uuid::Uuid::new_v4().to_string(),
                    schedule_id: s.id.clone(),
                    trigger_type,
                    started_at: now.to_rfc3339(),
                    finished_at: Some(now.to_rfc3339()),
                    status: "missed_pending".to_string(),
                    output_summary: None,
                    error: Some("进程停机期间错过触发点（auto_catch_up=false）".to_string()),
                    rollout_path: None,
                    token_usage: None,
                    evidence: Vec::new(),
                    artifacts: Vec::new(),
                };
                if let Err(err) = state.runs.write(&run).await {
                    tracing::error!(schedule.id = %s.id, "missed_pending 记录落盘失败: {err}");
                }
                state.metrics.record_missed();
                let _ = state
                    .store
                    .update(&s.id, |m| {
                        m.last_status = Some("missed_pending".to_string());
                    })
                    .await;
            }
        }
    }
}

fn trigger_type_of(s: &Schedule) -> String {
    if s.is_handoff() {
        "handoff".to_string()
    } else {
        "scheduled".to_string()
    }
}

/// 触发后推进 next_run_at（基于本次触发点计算，防慢机器漂移累积；
/// 含 Jitter）。once 任务触发即清空 next_run_at（单次语义：运行期与终态
/// 判定均不再依赖该字段，finalize 置 expired，见 scheduling_loop None 分支）。
async fn advance_next_run_at(state: &Arc<DaemonState>, s: &Schedule, fired_at: DateTime<Utc>) {
    if s.frequency_type == "once" {
        let _ = state
            .store
            .update(&s.id, |m| {
                m.next_run_at = None;
            })
            .await;
        return;
    }
    let next = trigger::next_run_at_with_jitter(s, fired_at + chrono::Duration::seconds(1));
    let _ = state
        .store
        .update(&s.id, |m| {
            m.next_run_at = next.map(|t| t.to_rfc3339());
        })
        .await;
}

async fn init_next_run_at(state: &Arc<DaemonState>, s: &Schedule, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let next = trigger::next_run_at_with_jitter(s, now);
    // 仅在算出有效触发点时写盘：cron 未启用/配置非法返回 None 时避免
    // 调度循环每 30s 重复落盘（告警去重见 scheduling_loop 的 warned_no_trigger）
    if let Some(t) = next {
        let _ = state
            .store
            .update(&s.id, |m| {
                m.next_run_at = Some(t.to_rfc3339());
            })
            .await;
    }
    next
}

/// 时钟跳变后的重对齐：全部重算 next_run_at（跳变期间跨过的触发点不补跑，
/// §5.2.1 时钟稳定性）。
async fn realign_all(state: &Arc<DaemonState>, now: DateTime<Utc>) {
    for s in state.store.snapshot().await {
        if s.frequency_type == "interval" {
            // interval 为 monotonic 语义：不基于墙钟锚点回溯，时钟前跳后把
            // next_run_at 重算为 now + interval（推到未来）——若沿用墙钟算出的
            // 过去值，调度循环 now >= nra 会立即触发一次（等效补跑），违背
            // "跳变跨过的触发点不补跑"口径
            let next = trigger::next_run_at_with_jitter(&s, now);
            if let Some(t) = next {
                let _ = state
                    .store
                    .update(&s.id, |m| {
                        m.next_run_at = Some(t.to_rfc3339());
                    })
                    .await;
            }
            continue;
        }
        init_next_run_at(state, &s, now).await;
    }
}

async fn scheduling_loop(state: Arc<DaemonState>) {
    let mut last_wall = Utc::now();
    // 无法计算触发点的条目（cron 未启用/配置非法）告警一次即止，避免每 30s
    // 重复告警；配置经 POST 修正后条目算出有效 next_run_at 时自动重置
    let mut warned_no_trigger: HashSet<String> = HashSet::new();
    loop {
        // 调度循环心跳：/health 检测超 90s（3 个周期）未更新即判卡死（503）
        state.last_tick.store(Utc::now().timestamp(), Ordering::Relaxed);
        let now = Utc::now();
        if now < last_wall || (now - last_wall).num_seconds() > 3600 {
            tracing::warn!(
                "检测到系统时钟跳变（{last_wall} → {now}），重对齐 next_run_at；跳变期间跨过的触发点不补跑"
            );
            realign_all(&state, now).await;
        }
        last_wall = now;

        let mut next_wake = chrono::Duration::seconds(30);
        for s in state.store.snapshot().await {
            if s.status != "active" || !trigger::in_effective_range(&s, now) {
                // 条目暂停/出区间时顺带重置告警去重（恢复后若仍无法计算可再告警）
                warned_no_trigger.remove(&s.id);
                continue;
            }
            let Some(nra) = s
                .next_run_at
                .as_deref()
                .and_then(|v| DateTime::parse_from_rfc3339(v).ok())
                .map(|d| d.with_timezone(&Utc))
            else {
                if init_next_run_at(&state, &s, now).await.is_none() {
                    if s.frequency_type == "once" {
                        // once 且算不出触发点（run_at 已过）：已触发（next_run_at
                        // 已清）或错过触发点。实例在跑（含排队）→ 静默等待
                        // finalize 置 expired；无实例（错过触发点/崩溃后遗留）→
                        // 直接 expired（§4.3 单次任务错过不补跑）
                        if !state.registry.contains(&s.id) {
                            let _ = state
                                .store
                                .update(&s.id, |m| {
                                    m.status = "expired".to_string();
                                })
                                .await;
                            tracing::info!(
                                schedule.id = %s.id,
                                "once 任务无实例且触发点已过，标记 expired（不补跑）"
                            );
                        }
                    } else if warned_no_trigger.insert(s.id.clone()) {
                        tracing::warn!(
                            schedule.id = %s.id,
                            "无法计算下次触发点（cron 模式未启用或频率配置非法），跳过该 schedule；配置修正后自动恢复"
                        );
                    }
                }
                continue;
            };
            warned_no_trigger.remove(&s.id);
            if now >= nra {
                // 先推进 next_run_at（防 wrapper panic 丢触发点），再 spawn
                advance_next_run_at(&state, &s, nra).await;
                let trigger_type = trigger_type_of(&s);
                executor::spawn_task(state.clone(), s, trigger_type);
            } else {
                let delta = nra - now;
                if delta < next_wake {
                    next_wake = delta;
                }
            }
        }

        let sleep_secs = (next_wake.num_seconds() + 1).clamp(1, 30) as u64;
        tokio::select! {
            _ = state.shutdown.cancelled() => break,
            _ = tokio::time::sleep(Duration::from_secs(sleep_secs)) => {}
        }
    }
    tracing::info!("调度主循环退出");
}

async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::SignalKind;
        use tokio::signal::unix::signal;
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        tokio::select! {
            _ = term.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}