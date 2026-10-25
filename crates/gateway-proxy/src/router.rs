//! Which API format a request speaks, from its backend, method and path. Exact segment matches after
//! trimming a trailing slash (gin redirects `/api/chat/` to `/api/chat`); Ollama-Monitor's `contains`
//! matching also caught unrelated paths. Gemini's own paths carry the model and the method in the path
//! (`/v1beta/models/{model}:streamGenerateContent`), and its OpenAI-compatible surface under
//! `/v1beta/openai/` is the `/v1` family under another prefix. Anything unknown is `Passthrough`:
//! forwarded, never parsed.

use axum::http::{HeaderMap, Method, header};
use gateway_core::types::{ApiFormat, Backend};

/// A Gemini method call: `/v1beta/models/{model}:{method}` (or under `/v1/`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeminiCall {
    pub model: String,
    pub method: String,
}

pub fn gemini_call(path: &str) -> Option<GeminiCall> {
    let path = path.trim_end_matches('/');
    let rest = path
        .strip_prefix("/v1beta/models/")
        .or_else(|| path.strip_prefix("/v1/models/"))?;
    let (model, method) = rest.rsplit_once(':')?;
    (!model.is_empty() && !model.contains('/') && is_gemini_method(method)).then(|| GeminiCall {
        model: model.to_string(),
        method: method.to_string(),
    })
}

/// Gemini's RPC methods are camelCase verbs (`generateContent`, `countTokens`). Ollama's model tags use a
/// colon too (`gemma4:12b-mlx`, `qwen3:latest`), so a tag with digits, hyphens or no capital is a tag.
fn is_gemini_method(method: &str) -> bool {
    const KNOWN: &[&str] = &[
        "generateContent",
        "streamGenerateContent",
        "countTokens",
        "embedContent",
        "batchEmbedContents",
        "predict",
        "predictLongRunning",
        "generateAnswer",
    ];
    KNOWN.contains(&method)
        || (!method.is_empty()
            && method.chars().all(|c| c.is_ascii_alphabetic())
            && method.starts_with(|c: char| c.is_ascii_lowercase())
            && method.chars().any(|c| c.is_ascii_uppercase()))
}

/// Gemini's OpenAI-compatible paths, spelled as the `/v1` family they are.
fn gemini_compat_path(path: &str) -> Option<String> {
    path.strip_prefix("/v1beta/openai/")
        .map(|rest| format!("/v1/{rest}"))
}

pub fn classify(backend: Backend, method: &Method, path: &str) -> ApiFormat {
    if method != Method::POST {
        return ApiFormat::Passthrough;
    }
    let path = path.trim_end_matches('/');
    let compat = (backend == Backend::Gemini)
        .then(|| gemini_compat_path(path))
        .flatten();
    let path = compat.as_deref().unwrap_or(path);
    match path {
        "/v1/chat/completions" => ApiFormat::OpenAiChat,
        "/v1/completions" => ApiFormat::OpenAiCompletions,
        "/v1/embeddings" => ApiFormat::OpenAiEmbeddings,
        "/v1/responses" => ApiFormat::OpenAiResponses,
        "/v1/messages" => ApiFormat::AnthropicMessages,
        _ => match backend {
            Backend::Ollama => match path {
                "/api/chat" | "/api/generate" => ApiFormat::OllamaNative,
                "/api/embed" | "/api/embeddings" => ApiFormat::OllamaEmbeddings,
                _ => ApiFormat::Passthrough,
            },
            Backend::LmStudio => match path {
                "/api/v0/chat/completions" | "/api/v0/completions" => ApiFormat::LmStudioNative,
                "/api/v0/embeddings" => ApiFormat::LmStudioEmbeddings,
                _ => ApiFormat::Passthrough,
            },
            Backend::Gemini => match gemini_call(path) {
                Some(call) => match call.method.as_str() {
                    "generateContent" | "streamGenerateContent" => ApiFormat::GeminiGenerate,
                    "embedContent" | "batchEmbedContents" => ApiFormat::GeminiEmbed,
                    // countTokens and the rest carry no generation; they are scanned as cloud passthrough.
                    _ => ApiFormat::Passthrough,
                },
                None => ApiFormat::Passthrough,
            },
            // Anthropic and OpenAI serve only the `/v1` family above.
            Backend::Anthropic | Backend::OpenAi => ApiFormat::Passthrough,
        },
    }
}

