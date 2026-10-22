//! Schema versions and the migration runner.
//!
//! Each version is one SQL batch, applied inside an `IMMEDIATE` transaction so a second process opening
//! the same file waits for the write lock instead of failing a lock upgrade half-way; the version is
//! re-checked under the lock. Versions are additive: an older binary sharing the file keeps working,
//! because its own batches are no-ops and it names its columns. `schema_version` holds one row per
//! version applied. The pattern comes from the two monitors' `db.rs`.

use anyhow::{Context, Result};
use rusqlite::{Connection, TransactionBehavior};

/// Timestamps are unix milliseconds (`INTEGER`), ids UUIDv7 strings, timings milliseconds (`REAL`).
const V1: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (
    version INTEGER PRIMARY KEY,
    applied_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS sessions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    started_at INTEGER NOT NULL,
    ended_at INTEGER,
    app_version TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS request_records (
    id TEXT PRIMARY KEY,
    session_id INTEGER NOT NULL REFERENCES sessions(id),
    backend TEXT NOT NULL,
    listener TEXT NOT NULL,
    kind TEXT NOT NULL,
    client_ip TEXT NOT NULL,
    user_agent TEXT,
    client_request_id TEXT,
    method TEXT NOT NULL,
    path TEXT NOT NULL,
    query TEXT,
    api_format TEXT NOT NULL,
    streamed INTEGER NOT NULL,
    model_requested TEXT,
    model_resolved TEXT,
    is_cloud INTEGER NOT NULL DEFAULT 0,
    started_at INTEGER NOT NULL,
    first_byte_at INTEGER,
    ended_at INTEGER NOT NULL,
    status INTEGER,
    outcome TEXT NOT NULL,
    failure_source TEXT,
    rejected_input_tokens INTEGER,
    rejected_context_length INTEGER,
    blocked_rule_id TEXT,
    error_summary TEXT,
    prompt_tokens INTEGER,
    gen_tokens INTEGER,
    tokens_estimated INTEGER NOT NULL DEFAULT 0,
    ttft_ms REAL,
    total_ms REAL NOT NULL,
    tokens_per_sec REAL,
    timings_estimated INTEGER NOT NULL DEFAULT 0,
    stop_reason TEXT,
    request_bytes INTEGER NOT NULL,
    response_bytes INTEGER NOT NULL,
    request_headers TEXT NOT NULL,
    response_headers TEXT NOT NULL,
    chunk_timeline TEXT,
    finding_count INTEGER NOT NULL DEFAULT 0,
    max_severity TEXT,
    hypothetical_cost TEXT
);
CREATE INDEX IF NOT EXISTS idx_req_started ON request_records(started_at DESC, id DESC);
CREATE INDEX IF NOT EXISTS idx_req_backend_started ON request_records(backend, started_at);
CREATE INDEX IF NOT EXISTS idx_req_model_started ON request_records(model_resolved, started_at);
CREATE INDEX IF NOT EXISTS idx_req_outcome ON request_records(outcome, started_at);
CREATE INDEX IF NOT EXISTS idx_req_cloud ON request_records(is_cloud, started_at) WHERE is_cloud = 1;
CREATE INDEX IF NOT EXISTS idx_req_format ON request_records(api_format, started_at);
CREATE INDEX IF NOT EXISTS idx_req_session ON request_records(session_id);

CREATE TABLE IF NOT EXISTS bodies (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    request_id TEXT NOT NULL REFERENCES request_records(id) ON DELETE CASCADE,
    direction TEXT NOT NULL,
    content_type TEXT,
    original_bytes INTEGER NOT NULL,
    stored_bytes INTEGER NOT NULL,
    truncated INTEGER NOT NULL,
    redacted INTEGER NOT NULL,
    is_text INTEGER NOT NULL,
    text TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE(request_id, direction)
);
CREATE INDEX IF NOT EXISTS idx_bodies_created ON bodies(created_at);

CREATE VIRTUAL TABLE IF NOT EXISTS bodies_fts USING fts5(text, content='bodies', content_rowid='id', tokenize='unicode61');
CREATE TRIGGER IF NOT EXISTS bodies_ai AFTER INSERT ON bodies BEGIN
    INSERT INTO bodies_fts(rowid, text) VALUES (new.id, new.text);
END;
CREATE TRIGGER IF NOT EXISTS bodies_ad AFTER DELETE ON bodies BEGIN
    INSERT INTO bodies_fts(bodies_fts, rowid, text) VALUES ('delete', old.id, old.text);
END;

CREATE TABLE IF NOT EXISTS findings (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    request_id TEXT NOT NULL REFERENCES request_records(id) ON DELETE CASCADE,
    direction TEXT NOT NULL,
    rule_id TEXT NOT NULL,
    category TEXT NOT NULL,
    severity TEXT NOT NULL,
    location TEXT NOT NULL,
    offset INTEGER,
    length INTEGER NOT NULL,
    fingerprint TEXT NOT NULL,
    preview TEXT NOT NULL,
    action TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_findings_request ON findings(request_id);
CREATE INDEX IF NOT EXISTS idx_findings_created ON findings(created_at DESC);
CREATE INDEX IF NOT EXISTS idx_findings_rule ON findings(rule_id, created_at);

CREATE TABLE IF NOT EXISTS alerts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at INTEGER NOT NULL,
    kind TEXT NOT NULL,
    severity TEXT NOT NULL,
    title TEXT NOT NULL,
    detail TEXT NOT NULL,
    request_id TEXT,
    acknowledged_at INTEGER,
    delivered TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_alerts_created ON alerts(created_at DESC);
CREATE INDEX IF NOT EXISTS idx_alerts_unacked ON alerts(acknowledged_at) WHERE acknowledged_at IS NULL;

CREATE TABLE IF NOT EXISTS hardware_samples (
    at INTEGER PRIMARY KEY,
    system_cpu_pct REAL,
    mem_used INTEGER,
    mem_total INTEGER,
    mem_available INTEGER,
    lms_cpu_pct REAL,
    lms_rss INTEGER,
    lms_procs INTEGER,
    ollama_cpu_pct REAL,
    ollama_rss INTEGER,
    ollama_procs INTEGER,
    gpu_util_pct REAL,
    gpu_renderer_pct REAL,
    gpu_tiler_pct REAL,
    gpu_mem_used INTEGER,
    gpu_mem_alloc INTEGER,
    ane_power_mw REAL
);

CREATE TABLE IF NOT EXISTS hardware_rollups_1m (
    bucket INTEGER PRIMARY KEY,
    samples INTEGER NOT NULL,
    sum_cpu REAL NOT NULL, max_cpu REAL NOT NULL,
    sum_mem_used INTEGER NOT NULL, max_mem_used INTEGER NOT NULL,
    sum_mem_available INTEGER NOT NULL,
    mem_total INTEGER NOT NULL,
    sum_lms_cpu REAL NOT NULL, sum_lms_rss INTEGER NOT NULL,
    sum_ollama_cpu REAL NOT NULL, sum_ollama_rss INTEGER NOT NULL,
    n_gpu INTEGER NOT NULL, sum_gpu REAL NOT NULL, max_gpu REAL NOT NULL,
    n_gpu_mem INTEGER NOT NULL, sum_gpu_mem INTEGER NOT NULL, max_gpu_mem INTEGER NOT NULL,
    n_ane INTEGER NOT NULL, sum_ane REAL NOT NULL, max_ane REAL NOT NULL
);

CREATE TABLE IF NOT EXISTS request_rollups_hourly (
    bucket INTEGER NOT NULL,
    backend TEXT NOT NULL,
    model TEXT NOT NULL,
    is_cloud INTEGER NOT NULL,
    requests INTEGER NOT NULL,
    completed INTEGER NOT NULL,
    failed INTEGER NOT NULL,
    rejected INTEGER NOT NULL,
    blocked INTEGER NOT NULL,
    aborted INTEGER NOT NULL,
    prompt_tokens INTEGER NOT NULL,
    gen_tokens INTEGER NOT NULL,
    sum_ttft_ms REAL NOT NULL,
    ttft_count INTEGER NOT NULL,
    sum_total_ms REAL NOT NULL,
    sum_tps REAL NOT NULL,
    tps_count INTEGER NOT NULL,
    PRIMARY KEY (bucket, backend, model, is_cloud)
);

CREATE TABLE IF NOT EXISTS models_seen (
    backend TEXT NOT NULL,
    model_id TEXT NOT NULL,
    first_seen INTEGER NOT NULL,
    last_seen INTEGER NOT NULL,
    kind TEXT,
    compatibility_type TEXT,
    quantization TEXT,
    max_context_length INTEGER,
    is_cloud INTEGER NOT NULL,
    last_state TEXT NOT NULL,
    PRIMARY KEY (backend, model_id)
);
"#;

/// Every version in order. A new version is a new entry; existing entries are frozen.
pub const MIGRATIONS: &[(u32, &str)] = &[(1, V1)];

pub const CURRENT_VERSION: u32 = 1;

/// Per-connection pragmas. `auto_vacuum` only takes effect on a database that has no tables yet, which
/// is exactly the first open; later it is a no-op. The pragmas run outside any transaction, as
/// `journal_mode` requires.
pub fn configure(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "PRAGMA auto_vacuum = INCREMENTAL;
         PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA busy_timeout = 5000;
         PRAGMA foreign_keys = ON;
         PRAGMA temp_store = MEMORY;",
    )
    .context("configure sqlite connection")?;
    Ok(())
}

/// Read-only connections need the pragmas that apply per connection; the file-level ones are set by the
/// writer.
pub fn configure_reader(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "PRAGMA busy_timeout = 5000;
         PRAGMA foreign_keys = ON;
         PRAGMA temp_store = MEMORY;",
    )
    .context("configure sqlite reader")?;
    Ok(())
}

