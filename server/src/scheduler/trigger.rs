// Copyright (c) 2026 Jehu Shaw. All rights reserved.

//! 触发时间计算（AUTOMATION-DESIGN.md §4.1/§4.2/§5.2.1）：
//! - periodic（daily/weekly/biweekly/monthly/yearly）+ interval + once，按 UTC 计算；
//! - Jitter：周期任务最多延迟周期时长的 10%（上限 15 分钟）；
//!   单次任务整点/半点最多提前 90 秒；
//! - 生效日期区间检查；
//! - cron 模式依赖 `cron` crate，为避免向远程部署机引入新的 crates.io 下载，
//!   当前版本暂不启用（返回 None 并由调用方告警跳过），后续版本补齐。

use std::time::Duration;

use chrono::DateTime;
use chrono::Datelike;
use chrono::Days;
use chrono::NaiveDate;
use chrono::NaiveTime;
use chrono::Timelike;
use chrono::Utc;

use super::config::Schedule;

/// 解析 "MO,TU,WE,TH,FR,SA,SU" 或 "0,1,2"（0=周日）为周序号集合（0..=6）。
pub fn parse_weekdays(byday: &str) -> Vec<u32> {
    let names = ["SU", "MO", "TU", "WE", "TH", "FR", "SA"];
    let mut days = Vec::new();
    for part in byday.split(',') {
        let part = part.trim().to_ascii_uppercase();
        if part.is_empty() {
            continue;
        }
        if let Ok(n) = part.parse::<u32>() {
            if n <= 6 {
                days.push(n);
            }
        } else if let Some(idx) = names.iter().position(|n| *n == part) {
            days.push(idx as u32);
        }
    }
    days
}

/// 解析 "1,15" 为日期集合（1..=31）。
fn parse_days_list(value: &str) -> Vec<u32> {
    value
        .split(',')
        .filter_map(|p| p.trim().parse::<u32>().ok())
        .filter(|d| (1..=31).contains(d))
        .collect()
}

pub fn in_effective_range(schedule: &Schedule, now: DateTime<Utc>) -> bool {
    let (from, to) = schedule.parse_effective_bounds();
    if let Some(from) = from {
        if now < from {
            return false;
        }
    }
    if let Some(to) = to {
        // effective_to 为日期时含当日全天
        let inclusive_to = if to.date_naive() == now.date_naive()
            && to.time() == NaiveTime::from_hms_opt(0, 0, 0).unwrap()
        {
            to + Days::new(1)
        } else {
            to
        };
        if now >= inclusive_to {
            return false;
        }
    }
    true
}

fn at_time(date: NaiveDate, hour: u32, minute: u32) -> Option<DateTime<Utc>> {
    Some(date.and_hms_opt(hour, minute, 0)?.and_utc())
}

fn next_daily(after: DateTime<Utc>, hour: u32, minute: u32) -> Option<DateTime<Utc>> {
    let mut t = at_time(after.date_naive(), hour, minute)?;
    while t <= after {
        t = at_time(t.date_naive() + Days::new(1), hour, minute)?;
    }
    Some(t)
}

fn weekday_of(date: NaiveDate) -> u32 {
    // chrono: Mon=0..Sun=6 → 转为 0=周日（与 parse_weekdays 对齐）
    (date.weekday().num_days_from_sunday()) % 7
}

/// biweekly 锚定：以 effective_from 所在 ISO 周为基准周（缺省取 1970-01-05 周一）。
fn biweekly_parity(schedule: &Schedule) -> i64 {
    const EPOCH_MONDAY: NaiveDate = NaiveDate::from_ymd_opt(1970, 1, 5).unwrap();
    let anchor = schedule
        .parse_effective_bounds()
        .0
        .map(|d| d.date_naive())
        .unwrap_or(EPOCH_MONDAY);
    let days = (anchor - EPOCH_MONDAY).num_days();
    (days.div_euclid(7)).rem_euclid(2)
}

fn next_weekday_occurrence(
    after: DateTime<Utc>,
    hour: u32,
    minute: u32,
    weekdays: &[u32],
    biweekly_parity: Option<i64>,
) -> Option<DateTime<Utc>> {
    let mut t = at_time(after.date_naive(), hour, minute)?;
    for _ in 0..=28 {
        if t > after
            && weekdays.contains(&weekday_of(t.date_naive()))
            && biweekly_parity
                .map(|p| {
                    let days = (t.date_naive() - NaiveDate::from_ymd_opt(1970, 1, 5).unwrap())
                        .num_days();
                    days.div_euclid(7).rem_euclid(2) == p
                })
                .unwrap_or(true)
        {
            return Some(t);
        }
        t = at_time(t.date_naive() + Days::new(1), hour, minute)?;
    }
    None
}

fn next_monthly(
    after: DateTime<Utc>,
    hour: u32,
    minute: u32,
    monthdays: &[u32],
) -> Option<DateTime<Utc>> {
    let mut t = at_time(after.date_naive(), hour, minute)?;
    for _ in 0..=400 {
        if t > after && monthdays.contains(&t.day()) {
            return Some(t);
        }
        t = at_time(t.date_naive() + Days::new(1), hour, minute)?;
    }
    None
}

