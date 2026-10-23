//! The request pipeline. Every request gets an id and an in-flight entry. Upgrades go to the tunnel.
//! Passthrough requests stream both ways unread and are recorded as metadata when the response ends.
//! Inference requests are buffered (capped), scanned, optionally rewritten and forwarded; a 4xx/5xx answer
//! is forwarded unchanged and recorded as failed (or rejected, for LM Studio's context overflow); a 2xx
//! answer is teed through the format's parser and captured while it streams to the client. The driver task
//! that owns the upstream stream is what makes client aborts propagate (dropping the stream cancels the
//! generation) and what keeps draining for the usage trailer when a client hangs up right after the
//! finish frame, which agent clients routinely do.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, Instant};

use axum::body::{Body, HttpBody};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures_util::{Stream, StreamExt};
use gateway_core::events::{GatewayEvent, publish};
use gateway_core::types::{
    ApiFormat, Backend, Body as StoredBody, Direction, FailureSource, Finding, InflightRequest,
    ModelInfo, Outcome, RequestKind, RequestRecord, Severity,
};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::capture::{BodyCapture, Timeline};
use crate::error::ProxyError;
use crate::http;
use crate::listener::ProxyState;
use crate::parsers::{self, StreamParser, WallClock};
use crate::rejection;
use crate::router;
use crate::scan::{Decision, ScanContext, ScanOutcome};

/// How long to keep draining upstream for the trailing usage frame after the client hangs up on a stream
/// that has already finished. The `:cloud` relay delivers it within ~100 ms of the finish frame.
pub const USAGE_TRAILER_GRACE: Duration = Duration::from_secs(3);
/// An error body is read up to this before being forwarded.
const ERROR_BODY_CAP: usize = 64 * 1024;
/// The channel between the driver task and the client body: its bound is the backpressure.
const CLIENT_CHANNEL: usize = 64;

/// Everything known about a request before its response.
#[derive(Debug, Clone)]
pub struct RequestMeta {
    pub id: Uuid,
    pub backend: Backend,
    pub listener: String,
    pub client_ip: String,
    pub user_agent: Option<String>,
    pub client_request_id: Option<String>,
    pub method: String,
    pub path: String,
    pub query: Option<String>,
    pub api_format: ApiFormat,
    pub started: Instant,
    pub started_at: DateTime<Utc>,
    pub request_headers: Vec<(String, String)>,
    pub request_content_type: Option<String>,
    pub request_bytes: u64,
    pub model_requested: Option<String>,
    pub is_cloud: bool,
    pub streamed: bool,
}

impl RequestMeta {
    fn new(st: &ProxyState, addr: SocketAddr, req: &Request) -> Self {
        let headers = req.headers();
        let header_str = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        Self {
            id: Uuid::now_v7(),
            backend: st.listener.backend,
            listener: st.listener.listen.to_string(),
            client_ip: addr.ip().to_string(),
            user_agent: header_str("user-agent"),
            client_request_id: header_str("x-request-id"),
            method: req.method().to_string(),
            path: req.uri().path().to_string(),
            query: req.uri().query().map(str::to_string),
            api_format: router::classify(st.listener.backend, req.method(), req.uri().path()),
            started: Instant::now(),
            started_at: Utc::now(),
            request_headers: http::redact_headers(headers),
            request_content_type: header_str("content-type"),
            request_bytes: 0,
            model_requested: None,
            is_cloud: false,
            streamed: false,
        }
    }

    pub fn path_and_query(&self) -> String {
        match &self.query {
            Some(q) => format!("{}?{q}", self.path),
            None => self.path.clone(),
        }
    }

    fn inflight(&self, kind: RequestKind) -> InflightRequest {
        InflightRequest {
            id: self.id,
            backend: self.backend,
            api_format: self.api_format,
            kind,
            method: self.method.clone(),
            path: self.path.clone(),
            model_requested: self.model_requested.clone(),
            is_cloud: self.is_cloud,
            client_ip: self.client_ip.clone(),
            user_agent: self.user_agent.clone(),
            started_at: self.started_at,
        }
    }
}

