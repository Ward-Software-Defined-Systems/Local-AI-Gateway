//! End-to-end proxy tests over real sockets: a mock backend on `127.0.0.1:0`, a listener in front of it,
//! reqwest as the client, and the event bus as the witness. Ported from Ollama-Monitor's `proxy.rs` tests
//! and extended for the second backend, the tunnel, the DLP hook and the inspector's capture.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::extract::ws::{Message, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode, header};
use axum::routing::{any, get, post};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use gateway_core::config::UnifiedConfig;
use gateway_core::events::{EventReceiver, GatewayEvent};
use gateway_core::pricing::PricingTable;
use gateway_core::routing::RoutingTable;
use gateway_core::state::BackendsState;
use gateway_core::types::{
    ApiFormat, Backend, BackendSnapshot, Direction, FailureSource, ModelInfo, ModelState, Outcome,
    RequestKind, RequestRecord,
};
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::listener::{
    BackendUpstream, CapturePolicy, GATEWAY_HEADER, GATEWAY_VERSION, ListenerConfig, ListenerMode,
    ProxyContext, TargetFlags, bind, serve,
};
use crate::scan::{Decision, RequestScanner, ScanContext, ScanOutcome};

const OLLAMA_STREAM: &str = include_str!("../../../fixtures/ollama/ollama-chat-stream.jsonl");
const OPENAI_SSE: &str = include_str!("../../../fixtures/ollama/openai-compat-sse.txt");

struct TestProxy {
    url: String,
    events: EventReceiver,
    ctx: Arc<ProxyContext>,
    _shutdown: watch::Sender<bool>,
}

struct Options {
    backend: Backend,
    capture_bodies: bool,
    record_passthrough: bool,
    rewrite_include_usage: bool,
    store: Option<gateway_core::store::StoreHandle>,
    scanner: Option<Arc<dyn RequestScanner>>,
    session_id: i64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            backend: Backend::Ollama,
            capture_bodies: true,
            record_passthrough: true,
            rewrite_include_usage: true,
            store: None,
            scanner: None,
            session_id: 7,
        }
    }
}

async fn spawn_upstream(router: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await });
    url
}

fn start_proxy(upstream: String, opts: Options) -> TestProxy {
    let listener = bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (events, rx) = gateway_core::events::channel();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let ctx = ProxyContext::new(
        opts.store,
        events,
        opts.session_id,
        Arc::new(PricingTable::baked()),
        Arc::new(BackendsState::new()),
        CapturePolicy {
            max_body_bytes: 1024 * 1024,
            max_buffered_request_bytes: 1024 * 1024,
        },
    )
    .unwrap();
    if let Some(scanner) = opts.scanner {
        ctx.set_scanner(scanner);
    }
    let config = Arc::new(ListenerConfig {
        listen: listener.local_addr().unwrap(),
        mode: ListenerMode::Fixed(BackendUpstream {
            backend: opts.backend,
            upstream,
            flags: TargetFlags {
                capture_bodies: opts.capture_bodies,
                record_passthrough: opts.record_passthrough,
                rewrite_include_usage: opts.rewrite_include_usage,
            },
        }),
    });
    tokio::spawn(serve(listener, config, ctx.clone(), shutdown_rx));
    TestProxy {
        url,
        events: rx,
        ctx,
        _shutdown: shutdown_tx,
    }
}

async fn spawn_proxy(router: Router, opts: Options) -> TestProxy {
    let upstream = spawn_upstream(router).await;
    start_proxy(upstream, opts)
}

/// The next completed record on the bus, within five seconds.
async fn next_record(events: &mut EventReceiver) -> RequestRecord {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await.expect("bus open").as_ref() {
                GatewayEvent::RequestCompleted { record } => return *record.clone(),
                _ => continue,
            }
        }
    })
    .await
    .expect("a record within five seconds")
}

async fn no_record_within(events: &mut EventReceiver, wait: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Ok(event)) => {
                if matches!(event.as_ref(), GatewayEvent::RequestCompleted { .. }) {
                    return false;
                }
            }
            _ => return true,
        }
    }
}

#[tokio::test]
async fn proxies_and_records_an_ollama_stream_over_real_sockets() {
    let mock = Router::new().route(
        "/api/chat",
        post(|| async {
            (
                [(header::CONTENT_TYPE, "application/x-ndjson")],
                OLLAMA_STREAM,
            )
        }),
    );
    let mut proxy = spawn_proxy(mock, Options::default()).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/api/chat", proxy.url))
        .body(r#"{"model":"qwen3:14b","messages":[{"role":"user","content":"hi"}]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let request_id = resp
        .headers()
        .get("x-request-id")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(
        resp.text().await.unwrap(),
        OLLAMA_STREAM,
        "the client sees upstream's bytes unchanged"
    );
    let record = next_record(&mut proxy.events).await;
    assert_eq!(record.id.to_string(), request_id);
    assert_eq!(record.session_id, 7);
    assert_eq!(record.backend, Backend::Ollama);
    assert_eq!(record.api_format, ApiFormat::OllamaNative);
    assert_eq!(record.kind, RequestKind::Http);
    assert_eq!(record.outcome, Outcome::Completed);
    assert_eq!(record.model_requested.as_deref(), Some("qwen3:14b"));
    assert_eq!(record.model_resolved.as_deref(), Some("qwen3:14b"));
    assert!(record.gen_tokens.unwrap() > 0);
    assert!(
        !record.timings_estimated,
        "the NDJSON terminal line carries engine timings"
    );
    assert!(record.streamed);
    assert!(!record.chunk_timeline.is_empty());
    assert!(record.hypothetical_cost.contains_key("claude-fable-5-1"));
    assert_eq!(record.status, Some(200));
    assert_eq!(proxy.ctx.inflight_count(), 0);
}

#[tokio::test]
async fn failed_inference_requests_pass_through_and_are_recorded_with_their_message() {
    const V1_ERROR: &str = r#"{"error":{"message":"too many requests","type":"api_error"}}"#;
    const NATIVE_ERROR: &str = r#"{"error":"model 'qwen9' not found"}"#;
    let mock = Router::new()
        .route(
            "/api/tags",
            get(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "tags failed") }),
        )
        .route(
            "/v1/chat/completions",
            post(|| async {
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    [(header::CONTENT_TYPE, "application/json")],
                    V1_ERROR,
                )
            }),
        )
        .route(
            "/api/chat",
            post(|| async {
                (
                    StatusCode::NOT_FOUND,
                    [(header::CONTENT_TYPE, "application/json")],
                    NATIVE_ERROR,
                )
            }),
        );
    let mut proxy = spawn_proxy(mock, Options::default()).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{}/api/tags", proxy.url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(resp.text().await.unwrap(), "tags failed");
    let record = next_record(&mut proxy.events).await;
    assert_eq!(record.api_format, ApiFormat::Passthrough);
    assert_eq!(
        record.outcome,
        Outcome::Failed {
            status: 500,
            source: FailureSource::Upstream
        }
    );
    assert_eq!(record.response_bytes, "tags failed".len() as u64);

    let resp = client
        .post(format!("{}/v1/chat/completions", proxy.url))
        .body(r#"{"model":"deepseek-v4-pro:cloud","messages":[{"role":"user","content":"hi"}]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(resp.text().await.unwrap(), V1_ERROR, "error body changed");
    let record = next_record(&mut proxy.events).await;
    assert_eq!(
        record.outcome,
        Outcome::Failed {
            status: 429,
            source: FailureSource::Upstream
        }
    );
    assert_eq!(
        record.model_requested.as_deref(),
        Some("deepseek-v4-pro:cloud")
    );
    assert!(record.is_cloud, "the :cloud tag marks it before any poll");
    assert_eq!(record.error_summary.as_deref(), Some("too many requests"));
    assert_eq!(
        record.outcome.summary().as_deref(),
        Some("failed (HTTP 429)")
    );

    let resp = client
        .post(format!("{}/api/chat", proxy.url))
        .body(r#"{"model":"qwen9","messages":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
    assert_eq!(resp.text().await.unwrap(), NATIVE_ERROR);
    let record = next_record(&mut proxy.events).await;
    assert_eq!(
        record.outcome,
        Outcome::Failed {
            status: 404,
            source: FailureSource::Upstream
        }
    );
    assert_eq!(
        record.model_requested.as_deref(),
        Some("qwen9"),
        "native bodies are read too now"
    );
    assert_eq!(
        record.error_summary.as_deref(),
        Some("model 'qwen9' not found")
    );
}

#[tokio::test]
async fn unreachable_upstream_is_a_gateway_failure_shaped_for_the_api() {
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let upstream = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    let mut proxy = start_proxy(upstream, Options::default());
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", proxy.url))
        .body(r#"{"model":"qwen3:14b","messages":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_GATEWAY);
    assert!(resp.headers().contains_key("x-request-id"));
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "upstream_unreachable");
    let record = next_record(&mut proxy.events).await;
    assert_eq!(
        record.outcome,
        Outcome::Failed {
            status: 502,
            source: FailureSource::Gateway
        }
    );
    assert_eq!(
        record.outcome.summary().as_deref(),
        Some("gateway (HTTP 502)")
    );
    assert_eq!(record.model_requested.as_deref(), Some("qwen3:14b"));
    // The Ollama-native shape for the same failure.
    let resp = reqwest::Client::new()
        .post(format!("{}/api/chat", proxy.url))
        .body(r#"{"model":"qwen3:14b"}"#)
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "upstream unreachable");
    let _ = next_record(&mut proxy.events).await;
}

#[tokio::test]
async fn large_passthrough_uploads_stream_with_their_content_length() {
    let mock = Router::new().route(
        "/api/blobs/{digest}",
        post(|headers: HeaderMap, body: Body| async move {
            let mut received = 0usize;
            let mut stream = body.into_data_stream();
            while let Some(chunk) = stream.next().await {
                received += chunk.unwrap().len();
            }
            let declared = headers
                .get(header::CONTENT_LENGTH)
                .map(|v| v.to_str().unwrap().to_string())
                .unwrap_or_default();
            format!("{received} {declared}")
        }),
    );
    let mut proxy = spawn_proxy(mock, Options::default()).await;
    let size = 1024 * 1024 + 1; // past the inference buffering cap, which never applies to passthrough
    let resp = reqwest::Client::new()
        .post(format!("{}/api/blobs/sha256:abc", proxy.url))
        .body(vec![7u8; size])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert_eq!(resp.text().await.unwrap(), format!("{size} {size}"));
    let record = next_record(&mut proxy.events).await;
    assert_eq!(record.api_format, ApiFormat::Passthrough);
    assert_eq!(record.outcome, Outcome::Completed);
    assert_eq!(record.method, "POST");
    assert_eq!(record.path, "/api/blobs/sha256:abc");
}

#[tokio::test]
async fn an_inference_body_over_the_cap_is_refused_with_413() {
    let mock = Router::new().route("/v1/chat/completions", post(|| async { "never" }));
    let mut proxy = spawn_proxy(mock, Options::default()).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", proxy.url))
        .body(vec![b'x'; 1024 * 1024 + 1])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE);
    let record = next_record(&mut proxy.events).await;
    assert_eq!(
        record.outcome,
        Outcome::Failed {
            status: 413,
            source: FailureSource::Gateway
        }
    );
}

