//! HTTP plumbing shared by the pipeline: hop-by-hop headers, capped body reads, the one request rewrite the
//! gateway makes (`stream_options.include_usage` on streaming OpenAI requests), the request's `model` and
//! `stream` fields, header redaction for storage, and error bodies shaped for each API family.

use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use gateway_core::types::ApiFormat;
use serde_json::{Map, Value, json};

use crate::error::ProxyError;

/// Longest `model` value kept from a request.
pub const MAX_MODEL_NAME: usize = 256;

pub fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailers"
            | "transfer-encoding"
            | "upgrade"
    )
}

/// Header names tokenised in a `Connection` header are hop-by-hop too (RFC 7230 §6.1).
pub fn connection_tokens(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .collect()
}

/// The headers to forward to the other side: everything but hop-by-hop ones, `host`, and (when `strip_length`)
/// `content-length`, which the sender sets for a buffered body.
pub fn forwardable(headers: &HeaderMap, strip_length: bool) -> HeaderMap {
    let tokens = connection_tokens(headers);
    let mut out = HeaderMap::new();
    for (name, value) in headers {
        let lower = name.as_str().to_ascii_lowercase();
        if is_hop_by_hop(&lower) || lower == "host" || tokens.contains(&lower) {
            continue;
        }
        if strip_length && lower == "content-length" {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

/// Collects a body, refusing it once it passes `cap` bytes.
pub async fn read_capped(body: Body, cap: usize) -> Result<Bytes, ProxyError> {
    let mut stream = body.into_data_stream();
    let mut buf = BytesMut::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| ProxyError::BodyUnreadable)?;
        if buf.len() + chunk.len() > cap {
            return Err(ProxyError::BodyTooLarge);
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf.freeze())
}

/// What the pipeline reads out of an inference request body.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RequestFields {
    pub model: Option<String>,
    /// The request's `stream` flag, or the format's default when absent.
    pub streamed: bool,
    pub json: Option<Value>,
}

pub fn request_fields(format: ApiFormat, body: &[u8]) -> RequestFields {
    let json = serde_json::from_slice::<Value>(body).ok();
    let obj = json.as_ref().and_then(Value::as_object);
    let model = obj.and_then(request_model);
    // Ollama's native endpoints stream unless told otherwise; the OpenAI and Anthropic families do not.
    let default_stream = matches!(format, ApiFormat::OllamaNative);
    let streamed = if format.is_embeddings() {
        false
    } else {
        obj.and_then(|o| o.get("stream"))
            .and_then(Value::as_bool)
            .unwrap_or(default_stream)
    };
    RequestFields {
        model,
        streamed,
        json,
    }
}

/// The request's `model` if it is a plausible name: non-empty, at most `MAX_MODEL_NAME` bytes, no control
/// characters. It comes from the client and is shown as it is.
pub fn request_model(obj: &Map<String, Value>) -> Option<String> {
    let model = obj.get("model")?.as_str()?;
    plausible_model(model).then(|| model.to_string())
}

fn plausible_model(model: &str) -> bool {
    !model.is_empty() && model.len() <= MAX_MODEL_NAME && !model.chars().any(char::is_control)
}

/// `request_fields` with the request path at hand: Gemini names the model and the streaming choice in
/// the path (`/v1beta/models/{model}:streamGenerateContent`), not in the body.
pub fn request_fields_for(format: ApiFormat, path: &str, body: &[u8]) -> RequestFields {
    match format {
        ApiFormat::GeminiGenerate | ApiFormat::GeminiEmbed => {
            let call = crate::router::gemini_call(path);
            RequestFields {
                model: call
                    .as_ref()
                    .map(|c| c.model.as_str())
                    .filter(|m| plausible_model(m))
                    .map(str::to_string),
                streamed: format == ApiFormat::GeminiGenerate
                    && call
                        .as_ref()
                        .is_some_and(|c| c.method == "streamGenerateContent"),
                json: serde_json::from_slice::<Value>(body).ok(),
            }
        }
        _ => request_fields(format, body),
    }
}

/// For streaming `/v1/chat/completions`, `/v1/completions` and LM Studio `/api/v0/chat/completions`
/// requests, sets `stream_options.include_usage =
/// true` so the final SSE frame carries token counts. Without it Ollama omits usage entirely, and that is
/// the default in most clients. The extra frame has `choices: []`, which clients ignore. Returns the body
/// unchanged when it is not a streaming JSON object; key order is not preserved on a rewrite.
pub fn ensure_include_usage(body: Bytes) -> Bytes {
    let Ok(mut value) = serde_json::from_slice::<Value>(&body) else {
        return body;
    };
    let Some(obj) = value.as_object_mut() else {
        return body;
    };
    if !obj.get("stream").and_then(Value::as_bool).unwrap_or(false) {
        return body;
    }
    let opts = obj.entry("stream_options").or_insert_with(|| json!({}));
    match opts.as_object_mut() {
        Some(opts) => {
            opts.insert("include_usage".to_string(), Value::Bool(true));
        }
        None => *opts = json!({"include_usage": true}),
    }
    match serde_json::to_vec(&value) {
        Ok(bytes) => Bytes::from(bytes),
        Err(_) => body,
    }
}

/// Query parameters that carry a credential in some provider's API: Gemini takes its key as `?key=`.
const SECRET_QUERY_KEYS: &[&str] = &[
    "key",
    "api_key",
    "apikey",
    "api-key",
    "access_token",
    "token",
    "x-goog-api-key",
];

/// A query string as stored, shown and logged: the values of credential parameters replaced. The raw
/// query still goes upstream.
pub fn redact_query(query: &str) -> String {
    query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((name, _))
                if SECRET_QUERY_KEYS.contains(&name.trim().to_ascii_lowercase().as_str()) =>
            {
                format!("{name}=[REDACTED]")
            }
            _ => pair.to_string(),
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// A URL safe to log: its query redacted.
pub fn redact_url(url: &str) -> String {
    match url.split_once('?') {
        Some((base, query)) => format!("{base}?{}", redact_query(query)),
        None => url.to_string(),
    }
}

/// The body with `model` replaced, for an alias or a `provider/model` spelling the unified listener
/// resolved: the target must see the name it knows. Anything but a JSON object is returned unchanged; key
/// order is not preserved on a rewrite.
pub fn rewrite_model(body: Bytes, model: &str) -> Bytes {
    let Ok(mut value) = serde_json::from_slice::<Value>(&body) else {
        return body;
    };
    let Some(obj) = value.as_object_mut() else {
        return body;
    };
    obj.insert("model".to_string(), Value::String(model.to_string()));
    match serde_json::to_vec(&value) {
        Ok(bytes) => Bytes::from(bytes),
        Err(_) => body,
    }
}

/// Headers as stored on a record: values of credentials replaced, everything else as sent.
pub fn redact_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            let lower = name.as_str().to_ascii_lowercase();
            let shown = if matches!(
                lower.as_str(),
                "authorization"
                    | "x-api-key"
                    | "x-goog-api-key"
                    | "cookie"
                    | "proxy-authorization"
                    | "set-cookie"
            ) {
                "[REDACTED]".to_string()
            } else {
                value
                    .to_str()
                    .map(str::to_string)
                    .unwrap_or_else(|_| "[binary]".to_string())
            };
            (lower, shown)
        })
        .collect()
}

