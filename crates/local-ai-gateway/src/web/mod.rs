//! The dashboard server: the token gate, the embedded single-page app, the REST API and the WebSocket
//! event stream, served by axum-server over plain HTTP or TLS. `AppState` is what the handlers see: the
//! configuration, the store, the live backend and hardware state, the rolling aggregator and the proxy's
//! in-flight table.

pub mod access;
pub mod api;
pub mod assets;
pub mod tls;
pub mod token;
pub mod ws;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use axum::routing::{delete, get, post};
use axum::{Router, middleware};
use axum_server::tls_rustls::RustlsConfig;
use chrono::{DateTime, Utc};
use gateway_core::aggregate::Aggregator;
use gateway_core::config::Config;
use gateway_core::events::EventSender;
use gateway_core::pricing::PricingTable;
use gateway_core::state::BackendsState;
use gateway_core::store::Store;
use gateway_hardware::sampler::Latest;
use gateway_proxy::ProxyContext;
use tokio::sync::watch;

pub use api::status::ListenerStatus;

use crate::hot::Hot;

/// Everything the dashboard needs, assembled by `main`.
pub struct AppStateParts {
    /// The configuration, DLP engine, pricing and retention policy, all hot-reloadable.
    pub hot: Arc<Hot>,
    pub token: String,
    pub secure_cookies: bool,
    pub events: EventSender,
    pub store: Option<Store>,
    pub backends: Arc<BackendsState>,
    pub hardware: Latest,
    pub aggregator: Arc<Mutex<Aggregator>>,
    pub proxy: Option<Arc<ProxyContext>>,
    pub listeners: Vec<ListenerStatus>,
    pub session_id: Option<i64>,
}

struct Inner {
    parts: AppStateParts,
    started_at: DateTime<Utc>,
}

/// Cheap to clone; one per server.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

impl AppState {
    pub fn new(parts: AppStateParts) -> Self {
        Self {
            inner: Arc::new(Inner {
                parts,
                started_at: Utc::now(),
            }),
        }
    }

    /// A state with no store, proxy or pollers, for tests.
    #[cfg(test)]
    pub fn minimal(
        config: Arc<Config>,
        token: String,
        secure_cookies: bool,
        events: EventSender,
    ) -> Self {
        Self::new(AppStateParts {
            hot: Hot::standalone(config, events.clone()),
            token,
            secure_cookies,
            events,
            store: None,
            backends: Arc::new(BackendsState::new()),
            hardware: Arc::default(),
            aggregator: Arc::new(Mutex::new(Aggregator::new(Utc::now()))),
            proxy: None,
            listeners: Vec::new(),
            session_id: None,
        })
    }

    /// The current configuration; a reload swaps it whole.
    pub fn config(&self) -> Arc<Config> {
        self.inner.parts.hot.config()
    }

    pub fn hot(&self) -> &Arc<Hot> {
        &self.inner.parts.hot
    }

    pub fn token(&self) -> &str {
        &self.inner.parts.token
    }

    pub fn auth_required(&self) -> bool {
        self.config().dashboard.auth
    }

    /// Whether the token cookie carries `Secure` (the dashboard is served over TLS).
    pub fn secure_cookies(&self) -> bool {
        self.inner.parts.secure_cookies
    }

    pub fn started_at(&self) -> DateTime<Utc> {
        self.inner.started_at
    }

    pub fn events(&self) -> &EventSender {
        &self.inner.parts.events
    }

    pub fn store(&self) -> Option<&Store> {
        self.inner.parts.store.as_ref()
    }

    pub fn backends(&self) -> &BackendsState {
        &self.inner.parts.backends
    }

    pub fn hardware(&self) -> &Latest {
        &self.inner.parts.hardware
    }

    pub fn aggregator(&self) -> &Mutex<Aggregator> {
        &self.inner.parts.aggregator
    }

    pub fn proxy(&self) -> Option<&Arc<ProxyContext>> {
        self.inner.parts.proxy.as_ref()
    }

    pub fn pricing(&self) -> Arc<PricingTable> {
        self.inner.parts.hot.pricing()
    }

    pub fn listeners(&self) -> &[ListenerStatus] {
        &self.inner.parts.listeners
    }

    pub fn session_id(&self) -> Option<i64> {
        self.inner.parts.session_id
    }
}

/// Claims the dashboard socket. Called before the runtime exists so a taken port stops startup.
pub fn bind(addr: SocketAddr) -> Result<std::net::TcpListener> {
    let listener = std::net::TcpListener::bind(addr)
        .with_context(|| format!("bind dashboard listener {addr}"))?;
    listener
        .set_nonblocking(true)
        .context("set dashboard listener non-blocking")?;
    Ok(listener)
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/status", get(api::status::api_status))
        .route("/api/models", get(api::models::api_models))
        .route("/api/hardware/current", get(api::hardware::api_current))
        .route("/api/hardware/series", get(api::hardware::api_series))
        .route("/api/metrics/rolling", get(api::metrics::api_rolling))
        .route("/api/metrics/series", get(api::metrics::api_series))
        .route("/api/metrics/cost", get(api::metrics::api_cost))
        .route("/api/metrics/lifetime", get(api::metrics::api_lifetime))
        .route("/api/requests", get(api::requests::api_list))
        .route("/api/requests/inflight", get(api::requests::api_inflight))
        .route("/api/requests/{id}", get(api::requests::api_detail))
        .route(
            "/api/requests/{id}/body/{direction}",
            get(api::requests::api_body),
        )
        .route(
            "/api/requests/{id}/bodies",
            delete(api::requests::api_delete_bodies),
        )
        .route("/api/findings", get(api::alerts::api_findings))
        .route("/api/alerts", get(api::alerts::api_alerts))
        .route("/api/alerts/ack-all", post(api::alerts::api_ack_all))
        .route("/api/alerts/{id}/ack", post(api::alerts::api_ack))
        .route(
            "/api/config",
            get(api::config::api_config).put(api::config::api_config_put),
        )
        .route("/api/config/file", get(api::config::api_config_file))
        .route(
            "/api/config/validate",
            post(api::config::api_config_validate),
        )
        .route("/api/config/reload", post(api::dlp::api_reload))
        .route("/api/dlp/rules", get(api::dlp::api_rules))
        .route("/api/dlp/test", post(api::dlp::api_test))
        .route("/api/dlp/reload", post(api::dlp::api_reload))
        .route("/api/session/reset", post(api::session::api_reset))
        .route("/api/retention/run", post(api::session::api_retention_run))
        .route("/api/sessions", get(api::session::api_sessions))
        .route("/ws", get(ws::ws_handler))
        .fallback(assets::static_handler)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            access::require_token,
        ))
        .with_state(state)
}

/// Serves until `shutdown` flips, then gives open connections `grace` to finish.
pub async fn serve(
    listener: std::net::TcpListener,
    tls: Option<RustlsConfig>,
    app: Router,
    mut shutdown: watch::Receiver<bool>,
    grace: Duration,
) -> Result<()> {
    let handle = axum_server::Handle::new();
    let stopper = handle.clone();
    tokio::spawn(async move {
        let _ = shutdown.changed().await;
        stopper.graceful_shutdown(Some(grace));
    });
    let served = match tls {
        Some(config) => {
            axum_server::from_tcp_rustls(listener, config)
                .handle(handle)
                .serve(app.into_make_service())
                .await
        }
        None => {
            axum_server::from_tcp(listener)
                .handle(handle)
                .serve(app.into_make_service())
                .await
        }
    };
    served.context("dashboard server")
}
