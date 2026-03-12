use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use parking_lot::Mutex;

#[derive(Debug)]
struct KeyState {
    key: String,
    cooling_until: Option<Instant>,
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
            .map(|k| Arc::new(Mutex::new(KeyState { key: k, cooling_until: None })))
            .collect();

        Rotator {
            keys,
            counter: AtomicUsize::new(0),
            cooldown: Duration::from_secs(cooldown_seconds),
        }
    }

    /// Returns the next available key, or `None` if all are cooling down.
    pub fn next_key(&self) -> Option<String> {
        let n = self.keys.len();
        let start = self.counter.fetch_add(1, Ordering::Relaxed) % n;

        // Try each key starting from `start`, wrapping around.
        for i in 0..n {
            let idx = (start + i) % n;
            let state = self.keys[idx].lock();
            if state.is_available() {
                return Some(state.key.clone());
            }
        }
        None
    }

    /// Mark a key as cooling down (called on 429 or 401).
    pub fn mark_cooldown(&self, key: &str) {
        for slot in &self.keys {
            let mut state: parking_lot::MutexGuard<KeyState> = slot.lock();
            if state.key == key {
                state.cooling_until = Some(Instant::now() + self.cooldown);
                return;
            }
        }
    }

    /// Stats for /health endpoint.
    pub fn stats(&self) -> RotatorStats {
        let mut total = 0usize;
        let mut active = 0usize;
        let mut cooling = 0usize;

        for slot in &self.keys {
            let state: parking_lot::MutexGuard<KeyState> = slot.lock();
            total += 1;
            if state.is_available() {
                active += 1;
            } else {
                cooling += 1;
            }
        }

        RotatorStats { total, active, cooling }
    }
}

#[derive(Debug, serde::Serialize)]
pub struct RotatorStats {
    pub total: usize,
    pub active: usize,
    pub cooling: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_robin_order() {
        let r = Rotator::new(vec!["a".into(), "b".into(), "c".into()], 60);
        let k1 = r.next_key().unwrap();
        let k2 = r.next_key().unwrap();
        let k3 = r.next_key().unwrap();
        let k4 = r.next_key().unwrap();
        // Should cycle: a, b, c, a
        assert_eq!(k1, "a");
        assert_eq!(k2, "b");
        assert_eq!(k3, "c");
        assert_eq!(k4, "a");
    }

    #[test]
    fn skip_cooling_key() {
        let r = Rotator::new(vec!["a".into(), "b".into()], 60);
        r.next_key(); // consume slot 0 (a)
        r.mark_cooldown("b");
        // next should skip b and return a
        let k = r.next_key().unwrap();
        assert_eq!(k, "a");
    }

    #[test]
    fn all_cooling_returns_none() {
        let r = Rotator::new(vec!["a".into(), "b".into()], 60);
        r.mark_cooldown("a");
        r.mark_cooldown("b");
        assert!(r.next_key().is_none());
    }

    #[test]
    fn cooldown_recovery() {
        let r = Rotator::new(vec!["a".into()], 0); // 0s cooldown
        r.mark_cooldown("a");
        // With 0s cooldown it should immediately recover
        std::thread::sleep(Duration::from_millis(1));
        assert!(r.next_key().is_some());
    }
}
