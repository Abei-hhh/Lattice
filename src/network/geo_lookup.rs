use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;
use tokio::time;

#[derive(Debug, Clone, Default)]
pub struct GeoInfo {
    pub country: String,
    pub region: String,
    pub city: String,
    pub isp: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GeoFailReason {
    Timeout,
    Network,
    Private,
    Invalid,
    Decode,
    Other,
}

impl GeoFailReason {
    /// Short label for the overlay (dim color, next to the missing city).
    pub fn label(&self) -> String {
        match self {
            GeoFailReason::Timeout => "超时".to_string(),
            GeoFailReason::Network => "网络".to_string(),
            GeoFailReason::Private => "私有段".to_string(),
            GeoFailReason::Invalid => "无效".to_string(),
            GeoFailReason::Decode => "解析失败".to_string(),
            GeoFailReason::Other => "未知".to_string(),
        }
    }

    /// Lower number = more diagnostic; used to pick the best reason across
    /// providers when both fail.
    fn priority(&self) -> u8 {
        match self {
            GeoFailReason::Private => 0,
            GeoFailReason::Invalid => 1,
            GeoFailReason::Decode => 2,
            GeoFailReason::Network => 3,
            GeoFailReason::Timeout => 4,
            GeoFailReason::Other => 5,
        }
    }
}

pub enum GeoLookupOutcome {
    Ok(GeoInfo),
    RateLimited,
    Failed(GeoFailReason),
}

enum ProviderOutcome {
    Ok(GeoInfo),
    RateLimited,
    Failed(GeoFailReason),
}

#[derive(Deserialize)]
struct IpApiResponse {
    status: String,
    message: Option<String>,
    country: Option<String>,
    #[serde(rename = "regionName")]
    region_name: Option<String>,
    city: Option<String>,
    isp: Option<String>,
}

#[derive(Deserialize)]
struct IpWhoIsResponse {
    success: Option<bool>,
    message: Option<String>,
    country: Option<String>,
    region: Option<String>,
    city: Option<String>,
    connection: Option<IpWhoIsConnection>,
}

#[derive(Deserialize)]
struct IpWhoIsConnection {
    isp: Option<String>,
}

/// Map a provider-supplied error message to a typed reason. Both ip-api.com
/// and ipwho.is use similar English phrasing for the same conditions.
fn classify_api_message(msg: &str) -> GeoFailReason {
    let m = msg.to_lowercase();
    if m.contains("private") || m.contains("reserved") {
        GeoFailReason::Private
    } else if m.contains("invalid") {
        GeoFailReason::Invalid
    } else {
        GeoFailReason::Other
    }
}

/// Classify a reqwest transport error into the coarser geo-side categories
/// (we don't surface HTTP status codes here — they're rarely useful for
/// users compared to "网络"/"超时").
fn classify_reqwest_error(e: &reqwest::Error) -> GeoFailReason {
    if e.is_timeout() {
        return GeoFailReason::Timeout;
    }
    let s = e.to_string().to_lowercase();
    if s.contains("dns") || s.contains("resolve") || s.contains("tls") || s.contains("ssl") {
        return GeoFailReason::Network;
    }
    if e.is_connect() || e.is_request() {
        return GeoFailReason::Network;
    }
    if e.is_decode() || e.is_body() {
        return GeoFailReason::Decode;
    }
    GeoFailReason::Other
}

pub async fn lookup_geo(client: &Client, ip: &str, timeout: Duration) -> GeoLookupOutcome {
    let mut saw_rate_limit = false;
    let mut reasons: Vec<GeoFailReason> = Vec::new();

    match lookup_ip_api(client, ip, timeout).await {
        ProviderOutcome::Ok(g) => return GeoLookupOutcome::Ok(g),
        ProviderOutcome::RateLimited => saw_rate_limit = true,
        ProviderOutcome::Failed(r) => reasons.push(r),
    }

    match lookup_ipwho_is(client, ip, timeout).await {
        ProviderOutcome::Ok(g) => return GeoLookupOutcome::Ok(g),
        ProviderOutcome::RateLimited => saw_rate_limit = true,
        ProviderOutcome::Failed(r) => reasons.push(r),
    }

    if saw_rate_limit {
        tracing::warn!("All geo providers rate-limited for {}", ip);
        return GeoLookupOutcome::RateLimited;
    }

    reasons.sort_by_key(|r| r.priority());
    let reason = reasons.into_iter().next().unwrap_or(GeoFailReason::Other);
    tracing::warn!("All geo providers failed for {}: {}", ip, reason.label());
    GeoLookupOutcome::Failed(reason)
}

async fn lookup_ip_api(client: &Client, ip: &str, timeout: Duration) -> ProviderOutcome {
    let url = format!("http://ip-api.com/json/{}?lang=zh-CN", ip);
    let result = time::timeout(timeout, async {
        client
            .get(&url)
            .header("User-Agent", "VpnMonitor/1.0")
            .send()
            .await
    })
    .await;

    let resp = match result {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            tracing::warn!("ip-api.com request failed for {}: {}", ip, e);
            return ProviderOutcome::Failed(classify_reqwest_error(&e));
        }
        Err(_) => {
            tracing::warn!("ip-api.com timed out for {}", ip);
            return ProviderOutcome::Failed(GeoFailReason::Timeout);
        }
    };

