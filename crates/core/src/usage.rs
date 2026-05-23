//! cc-switch SQLite 用量统计。
//!
//! 读取 `~/.cc-switch/cc-switch.db` 的 `proxy_request_logs` 表，
//! 按 provider / 时间窗口聚合：
//! - 5 小时滚动窗口（覆盖 Anthropic 的 5h rate limit 概念）
//! - 本周（自然周一 00:00 起，按本地时区）
//!
//! 数据库由 cc-switch 写入，本工具仅只读访问。**SQLite 在被其他进程写时
//! 我们的只读 SELECT 仍然安全**（cc-switch 用 WAL 模式）。
//!
//! 路径策略：用 `OpenFlags::SQLITE_OPEN_READ_ONLY` 打开，失败（文件不存在 /
//! 表不存在 / 表为空）都安静返回 None，UI 显示 `--` 占位即可。

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};

/// 单个时间窗口的用量汇总。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageWindow {
    /// **API 请求总数**（来自 `proxy_request_logs`）—— 含工具调用循环
    /// 每次往返。一次用户消息可能触发 5–10 次 API request。
    pub request_count: u64,
    /// 输入 token 累计
    pub input_tokens: u64,
    /// 输出 token 累计
    pub output_tokens: u64,
    /// 缓存读取 token（Claude prompt caching 优化）
    pub cache_read_tokens: u64,
    /// 累计费用 USD（仅明细窗口用，浮窗不展示）
    pub total_cost_usd: f64,
    /// 该窗口里出现频次最高的模型名（用于浮窗显示"当前主用模型"）
    pub top_model: String,
    /// 该窗口里**最早**一次请求的 unix timestamp（用于算 reset countdown）。
    pub oldest_unix: Option<u64>,
    /// **真实用户消息数**（来自 `~/.claude/projects/**/*.jsonl`）—— Claude
    /// Code session 日志里 `type:"user"` 且不含 `tool_use_id` 的行。
    /// 这是 Anthropic 限额计数的"消息"，浮窗百分比应该用这个不是 request_count。
    pub user_messages: u64,
    /// 上述用户消息中最早一条的 unix timestamp（更精确的 reset countdown）。
    pub user_messages_oldest_unix: Option<u64>,
}

/// 多窗口用量聚合 —— 浮窗第二行"AI 用量"模式直接读这个结构。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageStats {
    pub window_5h: UsageWindow,
    pub window_week: UsageWindow,
}

fn db_path() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    Some(home.join(".cc-switch").join("cc-switch.db"))
}

/// 读取指定 provider（cc-switch 里的 `app_type`，如 "claude" / "codex"）
/// 最近 5h + 本周的累计用量。
///
/// 数据库不存在 / 表为空 / 该 provider 还没用过 → 返回 None，调用方
/// 在 UI 上显示 `--` 占位。
pub fn read_usage_stats(app_type: &str) -> Option<UsageStats> {
    let path = db_path()?;
    if !path.exists() {
        return None;
    }
    let conn = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;

    let now = unix_now();
    let five_hours_ago = now.saturating_sub(5 * 3600);
    let week_start = current_week_start_unix();

    let mut window_5h = query_window(&conn, app_type, five_hours_ago).unwrap_or_default();
    let mut window_week = query_window(&conn, app_type, week_start).unwrap_or_default();

    // 用户消息数从 cc-switch 已索引的 .jsonl 文件路径取（session_log_sync 表）
    // —— 比扫整个 ~/.claude/projects/ 快，且和 cc-switch 自己的口径一致
    let (h5_msgs, h5_msgs_oldest) = count_user_messages(&conn, five_hours_ago);
    window_5h.user_messages = h5_msgs;
    window_5h.user_messages_oldest_unix = h5_msgs_oldest;
    let (wk_msgs, wk_msgs_oldest) = count_user_messages(&conn, week_start);
    window_week.user_messages = wk_msgs;
    window_week.user_messages_oldest_unix = wk_msgs_oldest;

    // 两个窗口都全 0 → 该用户根本没用过这个 provider，返回 None 比返回全 0
    // 更清楚（UI 才会显示 `--` 而不是 `0 req`）
    if window_5h.request_count == 0 && window_week.request_count == 0 {
        return None;
    }

    Some(UsageStats {
        window_5h,
        window_week,
    })
}