#[tokio::test]
async fn include_usage_is_injected_only_for_streaming_openai_requests() {
    let seen: Arc<tokio::sync::Mutex<Vec<serde_json::Value>>> = Arc::default();
    let seen2 = seen.clone();
    let mock = Router::new().route(
        "/v1/chat/completions",
        post(move |body: Bytes| {
            let seen = seen2.clone();
            async move {
                seen.lock()
                    .await
                    .push(serde_json::from_slice(&body).unwrap());
                ([(header::CONTENT_TYPE, "text/event-stream")], OPENAI_SSE)
            }
        }),
    );
    let mut proxy = spawn_proxy(mock, Options::default()).await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/chat/completions", proxy.url))
        .body(r#"{"model":"qwen3:14b","messages":[{"role":"user","content":"hi"}],"stream":true}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.text().await.unwrap(),
        OPENAI_SSE,
        "the response is untouched"
    );
    let record = next_record(&mut proxy.events).await;
    assert_eq!(
        (record.prompt_tokens, record.gen_tokens),
        (Some(11), Some(5))
    );
    assert_eq!(record.stop_reason.as_deref(), Some("length"));
    assert!(record.timings_estimated, "OpenAI SSE has no engine timings");
    assert!(record.ttft_ms.is_some());
    assert!(record.streamed);
    let resp = client
        .post(format!("{}/v1/chat/completions", proxy.url))
        .body(r#"{"model":"qwen3:14b","messages":[]}"#)
        .send()
        .await
        .unwrap();
    let _ = resp.text().await.unwrap();
    let record = next_record(&mut proxy.events).await;
    assert!(!record.streamed, "a missing stream field is non-streaming");
    let seen = seen.lock().await;
    assert_eq!(
        seen[0]["stream_options"]["include_usage"],
        serde_json::json!(true)
    );
    assert!(
        seen[1].get("stream_options").is_none(),
        "non-streaming requests are forwarded as sent"
    );
}

#[tokio::test]
async fn an_lm_studio_context_overflow_400_is_a_rejection() {
    const OVERFLOW: &str = r#"{"error":{"message":"Input does not fit in context length. The input has 359277 tokens, but the context length only supports 262144 tokens."}}"#;
    let mock = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            (
                StatusCode::BAD_REQUEST,
                [(header::CONTENT_TYPE, "application/json")],
                OVERFLOW,
            )
        }),
    );
    let mut proxy = spawn_proxy(
        mock,
        Options {
            backend: Backend::LmStudio,
            ..Default::default()
        },
    )
    .await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", proxy.url))
        .body(r#"{"model":"qwen3.8-27b","messages":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(resp.text().await.unwrap(), OVERFLOW);
    let record = next_record(&mut proxy.events).await;
    assert_eq!(record.backend, Backend::LmStudio);
    assert_eq!(
        record.outcome,
        Outcome::Rejected {
            input_tokens: Some(359_277),
            context_length: Some(262_144)
        }
    );
    assert_eq!(
        record.outcome.summary().as_deref(),
        Some("rejected (ctx 262144)")
    );
}

#[tokio::test]
async fn an_error_frame_inside_an_open_stream_is_a_rejection_too() {
    let frame = "data: {\"error\":{\"message\":\"Input does not fit in context length. The input has 359277 tokens, but the context length only supports 262144 tokens.\"}}\n\n";
    let mock = Router::new().route(
        "/v1/chat/completions",
        post(move || async move { ([(header::CONTENT_TYPE, "text/event-stream")], frame) }),
    );
    let mut proxy = spawn_proxy(
        mock,
        Options {
            backend: Backend::LmStudio,
            ..Default::default()
        },
    )
    .await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", proxy.url))
        .body(r#"{"model":"qwen3.8-27b","messages":[],"stream":true}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::OK,
        "the status was already sent"
    );
    let _ = resp.text().await.unwrap();
    let record = next_record(&mut proxy.events).await;
    assert!(
        matches!(
            record.outcome,
            Outcome::Rejected {
                context_length: Some(262_144),
                ..
            }
        ),
        "{:?}",
        record.outcome
    );
    assert!(
        record
            .error_summary
            .as_deref()
            .unwrap()
            .starts_with("Input does not fit")
    );
}

