//! Adapter for applications whose render backends require file paths. Logical
//! cache paths are SQLite keys; only requested images are materialized in a
//! private temporary directory owned by the session. Never register user data.
use super::{CacheStats, DiskCache};
use anyhow::{Context, Result};
use std::{
  collections::HashMap,
  path::{Path, PathBuf},
  sync::{Arc, LazyLock, Mutex, Weak},
};
static SESSIONS: LazyLock<Mutex<Vec<Weak<CachePaths>>>> = LazyLock::new(|| Mutex::new(Vec::new()));

/// A caller-defined mapping from logical paths to SQLite blobs.
/// First matching rule wins. Materialized entries stay readable for the session.
#[derive(Clone)]
pub struct NamespaceSpec {
  pub prefix: PathBuf,
  pub namespace: String,
  pub extension: Option<String>,
  pub materialize: bool,
}
impl NamespaceSpec {
  pub fn new(prefix: &str, namespace: &str, materialize: bool) -> Self {
    Self {
      prefix: prefix.into(),
      namespace: namespace.into(),
      extension: None,
      materialize,
    }
  }
  pub fn with_extension(mut self, extension: &str) -> Self {
    self.extension = Some(extension.into());
    self
  }
}

/// Lifetime guard for registered logical cache paths and temporary image files.
pub struct CachePaths {
  root: PathBuf,
  namespaces: Vec<NamespaceSpec>,
  pub database: DiskCache,
  max_bytes: u64,
  images: Mutex<HashMap<PathBuf, PathBuf>>,
}
impl CachePaths {
  /// The caller retains the returned guard for the application's lifetime.
  pub fn new(root: &Path, namespaces: &[NamespaceSpec], max_bytes: u64) -> Result<Arc<Self>> {
    std::fs::create_dir_all(root)?;
    let root = std::path::absolute(root)?;
    let session = Arc::new(Self {
      database: DiskCache::open(&root)?,
      root,
      namespaces: namespaces.to_vec(),
      max_bytes,
      images: Mutex::new(HashMap::new()),
    });
    let mut sessions = SESSIONS.lock().unwrap();
    sessions.retain(|s| s.strong_count() > 0);
    sessions.push(Arc::downgrade(&session));
    Ok(session)
  }
  fn key(&self, path: &Path) -> Option<(String, String, bool)> {
    let path = std::path::absolute(path).ok()?;
    let relative = path.strip_prefix(&self.root).ok()?;
    if relative
      .components()
      .any(|c| matches!(c, std::path::Component::ParentDir))
    {
      return None;
    }
    self.namespaces.iter().find_map(|spec| {
      let key = relative.strip_prefix(&spec.prefix).ok()?;
      if let Some(extension) = &spec.extension
        && key.extension().and_then(|e| e.to_str()) != Some(extension.as_str())
      {
        return None;
      }
      Some((
        spec.namespace.clone(),
        key.to_string_lossy().into_owned(),
        spec.materialize,
      ))
    })
  }

  pub fn clear(&self) -> Result<CacheStats> {
    self.images.lock().unwrap().clear();
    self.database.clear(None)
  }
  pub fn trim(&self) -> Result<CacheStats> {
    self.database.trim(None, self.max_bytes)
  }
}
fn lookup(path: &Path) -> Option<(Arc<CachePaths>, String, String, bool)> {
  SESSIONS
    .lock()
    .unwrap()
    .iter()
    .filter_map(Weak::upgrade)
    .find_map(|s| {
      s.key(path)
        .map(|(ns, k, materialize)| (s, ns, k, materialize))
    })
}
pub fn session(root: &Path) -> Option<Arc<CachePaths>> {
  SESSIONS
    .lock()
    .unwrap()
    .iter()
    .filter_map(Weak::upgrade)
    .find(|s| std::path::absolute(root).is_ok_and(|root| s.root == root))
}
pub fn is_managed(path: &Path) -> bool {
  lookup(path).is_some()
}
pub fn contains(path: &Path) -> bool {
  if let Some((s, ns, k, _)) = lookup(path) {
    s.images
      .lock()
      .unwrap()
      .get(path)
      .is_some_and(|file| file.is_file())
      || s.database.contains(&ns, &k).unwrap_or(false)
  } else {
    path.exists()
  }
}
pub fn read_sync(path: &Path) -> Result<Vec<u8>> {
  if let Some((s, ns, k, _)) = lookup(path) {
    s.database.get(&ns, &k)?.context("cache miss")
  } else {
    Ok(std::fs::read(path)?)
  }
}
pub async fn read(path: &Path) -> Result<Vec<u8>> {
  let path = path.to_owned();
  tokio::task::spawn_blocking(move || read_sync(&path)).await?
}
pub fn write_sync(path: &Path, bytes: &[u8]) -> Result<()> {
  let (s, ns, k, materialize) = lookup(path).context("path is not registered as cache data")?;
  if materialize {
    let file = s.database.materialize_bytes(bytes)?;
    s.images.lock().unwrap().insert(path.to_owned(), file);
  }
  s.database.put_bounded(&ns, &k, bytes, s.max_bytes)?;
  Ok(())
}
pub async fn write(path: &Path, bytes: &[u8]) -> Result<()> {
  let path = path.to_owned();
  let bytes = bytes.to_vec();
  tokio::task::spawn_blocking(move || write_sync(&path, &bytes)).await?
}
pub fn materialize(path: &Path) -> Result<PathBuf> {
  if let Some((s, ns, k, _)) = lookup(path) {
    if let Some(file) = s
      .images
      .lock()
      .unwrap()
      .get(path)
      .filter(|file| file.is_file())
    {
      return Ok(file.clone());
    }
    let file = s.database.materialize(&ns, &k)?.context("cache miss")?;
    s.images
      .lock()
      .unwrap()
      .insert(path.to_owned(), file.clone());
    Ok(file)
  } else {
    Ok(path.to_owned())
  }
}
pub fn remove(path: &Path) -> Result<()> {
  if let Some((s, ns, k, _)) = lookup(path) {
    s.images.lock().unwrap().remove(path);
    s.database.remove(&ns, &k)
  } else {
    Ok(std::fs::remove_file(path)?)
  }
}
/// Stable identity for content-addressed temporary images, independent of PID.
pub fn identity(path: &Path) -> PathBuf {
  for s in SESSIONS.lock().unwrap().iter().filter_map(Weak::upgrade) {
    if let Ok(relative) = path.strip_prefix(s.database.materialized_root()) {
      return PathBuf::from("img-tui-blob").join(relative);
    }
  }
  path.to_owned()
}

pub fn write_file_sync<E: std::fmt::Display>(
  path: &Path,
  write: impl FnOnce(&Path) -> Result<(), E>,
) -> Result<()> {
  let temp = tempfile::NamedTempFile::new()?;
  write(temp.path()).map_err(|e| anyhow::anyhow!("{e}"))?;
  write_sync(path, &std::fs::read(temp.path())?)
}
