//! One axum server per configured listen address, over plain HTTP or, with the certificate the gateway's CA
//! issued, HTTPS. A fixed listener sends every request to one backend.
//! `bind` claims the socket before the runtime exists so a taken port stops startup; `serve` runs the
//! pipeline as a catch-all handler with graceful shutdown, and stamps every response it sends with
//! `x-local-ai-gateway: <version>` so a client can tell it is talking to the gateway. `ProxyContext` is
//! what every listener shares:
//! the upstream HTTP clients (one follows redirects for the local servers, one refuses them for the cloud
//! providers so a credential never follows a redirect to another host), the store and event bus, the DLP
//! scanner, the capture policy, pricing, the routing table, the backend snapshots and the in-flight table.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::{Context, Result};
use axum::http::HeaderValue;
use axum::response::Response;
use axum::routing::any;
use axum::{Router, middleware};
use axum_server::tls_rustls::RustlsConfig;
use gateway_core::config::{BackendConfig, BackendsConfig, CaptureConfig, UnifiedConfig};
use gateway_core::events::EventSender;
use gateway_core::pricing::PricingTable;
use gateway_core::routing::RoutingTable;
use gateway_core::state::BackendsState;
use gateway_core::store::StoreHandle;
use gateway_core::types::{Backend, InflightRequest};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_util::task::TaskTracker;
use tracing::info;
use uuid::Uuid;

use crate::scan::{NoopScanner, RequestScanner};

/// The response header that names the gateway, on every response a proxy listener sends.
pub const GATEWAY_HEADER: &str = "x-local-ai-gateway";

/// Its value: the gateway's version and nothing else. The crates share the workspace version, so this is
/// the binary's `CARGO_PKG_VERSION`; a test in the binary pins the equality.
pub const GATEWAY_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Per-target behaviour, from the backend's `[backends.<name>]` table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetFlags {
    pub capture_bodies: bool,
    pub record_passthrough: bool,
    pub rewrite_include_usage: bool,
}

/// One backend as a listener reaches it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendUpstream {
    pub backend: Backend,
    /// `http://host:port` or `https://host`, no trailing slash.
    pub upstream: String,
    pub flags: TargetFlags,
}

impl BackendUpstream {
    pub fn from_config(backend: Backend, cfg: &BackendConfig) -> Self {
        Self {
            backend,
            upstream: cfg.upstream.trim_end_matches('/').to_string(),
            flags: TargetFlags {
                capture_bodies: cfg.capture_bodies,
                record_passthrough: cfg.record_passthrough,
                rewrite_include_usage: cfg.rewrite_include_usage,
            },
        }
    }
}

/// How a listener picks the backend for a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenerMode {
    /// Every request goes to this one backend.
    Fixed(BackendUpstream),
    /// The target is chosen per request from the enabled backends: an explicit prefix, a provider-specific
    /// path, the model, a provider header, or the configured default (`crate::route`).
    Unified {
        targets: BTreeMap<Backend, BackendUpstream>,
    },
}

#[derive(Debug, Clone)]
pub struct ListenerConfig {
    pub listen: SocketAddr,
    pub mode: ListenerMode,
    /// HTTPS with the certificate the gateway's CA issued (`tls` under `[backends.<name>]` or `[unified]`,
    /// resolved against the address).
    pub tls: bool,
}

impl ListenerConfig {
    /// One fixed listener per configured address of an enabled backend.
    pub fn from_backend(backend: Backend, cfg: &BackendConfig) -> Vec<ListenerConfig> {
        if !cfg.enabled {
            return Vec::new();
        }
        cfg.listen
            .iter()
            .map(|listen| ListenerConfig {
                listen: *listen,
                mode: ListenerMode::Fixed(BackendUpstream::from_config(backend, cfg)),
                tls: cfg.tls.enabled_for(*listen),
            })
            .collect()
    }

    /// One unified listener per configured address, over every enabled backend.
    pub fn unified(cfg: &UnifiedConfig, backends: &BackendsConfig) -> Vec<ListenerConfig> {
        if !cfg.enabled {
            return Vec::new();
        }
        let targets: BTreeMap<Backend, BackendUpstream> = backends
            .enabled()
            .map(|(backend, b)| (backend, BackendUpstream::from_config(backend, b)))
            .collect();
        cfg.listen
            .iter()
            .map(|listen| ListenerConfig {
                listen: *listen,
                mode: ListenerMode::Unified {
                    targets: targets.clone(),
                },
                tls: cfg.tls.enabled_for(*listen),
            })
            .collect()
    }

