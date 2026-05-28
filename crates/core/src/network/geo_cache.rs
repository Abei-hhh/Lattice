//! 归属地磁盘缓存。
//!
//! **关键设计**：
//! - **/24 子网归并**：v4 用 `a.b.c.0/24`、v6 用前 48 位作 key。同 ISP 节点池
//!   通常共享城市，命中率从精确匹配的 ~30% 提升到 ~80%。
//! - **LRU 淘汰**：超过 `max_entries`（默认 1000）淘汰最老条目，防止 JSON
//!   无限膨胀。
//! - **原子写盘**：每次 insert 都重写整个 JSON（量级 KB，便宜），用
//!   `tmp + rename` 避免中途崩溃留下半写文件。
//! - **旧格式兼容**：若 JSON 是早期版本（裸 HashMap），自动迁移到新的
//!   `DiskFormat { entries, lru }` 结构。

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::geo_lookup::GeoInfo;

/// 见模块文档。
pub struct GeoCache {
    path: PathBuf,
    ttl_secs: u64,
    max_entries: usize,
    inner: Mutex<Inner>,
}

struct Inner {
    entries: HashMap<String, CacheEntry>,
    /// Front = most-recently-used. Bounded by `max_entries`.
    lru: VecDeque<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    geo: GeoInfo,
    /// The unmasked IP that triggered this insert. Stored only for the
    /// history-timeline window — never compared against during lookup, which
    /// always goes through the network key.
    #[serde(default)]
    last_ip: String,
    /// Seconds since UNIX epoch when this entry was written.
    inserted_at: u64,
}

#[derive(Default, Serialize, Deserialize)]
struct DiskFormat {
    #[serde(default)]
    entries: HashMap<String, CacheEntry>,
    /// Persisted LRU order so eviction policy survives restarts.
    #[serde(default)]
    lru: Vec<String>,
}

/// One row for the history-timeline UI. The cache holds the full collection
/// chronologically (oldest-first via inserted_at).
#[derive(Debug, Clone)]
pub struct HistoryEntry {
    pub key: String,
    pub last_ip: String,
    pub geo: GeoInfo,
    pub inserted_at: u64,
}

impl GeoCache {
    pub fn new(path: PathBuf, ttl_hours: u64, max_entries: usize) -> Self {
        let mut loaded = load_from_disk(&path).unwrap_or_default();
        // 一次性丢弃 `country_code` 缺失的旧条目（GeoInfo 加 ISO 码字段
        // 之前写入的）。这些条目会让泄漏检测拿到空字符串、被上层 filter
        // 掉，导致 v6/DNS 泄漏徽章在 TTL 期内（最长 7 天）持续假阴。
        // 下次 poll 命中同 /24 时 fresh lookup 会重新填充并写盘。
        let before = loaded.entries.len();
        loaded.entries.retain(|_, e| {
            e.geo.country.is_empty() || !e.geo.country_code.is_empty()
        });
        let dropped = before - loaded.entries.len();
        if dropped > 0 {
            let valid: std::collections::HashSet<&String> =
                loaded.entries.keys().collect();
            loaded.lru.retain(|k| valid.contains(k));
            tracing::info!(
                "Geo cache: dropped {} legacy entries without country_code",
                dropped
            );
        }
        tracing::info!(
            "Geo cache loaded: {} entries from {:?}",
            loaded.entries.len(),
            path
        );
        let inner = Inner {
            lru: VecDeque::from(loaded.lru),
            entries: loaded.entries,
        };
        Self {
            path,
            ttl_secs: ttl_hours.saturating_mul(3600),
            max_entries: max_entries.max(16),
            inner: Mutex::new(inner),
        }
    }

    /// Look up by an exact IP — the cache normalises to its network key
    /// internally. Returns `None` if absent or expired. Hits are promoted to
    /// the front of the LRU queue (but not persisted until next insert, since
    /// rewriting the whole file on every read would be wasteful).
    pub fn get(&self, ip: &str) -> Option<GeoInfo> {
        let key = network_key(ip)?;
        let now = unix_now();
        let mut guard = lock_inner(&self.inner);
        let entry = guard.entries.get(&key)?;
        if now.saturating_sub(entry.inserted_at) > self.ttl_secs {
            return None;
        }
        let geo = entry.geo.clone();
        promote_lru(&mut guard.lru, &key);
        Some(geo)
    }

