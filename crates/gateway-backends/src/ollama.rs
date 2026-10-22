//! Ollama's model endpoints. `GET /api/version` decides reachability, then `/api/tags` (every installed
//! model, `:cloud` ones included, with `remote_host`) and `/api/ps` (resident models with their allocated
//! context, size and expiry) are fetched together and merged into one list ordered loaded → cloud → not
//! loaded. Model names are canonicalised so a record's `deepseek-v4-pro` matches the `deepseek-v4-pro:cloud`
//! row and `qwen3` matches `qwen3:latest`. Ported from Ollama-Monitor's `api.rs`.

use std::time::Duration;

use chrono::{DateTime, Utc};
use gateway_core::types::{Backend, BackendSnapshot, ModelInfo, ModelState};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tracing::debug;

/// `/api/tags` entry. Everything optional so older Ollama builds still parse.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct TagModel {
    pub name: String,
    pub model: String,
    pub remote_host: Option<String>,
    pub size: Option<u64>,
    pub details: Option<ModelDetails>,
    pub capabilities: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ModelDetails {
    pub format: Option<String>,
    pub family: Option<String>,
    pub parameter_size: Option<String>,
    pub quantization_level: Option<String>,
    /// Present in `/api/tags` details on Ollama 0.3x+: the model's maximum context.
    pub context_length: Option<u64>,
}

/// `/api/ps` entry: a model resident in memory.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct LoadedModel {
    pub name: String,
    pub model: String,
    pub size: Option<u64>,
    pub size_vram: Option<u64>,
    pub expires_at: Option<String>,
    pub details: Option<ModelDetails>,
    /// The allocated (`num_ctx`), not maximum, context; top-level here unlike `/api/tags`.
    pub context_length: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct TagsResp {
    pub models: Vec<TagModel>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct PsResp {
    pub models: Vec<LoadedModel>,
}

#[derive(Debug, Deserialize)]
struct VersionResp {
    version: String,
}

pub fn build_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(1))
        .timeout(Duration::from_secs(2))
        .build()
}

/// One poll: version, then tags and ps concurrently, merged. `previous_models` is kept when unreachable.
pub async fn snapshot(
    client: &reqwest::Client,
    base_url: &str,
    previous_models: &[ModelInfo],
    now: DateTime<Utc>,
) -> BackendSnapshot {
    let base_url = base_url.trim_end_matches('/');
    let unreachable = |error: String| BackendSnapshot {
        backend: Backend::Ollama,
        reachable: false,
        version: None,
        error: Some(error),
        models: previous_models.to_vec(),
        polled_at: now,
    };
    let version = match client.get(format!("{base_url}/api/version")).send().await {
        Ok(r) if r.status().is_success() => r.json::<VersionResp>().await.ok().map(|v| v.version),
        Ok(r) => return unreachable(format!("/api/version → HTTP {}", r.status())),
        Err(err) => return unreachable(crate::describe("/api/version", &err)),
    };
    let (tags, ps) = tokio::join!(
        get_json::<TagsResp>(client, base_url, "/api/tags"),
        get_json::<PsResp>(client, base_url, "/api/ps"),
    );
    match (tags, ps) {
        (Err(tags_err), Err(ps_err)) => {
            let mut snap = unreachable(format!("{tags_err}; {ps_err}"));
            snap.version = version;
            snap
        }
        (tags, ps) => {
            if let Err(err) = &tags {
                debug!(error = %err, "/api/tags failed; showing resident models only");
            }
            if let Err(err) = &ps {
                debug!(error = %err, "/api/ps failed; residency unknown");
            }
            BackendSnapshot {
                backend: Backend::Ollama,
                reachable: true,
                version,
                error: None,
                models: merge_models(
                    tags.map(|t| t.models).unwrap_or_default(),
                    ps.map(|p| p.models).unwrap_or_default(),
                ),
                polled_at: now,
            }
        }
    }
}

async fn get_json<T: DeserializeOwned>(
    client: &reqwest::Client,
    base_url: &str,
    path: &str,
) -> Result<T, String> {
    let resp = client
        .get(format!("{base_url}{path}"))
        .send()
        .await
        .map_err(|err| crate::describe(path, &err))?;
    if !resp.status().is_success() {
        return Err(format!("{path} → HTTP {}", resp.status()));
    }
    resp.json::<T>()
        .await
        .map_err(|err| format!("{path} decode: {}", crate::root_cause(&err)))
}

