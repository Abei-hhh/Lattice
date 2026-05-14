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

    #[serde(default)]
    pub enable_log: bool,

    /// System monitor refresh interval in seconds (CPU/memory/network speed)
    #[serde(default = "default_monitor_interval")]
    pub monitor_interval: u64,
    /// Proxy detection interval in seconds (registry + PAC + port + process)
    #[serde(default = "default_proxy_interval")]
    pub proxy_check_interval: u64,
    /// Claude model label refresh interval in seconds (0 = read once at startup only)
    #[serde(default = "default_model_refresh_interval")]
    pub model_refresh_interval: u64,
}

fn default_check_interval() -> u64 { 10 }
fn default_true() -> bool { true }
fn default_opacity() -> f32 { 0.85 }
fn default_position() -> String { "top-center".to_string() }
fn default_timeout() -> u64 { 5 }
fn default_max_retries() -> u32 { 3 }
fn default_monitor_interval() -> u64 { 2 }
fn default_proxy_interval() -> u64 { 30 }
fn default_model_refresh_interval() -> u64 { 0 }
fn default_hotkey_toggle() -> String { "ctrl+alt+h".to_string() }
fn default_hotkey_lookup() -> String { "ctrl+alt+i".to_string() }
fn default_hotkey_quit() -> String { "ctrl+alt+shift+k".to_string() }

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
            enable_log: false,
            monitor_interval: default_monitor_interval(),
            proxy_check_interval: default_proxy_interval(),
            model_refresh_interval: default_model_refresh_interval(),
        }
    }
}

pub fn config_path() -> PathBuf {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    exe_dir.join("config.toml")
}

const DEFAULT_CONFIG: &str = r#"# Vpn_Monitor 配置文件
# 修改后重启程序生效

check_interval = 10         # IP 检测间隔（秒），延迟也随此周期测量
auto_start = true           # 开机自启（预留）
click_through = false       # 鼠标穿透
opacity = 0.85              # 窗口透明度 (0.0 ~ 1.0)
position = "top-center"     # 窗口位置（预留）
show_isp = true             # 是否显示 ISP（查询窗口生效）

hotkey_toggle = "ctrl+alt+h"         # 切换显隐
hotkey_lookup = "ctrl+alt+i"         # 打开查询窗口
hotkey_quit = "ctrl+alt+shift+k"     # 退出程序

timeout = 5                 # 请求超时（秒）
max_retries = 3             # 最大重试次数
enable_log = false          # 是否启用日志记录
# proxy = "socks5://127.0.0.1:1080"  # 可选代理

monitor_interval = 2        # 系统监控刷新间隔（秒）：CPU/内存/网速
proxy_check_interval = 30   # 代理检测间隔（秒）：注册表+PAC+端口+进程
model_refresh_interval = 0  # Claude 模型标签刷新间隔（秒），0=仅启动时读取一次
"#;

pub fn load_config(path: Option<PathBuf>) -> AppConfig {
    let path = path.unwrap_or_else(config_path);
    if !path.exists() {
        let _ = fs::write(&path, DEFAULT_CONFIG);
        tracing::info!("Generated default config at {:?}", path);
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
