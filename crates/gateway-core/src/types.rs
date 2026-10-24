//! Vocabulary shared by every crate: backends, severities, DLP actions and the platform. Request records,
//! outcomes and API formats join this module in Phase 1.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The servers the gateway fronts: the two local inference servers, which are polled and get their own
/// listeners, and the three cloud providers the unified listener can route to. Every request to a cloud
/// provider leaves the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Backend {
    #[serde(rename = "lmstudio")]
    LmStudio,
    #[serde(rename = "ollama")]
    Ollama,
    #[serde(rename = "anthropic")]
    Anthropic,
    #[serde(rename = "gemini")]
    Gemini,
    #[serde(rename = "openai")]
    OpenAi,
}

impl Backend {
    pub const ALL: [Backend; 5] = [
        Backend::LmStudio,
        Backend::Ollama,
        Backend::Anthropic,
        Backend::Gemini,
        Backend::OpenAi,
    ];
    /// The local inference servers.
    pub const LOCAL: [Backend; 2] = [Backend::LmStudio, Backend::Ollama];
    /// The hosted APIs; a request to one of these is cloud egress.
    pub const CLOUD_PROVIDERS: [Backend; 3] =
        [Backend::Anthropic, Backend::Gemini, Backend::OpenAi];

    /// The stable identifier used in config keys, URLs and the database.
    pub fn as_str(self) -> &'static str {
        match self {
            Backend::LmStudio => "lmstudio",
            Backend::Ollama => "ollama",
            Backend::Anthropic => "anthropic",
            Backend::Gemini => "gemini",
            Backend::OpenAi => "openai",
        }
    }

    /// The name shown to people.
    pub fn display_name(self) -> &'static str {
        match self {
            Backend::LmStudio => "LM Studio",
            Backend::Ollama => "Ollama",
            Backend::Anthropic => "Anthropic",
            Backend::Gemini => "Gemini",
            Backend::OpenAi => "OpenAI",
        }
    }

    /// A server on this machine (or the operator's network): polled, never cloud egress by itself.
    pub fn is_local(self) -> bool {
        matches!(self, Backend::LmStudio | Backend::Ollama)
    }

    /// A hosted API: every request to it leaves the machine.
    pub fn is_cloud_provider(self) -> bool {
        !self.is_local()
    }
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown backend `{0}` (expected one of lmstudio, ollama, anthropic, gemini, openai)")]
pub struct UnknownBackend(pub String);

impl FromStr for Backend {
    type Err = UnknownBackend;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "lmstudio" | "lm-studio" | "lm_studio" | "lm studio" => Ok(Backend::LmStudio),
            "ollama" => Ok(Backend::Ollama),
            "anthropic" => Ok(Backend::Anthropic),
            "gemini" => Ok(Backend::Gemini),
            "openai" => Ok(Backend::OpenAi),
            other => Err(UnknownBackend(other.to_string())),
        }
    }
}

/// Severity of a DLP finding or an alert. The derived order is `Low < Medium < High < Critical`, which
/// is what `min_severity` thresholds compare against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Low => "low",
            Severity::Medium => "medium",
            Severity::High => "high",
            Severity::Critical => "critical",
        }
    }
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What the gateway does with a request-side DLP finding. `Record` keeps it in the Security tab only,
/// `Alert` also notifies, `Block` refuses the request with 403 and never contacts the upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DlpAction {
    Record,
    Alert,
    Block,
}

/// Response-side findings are recorded or alerted. A stream in flight is never cut (roadmap).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResponseAction {
    Record,
    Alert,
}

/// The operating system this binary was built for. Drives the hardware line (`ane` vs `vram`), the
/// notification sink and the service manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Macos,
    Linux,
    Other,
}

