//! Rolling metrics over completed inference requests: the 1m / 5m / 15m / session windows the TUIs
//! showed (requests, prompt and generated tokens, mean and p95 tokens per second, mean TTFT), plus the
//! per-model and per-backend breakdowns both apps computed but never displayed. Windows are keyed on
//! `ended_at`, as Ollama-Monitor did. The aggregator keeps a bounded number of records so a gateway that
//! runs for weeks does not grow without limit; the store holds the full history.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::types::{Backend, RequestRecord};

/// Records kept in memory for the session window; the oldest are dropped past this.
pub const MAX_RECORDS: usize = 50_000;

/// The figures for one window. `p50_tps` and `p95_tps` are nearest-rank percentiles over the records
/// that reported a positive rate.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct WindowStats {
    pub request_count: u64,
    pub prompt_tokens: u64,
    pub gen_tokens: u64,
    pub mean_tps: f64,
    pub p50_tps: f64,
    pub p95_tps: f64,
    pub mean_ttft_ms: f64,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct RollingMetrics {
    pub one_minute: WindowStats,
    pub five_minute: WindowStats,
    pub fifteen_minute: WindowStats,
    /// Since the gateway started or the session was last reset.
    pub session: WindowStats,
    /// Session-window figures per resolved (or requested) model id.
    pub per_model: BTreeMap<String, WindowStats>,
    /// Session-window figures per backend, keyed by its identifier.
    pub per_backend: BTreeMap<String, WindowStats>,
    pub session_started_at: DateTime<Utc>,
    pub computed_at: DateTime<Utc>,
}

/// The subset of a record the windows need.
#[derive(Debug, Clone, PartialEq)]
struct Sample {
    backend: Backend,
    model: String,
    ended_at: DateTime<Utc>,
    prompt_tokens: u64,
    gen_tokens: u64,
    tokens_per_sec: Option<f64>,
    ttft_ms: Option<f64>,
}

#[derive(Debug)]
pub struct Aggregator {
    samples: Vec<Sample>,
    session_started_at: DateTime<Utc>,
}

impl Aggregator {
    pub fn new(now: DateTime<Utc>) -> Self {
        Self {
            samples: Vec::new(),
            session_started_at: now,
        }
    }

    /// Takes a record into the windows when it is a completed inference exchange (the feed's rows, less
    /// the failures); anything else is ignored.
    pub fn ingest(&mut self, record: &RequestRecord) {
        if !record.is_feed_row() || !record.outcome.is_completed() {
            return;
        }
        self.samples.push(Sample {
            backend: record.backend,
            model: record
                .model_resolved
                .clone()
                .or_else(|| record.model_requested.clone())
                .unwrap_or_else(|| "?".to_string()),
            ended_at: record.ended_at,
            prompt_tokens: record.prompt_tokens.unwrap_or(0),
            gen_tokens: record.gen_tokens.unwrap_or(0),
            tokens_per_sec: record.tokens_per_sec,
            ttft_ms: record.ttft_ms,
        });
        if self.samples.len() > MAX_RECORDS {
            let excess = self.samples.len() - MAX_RECORDS;
            self.samples.drain(..excess);
        }
    }

    /// Clears the session, as the TUIs' `r` key did. The store keeps everything.
    pub fn reset(&mut self, now: DateTime<Utc>) {
        self.samples.clear();
        self.session_started_at = now;
    }

