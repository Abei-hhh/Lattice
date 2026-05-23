use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;

use crate::network::geo_lookup::GeoInfo;

use super::theme::Theme;

// BG_COLOR 保留作为 WNDCLASS 注册时的默认背景画刷（窗口创建一次性用），
// 实际绘制走 state.theme。窗口创建后 theme 切换会触发 InvalidateRect 重画。
pub const BG_COLOR: COLORREF = COLORREF(0x00_2D_2D_2D);

const LWA_ALPHA_RAW: u32 = 0x02;
pub const ROW_HEIGHT: i32 = 26;
pub const WIN_HEIGHT: i32 = ROW_HEIGHT * 2;

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

    w1.max(w2).max(280).min(600)
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

    if !state.proxy_enabled {
        x += 12; // gap
        x += txt_width(hdc, "未设置代理") + 6;
    }

    x + 4 // right padding
}

fn measure_row2(hdc: HDC, state: &OverlayState) -> i32 {
    let mut x: i32 = 18;

    // Each draw_text adds +6 spacing, mirrors draw calls exactly
    x += txt_width(hdc, "\u{2191}") + 6;
    x += txt_width(hdc, &format_speed(state.net_up)) + 6;
    x += 6; // gap before ↓
    x += txt_width(hdc, "\u{2193}") + 6;
    x += txt_width(hdc, &format_speed(state.net_down)) + 6;
    x += 2 + 12; // gap + sep
    x += txt_width(hdc, &format!("CPU {:.0}%", state.cpu_usage)) + 6;
    x += 2 + 12; // gap + sep
    x += txt_width(hdc, &format!("内存 {:.0}%", state.mem_usage)) + 6;

    x + 4 // right padding
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

        if !state.proxy_enabled {
            draw_text(hdc, "未设置代理", x + 12, 0, ROW_HEIGHT, th.fg_dim, max_x);
        }

        // 行分隔线
        {
            let sep_brush = CreateSolidBrush(th.separator);
            let sep_rect = RECT { left: 10, top: ROW_HEIGHT - 1, right: width - 10, bottom: ROW_HEIGHT };
            let _ = FillRect(hdc, &sep_rect, sep_brush);
            let _ = DeleteObject(sep_brush.into());
        }

        // === Row 2: 网速 + CPU + 内存 ===
        let y2 = ROW_HEIGHT;
        let mut x2: i32 = 18 + row2_offset;

        x2 = draw_text(hdc, "\u{2191}", x2, y2, ROW_HEIGHT, th.fg_secondary, max_x);
        x2 = draw_text(hdc, &format_speed(state.net_up), x2, y2, ROW_HEIGHT, th.fg_secondary, max_x);

        x2 = draw_text(hdc, "\u{2193}", x2 + 6, y2, ROW_HEIGHT, th.fg_secondary, max_x);
        x2 = draw_text(hdc, &format_speed(state.net_down), x2, y2, ROW_HEIGHT, th.fg_secondary, max_x);

        x2 = draw_sep(hdc, x2 + 2, y2, ROW_HEIGHT, th.separator);
        x2 = draw_text(hdc, &format!("CPU {:.0}%", state.cpu_usage), x2, y2, ROW_HEIGHT, th.fg_secondary, max_x);

        x2 = draw_sep(hdc, x2 + 2, y2, ROW_HEIGHT, th.separator);
        draw_text(hdc, &format!("内存 {:.0}%", state.mem_usage), x2, y2, ROW_HEIGHT, th.fg_secondary, max_x);

        let _ = SelectObject(hdc, old_font);
        let _ = DeleteObject(font.into());
        let _ = EndPaint(hwnd, &ps);
    }
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