impl Platform {
    pub const fn current() -> Platform {
        if cfg!(target_os = "macos") {
            Platform::Macos
        } else if cfg!(target_os = "linux") {
            Platform::Linux
        } else {
            Platform::Other
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Platform::Macos => "macos",
            Platform::Linux => "linux",
            Platform::Other => "other",
        }
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_parses_its_spellings_and_rejects_others() {
        assert_eq!("lmstudio".parse::<Backend>(), Ok(Backend::LmStudio));
        assert_eq!("LM Studio".parse::<Backend>(), Ok(Backend::LmStudio));
        assert_eq!(" ollama ".parse::<Backend>(), Ok(Backend::Ollama));
        assert_eq!("anthropic".parse::<Backend>(), Ok(Backend::Anthropic));
        assert_eq!("Gemini".parse::<Backend>(), Ok(Backend::Gemini));
        assert_eq!("openai".parse::<Backend>(), Ok(Backend::OpenAi));
        assert_eq!(
            "vllm".parse::<Backend>(),
            Err(UnknownBackend("vllm".into()))
        );
        for backend in Backend::ALL {
            assert_eq!(backend.as_str().parse::<Backend>(), Ok(backend));
            assert_eq!(
                serde_json::from_str::<Backend>(&format!("\"{}\"", backend.as_str())).unwrap(),
                backend
            );
        }
    }

    #[test]
    fn backends_split_into_local_servers_and_cloud_providers() {
        for backend in Backend::ALL {
            assert_ne!(backend.is_local(), backend.is_cloud_provider());
            let listed_local = Backend::LOCAL.contains(&backend);
            let listed_cloud = Backend::CLOUD_PROVIDERS.contains(&backend);
            assert!(
                listed_local != listed_cloud,
                "{backend} must be in exactly one list"
            );
            assert_eq!(listed_local, backend.is_local());
        }
        assert_eq!(
            Backend::LOCAL.len() + Backend::CLOUD_PROVIDERS.len(),
            Backend::ALL.len()
        );
    }

    #[test]
    fn backend_serialises_as_its_identifier() {
        assert_eq!(
            serde_json::to_string(&Backend::LmStudio).unwrap(),
            "\"lmstudio\""
        );
        assert_eq!(
            serde_json::from_str::<Backend>("\"ollama\"").unwrap(),
            Backend::Ollama
        );
        assert_eq!(Backend::LmStudio.to_string(), "lmstudio");
        assert_eq!(Backend::Ollama.display_name(), "Ollama");
    }

    #[test]
    fn severities_order_from_low_to_critical() {
        assert!(Severity::Low < Severity::Medium);
        assert!(Severity::Medium < Severity::High);
        assert!(Severity::High < Severity::Critical);
        assert_eq!(
            serde_json::from_str::<Severity>("\"high\"").unwrap(),
            Severity::High
        );
    }

    #[test]
    fn the_current_platform_is_one_of_the_two_supported_ones_here() {
        let p = Platform::current();
        assert!(
            matches!(p, Platform::Macos | Platform::Linux),
            "unsupported platform {p}"
        );
    }
}

// ---------------------------------------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------------------------------------

/// How a proxied request was classified. It decides the stream parser, whether bodies are captured and
/// scanned, and how errors are shaped. `Passthrough` is everything that is not inference: model lists,
/// pulls, blobs, unknown paths and WebSocket upgrades. `CloudPassthrough` is the same kind of request bound
/// for a cloud provider with a body, which is scanned and kept because the body leaves the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ApiFormat {
    /// Ollama `/api/chat` and `/api/generate`: NDJSON or a single JSON object with engine timings.
    #[serde(rename = "ollama_native")]
    OllamaNative,
    /// Ollama `/api/embed` and `/api/embeddings`.
    #[serde(rename = "ollama_embeddings")]
    OllamaEmbeddings,
    /// `/v1/chat/completions` on either backend.
    #[serde(rename = "openai_chat")]
    OpenAiChat,
    /// `/v1/completions` on either backend.
    #[serde(rename = "openai_completions")]
    OpenAiCompletions,
    /// `/v1/embeddings` on either backend.
    #[serde(rename = "openai_embeddings")]
    OpenAiEmbeddings,
    /// `/v1/responses` on either backend.
    #[serde(rename = "openai_responses")]
    OpenAiResponses,
    /// `/v1/messages` (Anthropic Messages API) on either backend.
    #[serde(rename = "anthropic_messages")]
    AnthropicMessages,
    /// LM Studio `/api/v0/chat/completions` and `/api/v0/completions`, which carry a `stats` object.
    #[serde(rename = "lmstudio_native")]
    LmStudioNative,
    /// LM Studio `/api/v0/embeddings`.
    #[serde(rename = "lmstudio_embeddings")]
    LmStudioEmbeddings,
    /// Everything else, recorded as metadata only.
    #[serde(rename = "passthrough")]
    Passthrough,
    /// A non-inference request with a body bound for a cloud provider (Gemini `cachedContents`, Anthropic
    /// `count_tokens`, uploads): buffered, scanned and stored like inference because the body leaves the
    /// machine, kept past the passthrough retention, not a feed row.
    #[serde(rename = "cloud_passthrough")]
    CloudPassthrough,
}

