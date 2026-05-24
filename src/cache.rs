//! Per-project, bounded, in-memory secret cache.
//!
//! Implements:
//! - FR-014: cache successful backend retrievals, keyed by `(project,
//!   secret_name)`. The per-project instance hangs off `Project`, so the
//!   key here is just the secret name.
//! - FR-015: in-memory only — `BoundedCache` owns a `HashMap` on the heap
//!   and nothing on this path touches disk.
//! - FR-016: values are wrapped in `secrecy::SecretString`, which zeroizes
//!   on drop. Eviction, expiry, replacement, `clear`, and the implicit drop
//!   on `unregister` (project Arc dropped → cache dropped → entries dropped)
//!   all trigger zeroization.
//!
//! Eviction policy is **drop-oldest-by-`fetched_at`** when an insert would
//! exceed `max_entries`. This is effectively LRU-on-write: re-inserting an
//! existing key refreshes its timestamp. We picked this over strict LRU
//! (which would require tracking read-access too) because reads on the warm
//! cache are the common case and we want them lock-light. OQ-3 in the spec
//! left LRU-vs-bounded open; this implementation picks bounded with FIFO
//! eviction.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use secrecy::{ExposeSecret, SecretString};

/// Per-project secret cache. Cheap to construct; intended to be held inside
/// `Arc<Project>` and shared across the project's connection handlers.
#[derive(Debug)]
pub struct BoundedCache {
    inner: Mutex<Inner>,
}

#[derive(Debug)]
struct Inner {
    map: HashMap<String, CacheEntry>,
    max_entries: usize,
    default_ttl: Duration,
}

struct CacheEntry {
    value: SecretString,
    fetched_at: Instant,
    ttl: Duration,
}

impl std::fmt::Debug for CacheEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never let the SecretString reach a Debug formatter. SecretString
        // itself redacts, but being explicit here prevents accidental leaks
        // if the inner type is ever swapped.
        f.debug_struct("CacheEntry")
            .field("value", &"[REDACTED]")
            .field("fetched_at", &self.fetched_at)
            .field("ttl", &self.ttl)
            .finish()
    }
}

impl CacheEntry {
    fn is_expired(&self, now: Instant) -> bool {
        now.duration_since(self.fetched_at) >= self.ttl
    }

    fn remaining_ttl(&self, now: Instant) -> Duration {
        self.ttl.saturating_sub(now.duration_since(self.fetched_at))
    }
}

/// Successful cache lookup. The caller materializes the plaintext into the
/// outgoing `GetResponse` (the unavoidable plaintext boundary); until then
/// the value stays inside a `SecretString` wrapper.
#[derive(Debug)]
pub struct CacheHit {
    pub value: SecretString,
    pub ttl_seconds: u32,
}

impl BoundedCache {
    pub fn new(max_entries: usize, default_ttl: Duration) -> Self {
        Self {
            inner: Mutex::new(Inner {
                map: HashMap::new(),
                max_entries,
                default_ttl,
            }),
        }
    }

    /// Look up `name`. Returns `None` for misses and for entries past their
    /// TTL (which are dropped — and therefore zeroized — as a side effect).
    pub fn get(&self, name: &str) -> Option<CacheHit> {
        let mut inner = self.lock();
        let now = Instant::now();
        let entry = inner.map.get(name)?;
        if entry.is_expired(now) {
            inner.map.remove(name);
            return None;
        }
        let ttl_seconds = entry.remaining_ttl(now).as_secs() as u32;
        // Re-wrap so the hit owns a fresh SecretString; the cache retains
        // the original. Both will zeroize independently when dropped.
        let value = SecretString::from(entry.value.expose_secret().to_string());
        Some(CacheHit { value, ttl_seconds })
    }

    /// Insert `name => value`. `ttl` falls back to the cache's configured
    /// default. If the cache is at `max_entries`, the oldest entry by
    /// `fetched_at` is evicted (and zeroized).
    pub fn insert(&self, name: String, value: SecretString, ttl: Option<Duration>) {
        let mut inner = self.lock();
        let ttl = ttl.unwrap_or(inner.default_ttl);
        // If this key already exists, removing it first means we don't
        // accidentally evict some *other* key when the cache is at cap.
        let replaced = inner.map.remove(&name).is_some();
        if !replaced && inner.map.len() >= inner.max_entries {
            evict_oldest(&mut inner.map);
        }
        inner.map.insert(
            name,
            CacheEntry {
                value,
                fetched_at: Instant::now(),
                ttl,
            },
        );
    }

    /// Drop every entry. Used on `unregister` (defense-in-depth — dropping
    /// the project's `Arc` would do the same) and may be useful for testing.
    pub fn clear(&self) {
        self.lock().map.clear();
    }

    /// Count of currently-held entries. Includes expired entries that
    /// haven't been observed yet (lazy expiry).
    pub fn len(&self) -> usize {
        self.lock().map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().map.is_empty()
    }

    /// Update the cache's bounds. Applies to **new** inserts only — current
    /// entries are not evicted to fit a smaller cap. Called from
    /// `ProjectRegistry::reload` when the manifest's `[cache]` block changes.
    pub fn set_config(&self, max_entries: usize, default_ttl: Duration) {
        let mut inner = self.lock();
        inner.max_entries = max_entries;
        inner.default_ttl = default_ttl;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A poisoned mutex here means a previous insert/get panicked while
        // holding the lock — the cache state is suspect. We propagate by
        // panicking too; the per-project task would restart on a future
        // re-register. Unwrap is acceptable for a cache that holds no
        // resources outside the heap.
        self.inner.lock().expect("cache mutex poisoned")
    }
}

