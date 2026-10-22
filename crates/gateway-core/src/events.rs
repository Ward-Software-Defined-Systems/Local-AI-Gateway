//! The in-process event bus. Every component publishes `GatewayEvent`s on one `tokio::sync::broadcast`
//! channel; the dashboard's WebSocket sessions, the metrics loop and the alert dispatcher subscribe. Events
//! are serialised as tagged JSON (`{"type": "...", ...}`) and sent to browsers verbatim, so this enum is
//! also the WebSocket protocol. A slow subscriber that lags past the capacity gets `Lagged` and resyncs
//! over REST rather than slowing the proxy.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::aggregate::RollingMetrics;
use crate::types::{
    Alert, BackendSnapshot, Finding, HardwareSample, InflightRequest, RequestRecord,
};

/// How many events a subscriber may fall behind before it is told to resync.
pub const EVENT_BUS_CAPACITY: usize = 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GatewayEvent {
    /// A request arrived at a listener and is being forwarded.
    RequestStarted {
        request: InflightRequest,
    },
    /// The first response byte reached the gateway.
    RequestFirstByte {
        id: Uuid,
        ttft_ms: f64,
    },
    /// Progress on a streaming response, at most a couple of times per second per request.
    RequestProgress {
        id: Uuid,
        gen_tokens_estimate: u64,
        response_bytes: u64,
    },
    /// The request ended, whatever the outcome; this is the row the feed and the store keep.
    RequestCompleted {
        record: Box<RequestRecord>,
    },
    Finding {
        finding: Finding,
    },
    Alert {
        alert: Alert,
    },
    /// A backend poll finished and something changed, or the periodic heartbeat.
    BackendSnapshot {
        snapshot: BackendSnapshot,
    },
    Hardware {
        sample: HardwareSample,
    },
    /// The rolling metrics, recomputed on a timer and after each completion.
    Metrics {
        rolling: RollingMetrics,
    },
    /// The configuration was reloaded; `restart_required` lists the keys that only take effect on restart.
    ConfigChanged {
        restart_required: Vec<String>,
    },
    /// The gateway is shutting down; browsers should stop reconnecting for a moment.
    Shutdown,
}

pub type EventSender = broadcast::Sender<Arc<GatewayEvent>>;
pub type EventReceiver = broadcast::Receiver<Arc<GatewayEvent>>;

pub fn channel() -> (EventSender, EventReceiver) {
    broadcast::channel(EVENT_BUS_CAPACITY)
}

/// Publishes without caring whether anyone listens: a bus with no subscribers is normal at startup.
pub fn publish(events: &EventSender, event: GatewayEvent) {
    let _ = events.send(Arc::new(event));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_serialise_with_a_type_tag() {
        let json = serde_json::to_string(&GatewayEvent::Shutdown).unwrap();
        assert_eq!(json, r#"{"type":"shutdown"}"#);
        let json = serde_json::to_string(&GatewayEvent::ConfigChanged {
            restart_required: vec!["dashboard.listen".into()],
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"type":"config_changed","restart_required":["dashboard.listen"]}"#
        );
        let json = serde_json::to_string(&GatewayEvent::RequestFirstByte {
            id: Uuid::nil(),
            ttft_ms: 12.5,
        })
        .unwrap();
        assert!(
            json.starts_with(r#"{"type":"request_first_byte","id":"00000000-"#),
            "{json}"
        );
    }

    #[test]
    fn publishing_without_subscribers_is_fine() {
        let (tx, rx) = channel();
        drop(rx);
        publish(&tx, GatewayEvent::Shutdown);
    }
}
