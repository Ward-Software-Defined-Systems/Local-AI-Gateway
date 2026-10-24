//! Row mapping between the domain types and the SQLite columns: inserts, upserts and the column lists
//! the readers select. Timestamps are unix milliseconds, headers / timelines / costs are JSON text, and an
//! `Outcome` spreads over five columns so it can be filtered and indexed.

use std::collections::BTreeMap;

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, Row, params};
use uuid::Uuid;

use crate::types::{
    Alert, Backend, Body, Direction, FailureSource, Finding, HardwareSample, ModelInfo, Outcome,
    RequestKind, RequestRecord, Severity,
};

pub fn ms(at: DateTime<Utc>) -> i64 {
    at.timestamp_millis()
}

pub fn from_ms(ms: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(ms).unwrap_or_default()
}

fn opt_ms(at: Option<DateTime<Utc>>) -> Option<i64> {
    at.map(ms)
}

fn opt_from_ms(ms: Option<i64>) -> Option<DateTime<Utc>> {
    ms.map(from_ms)
}

fn u64_from(v: Option<i64>) -> Option<u64> {
    v.and_then(|v| u64::try_from(v).ok())
}

fn severity_from(s: Option<String>) -> Option<Severity> {
    s.and_then(|s| serde_json::from_str(&format!("\"{s}\"")).ok())
}

/// The five outcome columns.
struct OutcomeColumns {
    outcome: &'static str,
    failure_source: Option<&'static str>,
    rejected_input_tokens: Option<i64>,
    rejected_context_length: Option<i64>,
    blocked_rule_id: Option<String>,
}

fn outcome_columns(outcome: &Outcome) -> OutcomeColumns {
    let mut cols = OutcomeColumns {
        outcome: outcome.as_str(),
        failure_source: None,
        rejected_input_tokens: None,
        rejected_context_length: None,
        blocked_rule_id: None,
    };
    match outcome {
        Outcome::Failed { source, .. } => cols.failure_source = Some(source.as_str()),
        Outcome::Rejected {
            input_tokens,
            context_length,
        } => {
            cols.rejected_input_tokens = input_tokens.map(|v| v as i64);
            cols.rejected_context_length = context_length.map(|v| v as i64);
        }
        Outcome::Blocked { rule_id } => cols.blocked_rule_id = Some(rule_id.clone()),
        Outcome::Completed | Outcome::ClientAborted => {}
    }
    cols
}

fn outcome_from_columns(
    outcome: &str,
    status: Option<i64>,
    failure_source: Option<String>,
    rejected_input_tokens: Option<i64>,
    rejected_context_length: Option<i64>,
    blocked_rule_id: Option<String>,
) -> Result<Outcome> {
    Ok(match outcome {
        "completed" => Outcome::Completed,
        "failed" => Outcome::Failed {
            status: u16::try_from(status.unwrap_or(0)).unwrap_or(0),
            source: failure_source
                .as_deref()
                .unwrap_or("upstream")
                .parse::<FailureSource>()
                .map_err(|e| anyhow!(e))?,
        },
        "rejected" => Outcome::Rejected {
            input_tokens: u64_from(rejected_input_tokens),
            context_length: u64_from(rejected_context_length),
        },
        "blocked" => Outcome::Blocked {
            rule_id: blocked_rule_id.unwrap_or_default(),
        },
        "client_aborted" => Outcome::ClientAborted,
        other => return Err(anyhow!("unknown outcome `{other}` in request_records")),
    })
}

// ---------------------------------------------------------------------------------------------------------
// request_records
// ---------------------------------------------------------------------------------------------------------

/// The columns every record read selects, in the order `map_record` expects. The heavy JSON columns
/// (headers, timeline) come last so `SUMMARY_COLUMNS` can stop short of them.
pub const SUMMARY_COLUMNS: &str = "id, session_id, backend, listener, kind, client_ip, user_agent, client_request_id, method, path, query, \
     api_format, streamed, model_requested, model_resolved, is_cloud, started_at, first_byte_at, ended_at, status, outcome, \
     failure_source, rejected_input_tokens, rejected_context_length, blocked_rule_id, error_summary, prompt_tokens, gen_tokens, \
     tokens_estimated, ttft_ms, total_ms, tokens_per_sec, timings_estimated, stop_reason, request_bytes, response_bytes, \
     finding_count, max_severity, hypothetical_cost, actual_cost_usd, routed_by";

