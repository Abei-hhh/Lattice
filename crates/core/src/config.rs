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

    /// Mask the last two octets of public IPs in log lines (1.2.x.x). The
    /// overlay still shows the full IP — this only affects the on-disk log
    /// file so sharing the log can't leak the user's public IP.
    #[serde(default = "default_true")]
    pub mask_ip_in_log: bool,
    /// Mask country/city/ISP strings in logs as `geo:xxxxxxxx`. The overlay
    /// still shows the real names — only the log file is affected.
    #[serde(default = "default_true")]
    pub mask_geo_in_log: bool,
    /// Cache Geo lookups to disk (~appdata/Vpn_Monitor/geo_cache.json) so
    /// returning to a known node shows the city instantly without re-querying.
    #[serde(default = "default_true")]
    pub geo_cache_enabled: bool,
    /// Geo cache TTL in hours. After this, the entry is re-fetched.
    #[serde(default = "default_geo_cache_ttl_hours")]
    pub geo_cache_ttl_hours: u64,
    /// Maximum number of entries kept in the geo cache (LRU eviction).
    #[serde(default = "default_geo_cache_max_entries")]
    pub geo_cache_max_entries: usize,
    /// Idle threshold in seconds. When `GetLastInputInfo` reports the user has
    /// been idle longer than this, IP poll and monitor intervals are multiplied
    /// by `idle_multiplier` to reduce battery / CPU drain.
    /// 0 disables idle-aware scaling.
    #[serde(default = "default_idle_threshold")]
    pub idle_threshold_seconds: u64,
    #[serde(default = "default_idle_multiplier")]
    pub idle_multiplier: u64,
    /// Cross-check geo across HTTPS (ipwho.is) and HTTP (ip-api.com) providers.
    /// When both succeed and report different countries, log a warning — this
    /// catches MITM-spoofed HTTP responses. The HTTPS result is always preferred
    /// when both succeed.
    #[serde(default = "default_true")]
    pub geo_cross_check: bool,

    /// 主题模式："system" / "light" / "dark"。system = 跟随 OS 个性化设置。
    #[serde(default = "default_theme")]
    pub theme: String,

    /// 浮窗左上 tag 显示哪个 cc-switch 工具的当前模型。
    /// 候选：claude / codex / gemini / opencode / openclaw / hermes。
    #[serde(default = "default_cc_switch_provider")]
    pub active_cc_switch_provider: String,

    /// 第二行显示内容："system"（↑↓ + CPU + 内存）或 "usage"
    /// （主用模型 + 5h/本周 token + cost）。
    #[serde(default = "default_row2_mode")]
    pub row2_mode: String,

    /// AI 用量数据刷新间隔（秒）。读 cc-switch SQLite，~毫秒级，
    /// 但 UI 也不需要秒级更新，默认 30s 足够。0 关闭。
    #[serde(default = "default_usage_refresh_interval")]
    pub usage_refresh_interval: u64,

    /// 5 小时滚动窗口的**请求次数**上限，用于浮窗百分比基准。
    /// cc-switch UI 也是按请求数算 % —— 之前用 USD 累计会因为
    /// total_cost_usd 是 API 列表价（非订阅价）而严重偏高。
    /// Anthropic Pro 默认 ~50 req/5h；Max ~250 req/5h；Team/Enterprise 更高。
    /// 0 = 不显示百分比，只显示绝对请求数。
    #[serde(default = "default_usage_5h_limit_requests")]
    pub usage_5h_limit_requests: u64,

    /// 7 天滚动窗口的请求次数上限。Pro ~1000 / Max ~5000，按需调整。
    #[serde(default = "default_usage_week_limit_requests")]
    pub usage_week_limit_requests: u64,
}

