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

pub enum GeoLookupOutcome {
    Ok(GeoInfo),
    RateLimited,
    Failed,
}

enum ProviderOutcome {
    Ok(GeoInfo),
    RateLimited,
    Failed,
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

pub async fn lookup_geo(client: &Client, ip: &str, timeout: Duration) -> GeoLookupOutcome {
    let mut saw_rate_limit = false;

    match lookup_ip_api(client, ip, timeout).await {
        ProviderOutcome::Ok(g) => return GeoLookupOutcome::Ok(g),
        ProviderOutcome::RateLimited => saw_rate_limit = true,
        ProviderOutcome::Failed => {}
    }

    match lookup_ipwho_is(client, ip, timeout).await {
        ProviderOutcome::Ok(g) => return GeoLookupOutcome::Ok(g),
        ProviderOutcome::RateLimited => saw_rate_limit = true,
        ProviderOutcome::Failed => {}
    }

    if saw_rate_limit {
        tracing::warn!("All geo providers rate-limited for {}", ip);
        GeoLookupOutcome::RateLimited
    } else {
        tracing::warn!("All geo providers failed for {}", ip);
        GeoLookupOutcome::Failed
    }
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
        _ => {
            tracing::warn!("ip-api.com request failed for {}", ip);
            return ProviderOutcome::Failed;
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
            // ip-api.com reports rate-limiting via status="fail" with a "rate"-style message
            // (e.g. "too many requests"), not via HTTP 429.
            let msg = data.message.unwrap_or_default().to_lowercase();
            if msg.contains("rate") || msg.contains("quota") || msg.contains("limit") {
                ProviderOutcome::RateLimited
            } else {
                tracing::warn!("ip-api.com non-success for {}: {}", ip, msg);
                ProviderOutcome::Failed
            }
        }
        Err(e) => {
            tracing::warn!("ip-api.com parse error: {}", e);
            ProviderOutcome::Failed
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
        _ => {
            tracing::warn!("ipwho.is request failed for {}", ip);
            return ProviderOutcome::Failed;
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
            let msg = data.message.unwrap_or_default().to_lowercase();
            if msg.contains("rate") || msg.contains("quota") || msg.contains("limit") {
                ProviderOutcome::RateLimited
            } else {
                tracing::warn!("ipwho.is non-success for {}: {}", ip, msg);
                ProviderOutcome::Failed
            }
        }
        Err(e) => {
            tracing::warn!("ipwho.is parse error: {}", e);
            ProviderOutcome::Failed
        }
    }
}
