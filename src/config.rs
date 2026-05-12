use serde::Deserialize;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Deserialize, Clone)]
pub struct AppConfig {
    #[serde(default = "default_check_interval")]
    pub check_interval: u64,
    #[serde(default = "default_true")]
    pub auto_start: bool,
    #[serde(default)]
    pub click_through: bool,

    #[serde(default = "default_opacity")]
    pub opacity: f32,
    #[serde(default = "default_position")]
    pub position: String,
    #[serde(default = "default_true")]
    pub show_isp: bool,

    #[serde(default = "default_hotkey_toggle")]
    pub hotkey_toggle: String,
    #[serde(default = "default_hotkey_lookup")]
    pub hotkey_lookup: String,
    #[serde(default = "default_hotkey_quit")]
    pub hotkey_quit: String,

    #[serde(default = "default_timeout")]
    pub timeout: u64,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    pub proxy: Option<String>,
}

fn default_check_interval() -> u64 { 30 }
fn default_true() -> bool { true }
fn default_opacity() -> f32 { 0.85 }
fn default_position() -> String { "top-center".to_string() }
fn default_timeout() -> u64 { 5 }
fn default_max_retries() -> u32 { 3 }
fn default_hotkey_toggle() -> String { "ctrl+alt+h".to_string() }
fn default_hotkey_lookup() -> String { "ctrl+alt+i".to_string() }
fn default_hotkey_quit() -> String { "ctrl+alt+q".to_string() }

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            check_interval: default_check_interval(),
            auto_start: true,
            click_through: false,
            opacity: default_opacity(),
            position: default_position(),
            show_isp: true,
            hotkey_toggle: default_hotkey_toggle(),
            hotkey_lookup: default_hotkey_lookup(),
            hotkey_quit: default_hotkey_quit(),
            timeout: default_timeout(),
            max_retries: default_max_retries(),
            proxy: None,
        }
    }
}

pub fn config_path() -> PathBuf {
    let app_data = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    app_data.join("Vpn_Monitor").join("config.toml")
}

pub fn load_config(path: Option<PathBuf>) -> AppConfig {
    let path = path.unwrap_or_else(config_path);
    if !path.exists() {
        tracing::info!("Config file not found at {:?}, using defaults", path);
        return AppConfig::default();
    }
    match fs::read_to_string(&path) {
        Ok(content) => match toml::from_str(&content) {
            Ok(config) => {
                tracing::info!("Loaded config from {:?}", path);
                config
            }
            Err(e) => {
                tracing::warn!("Failed to parse config.toml: {}, using defaults", e);
                AppConfig::default()
            }
        },
        Err(e) => {
            tracing::warn!("Failed to read config.toml: {}, using defaults", e);
            AppConfig::default()
        }
    }
}

/// Parse hotkey string like "ctrl+alt+h" into (modifier_flags, vk_code).
/// Returns None if parsing fails.
pub fn parse_hotkey(s: &str) -> Option<(u32, u8)> {
    let lower = s.to_lowercase();
    let parts: Vec<&str> = lower.split('+').collect();
    if parts.is_empty() {
        return None;
    }

    let mut modifiers: u32 = 0;
    let mut vk: Option<u8> = None;

    for part in parts {
        let part = part.trim();
        match part {
            "ctrl" | "control" => modifiers |= 0x0002, // MOD_CONTROL
            "alt" => modifiers |= 0x0001,              // MOD_ALT
            "shift" => modifiers |= 0x0004,            // MOD_SHIFT
            "win" | "super" => modifiers |= 0x0008,    // MOD_WIN
            _ => {
                // Try single character
                if part.len() == 1 {
                    vk = Some(part.chars().next().unwrap().to_ascii_uppercase() as u8);
                } else {
                    // Try function keys etc.
                    vk = match part {
                        "f1" => Some(0x70),
                        "f2" => Some(0x71),
                        "f3" => Some(0x72),
                        "f4" => Some(0x73),
                        "f5" => Some(0x74),
                        "f6" => Some(0x75),
                        "f7" => Some(0x76),
                        "f8" => Some(0x77),
                        "f9" => Some(0x78),
                        "f10" => Some(0x79),
                        "f11" => Some(0x7A),
                        "f12" => Some(0x7B),
                        _ => None,
                    };
                }
            }
        }
    }

    vk.map(|v| (modifiers, v))
}
