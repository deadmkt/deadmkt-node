// #10 Path 1: per-node is_active cache.
//
// Keeps recently-observed escrow::is_active(nft_id) results so the settle
// path can skip matches involving known-dead peers without hitting RPC per
// match. Pure data: no chain access, no async — caller fetches and inserts.
//
// NOT used to alter matching output (that would break determinism). Only
// consumed by the designated gas_payer_nft_id node at settle submission.

use std::collections::HashMap;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Entry {
    is_active: bool,
    fetched_at: Instant,
}

pub struct IsActiveCache {
    entries: HashMap<u64, Entry>,
    ttl: Duration,
    max_entries: usize,
}

impl IsActiveCache {
    pub fn new(ttl: Duration, max_entries: usize) -> Self {
        Self {
            entries: HashMap::new(),
            ttl,
            max_entries: max_entries.max(1),
        }
    }

    /// Returns the cached value iff it exists and is younger than TTL.
    pub fn get(&self, nft_id: u64) -> Option<bool> {
        let e = self.entries.get(&nft_id)?;
        if e.fetched_at.elapsed() < self.ttl {
            Some(e.is_active)
        } else {
            None
        }
    }

    /// Record a fresh observation. Evicts oldest entry if over capacity.
    pub fn insert(&mut self, nft_id: u64, is_active: bool) {
        if self.entries.len() >= self.max_entries && !self.entries.contains_key(&nft_id) {
            if let Some(&oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.fetched_at)
                .map(|(k, _)| k)
            {
                self.entries.remove(&oldest);
            }
        }
        self.entries.insert(
            nft_id,
            Entry { is_active, fetched_at: Instant::now() },
        );
    }

    /// Force next get() to miss. Used on escrow::Reactivated events.
    pub fn invalidate(&mut self, nft_id: u64) {
        self.entries.remove(&nft_id);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// (active, inactive) counts of non-expired entries. For telemetry.
    pub fn live_counts(&self) -> (usize, usize) {
        let (mut a, mut i) = (0, 0);
        for e in self.entries.values() {
            if e.fetched_at.elapsed() < self.ttl {
                if e.is_active { a += 1 } else { i += 1 }
            }
        }
        (a, i)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn t_cache_01_basic_hit() {
        let mut c = IsActiveCache::new(Duration::from_secs(10), 100);
        c.insert(5, true);
        assert_eq!(c.get(5), Some(true));
        c.insert(5, false);
        assert_eq!(c.get(5), Some(false));
    }

    #[test]
    fn t_cache_02_miss_unknown() {
        let c = IsActiveCache::new(Duration::from_secs(10), 100);
        assert_eq!(c.get(99), None);
    }

    #[test]
    fn t_cache_03_ttl_expiry() {
        let mut c = IsActiveCache::new(Duration::from_millis(20), 100);
        c.insert(5, true);
        assert_eq!(c.get(5), Some(true));
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(c.get(5), None, "should expire after TTL");
    }

    #[test]
    fn t_cache_04_invalidate() {
        let mut c = IsActiveCache::new(Duration::from_secs(10), 100);
        c.insert(5, true);
        assert_eq!(c.get(5), Some(true));
        c.invalidate(5);
        assert_eq!(c.get(5), None);
    }

    #[test]
    fn t_cache_05_lru_eviction() {
        let mut c = IsActiveCache::new(Duration::from_secs(10), 3);
        c.insert(1, true);
        std::thread::sleep(Duration::from_millis(2));
        c.insert(2, true);
        std::thread::sleep(Duration::from_millis(2));
        c.insert(3, true);
        assert_eq!(c.len(), 3);
        // Inserting a 4th evicts the oldest (1).
        c.insert(4, true);
        assert_eq!(c.len(), 3);
        assert_eq!(c.get(1), None);
        assert_eq!(c.get(4), Some(true));
    }

    #[test]
    fn t_cache_06_live_counts() {
        let mut c = IsActiveCache::new(Duration::from_secs(10), 100);
        c.insert(1, true);
        c.insert(2, true);
        c.insert(3, false);
        assert_eq!(c.live_counts(), (2, 1));
    }

    #[test]
    fn t_cache_07_refresh_same_key_no_evict() {
        // Re-inserting an existing key must not evict a different entry.
        let mut c = IsActiveCache::new(Duration::from_secs(10), 2);
        c.insert(1, true);
        c.insert(2, false);
        c.insert(1, false); // refresh key 1
        assert_eq!(c.len(), 2);
        assert_eq!(c.get(1), Some(false));
        assert_eq!(c.get(2), Some(false));
    }
}