pub async fn handle(
    State(st): State<ProxyState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: Request,
) -> Response {
    let meta = RequestMeta::new(&st, addr, &req);
    debug!(request = %meta.id, backend = %meta.backend, method = %meta.method, path = %meta.path, format = %meta.api_format, "request received");
    if router::is_upgrade(req.headers()) {
        st.ctx.inflight_insert(meta.inflight(RequestKind::Upgrade));
        publish(
            &st.ctx.events,
            GatewayEvent::RequestStarted {
                request: meta.inflight(RequestKind::Upgrade),
            },
        );
        return crate::tunnel::handle(st, meta, req).await;
    }
    if meta.api_format.is_inference() {
        inference(st, meta, req).await
    } else {
        passthrough(st, meta, req).await
    }
}

// ---------------------------------------------------------------------------------------------------------
// passthrough
// ---------------------------------------------------------------------------------------------------------

async fn passthrough(st: ProxyState, meta: RequestMeta, req: Request) -> Response {
    st.ctx.inflight_insert(meta.inflight(RequestKind::Http));
    publish(
        &st.ctx.events,
        GatewayEvent::RequestStarted {
            request: meta.inflight(RequestKind::Http),
        },
    );
    let (parts, body) = req.into_parts();
    let streamed = !body.is_end_stream();
    let upstream_body = if streamed {
        reqwest::Body::wrap_stream(body.into_data_stream())
    } else {
        reqwest::Body::from(Bytes::new())
    };
    let url = format!("{}{}", st.listener.upstream, meta.path_and_query());
    // A streamed body is forwarded unchanged, so its declared length still holds.
    let mut headers = http::forwardable(&parts.headers, !streamed);
    headers.insert("x-request-id", request_id_value(meta.id));
    let upstream = match st
        .ctx
        .client
        .request(parts.method.clone(), &url)
        .headers(headers)
        .body(upstream_body)
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(err) => {
            warn!(request = %meta.id, %url, error = %err, "upstream request failed");
            let err = ProxyError::UpstreamUnreachable;
            if st.listener.record_passthrough {
                finish_simple(
                    &st,
                    meta.clone(),
                    RequestKind::Http,
                    Some(502),
                    Outcome::Failed {
                        status: 502,
                        source: FailureSource::Gateway,
                    },
                    Some(err.to_string()),
                    0,
                )
                .await;
            } else {
                st.ctx.inflight_remove(meta.id);
            }
            return gateway_error_response(&meta, &err);
        }
    };
    let status = upstream.status();
    let resp_headers = upstream.headers().clone();
    let record = st.listener.record_passthrough;
    let st2 = st.clone();
    let meta2 = meta.clone();
    let tracked = Tracked::new(
        Box::pin(upstream.bytes_stream()),
        Box::new(move |bytes, complete| {
            tokio::spawn(async move {
                if !record {
                    st2.ctx.inflight_remove(meta2.id);
                    return;
                }
                let outcome = if status.is_client_error() || status.is_server_error() {
                    Outcome::Failed {
                        status: status.as_u16(),
                        source: FailureSource::Upstream,
                    }
                } else if complete {
                    Outcome::Completed
                } else {
                    Outcome::ClientAborted
                };
                finish_simple(
                    &st2,
                    meta2,
                    RequestKind::Http,
                    Some(status.as_u16()),
                    outcome,
                    None,
                    bytes,
                )
                .await;
            });
        }),
    );
    build_response(&meta, status, &resp_headers, Body::from_stream(tracked))
}

/// A byte-counting wrapper that reports how a response body ended: `complete` when upstream finished,
/// otherwise (an upstream error or the client dropping the body) not.
struct Tracked {
    inner: Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>,
    bytes: u64,
    on_end: Option<Box<dyn FnOnce(u64, bool) + Send>>,
}

impl Tracked {
    fn new(
        inner: Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>,
        on_end: Box<dyn FnOnce(u64, bool) + Send>,
    ) -> Self {
        Self {
            inner,
            bytes: 0,
            on_end: Some(on_end),
        }
    }

    fn end(&mut self, complete: bool) {
        if let Some(on_end) = self.on_end.take() {
            on_end(self.bytes, complete);
        }
    }
}

