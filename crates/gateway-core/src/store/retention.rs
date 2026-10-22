//! Retention: bodies past their window, passthrough rows past theirs, raw hardware samples past 48 h and
//! acknowledged alerts past 90 days are deleted in small batches, each its own transaction, so the writer
//! is never held for long. Request metadata and findings are kept: neither holds plaintext. Freed pages go
//! back through `incremental_vacuum`, which the `auto_vacuum` pragma set at creation makes possible.

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use rusqlite::{Connection, params};
use serde::Serialize;

use crate::config::CaptureConfig;

const BATCH: i64 = 500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionPolicy {
    pub body_retention: Duration,
    pub passthrough_retention: Duration,
    pub hardware_raw_retention: Duration,
    pub acked_alert_retention: Duration,
}

impl RetentionPolicy {
    pub fn from_config(capture: &CaptureConfig) -> Self {
        Self {
            body_retention: Duration::days(i64::from(capture.retention_days)),
            passthrough_retention: Duration::hours(i64::from(capture.passthrough_retention_hours)),
            hardware_raw_retention: Duration::hours(i64::from(
                capture.hardware_raw_retention_hours,
            )),
            acked_alert_retention: Duration::days(90),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RetentionReport {
    pub bodies_deleted: usize,
    pub passthrough_records_deleted: usize,
    pub hardware_samples_deleted: usize,
    pub alerts_deleted: usize,
}

pub fn run(
    conn: &mut Connection,
    policy: &RetentionPolicy,
    now: DateTime<Utc>,
) -> Result<RetentionReport> {
    let cutoff = |d: Duration| (now - d).timestamp_millis();
    let report = RetentionReport {
        bodies_deleted: delete_batched(
            conn,
            "DELETE FROM bodies WHERE id IN (SELECT id FROM bodies WHERE created_at < ?1 LIMIT ?2)",
            cutoff(policy.body_retention),
        )?,
        passthrough_records_deleted: delete_batched(
            conn,
            "DELETE FROM request_records WHERE id IN (SELECT id FROM request_records WHERE api_format = 'passthrough' AND started_at < ?1 LIMIT ?2)",
            cutoff(policy.passthrough_retention),
        )?,
        hardware_samples_deleted: delete_batched(
            conn,
            "DELETE FROM hardware_samples WHERE at IN (SELECT at FROM hardware_samples WHERE at < ?1 LIMIT ?2)",
            cutoff(policy.hardware_raw_retention),
        )?,
        alerts_deleted: delete_batched(
            conn,
            "DELETE FROM alerts WHERE id IN (SELECT id FROM alerts WHERE acknowledged_at IS NOT NULL AND acknowledged_at < ?1 LIMIT ?2)",
            cutoff(policy.acked_alert_retention),
        )?,
    };
    conn.execute_batch("PRAGMA incremental_vacuum(2000); PRAGMA wal_checkpoint(PASSIVE);")
        .context("vacuum after retention")?;
    Ok(report)
}

/// Runs `sql` (with `?1` = cutoff, `?2` = batch size) until it deletes nothing, one transaction per batch.
fn delete_batched(conn: &mut Connection, sql: &str, cutoff: i64) -> Result<usize> {
    let mut total = 0usize;
    loop {
        let tx = conn.transaction().context("begin retention batch")?;
        let deleted = tx
            .execute(sql, params![cutoff, BATCH])
            .with_context(|| format!("retention: {sql}"))?;
        tx.commit().context("commit retention batch")?;
        total += deleted;
        if deleted < BATCH as usize {
            return Ok(total);
        }
    }
}
