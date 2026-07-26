use std::{cell::Cell, collections::BTreeMap, fmt, time::Duration};

/// Caller-provided monotonic time for deterministic cache behavior.
pub trait Clock {
    fn now_millis(&self) -> u64;
}

/// A manually advanced clock suitable for tests and simulations.
#[derive(Debug, Default)]
pub struct ManualClock {
    now_millis: Cell<u64>,
}

impl ManualClock {
    #[must_use]
    pub const fn new(now_millis: u64) -> Self {
        Self {
            now_millis: Cell::new(now_millis),
        }
    }

    pub fn set(&self, now_millis: u64) {
        self.now_millis.set(now_millis);
    }

    pub fn advance(&self, duration: Duration) {
        let millis = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
        self.now_millis
            .set(self.now_millis.get().saturating_add(millis));
    }
}

impl Clock for ManualClock {
    fn now_millis(&self) -> u64 {
        self.now_millis.get()
    }
}

/// Invalid bounded-cache configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CacheError {
    ZeroCapacity,
}

impl fmt::Display for CacheError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("cache capacity must be greater than zero")
    }
}

impl std::error::Error for CacheError {}

#[derive(Clone, Debug)]
struct Entry<V> {
    value: V,
    expires_at: u64,
    last_used: u64,
}

/// A TTL cache with deterministic least-recently-used eviction.
#[derive(Clone, Debug)]
pub struct BoundedCache<K, V> {
    capacity: usize,
    ttl_millis: u64,
    access: u64,
    entries: BTreeMap<K, Entry<V>>,
}

impl<K: Clone + Ord, V> BoundedCache<K, V> {
    /// Creates a cache with fixed capacity and TTL.
    ///
    /// # Errors
    ///
    /// Returns [`CacheError::ZeroCapacity`] when `capacity` is zero.
    pub fn new(capacity: usize, ttl: Duration) -> Result<Self, CacheError> {
        if capacity == 0 {
            return Err(CacheError::ZeroCapacity);
        }
        Ok(Self {
            capacity,
            ttl_millis: u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX),
            access: 0,
            entries: BTreeMap::new(),
        })
    }

    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn insert(&mut self, key: K, value: V, clock: &impl Clock) -> Option<V> {
        let now = clock.now_millis();
        self.purge_expired(now);
        let access = self.next_access();
        let replaced = self
            .entries
            .insert(
                key,
                Entry {
                    value,
                    expires_at: now.saturating_add(self.ttl_millis),
                    last_used: access,
                },
            )
            .map(|entry| entry.value);
        if self.entries.len() > self.capacity {
            self.evict_lru();
        }
        replaced
    }

    pub fn get(&mut self, key: &K, clock: &impl Clock) -> Option<&V> {
        let now = clock.now_millis();
        if self
            .entries
            .get(key)
            .is_some_and(|entry| now >= entry.expires_at)
        {
            self.entries.remove(key);
            return None;
        }
        let access = self.next_access();
        let entry = self.entries.get_mut(key)?;
        entry.last_used = access;
        Some(&entry.value)
    }

    pub fn remove(&mut self, key: &K) -> Option<V> {
        self.entries.remove(key).map(|entry| entry.value)
    }

    pub fn len(&mut self, clock: &impl Clock) -> usize {
        self.purge_expired(clock.now_millis());
        self.entries.len()
    }

    pub fn is_empty(&mut self, clock: &impl Clock) -> bool {
        self.len(clock) == 0
    }

    fn next_access(&mut self) -> u64 {
        let next = self.access;
        self.access = self.access.saturating_add(1);
        next
    }

    fn purge_expired(&mut self, now: u64) {
        self.entries.retain(|_, entry| now < entry.expires_at);
    }

    fn evict_lru(&mut self) {
        if let Some(key) = self
            .entries
            .iter()
            .min_by_key(|(_, entry)| entry.last_used)
            .map(|(key, _)| key.clone())
        {
            self.entries.remove(&key);
        }
    }
}