impl ApiFormat {
    pub const ALL: [ApiFormat; 11] = [
        ApiFormat::OllamaNative,
        ApiFormat::OllamaEmbeddings,
        ApiFormat::OpenAiChat,
        ApiFormat::OpenAiCompletions,
        ApiFormat::OpenAiEmbeddings,
        ApiFormat::OpenAiResponses,
        ApiFormat::AnthropicMessages,
        ApiFormat::LmStudioNative,
        ApiFormat::LmStudioEmbeddings,
        ApiFormat::Passthrough,
        ApiFormat::CloudPassthrough,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ApiFormat::OllamaNative => "ollama_native",
            ApiFormat::OllamaEmbeddings => "ollama_embeddings",
            ApiFormat::OpenAiChat => "openai_chat",
            ApiFormat::OpenAiCompletions => "openai_completions",
            ApiFormat::OpenAiEmbeddings => "openai_embeddings",
            ApiFormat::OpenAiResponses => "openai_responses",
            ApiFormat::AnthropicMessages => "anthropic_messages",
            ApiFormat::LmStudioNative => "lmstudio_native",
            ApiFormat::LmStudioEmbeddings => "lmstudio_embeddings",
            ApiFormat::Passthrough => "passthrough",
            ApiFormat::CloudPassthrough => "cloud_passthrough",
        }
    }

    /// Whether the request is an inference call whose body is buffered, scanned and parsed.
    pub fn is_inference(self) -> bool {
        !self.is_passthrough()
    }

    /// Whether the request is not an inference call: never parsed, never a feed row.
    pub fn is_passthrough(self) -> bool {
        matches!(self, ApiFormat::Passthrough | ApiFormat::CloudPassthrough)
    }

    pub fn is_embeddings(self) -> bool {
        matches!(
            self,
            ApiFormat::OllamaEmbeddings
                | ApiFormat::OpenAiEmbeddings
                | ApiFormat::LmStudioEmbeddings
        )
    }

    /// Whether the backend's own engine timings (TTFT, tokens per second) arrive in the response, as
    /// opposed to the gateway measuring them from its own clock.
    pub fn has_engine_timings(self) -> bool {
        matches!(self, ApiFormat::OllamaNative | ApiFormat::LmStudioNative)
    }
}

impl fmt::Display for ApiFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ApiFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        ApiFormat::ALL
            .into_iter()
            .find(|f| f.as_str() == s)
            .ok_or_else(|| format!("unknown api format `{s}`"))
    }
}

/// What kind of exchange a record describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestKind {
    /// An ordinary HTTP request.
    Http,
    /// A WebSocket (or other) upgrade tunnelled to the backend.
    Upgrade,
    /// An Ollama model load or unload call (`done_reason` load/unload): kept for the inspector,
    /// excluded from the feed and the metrics.
    Load,
}

