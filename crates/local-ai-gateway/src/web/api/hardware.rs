//! `GET /api/hardware/current` and `GET /api/hardware/series`.

use axum::extract::{Query, State};
use chrono::{DateTime, Duration, Utc};
use gateway_core::store::read;
use gateway_core::types::HardwareSample;
use serde::Deserialize;

use super::{ApiError, ApiResult, default_bucket_ms, ok, store, window};
use crate::web::AppState;

pub async fn api_current(State(st): State<AppState>) -> ApiResult<Option<HardwareSample>> {
    ok(st.hardware().read().await.clone())
}

#[derive(Debug, Deserialize)]
pub struct SeriesQuery {
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub step_s: Option<i64>,
}

pub async fn api_series(
    State(st): State<AppState>,
    Query(q): Query<SeriesQuery>,
) -> ApiResult<Vec<read::HardwarePoint>> {
    let store = store(&st)?;
    let (from, to) = window(q.from, q.to, Duration::minutes(15))?;
    let step_ms = q
        .step_s
        .map(|s| s.max(1) * 1000)
        .unwrap_or_else(|| default_bucket_ms(from, to, 300));
    let points = store
        .reads
        .run(move |c| read::hardware_series(c, from, to, step_ms))
        .await
        .map_err(ApiError::internal)?;
    ok(points)
}
