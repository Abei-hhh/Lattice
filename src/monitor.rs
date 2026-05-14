use std::net::TcpStream;
use std::time::Duration;
use sysinfo::{Networks, System};

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

/// Check ProxyEnable DWORD in registry
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
        ) == 0 && data != 0
    }
}

/// Check AutoConfigURL (PAC) in registry — non-empty means PAC proxy is active
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
        // Check the string is non-empty (size includes null terminator)
        let len = (size / 2) as usize;
        buf[..len.saturating_sub(1)].iter().any(|&c| c != 0)
    }
}

// ── Port scan ─────────────────────────────────────────────────────

/// Common proxy ports used by Clash, V2Ray, Shadowsocks, etc.
const PROXY_PORTS: &[u16] = &[
    7890,  // Clash HTTP
    7891,  // Clash SOCKS
    7892,  // Clash mixed
    7893,  // Clash redir
    1080,  // Generic SOCKS5
    10808, // V2RayN HTTP
    10809, // V2RayN SOCKS
    1081,  // Shadowsocks SOCKS
    1087,  // Shadowsocks HTTP
    8080,  // Generic HTTP proxy
    8118,  // Privoxy
    9090,  // Clash API / Generic
    2080,  // Generic
    8388,  // Shadowsocks default
    10800, // SOCKS variant
];

/// Check if any common proxy port is listening (non-blocking, short timeout)
fn check_proxy_ports() -> bool {
    for &port in PROXY_PORTS {
        if TcpStream::connect_timeout(
            &format!("127.0.0.1:{}", port).parse().unwrap(),
            Duration::from_millis(200),
        ).is_ok() {
            return true;
        }
    }
    false
}

// ── Process detection ─────────────────────────────────────────────

/// Known proxy/VPN process names (lowercase, without extension)
const PROXY_PROCESSES: &[&str] = &[
    "clash",
    "clash-win64",
    "clash-core",
    "clash-meta",
    "mihomo",         // Clash Meta successor
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

/// Multi-layer proxy detection:
/// 1. Registry ProxyEnable (system proxy toggle)
/// 2. Registry AutoConfigURL (PAC auto-config)
/// 3. Common proxy port listening check
/// 4. Known proxy process detection
fn detect_proxy_active(sys: &System) -> bool {
    reg_proxy_enable()
        || reg_pac_url()
        || check_proxy_ports()
        || check_proxy_processes(sys)
}

// ── Monitor loop ──────────────────────────────────────────────────

pub async fn monitor_loop(
    tx: tokio::sync::mpsc::UnboundedSender<UiUpdate>,
    monitor_interval: u64,
    proxy_check_interval: u64,
) {
    let mut sys = System::new();
    let mut networks = Networks::new_with_refreshed_list();

    sys.refresh_cpu_all();

    let mut last_rx = 0u64;
    let mut last_tx = 0u64;
    for (_, net) in &networks {
        last_rx += net.total_received();
        last_tx += net.total_transmitted();
    }

    let mut proxy_enabled = detect_proxy_active(&sys);
    let proxy_ticks = if monitor_interval > 0 && proxy_check_interval >= monitor_interval {
        (proxy_check_interval / monitor_interval).max(1) as u32
    } else {
        1
    };
    let mut proxy_check_count: u32 = 0;

    loop {
        tokio::time::sleep(Duration::from_secs(monitor_interval.max(1))).await;

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

        let download_bps = if monitor_interval > 0 { cur_rx.saturating_sub(last_rx) / monitor_interval } else { 0 };
        let upload_bps = if monitor_interval > 0 { cur_tx.saturating_sub(last_tx) / monitor_interval } else { 0 };
        last_rx = cur_rx;
        last_tx = cur_tx;

        proxy_check_count += 1;
        if proxy_check_count >= proxy_ticks {
            proxy_check_count = 0;
            sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
            proxy_enabled = detect_proxy_active(&sys);
        }

        if tx.send(UiUpdate::Monitor(MonitorSample {
            cpu_usage: cpu,
            mem_usage: mem_pct,
            net_upload_bps: upload_bps,
            net_download_bps: download_bps,
            proxy_enabled,
        })).is_err() {
            break;
        }
    }
}
