use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;

use crate::network::geo_lookup::GeoInfo;
use vpn_monitor_core::network::leak_check::LeakReport;
use vpn_monitor_core::proxy_rpc::ProxyRpcSnapshot;
use vpn_monitor_core::usage::UsageStats;

use super::theme::Theme;

/// 流量曲线保留最近 N 个采样点（每个 = 一次 monitor 上报，间隔
/// = monitor_interval）。60 个点在 2s 间隔下 = 近 2 分钟，足够 sparkline。
pub const TRAFFIC_HISTORY_CAP: usize = 60;

/// 流量曲线一个采样点：上行 bps + 下行 bps。
#[derive(Debug, Clone, Copy, Default)]
pub struct TrafficSample {
    pub up_bps: u64,
    pub down_bps: u64,
}

// BG_COLOR 保留作为 WNDCLASS 注册时的默认背景画刷（窗口创建一次性用），
// 实际绘制走 state.theme。窗口创建后 theme 切换会触发 InvalidateRect 重画。
pub const BG_COLOR: COLORREF = COLORREF(0x00_2D_2D_2D);

const LWA_ALPHA_RAW: u32 = 0x02;
pub const ROW_HEIGHT: i32 = 28; // 整体行高比之前略加 (26 → 28)，文字呼吸感更好
/// 第三行（detailed 模式专属）—— 流量曲线 + 国家分布
pub const ROW3_HEIGHT: i32 = 64;
/// 简易模式总高 = 2 行
pub const WIN_HEIGHT_SIMPLE: i32 = ROW_HEIGHT * 2;
/// 完整模式总高 = 2 行 + 第三行
pub const WIN_HEIGHT_DETAILED: i32 = ROW_HEIGHT * 2 + ROW3_HEIGHT;
/// 兼容旧 API：默认按简易高度（拖动 / 创建窗口时若不知道 form 走这个）
pub const WIN_HEIGHT: i32 = WIN_HEIGHT_SIMPLE;