impl RequestKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RequestKind::Http => "http",
            RequestKind::Upgrade => "upgrade",
            RequestKind::Load => "load",
        }
    }
}

impl FromStr for RequestKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "http" => Ok(RequestKind::Http),
            "upgrade" => Ok(RequestKind::Upgrade),
            "load" => Ok(RequestKind::Load),
            other => Err(format!("unknown request kind `{other}`")),
        }
    }
}

/// Who produced a failed request's status: the backend (or the cloud relay behind it), or the gateway
/// itself (502 unreachable, 413 over the cap, 400 unreadable).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureSource {
    Upstream,
    Gateway,
}

impl FailureSource {
    pub fn as_str(self) -> &'static str {
        match self {
            FailureSource::Upstream => "upstream",
            FailureSource::Gateway => "gateway",
        }
    }
}

impl FromStr for FailureSource {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "upstream" => Ok(FailureSource::Upstream),
            "gateway" => Ok(FailureSource::Gateway),
            other => Err(format!("unknown failure source `{other}`")),
        }
    }
}

/// How a request ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    Completed,
    /// A 4xx or 5xx reached the client.
    Failed {
        status: u16,
        source: FailureSource,
    },
    /// LM Studio refused the prompt because it does not fit the loaded context.
    Rejected {
        input_tokens: Option<u64>,
        context_length: Option<u64>,
    },
    /// The gateway's DLP policy refused the request before it reached the backend.
    Blocked {
        rule_id: String,
    },
    /// The client hung up while the reply was still being generated.
    ClientAborted,
}

impl Outcome {
    /// The stable name stored in the database and used in filters.
    pub fn as_str(&self) -> &'static str {
        match self {
            Outcome::Completed => "completed",
            Outcome::Failed { .. } => "failed",
            Outcome::Rejected { .. } => "rejected",
            Outcome::Blocked { .. } => "blocked",
            Outcome::ClientAborted => "client_aborted",
        }
    }

    pub fn is_completed(&self) -> bool {
        matches!(self, Outcome::Completed)
    }

    /// The feed's stop cell for anything but a completed request, in the TUIs' wording: `failed (HTTP
    /// 429)`, `gateway (HTTP 502)`, `rejected (ctx 262144)`, `blocked (aws-access-key-id)`, `aborted`.
    pub fn summary(&self) -> Option<String> {
        match self {
            Outcome::Completed => None,
            Outcome::Failed { status, source } => Some(match source {
                FailureSource::Upstream => format!("failed (HTTP {status})"),
                FailureSource::Gateway => format!("gateway (HTTP {status})"),
            }),
            Outcome::Rejected { context_length, .. } => Some(match context_length {
                Some(ctx) => format!("rejected (ctx {ctx})"),
                None => "rejected".to_string(),
            }),
            Outcome::Blocked { rule_id } => Some(format!("blocked ({rule_id})")),
            Outcome::ClientAborted => Some("aborted".to_string()),
        }
    }
}

/// Request or response side of an exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Request,
    Response,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::Request => "request",
            Direction::Response => "response",
        }
    }
}

impl FromStr for Direction {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "request" => Ok(Direction::Request),
            "response" => Ok(Direction::Response),
            other => Err(format!("unknown direction `{other}`")),
        }
    }
}