/// Installed models (`/api/tags`) merged with resident ones (`/api/ps`), ordered loaded → cloud → not
/// loaded, stable within each group.
pub fn merge_models(tags: Vec<TagModel>, ps: Vec<LoadedModel>) -> Vec<ModelInfo> {
    let mut ps_matched = vec![false; ps.len()];
    let mut rows = Vec::with_capacity(tags.len() + ps.len());

    for tag in &tags {
        let Some(id) = non_empty(Some(&tag.name)).or_else(|| non_empty(Some(&tag.model))) else {
            continue;
        };
        let resident_idx = ps.iter().position(|p| {
            [p.name.as_str(), p.model.as_str()]
                .iter()
                .any(|n| !n.is_empty() && (*n == id || *n == tag.model))
        });
        if let Some(i) = resident_idx {
            ps_matched[i] = true;
        }
        let resident = resident_idx.map(|i| &ps[i]);
        let cloud = is_cloud(&id, tag.remote_host.as_deref());
        let details = tag.details.as_ref();
        let resident_details = resident.and_then(|p| p.details.as_ref());
        rows.push(ModelInfo {
            backend: Backend::Ollama,
            kind: kind_from_capabilities(tag.capabilities.as_deref()),
            compatibility_type: if cloud {
                Some("cloud".into())
            } else {
                non_empty(details.and_then(|d| d.format.as_ref()))
                    .or_else(|| non_empty(resident_details.and_then(|d| d.format.as_ref())))
            },
            quantization: non_empty(details.and_then(|d| d.quantization_level.as_ref())).or_else(
                || non_empty(resident_details.and_then(|d| d.quantization_level.as_ref())),
            ),
            max_context_length: details
                .and_then(|d| d.context_length)
                .or_else(|| resident.and_then(|p| p.context_length)),
            loaded_context_length: resident.and_then(|p| p.context_length),
            state: if cloud {
                ModelState::Cloud
            } else if resident.is_some() {
                ModelState::Loaded
            } else {
                ModelState::NotLoaded
            },
            publisher: None,
            arch: details.and_then(|d| d.family.clone()),
            capabilities: tag.capabilities.clone().unwrap_or_default(),
            size_bytes: tag.size.or_else(|| resident.and_then(|p| p.size)),
            size_vram_bytes: resident.and_then(|p| p.size_vram),
            expires_at: resident.and_then(|p| parse_expires(p.expires_at.as_deref())),
            remote_host: non_empty(tag.remote_host.as_ref()),
            id,
        });
    }

    // Resident but missing from tags (for instance when /api/tags failed this poll).
    for (p, _) in ps.iter().zip(&ps_matched).filter(|(_, matched)| !**matched) {
        let Some(id) = non_empty(Some(&p.name)).or_else(|| non_empty(Some(&p.model))) else {
            continue;
        };
        let cloud = is_cloud(&id, None);
        let details = p.details.as_ref();
        rows.push(ModelInfo {
            backend: Backend::Ollama,
            kind: None,
            compatibility_type: if cloud {
                Some("cloud".into())
            } else {
                non_empty(details.and_then(|d| d.format.as_ref()))
            },
            quantization: non_empty(details.and_then(|d| d.quantization_level.as_ref())),
            max_context_length: p
                .context_length
                .or_else(|| details.and_then(|d| d.context_length)),
            loaded_context_length: p.context_length,
            state: if cloud {
                ModelState::Cloud
            } else {
                ModelState::Loaded
            },
            publisher: None,
            arch: details.and_then(|d| d.family.clone()),
            capabilities: Vec::new(),
            size_bytes: p.size,
            size_vram_bytes: p.size_vram,
            expires_at: parse_expires(p.expires_at.as_deref()),
            remote_host: None,
            id,
        });
    }

    rows.sort_by_key(|m| m.state.sort_key());
    rows
}

/// Ollama writes `expires_at` as RFC 3339 with nanoseconds and a zone offset; `0001-01-01T00:00:00Z`
/// means "never" and reads as `None`.
fn parse_expires(raw: Option<&str>) -> Option<DateTime<Utc>> {
    let parsed = DateTime::parse_from_rfc3339(raw?).ok()?.with_timezone(&Utc);
    (parsed.timestamp() > 0).then_some(parsed)
}

