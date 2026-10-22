//! The request list, in-flight table, detail and bodies.

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use chrono::{DateTime, Utc};
use gateway_core::store::read::{self, RequestDetail, RequestFilter, RequestPage};
use gateway_core::types::{Backend, Direction, InflightRequest};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{ApiError, ApiResult, ok, store};
use crate::web::AppState;

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub cursor: Option<String>,
    pub limit: Option<usize>,
    pub backend: Option<Backend>,
    pub model: Option<String>,
    pub outcome: Option<String>,
    pub format: Option<String>,
    pub client: Option<String>,
    pub cloud: Option<bool>,
    pub has_findings: Option<bool>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub q: Option<String>,
    pub include_passthrough: Option<bool>,
}

pub async fn api_list(
    State(st): State<AppState>,
    Query(q): Query<ListQuery>,
) -> ApiResult<RequestPage> {
    let store = store(&st)?;
    let filter = RequestFilter {
        backend: q.backend,
        model: q.model.filter(|s| !s.is_empty()),
        outcome: q.outcome.filter(|s| !s.is_empty()),
        api_format: q.format.filter(|s| !s.is_empty()),
        client: q.client.filter(|s| !s.is_empty()),
        cloud: q.cloud,
        has_findings: q.has_findings,
        from: q.from,
        to: q.to,
        q: q.q.filter(|s| !s.trim().is_empty()),
        include_passthrough: q.include_passthrough.unwrap_or(false),
        limit: q.limit.unwrap_or(50),
        cursor: q.cursor.filter(|s| !s.is_empty()),
    };
    let page = store
        .reads
        .run(move |c| read::list_requests(c, &filter))
        .await
        .map_err(|err| {
            if err.to_string().contains("invalid cursor") {
                ApiError::bad_request("invalid cursor")
            } else {
                ApiError::internal(err)
            }
        })?;
    ok(page)
}

pub async fn api_inflight(State(st): State<AppState>) -> ApiResult<Vec<InflightRequest>> {
    let mut inflight = st.proxy().map(|p| p.inflight()).unwrap_or_default();
    inflight.sort_by_key(|r| r.started_at);
    ok(inflight)
}

pub async fn api_detail(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<RequestDetail> {
    let store = store(&st)?;
    let detail = store
        .reads
        .run(move |c| read::get_request(c, id))
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("no request {id}")))?;
    ok(detail)
}

#[derive(Debug, Deserialize)]
pub struct BodyQuery {
    pub download: Option<bool>,
}

pub async fn api_body(
    State(st): State<AppState>,
    Path((id, direction)): Path<(Uuid, String)>,
    Query(q): Query<BodyQuery>,
) -> Result<Response, ApiError> {
    let store = store(&st)?;
    let direction: Direction = direction
        .parse()
        .map_err(|_| ApiError::bad_request("direction must be `request` or `response`"))?;
    let body = store
        .reads
        .run(move |c| read::get_body(c, id, direction))
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("no {} body for {id}", direction.as_str())))?;
    let content_type = if body.is_text {
        body.content_type
            .clone()
            .filter(|ct| ct.starts_with("application/json") || ct.starts_with("text/"))
            .unwrap_or_else(|| "text/plain; charset=utf-8".to_string())
    } else {
        "text/plain; charset=utf-8".to_string()
    };
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "no-store")
        .header(
            "x-body-truncated",
            if body.truncated { "true" } else { "false" },
        )
        .header(
            "x-body-redacted",
            if body.redacted { "true" } else { "false" },
        )
        .header("x-body-original-bytes", body.original_bytes.to_string());
    if q.download.unwrap_or(false) {
        builder = builder.header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{id}-{}.txt\"", direction.as_str()),
        );
    }
    builder
        .body(Body::from(body.text))
        .map_err(|e| ApiError::internal(e.into()))
}

#[derive(Debug, Serialize)]
pub struct Deleted {
    pub deleted: usize,
}

pub async fn api_delete_bodies(
    State(st): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Deleted> {
    let store = store(&st)?;
    let deleted = store
        .writer
        .delete_bodies(id)
        .await
        .map_err(ApiError::internal)?;
    ok(Deleted { deleted })
}
