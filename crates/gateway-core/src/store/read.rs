//! Read side: a small pool of read-only connections and the queries behind the REST API. Every query
//! runs on a blocking thread. Lists paginate by keyset (`started_at`, `id`), so a page is stable while
//! the proxy keeps inserting. Full-text search goes through the FTS5 index over the bodies; the user's
//! words are quoted term by term, so FTS syntax in a search box cannot error.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use rusqlite::types::ToSql;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params, params_from_iter};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use uuid::Uuid;

use super::rows::{self, from_ms, ms};
use super::schema;
use crate::types::{
    Alert, Backend, Body, Direction, Finding, LifetimeTotals, RequestRecord, Session, Severity,
};

pub const DEFAULT_POOL_SIZE: usize = 4;

/// Read-only connections handed out one per query.
pub struct ReadPool {
    path: PathBuf,
    permits: Semaphore,
    idle: Mutex<Vec<Connection>>,
}

impl ReadPool {
    pub fn new(path: PathBuf, size: usize) -> Self {
        Self {
            path,
            permits: Semaphore::new(size.max(1)),
            idle: Mutex::new(Vec::new()),
        }
    }

    /// Runs `query` on a pooled connection on a blocking thread.
    pub async fn run<T, F>(&self, query: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
    {
        let _permit = self.permits.acquire().await.context("read pool closed")?;
        let conn = match self
            .idle
            .lock()
            .map_err(|_| anyhow!("read pool poisoned"))?
            .pop()
        {
            Some(conn) => conn,
            None => open_reader(&self.path)?,
        };
        let (conn, result) = tokio::task::spawn_blocking(move || {
            let result = query(&conn);
            (conn, result)
        })
        .await
        .context("read query panicked")?;
        if let Ok(mut idle) = self.idle.lock() {
            idle.push(conn);
        }
        result
    }
}

fn open_reader(path: &std::path::Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open reader {}", path.display()))?;
    schema::configure_reader(&conn)?;
    Ok(conn)
}

// ---------------------------------------------------------------------------------------------------------
// totals and sessions
// ---------------------------------------------------------------------------------------------------------

/// Sessions and completed inference requests over the whole file.
pub fn lifetime_totals(conn: &Connection) -> Result<LifetimeTotals> {
    conn.query_row(
        "SELECT (SELECT COUNT(*) FROM sessions),
                COUNT(*), COALESCE(SUM(prompt_tokens), 0), COALESCE(SUM(gen_tokens), 0)
         FROM request_records
         WHERE kind = 'http' AND api_format NOT IN ('passthrough', 'cloud_passthrough') AND outcome = 'completed'",
        [],
        |r| {
            Ok(LifetimeTotals {
                session_count: r.get::<_, i64>(0)? as u64,
                request_count: r.get::<_, i64>(1)? as u64,
                prompt_tokens: r.get::<_, i64>(2)? as u64,
                gen_tokens: r.get::<_, i64>(3)? as u64,
            })
        },
    )
    .context("lifetime totals")
}

pub fn list_sessions(conn: &Connection, limit: usize) -> Result<Vec<Session>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, started_at, ended_at, app_version FROM sessions ORDER BY id DESC LIMIT ?1",
    )?;
    let rows = stmt
        .query_map(params![limit as i64], |r| {
            Ok(Session {
                id: r.get(0)?,
                started_at: from_ms(r.get(1)?),
                ended_at: r.get::<_, Option<i64>>(2)?.map(from_ms),
                app_version: r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

// ---------------------------------------------------------------------------------------------------------
// requests
// ---------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct RequestFilter {
    pub backend: Option<Backend>,
    pub model: Option<String>,
    /// An `Outcome::as_str` value.
    pub outcome: Option<String>,
    /// An `ApiFormat::as_str` value.
    pub api_format: Option<String>,
    /// Client IP, exact.
    pub client: Option<String>,
    pub cloud: Option<bool>,
    pub has_findings: Option<bool>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    /// Full-text search over captured bodies.
    pub q: Option<String>,
    pub include_passthrough: bool,
    pub limit: usize,
    /// `"<started_at_ms>_<id>"` of the last row of the previous page.
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RequestPage {
    pub items: Vec<RequestSummary>,
    pub next_cursor: Option<String>,
}

/// A record without its headers and timeline, plus which bodies are stored.
#[derive(Debug, Clone, Serialize)]
pub struct RequestSummary {
    #[serde(flatten)]
    pub record: RequestRecord,
    pub has_request_body: bool,
    pub has_response_body: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct BodyMeta {
    pub content_type: Option<String>,
    pub original_bytes: u64,
    pub stored_bytes: u64,
    pub truncated: bool,
    pub redacted: bool,
    pub is_text: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct RequestDetail {
    pub record: RequestRecord,
    pub request_body: Option<BodyMeta>,
    pub response_body: Option<BodyMeta>,
    pub findings: Vec<Finding>,
}

pub fn encode_cursor(record: &RequestRecord) -> String {
    format!("{}_{}", ms(record.started_at), record.id)
}

fn decode_cursor(cursor: &str) -> Result<(i64, String)> {
    let (ts, id) = cursor
        .split_once('_')
        .ok_or_else(|| anyhow!("invalid cursor"))?;
    let ts: i64 = ts.parse().map_err(|_| anyhow!("invalid cursor"))?;
    Uuid::parse_str(id).map_err(|_| anyhow!("invalid cursor"))?;
    Ok((ts, id.to_string()))
}

/// Quotes a search box's words for FTS5: `AKIA foo*` → `"AKIA" "foo"*`. Quotes inside a word are doubled.
pub fn fts_query(q: &str) -> Option<String> {
    let terms: Vec<String> = q
        .split_whitespace()
        .map(|term| {
            let (word, prefix) = match term.strip_suffix('*') {
                Some(stem) => (stem, "*"),
                None => (term, ""),
            };
            let word = word.replace('"', "\"\"");
            format!("\"{word}\"{prefix}")
        })
        .filter(|t| t != "\"\"" && t != "\"\"*")
        .collect();
    (!terms.is_empty()).then(|| terms.join(" "))
}

/// Builds `WHERE` clauses with numbered placeholders, one argument per clause; every `?` in a clause binds
/// the same value, so a clause may test two columns against one input.
struct QueryBuilder {
    sql: String,
    args: Vec<Box<dyn ToSql>>,
}

impl QueryBuilder {
    fn new(base: String) -> Self {
        Self {
            sql: base,
            args: Vec::new(),
        }
    }

    fn and(&mut self, clause: &str, value: impl ToSql + 'static) {
        let index = self.arg(value);
        self.sql.push_str(" AND ");
        self.sql
            .push_str(&clause.replace('?', &format!("?{index}")));
    }

    fn raw(&mut self, text: &str) {
        self.sql.push_str(text);
    }

    fn arg(&mut self, value: impl ToSql + 'static) -> usize {
        self.args.push(Box::new(value));
        self.args.len()
    }
}

pub fn list_requests(conn: &Connection, filter: &RequestFilter) -> Result<RequestPage> {
    let limit = filter.limit.clamp(1, 200);
    let mut q = QueryBuilder::new(format!(
        "SELECT {}, \
         EXISTS(SELECT 1 FROM bodies b WHERE b.request_id = r.id AND b.direction = 'request'), \
         EXISTS(SELECT 1 FROM bodies b WHERE b.request_id = r.id AND b.direction = 'response') \
         FROM request_records r WHERE 1 = 1",
        rows::SUMMARY_COLUMNS
            .split(", ")
            .map(|c| format!("r.{c}"))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    if let Some(backend) = filter.backend {
        q.and("r.backend = ?", backend.as_str());
    }
    if let Some(model) = &filter.model {
        q.and(
            "(r.model_resolved = ? OR r.model_requested = ?)",
            model.clone(),
        );
    }
    if let Some(outcome) = &filter.outcome {
        q.and("r.outcome = ?", outcome.clone());
    }
    if let Some(format) = &filter.api_format {
        q.and("r.api_format = ?", format.clone());
    }
    if let Some(client) = &filter.client {
        q.and("r.client_ip = ?", client.clone());
    }
    if let Some(cloud) = filter.cloud {
        q.and("r.is_cloud = ?", cloud);
    }
    if let Some(has) = filter.has_findings {
        q.raw(if has {
            " AND r.finding_count > 0"
        } else {
            " AND r.finding_count = 0"
        });
    }
    if let Some(from) = filter.from {
        q.and("r.started_at >= ?", ms(from));
    }
    if let Some(to) = filter.to {
        q.and("r.started_at < ?", ms(to));
    }
    if !filter.include_passthrough {
        q.raw(" AND r.api_format != 'passthrough'");
    }
    if let Some(fts) = filter.q.as_deref().and_then(fts_query) {
        q.and(
            "r.id IN (SELECT b.request_id FROM bodies b WHERE b.id IN (SELECT rowid FROM bodies_fts WHERE bodies_fts MATCH ?))",
            fts,
        );
    }
    if let Some(cursor) = &filter.cursor {
        let (ts, id) = decode_cursor(cursor)?;
        let ts_idx = q.arg(ts);
        let id_idx = q.arg(id);
        q.raw(&format!(
            " AND (r.started_at < ?{ts_idx} OR (r.started_at = ?{ts_idx} AND r.id < ?{id_idx}))"
        ));
    }
    let limit_idx = q.arg((limit + 1) as i64);
    q.raw(&format!(
        " ORDER BY r.started_at DESC, r.id DESC LIMIT ?{limit_idx}"
    ));

    let QueryBuilder { sql, args } = q;
    let mut stmt = conn
        .prepare(&sql)
        .with_context(|| format!("prepare list_requests: {sql}"))?;
    let heavy_count = rows::SUMMARY_COLUMNS.split(", ").count();
    let mut items: Vec<RequestSummary> = stmt
        .query_map(params_from_iter(args.iter()), |row| {
            Ok(RequestSummary {
                record: rows::map_record(row, false)?,
                has_request_body: row.get(heavy_count)?,
                has_response_body: row.get(heavy_count + 1)?,
            })
        })
        .context("list_requests")?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let next_cursor = if items.len() > limit {
        items.truncate(limit);
        items.last().map(|s| encode_cursor(&s.record))
    } else {
        None
    };
    Ok(RequestPage { items, next_cursor })
}

pub fn get_request(conn: &Connection, id: Uuid) -> Result<Option<RequestDetail>> {
    let sql = format!(
        "SELECT {} FROM request_records WHERE id = ?1",
        rows::RECORD_COLUMNS
    );
    let record = conn
        .query_row(&sql, params![id.to_string()], |row| {
            rows::map_record(row, true)
        })
        .optional()
        .context("get_request")?;
    let Some(record) = record else {
        return Ok(None);
    };
    let mut body_stmt = conn.prepare_cached(
        "SELECT direction, content_type, original_bytes, stored_bytes, truncated, redacted, is_text FROM bodies WHERE request_id = ?1",
    )?;
    let mut request_body = None;
    let mut response_body = None;
    for row in body_stmt.query_map(params![id.to_string()], |r| {
        let direction: String = r.get(0)?;
        Ok((
            direction,
            BodyMeta {
                content_type: r.get(1)?,
                original_bytes: r.get::<_, i64>(2)? as u64,
                stored_bytes: r.get::<_, i64>(3)? as u64,
                truncated: r.get(4)?,
                redacted: r.get(5)?,
                is_text: r.get(6)?,
            },
        ))
    })? {
        let (direction, meta) = row?;
        match direction.as_str() {
            "request" => request_body = Some(meta),
            "response" => response_body = Some(meta),
            _ => {}
        }
    }
    let mut finding_stmt = conn.prepare_cached(&format!(
        "SELECT {} FROM findings WHERE request_id = ?1 ORDER BY id",
        rows::FINDING_COLUMNS
    ))?;
    let findings = finding_stmt
        .query_map(params![id.to_string()], rows::map_finding)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Some(RequestDetail {
        record,
        request_body,
        response_body,
        findings,
    }))
}

pub fn get_body(conn: &Connection, id: Uuid, direction: Direction) -> Result<Option<Body>> {
    conn.query_row(
        &format!(
            "SELECT {} FROM bodies WHERE request_id = ?1 AND direction = ?2",
            rows::BODY_COLUMNS
        ),
        params![id.to_string(), direction.as_str()],
        rows::map_body,
    )
    .optional()
    .context("get_body")
}

/// Distinct client IPs and models seen in records, for filter dropdowns.
pub fn distinct_clients(conn: &Connection, limit: usize) -> Result<Vec<String>> {
    let mut stmt = conn.prepare_cached(
        "SELECT client_ip, COUNT(*) AS n FROM request_records GROUP BY client_ip ORDER BY n DESC LIMIT ?1",
    )?;
    let rows = stmt
        .query_map(params![limit as i64], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

// ---------------------------------------------------------------------------------------------------------
// findings and alerts
// ---------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
pub struct FindingFilter {
    pub rule: Option<String>,
    pub severity: Option<Severity>,
    pub backend: Option<Backend>,
    pub direction: Option<Direction>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub limit: usize,
    /// The `findings.id` of the last row of the previous page.
    pub before_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FindingRow {
    pub id: i64,
    #[serde(flatten)]
    pub finding: Finding,
    pub backend: Backend,
    pub model: Option<String>,
    pub is_cloud: bool,
}

pub fn list_findings(
    conn: &Connection,
    filter: &FindingFilter,
) -> Result<(Vec<FindingRow>, Option<i64>)> {
    let limit = filter.limit.clamp(1, 200);
    let mut q = QueryBuilder::new(format!(
        "SELECT f.id, {}, r.backend, COALESCE(r.model_resolved, r.model_requested), r.is_cloud \
         FROM findings f JOIN request_records r ON r.id = f.request_id WHERE 1 = 1",
        rows::FINDING_COLUMNS
            .split(", ")
            .map(|c| format!("f.{c}"))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    if let Some(rule) = &filter.rule {
        q.and("f.rule_id = ?", rule.clone());
    }
    if let Some(severity) = filter.severity {
        q.and("f.severity = ?", severity.as_str());
    }
    if let Some(backend) = filter.backend {
        q.and("r.backend = ?", backend.as_str());
    }
    if let Some(direction) = filter.direction {
        q.and("f.direction = ?", direction.as_str());
    }
    if let Some(from) = filter.from {
        q.and("f.created_at >= ?", ms(from));
    }
    if let Some(to) = filter.to {
        q.and("f.created_at < ?", ms(to));
    }
    if let Some(before) = filter.before_id {
        q.and("f.id < ?", before);
    }
    let limit_idx = q.arg((limit + 1) as i64);
    q.raw(&format!(" ORDER BY f.id DESC LIMIT ?{limit_idx}"));
    let QueryBuilder { sql, args } = q;
    let mut stmt = conn.prepare(&sql).context("prepare list_findings")?;
    let finding_cols = rows::FINDING_COLUMNS.split(", ").count();
    let mut items: Vec<FindingRow> = stmt
        .query_map(params_from_iter(args.iter()), |row| {
            let backend: String = row.get(1 + finding_cols)?;
            Ok(FindingRow {
                id: row.get(0)?,
                finding: map_finding_offset(row, 1)?,
                backend: backend.parse().unwrap_or(Backend::Ollama),
                model: row.get(2 + finding_cols)?,
                is_cloud: row.get(3 + finding_cols)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let next = if items.len() > limit {
        items.truncate(limit);
        items.last().map(|f| f.id)
    } else {
        None
    };
    Ok((items, next))
}

/// `rows::map_finding` reads from column 0; this shifts the start.
fn map_finding_offset(row: &rusqlite::Row<'_>, start: usize) -> rusqlite::Result<Finding> {
    let id: String = row.get(start)?;
    let direction: String = row.get(start + 1)?;
    let category: String = row.get(start + 3)?;
    let severity: String = row.get(start + 4)?;
    let action: String = row.get(start + 10)?;
    Ok(Finding {
        request_id: Uuid::parse_str(&id).unwrap_or_default(),
        direction: direction.parse().unwrap_or(Direction::Request),
        rule_id: row.get(start + 2)?,
        category: serde_json::from_str(&format!("\"{category}\""))
            .unwrap_or(crate::types::FindingCategory::Custom),
        severity: serde_json::from_str(&format!("\"{severity}\"")).unwrap_or(Severity::Low),
        location: row.get(start + 5)?,
        offset: row
            .get::<_, Option<i64>>(start + 6)?
            .and_then(|v| u64::try_from(v).ok()),
        length: u32::try_from(row.get::<_, i64>(start + 7)?).unwrap_or(0),
        fingerprint: row.get(start + 8)?,
        preview: row.get(start + 9)?,
        action: serde_json::from_str(&format!("\"{action}\""))
            .unwrap_or(crate::types::DlpAction::Alert),
        created_at: from_ms(row.get(start + 11)?),
    })
}

pub fn list_alerts(
    conn: &Connection,
    unacked_only: bool,
    before_id: Option<i64>,
    limit: usize,
) -> Result<(Vec<Alert>, Option<i64>)> {
    let limit = limit.clamp(1, 200);
    let mut sql = format!("SELECT {} FROM alerts WHERE 1 = 1", rows::ALERT_COLUMNS);
    let mut args: Vec<Box<dyn ToSql>> = Vec::new();
    if unacked_only {
        sql.push_str(" AND acknowledged_at IS NULL");
    }
    if let Some(before) = before_id {
        args.push(Box::new(before));
        sql.push_str(&format!(" AND id < ?{}", args.len()));
    }
    args.push(Box::new((limit + 1) as i64));
    sql.push_str(&format!(" ORDER BY id DESC LIMIT ?{}", args.len()));
    let mut stmt = conn.prepare(&sql)?;
    let mut items: Vec<Alert> = stmt
        .query_map(params_from_iter(args.iter()), rows::map_alert)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let next = if items.len() > limit {
        items.truncate(limit);
        items.last().map(|a| a.id)
    } else {
        None
    };
    Ok((items, next))
}

pub fn unacked_alert_count(conn: &Connection) -> Result<u64> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM alerts WHERE acknowledged_at IS NULL",
        [],
        |r| r.get(0),
    )?;
    Ok(n as u64)
}

// ---------------------------------------------------------------------------------------------------------
// series
// ---------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeriesMetric {
    /// `requests`, `failed` per bucket.
    ReqPerMin,
    /// `mean`, `max`, `p95` tokens per second per bucket.
    TokPerSec,
    /// `mean`, `p50`, `p95` TTFT (ms) per bucket.
    Ttft,
    /// Prompt plus generated tokens per model per bucket.
    TokensByModel,
    /// Prompt plus generated tokens per client IP per bucket.
    TokensByClient,
    /// Cumulative hypothetical cost per frontier key, running across buckets.
    CostCumulative,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SeriesQuery {
    pub metric: SeriesMetric,
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub bucket_ms: i64,
    pub backend: Option<Backend>,
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SeriesPoint {
    pub t: DateTime<Utc>,
    pub values: BTreeMap<String, f64>,
}

/// The rows a series needs; aggregation happens in Rust, which keeps the SQL simple and lets percentiles
/// be exact.
#[derive(Debug, Clone)]
pub struct SeriesRow {
    pub started_at: i64,
    pub model: String,
    pub client: String,
    pub completed: bool,
    pub prompt_tokens: u64,
    pub gen_tokens: u64,
    pub tokens_per_sec: Option<f64>,
    pub ttft_ms: Option<f64>,
    pub cost: BTreeMap<String, f64>,
}

pub fn series_rows(conn: &Connection, query: &SeriesQuery) -> Result<Vec<SeriesRow>> {
    let mut sql = String::from(
        "SELECT started_at, COALESCE(model_resolved, model_requested, '?'), client_ip, outcome, COALESCE(prompt_tokens, 0), \
         COALESCE(gen_tokens, 0), tokens_per_sec, ttft_ms, hypothetical_cost \
         FROM request_records WHERE kind = 'http' AND api_format NOT IN ('passthrough', 'cloud_passthrough') \
         AND started_at >= ?1 AND started_at < ?2",
    );
    let mut args: Vec<Box<dyn ToSql>> = vec![Box::new(ms(query.from)), Box::new(ms(query.to))];
    if let Some(backend) = query.backend {
        args.push(Box::new(backend.as_str()));
        sql.push_str(&format!(" AND backend = ?{}", args.len()));
    }
    if let Some(model) = &query.model {
        args.push(Box::new(model.clone()));
        sql.push_str(&format!(
            " AND (model_resolved = ?{n} OR model_requested = ?{n})",
            n = args.len()
        ));
    }
    sql.push_str(" ORDER BY started_at LIMIT 500000");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params_from_iter(args.iter()), |r| {
            let outcome: String = r.get(3)?;
            let cost: Option<String> = r.get(8)?;
            Ok(SeriesRow {
                started_at: r.get(0)?,
                model: r.get(1)?,
                client: r.get(2)?,
                completed: outcome == "completed",
                prompt_tokens: r.get::<_, i64>(4)? as u64,
                gen_tokens: r.get::<_, i64>(5)? as u64,
                tokens_per_sec: r.get(6)?,
                ttft_ms: r.get(7)?,
                cost: cost
                    .and_then(|c| serde_json::from_str(&c).ok())
                    .unwrap_or_default(),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Buckets `rows` for `query`. Pure, so it is tested without a database.
pub fn aggregate_series(rows: &[SeriesRow], query: &SeriesQuery) -> Vec<SeriesPoint> {
    let bucket_ms = query.bucket_ms.max(1000);
    let mut buckets: BTreeMap<i64, Vec<&SeriesRow>> = BTreeMap::new();
    for row in rows {
        let bucket = row.started_at - row.started_at.rem_euclid(bucket_ms);
        buckets.entry(bucket).or_default().push(row);
    }
    let mut running: BTreeMap<String, f64> = BTreeMap::new();
    buckets
        .into_iter()
        .map(|(bucket, rows)| {
            let mut values = BTreeMap::new();
            match query.metric {
                SeriesMetric::ReqPerMin => {
                    values.insert("requests".to_string(), rows.len() as f64);
                    values.insert(
                        "failed".to_string(),
                        rows.iter().filter(|r| !r.completed).count() as f64,
                    );
                }
                SeriesMetric::TokPerSec => {
                    let mut tps: Vec<f64> = rows
                        .iter()
                        .filter_map(|r| r.tokens_per_sec)
                        .filter(|v| v.is_finite() && *v > 0.0)
                        .collect();
                    if !tps.is_empty() {
                        tps.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                        values.insert(
                            "mean".to_string(),
                            tps.iter().sum::<f64>() / tps.len() as f64,
                        );
                        values.insert("max".to_string(), tps[tps.len() - 1]);
                        values.insert("p95".to_string(), percentile(&tps, 0.95));
                    }
                }
                SeriesMetric::Ttft => {
                    let mut ttft: Vec<f64> = rows
                        .iter()
                        .filter_map(|r| r.ttft_ms)
                        .filter(|v| v.is_finite())
                        .collect();
                    if !ttft.is_empty() {
                        ttft.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                        values.insert(
                            "mean".to_string(),
                            ttft.iter().sum::<f64>() / ttft.len() as f64,
                        );
                        values.insert("p50".to_string(), percentile(&ttft, 0.50));
                        values.insert("p95".to_string(), percentile(&ttft, 0.95));
                    }
                }
                SeriesMetric::TokensByModel => {
                    for r in &rows {
                        *values.entry(r.model.clone()).or_insert(0.0) +=
                            (r.prompt_tokens + r.gen_tokens) as f64;
                    }
                }
                SeriesMetric::TokensByClient => {
                    for r in &rows {
                        *values.entry(r.client.clone()).or_insert(0.0) +=
                            (r.prompt_tokens + r.gen_tokens) as f64;
                    }
                }
                SeriesMetric::CostCumulative => {
                    for r in &rows {
                        for (key, usd) in &r.cost {
                            *running.entry(key.clone()).or_insert(0.0) += usd;
                        }
                    }
                    values = running.clone();
                }
            }
            SeriesPoint {
                t: from_ms(bucket),
                values,
            }
        })
        .collect()
}

fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * q).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// Prompt and generated token totals over a range, for the cost table.
pub fn token_totals(
    conn: &Connection,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    backend: Option<Backend>,
) -> Result<(u64, u64)> {
    let mut sql = String::from(
        "SELECT COALESCE(SUM(prompt_tokens), 0), COALESCE(SUM(gen_tokens), 0) FROM request_records \
         WHERE kind = 'http' AND api_format NOT IN ('passthrough', 'cloud_passthrough') AND outcome = 'completed' \
         AND started_at >= ?1 AND started_at < ?2",
    );
    let mut args: Vec<Box<dyn ToSql>> = vec![Box::new(ms(from)), Box::new(ms(to))];
    if let Some(backend) = backend {
        args.push(Box::new(backend.as_str()));
        sql.push_str(" AND backend = ?3");
    }
    conn.query_row(&sql, params_from_iter(args.iter()), |r| {
        Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64))
    })
    .context("token totals")
}

// ---------------------------------------------------------------------------------------------------------
// hardware
// ---------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HardwarePoint {
    pub t: DateTime<Utc>,
    pub cpu: Option<f64>,
    pub mem_used: Option<f64>,
    pub mem_available: Option<f64>,
    pub mem_total: Option<f64>,
    pub gpu: Option<f64>,
    pub gpu_mem: Option<f64>,
    pub ane: Option<f64>,
    pub lms_cpu: Option<f64>,
    pub lms_rss: Option<f64>,
    pub ollama_cpu: Option<f64>,
    pub ollama_rss: Option<f64>,
}

/// Raw samples averaged per `step_ms` bucket for windows up to two hours, the minute rollups beyond.
pub fn hardware_series(
    conn: &Connection,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    step_ms: i64,
) -> Result<Vec<HardwarePoint>> {
    let step = step_ms.max(1000);
    let span_ms = ms(to) - ms(from);
    let sql = if span_ms <= 2 * 3_600_000 {
        "SELECT (at / ?3) * ?3 AS b, AVG(system_cpu_pct), AVG(mem_used), AVG(mem_available), MAX(mem_total), AVG(gpu_util_pct), \
         AVG(gpu_mem_used), AVG(ane_power_mw), AVG(lms_cpu_pct), AVG(lms_rss), AVG(ollama_cpu_pct), AVG(ollama_rss) \
         FROM hardware_samples WHERE at >= ?1 AND at < ?2 GROUP BY b ORDER BY b"
    } else {
        "SELECT (bucket / ?3) * ?3 AS b, SUM(sum_cpu) / SUM(samples), SUM(sum_mem_used) / SUM(samples), \
         SUM(sum_mem_available) / SUM(samples), MAX(mem_total), \
         CASE WHEN SUM(n_gpu) > 0 THEN SUM(sum_gpu) / SUM(n_gpu) END, \
         CASE WHEN SUM(n_gpu_mem) > 0 THEN SUM(sum_gpu_mem) * 1.0 / SUM(n_gpu_mem) END, \
         CASE WHEN SUM(n_ane) > 0 THEN SUM(sum_ane) / SUM(n_ane) END, \
         SUM(sum_lms_cpu) / SUM(samples), SUM(sum_lms_rss) / SUM(samples), SUM(sum_ollama_cpu) / SUM(samples), \
         SUM(sum_ollama_rss) / SUM(samples) \
         FROM hardware_rollups_1m WHERE bucket >= ?1 AND bucket < ?2 GROUP BY b ORDER BY b"
    };
    let mut stmt = conn.prepare_cached(sql)?;
    let rows = stmt
        .query_map(params![ms(from), ms(to), step], |r| {
            Ok(HardwarePoint {
                t: from_ms(r.get(0)?),
                cpu: r.get(1)?,
                mem_used: r.get(2)?,
                mem_available: r.get(3)?,
                mem_total: r.get(4)?,
                gpu: r.get(5)?,
                gpu_mem: r.get(6)?,
                ane: r.get(7)?,
                lms_cpu: r.get(8)?,
                lms_rss: r.get(9)?,
                ollama_cpu: r.get(10)?,
                ollama_rss: r.get(11)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn db_size_bytes(conn: &Connection) -> Result<u64> {
    let (pages, page_size): (i64, i64) = conn.query_row(
        "SELECT (SELECT page_count FROM pragma_page_count), (SELECT page_size FROM pragma_page_size)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok((pages * page_size).max(0) as u64)
}

pub fn body_count(conn: &Connection) -> Result<u64> {
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM bodies", [], |r| r.get(0))?;
    Ok(n as u64)
}

/// Counts for the Security tab's summary tiles: findings, blocked requests and requests whose prompt left
/// the machine (cloud-routed inference and scanned cloud passthrough) since `since`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ActivitySummary {
    pub findings: u64,
    pub blocked_requests: u64,
    pub cloud_requests: u64,
}

pub fn activity_since(conn: &Connection, since: DateTime<Utc>) -> Result<ActivitySummary> {
    let since_ms = rows::ms(since);
    let findings: i64 = conn.query_row(
        "SELECT COUNT(*) FROM findings WHERE created_at >= ?1",
        [since_ms],
        |r| r.get(0),
    )?;
    let blocked: i64 = conn.query_row(
        "SELECT COUNT(*) FROM request_records WHERE outcome = 'blocked' AND started_at >= ?1",
        [since_ms],
        |r| r.get(0),
    )?;
    let cloud: i64 = conn.query_row(
        "SELECT COUNT(*) FROM request_records WHERE is_cloud = 1 AND kind = 'http' \
         AND api_format != 'passthrough' AND started_at >= ?1",
        [since_ms],
        |r| r.get(0),
    )?;
    Ok(ActivitySummary {
        findings: findings.max(0) as u64,
        blocked_requests: blocked.max(0) as u64,
        cloud_requests: cloud.max(0) as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fts_queries_quote_each_word_and_keep_prefix_stars() {
        assert_eq!(fts_query("AKIA foo*").as_deref(), Some("\"AKIA\" \"foo\"*"));
        assert_eq!(
            fts_query("say \"hi\"").as_deref(),
            Some("\"say\" \"\"\"hi\"\"\"")
        );
        assert_eq!(fts_query("   "), None);
        assert_eq!(fts_query("*"), None);
    }

    #[test]
    fn cursors_round_trip() {
        let id = Uuid::now_v7();
        let cursor = format!("1700000000000_{id}");
        assert_eq!(
            decode_cursor(&cursor).unwrap(),
            (1_700_000_000_000, id.to_string())
        );
        assert!(decode_cursor("nope").is_err());
        assert!(decode_cursor("12_not-a-uuid").is_err());
    }

    fn row(
        at_ms: i64,
        model: &str,
        client: &str,
        completed: bool,
        tps: Option<f64>,
        ttft: Option<f64>,
        cost: f64,
    ) -> SeriesRow {
        SeriesRow {
            started_at: at_ms,
            model: model.into(),
            client: client.into(),
            completed,
            prompt_tokens: 10,
            gen_tokens: 5,
            tokens_per_sec: tps,
            ttft_ms: ttft,
            cost: BTreeMap::from([("k".to_string(), cost)]),
        }
    }

    fn query(metric: SeriesMetric) -> SeriesQuery {
        SeriesQuery {
            metric,
            from: from_ms(0),
            to: from_ms(10_000_000),
            bucket_ms: 60_000,
            backend: None,
            model: None,
        }
    }

    #[test]
    fn series_bucket_and_aggregate_each_metric() {
        let rows = vec![
            row(1_000, "a", "10.0.0.1", true, Some(10.0), Some(100.0), 1.0),
            row(2_000, "b", "10.0.0.2", false, Some(30.0), Some(300.0), 2.0),
            row(61_000, "a", "10.0.0.1", true, None, None, 4.0),
        ];
        let req = aggregate_series(&rows, &query(SeriesMetric::ReqPerMin));
        assert_eq!(req.len(), 2);
        assert_eq!(req[0].values["requests"], 2.0);
        assert_eq!(req[0].values["failed"], 1.0);
        assert_eq!(req[1].values["requests"], 1.0);

        let tps = aggregate_series(&rows, &query(SeriesMetric::TokPerSec));
        assert_eq!(tps[0].values["mean"], 20.0);
        assert_eq!(tps[0].values["max"], 30.0);
        assert!(tps[1].values.is_empty(), "no rates in the second bucket");

        let ttft = aggregate_series(&rows, &query(SeriesMetric::Ttft));
        assert_eq!(ttft[0].values["p50"], 300.0);
        assert_eq!(ttft[0].values["mean"], 200.0);

        let by_model = aggregate_series(&rows, &query(SeriesMetric::TokensByModel));
        assert_eq!(by_model[0].values["a"], 15.0);
        assert_eq!(by_model[0].values["b"], 15.0);
        assert_eq!(by_model[1].values["a"], 15.0);

        let by_client = aggregate_series(&rows, &query(SeriesMetric::TokensByClient));
        assert_eq!(by_client[0].values["10.0.0.2"], 15.0);

        let cost = aggregate_series(&rows, &query(SeriesMetric::CostCumulative));
        assert_eq!(cost[0].values["k"], 3.0);
        assert_eq!(cost[1].values["k"], 7.0, "cumulative across buckets");
    }
}