#[tokio::test]
async fn response_bytes_are_counted_when_bodies_are_not_captured() {
    let mock = Router::new().route(
        "/api/chat",
        post(|| async {
            (
                [(header::CONTENT_TYPE, "application/x-ndjson")],
                OLLAMA_STREAM,
            )
        }),
    );
    let mut proxy = spawn_proxy(
        mock,
        Options {
            capture_bodies: false,
            ..Default::default()
        },
    )
    .await;
    let resp = reqwest::Client::new()
        .post(format!("{}/api/chat", proxy.url))
        .body(r#"{"model":"qwen3:14b","messages":[{"role":"user","content":"hi"}]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.text().await.unwrap(), OLLAMA_STREAM);
    let record = next_record(&mut proxy.events).await;
    assert_eq!(record.outcome, Outcome::Completed);
    assert_eq!(record.response_bytes, OLLAMA_STREAM.len() as u64);
    assert_eq!(record.routed_by.as_deref(), Some("listener"));
    assert!(
        record.gen_tokens.unwrap() > 0,
        "the parser still saw the stream"
    );
}

/// A fixed listener for a cloud provider: the record is cloud egress, credentials in the query are stored
/// redacted while the upstream still receives them, and a redirect is handed to the client unfollowed.
#[tokio::test]
async fn a_cloud_listener_redacts_the_key_query_and_passes_redirects_through() {
    use axum::extract::RawQuery;
    use std::sync::Mutex;
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen2 = seen.clone();
    let mock = Router::new()
        .route(
            "/v1beta/models",
            get(move |RawQuery(q): RawQuery| {
                let seen = seen2.clone();
                async move {
                    seen.lock().unwrap().push(q.unwrap_or_default());
                    (
                        [(header::CONTENT_TYPE, "application/json")],
                        r#"{"models":[]}"#,
                    )
                }
            }),
        )
        .route(
            "/v1/chat/completions",
            post(|| async {
                (
                    StatusCode::TEMPORARY_REDIRECT,
                    [(
                        header::LOCATION,
                        "https://elsewhere.example/v1/chat/completions",
                    )],
                    "",
                )
            }),
        );
    let mut proxy = spawn_proxy(
        mock,
        Options {
            backend: Backend::Gemini,
            ..Default::default()
        },
    )
    .await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let resp = client
        .get(format!(
            "{}/v1beta/models?key=AIzaSECRET&pageSize=5",
            proxy.url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let record = next_record(&mut proxy.events).await;
    assert_eq!(record.backend, Backend::Gemini);
    assert!(
        record.is_cloud,
        "every request to a cloud provider is egress"
    );
    assert_eq!(record.api_format, ApiFormat::Passthrough);
    assert_eq!(record.query.as_deref(), Some("key=[REDACTED]&pageSize=5"));
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        ["key=AIzaSECRET&pageSize=5"],
        "the upstream received the raw query"
    );

    let resp = client
        .post(format!("{}/v1/chat/completions", proxy.url))
        .body(r#"{"model":"gemini-3-flash","messages":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        resp.headers().get(header::LOCATION).unwrap(),
        "https://elsewhere.example/v1/chat/completions"
    );
    let record = next_record(&mut proxy.events).await;
    assert_eq!(
        record.outcome,
        Outcome::Failed {
            status: 307,
            source: FailureSource::Upstream
        }
    );
    assert_eq!(record.status, Some(307));
    assert!(record.is_cloud);
}

/// Every value of the gateway's own response header, so a test sees a duplicate as well as a wrong value.
fn gateway_header(resp: &reqwest::Response) -> Vec<String> {
    resp.headers()
        .get_all(GATEWAY_HEADER)
        .iter()
        .map(|v| v.to_str().unwrap().to_string())
        .collect()
}

/// Every response from a proxy listener names the gateway and its version, so a client can check what it is
/// talking to before it routes provider traffic through it. A fixed listener: a proxied stream (the header
/// on the head, upstream's bytes in the body), a passthrough, a gateway refusal, and an upstream that
/// claims the header itself.
#[tokio::test]
async fn every_response_from_a_fixed_listener_names_the_gateway_and_its_version() {
    let mock = Router::new()
        .route(
            "/v1/chat/completions",
            post(|| async {
                (
                    [
                        (header::CONTENT_TYPE, "text/event-stream"),
                        (header::HeaderName::from_static(GATEWAY_HEADER), "evil"),
                    ],
                    OPENAI_SSE,
                )
            }),
        )
        .route("/api/tags", get(|| async { r#"{"models":[]}"# }));
    let mut proxy = spawn_proxy(mock, Options::default()).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{}/v1/chat/completions", proxy.url))
        .body(r#"{"model":"m","messages":[],"stream":true}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert_eq!(
        gateway_header(&resp),
        vec![GATEWAY_VERSION],
        "the upstream's claim is replaced, not appended to"
    );
    assert_eq!(
        resp.text().await.unwrap(),
        OPENAI_SSE,
        "the body is untouched"
    );
    next_record(&mut proxy.events).await;

    let resp = client
        .get(format!("{}/api/tags", proxy.url))
        .send()
        .await
        .unwrap();
    assert_eq!(gateway_header(&resp), vec![GATEWAY_VERSION], "passthrough");
    next_record(&mut proxy.events).await;

    let resp = client
        .post(format!("{}/v1/chat/completions", proxy.url))
        .body(vec![b'x'; 1024 * 1024 + 1])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        gateway_header(&resp),
        vec![GATEWAY_VERSION],
        "a 413 refusal"
    );
}

/// The unified listener's gateway-generated answers carry it too: the merged model list, a DLP block and a
/// format the provider lacks. None of these reaches an upstream.
#[tokio::test]
async fn gateway_generated_answers_on_the_unified_listener_name_the_gateway_too() {
    let (anthropic_router, anthropic_seen) = recording_mock("anthropic");
    let (ollama_router, _ollama_seen) = recording_mock("ollama");
    let targets = BTreeMap::from([
        (Backend::Anthropic, spawn_upstream(anthropic_router).await),
        (Backend::Ollama, spawn_upstream(ollama_router).await),
    ]);
    let mut proxy = start_unified(
        targets,
        RoutingTable::default(),
        Options {
            scanner: Some(Arc::new(BlockAkia {
                calls: AtomicUsize::new(0),
            })),
            ..Default::default()
        },
    );
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{}/v1/models", proxy.url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert_eq!(
        gateway_header(&resp),
        vec![GATEWAY_VERSION],
        "the merged list"
    );
    let list: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(list["object"], "list");

    let resp = client
        .post(format!("{}/v1/messages", proxy.url))
        .header(header::CONTENT_TYPE, "application/json")
        .body(r#"{"model":"claude-opus-5-5","max_tokens":1,"messages":[{"role":"user","content":"AKIAIOSFODNN7EXAMPLE"}]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);
    assert_eq!(gateway_header(&resp), vec![GATEWAY_VERSION], "a DLP block");
    let record = next_record(&mut proxy.events).await;
    assert!(matches!(record.outcome, Outcome::Blocked { .. }));

    let resp = client
        .post(format!("{}/v1/embeddings", proxy.url))
        .header(header::CONTENT_TYPE, "application/json")
        .body(r#"{"model":"claude-opus-5-5","input":"x"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(
        gateway_header(&resp),
        vec![GATEWAY_VERSION],
        "a format refusal"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "format_not_served", "{body}");
    next_record(&mut proxy.events).await;
    assert!(
        anthropic_seen.lock().unwrap().is_empty(),
        "nothing reached the provider"
    );
}

/// An OpenAI chat completion as a provider's compat endpoint answers it: a million tokens each way so the
/// cost at list price is a round number.
const CHAT_JSON: &str = r#"{"id":"chatcmpl-1","object":"chat.completion","created":0,"model":"claude-opus-5-5","choices":[{"index":0,"message":{"role":"assistant","content":"Hi"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1000000,"completion_tokens":1000000,"total_tokens":2000000}}"#;

/// Anthropic's real answer to `POST /v1/messages/count_tokens`.
const COUNT_TOKENS_JSON: &str =
    include_str!("../../../fixtures/anthropic/v1-messages-count-tokens.json");

/// A POST outside the inference formats to a cloud provider is scanned and recorded with its bodies; a
/// finding under a blocking policy stops it with the provider's own error shape.
#[tokio::test]
async fn a_cloud_post_outside_the_inference_formats_is_scanned_and_recorded() {
    let hits = Arc::new(AtomicUsize::new(0));
    let hits2 = hits.clone();
    let mock = Router::new().route(
        "/v1/messages/count_tokens",
        post(move || {
            let hits = hits2.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                (
                    [(header::CONTENT_TYPE, "application/json")],
                    COUNT_TOKENS_JSON,
                )
            }
        }),
    );
    let upstream = spawn_upstream(mock).await;
    let mut blocked = start_proxy(
        upstream.clone(),
        Options {
            backend: Backend::Anthropic,
            scanner: Some(Arc::new(BlockAkia {
                calls: AtomicUsize::new(0),
            })),
            ..Default::default()
        },
    );
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/v1/messages/count_tokens", blocked.url))
        .header("x-api-key", "sk-ant-test")
        .body(r#"{"model":"claude-opus-5-5","messages":[{"role":"user","content":"AKIAIOSFODNN7EXAMPLE"}]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["type"], "error", "Anthropic's error shape: {body}");
    assert_eq!(body["error"]["type"], "permission_error");
    let record = next_record(&mut blocked.events).await;
    assert_eq!(record.api_format, ApiFormat::CloudPassthrough);
    assert_eq!(
        record.outcome,
        Outcome::Blocked {
            rule_id: "aws-access-key-id".into()
        }
    );
    assert!(record.is_cloud);
    assert_eq!(record.model_requested.as_deref(), Some("claude-opus-5-5"));
    assert!(!record.is_feed_row());
    assert_eq!(hits.load(Ordering::SeqCst), 0, "nothing left the machine");

    let mut clean = start_proxy(
        upstream,
        Options {
            backend: Backend::Anthropic,
            ..Default::default()
        },
    );
    let resp = client
        .post(format!("{}/v1/messages/count_tokens", clean.url))
        .body(r#"{"model":"claude-opus-5-5","messages":[{"role":"user","content":"hello"}]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert_eq!(resp.text().await.unwrap(), COUNT_TOKENS_JSON);
    let record = next_record(&mut clean.events).await;
    assert_eq!(record.api_format, ApiFormat::CloudPassthrough);
    assert_eq!(record.outcome, Outcome::Completed);
    assert_eq!(record.status, Some(200));
    assert_eq!(record.response_bytes, COUNT_TOKENS_JSON.len() as u64);
    assert!(record.is_cloud);
    assert_eq!(
        record.prompt_tokens, None,
        "nothing is read out of a passthrough answer"
    );
    assert!(record.actual_cost_usd.is_none());
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

/// Cloud targets get identity encoding so the parsers can read the answer; local ones keep the client's
/// choice. A completed request to a provider carries its real cost and no hypothetical columns.
#[tokio::test]
async fn cloud_targets_get_identity_encoding_and_a_real_cost() {
    use std::sync::Mutex;
    let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::default();
    let seen2 = seen.clone();
    let mock = Router::new().route(
        "/v1/chat/completions",
        post(move |headers: HeaderMap| {
            let seen = seen2.clone();
            async move {
                seen.lock().unwrap().push(
                    headers
                        .get(header::ACCEPT_ENCODING)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string),
                );
                // Anthropic's compat endpoint names its own request id in `x-request-id`.
                (
                    [
                        ("content-type", "application/json"),
                        ("x-request-id", "req_upstream"),
                    ],
                    CHAT_JSON,
                )
            }
        }),
    );
    let upstream = spawn_upstream(mock).await;
    let body = r#"{"model":"claude-opus-5-5","messages":[{"role":"user","content":"hi"}]}"#;
    let client = reqwest::Client::builder()
        .no_gzip()
        .no_brotli()
        .build()
        .unwrap();

    let mut cloud = start_proxy(
        upstream.clone(),
        Options {
            backend: Backend::Anthropic,
            ..Default::default()
        },
    );
    let resp = client
        .post(format!("{}/v1/chat/completions", cloud.url))
        .header(header::ACCEPT_ENCODING, "gzip, br")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let ids: Vec<String> = resp
        .headers()
        .get_all("x-request-id")
        .iter()
        .map(|v| v.to_str().unwrap().to_string())
        .collect();
    assert_eq!(
        ids.len(),
        1,
        "the client sees the gateway's request id alone: {ids:?}"
    );
    assert_eq!(resp.text().await.unwrap(), CHAT_JSON);
    let record = next_record(&mut cloud.events).await;
    assert_eq!(ids[0], record.id.to_string());
    assert!(
        record
            .response_headers
            .iter()
            .any(|(name, value)| name == "x-request-id" && value == "req_upstream"),
        "the provider's id stays in the stored headers: {:?}",
        record.response_headers
    );
    assert_eq!(record.outcome, Outcome::Completed);
    assert!(record.is_cloud);
    assert_eq!(record.model_resolved.as_deref(), Some("claude-opus-5-5"));
    assert_eq!(
        record.actual_cost_usd,
        Some(24.0),
        "4 + 20 USD per million at Claude Opus 5.5's list price"
    );
    assert!(
        record.hypothetical_cost.is_empty(),
        "no frontier columns for a provider's own request"
    );
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        [None],
        "accept-encoding stripped"
    );

    let mut local = start_proxy(upstream, Options::default());
    let resp = client
        .post(format!("{}/v1/chat/completions", local.url))
        .header(header::ACCEPT_ENCODING, "gzip, br")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let record = next_record(&mut local.events).await;
    assert!(!record.is_cloud);
    assert_eq!(record.actual_cost_usd, None);
    assert!(record.hypothetical_cost.contains_key("claude-fable-5-1"));
    assert_eq!(
        seen.lock().unwrap()[1].as_deref(),
        Some("gzip, br"),
        "a local server sees the client's header"
    );
}

/// A format the provider does not serve, or a header that belongs to another provider, is refused by the
/// gateway before anything is sent.
#[tokio::test]
async fn mismatched_formats_and_credentials_are_refused_before_anything_leaves() {
    let hits = Arc::new(AtomicUsize::new(0));
    let hits2 = hits.clone();
    let mock = Router::new().fallback(any(move || {
        let hits = hits2.clone();
        async move {
            hits.fetch_add(1, Ordering::SeqCst);
            ([(header::CONTENT_TYPE, "application/json")], "{}")
        }
    }));
    let upstream = spawn_upstream(mock).await;
    let client = reqwest::Client::new();

    let mut anthropic = start_proxy(
        upstream.clone(),
        Options {
            backend: Backend::Anthropic,
            ..Default::default()
        },
    );
    let resp = client
        .post(format!("{}/v1/embeddings", anthropic.url))
        .body(r#"{"model":"claude-opus-5-5","input":"x"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "format_not_served", "{body}");
    let record = next_record(&mut anthropic.events).await;
    assert_eq!(
        record.outcome,
        Outcome::Failed {
            status: 400,
            source: FailureSource::Gateway
        }
    );
    assert_eq!(record.api_format, ApiFormat::OpenAiEmbeddings);

    let mut gemini = start_proxy(
        upstream.clone(),
        Options {
            backend: Backend::Gemini,
            ..Default::default()
        },
    );
    let resp = client
        .get(format!("{}/v1beta/models", gemini.url))
        .header("anthropic-version", "2023-06-01")
        .header("x-api-key", "sk-ant-oops")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body["error"]["status"], "INVALID_ARGUMENT",
        "Google's shape: {body}"
    );
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("anthropic-version")
    );
    let record = next_record(&mut gemini.events).await;
    assert_eq!(
        record.outcome,
        Outcome::Failed {
            status: 400,
            source: FailureSource::Gateway
        }
    );
    assert!(
        record
            .error_summary
            .as_deref()
            .unwrap()
            .contains("anthropic-version")
    );
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "neither request left the machine"
    );

    let mut ollama = start_proxy(upstream, Options::default());
    let resp = client
        .get(format!("{}/v1/models", ollama.url))
        .header("anthropic-version", "2023-06-01")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::OK,
        "local targets take any header"
    );
    let _ = next_record(&mut ollama.events).await;
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

/// A unified listener over `targets` (backend → mock URL), with the routing table given.
fn start_unified(
    targets: BTreeMap<Backend, String>,
    routing: RoutingTable,
    opts: Options,
) -> TestProxy {
    let listener = bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (events, rx) = gateway_core::events::channel();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let ctx = ProxyContext::new(
        opts.store,
        events,
        opts.session_id,
        Arc::new(PricingTable::baked()),
        Arc::new(BackendsState::new()),
        CapturePolicy {
            max_body_bytes: 1024 * 1024,
            max_buffered_request_bytes: 1024 * 1024,
        },
    )
    .unwrap();
    if let Some(scanner) = opts.scanner {
        ctx.set_scanner(scanner);
    }
    ctx.set_routing(Arc::new(routing));
    let flags = TargetFlags {
        capture_bodies: opts.capture_bodies,
        record_passthrough: opts.record_passthrough,
        rewrite_include_usage: opts.rewrite_include_usage,
    };
    let targets = targets
        .into_iter()
        .map(|(backend, upstream)| {
            (
                backend,
                BackendUpstream {
                    backend,
                    upstream,
                    flags,
                },
            )
        })
        .collect();
    let config = Arc::new(ListenerConfig {
        listen: listener.local_addr().unwrap(),
        mode: ListenerMode::Unified { targets },
    });
    tokio::spawn(serve(listener, config, ctx.clone(), shutdown_rx));
    TestProxy {
        url,
        events: rx,
        ctx,
        _shutdown: shutdown_tx,
    }
}

/// What a mock saw: method, path with query, and the body's `model` when it had one.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Seen {
    method: String,
    path: String,
    model: Option<String>,
}

type SeenLog = Arc<std::sync::Mutex<Vec<Seen>>>;

/// A mock that answers every route with a small JSON body and records what it was asked.
fn recording_mock(name: &'static str) -> (Router, SeenLog) {
    let seen: SeenLog = Arc::default();
    let seen2 = seen.clone();
    let router = Router::new().fallback(any(move |req: Request| {
        let seen = seen2.clone();
        async move {
            let (parts, body) = req.into_parts();
            let bytes = axum::body::to_bytes(body, 1 << 20)
                .await
                .unwrap_or_default();
            let model = serde_json::from_slice::<serde_json::Value>(&bytes)
                .ok()
                .and_then(|v| v.get("model")?.as_str().map(str::to_string));
            seen.lock().unwrap().push(Seen {
                method: parts.method.to_string(),
                path: parts
                    .uri
                    .path_and_query()
                    .map(|p| p.to_string())
                    .unwrap_or_default(),
                model,
            });
            (
                [(header::CONTENT_TYPE, "application/json")],
                format!(r#"{{"served_by":"{name}"}}"#),
            )
        }
    }));
    (router, seen)
}

fn last_seen(log: &SeenLog) -> Seen {
    log.lock()
        .unwrap()
        .last()
        .cloned()
        .expect("the mock was called")
}

/// Ollama's model list with cloud models marked.
fn ollama_snapshot(ids: &[(&str, bool)]) -> BackendSnapshot {
    BackendSnapshot {
        backend: Backend::Ollama,
        reachable: true,
        version: None,
        error: None,
        models: ids
            .iter()
            .map(|(id, cloud)| ModelInfo {
                backend: Backend::Ollama,
                id: id.to_string(),
                kind: Some("llm".into()),
                compatibility_type: None,
                quantization: None,
                max_context_length: None,
                loaded_context_length: None,
                state: if *cloud {
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
            })
            .collect(),
        polled_at: chrono::Utc::now(),
    }
}

/// One listener, four mock providers: every way a request picks its target, in order.
#[tokio::test]
async fn the_unified_listener_routes_by_prefix_path_model_header_and_default() {
    let (lms_router, lms_seen) = recording_mock("lmstudio");
    let (ollama_router, ollama_seen) = recording_mock("ollama");
    let (anthropic_router, anthropic_seen) = recording_mock("anthropic");
    let (gemini_router, gemini_seen) = recording_mock("gemini");
    let targets = BTreeMap::from([
        (Backend::LmStudio, spawn_upstream(lms_router).await),
        (Backend::Ollama, spawn_upstream(ollama_router).await),
        (Backend::Anthropic, spawn_upstream(anthropic_router).await),
        (Backend::Gemini, spawn_upstream(gemini_router).await),
    ]);
    let routing = RoutingTable::from_config(&UnifiedConfig {
        aliases: BTreeMap::from([("fast".to_string(), "ollama/gemma4:12b-mlx".to_string())]),
        ..UnifiedConfig::default()
    });
    let mut proxy = start_unified(targets, routing, Options::default());
    proxy
        .ctx
        .backends
        .set(snapshot_with_loaded_model("qwen/qwen3.6-27b", 4096))
        .await;
    proxy
        .ctx
        .backends
        .set(ollama_snapshot(&[
            ("gemma4:12b-mlx", false),
            ("deepseek-v4-pro:cloud", true),
        ]))
        .await;
    let client = reqwest::Client::new();
    let post = |path: &str, body: &str| {
        client
            .post(format!("{}{path}", proxy.url))
            .header(header::CONTENT_TYPE, "application/json")
            .body(body.to_string())
    };

    // The live model list.
    let resp = post(
        "/v1/chat/completions",
        r#"{"model":"qwen/qwen3.6-27b","messages":[]}"#,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert_eq!(resp.text().await.unwrap(), r#"{"served_by":"lmstudio"}"#);
    let record = next_record(&mut proxy.events).await;
    assert_eq!(record.backend, Backend::LmStudio);
    assert_eq!(record.routed_by.as_deref(), Some("model_list"));
    assert!(!record.is_cloud);
    assert_eq!(last_seen(&lms_seen).path, "/v1/chat/completions");

    // A built-in pattern, through Gemini's OpenAI-compatible surface.
    let resp = post(
        "/v1/chat/completions",
        r#"{"model":"gemini-3-flash","messages":[]}"#,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let record = next_record(&mut proxy.events).await;
    assert_eq!(record.backend, Backend::Gemini);
    assert_eq!(record.routed_by.as_deref(), Some("model_pattern"));
    assert!(record.is_cloud);
    assert_eq!(
        record.path, "/v1/chat/completions",
        "the client's path is what is recorded"
    );
    assert_eq!(
        last_seen(&gemini_seen).path,
        "/v1beta/openai/chat/completions"
    );

    // A built-in pattern in Anthropic's own format.
    let resp = post(
        "/v1/messages",
        r#"{"model":"claude-opus-5-5","max_tokens":1,"messages":[]}"#,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let record = next_record(&mut proxy.events).await;
    assert_eq!(record.backend, Backend::Anthropic);
    assert_eq!(record.routed_by.as_deref(), Some("model_pattern"));
    assert_eq!(last_seen(&anthropic_seen).path, "/v1/messages");

    // An explicit prefix beats the model.
    let resp = post(
        "/anthropic/v1/messages",
        r#"{"model":"gemini-3-flash","max_tokens":1,"messages":[]}"#,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let record = next_record(&mut proxy.events).await;
    assert_eq!(record.backend, Backend::Anthropic);
    assert_eq!(record.routed_by.as_deref(), Some("prefix"));
    assert_eq!(record.path, "/anthropic/v1/messages");
    assert_eq!(
        last_seen(&anthropic_seen).path,
        "/v1/messages",
        "the prefix is stripped"
    );

    // Provider-specific path families.
    let resp = client
        .get(format!("{}/api/ps", proxy.url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let record = next_record(&mut proxy.events).await;
    assert_eq!(
        (record.backend, record.api_format),
        (Backend::Ollama, ApiFormat::Passthrough)
    );
    assert_eq!(record.routed_by.as_deref(), Some("path"));
    assert_eq!(last_seen(&ollama_seen).path, "/api/ps");
    let resp = client
        .get(format!("{}/v1beta/models", proxy.url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let record = next_record(&mut proxy.events).await;
    assert_eq!(record.backend, Backend::Gemini);
    assert!(record.is_cloud);
    assert_eq!(last_seen(&gemini_seen).path, "/v1beta/models");

    // A model-less request with a provider header.
    let resp = client
        .get(format!("{}/v1/models", proxy.url))
        .header("anthropic-version", "2023-06-01")
        .header("x-api-key", "sk-ant-test")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let record = next_record(&mut proxy.events).await;
    assert_eq!(record.backend, Backend::Anthropic);
    assert_eq!(record.routed_by.as_deref(), Some("header"));
    assert_eq!(last_seen(&anthropic_seen).path, "/v1/models");

    // The merged model list, answered by the gateway and not recorded.
    let resp = client
        .get(format!("{}/v1/models", proxy.url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert!(resp.headers().contains_key("x-request-id"));
    let list: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(list["object"], "list");
    let ids: Vec<(String, String)> = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["id"].as_str().unwrap().to_string(),
                m["owned_by"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert!(
        ids.contains(&("qwen/qwen3.6-27b".to_string(), "lmstudio".to_string())),
        "{ids:?}"
    );
    assert!(
        ids.contains(&("gemma4:12b-mlx".to_string(), "ollama".to_string())),
        "{ids:?}"
    );
    assert!(
        ids.contains(&("fast".to_string(), "alias".to_string())),
        "{ids:?}"
    );
    assert!(no_record_within(&mut proxy.events, Duration::from_millis(300)).await);

    // An alias is rewritten for the target but recorded as the client spelled it.
    let resp = post("/v1/chat/completions", r#"{"model":"fast","messages":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let record = next_record(&mut proxy.events).await;
    assert_eq!(record.backend, Backend::Ollama);
    assert_eq!(record.routed_by.as_deref(), Some("alias"));
    assert_eq!(record.model_requested.as_deref(), Some("fast"));
    assert_eq!(
        last_seen(&ollama_seen).model.as_deref(),
        Some("gemma4:12b-mlx")
    );

    // An Ollama cloud model is egress through Ollama.
    let resp = post(
        "/v1/chat/completions",
        r#"{"model":"deepseek-v4-pro:cloud","messages":[]}"#,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let record = next_record(&mut proxy.events).await;
    assert_eq!(record.backend, Backend::Ollama);
    assert!(record.is_cloud);

    // Nothing decides: the default backend.
    let resp = post(
        "/v1/chat/completions",
        r#"{"model":"llama-9","messages":[]}"#,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let record = next_record(&mut proxy.events).await;
    assert_eq!(record.backend, Backend::LmStudio);
    assert_eq!(record.routed_by.as_deref(), Some("default"));

    // A local target outside the inference formats: forwarded as passthrough, body and all.
    let resp = post("/api/show", r#"{"name":"gemma4:12b-mlx"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let record = next_record(&mut proxy.events).await;
    assert_eq!(
        (record.backend, record.api_format),
        (Backend::Ollama, ApiFormat::Passthrough)
    );
    assert_eq!(last_seen(&ollama_seen).path, "/api/show");
    let resp = post("/v1/files", r#"{"purpose":"fine-tune"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let record = next_record(&mut proxy.events).await;
    assert_eq!(
        (record.backend, record.api_format),
        (Backend::LmStudio, ApiFormat::Passthrough)
    );
    assert_eq!(record.routed_by.as_deref(), Some("default"));
    assert_eq!(last_seen(&lms_seen).path, "/v1/files");

    // A cloud POST outside the inference formats is scanned cloud passthrough once the model says where.
    let resp = post(
        "/v1/messages/count_tokens",
        r#"{"model":"claude-opus-5-5","messages":[{"role":"user","content":"hi"}]}"#,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let record = next_record(&mut proxy.events).await;
    assert_eq!(
        (record.backend, record.api_format),
        (Backend::Anthropic, ApiFormat::CloudPassthrough)
    );
    assert!(record.is_cloud);
    assert_eq!(last_seen(&anthropic_seen).path, "/v1/messages/count_tokens");

    // `GET /v1/models/{id}` resolves the id like a model.
    let resp = client
        .get(format!("{}/v1/models/gemma4:12b-mlx", proxy.url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let record = next_record(&mut proxy.events).await;
    assert_eq!(record.backend, Backend::Ollama);
    assert_eq!(record.routed_by.as_deref(), Some("model_list"));
    assert_eq!(last_seen(&ollama_seen).path, "/v1/models/gemma4:12b-mlx");

    // Refusals: a format the provider lacks, another provider's header, a backend this listener lacks.
    let resp = post(
        "/v1/embeddings",
        r#"{"model":"claude-opus-5-5","input":"x"}"#,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "format_not_served", "{body}");
    let record = next_record(&mut proxy.events).await;
    assert_eq!(record.backend, Backend::Anthropic);
    assert_eq!(
        record.outcome,
        Outcome::Failed {
            status: 400,
            source: FailureSource::Gateway
        }
    );
    let resp = post(
        "/v1/chat/completions",
        r#"{"model":"gemini-3-flash","messages":[]}"#,
    )
    .header("anthropic-version", "2023-06-01")
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "credential_mismatch", "{body}");
    let _ = next_record(&mut proxy.events).await;
    let resp = post(
        "/openai/v1/chat/completions",
        r#"{"model":"gpt-5","messages":[]}"#,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "backend_disabled", "{body}");
    let record = next_record(&mut proxy.events).await;
    assert_eq!(record.backend, Backend::OpenAi);
    assert_eq!(record.routed_by.as_deref(), Some("prefix"));
    assert_eq!(proxy.ctx.inflight_count(), 0);
}

#[tokio::test]
async fn upgrades_to_cloud_providers_are_refused_on_the_unified_listener() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (anthropic_router, anthropic_seen) = recording_mock("anthropic");
    let targets = BTreeMap::from([
        (Backend::LmStudio, spawn_upstream(Router::new()).await),
        (Backend::Anthropic, spawn_upstream(anthropic_router).await),
    ]);
    let mut proxy = start_unified(targets, RoutingTable::default(), Options::default());
    let addr = proxy.url.trim_start_matches("http://").to_string();
    let mut stream = tokio::net::TcpStream::connect(&addr).await.unwrap();
    stream
        .write_all(
            b"GET /anthropic/v1/ws HTTP/1.1\r\nHost: gateway\r\nConnection: Upgrade, close\r\nUpgrade: websocket\r\n\
              Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
        )
        .await
        .unwrap();
    let mut raw = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut raw))
        .await
        .expect("the gateway answers and closes")
        .unwrap();
    let text = String::from_utf8_lossy(&raw);
    assert!(text.starts_with("HTTP/1.1 400"), "{text}");
    assert!(
        text.contains("a tunnel cannot be scanned"),
        "Anthropic's error shape carries the message, not a code: {text}"
    );
    assert!(text.contains(r#""type":"error""#), "{text}");
    let record = next_record(&mut proxy.events).await;
    assert_eq!(record.backend, Backend::Anthropic);
    assert_eq!(
        record.outcome,
        Outcome::Failed {
            status: 400,
            source: FailureSource::Gateway
        }
    );
    assert!(
        anthropic_seen.lock().unwrap().is_empty(),
        "nothing left the machine"
    );
}

/// What the LM Studio poller publishes for a loaded model: enough for the context-length fallback.
fn snapshot_with_loaded_model(id: &str, context: u64) -> BackendSnapshot {
    BackendSnapshot {
        backend: Backend::LmStudio,
        reachable: true,
        version: None,
        error: None,
        models: vec![ModelInfo {
            backend: Backend::LmStudio,
            id: id.to_string(),
            kind: Some("vlm".into()),
            compatibility_type: Some("mlx".into()),
            quantization: None,
            max_context_length: Some(262_144),
            loaded_context_length: Some(context),
            state: ModelState::Loaded,
            publisher: None,
            arch: None,
            capabilities: Vec::new(),
            size_bytes: None,
            size_vram_bytes: None,
            expires_at: None,
            remote_host: None,
        }],
        polled_at: chrono::Utc::now(),
    }
}

#[tokio::test]
async fn an_lm_studio_overflow_500_is_a_rejection_with_the_loaded_context() {
    // LM Studio 0.4.25 answers an oversized /v1/messages prompt with 500 and a message that carries no
    // numbers; the record keeps the real status and takes the context length from the model list.
    const BODY: &str =
        include_str!("../../../fixtures/lmstudio/v1-messages-500-context-overflow.json");
    let mock = Router::new().route(
        "/v1/messages",
        post(|| async {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
                BODY,
            )
        }),
    );
    let mut proxy = spawn_proxy(
        mock,
        Options {
            backend: Backend::LmStudio,
            ..Default::default()
        },
    )
    .await;
    proxy
        .ctx
        .backends
        .set(snapshot_with_loaded_model("laig-overflow-test", 2048))
        .await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", proxy.url))
        .body(r#"{"model":"laig-overflow-test","max_tokens":1,"messages":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(resp.text().await.unwrap(), BODY);
    let record = next_record(&mut proxy.events).await;
    assert_eq!(record.status, Some(500), "the status the client received");
    assert_eq!(
        record.outcome,
        Outcome::Rejected {
            input_tokens: None,
            context_length: Some(2048)
        }
    );
    assert_eq!(
        record.outcome.summary().as_deref(),
        Some("rejected (ctx 2048)")
    );
    assert!(
        record
            .error_summary
            .as_deref()
            .unwrap()
            .starts_with("The number of tokens to keep")
    );
}

#[tokio::test]
async fn an_error_event_in_a_responses_stream_is_a_rejection_with_the_loaded_context() {
    const STREAM: &str =
        include_str!("../../../fixtures/lmstudio/v1-responses-sse-context-overflow.txt");
    let mock = Router::new().route(
        "/v1/responses",
        post(|| async { ([(header::CONTENT_TYPE, "text/event-stream")], STREAM) }),
    );
    let mut proxy = spawn_proxy(
        mock,
        Options {
            backend: Backend::LmStudio,
            ..Default::default()
        },
    )
    .await;
    proxy
        .ctx
        .backends
        .set(snapshot_with_loaded_model("laig-overflow-test", 2048))
        .await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/responses", proxy.url))
        .body(
            r#"{"model":"laig-overflow-test","input":"hello","max_output_tokens":1,"stream":true}"#,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::OK,
        "the status was already sent"
    );
    assert_eq!(
        resp.text().await.unwrap(),
        STREAM,
        "bytes pass through untouched"
    );
    let record = next_record(&mut proxy.events).await;
    assert_eq!(record.status, Some(200));
    assert_eq!(
        record.outcome,
        Outcome::Rejected {
            input_tokens: None,
            context_length: Some(2048)
        }
    );
    assert!(
        record
            .error_summary
            .as_deref()
            .unwrap()
            .starts_with("The number of tokens to keep")
    );
}

#[tokio::test]
async fn client_abort_after_finish_reason_still_captures_the_usage_trailer() {
    let mock = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            let frames = futures_util::stream::unfold(0u8, |step| async move {
                match step {
                    0 => Some((Ok::<Bytes, std::io::Error>(Bytes::from("data: {\"model\":\"m\",\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n")), 1)),
                    1 => Some((Ok(Bytes::from("data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n")), 2)),
                    2 => {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        Some((Ok(Bytes::from("data: {\"choices\":[],\"usage\":{\"prompt_tokens\":42,\"completion_tokens\":7}}\n\ndata: [DONE]\n\n")), 3))
                    }
                    _ => None,
                }
            });
            ([(header::CONTENT_TYPE, "text/event-stream")], Body::from_stream(frames))
        }),
    );
    let mut proxy = spawn_proxy(mock, Options::default()).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", proxy.url))
        .body(r#"{"model":"m","messages":[],"stream":true}"#)
        .send()
        .await
        .unwrap();
    let mut stream = resp.bytes_stream();
    let mut seen = Vec::new();
    while let Some(Ok(chunk)) = stream.next().await {
        seen.extend_from_slice(&chunk);
        if String::from_utf8_lossy(&seen).contains("finish_reason\":\"stop\"") {
            break;
        }
    }
    drop(stream); // the agent-client hang-up
    let record = next_record(&mut proxy.events).await;
    assert_eq!(
        record.outcome,
        Outcome::Completed,
        "finished before the hang-up"
    );
    assert_eq!(
        (record.prompt_tokens, record.gen_tokens),
        (Some(42), Some(7))
    );
    assert!(!record.tokens_estimated, "the trailer was drained");
}

#[tokio::test]
async fn client_abort_mid_generation_is_recorded_as_aborted_without_waiting() {
    let mock = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            let frames = futures_util::stream::unfold(0u8, |step| async move {
                match step {
                    0 => Some((Ok::<Bytes, std::io::Error>(Bytes::from("data: {\"model\":\"m\",\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n")), 1)),
                    _ => {
                        std::future::pending::<()>().await;
                        None
                    }
                }
            });
            ([(header::CONTENT_TYPE, "text/event-stream")], Body::from_stream(frames))
        }),
    );
    let mut proxy = spawn_proxy(mock, Options::default()).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", proxy.url))
        .body(r#"{"model":"m","messages":[],"stream":true}"#)
        .send()
        .await
        .unwrap();
    let mut stream = resp.bytes_stream();
    let _ = stream.next().await.unwrap().unwrap();
    drop(stream);
    let started = tokio::time::Instant::now();
    let record = next_record(&mut proxy.events).await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "no trailer drain mid-generation"
    );
    assert_eq!(record.outcome, Outcome::ClientAborted);
    assert!(record.tokens_estimated);
    assert_eq!(record.gen_tokens, Some(1));
}

#[tokio::test]
async fn websocket_upgrades_are_tunnelled_bidirectionally() {
    let mock = Router::new().route(
        "/ws",
        any(|ws: WebSocketUpgrade| async move {
            ws.on_upgrade(|mut socket| async move {
                while let Some(Ok(message)) = socket.recv().await {
                    if let Message::Text(text) = message {
                        let reply = format!("echo: {text}");
                        if socket.send(Message::Text(reply.into())).await.is_err() {
                            break;
                        }
                    }
                }
            })
        }),
    );
    let mut proxy = spawn_proxy(
        mock,
        Options {
            backend: Backend::LmStudio,
            ..Default::default()
        },
    )
    .await;
    let ws_url = format!("{}/ws", proxy.url.replacen("http://", "ws://", 1));
    let (mut socket, response) = tokio_tungstenite::connect_async(&ws_url)
        .await
        .expect("handshake through the proxy");
    assert_eq!(response.status().as_u16(), 101);
    assert!(response.headers().contains_key("x-request-id"));
    assert_eq!(
        response.headers().get(GATEWAY_HEADER).unwrap(),
        GATEWAY_VERSION,
        "the 101 of a tunnel names the gateway too"
    );
    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(
            "hello".into(),
        ))
        .await
        .unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(reply.into_text().unwrap(), "echo: hello");
    socket.close(None).await.unwrap();
    drop(socket);
    let record = next_record(&mut proxy.events).await;
    assert_eq!(record.kind, RequestKind::Upgrade);
    assert_eq!(record.status, Some(101));
    assert_eq!(record.path, "/ws");
}

struct BlockAkia {
    calls: AtomicUsize,
}

impl RequestScanner for BlockAkia {
    fn scan_request(&self, _ctx: &ScanContext, body: &[u8]) -> ScanOutcome {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut outcome = ScanOutcome::clean();
        if body.windows(4).any(|w| w == b"AKIA") {
            outcome.decision = Decision::Block {
                rule_id: "aws-access-key-id".into(),
            };
            outcome.redacted = Some(
                String::from_utf8_lossy(body)
                    .replace("AKIAIOSFODNN7EXAMPLE", "[REDACTED]")
                    .into_bytes(),
            );
        }
        outcome
    }

    fn scan_response(&self, _ctx: &ScanContext, _text: &str, _raw: &[u8]) -> ScanOutcome {
        ScanOutcome::clean()
    }
}

#[tokio::test]
async fn a_blocked_request_gets_403_and_never_reaches_the_backend() {
    let hits = Arc::new(AtomicUsize::new(0));
    let hits2 = hits.clone();
    let mock = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let hits = hits2.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                "should not be reached"
            }
        }),
    );
    let dir = tempfile::tempdir().unwrap();
    let store = gateway_core::store::open(&dir.path().join("t.db"))
        .await
        .unwrap();
    let session_id = store.writer.start_session("test").await.unwrap();
    let mut proxy = spawn_proxy(
        mock,
        Options {
            store: Some(store.writer.clone()),
            session_id,
            scanner: Some(Arc::new(BlockAkia {
                calls: AtomicUsize::new(0),
            })),
            ..Default::default()
        },
    )
    .await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", proxy.url))
        .body(r#"{"model":"m","messages":[{"role":"user","content":"my key is AKIAIOSFODNN7EXAMPLE"}]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);
    assert_eq!(resp.headers().get("x-gateway-blocked").unwrap(), "dlp");
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "dlp_blocked");
    let record = next_record(&mut proxy.events).await;
    assert_eq!(
        record.outcome,
        Outcome::Blocked {
            rule_id: "aws-access-key-id".into()
        }
    );
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "the backend never saw the request"
    );
    // The stored copy is the redacted one.
    store.writer.checkpoint().await.unwrap();
    let id = record.id;
    let stored = store
        .reads
        .run(move |c| gateway_core::store::read::get_body(c, id, Direction::Request))
        .await
        .unwrap()
        .expect("body stored");
    assert!(stored.text.contains("[REDACTED]"));
    assert!(!stored.text.contains("AKIAIOSFODNN7EXAMPLE"));
    assert!(stored.redacted);
}

#[tokio::test]
async fn bodies_are_captured_and_stored_for_a_streamed_exchange() {
    let mock = Router::new().route(
        "/v1/chat/completions",
        post(|| async { ([(header::CONTENT_TYPE, "text/event-stream")], OPENAI_SSE) }),
    );
    let dir = tempfile::tempdir().unwrap();
    let store = gateway_core::store::open(&dir.path().join("t.db"))
        .await
        .unwrap();
    let session_id = store.writer.start_session("test").await.unwrap();
    let mut proxy = spawn_proxy(
        mock,
        Options {
            store: Some(store.writer.clone()),
            session_id,
            ..Default::default()
        },
    )
    .await;
    let request_body =
        r#"{"model":"qwen3:14b","messages":[{"role":"user","content":"say hi"}],"stream":true}"#;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", proxy.url))
        .header("authorization", "Bearer sk-secret")
        .body(request_body)
        .send()
        .await
        .unwrap();
    let _ = resp.text().await.unwrap();
    let record = next_record(&mut proxy.events).await;
    store.writer.checkpoint().await.unwrap();
    let id = record.id;
    let detail = store
        .reads
        .run(move |c| gateway_core::store::read::get_request(c, id))
        .await
        .unwrap()
        .expect("stored");
    assert_eq!(detail.record.request_bytes, request_body.len() as u64);
    assert!(
        detail
            .record
            .request_headers
            .iter()
            .any(|(k, v)| k == "authorization" && v == "[REDACTED]")
    );
    assert!(detail.request_body.is_some());
    assert_eq!(
        detail
            .response_body
            .as_ref()
            .unwrap()
            .content_type
            .as_deref(),
        Some("text/event-stream")
    );
    let request = store
        .reads
        .run(move |c| gateway_core::store::read::get_body(c, id, Direction::Request))
        .await
        .unwrap()
        .unwrap();
    assert!(request.text.contains("say hi"));
    let response = store
        .reads
        .run(move |c| gateway_core::store::read::get_body(c, id, Direction::Response))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        response.text, OPENAI_SSE,
        "the raw stream is what the inspector keeps"
    );
}

#[tokio::test]
async fn passthrough_recording_can_be_switched_off() {
    let mock = Router::new().route("/api/tags", get(|| async { r#"{"models":[]}"# }));
    let mut proxy = spawn_proxy(
        mock,
        Options {
            record_passthrough: false,
            ..Default::default()
        },
    )
    .await;
    let resp = reqwest::Client::new()
        .get(format!("{}/api/tags", proxy.url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert!(resp.headers().contains_key("x-request-id"));
    let _ = resp.text().await.unwrap();
    assert!(no_record_within(&mut proxy.events, Duration::from_millis(300)).await);
    assert_eq!(proxy.ctx.inflight_count(), 0);
}

#[tokio::test]
async fn two_listeners_route_to_their_own_upstreams() {
    let lms = spawn_upstream(Router::new().route("/api/v0/models", get(|| async { "lms" }))).await;
    let ollama = spawn_upstream(Router::new().route("/api/tags", get(|| async { "ollama" }))).await;
    let lms_proxy = start_proxy(
        lms,
        Options {
            backend: Backend::LmStudio,
            ..Default::default()
        },
    );
    let ollama_proxy = start_proxy(ollama, Options::default());
    let client = reqwest::Client::new();
    assert_eq!(
        client
            .get(format!("{}/api/v0/models", lms_proxy.url))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "lms"
    );
    assert_eq!(
        client
            .get(format!("{}/api/tags", ollama_proxy.url))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "ollama"
    );
    assert_eq!(
        client
            .get(format!("{}/api/tags", lms_proxy.url))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn the_timeline_and_first_byte_are_recorded_for_streams() {
    let mock = Router::new().route(
        "/api/chat",
        post(|| async {
            (
                [(header::CONTENT_TYPE, "application/x-ndjson")],
                OLLAMA_STREAM,
            )
        }),
    );
    let mut proxy = spawn_proxy(mock, Options::default()).await;
    let _ = reqwest::Client::new()
        .post(format!("{}/api/chat", proxy.url))
        .body(r#"{"model":"qwen3:14b","messages":[]}"#)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let record = next_record(&mut proxy.events).await;
    assert!(record.first_byte_at.is_some());
    assert!(!record.chunk_timeline.is_empty());
    assert_eq!(
        record
            .chunk_timeline
            .iter()
            .map(|(_, b)| u64::from(*b))
            .sum::<u64>(),
        record.response_bytes
    );
}