/// 按当前形态返回窗口高度
pub fn window_height_for(form: &str) -> i32 {
    if form == "detailed" {
        WIN_HEIGHT_DETAILED
    } else {
        WIN_HEIGHT_SIMPLE
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum CheckStatus {
    Success,
    NetworkError,
    ApiLimited,
    Checking,
}

#[derive(Debug, Clone)]
pub struct IpUpdate {
    pub ip: Option<String>,
    pub geo: Option<GeoInfo>,
    pub status: CheckStatus,
    pub latency_ms: Option<u64>,
    /// Short reason string shown when status == NetworkError, e.g. "超时" / "DNS 失败".
    pub error_reason: Option<String>,
    /// Short reason string shown when status == Success but geo lookup failed,
    /// e.g. "超时" / "私有段" / "限流".
    pub geo_error_reason: Option<String>,
    /// Non-fatal advisory shown next to a successful geo result, e.g. a
    /// cross-source country mismatch warning. Rendered as an orange ⚠ in row 1.
    pub geo_warning: Option<String>,
}

#[allow(dead_code)]
pub struct OverlayState {
    pub current_update: IpUpdate,
    pub visible: bool,
    pub show_isp: bool,
    pub opacity: f32,
    pub proxy_enabled: bool,
    pub cpu_usage: f32,
    pub mem_usage: f32,
    pub net_up: u64,
    pub net_down: u64,
    pub claude_model: String,
    /// 当前活动主题色板（从 RuntimeFlags.theme_mode 同步过来）。
    /// 切主题时主线程读 mode → 调 theme::resolve → 写回这里 → InvalidateRect。
    pub theme: Theme,

    /// 浮窗形态："simple" / "detailed"（与 RuntimeFlags.overlay_form 同步）。
    pub overlay_form: String,
    /// 第二行模式："system" / "usage"（与 RuntimeFlags.row2_mode 同步）。
    pub row2_mode: String,
    /// cc-switch SQLite 读到的最新用量。None = DB 不存在 / 无记录。
    pub usage: Option<UsageStats>,
    /// 流量历史滚动窗口（detailed 模式右侧曲线用）。每次 monitor tick 时 push。
    pub traffic_history: VecDeque<TrafficSample>,

    /// DNS / IPv6 泄漏检测结果（后台任务每数分钟刷新）。None = 尚未检测过。
    pub leak: Option<LeakReport>,
    /// Clash / sing-box 当前节点（后台 RPC 探测）。None = 未检测到代理工具。
    pub proxy_rpc: Option<ProxyRpcSnapshot>,
    /// detailed 模式右侧的国别流量分布（按缓存匹配的 TCP 远端 IP 国家计数）
    pub traffic_by_country: Vec<(String, u32)>,

    /// 5h 滚动窗口请求次数上限，浮窗 row2 usage 百分比基准（与 cc-switch UI 一致）。
    /// 0 = 不显示百分比，只显示绝对请求数。
    pub usage_5h_limit_requests: u64,
    /// 7d 滚动窗口请求次数上限
    pub usage_week_limit_requests: u64,
}

impl Default for OverlayState {
    fn default() -> Self {
        Self {
            current_update: IpUpdate {
                ip: None,
                geo: None,
                status: CheckStatus::Checking,
                latency_ms: None,
                error_reason: None,
                geo_error_reason: None,
                geo_warning: None,
            },
            visible: true,
            show_isp: true,
            opacity: 0.92,
            proxy_enabled: false,
            cpu_usage: 0.0,
            mem_usage: 0.0,
            net_up: 0,
            net_down: 0,
            claude_model: String::new(),
            theme: super::theme::DARK,
            overlay_form: "simple".to_string(),
            row2_mode: "system".to_string(),
            usage: None,
            traffic_history: VecDeque::with_capacity(TRAFFIC_HISTORY_CAP),
            leak: None,
            proxy_rpc: None,
            traffic_by_country: Vec::new(),
            usage_5h_limit_requests: 50,
            usage_week_limit_requests: 1000,
        }
    }
}

pub type SharedState = Arc<Mutex<OverlayState>>;

extern "system" {
    fn SetLayeredWindowAttributes(hwnd: HWND, crkey: COLORREF, balpha: u8, dwflags: u32) -> BOOL;
}

fn create_font() -> HFONT {
    unsafe {
        CreateFontW(
            -12, 0, 0, 0,
            FW_NORMAL.0 as i32,
            0, 0, 0,
            DEFAULT_CHARSET,
            FONT_OUTPUT_PRECISION(0),
            FONT_CLIP_PRECISION(0),
            FONT_QUALITY(6), // CLEARTYPE_QUALITY
            DEFAULT_PITCH.0 as u32,
            windows::core::w!("Segoe UI"),
        )
    }
}

fn txt_width(hdc: HDC, text: &str) -> i32 {
    let mut wbuf: Vec<u16> = text.encode_utf16().collect();
    let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    unsafe {
        let _ = DrawTextW(hdc, &mut wbuf, &mut rect, DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX);
    }
    rect.right - rect.left
}

pub fn measure_required_width(hdc: HDC, state: &OverlayState) -> i32 {
    let font = create_font();
    let old = unsafe { SelectObject(hdc, font.into()) };

    let w1 = measure_row1(hdc, state);
    let w2 = measure_row2(hdc, state);

    unsafe {
        let _ = SelectObject(hdc, old);
        let _ = DeleteObject(font.into());
    }

    // detailed 模式的第三行（sparkline + 国家分布）跨整宽，所以不再额外加宽。
    // 给 sparkline 一个最小宽度 320 让曲线能看清。
    let base = w1.max(w2).max(280).min(720);
    if state.overlay_form == "detailed" {
        base.max(320)
    } else {
        base
    }
}

fn measure_row1(hdc: HDC, state: &OverlayState) -> i32 {
    let update = &state.current_update;
    let mut x: i32 = 30;

    // Claude model tag
    if !state.claude_model.is_empty() {
        x += txt_width(hdc, &state.claude_model) + 6;
        x += 12; // sep
    }

    match &update.status {
        CheckStatus::Checking => {
            x += txt_width(hdc, "检测中...") + 6;
        }
        CheckStatus::NetworkError => {
            // Show last-known IP/geo (dim) before the error so the user still
            // has context about which network they were on.
            if let Some(ip) = update.ip.as_deref() {
                x += txt_width(hdc, ip) + 6;
                x += 12; // sep
                if let Some(geo) = &update.geo {
                    x += txt_width(hdc, &format_location(geo)) + 6;
                    x += 12; // sep
                }
            }
            let err = format_network_error(update.error_reason.as_deref());
            x += txt_width(hdc, &err) + 6;
        }
        CheckStatus::ApiLimited => {
            if let Some(ip) = update.ip.as_deref() {
                x += txt_width(hdc, ip) + 6;
                x += 12; // sep
            }
            x += txt_width(hdc, "查询受限") + 6;
        }
        CheckStatus::Success => {
            x += txt_width(hdc, update.ip.as_deref().unwrap_or("--")) + 6;
            x += 12; // sep
            if let Some(geo) = &update.geo {
                x += txt_width(hdc, &format_location(geo)) + 6;
            } else {
                let placeholder = format_geo_missing(update.geo_error_reason.as_deref());
                x += txt_width(hdc, &placeholder) + 6;
            }
        }
    }

    // Latency display
    if let Some(ms) = update.latency_ms {
        x += 12; // sep
        x += txt_width(hdc, &format!("{}ms", ms)) + 6;
    }

    // Cross-source warning ⚠
    if let Some(warn) = &update.geo_warning {
        x += 12; // sep
        x += txt_width(hdc, "\u{26A0}") + 6;
        x += txt_width(hdc, warn) + 6;
    }

    // 代理 RPC 节点名 优先于"未设置代理"
    if let Some(rpc) = &state.proxy_rpc {
        if rpc.is_available() {
            if let Some(node) = &rpc.current_node {
                x += 12;
                x += txt_width(hdc, &format!("→ {}", node)) + 6;
            }
        }
    } else if !state.proxy_enabled {
        x += 12;
        x += txt_width(hdc, "未设置代理") + 6;
    }

    // 泄漏徽章
    if let Some(leak) = &state.leak {
        if leak.v6_leak {
            x += 8;
            x += txt_width(hdc, "[v6泄漏]") + 6;
        }
        if leak.dns_leak {
            x += 8;
            x += txt_width(hdc, "[DNS泄漏]") + 6;
        }
    }

    x + 4 // right padding
}

fn measure_row2(hdc: HDC, state: &OverlayState) -> i32 {
    if state.row2_mode == "usage" {
        measure_row2_usage(hdc, state)
    } else {
        measure_row2_system(hdc, state)
    }
}

fn measure_row2_system(hdc: HDC, state: &OverlayState) -> i32 {
    let mut x: i32 = 18;
    x += txt_width(hdc, "\u{2191}") + 6;
    x += txt_width(hdc, &format_speed(state.net_up)) + 6;
    x += 6;
    x += txt_width(hdc, "\u{2193}") + 6;
    x += txt_width(hdc, &format_speed(state.net_down)) + 6;
    x += 2 + 12;
    x += txt_width(hdc, &format!("CPU {:.0}%", state.cpu_usage)) + 6;
    x += 2 + 12;
    x += txt_width(hdc, &format!("内存 {:.0}%", state.mem_usage)) + 6;
    x + 4
}

/// Row 2 "usage" 模式：模型 · 5小时:{pct}% {countdown} · 7天:{pct}% {countdown}。
/// 百分比基准 = 配置的请求次数上限（cc-switch UI 也是此口径）；
/// countdown = 窗口内最早请求 + 窗口长度 - 当前时间。
fn measure_row2_usage(hdc: HDC, state: &OverlayState) -> i32 {
    let mut x: i32 = 18;
    let model = pick_top_model(state);
    x += txt_width(hdc, model) + 6;
    if let Some(u) = &state.usage {
        // 用 user_messages 不是 request_count（后者含工具调用循环，会
        // 把一次用户消息算 5-10 次）。oldest_unix 同理用 user_messages 的。
        let h5 = format_usage_segment(
            "5小时",
            u.window_5h.user_messages,
            u.window_5h.user_messages_oldest_unix,
            5 * 3600,
            state.usage_5h_limit_requests,
        );
        let wk = format_usage_segment(
            "7天",
            u.window_week.user_messages,
            u.window_week.user_messages_oldest_unix,
            7 * 24 * 3600,
            state.usage_week_limit_requests,
        );
        x += 12;
        x += txt_width(hdc, &h5) + 6;
        x += 12;
        x += txt_width(hdc, &wk) + 6;
    }
    x + 4
}

fn pick_top_model(state: &OverlayState) -> &str {
    match &state.usage {
        Some(u) if !u.window_5h.top_model.is_empty() => u.window_5h.top_model.as_str(),
        Some(u) if !u.window_week.top_model.is_empty() => u.window_week.top_model.as_str(),
        Some(_) => "(无模型记录)",
        None => "-- 暂无用量数据 (cc-switch DB 为空)",
    }
}

/// 把 (req_count, oldest_unix) 渲染成 "{label}:{pct}% {countdown}" 形式。
/// limit = 0 时退化为 "{label} {req}req {countdown}"。
fn format_usage_segment(
    label: &str,
    req: u64,
    oldest_unix: Option<u64>,
    window_secs: u64,
    limit_requests: u64,
) -> String {
    let countdown = format_reset_countdown(oldest_unix, window_secs);
    if limit_requests > 0 {
        let pct = ((req as f64 / limit_requests as f64) * 100.0).round() as i32;
        if countdown.is_empty() {
            format!("{}:{}%", label, pct)
        } else {
            format!("{}:{}% {}", label, pct, countdown)
        }
    } else if countdown.is_empty() {
        format!("{} {}req", label, req)
    } else {
        format!("{} {}req {}", label, req, countdown)
    }
}

/// 计算 reset 倒计时：窗口内最早请求 + 窗口长度 - now。
/// 没有请求 / 已超窗口 → 空串（不显示）。
/// 格式化规则：
///   < 60 分钟 → "Nm"（41m）
///   < 24 小时 → "Nh" 或 "NhMm"（5h、5h30m）
///   >= 24 小时 → "Nd" 或 "NdMh"（1d、1d1h）
fn format_reset_countdown(oldest_unix: Option<u64>, window_secs: u64) -> String {
    let oldest = match oldest_unix {
        Some(t) => t,
        None => return String::new(),
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let reset_at = oldest.saturating_add(window_secs);
    if reset_at <= now {
        return String::new();
    }
    let remain = reset_at - now;
    if remain < 60 * 60 {
        let m = (remain / 60).max(1);
        format!("{}m", m)
    } else if remain < 24 * 3600 {
        let h = remain / 3600;
        let m = (remain % 3600) / 60;
        if m == 0 { format!("{}h", h) } else { format!("{}h{}m", h, m) }
    } else {
        let d = remain / (24 * 3600);
        let h = (remain % (24 * 3600)) / 3600;
        if h == 0 { format!("{}d", d) } else { format!("{}d{}h", d, h) }
    }
}

pub fn paint_overlay(hwnd: HWND, state: &OverlayState, width: i32, height: i32) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let th = &state.theme;

        // 整窗背景：每次都按当前 theme 重画，无需 WNDCLASS 默认画刷
        let bg_brush = CreateSolidBrush(th.bg);
        let full_rect = RECT { left: 0, top: 0, right: width, bottom: height };
        let _ = FillRect(hdc, &full_rect, bg_brush);
        let _ = DeleteObject(bg_brush.into());

        let _ = SetBkMode(hdc, BACKGROUND_MODE(1));

        let font = create_font();
        let old_font = SelectObject(hdc, font.into());

        let row1_w = measure_row1(hdc, state);
        let row2_w = measure_row2(hdc, state);
        let row1_offset = ((width - row1_w) / 2).max(0);
        let row2_offset = ((width - row2_w) / 2).max(0);

        let update = &state.current_update;
        let max_x = width - 10;

        // === Row 1: 状态点 + IP + 归属地 + 代理 ===
        let dot_color = match update.status {
            CheckStatus::Success => th.accent_green,
            CheckStatus::NetworkError => th.accent_red,
            CheckStatus::Checking => th.accent_blue,
            CheckStatus::ApiLimited => th.accent_orange,
        };
        draw_dot(hdc, 18 + row1_offset, ROW_HEIGHT / 2, 4, dot_color);

        let mut x: i32 = 30 + row1_offset;

        if !state.claude_model.is_empty() {
            x = draw_text(hdc, &state.claude_model, x, 0, ROW_HEIGHT, th.fg_dim, max_x);
            x = draw_sep(hdc, x, 0, ROW_HEIGHT, th.separator);
        }

        match &update.status {
            CheckStatus::Checking => {
                x = draw_text(hdc, "检测中...", x, 0, ROW_HEIGHT, th.accent_blue, max_x);
            }
            CheckStatus::NetworkError => {
                if let Some(ip) = update.ip.as_deref() {
                    x = draw_text(hdc, ip, x, 0, ROW_HEIGHT, th.fg_dim, max_x);
                    x = draw_sep(hdc, x, 0, ROW_HEIGHT, th.separator);
                    if let Some(geo) = &update.geo {
                        x = draw_text(
                            hdc, &format_location(geo), x, 0, ROW_HEIGHT, th.fg_dim, max_x,
                        );
                        x = draw_sep(hdc, x, 0, ROW_HEIGHT, th.separator);
                    }
                }
                let err = format_network_error(update.error_reason.as_deref());
                x = draw_text(hdc, &err, x, 0, ROW_HEIGHT, th.accent_red, max_x);
            }
            CheckStatus::ApiLimited => {
                if let Some(ip) = update.ip.as_deref() {
                    x = draw_text(hdc, ip, x, 0, ROW_HEIGHT, th.fg_primary, max_x);
                    x = draw_sep(hdc, x, 0, ROW_HEIGHT, th.separator);
                }
                x = draw_text(hdc, "查询受限", x, 0, ROW_HEIGHT, th.accent_orange, max_x);
            }
            CheckStatus::Success => {
                let ip_str = update.ip.as_deref().unwrap_or("--");
                x = draw_text(hdc, ip_str, x, 0, ROW_HEIGHT, th.fg_primary, max_x);
                x = draw_sep(hdc, x, 0, ROW_HEIGHT, th.separator);

                if let Some(geo) = &update.geo {
                    let loc = format_location(geo);
                    x = draw_text(hdc, &loc, x, 0, ROW_HEIGHT, th.fg_secondary, max_x);
                } else {
                    let placeholder =
                        format_geo_missing(update.geo_error_reason.as_deref());
                    x = draw_text(hdc, &placeholder, x, 0, ROW_HEIGHT, th.fg_dim, max_x);
                }

                if let Some(ms) = update.latency_ms {
                    x = draw_sep(hdc, x, 0, ROW_HEIGHT, th.separator);
                    let color = if ms < 200 { th.fg_latency } else { th.accent_orange };
                    x = draw_text(hdc, &format!("{}ms", ms), x, 0, ROW_HEIGHT, color, max_x);
                }

                if let Some(warn) = &update.geo_warning {
                    x = draw_sep(hdc, x, 0, ROW_HEIGHT, th.separator);
                    x = draw_text(hdc, "\u{26A0}", x, 0, ROW_HEIGHT, th.accent_orange, max_x);
                    x = draw_text(hdc, warn, x, 0, ROW_HEIGHT, th.accent_orange, max_x);
                }
            }
        }

        if !matches!(update.status, CheckStatus::Success) {
            if let Some(ms) = update.latency_ms {
                x = draw_sep(hdc, x, 0, ROW_HEIGHT, th.separator);
                x = draw_text(hdc, &format!("{}ms", ms), x, 0, ROW_HEIGHT, th.fg_latency, max_x);
            }
        }

        // 代理节点名（优先）或"未设置代理"
        let mut row1_tail_x = x;
        if let Some(rpc) = &state.proxy_rpc {
            if rpc.is_available() {
                if let Some(node) = &rpc.current_node {
                    row1_tail_x = draw_text(
                        hdc,
                        &format!("→ {}", node),
                        x + 12, 0, ROW_HEIGHT, th.accent_green, max_x,
                    );
                }
            }
        } else if !state.proxy_enabled {
            row1_tail_x = draw_text(hdc, "未设置代理", x + 12, 0, ROW_HEIGHT, th.fg_dim, max_x);
        }

        // 泄漏徽章（红色突出）
        if let Some(leak) = &state.leak {
            if leak.v6_leak {
                row1_tail_x = draw_text(
                    hdc, "[v6泄漏]", row1_tail_x + 8, 0, ROW_HEIGHT, th.accent_red, max_x,
                );
            }
            if leak.dns_leak {
                row1_tail_x = draw_text(
                    hdc, "[DNS泄漏]", row1_tail_x + 8, 0, ROW_HEIGHT, th.accent_red, max_x,
                );
            }
        }
        let _ = row1_tail_x; // silence unused warning

        // 行分隔线（row 1 / row 2 之间，跨整宽）
        {
            let sep_brush = CreateSolidBrush(th.separator);
            let sep_rect = RECT { left: 10, top: ROW_HEIGHT - 1, right: width - 10, bottom: ROW_HEIGHT };
            let _ = FillRect(hdc, &sep_rect, sep_brush);
            let _ = DeleteObject(sep_brush.into());
        }

        // === Row 2 按模式分流 ===
        let y2 = ROW_HEIGHT;
        let x2_start = 18 + row2_offset;
        if state.row2_mode == "usage" {
            paint_row2_usage(hdc, state, x2_start, y2, max_x);
        } else {
            paint_row2_system(hdc, state, x2_start, y2, max_x);
        }

        // === detailed 模式：第三行 = 国家分布堆叠条 + 全宽流量曲线 ===
        if state.overlay_form == "detailed" {
            // row 2 / row 3 之间也画一条分隔线
            let sep_brush = CreateSolidBrush(th.separator);
            let sep_rect = RECT { left: 10, top: ROW_HEIGHT * 2 - 1, right: width - 10, bottom: ROW_HEIGHT * 2 };
            let _ = FillRect(hdc, &sep_rect, sep_brush);
            let _ = DeleteObject(sep_brush.into());

            let row3_rect = RECT {
                left: 10,
                top: ROW_HEIGHT * 2 + 4,
                right: width - 10,
                bottom: WIN_HEIGHT_DETAILED - 4,
            };
            draw_row3_sparkline_geo(hdc, state, row3_rect, th);
        }

        let _ = SelectObject(hdc, old_font);
        let _ = DeleteObject(font.into());
        let _ = EndPaint(hwnd, &ps);
    }
}

/// Row 2 "system" 模式：上行 ↑ 下行 ↓ CPU 内存
unsafe fn paint_row2_system(hdc: HDC, state: &OverlayState, x_start: i32, y: i32, max_x: i32) {
    let th = &state.theme;
    let mut x = draw_text(hdc, "\u{2191}", x_start, y, ROW_HEIGHT, th.fg_secondary, max_x);
    x = draw_text(hdc, &format_speed(state.net_up), x, y, ROW_HEIGHT, th.fg_secondary, max_x);
    x = draw_text(hdc, "\u{2193}", x + 6, y, ROW_HEIGHT, th.fg_secondary, max_x);
    x = draw_text(hdc, &format_speed(state.net_down), x, y, ROW_HEIGHT, th.fg_secondary, max_x);
    x = draw_sep(hdc, x + 2, y, ROW_HEIGHT, th.separator);
    x = draw_text(hdc, &format!("CPU {:.0}%", state.cpu_usage), x, y, ROW_HEIGHT, th.fg_secondary, max_x);
    x = draw_sep(hdc, x + 2, y, ROW_HEIGHT, th.separator);
    draw_text(hdc, &format!("内存 {:.0}%", state.mem_usage), x, y, ROW_HEIGHT, th.fg_secondary, max_x);
}

/// Row 2 "usage" 模式：模型 · 5小时:{pct}% {countdown} · 7天:{pct}% {countdown}。
/// 百分比用请求次数 / 配额，与 cc-switch UI 口径一致（之前用 USD 累计会
/// 因 total_cost_usd 是 API 列表价而严重偏高）。
/// 倒计时 = 窗口内最早请求时间 + 窗口长度 - now，反映"窗口何时滚动释放配额"。
unsafe fn paint_row2_usage(hdc: HDC, state: &OverlayState, x_start: i32, y: i32, max_x: i32) {
    let th = &state.theme;
    match &state.usage {
        Some(u) => {
            let model = pick_top_model(state).to_string();
            let mut x = draw_text(hdc, &model, x_start, y, ROW_HEIGHT, th.fg_primary, max_x);
            x = draw_sep(hdc, x, y, ROW_HEIGHT, th.separator);

            // 5 小时窗口 —— 用 user_messages 不是 request_count
            let h5_pct_f = if state.usage_5h_limit_requests > 0 {
                Some(u.window_5h.user_messages as f64 / state.usage_5h_limit_requests as f64 * 100.0)
            } else { None };
            let h5_text = format_usage_segment(
                "5小时",
                u.window_5h.user_messages,
                u.window_5h.user_messages_oldest_unix,
                5 * 3600,
                state.usage_5h_limit_requests,
            );
            x = draw_text(hdc, &h5_text, x, y, ROW_HEIGHT, pct_color(h5_pct_f, th), max_x);
            x = draw_sep(hdc, x, y, ROW_HEIGHT, th.separator);

            // 7 天窗口
            let wk_pct_f = if state.usage_week_limit_requests > 0 {
                Some(u.window_week.user_messages as f64 / state.usage_week_limit_requests as f64 * 100.0)
            } else { None };
            let wk_text = format_usage_segment(
                "7天",
                u.window_week.user_messages,
                u.window_week.user_messages_oldest_unix,
                7 * 24 * 3600,
                state.usage_week_limit_requests,
            );
            draw_text(hdc, &wk_text, x, y, ROW_HEIGHT, pct_color(wk_pct_f, th), max_x);
        }
        None => {
            draw_text(
                hdc,
                "-- 暂无用量数据 (cc-switch DB 为空)",
                x_start, y, ROW_HEIGHT, th.fg_dim, max_x,
            );
        }
    }
}

/// 用量百分比 → 颜色映射：<60% 次要色（绿）/ 60-85% 橙 / >85% 红。
/// None（未配置上限）退化为次要色。
fn pct_color(pct: Option<f64>, th: &Theme) -> COLORREF {
    match pct {
        None => th.fg_secondary,
        Some(p) if p < 60.0 => th.fg_secondary,
        Some(p) if p < 85.0 => th.accent_orange,
        Some(_) => th.accent_red,
    }
}

/// detailed 模式的第三行：顶部国家分布堆叠条 + 主体全宽双折线流量曲线。
/// 整体使用 surface 色卡片让它和上方两行有视觉区分。
unsafe fn draw_row3_sparkline_geo(hdc: HDC, state: &OverlayState, rect: RECT, th: &Theme) {
    // 卡片背景
    let card_brush = CreateSolidBrush(th.surface);
    let _ = FillRect(hdc, &rect, card_brush);
    let _ = DeleteObject(card_brush.into());

    // 顶部 8px 国家分布堆叠条（第三行行高更宽，分布条可以稍厚一点）
    let geo_bar_height: i32 = 8;
    if !state.traffic_by_country.is_empty() {
        let bar_rect = RECT {
            left: rect.left + 4,
            top: rect.top + 2,
            right: rect.right - 4,
            bottom: rect.top + 2 + geo_bar_height,
        };
        draw_country_bar(hdc, &state.traffic_by_country, bar_rect);
        // 国家名标签（取最大的 3 个简短展示在条下方左侧）
        draw_country_legend(hdc, &state.traffic_by_country, rect.left + 4, rect.top + geo_bar_height + 4, th);
    }

    // 流量曲线占余下区域（顶部跳过国家条 + label，底部留 4 像素 padding）
    let label_strip = if state.traffic_by_country.is_empty() { 0 } else { 14 };
    let spark_rect_inner = RECT {
        left: rect.left + 4,
        top: rect.top + geo_bar_height + label_strip + 4,
        right: rect.right - 4,
        bottom: rect.bottom - 2,
    };
    draw_sparkline_inner(hdc, &state.traffic_history, spark_rect_inner, th);
}

/// 国家分布 legend：左对齐画 top-3 的 "● US 60%" 形式。
/// 圆点颜色与堆叠条一致，方便对应。
unsafe fn draw_country_legend(
    hdc: HDC,
    dist: &[(String, u32)],
    x_start: i32,
    y: i32,
    th: &Theme,
) {
    let total: u32 = dist.iter().map(|(_, c)| *c).sum();
    if total == 0 {
        return;
    }
    let mut x = x_start;
    for (country, count) in dist.iter().take(3) {
        let pct = (*count as f64 / total as f64 * 100.0).round() as i32;
        let color = country_color(country);
        // 小色块
        let brush = CreateSolidBrush(color);
        let dot = RECT { left: x, top: y + 3, right: x + 8, bottom: y + 11 };
        let _ = FillRect(hdc, &dot, brush);
        let _ = DeleteObject(brush.into());
        x += 12;
        // 文字
        let label = format!("{} {}%", country, pct);
        x = draw_text(hdc, &label, x, y, 12, th.fg_dim, x_start + 600);
        x += 6;
    }
}

/// 把 [(国家, 计数)] 渲染成水平堆叠彩条。各段宽度按计数比例分配。
/// 颜色按国家名字符串 hash 取，同一国家每次画都是同色。
unsafe fn draw_country_bar(hdc: HDC, dist: &[(String, u32)], rect: RECT) {
    let total: u32 = dist.iter().map(|(_, c)| *c).sum();
    if total == 0 {
        return;
    }
    let total_w = rect.right - rect.left;
    let mut cursor = rect.left as f64;
    for (country, count) in dist {
        let seg_w = (total_w as f64) * (*count as f64 / total as f64);
        let seg_end = (cursor + seg_w).round() as i32;
        let seg_rect = RECT {
            left: cursor.round() as i32,
            top: rect.top,
            right: seg_end.min(rect.right),
            bottom: rect.bottom,
        };
        let color = country_color(country);
        let brush = CreateSolidBrush(color);
        let _ = FillRect(hdc, &seg_rect, brush);
        let _ = DeleteObject(brush.into());
        cursor = seg_end as f64;
    }
}

/// 字符串 hash → HSV 色相 → COLORREF。固定饱和/明度，色相均匀分布。
fn country_color(name: &str) -> COLORREF {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in name.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    let hue = (h % 360) as f32;
    hsv_to_colorref(hue, 0.65, 0.85)
}

fn hsv_to_colorref(h: f32, s: f32, v: f32) -> COLORREF {
    let c = v * s;
    let h6 = h / 60.0;
    let x = c * (1.0 - (h6 % 2.0 - 1.0).abs());
    let (r1, g1, b1) = match h6 as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    let r = ((r1 + m) * 255.0) as u32;
    let g = ((g1 + m) * 255.0) as u32;
    let b = ((b1 + m) * 255.0) as u32;
    COLORREF((b << 16) | (g << 8) | r)
}

/// 原 sparkline 渲染：上行/下行双折线。
unsafe fn draw_sparkline_inner(hdc: HDC, hist: &VecDeque<TrafficSample>, rect: RECT, th: &Theme) {
    if hist.is_empty() {
        return;
    }

    let max_val = hist
        .iter()
        .map(|s| s.up_bps.max(s.down_bps))
        .max()
        .unwrap_or(1)
        .max(1) as f64
        * 1.1;

    let w = (rect.right - rect.left - 4) as f64;
    let h = (rect.bottom - rect.top - 4) as f64;
    let left = rect.left as f64 + 2.0;
    let baseline = rect.bottom as f64 - 2.0;

    let n = hist.len();
    let dx = if n > 1 { w / (n - 1) as f64 } else { 0.0 };
    let mut up_pts: Vec<POINT> = Vec::with_capacity(n);
    let mut dn_pts: Vec<POINT> = Vec::with_capacity(n);
    for (i, s) in hist.iter().enumerate() {
        let x = (left + dx * i as f64).round() as i32;
        let y_up = (baseline - h * (s.up_bps as f64 / max_val)).round() as i32;
        let y_dn = (baseline - h * (s.down_bps as f64 / max_val)).round() as i32;
        up_pts.push(POINT { x, y: y_up });
        dn_pts.push(POINT { x, y: y_dn });
    }

    draw_polyline(hdc, &dn_pts, th.accent_green, 2);
    draw_polyline(hdc, &up_pts, th.fg_latency, 2);

    // 右下角标当前最高速率
    let label = format_speed(hist.back().map(|s| s.up_bps.max(s.down_bps)).unwrap_or(0));
    let mut wbuf: Vec<u16> = label.encode_utf16().collect();
    let mut measure = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    let _ = DrawTextW(hdc, &mut wbuf, &mut measure, DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX);
    let lw = measure.right - measure.left;
    let mut draw_r = RECT {
        left: rect.right - lw - 4,
        top: rect.bottom - 16,
        right: rect.right - 4,
        bottom: rect.bottom - 2,
    };
    let _ = SetTextColor(hdc, th.fg_dim);
    let _ = DrawTextW(hdc, &mut wbuf, &mut draw_r, DT_RIGHT | DT_SINGLELINE | DT_NOPREFIX);
}

/// GDI 折线辅助 —— MoveToEx + LineTo 序列，一支 PEN 画完即销毁。
unsafe fn draw_polyline(hdc: HDC, pts: &[POINT], color: COLORREF, width: i32) {
    if pts.len() < 2 {
        return;
    }
    let pen = CreatePen(PS_SOLID, width, color);
    let old_pen = SelectObject(hdc, pen.into());
    let _ = MoveToEx(hdc, pts[0].x, pts[0].y, None);
    for p in &pts[1..] {
        let _ = LineTo(hdc, p.x, p.y);
    }
    let _ = SelectObject(hdc, old_pen);
    let _ = DeleteObject(pen.into());
}

fn format_network_error(reason: Option<&str>) -> String {
    match reason {
        Some(r) if !r.is_empty() => format!("网络异常 ({})", r),
        _ => "网络不可达".to_string(),
    }
}

fn format_geo_missing(reason: Option<&str>) -> String {
    match reason {
        Some(r) if !r.is_empty() => format!("归属地? ({})", r),
        _ => "--".to_string(),
    }
}

fn format_location(geo: &GeoInfo) -> String {
    if !geo.city.is_empty() {
        geo.city.clone()
    } else if !geo.country.is_empty() {
        geo.country.clone()
    } else {
        "--".to_string()
    }
}

fn format_speed(bps: u64) -> String {
    if bps >= 1_000_000 {
        format!("{:.1}MB/s", bps as f64 / 1_000_000.0)
    } else if bps >= 1_000 {
        format!("{:.1}KB/s", bps as f64 / 1_000.0)
    } else {
        format!("{}B/s", bps)
    }
}

unsafe fn draw_dot(hdc: HDC, cx: i32, cy: i32, r: i32, color: COLORREF) {
    let brush = CreateSolidBrush(color);
    let pen = CreatePen(PS_SOLID, 1, color);
    let old_pen = SelectObject(hdc, pen.into());
    let old_brush = SelectObject(hdc, brush.into());
    let _ = Ellipse(hdc, cx - r, cy - r, cx + r, cy + r);
    let _ = SelectObject(hdc, old_brush);
    let _ = SelectObject(hdc, old_pen);
    let _ = DeleteObject(brush.into());
    let _ = DeleteObject(pen.into());
}

unsafe fn draw_text(hdc: HDC, text: &str, x: i32, y: i32, h: i32, color: COLORREF, max_x: i32) -> i32 {
    let _ = SetTextColor(hdc, color);
    let mut wbuf: Vec<u16> = text.encode_utf16().collect();

    let mut measure = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    let _ = DrawTextW(hdc, &mut wbuf, &mut measure, DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX);
    let text_w = measure.right - measure.left;

    if x + text_w > max_x {
        let suffix: Vec<u16> = "...".encode_utf16().collect();
        let mut truncated = wbuf.clone();
        while truncated.len() > 1 {
            truncated.pop();
            let mut combined = truncated.clone();
            combined.extend_from_slice(&suffix);
            let mut m = RECT { left: 0, top: 0, right: 0, bottom: 0 };
            let _ = DrawTextW(hdc, &mut combined, &mut m, DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX);
            if x + m.right - m.left <= max_x {
                let mut draw_rect = RECT { left: x, top: y, right: max_x, bottom: y + h };
                let _ = DrawTextW(hdc, &mut combined, &mut draw_rect, DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX);
                return max_x;
            }
        }
        return x;
    }

    let mut draw_rect = RECT { left: x, top: y, right: x + text_w, bottom: y + h };
    let _ = DrawTextW(hdc, &mut wbuf, &mut draw_rect, DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX);
    x + text_w + 6
}

unsafe fn draw_sep(hdc: HDC, x: i32, y: i32, h: i32, color: COLORREF) -> i32 {
    let brush = CreateSolidBrush(color);
    let rect = RECT { left: x, top: y + h / 2 - 6, right: x + 1, bottom: y + h / 2 + 6 };
    let _ = FillRect(hdc, &rect, brush);
    let _ = DeleteObject(brush.into());
    x + 12
}

pub fn set_window_opacity(hwnd: HWND, opacity: f32) {
    unsafe {
        let alpha = (opacity * 255.0) as u8;
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA_RAW);
    }
}
