//! vpn-monitor-core —— 平台无关的核心 crate。
//!
//! 这个 crate 把可以在 Windows / Linux / macOS / Android / iOS 上不加修改运行的
//! 所有逻辑集中起来，便于未来支持多端时各平台只写 GUI 壳。
//!
//! 现包含：
//! - [`network`] —— 公网 IP 抓取、归属地查询、磁盘 LRU 缓存
//! - [`config`] —— TOML 配置文件读取 + AppConfig 结构
//! - [`cc_switch`] —— 通用 cc-switch 多源（claude / codex / gemini / opencode / hermes / openclaw）读取
//! - [`runtime`] —— 跨线程共享的运行时标志位（AtomicBool + RwLock<String>）
//!
//! 未包含（仍在 binary crate 里）：
//! - Win32 / GUI 渲染
//! - 平台特定的代理检测（注册表 ProxyEnable / PAC URL）、空闲探测（GetLastInputInfo）、单实例守卫等
//!   —— 未来在 platform crate 抽 trait

pub mod cc_switch;
pub mod config;
pub mod network;
pub mod proxy_rpc;
pub mod runtime;
pub mod usage;
