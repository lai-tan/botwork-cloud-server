// Copyright (c) 2026 Jehu Shaw. All rights reserved.

//! schedules.json 读写协议（AUTOMATION-DESIGN.md §5.2 / §5.2.1）：
//! - 进程内 `tokio::sync::RwLock` 单写者模型，所有写入 tmp+rename 原子落盘；
//! - 每次成功写盘前将上一版备份为 `.bak`，启动时主文件损坏则回退加载 `.bak`；
//! - 文件不存在时自动创建空配置 `{"schedules":[]}`（首次部署引导）；
//! - 运行时字段（last_status 等）仅由调度器 merge 写入，外部 POST 携带的
//!   运行时字段被剥离，不覆盖已有值。

use std::path::Path;
use std::path::PathBuf;

use anyhow::anyhow;
use anyhow::Context;
use anyhow::Result;
use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::RwLock;

pub const MAX_SCHEDULES: usize = 100;

/// schedules.json 中的单条调度配置。字段与本地 `schedules` 表一一对应，
/// JSON 字段名为 snake_case（设计文档 §5.2 schema）。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Schedule {
    pub id: String,
    pub agent_id: String,
    pub session_id: String,
    pub name: String,
    pub prompt: String,
    pub work_dir: Option<String>,
    /// periodic | interval | once
    pub frequency_type: String,
    /// daily | weekly | biweekly | monthly | yearly | cron
    pub periodic_mode: Option<String>,
    pub byhour: Option<u32>,
    pub byminute: Option<u32>,
    /// 星期列表："MO,WE,FR" 或数字 "0,2"（0=周日）
    pub byday: Option<String>,
    /// 日期列表："1,15"
    pub bymonthday: Option<String>,
    pub bymonth: Option<u32>,
    pub cron_expr: Option<String>,
    pub interval_seconds: Option<i64>,
    /// ISO8601 单次执行时间
    pub run_at: Option<String>,
    pub authorized_connectors: Vec<String>,
    /// default | full_access
    pub permission_level: String,
    pub push_to_bot: bool,
    pub auto_catch_up: bool,
    /// 生效日期区间（YYYY-MM-DD 或 ISO8601），留空表示始终生效
    pub effective_from: Option<String>,
    pub effective_to: Option<String>,
    /// active | paused | archived | expired | handed_off
    pub status: String,
    /// 单次执行超时秒数（NULL=按 trigger_type 取全局默认）
    pub max_execution_time: Option<i64>,
    pub handoff_to_agent_id: Option<String>,
    pub handoff_trigger: Option<String>,
    pub handoff_timeout_minutes: Option<i64>,
    pub original_schedule_id: Option<String>,
    /// 交接记录的 resume 上下文来源（§5.6.2 v2.43）：远程独有配置字段，
    /// POST body 可携带——本地交接时上传的 rollout 在远程的绝对路径；
    /// executor wrapper 以 is_handoff && 本字段非空 分流
    /// thread_resume/thread_start；本地表不镜像、同步忽略
    pub handoff_rollout_path: Option<String>,
    /// 上游任务 id 列表（§5.7/§4.13：全部 success 后触发本任务；
    /// 同一 agent 内依赖，POST body 可携带）
    pub depends_on: Option<Vec<String>>,
    /// 模型覆盖（DESIGN-ChatModelSelector §9.2 批次 5）：JSON 复合结构字符串
    /// `{providerIndex, baseUrl, model, effort}`（POST body 可携带）；
    /// 远程执行时解析出 model+effort 透传 turn/start（providerIndex 远程无效，
    /// 多上游路由由 adapter 按模型名承担——§9.3 层 2）
    pub model_override: Option<String>,
    // ---- 运行时字段（仅调度器写入）----
    pub last_run_at: Option<String>,
    pub next_run_at: Option<String>,
    pub last_status: Option<String>,
    pub last_error: Option<String>,
    /// 依赖合并补发标志：B running 中上游 success 置 1，B finalize 补发
    /// 清零（body 携带无效，config_fields_only 剔除）
    pub pending_dependency_trigger: bool,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

