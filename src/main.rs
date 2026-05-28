#![windows_subsystem = "windows"]

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use clap::Parser;
use tokio::sync::{mpsc, Notify};
use windows::core::{s, PCWSTR};
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowA, MessageBoxW, SetWindowPos, ShowWindow, HWND_TOPMOST, MB_ICONERROR,
    MB_OK, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_SHOWNOACTIVATE,
};

mod gui;
mod monitor;

// 把 core crate 的子模块 re-export 成本地 `crate::xxx`，让 gui/* 等子模块原有
// `crate::config::...` / `crate::network::...` 等路径不用动。
// 这是过渡阶段的简化策略；后续可逐步迁移到直接 `vpn_monitor_core::...`。
pub use vpn_monitor_core::{cc_switch, config, network, runtime};

use gui::render::{CheckStatus, IpUpdate, OverlayState, SharedState};
use gui::window::UiUpdate;
use network::geo_cache::GeoCache;
use network::geo_lookup::{self, GeoLookupOutcome};
use network::ip_fetcher::{self, mask_ip, IpFetchOutcome};
use runtime::RuntimeFlags;

/// Best-effort extraction of a human-readable string from a `catch_unwind`
/// payload. Most panics carry either `&str` or `String`; anything else falls
/// back to a placeholder.
fn panic_payload_string(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        return (*s).to_string();
    }
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    "<non-string panic payload>".to_string()
}

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

