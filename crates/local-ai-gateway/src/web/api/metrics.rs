//! Rolling windows, time series, the cost table and lifetime totals.

use std::collections::BTreeMap;

use axum::extract::{Query, State};
use chrono::{DateTime, Duration, Utc};
use gateway_core::aggregate::RollingMetrics;
use gateway_core::pricing::HypotheticalCost;
use gateway_core::store::read::{self, SeriesMetric, SeriesPoint, SpendRow};
use gateway_core::types::{Backend, BackendGroup, LifetimeTotals};
use serde::{Deserialize, Serialize};

use super::{ApiError, ApiResult, default_bucket_ms, ok, store, window};
use crate::web::AppState;

/// Whether a backend is inside a `backend=` / `group=` scope; both absent means everything.
fn in_scope(backend: Option<Backend>, group: Option<BackendGroup>, candidate: Backend) -> bool {
    backend.is_none_or(|want| candidate == want) && group.is_none_or(|g| g.contains(candidate))
}

#[derive(Debug, Deserialize)]
pub struct RollingQuery {
    pub backend: Option<Backend>,
    pub group: Option<BackendGroup>,
}

pub async fn api_rolling(
    State(st): State<AppState>,
    Query(q): Query<RollingQuery>,
) -> ApiResult<RollingMetrics> {
    let snapshot = st
        .aggregator()
        .lock()
        .map(|agg| agg.snapshot_where(|b| in_scope(q.backend, q.group, b), Utc::now()))
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
    pub group: Option<BackendGroup>,
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
        group: q.group,
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
    pub group: Option<BackendGroup>,
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
    let (backend, group) = (q.backend, q.group);
    let (prompt_tokens, gen_tokens) = store
        .reads
        .run(move |c| read::token_totals(c, from, to, backend, group))
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

#[derive(Debug, Deserialize)]
pub struct SpendQuery {
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
}

/// What the cloud providers charged at list price over a window, per provider.
#[derive(Debug, Serialize)]
pub struct Spend {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub rows: Vec<SpendRow>,
    pub total_usd: f64,
}

pub async fn api_spend(
    State(st): State<AppState>,
    Query(q): Query<SpendQuery>,
) -> ApiResult<Spend> {
    let store = store(&st)?;
    let (from, to) = window(q.from, q.to, Duration::hours(24))?;
    let rows = store
        .reads
        .run(move |c| read::cloud_spend(c, from, to))
        .await
        .map_err(ApiError::internal)?;
    // `f64::sum` starts from -0.0, which an empty table would show as "-0.0".
    let total_usd = rows.iter().map(|r| r.spend_usd).fold(0.0, |a, b| a + b);
    ok(Spend {
        from,
        to,
        rows,
        total_usd,
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