/// One proxied request, written once when it completes. Metadata lives here; bodies are separate rows
/// with their own retention. Token counts and timings are `None` when the format did not provide them,
/// and flagged `estimated` when they were inferred (chunk counts, the gateway's wall clock).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestRecord {
    pub id: Uuid,
    pub session_id: i64,
    pub backend: Backend,
    /// The gateway address the client connected to, e.g. `0.0.0.0:11435`.
    pub listener: String,
    pub kind: RequestKind,
    pub client_ip: String,
    pub user_agent: Option<String>,
    /// An `x-request-id` the client sent, kept for correlation; the gateway's own id is `id`.
    pub client_request_id: Option<String>,
    pub method: String,
    pub path: String,
    pub query: Option<String>,
    pub api_format: ApiFormat,
    pub streamed: bool,
    /// The `model` the client asked for.
    pub model_requested: Option<String>,
    /// The model the backend reported, or the requested one resolved against the model list.
    pub model_resolved: Option<String>,
    /// The prompt left the machine: an Ollama `:cloud` model or a cloud provider.
    pub is_cloud: bool,
    pub started_at: DateTime<Utc>,
    pub first_byte_at: Option<DateTime<Utc>>,
    pub ended_at: DateTime<Utc>,
    pub status: Option<u16>,
    pub outcome: Outcome,
    /// The backend's error message for a failed request, cut to a line; never a prompt.
    pub error_summary: Option<String>,
    pub prompt_tokens: Option<u64>,
    pub gen_tokens: Option<u64>,
    pub tokens_estimated: bool,
    pub ttft_ms: Option<f64>,
    pub total_ms: f64,
    pub tokens_per_sec: Option<f64>,
    pub timings_estimated: bool,
    pub stop_reason: Option<String>,
    pub request_bytes: u64,
    pub response_bytes: u64,
    /// Header names and values with `authorization`, `x-api-key`, `cookie` and `proxy-authorization`
    /// values redacted.
    pub request_headers: Vec<(String, String)>,
    pub response_headers: Vec<(String, String)>,
    /// `(offset_ms from started_at, bytes)` per response chunk, capped; the inspector's timeline.
    pub chunk_timeline: Vec<(u32, u32)>,
    pub finding_count: u32,
    pub max_severity: Option<Severity>,
    /// Frontier model key → total USD at the prices in force when the record was written.
    pub hypothetical_cost: BTreeMap<String, f64>,
    /// What a completed request to a cloud provider cost at that provider's list price, when the model is
    /// priced and both token counts are known. Local requests have none.
    pub actual_cost_usd: Option<f64>,
    /// How the backend was chosen: `listener` on a per-backend listener, otherwise the unified listener's
    /// reason (`alias`, `model_list`, `provider_prefix`, `route`, `model_pattern`, `header`, `default`,
    /// `prefix`, `path`).
    pub routed_by: Option<String>,
}

impl RequestRecord {
    /// Whether the record belongs in the live feed and the rolling metrics: a real inference exchange,
    /// not a passthrough, an upgrade or a model load.
    pub fn is_feed_row(&self) -> bool {
        self.kind == RequestKind::Http && self.api_format.is_inference()
    }
}

/// A request that is still in flight: what the feed shows with a spinner until the record arrives.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InflightRequest {
    pub id: Uuid,
    pub backend: Backend,
    pub api_format: ApiFormat,
    pub kind: RequestKind,
    pub method: String,
    pub path: String,
    pub model_requested: Option<String>,
    pub is_cloud: bool,
    pub client_ip: String,
    pub user_agent: Option<String>,
    pub started_at: DateTime<Utc>,
}

/// A captured request or response body, stored DLP-redacted and subject to retention.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Body {
    pub request_id: Uuid,
    pub direction: Direction,
    pub content_type: Option<String>,
    pub original_bytes: u64,
    pub stored_bytes: u64,
    pub truncated: bool,
    pub redacted: bool,
    pub is_text: bool,
    pub text: String,
}

// ---------------------------------------------------------------------------------------------------------
// DLP findings and alerts (produced from Phase 4; the types live here because records and the store
// reference them)
// ---------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingCategory {
    Secret,
    Pii,
    KnownSecret,
    Custom,
}

impl FindingCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            FindingCategory::Secret => "secret",
            FindingCategory::Pii => "pii",
            FindingCategory::KnownSecret => "known_secret",
            FindingCategory::Custom => "custom",
        }
    }
}

