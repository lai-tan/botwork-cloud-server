// Copyright (c) 2026 Jehu Shaw. All rights reserved.

//! TaskSlot 注册表（AUTOMATION-DESIGN.md §5.2.1 v2.16 定稿）：
//! - 双重职责：(a) 同 schedule 重入检测（entry 原子占位）；
//!   (b) 外部取消（超时/DELETE/内存熔断/优雅停机）凭 schedule_id 取
//!   cancel_token + abort_handle 执行统一取消链。
//! - DashMap guard 不跨 await：所有读取均克隆快照后立即释放。

use std::sync::Arc;

use chrono::DateTime;
use chrono::Utc;
use dashmap::DashMap;
use tokio::task::AbortHandle;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CancelReason {
    Timeout,
    Deleted,
    MemoryCircuitBreaker,
    Shutdown,
}

impl CancelReason {
    /// finalize 依据 cancel_reason 标注最终状态（§5.2.1 收尾单点）。
    pub fn terminal_status(&self) -> (&'static str, &'static str) {
        match self {
            Self::Timeout => ("timeout", "执行超时"),
            Self::Deleted => ("interrupted", "任务被删除，取消执行"),
            Self::MemoryCircuitBreaker => ("interrupted", "内存硬熔断"),
            Self::Shutdown => ("interrupted", "优雅停机"),
        }
    }
}

pub struct TaskSlot {
    pub cancel_token: CancellationToken,
    /// None = 排队中（内层 task 尚未 spawn），无需 abort。
    /// AbortHandle 可克隆快照；JoinHandle 不可 Clone，回收由 wrapper 持有执行。
    pub abort_handle: Option<AbortHandle>,
    /// 取消原因在 token.cancel() 之前写入（状态先于信号）。
    pub cancel_reason: Option<CancelReason>,
    pub started_at: DateTime<Utc>,
    pub trigger_type: String,
}

/// 可克隆句柄（scopeguard 闭包与取消链调用方各持一份）。
#[derive(Clone)]
pub struct TaskRegistry {
    inner: Arc<DashMap<String, TaskSlot>>,
}

impl TaskRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(DashMap::new()),
        }
    }

    /// entry 原子占位：已占用返回 None（调用方走 missed_pending 路径），
    /// 空闲则插入 TaskSlot 并返回其 cancel_token。
    pub fn try_register(&self, schedule_id: &str, trigger_type: &str) -> Option<CancellationToken> {
        let entry = self.inner.entry(schedule_id.to_string());
        if let dashmap::mapref::entry::Entry::Vacant(v) = entry {
            let token = CancellationToken::new();
            v.insert(TaskSlot {
                cancel_token: token.clone(),
                abort_handle: None,
                cancel_reason: None,
                started_at: Utc::now(),
                trigger_type: trigger_type.to_string(),
            });
            Some(token)
        } else {
            None
        }
    }

    /// 内层 task spawn 后回填 abort 句柄（供外部取消链兜底强杀）。
    pub fn set_abort_handle(&self, schedule_id: &str, abort_handle: AbortHandle) {
        if let Some(mut slot) = self.inner.get_mut(schedule_id) {
            slot.abort_handle = Some(abort_handle);
        }
    }

    /// 取消链专用：先写 reason 再 token.cancel（状态先于信号，§5.2.1）。
    pub fn set_cancel_reason(&self, schedule_id: &str, reason: CancelReason) {
        if let Some(mut slot) = self.inner.get_mut(schedule_id) {
            slot.cancel_reason = Some(reason);
        }
    }

    /// 克隆快照（DashMap guard 立即释放，不跨 await）。
    pub fn slot_snapshot(
        &self,
        schedule_id: &str,
    ) -> Option<(CancellationToken, Option<AbortHandle>, Option<CancelReason>)> {
        self.inner.get(schedule_id).map(|slot| {
            (
                slot.cancel_token.clone(),
                slot.abort_handle.clone(),
                slot.cancel_reason.clone(),
            )
        })
    }

    pub fn cancel_reason(&self, schedule_id: &str) -> Option<CancelReason> {
        self.inner.get(schedule_id).and_then(|s| s.cancel_reason.clone())
    }

    /// 运维观测读出：条目的触发类型与开始时间（停机日志用）。
    pub fn trigger_info(&self, schedule_id: &str) -> Option<(String, DateTime<Utc>)> {
        self.inner
            .get(schedule_id)
            .map(|s| (s.trigger_type.clone(), s.started_at))
    }

    pub fn remove(&self, schedule_id: &str) {
        self.inner.remove(schedule_id);
    }

    pub fn ids(&self) -> Vec<String> {
        self.inner.iter().map(|e| e.key().clone()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// 取消链 grace 轮询用：条目移除 = 任务退出且 finalize 完成。
    pub fn contains(&self, schedule_id: &str) -> bool {
        self.inner.contains_key(schedule_id)
    }

    /// 硬熔断用：按 started_at 最老优先返回 (id, started_at) 列表。
    pub fn oldest_first(&self) -> Vec<(String, DateTime<Utc>)> {
        let mut entries: Vec<(String, DateTime<Utc>)> = self
            .inner
            .iter()
            .map(|e| (e.key().clone(), e.started_at))
            .collect();
        entries.sort_by_key(|(_, started)| *started);
        entries
    }
}

impl Default for TaskRegistry {
    fn default() -> Self {
        Self::new()
    }
}