    /// The one backend a fixed listener serves; `None` for the unified listener.
    pub fn fixed(&self) -> Option<&BackendUpstream> {
        match &self.mode {
            ListenerMode::Fixed(target) => Some(target),
            ListenerMode::Unified { .. } => None,
        }
    }

    /// The target for `backend` on this listener, if it can reach it.
    pub fn target(&self, backend: Backend) -> Option<&BackendUpstream> {
        match &self.mode {
            ListenerMode::Fixed(target) => (target.backend == backend).then_some(target),
            ListenerMode::Unified { targets } => targets.get(&backend),
        }
    }

    /// Every backend this listener can send a request to.
    pub fn backends(&self) -> Vec<Backend> {
        match &self.mode {
            ListenerMode::Fixed(target) => vec![target.backend],
            ListenerMode::Unified { targets } => targets.keys().copied().collect(),
        }
    }

    /// A line for the startup log.
    pub fn summary(&self) -> String {
        match &self.mode {
            ListenerMode::Fixed(target) => format!("{} → {}", target.backend, target.upstream),
            ListenerMode::Unified { targets } => format!(
                "unified → {}",
                targets
                    .keys()
                    .map(|b| b.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
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
    /// For the local servers: follows redirects (Ollama answers `/api/chat/` with a 307).
    pub client: reqwest::Client,
    /// For the cloud providers: a redirect is passed to the client instead of being followed, because
    /// reqwest would carry `x-api-key` and `x-goog-api-key` to the new host.
    pub cloud_client: reqwest::Client,
    pub store: Option<StoreHandle>,
    pub events: EventSender,
    scanner: RwLock<Arc<dyn RequestScanner>>,
    capture: RwLock<CapturePolicy>,
    pricing: RwLock<Arc<PricingTable>>,
    routing: RwLock<Arc<RoutingTable>>,
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
        let builder = || {
            reqwest::Client::builder()
                .pool_idle_timeout(Duration::from_secs(90))
                .connect_timeout(Duration::from_secs(5))
                .tcp_nodelay(true)
        };
        let client = builder().build().context("build proxy http client")?;
        let cloud_client = builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("build cloud http client")?;
        Ok(Arc::new(Self {
            client,
            cloud_client,
            store,
            events,
            scanner: RwLock::new(Arc::new(NoopScanner)),
            capture: RwLock::new(capture),
            pricing: RwLock::new(pricing),
            routing: RwLock::new(Arc::new(RoutingTable::default())),
            session_id,
            backends,
            inflight: Mutex::new(BTreeMap::new()),
            tunnels: TaskTracker::new(),
        }))
    }

    /// The HTTP client for a backend: redirects are followed for the local servers only.
    pub fn client_for(&self, backend: Backend) -> &reqwest::Client {
        if backend.is_cloud_provider() {
            &self.cloud_client
        } else {
            &self.client
        }
    }

    pub fn routing(&self) -> Arc<RoutingTable> {
        self.routing
            .read()
            .map(|r| r.clone())
            .unwrap_or_else(|_| Arc::new(RoutingTable::default()))
    }

    pub fn set_routing(&self, table: Arc<RoutingTable>) {
        if let Ok(mut slot) = self.routing.write() {
            *slot = table;
        }
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

/// Puts `x-local-ai-gateway: <version>` on a response head. It runs after the handler, so it covers proxied
/// streams (the body bytes stand), gateway-generated answers and refusals, and the 101 of a tunnel; a
/// header of that name from an upstream is replaced, never passed off as the gateway's.
async fn stamp_gateway_header(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(GATEWAY_HEADER, HeaderValue::from_static(GATEWAY_VERSION));
    response
}

/// Serves until `shutdown` flips, then waits for open connections (the caller bounds the wait). With
/// `tls`, the socket speaks HTTPS through axum-server's rustls acceptor, which serves hyper with upgrades,
/// so the WebSocket tunnel works over TLS as well.
pub async fn serve(
    listener: std::net::TcpListener,
    config: Arc<ListenerConfig>,
    ctx: Arc<ProxyContext>,
    mut shutdown: watch::Receiver<bool>,
    tls: Option<RustlsConfig>,
) -> Result<()> {
    let addr = listener.local_addr().context("proxy listener address")?;
    info!(listen = %addr, target = %config.summary(), tls = tls.is_some(), "proxy listener ready");
    let app = Router::new()
        .fallback(any(crate::pipeline::handle))
        .layer(middleware::map_response(stamp_gateway_header))
        .with_state(ProxyState {
            listener: config,
            ctx,
        })
        .into_make_service_with_connect_info::<SocketAddr>();
    match tls {
        Some(tls) => {
            let handle = axum_server::Handle::new();
            let stopper = handle.clone();
            tokio::spawn(async move {
                let _ = shutdown.changed().await;
                stopper.graceful_shutdown(None);
            });
            axum_server::from_tcp_rustls(listener, tls)
                .handle(handle)
                .serve(app)
                .await
                .context("proxy serve (tls)")
        }
        None => {
            let listener = TcpListener::from_std(listener).context("register proxy listener")?;
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown.changed().await;
                })
                .await
                .context("proxy serve")
        }
    }
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
        let target = listeners[0].fixed().expect("a fixed listener");
        assert_eq!(
            target.upstream, "http://127.0.0.1:11400",
            "trailing slash trimmed"
        );
        assert_eq!(target.backend, Backend::Ollama);
        assert!(target.flags.capture_bodies);
        assert_eq!(listeners[1].listen.port(), 11435);
        assert_eq!(listeners[1].backends(), vec![Backend::Ollama]);
        assert_eq!(listeners[1].summary(), "ollama → http://127.0.0.1:11400");
        assert!(!listeners[0].tls, "plain HTTP unless asked");
        cfg.tls = gateway_core::config::TlsMode::On;
        assert!(
            ListenerConfig::from_backend(Backend::Ollama, &cfg)
                .iter()
                .all(|l| l.tls)
        );
        cfg.enabled = false;
        assert!(ListenerConfig::from_backend(Backend::Ollama, &cfg).is_empty());
    }

    #[test]
    fn the_unified_listener_covers_every_enabled_backend() {
        let config = gateway_core::config::Config::parse(
            "[backends.openai]\nenabled = false\n[unified]\nlisten = [\"127.0.0.1:4000\", \"127.0.0.1:4001\"]\n",
        )
        .unwrap();
        let listeners = ListenerConfig::unified(&config.unified, &config.backends);
        assert_eq!(listeners.len(), 2);
        assert!(listeners[0].fixed().is_none());
        assert_eq!(
            listeners[0].backends(),
            vec![
                Backend::LmStudio,
                Backend::Ollama,
                Backend::Anthropic,
                Backend::Gemini
            ]
        );
        assert_eq!(
            listeners[0]
                .target(Backend::Gemini)
                .map(|t| t.upstream.as_str()),
            Some("https://generativelanguage.googleapis.com")
        );
        assert!(
            listeners[0].target(Backend::OpenAi).is_none(),
            "disabled backends are not targets"
        );
        assert_eq!(listeners[1].listen.port(), 4001);
        assert_eq!(
            listeners[1].summary(),
            "unified → lmstudio, ollama, anthropic, gemini"
        );
        let config = gateway_core::config::Config::parse("[unified]\nenabled = false\n").unwrap();
        assert!(ListenerConfig::unified(&config.unified, &config.backends).is_empty());
    }

    #[test]
    fn the_cloud_client_does_not_follow_redirects_and_the_local_one_does() {
        let (events, _rx) = gateway_core::events::channel();
        let ctx = ProxyContext::new(
            None,
            events,
            1,
            Arc::new(PricingTable::baked()),
            Arc::new(BackendsState::new()),
            CapturePolicy {
                max_body_bytes: 1,
                max_buffered_request_bytes: 1,
            },
        )
        .unwrap();
        assert!(std::ptr::eq(ctx.client_for(Backend::Ollama), &ctx.client));
        assert!(std::ptr::eq(
            ctx.client_for(Backend::Anthropic),
            &ctx.cloud_client
        ));
        assert_eq!(ctx.routing().default_backend, Backend::LmStudio);
        ctx.set_routing(Arc::new(RoutingTable {
            default_backend: Backend::Ollama,
            ..RoutingTable::default()
        }));
        assert_eq!(ctx.routing().default_backend, Backend::Ollama);
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
