//! 系统监控线程。
//!
//! 跑在专属 OS 线程上（不是 tokio worker），因为端口扫描和
//! `refresh_processes` 是同步阻塞 syscall，跑在 tokio 上会卡住 IP 轮询。
//!
//! **代理检测三层**（按可信度从高到低）：
//! 1. 注册表 `ProxyEnable` / `AutoConfigURL`
//! 2. 已知代理进程名（Clash / V2Ray / Sing-Box 等）
//! 3. 代理专用端口监听（剔除了 8080 / 9090 等易冲突端口）
//!
//! **代理状态翻转**：检测到变化时通过 `proxy_change_notify.notify_one()`
//! 通知 IP 轮询任务立即重查，无需等下一个 tick。
//!
//! **空闲降频**：`GetLastInputInfo` 探测用户空闲秒数，超过阈值时所有
//! 间隔乘以 `idle_multiplier`，AFK 时降低 CPU/电量消耗。

use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use sysinfo::{Networks, ProcessRefreshKind, ProcessesToUpdate, System};

use crate::gui::window::UiUpdate;

#[derive(Debug, Clone)]
pub struct MonitorSample {
    pub cpu_usage: f32,
    pub mem_usage: f32,
    pub net_upload_bps: u64,
    pub net_download_bps: u64,
    pub proxy_enabled: bool,
}

// ── Registry helpers ──────────────────────────────────────────────

#[link(name = "advapi32")]
extern "system" {
    fn RegGetValueW(
        hkey: isize,
        subkey: *const u16,
        value: *const u16,
        flags: u32,
        dtype: *mut u32,
        data: *mut u8,
        size: *mut u32,
    ) -> i32;
}

const HKEY_CURRENT_USER: isize = 0x80000001;
const RRF_RT_REG_DWORD: u32 = 0x10;
const RRF_RT_REG_SZ: u32 = 0x02;

fn reg_proxy_enable() -> bool {
    unsafe {
        let subkey = windows::core::w!("Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings");
        let value = windows::core::w!("ProxyEnable");
        let mut data: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;
        let mut dtype = 0u32;

        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            &mut dtype,
            &mut data as *mut u32 as *mut u8,
            &mut size,
        ) == 0
            && data != 0
    }
}

fn reg_pac_url() -> bool {
    unsafe {
        let subkey = windows::core::w!("Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings");
        let value = windows::core::w!("AutoConfigURL");
        let mut buf = [0u16; 512];
        let mut size = (buf.len() * 2) as u32;
        let mut dtype = 0u32;

        let ok = RegGetValueW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            &mut dtype,
            buf.as_mut_ptr() as *mut u8,
            &mut size,
        ) == 0;
        if !ok || size <= 2 {
            return false;
        }
        let len = (size / 2) as usize;
        buf[..len.saturating_sub(1)].iter().any(|&c| c != 0)
    }
}

// ── Port scan ─────────────────────────────────────────────────────

/// Proxy-specific ports. We deliberately **omit** ports that commonly collide
/// with non-proxy software (8080 dev HTTP, 9090 Prometheus/Clash API,
/// 2080 generic, 8118 Privoxy, 10800 misc) — port scan alone is too noisy.
/// Detection now relies on the registry first, then process names, with
/// proxy-specific ports as a supplemental signal.
const PROXY_PORTS: &[u16] = &[
    7890,  // Clash HTTP
    7891,  // Clash SOCKS
    7892,  // Clash mixed
    7893,  // Clash redir
    7897,  // Mihomo mixed default
    10808, // V2RayN HTTP
    10809, // V2RayN SOCKS
    8388,  // Shadowsocks default
    1080,  // Generic SOCKS5
    1087,  // SS HTTP
];

