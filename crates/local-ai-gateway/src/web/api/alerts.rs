//! Findings and alerts: lists, acknowledgement. The engine that produces them arrives in Phase 4; the
//! routes already serve what the store holds.

use axum::extract::{Path, Query, State};
use chrono::{DateTime, Utc};
use gateway_core::store::read::{self, FindingFilter, FindingRow};
use gateway_core::types::{Alert, Backend, Direction, Severity};
use serde::{Deserialize, Serialize};

use super::{ApiError, ApiResult, ok, store};
use crate::web::AppState;

#[derive(Debug, Deserialize)]
pub struct FindingsQuery {
    pub rule: Option<String>,
    pub severity: Option<Severity>,
    pub backend: Option<Backend>,
    pub direction: Option<Direction>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub limit: Option<usize>,
    pub before: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct FindingsPage {
    pub items: Vec<FindingRow>,
    pub next_before: Option<i64>,
}

pub async fn api_findings(
    State(st): State<AppState>,
    Query(q): Query<FindingsQuery>,
) -> ApiResult<FindingsPage> {
    let store = store(&st)?;
    let filter = FindingFilter {
        rule: q.rule.filter(|s| !s.is_empty()),
        severity: q.severity,
        backend: q.backend,
        direction: q.direction,
        from: q.from,
        to: q.to,
        limit: q.limit.unwrap_or(50),
        before_id: q.before,
    };
    let (items, next_before) = store
        .reads
        .run(move |c| read::list_findings(c, &filter))
        .await
        .map_err(ApiError::internal)?;
    ok(FindingsPage { items, next_before })
}

#[derive(Debug, Deserialize)]
pub struct AlertsQuery {
    pub unacked: Option<bool>,
    pub limit: Option<usize>,
    pub before: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct AlertsPage {
    pub items: Vec<Alert>,
    pub next_before: Option<i64>,
    pub unacked: u64,
}

pub async fn api_alerts(
    State(st): State<AppState>,
    Query(q): Query<AlertsQuery>,
) -> ApiResult<AlertsPage> {
    let store = store(&st)?;
    let unacked_only = q.unacked.unwrap_or(false);
    let limit = q.limit.unwrap_or(50);
    let before = q.before;
    let (items, next_before, unacked) = store
        .reads
        .run(move |c| {
            let (items, next) = read::list_alerts(c, unacked_only, before, limit)?;
            Ok((items, next, read::unacked_alert_count(c)?))
        })
        .await
        .map_err(ApiError::internal)?;
    ok(AlertsPage {
        items,
        next_before,
        unacked,
    })
}

#[derive(Debug, Serialize)]
pub struct Acked {
    pub acknowledged: usize,
}

pub async fn api_ack(State(st): State<AppState>, Path(id): Path<i64>) -> ApiResult<Acked> {
    let store = store(&st)?;
    let acked = store
        .writer
        .ack_alert(id)
        .await
        .map_err(ApiError::internal)?;
    if !acked {
        return Err(ApiError::not_found(format!("no unacknowledged alert {id}")));
    }
    ok(Acked { acknowledged: 1 })
}

pub async fn api_ack_all(State(st): State<AppState>) -> ApiResult<Acked> {
    let store = store(&st)?;
    let acknowledged = store
        .writer
        .ack_all_alerts()
        .await
        .map_err(ApiError::internal)?;
    ok(Acked { acknowledged })
}
