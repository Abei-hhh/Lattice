//! 主题色板 —— Material Design 3 概念的精简映射。
//!
//! 三种模式：
//! - `Light` / `Dark` —— 固定预设
//! - `System` —— 读 HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\
//!   Personalize\AppsUseLightTheme（0 = dark, 1 = light）
//!
//! 浮窗 / 对话框的颜色全部走这里取，便于全局切换。

use windows::Win32::Foundation::{COLORREF, HWND};
use windows::Win32::Graphics::Dwm::*;

/// 给一个对话框 HWND 应用 dark / light 标题栏样式（Win10 1809+ / Win11）。
/// 对老系统这个 API 失败但不影响其他渲染，可以安全无视错误。
pub fn apply_dark_titlebar(hwnd: HWND, dark: bool) {
    unsafe {
        let value: u32 = if dark { 1 } else { 0 };
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &value as *const _ as *const _,
            std::mem::size_of::<u32>() as u32,
        );
    }
}

/// 一个完整的主题色板。所有 UI 颜色都从这里取，不再写硬编码 const。
#[derive(Debug, Clone)]
pub struct Theme {
    /// 主背景
    pub bg: COLORREF,
    /// surface（卡片、按钮 idle 等比 bg 微亮 / 微暗的一层）
    pub surface: COLORREF,
    /// surface hover 态
    pub surface_hover: COLORREF,
    /// surface 按下态
    pub surface_pressed: COLORREF,
    /// 主要文本（最高对比）
    pub fg_primary: COLORREF,
    /// 次要文本（说明文字、副标题）
    pub fg_secondary: COLORREF,
    /// 弱化文本（缓存状态、占位、disabled）
    pub fg_dim: COLORREF,
    /// 分割线 / 边框
    pub separator: COLORREF,
    /// 延迟显示用淡色
    pub fg_latency: COLORREF,
    /// 状态点：正常
    pub accent_green: COLORREF,
    /// 状态点：错误
    pub accent_red: COLORREF,
    /// 状态点：检测中
    pub accent_blue: COLORREF,
    /// 状态点：限流 / 警告
    pub accent_orange: COLORREF,
}

/// 把 `#RRGGBB` 字面量转 GDI COLORREF（BGR 顺序）。
const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF((b as u32) << 16 | (g as u32) << 8 | (r as u32))
}

/// 暗色（沿用原有浮窗配色）。
pub const DARK: Theme = Theme {
    bg: rgb(0x2D, 0x2D, 0x2D),
    surface: rgb(0x3A, 0x3A, 0x3A),
    surface_hover: rgb(0x45, 0x45, 0x45),
    surface_pressed: rgb(0x50, 0x50, 0x50),
    fg_primary: rgb(0xFF, 0xFF, 0xFF),
    fg_secondary: rgb(0xB0, 0xB3, 0xB3),
    fg_dim: rgb(0x88, 0x88, 0x88),
    separator: rgb(0x55, 0x55, 0x55),
    fg_latency: rgb(0xCE, 0xBE, 0x8A),
    accent_green: rgb(0x4C, 0xAF, 0x50),
    accent_red: rgb(0xF4, 0x43, 0x36),
    accent_blue: rgb(0x21, 0x96, 0xF3),
    accent_orange: rgb(0xFF, 0x6F, 0x00),
};

/// 亮色（白色背景、深色文字，强调色保持一致）。
pub const LIGHT: Theme = Theme {
    bg: rgb(0xF7, 0xF7, 0xF7),
    surface: rgb(0xFF, 0xFF, 0xFF),
    surface_hover: rgb(0xEC, 0xEC, 0xEC),
    surface_pressed: rgb(0xDD, 0xDD, 0xDD),
    fg_primary: rgb(0x1A, 0x1A, 0x1A),
    fg_secondary: rgb(0x55, 0x55, 0x55),
    fg_dim: rgb(0x88, 0x88, 0x88),
    separator: rgb(0xCC, 0xCC, 0xCC),
    fg_latency: rgb(0x55, 0x6B, 0x82),
    accent_green: rgb(0x2E, 0x7D, 0x32),
    accent_red: rgb(0xC6, 0x28, 0x28),
    accent_blue: rgb(0x15, 0x65, 0xC0),
    accent_orange: rgb(0xE6, 0x5A, 0x00),
};

/// 根据 mode 字符串解析为具体 Theme。
/// "light" → LIGHT、"dark" → DARK、"system" / 其它 → 探测系统设置。
pub fn resolve(mode: &str) -> Theme {
    match mode {
        "light" => LIGHT,
        "dark" => DARK,
        _ => system_theme(),
    }
}

/// 探测 Win10/11 个性化里 "Apps use light theme" 设置。
/// AppsUseLightTheme = 1 → 亮色；0 → 暗色；缺失 → 默认暗色（应用主色调）。
pub fn system_theme() -> Theme {
    if is_system_light() { LIGHT } else { DARK }
}

/// 对外暴露：当前活动 theme 是否为暗色 —— 三个对话框创建后调
/// `DwmSetWindowAttribute(USE_IMMERSIVE_DARK_MODE)` 时用。
pub fn is_active_dark(mode: &str) -> bool {
    match mode {
        "dark" => true,
        "light" => false,
        _ => !is_system_light(),
    }
}

fn is_system_light() -> bool {
    use windows::core::w;
    use windows::Win32::System::Registry::*;

    unsafe {
        let mut hkey = HKEY::default();
        let subkey = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
        if RegOpenKeyExW(HKEY_CURRENT_USER, subkey, Some(0), KEY_QUERY_VALUE, &mut hkey)
            .is_err()
        {
            return false; // 缺失时默认暗色
        }
        let mut data: u32 = 0;
        let mut size: u32 = std::mem::size_of::<u32>() as u32;
        let mut ty: REG_VALUE_TYPE = REG_DWORD;
        let value_name = w!("AppsUseLightTheme");
        let result = RegQueryValueExW(
            hkey,
            value_name,
            None,
            Some(&mut ty),
            Some(&mut data as *mut u32 as *mut u8),
            Some(&mut size),
        );
        let _ = RegCloseKey(hkey);
        result.is_ok() && ty == REG_DWORD && data == 1
    }
}
