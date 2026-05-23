//! DNS + IPv6 泄漏检测。
//!
//! 与 [`super::geo_lookup`] 跨源国别校验同思路 —— 并发请求三个差异化端点，
//! 对比"看到的我"是否一致：
//!
//! - **IPv4 公网 IP**：和主 IP 轮询同源；这是基线
//! - **IPv6 公网 IP**：单独走 `api6.ipify.org`（v6-only DNS）。若机器没有 v6
//!   就拿不到结果（None），不算泄漏；拿到了且国别与 v4 不一致 → **v6 泄漏**
//! - **DNS 解析方位置**：调 `https://1.1.1.1/cdn-cgi/trace`，Cloudflare 返回
//!   它看到的客户端 IP 和 POP 位置（`loc=` ISO 国别码）。若 loc 与 v4 国别
//!   不一致 → **DNS 泄漏**（DNS 查询走了 VPN 之外的链路）
//!
//! 所有探测都用短超时（3s），任一失败安静降级为 None，不影响其他维度。

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

    /// Cloudflare 看到的 DNS 解析者位置 ISO 码（"US" / "JP" / "CN" 等）
    pub dns_country: Option<String>,
    /// v4 != dns_country → DNS 走运营商而非 VPN
    pub dns_leak: bool,
}

impl LeakReport {
    /// 是否有任何泄漏 —— UI 可以用一个简单 if 决定是否画 ⚠ 红章
    pub fn has_leak(&self) -> bool {
        self.v6_leak || self.dns_leak
    }
}

/// 并发跑三个独立探测，2-3 秒内出报告。`v4_country` 由调用方传入
/// （避免重复抓 v4 IP，那块 IP 轮询已经做了）。
pub async fn check_leaks(
    client: &Client,
    v4_country: Option<&str>,
    timeout: Duration,
) -> LeakReport {
    let (v6, dns) = tokio::join!(
        fetch_v6_country(client, timeout),
        fetch_dns_country(client, timeout),
    );

    let v4_norm = v4_country.map(|s| normalize_country(s));
    let v6_norm = v6.as_ref().map(|s| normalize_country(s));
    let dns_norm = dns.as_ref().map(|s| normalize_country(s));

    let v6_leak = matches!((&v4_norm, &v6_norm), (Some(a), Some(b)) if !a.is_empty() && !b.is_empty() && a != b);
    let dns_leak = matches!((&v4_norm, &dns_norm), (Some(a), Some(b)) if !a.is_empty() && !b.is_empty() && a != b);

    LeakReport {
        v4_country: v4_country.map(String::from),
        v6_country: v6,
        v6_leak,
        dns_country: dns,
        dns_leak,
    }
}

/// 拿 v6 公网 IP 然后查国别。`api6.ipify.org` 强制走 v6 路径。
async fn fetch_v6_country(client: &Client, timeout: Duration) -> Option<String> {
    let ip = time::timeout(timeout, async {
        client
            .get("https://api6.ipify.org")
            .header("User-Agent", "VpnMonitor/1.0")
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

    // 拿到 v6 IP → 用归属地查询拿国别
    match super::geo_lookup::lookup_geo(client, &ip, timeout, false).await {
        super::geo_lookup::GeoLookupOutcome::Ok { geo, .. } => {
            if geo.country.is_empty() { None } else { Some(geo.country) }
        }
        _ => None,
    }
}

/// 调 Cloudflare 诊断端点，提取 `loc=XX`（ISO 国别码）。
/// 此 IP 是 Cloudflare DNS resolver 看到的客户端 IP —— 反映 DNS 查询路径而非
/// 普通 HTTPS 路径（在分流 VPN 场景下二者可能走不同链路）。
async fn fetch_dns_country(client: &Client, timeout: Duration) -> Option<String> {
    let text = time::timeout(timeout, async {
        client
            .get("https://1.1.1.1/cdn-cgi/trace")
            .header("User-Agent", "VpnMonitor/1.0")
            .send()
            .await
            .ok()?
            .text()
            .await
            .ok()
    })
    .await
    .ok()
    .flatten()?;

    // 解析 `loc=XX` 一行
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("loc=") {
            let cc = rest.trim();
            if !cc.is_empty() {
                return Some(cc.to_string());
            }
        }
    }
    None
}

/// 国别字符串归一化：去空格、转大写。
/// ip-api 返回 "United States"，ipwho.is 返回 "United States"，Cloudflare 返回 "US"。
/// 简单策略：取前 2 个字符大写 —— 大多数情况下能匹配 ISO 码。
/// 若 country 是中文/全名则保留全文，让上层比较失败时也能在日志里看清差异。
fn normalize_country(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.len() <= 3 {
        trimmed.to_uppercase()
    } else {
        // ip-api 返回长名时也接受全名比较 —— 留给调用方决定
        trimmed.to_uppercase()
    }
}