/// An error body in the shape the client's API family expects, so SDKs surface a readable message.
pub fn error_body(
    format: ApiFormat,
    status: StatusCode,
    message: &str,
    code: &str,
) -> (HeaderValue, Bytes) {
    let json_type = HeaderValue::from_static("application/json");
    let body = match format {
        ApiFormat::OllamaNative | ApiFormat::OllamaEmbeddings => json!({"error": message}),
        ApiFormat::AnthropicMessages => json!({
            "type": "error",
            "error": {"type": anthropic_error_type(status), "message": message}
        }),
        ApiFormat::GeminiGenerate | ApiFormat::GeminiEmbed => json!({
            "error": {"code": status.as_u16(), "message": message, "status": google_status(status)}
        }),
        ApiFormat::Passthrough | ApiFormat::CloudPassthrough => {
            return (
                HeaderValue::from_static("text/plain; charset=utf-8"),
                Bytes::from(format!("{message}\n")),
            );
        }
        _ => json!({"error": {"message": message, "type": "gateway_error", "code": code}}),
    };
    (json_type, Bytes::from(body.to_string()))
}

fn anthropic_error_type(status: StatusCode) -> &'static str {
    match status.as_u16() {
        400 | 413 => "invalid_request_error",
        403 => "permission_error",
        502 | 503 => "api_error",
        _ => "api_error",
    }
}