pub const RECORD_COLUMNS: &str = "id, session_id, backend, listener, kind, client_ip, user_agent, client_request_id, method, path, query, \
     api_format, streamed, model_requested, model_resolved, is_cloud, started_at, first_byte_at, ended_at, status, outcome, \
     failure_source, rejected_input_tokens, rejected_context_length, blocked_rule_id, error_summary, prompt_tokens, gen_tokens, \
     tokens_estimated, ttft_ms, total_ms, tokens_per_sec, timings_estimated, stop_reason, request_bytes, response_bytes, \
     finding_count, max_severity, hypothetical_cost, actual_cost_usd, routed_by, request_headers, response_headers, chunk_timeline";

pub fn insert_request(conn: &Connection, r: &RequestRecord) -> Result<()> {
    let o = outcome_columns(&r.outcome);
    conn.prepare_cached(
        "INSERT INTO request_records (
            id, session_id, backend, listener, kind, client_ip, user_agent, client_request_id, method, path, query,
            api_format, streamed, model_requested, model_resolved, is_cloud, started_at, first_byte_at, ended_at, status, outcome,
            failure_source, rejected_input_tokens, rejected_context_length, blocked_rule_id, error_summary, prompt_tokens, gen_tokens,
            tokens_estimated, ttft_ms, total_ms, tokens_per_sec, timings_estimated, stop_reason, request_bytes, response_bytes,
            request_headers, response_headers, chunk_timeline, finding_count, max_severity, hypothetical_cost,
            actual_cost_usd, routed_by
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24,
                   ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33, ?34, ?35, ?36, ?37, ?38, ?39, ?40, ?41, ?42, ?43, ?44)",
    )?
    .execute(params![
        r.id.to_string(),
        r.session_id,
        r.backend.as_str(),
        r.listener,
        r.kind.as_str(),
        r.client_ip,
        r.user_agent,
        r.client_request_id,
        r.method,
        r.path,
        r.query,
        r.api_format.as_str(),
        r.streamed,
        r.model_requested,
        r.model_resolved,
        r.is_cloud,
        ms(r.started_at),
        opt_ms(r.first_byte_at),
        ms(r.ended_at),
        r.status.map(i64::from),
        o.outcome,
        o.failure_source,
        o.rejected_input_tokens,
        o.rejected_context_length,
        o.blocked_rule_id,
        r.error_summary,
        r.prompt_tokens.map(|v| v as i64),
        r.gen_tokens.map(|v| v as i64),
        r.tokens_estimated,
        r.ttft_ms,
        r.total_ms,
        r.tokens_per_sec,
        r.timings_estimated,
        r.stop_reason,
        r.request_bytes as i64,
        r.response_bytes as i64,
        serde_json::to_string(&r.request_headers)?,
        serde_json::to_string(&r.response_headers)?,
        serde_json::to_string(&r.chunk_timeline)?,
        i64::from(r.finding_count),
        r.max_severity.map(|s| s.as_str()),
        serde_json::to_string(&r.hypothetical_cost)?,
        r.actual_cost_usd,
        r.routed_by,
    ])
    .context("insert request_records")?;
    Ok(())
}

