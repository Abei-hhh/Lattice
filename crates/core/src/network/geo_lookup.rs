//! IP → 归属地查询。
//!
//! **双 provider 策略**：
//! - `ipwho.is` 走 HTTPS（防中间人篡改 country）
//! - `ip-api.com` 走 HTTP（更快但可被劫持）
//!
//! 当 `cross_check = true`（默认）：等两个 provider 都完成 → 优先用
//! HTTPS 结果 → 若两边国别不一致，回传 warning 让 UI 显示 ⚠ 标记。
//!
//! 当 `cross_check = false`：竞速取最快 Ok 的那个（手动 lookup 对话框走这条）。

use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::time;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GeoInfo {
    #[serde(default)]
    pub country: String,
    /// ISO 3166-1 alpha-2 国别码（"US" / "JP" / "CN" ...）。
    /// 用于跨源稳健比较 —— `country` 字段在不同语言下文案不同
    /// （ip-api lang=zh-CN 给 "美国"，ipwho.is 给 "United States"，
    /// Cloudflare trace 给 "US"），直接比较会假阳性。
    #[serde(default)]
    pub country_code: String,
    #[serde(default)]
    pub region: String,
    #[serde(default)]
    pub city: String,
    #[serde(default)]
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
    /// `warning` is `Some(label)` when the HTTPS and HTTP providers reported
    /// different country names in a cross-check run — the displayed result is
    /// the HTTPS one, but the UI surfaces the warning so the user knows their
    /// HTTP-fetched geo could have been MITM-spoofed.
    Ok {
        geo: GeoInfo,
        warning: Option<String>,
    },
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
    #[serde(rename = "countryCode")]
    country_code: Option<String>,
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
    country_code: Option<String>,
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

/// Look up geo info for `ip`. When `cross_check` is true, both providers run
/// concurrently and we wait for both, preferring the HTTPS provider
/// (`ipwho.is`) when both succeed. If the two providers report different
/// non-empty country names we log a warning — that means an HTTP-layer
/// response was likely tampered with, or one provider's data is stale.
/// When `cross_check` is false, the providers race and the first `Ok` wins
/// (latency-optimal but no MITM mitigation for ip-api.com).
pub async fn lookup_geo(
    client: &Client,
    ip: &str,
    timeout: Duration,
    cross_check: bool,
) -> GeoLookupOutcome {
    let c1 = client.clone();
    let c2 = client.clone();
    let ip1 = ip.to_string();
    let ip2 = ip.to_string();
    let mut h_http = tokio::spawn(async move { lookup_ip_api(&c1, &ip1, timeout).await });
    let mut h_https = tokio::spawn(async move { lookup_ipwho_is(&c2, &ip2, timeout).await });

    let mut saw_rate_limit = false;
    let mut reasons: Vec<GeoFailReason> = Vec::new();

    if cross_check {
        // Wait for BOTH, prefer HTTPS. The latency cost is at most one
        // provider's timeout (3s); average ~one provider's actual response
        // time since both run concurrently.
        let (r_https, r_http) = tokio::join!(&mut h_https, &mut h_http);

        let mut warning: Option<String> = None;
        // If both succeeded with non-empty countries, flag any disagreement.
        if let (Ok(ProviderOutcome::Ok(g_https)), Ok(ProviderOutcome::Ok(g_http))) =
            (&r_https, &r_http)
        {
            if !g_https.country.is_empty()
                && !g_http.country.is_empty()
                && g_https.country != g_http.country
            {
                tracing::warn!(
                    "Geo cross-check MISMATCH for {}: ipwho.is(HTTPS)={} vs ip-api(HTTP)={} — possible MITM or stale data; using HTTPS",
                    crate::network::ip_fetcher::mask_ip(ip),
                    g_https.country,
                    g_http.country
                );
                warning = Some(format!("跨源不一致: HTTPS={}/HTTP={}", g_https.country, g_http.country));
            }
        }

        match r_https {
            Ok(ProviderOutcome::Ok(g)) => return GeoLookupOutcome::Ok { geo: g, warning },
            Ok(ProviderOutcome::RateLimited) => saw_rate_limit = true,
            Ok(ProviderOutcome::Failed(r)) => reasons.push(r),
            Err(_) => reasons.push(GeoFailReason::Other),
        }
        match r_http {
            Ok(ProviderOutcome::Ok(g)) => {
                // HTTPS provider failed, fell back to HTTP — surface that
                // the answer is from an untrusted (MITMable) source so the
                // user can take it with a grain of salt.
                return GeoLookupOutcome::Ok {
                    geo: g,
                    warning: Some("仅 HTTP 源（HTTPS 失败）".to_string()),
                };
            }
            Ok(ProviderOutcome::RateLimited) => saw_rate_limit = true,
            Ok(ProviderOutcome::Failed(r)) => reasons.push(r),
            Err(_) => reasons.push(GeoFailReason::Other),
        }
    } else {
        // Race: take whichever finishes first.
        let (first, second_handle) = tokio::select! {
            r = &mut h_http => (r, h_https),
            r = &mut h_https => (r, h_http),
        };

        match first {
            Ok(ProviderOutcome::Ok(g)) => {
                second_handle.abort();
                return GeoLookupOutcome::Ok { geo: g, warning: None };
            }
            Ok(ProviderOutcome::RateLimited) => saw_rate_limit = true,
            Ok(ProviderOutcome::Failed(r)) => reasons.push(r),
            Err(_) => reasons.push(GeoFailReason::Other),
        }
        match second_handle.await {
            Ok(ProviderOutcome::Ok(g)) => return GeoLookupOutcome::Ok { geo: g, warning: None },
            Ok(ProviderOutcome::RateLimited) => saw_rate_limit = true,
            Ok(ProviderOutcome::Failed(r)) => reasons.push(r),
            Err(_) => reasons.push(GeoFailReason::Other),
        }
    }

    if saw_rate_limit {
        tracing::warn!(
            "All geo providers rate-limited for {}",
            crate::network::ip_fetcher::mask_ip(ip)
        );
        return GeoLookupOutcome::RateLimited;
    }

    reasons.sort_by_key(|r| r.priority());
    let reason = reasons.into_iter().next().unwrap_or(GeoFailReason::Other);
    tracing::warn!(
        "All geo providers failed for {}: {}",
        crate::network::ip_fetcher::mask_ip(ip),
        reason.label()
    );
    GeoLookupOutcome::Failed(reason)
}

async fn lookup_ip_api(client: &Client, ip: &str, timeout: Duration) -> ProviderOutcome {
    let url = format!("http://ip-api.com/json/{}?lang=zh-CN", ip);
    let result = time::timeout(timeout, async {
        client
            .get(&url)
            .header("User-Agent", "Lattice/1.0")
            .send()
            .await
    })
    .await;

    let resp = match result {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            tracing::warn!("ip-api.com request failed for {}: {}", crate::network::ip_fetcher::mask_ip(ip), e);
            return ProviderOutcome::Failed(classify_reqwest_error(&e));
        }
        Err(_) => {
            tracing::warn!("ip-api.com timed out for {}", crate::network::ip_fetcher::mask_ip(ip));
            return ProviderOutcome::Failed(GeoFailReason::Timeout);
        }
    };

    if resp.status().as_u16() == 429 {
        return ProviderOutcome::RateLimited;
    }

    match resp.json::<IpApiResponse>().await {
        Ok(data) if data.status == "success" => ProviderOutcome::Ok(GeoInfo {
            country: data.country.unwrap_or_default(),
            country_code: data.country_code.unwrap_or_default().to_uppercase(),
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
                tracing::warn!("ip-api.com non-success for {}: {}", crate::network::ip_fetcher::mask_ip(ip), msg);
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
            .header("User-Agent", "Lattice/1.0")
            .send()
            .await
    })
    .await;

    let resp = match result {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            tracing::warn!("ipwho.is request failed for {}: {}", crate::network::ip_fetcher::mask_ip(ip), e);
            return ProviderOutcome::Failed(classify_reqwest_error(&e));
        }
        Err(_) => {
            tracing::warn!("ipwho.is timed out for {}", crate::network::ip_fetcher::mask_ip(ip));
            return ProviderOutcome::Failed(GeoFailReason::Timeout);
        }
    };

    if resp.status().as_u16() == 429 {
        return ProviderOutcome::RateLimited;
    }

    match resp.json::<IpWhoIsResponse>().await {
        Ok(data) if data.success.unwrap_or(false) => ProviderOutcome::Ok(GeoInfo {
            country: data.country.unwrap_or_default(),
            country_code: data.country_code.unwrap_or_default().to_uppercase(),
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
                tracing::warn!("ipwho.is non-success for {}: {}", crate::network::ip_fetcher::mask_ip(ip), msg);
                ProviderOutcome::Failed(classify_api_message(&msg))
            }
        }
        Err(e) => {
            tracing::warn!("ipwho.is parse error: {}", e);
            ProviderOutcome::Failed(GeoFailReason::Decode)
        }
    }
}
