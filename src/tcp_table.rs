//! Win32 GetExtendedTcpTable 拿活动 TCP 连接 → 配合 GeoCache 反查国家分布。
//!
//! 用于 detailed 浮窗右侧"流量分流"显示：让用户一眼看出多大比例流量真的出墙。
//!
//! 局限：
//! - 只看连接数量，不看实际字节数（GetTcpTable 不提供字节流量；要拿真实带宽需 ETW，超出本工具规模）
//! - 国家归属依赖 geo_cache 命中；未命中的 IP 归到 "Unknown" 桶
//! - 不打 API 反查 —— 否则一次扫描可能触发上百次 HTTP 请求，被 ip-api 拒之门外
//!
//! 调用方：监控线程（或独立 task）周期性扫描 → 调 `summarize_by_country` →
//! 写到 `OverlayState.traffic_by_country` → render.rs 画水平条形

use std::net::Ipv4Addr;
use std::sync::Arc;

use vpn_monitor_core::network::geo_cache::GeoCache;

use windows::Win32::Foundation::NO_ERROR;
use windows::Win32::NetworkManagement::IpHelper::*;
use windows::Win32::Networking::WinSock::AF_INET;

/// 扫描本机所有 ESTABLISHED IPv4 TCP 连接的远端 IP（去重），按缓存命中
/// 的国家聚合 → 返回 [(country, conn_count)] 按降序。
///
/// `top_n`：保留最多 N 个国家，其它合到 "其它" 一桶；UI 上节省空间。
pub fn summarize_by_country(cache: Option<&Arc<GeoCache>>, top_n: usize) -> Vec<(String, u32)> {
    let remotes = match collect_established_remotes() {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };

    // 去重 + 跳过私有/loopback —— 这些不算"出墙流量"
    let mut unique: std::collections::HashSet<Ipv4Addr> = std::collections::HashSet::new();
    for ip in remotes {
        if is_public_v4(&ip) {
            unique.insert(ip);
        }
    }

    // 国家计数
    let mut counts: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    for ip in unique {
        let key = if let Some(cache) = cache {
            if let Some(geo) = cache.get(&ip.to_string()) {
                if geo.country.is_empty() { "未知".to_string() } else { geo.country }
            } else {
                "未知".to_string()
            }
        } else {
            "未知".to_string()
        };
        *counts.entry(key).or_insert(0) += 1;
    }

    let mut v: Vec<(String, u32)> = counts.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));

    if v.len() > top_n {
        let mut top: Vec<(String, u32)> = v.drain(..top_n).collect();
        let rest_sum: u32 = v.iter().map(|(_, c)| c).sum();
        if rest_sum > 0 {
            top.push(("其它".to_string(), rest_sum));
        }
        top
    } else {
        v
    }
}

/// 用 GetExtendedTcpTable 拿所有 ESTABLISHED 连接的远端 IPv4。
fn collect_established_remotes() -> std::io::Result<Vec<Ipv4Addr>> {
    unsafe {
        // 先用 0 size 拿所需 buffer 大小
        let mut size: u32 = 0;
        let _ = GetExtendedTcpTable(
            None,
            &mut size,
            false,
            AF_INET.0 as u32,
            TCP_TABLE_BASIC_CONNECTIONS,
            0,
        );
        if size == 0 {
            return Ok(Vec::new());
        }

        let mut buf = vec![0u8; size as usize];
        let ret = GetExtendedTcpTable(
            Some(buf.as_mut_ptr() as *mut _),
            &mut size,
            false,
            AF_INET.0 as u32,
            TCP_TABLE_BASIC_CONNECTIONS,
            0,
        );
        if ret != NO_ERROR.0 {
            return Err(std::io::Error::from_raw_os_error(ret as i32));
        }

        // 内存布局：MIB_TCPTABLE { dwNumEntries: DWORD, table: [MIB_TCPROW; n] }
        // MIB_TCPROW: { state, localAddr, localPort, remoteAddr, remotePort }
        let header = &*(buf.as_ptr() as *const MIB_TCPTABLE);
        let count = header.dwNumEntries as usize;
        let entries_ptr = header.table.as_ptr();

        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            let row = &*entries_ptr.add(i);
            // 只看 ESTABLISHED (5)
            if row.Anonymous.dwState == MIB_TCP_STATE_ESTAB.0 as u32 {
                // dwRemoteAddr 是网络字节序，u32 → 4 字节
                let addr_bytes = row.dwRemoteAddr.to_le_bytes();
                let ip = Ipv4Addr::new(addr_bytes[0], addr_bytes[1], addr_bytes[2], addr_bytes[3]);
                out.push(ip);
            }
        }
        Ok(out)
    }
}

fn is_public_v4(ip: &Ipv4Addr) -> bool {
    if ip.is_loopback() || ip.is_link_local() || ip.is_broadcast() || ip.is_multicast() || ip.is_unspecified() {
        return false;
    }
    let o = ip.octets();
    // 10.0.0.0/8
    if o[0] == 10 { return false; }
    // 172.16.0.0/12
    if o[0] == 172 && (o[1] & 0xF0) == 16 { return false; }
    // 192.168.0.0/16
    if o[0] == 192 && o[1] == 168 { return false; }
    // 100.64.0.0/10 (CGNAT)
    if o[0] == 100 && (o[1] & 0xC0) == 64 { return false; }
    true
}
