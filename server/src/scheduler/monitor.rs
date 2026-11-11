// Copyright (c) 2026 Jehu Shaw. All rights reserved.

//! 资源监控（AUTOMATION-DESIGN.md §5.2.1 资源监控）：
//! - RSS > 80%：拒绝新任务（acquire 后放弃触发），空闲时 malloc_trim 归还堆；
//! - RSS > 95%（硬熔断）：按 started_at 最老优先取消，每取消一个等 5s 复测，
//!   降至 <90% 停止，防连环误杀；
//! - runs/ 所在分区 > 90%：停止接受新任务；
//! - 每小时执行一次 runs 清理（每 schedule 保留 1000 条）。

use std::sync::Arc;

use super::registry::CancelReason;
use super::DaemonState;

const MONITOR_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
const CIRCUIT_RECHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
const CLEANUP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3600);
const RUNS_PER_SCHEDULE_LIMIT: usize = 1000;

#[cfg(target_os = "linux")]
fn read_memory_kb() -> Option<(u64, u64)> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let rss_kb = status
        .lines()
        .find(|l| l.starts_with("VmRSS:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse::<u64>().ok())?;
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let total_kb = meminfo
        .lines()
        .find(|l| l.starts_with("MemTotal:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse::<u64>().ok())?;
    Some((rss_kb, total_kb))
}

#[cfg(not(target_os = "linux"))]
fn read_memory_kb() -> Option<(u64, u64)> {
    None
}

fn disk_usage_percent(path: &std::path::Path) -> Option<f64> {
    #[cfg(target_os = "linux")]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
            return None;
        }
        let total = stat.f_frsize as u64 * stat.f_blocks as u64;
        if total == 0 {
            return None;
        }
        let free = stat.f_frsize as u64 * stat.f_bavail as u64;
        Some((total - free) as f64 / total as f64 * 100.0)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        None
    }
}

#[cfg(target_os = "linux")]
fn malloc_trim() {
    unsafe {
        libc::malloc_trim(0);
    }
}

#[cfg(not(target_os = "linux"))]
fn malloc_trim() {}

pub async fn run_monitor(state: Arc<DaemonState>) {
    let mut cleanup_tick = tokio::time::interval(CLEANUP_INTERVAL);
    loop {
        tokio::select! {
            _ = state.shutdown.cancelled() => break,
            _ = tokio::time::sleep(MONITOR_INTERVAL) => {}
            _ = cleanup_tick.tick() => {
                match state.runs.cleanup(RUNS_PER_SCHEDULE_LIMIT).await {
                    Ok(removed) if removed > 0 => {
                        tracing::info!("runs 清理：删除 {removed} 条超限旧记录");
                    }
                    Err(err) => tracing::warn!("runs 清理失败: {err}"),
                    _ => {}
                }
                continue;
            }
        }

        if let Some((rss_kb, total_kb)) = read_memory_kb() {
            let pct = rss_kb as f64 / total_kb as f64 * 100.0;
            if pct > 95.0 {
                tracing::warn!("内存 RSS {pct:.1}% > 95%，触发硬熔断");
                circuit_break(&state).await;
                continue;
            } else if pct > 80.0 {
                state.memory_ok.store(false, std::sync::atomic::Ordering::Relaxed);
                tracing::warn!("内存 RSS {pct:.1}% > 80%，暂停接受新任务");
            } else {
                // 仅恢复内存标志；磁盘标志独立判定（单标志会被此分支周期性
                // 覆盖磁盘的熔断写，磁盘保护失效 <30s）
                state.memory_ok.store(true, std::sync::atomic::Ordering::Relaxed);
                malloc_trim();
            }
        }

        if let Some(usage) = disk_usage_percent(state.runs.base_dir()) {
            if usage > 90.0 {
                state.disk_ok.store(false, std::sync::atomic::Ordering::Relaxed);
                tracing::warn!("runs/ 分区使用率 {usage:.1}% > 90%，停止接受新任务");
            } else {
                state.disk_ok.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
    tracing::info!("资源监控循环退出");
}

/// 硬熔断：最老优先取消，每取消一个等 5 秒复测 RSS，降至 <90% 停止。
async fn circuit_break(state: &Arc<DaemonState>) {
    state
        .memory_ok
        .store(false, std::sync::atomic::Ordering::Relaxed);
    loop {
        if state.shutdown.is_cancelled() {
            return;
        }
        let Some((rss_kb, total_kb)) = read_memory_kb() else {
            return;
        };
        let pct = rss_kb as f64 / total_kb as f64 * 100.0;
        if pct < 90.0 {
            tracing::info!("内存降至 {pct:.1}%，硬熔断结束");
            return;
        }
        let Some((oldest_id, _)) = state.registry.oldest_first().first().cloned() else {
            tracing::warn!("内存 {pct:.1}% 仍超限但注册表为空，等待现有任务结束");
            tokio::time::sleep(CIRCUIT_RECHECK_INTERVAL).await;
            continue;
        };
        tracing::warn!(schedule.id = %oldest_id, "硬熔断：取消最老任务");
        super::executor::cancel_chain(state, &oldest_id, CancelReason::MemoryCircuitBreaker)
            .await;
        tokio::time::sleep(CIRCUIT_RECHECK_INTERVAL).await;
    }
}