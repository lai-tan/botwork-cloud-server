// Copyright (c) 2026 Jehu Shaw. All rights reserved.

//! run 记录存储（AUTOMATION-DESIGN.md §5.2 run JSON schema / §5.2.1）：
//! - 文件名 `{schedule_id}_{timestamp}_{run_id}.json`，timestamp 为
//!   ISO8601 基本格式 UTC 且**前置于 run_id**（同 schedule 下文件名字典序 =
//!   时间序，清理按文件名删最旧；run_id 为 UUID 仅作同秒写入的冲突区分）；
//! - tmp + rename 原子落盘，崩溃恢复扫描不会读到半截 JSON；
//! - 解析失败的文件重命名 `.corrupt` 后缀并告警，不阻塞启动。

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Result;
use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct TokenUsage {
    pub input: i64,
    pub output: i64,
    pub total: i64,
}

/// run 记录，字段对齐本地 `schedule_runs` 表（§3.2）。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RunRecord {
    pub id: String,
    pub schedule_id: String,
    /// scheduled | manual_test | catch_up | handoff
    pub trigger_type: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    /// success | failed | timeout | interrupted | missed_pending | ...
    pub status: String,
    pub output_summary: Option<String>,
    pub error: Option<String>,
    /// codex thread rollout 落盘路径（§5.2 v2.43：wrapper 从
    /// thread_start/thread_resume 响应的 thread.path 取得，启动时写入
    /// 占位记录——反向交接的下载源，经 GET /runs 同步到本地）
    pub rollout_path: Option<String>,
    pub token_usage: Option<TokenUsage>,
    pub evidence: Vec<Value>,
    pub artifacts: Vec<Value>,
}

impl Default for RunRecord {
    fn default() -> Self {
        Self {
            id: String::new(),
            schedule_id: String::new(),
            trigger_type: "scheduled".to_string(),
            started_at: String::new(),
            finished_at: None,
            status: "running".to_string(),
            output_summary: None,
            error: None,
            rollout_path: None,
            token_usage: None,
            evidence: Vec::new(),
            artifacts: Vec::new(),
        }
    }
}

impl RunRecord {

    fn file_name(&self) -> String {
        let ts = DateTime::parse_from_rfc3339(&self.started_at)
            .map(|d| d.with_timezone(&Utc).format("%Y%m%dT%H%M%SZ").to_string())
            .unwrap_or_else(|_| "unknown".to_string());
        format!("{}_{}_{}.json", self.schedule_id, ts, self.id)
    }
}

#[derive(Clone)]
pub struct RunsStore {
    base_dir: PathBuf,
}

impl RunsStore {
    pub async fn open(base_dir: PathBuf) -> Result<Self> {
        tokio::fs::create_dir_all(&base_dir).await?;
        Ok(Self { base_dir })
    }

    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    pub async fn write(&self, record: &RunRecord) -> Result<()> {
        let path = self.base_dir.join(record.file_name());
        let tmp = {
            let mut s = path.as_os_str().to_os_string();
            s.push(".tmp");
            PathBuf::from(s)
        };
        let bytes = serde_json::to_vec_pretty(record)?;
        tokio::fs::write(&tmp, &bytes).await?;
        tokio::fs::rename(&tmp, &path).await?;
        Ok(())
    }

