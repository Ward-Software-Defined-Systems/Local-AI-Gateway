//! The alert dispatcher. It watches the event bus: findings and completions become one alert per request
//! when the findings reach `dlp.notify_min_severity` or the request was blocked; backend polls that fail
//! three times in a row raise an alert, and the recovery clears it. Every alert is stored and published
//! (the in-app list is never throttled); the desktop and webhook sinks are throttled per `(kind, rule,
//! backend)` so one noisy rule cannot flood notifications, and the suppressed count rides on the next one.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use chrono::Utc;
use gateway_core::config::{AlertsConfig, DlpConfig};
use gateway_core::events::{EventReceiver, EventSender, GatewayEvent, publish};
use gateway_core::paths::Paths;
use gateway_core::store::StoreHandle;
use gateway_core::types::{
    Alert, AlertKind, Backend, Finding, Outcome, Platform, RequestRecord, Severity,
};
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::watch;
use tracing::{debug, info, warn};
use uuid::Uuid;

/// Unreachable polls in a row before an alert.
const UNREACHABLE_AFTER: u32 = 3;

/// What the sinks need; swapped on config reload.
#[derive(Debug, Clone)]
pub struct AlertSettings {
    pub alerts: AlertsConfig,
    pub notify_min_severity: Severity,
}

impl AlertSettings {
    /// `webhook.headers_file` (a TOML table of header name to value, kept out of `config.toml` so the
    /// config editor never shows it) is merged into `webhook.headers`; an unreadable file is logged and
    /// skipped, never printed.
    pub fn from_config(alerts: &AlertsConfig, dlp: &DlpConfig, paths: &Paths) -> Self {
        let mut alerts = alerts.clone();
        if let Some(file) = &alerts.webhook.headers_file {
            let path = paths.in_data_dir(file);
            match load_headers_file(&path) {
                Ok(headers) => alerts.webhook.headers.extend(headers),
                Err(err) => {
                    warn!(path = %path.display(), error = %err, "webhook headers file not loaded")
                }
            }
        }
        Self {
            alerts,
            notify_min_severity: dlp.notify_min_severity,
        }
    }
}

fn load_headers_file(
    path: &std::path::Path,
) -> Result<std::collections::BTreeMap<String, String>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    toml::from_str(&text).map_err(|e| e.to_string())
}

pub type SharedSettings = Arc<RwLock<AlertSettings>>;

struct Dispatcher {
    settings: SharedSettings,
    store: StoreHandle,
    events: EventSender,
    client: reqwest::Client,
    /// Findings waiting for their request's completion.
    pending: HashMap<Uuid, Vec<Finding>>,
    unreachable_streak: HashMap<Backend, u32>,
    alerted_unreachable: HashMap<Backend, bool>,
    throttle: HashMap<(String, String, String), (Instant, u32)>,
    notify_send_available: Option<bool>,
}

pub async fn run(
    mut events_in: EventReceiver,
    events_out: EventSender,
    store: StoreHandle,
    settings: SharedSettings,
    mut shutdown: watch::Receiver<bool>,
) {
    let client = match reqwest::Client::builder().build() {
        Ok(c) => c,
        Err(err) => {
            warn!(error = %err, "alert dispatcher could not build its HTTP client; webhooks disabled");
            return;
        }
    };
    let mut dispatcher = Dispatcher {
        settings,
        store,
        events: events_out,
        client,
        pending: HashMap::new(),
        unreachable_streak: HashMap::new(),
        alerted_unreachable: HashMap::new(),
        throttle: HashMap::new(),
        notify_send_available: None,
    };
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            event = events_in.recv() => match event {
                Ok(event) => dispatcher.handle(&event).await,
                Err(RecvError::Lagged(n)) => debug!(dropped = n, "alert dispatcher lagged behind the bus"),
                Err(RecvError::Closed) => break,
            },
        }
    }
    info!("alert dispatcher stopped");
}