impl Default for Schedule {
    fn default() -> Self {
        Self {
            id: String::new(),
            agent_id: String::new(),
            session_id: String::new(),
            name: String::new(),
            prompt: String::new(),
            work_dir: None,
            frequency_type: "periodic".to_string(),
            periodic_mode: Some("daily".to_string()),
            byhour: Some(9),
            byminute: Some(0),
            byday: None,
            bymonthday: None,
            bymonth: None,
            cron_expr: None,
            interval_seconds: None,
            run_at: None,
            authorized_connectors: Vec::new(),
            permission_level: "default".to_string(),
            push_to_bot: false,
            auto_catch_up: false,
            effective_from: None,
            effective_to: None,
            status: "active".to_string(),
            max_execution_time: None,
            handoff_to_agent_id: None,
            handoff_trigger: None,
            handoff_timeout_minutes: None,
            original_schedule_id: None,
            handoff_rollout_path: None,
            depends_on: None,
            model_override: None,
            last_run_at: None,
            next_run_at: None,
            last_status: None,
            last_error: None,
            pending_dependency_trigger: false,
            created_at: None,
            updated_at: None,
        }
    }
}

impl Schedule {
    /// 返回只含配置字段、运行时字段清空的副本（POST body 规范化）。
    pub fn config_fields_only(&self) -> Self {
        let mut s = self.clone();
        s.last_run_at = None;
        s.next_run_at = None;
        s.last_status = None;
        s.last_error = None;
        s.pending_dependency_trigger = false;
        s
    }

    /// 是否为交接产生的记录（§5.6.2）：original_schedule_id 非空 = 交接来源
    /// 记录。注意：配置了 handoff_to_agent_id/handoff_trigger 的**独立调度
    /// 任务不是交接任务**（协作与独立调度并行，§5.6）——若以协作字段判定，
    /// 常规定时触发会被错误路由到交接池（2 槽 + 24h 超时）。
    pub fn is_handoff(&self) -> bool {
        self.original_schedule_id.is_some()
    }