/// Maps a row selected with `SUMMARY_COLUMNS` (headers and timeline empty) or `RECORD_COLUMNS`.
pub fn map_record(row: &Row<'_>, with_heavy: bool) -> rusqlite::Result<RequestRecord> {
    let fail = |e: anyhow::Error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, e.into())
    };
    let id: String = row.get(0)?;
    let backend: String = row.get(2)?;
    let kind: String = row.get(4)?;
    let api_format: String = row.get(11)?;
    let outcome: String = row.get(20)?;
    let cost: Option<String> = row.get(38)?;
    let (request_headers, response_headers, chunk_timeline) = if with_heavy {
        let rh: String = row.get(41)?;
        let sh: String = row.get(42)?;
        let tl: Option<String> = row.get(43)?;
        (
            serde_json::from_str(&rh).unwrap_or_default(),
            serde_json::from_str(&sh).unwrap_or_default(),
            tl.and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or_default(),
        )
    } else {
        (Vec::new(), Vec::new(), Vec::new())
    };
    Ok(RequestRecord {
        id: Uuid::parse_str(&id).map_err(|e| fail(e.into()))?,
        session_id: row.get(1)?,
        backend: backend.parse::<Backend>().map_err(|e| fail(e.into()))?,
        listener: row.get(3)?,
        kind: kind.parse::<RequestKind>().map_err(|e| fail(anyhow!(e)))?,
        client_ip: row.get(5)?,
        user_agent: row.get(6)?,
        client_request_id: row.get(7)?,
        method: row.get(8)?,
        path: row.get(9)?,
        query: row.get(10)?,
        api_format: api_format.parse().map_err(|e: String| fail(anyhow!(e)))?,
        streamed: row.get(12)?,
        model_requested: row.get(13)?,
        model_resolved: row.get(14)?,
        is_cloud: row.get(15)?,
        started_at: from_ms(row.get(16)?),
        first_byte_at: opt_from_ms(row.get(17)?),
        ended_at: from_ms(row.get(18)?),
        status: row
            .get::<_, Option<i64>>(19)?
            .and_then(|s| u16::try_from(s).ok()),
        outcome: outcome_from_columns(
            &outcome,
            row.get(19)?,
            row.get(21)?,
            row.get(22)?,
            row.get(23)?,
            row.get(24)?,
        )
        .map_err(fail)?,
        error_summary: row.get(25)?,
        prompt_tokens: u64_from(row.get(26)?),
        gen_tokens: u64_from(row.get(27)?),
        tokens_estimated: row.get(28)?,
        ttft_ms: row.get(29)?,
        total_ms: row.get(30)?,
        tokens_per_sec: row.get(31)?,
        timings_estimated: row.get(32)?,
        stop_reason: row.get(33)?,
        request_bytes: u64_from(row.get(34)?).unwrap_or(0),
        response_bytes: u64_from(row.get(35)?).unwrap_or(0),
        request_headers,
        response_headers,
        chunk_timeline,
        finding_count: u32::try_from(row.get::<_, i64>(36)?).unwrap_or(0),
        max_severity: severity_from(row.get(37)?),
        hypothetical_cost: cost
            .and_then(|c| serde_json::from_str::<BTreeMap<String, f64>>(&c).ok())
            .unwrap_or_default(),
        actual_cost_usd: row.get(39)?,
        routed_by: row.get(40)?,
    })
}

/// Keeps the hourly rollup for the record's bucket current.
pub fn upsert_request_rollup(conn: &Connection, r: &RequestRecord) -> Result<()> {
    if !r.is_feed_row() {
        return Ok(());
    }
    let bucket = ms(r.started_at) - ms(r.started_at).rem_euclid(3_600_000);
    let model = r
        .model_resolved
        .clone()
        .or_else(|| r.model_requested.clone())
        .unwrap_or_else(|| "?".to_string());
    let (completed, failed, rejected, blocked, aborted) = match r.outcome {
        Outcome::Completed => (1, 0, 0, 0, 0),
        Outcome::Failed { .. } => (0, 1, 0, 0, 0),
        Outcome::Rejected { .. } => (0, 0, 1, 0, 0),
        Outcome::Blocked { .. } => (0, 0, 0, 1, 0),
        Outcome::ClientAborted => (0, 0, 0, 0, 1),
    };
    let (ttft, ttft_n) = match r.ttft_ms {
        Some(v) if v.is_finite() => (v, 1),
        _ => (0.0, 0),
    };
    let (tps, tps_n) = match r.tokens_per_sec {
        Some(v) if v.is_finite() && v > 0.0 => (v, 1),
        _ => (0.0, 0),
    };
    conn.prepare_cached(
        "INSERT INTO request_rollups_hourly (bucket, backend, model, is_cloud, requests, completed, failed, rejected, blocked,
            aborted, prompt_tokens, gen_tokens, sum_ttft_ms, ttft_count, sum_total_ms, sum_tps, tps_count)
         VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
         ON CONFLICT(bucket, backend, model, is_cloud) DO UPDATE SET
            requests = requests + 1, completed = completed + excluded.completed, failed = failed + excluded.failed,
            rejected = rejected + excluded.rejected, blocked = blocked + excluded.blocked, aborted = aborted + excluded.aborted,
            prompt_tokens = prompt_tokens + excluded.prompt_tokens, gen_tokens = gen_tokens + excluded.gen_tokens,
            sum_ttft_ms = sum_ttft_ms + excluded.sum_ttft_ms, ttft_count = ttft_count + excluded.ttft_count,
            sum_total_ms = sum_total_ms + excluded.sum_total_ms, sum_tps = sum_tps + excluded.sum_tps,
            tps_count = tps_count + excluded.tps_count",
    )?
    .execute(params![
        bucket,
        r.backend.as_str(),
        model,
        r.is_cloud,
        completed,
        failed,
        rejected,
        blocked,
        aborted,
        r.prompt_tokens.unwrap_or(0) as i64,
        r.gen_tokens.unwrap_or(0) as i64,
        ttft,
        ttft_n,
        r.total_ms,
        tps,
        tps_n,
    ])
    .context("upsert request_rollups_hourly")?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------
