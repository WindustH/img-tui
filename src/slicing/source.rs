//! Weighted prepared-image cache with one decode per concurrent source request.
use crate::{
  cache::MemoryCache,
  native_image::{self, PreparedNativeImage},
};
use std::{
  collections::HashMap,
  path::Path,
  sync::{Arc, Mutex, Weak},
};
use tokio::sync::OnceCell;
#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
  identity: String,
  cells: (u16, u16),
  pixels: Option<(u16, u16)>,
}
type Flight = OnceCell<Result<PreparedNativeImage, String>>;
/// Cache of full, scaled sources. Concurrent slices share the same decode;
/// unrelated images can prepare in parallel. Zero budget means unlimited.
pub struct PreparedImageCache {
  images: MemoryCache<Key, PreparedNativeImage>,
  flights: Mutex<HashMap<Key, Weak<Flight>>>,
}
impl PreparedImageCache {
  pub fn new(max_bytes: u64) -> Self {
    Self {
      images: MemoryCache::new(max_bytes),
      flights: Mutex::new(HashMap::new()),
    }
  }
  /// `identity` includes the source revision; dimensions are added by the cache.
  pub async fn prepare(
    &self,
    identity: String,
    path: &Path,
    cells: (u16, u16),
    pixels: Option<(u16, u16)>,
  ) -> Result<PreparedNativeImage, String> {
    let key = Key {
      identity,
      cells,
      pixels,
    };
    if let Some(image) = self.images.get(&key) {
      return Ok((*image).clone());
    }
    let flight = {
      let mut flights = self.flights.lock().unwrap();
      flights.retain(|_, flight| flight.strong_count() > 0);
      if let Some(flight) = flights.get(&key).and_then(Weak::upgrade) {
        flight
      } else {
        let flight = Arc::new(OnceCell::new());
        flights.insert(key.clone(), Arc::downgrade(&flight));
        flight
      }
    };
    flight
      .get_or_init(|| async {
        if let Some(image) = self.images.get(&key) {
          return Ok((*image).clone());
        }
        let image = native_image::prepare_cell_grid(path, cells.0, cells.1, pixels)
          .await
          .map_err(|e| e.to_string())?;
        self.images.insert(key, image.clone(), image.byte_len());
        Ok(image)
      })
      .await
      .clone()
  }
  pub fn clear(&self) {
    self.images.clear();
  }
}
