//! cc-switch 多源读取。
//!
//! cc-switch (https://github.com/farion1231/cc-switch) 是 AI CLI 厂商切换器，
//! 在 `~/.cc-switch/settings.json` 里通过 `currentProvider<Tool>` 字段记录
//! 每个工具当前选中的 provider，例如：
//! ```json
//! { "currentProviderClaude": "claude-official",
//!   "currentProviderCodex": "codex-openai",
//!   "currentProviderGemini": "gemini-google" }
//! ```
//!
//! 本模块两个职责：
//! 1. `detect_available_sources` —— 扫 settings.json，列出所有有 `currentProvider*`
//!    字段的工具（用于设置对话框的 radio 列表）
//! 2. `read_label(source)` —— 给定工具名返回浮窗左上要显示的友好字串
//!    （Claude 还会进一步读 `~/.claude/settings.json` 的 env.ANTHROPIC_MODEL）

use std::collections::HashMap;
use std::path::PathBuf;

use serde_json::Value;

/// cc-switch 支持的工具白名单 —— 用户即使没装也能在 UI 里看到，
/// 切到没装的工具时浮窗显示工具名占位（如 "Gemini"）。
/// 顺序就是设置对话框 radio 的显示顺序。
pub const KNOWN_TOOLS: &[&str] = &[
    "claude", "codex", "gemini", "opencode", "openclaw", "hermes",
];

fn cc_switch_settings_path() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    Some(home.join(".cc-switch").join("settings.json"))
}

fn cc_switch_db_path() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    Some(home.join(".cc-switch").join("cc-switch.db"))
}

/// cc-switch 是否"安装过"——靠数据库文件存在判定。
/// 进程是否在跑由 binary 端（用 sysinfo）补，core 不依赖 OS-specific 进程枚举。
/// 完整可用性 = files_present AND process_running，写入 RuntimeFlags.cc_switch_available。
pub fn files_present() -> bool {
    cc_switch_db_path().map(|p| p.exists()).unwrap_or(false)
}

fn claude_settings_path() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    Some(home.join(".claude").join("settings.json"))
}

/// 读 ~/.cc-switch/settings.json 一次，扫所有 `currentProvider<Tool>` 字段，
/// 返回工具名小写列表（claude / codex / gemini / ...）。
/// 即使文件不存在或解析失败也返回空 vec，调用方需 fallback 到 KNOWN_TOOLS。
pub fn detect_available_sources() -> Vec<String> {
    let Some(path) = cc_switch_settings_path() else {
        return Vec::new();
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(val) = serde_json::from_str::<Value>(&content) else {
        return Vec::new();
    };
    let Some(obj) = val.as_object() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for key in obj.keys() {
        if let Some(rest) = key.strip_prefix("currentProvider") {
            if !rest.is_empty() {
                out.push(rest.to_lowercase());
            }
        }
    }
    out
}

/// 读 ~/.cc-switch/settings.json 里指定工具的 provider id。
/// 例如 source = "claude" 读 `currentProviderClaude` 字段。
fn read_provider_id(source: &str) -> Option<String> {
    let path = cc_switch_settings_path()?;
    let content = std::fs::read_to_string(path).ok()?;
    let val: Value = serde_json::from_str(&content).ok()?;
    let key = format!("currentProvider{}", title_case(source));
    val.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// 读 ~/.claude/settings.json `env.ANTHROPIC_MODEL` —— Claude 专属优先级。
/// cc-switch 切换到第三方 provider 时会把实际模型 ID 写到这里。
fn read_anthropic_model_env() -> Option<String> {
    let path = claude_settings_path()?;
    let content = std::fs::read_to_string(path).ok()?;
    let val: Value = serde_json::from_str(&content).ok()?;
    val.get("env")
        .and_then(|e| e.get("ANTHROPIC_MODEL"))
        .and_then(|m| m.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// 给定 source（小写工具名），返回浮窗左上 tag 的友好字串。
/// 三层 fallback：
///   1. Claude 特殊：先看 env.ANTHROPIC_MODEL（cc-switch 同步过来的真实模型）
///   2. 读 cc-switch settings.json 里该工具的 currentProvider 字段值
///      → 套友好名映射（如 "claude-official" → "Claude Official"）
///   3. 都没有时 fallback 到工具名首字母大写（"Gemini" / "Codex"）
pub fn read_label(source: &str) -> String {
    if source == "claude" {
        if let Some(m) = read_anthropic_model_env() {
            return m;
        }
    }
    if let Some(id) = read_provider_id(source) {
        return friendly_provider_label(source, &id);
    }
    title_case(source)
}

/// 把 provider id 映射到人类可读的标签。
/// 已知约定：`{tool}-official` → "{Tool} Official"；
/// UUID 形态（自定义 provider） → 工具名兜底。
fn friendly_provider_label(source: &str, id: &str) -> String {
    let lower = id.to_lowercase();
    let tool_name = title_case(source);
    let well_known: HashMap<&str, &str> = [
        ("claude-official", "Claude Official"),
        ("codex-openai", "Codex (OpenAI)"),
        ("gemini-google", "Gemini Google"),
    ]
    .into_iter()
    .collect();
    if let Some(label) = well_known.get(lower.as_str()) {
        return (*label).to_string();
    }
    if lower.ends_with("-official") {
        return format!("{} Official", tool_name);
    }
    // UUID 形态（cc-switch 自定义 provider 用 uuid）—— 8 字符 + 4 + ... = 36 字符或带 4 个 '-'
    if id.len() >= 32 && id.matches('-').count() >= 4 {
        return tool_name;
    }
    // 其它形态原样回显，但加工具名前缀让用户看得懂
    format!("{} · {}", tool_name, id)
}

fn title_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for (i, c) in s.chars().enumerate() {
        if i == 0 {
            out.extend(c.to_uppercase());
        } else {
            out.push(c);
        }
    }
    out
}