impl Stream for Tracked {
    type Item = Result<Bytes, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        match self.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                self.bytes += bytes.len() as u64;
                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(Some(Err(err))) => {
                warn!(error = %err, "upstream stream error mid-response (passthrough)");
                self.end(false);
                Poll::Ready(None)
            }
            Poll::Ready(None) => {
                self.end(true);
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for Tracked {
    fn drop(&mut self) {
        self.end(false);
    }
}

// ---------------------------------------------------------------------------------------------------------
// inference
// ---------------------------------------------------------------------------------------------------------

async fn inference(st: ProxyState, mut meta: RequestMeta, req: Request) -> Response {
    let policy = st.ctx.capture();
    let (parts, body) = req.into_parts();
    let body = match http::read_capped(body, policy.max_buffered_request_bytes).await {
        Ok(body) => body,
        Err(err) => {
            warn!(request = %meta.id, error = %err, "request body refused");
            st.ctx.inflight_insert(meta.inflight(RequestKind::Http));
            return fail_gateway(&st, meta, err, None, Vec::new()).await;
        }
    };
    meta.request_bytes = body.len() as u64;
    let fields = http::request_fields(meta.api_format, &body);
    meta.model_requested = fields.model.clone();
    meta.streamed = fields.streamed;
    meta.is_cloud = match (&meta.model_requested, meta.backend) {
        (Some(model), Backend::Ollama) => {
            gateway_backends::ollama::is_cloud_name(model)
                || st.ctx.backends.is_cloud_model(Backend::Ollama, model).await
        }
        _ => false,
    };
    st.ctx.inflight_insert(meta.inflight(RequestKind::Http));
    publish(
        &st.ctx.events,
        GatewayEvent::RequestStarted {
            request: meta.inflight(RequestKind::Http),
        },
    );

    let scan_ctx = ScanContext {
        request_id: meta.id,
        backend: meta.backend,
        api_format: meta.api_format,
        model: meta.model_requested.clone(),
        is_cloud: meta.is_cloud,
        direction: Direction::Request,
    };
    let scan = st.ctx.scanner().scan_request(&scan_ctx, &body);
    let stored_request = st
        .listener
        .capture_bodies
        .then(|| match scan.redacted.clone() {
            Some(redacted) => (redacted, true),
            None => (body.to_vec(), false),
        });
    if let Decision::Block { rule_id } = &scan.decision {
        info!(request = %meta.id, rule = %rule_id, backend = %meta.backend, cloud = meta.is_cloud, "request blocked by DLP policy");
        let err = ProxyError::Blocked {
            rule_id: rule_id.clone(),
        };
        let response = gateway_error_response(&meta, &err);
        finish_failed(
            &st,
            meta,
            Failure {
                status: err.status().as_u16(),
                outcome: Outcome::Blocked {
                    rule_id: rule_id.clone(),
                },
                error_summary: Some(err.to_string()),
            },
            stored_request,
            None,
            scan.findings,
        )
        .await;
        return response;
    }

    let forward_body = if st.listener.rewrite_include_usage
        && matches!(
            meta.api_format,
            ApiFormat::OpenAiChat | ApiFormat::OpenAiCompletions | ApiFormat::LmStudioNative
        )
        && fields.streamed
    {
        http::ensure_include_usage(body.clone())
    } else {
        body.clone()
    };
    let url = format!("{}{}", st.listener.upstream, meta.path_and_query());
    let mut headers = http::forwardable(&parts.headers, true);
    headers.insert("x-request-id", request_id_value(meta.id));
    let upstream = match st
        .ctx
        .client
        .request(parts.method.clone(), &url)
        .headers(headers)
        .body(forward_body)
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(err) => {
            warn!(request = %meta.id, %url, error = %err, "upstream request failed");
            return fail_gateway(
                &st,
                meta,
                ProxyError::UpstreamUnreachable,
                stored_request,
                scan.findings,
            )
            .await;
        }
    };
    let status = upstream.status();
    let resp_headers = upstream.headers().clone();
    let content_type = resp_headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    if status.is_client_error() || status.is_server_error() {
        let error_body = collect_capped(upstream.bytes_stream(), ERROR_BODY_CAP).await;
        let summary = rejection::error_summary(&error_body);
        // LM Studio refuses an oversized prompt with 400 on its chat and completion routes and 500 on
        // /v1/messages and /v1/responses; the message decides, not the status.
        let overflow = (meta.backend == Backend::LmStudio)
            .then(|| summary.as_deref().and_then(rejection::parse_overflow))
            .flatten();
        let outcome = match overflow {
            Some((input_tokens, context_length)) => {
                let models = st.ctx.backends.models(meta.backend).await;
                Outcome::Rejected {
                    input_tokens,
                    context_length: context_length.or_else(|| {
                        loaded_context_length(&models, meta.model_requested.as_deref())
                    }),
                }
            }
            None => Outcome::Failed {
                status: status.as_u16(),
                source: FailureSource::Upstream,
            },
        };
        let stored_response = st.listener.capture_bodies.then(|| StoredBody {
            request_id: meta.id,
            direction: Direction::Response,
            content_type: content_type.clone(),
            original_bytes: error_body.len() as u64,
            stored_bytes: error_body.len() as u64,
            truncated: false,
            redacted: false,
            is_text: std::str::from_utf8(&error_body).is_ok(),
            text: String::from_utf8_lossy(&error_body).into_owned(),
        });
        let response = build_response(&meta, status, &resp_headers, Body::from(error_body));
        finish_failed(
            &st,
            meta,
            Failure {
                status: status.as_u16(),
                outcome,
                error_summary: summary,
            },
            stored_request,
            stored_response,
            scan.findings,
        )
        .await;
        return response;
    }

    let parser = parsers::select(meta.api_format, content_type.as_deref(), fields.streamed);
    let body = match parser {
        Some(parser) => {
            let finisher = Finisher {
                st: st.clone(),
                meta: meta.clone(),
                status: status.as_u16(),
                response_headers: http::redact_headers(&resp_headers),
                response_content_type: content_type,
                stored_request,
                request_findings: scan.findings,
                parser,
                capture: BodyCapture::new(policy.max_body_bytes),
                timeline: Timeline::new(meta.started),
                capture_bodies: st.listener.capture_bodies,
            };
            drive(upstream.bytes_stream(), finisher)
        }
        None => Body::from_stream(
            upstream
                .bytes_stream()
                .map(|chunk| chunk.map_err(|e| e.to_string()))
                .map(|r| r.map_err(axum::Error::new)),
        ),
    };
    build_response(&meta, status, &resp_headers, body)
}

/// Reads at most `cap` bytes of a body and drops the rest.
async fn collect_capped<S>(mut stream: S, cap: usize) -> Vec<u8>
where
    S: Stream<Item = reqwest::Result<Bytes>> + Unpin,
{
    let mut out = Vec::new();
    while let Some(Ok(chunk)) = stream.next().await {
        if out.len() + chunk.len() > cap {
            out.extend_from_slice(&chunk[..cap - out.len()]);
            break;
        }
        out.extend_from_slice(&chunk);
    }
    out
}

/// What the driver task needs to finish a 2xx inference response.
struct Finisher {
    st: ProxyState,
    meta: RequestMeta,
    status: u16,
    response_headers: Vec<(String, String)>,
    response_content_type: Option<String>,
    stored_request: Option<(Vec<u8>, bool)>,
    request_findings: Vec<Finding>,
    parser: Box<dyn StreamParser>,
    capture: BodyCapture,
    timeline: Timeline,
    capture_bodies: bool,
}

enum StreamEnd {
    UpstreamDone,
    UpstreamError(String),
    ClientGone,
}

/// Owns the upstream stream, tees every chunk into the parser, the capture and the timeline, and forwards
/// it to the client through a bounded channel so a slow client backpressures upstream reads.
fn drive<S>(stream: S, mut finisher: Finisher) -> Body
where
    S: Stream<Item = reqwest::Result<Bytes>> + Send + 'static,
{
    let (client_tx, client_rx) = mpsc::channel::<Bytes>(CLIENT_CHANNEL);
    tokio::spawn(async move {
        let mut stream = std::pin::pin!(stream);
        let mut first_byte: Option<Instant> = None;
        let end = loop {
            tokio::select! {
                _ = client_tx.closed() => break StreamEnd::ClientGone,
                chunk = stream.next() => match chunk {
                    Some(Ok(bytes)) => {
                        let now = Instant::now();
                        if first_byte.is_none() && !bytes.is_empty() {
                            first_byte = Some(now);
                            let ttft_ms = now.saturating_duration_since(finisher.meta.started).as_secs_f64() * 1000.0;
                            publish(&finisher.st.ctx.events, GatewayEvent::RequestFirstByte { id: finisher.meta.id, ttft_ms });
                        }
                        finisher.push(&bytes, now);
                        if client_tx.send(bytes).await.is_err() {
                            break StreamEnd::ClientGone;
                        }
                    }
                    Some(Err(err)) => break StreamEnd::UpstreamError(err.to_string()),
                    None => break StreamEnd::UpstreamDone,
                },
            }
        };
        // The client hung up. If only the usage trailer is outstanding, drain a little longer so the record
        // keeps real token counts. A mid-generation abort skips this on purpose: dropping the upstream
        // stream is what propagates cancellation to the backend.
        if matches!(end, StreamEnd::ClientGone) && finisher.parser.awaiting_trailer() {
            let drained = tokio::time::timeout(USAGE_TRAILER_GRACE, async {
                while finisher.parser.awaiting_trailer() {
                    match stream.next().await {
                        Some(Ok(bytes)) => finisher.push(&bytes, Instant::now()),
                        Some(Err(_)) | None => break,
                    }
                }
            })
            .await;
            debug!(request = %finisher.meta.id, drained = drained.is_ok() && !finisher.parser.awaiting_trailer(), "client left before the usage frame; drained upstream");
        }
        finisher.finish(end, first_byte).await;
    });
    Body::from_stream(
        tokio_stream::wrappers::ReceiverStream::new(client_rx).map(Ok::<Bytes, Infallible>),
    )
}

impl Finisher {
    fn push(&mut self, bytes: &Bytes, at: Instant) {
        self.parser.push(bytes);
        // Incremental response scanning arrives with the DLP engine; the deltas are drained so they do not pile up.
        let _ = self.parser.take_text_deltas();
        if self.capture_bodies {
            self.capture.push(bytes);
        } else {
            let mut counter = BodyCapture::new(0);
            counter.push(bytes);
            self.capture.push(&[]);
            self.capture_total_add(counter.total_bytes());
        }
        self.timeline.push(at, bytes.len());
    }

