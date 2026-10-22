//! The SQLite store: every request's metadata forever, bodies and raw hardware samples for a retention
//! window, findings, alerts, sessions and the models seen. One writer task owns the only writing
//! connection and applies commands in batches; readers use a small pool of read-only connections in WAL
//! mode, so a dashboard query never blocks the proxy's inserts. The schema is versioned and additive.

pub mod read;
pub mod retention;
pub mod rows;
pub mod schema;
pub mod writer;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use rusqlite::Connection;

pub use read::{ReadPool, RequestFilter, RequestPage, RequestSummary, SeriesPoint, SeriesQuery};
pub use retention::{RetentionPolicy, RetentionReport};
pub use writer::StoreHandle;

/// The writer handle and the read pool for one database file.
#[derive(Clone)]
pub struct Store {
    pub path: PathBuf,
    pub writer: StoreHandle,
    pub reads: Arc<ReadPool>,
}

/// Opens (creating and migrating as needed) the database at `path` and starts the writer task.
pub async fn open(path: &Path) -> Result<Store> {
    let owned = path.to_path_buf();
    let conn = tokio::task::spawn_blocking({
        let path = owned.clone();
        move || open_writer_connection(&path)
    })
    .await
    .context("open store")??;
    let writer = writer::spawn(conn, owned.clone());
    let reads = Arc::new(ReadPool::new(owned.clone(), read::DEFAULT_POOL_SIZE));
    Ok(Store {
        path: owned,
        writer,
        reads,
    })
}

/// The one read-write connection: creates the file and its parent, applies the pragmas and migrations.
pub fn open_writer_connection(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let mut conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
    schema::configure(&conn)?;
    schema::migrate(&mut conn)?;
    Ok(conn)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) fn temp_db_path(dir: &tempfile::TempDir) -> PathBuf {
    // A counter, not the clock: macOS SystemTime only resolves microseconds, so parallel tests could
    // otherwise share a file. The temp dir is per test anyway; the counter guards reuse within one.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    dir.path().join(format!(
        "gateway-{}.db",
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ))
}
