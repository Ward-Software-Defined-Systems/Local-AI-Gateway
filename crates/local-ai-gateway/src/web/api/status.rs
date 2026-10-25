//! `GET /api/status`: what the gateway is, where it listens, what it fronts, and how the store is doing.

use std::net::SocketAddr;

use axum::extract::State;
use chrono::{DateTime, Duration, Utc};
use gateway_core::store::read::ActivitySummary;
use gateway_core::types::{Backend, Platform};
use serde::Serialize;

use super::{ApiResult, ok};
use crate::VERSION;
use crate::web::AppState;

#[derive(Debug, Clone, Serialize)]
pub struct ListenerStatus {
    pub listen: SocketAddr,
    /// `true` for the unified listener, which picks the backend per request.
    pub unified: bool,
    /// The one backend of a fixed listener.
    pub backend: Option<Backend>,
    pub upstream: Option<String>,
    /// Every backend the listener can send a request to.
    pub targets: Vec<Backend>,
    pub bound: bool,
}

#[derive(Debug, Serialize)]
pub struct Status {
    pub version: &'static str,
    pub platform: Platform,
    pub started_at: DateTime<Utc>,
    pub uptime_s: i64,
    pub session_id: Option<i64>,
    pub dashboard: DashboardStatus,
    pub listeners: Vec<ListenerStatus>,
    pub backends: Vec<BackendStatus>,
    pub db: Option<DbStatus>,
    pub inflight: usize,
    pub unacked_alerts: u64,
    pub dlp: DlpStatus,
    pub paths: PathsStatus,
}

#[derive(Debug, Serialize)]
pub struct DlpStatus {
    pub enabled: bool,
    pub rules_total: usize,
    pub rules_enabled: usize,
    pub custom_rules: usize,
    pub known_secrets: usize,
    pub allowlist_entries: usize,
    pub findings_24h: u64,
    pub blocked_24h: u64,
    pub cloud_requests_24h: u64,
    /// What the cloud providers charged at list price in the last 24 hours.
    pub cloud_spend_24h_usd: f64,
}

#[derive(Debug, Serialize)]
pub struct PathsStatus {
    pub data_dir: String,
    pub config_file: String,
    pub config_exists: bool,
    pub token_file: String,
    pub db_file: String,
    pub known_secrets_file: String,
    pub logs_dir: String,
}

#[derive(Debug, Serialize)]
pub struct DashboardStatus {
    pub listen: SocketAddr,
    pub tls: bool,
    pub auth: bool,
}

#[derive(Debug, Serialize)]
pub struct BackendStatus {
    pub backend: Backend,
    pub name: &'static str,
    pub enabled: bool,
    pub listen: Vec<SocketAddr>,
    pub upstream: String,
    pub reachable: Option<bool>,
    pub version: Option<String>,
    pub error: Option<String>,
    pub polled_at: Option<DateTime<Utc>>,
    pub models_total: usize,
    pub models_loaded: usize,
    pub models_cloud: usize,
}

#[derive(Debug, Serialize)]
pub struct DbStatus {
    pub path: String,
    pub size_bytes: u64,
    pub bodies: u64,
    pub retention_days: u32,
}

pub async fn api_status(State(st): State<AppState>) -> ApiResult<Status> {
    let config = st.config();
    let now = Utc::now();
    let mut backends = Vec::new();
    for (backend, cfg) in config.backends.iter() {
        let snapshot = st.backends().get(backend).await;
        backends.push(BackendStatus {
            backend,
            name: backend.display_name(),
            enabled: cfg.enabled,
            listen: cfg.listen.clone(),
            upstream: cfg.upstream.clone(),
            reachable: snapshot.as_ref().map(|s| s.reachable),
            version: snapshot.as_ref().and_then(|s| s.version.clone()),
            error: snapshot.as_ref().and_then(|s| s.error.clone()),
            polled_at: snapshot.as_ref().map(|s| s.polled_at),
            models_total: snapshot.as_ref().map(|s| s.models.len()).unwrap_or(0),
            models_loaded: snapshot
                .as_ref()
                .map(|s| {
                    s.models
                        .iter()
                        .filter(|m| m.state == gateway_core::types::ModelState::Loaded)
                        .count()
                })
                .unwrap_or(0),
            models_cloud: snapshot
                .as_ref()
                .map(|s| s.models.iter().filter(|m| m.is_cloud()).count())
                .unwrap_or(0),
        });
    }
    let since = now - Duration::hours(24);
    let (db, unacked, activity) = match st.store() {
        Some(store) => {
            let path = store.path.display().to_string();
            let stats = store
                .reads
                .run(move |c| {
                    Ok((
                        gateway_core::store::read::db_size_bytes(c)?,
                        gateway_core::store::read::body_count(c)?,
                        gateway_core::store::read::unacked_alert_count(c)?,
                        gateway_core::store::read::activity_since(c, since)?,
                    ))
                })
                .await
                .unwrap_or((0, 0, 0, ActivitySummary::default()));
            (
                Some(DbStatus {
                    path,
                    size_bytes: stats.0,
                    bodies: stats.1,
                    retention_days: config.capture.retention_days,
                }),
                stats.2,
                stats.3,
            )
        }
        None => (None, 0, ActivitySummary::default()),
    };
    let engine = st.hot().engine();
    let paths = st.hot().paths();
    let config_file = st.hot().config_path();
    ok(Status {
        version: VERSION,
        platform: Platform::current(),
        started_at: st.started_at(),
        uptime_s: (now - st.started_at()).num_seconds().max(0),
        session_id: st.session_id(),
        dashboard: DashboardStatus {
            listen: config.dashboard.listen,
            tls: st.secure_cookies(),
            auth: config.dashboard.auth,
        },
        listeners: st.listeners().to_vec(),
        backends,
        db,
        inflight: st.proxy().map(|p| p.inflight_count()).unwrap_or(0),
        unacked_alerts: unacked,
        dlp: DlpStatus {
            enabled: engine.is_enabled(),
            rules_total: engine.rules().len(),
            rules_enabled: engine.rules().iter().filter(|r| r.enabled).count(),
            custom_rules: config.dlp.custom_rules.len(),
            known_secrets: engine.known_secret_count(),
            allowlist_entries: config.dlp.allowlist.len(),
            findings_24h: activity.findings,
            blocked_24h: activity.blocked_requests,
            cloud_requests_24h: activity.cloud_requests,
            cloud_spend_24h_usd: activity.cloud_spend_usd,
        },
        paths: PathsStatus {
            data_dir: paths.data_dir.display().to_string(),
            config_file: config_file.display().to_string(),
            config_exists: config_file.exists(),
            token_file: paths
                .in_data_dir(&config.dashboard.token_file)
                .display()
                .to_string(),
            db_file: paths.db_file().display().to_string(),
            known_secrets_file: paths
                .in_data_dir(&config.dlp.known_secrets_file)
                .display()
                .to_string(),
            logs_dir: paths.logs_dir().display().to_string(),
        },
    })
}
