use std::time::Duration;
use sysinfo::{Networks, System};

use crate::gui::window::UiUpdate;

#[derive(Debug, Clone)]
pub struct MonitorSample {
    pub cpu_usage: f32,
    pub mem_usage: f32,
    pub net_upload_bps: u64,
    pub net_download_bps: u64,
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

        if tx.send(UiUpdate::Monitor(MonitorSample {
            cpu_usage: cpu,
            mem_usage: mem_pct,
            net_upload_bps: upload_bps,
            net_download_bps: download_bps,
        })).is_err() {
            break;
        }
    }
}
