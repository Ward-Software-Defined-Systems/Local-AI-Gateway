//! End-to-end proxy tests over real sockets: a mock backend on `127.0.0.1:0`, a listener in front of it,
//! reqwest as the client, and the event bus as the witness. Ported from Ollama-Monitor's `proxy.rs` tests
//! and extended for the second backend, the tunnel, the DLP hook and the inspector's capture.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::ws::{Message, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode, header};
use axum::routing::{any, get, post};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use gateway_core::events::{EventReceiver, GatewayEvent};
use gateway_core::pricing::PricingTable;
use gateway_core::state::BackendsState;
use gateway_core::types::{
    ApiFormat, Backend, Direction, FailureSource, Outcome, RequestKind, RequestRecord,
};
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::listener::{CapturePolicy, ListenerConfig, ProxyContext, bind, serve};
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
        backend: opts.backend,
        listen: listener.local_addr().unwrap(),
        upstream,
        capture_bodies: opts.capture_bodies,
        record_passthrough: opts.record_passthrough,
        rewrite_include_usage: opts.rewrite_include_usage,
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