/// 扫 cc-switch 已索引的 .jsonl 文件，统计窗口内的**真用户消息数**。
///
/// "真用户消息" = `"type":"user"` 行且**不含** `tool_use_id`（后者是 Claude
/// 工具调用结果的回传，不是用户键入）。这是 Anthropic 计费/限流口径的
/// "messages"，cc-switch UI 的百分比也按这个算。
///
/// 用 cc-switch `session_log_sync` 表拿候选文件路径，只读 `last_modified`
/// 落在窗口内的（旧文件不会有新消息）。
fn count_user_messages(conn: &Connection, since_unix: u64) -> (u64, Option<u64>) {
    let files: Vec<String> = match conn.prepare(
        "SELECT file_path FROM session_log_sync WHERE last_modified >= ?1",
    ) {
        Ok(mut stmt) => stmt
            .query_map([since_unix as i64], |r| r.get::<_, String>(0))
            .map(|it| it.filter_map(|r| r.ok()).collect())
            .unwrap_or_default(),
        Err(_) => return (0, None),
    };

    let mut total = 0u64;
    let mut oldest: Option<u64> = None;
    for fp in files {
        let Ok(content) = std::fs::read_to_string(&fp) else { continue };
        for line in content.lines() {
            // 快速字符串过滤，避免对每行做完整 JSON 解析
            if !line.contains("\"type\":\"user\"") {
                continue;
            }
            if line.contains("tool_use_id") {
                continue;
            }
            let Some(ts) = extract_iso8601_unix(line) else { continue };
            if ts < since_unix {
                continue;
            }
            total += 1;
            oldest = Some(match oldest {
                Some(o) => o.min(ts),
                None => ts,
            });
        }
    }
    (total, oldest)
}

/// 从 Claude jsonl 行里提取 `"timestamp":"YYYY-MM-DDTHH:MM:SS.fffZ"` 并转 unix。
/// 不引 chrono —— 格式固定，手算更省依赖。
fn extract_iso8601_unix(line: &str) -> Option<u64> {
    const KEY: &str = "\"timestamp\":\"";
    let start = line.find(KEY)? + KEY.len();
    let rest = &line[start..];
    let end = rest.find('"')?;
    iso8601_z_to_unix(&rest[..end])
}

fn iso8601_z_to_unix(s: &str) -> Option<u64> {
    // "2026-05-23T06:01:52.138Z" 最小 19 字符到 Y-M-D-T-H-M-S
    if s.len() < 19 { return None; }
    let year: i64 = s.get(0..4)?.parse().ok()?;
    let month: i64 = s.get(5..7)?.parse().ok()?;
    let day: i64 = s.get(8..10)?.parse().ok()?;
    let hour: i64 = s.get(11..13)?.parse().ok()?;
    let minute: i64 = s.get(14..16)?.parse().ok()?;
    let second: i64 = s.get(17..19)?.parse().ok()?;
    let days = days_since_epoch(year, month, day)?;
    Some((days * 86400 + hour * 3600 + minute * 60 + second) as u64)
}

