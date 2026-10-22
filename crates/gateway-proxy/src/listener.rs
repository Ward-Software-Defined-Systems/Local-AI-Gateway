//! One axum server per configured listen address, each bound to one backend. `bind` claims the socket
//! before the runtime exists so a taken port stops startup; `serve` runs the pipeline as a catch-all
//! handler with graceful shutdown. `ProxyContext` is what every listener shares: the upstream HTTP client,
//! the store and event bus, the DLP scanner, the capture policy, pricing, the backend snapshots and the
//! in-flight table.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::{Context, Result};
use axum::Router;
use axum::routing::any;
use gateway_core::config::{BackendConfig, CaptureConfig};
use gateway_core::events::EventSender;
use gateway_core::pricing::PricingTable;
use gateway_core::state::BackendsState;
use gateway_core::store::StoreHandle;
use gateway_core::types::{Backend, InflightRequest};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_util::task::TaskTracker;
use tracing::info;
use uuid::Uuid;

use crate::scan::{NoopScanner, RequestScanner};

#[derive(Debug, Clone)]
pub struct ListenerConfig {
    pub backend: Backend,
    pub listen: SocketAddr,
    /// `http://host:port`, no trailing slash.
    pub upstream: String,
    pub capture_bodies: bool,
    pub record_passthrough: bool,
    pub rewrite_include_usage: bool,
}

