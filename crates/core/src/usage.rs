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
    /// 该窗口里出现频次最高的模型名（用于浮窗显示"当前主用模型"）。
    /// 已经过 humanize_model_name 处理，可直接展示。
    pub top_model: String,
    /// 该窗口里**最早**一次请求的 unix timestamp（用于算 reset countdown）。
    pub oldest_unix: Option<u64>,
    /// **真实用户消息数**（来自 `~/.claude/projects/**/*.jsonl`）—— Claude
    /// Code session 日志里 `type:"user"` 且不含 `tool_use_id` 的行。
    /// 这是 Anthropic 限额计数的"消息"，浮窗百分比应该用这个不是 request_count。
    pub user_messages: u64,
    /// 5h 窗口：**当前 block 的起点**（block = 以本 block 第一条 user 消息为
    /// 锚的固定 5h，到时整体重置；这是 Anthropic 官方语义）。
    /// 7d 窗口：滚动窗口内的最早 user 消息时间（不那么严格）。
    /// reset_at = 此值 + window_secs。
    pub user_messages_oldest_unix: Option<u64>,
    /// **预计耗尽倒计时（秒）**。仅 5h 窗口计算，基于最近 1h 的消息速率推算。
    /// None 表示：速率为 0、配额未设置、或已耗尽。
    pub eta_seconds: Option<u64>,
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
///
/// 注意：5h 走"固定 block from first user msg in current block"算法
/// （Anthropic 真实语义），不是滚动窗口 —— 后者会导致 % 和倒计时双双失准。
/// 7d 走滚动窗口近似（reset 时间不重要）。
pub fn read_usage_stats(app_type: &str) -> Option<UsageStats> {
    read_usage_stats_with_limits(app_type, 50, 1000)
}

/// 同 [`read_usage_stats`]，但显式传入 5h / 7d 配额上限以便计算 ETA。
/// 浮窗实际用这个版本——传入 config 的 usage_5h_limit_requests 才能让 ETA 准确。
pub fn read_usage_stats_with_limits(
    app_type: &str,
    limit_5h: u64,
    limit_week: u64,
) -> Option<UsageStats> {
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
    // 5h block 算法需要回看 10h 才能确认当前 block 边界（最坏情况：上个 block
    // 刚结束 + 当前 block 已 5h - ε）
    let scan_since = now.saturating_sub(10 * 3600);
    let week_start = current_week_start_unix();

    let mut window_5h = query_window(&conn, app_type, scan_since).unwrap_or_default();
    let mut window_week = query_window(&conn, app_type, week_start).unwrap_or_default();

    // 5h: 拉过去 10h 内所有 user 消息 ts，做 block 切分
    let ts_10h = collect_user_msg_timestamps(&conn, scan_since);
    let (count_5h, block_start_5h, msgs_last_1h) = analyze_5h_block(&ts_10h, now);
    window_5h.user_messages = count_5h;
    window_5h.user_messages_oldest_unix = block_start_5h;
    // 5h block 算法保留的 SQL request 统计可能跨多个 block，要重算只统计当前 block 内的。
    // 但 request_count 在 UI 上不展示（浮窗只显示 user_messages 的百分比），
    // 明细窗口又是按"用户选择的时间范围"独立查询，所以这里保留 10h 的 sum 不影响功能。
    // 为避免误导，将 user_messages_oldest_unix 与 request_count 的语义分离即可。
    window_5h.eta_seconds = compute_eta(count_5h, limit_5h, msgs_last_1h);

    // 7d: 滚动近似就够
    let ts_week = collect_user_msg_timestamps(&conn, week_start);
    let (count_wk, oldest_wk) = window_count_and_oldest(&ts_week, week_start);
    window_week.user_messages = count_wk;
    window_week.user_messages_oldest_unix = oldest_wk;
    window_week.eta_seconds = None; // 7d 不算 ETA（消息率波动太大）
    let _ = limit_week; // reserved for future per-window ETA

    // 两个窗口都没用户消息且没 request → 该 provider 真的没用过
    if window_5h.user_messages == 0
        && window_week.user_messages == 0
        && window_5h.request_count == 0
        && window_week.request_count == 0
    {
        return None;
    }

    Some(UsageStats {
        window_5h,
        window_week,
    })
}

