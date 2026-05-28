//! 浮窗 UI 状态持久化。
//!
//! 为什么独立成文件而不写 `config.toml`：用户每次拖动都要刷盘，频繁覆盖
//! 主配置会丢注释、丢顺序。这里用一个独立的小 JSON 文件，写错也不影响
//! 主配置。文件路径：`%APPDATA%\Lattice\overlay_state.json`。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// 见模块文档。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OverlayPersistedState {
    /// Last known window top-left in screen coordinates. `None` means
    /// "fall back to the auto-centered position on the current monitor".
    #[serde(default)]
    pub x: Option<i32>,
    #[serde(default)]
    pub y: Option<i32>,
    /// When true, the overlay ignores left-click drags. Toggleable from
    /// the tray menu; persisted so the choice survives restarts.
    #[serde(default)]
    pub locked: bool,
}

pub fn default_state_path() -> Option<PathBuf> {
    let data = dirs::data_dir()?;
    Some(data.join("Lattice").join("overlay_state.json"))
}

pub fn load() -> OverlayPersistedState {
    let Some(path) = default_state_path() else {
        return OverlayPersistedState::default();
    };
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return OverlayPersistedState::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

pub fn save(state: &OverlayPersistedState) {
    let Some(path) = default_state_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(serialized) = serde_json::to_string(state) else {
        return;
    };
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, serialized).is_err() {
        return;
    }
    let _ = std::fs::rename(&tmp, &path);
}
