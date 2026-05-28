//! 公网 IP 抓取 + 日志脱敏工具。
//!
//! - 并发请求 3 个免费 IP 源（ipify / ip.sb / ifconfig.me），任一成功即返回；
//!   全失败时按"诊断价值"聚合失败原因（DNS > Connect > TLS > Timeout > ...）。
//! - `mask_ip` / `mask_geo` / `mask_proxy_url` 给日志路径用 —— 浮窗仍显
//!   示真值，只让磁盘日志变得不可反查用户隐私。
//! - 掩码开关是 `AtomicBool`，托盘菜单 / 设置对话框可运行时翻转。

use reqwest::Client;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::time;

// 三源并发，任一成功即返回。全部 HTTPS —— HTTP 明文在某些代理 / Clash 全局模式
// 下会被节点拒绝或被广告/反劫持规则误杀，导致"浏览器能上但本工具超时"。
//
// 已剔除：
//   - api.ip.sb        Cloudflare WAF 对劣质节点 IP 频繁挂 403 / 长挂起
//   - ifconfig.me      多数海外节点限速，且在部分代理出口被识别为爬虫源
//   - ipinfo.io/ip     未鉴权 ~1000/day 软限，10s 轮询 + 三源并发半小时内撞 429
// 当前选择（全部无显式 day-quota）：
//   - api.ipify.org    全球可达，HTTPS 稳，纯文本响应
//   - icanhazip.com    Cloudflare 维护，HTTPS，无显式 quota，纯文本响应
//   - ifconfig.co/ip   HTTPS，ifconfig.me 的现代替代，纯文本响应
const IP_SOURCES: &[&str] = &[
    "https://api.ipify.org",
    "https://icanhazip.com",
    "https://ifconfig.co/ip",
];

/// Runtime-toggleable mask flags. Default true (safest); the tray menu and
/// config loader can flip them. `AtomicBool` so the toggle takes effect on
/// the next tracing call without restart.
static MASK_IP_LOGS: AtomicBool = AtomicBool::new(true);
static MASK_GEO_LOGS: AtomicBool = AtomicBool::new(true);

pub fn set_mask_ip_logs(enabled: bool) {
    MASK_IP_LOGS.store(enabled, Ordering::Relaxed);
}

pub fn set_mask_geo_logs(enabled: bool) {
    MASK_GEO_LOGS.store(enabled, Ordering::Relaxed);
}

pub fn get_mask_ip_logs() -> bool {
    MASK_IP_LOGS.load(Ordering::Relaxed)
}

pub fn get_mask_geo_logs() -> bool {
    MASK_GEO_LOGS.load(Ordering::Relaxed)
}

/// Render a geo string (country/city/ISP) for logging. When masking is on,
/// non-empty values become a stable 8-char hash so the log can still tell
/// "did the city change between line A and line B?" without revealing where.
pub fn mask_geo(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    if !MASK_GEO_LOGS.load(Ordering::Relaxed) {
        return value.to_string();
    }
    // FNV-1a 64 — small, stable, no external dep needed.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in value.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("geo:{:08x}", h as u32)
}

/// Replace any embedded `user:password@` in a proxy URL with `***@`. Other
/// parts of the URL (scheme, host, port) pass through so log entries about
/// proxy config are still diagnosable.
pub fn mask_proxy_url(raw: &str) -> String {
    // Find scheme://
    let Some(scheme_end) = raw.find("://") else {
        return raw.to_string();
    };
    let body_start = scheme_end + 3;
    let after = &raw[body_start..];
    let Some(at_pos) = after.find('@') else {
        return raw.to_string();
    };
    format!("{}://***@{}", &raw[..scheme_end], &after[at_pos + 1..])
}