    /// 崩溃恢复（§5.2.1）：扫描无 `finished_at` 的记录，标记 `interrupted`
    /// 并回写。解析失败的文件重命名 `.corrupt` 并告警。
    pub async fn scan_and_recover(&self) -> Result<Vec<RunRecord>> {
        let mut recovered = Vec::new();
        let mut read_dir = match tokio::fs::read_dir(&self.base_dir).await {
            Ok(rd) => rd,
            Err(_) => return Ok(recovered),
        };
        while let Some(entry) = read_dir.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let bytes = match tokio::fs::read(&path).await {
                Ok(b) => b,
                Err(err) => {
                    tracing::warn!("run 记录读取失败 {}: {err}", path.display());
                    continue;
                }
            };
            let mut record: RunRecord = match serde_json::from_slice(&bytes) {
                Ok(r) => r,
                Err(err) => {
                    tracing::warn!("run 记录解析失败 {}: {err}，重命名为 .corrupt", path.display());
                    let mut corrupt = path.clone().into_os_string();
                    corrupt.push(".corrupt");
                    let _ = tokio::fs::rename(&path, corrupt).await;
                    continue;
                }
            };
            if record.finished_at.is_none() {
                record.finished_at = Some(Utc::now().to_rfc3339());
                record.status = "interrupted".to_string();
                record.error = Some("进程崩溃恢复".to_string());
                let new_path = self.base_dir.join(record.file_name());
                if let Err(err) = self.write(&record).await {
                    tracing::warn!("崩溃恢复回写失败 {}: {err}", path.display());
                } else if new_path != path {
                    // 旧版命名格式（run_id 前置）经规范化后文件名改变，
                    // 清理旧文件；同名时不删（否则会误删刚回写的记录）
                    let _ = tokio::fs::remove_file(&path).await;
                }
                recovered.push(record);
            }
        }
        if !recovered.is_empty() {
            tracing::warn!("崩溃恢复：标记 {} 条无 finished_at 的 run 记录为 interrupted", recovered.len());
        }
        Ok(recovered)
    }

    /// 每个 schedule 保留最近 `per_schedule_limit` 条（按文件名字典序删最旧）。
    pub async fn cleanup(&self, per_schedule_limit: usize) -> Result<usize> {
        let mut by_schedule: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
        let mut read_dir = match tokio::fs::read_dir(&self.base_dir).await {
            Ok(rd) => rd,
            Err(_) => return Ok(0),
        };
        while let Some(entry) = read_dir.next_entry().await? {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !name.ends_with(".json") {
                continue;
            }
            let schedule_id = name.split('_').next().unwrap_or("").to_string();
            if schedule_id.is_empty() {
                continue;
            }
            by_schedule.entry(schedule_id).or_default().push(path);
        }
        let mut removed = 0;
        for (_, mut files) in by_schedule {
            files.sort();
            while files.len() > per_schedule_limit {
                let victim = files.remove(0);
                if tokio::fs::remove_file(&victim).await.is_ok() {
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }

    /// 列出 run 记录（§5.2.1 GET /runs，步骤 22 同步通道）：
    /// - `schedule_id` 为 None 时**每个 schedule 各返回最近 `limit` 条**
    ///   （同步语义：避免活跃 schedule 挤占全局窗口致其他 schedule 拿不到
    ///   最新记录）；
    /// - 按文件名字典序倒序（ts 前置 = started_at 时间倒序）取前 N 后逐文件
    ///   读取；与 write 的 tmp+rename 原子性 / cleanup 的删除并发安全（读失败
    ///   单条跳过并告警，不改名 .corrupt——与启动扫描 scan_and_recover 区分）。
    pub async fn list(&self, schedule_id: Option<&str>, limit: usize) -> Result<Vec<RunRecord>> {
        let mut by_schedule: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut read_dir = match tokio::fs::read_dir(&self.base_dir).await {
            Ok(rd) => rd,
            Err(_) => return Ok(Vec::new()),
        };
        while let Some(entry) = read_dir.next_entry().await? {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !name.ends_with(".json") {
                continue;
            }
            let sid = name.split('_').next().unwrap_or("").to_string();
            if sid.is_empty() {
                continue;
            }
            if let Some(want) = schedule_id {
                if sid != want {
                    continue;
                }
            }
            by_schedule.entry(sid).or_default().push(name.to_string());
        }
        let mut records = Vec::new();
        for (_, mut names) in by_schedule {
            names.sort();
            names.reverse();
            // 同 run_id 双文件去重（第十轮 P3-1）：旧命名格式（run_id 前置）
            // 与新命名并存的过渡窗口会解析出同 id 两条，保留字典序最前
            //（= 最新）的一条；limit 对去重后的条数生效。
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            let mut taken = 0usize;
            for name in names {
                if taken >= limit {
                    break;
                }
                let path = self.base_dir.join(&name);
                let bytes = match tokio::fs::read(&path).await {
                    Ok(b) => b,
                    Err(err) => {
                        tracing::warn!("run 记录读取失败 {}: {err}", path.display());
                        continue;
                    }
                };
                match serde_json::from_slice::<RunRecord>(&bytes) {
                    Ok(record) => {
                        if seen.insert(record.id.clone()) {
                            records.push(record);
                            taken += 1;
                        }
                    }
                    Err(err) => {
                        tracing::warn!("run 记录解析失败 {}: {err}，跳过", path.display());
                    }
                }
            }
        }
        Ok(records)
    }
}