/// Google's `status` names for the statuses the gateway itself answers with.
fn google_status(status: StatusCode) -> &'static str {
    match status.as_u16() {
        400 | 413 => "INVALID_ARGUMENT",
        403 => "PERMISSION_DENIED",
        404 => "NOT_FOUND",
        502 | 503 => "UNAVAILABLE",
        _ => "INTERNAL",
    }
}

pub fn header_name(name: &str) -> Option<HeaderName> {
    HeaderName::from_bytes(name.as_bytes()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn val(b: &Bytes) -> Value {
        serde_json::from_slice(b).unwrap()
    }

    #[test]
    fn injects_include_usage_on_streaming_requests_only() {
        let out = ensure_include_usage(Bytes::from(
            r#"{"model":"qwen3:14b","messages":[{"role":"user","content":"hi"}],"stream":true}"#,
        ));
        assert_eq!(val(&out)["stream_options"]["include_usage"], json!(true));
        let out = ensure_include_usage(Bytes::from(
            r#"{"model":"x","messages":[],"stream":true,"stream_options":{"include_usage":false,"foo":"bar"}}"#,
        ));
        assert_eq!(val(&out)["stream_options"]["include_usage"], json!(true));
        assert_eq!(val(&out)["stream_options"]["foo"], json!("bar"));
        let out = ensure_include_usage(Bytes::from(
            r#"{"model":"x","messages":[],"stream":true,"stream_options":7}"#,
        ));
        assert_eq!(val(&out)["stream_options"], json!({"include_usage": true}));
        for untouched in [
            r#"{"model":"x","messages":[],"stream":false}"#,
            r#"{"model":"x","messages":[]}"#,
            "not json {{",
            "",
            "[1,2]",
        ] {
            assert_eq!(
                ensure_include_usage(Bytes::from(untouched)),
                Bytes::from(untouched),
                "{untouched}"
            );
        }
    }

    #[test]
    fn request_fields_read_model_and_stream_with_per_format_defaults() {
        let fields = request_fields(
            ApiFormat::OpenAiChat,
            br#"{"model":"deepseek-v4-pro:cloud","messages":[]}"#,
        );
        assert_eq!(fields.model.as_deref(), Some("deepseek-v4-pro:cloud"));
        assert!(!fields.streamed, "OpenAI defaults to non-streaming");
        let fields = request_fields(
            ApiFormat::OllamaNative,
            br#"{"model":"qwen3:14b","messages":[]}"#,
        );
        assert!(fields.streamed, "Ollama defaults to streaming");
        let fields = request_fields(
            ApiFormat::OllamaNative,
            br#"{"model":"qwen3:14b","stream":false}"#,
        );
        assert!(!fields.streamed);
        let fields = request_fields(
            ApiFormat::OpenAiEmbeddings,
            br#"{"model":"e","input":"x","stream":true}"#,
        );
        assert!(!fields.streamed, "embeddings never stream");
        let fields = request_fields(ApiFormat::OpenAiChat, b"garbage");
        assert_eq!(
            fields,
            RequestFields {
                model: None,
                streamed: false,
                json: None
            }
        );
    }

    #[test]
    fn request_model_keeps_only_plausible_names() {
        let model = |v: Value| request_model(v.as_object().unwrap());
        assert_eq!(
            model(json!({"model": "deepseek-v4-pro:cloud"})).as_deref(),
            Some("deepseek-v4-pro:cloud")
        );
        let longest = "m".repeat(MAX_MODEL_NAME);
        assert_eq!(
            model(json!({"model": longest})).as_deref(),
            Some(longest.as_str())
        );
        for bad in [
            json!({}),
            json!({"model": ""}),
            json!({"model": 7}),
            json!({"model": "m".repeat(MAX_MODEL_NAME + 1)}),
            json!({"model": "evil\u{1b}[2J"}),
            json!({"model": "two\nlines"}),
        ] {
            assert_eq!(model(bad.clone()), None, "{bad}");
        }
    }

    #[test]
    fn forwardable_headers_drop_hop_by_hop_host_and_connection_tokens() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "gateway:11435".parse().unwrap());
        headers.insert(
            header::CONNECTION,
            "keep-alive, X-Custom-Hop".parse().unwrap(),
        );
        headers.insert("x-custom-hop", "1".parse().unwrap());
        headers.insert(header::TRANSFER_ENCODING, "chunked".parse().unwrap());
        headers.insert(header::CONTENT_LENGTH, "12".parse().unwrap());
        headers.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
        headers.insert(header::AUTHORIZATION, "Bearer x".parse().unwrap());
        let kept = forwardable(&headers, true);
        assert_eq!(kept.len(), 2, "{kept:?}");
        assert!(kept.contains_key(header::CONTENT_TYPE));
        assert!(kept.contains_key(header::AUTHORIZATION));
        let kept = forwardable(&headers, false);
        assert!(kept.contains_key(header::CONTENT_LENGTH));
    }

    #[test]
    fn stored_headers_redact_credentials() {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer secret".parse().unwrap());
        headers.insert("x-api-key", "k".parse().unwrap());
        headers.insert("X-Goog-Api-Key", "g".parse().unwrap());
        headers.insert(header::USER_AGENT, "curl/8".parse().unwrap());
        let stored = redact_headers(&headers);
        assert!(stored.contains(&("authorization".to_string(), "[REDACTED]".to_string())));
        assert!(stored.contains(&("x-api-key".to_string(), "[REDACTED]".to_string())));
        assert!(stored.contains(&("x-goog-api-key".to_string(), "[REDACTED]".to_string())));
        assert!(stored.contains(&("user-agent".to_string(), "curl/8".to_string())));
    }

    #[test]
    fn query_strings_and_urls_redact_credential_parameters() {
        assert_eq!(
            redact_query("alt=sse&key=AIzaSyA1234567890abcdefghijklmnopqrstuv"),
            "alt=sse&key=[REDACTED]"
        );
        assert_eq!(
            redact_query("KEY=x&api_key=y&token=z&q=hello"),
            "KEY=[REDACTED]&api_key=[REDACTED]&token=[REDACTED]&q=hello"
        );
        assert_eq!(
            redact_query("flag&key"),
            "flag&key",
            "a parameter without a value is kept"
        );
        assert_eq!(redact_query(""), "");
        assert_eq!(
            redact_url(
                "https://generativelanguage.googleapis.com/v1beta/models?key=secret&pageSize=5"
            ),
            "https://generativelanguage.googleapis.com/v1beta/models?key=[REDACTED]&pageSize=5"
        );
        assert_eq!(
            redact_url("http://127.0.0.1:1234/v1/models"),
            "http://127.0.0.1:1234/v1/models"
        );
    }

    #[test]
    fn error_bodies_follow_the_api_family() {
        let (ct, body) = error_body(
            ApiFormat::OllamaNative,
            StatusCode::BAD_GATEWAY,
            "upstream unreachable",
            "upstream_unreachable",
        );
        assert_eq!(ct, "application/json");
        assert_eq!(val(&body), json!({"error": "upstream unreachable"}));
        let (_, body) = error_body(
            ApiFormat::AnthropicMessages,
            StatusCode::FORBIDDEN,
            "blocked",
            "dlp_blocked",
        );
        assert_eq!(val(&body)["error"]["type"], json!("permission_error"));
        let (_, body) = error_body(
            ApiFormat::OpenAiChat,
            StatusCode::FORBIDDEN,
            "blocked",
            "dlp_blocked",
        );
        assert_eq!(val(&body)["error"]["code"], json!("dlp_blocked"));
        let (ct, body) = error_body(
            ApiFormat::Passthrough,
            StatusCode::BAD_GATEWAY,
            "upstream unreachable",
            "x",
        );
        assert_eq!(ct, "text/plain; charset=utf-8");
        assert_eq!(body, Bytes::from("upstream unreachable\n"));
    }

    #[test]
    fn rewrite_model_replaces_the_name_and_leaves_other_bodies_alone() {
        let out = rewrite_model(
            Bytes::from(r#"{"model":"fast","messages":[],"stream":true}"#),
            "gemma4:12b-mlx",
        );
        let v = val(&out);
        assert_eq!(v["model"], json!("gemma4:12b-mlx"));
        assert_eq!(v["stream"], json!(true));
        assert_eq!(
            rewrite_model(Bytes::from("[1,2]"), "x"),
            Bytes::from("[1,2]"),
            "not an object"
        );
        assert_eq!(rewrite_model(Bytes::from("nope"), "x"), Bytes::from("nope"));
        assert_eq!(
            val(&rewrite_model(Bytes::from("{}"), "m"))["model"],
            json!("m")
        );
    }

    #[test]
    fn gemini_requests_take_model_and_streaming_from_the_path() {
        let fields = request_fields_for(
            ApiFormat::GeminiGenerate,
            "/v1beta/models/gemini-3.1-pro-preview:streamGenerateContent",
            br#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#,
        );
        assert_eq!(fields.model.as_deref(), Some("gemini-3.1-pro-preview"));
        assert!(fields.streamed);
        assert!(fields.json.is_some());
        let fields = request_fields_for(
            ApiFormat::GeminiGenerate,
            "/v1beta/models/gemini-3-flash:generateContent",
            b"{}",
        );
        assert_eq!(fields.model.as_deref(), Some("gemini-3-flash"));
        assert!(!fields.streamed);
        let fields = request_fields_for(
            ApiFormat::GeminiEmbed,
            "/v1beta/models/gemini-embedding-2:embedContent",
            b"{}",
        );
        assert_eq!(fields.model.as_deref(), Some("gemini-embedding-2"));
        assert!(!fields.streamed, "embeddings never stream");
        let fields = request_fields_for(
            ApiFormat::OpenAiChat,
            "/v1/chat/completions",
            br#"{"model":"gpt-5","messages":[],"stream":true}"#,
        );
        assert_eq!(fields.model.as_deref(), Some("gpt-5"));
        assert!(fields.streamed, "other formats read the body as before");
    }

    #[test]
    fn gemini_error_bodies_use_googles_shape() {
        let (ct, body) = error_body(
            ApiFormat::GeminiGenerate,
            StatusCode::FORBIDDEN,
            "blocked",
            "dlp_blocked",
        );
        assert_eq!(ct, "application/json");
        assert_eq!(
            val(&body),
            json!({"error": {"code": 403, "message": "blocked", "status": "PERMISSION_DENIED"}})
        );
        let (_, body) = error_body(ApiFormat::GeminiEmbed, StatusCode::BAD_GATEWAY, "down", "x");
        assert_eq!(val(&body)["error"]["status"], json!("UNAVAILABLE"));
        assert_eq!(val(&body)["error"]["code"], json!(502));
    }

    #[tokio::test]
    async fn read_capped_refuses_bodies_over_the_cap() {
        let ok = read_capped(Body::from("12345678"), 8).await.unwrap();
        assert_eq!(ok, Bytes::from_static(b"12345678"));
        assert!(matches!(
            read_capped(Body::from("123456789"), 8).await,
            Err(ProxyError::BodyTooLarge)
        ));
    }
}