impl ListenerConfig {
    /// One listener per configured address of an enabled backend.
    pub fn from_backend(backend: Backend, cfg: &BackendConfig) -> Vec<ListenerConfig> {
        if !cfg.enabled {
            return Vec::new();
        }
        cfg.listen
            .iter()
            .map(|listen| ListenerConfig {
                backend,
                listen: *listen,
                upstream: cfg.upstream.trim_end_matches('/').to_string(),
                capture_bodies: cfg.capture_bodies,
                record_passthrough: cfg.record_passthrough,
                rewrite_include_usage: cfg.rewrite_include_usage,
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapturePolicy {
    pub max_body_bytes: usize,
    pub max_buffered_request_bytes: usize,
}

impl From<&CaptureConfig> for CapturePolicy {
    fn from(cfg: &CaptureConfig) -> Self {
        Self {
            max_body_bytes: usize::try_from(cfg.max_body_bytes).unwrap_or(usize::MAX),
            max_buffered_request_bytes: usize::try_from(cfg.max_buffered_request_bytes)
                .unwrap_or(usize::MAX),
        }
    }
}

pub struct ProxyContext {
    pub client: reqwest::Client,
    pub store: Option<StoreHandle>,
    pub events: EventSender,
    scanner: RwLock<Arc<dyn RequestScanner>>,
    capture: RwLock<CapturePolicy>,
    pricing: RwLock<Arc<PricingTable>>,
    pub session_id: i64,
    pub backends: Arc<BackendsState>,
    inflight: Mutex<BTreeMap<Uuid, InflightRequest>>,
    /// Open WebSocket tunnels, so shutdown can wait for them.
    pub tunnels: TaskTracker,
}

impl ProxyContext {
    pub fn new(
        store: Option<StoreHandle>,
        events: EventSender,
        session_id: i64,
        pricing: Arc<PricingTable>,
        backends: Arc<BackendsState>,
        capture: CapturePolicy,
    ) -> Result<Arc<Self>> {
        // No overall timeout: a long generation is legitimate. No decompression: bytes reach the client
        // exactly as the backend sent them.
        let client = reqwest::Client::builder()
            .pool_idle_timeout(Duration::from_secs(90))
            .connect_timeout(Duration::from_secs(5))
            .tcp_nodelay(true)
            .build()
            .context("build proxy http client")?;
        Ok(Arc::new(Self {
            client,
            store,
            events,
            scanner: RwLock::new(Arc::new(NoopScanner)),
            capture: RwLock::new(capture),
            pricing: RwLock::new(pricing),
            session_id,
            backends,
            inflight: Mutex::new(BTreeMap::new()),
            tunnels: TaskTracker::new(),
        }))
    }

    pub fn scanner(&self) -> Arc<dyn RequestScanner> {
        self.scanner
            .read()
            .map(|s| s.clone())
            .unwrap_or_else(|_| Arc::new(NoopScanner))
    }

    pub fn set_scanner(&self, scanner: Arc<dyn RequestScanner>) {
        if let Ok(mut slot) = self.scanner.write() {
            *slot = scanner;
        }
    }

    pub fn capture(&self) -> CapturePolicy {
        self.capture.read().map(|c| *c).unwrap_or(CapturePolicy {
            max_body_bytes: 4 * 1024 * 1024,
            max_buffered_request_bytes: 64 * 1024 * 1024,
        })
    }

    pub fn set_capture(&self, policy: CapturePolicy) {
        if let Ok(mut slot) = self.capture.write() {
            *slot = policy;
        }
    }

    pub fn pricing(&self) -> Arc<PricingTable> {
        self.pricing
            .read()
            .map(|p| p.clone())
            .unwrap_or_else(|_| Arc::new(PricingTable::baked()))
    }

    pub fn set_pricing(&self, pricing: Arc<PricingTable>) {
        if let Ok(mut slot) = self.pricing.write() {
            *slot = pricing;
        }
    }

    pub fn inflight(&self) -> Vec<InflightRequest> {
        self.inflight
            .lock()
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default()
    }

    pub fn inflight_count(&self) -> usize {
        self.inflight.lock().map(|m| m.len()).unwrap_or(0)
    }

    pub(crate) fn inflight_insert(&self, request: InflightRequest) {
        if let Ok(mut m) = self.inflight.lock() {
            m.insert(request.id, request);
        }
    }

    pub(crate) fn inflight_remove(&self, id: Uuid) {
        if let Ok(mut m) = self.inflight.lock() {
            m.remove(&id);
        }
    }
}

/// What the handler gets per request.
#[derive(Clone)]
pub struct ProxyState {
    pub listener: Arc<ListenerConfig>,
    pub ctx: Arc<ProxyContext>,
}

/// Claims a listening socket. Called before the runtime exists so a taken port stops startup.
pub fn bind(addr: SocketAddr) -> Result<std::net::TcpListener> {
    let listener =
        std::net::TcpListener::bind(addr).with_context(|| format!("bind proxy listener {addr}"))?;
    listener
        .set_nonblocking(true)
        .context("set proxy listener non-blocking")?;
    Ok(listener)
}

/// Serves until `shutdown` flips, then waits for open connections (the caller bounds the wait).
pub async fn serve(
    listener: std::net::TcpListener,
    config: Arc<ListenerConfig>,
    ctx: Arc<ProxyContext>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let listener = TcpListener::from_std(listener).context("register proxy listener")?;
    let addr = listener.local_addr().context("proxy listener address")?;
    info!(backend = %config.backend, listen = %addr, upstream = %config.upstream, "proxy listener ready");
    let app = Router::new()
        .fallback(any(crate::pipeline::handle))
        .with_state(ProxyState {
            listener: config,
            ctx,
        });
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        let _ = shutdown.changed().await;
    })
    .await
    .context("proxy serve")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_backend_config_yields_one_listener_per_address_and_none_when_disabled() {
        let mut cfg = BackendConfig::defaults_for(Backend::Ollama);
        cfg.listen = vec![
            "0.0.0.0:11434".parse().unwrap(),
            "0.0.0.0:11435".parse().unwrap(),
        ];
        cfg.upstream = "http://127.0.0.1:11400/".into();
        let listeners = ListenerConfig::from_backend(Backend::Ollama, &cfg);
        assert_eq!(listeners.len(), 2);
        assert_eq!(
            listeners[0].upstream, "http://127.0.0.1:11400",
            "trailing slash trimmed"
        );
        assert_eq!(listeners[1].listen.port(), 11435);
        cfg.enabled = false;
        assert!(ListenerConfig::from_backend(Backend::Ollama, &cfg).is_empty());
    }

    #[test]
    fn bind_rejects_taken_ports() {
        let first = bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let taken = first.local_addr().unwrap();
        let err = bind(taken).unwrap_err();
        assert!(
            format!("{err:#}").contains(&format!("bind proxy listener {taken}")),
            "{err:#}"
        );
    }
}
