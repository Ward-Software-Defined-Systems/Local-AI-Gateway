//! The DLP rules with their effective policy, a test box that scans text without storing anything, and a
//! reload of the configuration and known-secrets file.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::State;
use gateway_core::config::DlpCloudConfig;
use gateway_core::types::{
    Backend, Direction, DlpAction, Finding, FindingCategory, ResponseAction, Severity,
};
use gateway_dlp::engine::ScanInput;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{ApiError, ApiResult, ok};
use crate::hot::ApplyReport;
use crate::web::AppState;

/// The test box refuses more than this; the proxy's own cap is separate.
const TEST_TEXT_MAX: usize = 1024 * 1024;

#[derive(Debug, Serialize)]
pub struct RuleView {
    pub id: String,
    pub description: String,
    pub category: FindingCategory,
    pub severity: Severity,
    pub builtin: bool,
    pub enabled: bool,
    pub redact: bool,
    pub keywords: Vec<String>,
    pub min_entropy: Option<f32>,
    pub validator: Option<String>,
    /// Effective request action per backend (the cloud providers resolved as cloud egress), plus `cloud`
    /// for Ollama cloud models.
    pub request_action: BTreeMap<String, DlpAction>,
    pub response_action: BTreeMap<String, ResponseAction>,
}

#[derive(Debug, Serialize)]
pub struct RulesView {
    pub enabled: bool,
    pub default_action: DlpAction,
    pub response_action: ResponseAction,
    pub cloud: DlpCloudConfig,
    pub notify_min_severity: Severity,
    pub redact_stored_copies: bool,
    pub known_secrets: usize,
    pub known_secrets_file: String,
    pub allowlist_entries: usize,
    pub custom_rules: usize,
    pub rules: Vec<RuleView>,
}

pub async fn api_rules(State(st): State<AppState>) -> ApiResult<RulesView> {
    let engine = st.hot().engine();
    let config = st.config();
    let policy = engine.policy();
    let rules = engine
        .rules()
        .iter()
        .map(|compiled| {
            let rule = &compiled.rule;
            let ollama = policy.resolve(rule, Backend::Ollama, false);
            let mut request_action = BTreeMap::new();
            let mut response_action = BTreeMap::new();
            for backend in Backend::ALL {
                let effective = policy.resolve(rule, backend, backend.is_cloud_provider());
                request_action.insert(backend.as_str().to_string(), effective.request_action);
                response_action.insert(backend.as_str().to_string(), effective.response_action);
            }
            request_action.insert(
                "cloud".to_string(),
                policy.resolve(rule, Backend::Ollama, true).request_action,
            );
            RuleView {
                id: rule.id.clone(),
                description: rule.description.clone(),
                category: rule.category,
                severity: ollama.severity,
                builtin: rule.builtin,
                enabled: compiled.enabled,
                redact: ollama.redact,
                keywords: rule.keywords.clone(),
                min_entropy: rule.min_entropy,
                validator: rule
                    .validator
                    .map(|v| format!("{v:?}").to_ascii_lowercase()),
                request_action,
                response_action,
            }
        })
        .collect();
    ok(RulesView {
        enabled: engine.is_enabled(),
        default_action: config.dlp.default_action,
        response_action: config.dlp.response_action,
        cloud: config.dlp.cloud.clone(),
        notify_min_severity: config.dlp.notify_min_severity,
        redact_stored_copies: config.dlp.redact_stored_copies,
        known_secrets: engine.known_secret_count(),
        known_secrets_file: st
            .hot()
            .paths()
            .in_data_dir(&config.dlp.known_secrets_file)
            .display()
            .to_string(),
        allowlist_entries: config.dlp.allowlist.len(),
        custom_rules: config.dlp.custom_rules.len(),
        rules,
    })
}

#[derive(Debug, Deserialize)]
pub struct TestBody {
    pub text: String,
    pub backend: Option<Backend>,
    pub is_cloud: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct TestResult {
    pub enabled: bool,
    pub findings: Vec<Finding>,
    pub would_block: Option<String>,
    /// What the stored copy would look like; `None` when nothing needed redacting.
    pub redacted: Option<String>,
    pub allowlisted: usize,
    pub scan_truncated: bool,
}

/// Scans `text` as a request body under the current policy. Nothing is stored or logged.
pub async fn api_test(
    State(st): State<AppState>,
    Json(body): Json<TestBody>,
) -> ApiResult<TestResult> {
    if body.text.len() > TEST_TEXT_MAX {
        return Err(ApiError::bad_request(format!(
            "text is longer than {TEST_TEXT_MAX} bytes"
        )));
    }
    let engine = st.hot().engine();
    let backend = body.backend.unwrap_or(Backend::Ollama);
    let input = ScanInput {
        request_id: Uuid::nil(),
        backend,
        // A request to a cloud provider always leaves the machine, whatever the box says.
        is_cloud: body.is_cloud.unwrap_or(false) || backend.is_cloud_provider(),
        direction: Direction::Request,
    };
    let out = engine.scan_request(&input, body.text.as_bytes());
    ok(TestResult {
        enabled: engine.is_enabled(),
        findings: out.findings,
        would_block: out.blocked_by,
        redacted: out
            .redacted
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned()),
        allowlisted: out.allowlisted,
        scan_truncated: out.scan_truncated,
    })
}

/// Re-reads `config.toml` and the known-secrets file and applies them, as SIGHUP does.
pub async fn api_reload(State(st): State<AppState>) -> ApiResult<ApplyReport> {
    let report = st
        .hot()
        .reload_from_disk()
        .map_err(|err| ApiError::bad_request(format!("{err:#}")))?;
    ok(report)
}
