//! 代理工具本地 HTTP API 集成（Clash / Mihomo / sing-box）。
//!
//! 这些代理工具通常在本地起一个 RESTful controller，可通过 HTTP 查询：
//! - 当前选中节点名
//! - 节点延迟
//! - 流量统计（已设计但本期不接，未来 detailed 视图可加）
//!
//! 设计原则：
//! - **不强求**：探测失败安静返回 None，浮窗仍正常工作
//! - **零配置自动发现**：依次探测最常见的本地端口（9090 Clash/Mihomo
//!   默认；9001 / 9000 也有一些 fork 在用），第一个 200 OK 即认定
//! - **不阻塞主轮询**：调用方自己起独立 task 异步刷新

use std::time::Duration;

use reqwest::Client;
use serde::{Deserialize, Serialize};

/// 探测到的代理工具状态。所有字段都是 Option 因为各工具支持的端点不同。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProxyRpcSnapshot {
    /// 被识别的工具名（"Clash" / "Mihomo" / "sing-box"），探测失败为空
    pub tool: String,
    /// 当前选中节点名（如 "🇯🇵 Tokyo-01"）
    pub current_node: Option<String>,
    /// 选中节点的延迟 ms（若工具提供）
    pub current_latency_ms: Option<u32>,
    /// 该 selector group 的全部可用节点数
    pub total_nodes: Option<u32>,
}

impl ProxyRpcSnapshot {
    pub fn is_available(&self) -> bool {
        !self.tool.is_empty() && self.current_node.is_some()
    }
}

/// 常见 controller 端口（按命中概率从高到低）
const CANDIDATE_BASES: &[&str] = &[
    "http://127.0.0.1:9090", // Clash / Mihomo 默认
    "http://127.0.0.1:9001", // 部分 fork
    "http://127.0.0.1:6170", // sing-box clash-api compat
];

/// 探测本地代理 RPC，返回快照。短超时 (1s) —— 这是本地回环，
/// 慢说明工具有问题，没必要等太久。
pub async fn detect(client: &Client) -> Option<ProxyRpcSnapshot> {
    let timeout = Duration::from_secs(1);
    for &base in CANDIDATE_BASES {
        if let Some(snap) = try_clash_api(client, base, timeout).await {
            return Some(snap);
        }
    }
    None
}

/// Clash / Mihomo / sing-box clash-api 通用查询：
/// 1. GET `/version` 验证存在 + 识别工具名
/// 2. GET `/proxies` 拿全部 proxy 组
/// 3. 找出 `type == "Selector"` 的组，取其 `now` 字段 = 当前选中节点
async fn try_clash_api(client: &Client, base: &str, timeout: Duration) -> Option<ProxyRpcSnapshot> {
    // 1. version 探测（也鉴别工具种类）
    let version_url = format!("{}/version", base);
    let version_resp = tokio::time::timeout(timeout, client.get(&version_url).send())
        .await
        .ok()?
        .ok()?;
    if !version_resp.status().is_success() {
        return None;
    }
    let version_json: serde_json::Value = version_resp.json().await.ok()?;
    let tool = identify_tool(&version_json);

    // 2. /proxies
    let proxies_url = format!("{}/proxies", base);
    let proxies_resp = tokio::time::timeout(timeout, client.get(&proxies_url).send())
        .await
        .ok()?
        .ok()?;
    let proxies: ProxiesResponse = proxies_resp.json().await.ok()?;

    // 3. 找 Selector 组的 now
    let (current_node, current_latency_ms, total_nodes) = pick_active_selector(&proxies);

    Some(ProxyRpcSnapshot {
        tool,
        current_node,
        current_latency_ms,
        total_nodes,
    })
}

#[derive(Deserialize)]
struct ProxiesResponse {
    proxies: std::collections::HashMap<String, ProxyEntry>,
}

#[derive(Deserialize)]
struct ProxyEntry {
    #[serde(default, rename = "type")]
    proxy_type: String,
    #[serde(default)]
    now: String,
    #[serde(default)]
    all: Vec<String>,
    #[serde(default)]
    history: Vec<HistoryEntry>,
}

#[derive(Deserialize)]
struct HistoryEntry {
    #[serde(default)]
    delay: u32,
}

fn identify_tool(version: &serde_json::Value) -> String {
    // Clash core 返回 {"premium":"...", "version":"..."}
    // Mihomo 返回 {"meta":true, "version":"..."}
    // sing-box clash-api 返回 {"version":"sing-box ..."}
    if let Some(v) = version.get("version").and_then(|x| x.as_str()) {
        if v.to_lowercase().contains("sing-box") {
            return "sing-box".to_string();
        }
    }
    if version
        .get("meta")
        .and_then(|x| x.as_bool())
        .unwrap_or(false)
    {
        return "Mihomo".to_string();
    }
    "Clash".to_string()
}

/// 优先选名字含 "select" / "代理" / "节点" 的组；否则取第一个 Selector。
/// 返回 (节点名, 该节点最新延迟, 组内节点总数)
fn pick_active_selector(p: &ProxiesResponse) -> (Option<String>, Option<u32>, Option<u32>) {
    let preferred_names = ["select", "proxy", "代理", "节点", "GLOBAL"];

    let mut candidate: Option<&ProxyEntry> = None;
    for name in preferred_names {
        if let Some(e) = p.proxies.get(name) {
            if e.proxy_type.eq_ignore_ascii_case("Selector") && !e.now.is_empty() {
                candidate = Some(e);
                break;
            }
        }
    }
    if candidate.is_none() {
        candidate = p
            .proxies
            .values()
            .find(|e| e.proxy_type.eq_ignore_ascii_case("Selector") && !e.now.is_empty());
    }

    let entry = match candidate {
        Some(e) => e,
        None => return (None, None, None),
    };

    // 当前节点的延迟从 history 末尾取
    let now_node = entry.now.clone();
    let latency = p
        .proxies
        .get(&now_node)
        .and_then(|e| e.history.last())
        .map(|h| h.delay)
        .filter(|d| *d > 0);

    (Some(now_node), latency, Some(entry.all.len() as u32))
}