    pub fn session_started_at(&self) -> DateTime<Utc> {
        self.session_started_at
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Every window over every backend.
    pub fn snapshot(&self, now: DateTime<Utc>) -> RollingMetrics {
        self.snapshot_for(None, now)
    }

    /// Every window over one backend's records, or all of them.
    pub fn snapshot_for(&self, backend: Option<Backend>, now: DateTime<Utc>) -> RollingMetrics {
        let samples: Vec<&Sample> = self
            .samples
            .iter()
            .filter(|s| backend.is_none_or(|b| s.backend == b))
            .collect();
        let since = |secs: i64| {
            let cutoff = now - Duration::seconds(secs);
            window(
                samples
                    .iter()
                    .copied()
                    .filter(move |s| s.ended_at >= cutoff),
            )
        };

        let mut per_model: BTreeMap<String, Vec<&Sample>> = BTreeMap::new();
        let mut per_backend: BTreeMap<String, Vec<&Sample>> = BTreeMap::new();
        for s in &samples {
            per_model.entry(s.model.clone()).or_default().push(s);
            per_backend
                .entry(s.backend.as_str().to_string())
                .or_default()
                .push(s);
        }

        RollingMetrics {
            one_minute: since(60),
            five_minute: since(5 * 60),
            fifteen_minute: since(15 * 60),
            session: window(samples.iter().copied()),
            per_model: per_model
                .into_iter()
                .map(|(k, v)| (k, window(v.into_iter())))
                .collect(),
            per_backend: per_backend
                .into_iter()
                .map(|(k, v)| (k, window(v.into_iter())))
                .collect(),
            session_started_at: self.session_started_at,
            computed_at: now,
        }
    }
}

fn window<'a>(samples: impl Iterator<Item = &'a Sample>) -> WindowStats {
    let mut stats = WindowStats::default();
    let mut tps = Vec::new();
    let mut ttft_sum = 0.0;
    let mut ttft_count = 0u64;
    for s in samples {
        stats.request_count += 1;
        stats.prompt_tokens += s.prompt_tokens;
        stats.gen_tokens += s.gen_tokens;
        if let Some(rate) = s.tokens_per_sec
            && rate.is_finite()
            && rate > 0.0
        {
            tps.push(rate);
        }
        if let Some(ttft) = s.ttft_ms
            && ttft.is_finite()
        {
            ttft_sum += ttft;
            ttft_count += 1;
        }
    }
    if !tps.is_empty() {
        stats.mean_tps = tps.iter().sum::<f64>() / tps.len() as f64;
        tps.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        stats.p50_tps = percentile(&tps, 0.50);
        stats.p95_tps = percentile(&tps, 0.95);
    }
    if ttft_count > 0 {
        stats.mean_ttft_ms = ttft_sum / ttft_count as f64;
    }
    stats
}

