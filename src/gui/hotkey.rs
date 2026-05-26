use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS,
};

use crate::config::AppConfig;

pub const HOTKEY_TOGGLE: i32 = 1;
pub const HOTKEY_QUIT: i32 = 3;
/// 浮窗位置 ±20px 微调。写死 Ctrl+Alt+方向键 —— 锁定时也能用，
/// 比拖动更精准；用户精调对齐时不必先解锁再锁。Pixel step 在
/// window.rs::NUDGE_STEP 里定义。
pub const HOTKEY_NUDGE_UP: i32 = 4;
pub const HOTKEY_NUDGE_DOWN: i32 = 5;
pub const HOTKEY_NUDGE_LEFT: i32 = 6;
pub const HOTKEY_NUDGE_RIGHT: i32 = 7;

/// Windows VK 常量。windows crate 里有等价物，但拉一组裸常量更直观，
/// 避免再加 import。
const VK_LEFT: u8 = 0x25;
const VK_UP: u8 = 0x26;
const VK_RIGHT: u8 = 0x27;
const VK_DOWN: u8 = 0x28;

/// 注册结果：成功返回空 Vec，每条失败为 `(用途名, 组合键文本)`，调用方
/// 据此弹 MessageBox 提示用户哪几个键被其他程序占用、去哪改键。
///
/// 选择"返回失败列表"而非 bool：用户经常只有一两个键冲突（典型如
/// `Ctrl+Alt+I` 被 IDE 占），笼统地说"热键注册失败"无法指引下一步动作。
pub fn register_hotkeys(hwnd: HWND, config: &AppConfig) -> Vec<(&'static str, String)> {
    let mut failed: Vec<(&'static str, String)> = Vec::new();

    fn try_one(
        hwnd: HWND,
        id: i32,
        combo: &str,
        label: &'static str,
        failed: &mut Vec<(&'static str, String)>,
    ) {
        if let Some((mod_flags, vk)) = crate::config::parse_hotkey(combo) {
            let ok = unsafe {
                RegisterHotKey(Some(hwnd), id, HOT_KEY_MODIFIERS(mod_flags), vk as u32).is_ok()
            };
            if ok {
                tracing::info!("Registered hotkey '{}' for {}", combo, label);
            } else {
                tracing::error!("Failed to register hotkey '{}' for {}", combo, label);
                failed.push((label, combo.to_string()));
            }
        } else {
            // 解析失败也算"失败"，让用户知道 config 里的字符串写错了
            tracing::error!("Cannot parse hotkey '{}' for {}", combo, label);
            failed.push((label, combo.to_string()));
        }
    }

    try_one(hwnd, HOTKEY_TOGGLE, &config.hotkey_toggle, "显隐", &mut failed);
    try_one(hwnd, HOTKEY_QUIT, &config.hotkey_quit, "退出", &mut failed);

    // Nudge 热键：4 个一起算，全失败才合并报一条；只是 nice-to-have，
    // 不至于因为方向键冲突就刷 4 条提示。
    let mod_ctrl_alt = 0x0002u32 | 0x0001u32; // MOD_CONTROL | MOD_ALT
    let nudges = [
        (HOTKEY_NUDGE_UP, VK_UP),
        (HOTKEY_NUDGE_DOWN, VK_DOWN),
        (HOTKEY_NUDGE_LEFT, VK_LEFT),
        (HOTKEY_NUDGE_RIGHT, VK_RIGHT),
    ];
    let nudge_ok = nudges.iter().filter(|(id, vk)| unsafe {
        RegisterHotKey(Some(hwnd), *id, HOT_KEY_MODIFIERS(mod_ctrl_alt), *vk as u32).is_ok()
    }).count();
    if nudge_ok == 0 {
        failed.push(("位置微调", "Ctrl+Alt+方向键".to_string()));
    } else if nudge_ok < nudges.len() {
        tracing::warn!("Nudge hotkeys partially registered ({}/4)", nudge_ok);
    }

    failed
}

pub fn unregister_hotkeys(hwnd: HWND) {
    unsafe {
        let _ = UnregisterHotKey(Some(hwnd), HOTKEY_TOGGLE);
        let _ = UnregisterHotKey(Some(hwnd), HOTKEY_QUIT);
        let _ = UnregisterHotKey(Some(hwnd), HOTKEY_NUDGE_UP);
        let _ = UnregisterHotKey(Some(hwnd), HOTKEY_NUDGE_DOWN);
        let _ = UnregisterHotKey(Some(hwnd), HOTKEY_NUDGE_LEFT);
        let _ = UnregisterHotKey(Some(hwnd), HOTKEY_NUDGE_RIGHT);
    }
}