fn default_check_interval() -> u64 { 10 }
fn default_true() -> bool { true }
fn default_opacity() -> f32 { 0.85 }
fn default_position() -> String { "top-center".to_string() }
fn default_timeout() -> u64 { 60 }
fn default_max_retries() -> u32 { 3 }
fn default_monitor_interval() -> u64 { 2 }
fn default_proxy_interval() -> u64 { 30 }
fn default_model_refresh_interval() -> u64 { 5 }
fn default_geo_cache_ttl_hours() -> u64 { 24 * 7 }
fn default_geo_cache_max_entries() -> usize { 1000 }
fn default_idle_threshold() -> u64 { 15 * 60 }
fn default_idle_multiplier() -> u64 { 5 }
fn default_theme() -> String { "system".to_string() }
fn default_cc_switch_provider() -> String { "claude".to_string() }
fn default_row2_mode() -> String { "system".to_string() }
fn default_usage_refresh_interval() -> u64 { 30 }
// 默认按 Anthropic Pro：5h 50 条用户消息 / 7d 约 1000 条。
// Max 用户改大一档：250 / 5000。
fn default_usage_5h_limit_requests() -> u64 { 50 }
fn default_usage_week_limit_requests() -> u64 { 1000 }
fn default_hotkey_toggle() -> String { "ctrl+alt+h".to_string() }
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
            hotkey_quit: default_hotkey_quit(),
            timeout: default_timeout(),
            max_retries: default_max_retries(),
            proxy: None,
            enable_log: false,
            monitor_interval: default_monitor_interval(),
            proxy_check_interval: default_proxy_interval(),
            model_refresh_interval: default_model_refresh_interval(),
            mask_ip_in_log: true,
            mask_geo_in_log: true,
            geo_cache_enabled: true,
            geo_cache_ttl_hours: default_geo_cache_ttl_hours(),
            geo_cache_max_entries: default_geo_cache_max_entries(),
            idle_threshold_seconds: default_idle_threshold(),
            idle_multiplier: default_idle_multiplier(),
            geo_cross_check: true,
            theme: default_theme(),
            active_cc_switch_provider: default_cc_switch_provider(),
            row2_mode: default_row2_mode(),
            usage_refresh_interval: default_usage_refresh_interval(),
            usage_5h_limit_requests: default_usage_5h_limit_requests(),
            usage_week_limit_requests: default_usage_week_limit_requests(),
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
hotkey_quit = "ctrl+alt+shift+k"     # 退出程序

timeout = 60                # 请求超时（秒）
max_retries = 3             # 最大重试次数
enable_log = false          # 是否启用日志记录
# proxy = "socks5://127.0.0.1:1080"  # 可选代理

monitor_interval = 2        # 系统监控刷新间隔（秒）：CPU/内存/网速
proxy_check_interval = 30   # 代理检测间隔（秒）：注册表+PAC+端口+进程
model_refresh_interval = 5  # Claude 模型标签刷新间隔（秒），0=仅启动时读取一次

mask_ip_in_log = true             # 日志中将公网 IP 掩码为 1.2.x.x（浮窗不受影响）
mask_geo_in_log = true            # 日志中将归属地脱敏为哈希（浮窗不受影响）
geo_cache_enabled = true          # 启用归属地磁盘缓存
geo_cache_ttl_hours = 168         # 缓存有效期（小时），默认 7 天
geo_cache_max_entries = 1000      # LRU 上限，超过淘汰最老条目
idle_threshold_seconds = 900      # 用户空闲多少秒后降频；0 关闭
idle_multiplier = 5               # 空闲时所有轮询间隔的倍数
geo_cross_check = true            # 跨源比对国别，HTTPS 优先 ipwho.is

theme = "system"                            # 主题：system / light / dark
active_cc_switch_provider = "claude"        # 浮窗左上 tag 读哪个 cc-switch 工具

row2_mode = "system"                        # 第二行：system（↑↓+CPU+内存）/ usage（AI 用量）
usage_refresh_interval = 30                 # 读 cc-switch SQLite 用量的间隔（秒），0 关闭
usage_5h_limit_requests = 50                # 5h 滚动窗口**用户消息数**上限（Anthropic Pro ≈ 50 / Max ≈ 250）
usage_week_limit_requests = 1000            # 7d 滚动窗口用户消息数上限
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
