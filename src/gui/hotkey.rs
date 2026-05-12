use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS,
};

use crate::config::AppConfig;

pub const HOTKEY_TOGGLE: i32 = 1;
pub const HOTKEY_LOOKUP: i32 = 2;
pub const HOTKEY_QUIT: i32 = 3;

pub fn register_hotkeys(hwnd: HWND, config: &AppConfig) -> bool {
    let mut all_ok = true;

    if let Some((mod_flags, vk)) = crate::config::parse_hotkey(&config.hotkey_toggle) {
        unsafe {
            if RegisterHotKey(Some(hwnd), HOTKEY_TOGGLE, HOT_KEY_MODIFIERS(mod_flags), vk as u32).is_err() {
                tracing::error!("Failed to register hotkey '{}' for toggle", config.hotkey_toggle);
                all_ok = false;
            } else {
                tracing::info!("Registered hotkey '{}' for toggle", config.hotkey_toggle);
            }
        }
    }

    if let Some((mod_flags, vk)) = crate::config::parse_hotkey(&config.hotkey_lookup) {
        unsafe {
            if RegisterHotKey(Some(hwnd), HOTKEY_LOOKUP, HOT_KEY_MODIFIERS(mod_flags), vk as u32).is_err() {
                tracing::error!("Failed to register hotkey '{}' for lookup", config.hotkey_lookup);
                all_ok = false;
            } else {
                tracing::info!("Registered hotkey '{}' for lookup", config.hotkey_lookup);
            }
        }
    }

    if let Some((mod_flags, vk)) = crate::config::parse_hotkey(&config.hotkey_quit) {
        unsafe {
            if RegisterHotKey(Some(hwnd), HOTKEY_QUIT, HOT_KEY_MODIFIERS(mod_flags), vk as u32).is_err() {
                tracing::error!("Failed to register hotkey '{}' for quit", config.hotkey_quit);
                all_ok = false;
            } else {
                tracing::info!("Registered hotkey '{}' for quit", config.hotkey_quit);
            }
        }
    }

    all_ok
}

pub fn unregister_hotkeys(hwnd: HWND) {
    unsafe {
        let _ = UnregisterHotKey(Some(hwnd), HOTKEY_TOGGLE);
        let _ = UnregisterHotKey(Some(hwnd), HOTKEY_LOOKUP);
        let _ = UnregisterHotKey(Some(hwnd), HOTKEY_QUIT);
    }
}
