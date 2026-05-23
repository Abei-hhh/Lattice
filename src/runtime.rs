//! 运行时可热切换的布尔标志位集合。
//!
//! 由 IP 轮询任务 / 监控线程 / window_proc / 托盘菜单共享。每个字段都是
//! `AtomicBool`，托盘菜单翻转后下一次轮询 / 重绘自动读到新值，无需重启。
//!
//! 设计上只放 bool —— 任何数值类（间隔 / 阈值）仍走 `AppConfig` + 磁盘
//! `config.toml`，由"高级设置..."对话框编辑。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

/// 见模块文档。
pub struct RuntimeFlags {
    /// 启用归属地磁盘缓存。关掉后 IP 轮询既不查也不写缓存。
    pub geo_cache_enabled: AtomicBool,
    /// HTTPS 与 HTTP 双源跨源校验：开启时等两个 provider，HTTPS 优先并
    /// 在国别不一致时给 UI 警告；关掉退回竞速取最快。
    pub geo_cross_check: AtomicBool,
    /// 浮窗是否锁定位置。锁定时 WM_NCHITTEST 不再把 HTCLIENT 提升为 HTCAPTION，
    /// 也就无法拖动。锁定状态会写到 overlay_state.json 持久化。
    pub overlay_locked: AtomicBool,
    /// 浮窗当前是否可见（菜单切换 / 全局热键都会改它）。
    pub overlay_visible: AtomicBool,
    /// 鼠标穿透 = WS_EX_TRANSPARENT。运行时切换通过 SetWindowLongPtr + SWP_FRAMECHANGED。
    pub click_through: AtomicBool,

    /// 主题模式："system" / "light" / "dark"。RwLock 因为是字符串。
    /// 设置对话框切换后下一次 WM_PAINT / WM_SETTINGCHANGE 读到新值。
    pub theme_mode: Arc<RwLock<String>>,

    /// 浮窗左上 tag 显示哪个 cc-switch 工具的当前模型。
    /// 切换后下一次 model refresh task tick 即生效。
    pub active_cc_switch_provider: Arc<RwLock<String>>,
}

impl RuntimeFlags {
    pub fn from_config(cfg: &crate::config::AppConfig, persisted_locked: bool) -> Arc<Self> {
        Arc::new(Self {
            geo_cache_enabled: AtomicBool::new(cfg.geo_cache_enabled),
            geo_cross_check: AtomicBool::new(cfg.geo_cross_check),
            overlay_locked: AtomicBool::new(persisted_locked),
            overlay_visible: AtomicBool::new(true),
            click_through: AtomicBool::new(cfg.click_through),
            theme_mode: Arc::new(RwLock::new(cfg.theme.clone())),
            active_cc_switch_provider: Arc::new(RwLock::new(
                cfg.active_cc_switch_provider.clone(),
            )),
        })
    }

    pub fn toggle(b: &AtomicBool) -> bool {
        let new = !b.load(Ordering::Relaxed);
        b.store(new, Ordering::Relaxed);
        new
    }
}