impl Dispatcher {
    async fn handle(&mut self, event: &GatewayEvent) {
        match event {
            GatewayEvent::Finding { finding } => {
                self.pending
                    .entry(finding.request_id)
                    .or_default()
                    .push(finding.clone());
            }
            GatewayEvent::RequestCompleted { record } => {
                let findings = self.pending.remove(&record.id).unwrap_or_default();
                if let Some(alert) = self.dlp_alert(record, &findings) {
                    self.dispatch(alert).await;
                }
            }
            GatewayEvent::BackendSnapshot { snapshot } => {
                let streak = self.unreachable_streak.entry(snapshot.backend).or_insert(0);
                let alerted = self
                    .alerted_unreachable
                    .entry(snapshot.backend)
                    .or_insert(false);
                if snapshot.reachable {
                    *streak = 0;
                    if *alerted {
                        *alerted = false;
                        let alert = Alert {
                            id: 0,
                            created_at: Utc::now(),
                            kind: AlertKind::BackendRecovered,
                            severity: Severity::Low,
                            title: format!(
                                "{} is reachable again",
                                snapshot.backend.display_name()
                            ),
                            detail: json!({"backend": snapshot.backend, "version": snapshot.version}),
                            request_id: None,
                            acknowledged_at: None,
                            delivered: Vec::new(),
                        };
                        self.dispatch(alert).await;
                    }
                } else {
                    *streak += 1;
                    if *streak == UNREACHABLE_AFTER && !*alerted {
                        *alerted = true;
                        let alert = Alert {
                            id: 0,
                            created_at: Utc::now(),
                            kind: AlertKind::BackendUnreachable,
                            severity: Severity::High,
                            title: format!("{} is unreachable", snapshot.backend.display_name()),
                            detail: json!({"backend": snapshot.backend, "error": snapshot.error, "polls": UNREACHABLE_AFTER}),
                            request_id: None,
                            acknowledged_at: None,
                            delivered: Vec::new(),
                        };
                        self.dispatch(alert).await;
                    }
                }
            }
            _ => {}
        }
    }

    fn dlp_alert(&self, record: &RequestRecord, findings: &[Finding]) -> Option<Alert> {
        let blocked = matches!(record.outcome, Outcome::Blocked { .. });
        let max_severity = findings.iter().map(|f| f.severity).max();
        let min = self
            .settings
            .read()
            .map(|s| s.notify_min_severity)
            .unwrap_or(Severity::High);
        if !blocked && max_severity.is_none_or(|s| s < min) {
            return None;
        }
        let severity = max_severity.unwrap_or(Severity::High);
        let model = record
            .model_resolved
            .clone()
            .or_else(|| record.model_requested.clone())
            .unwrap_or_else(|| "?".to_string());
        let mut rules: Vec<&str> = findings.iter().map(|f| f.rule_id.as_str()).collect();
        rules.sort_unstable();
        rules.dedup();
        let title = alert_title(record.backend, &model, &rules, findings, blocked);
        Some(Alert {
            id: 0,
            created_at: Utc::now(),
            kind: if blocked {
                AlertKind::DlpBlocked
            } else {
                AlertKind::DlpFinding
            },
            severity,
            title,
            detail: json!({
                "backend": record.backend,
                "model": model,
                "client_ip": record.client_ip,
                "is_cloud": record.is_cloud,
                "rules": rules,
                "findings": findings.iter().map(|f| json!({"rule": f.rule_id, "severity": f.severity, "direction": f.direction, "location": f.location, "preview": f.preview})).collect::<Vec<_>>(),
            }),
            request_id: Some(record.id),
            acknowledged_at: None,
            delivered: Vec::new(),
        })
    }

