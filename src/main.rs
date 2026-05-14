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

fn read_claude_model() -> String {
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    let settings_path = home.join(".claude").join("settings.json");

    let content = match std::fs::read_to_string(&settings_path) {
        Ok(c) => c,
        Err(_) => return String::new(),
    };

    let val: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return String::new(),
    };

    val.get("env")
        .and_then(|e| e.get("ANTHROPIC_MODEL"))
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string()
}

#[derive(Parser)]
#[command(name = "vpn-monitor")]
#[command(about = "Windows IP status overlay - shows public IP and geolocation")]
struct Args {
    /// Path to config file
    #[arg(short, long)]
    config: Option<String>,
}

fn main() {
    // Single instance check via named mutex
    let _mutex = unsafe {
        windows::Win32::System::Threading::CreateMutexW(
            None, true, windows::core::w!("Vpn_Monitor_SingleInstance"),
        )
    };
    if _mutex.is_ok() && unsafe { windows::Win32::Foundation::GetLastError().0 == 183 } {
        return; // ERROR_ALREADY_EXISTS — another instance is running
    }

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

    let claude_model = read_claude_model();

    let state: SharedState = Arc::new(Mutex::new(OverlayState {
        opacity: config.opacity,
        claude_model,
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
        // Track whether we need to retry geo lookup for the current IP
        let mut geo_needs_retry = false;

        loop {
            poll_count += 1;
            tracing::info!("[poll#{}] === 开始第 {} 次检测 ===", poll_count, poll_count);

            let timeout = Duration::from_secs(5);
            let outcome = ip_fetcher::fetch_public_ip(&poll_client, timeout).await;

            match outcome {
                IpFetchOutcome::Ok { ip, latency_ms, .. } => {
                    consecutive_failures = 0;
                    current_interval = check_interval;

                    let ip_changed = last_ip.as_ref() != Some(&ip);
                    tracing::info!(
                        "[poll#{}] IP获取成功: ip={}, last_ip={}, ip_changed={}, latency={}ms",
                        poll_count, ip,
                        last_ip.as_deref().unwrap_or("None"), ip_changed, latency_ms
                    );

                    if ip_changed {
                        last_ip = Some(ip.clone());
                        geo_needs_retry = true;
                    }

                    // Send IP update immediately so UI shows the IP quickly
                    let _ = ip_tx.send(UiUpdate::Ip(IpUpdate {
                        ip: Some(ip.clone()),
                        geo: last_geo.clone(),
                        status: CheckStatus::Success,
                        latency_ms: Some(latency_ms),
                    }));

                    // Geo lookup: do it when IP changed or previous lookup failed
                    if geo_needs_retry {
                        let geo_timeout = Duration::from_secs(5);
                        tracing::info!("[poll#{}] 查询归属地...", poll_count);
                        match geo_lookup::lookup_geo(&poll_client, &ip, geo_timeout).await {
                            GeoLookupOutcome::Ok(g) => {
                                tracing::info!(
                                    "[poll#{}] 归属地查询成功: {} {} (ISP: {})",
                                    poll_count, g.country, g.city, g.isp
                                );
                                last_geo = Some(g.clone());
                                geo_needs_retry = false;

                                // Send geo update
                                let _ = ip_tx.send(UiUpdate::Ip(IpUpdate {
                                    ip: Some(ip.clone()),
                                    geo: Some(g),
                                    status: CheckStatus::Success,
                                    latency_ms: Some(latency_ms),
                                }));
                            }
                            GeoLookupOutcome::RateLimited => {
                                tracing::warn!("[poll#{}] 归属地查询被限流", poll_count);
                            }
                            GeoLookupOutcome::Failed => {
                                tracing::warn!("[poll#{}] 归属地查询失败，下次重试", poll_count);
                            }
                        }
                    }
                }
                IpFetchOutcome::RateLimited => {
                    tracing::warn!("[poll#{}] IP源被限流", poll_count);
                    let _ = ip_tx.send(UiUpdate::Ip(IpUpdate {
                        ip: last_ip.clone(),
                        geo: last_geo.clone(),
                        status: CheckStatus::ApiLimited,
                        latency_ms: None,
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
                            latency_ms: None,
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
    let monitor_interval = config.monitor_interval;
    let proxy_check_interval = config.proxy_check_interval;
    rt.spawn(async move {
        monitor::monitor_loop(monitor_tx, monitor_interval, proxy_check_interval).await;
    });

    // Claude model refresh task (0 = disabled, only read at startup)
    let model_refresh_interval = config.model_refresh_interval;
    if model_refresh_interval > 0 {
        let model_state = state.clone();
        rt.spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(model_refresh_interval)).await;
                let model = read_claude_model();
                if !model.is_empty() {
                    if let Ok(mut s) = model_state.lock() {
                        s.claude_model = model;
                    }
                }
            }
        });
    }

    drop(update_tx); // All senders cloned into tasks, drop the original

    gui::window::create_and_run(&config, state, update_rx, client);

    tracing::info!("Vpn_Monitor exiting...");
}