    fn capture_total_add(&mut self, _n: u64) {}

    async fn finish(self, end: StreamEnd, first_byte: Option<Instant>) {
        let Finisher {
            st,
            meta,
            status,
            response_headers,
            response_content_type,
            stored_request,
            request_findings,
            parser,
            capture,
            timeline,
            capture_bodies,
        } = self;
        let ended = Instant::now();
        let wall = WallClock {
            started: meta.started,
            first_byte,
            ended,
        };
        let parsed = parser.finalize(&wall);
        let (raw, response_bytes, truncated) = capture.into_bytes();
        let parsed = parsed.unwrap_or_default();
        let unfinished = parsed.stop_reason.as_deref().is_none_or(|s| s == "unknown");
        let models = st.ctx.backends.models(meta.backend).await;
        let outcome = match (&parsed.in_stream_error, &end) {
            (Some(message), _) => match (meta.backend, rejection::parse_overflow(message)) {
                (Backend::LmStudio, Some((input_tokens, context_length))) => Outcome::Rejected {
                    input_tokens,
                    context_length: context_length.or_else(|| {
                        loaded_context_length(
                            &models,
                            parsed.model.as_deref().or(meta.model_requested.as_deref()),
                        )
                    }),
                },
                _ => Outcome::Failed {
                    status,
                    source: FailureSource::Upstream,
                },
            },
            (None, StreamEnd::ClientGone) if unfinished => Outcome::ClientAborted,
            (None, StreamEnd::UpstreamError(_)) if unfinished => Outcome::Failed {
                status,
                source: FailureSource::Upstream,
            },
            _ => Outcome::Completed,
        };
        let error_summary = parsed.in_stream_error.clone().or_else(|| match &end {
            StreamEnd::UpstreamError(err) => {
                Some(rejection::truncate(err, rejection::ERROR_MESSAGE_MAX).to_string())
            }
            _ => None,
        });
        let kind = if parsed.is_load_or_unload {
            RequestKind::Load
        } else {
            RequestKind::Http
        };

        // Response-side scan on the reassembled text; the raw copy is what gets stored (redacted).
        let scan_ctx = ScanContext {
            request_id: meta.id,
            backend: meta.backend,
            api_format: meta.api_format,
            model: parsed
                .model
                .clone()
                .or_else(|| meta.model_requested.clone()),
            is_cloud: meta.is_cloud,
            direction: Direction::Response,
        };
        let response_scan = if parsed.text.is_empty() {
            ScanOutcome::clean()
        } else {
            st.ctx
                .scanner()
                .scan_response(&scan_ctx, &parsed.text, &raw)
        };
        let mut findings = request_findings;
        findings.extend(response_scan.findings.clone());

        let model_resolved = gateway_backends::ollama::resolve_model_id(
            &models,
            parsed.model.as_deref().or(meta.model_requested.as_deref()),
        )
        .map(str::to_string)
        .or_else(|| parsed.model.clone())
        .or_else(|| meta.model_requested.clone());
        let hypothetical_cost = match (parsed.prompt_tokens, parsed.gen_tokens) {
            (Some(prompt), Some(generated)) if outcome.is_completed() => {
                st.ctx.pricing().totals(prompt, generated)
            }
            _ => Default::default(),
        };
        let total_ms =
            if outcome.is_completed() && !parsed.timings_estimated && parsed.total_ms > 0.0 {
                parsed.total_ms
            } else {
                wall.total_ms()
            };

        let mut bodies = Vec::new();
        if let Some((stored, redacted)) = stored_request {
            bodies.push(stored_body(
                &st,
                &meta,
                Direction::Request,
                meta.request_content_type.clone(),
                meta.request_bytes,
                stored,
                redacted,
            ));
        }
        if capture_bodies && !raw.is_empty() {
            let (bytes, redacted) = match response_scan.redacted {
                Some(redacted) => (redacted, true),
                None => (raw, false),
            };
            let mut body = stored_body(
                &st,
                &meta,
                Direction::Response,
                response_content_type,
                response_bytes,
                bytes,
                redacted,
            );
            body.truncated |= truncated;
            bodies.push(body);
        }

        let record = RequestRecord {
            id: meta.id,
            session_id: st.ctx.session_id,
            backend: meta.backend,
            listener: meta.listener.clone(),
            kind,
            client_ip: meta.client_ip.clone(),
            user_agent: meta.user_agent.clone(),
            client_request_id: meta.client_request_id.clone(),
            method: meta.method.clone(),
            path: meta.path.clone(),
            query: meta.query.clone(),
            api_format: meta.api_format,
            streamed: meta.streamed,
            model_requested: meta.model_requested.clone(),
            model_resolved,
            is_cloud: meta.is_cloud,
            started_at: meta.started_at,
            first_byte_at: first_byte.map(|t| {
                meta.started_at
                    + chrono::Duration::from_std(t.saturating_duration_since(meta.started))
                        .unwrap_or_default()
            }),
            ended_at: Utc::now(),
            status: Some(status),
            outcome,
            error_summary,
            prompt_tokens: parsed.prompt_tokens,
            gen_tokens: parsed.gen_tokens,
            tokens_estimated: parsed.tokens_estimated,
            ttft_ms: parsed.ttft_ms,
            total_ms,
            tokens_per_sec: parsed.tokens_per_sec,
            timings_estimated: parsed.timings_estimated,
            stop_reason: parsed.stop_reason,
            request_bytes: meta.request_bytes,
            response_bytes,
            request_headers: meta.request_headers.clone(),
            response_headers,
            chunk_timeline: timeline.into_entries(),
            finding_count: findings.len() as u32,
            max_severity: findings.iter().map(|f| f.severity).max(),
            hypothetical_cost,
        };
        complete(&st, record, bodies, findings).await;
    }
}

fn stored_body(
    st: &ProxyState,
    meta: &RequestMeta,
    direction: Direction,
    content_type: Option<String>,
    original_bytes: u64,
    bytes: Vec<u8>,
    redacted: bool,
) -> StoredBody {
    let cap = st.ctx.capture().max_body_bytes;
    let truncated = bytes.len() > cap;
    let kept = if truncated { &bytes[..cap] } else { &bytes[..] };
    let is_text = std::str::from_utf8(kept).is_ok();
    StoredBody {
        request_id: meta.id,
        direction,
        content_type,
        original_bytes,
        stored_bytes: kept.len() as u64,
        truncated,
        redacted,
        is_text,
        text: String::from_utf8_lossy(kept).into_owned(),
    }
}

/// Persists the record, publishes it, and drops the in-flight entry.
async fn complete(
    st: &ProxyState,
    record: RequestRecord,
    bodies: Vec<StoredBody>,
    findings: Vec<Finding>,
) {
    st.ctx.inflight_remove(record.id);
    debug!(
        request = %record.id,
        backend = %record.backend,
        format = %record.api_format,
        outcome = record.outcome.as_str(),
        model = record.model_resolved.as_deref().unwrap_or("?"),
        prompt = ?record.prompt_tokens,
        generated = ?record.gen_tokens,
        tps = ?record.tokens_per_sec,
        "request recorded"
    );
    if let Some(store) = &st.ctx.store
        && let Err(err) = store
            .persist_request(record.clone(), bodies, findings.clone())
            .await
    {
        warn!(request = %record.id, error = %format!("{err:#}"), "could not persist request");
    }
    for finding in findings {
        publish(&st.ctx.events, GatewayEvent::Finding { finding });
    }
    publish(
        &st.ctx.events,
        GatewayEvent::RequestCompleted {
            record: Box::new(record),
        },
    );
}

/// The context the backend has loaded for the model a request named, for a rejection whose message
/// carries no number: LM Studio 0.4.25 only says the prompt is longer than the context.
fn loaded_context_length(models: &[ModelInfo], model: Option<&str>) -> Option<u64> {
    let id = gateway_backends::ollama::resolve_model_id(models, model)?;
    models
        .iter()
        .find(|m| m.id == id)
        .and_then(|m| m.loaded_context_length)
}

/// How a request ended without a parsed response, and what the client received.
struct Failure {
    /// The status the client received.
    status: u16,
    outcome: Outcome,
    error_summary: Option<String>,
}

/// Records a request that ended without a parsed response: a gateway failure, a block, or an upstream
/// error.
async fn finish_failed(
    st: &ProxyState,
    meta: RequestMeta,
    failure: Failure,
    stored_request: Option<(Vec<u8>, bool)>,
    stored_response: Option<StoredBody>,
    findings: Vec<Finding>,
) {
    let Failure {
        status,
        outcome,
        error_summary,
    } = failure;
    let status = Some(status);
    let mut bodies = Vec::new();
    if let Some((stored, redacted)) = stored_request {
        bodies.push(stored_body(
            st,
            &meta,
            Direction::Request,
            meta.request_content_type.clone(),
            meta.request_bytes,
            stored,
            redacted,
        ));
    }
    let response_bytes = stored_response
        .as_ref()
        .map(|b| b.original_bytes)
        .unwrap_or(0);
    bodies.extend(stored_response);
    let record = RequestRecord {
        id: meta.id,
        session_id: st.ctx.session_id,
        backend: meta.backend,
        listener: meta.listener.clone(),
        kind: RequestKind::Http,
        client_ip: meta.client_ip.clone(),
        user_agent: meta.user_agent.clone(),
        client_request_id: meta.client_request_id.clone(),
        method: meta.method.clone(),
        path: meta.path.clone(),
        query: meta.query.clone(),
        api_format: meta.api_format,
        streamed: meta.streamed,
        model_requested: meta.model_requested.clone(),
        model_resolved: meta.model_requested.clone(),
        is_cloud: meta.is_cloud,
        started_at: meta.started_at,
        first_byte_at: None,
        ended_at: Utc::now(),
        status,
        outcome,
        error_summary,
        prompt_tokens: None,
        gen_tokens: None,
        tokens_estimated: false,
        ttft_ms: None,
        total_ms: meta.started.elapsed().as_secs_f64() * 1000.0,
        tokens_per_sec: None,
        timings_estimated: true,
        stop_reason: None,
        request_bytes: meta.request_bytes,
        response_bytes,
        request_headers: meta.request_headers.clone(),
        response_headers: Vec::new(),
        chunk_timeline: Vec::new(),
        finding_count: findings.len() as u32,
        max_severity: findings.iter().map(|f| f.severity).max(),
        hypothetical_cost: Default::default(),
    };
    complete(st, record, bodies, findings).await;
}

/// Records a passthrough or tunnel exchange: metadata only.
pub async fn finish_simple(
    st: &ProxyState,
    meta: RequestMeta,
    kind: RequestKind,
    status: Option<u16>,
    outcome: Outcome,
    error_summary: Option<String>,
    response_bytes: u64,
) {
    let record = RequestRecord {
        id: meta.id,
        session_id: st.ctx.session_id,
        backend: meta.backend,
        listener: meta.listener.clone(),
        kind,
        client_ip: meta.client_ip.clone(),
        user_agent: meta.user_agent.clone(),
        client_request_id: meta.client_request_id.clone(),
        method: meta.method.clone(),
        path: meta.path.clone(),
        query: meta.query.clone(),
        api_format: ApiFormat::Passthrough,
        streamed: false,
        model_requested: None,
        model_resolved: None,
        is_cloud: false,
        started_at: meta.started_at,
        first_byte_at: None,
        ended_at: Utc::now(),
        status,
        outcome,
        error_summary,
        prompt_tokens: None,
        gen_tokens: None,
        tokens_estimated: false,
        ttft_ms: None,
        total_ms: meta.started.elapsed().as_secs_f64() * 1000.0,
        tokens_per_sec: None,
        timings_estimated: true,
        stop_reason: None,
        request_bytes: meta.request_bytes,
        response_bytes,
        request_headers: meta.request_headers.clone(),
        response_headers: Vec::new(),
        chunk_timeline: Vec::new(),
        finding_count: 0,
        max_severity: None::<Severity>,
        hypothetical_cost: Default::default(),
    };
    complete(st, record, Vec::new(), Vec::new()).await;
}

async fn fail_gateway(
    st: &ProxyState,
    meta: RequestMeta,
    err: ProxyError,
    stored_request: Option<(Vec<u8>, bool)>,
    findings: Vec<Finding>,
) -> Response {
    let response = gateway_error_response(&meta, &err);
    finish_failed(
        st,
        meta,
        Failure {
            status: err.status().as_u16(),
            outcome: Outcome::Failed {
                status: err.status().as_u16(),
                source: FailureSource::Gateway,
            },
            error_summary: Some(err.to_string()),
        },
        stored_request,
        None,
        findings,
    )
    .await;
    response
}

/// The gateway's own error, shaped for the request's API family.
pub fn gateway_error_response(meta: &RequestMeta, err: &ProxyError) -> Response {
    let status = err.status();
    let (content_type, body) =
        http::error_body(meta.api_format, status, &err.to_string(), err.code());
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .header("x-request-id", meta.id.to_string());
    if matches!(err, ProxyError::Blocked { .. }) {
        builder = builder.header("x-gateway-blocked", "dlp");
    }
    builder
        .body(Body::from(body))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

/// The client's response: the upstream status and headers minus hop-by-hop ones and `content-length`
/// (everything streams as chunked), plus the request id.
pub fn build_response(
    meta: &RequestMeta,
    status: StatusCode,
    upstream_headers: &HeaderMap,
    body: Body,
) -> Response {
    let mut builder = Response::builder().status(status);
    let tokens = http::connection_tokens(upstream_headers);
    for (name, value) in upstream_headers {
        let lower = name.as_str().to_ascii_lowercase();
        if http::is_hop_by_hop(&lower) || lower == "content-length" || tokens.contains(&lower) {
            continue;
        }
        builder = builder.header(name.clone(), value.clone());
    }
    builder = builder.header("x-request-id", meta.id.to_string());
    builder.body(body).unwrap_or_else(|_| {
        Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(Body::from(ProxyError::ResponseBuild.to_string()))
            .unwrap_or_else(|_| Response::new(Body::empty()))
    })
}

fn request_id_value(id: Uuid) -> HeaderValue {
    HeaderValue::from_str(&id.to_string()).expect("a uuid is a valid header value")
}

#[allow(
    dead_code,
    reason = "kept for the tests module that ports the monitor's method mapping"
)]
fn reqwest_method(m: &Method) -> Result<reqwest::Method, ProxyError> {
    reqwest::Method::from_bytes(m.as_str().as_bytes()).map_err(|_| ProxyError::UnsupportedMethod)
}