/// Return a log-safe rendering of an IP. v4 keeps the first two octets;
/// v6 keeps the first hextet. Non-IP strings pass through unchanged.
pub fn mask_ip(ip: &str) -> String {
    if !MASK_IP_LOGS.load(Ordering::Relaxed) {
        return ip.to_string();
    }
    match ip.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => {
            let o = v4.octets();
            format!("{}.{}.x.x", o[0], o[1])
        }
        Ok(IpAddr::V6(v6)) => {
            let s = v6.segments();
            format!("{:x}:x:x:x:x:x:x:x", s[0])
        }
        Err(_) => ip.to_string(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailReason {
    Timeout,
    Connect,
    Dns,
    Tls,
    Http(u16),
    Decode,
    Other,
}

impl FailReason {
    /// Short human-readable label suitable for the overlay's row 1.
    pub fn label(&self) -> String {
        match self {
            FailReason::Timeout => "超时".to_string(),
            FailReason::Connect => "无连接".to_string(),
            FailReason::Dns => "DNS 失败".to_string(),
            FailReason::Tls => "TLS 错误".to_string(),
            FailReason::Http(s) => format!("HTTP {}", s),
            FailReason::Decode => "响应无效".to_string(),
            FailReason::Other => "未知错误".to_string(),
        }
    }

    /// Sort key: lower number = more diagnostic, surface first when
    /// aggregating across multiple sources.
    fn priority(&self) -> u8 {
        match self {
            FailReason::Dns => 0,
            FailReason::Connect => 1,
            FailReason::Tls => 2,
            FailReason::Timeout => 3,
            FailReason::Http(_) => 4,
            FailReason::Decode => 5,
            FailReason::Other => 6,
        }
    }
}

pub enum IpFetchOutcome {
    Ok {
        ip: String,
        latency_ms: u64,
        #[allow(dead_code)]
        source: &'static str,
    },
    RateLimited,
    Failed(FailReason),
}

enum SourceResult {
    Ok(String, u64),
    RateLimited,
    Failed(FailReason),
}

fn classify_reqwest_error(e: &reqwest::Error) -> FailReason {
    let msg = e.to_string().to_lowercase();
    if e.is_timeout() {
        return FailReason::Timeout;
    }
    if msg.contains("dns") || msg.contains("resolve") || msg.contains("name") && msg.contains("look") {
        return FailReason::Dns;
    }
    if e.is_connect() {
        return FailReason::Connect;
    }
    if msg.contains("tls") || msg.contains("ssl") || msg.contains("certificate") || msg.contains("handshake") {
        return FailReason::Tls;
    }
    if let Some(status) = e.status() {
        return FailReason::Http(status.as_u16());
    }
    if e.is_decode() || e.is_body() {
        return FailReason::Decode;
    }
    FailReason::Other
}

pub async fn fetch_public_ip(client: &Client, timeout: Duration) -> IpFetchOutcome {
    let mut tasks = Vec::with_capacity(IP_SOURCES.len());
    for &url in IP_SOURCES {
        let client = client.clone();
        tasks.push((
            url,
            tokio::spawn(async move {
                let start = std::time::Instant::now();
                let result = time::timeout(timeout, async {
                    let resp = client
                        .get(url)
                        .header("User-Agent", "VpnMonitor/1.0")
                        .send()
                        .await?;
                    let status = resp.status().as_u16();
                    let text = resp.text().await?;
                    Ok::<(u16, String), reqwest::Error>((status, text))
                })
                .await;

                match result {
                    // Tokio outer timeout
                    Err(_) => SourceResult::Failed(FailReason::Timeout),
                    Ok(Ok((429, _))) => SourceResult::RateLimited,
                    Ok(Ok((status, text))) if (200..300).contains(&status) => {
                        let ip = text.trim().to_string();
                        let latency_ms = start.elapsed().as_millis() as u64;
                        if ip.parse::<IpAddr>().is_ok() {
                            SourceResult::Ok(ip, latency_ms)
                        } else {
                            SourceResult::Failed(FailReason::Decode)
                        }
                    }
                    Ok(Ok((status, _))) => SourceResult::Failed(FailReason::Http(status)),
                    Ok(Err(e)) => SourceResult::Failed(classify_reqwest_error(&e)),
                }
            }),
        ));
    }

    let mut saw_rate_limit = false;
    let mut reasons: Vec<FailReason> = Vec::new();
    // 顺序 await 每个 task；首个 Ok 返回时 abort 剩余 JoinHandle ——
    // tokio 中 drop JoinHandle 不会中止任务，剩余 HTTPS 请求会跑完，
    // 这对带 day-quota 的源等同于每轮白白消耗一次配额。
    let total = tasks.len();
    for idx in 0..total {
        let url = tasks[idx].0;
        let result = (&mut tasks[idx].1).await;
        match result {
            Ok(SourceResult::Ok(ip, latency_ms)) => {
                tracing::info!(
                    "Public IP {} fetched from {} in {}ms",
                    mask_ip(&ip), url, latency_ms
                );
                for (_, t) in &tasks[idx + 1..] {
                    t.abort();
                }
                return IpFetchOutcome::Ok {
                    ip,
                    source: url,
                    latency_ms,
                };
            }
            Ok(SourceResult::RateLimited) => {
                tracing::warn!("IP source {} returned 429", url);
                saw_rate_limit = true;
            }
            Ok(SourceResult::Failed(r)) => {
                tracing::warn!("IP source {} failed: {}", url, r.label());
                reasons.push(r);
            }
            Err(_) => {
                reasons.push(FailReason::Other);
            }
        }
    }

    if saw_rate_limit {
        return IpFetchOutcome::RateLimited;
    }

    // Aggregate: pick the most diagnostic reason across sources.
    reasons.sort_by_key(|r| r.priority());
    let reason = reasons.into_iter().next().unwrap_or(FailReason::Other);
    tracing::warn!("All IP sources failed: {}", reason.label());
    IpFetchOutcome::Failed(reason)
}
