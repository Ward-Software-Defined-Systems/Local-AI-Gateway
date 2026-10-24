//! WebSocket (and any other `Upgrade`) tunnelling. LM Studio's SDK clients speak WebSocket on the same
//! port as its HTTP API, so a gateway that owns a listener must carry upgrades through transparently. The
//! handshake is forwarded with its `Connection`, `Upgrade` and `Sec-WebSocket-*` headers; when the backend
//! answers 101 the client gets a 101 too, and once hyper has flushed it both upgraded connections are
//! copied in both directions until either side closes. A non-101 answer passes through as it is.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use gateway_core::types::{FailureSource, Outcome, RequestKind};
use hyper_util::rt::TokioIo;
use tracing::{debug, warn};

use crate::error::ProxyError;
use crate::listener::ProxyState;
use crate::pipeline::{RequestMeta, build_response, finish_simple, gateway_error_response};

pub async fn handle(st: ProxyState, meta: RequestMeta, mut req: Request) -> Response {
    // Taken before the request is consumed: this is the handle hyper resolves once the 101 is sent.
    let on_client = hyper::upgrade::on(&mut req);
    let (parts, _body) = req.into_parts();
    let url = meta.upstream_url();
    let mut headers = HeaderMap::new();
    for (name, value) in &parts.headers {
        // The handshake headers are hop-by-hop in general but are exactly what the upstream needs here.
        if name == header::HOST {
            continue;
        }
        headers.append(name.clone(), value.clone());
    }
    headers.insert(
        "x-request-id",
        meta.id
            .to_string()
            .parse()
            .expect("uuid is a valid header value"),
    );
    let upstream = match st
        .ctx
        .client_for(meta.backend)
        .request(parts.method.clone(), &url)
        .headers(headers)
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(err) => {
            warn!(request = %meta.id, error = %err.without_url(), "upgrade handshake to upstream failed");
            let st2 = st.clone();
            let meta2 = meta.clone();
            tokio::spawn(async move {
                finish_simple(
                    &st2,
                    meta2,
                    RequestKind::Upgrade,
                    Some(502),
                    Outcome::Failed {
                        status: 502,
                        source: FailureSource::Gateway,
                    },
                    Some(ProxyError::UpstreamUnreachable.to_string()),
                    0,
                )
                .await;
            });
            return gateway_error_response(&meta, &ProxyError::UpstreamUnreachable);
        }
    };
    if upstream.status() != StatusCode::SWITCHING_PROTOCOLS {
        let status = upstream.status();
        let resp_headers = upstream.headers().clone();
        let bytes = upstream.bytes().await.unwrap_or_default();
        debug!(request = %meta.id, %status, "upgrade refused by upstream; passing the answer through");
        let outcome = if status.is_client_error() || status.is_server_error() {
            Outcome::Failed {
                status: status.as_u16(),
                source: FailureSource::Upstream,
            }
        } else {
            Outcome::Completed
        };
        let summary = crate::rejection::error_summary(&bytes);
        let st2 = st.clone();
        let meta2 = meta.clone();
        let len = bytes.len() as u64;
        tokio::spawn(async move {
            finish_simple(
                &st2,
                meta2,
                RequestKind::Upgrade,
                Some(status.as_u16()),
                outcome,
                summary,
                len,
            )
            .await;
        });
        return build_response(&meta, status, &resp_headers, Body::from(bytes));
    }

    let mut builder = Response::builder().status(StatusCode::SWITCHING_PROTOCOLS);
    for (name, value) in upstream.headers() {
        let lower = name.as_str().to_ascii_lowercase();
        if lower == "upgrade" || lower == "connection" || lower.starts_with("sec-websocket-") {
            builder = builder.header(name.clone(), value.clone());
        }
    }
    builder = builder.header("x-request-id", meta.id.to_string());
    let response = match builder.body(Body::empty()) {
        Ok(r) => r,
        Err(_) => return gateway_error_response(&meta, &ProxyError::ResponseBuild),
    };

    let st2 = st.clone();
    let meta2 = meta.clone();
    st.ctx.tunnels.spawn(async move {
        let (status, outcome, summary, bytes) = match (upstream.upgrade().await, on_client.await) {
            (Ok(mut server_side), Ok(client_side)) => {
                let mut client_side = TokioIo::new(client_side);
                match tokio::io::copy_bidirectional(&mut client_side, &mut server_side).await {
                    Ok((from_client, from_server)) => {
                        debug!(request = %meta2.id, from_client, from_server, "tunnel closed");
                        (
                            Some(101),
                            Outcome::Completed,
                            None,
                            from_client + from_server,
                        )
                    }
                    Err(err) => {
                        debug!(request = %meta2.id, error = %err, "tunnel ended with an error");
                        (Some(101), Outcome::ClientAborted, Some(err.to_string()), 0)
                    }
                }
            }
            (Err(err), _) => {
                let err = err.without_url();
                warn!(request = %meta2.id, error = %err, "upstream upgrade failed after 101");
                (
                    Some(502),
                    Outcome::Failed {
                        status: 502,
                        source: FailureSource::Gateway,
                    },
                    Some(err.to_string()),
                    0,
                )
            }
            (_, Err(err)) => {
                warn!(request = %meta2.id, error = %err, "client upgrade failed after 101");
                (Some(101), Outcome::ClientAborted, Some(err.to_string()), 0)
            }
        };
        finish_simple(
            &st2,
            meta2,
            RequestKind::Upgrade,
            status,
            outcome,
            summary,
            bytes,
        )
        .await;
    });
    response
}