/// 并发扫描代理端口：N 个线程各做一次 `connect_timeout`，任一成功就
/// 写共享 `AtomicBool`，其他线程下次 load 时短路返回。
/// 最差耗时 ≈ 一次 connect 超时（~150ms）而非 N×150ms 串行。
fn check_proxy_ports() -> bool {
    let found = Arc::new(AtomicBool::new(false));
    let mut handles = Vec::with_capacity(PROXY_PORTS.len());
    for &port in PROXY_PORTS {
        let found = found.clone();
        let h = std::thread::Builder::new()
            .name("lattice-portscan".into())
            .spawn(move || {
                if found.load(Ordering::Relaxed) {
                    return;
                }
                if let Ok(addr) = format!("127.0.0.1:{}", port).parse::<SocketAddr>() {
                    if TcpStream::connect_timeout(&addr, Duration::from_millis(150)).is_ok() {
                        found.store(true, Ordering::Relaxed);
                    }
                }
            });
        if let Ok(handle) = h {
            handles.push(handle);
        }
    }
    for h in handles {
        let _ = h.join();
    }
    found.load(Ordering::Relaxed)
}

// ── Process detection ─────────────────────────────────────────────

const PROXY_PROCESSES: &[&str] = &[
    "clash",
    "clash-win64",
    "clash-core",
    "clash-meta",
    "mihomo",
    "v2ray",
    "v2rayn",
    "v2rayng",
    "xray",
    "shadowsocks",
    "shadowsocksr",
    "ss-local",
    "ssr-local",
    "sing-box",
    "hysteria",
    "naiveproxy",
    "trojan",
    "trojan-go",
    "wireguard",
    "wg",
    "openvpn",
    "netch",
    "qv2ray",
    "nekoray",
    "nekobox",
    "clash-verge",
    "clash-verge-service",
    "flclash",
];

fn check_proxy_processes(sys: &System) -> bool {
    for (_pid, process) in sys.processes() {
        let name = process.name().to_string_lossy().to_lowercase();
        let name = name.trim_end_matches(".exe");
        if PROXY_PROCESSES.iter().any(|&p| name.contains(p)) {
            return true;
        }
    }
    false
}

// ── Combined detection ────────────────────────────────────────────

/// Tiered detection:
/// 1. Registry ProxyEnable / PAC URL — definitive system-proxy signal.
/// 2. Known proxy process running — reliable when system proxy is off but a
///    tunnel client is active (e.g. TUN mode).
/// 3. Proxy-specific port listening — supplemental.
fn detect_proxy_active(sys: &System) -> bool {
    if reg_proxy_enable() || reg_pac_url() {
        return true;
    }
    if check_proxy_processes(sys) {
        return true;
    }
    check_proxy_ports()
}

// ── Synchronous monitor loop ──────────────────────────────────────
//
// Runs on a dedicated OS thread (not a tokio worker) because port scans and
// `sysinfo::refresh_processes` are blocking syscalls that would otherwise
// stall the tokio runtime servicing IP/geo lookups.

/// 增量刷新进程表：用 `ProcessRefreshKind::nothing()` 只刷进程名，
/// 跳过 CPU/内存/exe/cmdline/env 等昂贵字段，单次刷新成本降到 ~1/5。
fn refresh_processes_name_only(sys: &mut System) {
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing(),
    );
}

