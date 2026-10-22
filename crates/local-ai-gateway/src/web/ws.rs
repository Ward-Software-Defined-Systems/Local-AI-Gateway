//! `/ws`: the live event stream. A session gets `hello` first (version, platform, session and the requests
//! in flight), then every `GatewayEvent` as JSON, verbatim from the bus. It may send `ping` (answered with
//! `pong`) and `subscribe` (acknowledged; topic filtering arrives with the events it filters). The server
//! pings every 20 s and closes a socket silent for 60 s. A subscriber that lags behind the bus gets `resync`
//! and refetches over REST; it never slows the proxy.

use std::time::{Duration, Instant};

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use gateway_core::types::Platform;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;
use tracing::debug;

use super::AppState;
use crate::VERSION;

const PING_EVERY: Duration = Duration::from_secs(20);
const IDLE_LIMIT: Duration = Duration::from_secs(60);

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMessage {
    Ping,
    Subscribe {
        #[serde(default)]
        topics: Vec<String>,
    },
}

pub async fn ws_handler(ws: WebSocketUpgrade, State(st): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| session(socket, st))
}

pub fn hello(st: &AppState) -> String {
    let inflight = st.proxy().map(|p| p.inflight()).unwrap_or_default();
    json!({
        "type": "hello",
        "session_id": st.session_id(),
        "server_time": Utc::now(),
        "version": VERSION,
        "platform": Platform::current(),
        "started_at": st.started_at(),
        "inflight": inflight,
    })
    .to_string()
}

async fn session(socket: WebSocket, st: AppState) {
    let (mut tx, mut rx) = socket.split();
    let mut events = st.events().subscribe();
    if tx.send(Message::Text(hello(&st).into())).await.is_err() {
        return;
    }
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.tick().await; // the first tick fires at once
    let mut last_inbound = Instant::now();
    loop {
        tokio::select! {
            event = events.recv() => {
                let text = match event {
                    Ok(event) => match serde_json::to_string(&*event) {
                        Ok(text) => text,
                        Err(err) => { debug!(error = %err, "event did not serialise"); continue; }
                    },
                    Err(RecvError::Lagged(dropped)) => json!({"type": "resync", "dropped": dropped}).to_string(),
                    Err(RecvError::Closed) => break,
                };
                if tx.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
            _ = ping.tick() => {
                if last_inbound.elapsed() > IDLE_LIMIT {
                    debug!("websocket idle; closing");
                    break;
                }
                if tx.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
            inbound = rx.next() => match inbound {
                Some(Ok(Message::Text(text))) => {
                    last_inbound = Instant::now();
                    let reply = match serde_json::from_str::<ClientMessage>(&text) {
                        Ok(ClientMessage::Ping) => Some(json!({"type": "pong"})),
                        Ok(ClientMessage::Subscribe { topics }) => Some(json!({"type": "subscribed", "topics": topics})),
                        Err(_) => None,
                    };
                    if let Some(reply) = reply
                        && tx.send(Message::Text(reply.to_string().into())).await.is_err()
                    {
                        break;
                    }
                }
                Some(Ok(Message::Pong(_))) | Some(Ok(Message::Ping(_))) | Some(Ok(Message::Binary(_))) => {
                    last_inbound = Instant::now();
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
            },
        }
    }
    let _ = tx.close().await;
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gateway_core::config::Config;

    use super::*;

    #[test]
    fn hello_carries_the_version_platform_and_an_empty_inflight_list() {
        let (events, _rx) = gateway_core::events::channel();
        let st = AppState::minimal(Arc::new(Config::default()), "t".repeat(32), false, events);
        let hello: serde_json::Value = serde_json::from_str(&hello(&st)).unwrap();
        assert_eq!(hello["type"], "hello");
        assert_eq!(hello["version"], VERSION);
        assert!(hello["inflight"].as_array().unwrap().is_empty());
        assert!(hello["session_id"].is_null());
    }

    #[test]
    fn client_messages_parse_by_type_tag() {
        assert!(matches!(
            serde_json::from_str::<ClientMessage>(r#"{"type":"ping"}"#).unwrap(),
            ClientMessage::Ping
        ));
        let sub =
            serde_json::from_str::<ClientMessage>(r#"{"type":"subscribe","topics":["hardware"]}"#)
                .unwrap();
        assert!(matches!(sub, ClientMessage::Subscribe { topics } if topics == ["hardware"]));
        assert!(serde_json::from_str::<ClientMessage>(r#"{"type":"explode"}"#).is_err());
    }
}