fn days_since_epoch(year: i64, month: i64, day: i64) -> Option<i64> {
    if year < 1970 || month < 1 || month > 12 || day < 1 { return None; }
    let mut days = 0i64;
    for y in 1970..year {
        days += if is_leap_year(y) { 366 } else { 365 };
    }
    const MD: [i64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    for m in 1..month {
        days += MD[(m - 1) as usize];
        if m == 2 && is_leap_year(year) { days += 1; }
    }
    days += day - 1;
    Some(days)
}

fn is_leap_year(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// 聚合查询：[since_unix, now] 内该 app_type 的所有 request 累计。
/// 顺便算出 top_model（按 request_count 排序的第一名）。
fn query_window(
    conn: &Connection,
    app_type: &str,
    since_unix: u64,
) -> rusqlite::Result<UsageWindow> {
    // 1. 累计指标 + 最早请求时间戳（用于算 reset countdown）
    let mut win: UsageWindow = conn
        .query_row(
            "SELECT
                COUNT(*) as req,
                COALESCE(SUM(input_tokens), 0) as in_tok,
                COALESCE(SUM(output_tokens), 0) as out_tok,
                COALESCE(SUM(cache_read_tokens), 0) as cache_tok,
                COALESCE(SUM(CAST(total_cost_usd AS REAL)), 0) as cost,
                MIN(created_at) as oldest
             FROM proxy_request_logs
             WHERE app_type = ?1 AND created_at >= ?2",
            (app_type, since_unix as i64),
            |row| {
                let oldest: Option<i64> = row.get(5)?;
                Ok(UsageWindow {
                    request_count: row.get::<_, i64>(0)? as u64,
                    input_tokens: row.get::<_, i64>(1)? as u64,
                    output_tokens: row.get::<_, i64>(2)? as u64,
                    cache_read_tokens: row.get::<_, i64>(3)? as u64,
                    total_cost_usd: row.get::<_, f64>(4)?,
                    top_model: String::new(),
                    oldest_unix: oldest.map(|v| v as u64),
                    // user_messages 后面会被 count_user_messages() 填充
                    user_messages: 0,
                    user_messages_oldest_unix: None,
                })
            },
        )
        .optional()?
        .unwrap_or_default();

    // 2. 主用模型
    if win.request_count > 0 {
        win.top_model = conn
            .query_row(
                "SELECT model FROM proxy_request_logs
                 WHERE app_type = ?1 AND created_at >= ?2
                 GROUP BY model
                 ORDER BY COUNT(*) DESC
                 LIMIT 1",
                (app_type, since_unix as i64),
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .unwrap_or_default();
    }

    Ok(win)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 本周第一秒（周一 00:00:00）—— 用 SystemTime + 7 天滑动近似。
/// 注意：这里采用"过去 7×24 小时滚动"而非"日历周一"，避免 chrono 依赖；
/// 对用户感知"本周用了多少"来说差异可忽略。
fn current_week_start_unix() -> u64 {
    unix_now().saturating_sub(7 * 24 * 3600)
}

// ── 浮窗渲染辅助：人类可读 ─────────────────────────────────────

/// 把 token 数字格式化短形（10500 → "10.5K"，2100000 → "2.1M"）。
pub fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// 格式化金额（$ 前缀，自动小数位数）。
pub fn format_cost(usd: f64) -> String {
    if usd >= 100.0 {
        format!("${:.0}", usd)
    } else if usd >= 10.0 {
        format!("${:.1}", usd)
    } else {
        format!("${:.2}", usd)
    }
}

/// 用量明细窗口的一行：(app_type, provider_id, model) 维度的累计。
#[derive(Debug, Clone, Default)]
pub struct UsageRow {
    pub app_type: String,
    pub provider_id: String,
    pub model: String,
    pub request_count: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_cost_usd: f64,
    pub avg_latency_ms: u32,
}

/// 列出最近 `since_secs` 秒内的用量明细，按 cost 降序。
/// 数据库不存在 / 无数据时返回空 vec。
pub fn list_usage_breakdown(since_secs: u64) -> Vec<UsageRow> {
    let Some(path) = db_path() else { return Vec::new() };
    if !path.exists() {
        return Vec::new();
    }
    let Ok(conn) = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
        return Vec::new();
    };

    let since = unix_now().saturating_sub(since_secs) as i64;
    let mut stmt = match conn.prepare(
        "SELECT app_type, provider_id, model,
                COUNT(*) as req,
                COALESCE(SUM(input_tokens), 0),
                COALESCE(SUM(output_tokens), 0),
                COALESCE(SUM(CAST(total_cost_usd AS REAL)), 0),
                COALESCE(AVG(latency_ms), 0)
         FROM proxy_request_logs
         WHERE created_at >= ?1
         GROUP BY app_type, provider_id, model
         ORDER BY 7 DESC",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    let iter = match stmt.query_map([since], |row| {
        Ok(UsageRow {
            app_type: row.get(0)?,
            provider_id: row.get(1)?,
            model: row.get(2)?,
            request_count: row.get::<_, i64>(3)? as u64,
            input_tokens: row.get::<_, i64>(4)? as u64,
            output_tokens: row.get::<_, i64>(5)? as u64,
            total_cost_usd: row.get::<_, f64>(6)?,
            avg_latency_ms: row.get::<_, f64>(7)? as u32,
        })
    }) {
        Ok(it) => it,
        Err(_) => return Vec::new(),
    };

    iter.filter_map(|r| r.ok()).collect()
}