fn evict_oldest(map: &mut HashMap<String, CacheEntry>) {
    // Linear scan is fine: max_entries is bounded (1..=1024 per the
    // manifest schema) and eviction is uncommon vs. lookup.
    let oldest = map
        .iter()
        .min_by_key(|(_, e)| e.fetched_at)
        .map(|(k, _)| k.clone());
    if let Some(k) = oldest {
        map.remove(&k);
    }
}

// -------------------------------------------------------------------------
// Tests
// -------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make(max: usize, default_ttl: Duration) -> BoundedCache {
        BoundedCache::new(max, default_ttl)
    }

    #[test]
    fn miss_on_empty_cache() {
        let c = make(4, Duration::from_secs(60));
        assert!(c.get("X").is_none());
        assert_eq!(c.len(), 0);
    }

    #[test]
    fn hit_returns_value_and_ttl() {
        let c = make(4, Duration::from_secs(60));
        c.insert("FOO".into(), SecretString::from("bar".to_string()), None);
        let hit = c.get("FOO").unwrap();
        assert_eq!(hit.value.expose_secret(), "bar");
        // ttl_seconds should be close to the default (60s); allow the small
        // amount of clock advance between insert and get.
        assert!(hit.ttl_seconds >= 59 && hit.ttl_seconds <= 60);
    }

    #[test]
    fn expired_entry_is_dropped_on_get() {
        let c = make(4, Duration::from_secs(60));
        c.insert(
            "FOO".into(),
            SecretString::from("bar".to_string()),
            Some(Duration::ZERO),
        );
        assert_eq!(c.len(), 1);
        assert!(c.get("FOO").is_none(), "ZERO-ttl entry must miss");
        assert_eq!(c.len(), 0, "expired entry must be removed on get");
    }

    #[test]
    fn insert_at_cap_evicts_oldest() {
        let c = make(2, Duration::from_secs(60));
        c.insert("A".into(), SecretString::from("1".to_string()), None);
        std::thread::sleep(Duration::from_millis(2));
        c.insert("B".into(), SecretString::from("2".to_string()), None);
        std::thread::sleep(Duration::from_millis(2));
        c.insert("C".into(), SecretString::from("3".to_string()), None);

        // A was oldest → evicted.
        assert!(c.get("A").is_none(), "A should have been evicted");
        assert!(c.get("B").is_some());
        assert!(c.get("C").is_some());
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn reinsert_replaces_in_place_without_evicting_others() {
        let c = make(2, Duration::from_secs(60));
        c.insert("A".into(), SecretString::from("1".to_string()), None);
        c.insert("B".into(), SecretString::from("2".to_string()), None);
        // Re-inserting A while at cap must not evict B.
        c.insert(
            "A".into(),
            SecretString::from("1-updated".to_string()),
            None,
        );
        assert_eq!(c.get("A").unwrap().value.expose_secret(), "1-updated");
        assert_eq!(c.get("B").unwrap().value.expose_secret(), "2");
    }

    #[test]
    fn clear_drops_all_entries() {
        let c = make(4, Duration::from_secs(60));
        c.insert("A".into(), SecretString::from("1".to_string()), None);
        c.insert("B".into(), SecretString::from("2".to_string()), None);
        c.clear();
        assert!(c.is_empty());
        assert!(c.get("A").is_none());
    }

    #[test]
    fn set_config_applies_to_new_inserts_only() {
        let c = make(4, Duration::from_secs(60));
        c.insert("A".into(), SecretString::from("1".to_string()), None);
        // A was inserted with the original 60s default.
        let a = c.get("A").unwrap();
        assert!(a.ttl_seconds >= 59);

        // Reduce default TTL; existing entries keep their original TTL.
        c.set_config(4, Duration::from_secs(10));
        let a = c.get("A").unwrap();
        assert!(a.ttl_seconds >= 59, "existing entry must keep original ttl");

        // New insert picks up the new default.
        c.insert("B".into(), SecretString::from("2".to_string()), None);
        let b = c.get("B").unwrap();
        assert!(b.ttl_seconds <= 10);
    }

    #[test]
    fn set_config_smaller_cap_does_not_actively_evict() {
        let c = make(4, Duration::from_secs(60));
        c.insert("A".into(), SecretString::from("1".to_string()), None);
        c.insert("B".into(), SecretString::from("2".to_string()), None);
        c.insert("C".into(), SecretString::from("3".to_string()), None);
        c.set_config(1, Duration::from_secs(60));
        // Existing entries stay; documented behavior.
        assert_eq!(c.len(), 3);

        // But the next insert will evict back down to (new_cap - 1) for the
        // newcomer? No — we only evict one per insert. So len becomes 3
        // again (3 existing minus 1 evicted + 1 new). Documented behavior.
        c.insert("D".into(), SecretString::from("4".to_string()), None);
        assert_eq!(c.len(), 3);
    }

    #[test]
    fn explicit_ttl_overrides_default() {
        let c = make(4, Duration::from_secs(60));
        c.insert(
            "FOO".into(),
            SecretString::from("bar".to_string()),
            Some(Duration::from_secs(5)),
        );
        let hit = c.get("FOO").unwrap();
        assert!(hit.ttl_seconds <= 5);
    }

    #[test]
    fn debug_redacts_value() {
        // CacheEntry::Debug must not surface the underlying secret. Test
        // via the cache's Debug, which prints the inner map.
        let c = make(4, Duration::from_secs(60));
        c.insert(
            "FOO".into(),
            SecretString::from("super-secret".to_string()),
            None,
        );
        let dbg = format!("{c:?}");
        assert!(
            !dbg.contains("super-secret"),
            "cache Debug leaked secret: {dbg}"
        );
        assert!(dbg.contains("[REDACTED]"));
    }
}