// bodies, findings, alerts
// ---------------------------------------------------------------------------------------------------------

pub fn insert_body(conn: &Connection, body: &Body, created_at: DateTime<Utc>) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO bodies (request_id, direction, content_type, original_bytes, stored_bytes, truncated, redacted, is_text, text, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT(request_id, direction) DO UPDATE SET content_type = excluded.content_type, original_bytes = excluded.original_bytes,
            stored_bytes = excluded.stored_bytes, truncated = excluded.truncated, redacted = excluded.redacted, is_text = excluded.is_text,
            text = excluded.text, created_at = excluded.created_at",
    )?
    .execute(params![
        body.request_id.to_string(),
        body.direction.as_str(),
        body.content_type,
        body.original_bytes as i64,
        body.stored_bytes as i64,
        body.truncated,
        body.redacted,
        body.is_text,
        body.text,
        ms(created_at),
    ])
    .context("insert bodies")?;
    Ok(())
}

pub fn map_body(row: &Row<'_>) -> rusqlite::Result<Body> {
    let id: String = row.get(0)?;
    let direction: String = row.get(1)?;
    Ok(Body {
        request_id: Uuid::parse_str(&id).unwrap_or_default(),
        direction: direction.parse().unwrap_or(Direction::Request),
        content_type: row.get(2)?,
        original_bytes: u64_from(row.get(3)?).unwrap_or(0),
        stored_bytes: u64_from(row.get(4)?).unwrap_or(0),
        truncated: row.get(5)?,
        redacted: row.get(6)?,
        is_text: row.get(7)?,
        text: row.get(8)?,
    })
}

pub const BODY_COLUMNS: &str = "request_id, direction, content_type, original_bytes, stored_bytes, truncated, redacted, is_text, text";

pub fn insert_finding(conn: &Connection, f: &Finding) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO findings (request_id, direction, rule_id, category, severity, location, offset, length, fingerprint, preview, action, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
    )?
    .execute(params![
        f.request_id.to_string(),
        f.direction.as_str(),
        f.rule_id,
        f.category.as_str(),
        f.severity.as_str(),
        f.location,
        f.offset.map(|v| v as i64),
        i64::from(f.length),
        f.fingerprint,
        f.preview,
        serde_json::to_value(f.action)?.as_str().unwrap_or("alert").to_string(),
        ms(f.created_at),
    ])
    .context("insert findings")?;
    Ok(())
}

pub const FINDING_COLUMNS: &str = "request_id, direction, rule_id, category, severity, location, offset, length, fingerprint, preview, action, created_at";