pub fn current_version(conn: &Connection) -> Result<u32> {
    let has_table: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'schema_version'",
        [],
        |r| r.get(0),
    )?;
    if has_table == 0 {
        return Ok(0);
    }
    let version: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_version",
        [],
        |r| r.get(0),
    )?;
    Ok(u32::try_from(version).unwrap_or(0))
}

/// Applies every version above the file's current one.
pub fn migrate(conn: &mut Connection) -> Result<()> {
    for (version, sql) in MIGRATIONS {
        if current_version(conn)? >= *version {
            continue;
        }
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("begin migration")?;
        // Another process may have migrated while this one waited for the lock.
        if current_version(&tx)? < *version {
            tx.execute_batch(sql)
                .with_context(|| format!("apply schema v{version}"))?;
            tx.execute(
                "INSERT OR IGNORE INTO schema_version (version, applied_at) VALUES (?1, ?2)",
                rusqlite::params![*version, chrono::Utc::now().timestamp_millis()],
            )?;
        }
        tx.commit().context("commit migration")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{open_writer_connection, temp_db_path};

    fn versions(conn: &Connection) -> Vec<i64> {
        let mut stmt = conn
            .prepare("SELECT version FROM schema_version ORDER BY version")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    #[test]
    fn a_fresh_database_is_at_the_current_version_with_wal_and_fts() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_db_path(&dir);
        let conn = open_writer_connection(&path).unwrap();
        assert_eq!(versions(&conn), vec![i64::from(CURRENT_VERSION)]);
        let journal: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(journal, "wal");
        let auto_vacuum: i64 = conn
            .query_row("PRAGMA auto_vacuum", [], |r| r.get(0))
            .unwrap();
        assert_eq!(auto_vacuum, 2, "incremental");
        let fts: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'bodies_fts'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(fts, 1, "FTS5 is compiled into the bundled SQLite");
    }

    #[test]
    fn migrations_are_idempotent_and_a_second_open_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_db_path(&dir);
        let first = open_writer_connection(&path).unwrap();
        let second = open_writer_connection(&path).unwrap();
        assert_eq!(versions(&first), versions(&second));
        assert_eq!(current_version(&second).unwrap(), CURRENT_VERSION);
        drop(first);
        let mut again = Connection::open(&path).unwrap();
        configure(&again).unwrap();
        migrate(&mut again).unwrap();
        assert_eq!(versions(&again), vec![i64::from(CURRENT_VERSION)]);
    }

    #[test]
    fn an_empty_file_reports_version_zero() {
        let dir = tempfile::tempdir().unwrap();
        let conn = Connection::open(temp_db_path(&dir)).unwrap();
        assert_eq!(current_version(&conn).unwrap(), 0);
    }
}
