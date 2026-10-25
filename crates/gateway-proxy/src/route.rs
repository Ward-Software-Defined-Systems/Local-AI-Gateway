//! What routing needs from a live request, as pure functions: the explicit `/anthropic/…` prefixes, the
//! provider-specific path families, the header hints for model-less requests, the live model lists, the
//! format/target compatibility matrix, Gemini's OpenAI-compatible path, the cross-provider credential guard
//! and the merged model list. The configured part (default backend, aliases, routes) is
//! `gateway_core::routing`; the unified listener wires both together per request.

use axum::http::HeaderMap;
use gateway_core::routing::{self, ModelRef, RoutedBy, RoutingTable};
use gateway_core::state::BackendsState;
use gateway_core::types::{ApiFormat, Backend, ModelInfo};
use serde_json::{Value, json};

/// The latest model lists of the two local servers.
#[derive(Debug, Clone, Default)]
pub struct LocalModels {
    pub lmstudio: Vec<ModelInfo>,
    pub ollama: Vec<ModelInfo>,
}

impl LocalModels {
    pub async fn fetch(state: &BackendsState) -> Self {
        Self {
            lmstudio: state.models(Backend::LmStudio).await,
            ollama: state.models(Backend::Ollama).await,
        }
    }
}

/// Where a model name sends a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRoute {
    pub backend: Backend,
    /// The model the forwarded request must name instead: an alias target or a stripped `provider/`.
    pub rewrite: Option<String>,
    pub routed_by: RoutedBy,
    /// One of Ollama's `:cloud` models, which leaves the machine through Ollama.
    pub ollama_cloud: bool,
}

/// `(backend, Ollama cloud)` when a local server lists `id`: an exact id (case-insensitive) on either
/// list, then Ollama's canonical spelling (`model` for `model:latest`). LM Studio's `publisher/model` ids
/// are matched here before any `provider/` reading, which is why the live lists beat that spelling.
pub fn locate_model(local: &LocalModels, id: &str) -> Option<(Backend, bool)> {
    let exact = |models: &[ModelInfo]| {
        models
            .iter()
            .find(|m| m.id.eq_ignore_ascii_case(id))
            .map(ModelInfo::is_cloud)
    };
    if let Some(cloud) = exact(&local.lmstudio) {
        return Some((Backend::LmStudio, cloud));
    }
    if let Some(cloud) = exact(&local.ollama) {
        return Some((Backend::Ollama, cloud));
    }
    let canonical = gateway_backends::ollama::resolve_model_id(&local.ollama, Some(id))?;
    let cloud = local
        .ollama
        .iter()
        .find(|m| m.id == canonical)
        .is_some_and(ModelInfo::is_cloud);
    Some((Backend::Ollama, cloud))
}

fn is_ollama_cloud(local: &LocalModels, model: &str) -> bool {
    gateway_backends::ollama::is_cloud_name(model)
        || local
            .ollama
            .iter()
            .any(|m| m.is_cloud() && m.id.eq_ignore_ascii_case(model))
}

/// The resolution order: alias, the live model lists, a `provider/model` spelling, the configured routes,
/// the built-in provider patterns. `None` leaves the caller to the header hint and the default backend.
pub fn resolve_model(table: &RoutingTable, local: &LocalModels, model: &str) -> Option<ModelRoute> {
    if let Some(target) = table.alias(model) {
        let (backend, ollama_cloud) = match target.backend {
            Some(backend) => (
                backend,
                backend == Backend::Ollama && is_ollama_cloud(local, &target.model),
            ),
            None => resolve_plain(table, local, &target.model)
                .map(|r| (r.backend, r.ollama_cloud))
                .unwrap_or((table.default_backend, false)),
        };
        return Some(ModelRoute {
            backend,
            rewrite: Some(target.model.clone()),
            routed_by: RoutedBy::Alias,
            ollama_cloud,
        });
    }
    resolve_plain(table, local, model)
}

