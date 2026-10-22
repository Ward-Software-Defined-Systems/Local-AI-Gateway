//! Shared live state: the latest snapshot per backend (reachability, version, models). The pollers write
//! it, the proxy reads it to tell cloud models from local ones and to resolve model ids, and the dashboard
//! serves it. A `tokio::sync::RwLock` so readers on the request path never block each other.

use std::collections::BTreeMap;

use chrono::Utc;
use tokio::sync::RwLock;

use crate::types::{Backend, BackendSnapshot, ModelInfo};

#[derive(Debug, Default)]
pub struct BackendsState {
    inner: RwLock<BTreeMap<Backend, BackendSnapshot>>,
}

impl BackendsState {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn set(&self, snapshot: BackendSnapshot) {
        self.inner.write().await.insert(snapshot.backend, snapshot);
    }

    pub async fn get(&self, backend: Backend) -> Option<BackendSnapshot> {
        self.inner.read().await.get(&backend).cloned()
    }

    pub async fn all(&self) -> Vec<BackendSnapshot> {
        self.inner.read().await.values().cloned().collect()
    }

    pub async fn models(&self, backend: Backend) -> Vec<ModelInfo> {
        self.inner
            .read()
            .await
            .get(&backend)
            .map(|s| s.models.clone())
            .unwrap_or_default()
    }

    /// Whether `model` is known to run on the vendor's cloud according to the latest snapshot.
    pub async fn is_cloud_model(&self, backend: Backend, model: &str) -> bool {
        let guard = self.inner.read().await;
        guard.get(&backend).is_some_and(|s| {
            s.models
                .iter()
                .any(|m| m.is_cloud() && m.id.eq_ignore_ascii_case(model))
        })
    }

    /// A placeholder snapshot for a backend that has not been polled yet.
    pub fn unknown(backend: Backend) -> BackendSnapshot {
        BackendSnapshot {
            backend,
            reachable: false,
            version: None,
            error: None,
            models: Vec::new(),
            polled_at: Utc::now(),
        }
    }
}