/// 把 user 消息时间戳列表按"固定 5h block from first msg"切块，返回
/// `(当前 block 内消息数, 当前 block 起点, 最近 1h 消息数)`。
///
/// **block 语义**：第一条消息触发本 block 起点 `t0`，本 block = `[t0, t0+5h)`；
/// 落在区间内的所有消息归到本 block；区间外的下一条消息开启下一个 block。
/// **当前 block** = 最后一个仍在进行中（`t0+5h > now`）的 block。
/// 若最后一个 block 已经超过 5h，说明配额已重置但用户还没发新消息 → 返回 (0, None, _)。
///
/// **最近 1h 消息数** 用于 ETA 速率估算（与 block 边界无关，单纯看 now-1h 内 count）。
fn analyze_5h_block(ts_sorted_asc: &[u64], now: u64) -> (u64, Option<u64>, u64) {
    if ts_sorted_asc.is_empty() {
        return (0, None, 0);
    }

    const BLOCK: u64 = 5 * 3600;

    // greedy 找最后一个 block 的起点
    let mut block_start = ts_sorted_asc[0];
    for &ts in &ts_sorted_asc[1..] {
        if ts >= block_start + BLOCK {
            block_start = ts;
        }
    }

    // 当前 block 是否仍在进行中？
    let block_end = block_start + BLOCK;
    let (count, start_opt) = if block_end <= now {
        // block 已结束，配额已重置但用户没发新消息
        (0, None)
    } else {
        let c = ts_sorted_asc.iter().filter(|&&t| t >= block_start).count() as u64;
        (c, Some(block_start))
    };

    // 最近 1h 消息数（ETA 速率算）
    let one_hour_ago = now.saturating_sub(3600);
    let msgs_1h = ts_sorted_asc.iter().filter(|&&t| t >= one_hour_ago).count() as u64;

    (count, start_opt, msgs_1h)
}

/// 滚动窗口的简单 (count, oldest) 计算，给 7d 用。
fn window_count_and_oldest(ts_sorted_asc: &[u64], since: u64) -> (u64, Option<u64>) {
    let mut count = 0u64;
    let mut oldest: Option<u64> = None;
    for &ts in ts_sorted_asc {
        if ts < since {
            continue;
        }
        count += 1;
        oldest = Some(match oldest {
            Some(o) => o.min(ts),
            None => ts,
        });
    }
    (count, oldest)
}

/// 基于"最近 1h 速率"推算还剩多少秒耗尽配额。
/// rate = msgs_last_1h / 3600 (msgs/s)；remaining = limit - current；eta = remaining/rate。
/// rate=0（最近 1h 没发消息）/ limit=0 / remaining<=0 → None。
fn compute_eta(current: u64, limit: u64, msgs_last_1h: u64) -> Option<u64> {
    if limit == 0 || msgs_last_1h == 0 || current >= limit {
        return None;
    }
    let remaining = limit - current;
    let rate = msgs_last_1h as f64 / 3600.0; // msgs per second
    let secs = remaining as f64 / rate;
    if secs.is_finite() && secs >= 0.0 {
        Some(secs as u64)
    } else {
        None
    }
}

/// 扫 cc-switch 已索引的 .jsonl 文件，返回窗口内所有**真用户消息**的 unix 时间戳（升序）。
///
/// "真用户消息" = `"type":"user"` 行且**不含** `tool_use_id`（后者是 Claude
/// 工具调用结果的回传，不是用户键入）。这是 Anthropic 计费/限流口径的
/// "messages"，cc-switch UI 的百分比也按这个算。
///
/// 返回 sorted Vec 而非 count，是为了让 5h block 切分 + 1h 速率统计能复用
/// 同一次扫盘的结果。
fn collect_user_msg_timestamps(conn: &Connection, since_unix: u64) -> Vec<u64> {
    let files: Vec<String> = match conn.prepare(
        "SELECT file_path FROM session_log_sync WHERE last_modified >= ?1",
    ) {
        Ok(mut stmt) => stmt
            .query_map([since_unix as i64], |r| r.get::<_, String>(0))
            .map(|it| it.filter_map(|r| r.ok()).collect())
            .unwrap_or_default(),
        Err(_) => return Vec::new(),
    };

    let mut out: Vec<u64> = Vec::new();
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
            out.push(ts);
        }
    }
    out.sort_unstable();
    out
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
                    // user_messages / eta_seconds 由 collect_user_msg_timestamps +
                    // analyze_5h_block / window_count_and_oldest 在 read_usage_stats 里填充
                    user_messages: 0,
                    user_messages_oldest_unix: None,
                    eta_seconds: None,
                })
            },
        )
        .optional()?
        .unwrap_or_default();

    // 2. 主用模型（humanize → "Sonnet 4.6" 这种友好名）
    if win.request_count > 0 {
        let raw: String = conn
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
        win.top_model = humanize_model_name(&raw);
    }

    Ok(win)
}

