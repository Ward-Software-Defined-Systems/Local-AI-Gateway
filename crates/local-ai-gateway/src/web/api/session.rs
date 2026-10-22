//! Session reset (the TUIs' `r`), retention on demand, and the session list.

use axum::extract::State;
use chrono::{DateTime, Utc};
use gateway_core::store::RetentionReport;
use gateway_core::store::read;
use gateway_core::types::Session;
use serde::Serialize;

use super::{ApiError, ApiResult, ok, store};
use crate::web::AppState;

#[derive(Debug, Serialize)]
pub struct Reset {
    pub session_started_at: DateTime<Utc>,
}

pub async fn api_reset(State(st): State<AppState>) -> ApiResult<Reset> {
    let now = Utc::now();
    st.aggregator()
        .lock()
        .map(|mut agg| agg.reset(now))
        .map_err(|_| ApiError::internal(anyhow::anyhow!("aggregator poisoned")))?;
    ok(Reset {
        session_started_at: now,
    })
}

pub async fn api_retention_run(State(st): State<AppState>) -> ApiResult<RetentionReport> {
    let store = store(&st)?;
    let policy = st.hot().retention();
    let report = store
        .writer
        .run_retention(policy, Utc::now())
        .await
        .map_err(ApiError::internal)?;
    ok(report)
}

pub async fn api_sessions(State(st): State<AppState>) -> ApiResult<Vec<Session>> {
    let store = store(&st)?;
    let sessions = store
        .reads
        .run(|c| read::list_sessions(c, 100))
        .await
        .map_err(ApiError::internal)?;
    ok(sessions)
}
