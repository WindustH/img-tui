use moka::{policy::EvictionPolicy, sync::Cache};
use std::{
  borrow::Borrow,
  hash::Hash,
  sync::{Arc, Mutex},
};

/// Byte-weighted LRU. Zero means unlimited. A single oversized result is kept
/// separately so a render larger than the budget can still reach the screen.
pub struct MemoryCache<K, V> {
  cache: Cache<K, (Arc<V>, u32)>,
  oversized: Mutex<Option<(K, Arc<V>)>>,
  max_bytes: u64,
}
impl<K: Eq + Hash + Clone + Send + Sync + 'static, V: Send + Sync + 'static> MemoryCache<K, V> {
  pub fn new(max_bytes: u64) -> Self {
    let cache = Cache::builder()
      .max_capacity(if max_bytes == 0 { u64::MAX } else { max_bytes })
      .weigher(|_: &K, value: &(Arc<V>, u32)| value.1)
      .eviction_policy(EvictionPolicy::lru())
      .build();
    Self {
      cache,
      oversized: Mutex::new(None),
      max_bytes,
    }
  }
  pub fn get<Q>(&self, key: &Q) -> Option<Arc<V>>
  where
    K: Borrow<Q>,
    Q: Eq + Hash + ?Sized,
  {
    if let Some((k, value)) = self.oversized.lock().unwrap().as_ref()
      && k.borrow() == key
    {
      return Some(value.clone());
    }
    self.cache.get(key).map(|(value, _)| value)
  }
  pub fn touch<Q>(&self, key: &Q) -> bool
  where
    K: Borrow<Q>,
    Q: Eq + Hash + ?Sized,
  {
    self.get(key).is_some()
  }
  pub fn contains_key<Q>(&self, key: &Q) -> bool
  where
    K: Borrow<Q>,
    Q: Eq + Hash + ?Sized,
  {
    self
      .oversized
      .lock()
      .unwrap()
      .as_ref()
      .is_some_and(|(k, _)| k.borrow() == key)
      || self.cache.contains_key(key)
  }
  pub fn insert(&self, key: K, value: V, bytes: u64) {
    self.remove(&key);
    if bytes > u64::from(u32::MAX) || (self.max_bytes != 0 && bytes > self.max_bytes) {
      *self.oversized.lock().unwrap() = Some((key, Arc::new(value)));
    } else {
      self
        .cache
        .insert(key, (Arc::new(value), bytes.max(1) as u32));
    }
  }
  pub fn remove<Q>(&self, key: &Q)
  where
    K: Borrow<Q>,
    Q: Eq + Hash + ?Sized,
  {
    let mut large = self.oversized.lock().unwrap();
    if large.as_ref().is_some_and(|(k, _)| k.borrow() == key) {
      *large = None;
    }
    self.cache.invalidate(key);
  }
  pub fn clear(&self) {
    *self.oversized.lock().unwrap() = None;
    self.cache.invalidate_all();
    self.cache.run_pending_tasks();
  }
  pub fn run_pending_tasks(&self) {
    self.cache.run_pending_tasks();
  }
}

impl<K, V> std::fmt::Debug for MemoryCache<K, V> {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("MemoryCache")
      .field("max_bytes", &self.max_bytes)
      .finish_non_exhaustive()
  }
}
