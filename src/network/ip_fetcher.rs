use reqwest::Client;
use std::time::Duration;
use tokio::time;

const IP_SOURCES: &[&str] = &[
    "https://api.ipify.org",
    "https://api.ip.sb/ip",
    "https://ifconfig.me/ip",
];

pub enum IpFetchOutcome {
    Ok { ip: String, source: &'static str },
    RateLimited,
    Failed,
}

enum SourceResult {
    Ok(String),
    RateLimited,
    Failed,
}

pub async fn fetch_public_ip(client: &Client, timeout: Duration) -> IpFetchOutcome {
    let mut tasks = Vec::with_capacity(IP_SOURCES.len());
    for &url in IP_SOURCES {
        let client = client.clone();
        tasks.push((
            url,
            tokio::spawn(async move {
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
                    Ok(Ok((429, _))) => SourceResult::RateLimited,
                    Ok(Ok((status, text))) if (200..300).contains(&status) => {
                        let ip = text.trim().to_string();
                        if ip.contains('.') || ip.contains(':') {
                            SourceResult::Ok(ip)
                        } else {
                            SourceResult::Failed
                        }
                    }
                    _ => SourceResult::Failed,
                }
            }),
        ));
    }

    let mut saw_rate_limit = false;
    for (url, task) in tasks {
        match task.await {
            Ok(SourceResult::Ok(ip)) => {
                tracing::info!("Public IP {} fetched from {}", ip, url);
                return IpFetchOutcome::Ok { ip, source: url };
            }
            Ok(SourceResult::RateLimited) => {
                tracing::warn!("IP source {} returned 429", url);
                saw_rate_limit = true;
            }
            _ => continue,
        }
    }

    if saw_rate_limit {
        IpFetchOutcome::RateLimited
    } else {
        tracing::warn!("All IP sources failed");
        IpFetchOutcome::Failed
    }
}