    if resp.status().as_u16() == 429 {
        return ProviderOutcome::RateLimited;
    }

    match resp.json::<IpApiResponse>().await {
        Ok(data) if data.status == "success" => ProviderOutcome::Ok(GeoInfo {
            country: data.country.unwrap_or_default(),
            region: data.region_name.unwrap_or_default(),
            city: data.city.unwrap_or_default(),
            isp: data.isp.unwrap_or_default(),
        }),
        Ok(data) => {
            let msg = data.message.unwrap_or_default();
            let lower = msg.to_lowercase();
            if lower.contains("rate") || lower.contains("quota") || lower.contains("limit") {
                ProviderOutcome::RateLimited
            } else {
                tracing::warn!("ip-api.com non-success for {}: {}", ip, msg);
                ProviderOutcome::Failed(classify_api_message(&msg))
            }
        }
        Err(e) => {
            tracing::warn!("ip-api.com parse error: {}", e);
            ProviderOutcome::Failed(GeoFailReason::Decode)
        }
    }
}

async fn lookup_ipwho_is(client: &Client, ip: &str, timeout: Duration) -> ProviderOutcome {
    let url = format!("https://ipwho.is/{}", ip);
    let result = time::timeout(timeout, async {
        client
            .get(&url)
            .header("User-Agent", "VpnMonitor/1.0")
            .send()
            .await
    })
    .await;

    let resp = match result {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            tracing::warn!("ipwho.is request failed for {}: {}", ip, e);
            return ProviderOutcome::Failed(classify_reqwest_error(&e));
        }
        Err(_) => {
            tracing::warn!("ipwho.is timed out for {}", ip);
            return ProviderOutcome::Failed(GeoFailReason::Timeout);
        }
    };

    if resp.status().as_u16() == 429 {
        return ProviderOutcome::RateLimited;
    }

    match resp.json::<IpWhoIsResponse>().await {
        Ok(data) if data.success.unwrap_or(false) => ProviderOutcome::Ok(GeoInfo {
            country: data.country.unwrap_or_default(),
            region: data.region.unwrap_or_default(),
            city: data.city.unwrap_or_default(),
            isp: data.connection.and_then(|c| c.isp).unwrap_or_default(),
        }),
        Ok(data) => {
            let msg = data.message.unwrap_or_default();
            let lower = msg.to_lowercase();
            if lower.contains("rate") || lower.contains("quota") || lower.contains("limit") {
                ProviderOutcome::RateLimited
            } else {
                tracing::warn!("ipwho.is non-success for {}: {}", ip, msg);
                ProviderOutcome::Failed(classify_api_message(&msg))
            }
        }
        Err(e) => {
            tracing::warn!("ipwho.is parse error: {}", e);
            ProviderOutcome::Failed(GeoFailReason::Decode)
        }
    }
}
