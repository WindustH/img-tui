use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::{
  fs,
  path::{Path, PathBuf},
  sync::{Arc, Mutex},
  time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Default, Clone, Copy)]
pub struct CacheStats {
  pub before_bytes: u64,
  pub after_bytes: u64,
  pub removed_files: usize,
  pub removed_bytes: u64,
}

/// Cloneable SQLite BLOB store. All operations are short transactions and
/// serialize per connection; SQLite coordinates independent processes in WAL.
#[derive(Clone)]
pub struct DiskCache {
  inner: Arc<Inner>,
}
struct Inner {
  connection: Mutex<Connection>,
  materialized: tempfile::TempDir,
  materialize_lock: Mutex<()>,
}
impl DiskCache {
  pub fn open(directory: &Path) -> Result<Self> {
    fs::create_dir_all(directory)?;
    let connection = Connection::open(directory.join("cache.sqlite3"))?;
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.execute_batch(
      "PRAGMA auto_vacuum=INCREMENTAL; PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
      CREATE TABLE IF NOT EXISTS cache_entries (
        namespace TEXT NOT NULL, key TEXT NOT NULL, payload BLOB NOT NULL,
        size INTEGER NOT NULL, accessed INTEGER NOT NULL,
        PRIMARY KEY(namespace,key));
      CREATE INDEX IF NOT EXISTS cache_lru ON cache_entries(accessed);
      PRAGMA user_version=1;",
    )?;
    Ok(Self {
      inner: Arc::new(Inner {
        connection: Mutex::new(connection),
        materialized: tempfile::Builder::new()
          .prefix("img-tui-cache-")
          .tempdir()?,
        materialize_lock: Mutex::new(()),
      }),
    })
  }
  pub fn get(&self, namespace: &str, key: &str) -> Result<Option<Vec<u8>>> {
    let conn = self.inner.connection.lock().unwrap();
    let bytes = conn
      .query_row(
        "SELECT payload FROM cache_entries WHERE namespace=?1 AND key=?2",
        params![namespace, key],
        |r| r.get(0),
      )
      .optional()?;
    if bytes.is_some() {
      // Rate-limit access writes to one per minute per entry across processes.
      let now = now();
      conn.execute(
        "UPDATE cache_entries SET accessed=?3 WHERE namespace=?1 AND key=?2 AND accessed < ?3-60",
        params![namespace, key, now],
      )?;
    }
    Ok(bytes)
  }
  pub fn contains(&self, namespace: &str, key: &str) -> Result<bool> {
    let conn = self.inner.connection.lock().unwrap();
    Ok(
      conn
        .query_row(
          "SELECT 1 FROM cache_entries WHERE namespace=?1 AND key=?2",
          params![namespace, key],
          |_| Ok(()),
        )
        .optional()?
        .is_some(),
    )
  }
  pub fn put(&self, namespace: &str, key: &str, bytes: &[u8]) -> Result<()> {
    self.inner.connection.lock().unwrap().execute(
      "INSERT INTO cache_entries(namespace,key,payload,size,accessed) VALUES(?1,?2,?3,?4,?5)
       ON CONFLICT(namespace,key) DO UPDATE SET payload=excluded.payload,size=excluded.size,accessed=excluded.accessed",
      params![namespace,key,bytes,bytes.len() as i64,now()])?;
    Ok(())
  }
  /// Store and enforce the live-byte budget in one transaction. Keep the
  /// just-produced entry even when it alone exceeds the budget.
  pub fn put_bounded(
    &self,
    namespace: &str,
    key: &str,
    bytes: &[u8],
    max_bytes: u64,
  ) -> Result<()> {
    let mut conn = self.inner.connection.lock().unwrap();
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute("INSERT INTO cache_entries(namespace,key,payload,size,accessed) VALUES(?1,?2,?3,?4,?5)
      ON CONFLICT(namespace,key) DO UPDATE SET payload=excluded.payload,size=excluded.size,accessed=excluded.accessed",
      params![namespace,key,bytes,bytes.len() as i64,now()])?;
    if max_bytes > 0 {
      let mut total: u64 =
        tx.query_row("SELECT COALESCE(SUM(size),0) FROM cache_entries", [], |r| {
          r.get(0)
        })?;
      let victims = if total > max_bytes {
        let mut stmt = tx.prepare("SELECT namespace,key,size FROM cache_entries WHERE NOT(namespace=?1 AND key=?2) ORDER BY accessed,namespace,key")?;
        stmt
          .query_map(params![namespace, key], |r| {
            Ok((
              r.get::<_, String>(0)?,
              r.get::<_, String>(1)?,
              r.get::<_, u64>(2)?,
            ))
          })?
          .collect::<rusqlite::Result<Vec<_>>>()?
      } else {
        Vec::new()
      };
      for (ns, k, size) in victims {
        if total <= max_bytes {
          break;
        }
        tx.execute(
          "DELETE FROM cache_entries WHERE namespace=?1 AND key=?2",
          params![ns, k],
        )?;
        total = total.saturating_sub(size);
      }
    }
    tx.commit()?;
    Ok(())
  }

