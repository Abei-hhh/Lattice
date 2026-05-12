#![windows_subsystem = "windows"]

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use clap::Parser;
use tokio::sync::mpsc;

mod config;
mod gui;
mod network;

use gui::render::{CheckStatus, IpUpdate, OverlayState, SharedState};
use network::geo_lookup::{self, GeoLookupOutcome};
use network::ip_fetcher::{self, IpFetchOutcome};

#[derive(Parser)]
#[command(name = "vpn-monitor")]
#[command(about = "Windows IP status overlay - shows public IP and geolocation")]
struct Args {
    /// Path to config file
    #[arg(short, long)]
    config: Option<String>,
}

fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();

    let args = Args::parse();
    let config_path = args.config.map(std::path::PathBuf::from);
    let config = config::load_config(config_path);

    tracing::info!("Vpn_Monitor starting...");
    tracing::info!("Check interval: {}s", config.check_interval);

    let mut client_builder = reqwest::Client::builder()
        .timeout(Duration::from_secs(config.timeout))
        .user_agent("VpnMonitor/1.0");

    if let Some(proxy) = &config.proxy {
        match reqwest::Proxy::all(proxy) {
            Ok(p) => {
                client_builder = client_builder.proxy(p);
            }
            Err(e) => {
                tracing::warn!("Invalid proxy config '{}': {}", proxy, e);
            }
        }
    }

    let client = client_builder.build().expect("Failed to build HTTP client");

    let state: SharedState = Arc::new(Mutex::new(OverlayState {
        show_isp: config.show_isp,
        opacity: config.opacity,
        ..Default::default()
    }));

    let (ip_tx, ip_rx) = mpsc::unbounded_channel::<IpUpdate>();

    let poll_client = client.clone();
    let check_interval = config.check_interval;
    let max_retries = config.max_retries;

    let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
    rt.spawn(async move {
        let mut last_ip: Option<String> = None;
        let mut consecutive_failures: u32 = 0;
        let mut current_interval = check_interval;

        loop {
            let timeout = Duration::from_secs(5);
            let outcome = ip_fetcher::fetch_public_ip(&poll_client, timeout).await;

            match outcome {
                IpFetchOutcome::Ok { ip, .. } => {
                    consecutive_failures = 0;
                    current_interval = check_interval;

                    let ip_changed = last_ip.as_ref() != Some(&ip);
                    let (geo, geo_rate_limited) = if ip_changed {
                        let timeout = Duration::from_secs(5);
                        match geo_lookup::lookup_geo(&poll_client, &ip, timeout).await {
                            GeoLookupOutcome::Ok(g) => (Some(g), false),
                            GeoLookupOutcome::RateLimited => (None, true),
                            GeoLookupOutcome::Failed => (None, false),
                        }
                    } else {
                        (None, false)
                    };

                    let status = if geo_rate_limited {
                        CheckStatus::ApiLimited
                    } else {
                        CheckStatus::Success
                    };

                    let update = IpUpdate {
                        ip: Some(ip.clone()),
                        geo,
                        status,
                    };

                    if ip_changed {
                        last_ip = Some(ip);
                    }

                    let _ = ip_tx.send(update);
                }
                IpFetchOutcome::RateLimited => {
                    // Keep last known IP visible, just flag the status as limited.
                    let _ = ip_tx.send(IpUpdate {
                        ip: last_ip.clone(),
                        geo: None,
                        status: CheckStatus::ApiLimited,
                    });
                    // Brief cool-down so we don't hammer rate-limited endpoints.
                    current_interval = check_interval.max(60);
                    tracing::warn!("IP sources rate-limited, cooling down to {}s", current_interval);
                }
                IpFetchOutcome::Failed => {
                    consecutive_failures += 1;
                    if consecutive_failures >= max_retries {
                        let _ = ip_tx.send(IpUpdate {
                            ip: None,
                            geo: None,
                            status: CheckStatus::NetworkError,
                        });
                    }

                    current_interval = (current_interval * 2).min(300);
                    tracing::warn!(
                        "IP fetch failed ({} consecutive), backing off to {}s",
                        consecutive_failures,
                        current_interval
                    );
                }
            }

            tokio::time::sleep(Duration::from_secs(current_interval)).await;
        }
    });

    gui::window::create_and_run(&config, state, ip_rx, client);

    tracing::info!("Vpn_Monitor exiting...");
}
