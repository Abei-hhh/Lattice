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

fn is_system_proxy_enabled() -> bool {
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

pub async fn monitor_loop(tx: tokio::sync::mpsc::UnboundedSender<UiUpdate>) {
    let mut sys = System::new();
    let mut networks = Networks::new_with_refreshed_list();

    sys.refresh_cpu_all();

    let mut last_rx = 0u64;
    let mut last_tx = 0u64;
    for (_, net) in &networks {
        last_rx += net.total_received();
        last_tx += net.total_transmitted();
    }

    let mut proxy_enabled = is_system_proxy_enabled();
    let mut proxy_check_count: u32 = 0;

    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;

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

        let download_bps = cur_rx.saturating_sub(last_rx) / 2;
        let upload_bps = cur_tx.saturating_sub(last_tx) / 2;
        last_rx = cur_rx;
        last_tx = cur_tx;

        // Check proxy every 30s (15 iterations)
        proxy_check_count += 1;
        if proxy_check_count >= 15 {
            proxy_check_count = 0;
            proxy_enabled = is_system_proxy_enabled();
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
