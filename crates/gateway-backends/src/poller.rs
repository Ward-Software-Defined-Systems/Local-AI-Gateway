//! One poller per enabled backend. Each tick fetches a snapshot, keeps the last known models when the
//! backend is unreachable, stores the model list when it changes, and publishes a `BackendSnapshot` event
//! when anything changed (or every 30 s as a heartbeat). Reachability transitions are logged; the alert
//! dispatcher decides whether they are worth a notification.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use gateway_core::events::{EventSender, GatewayEvent, publish};
use gateway_core::state::BackendsState;
use gateway_core::store::StoreHandle;
use gateway_core::types::{Backend, BackendSnapshot};
use tokio::sync::watch;
use tracing::{info, warn};

use crate::version::lmstudio_version;

const HEARTBEAT: Duration = Duration::from_secs(30);
const VERSION_REFRESH: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone)]
pub struct PollerConfig {
    pub backend: Backend,
    pub upstream: String,
    pub interval: Duration,
    pub lms_app_path: Option<PathBuf>,
    pub lms_bin: Option<PathBuf>,
}

/// Runs until `shutdown` flips. Never returns early on errors: an unreachable backend is a state to show,
/// not a reason to stop polling.
pub async fn run(
    cfg: PollerConfig,
    state: Arc<BackendsState>,
    events: EventSender,
    store: Option<StoreHandle>,
    mut shutdown: watch::Receiver<bool>,
) {
    let client = match cfg.backend {
        Backend::LmStudio => crate::lmstudio::build_client(),
        Backend::Ollama => crate::ollama::build_client(),
    };
    let client = match client {
        Ok(c) => c,
        Err(err) => {
            warn!(backend = %cfg.backend, error = %err, "could not build the polling HTTP client; poller disabled");
            return;
        }
    };
    let mut ticker = tokio::time::interval(cfg.interval.max(Duration::from_secs(1)));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut previous: Option<BackendSnapshot> = None;
    let mut last_published = tokio::time::Instant::now() - HEARTBEAT;
    let mut lms_version: Option<String> = None;
    let mut lms_version_checked: Option<tokio::time::Instant> = None;

    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            _ = ticker.tick() => {}
        }
        let previous_models = previous
            .as_ref()
            .map(|p| p.models.as_slice())
            .unwrap_or(&[]);
        let snapshot = match cfg.backend {
            Backend::LmStudio => {
                if lms_version_checked.is_none_or(|t| t.elapsed() >= VERSION_REFRESH) {
                    lms_version =
                        lmstudio_version(cfg.lms_app_path.as_deref(), cfg.lms_bin.as_deref()).await;
                    lms_version_checked = Some(tokio::time::Instant::now());
                }
                crate::lmstudio::snapshot(
                    &client,
                    &cfg.upstream,
                    lms_version.clone(),
                    previous_models,
                    Utc::now(),
                )
                .await
            }
            Backend::Ollama => {
                crate::ollama::snapshot(&client, &cfg.upstream, previous_models, Utc::now()).await
            }
        };

        let changed = previous.as_ref().is_none_or(|p| {
            p.reachable != snapshot.reachable
                || p.version != snapshot.version
                || p.models != snapshot.models
                || p.error != snapshot.error
        });
        if let Some(p) = &previous
            && p.reachable != snapshot.reachable
        {
            if snapshot.reachable {
                info!(backend = %cfg.backend, version = ?snapshot.version, "backend reachable again");
            } else {
                warn!(backend = %cfg.backend, error = ?snapshot.error, "backend unreachable");
            }
        } else if previous.is_none() {
            info!(backend = %cfg.backend, reachable = snapshot.reachable, version = ?snapshot.version, models = snapshot.models.len(), "first poll");
        }
        let models_changed = previous
            .as_ref()
            .is_none_or(|p| p.models != snapshot.models);
        if models_changed
            && snapshot.reachable
            && !snapshot.models.is_empty()
            && let Some(store) = &store
            && let Err(err) = store.upsert_models_seen(snapshot.models.clone()).await
        {
            warn!(error = %format!("{err:#}"), "could not record models seen");
        }
        state.set(snapshot.clone()).await;
        if changed || last_published.elapsed() >= HEARTBEAT {
            publish(
                &events,
                GatewayEvent::BackendSnapshot {
                    snapshot: snapshot.clone(),
                },
            );
            last_published = tokio::time::Instant::now();
        }
        previous = Some(snapshot);
    }
    info!(backend = %cfg.backend, "poller stopped");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A closed port: the poller publishes an unreachable snapshot on its first tick and keeps running.
    #[tokio::test]
    async fn the_first_poll_is_published_even_when_unreachable() {
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let upstream = format!("http://{}", closed.local_addr().unwrap());
        drop(closed);
        let state = Arc::new(BackendsState::new());
        let (events, mut rx) = gateway_core::events::channel();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(run(
            PollerConfig {
                backend: Backend::Ollama,
                upstream,
                interval: Duration::from_secs(1),
                lms_app_path: None,
                lms_bin: None,
            },
            state.clone(),
            events,
            None,
            shutdown_rx,
        ));
        let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("an event")
            .unwrap();
        match &*event {
            GatewayEvent::BackendSnapshot { snapshot } => {
                assert_eq!(snapshot.backend, Backend::Ollama);
                assert!(!snapshot.reachable);
                assert!(snapshot.error.is_some());
            }
            other => panic!("unexpected event {other:?}"),
        }
        assert!(state.get(Backend::Ollama).await.is_some());
        let _ = shutdown_tx.send(true);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("poller stops")
            .unwrap();
    }
}
