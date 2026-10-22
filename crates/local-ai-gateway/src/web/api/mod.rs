//! The REST API behind the dashboard. Every handler returns JSON; errors are `{"error": {"code",
//! "message"}}` with a fitting status. Query times are RFC 3339; windows default to the recent past.

pub mod alerts;
pub mod config;
pub mod dlp;
pub mod hardware;
pub mod metrics;
pub mod models;
pub mod requests;
pub mod session;
pub mod status;

use axum::Json;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Duration, Utc};
use gateway_core::store::Store;
use serde::Serialize;
use serde_json::json;

use super::AppState;

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

impl ApiError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "bad_request",
            message: message.into(),
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "not_found",
            message: message.into(),
        }
    }

    pub fn internal(err: anyhow::Error) -> Self {
        tracing::warn!(error = %format!("{err:#}"), "api request failed");
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal",
            message: format!("{err:#}"),
        }
    }

    pub fn store_disabled() -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "store_disabled",
            message: "the store is not available".into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            [(header::CACHE_CONTROL, "no-store")],
            Json(json!({"error": {"code": self.code, "message": self.message}})),
        )
            .into_response()
    }
}

pub type ApiResult<T> = Result<Json<T>, ApiError>;

pub fn ok<T: Serialize>(value: T) -> ApiResult<T> {
    Ok(Json(value))
}

pub fn store(st: &AppState) -> Result<&Store, ApiError> {
    st.store().ok_or_else(ApiError::store_disabled)
}

/// `[from, to)` with defaults: `to` is now, `from` is `default_span` before `to`.
pub fn window(
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    default_span: Duration,
) -> Result<(DateTime<Utc>, DateTime<Utc>), ApiError> {
    let to = to.unwrap_or_else(Utc::now);
    let from = from.unwrap_or(to - default_span);
    if from >= to {
        return Err(ApiError::bad_request("`from` must be before `to`"));
    }
    Ok((from, to))
}

/// A bucket width that yields about `target_points` points over the window, at least one second.
pub fn default_bucket_ms(from: DateTime<Utc>, to: DateTime<Utc>, target_points: i64) -> i64 {
    let span = (to - from).num_milliseconds().max(1000);
    let raw = span / target_points.max(1);
    // Round up to a tidy step: 1s, 5s, 10s, 30s, 1m, 5m, 15m, 30m, 1h, 2h, 6h, 12h, 1d.
    const STEPS: [i64; 13] = [
        1_000, 5_000, 10_000, 30_000, 60_000, 300_000, 900_000, 1_800_000, 3_600_000, 7_200_000,
        21_600_000, 43_200_000, 86_400_000,
    ];
    STEPS
        .iter()
        .copied()
        .find(|s| *s >= raw)
        .unwrap_or(86_400_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_default_to_the_recent_past_and_reject_inverted_ranges() {
        let (from, to) = window(None, None, Duration::hours(1)).unwrap();
        assert_eq!((to - from).num_minutes(), 60);
        let now = Utc::now();
        assert!(
            window(
                Some(now),
                Some(now - Duration::seconds(1)),
                Duration::hours(1)
            )
            .is_err()
        );
    }

    #[test]
    fn default_buckets_are_tidy_steps() {
        let now = Utc::now();
        assert_eq!(
            default_bucket_ms(now - Duration::minutes(2), now, 120),
            1_000
        );
        assert_eq!(
            default_bucket_ms(now - Duration::hours(1), now, 120),
            30_000
        );
        assert_eq!(
            default_bucket_ms(now - Duration::days(7), now, 120),
            7_200_000
        );
        assert_eq!(
            default_bucket_ms(now - Duration::days(365), now, 120),
            86_400_000
        );
    }
}
