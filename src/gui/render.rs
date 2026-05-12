use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;

use crate::network::geo_lookup::GeoInfo;

// Material Design Dark Theme
pub const BG_COLOR: COLORREF = COLORREF(0x00_2D_2D_2D); // #2D2D2D
const TEXT_PRIMARY: COLORREF = COLORREF(0x00_FF_FF_FF);
const TEXT_SECONDARY: COLORREF = COLORREF(0x00_B3_B3_B0);
const ACCENT_GREEN: COLORREF = COLORREF(0x00_50_AF_CA);
const ACCENT_RED: COLORREF = COLORREF(0x00_36_43_F4);
// Checking: vivid blue (#2196F3) — distinct from the orange rate-limited state.
const ACCENT_BLUE: COLORREF = COLORREF(0x00_F3_96_21);
// ApiLimited: saturated orange (#FF6F00) — clearly hotter than the blue.
const ACCENT_ORANGE: COLORREF = COLORREF(0x00_00_6F_FF);
const SEPARATOR_COLOR: COLORREF = COLORREF(0x00_55_55_55);

const LWA_ALPHA_RAW: u32 = 0x02;

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
}

#[allow(dead_code)]
pub struct OverlayState {
    pub current_update: IpUpdate,
    pub visible: bool,
    pub show_isp: bool,
    pub opacity: f32,
}

impl Default for OverlayState {
    fn default() -> Self {
        Self {
            current_update: IpUpdate {
                ip: None,
                geo: None,
                status: CheckStatus::Checking,
            },
            visible: true,
            show_isp: true,
            opacity: 0.92,
        }
    }
}

pub type SharedState = Arc<Mutex<OverlayState>>;

extern "system" {
    fn SetLayeredWindowAttributes(hwnd: HWND, crkey: COLORREF, balpha: u8, dwflags: u32) -> BOOL;
}

pub fn paint_overlay(hwnd: HWND, state: &OverlayState, width: i32, height: i32) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);

        // Fill background
        let bg_brush = CreateSolidBrush(BG_COLOR);
        let full_rect = RECT { left: 0, top: 0, right: width, bottom: height };
        let _ = FillRect(hdc, &full_rect, bg_brush);
        let _ = DeleteObject(bg_brush.into());

        let _ = SetBkMode(hdc, BACKGROUND_MODE(1)); // TRANSPARENT

        // Font: Segoe UI 13px
        let font = CreateFontW(
            -13, 0, 0, 0,
            FW_NORMAL.0 as i32,
            0, 0, 0,
            DEFAULT_CHARSET,
            FONT_OUTPUT_PRECISION(0),
            FONT_CLIP_PRECISION(0),
            FONT_QUALITY(0),
            DEFAULT_PITCH.0 as u32,
            windows::core::w!("Segoe UI"),
        );
        let old_font = SelectObject(hdc, font.into());

        let update = &state.current_update;

        // Status dot
        let dot_color = match update.status {
            CheckStatus::Success => ACCENT_GREEN,
            CheckStatus::NetworkError => ACCENT_RED,
            CheckStatus::Checking => ACCENT_BLUE,
            CheckStatus::ApiLimited => ACCENT_ORANGE,
        };
        let dot_cx = 18;
        let dot_cy = height / 2;
        let dot_r: i32 = 4;
        let dot_brush = CreateSolidBrush(dot_color);
        let dot_pen = CreatePen(PS_SOLID, 1, dot_color);
        let old_pen = SelectObject(hdc, dot_pen.into());
        let old_brush = SelectObject(hdc, dot_brush.into());
        let _ = Ellipse(hdc, dot_cx - dot_r, dot_cy - dot_r, dot_cx + dot_r, dot_cy + dot_r);
        let _ = SelectObject(hdc, old_brush);
        let _ = SelectObject(hdc, old_pen);
        let _ = DeleteObject(dot_brush.into());
        let _ = DeleteObject(dot_pen.into());

        let mut x: i32 = 30;

        match &update.status {
            CheckStatus::Checking => {
                x = draw_text(hdc, "检测中...", x, height, ACCENT_BLUE);
            }
            CheckStatus::NetworkError => {
                x = draw_text(hdc, "网络不可达", x, height, ACCENT_RED);
            }
            CheckStatus::ApiLimited => {
                // Still show the last-known IP so the user has context while waiting.
                if let Some(ip) = update.ip.as_deref() {
                    x = draw_text(hdc, ip, x, height, TEXT_PRIMARY);
                    x = draw_separator(hdc, x, height);
                }
                x = draw_text(hdc, "查询受限", x, height, ACCENT_ORANGE);
            }
            CheckStatus::Success => {
                // IP
                let ip_str = update.ip.as_deref().unwrap_or("--");
                x = draw_text(hdc, ip_str, x, height, TEXT_PRIMARY);

                // Separator line
                x = draw_separator(hdc, x, height);

                // Location
                if let Some(geo) = &update.geo {
                    let loc = format_location(geo);
                    x = draw_text(hdc, &loc, x, height, TEXT_SECONDARY);
                } else {
                    x = draw_text(hdc, "--", x, height, TEXT_SECONDARY);
                }

                // ISP
                if state.show_isp {
                    if let Some(geo) = &update.geo {
                        if !geo.isp.is_empty() {
                            x = draw_separator(hdc, x, height);
                            draw_text(hdc, &geo.isp, x, height, TEXT_SECONDARY);
                        }
                    }
                }
            }
        }

        let _ = SelectObject(hdc, old_font);
        let _ = DeleteObject(font.into());
        let _ = EndPaint(hwnd, &ps);
    }
}

fn format_location(geo: &GeoInfo) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if !geo.country.is_empty() {
        parts.push(&geo.country);
    }
    if !geo.region.is_empty() && geo.region != geo.city {
        parts.push(&geo.region);
    }
    if !geo.city.is_empty() {
        parts.push(&geo.city);
    }
    parts.join(" · ")
}

unsafe fn draw_text(hdc: HDC, text: &str, x: i32, height: i32, color: COLORREF) -> i32 {
    let _ = SetTextColor(hdc, color);
    let mut wbuf: Vec<u16> = text.encode_utf16().collect();

    // Measure the rendered width with the currently selected font.
    let mut measure = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    let _ = DrawTextW(
        hdc,
        &mut wbuf,
        &mut measure,
        DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX,
    );
    let text_w = measure.right - measure.left;

    let mut draw_rect = RECT { left: x, top: 0, right: x + text_w, bottom: height };
    let _ = DrawTextW(
        hdc,
        &mut wbuf,
        &mut draw_rect,
        DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
    );
    x + text_w + 6
}

unsafe fn draw_separator(hdc: HDC, x: i32, height: i32) -> i32 {
    let sep_brush = CreateSolidBrush(SEPARATOR_COLOR);
    let sep_rect = RECT { left: x, top: height / 2 - 7, right: x + 1, bottom: height / 2 + 7 };
    let _ = FillRect(hdc, &sep_rect, sep_brush);
    let _ = DeleteObject(sep_brush.into());
    x + 12
}

pub fn set_window_opacity(hwnd: HWND, opacity: f32) {
    unsafe {
        let alpha = (opacity * 255.0) as u8;
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA_RAW);
    }
}
