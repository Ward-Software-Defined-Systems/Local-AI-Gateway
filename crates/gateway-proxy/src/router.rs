//! Which API format a request speaks, from its backend, method and path. Exact segment matches after
//! trimming a trailing slash (gin redirects `/api/chat/` to `/api/chat`); Ollama-Monitor's `contains`
//! matching also caught unrelated paths. Anything unknown is `Passthrough`: forwarded, never parsed.

use axum::http::{HeaderMap, Method, header};
use gateway_core::types::{ApiFormat, Backend};

pub fn classify(backend: Backend, method: &Method, path: &str) -> ApiFormat {
    if method != Method::POST {
        return ApiFormat::Passthrough;
    }
    let path = path.trim_end_matches('/');
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