fn next_yearly(
    after: DateTime<Utc>,
    hour: u32,
    minute: u32,
    month: u32,
    day: u32,
) -> Option<DateTime<Utc>> {
    for year in after.year()..=after.year() + 100 {
        if let Some(date) = NaiveDate::from_ymd_opt(year, month, day) {
            let t = at_time(date, hour, minute)?;
            if t > after {
                return Some(t);
            }
        }
    }
    None
}

/// 计算下一次触发点（不含 Jitter）。无有效触发（配置非法 / once 已过 /
/// cron 未启用）返回 None。
pub fn next_occurrence(schedule: &Schedule, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let hour = schedule.byhour.unwrap_or(0).min(23);
    let minute = schedule.byminute.unwrap_or(0).min(59);
    match schedule.frequency_type.as_str() {
        "interval" => {
            let secs = schedule.interval_seconds?;
            if secs <= 0 {
                return None;
            }
            Some(after + chrono::Duration::seconds(secs))
        }
        "once" => {
            let run_at = DateTime::parse_from_rfc3339(schedule.run_at.as_deref()?)
                .ok()?
                .with_timezone(&Utc);
            if run_at > after {
                Some(run_at)
            } else {
                None
            }
        }
        "periodic" => match schedule.periodic_mode.as_deref()? {
            "daily" => next_daily(after, hour, minute),
            "weekly" => {
                let days = schedule
                    .byday
                    .as_deref()
                    .map(parse_weekdays)
                    .filter(|d| !d.is_empty())?;
                next_weekday_occurrence(after, hour, minute, &days, None)
            }
            "biweekly" => {
                let days = schedule
                    .byday
                    .as_deref()
                    .map(parse_weekdays)
                    .filter(|d| !d.is_empty())?;
                next_weekday_occurrence(after, hour, minute, &days, Some(biweekly_parity(schedule)))
            }
            "monthly" => {
                let days = schedule
                    .bymonthday
                    .as_deref()
                    .map(parse_days_list)
                    .filter(|d| !d.is_empty())?;
                next_monthly(after, hour, minute, &days)
            }
            "yearly" => {
                let month = schedule.bymonth?;
                let day = schedule
                    .bymonthday
                    .as_deref()
                    .and_then(|s| parse_days_list(s).first().copied())?;
                next_yearly(after, hour, minute, month, day)
            }
            "cron" => {
                // cron crate 未引入（见模块注释）：返回 None，由调度循环
                // warned_no_trigger 告警一次（此处不打日志——调度循环每 30s
                // 重试计算，逐次告警会刷屏）
                None
            }
            _ => None,
        },
        _ => None,
    }
}

/// 周期时长（用于 Jitter 上限与交接排队告警）。
pub fn period_seconds(schedule: &Schedule) -> Option<i64> {
    match schedule.frequency_type.as_str() {
        "interval" => schedule.interval_seconds,
        "periodic" => match schedule.periodic_mode.as_deref()? {
            "daily" => Some(86_400),
            "weekly" => Some(604_800),
            "biweekly" => Some(1_209_600),
            "monthly" => Some(2_592_000),
            "yearly" => Some(31_536_000),
            _ => None,
        },
        _ => None,
    }
}

const JITTER_MAX: Duration = Duration::from_secs(15 * 60);
const ONCE_EARLY_MAX: Duration = Duration::from_secs(90);

/// Jitter（§4.2）：周期/间隔任务随机延迟 0..min(周期 10%, 15min)；
/// once 任务在整点/半点时随机提前 0..90s。
/// 返回 (delta, negative)：触发点 = base + delta（negative=true 表示提前）。
pub fn jitter_delta(schedule: &Schedule, base: DateTime<Utc>) -> (Duration, bool) {
    let rand01 = || rand::random::<f64>();
    match schedule.frequency_type.as_str() {
        "once" => {
            let minute = base.minute();
            if minute == 0 || minute == 30 {
                let secs = (rand01() * ONCE_EARLY_MAX.as_secs_f64()) as u64;
                (Duration::from_secs(secs), true)
            } else {
                (Duration::ZERO, false)
            }
        }
        _ => {
            let cap = period_seconds(schedule)
                .map(|p| Duration::from_secs((p as f64 * 0.10) as u64))
                .unwrap_or(Duration::ZERO)
                .min(JITTER_MAX);
            let secs = (rand01() * cap.as_secs_f64()) as u64;
            (Duration::from_secs(secs), false)
        }
    }
}

/// 含 Jitter 的下一次触发点（写入 next_run_at 的值）。
pub fn next_run_at_with_jitter(schedule: &Schedule, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let base = next_occurrence(schedule, after)?;
    let (delta, negative) = jitter_delta(schedule, base);
    let delta = chrono::Duration::from_std(delta).unwrap_or_default();
    if negative {
        Some(base - delta)
    } else {
        Some(base + delta)
    }
}

/// per-schedule 超时（§5.2.1）：schedule.max_execution_time 覆盖；
/// 普通任务取全局默认，handoff 交接任务默认 24h。
pub fn effective_max_execution_time(
    schedule: &Schedule,
    global_default_secs: u64,
) -> Duration {
    if let Some(secs) = schedule.max_execution_time {
        return Duration::from_secs(secs.max(1) as u64);
    }
    if schedule.is_handoff() {
        Duration::from_secs(86_400)
    } else {
        Duration::from_secs(global_default_secs)
    }
}