/// `proxy_change_notify` is signalled whenever the detected proxy state flips,
/// so the IP poll task can immediately re-fetch instead of waiting for its
/// next tick. `idle_threshold_seconds`/`idle_multiplier` provide idle-aware
/// scaling: when the user has been idle ≥ threshold, sleep intervals are
/// multiplied by the multiplier (1 = disabled).
pub fn monitor_loop_sync(
    tx: tokio::sync::mpsc::UnboundedSender<UiUpdate>,
    monitor_interval: u64,
    proxy_check_interval: u64,
    proxy_change_notify: Arc<tokio::sync::Notify>,
    idle_threshold_seconds: u64,
    idle_multiplier: u64,
) {
    let monitor_interval = monitor_interval.max(1);
    let idle_multiplier = idle_multiplier.max(1);
    let mut sys = System::new();
    let mut networks = Networks::new_with_refreshed_list();

    sys.refresh_cpu_all();

    let mut last_rx = 0u64;
    let mut last_tx = 0u64;
    for (_, net) in &networks {
        last_rx += net.total_received();
        last_tx += net.total_transmitted();
    }

    refresh_processes_name_only(&mut sys);
    let mut proxy_enabled = detect_proxy_active(&sys);

    let proxy_ticks = if proxy_check_interval >= monitor_interval {
        (proxy_check_interval / monitor_interval).max(1) as u32
    } else {
        1
    };
    let mut proxy_check_count: u32 = 0;

    loop {
        let mult = current_idle_multiplier(idle_threshold_seconds, idle_multiplier);
        std::thread::sleep(Duration::from_secs(monitor_interval * mult));

        sys.refresh_cpu_all();
        sys.refresh_memory();
        networks.refresh(true);

        let cpu = sys.global_cpu_usage();

        let total = sys.total_memory();
        let used = sys.used_memory();
        let mem_pct = if total > 0 {
            used as f32 / total as f32 * 100.0
        } else {
            0.0
        };

        let mut cur_rx = 0u64;
        let mut cur_tx = 0u64;
        for (_, net) in &networks {
            cur_rx += net.total_received();
            cur_tx += net.total_transmitted();
        }

        // Use actual elapsed (post-sleep multiplier) for the bps math so the
        // displayed rate is correct in idle-scaled mode.
        let elapsed = (monitor_interval * mult).max(1);
        let download_bps = cur_rx.saturating_sub(last_rx) / elapsed;
        let upload_bps = cur_tx.saturating_sub(last_tx) / elapsed;
        last_rx = cur_rx;
        last_tx = cur_tx;

        proxy_check_count += 1;
        if proxy_check_count >= proxy_ticks {
            proxy_check_count = 0;
            refresh_processes_name_only(&mut sys);
            let new_state = detect_proxy_active(&sys);
            if new_state != proxy_enabled {
                tracing::info!(
                    "Proxy state changed: {} → {}, signalling immediate IP re-check",
                    proxy_enabled, new_state
                );
                proxy_change_notify.notify_one();
            }
            proxy_enabled = new_state;
        }

        if tx
            .send(UiUpdate::Monitor(MonitorSample {
                cpu_usage: cpu,
                mem_usage: mem_pct,
                net_upload_bps: upload_bps,
                net_download_bps: download_bps,
                proxy_enabled,
            }))
            .is_err()
        {
            // UI side dropped the receiver — shut down.
            break;
        }
    }
}

// ── Idle detection ───────────────────────────────────────────────

#[link(name = "user32")]
extern "system" {
    fn GetLastInputInfo(plii: *mut LastInputInfo) -> i32;
    fn GetTickCount() -> u32;
}

#[repr(C)]
struct LastInputInfo {
    cb_size: u32,
    dw_time: u32,
}

/// 返回用户键鼠最后一次输入距今的秒数。GetLastInputInfo 失败时返回 0，
/// 调用方按"刚有输入"处理（不会误降频）。
pub fn user_idle_seconds() -> u64 {
    unsafe {
        let mut info = LastInputInfo {
            cb_size: std::mem::size_of::<LastInputInfo>() as u32,
            dw_time: 0,
        };
        if GetLastInputInfo(&mut info) == 0 {
            return 0;
        }
        // GetTickCount wraps every ~49 days; wrapping_sub handles the rollover.
        let now = GetTickCount();
        let elapsed_ms = now.wrapping_sub(info.dw_time);
        (elapsed_ms / 1000) as u64
    }
}

/// 用户空闲 ≥ 阈值时返回 `multiplier`，否则 1。threshold = 0 关闭整个机制。
pub fn current_idle_multiplier(threshold_seconds: u64, multiplier: u64) -> u64 {
    if threshold_seconds == 0 {
        return 1;
    }
    if user_idle_seconds() >= threshold_seconds {
        multiplier.max(1)
    } else {
        1
    }
}