    pub fn parse_effective_bounds(&self) -> (Option<DateTime<Utc>>, Option<DateTime<Utc>>) {
        let parse = |v: &Option<String>| {
            v.as_deref().and_then(|s| {
                DateTime::parse_from_rfc3339(s)
                    .map(|d| d.with_timezone(&Utc))
                    .ok()
                    .or_else(|| {
                        chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
                            .ok()
                            .and_then(|d| d.and_hms_opt(0, 0, 0))
                            .map(|dt| dt.and_utc())
                    })
            })
        };
        (parse(&self.effective_from), parse(&self.effective_to))
    }
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct ScheduleFile {
    #[serde(default)]
    schedules: Vec<Schedule>,
}

/// schedules.json 的进程内唯一读写入口。
pub struct ScheduleStore {
    path: PathBuf,
    inner: RwLock<Vec<Schedule>>,
}

impl ScheduleStore {
    pub async fn open(path: PathBuf) -> Result<Self> {
        let schedules = match tokio::fs::read(&path).await {
            Ok(bytes) => match serde_json::from_slice::<ScheduleFile>(&bytes) {
                Ok(file) => file.schedules,
                Err(err) => {
                    tracing::warn!("schedules.json 解析失败（{err}），尝试加载 .bak");
                    load_backup(&path).await?
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                // 主文件缺失：优先尝试 .bak——persist_atomic 在 rename(主→bak)
                // 与 rename(tmp→主) 之间崩溃时主文件缺失但 bak 仍是上一版，
                // 若直接走空配置引导会丢失全部 schedules 且下次写盘覆盖 bak
                match load_backup(&path).await {
                    Ok(schedules) => {
                        tracing::warn!(
                            "schedules.json 不存在但检测到 .bak（疑似中断的写盘），从备份恢复 {} 条",
                            schedules.len()
                        );
                        schedules
                    }
                    Err(_) => {
                        tracing::info!("schedules.json 不存在，自动创建空配置（首次部署引导）");
                        if let Some(parent) = path.parent() {
                            tokio::fs::create_dir_all(parent).await.with_context(|| {
                                format!("创建配置目录失败: {}", parent.display())
                            })?;
                        }
                        Vec::new()
                    }
                }
            }
            Err(err) => return Err(err).context(format!("读取 {} 失败", path.display())),
        };
        let store = Self {
            path,
            inner: RwLock::new(schedules),
        };
        // 首次部署或从 .bak 恢复时立即落盘一份合法主文件
        store.persist(&store.inner.read().await).await?;
        Ok(store)
    }

    pub async fn snapshot(&self) -> Vec<Schedule> {
        self.inner.read().await.clone()
    }

    pub async fn find(&self, id: &str) -> Option<Schedule> {
        self.inner.read().await.iter().find(|s| s.id == id).cloned()
    }

    pub async fn len(&self) -> usize {
        self.inner.read().await.len()
    }


    /// POST 临界区（§5.7 v2.39）：持写锁"快照 → 依赖校验 → upsert"
    /// 原子完成——两并发 POST 各自以"校验时对方未落库"的快照通过会
    /// 落库成环，锁边界是防环最后防线。校验规则：各项存在、同
    /// agent_id、无环（含自依赖）。内层 Err(String) 为校验失败（422）。
    /// merge 语义：已存在条目保留运行时字段，仅覆盖配置字段；
    /// pending_dependency_trigger 在 depends_on 未变时保留、变更时清零
    /// （§5.7 对齐本地 v2.39 P2：旧 pending 锚定已变更的依赖关系）。
    pub async fn upsert_with_dependency_check(
        &self,
        config: Schedule,
    ) -> Result<std::result::Result<Schedule, String>> {
        let mut guard = self.inner.write().await;
        if let Err(msg) = validate_depends_on_locked(&guard, &config) {
            return Ok(Err(msg));
        }
        // 复用 upsert 的 merge 语义（持锁手动展开，避免锁重入）
        let now = Utc::now().to_rfc3339();
        let merged = if let Some(existing) = guard.iter_mut().find(|s| s.id == config.id) {
            let mut m = config.clone();
            m.last_run_at = existing.last_run_at.take();
            m.next_run_at = existing.next_run_at.take();
            m.last_status = existing.last_status.take();
            m.last_error = existing.last_error.take();
            m.pending_dependency_trigger =
                existing.depends_on == config.depends_on && existing.pending_dependency_trigger;
            m.created_at = existing.created_at.take();
            m.updated_at = Some(now);
            *existing = m.clone();
            m
        } else {
            let mut m = config.clone();
            m.created_at = Some(now.clone());
            m.updated_at = Some(now);
            guard.push(m.clone());
            m
        };
        self.persist(&guard).await?;
        Ok(Ok(merged))
    }

    /// DELETE 临界区（§5.7 v2.39）：持写锁扫描被 depends_on 引用的条目，
    /// 非空拒绝（内层 Err 为引用方名称列表，422）；无引用才 remove。
    pub async fn remove_if_unreferenced(
        &self,
        id: &str,
    ) -> Result<std::result::Result<Option<Schedule>, Vec<String>>> {
        let mut guard = self.inner.write().await;
        let referrers: Vec<String> = guard
            .iter()
            .filter(|s| {
                s.depends_on
                    .as_ref()
                    .map(|d| d.iter().any(|dep| dep == id))
                    .unwrap_or(false)
            })
            .map(|s| s.name.clone())
            .collect();
        if !referrers.is_empty() {
            return Ok(Err(referrers));
        }
        let removed = guard.iter().position(|s| s.id == id).map(|idx| guard.remove(idx));
        self.persist(&guard).await?;
        Ok(Ok(removed))
    }

    /// 持写锁对单条目做任意修改并落盘（运行时字段 merge、状态变更等）。
    /// 条目不存在时返回 false（no-op，幂等）。
    pub async fn update<F>(&self, id: &str, mutate: F) -> Result<bool>
    where
        F: FnOnce(&mut Schedule),
    {
        let mut guard = self.inner.write().await;
        let Some(target) = guard.iter_mut().find(|s| s.id == id) else {
            return Ok(false);
        };
        mutate(target);
        target.updated_at = Some(Utc::now().to_rfc3339());
        self.persist(&guard).await?;
        Ok(true)
    }

    async fn persist(&self, schedules: &[Schedule]) -> Result<()> {
        let file = ScheduleFile {
            schedules: schedules.to_vec(),
        };
        persist_atomic(&self.path, &file).await
    }
}

async fn load_backup(path: &Path) -> Result<Vec<Schedule>> {
    let bak = backup_path(path);
    let bytes = tokio::fs::read(&bak)
        .await
        .map_err(|err| anyhow!("主配置与备份均不可读: {err}（bak 路径 {}）", bak.display()))?;
    let file = serde_json::from_slice::<ScheduleFile>(&bytes)
        .map_err(|err| anyhow!("备份配置解析失败: {err}"))?;
    Ok(file.schedules)
}

fn backup_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".bak");
    PathBuf::from(s)
}