fn resolve_plain(table: &RoutingTable, local: &LocalModels, model: &str) -> Option<ModelRoute> {
    if let Some((backend, ollama_cloud)) = locate_model(local, model) {
        return Some(ModelRoute {
            backend,
            rewrite: None,
            routed_by: RoutedBy::ModelList,
            ollama_cloud,
        });
    }
    if let Ok(ModelRef {
        backend: Some(backend),
        model: rest,
    }) = ModelRef::parse(model)
    {
        let ollama_cloud = backend == Backend::Ollama && is_ollama_cloud(local, &rest);
        return Some(ModelRoute {
            backend,
            rewrite: Some(rest),
            routed_by: RoutedBy::ProviderPrefix,
            ollama_cloud,
        });
    }
    if let Some(backend) = table.route(model) {
        return Some(ModelRoute {
            backend,
            rewrite: None,
            routed_by: RoutedBy::Route,
            ollama_cloud: false,
        });
    }
    if let Some(backend) = routing::builtin_pattern(model) {
        return Some(ModelRoute {
            backend,
            rewrite: None,
            routed_by: RoutedBy::ModelPattern,
            ollama_cloud: false,
        });
    }
    if gateway_backends::ollama::is_cloud_name(model) {
        return Some(ModelRoute {
            backend: Backend::Ollama,
            rewrite: None,
            routed_by: RoutedBy::ModelPattern,
            ollama_cloud: true,
        });
    }
    None
}

/// `/anthropic/v1/messages` → `(Anthropic, "/v1/messages")`. Only the five backend ids are prefixes.
pub fn strip_provider_prefix(path: &str) -> Option<(Backend, String)> {
    let rest = path.strip_prefix('/')?;
    let (head, tail) = match rest.split_once('/') {
        Some((head, tail)) => (head, format!("/{tail}")),
        None => (rest, "/".to_string()),
    };
    let backend = routing::provider_id(head)?;
    Some((backend, tail))
}

/// A path only one backend serves: LM Studio's `/api/v0/*`, Ollama's other `/api/*`, Gemini's `/v1beta/*`
/// and its `/v1/models/{model}:{method}` calls.
pub fn path_family(path: &str) -> Option<Backend> {
    let under = |prefix: &str| path == prefix || path.starts_with(&format!("{prefix}/"));
    if under("/api/v0") {
        Some(Backend::LmStudio)
    } else if under("/api") {
        Some(Backend::Ollama)
    } else if under("/v1beta") || crate::router::gemini_call(path).is_some() {
        Some(Backend::Gemini)
    } else {
        None
    }
}

/// The provider a model-less request belongs to, from the headers only its clients send.
pub fn header_hint(headers: &HeaderMap) -> Option<Backend> {
    if headers.contains_key("anthropic-version") || headers.contains_key("x-api-key") {
        Some(Backend::Anthropic)
    } else if headers.contains_key("x-goog-api-key") {
        Some(Backend::Gemini)
    } else if headers.contains_key("openai-organization")
        || headers.contains_key("openai-project")
        || headers.contains_key("openai-beta")
    {
        Some(Backend::OpenAi)
    } else {
        None
    }
}

/// The header that binds a request to a cloud provider other than `backend`: a Gemini key going to
/// Anthropic would be a leak, so the request is refused instead. Local targets accept every header, because
/// Anthropic-format clients send `anthropic-version` to LM Studio and Ollama legitimately.
pub fn credential_conflict(headers: &HeaderMap, backend: Backend) -> Option<&'static str> {
    if backend.is_local() {
        return None;
    }
    const BOUND: &[(&str, Backend)] = &[
        ("x-goog-api-key", Backend::Gemini),
        ("anthropic-version", Backend::Anthropic),
        ("openai-organization", Backend::OpenAi),
        ("openai-project", Backend::OpenAi),
    ];
    BOUND
        .iter()
        .find(|(name, owner)| *owner != backend && headers.contains_key(*name))
        .map(|(name, _)| *name)
}

/// Whether `backend` serves `format`. The local servers answer for themselves (a path they lack is their
/// 404); the cloud providers only take what they document, so a mismatch is refused before it leaves.
pub fn compatible(format: ApiFormat, backend: Backend) -> bool {
    use ApiFormat::*;
    match backend {
        Backend::LmStudio | Backend::Ollama => true,
        Backend::Anthropic => matches!(
            format,
            AnthropicMessages | OpenAiChat | Passthrough | CloudPassthrough
        ),
        Backend::Gemini => matches!(
            format,
            GeminiGenerate
                | GeminiEmbed
                | OpenAiChat
                | OpenAiEmbeddings
                | Passthrough
                | CloudPassthrough
        ),
        Backend::OpenAi => matches!(
            format,
            OpenAiChat
                | OpenAiCompletions
                | OpenAiEmbeddings
                | OpenAiResponses
                | Passthrough
                | CloudPassthrough
        ),
    }
}

