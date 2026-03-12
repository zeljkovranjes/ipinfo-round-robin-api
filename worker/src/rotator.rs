// Same round-robin logic as server/src/rotator.rs.
// Differences:
//   - std::sync::Mutex instead of parking_lot::Mutex (no parking_lot in WASM)
//   - web_time::Instant instead of std::time::Instant (std Instant panics in wasm32)

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use web_time::{Duration, Instant};

use crate::config::mask_key;

struct KeyState {
    key: String,
    cooling_until: Option<Instant>,
    requests: usize,
}

impl KeyState {
    fn is_available(&self) -> bool {
        match self.cooling_until {
            None => true,
            Some(until) => Instant::now() >= until,
        }
    }
}

pub struct Rotator {
    keys: Vec<Arc<Mutex<KeyState>>>,
    counter: AtomicUsize,
    cooldown: Duration,
}

impl Rotator {
    pub fn new(keys: Vec<String>, cooldown_seconds: u64) -> Self {
        let keys = keys
            .into_iter()
            .map(|k| Arc::new(Mutex::new(KeyState { key: k, cooling_until: None, requests: 0 })))
            .collect();

        Rotator {
            keys,
            counter: AtomicUsize::new(0),
            cooldown: Duration::from_secs(cooldown_seconds),
        }
    }

    /// Returns the slot index and key for the next available key,
    /// or `None` if all are cooling down.
    pub fn next_key(&self) -> Option<(usize, String)> {
        let n = self.keys.len();
        let start = self.counter.fetch_add(1, Ordering::Relaxed) % n;

        for i in 0..n {
            let idx = (start + i) % n;
            let mut state = self.keys[idx].lock().unwrap();
            if state.is_available() {
                state.requests += 1;
                return Some((idx, state.key.clone()));
            }
        }
        None
    }

    /// Mark a key as cooling down (called on 429 or 401).
    pub fn mark_cooldown(&self, key: &str) {
        for slot in &self.keys {
            let mut state = slot.lock().unwrap();
            if state.key == key {
                state.cooling_until = Some(Instant::now() + self.cooldown);
                return;
            }
        }
    }

    /// Key stats for /health and /stats endpoints.
    pub fn stats(&self) -> RotatorStats {
        let (mut total, mut active, mut cooling) = (0, 0, 0);
        let mut keys = Vec::with_capacity(self.keys.len());
        for slot in &self.keys {
            let state = slot.lock().unwrap();
            total += 1;
            let is_active = state.is_available();
            if is_active { active += 1; } else { cooling += 1; }
            keys.push(KeyStat {
                id: mask_key(&state.key),
                requests: state.requests,
                status: if is_active { "active" } else { "cooling" },
            });
        }
        RotatorStats { total, active, cooling, keys }
    }
}

#[derive(Debug, serde::Serialize)]
pub struct KeyStat {
    pub id: String,
    pub requests: usize,
    pub status: &'static str,
}

#[derive(Debug, serde::Serialize)]
pub struct RotatorStats {
    pub total: usize,
    pub active: usize,
    pub cooling: usize,
    pub keys: Vec<KeyStat>,
}
