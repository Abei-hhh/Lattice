//! IPv6 泄漏检测。
//!
//! 与 [`super::geo_lookup`] 跨源国别校验同思路 —— 对比"代理出口看到的我"是否
//! 和"v6 直连看到的我"在同一个国家：
//!
//! - **IPv4 公网 IP**：和主 IP 轮询同源；这是基线
//! - **IPv6 公网 IP**：单独走 `api6.ipify.org`（v6-only DNS）。若机器没有 v6
//!   就拿不到结果（None），不算泄漏；拿到了且国别与 v4 不一致 → **v6 泄漏**
//!
//! ## 已删除 DNS 泄漏维度（2026-06-11）
//!
//! 历史版本通过 `https://1.1.1.1/cdn-cgi/trace` 的 `loc=XX` 字段做"DNS 泄漏"
//! 检测，但这个端点返回的 loc 是 **Cloudflare 看到的本次 HTTPS 请求源 IP** 的
//! 位置，**不是 DNS resolver 的位置**。在分流模式（规则模式）下，几乎所有代理
//! 工具都把 1.1.1.1 当作国内可直连段处理 → trace 走直连 → loc=CN，而 v4
//! 主 IP 走代理 → 假阳性"DNS 泄漏"。真正的 DNS 泄漏检测需要随机子域名 +
//! 服务端配合（dnsleaktest.com 模式），本工具暂不实现。
//!
//! 所有探测都用短超时（3s），任一失败安静降级为 None。

use std::time::Duration;

use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::time;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LeakReport {
    /// v4 出口 IP 对应的国别（与主 IP 轮询一致）。None = 检测失败
    pub v4_country: Option<String>,
    /// v6 出口 IP 国别。None = 机器无 v6 / v6 探测失败
    pub v6_country: Option<String>,
    /// v4 != v6 国别 → 提示用户 v6 流量绕过了 VPN
    pub v6_leak: bool,
}

impl LeakReport {
    /// 是否有任何泄漏 —— UI 可以用一个简单 if 决定是否画 ⚠ 红章
    pub fn has_leak(&self) -> bool {
        self.v6_leak
    }
}

/// 跑 v6 国别探测（DNS 维度已删除）。`v4_country_code` 由调用方传入
/// （避免重复抓 v4 IP），**必须是 ISO 3166-1 alpha-2 二字母码**
/// （"US" / "JP" / "CN"），否则会与 v6 ISO 比较失败。
pub async fn check_leaks(
    client: &Client,
    v4_country_code: Option<&str>,
    timeout: Duration,
) -> LeakReport {
    let v6 = fetch_v6_country(client, timeout).await;

    let v4_norm = v4_country_code.map(normalize_country);
    let v6_norm = v6.as_ref().map(|s| normalize_country(s));

    let v6_leak = matches!((&v4_norm, &v6_norm), (Some(a), Some(b)) if !a.is_empty() && !b.is_empty() && a != b);

    LeakReport {
        v4_country: v4_country_code.map(String::from),
        v6_country: v6,
        v6_leak,
    }
}

/// 拿 v6 公网 IP 然后查国别。`api6.ipify.org` 强制走 v6 路径。
async fn fetch_v6_country(client: &Client, timeout: Duration) -> Option<String> {
    let ip = time::timeout(timeout, async {
        client
            .get("https://api6.ipify.org")
            .header("User-Agent", "Lattice/1.0")
            .send()
            .await
            .ok()?
            .text()
            .await
            .ok()
    })
    .await
    .ok()
    .flatten()?
    .trim()
    .to_string();

    if ip.is_empty() || !ip.contains(':') {
        return None; // 不是合法 v6 → 该机器无 v6
    }

    // 拿到 v6 IP → 用归属地查询拿 ISO 码（用 country_code 而非 country 全名，
    // 后续与 v4 country_code 直接做 ISO 对 ISO 比较）。
    match super::geo_lookup::lookup_geo(client, &ip, timeout, false).await {
        super::geo_lookup::GeoLookupOutcome::Ok { geo, .. } => {
            if geo.country_code.is_empty() { None } else { Some(geo.country_code) }
        }
        _ => None,
    }
}

/// ISO 码归一化：去空格、转大写。
fn normalize_country(s: &str) -> String {
    s.trim().to_uppercase()
}
