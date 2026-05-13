#![windows_subsystem = "windows"]

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use clap::Parser;
use tokio::sync::mpsc;

mod config;
mod gui;
mod monitor;
mod network;

use gui::render::{CheckStatus, IpUpdate, OverlayState, SharedState};
use gui::window::UiUpdate;
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
    let args = Args::parse();
    let config_path = args.config.map(std::path::PathBuf::from);
    let config = config::load_config(config_path);

    if config.enable_log {
        let log_path = dirs::data_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("Vpn_Monitor")
            .join("vpn-monitor.log");

        if let Some(parent) = log_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        const MAX_LOG_SIZE: u64 = 5 * 1024 * 1024;
        if let Ok(meta) = std::fs::metadata(&log_path) {
            if meta.len() > MAX_LOG_SIZE {
                let _ = std::fs::remove_file(&log_path);
            }
        }

        let mut log_file = std::fs::File::create(&log_path).expect("Failed to create log file");
        use std::io::Write;
        let _ = log_file.write_all(&[0xEF, 0xBB, 0xBF]);
        tracing_subscriber::fmt()
            .with_writer(std::sync::Mutex::new(log_file))
            .with_target(false)
            .with_ansi(false)
            .init();

        tracing::info!("日志文件: {}", log_path.display());
        tracing::info!("Vpn_Monitor starting...");
        tracing::info!("Check interval: {}s", config.check_interval);
    }

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

    let has_proxy = config.proxy.is_some();
    let state: SharedState = Arc::new(Mutex::new(OverlayState {
        has_proxy,
        opacity: config.opacity,
        ..Default::default()
    }));

    let (update_tx, update_rx) = mpsc::unbounded_channel::<UiUpdate>();

    // IP poll task
    let poll_client = client.clone();
    let check_interval = config.check_interval;
    let max_retries = config.max_retries;
    let ip_tx = update_tx.clone();

    let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
    rt.spawn(async move {
        let mut last_ip: Option<String> = None;
        let mut last_geo: Option<network::geo_lookup::GeoInfo> = None;
        let mut consecutive_failures: u32 = 0;
        let mut current_interval = check_interval;
        let mut poll_count: u32 = 0;

        loop {
            poll_count += 1;
            tracing::info!("[poll#{}] === 开始第 {} 次检测 ===", poll_count, poll_count);

            let timeout = Duration::from_secs(5);
            let outcome = ip_fetcher::fetch_public_ip(&poll_client, timeout).await;

            match outcome {
                IpFetchOutcome::Ok { ip, .. } => {
                    consecutive_failures = 0;
                    current_interval = check_interval;

                    let ip_changed = last_ip.as_ref() != Some(&ip);
                    tracing::info!(
                        "[poll#{}] IP获取成功: ip={}, last_ip={}, ip_changed={}",
                        poll_count, ip,
                        last_ip.as_deref().unwrap_or("None"), ip_changed
                    );

                    let (geo, geo_rate_limited) = if ip_changed {
                        let timeout = Duration::from_secs(5);
                        tracing::info!("[poll#{}] IP已变更，开始查询归属地...", poll_count);
                        match geo_lookup::lookup_geo(&poll_client, &ip, timeout).await {
                            GeoLookupOutcome::Ok(g) => {
                                tracing::info!(
                                    "[poll#{}] 归属地查询成功: {} {} (ISP: {})",
                                    poll_count, g.country, g.city, g.isp
                                );
                                last_geo = Some(g.clone());
                                (Some(g), false)
                            }
                            GeoLookupOutcome::RateLimited => {
                                tracing::warn!("[poll#{}] 归属地查询被限流，使用缓存", poll_count);
                                (last_geo.clone(), true)
                            }
                            GeoLookupOutcome::Failed => {
                                tracing::warn!("[poll#{}] 归属地查询失败，使用缓存", poll_count);
                                (last_geo.clone(), false)
                            }
                        }
                    } else {
                        tracing::info!("[poll#{}] IP未变更，使用缓存归属地", poll_count);
                        (last_geo.clone(), false)
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

                    tracing::info!(
                        "[poll#{}] 发送UI更新: ip={:?}, geo={:?}, status={:?}",
                        poll_count, update.ip, update.geo, update.status
                    );

                    if ip_changed {
                        last_ip = Some(ip);
                    }

                    let _ = ip_tx.send(UiUpdate::Ip(update));
                }
                IpFetchOutcome::RateLimited => {
                    tracing::warn!("[poll#{}] IP源被限流", poll_count);
                    let _ = ip_tx.send(UiUpdate::Ip(IpUpdate {
                        ip: last_ip.clone(),
                        geo: last_geo.clone(),
                        status: CheckStatus::ApiLimited,
                    }));
                    current_interval = check_interval.max(60);
                }
                IpFetchOutcome::Failed => {
                    consecutive_failures += 1;
                    tracing::warn!(
                        "[poll#{}] IP获取失败 (连续{}次)", poll_count, consecutive_failures
                    );
                    if consecutive_failures >= max_retries {
                        let _ = ip_tx.send(UiUpdate::Ip(IpUpdate {
                            ip: None,
                            geo: None,
                            status: CheckStatus::NetworkError,
                        }));
                    }
                    current_interval = (current_interval * 2).min(300);
                }
            }

            tracing::info!("[poll#{}] === 检测完成，下次间隔 {}s ===", poll_count, current_interval);
            tokio::time::sleep(Duration::from_secs(current_interval)).await;
        }
    });

    // System monitor task
    let monitor_tx = update_tx.clone();
    rt.spawn(async move {
        monitor::monitor_loop(monitor_tx).await;
    });

    drop(update_tx); // All senders cloned into tasks, drop the original

    gui::window::create_and_run(&config, state, update_rx, client);

    tracing::info!("Vpn_Monitor exiting...");
}