/// 浮窗左上 tag 字符串解析入口 —— 直接转给 cc_switch 模块按 active source 读。
/// 默认 source = "claude"（保持向后兼容）。
fn read_active_source_label(source: &str) -> String {
    let label = cc_switch::read_label(source);
    if label.is_empty() { "Claude".to_string() } else { label }
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
    // ── DPI awareness ─────────────────────────────────────────────
    // Per-Monitor V2 让我们自己处理每个显示器的 DPI 缩放（接 WM_DPICHANGED），
    // 而不是被 OS bitmap-stretch（默认行为）。后者在 125%/150%/175% 缩放下
    // 会让文字 / 控件边缘出现明显毛刺。必须在创建任何 HWND 之前调用。
    // 失败可能是老系统（Win10 1607 之前），就退化回 system-aware；再不行
    // 干脆静默——bitmap stretch 不是致命错误。
    unsafe {
        use windows::Win32::UI::HiDpi::{
            SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            DPI_AWARENESS_CONTEXT_SYSTEM_AWARE,
        };
        if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2).is_err() {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_SYSTEM_AWARE);
        }
    }

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

    // Wire IP / geo mask state BEFORE any tracing call that may print one.
    ip_fetcher::set_mask_ip_logs(config.mask_ip_in_log);
    ip_fetcher::set_mask_geo_logs(config.mask_geo_in_log);
    try_init_logging(config.enable_log);
    tracing::info!("Check interval: {}s", config.check_interval);

    // ── Geo cache (disk-persistent) ────────────────────────────────
    let geo_cache: Option<Arc<GeoCache>> = if config.geo_cache_enabled {
        network::geo_cache::default_cache_path().map(|p| {
            Arc::new(GeoCache::new(
                p,
                config.geo_cache_ttl_hours,
                config.geo_cache_max_entries,
            ))
        })
    } else {
        None
    };

    // Notified by:
    //   • the GUI thread on WM_POWERBROADCAST (resume from sleep)
    //   • the monitor thread when the detected proxy state flips
    // The IP poll task races this against its sleep timer so either signal
    // cuts short the wait and triggers an immediate re-check.
    let ip_check_notify = Arc::new(Notify::new());

    // Persisted overlay state (position + locked) — loaded before window
    // creation so we can place the window where the user last left it.
    let persisted = gui::overlay_state::load();
    let runtime_flags = RuntimeFlags::from_config(&config, persisted.locked);

    let mut client_builder = reqwest::Client::builder()
        .timeout(Duration::from_secs(config.timeout))
        .user_agent("VpnMonitor/1.0");

    if let Some(proxy) = &config.proxy {
        match reqwest::Proxy::all(proxy) {
            Ok(p) => {
                client_builder = client_builder.proxy(p);
            }
            Err(e) => {
                tracing::warn!(
                    "Invalid proxy config '{}': {}",
                    ip_fetcher::mask_proxy_url(proxy),
                    e
                );
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

    let claude_model = read_active_source_label(&config.active_cc_switch_provider);

    // 启动时根据 config.theme 解析出实际主题色板
    let initial_theme = gui::theme::resolve(&config.theme);
    let state: SharedState = Arc::new(Mutex::new(OverlayState {
        opacity: config.opacity,
        claude_model,
        theme: initial_theme,
        row2_mode: config.row2_mode.clone(),
        usage_5h_limit_requests: config.usage_5h_limit_requests,
        usage_week_limit_requests: config.usage_week_limit_requests,
        runtime_flags: Some(runtime_flags.clone()),
        ..Default::default()
    }));

    let (update_tx, update_rx) = mpsc::unbounded_channel::<UiUpdate>();

    // IP poll task
    let poll_client = client.clone();
    let check_interval = config.check_interval;
    let max_retries = config.max_retries;
    let ip_timeout = Duration::from_secs(config.timeout);
    // Geo lookups get a tighter timeout than IP fetch so a slow provider
    // doesn't keep the city blank: providers race concurrently, and 3s is
    // enough for both ip-api.com and ipwho.is on a healthy link.
    let geo_timeout = Duration::from_secs(3);
    let geo_cache_for_poll = geo_cache.clone();
    let ip_check_notify_for_poll = ip_check_notify.clone();
    let flags_for_poll = runtime_flags.clone();
    let idle_threshold = config.idle_threshold_seconds;
    let idle_multiplier = config.idle_multiplier;
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
                        poll_count, mask_ip(&ip), ip_changed, latency_ms
                    );

                    if ip_changed {
                        last_ip = Some(ip.clone());
                        last_geo = None;
                        last_geo_error = None;
                        geo_needs_retry = true;

                        // 缓存命中直接跳过 geo 查询 —— 切回常用节点能瞬间
                        // 显示城市。托盘菜单关闭缓存则完全绕过。
                        if flags_for_poll.geo_cache_enabled.load(std::sync::atomic::Ordering::Relaxed) {
                        if let Some(cache) = &geo_cache_for_poll {
                            if let Some(cached) = cache.get(&ip) {
                                tracing::info!(
                                    "[poll#{}] 归属地命中缓存: {} {}",
                                    poll_count,
                                    ip_fetcher::mask_geo(&cached.country),
                                    ip_fetcher::mask_geo(&cached.city)
                                );
                                last_geo = Some(cached);
                                geo_needs_retry = false;
                            }
                        }
                        }
                    }

                    let _ = ip_tx.send(UiUpdate::Ip(IpUpdate {
                        ip: Some(ip.clone()),
                        geo: last_geo.clone(),
                        status: CheckStatus::Success,
                        latency_ms: Some(latency_ms),
                        error_reason: None,
                        geo_error_reason: last_geo_error.clone(),
                        geo_warning: None,
                    }));

                    if geo_needs_retry {
                        // 同一轮内最多重试 2 次（间隔 1s），失败后下一轮
                        // poll 还会再试一次。RateLimited 不重试免得加深节流。
                        const GEO_MAX_ATTEMPTS: u32 = 2;
                        for attempt in 1..=GEO_MAX_ATTEMPTS {
                            tracing::info!(
                                "[poll#{}] 查询归属地 (尝试 {}/{})...",
                                poll_count, attempt, GEO_MAX_ATTEMPTS
                            );
                            match geo_lookup::lookup_geo(
                                &poll_client,
                                &ip,
                                geo_timeout,
                                flags_for_poll.geo_cross_check.load(std::sync::atomic::Ordering::Relaxed),
                            )
                            .await
                            {
                                GeoLookupOutcome::Ok { geo: g, warning } => {
                                    tracing::info!(
                                        "[poll#{}] 归属地查询成功: {} {} (ISP: {})",
                                        poll_count,
                                        ip_fetcher::mask_geo(&g.country),
                                        ip_fetcher::mask_geo(&g.city),
                                        ip_fetcher::mask_geo(&g.isp)
                                    );
                                    if flags_for_poll.geo_cache_enabled.load(std::sync::atomic::Ordering::Relaxed) {
                                        if let Some(cache) = &geo_cache_for_poll {
                                            cache.insert(ip.clone(), g.clone());
                                        }
                                    }
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
                                        geo_warning: warning,
                                    }));
                                    break;
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
                                        geo_warning: None,
                                    }));
                                    break;
                                }
                                GeoLookupOutcome::Failed(reason) => {
                                    tracing::warn!(
                                        "[poll#{}] 归属地查询失败 (尝试 {}/{}): {}",
                                        poll_count, attempt, GEO_MAX_ATTEMPTS,
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
                                        geo_warning: None,
                                    }));
                                    if attempt < GEO_MAX_ATTEMPTS {
                                        tokio::time::sleep(Duration::from_secs(1)).await;
                                    }
                                }
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
                        geo_warning: None,
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
                        let _ = ip_tx.send(UiUpdate::Ip(IpUpdate {
                            ip: last_ip.clone(),
                            geo: last_geo.clone(),
                            status: CheckStatus::NetworkError,
                            latency_ms: None,
                            error_reason: Some(reason.label()),
                            geo_error_reason: last_geo_error.clone(),
                            geo_warning: None,
                        }));
                    }
                    current_interval = (current_interval * 2).min(300);
                }
            }

            // 把 sleep 和"立即重查"信号放进同一个 select，谁先到谁触发
            // 下一轮：休眠唤醒（window_proc WM_POWERBROADCAST）和代理变化
            // （monitor_loop_sync）都会 notify_one，浮窗第一时间反映真实
            // IP 而不是停滞最多一个 check_interval。
            // 空闲降频：用户空闲超阈值则 sleep × idle_multiplier，AFK 不
            // 浪费 CPU / 电量。
            let idle_mult = monitor::current_idle_multiplier(idle_threshold, idle_multiplier);
            let scaled = (current_interval as u128 * idle_mult as u128).min(3600) as u64;
            let sleep = tokio::time::sleep(Duration::from_secs(scaled));
            tokio::pin!(sleep);
            tokio::select! {
                _ = &mut sleep => {}
                _ = ip_check_notify_for_poll.notified() => {
                    tracing::info!("[poll#{}] 收到外部触发信号，立即重查", poll_count + 1);
                    // Force geo re-check on next iteration even if IP appears
                    // unchanged — e.g. after proxy toggle, we want fresh geo.
                    last_ip = None;
                    // Reset failure counter so a transient pre-sleep error
                    // doesn't keep showing red after a fresh re-check trigger.
                    consecutive_failures = 0;
                    current_interval = check_interval;
                }
            }
        }
    });

    // System monitor — run on a dedicated OS thread because it makes blocking
    // syscalls (port scan, refresh_processes) that would otherwise stall the
    // tokio runtime. The closure is wrapped in catch_unwind + restart loop so
    // a panic deep inside sysinfo doesn't silently freeze the CPU/RAM/proxy
    // panel for the rest of the session.
    let monitor_tx = update_tx.clone();
    let monitor_interval = config.monitor_interval;
    let proxy_check_interval = config.proxy_check_interval;
    let monitor_notify = ip_check_notify.clone();
    let monitor_idle_threshold = config.idle_threshold_seconds;
    let monitor_idle_multiplier = config.idle_multiplier;
    std::thread::Builder::new()
        .name("vpn-monitor-sysmon".into())
        .spawn(move || {
            // Restart on panic; if the channel is gone, monitor_loop_sync
            // returns normally and we exit the supervisor too.
            const MAX_RESTARTS: u32 = 10;
            let mut restarts = 0u32;
            loop {
                let tx = monitor_tx.clone();
                let notify = monitor_notify.clone();
                let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    monitor::monitor_loop_sync(
                        tx,
                        monitor_interval,
                        proxy_check_interval,
                        notify,
                        monitor_idle_threshold,
                        monitor_idle_multiplier,
                    );
                }));
                match result {
                    Ok(()) => {
                        tracing::info!("Monitor thread exited cleanly");
                        return;
                    }
                    Err(payload) => {
                        let msg = panic_payload_string(&payload);
                        restarts += 1;
                        if restarts > MAX_RESTARTS {
                            tracing::error!(
                                "Monitor thread panicked {} times, giving up: {}",
                                restarts, msg
                            );
                            return;
                        }
                        tracing::error!(
                            "Monitor thread panicked (restart {}/{}): {}",
                            restarts, MAX_RESTARTS, msg
                        );
                        // Small backoff so a tight panic loop doesn't burn CPU.
                        std::thread::sleep(Duration::from_secs(2));
                    }
                }
            }
        })
        .ok();

    // 代理 RPC 探测 task（Clash / Mihomo / sing-box 当前节点名）
    // 间隔 5s 足够 —— 切节点不是常态操作。client_clone 与 IP 轮询共享同一个
    // reqwest::Client，连接池复用。
    {
        let rpc_state = state.clone();
        let rpc_client = client.clone();
        rt.spawn(async move {
            loop {
                let snap = vpn_monitor_core::proxy_rpc::detect(&rpc_client).await;
                if let Ok(mut s) = rpc_state.lock() {
                    s.proxy_rpc = snap;
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });
    }

    // DNS + v6 泄漏检测 task。开销大（3 个并发 HTTPS），间隔放长到 2 分钟。
    // 用最新的 v4 country 作为基准对比。
    {
        let leak_state = state.clone();
        let leak_client = client.clone();
        rt.spawn(async move {
            // 启动后稍等一下让 IP 轮询拿到 v4 country
            tokio::time::sleep(Duration::from_secs(15)).await;
            loop {
                // 必须传 ISO2 country_code（不是 country 全名）—— 三个泄漏维度都
                // 在 ISO 码层面比较，否则中文/英文长名永远 != "US" 之类的 ISO 码。
                let v4_cc = {
                    let s = match leak_state.lock() {
                        Ok(g) => g,
                        Err(p) => p.into_inner(),
                    };
                    s.current_update
                        .geo
                        .as_ref()
                        .map(|g| g.country_code.clone())
                        .filter(|c| !c.is_empty())
                };
                let report = vpn_monitor_core::network::leak_check::check_leaks(
                    &leak_client,
                    v4_cc.as_deref(),
                    Duration::from_secs(3),
                )
                .await;
                if let Ok(mut s) = leak_state.lock() {
                    s.leak = Some(report);
                }
                tokio::time::sleep(Duration::from_secs(120)).await;
            }
        });
    }

    // cc-switch 可用性检测 task：每 15s 探测 SQLite 文件存在 + cc-switch.exe
    // 进程在跑。结果写入 RuntimeFlags.cc_switch_available，所有 AI 相关 UI
    // 都读这个 atomic 决定隐藏/显示。
    {
        let flags = runtime_flags.clone();
        std::thread::Builder::new()
            .name("vpn-monitor-ccswitch-probe".into())
            .spawn(move || {
                use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};
                let mut sys = System::new();
                loop {
                    let files_ok = vpn_monitor_core::cc_switch::files_present();
                    let proc_ok = if files_ok {
                        // 只在文件存在时才付进程枚举的代价。
                        sys.refresh_processes_specifics(
                            ProcessesToUpdate::All,
                            true,
                            ProcessRefreshKind::nothing(),
                        );
                        sys.processes().values().any(|p| {
                            let name = p.name().to_string_lossy().to_ascii_lowercase();
                            name == "cc-switch.exe" || name == "cc-switch"
                        })
                    } else {
                        false
                    };
                    flags
                        .cc_switch_available
                        .store(files_ok && proc_ok, std::sync::atomic::Ordering::Relaxed);
                    std::thread::sleep(Duration::from_secs(15));
                }
            })
            .ok();
    }

    // cc-switch SQLite 用量刷新 task —— 每 N 秒重读 5h/周用量并写到 state。
    // 传入用户配置的配额上限给 ETA 算法使用。
    let usage_refresh_interval = config.usage_refresh_interval;
    let limit_5h = config.usage_5h_limit_requests;
    let limit_week = config.usage_week_limit_requests;
    if usage_refresh_interval > 0 {
        let usage_state = state.clone();
        let usage_source = runtime_flags.active_cc_switch_provider.clone();
        rt.spawn(async move {
            loop {
                let source = usage_source
                    .read()
                    .map(|g| g.clone())
                    .unwrap_or_else(|p| p.into_inner().clone());
                let usage = vpn_monitor_core::usage::read_usage_stats_with_limits(
                    &source, limit_5h, limit_week,
                );
                if let Ok(mut s) = usage_state.lock() {
                    s.usage = usage;
                }
                tokio::time::sleep(Duration::from_secs(usage_refresh_interval)).await;
            }
        });
    }

    // Claude / CC-Switch model refresh task —— 每 N 秒重读一次 active
    // provider 的当前模型名。`active_cc_switch_provider` 通过 RwLock 共享，
    // 设置对话框切换源后立刻生效，无需等下个 tick。
    // model_refresh_interval = 0 关闭后台刷新，仅启动时读一次。
    let model_refresh_interval = config.model_refresh_interval;
    if model_refresh_interval > 0 {
        let model_state = state.clone();
        let active_source = runtime_flags.active_cc_switch_provider.clone();
        rt.spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(model_refresh_interval)).await;
                let source = {
                    let g = active_source.read().unwrap_or_else(|p| p.into_inner());
                    g.clone()
                };
                let model = read_active_source_label(&source);
                if !model.is_empty() {
                    if let Ok(mut s) = model_state.lock() {
                        s.claude_model = model;
                    }
                }
            }
        });
    }

    drop(update_tx);

    gui::window::create_and_run(
        &config,
        state,
        update_rx,
        ip_check_notify,
        geo_cache,
        runtime_flags,
        persisted,
    );

    tracing::info!("Vpn_Monitor exiting...");
}