/// One DLP match. Never the plaintext: a fingerprint (the first 32 hex characters of the SHA-256 of the
/// value) and a masked preview such as `AKIA…MPLE`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    pub request_id: Uuid,
    pub direction: Direction,
    pub rule_id: String,
    pub category: FindingCategory,
    pub severity: Severity,
    /// A JSON path such as `$.messages[3].content`, or `raw` for a non-JSON body.
    pub location: String,
    pub offset: Option<u64>,
    pub length: u32,
    pub fingerprint: String,
    pub preview: String,
    pub action: DlpAction,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertKind {
    DlpFinding,
    DlpBlocked,
    CloudEgress,
    BackendUnreachable,
    BackendRecovered,
}

impl AlertKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AlertKind::DlpFinding => "dlp_finding",
            AlertKind::DlpBlocked => "dlp_blocked",
            AlertKind::CloudEgress => "cloud_egress",
            AlertKind::BackendUnreachable => "backend_unreachable",
            AlertKind::BackendRecovered => "backend_recovered",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Alert {
    pub id: i64,
    pub created_at: DateTime<Utc>,
    pub kind: AlertKind,
    pub severity: Severity,
    pub title: String,
    pub detail: serde_json::Value,
    pub request_id: Option<Uuid>,
    pub acknowledged_at: Option<DateTime<Utc>>,
    /// Sink names the alert was delivered to.
    pub delivered: Vec<String>,
}

// ---------------------------------------------------------------------------------------------------------
// Sessions, backends, models, hardware
// ---------------------------------------------------------------------------------------------------------

/// One run of the gateway process.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: i64,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub app_version: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModelState {
    Loaded,
    NotLoaded,
    /// Runs on the vendor's cloud; prompts leave the machine.
    Cloud,
}

impl ModelState {
    pub fn as_str(self) -> &'static str {
        match self {
            ModelState::Loaded => "loaded",
            ModelState::NotLoaded => "not-loaded",
            ModelState::Cloud => "cloud",
        }
    }

    /// Display order: loaded, then cloud, then not loaded.
    pub fn sort_key(self) -> u8 {
        match self {
            ModelState::Loaded => 0,
            ModelState::Cloud => 1,
            ModelState::NotLoaded => 2,
        }
    }
}

/// One row of the models table, the same shape for every backend. Field names follow LMS-Monitor's
/// `ModelInfo`, which Ollama-Monitor mirrored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub backend: Backend,
    pub id: String,
    /// `llm`, `vlm` or `embeddings`.
    pub kind: Option<String>,
    /// Weight format: `gguf`, `mlx`, `safetensors`, or `cloud` for remote models.
    pub compatibility_type: Option<String>,
    pub quantization: Option<String>,
    pub max_context_length: Option<u64>,
    /// The context the loaded instance was given, when known.
    pub loaded_context_length: Option<u64>,
    pub state: ModelState,
    pub publisher: Option<String>,
    pub arch: Option<String>,
    pub capabilities: Vec<String>,
    pub size_bytes: Option<u64>,
    pub size_vram_bytes: Option<u64>,
    /// Ollama: when the loaded model will be unloaded.
    pub expires_at: Option<DateTime<Utc>>,
    pub remote_host: Option<String>,
}

impl ModelInfo {
    pub fn is_cloud(&self) -> bool {
        self.state == ModelState::Cloud
    }
}

/// What a backend poll found.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackendSnapshot {
    pub backend: Backend,
    pub reachable: bool,
    pub version: Option<String>,
    /// Why the backend was unreachable, in a line.
    pub error: Option<String>,
    /// The last known model list, kept across unreachable polls.
    pub models: Vec<ModelInfo>,
    pub polled_at: DateTime<Utc>,
}

/// Per-backend process stats from sysinfo.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct ProcStats {
    pub process_count: u32,
    /// Summed over the process tree and per core, so it can exceed 100.
    pub cpu_pct: f32,
    pub rss_bytes: u64,
}