    async fn dispatch(&mut self, mut alert: Alert) {
        let settings = match self.settings.read() {
            Ok(s) => s.clone(),
            Err(_) => return,
        };
        alert.delivered.push("in_app".to_string());
        let rule = alert
            .detail
            .get("rules")
            .and_then(|r| r.get(0))
            .and_then(|r| r.as_str())
            .unwrap_or("")
            .to_string();
        let backend = alert
            .detail
            .get("backend")
            .and_then(|b| b.as_str())
            .unwrap_or("")
            .to_string();
        let key = (alert.kind.as_str().to_string(), rule, backend);
        let suppressed = self.throttled(&key, settings.alerts.throttle_secs);
        if let Some(n) = suppressed.filter(|n| *n > 0)
            && let Some(obj) = alert.detail.as_object_mut()
        {
            obj.insert("suppressed_before_this".to_string(), json!(n));
        }
        let notify = suppressed.is_some();
        if notify
            && settings.alerts.desktop_notification.enabled
            && alert.severity >= settings.alerts.desktop_notification.min_severity
        {
            self.desktop_notify(&alert).await;
            alert.delivered.push("desktop".to_string());
        }
        if notify
            && settings.alerts.webhook.enabled
            && alert.severity >= settings.alerts.webhook.min_severity
            && self.webhook(&settings, &alert).await
        {
            alert.delivered.push("webhook".to_string());
        }
        match self.store.persist_alert(alert.clone()).await {
            Ok(id) => alert.id = id,
            Err(err) => warn!(error = %format!("{err:#}"), "could not store alert"),
        }
        info!(kind = alert.kind.as_str(), severity = %alert.severity, title = %alert.title, delivered = ?alert.delivered, "alert");
        publish(&self.events, GatewayEvent::Alert { alert });
    }

    /// `Some(suppressed_count)` when the sinks may fire now, `None` when still inside the throttle window.
    fn throttled(&mut self, key: &(String, String, String), throttle_secs: u64) -> Option<u32> {
        let now = Instant::now();
        let window = Duration::from_secs(throttle_secs);
        match self.throttle.get_mut(key) {
            Some((last, suppressed)) if now.duration_since(*last) < window => {
                *suppressed += 1;
                None
            }
            Some((last, suppressed)) => {
                let n = *suppressed;
                *last = now;
                *suppressed = 0;
                Some(n)
            }
            None => {
                self.throttle.insert(key.clone(), (now, 0));
                Some(0)
            }
        }
    }

    /// `osascript` on macOS, `notify-send` on a Linux desktop. The text is built from masked fields only.
    async fn desktop_notify(&mut self, alert: &Alert) {
        let title = "Local AI Gateway";
        let body = alert.title.chars().take(200).collect::<String>();
        let result = match Platform::current() {
            Platform::Macos => {
                let script = format!(
                    "display notification \"{}\" with title \"{}\" subtitle \"{}\"",
                    escape_applescript(&body),
                    title,
                    escape_applescript(&format!(
                        "{} · {}",
                        alert.severity,
                        alert.kind.as_str().replace('_', " ")
                    ))
                );
                run_quiet("osascript", &["-e", &script]).await
            }
            Platform::Linux => {
                if self.notify_send_available.is_none() {
                    self.notify_send_available = Some(
                        run_quiet("sh", &["-c", "command -v notify-send >/dev/null 2>&1"])
                            .await
                            .is_ok(),
                    );
                    if self.notify_send_available == Some(false) {
                        info!(
                            "notify-send is not available; desktop notifications are off (webhooks still work)"
                        );
                    }
                }
                if self.notify_send_available != Some(true) {
                    return;
                }
                run_quiet(
                    "notify-send",
                    &[&format!("{title}: {}", alert.severity), &body],
                )
                .await
            }
            Platform::Other => return,
        };
        if let Err(err) = result {
            debug!(error = %err, "desktop notification failed");
        }
    }

    async fn webhook(&self, settings: &AlertSettings, alert: &Alert) -> bool {
        let Some(url) = settings.alerts.webhook.url.as_deref() else {
            return false;
        };
        let payload = json!({
            "kind": alert.kind,
            "severity": alert.severity,
            "title": alert.title,
            "detail": alert.detail,
            "request_id": alert.request_id,
            "created_at": alert.created_at,
            "source": "local-ai-gateway",
        });
        let timeout = Duration::from_secs(settings.alerts.webhook.timeout_secs.max(1));
        for attempt in 1..=2 {
            let mut req = self.client.post(url).timeout(timeout).json(&payload);
            for (name, value) in &settings.alerts.webhook.headers {
                req = req.header(name, value);
            }
            match req.send().await {
                Ok(resp) if resp.status().is_success() => return true,
                Ok(resp) => warn!(status = %resp.status(), attempt, "webhook refused the alert"),
                Err(err) => warn!(error = %err, attempt, "webhook request failed"),
            }
        }
        false
    }
}