/// tmp + rename 原子落盘；写盘前把现有主文件挪为 `.bak`。
async fn persist_atomic(path: &Path, file: &ScheduleFile) -> Result<()> {
    let tmp = {
        let mut s = path.as_os_str().to_os_string();
        s.push(".tmp");
        PathBuf::from(s)
    };
    let bytes = serde_json::to_vec_pretty(file).context("序列化 schedules.json 失败")?;
    tokio::fs::write(&tmp, &bytes)
        .await
        .with_context(|| format!("写入 {} 失败", tmp.display()))?;
    if tokio::fs::try_exists(path).await.unwrap_or(false) {
        let _ = tokio::fs::rename(path, backup_path(path)).await;
    }
    tokio::fs::rename(&tmp, path)
        .await
        .with_context(|| format!("rename {} -> {} 失败", tmp.display(), path.display()))?;
    Ok(())
}

/// 崩溃恢复（§5.2.1）：启动时将 `last_status='running'` 的条目标记为
/// `interrupted` / `进程崩溃恢复`。
pub async fn recover_running_flags(store: &ScheduleStore) -> Result<()> {
    let running_ids: Vec<String> = store
        .snapshot()
        .await
        .into_iter()
        .filter(|s| s.last_status.as_deref() == Some("running"))
        .map(|s| s.id)
        .collect();
    for id in running_ids {
        store
            .update(&id, |s| {
                s.last_status = Some("interrupted".to_string());
                s.last_error = Some("进程崩溃恢复".to_string());
            })
            .await?;
    }
    Ok(())
}
/// 持锁依赖校验（§5.7 POST 规则）：各项存在、同 agent_id、无环
/// （含自依赖）。被写条目以**新值**参与解析（更新时覆盖快照旧值）。
fn validate_depends_on_locked(snapshot: &[Schedule], config: &Schedule) -> std::result::Result<(), String> {
    let Some(deps) = config.depends_on.as_ref() else {
        return Ok(());
    };
    if deps.is_empty() {
        return Ok(());
    }
    if deps.iter().any(|d| d == &config.id) {
        return Err("任务不能依赖自身".to_string());
    }
    for dep_id in deps {
        let Some(dep) = snapshot.iter().find(|s| &s.id == dep_id) else {
            return Err(format!("依赖的任务不存在: {dep_id}"));
        };
        if dep.agent_id != config.agent_id {
            return Err(format!("依赖的任务须为同一智能体的任务: {dep_id}"));
        }
    }
    if detect_dependency_cycle(snapshot, config) {
        return Err("依赖配置成环（含自依赖），已拒绝保存".to_string());
    }
    Ok(())
}

/// DFS 环检测（§4.13 语义同本地 detect_dependency_cycle）：从被写条目
/// 出发沿 depends_on 走（其他条目取快照值，被写条目取新值），回到
/// 自身即成环。
fn detect_dependency_cycle(snapshot: &[Schedule], config: &Schedule) -> bool {
    fn deps_of<'a>(
        snapshot: &'a [Schedule],
        config: &'a Schedule,
        id: &'a str,
    ) -> Vec<&'a str> {
        if id == config.id {
            config
                .depends_on
                .as_deref()
                .unwrap_or(&[])
                .iter()
                .map(|s| s.as_str())
                .collect()
        } else {
            snapshot
                .iter()
                .find(|s| s.id == id)
                .and_then(|s| s.depends_on.as_deref())
                .unwrap_or(&[])
                .iter()
                .map(|s| s.as_str())
                .collect()
        }
    }

    let mut visited: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut stack: Vec<&str> = vec![config.id.as_str()];
    while let Some(cur) = stack.pop() {
        if !visited.insert(cur) {
            continue;
        }
        for dep in deps_of(snapshot, config, cur) {
            if dep == config.id {
                return true;
            }
            if !visited.contains(dep) {
                stack.push(dep);
            }
        }
    }
    false
}