    /// Insert or refresh an entry and persist to disk. Drops the oldest LRU
    /// entries if over `max_entries`.
    pub fn insert(&self, ip: String, geo: GeoInfo) {
        let Some(key) = network_key(&ip) else {
            return;
        };
        let entry = CacheEntry {
            geo,
            last_ip: ip,
            inserted_at: unix_now(),
        };
        let snapshot = {
            let mut guard = lock_inner(&self.inner);
            guard.entries.insert(key.clone(), entry);
            promote_lru(&mut guard.lru, &key);
            // Evict from the tail until we're at the cap.
            while guard.lru.len() > self.max_entries {
                if let Some(oldest) = guard.lru.pop_back() {
                    guard.entries.remove(&oldest);
                }
            }
            DiskFormat {
                entries: guard.entries.clone(),
                lru: guard.lru.iter().cloned().collect(),
            }
        };
        if let Err(e) = save_to_disk(&self.path, &snapshot) {
            tracing::warn!("Geo cache write failed: {}", e);
        }
    }

    /// 按网段 key 删除一条记录并立刻刷盘。历史窗口右键菜单"删除"走这条。
    /// 不存在的 key 静默忽略。返回 true 表示确实删了一条。
    pub fn remove(&self, network_key: &str) -> bool {
        let snapshot = {
            let mut guard = lock_inner(&self.inner);
            let removed = guard.entries.remove(network_key).is_some();
            if removed {
                if let Some(pos) = guard.lru.iter().position(|k| k == network_key) {
                    guard.lru.remove(pos);
                }
            }
            if !removed {
                return false;
            }
            DiskFormat {
                entries: guard.entries.clone(),
                lru: guard.lru.iter().cloned().collect(),
            }
        };
        if let Err(e) = save_to_disk(&self.path, &snapshot) {
            tracing::warn!("Geo cache delete-flush failed: {}", e);
        }
        true
    }

    /// Snapshot of all entries newest-first, for the history window. Stale
    /// entries are included — the UI can grey them out if it wants.
    pub fn history(&self) -> Vec<HistoryEntry> {
        let guard = lock_inner(&self.inner);
        let mut rows: Vec<HistoryEntry> = guard
            .entries
            .iter()
            .map(|(k, e)| HistoryEntry {
                key: k.clone(),
                last_ip: e.last_ip.clone(),
                geo: e.geo.clone(),
                inserted_at: e.inserted_at,
            })
            .collect();
        rows.sort_by(|a, b| b.inserted_at.cmp(&a.inserted_at));
        rows
    }
}

fn lock_inner(m: &Mutex<Inner>) -> std::sync::MutexGuard<'_, Inner> {
    match m.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

/// 把 `key` 提到 LRU 队首（标记为最近访问）；不存在则推入。
/// O(n) 复杂度对 1000 量级足够；要更大量级可换 LinkedHashMap。
fn promote_lru(lru: &mut VecDeque<String>, key: &str) {
    if let Some(pos) = lru.iter().position(|k| k == key) {
        if pos != 0 {
            let item = lru.remove(pos).unwrap();
            lru.push_front(item);
        }
    } else {
        lru.push_front(key.to_string());
    }
}

/// v4 → `a.b.c.0/24`，v6 → 取前 3 个 hextet 拼 `xxxx:xxxx:xxxx::/48`。
/// 非 IP 字符串返回 None（缓存只为真实 IP 服务）。
fn network_key(ip: &str) -> Option<String> {
    match ip.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => {
            let o = v4.octets();
            Some(format!("{}.{}.{}.0/24", o[0], o[1], o[2]))
        }
        Ok(IpAddr::V6(v6)) => {
            let s = v6.segments();
            Some(format!("{:x}:{:x}:{:x}::/48", s[0], s[1], s[2]))
        }
        Err(_) => None,
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn load_from_disk(path: &PathBuf) -> Option<DiskFormat> {
    let raw = std::fs::read_to_string(path).ok()?;
    // Tolerate the old flat-HashMap format so existing cache files don't get
    // wiped on first run after upgrade — re-parse as a plain map if the new
    // `DiskFormat` shape fails.
    if let Ok(d) = serde_json::from_str::<DiskFormat>(&raw) {
        return Some(d);
    }
    let legacy: HashMap<String, CacheEntry> = serde_json::from_str(&raw).ok()?;
    Some(DiskFormat {
        lru: legacy.keys().cloned().collect(),
        entries: legacy,
    })
}

fn save_to_disk(path: &PathBuf, data: &DiskFormat) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let serialized = serde_json::to_string(data)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serialized)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Default cache location: `%APPDATA%\Vpn_Monitor\geo_cache.json` on Windows.
pub fn default_cache_path() -> Option<PathBuf> {
    let data = dirs::data_dir()?;
    Some(data.join("Vpn_Monitor").join("geo_cache.json"))
}
