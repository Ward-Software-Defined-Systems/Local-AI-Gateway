//! LM Studio's native model list: `GET {base}/api/v0/models` returns every downloaded model with `type`
//! (`llm`, `vlm`, `embeddings`), `state` (`loaded` / `not-loaded`), quantization and context lengths.
//! `loaded_context_length` and `capabilities` appear only on loaded models, so everything is optional.
//! Reachability is the poll succeeding; LM Studio has no version endpoint (see `version.rs`).

use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use gateway_core::types::{Backend, BackendSnapshot, ModelInfo, ModelState};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct LmsModel {
    pub id: String,
    #[serde(default)]
    pub object: String,
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub publisher: Option<String>,
    pub arch: Option<String>,
    pub compatibility_type: Option<String>,
    pub quantization: Option<String>,
    pub state: String,
    pub max_context_length: Option<u64>,
    pub loaded_context_length: Option<u64>,
    pub capabilities: Option<Vec<String>>,
}

impl LmsModel {
    pub fn is_loaded(&self) -> bool {
        self.state == "loaded"
    }

    pub fn to_model_info(&self) -> ModelInfo {
        ModelInfo {
            backend: Backend::LmStudio,
            id: self.id.clone(),
            kind: self.kind.clone(),
            compatibility_type: self.compatibility_type.clone(),
            quantization: self.quantization.clone(),
            max_context_length: self.max_context_length,
            loaded_context_length: self.loaded_context_length,
            state: if self.is_loaded() {
                ModelState::Loaded
            } else {
                ModelState::NotLoaded
            },
            publisher: self.publisher.clone(),
            arch: self.arch.clone(),
            capabilities: self.capabilities.clone().unwrap_or_default(),
            size_bytes: None,
            size_vram_bytes: None,
            expires_at: None,
            remote_host: None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct ModelsResponse {
    data: Vec<LmsModel>,
}

/// Short timeouts: a poll that hangs would otherwise stall the status line for a whole interval.
pub fn build_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(1))
        .timeout(Duration::from_secs(3))
        .build()
}

pub async fn list_models(client: &reqwest::Client, base_url: &str) -> Result<Vec<LmsModel>> {
    let url = format!("{}/api/v0/models", base_url.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!(crate::describe("/api/v0/models", &e)))?
        .error_for_status()
        .map_err(|e| {
            anyhow::anyhow!(
                "/api/v0/models → HTTP {}",
                e.status().map(|s| s.as_u16()).unwrap_or(0)
            )
        })?;
    let body: ModelsResponse = resp.json().await.context("/api/v0/models decode")?;
    Ok(body.data)
}

/// Loaded models first, then the rest, in the order LM Studio gave them.
pub fn to_model_infos(models: &[LmsModel]) -> Vec<ModelInfo> {
    let mut infos: Vec<ModelInfo> = models.iter().map(LmsModel::to_model_info).collect();
    infos.sort_by_key(|m| m.state.sort_key());
    infos
}

/// One poll. `version` is whatever `version.rs` found; `previous_models` is kept when unreachable.
pub async fn snapshot(
    client: &reqwest::Client,
    base_url: &str,
    version: Option<String>,
    previous_models: &[ModelInfo],
    now: DateTime<Utc>,
) -> BackendSnapshot {
    match list_models(client, base_url).await {
        Ok(models) => BackendSnapshot {
            backend: Backend::LmStudio,
            reachable: true,
            version,
            error: None,
            models: to_model_infos(&models),
            polled_at: now,
        },
        Err(err) => BackendSnapshot {
            backend: Backend::LmStudio,
            reachable: false,
            version,
            error: Some(format!("{err:#}")),
            models: previous_models.to_vec(),
            polled_at: now,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOADED_FIXTURE: &str = include_str!("../../../fixtures/lmstudio/api-models-loaded.json");
    const EMPTY_FIXTURE: &str = include_str!("../../../fixtures/lmstudio/api-models-empty.json");

    #[test]
    fn parses_loaded_fixture() {
        let resp: ModelsResponse =
            serde_json::from_str(LOADED_FIXTURE).expect("loaded fixture parses");
        assert_eq!(resp.data.len(), 2);
        let mlx = &resp.data[0];
        assert_eq!(mlx.id, "qwen3.6-35b-a3b-ud-mlx");
        assert!(mlx.is_loaded());
        assert_eq!(mlx.kind.as_deref(), Some("vlm"));
        assert_eq!(mlx.compatibility_type.as_deref(), Some("mlx"));
        assert_eq!(mlx.quantization.as_deref(), Some("4bit"));
        assert_eq!(mlx.max_context_length, Some(262144));
        assert_eq!(mlx.loaded_context_length, Some(262144));
        assert_eq!(
            mlx.capabilities.as_deref().unwrap(),
            &["tool_use".to_string()]
        );
        let embed = &resp.data[1];
        assert_eq!(embed.id, "text-embedding-nomic-embed-text-v1.5");
        assert!(!embed.is_loaded());
        assert_eq!(embed.kind.as_deref(), Some("embeddings"));
        assert_eq!(embed.loaded_context_length, None);
        assert_eq!(embed.capabilities, None);
    }

    #[test]
    fn parses_empty_fixture() {
        let resp: ModelsResponse =
            serde_json::from_str(EMPTY_FIXTURE).expect("empty fixture parses");
        assert!(resp.data.is_empty());
    }

    #[test]
    fn model_infos_carry_the_backend_and_sort_loaded_first() {
        let resp: ModelsResponse = serde_json::from_str(LOADED_FIXTURE).unwrap();
        let mut reversed = resp.data.clone();
        reversed.reverse();
        let infos = to_model_infos(&reversed);
        assert_eq!(infos[0].state, ModelState::Loaded);
        assert_eq!(infos[0].backend, Backend::LmStudio);
        assert_eq!(infos[0].capabilities, vec!["tool_use".to_string()]);
        assert_eq!(infos[1].state, ModelState::NotLoaded);
        assert!(infos[1].capabilities.is_empty());
        assert!(!infos[0].is_cloud());
    }

    #[tokio::test]
    async fn an_unreachable_server_keeps_the_previous_models() {
        let client = build_client().unwrap();
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", closed.local_addr().unwrap());
        drop(closed);
        let previous = to_model_infos(
            &serde_json::from_str::<ModelsResponse>(LOADED_FIXTURE)
                .unwrap()
                .data,
        );
        let snap = snapshot(&client, &url, Some("0.4.25".into()), &previous, Utc::now()).await;
        assert!(!snap.reachable);
        assert_eq!(snap.models.len(), 2, "last known list stays");
        assert!(
            snap.error.as_deref().unwrap().starts_with("/api/v0/models"),
            "{:?}",
            snap.error
        );
        assert_eq!(snap.version.as_deref(), Some("0.4.25"));
    }
}