/// One hardware sample. GPU fields come from `ioreg` on macOS and `nvidia-smi` on Linux; `ane_power_mw`
/// and the renderer/tiler percentages are macOS only. Anything a platform cannot measure is `None`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct HardwareSample {
    pub at: DateTime<Utc>,
    pub system_cpu_pct: f32,
    pub mem_used_bytes: u64,
    pub mem_total_bytes: u64,
    pub mem_available_bytes: u64,
    pub lmstudio: ProcStats,
    pub ollama: ProcStats,
    pub gpu_util_pct: Option<f32>,
    pub gpu_renderer_pct: Option<f32>,
    pub gpu_tiler_pct: Option<f32>,
    pub gpu_mem_used_bytes: Option<u64>,
    pub gpu_mem_alloc_bytes: Option<u64>,
    pub ane_power_mw: Option<f32>,
}

/// Lifetime totals over every session, from the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LifetimeTotals {
    pub session_count: u64,
    pub request_count: u64,
    pub prompt_tokens: u64,
    pub gen_tokens: u64,
}

#[cfg(test)]
mod request_tests {
    use super::*;

    #[test]
    fn api_formats_round_trip_through_their_names() {
        for format in ApiFormat::ALL {
            assert_eq!(format.as_str().parse::<ApiFormat>().unwrap(), format);
            let json = serde_json::to_string(&format).unwrap();
            assert_eq!(json, format!("\"{}\"", format.as_str()));
        }
        assert!(ApiFormat::OpenAiChat.is_inference());
        assert!(!ApiFormat::Passthrough.is_inference());
        assert!(ApiFormat::OllamaEmbeddings.is_embeddings());
        assert!(ApiFormat::OllamaNative.has_engine_timings());
        assert!(!ApiFormat::OpenAiChat.has_engine_timings());
    }

    #[test]
    fn outcomes_summarise_in_the_feeds_wording() {
        assert_eq!(Outcome::Completed.summary(), None);
        assert_eq!(
            Outcome::Failed {
                status: 429,
                source: FailureSource::Upstream
            }
            .summary()
            .as_deref(),
            Some("failed (HTTP 429)")
        );
        assert_eq!(
            Outcome::Failed {
                status: 502,
                source: FailureSource::Gateway
            }
            .summary()
            .as_deref(),
            Some("gateway (HTTP 502)")
        );
        assert_eq!(
            Outcome::Rejected {
                input_tokens: Some(359_277),
                context_length: Some(262_144)
            }
            .summary()
            .as_deref(),
            Some("rejected (ctx 262144)")
        );
        assert_eq!(
            Outcome::Blocked {
                rule_id: "aws-access-key-id".into()
            }
            .summary()
            .as_deref(),
            Some("blocked (aws-access-key-id)")
        );
        assert_eq!(Outcome::ClientAborted.summary().as_deref(), Some("aborted"));
        assert_eq!(Outcome::ClientAborted.as_str(), "client_aborted");
    }

    #[test]
    fn outcomes_serialise_tagged() {
        let json = serde_json::to_string(&Outcome::Failed {
            status: 404,
            source: FailureSource::Upstream,
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"kind":"failed","status":404,"source":"upstream"}"#
        );
        assert_eq!(
            serde_json::to_string(&Outcome::Completed).unwrap(),
            r#"{"kind":"completed"}"#
        );
    }

    #[test]
    fn model_states_sort_loaded_cloud_not_loaded() {
        let mut states = vec![ModelState::NotLoaded, ModelState::Cloud, ModelState::Loaded];
        states.sort_by_key(|s| s.sort_key());
        assert_eq!(
            states,
            [ModelState::Loaded, ModelState::Cloud, ModelState::NotLoaded]
        );
        assert_eq!(
            serde_json::to_string(&ModelState::NotLoaded).unwrap(),
            "\"not-loaded\""
        );
    }
}
