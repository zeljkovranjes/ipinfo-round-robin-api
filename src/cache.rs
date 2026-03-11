use axum::body::Bytes;
use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone)]
pub struct CacheEntry {
    pub body: Bytes,
    pub content_type: String,
    pub status: u16,
    inserted_at: Instant,
    last_used: Arc<AtomicU64>, // epoch nanos, for LRU tracking
}

impl CacheEntry {
    fn is_expired(&self, ttl: Duration) -> bool {
        self.inserted_at.elapsed() >= ttl
    }

    fn touch(&self) {
        let now = epoch_nanos();
        self.last_used.store(now, Ordering::Relaxed);
    }

    fn last_used_nanos(&self) -> u64 {
        self.last_used.load(Ordering::Relaxed)
    }
}

pub struct Cache {
    map: DashMap<String, CacheEntry>,
    ttl: Duration,
    max_entries: usize,
    hits: AtomicU64,
    misses: AtomicU64,
    evictions: AtomicU64,
}

impl Cache {
    pub fn new(ttl_seconds: u64, max_entries: usize) -> Self {
        Cache {
            map: DashMap::new(),
            ttl: Duration::from_secs(ttl_seconds),
            max_entries,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
        }
    }

    /// Look up a cached entry. Returns `None` on miss or expiry.
    pub fn get(&self, key: &str) -> Option<CacheEntry> {
        if let Some(entry) = self.map.get(key) {
            if entry.is_expired(self.ttl) {
                drop(entry);
                self.map.remove(key);
                self.misses.fetch_add(1, Ordering::Relaxed);
                return None;
            }
            entry.touch();
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Some(entry.clone());
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        None
    }

    /// Insert an entry. Evicts the LRU entry if at capacity.
    pub fn insert(&self, key: String, body: Bytes, content_type: String, status: u16) {
        // Evict if full (check before inserting)
        if self.map.len() >= self.max_entries && !self.map.contains_key(&key) {
            self.evict_lru();
        }

        let entry = CacheEntry {
            body,
            content_type,
            status,
            inserted_at: Instant::now(),
            last_used: Arc::new(AtomicU64::new(epoch_nanos())),
        };
        self.map.insert(key, entry);
    }

    /// Flush all entries.
    pub fn clear(&self) {
        self.map.clear();
    }

    pub fn stats(&self) -> CacheStats {
        CacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            size: self.map.len(),
            evictions: self.evictions.load(Ordering::Relaxed),
        }
    }

    fn evict_lru(&self) {
        // Find the key with the smallest last_used timestamp.
        let oldest_key = self
            .map
            .iter()
            .min_by_key(|e| e.value().last_used_nanos())
            .map(|e| e.key().clone());

        if let Some(key) = oldest_key {
            self.map.remove(&key);
            self.evictions.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[derive(Debug, serde::Serialize)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub size: usize,
    pub evictions: u64,
}

/// Nanoseconds since an arbitrary epoch (used for LRU ordering only).
fn epoch_nanos() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let base = START.get_or_init(Instant::now);
    base.elapsed().as_nanos() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(body: &str) -> (Bytes, String, u16) {
        (Bytes::from(body.to_string()), "application/json".into(), 200)
    }

    #[test]
    fn basic_hit_miss() {
        let c = Cache::new(300, 100);
        assert!(c.get("k1").is_none());
        let (b, ct, s) = make_entry("{}");
        c.insert("k1".into(), b, ct, s);
        assert!(c.get("k1").is_some());
        let stats = c.stats();
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);
    }

    #[test]
    fn ttl_expiry() {
        let c = Cache::new(0, 100); // 0s TTL — expires immediately
        let (b, ct, s) = make_entry("{}");
        c.insert("k1".into(), b, ct, s);
        std::thread::sleep(Duration::from_millis(1));
        assert!(c.get("k1").is_none());
    }

    #[test]
    fn lru_eviction_at_capacity() {
        let c = Cache::new(300, 2);
        let (b1, ct1, s1) = make_entry("1");
        let (b2, ct2, s2) = make_entry("2");
        let (b3, ct3, s3) = make_entry("3");

        c.insert("k1".into(), b1, ct1, s1);
        std::thread::sleep(Duration::from_millis(1));
        c.insert("k2".into(), b2, ct2, s2);

        // Touch k1 so k2 becomes LRU
        c.get("k1");

        // Inserting k3 should evict k2 (least recently used)
        c.insert("k3".into(), b3, ct3, s3);

        assert!(c.get("k1").is_some(), "k1 should still be present");
        assert!(c.get("k2").is_none(), "k2 should have been evicted");
        assert!(c.get("k3").is_some(), "k3 should be present");
        assert_eq!(c.stats().evictions, 1);
    }

    #[test]
    fn clear_flushes_all() {
        let c = Cache::new(300, 100);
        let (b, ct, s) = make_entry("{}");
        c.insert("k1".into(), b.clone(), ct.clone(), s);
        c.insert("k2".into(), b, ct, s);
        c.clear();
        assert_eq!(c.stats().size, 0);
    }
}
