use reqwest::Client;
use std::net::IpAddr;
use std::time::Duration;
use tokio::time;

const IP_SOURCES: &[&str] = &[
    "https://api.ipify.org",
    "https://api.ip.sb/ip",
    "https://ifconfig.me/ip",
];

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
    for (url, task) in tasks {
        match task.await {
            Ok(SourceResult::Ok(ip, latency_ms)) => {
                tracing::info!("Public IP {} fetched from {} in {}ms", ip, url, latency_ms);
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
