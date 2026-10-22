//! Rolling windows, time series, the cost table and lifetime totals.

use std::collections::BTreeMap;

use axum::extract::{Query, State};
use chrono::{DateTime, Duration, Utc};
use gateway_core::aggregate::RollingMetrics;
use gateway_core::pricing::HypotheticalCost;
use gateway_core::store::read::{self, SeriesMetric, SeriesPoint};
use gateway_core::types::{Backend, LifetimeTotals};
use serde::{Deserialize, Serialize};

use super::{ApiError, ApiResult, default_bucket_ms, ok, store, window};
use crate::web::AppState;

#[derive(Debug, Deserialize)]
pub struct RollingQuery {
    pub backend: Option<Backend>,
}

pub async fn api_rolling(
    State(st): State<AppState>,
    Query(q): Query<RollingQuery>,
) -> ApiResult<RollingMetrics> {
    let snapshot = st
        .aggregator()
        .lock()
        .map(|agg| agg.snapshot_for(q.backend, Utc::now()))
        .map_err(|_| ApiError::internal(anyhow::anyhow!("aggregator poisoned")))?;
    ok(snapshot)
}

#[derive(Debug, Deserialize)]
pub struct SeriesQuery {
    pub metric: SeriesMetric,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub bucket_s: Option<i64>,
    pub backend: Option<Backend>,
    pub model: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Series {
    pub metric: SeriesMetric,
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub bucket_ms: i64,
    pub points: Vec<SeriesPoint>,
}

pub async fn api_series(
    State(st): State<AppState>,
    Query(q): Query<SeriesQuery>,
) -> ApiResult<Series> {
    let store = store(&st)?;
    let (from, to) = window(q.from, q.to, Duration::hours(1))?;
    let bucket_ms = q
        .bucket_s
        .map(|s| s.max(1) * 1000)
        .unwrap_or_else(|| default_bucket_ms(from, to, 120));
    let query = read::SeriesQuery {
        metric: q.metric,
        from,
        to,
        bucket_ms,
        backend: q.backend,
        model: q.model,
    };
    let points = store
        .reads
        .run(move |c| {
            let rows = read::series_rows(c, &query)?;
            Ok(read::aggregate_series(&rows, &query))
        })
        .await
        .map_err(ApiError::internal)?;
    ok(Series {
        metric: q.metric,
        from,
        to,
        bucket_ms,
        points,
    })
}

#[derive(Debug, Deserialize)]
pub struct CostQuery {
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub backend: Option<Backend>,
}

#[derive(Debug, Serialize)]
pub struct CostTable {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub prompt_tokens: u64,
    pub gen_tokens: u64,
    /// Frontier model key → cost; keys without a rate are absent and the UI shows `(no rate)`.
    pub costs: BTreeMap<String, HypotheticalCost>,
    pub columns: Vec<&'static str>,
}

pub async fn api_cost(
    State(st): State<AppState>,
    Query(q): Query<CostQuery>,
) -> ApiResult<CostTable> {
    let store = store(&st)?;
    let (from, to) = window(q.from, q.to, Duration::hours(24))?;
    let backend = q.backend;
    let (prompt_tokens, gen_tokens) = store
        .reads
        .run(move |c| read::token_totals(c, from, to, backend))
        .await
        .map_err(ApiError::internal)?;
    ok(CostTable {
        from,
        to,
        prompt_tokens,
        gen_tokens,
        costs: st.pricing().costs(prompt_tokens, gen_tokens),
        columns: gateway_core::pricing::FRONTIER_MODELS.to_vec(),
    })
}

#[derive(Debug, Serialize)]
pub struct Lifetime {
    #[serde(flatten)]
    pub totals: LifetimeTotals,
    pub session_started_at: DateTime<Utc>,
}

pub async fn api_lifetime(State(st): State<AppState>) -> ApiResult<Lifetime> {
    let store = store(&st)?;
    let totals = store
        .reads
        .run(read::lifetime_totals)
        .await
        .map_err(ApiError::internal)?;
    let session_started_at = st
        .aggregator()
        .lock()
        .map(|agg| agg.session_started_at())
        .unwrap_or_else(|_| st.started_at());
    ok(Lifetime {
        totals,
        session_started_at,
    })
}