pub fn map_finding(row: &Row<'_>) -> rusqlite::Result<Finding> {
    let id: String = row.get(0)?;
    let direction: String = row.get(1)?;
    let category: String = row.get(3)?;
    let severity: String = row.get(4)?;
    let action: String = row.get(10)?;
    Ok(Finding {
        request_id: Uuid::parse_str(&id).unwrap_or_default(),
        direction: direction.parse().unwrap_or(Direction::Request),
        rule_id: row.get(2)?,
        category: serde_json::from_str(&format!("\"{category}\""))
            .unwrap_or(crate::types::FindingCategory::Custom),
        severity: severity_from(Some(severity)).unwrap_or(Severity::Low),
        location: row.get(5)?,
        offset: u64_from(row.get(6)?),
        length: u32::try_from(row.get::<_, i64>(7)?).unwrap_or(0),
        fingerprint: row.get(8)?,
        preview: row.get(9)?,
        action: serde_json::from_str(&format!("\"{action}\""))
            .unwrap_or(crate::types::DlpAction::Alert),
        created_at: from_ms(row.get(11)?),
    })
}

pub fn insert_alert(conn: &Connection, a: &Alert) -> Result<i64> {
    conn.prepare_cached(
        "INSERT INTO alerts (created_at, kind, severity, title, detail, request_id, acknowledged_at, delivered)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )?
    .execute(params![
        ms(a.created_at),
        a.kind.as_str(),
        a.severity.as_str(),
        a.title,
        a.detail.to_string(),
        a.request_id.map(|id| id.to_string()),
        opt_ms(a.acknowledged_at),
        serde_json::to_string(&a.delivered)?,
    ])
    .context("insert alerts")?;
    Ok(conn.last_insert_rowid())
}

pub const ALERT_COLUMNS: &str =
    "id, created_at, kind, severity, title, detail, request_id, acknowledged_at, delivered";

pub fn map_alert(row: &Row<'_>) -> rusqlite::Result<Alert> {
    let kind: String = row.get(2)?;
    let severity: String = row.get(3)?;
    let detail: String = row.get(5)?;
    let request_id: Option<String> = row.get(6)?;
    let delivered: String = row.get(8)?;
    Ok(Alert {
        id: row.get(0)?,
        created_at: from_ms(row.get(1)?),
        kind: serde_json::from_str(&format!("\"{kind}\""))
            .unwrap_or(crate::types::AlertKind::DlpFinding),
        severity: severity_from(Some(severity)).unwrap_or(Severity::Low),
        title: row.get(4)?,
        detail: serde_json::from_str(&detail).unwrap_or(serde_json::Value::Null),
        request_id: request_id.and_then(|s| Uuid::parse_str(&s).ok()),
        acknowledged_at: opt_from_ms(row.get(7)?),
        delivered: serde_json::from_str(&delivered).unwrap_or_default(),
    })
}

// ---------------------------------------------------------------------------------------------------------
// hardware, models
// ---------------------------------------------------------------------------------------------------------

pub fn insert_hardware(conn: &Connection, s: &HardwareSample) -> Result<()> {
    conn.prepare_cached(
        "INSERT OR REPLACE INTO hardware_samples (at, system_cpu_pct, mem_used, mem_total, mem_available, lms_cpu_pct, lms_rss, lms_procs,
            ollama_cpu_pct, ollama_rss, ollama_procs, gpu_util_pct, gpu_renderer_pct, gpu_tiler_pct, gpu_mem_used, gpu_mem_alloc, ane_power_mw)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
    )?
    .execute(params![
        ms(s.at),
        f64::from(s.system_cpu_pct),
        s.mem_used_bytes as i64,
        s.mem_total_bytes as i64,
        s.mem_available_bytes as i64,
        f64::from(s.lmstudio.cpu_pct),
        s.lmstudio.rss_bytes as i64,
        i64::from(s.lmstudio.process_count),
        f64::from(s.ollama.cpu_pct),
        s.ollama.rss_bytes as i64,
        i64::from(s.ollama.process_count),
        s.gpu_util_pct.map(f64::from),
        s.gpu_renderer_pct.map(f64::from),
        s.gpu_tiler_pct.map(f64::from),
        s.gpu_mem_used_bytes.map(|v| v as i64),
        s.gpu_mem_alloc_bytes.map(|v| v as i64),
        s.ane_power_mw.map(f64::from),
    ])
    .context("insert hardware_samples")?;
    let bucket = ms(s.at) - ms(s.at).rem_euclid(60_000);
    let cpu = f64::from(s.system_cpu_pct);
    let gpu = s.gpu_util_pct.map(f64::from);
    let gpu_mem = s.gpu_mem_used_bytes.map(|v| v as i64);
    let ane = s.ane_power_mw.map(f64::from);
    conn.prepare_cached(
        "INSERT INTO hardware_rollups_1m (bucket, samples, sum_cpu, max_cpu, sum_mem_used, max_mem_used, sum_mem_available, mem_total,
            sum_lms_cpu, sum_lms_rss, sum_ollama_cpu, sum_ollama_rss, n_gpu, sum_gpu, max_gpu, n_gpu_mem, sum_gpu_mem, max_gpu_mem,
            n_ane, sum_ane, max_ane)
         VALUES (?1, 1, ?2, ?2, ?3, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11, ?12, ?13, ?13, ?14, ?15, ?15)
         ON CONFLICT(bucket) DO UPDATE SET
            samples = samples + 1,
            sum_cpu = sum_cpu + excluded.sum_cpu, max_cpu = MAX(max_cpu, excluded.max_cpu),
            sum_mem_used = sum_mem_used + excluded.sum_mem_used, max_mem_used = MAX(max_mem_used, excluded.max_mem_used),
            sum_mem_available = sum_mem_available + excluded.sum_mem_available, mem_total = excluded.mem_total,
            sum_lms_cpu = sum_lms_cpu + excluded.sum_lms_cpu, sum_lms_rss = sum_lms_rss + excluded.sum_lms_rss,
            sum_ollama_cpu = sum_ollama_cpu + excluded.sum_ollama_cpu, sum_ollama_rss = sum_ollama_rss + excluded.sum_ollama_rss,
            n_gpu = n_gpu + excluded.n_gpu, sum_gpu = sum_gpu + excluded.sum_gpu, max_gpu = MAX(max_gpu, excluded.max_gpu),
            n_gpu_mem = n_gpu_mem + excluded.n_gpu_mem, sum_gpu_mem = sum_gpu_mem + excluded.sum_gpu_mem,
            max_gpu_mem = MAX(max_gpu_mem, excluded.max_gpu_mem),
            n_ane = n_ane + excluded.n_ane, sum_ane = sum_ane + excluded.sum_ane, max_ane = MAX(max_ane, excluded.max_ane)",
    )?
    .execute(params![
        bucket,
        cpu,
        s.mem_used_bytes as i64,
        s.mem_available_bytes as i64,
        s.mem_total_bytes as i64,
        f64::from(s.lmstudio.cpu_pct),
        s.lmstudio.rss_bytes as i64,
        f64::from(s.ollama.cpu_pct),
        s.ollama.rss_bytes as i64,
        i64::from(gpu.is_some()),
        gpu.unwrap_or(0.0),
        i64::from(gpu_mem.is_some()),
        gpu_mem.unwrap_or(0),
        i64::from(ane.is_some()),
        ane.unwrap_or(0.0),
    ])
    .context("upsert hardware_rollups_1m")?;
    Ok(())
}

pub fn upsert_model_seen(conn: &Connection, m: &ModelInfo, now: DateTime<Utc>) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO models_seen (backend, model_id, first_seen, last_seen, kind, compatibility_type, quantization, max_context_length, is_cloud, last_state)
         VALUES (?1, ?2, ?3, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(backend, model_id) DO UPDATE SET last_seen = excluded.last_seen, kind = COALESCE(excluded.kind, kind),
            compatibility_type = COALESCE(excluded.compatibility_type, compatibility_type),
            quantization = COALESCE(excluded.quantization, quantization),
            max_context_length = COALESCE(excluded.max_context_length, max_context_length),
            is_cloud = excluded.is_cloud, last_state = excluded.last_state",
    )?
    .execute(params![
        m.backend.as_str(),
        m.id,
        ms(now),
        m.kind,
        m.compatibility_type,
        m.quantization,
        m.max_context_length.map(|v| v as i64),
        m.is_cloud(),
        m.state.as_str(),
    ])
    .context("upsert models_seen")?;
    Ok(())
}