fn non_empty(s: Option<&String>) -> Option<String> {
    s.map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn kind_from_capabilities(caps: Option<&[String]>) -> Option<String> {
    let caps = caps.filter(|c| !c.is_empty())?;
    let has = |want: &str| caps.iter().any(|c| c.eq_ignore_ascii_case(want));
    let kind = if has("embedding") {
        "embeddings"
    } else if has("vision") {
        "vlm"
    } else {
        "llm"
    };
    Some(kind.to_string())
}

/// A `remote_host`, or a `cloud` / `*-cloud` tag, marks a model that runs on ollama.com.
pub fn is_cloud(name: &str, remote_host: Option<&str>) -> bool {
    if remote_host.is_some_and(|h| !h.trim().is_empty()) {
        return true;
    }
    is_cloud_name(name)
}

/// The name alone: what the proxy can tell before the first poll.
pub fn is_cloud_name(name: &str) -> bool {
    split_tag(name).is_some_and(|(_, tag)| {
        let tag = tag.to_ascii_lowercase();
        tag == "cloud" || tag.ends_with("-cloud")
    })
}

/// Splits `name:tag` at the last `:`. A remainder containing `/` means the colon belongs to a registry
/// `host:port/...` prefix, so there is no tag.
fn split_tag(name: &str) -> Option<(&str, &str)> {
    let (base, tag) = name.rsplit_once(':')?;
    if base.is_empty() || tag.contains('/') {
        return None;
    }
    Some((base, tag))
}

/// Normalises a model name for matching records to model rows: `qwen3:latest` ≡ `qwen3`, and cloud tags
/// (`deepseek-v4-pro:cloud`, `gpt-oss:120b-cloud`) ≡ the bare names their OpenAI-compatible responses use.
pub fn canonical_model_name(name: &str) -> String {
    let name = name.trim().to_ascii_lowercase();
    if let Some((base, tag)) = split_tag(&name) {
        if matches!(tag, "" | "latest" | "cloud") {
            return base.to_string();
        }
        if let Some(stem) = tag.strip_suffix("-cloud") {
            return if stem.is_empty() {
                base.to_string()
            } else {
                format!("{base}:{stem}")
            };
        }
    }
    name
}

/// The model row a record's name refers to: an exact (case-insensitive) id match wins, otherwise the
/// first canonical match in display order.
pub fn resolve_model_id<'a>(
    models: &'a [ModelInfo],
    record_model: Option<&str>,
) -> Option<&'a str> {
    let record_model = record_model?;
    if let Some(m) = models
        .iter()
        .find(|m| m.id.eq_ignore_ascii_case(record_model))
    {
        return Some(&m.id);
    }
    let want = canonical_model_name(record_model);
    models
        .iter()
        .find(|m| canonical_model_name(&m.id) == want)
        .map(|m| m.id.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAGS_CLOUD_FIXTURE: &str = include_str!("../../../fixtures/ollama/api-tags-cloud.json");
    const PS_FIXTURE: &str = include_str!("../../../fixtures/ollama/api-ps.json");

    fn tags_fixture() -> Vec<TagModel> {
        serde_json::from_str::<TagsResp>(TAGS_CLOUD_FIXTURE)
            .unwrap()
            .models
    }

    fn ps_fixture() -> Vec<LoadedModel> {
        serde_json::from_str::<PsResp>(PS_FIXTURE).unwrap().models
    }

    fn tag(name: &str, format: &str, quant: &str, ctx: Option<u64>, caps: &[&str]) -> TagModel {
        TagModel {
            name: name.into(),
            model: name.into(),
            remote_host: None,
            size: Some(9_000_000_000),
            details: Some(ModelDetails {
                format: Some(format.into()),
                quantization_level: Some(quant.into()),
                context_length: ctx,
                ..Default::default()
            }),
            capabilities: Some(caps.iter().map(|c| c.to_string()).collect()),
        }
    }

    fn info(id: &str, state: ModelState) -> ModelInfo {
        ModelInfo {
            backend: Backend::Ollama,
            id: id.into(),
            kind: None,
            compatibility_type: None,
            quantization: None,
            max_context_length: None,
            loaded_context_length: None,
            state,
            publisher: None,
            arch: None,
            capabilities: vec![],
            size_bytes: None,
            size_vram_bytes: None,
            expires_at: None,
            remote_host: None,
        }
    }

    #[test]
    fn parse_tags_cloud_fixture() {
        let tags = tags_fixture();
        assert_eq!(tags.len(), 1);
        let m = &tags[0];
        assert_eq!(m.name, "deepseek-v4-pro:cloud");
        assert_eq!(m.remote_host.as_deref(), Some("https://ollama.com"));
        let d = m.details.as_ref().unwrap();
        assert_eq!(d.context_length, Some(1_048_576));
        assert_eq!(d.quantization_level.as_deref(), Some("FP8"));
        assert_eq!(m.capabilities.as_ref().unwrap().len(), 3);
    }

    #[test]
    fn parse_tags_minimal_older_ollama() {
        let tags: TagsResp = serde_json::from_str(r#"{"models":[{"name":"x"}]}"#).unwrap();
        assert_eq!(tags.models.len(), 1);
        assert!(tags.models[0].details.is_none());
        let empty: TagsResp = serde_json::from_str("{}").unwrap();
        assert!(empty.models.is_empty());
    }

    #[test]
    fn parse_api_ps_fixture_with_size_vram_and_expiry() {
        let ps = ps_fixture();
        assert_eq!(ps.len(), 1);
        assert_eq!(ps[0].name, "qwen3:14b");
        assert_eq!(ps[0].context_length, Some(4096));
        assert!(ps[0].size_vram.is_some(), "size_vram is read now");
        assert!(ps[0].expires_at.is_some());
        let rows = merge_models(vec![], ps_fixture());
        assert_eq!(rows[0].size_vram_bytes, ps[0].size_vram);
        assert!(rows[0].expires_at.is_some());
        assert_eq!(rows[0].loaded_context_length, Some(4096));
    }

    #[test]
    fn merge_cloud_only() {
        let rows = merge_models(tags_fixture(), vec![]);
        assert_eq!(rows.len(), 1);
        let m = &rows[0];
        assert_eq!(m.id, "deepseek-v4-pro:cloud");
        assert_eq!(m.kind.as_deref(), Some("llm"));
        assert_eq!(m.compatibility_type.as_deref(), Some("cloud"));
        assert_eq!(m.quantization.as_deref(), Some("FP8"));
        assert_eq!(m.max_context_length, Some(1_048_576));
        assert_eq!(m.state, ModelState::Cloud);
        assert!(m.is_cloud());
        assert_eq!(m.remote_host.as_deref(), Some("https://ollama.com"));
    }

    #[test]
    fn merge_local_loaded_and_embedding() {
        let tags = vec![
            tag(
                "nomic-embed-text:latest",
                "gguf",
                "F16",
                Some(2048),
                &["embedding"],
            ),
            tag(
                "qwen3:14b",
                "gguf",
                "Q4_K_M",
                None,
                &["completion", "tools"],
            ),
            tag(
                "llava:7b",
                "gguf",
                "Q4_0",
                Some(4096),
                &["completion", "vision"],
            ),
        ];
        let rows = merge_models(tags, ps_fixture());
        let ids: Vec<&str> = rows.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["qwen3:14b", "nomic-embed-text:latest", "llava:7b"]);
        let qwen = &rows[0];
        assert_eq!(qwen.state, ModelState::Loaded);
        assert_eq!(qwen.kind.as_deref(), Some("llm"));
        assert_eq!(qwen.compatibility_type.as_deref(), Some("gguf"));
        // No context_length in tags details: falls back to the allocated ctx from /api/ps.
        assert_eq!(qwen.max_context_length, Some(4096));
        assert_eq!(
            qwen.capabilities,
            vec!["completion".to_string(), "tools".to_string()]
        );
        assert_eq!(rows[1].state, ModelState::NotLoaded);
        assert_eq!(rows[1].kind.as_deref(), Some("embeddings"));
        assert_eq!(rows[2].kind.as_deref(), Some("vlm"));
    }

    #[test]
    fn merge_sort_is_stable_within_groups() {
        let mut cloud = tag("b:cloud", "", "FP8", None, &["completion"]);
        cloud.remote_host = Some("https://ollama.com".into());
        let tags = vec![
            tag("a", "gguf", "Q4", None, &[]),
            cloud,
            tag("c", "gguf", "Q4", None, &[]),
            tag("qwen3:14b", "gguf", "Q4_K_M", None, &[]),
        ];
        let rows = merge_models(tags, ps_fixture());
        let ids: Vec<&str> = rows.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["qwen3:14b", "b:cloud", "a", "c"]);
        assert_eq!(rows[2].kind, None, "empty capabilities give no kind");
    }

    #[test]
    fn merge_ps_only_is_appended_as_loaded_and_cloud_in_ps_stays_cloud() {
        let rows = merge_models(vec![], ps_fixture());
        assert_eq!(rows[0].id, "qwen3:14b");
        assert_eq!(rows[0].state, ModelState::Loaded);
        assert_eq!(rows[0].compatibility_type.as_deref(), Some("gguf"));
        let ps = vec![LoadedModel {
            name: "deepseek-v4-pro:cloud".into(),
            model: "deepseek-v4-pro:cloud".into(),
            ..Default::default()
        }];
        let rows = merge_models(tags_fixture(), ps);
        assert_eq!(rows.len(), 1, "the ps entry merges into the tags row");
        assert_eq!(rows[0].state, ModelState::Cloud);
    }

    #[test]
    fn cloud_detected_from_tag_without_remote_host() {
        let rows = merge_models(vec![tag("gpt-oss:120b-cloud", "", "", None, &[])], vec![]);
        assert_eq!(rows[0].state, ModelState::Cloud);
        assert_eq!(rows[0].quantization, None, "empty strings count as missing");
        assert!(is_cloud_name("deepseek-v4-pro:cloud"));
        assert!(is_cloud_name("gpt-oss:120b-cloud"));
        assert!(!is_cloud_name("qwen3:14b"));
        assert!(!is_cloud_name("registry:5000/ns/cloud"));
    }

    #[test]
    fn expiry_parses_and_never_reads_as_none() {
        assert!(parse_expires(Some("2026-05-05T11:00:00Z")).is_some());
        assert!(parse_expires(Some("2026-05-05T11:00:00.123456789-07:00")).is_some());
        assert_eq!(parse_expires(Some("0001-01-01T00:00:00Z")), None);
        assert_eq!(parse_expires(Some("garbage")), None);
        assert_eq!(parse_expires(None), None);
    }

    #[test]
    fn canonical_model_name_table() {
        for (input, want) in [
            ("deepseek-v4-pro:cloud", "deepseek-v4-pro"),
            ("deepseek-v4-pro", "deepseek-v4-pro"),
            ("gpt-oss:120b-cloud", "gpt-oss:120b"),
            ("qwen3:latest", "qwen3"),
            ("Qwen3:14B", "qwen3:14b"),
            ("  qwen3:14b ", "qwen3:14b"),
            ("qwen3:", "qwen3"),
            (
                "registry.local:5000/ns/model",
                "registry.local:5000/ns/model",
            ),
            (
                "registry.local:5000/ns/model:latest",
                "registry.local:5000/ns/model",
            ),
        ] {
            assert_eq!(canonical_model_name(input), want, "input {input:?}");
        }
    }

    #[test]
    fn resolve_prefers_exact_then_canonical() {
        let models = vec![
            info("gpt-oss:120b-cloud", ModelState::Cloud),
            info("gpt-oss:120b", ModelState::Loaded),
        ];
        assert_eq!(
            resolve_model_id(&models, Some("gpt-oss:120b")),
            Some("gpt-oss:120b")
        );
        assert_eq!(
            resolve_model_id(&models, Some("GPT-OSS:120B")),
            Some("gpt-oss:120b")
        );
        let models = vec![
            info("deepseek-v4-pro:cloud", ModelState::Cloud),
            info("qwen3:latest", ModelState::NotLoaded),
        ];
        assert_eq!(
            resolve_model_id(&models, Some("deepseek-v4-pro")),
            Some("deepseek-v4-pro:cloud")
        );
        assert_eq!(
            resolve_model_id(&models, Some("qwen3")),
            Some("qwen3:latest")
        );
        assert_eq!(resolve_model_id(&models, Some("llama3")), None);
        assert_eq!(resolve_model_id(&models, None), None);
    }

    #[tokio::test]
    async fn an_unreachable_server_is_reported_with_its_cause_and_keeps_models() {
        let client = build_client().unwrap();
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", closed.local_addr().unwrap());
        drop(closed);
        let previous = merge_models(tags_fixture(), vec![]);
        let snap = snapshot(&client, &url, &previous, Utc::now()).await;
        assert!(!snap.reachable);
        assert_eq!(snap.models.len(), 1);
        assert!(
            snap.error.as_deref().unwrap().starts_with("/api/version: "),
            "{:?}",
            snap.error
        );
    }

    /// Read-only GETs against the local Ollama: `cargo test -p gateway-backends live_ollama -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore]
    async fn live_ollama_snapshot() {
        let client = build_client().unwrap();
        let snap = snapshot(&client, "http://127.0.0.1:11434", &[], Utc::now()).await;
        println!(
            "reachable={} version={:?} error={:?}",
            snap.reachable, snap.version, snap.error
        );
        for m in &snap.models {
            println!(
                "{:<40} {:<10} {:?} {:?} ctx={:?}",
                m.id,
                m.state.as_str(),
                m.kind,
                m.quantization,
                m.max_context_length
            );
        }
    }
}