/// 把官方长 ID（如 `claude-sonnet-4-6-20251201` / `gpt-4o-2024-11-20` /
/// `gemini-2.0-flash-exp`）压缩成浮窗友好的短名（"Sonnet 4.6"）。
/// 不识别的 model 名原样返回——保底，至少不会丢信息。
///
/// 匹配是 case-insensitive 的子串识别：版本号优先抓更具体的（4.7 比 4 优先）。
pub fn humanize_model_name(raw: &str) -> String {
    if raw.is_empty() {
        return String::new();
    }
    let lc = raw.to_ascii_lowercase();

    // Claude 家族
    if lc.contains("claude") || lc.contains("sonnet") || lc.contains("opus") || lc.contains("haiku") {
        let family = if lc.contains("opus") {
            "Opus"
        } else if lc.contains("haiku") {
            "Haiku"
        } else if lc.contains("sonnet") {
            "Sonnet"
        } else {
            "Claude"
        };
        // 抓版本号：4-7 / 4.7 / 3-5 / 3.5 等。优先匹配带小数点的
        // （日期后缀也是 -数字，要区分开）
        let ver = pick_claude_version(&lc);
        return match ver {
            Some(v) => format!("{} {}", family, v),
            None => family.to_string(),
        };
    }

    // GPT 家族
    if lc.contains("gpt-") || lc.starts_with("gpt") || lc.contains("o1") || lc.contains("o3") {
        if lc.contains("4o-mini") { return "GPT-4o mini".to_string(); }
        if lc.contains("4o") { return "GPT-4o".to_string(); }
        if lc.contains("4.1") { return "GPT-4.1".to_string(); }
        if lc.contains("4-turbo") || lc.contains("4.0-turbo") { return "GPT-4 Turbo".to_string(); }
        if lc.contains("gpt-4") { return "GPT-4".to_string(); }
        if lc.contains("o3-mini") { return "o3 mini".to_string(); }
        if lc.starts_with("o3") { return "o3".to_string(); }
        if lc.starts_with("o1") { return "o1".to_string(); }
        if lc.contains("gpt-3.5") { return "GPT-3.5".to_string(); }
    }

    // Gemini 家族
    if lc.contains("gemini") {
        if lc.contains("2.0-flash") || lc.contains("2-0-flash") { return "Gemini 2.0 Flash".to_string(); }
        if lc.contains("1.5-pro") { return "Gemini 1.5 Pro".to_string(); }
        if lc.contains("1.5-flash") { return "Gemini 1.5 Flash".to_string(); }
        if lc.contains("2.5-pro") { return "Gemini 2.5 Pro".to_string(); }
        if lc.contains("2.5-flash") { return "Gemini 2.5 Flash".to_string(); }
        return "Gemini".to_string();
    }

    // DeepSeek / Qwen 等
    if lc.contains("deepseek") {
        if lc.contains("v3") { return "DeepSeek V3".to_string(); }
        if lc.contains("r1") { return "DeepSeek R1".to_string(); }
        return "DeepSeek".to_string();
    }
    if lc.contains("qwen") {
        return "Qwen".to_string();
    }

    // 不识别 → 截掉常见日期后缀 (-YYYYMMDD) 再返回
    strip_date_suffix(raw).to_string()
}

/// 从 claude-* 模型字符串里抓出"主版本 + 小版本"（如 "4.7" / "3.5"）。
/// Claude 命名习惯：`claude-sonnet-4-7-20251201` / `claude-3-5-sonnet-20241022`。
/// 优先匹配相邻两个个位数（被横线分隔）作为 "X.Y"。
fn pick_claude_version(lc: &str) -> Option<String> {
    let bytes = lc.as_bytes();
    let mut i = 0;
    while i + 2 < bytes.len() {
        // 找 "-d-d" 形式且后面不是日期（>=4 位数字算日期）
        if bytes[i] == b'-' && bytes[i + 1].is_ascii_digit() && bytes[i + 2] == b'-'
            && i + 3 < bytes.len() && bytes[i + 3].is_ascii_digit()
        {
            // 但若 i+3 之后还有 5+ 位数字，说明 i+3 是日期开头，跳过
            let mut tail_digits = 0;
            let mut k = i + 3;
            while k < bytes.len() && bytes[k].is_ascii_digit() {
                tail_digits += 1;
                k += 1;
            }
            if tail_digits == 1 {
                // 完美：-X-Y 后面不是长数字串
                return Some(format!(
                    "{}.{}",
                    (bytes[i + 1] - b'0'),
                    (bytes[i + 3] - b'0')
                ));
            }
        }
        i += 1;
    }
    None
}

fn strip_date_suffix(s: &str) -> &str {
    // 尾巴是 -YYYYMMDD 之类的 8 位数字
    let bytes = s.as_bytes();
    if bytes.len() > 9 && bytes[bytes.len() - 9] == b'-'
        && bytes[bytes.len() - 8..].iter().all(|b| b.is_ascii_digit())
    {
        return &s[..s.len() - 9];
    }
    s
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
        let raw_model: String = row.get(2)?;
        Ok(UsageRow {
            app_type: row.get(0)?,
            provider_id: row.get(1)?,
            model: humanize_model_name(&raw_model),
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