/// Nearest-rank percentile over a sorted slice.
fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * q).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use uuid::Uuid;

    use super::*;
    use crate::types::{ApiFormat, FailureSource, Outcome, RequestKind};

    fn record(
        backend: Backend,
        model: &str,
        tps: f64,
        prompt: u64,
        gen_n: u64,
        ago_secs: i64,
    ) -> RequestRecord {
        let now = Utc::now();
        RequestRecord {
            id: Uuid::now_v7(),
            session_id: 1,
            backend,
            listener: "127.0.0.1:1".into(),
            kind: RequestKind::Http,
            client_ip: "127.0.0.1".into(),
            user_agent: None,
            client_request_id: None,
            method: "POST".into(),
            path: "/v1/chat/completions".into(),
            query: None,
            api_format: ApiFormat::OpenAiChat,
            streamed: true,
            model_requested: Some(model.into()),
            model_resolved: Some(model.into()),
            is_cloud: false,
            started_at: now - Duration::seconds(ago_secs + 1),
            first_byte_at: None,
            ended_at: now - Duration::seconds(ago_secs),
            status: Some(200),
            outcome: Outcome::Completed,
            error_summary: None,
            prompt_tokens: Some(prompt),
            gen_tokens: Some(gen_n),
            tokens_estimated: false,
            ttft_ms: Some(500.0),
            total_ms: 1000.0,
            tokens_per_sec: Some(tps),
            timings_estimated: false,
            stop_reason: Some("stop".into()),
            request_bytes: 10,
            response_bytes: 20,
            request_headers: vec![],
            response_headers: vec![],
            chunk_timeline: vec![],
            finding_count: 0,
            max_severity: None,
            hypothetical_cost: BTreeMap::new(),
            actual_cost_usd: None,
            routed_by: None,
        }
    }

    #[test]
    fn windows_filter_by_cutoff() {
        let mut agg = Aggregator::new(Utc::now());
        agg.ingest(&record(Backend::Ollama, "m1", 10.0, 1, 10, 30)); // 1m
        agg.ingest(&record(Backend::Ollama, "m1", 20.0, 2, 20, 90)); // 5m
        agg.ingest(&record(Backend::Ollama, "m1", 30.0, 3, 30, 600)); // 15m
        agg.ingest(&record(Backend::Ollama, "m1", 40.0, 4, 40, 5_000)); // session
        let s = agg.snapshot(Utc::now());
        assert_eq!(s.one_minute.request_count, 1);
        assert_eq!(s.five_minute.request_count, 2);
        assert_eq!(s.fifteen_minute.request_count, 3);
        assert_eq!(s.session.request_count, 4);
        assert_eq!(s.session.prompt_tokens, 10);
        assert_eq!(s.session.gen_tokens, 100);
        assert!((s.session.mean_tps - 25.0).abs() < 1e-9);
        assert!((s.session.mean_ttft_ms - 500.0).abs() < 1e-9);
    }

    #[test]
    fn per_model_and_per_backend_breakdowns_and_backend_filter() {
        let mut agg = Aggregator::new(Utc::now());
        agg.ingest(&record(Backend::Ollama, "a", 10.0, 1, 1, 0));
        agg.ingest(&record(Backend::Ollama, "a", 30.0, 1, 1, 0));
        agg.ingest(&record(Backend::LmStudio, "b", 50.0, 1, 1, 0));
        let s = agg.snapshot(Utc::now());
        assert_eq!(s.per_model["a"].request_count, 2);
        assert!((s.per_model["a"].mean_tps - 20.0).abs() < 1e-9);
        assert_eq!(s.per_backend["ollama"].request_count, 2);
        assert_eq!(s.per_backend["lmstudio"].request_count, 1);
        let lms = agg.snapshot_for(Some(Backend::LmStudio), Utc::now());
        assert_eq!(lms.session.request_count, 1);
        assert_eq!(lms.per_model.len(), 1);
        assert!(lms.per_model.contains_key("b"));
    }

    #[test]
    fn only_completed_inference_rows_count() {
        let mut agg = Aggregator::new(Utc::now());
        let mut failed = record(Backend::Ollama, "m", 10.0, 5, 5, 0);
        failed.outcome = Outcome::Failed {
            status: 500,
            source: FailureSource::Upstream,
        };
        agg.ingest(&failed);
        let mut passthrough = record(Backend::Ollama, "m", 10.0, 5, 5, 0);
        passthrough.api_format = ApiFormat::Passthrough;
        agg.ingest(&passthrough);
        let mut load = record(Backend::Ollama, "m", 10.0, 5, 5, 0);
        load.kind = RequestKind::Load;
        agg.ingest(&load);
        assert!(agg.is_empty());
        agg.ingest(&record(Backend::Ollama, "m", 10.0, 5, 5, 0));
        assert_eq!(agg.len(), 1);
    }

    #[test]
    fn zero_or_missing_rates_do_not_drag_the_mean_and_percentiles() {
        let mut agg = Aggregator::new(Utc::now());
        let mut no_rate = record(Backend::Ollama, "m", 0.0, 1, 1, 0);
        no_rate.tokens_per_sec = None;
        no_rate.ttft_ms = None;
        agg.ingest(&no_rate);
        agg.ingest(&record(Backend::Ollama, "m", 0.0, 1, 1, 0));
        agg.ingest(&record(Backend::Ollama, "m", 40.0, 1, 1, 0));
        let s = agg.snapshot(Utc::now());
        assert_eq!(s.session.request_count, 3);
        assert!((s.session.mean_tps - 40.0).abs() < 1e-9);
        assert!((s.session.p95_tps - 40.0).abs() < 1e-9);
        assert!(
            (s.session.mean_ttft_ms - 500.0).abs() < 1e-9,
            "two records reported TTFT"
        );
    }

    #[test]
    fn reset_clears_the_session_and_moves_its_start() {
        let start = Utc::now() - Duration::seconds(100);
        let mut agg = Aggregator::new(start);
        agg.ingest(&record(Backend::Ollama, "m", 10.0, 1, 1, 0));
        let later = Utc::now();
        agg.reset(later);
        assert!(agg.is_empty());
        assert_eq!(agg.session_started_at(), later);
        assert_eq!(agg.snapshot(later).session.request_count, 0);
    }

    #[test]
    fn the_record_cap_drops_the_oldest() {
        let mut agg = Aggregator::new(Utc::now());
        for i in 0..(MAX_RECORDS + 10) {
            agg.ingest(&record(Backend::Ollama, "m", 1.0, i as u64, 0, 0));
        }
        assert_eq!(agg.len(), MAX_RECORDS);
        assert_eq!(
            agg.samples[0].prompt_tokens, 10,
            "the first ten were dropped"
        );
    }

    #[test]
    fn percentile_indexing() {
        let v = [1.0, 2.0, 3.0, 4.0, 100.0];
        assert!((percentile(&v, 0.5) - 3.0).abs() < 1e-9);
        assert!((percentile(&v, 0.95) - 100.0).abs() < 1e-9);
        assert_eq!(percentile(&[], 0.5), 0.0);
    }
}