/// The path sent upstream: OpenAI-format requests to Gemini go to its `/v1beta/openai/` surface.
pub fn forward_path(format: ApiFormat, backend: Backend, path: &str) -> String {
    use ApiFormat::*;
    if backend == Backend::Gemini
        && matches!(
            format,
            OpenAiChat | OpenAiCompletions | OpenAiEmbeddings | OpenAiResponses
        )
        && let Some(rest) = path.strip_prefix("/v1/")
    {
        return format!("/v1beta/openai/{rest}");
    }
    path.to_string()
}

/// The unified listener's answer to `GET /v1/models` when nothing names a provider: every model the local
/// servers list plus the aliases, in the OpenAI list shape with `owned_by` naming the backend.
pub fn merged_models_json(local: &LocalModels, table: &RoutingTable) -> Value {
    let mut data: Vec<Value> = Vec::new();
    for (backend, models) in [
        (Backend::LmStudio, &local.lmstudio),
        (Backend::Ollama, &local.ollama),
    ] {
        for model in models {
            data.push(json!({
                "id": model.id,
                "object": "model",
                "created": 0,
                "owned_by": backend.as_str(),
            }));
        }
    }
    for (alias, target) in &table.aliases {
        let spelled = match target.backend {
            Some(backend) => format!("{}/{}", backend.as_str(), target.model),
            None => target.model.clone(),
        };
        data.push(json!({
            "id": alias,
            "object": "model",
            "created": 0,
            "owned_by": "alias",
            "target": spelled,
        }));
    }
    json!({"object": "list", "data": data})
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use gateway_core::config::{RouteRule, UnifiedConfig};
    use gateway_core::types::ModelState;

    use super::*;

    fn model(backend: Backend, id: &str, cloud: bool) -> ModelInfo {
        ModelInfo {
            backend,
            id: id.to_string(),
            kind: Some("llm".into()),
            compatibility_type: None,
            quantization: None,
            max_context_length: None,
            loaded_context_length: None,
            state: if cloud {
                ModelState::Cloud
            } else {
                ModelState::NotLoaded
            },
            publisher: None,
            arch: None,
            capabilities: Vec::new(),
            size_bytes: None,
            size_vram_bytes: None,
            expires_at: None,
            remote_host: cloud.then(|| "ollama.com".to_string()),
        }
    }

    fn local() -> LocalModels {
        LocalModels {
            lmstudio: vec![
                model(Backend::LmStudio, "qwen/qwen3.6-27b", false),
                model(Backend::LmStudio, "openai/gpt-oss-20b", false),
            ],
            ollama: vec![
                model(Backend::Ollama, "gemma4:12b-mlx", false),
                model(Backend::Ollama, "deepseek-v4-pro:cloud", true),
                model(Backend::Ollama, "qwen3:latest", false),
            ],
        }
    }

    fn table() -> RoutingTable {
        RoutingTable::from_config(&UnifiedConfig {
            aliases: BTreeMap::from([
                ("fast".to_string(), "ollama/gemma4:12b-mlx".to_string()),
                ("smart".to_string(), "claude-opus-5-5".to_string()),
                ("far".to_string(), "deepseek-v4-pro:cloud".to_string()),
                ("odd".to_string(), "mystery-model".to_string()),
            ]),
            routes: vec![RouteRule {
                model_prefix: "mistral-".into(),
                backend: Backend::Ollama,
            }],
            ..UnifiedConfig::default()
        })
    }

    fn route(model: &str) -> ModelRoute {
        resolve_model(&table(), &local(), model).unwrap_or_else(|| panic!("{model} resolves"))
    }

    #[test]
    fn models_resolve_in_order_alias_list_prefix_route_pattern() {
        let fast = route("FAST");
        assert_eq!(
            (fast.backend, fast.rewrite.as_deref(), fast.routed_by),
            (Backend::Ollama, Some("gemma4:12b-mlx"), RoutedBy::Alias)
        );
        let smart = route("smart");
        assert_eq!(
            smart.backend,
            Backend::Anthropic,
            "a bare alias target resolves on"
        );
        assert_eq!(smart.rewrite.as_deref(), Some("claude-opus-5-5"));
        assert!(
            route("far").ollama_cloud,
            "an alias to an Ollama cloud model is egress"
        );
        assert_eq!(
            route("odd").backend,
            Backend::LmStudio,
            "an unresolvable alias target takes the default"
        );

        let listed = route("qwen/qwen3.6-27b");
        assert_eq!(
            (listed.backend, listed.routed_by),
            (Backend::LmStudio, RoutedBy::ModelList)
        );
        assert_eq!(listed.rewrite, None);
        let oss = route("openai/gpt-oss-20b");
        assert_eq!(
            oss.backend,
            Backend::LmStudio,
            "the live list beats the provider spelling"
        );
        let canonical = route("qwen3");
        assert_eq!(
            (canonical.backend, canonical.routed_by),
            (Backend::Ollama, RoutedBy::ModelList),
            "`qwen3` is `qwen3:latest` to Ollama"
        );
        let canonical_cloud = route("deepseek-v4-pro");
        assert!(
            canonical_cloud.ollama_cloud,
            "the tagless spelling of a cloud model is still egress"
        );
        let cloud = route("DeepSeek-V4-Pro:cloud");
        assert!(cloud.ollama_cloud);
        assert_eq!(cloud.backend, Backend::Ollama);

        let spelled = route("openai/gpt-oss-120b");
        assert_eq!(
            (
                spelled.backend,
                spelled.rewrite.as_deref(),
                spelled.routed_by
            ),
            (
                Backend::OpenAi,
                Some("gpt-oss-120b"),
                RoutedBy::ProviderPrefix
            )
        );
        let routed = route("mistral-large-3");
        assert_eq!(
            (routed.backend, routed.routed_by),
            (Backend::Ollama, RoutedBy::Route)
        );
        let pattern = route("gemini-3.1-pro-preview");
        assert_eq!(
            (pattern.backend, pattern.routed_by),
            (Backend::Gemini, RoutedBy::ModelPattern)
        );
        let unlisted_cloud = route("kimi-k3:cloud");
        assert_eq!(
            (unlisted_cloud.backend, unlisted_cloud.ollama_cloud),
            (Backend::Ollama, true)
        );
        assert_eq!(resolve_model(&table(), &local(), "llama-9"), None);
        assert_eq!(resolve_model(&table(), &local(), ""), None);
    }

    #[test]
    fn prefixes_and_path_families_name_a_backend() {
        assert_eq!(
            strip_provider_prefix("/anthropic/v1/messages"),
            Some((Backend::Anthropic, "/v1/messages".to_string()))
        );
        assert_eq!(
            strip_provider_prefix("/Gemini/v1beta/models"),
            Some((Backend::Gemini, "/v1beta/models".to_string()))
        );
        assert_eq!(
            strip_provider_prefix("/ollama"),
            Some((Backend::Ollama, "/".to_string()))
        );
        assert_eq!(strip_provider_prefix("/anthropicx/v1/messages"), None);
        assert_eq!(strip_provider_prefix("/v1/messages"), None);
        assert_eq!(strip_provider_prefix(""), None);

        assert_eq!(path_family("/api/v0/models"), Some(Backend::LmStudio));
        assert_eq!(path_family("/api/v0"), Some(Backend::LmStudio));
        assert_eq!(path_family("/api/ps"), Some(Backend::Ollama));
        assert_eq!(path_family("/api/chat"), Some(Backend::Ollama));
        assert_eq!(path_family("/v1beta/models"), Some(Backend::Gemini));
        assert_eq!(path_family("/v1beta/cachedContents"), Some(Backend::Gemini));
        assert_eq!(
            path_family("/v1/models/gemini-3-flash:generateContent"),
            Some(Backend::Gemini)
        );
        assert_eq!(path_family("/v1/models"), None);
        assert_eq!(path_family("/v1/chat/completions"), None);
        assert_eq!(path_family("/apiary"), None);
    }

    #[test]
    fn header_hints_and_credential_conflicts_follow_the_provider_headers() {
        let mut headers = HeaderMap::new();
        assert_eq!(header_hint(&headers), None);
        headers.insert("authorization", "Bearer x".parse().unwrap());
        assert_eq!(header_hint(&headers), None, "a bearer token names nobody");
        headers.insert("x-goog-api-key", "g".parse().unwrap());
        assert_eq!(header_hint(&headers), Some(Backend::Gemini));
        headers.insert("anthropic-version", "2023-06-01".parse().unwrap());
        assert_eq!(
            header_hint(&headers),
            Some(Backend::Anthropic),
            "Anthropic's headers rank first"
        );

        assert_eq!(
            credential_conflict(&headers, Backend::Gemini),
            Some("anthropic-version")
        );
        assert_eq!(
            credential_conflict(&headers, Backend::Anthropic),
            Some("x-goog-api-key")
        );
        assert!(credential_conflict(&headers, Backend::OpenAi).is_some());
        assert_eq!(
            credential_conflict(&headers, Backend::LmStudio),
            None,
            "local targets take anything"
        );
        let mut openai_only = HeaderMap::new();
        openai_only.insert("openai-project", "proj".parse().unwrap());
        assert_eq!(header_hint(&openai_only), Some(Backend::OpenAi));
        assert_eq!(credential_conflict(&openai_only, Backend::OpenAi), None);
        assert_eq!(
            credential_conflict(&openai_only, Backend::Anthropic),
            Some("openai-project")
        );
        let bearer_only = {
            let mut h = HeaderMap::new();
            h.insert("authorization", "Bearer x".parse().unwrap());
            h
        };
        assert_eq!(credential_conflict(&bearer_only, Backend::Gemini), None);
    }

    #[test]
    fn the_compatibility_matrix_and_gemini_compat_paths() {
        use ApiFormat::*;
        for backend in Backend::LOCAL {
            for format in ApiFormat::ALL {
                assert!(compatible(format, backend), "{format} on {backend}");
            }
        }
        assert!(compatible(OpenAiChat, Backend::Anthropic));
        assert!(compatible(AnthropicMessages, Backend::Anthropic));
        assert!(!compatible(OpenAiEmbeddings, Backend::Anthropic));
        assert!(!compatible(OpenAiResponses, Backend::Anthropic));
        assert!(!compatible(GeminiGenerate, Backend::Anthropic));
        assert!(compatible(GeminiGenerate, Backend::Gemini));
        assert!(compatible(OpenAiEmbeddings, Backend::Gemini));
        assert!(!compatible(AnthropicMessages, Backend::Gemini));
        assert!(!compatible(OpenAiResponses, Backend::Gemini));
        assert!(compatible(OpenAiResponses, Backend::OpenAi));
        assert!(compatible(OpenAiCompletions, Backend::OpenAi));
        assert!(!compatible(AnthropicMessages, Backend::OpenAi));
        assert!(!compatible(OllamaNative, Backend::OpenAi));
        for backend in Backend::CLOUD_PROVIDERS {
            assert!(compatible(Passthrough, backend));
            assert!(compatible(CloudPassthrough, backend));
        }

        assert_eq!(
            forward_path(OpenAiChat, Backend::Gemini, "/v1/chat/completions"),
            "/v1beta/openai/chat/completions"
        );
        assert_eq!(
            forward_path(OpenAiEmbeddings, Backend::Gemini, "/v1/embeddings"),
            "/v1beta/openai/embeddings"
        );
        assert_eq!(
            forward_path(
                OpenAiChat,
                Backend::Gemini,
                "/v1beta/openai/chat/completions"
            ),
            "/v1beta/openai/chat/completions",
            "already Gemini's own spelling"
        );
        assert_eq!(
            forward_path(
                GeminiGenerate,
                Backend::Gemini,
                "/v1beta/models/x:generateContent"
            ),
            "/v1beta/models/x:generateContent"
        );
        assert_eq!(
            forward_path(OpenAiChat, Backend::Anthropic, "/v1/chat/completions"),
            "/v1/chat/completions"
        );
    }

    #[test]
    fn the_merged_model_list_names_each_models_backend_and_the_aliases() {
        let value = merged_models_json(&local(), &table());
        assert_eq!(value["object"], "list");
        let data = value["data"].as_array().unwrap();
        assert_eq!(data.len(), 5 + 4);
        let find = |id: &str| data.iter().find(|m| m["id"] == id).cloned().unwrap();
        assert_eq!(find("qwen/qwen3.6-27b")["owned_by"], "lmstudio");
        assert_eq!(find("deepseek-v4-pro:cloud")["owned_by"], "ollama");
        assert_eq!(find("fast")["owned_by"], "alias");
        assert_eq!(find("fast")["target"], "ollama/gemma4:12b-mlx");
        assert_eq!(find("smart")["target"], "claude-opus-5-5");
        assert!(data.iter().all(|m| m["object"] == "model"));
    }
}