/// `Blocked: aws-access-key-id in a request to Ollama (model)`, or `email, phone in a reply from LM Studio (model)`.
fn alert_title(
    backend: Backend,
    model: &str,
    rules: &[&str],
    findings: &[Finding],
    blocked: bool,
) -> String {
    let rules = rules.join(", ");
    let name = backend.display_name();
    if blocked {
        return format!("Blocked: {rules} in a request to {name} ({model})");
    }
    let only_replies = !findings.is_empty()
        && findings
            .iter()
            .all(|f| f.direction == gateway_core::types::Direction::Response);
    if only_replies {
        format!("{rules} in a reply from {name} ({model})")
    } else {
        format!("{rules} in a request to {name} ({model})")
    }
}

fn escape_applescript(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

async fn run_quiet(program: &str, args: &[&str]) -> anyhow::Result<()> {
    let status = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("{program} timed out"))??;
    anyhow::ensure!(status.success(), "{program} exited with {status}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applescript_strings_are_escaped() {
        assert_eq!(
            escape_applescript(r#"say "hi" \ bye"#),
            r#"say \"hi\" \\ bye"#
        );
    }

    #[test]
    fn a_headers_file_is_merged_into_the_webhook_headers() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            data_dir: dir.path().to_path_buf(),
            config_dir: dir.path().to_path_buf(),
        };
        std::fs::write(
            dir.path().join("hook.toml"),
            "Authorization = \"Bearer s3cret\"\n",
        )
        .unwrap();
        let mut alerts = AlertsConfig::default();
        alerts.webhook.headers_file = Some("hook.toml".into());
        alerts.webhook.headers.insert("X-Env".into(), "prod".into());
        let settings = AlertSettings::from_config(&alerts, &DlpConfig::default(), &paths);
        assert_eq!(
            settings.alerts.webhook.headers["Authorization"],
            "Bearer s3cret"
        );
        assert_eq!(settings.alerts.webhook.headers["X-Env"], "prod");
        alerts.webhook.headers_file = Some("missing.toml".into());
        let settings = AlertSettings::from_config(&alerts, &DlpConfig::default(), &paths);
        assert_eq!(
            settings.alerts.webhook.headers.len(),
            1,
            "a missing file is skipped"
        );
    }

    #[tokio::test]
    async fn the_throttle_lets_the_first_through_and_counts_the_rest() {
        let (events, _rx) = gateway_core::events::channel();
        let dir = tempfile::tempdir().unwrap();
        let store = gateway_core::store::open(&dir.path().join("a.db"))
            .await
            .unwrap();
        let paths = Paths {
            data_dir: dir.path().to_path_buf(),
            config_dir: dir.path().to_path_buf(),
        };
        let settings = Arc::new(RwLock::new(AlertSettings::from_config(
            &AlertsConfig::default(),
            &DlpConfig::default(),
            &paths,
        )));
        let mut d = Dispatcher {
            settings,
            store: store.writer.clone(),
            events,
            client: reqwest::Client::new(),
            pending: HashMap::new(),
            unreachable_streak: HashMap::new(),
            alerted_unreachable: HashMap::new(),
            throttle: HashMap::new(),
            notify_send_available: Some(false),
        };
        let key = (
            "dlp_finding".to_string(),
            "aws".to_string(),
            "ollama".to_string(),
        );
        assert_eq!(d.throttled(&key, 30), Some(0));
        assert_eq!(d.throttled(&key, 30), None);
        assert_eq!(d.throttled(&key, 30), None);
        assert_eq!(
            d.throttled(&key, 0),
            Some(2),
            "a zero window releases at once and reports the suppressed count"
        );
        assert_eq!(
            d.throttled(&("x".into(), "".into(), "".into()), 30),
            Some(0)
        );
    }
}
