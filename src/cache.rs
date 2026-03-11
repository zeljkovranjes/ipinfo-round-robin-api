use axum::body::Bytes;
use moka::future::Cache as MokaCache;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
pub struct CacheEntry {
    pub body: Bytes,
    pub content_type: String,
    pub status: u16,
}

pub struct Cache {
    inner: MokaCache<String, CacheEntry>,
    hits: AtomicU64,
    misses: AtomicU64,
    evictions: Arc<AtomicU64>,
}

impl Cache {
    pub fn new(ttl: Duration, max_entries: usize) -> Self {
        let evictions = Arc::new(AtomicU64::new(0));
        let ev = Arc::clone(&evictions);
        let inner = MokaCache::builder()
            .max_capacity(max_entries as u64)
            .time_to_live(ttl)
            .eviction_listener(move |_k, _v, _cause| {
                ev.fetch_add(1, Ordering::Relaxed);
            })
            .build();
        Cache {
            inner,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            evictions,
        }
    }

    pub async fn get(&self, key: &str) -> Option<CacheEntry> {
        match self.inner.get(key).await {
            Some(e) => {
                self.hits.fetch_add(1, Ordering::Relaxed);
                Some(e)
            }
            None => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    pub async fn insert(&self, key: String, body: Bytes, content_type: String, status: u16) {
        self.inner
            .insert(key, CacheEntry { body, content_type, status })
            .await;
    }

    pub fn clear(&self) {
        self.inner.invalidate_all();
    }

    pub fn stats(&self) -> CacheStats {
        CacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            size: self.inner.entry_count() as usize,
            evictions: self.evictions.load(Ordering::Relaxed),
        }
    }

    #[cfg(test)]
    async fn run_pending_tasks(&self) {
        self.inner.run_pending_tasks().await;
    }
}

#[derive(Debug, serde::Serialize)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub size: usize,
    pub evictions: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(body: &str) -> (Bytes, String, u16) {
        (Bytes::from(body.to_string()), "application/json".into(), 200)
    }

    #[tokio::test]
    async fn basic_hit_miss() {
        let c = Cache::new(Duration::from_secs(300), 100);
        assert!(c.get("k1").await.is_none());
        let (b, ct, s) = make_entry("{}");
        c.insert("k1".into(), b, ct, s).await;
        assert!(c.get("k1").await.is_some());
        let stats = c.stats();
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);
    }

    #[tokio::test]
    async fn ttl_expiry() {
        let c = Cache::new(Duration::from_millis(50), 100);
        let (b, ct, s) = make_entry("{}");
        c.insert("k1".into(), b, ct, s).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        c.run_pending_tasks().await;
        assert!(c.get("k1").await.is_none());
    }

    #[tokio::test]
    async fn eviction_at_capacity() {
        let c = Cache::new(Duration::from_secs(300), 2);
        let (b1, ct1, s1) = make_entry("1");
        let (b2, ct2, s2) = make_entry("2");
        let (b3, ct3, s3) = make_entry("3");

        c.insert("k1".into(), b1, ct1, s1).await;
        c.insert("k2".into(), b2, ct2, s2).await;
        c.insert("k3".into(), b3, ct3, s3).await;
        c.run_pending_tasks().await;

        // moka uses TinyLFU, not pure LRU — we don't assert which entry was evicted,
        // just that the cache respects its capacity and the eviction counter fired.
        assert!(c.stats().size <= 2, "cache exceeded max_capacity");
        assert!(c.stats().evictions >= 1, "at least one eviction should have occurred");
    }

    #[tokio::test]
    async fn clear_flushes_all() {
        let c = Cache::new(Duration::from_secs(300), 100);
        let (b, ct, s) = make_entry("{}");
        c.insert("k1".into(), b.clone(), ct.clone(), s).await;
        c.insert("k2".into(), b, ct, s).await;
        c.clear();
        c.run_pending_tasks().await;
        assert_eq!(c.stats().size, 0);
    }
}
