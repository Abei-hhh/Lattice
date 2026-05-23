//! Material Design 3 风格的 owner-draw 辅助。
//!
//! 当前覆盖：按钮（Filled 风格）—— 圆角矩形 + 主题色背景 + ClearType 文字。
//! Hover 态需要每个按钮 subclass + TrackMouseEvent，工作量大，先不做；
//! 焦点态通过 ODS_FOCUS 画 1px 描边。
//!
//! 用法：在对话框 dialog_proc 处理 WM_DRAWITEM，把每个 button 转发到
//! [`draw_button`]。CreateWindow 时给 button 加 BS_OWNERDRAW 样式。

use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Controls::{DRAWITEMSTRUCT, ODS_DISABLED, ODS_FOCUS, ODS_SELECTED};
use windows::Win32::UI::WindowsAndMessaging::*;

use super::theme::Theme;

const BS_OWNERDRAW_RAW: u32 = 0x0000_000B;
pub const BS_OWNERDRAW_STYLE: windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE =
    windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(BS_OWNERDRAW_RAW);

/// 给一个 owner-draw 按钮画 MD3 Filled 风格背景 + 居中文字。
///
/// `is_primary` 区分主操作（"确定"）和次要操作（"取消" / "应用"）：
/// - 主操作：填充 accent_green，白字
/// - 次要操作：填充 surface，主题前景文字
pub unsafe fn draw_button(dis: &DRAWITEMSTRUCT, theme: &Theme, is_primary: bool) {
    let hdc = dis.hDC;
    let rect = dis.rcItem;
    let s = dis.itemState.0;
    let pressed = s & ODS_SELECTED.0 != 0;
    let focused = s & ODS_FOCUS.0 != 0;
    let disabled = s & ODS_DISABLED.0 != 0;

    // 背景色：主操作用 accent，次要用 surface。按下时整体变暗一档。
    let mut bg = if is_primary { theme.accent_green } else { theme.surface };
    if pressed {
        bg = darken(bg, 0.85);
    }
    let fg = if is_primary {
        COLORREF(0x00_FF_FF_FF) // 白
    } else {
        theme.fg_primary
    };
    let fg = if disabled { theme.fg_dim } else { fg };

    // 整体先擦背景（防止上次绘制残留），再 RoundRect 画圆角主体
    let parent_brush = CreateSolidBrush(theme.bg);
    let _ = FillRect(hdc, &rect, parent_brush);
    let _ = DeleteObject(parent_brush.into());

    let brush = CreateSolidBrush(bg);
    let pen_color = if focused { theme.accent_green } else { bg };
    let pen = CreatePen(PS_SOLID, if focused { 2 } else { 1 }, pen_color);

    let old_brush = SelectObject(hdc, brush.into());
    let old_pen = SelectObject(hdc, pen.into());

    // 圆角半径 12（MD3 small radius）
    let _ = RoundRect(
        hdc,
        rect.left, rect.top, rect.right, rect.bottom,
        20, 20,
    );

    let _ = SelectObject(hdc, old_brush);
    let _ = SelectObject(hdc, old_pen);
    let _ = DeleteObject(brush.into());
    let _ = DeleteObject(pen.into());

    // 取按钮当前文字
    let len = GetWindowTextLengthW(dis.hwndItem) as usize + 1;
    let mut buf = vec![0u16; len];
    let read = GetWindowTextW(dis.hwndItem, &mut buf) as usize;
    let text_slice = &mut buf[..read];

    let _ = SetBkMode(hdc, BACKGROUND_MODE(1));
    let _ = SetTextColor(hdc, fg);
    let mut draw_rect = rect;
    let _ = DrawTextW(
        hdc,
        text_slice,
        &mut draw_rect,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
    );
}

fn darken(c: COLORREF, factor: f32) -> COLORREF {
    let raw = c.0;
    let b = ((raw >> 16) & 0xFF) as f32;
    let g = ((raw >> 8) & 0xFF) as f32;
    let r = (raw & 0xFF) as f32;
    let nr = (r * factor).clamp(0.0, 255.0) as u32;
    let ng = (g * factor).clamp(0.0, 255.0) as u32;
    let nb = (b * factor).clamp(0.0, 255.0) as u32;
    COLORREF((nb << 16) | (ng << 8) | nr)
}

// 抑制未使用 PCWSTR 引入的 warning（保留以备未来 ToolTip 等扩展用）
#[allow(dead_code)]
fn _silence_unused() {
    let _: PCWSTR = PCWSTR::null();
}
