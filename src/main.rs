#![windows_subsystem = "windows"]

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use clap::Parser;
use tokio::sync::mpsc;
use windows::core::{s, PCWSTR};
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowA, MessageBoxW, SetWindowPos, ShowWindow, HWND_TOPMOST, MB_ICONERROR,
    MB_OK, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_SHOWNOACTIVATE,
};

mod config;
mod gui;
mod monitor;
mod network;

use gui::render::{CheckStatus, IpUpdate, OverlayState, SharedState};
use gui::window::UiUpdate;
use network::geo_lookup::{self, GeoLookupOutcome};
use network::ip_fetcher::{self, IpFetchOutcome};

fn show_error_dialog(msg: &str) {
    let body: Vec<u16> = msg.encode_utf16().chain(std::iter::once(0)).collect();
    let title: Vec<u16> = "Vpn Monitor"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(body.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

/// Find any running instance of the overlay and bring it to the front.
/// Returns true if an existing window was found.
fn try_focus_existing_instance() -> bool {
    unsafe {
        match FindWindowA(s!("VpnMonitorOverlay"), None) {
            Ok(hwnd) if !hwnd.is_invalid() => {
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
                true
            }
            _ => false,
        }
    }
}

/// Read the ANTHROPIC_MODEL env override Claude Code uses to pick a model.
/// This is the highest-priority source — cc-switch writes here when it
/// switches to a 3rd-party provider (Zhipu, mcodex, etc.).
fn read_anthropic_model_env() -> Option<String> {
    let home = dirs::home_dir()?;
    let content = std::fs::read_to_string(home.join(".claude").join("settings.json")).ok()?;
    let val: serde_json::Value = serde_json::from_str(&content).ok()?;
    let model = val
        .get("env")
        .and_then(|e| e.get("ANTHROPIC_MODEL"))
        .and_then(|m| m.as_str())?;
    if model.is_empty() {
        None
    } else {
        Some(model.to_string())
    }
}

/// Read the active cc-switch provider for Claude (e.g. "claude-official").
/// cc-switch's `~/.cc-switch/settings.json` records the currently selected
/// provider ID; the actual model is in a SQLite DB but is also mirrored into
/// ~/.claude/settings.json's env, so the JSON-only path here is sufficient.
fn read_ccswitch_provider() -> Option<String> {
    let home = dirs::home_dir()?;
    let content =
        std::fs::read_to_string(home.join(".cc-switch").join("settings.json")).ok()?;
    let val: serde_json::Value = serde_json::from_str(&content).ok()?;
    let id = val
        .get("currentProviderClaude")
        .and_then(|p| p.as_str())?;
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

/// Map cc-switch provider IDs to friendly labels. Custom providers use
/// UUIDs as IDs (unreadable) — for those we fall back to a generic label,
/// since the actual model name should already have been picked up from
/// env.ANTHROPIC_MODEL by `read_anthropic_model_env`.
fn friendly_provider_label(id: &str) -> String {
    match id {
        "claude-official" => "Claude Official".to_string(),
        s if s.len() >= 32 && s.matches('-').count() >= 4 => "Claude".to_string(),
        s => s.to_string(),
    }
}

/// Resolve the label shown in the overlay's top-left. Priority:
/// 1. `~/.claude/settings.json` env.ANTHROPIC_MODEL — explicit model override
/// 2. `~/.cc-switch/settings.json` currentProviderClaude — friendly name
/// 3. "Claude" fallback
fn read_claude_label() -> String {
    if let Some(m) = read_anthropic_model_env() {
        return m;
    }
    if let Some(id) = read_ccswitch_provider() {
        return friendly_provider_label(&id);
    }
    "Claude".to_string()
}

#[derive(Parser)]
#[command(name = "vpn-monitor")]
#[command(about = "Windows IP status overlay - shows public IP and geolocation")]
struct Args {
    /// Path to config file
    #[arg(short, long)]
    config: Option<String>,
}

fn try_init_logging(enable_log: bool) {
    if !enable_log {
        return;
    }
    let Some(data_dir) = dirs::data_dir() else {
        return;
    };
    let log_dir = data_dir.join("Vpn_Monitor");
    if std::fs::create_dir_all(&log_dir).is_err() {
        return;
    }
    let log_path = log_dir.join("vpn-monitor.log");

    const MAX_LOG_SIZE: u64 = 5 * 1024 * 1024;
    if let Ok(meta) = std::fs::metadata(&log_path) {
        if meta.len() > MAX_LOG_SIZE {
            let _ = std::fs::remove_file(&log_path);
        }
    }

    let Ok(mut log_file) = std::fs::File::create(&log_path) else {
        return;
    };
    use std::io::Write;
    let _ = log_file.write_all(&[0xEF, 0xBB, 0xBF]);
    let _ = tracing_subscriber::fmt()
        .with_writer(std::sync::Mutex::new(log_file))
        .with_target(false)
        .with_ansi(false)
        .try_init();

    tracing::info!("日志文件: {}", log_path.display());
    tracing::info!("Vpn_Monitor starting...");
}

fn main() {
    // ── Single-instance guard ──────────────────────────────────────
    // Capture GetLastError() *immediately* after CreateMutexW in the same
    // unsafe block so no intervening Rust code can clobber the thread-local
    // error code.
    let (mutex_result, already_exists) = unsafe {
        let h = windows::Win32::System::Threading::CreateMutexW(
            None,
            true,
            windows::core::w!("Vpn_Monitor_SingleInstance_v1"),
        );
        let exists = GetLastError() == ERROR_ALREADY_EXISTS;
        (h, exists)
    };

    // If another instance is running (or mutex creation failed in a way that
    // suggests one), fall back to FindWindow as a belt-and-braces check —
    // this catches edge cases where the mutex check races on rapid double-launch.
    if already_exists || mutex_result.is_err() || try_focus_existing_instance() {
        if mutex_result.is_ok() && !already_exists {
            // We created a fresh mutex but found an existing window — likely a
            // zombie from a previous crash. Bail anyway; user can kill the zombie.
        }
        return;
    }
    // Keep mutex alive for process lifetime. HANDLE has no Drop; OS reclaims
    // on process exit, releasing the named mutex.
    let _mutex_handle = mutex_result;

    let args = Args::parse();
    let config_path = args.config.map(std::path::PathBuf::from);
    let config = config::load_config(config_path);

    try_init_logging(config.enable_log);
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

    let client = match client_builder.build() {
        Ok(c) => c,
        Err(e) => {
            show_error_dialog(&format!("无法创建 HTTP 客户端: {}", e));
            return;
        }
    };

    let claude_model = read_claude_label();

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
    let ip_timeout = Duration::from_secs(config.timeout);
    let ip_tx = update_tx.clone();

    let rt = match tokio::runtime::Runtime::new() {
        Ok(r) => r,
        Err(e) => {
            show_error_dialog(&format!("无法创建 Tokio 运行时: {}", e));
            return;
        }
    };

    rt.spawn(async move {
        let mut last_ip: Option<String> = None;
        let mut last_geo: Option<network::geo_lookup::GeoInfo> = None;
        // Carries the latest geo failure reason (e.g. "限流", "私有段") so the
        // overlay can show *why* the city is missing. Cleared on geo success.
        let mut last_geo_error: Option<String> = None;
        let mut consecutive_failures: u32 = 0;
        let mut current_interval = check_interval;
        let mut poll_count: u32 = 0;
        let mut geo_needs_retry = false;

        loop {
            poll_count += 1;
            tracing::info!("[poll#{}] === 开始第 {} 次检测 ===", poll_count, poll_count);

            let outcome = ip_fetcher::fetch_public_ip(&poll_client, ip_timeout).await;

            match outcome {
                IpFetchOutcome::Ok { ip, latency_ms, .. } => {
                    consecutive_failures = 0;
                    current_interval = check_interval;

                    let ip_changed = last_ip.as_ref() != Some(&ip);
                    tracing::info!(
                        "[poll#{}] IP获取成功: ip={}, ip_changed={}, latency={}ms",
                        poll_count, ip, ip_changed, latency_ms
                    );

                    if ip_changed {
                        last_ip = Some(ip.clone());
                        // New IP — stale geo no longer applies, force re-lookup.
                        last_geo = None;
                        last_geo_error = None;
                        geo_needs_retry = true;
                    }

                    let _ = ip_tx.send(UiUpdate::Ip(IpUpdate {
                        ip: Some(ip.clone()),
                        geo: last_geo.clone(),
                        status: CheckStatus::Success,
                        latency_ms: Some(latency_ms),
                        error_reason: None,
                        geo_error_reason: last_geo_error.clone(),
                    }));

                    if geo_needs_retry {
                        tracing::info!("[poll#{}] 查询归属地...", poll_count);
                        match geo_lookup::lookup_geo(&poll_client, &ip, ip_timeout).await {
                            GeoLookupOutcome::Ok(g) => {
                                tracing::info!(
                                    "[poll#{}] 归属地查询成功: {} {} (ISP: {})",
                                    poll_count, g.country, g.city, g.isp
                                );
                                last_geo = Some(g.clone());
                                last_geo_error = None;
                                geo_needs_retry = false;
                                let _ = ip_tx.send(UiUpdate::Ip(IpUpdate {
                                    ip: Some(ip.clone()),
                                    geo: Some(g),
                                    status: CheckStatus::Success,
                                    latency_ms: Some(latency_ms),
                                    error_reason: None,
                                    geo_error_reason: None,
                                }));
                            }
                            GeoLookupOutcome::RateLimited => {
                                tracing::warn!("[poll#{}] 归属地查询被限流", poll_count);
                                last_geo_error = Some("限流".to_string());
                                let _ = ip_tx.send(UiUpdate::Ip(IpUpdate {
                                    ip: Some(ip.clone()),
                                    geo: last_geo.clone(),
                                    status: CheckStatus::Success,
                                    latency_ms: Some(latency_ms),
                                    error_reason: None,
                                    geo_error_reason: last_geo_error.clone(),
                                }));
                            }
                            GeoLookupOutcome::Failed(reason) => {
                                tracing::warn!(
                                    "[poll#{}] 归属地查询失败: {}",
                                    poll_count,
                                    reason.label()
                                );
                                last_geo_error = Some(reason.label());
                                let _ = ip_tx.send(UiUpdate::Ip(IpUpdate {
                                    ip: Some(ip.clone()),
                                    geo: last_geo.clone(),
                                    status: CheckStatus::Success,
                                    latency_ms: Some(latency_ms),
                                    error_reason: None,
                                    geo_error_reason: last_geo_error.clone(),
                                }));
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
                        error_reason: None,
                        geo_error_reason: last_geo_error.clone(),
                    }));
                    current_interval = check_interval.max(60);
                }
                IpFetchOutcome::Failed(reason) => {
                    consecutive_failures += 1;
                    tracing::warn!(
                        "[poll#{}] IP获取失败 (连续{}次, 原因: {})",
                        poll_count,
                        consecutive_failures,
                        reason.label()
                    );
                    if consecutive_failures >= max_retries {
                        // Preserve last-known IP/geo so the user can still see
                        // which network they were on; only the status dot turns
                        // red and we surface the failure reason.
                        let _ = ip_tx.send(UiUpdate::Ip(IpUpdate {
                            ip: last_ip.clone(),
                            geo: last_geo.clone(),
                            status: CheckStatus::NetworkError,
                            latency_ms: None,
                            error_reason: Some(reason.label()),
                            geo_error_reason: last_geo_error.clone(),
                        }));
                    }
                    current_interval = (current_interval * 2).min(300);
                }
            }

            tokio::time::sleep(Duration::from_secs(current_interval)).await;
        }
    });

    // System monitor — run on a dedicated OS thread because it makes blocking
    // syscalls (port scan, refresh_processes) that would otherwise stall the
    // tokio runtime.
    let monitor_tx = update_tx.clone();
    let monitor_interval = config.monitor_interval;
    let proxy_check_interval = config.proxy_check_interval;
    std::thread::Builder::new()
        .name("vpn-monitor-sysmon".into())
        .spawn(move || {
            monitor::monitor_loop_sync(monitor_tx, monitor_interval, proxy_check_interval);
        })
        .ok();

    // Claude model refresh task (0 = disabled, only read at startup)
    let model_refresh_interval = config.model_refresh_interval;
    if model_refresh_interval > 0 {
        let model_state = state.clone();
        rt.spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(model_refresh_interval)).await;
                let model = read_claude_label();
                if !model.is_empty() {
                    if let Ok(mut s) = model_state.lock() {
                        s.claude_model = model;
                    }
                }
            }
        });
    }

    drop(update_tx);

    gui::window::create_and_run(&config, state, update_rx, client);

    tracing::info!("Vpn_Monitor exiting...");
}