/// An HTTP upgrade request (WebSocket or otherwise): `Connection: upgrade` plus an `Upgrade` header.
pub fn is_upgrade(headers: &HeaderMap) -> bool {
    let wants_upgrade = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|token| token.trim().eq_ignore_ascii_case("upgrade"));
    wants_upgrade && headers.contains_key(header::UPGRADE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inference_paths_classify_per_backend_and_everything_else_passes_through() {
        let post = Method::POST;
        for backend in Backend::ALL {
            assert_eq!(
                classify(backend, &post, "/v1/chat/completions"),
                ApiFormat::OpenAiChat
            );
            assert_eq!(
                classify(backend, &post, "/v1/chat/completions/"),
                ApiFormat::OpenAiChat,
                "trailing slash"
            );
            assert_eq!(
                classify(backend, &post, "/v1/completions"),
                ApiFormat::OpenAiCompletions
            );
            assert_eq!(
                classify(backend, &post, "/v1/embeddings"),
                ApiFormat::OpenAiEmbeddings
            );
            assert_eq!(
                classify(backend, &post, "/v1/responses"),
                ApiFormat::OpenAiResponses
            );
            assert_eq!(
                classify(backend, &post, "/v1/messages"),
                ApiFormat::AnthropicMessages
            );
            assert_eq!(
                classify(backend, &post, "/v1/models"),
                ApiFormat::Passthrough
            );
            assert_eq!(
                classify(backend, &post, "/v1/chat/completions/extra"),
                ApiFormat::Passthrough
            );
            assert_eq!(
                classify(backend, &Method::GET, "/v1/chat/completions"),
                ApiFormat::Passthrough,
                "GET is never inference"
            );
        }
        assert_eq!(
            classify(Backend::Ollama, &post, "/api/chat"),
            ApiFormat::OllamaNative
        );
        assert_eq!(
            classify(Backend::Ollama, &post, "/api/generate"),
            ApiFormat::OllamaNative
        );
        assert_eq!(
            classify(Backend::Ollama, &post, "/api/embed"),
            ApiFormat::OllamaEmbeddings
        );
        assert_eq!(
            classify(Backend::Ollama, &post, "/api/embeddings"),
            ApiFormat::OllamaEmbeddings
        );
        assert_eq!(
            classify(Backend::Ollama, &post, "/api/pull"),
            ApiFormat::Passthrough
        );
        assert_eq!(
            classify(Backend::Ollama, &post, "/api/v0/chat/completions"),
            ApiFormat::Passthrough,
            "LM Studio paths are not Ollama's"
        );
        assert_eq!(
            classify(Backend::LmStudio, &post, "/api/v0/chat/completions"),
            ApiFormat::LmStudioNative
        );
        assert_eq!(
            classify(Backend::LmStudio, &post, "/api/v0/completions"),
            ApiFormat::LmStudioNative
        );
        assert_eq!(
            classify(Backend::LmStudio, &post, "/api/v0/embeddings"),
            ApiFormat::LmStudioEmbeddings
        );
        assert_eq!(
            classify(Backend::LmStudio, &post, "/api/chat"),
            ApiFormat::Passthrough
        );
    }

    #[test]
    fn gemini_paths_classify_by_their_method_and_compat_paths_as_the_v1_family() {
        let post = Method::POST;
        assert_eq!(
            gemini_call("/v1beta/models/gemini-3.1-pro-preview:streamGenerateContent"),
            Some(GeminiCall {
                model: "gemini-3.1-pro-preview".into(),
                method: "streamGenerateContent".into()
            })
        );
        assert_eq!(
            gemini_call("/v1/models/gemini-3-flash:generateContent/").map(|c| c.method),
            Some("generateContent".into())
        );
        assert_eq!(
            gemini_call("/v1/models/gpt-5"),
            None,
            "no method, no Gemini call"
        );
        assert_eq!(gemini_call("/v1beta/models/:generateContent"), None);
        assert_eq!(gemini_call("/v1beta/models/a/b:generateContent"), None);
        assert_eq!(
            gemini_call("/v1/models/gemma4:12b-mlx"),
            None,
            "an Ollama tag is not a method"
        );
        assert_eq!(gemini_call("/v1/models/qwen3:latest"), None);
        assert_eq!(gemini_call("/v1/models/qwen3:14b"), None);
        assert_eq!(
            gemini_call("/v1beta/models/gemini-3-flash:predict").map(|c| c.method),
            Some("predict".into())
        );
        assert_eq!(
            gemini_call("/v1beta/models/gemini-3-flash:someFutureMethod").map(|c| c.method),
            Some("someFutureMethod".into())
        );
        for (path, expected) in [
            (
                "/v1beta/models/gemini-3.1-pro-preview:streamGenerateContent",
                ApiFormat::GeminiGenerate,
            ),
            (
                "/v1beta/models/gemini-3-flash:generateContent",
                ApiFormat::GeminiGenerate,
            ),
            (
                "/v1beta/models/gemini-embedding-2:embedContent",
                ApiFormat::GeminiEmbed,
            ),
            (
                "/v1beta/models/gemini-embedding-2:batchEmbedContents",
                ApiFormat::GeminiEmbed,
            ),
            (
                "/v1beta/models/gemini-3-flash:countTokens",
                ApiFormat::Passthrough,
            ),
            ("/v1beta/cachedContents", ApiFormat::Passthrough),
            ("/v1beta/openai/chat/completions", ApiFormat::OpenAiChat),
            ("/v1beta/openai/embeddings", ApiFormat::OpenAiEmbeddings),
            ("/v1/chat/completions", ApiFormat::OpenAiChat),
        ] {
            assert_eq!(classify(Backend::Gemini, &post, path), expected, "{path}");
        }
        assert_eq!(
            classify(
                Backend::Gemini,
                &Method::GET,
                "/v1beta/models/gemini-3-flash:generateContent"
            ),
            ApiFormat::Passthrough
        );
        // The other backends do not speak Gemini's dialect.
        assert_eq!(
            classify(
                Backend::LmStudio,
                &post,
                "/v1beta/models/gemini-3-flash:generateContent"
            ),
            ApiFormat::Passthrough
        );
        assert_eq!(
            classify(Backend::Anthropic, &post, "/v1beta/openai/chat/completions"),
            ApiFormat::Passthrough
        );
        assert_eq!(
            classify(Backend::OpenAi, &post, "/v1/responses"),
            ApiFormat::OpenAiResponses
        );
    }

    #[test]
    fn upgrades_need_both_headers() {
        let mut headers = HeaderMap::new();
        assert!(!is_upgrade(&headers));
        headers.insert(header::CONNECTION, "keep-alive, Upgrade".parse().unwrap());
        assert!(!is_upgrade(&headers), "no Upgrade header yet");
        headers.insert(header::UPGRADE, "websocket".parse().unwrap());
        assert!(is_upgrade(&headers));
        headers.insert(header::CONNECTION, "keep-alive".parse().unwrap());
        assert!(!is_upgrade(&headers));
    }
}