  pub fn remove(&self, namespace: &str, key: &str) -> Result<()> {
    self.inner.connection.lock().unwrap().execute(
      "DELETE FROM cache_entries WHERE namespace=?1 AND key=?2",
      params![namespace, key],
    )?;
    Ok(())
  }
  /// namespace=None applies the budget to the entire DB. Zero disables limits.
  /// This limits live payload bytes, not SQLite page/WAL overhead.
  pub fn trim(&self, namespace: Option<&str>, max_bytes: u64) -> Result<CacheStats> {
    self.prune(namespace, if max_bytes == 0 { u64::MAX } else { max_bytes })
  }
  pub fn clear(&self, namespace: Option<&str>) -> Result<CacheStats> {
    self.prune(namespace, 0)
  }
  fn prune(&self, namespace: Option<&str>, max_bytes: u64) -> Result<CacheStats> {
    let mut conn = self.inner.connection.lock().unwrap();
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let mut stmt = tx.prepare("SELECT namespace,key,size FROM cache_entries WHERE (?1 IS NULL OR namespace=?1) ORDER BY accessed,namespace,key")?;
    let rows = stmt
      .query_map([namespace], |r| {
        Ok((
          r.get::<_, String>(0)?,
          r.get::<_, String>(1)?,
          r.get::<_, u64>(2)?,
        ))
      })?
      .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    let before_bytes = rows.iter().map(|r| r.2).sum();
    let mut report = CacheStats {
      before_bytes,
      after_bytes: before_bytes,
      ..CacheStats::default()
    };
    for (ns, key, size) in rows {
      if max_bytes != 0 && report.after_bytes <= max_bytes {
        break;
      }
      tx.execute(
        "DELETE FROM cache_entries WHERE namespace=?1 AND key=?2",
        params![ns, key],
      )?;
      report.after_bytes -= size;
      report.removed_bytes += size;
      report.removed_files += 1;
    }
    tx.commit()?;
    conn.execute_batch("PRAGMA incremental_vacuum(256); PRAGMA wal_checkpoint(PASSIVE);")?;
    Ok(report)
  }
  /// Materialize immutable bytes for file-only consumers. Files are temporary,
  /// content addressed and kept alive by this cache handle, even after pruning.
  pub fn materialize(&self, namespace: &str, key: &str) -> Result<Option<PathBuf>> {
    let Some(bytes) = self.get(namespace, key)? else {
      return Ok(None);
    };
    self.materialize_bytes(&bytes).map(Some)
  }
  pub fn materialize_bytes(&self, bytes: &[u8]) -> Result<PathBuf> {
    let digest = Sha256::digest(bytes)
      .iter()
      .map(|b| format!("{b:02x}"))
      .collect::<String>();
    let extension = image::guess_format(bytes)
      .ok()
      .and_then(|format| format.extensions_str().first().copied())
      .unwrap_or("bin");
    let path = self
      .inner
      .materialized
      .path()
      .join(format!("{digest}.{extension}"));
    let _guard = self.inner.materialize_lock.lock().unwrap();
    if !path.exists() {
      fs::write(&path, bytes).context("writing temporary image cache adapter")?;
      // Content-addressed images have stable timestamps across materializations.
      fs::File::open(&path)?.set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))?;
    }
    Ok(path)
  }
  pub fn materialized_root(&self) -> &Path {
    self.inner.materialized.path()
  }
}
fn now() -> i64 {
  SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .unwrap_or_default()
    .as_secs() as i64
}
