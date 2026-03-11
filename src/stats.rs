use std::sync::atomic::{AtomicU64, Ordering};
use serde::Serialize;

/// Global request counters shared across all handlers.
pub struct Stats {
    pub requests_total: AtomicU64,
    pub requests_proxied: AtomicU64,
    pub requests_cached: AtomicU64,
    pub upstream_errors: AtomicU64,
    pub keys_exhausted: AtomicU64,
}

impl Stats {
    pub fn new() -> Self {
        Stats {
            requests_total: AtomicU64::new(0),
            requests_proxied: AtomicU64::new(0),
            requests_cached: AtomicU64::new(0),
            upstream_errors: AtomicU64::new(0),
            keys_exhausted: AtomicU64::new(0),
        }
    }

    pub fn inc_total(&self) {
        self.requests_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_proxied(&self) {
        self.requests_proxied.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_cached(&self) {
        self.requests_cached.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_upstream_errors(&self) {
        self.upstream_errors.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_keys_exhausted(&self) {
        self.keys_exhausted.fetch_add(1, Ordering::Relaxed);
    }
}

/// Response body for `GET /stats`.
#[derive(Serialize)]
pub struct StatsResponse {
    pub requests_total: u64,
    pub requests_proxied: u64,
    pub requests_cached: u64,
    pub upstream_errors: u64,
    pub keys_exhausted: u64,
    pub cache: crate::cache::CacheStats,
    pub keys: crate::rotator::RotatorStats,
}

/// Response body for `GET /health`.
#[derive(Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub keys: crate::rotator::RotatorStats,
}
