//! The metrics loop: subscribes to the event bus, feeds completed inference rows into the rolling
//! aggregator, republishes the rolling metrics every two seconds, and logs one line per feed row (the
//! headless mode of the TUIs) under the `feed` target, so `--headless` shows traffic on stderr and the log
//! file keeps it.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{Local, Utc};
use gateway_core::aggregate::Aggregator;
use gateway_core::events::{EventReceiver, EventSender, GatewayEvent, publish};
use gateway_core::types::{Outcome, RequestRecord};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::watch;
use tracing::info;

const METRICS_EVERY: Duration = Duration::from_secs(2);

pub async fn run(
    mut events_in: EventReceiver,
    aggregator: Arc<Mutex<Aggregator>>,
    events_out: EventSender,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut ticker = tokio::time::interval(METRICS_EVERY);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            event = events_in.recv() => match event {
                Ok(event) => {
                    if let GatewayEvent::RequestCompleted { record } = event.as_ref()
                        && record.is_feed_row()
                    {
                        if let Ok(mut agg) = aggregator.lock() {
                            agg.ingest(record);
                        }
                        info!(target: "feed", "{}", feed_line(record));
                    }
                }
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            },
            _ = ticker.tick() => {
                let snapshot = aggregator.lock().ok().map(|agg| agg.snapshot(Utc::now()));
                if let Some(rolling) = snapshot {
                    publish(&events_out, GatewayEvent::Metrics { rolling });
                }
            }
        }
    }
}

/// One line per feed row, in the TUIs' headless format: `~` marks an estimate.
pub fn feed_line(record: &RequestRecord) -> String {
    let time = record.ended_at.with_timezone(&Local).format("%H:%M:%S");
    let model = record
        .model_resolved
        .as_deref()
        .or(record.model_requested.as_deref())
        .unwrap_or("?");
    let head = format!("{time} {} {model}", record.backend);
    match &record.outcome {
        Outcome::Completed => {
            let tok = if record.tokens_estimated { "~" } else { "" };
            let tim = if record.timings_estimated { "~" } else { "" };
            let usd = record
                .actual_cost_usd
                .map(|v| format!(" usd={v:.4}"))
                .unwrap_or_default();
            format!(
                "{head} | prompt={tok}{} gen={tok}{} tok/s={tim}{} ttft={tim}{} total={:.1}s ({}){usd}",
                record
                    .prompt_tokens
                    .map_or("?".to_string(), |v| v.to_string()),
                record.gen_tokens.map_or("?".to_string(), |v| v.to_string()),
                record
                    .tokens_per_sec
                    .map_or("-".to_string(), |v| format!("{v:.1}")),
                record
                    .ttft_ms
                    .map_or("-".to_string(), |v| format!("{v:.0}ms")),
                record.total_ms / 1000.0,
                record.stop_reason.as_deref().unwrap_or("-"),
            )
        }
        Outcome::Rejected {
            input_tokens,
            context_length,
        } => format!(
            "{head} | REJECTED input={} ctx={}",
            input_tokens.map_or("?".to_string(), |v| v.to_string()),
            context_length.map_or("?".to_string(), |v| v.to_string()),
        ),
        Outcome::Blocked { rule_id } => format!("{head} | BLOCKED rule={rule_id}"),
        other => format!(
            "{head} | {}{}",
            other.summary().unwrap_or_default(),
            record
                .error_summary
                .as_deref()
                .map(|m| format!(": {m}"))
                .unwrap_or_default()
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use gateway_core::types::{ApiFormat, Backend, FailureSource, RequestKind};
    use uuid::Uuid;

    use super::*;

    fn record(outcome: Outcome) -> RequestRecord {
        RequestRecord {
            id: Uuid::nil(),
            session_id: 1,
            backend: Backend::Ollama,
            listener: "l".into(),
            kind: RequestKind::Http,
            client_ip: "c".into(),
            user_agent: None,
            client_request_id: None,
            method: "POST".into(),
            path: "/api/chat".into(),
            query: None,
            api_format: ApiFormat::OllamaNative,
            streamed: true,
            model_requested: Some("qwen3:14b".into()),
            model_resolved: Some("qwen3:14b".into()),
            is_cloud: false,
            started_at: Utc::now(),
            first_byte_at: None,
            ended_at: Utc::now(),
            status: Some(200),
            outcome,
            error_summary: Some("too many requests".into()),
            prompt_tokens: Some(12),
            gen_tokens: Some(88),
            tokens_estimated: true,
            ttft_ms: Some(300.0),
            total_ms: 5200.0,
            tokens_per_sec: Some(18.4),
            timings_estimated: false,
            stop_reason: Some("stop".into()),
            request_bytes: 0,
            response_bytes: 0,
            request_headers: vec![],
            response_headers: vec![],
            chunk_timeline: vec![],
            finding_count: 0,
            max_severity: None,
            hypothetical_cost: BTreeMap::new(),
            actual_cost_usd: None,
            routed_by: None,
        }
    }

    #[test]
    fn feed_lines_follow_the_headless_format() {
        let line = feed_line(&record(Outcome::Completed));
        assert!(
            line.ends_with(
                "ollama qwen3:14b | prompt=~12 gen=~88 tok/s=18.4 ttft=300ms total=5.2s (stop)"
            ),
            "{line}"
        );
        let mut priced = record(Outcome::Completed);
        priced.backend = Backend::Anthropic;
        priced.actual_cost_usd = Some(0.01234);
        let line = feed_line(&priced);
        assert!(line.contains(" anthropic qwen3:14b | "), "{line}");
        assert!(line.ends_with("(stop) usd=0.0123"), "{line}");
        let line = feed_line(&record(Outcome::Failed {
            status: 429,
            source: FailureSource::Upstream,
        }));
        assert!(
            line.ends_with("| failed (HTTP 429): too many requests"),
            "{line}"
        );
        let line = feed_line(&record(Outcome::Rejected {
            input_tokens: Some(359_277),
            context_length: Some(262_144),
        }));
        assert!(
            line.ends_with("| REJECTED input=359277 ctx=262144"),
            "{line}"
        );
        let line = feed_line(&record(Outcome::Blocked {
            rule_id: "aws".into(),
        }));
        assert!(line.ends_with("| BLOCKED rule=aws"), "{line}");
    }
}
