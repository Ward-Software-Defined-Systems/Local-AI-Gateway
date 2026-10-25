//! The `config.toml` schema with its defaults, loading and validation.
//!
//! Every key is optional: a missing file means "all defaults" and each table fills in what it does not
//! name. Unknown keys are errors (`deny_unknown_fields`) so a typo cannot silently disable a feature. The
//! backend tables are merged onto per-backend defaults, so `[backends.ollama] capture_bodies = false`
//! keeps Ollama's ports. Validation separates errors, which stop startup, from warnings, which are logged
//! once tracing is up; `--check-config` prints both and exits. The DLP crate validates rule regexes; this
//! module checks the structure every crate shares.

use std::collections::{BTreeMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::routing::{MAX_MODEL_REF_LEN, ModelRef, provider_id};
use crate::tls::SanName;
use crate::types::{Backend, DlpAction, ResponseAction, Severity};

/// A commented `config.toml` holding every default, written by `config init`. A test keeps it equal to
/// `Config::default()`.
pub const SAMPLE: &str = include_str!("../config.sample.toml");

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub dashboard: DashboardConfig,
    pub backends: BackendsConfig,
    /// The listener that routes each request to a backend by model name.
    pub unified: UnifiedConfig,
    /// The names on the certificate every HTTPS listener serves, issued by the gateway's own CA.
    pub tls: TlsConfig,
    pub capture: CaptureConfig,
    pub dlp: DlpConfig,
    pub alerts: AlertsConfig,
    /// Frontier price overrides in the shape of `pricing.toml`
    /// (`[pricing.providers.<provider>.models.<key>]`). The pricing module parses them.
    pub pricing: Option<toml::Table>,
    pub hardware: HardwareConfig,
    pub log: LogConfig,
}

// ---------------------------------------------------------------------------------------------------------
// dashboard
// ---------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DashboardConfig {
    pub listen: SocketAddr,
    pub auth: bool,
    pub tls: TlsMode,
    pub token_file: PathBuf,
}

impl Default for DashboardConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], 8787)),
            auth: true,
            tls: TlsMode::Auto,
            token_file: PathBuf::from("dashboard.token"),
        }
    }
}

/// HTTPS on a listener, with the certificate the gateway's CA issues (`[tls]`). `auto` turns it on for
/// non-loopback binds, where tokens, keys and prompts would otherwise cross the network in the clear, and
/// leaves loopback on plain HTTP. The dashboard defaults to `auto`; the proxy listeners default to `off`,
/// because the clients of a local model server expect plain HTTP until they are told otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TlsMode {
    Auto,
    On,
    Off,
}

impl TlsMode {
    pub fn enabled_for(self, listen: SocketAddr) -> bool {
        match self {
            TlsMode::On => true,
            TlsMode::Off => false,
            TlsMode::Auto => !listen.ip().is_loopback(),
        }
    }
}

/// The gateway's certificate authority signs one server certificate for every HTTPS listener, dashboard
/// and proxy listeners alike (`gateway_core::tls`). It always carries `localhost`, the loopback addresses,
/// the host name and the specific addresses of TLS listeners; `sans` adds the other names clients use.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TlsConfig {
    /// Extra DNS names or IP addresses for the server certificate, e.g. `["gateway.lan", "192.168.1.10"]`.
    pub sans: Vec<String>,
}

// ---------------------------------------------------------------------------------------------------------
// backends
// ---------------------------------------------------------------------------------------------------------

/// One table per backend: the two local servers and the three cloud providers. The cloud providers are
/// enabled by default with no listener of their own, so they are reachable through the unified listener
/// and nothing else until an operator gives them a port.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BackendsConfig {
    pub lmstudio: BackendConfig,
    pub ollama: BackendConfig,
    pub anthropic: BackendConfig,
    pub gemini: BackendConfig,
    pub openai: BackendConfig,
}

impl Default for BackendsConfig {
    fn default() -> Self {
        Self {
            lmstudio: BackendConfig::defaults_for(Backend::LmStudio),
            ollama: BackendConfig::defaults_for(Backend::Ollama),
            anthropic: BackendConfig::defaults_for(Backend::Anthropic),
            gemini: BackendConfig::defaults_for(Backend::Gemini),
            openai: BackendConfig::defaults_for(Backend::OpenAi),
        }
    }
}

impl BackendsConfig {
    pub fn get(&self, backend: Backend) -> &BackendConfig {
        match backend {
            Backend::LmStudio => &self.lmstudio,
            Backend::Ollama => &self.ollama,
            Backend::Anthropic => &self.anthropic,
            Backend::Gemini => &self.gemini,
            Backend::OpenAi => &self.openai,
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (Backend, &BackendConfig)> {
        Backend::ALL.into_iter().map(move |b| (b, self.get(b)))
    }

    pub fn enabled(&self) -> impl Iterator<Item = (Backend, &BackendConfig)> {
        self.iter().filter(|(_, cfg)| cfg.enabled)
    }
}

/// The raw table: every field optional, merged onto the backend's own defaults.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct BackendsPatch {
    lmstudio: BackendPatch,
    ollama: BackendPatch,
    anthropic: BackendPatch,
    gemini: BackendPatch,
    openai: BackendPatch,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct BackendPatch {
    enabled: Option<bool>,
    listen: Option<Vec<SocketAddr>>,
    upstream: Option<String>,
    capture_bodies: Option<bool>,
    record_passthrough: Option<bool>,
    rewrite_include_usage: Option<bool>,
    tls: Option<TlsMode>,
    poll_interval_secs: Option<u64>,
    lms_bin: Option<PathBuf>,
    app_path: Option<PathBuf>,
}

impl<'de> Deserialize<'de> for BackendsConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let patch = BackendsPatch::deserialize(deserializer)?;
        Ok(Self {
            lmstudio: BackendConfig::defaults_for(Backend::LmStudio).apply(patch.lmstudio),
            ollama: BackendConfig::defaults_for(Backend::Ollama).apply(patch.ollama),
            anthropic: BackendConfig::defaults_for(Backend::Anthropic).apply(patch.anthropic),
            gemini: BackendConfig::defaults_for(Backend::Gemini).apply(patch.gemini),
            openai: BackendConfig::defaults_for(Backend::OpenAi).apply(patch.openai),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BackendConfig {
    pub enabled: bool,
    /// Every address the gateway listens on for this backend's clients. Empty means the backend is only
    /// reachable through the unified listener, which is the cloud providers' default.
    pub listen: Vec<SocketAddr>,
    /// Where the backend itself serves, as an `http://` or `https://` URL. A cloud provider's upstream
    /// must be `https://` unless it is on loopback.
    pub upstream: String,
    pub capture_bodies: bool,
    pub record_passthrough: bool,
    /// Add `stream_options.include_usage` to streaming OpenAI-format requests so the final frame carries
    /// token counts. Anthropic's and Gemini's compat endpoints send none without it either.
    pub rewrite_include_usage: bool,
    /// HTTPS on this backend's own listeners; `off` by default (see `TlsMode`).
    pub tls: TlsMode,
    /// Local backends only: seconds between model-list polls. Cloud providers are not polled.
    pub poll_interval_secs: u64,
    /// LM Studio only: the `lms` CLI, for version lookup on Linux.
    pub lms_bin: Option<PathBuf>,
    /// LM Studio only: the app bundle, for version lookup on macOS.
    pub app_path: Option<PathBuf>,
}

impl BackendConfig {
    pub fn defaults_for(backend: Backend) -> Self {
        let loopback = |port: u16| vec![SocketAddr::from(([127, 0, 0, 1], port))];
        let (listen, upstream, rewrite_include_usage) = match backend {
            Backend::LmStudio => (loopback(1235), "http://127.0.0.1:1234".to_string(), true),
            Backend::Ollama => (loopback(11435), "http://127.0.0.1:11434".to_string(), true),
            // The cloud providers' OpenAI-compatible endpoints send usage only when asked, like LM Studio
            // (settled by the Anthropic and Gemini captures in `fixtures/`).
            Backend::Anthropic => (Vec::new(), "https://api.anthropic.com".to_string(), true),
            Backend::Gemini => (
                Vec::new(),
                "https://generativelanguage.googleapis.com".to_string(),
                true,
            ),
            Backend::OpenAi => (Vec::new(), "https://api.openai.com".to_string(), true),
        };
        Self {
            enabled: true,
            listen,
            upstream,
            capture_bodies: true,
            record_passthrough: true,
            rewrite_include_usage,
            tls: TlsMode::Off,
            poll_interval_secs: 2,
            lms_bin: None,
            app_path: None,
        }
    }

    fn apply(mut self, patch: BackendPatch) -> Self {
        if let Some(v) = patch.enabled {
            self.enabled = v;
        }
        if let Some(v) = patch.listen {
            self.listen = v;
        }
        if let Some(v) = patch.upstream {
            self.upstream = v;
        }
        if let Some(v) = patch.capture_bodies {
            self.capture_bodies = v;
        }
        if let Some(v) = patch.record_passthrough {
            self.record_passthrough = v;
        }
        if let Some(v) = patch.rewrite_include_usage {
            self.rewrite_include_usage = v;
        }
        if let Some(v) = patch.tls {
            self.tls = v;
        }
        if let Some(v) = patch.poll_interval_secs {
            self.poll_interval_secs = v;
        }
        if patch.lms_bin.is_some() {
            self.lms_bin = patch.lms_bin;
        }
        if patch.app_path.is_some() {
            self.app_path = patch.app_path;
        }
        self
    }
}

// ---------------------------------------------------------------------------------------------------------
// unified listener
// ---------------------------------------------------------------------------------------------------------

/// The listener that serves every backend on one port and picks the target per request: an explicit path
/// prefix or a provider-specific path family first, then the `model` (alias, live local model lists,
/// `provider/model` spelling, `routes`, the built-in `claude-*` / `gemini-*` / `gpt-*` patterns), then a
/// provider header, then `default_backend`. The addresses and `tls` need a restart; everything else
/// reloads live.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UnifiedConfig {
    pub enabled: bool,
    pub listen: Vec<SocketAddr>,
    /// HTTPS on the unified listener; `off` by default (see `TlsMode`).
    pub tls: TlsMode,
    /// Where a request goes when nothing else decides.
    pub default_backend: Backend,
    /// Alias → model reference, e.g. `fast = "ollama/gemma4:12b-mlx"`. The alias is rewritten to its
    /// target in the forwarded request.
    pub aliases: BTreeMap<String, String>,
    /// Operator model-prefix routes, consulted after aliases and the live model lists.
    pub routes: Vec<RouteRule>,
}

impl Default for UnifiedConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            listen: vec![SocketAddr::from(([127, 0, 0, 1], 4000))],
            tls: TlsMode::Off,
            default_backend: Backend::LmStudio,
            aliases: BTreeMap::new(),
            routes: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteRule {
    /// Case-insensitive prefix of the requested model; the longest matching prefix wins.
    pub model_prefix: String,
    pub backend: Backend,
}

impl UnifiedConfig {
    /// The hot-reloadable part: everything but `enabled` and `listen`.
    pub fn routing_part(&self) -> (Backend, &BTreeMap<String, String>, &[RouteRule]) {
        (self.default_backend, &self.aliases, &self.routes)
    }
}

// ---------------------------------------------------------------------------------------------------------
// capture
// ---------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CaptureConfig {
    pub max_body_bytes: u64,
    pub retention_days: u32,
    pub passthrough_retention_hours: u32,
    pub hardware_raw_retention_hours: u32,
    pub max_buffered_request_bytes: u64,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            max_body_bytes: 4 * 1024 * 1024,
            retention_days: 7,
            passthrough_retention_hours: 24,
            hardware_raw_retention_hours: 48,
            max_buffered_request_bytes: 64 * 1024 * 1024,
        }
    }
}

// ---------------------------------------------------------------------------------------------------------
// dlp
// ---------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DlpConfig {
    pub enabled: bool,
    pub default_action: DlpAction,
    pub response_action: ResponseAction,
    pub redact_stored_copies: bool,
    pub notify_min_severity: Severity,
    pub known_secrets_file: PathBuf,
    pub max_scan_bytes: u64,
    pub cloud: DlpCloudConfig,
    /// Per-rule overrides keyed by rule id.
    pub rules: BTreeMap<String, RuleOverride>,
    /// Per-backend overrides keyed by `lmstudio` or `ollama`.
    pub backends: BTreeMap<String, BackendDlpOverride>,
    pub custom_rules: Vec<CustomRule>,
    pub allowlist: Vec<AllowlistEntry>,
}

impl Default for DlpConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            default_action: DlpAction::Alert,
            response_action: ResponseAction::Alert,
            redact_stored_copies: true,
            notify_min_severity: Severity::High,
            known_secrets_file: PathBuf::from("known-secrets.toml"),
            max_scan_bytes: 8 * 1024 * 1024,
            cloud: DlpCloudConfig::default(),
            rules: BTreeMap::new(),
            backends: BTreeMap::new(),
            custom_rules: Vec::new(),
            allowlist: Vec::new(),
        }
    }
}

/// Requests routed to Ollama `:cloud` models leave the machine; secrets in them are blocked by default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DlpCloudConfig {
    pub request_action: DlpAction,
    pub pii_action: DlpAction,
}

impl Default for DlpCloudConfig {
    fn default() -> Self {
        Self {
            request_action: DlpAction::Block,
            pii_action: DlpAction::Alert,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RuleOverride {
    pub enabled: Option<bool>,
    pub action: Option<DlpAction>,
    pub redact: Option<bool>,
    pub severity: Option<Severity>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BackendDlpOverride {
    pub request_action: Option<DlpAction>,
    pub response_action: Option<ResponseAction>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomRule {
    pub id: String,
    #[serde(default)]
    pub description: String,
    /// `regex` crate syntax (no look-around).
    pub regex: String,
    /// Which capture group holds the secret; 0 is the whole match.
    #[serde(default)]
    pub capture_group: usize,
    /// Case-insensitive literals that must appear in the text before the regex runs; empty means always.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Minimum Shannon entropy (bits per character) of the matched value.
    #[serde(default)]
    pub min_entropy: Option<f32>,
    #[serde(default = "CustomRule::default_severity")]
    pub severity: Severity,
    pub action: Option<DlpAction>,
    #[serde(default = "CustomRule::default_redact")]
    pub redact: bool,
}

impl CustomRule {
    fn default_severity() -> Severity {
        Severity::Medium
    }

    fn default_redact() -> bool {
        true
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AllowlistEntry {
    /// Limit the entry to one rule id.
    pub rule: Option<String>,
    /// A regex the matched value must satisfy.
    pub regex: Option<String>,
    /// The finding fingerprint (first 32 hex chars of the SHA-256 of the value), as shown in the UI.
    pub fingerprint: Option<String>,
    pub scope: AllowlistScope,
    /// Limit the entry to one backend.
    pub backend: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AllowlistScope {
    Request,
    Response,
    #[default]
    Both,
}

// ---------------------------------------------------------------------------------------------------------
// alerts
// ---------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AlertsConfig {
    pub throttle_secs: u64,
    pub desktop_notification: DesktopNotificationConfig,
    pub webhook: WebhookConfig,
}

impl Default for AlertsConfig {
    fn default() -> Self {
        Self {
            throttle_secs: 30,
            desktop_notification: DesktopNotificationConfig::default(),
            webhook: WebhookConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DesktopNotificationConfig {
    pub enabled: bool,
    pub min_severity: Severity,
}

impl Default for DesktopNotificationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_severity: Severity::High,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebhookConfig {
    pub enabled: bool,
    pub url: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub headers_file: Option<PathBuf>,
    pub min_severity: Severity,
    pub timeout_secs: u64,
}

impl Default for WebhookConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            url: None,
            headers: BTreeMap::new(),
            headers_file: None,
            min_severity: Severity::High,
            timeout_secs: 5,
        }
    }
}

// ---------------------------------------------------------------------------------------------------------
// hardware and log
// ---------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HardwareConfig {
    pub enabled: bool,
    pub sample_interval_secs: u64,
    pub gpu_source: GpuSource,
    pub powermetrics: PowermetricsMode,
}

impl Default for HardwareConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sample_interval_secs: 2,
            gpu_source: GpuSource::Auto,
            powermetrics: PowermetricsMode::Off,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GpuSource {
    Auto,
    Ioreg,
    NvidiaSmi,
    #[serde(rename = "none")]
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PowermetricsMode {
    Off,
    Sudo,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LogConfig {
    pub level: String,
    pub max_files: usize,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            max_files: 14,
        }
    }
}

// ---------------------------------------------------------------------------------------------------------
// loading and validation
// ---------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueLevel {
    /// Stops startup.
    Error,
    /// Logged once tracing is up.
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub level: IssueLevel,
    pub message: String,
}

impl Issue {
    fn error(message: impl Into<String>) -> Self {
        Self {
            level: IssueLevel::Error,
            message: message.into(),
        }
    }

    fn warning(message: impl Into<String>) -> Self {
        Self {
            level: IssueLevel::Warning,
            message: message.into(),
        }
    }
}

impl Config {
    /// A missing file is all defaults; an unreadable or invalid one is an error.
    pub fn load(path: &Path) -> Result<Config> {
        if !path.exists() {
            return Ok(Config::default());
        }
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("parse {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Config> {
        // toml's Display carries the line and column; anyhow keeps it as the root cause.
        toml::from_str(text).map_err(|err| anyhow::anyhow!("{err}"))
    }

    pub fn has_errors(issues: &[Issue]) -> bool {
        issues.iter().any(|i| i.level == IssueLevel::Error)
    }

    /// Structural checks shared by every crate. Regexes are compiled by the DLP engine, file modes are
    /// checked by the binary once it knows the data dir.
    pub fn validate(&self) -> Vec<Issue> {
        let mut issues = Vec::new();
        let dash = self.dashboard.listen;
        if !self.dashboard.auth && !dash.ip().is_loopback() {
            issues.push(Issue::error(format!(
                "dashboard.auth = false is only allowed on a loopback address; dashboard.listen is {dash}"
            )));
        }
        if self.dashboard.tls == TlsMode::Off && !dash.ip().is_loopback() {
            issues.push(Issue::warning(format!(
                "dashboard.tls = \"off\" on {dash}: the access token and captured prompts would cross the network unencrypted"
            )));
        }

        let mut listeners: Vec<(String, SocketAddr)> = vec![("dashboard.listen".to_string(), dash)];
        // (config key, host, port) of every enabled upstream, for the unified listener's self-proxy check.
        let mut upstreams: Vec<(String, String, u16)> = Vec::new();
        for (backend, cfg) in self.backends.iter() {
            let key = format!("backends.{backend}");
            if backend != Backend::LmStudio && (cfg.lms_bin.is_some() || cfg.app_path.is_some()) {
                issues.push(Issue::warning(format!(
                    "{key}.lms_bin and {key}.app_path only apply to LM Studio and are ignored"
                )));
            }
            if !cfg.enabled {
                continue;
            }
            if cfg.listen.is_empty() && !self.unified.enabled {
                if backend.is_local() {
                    issues.push(Issue::error(format!(
                        "{key}.listen must name at least one address while the backend is enabled and [unified] is disabled"
                    )));
                } else {
                    issues.push(Issue::warning(format!(
                        "{key} is enabled but reachable from no listener: it has no listen address and [unified] is disabled"
                    )));
                }
            }
            match upstream_host_port(&cfg.upstream) {
                None => issues.push(Issue::error(format!(
                    "{key}.upstream must be an http:// or https:// URL with a host, got {:?}",
                    cfg.upstream
                ))),
                Some((host, port)) => {
                    if backend.is_cloud_provider()
                        && is_plain_http(&cfg.upstream)
                        && !host_is_loopback(&host)
                    {
                        issues.push(Issue::error(format!(
                            "{key}.upstream {} is plain http:// to a host off this machine: API keys would cross the network unencrypted",
                            cfg.upstream
                        )));
                    }
                    for addr in &cfg.listen {
                        if addr.port() == port && host_overlaps(&host, addr.ip()) {
                            issues.push(Issue::error(format!(
                                "{key}.listen {addr} is the same socket as its upstream {}: the gateway would proxy to itself",
                                cfg.upstream
                            )));
                        }
                    }
                    upstreams.push((key.clone(), host, port));
                }
            }
            if backend.is_local() && cfg.poll_interval_secs == 0 {
                issues.push(Issue::error(format!(
                    "{key}.poll_interval_secs must be at least 1"
                )));
            }
            for addr in &cfg.listen {
                listeners.push((format!("{key}.listen"), *addr));
            }
        }

        let unified = &self.unified;
        if unified.enabled {
            if unified.listen.is_empty() {
                issues.push(Issue::error(
                    "unified.listen must name at least one address while [unified] is enabled",
                ));
            }
            for addr in &unified.listen {
                for (key, host, port) in &upstreams {
                    if addr.port() == *port && host_overlaps(host, addr.ip()) {
                        issues.push(Issue::error(format!(
                            "unified.listen {addr} is the same socket as {key}.upstream: the gateway would proxy to itself"
                        )));
                    }
                }
                listeners.push(("unified.listen".to_string(), *addr));
            }
            if !self.backends.get(unified.default_backend).enabled {
                issues.push(Issue::error(format!(
                    "unified.default_backend = {:?} names a disabled backend",
                    unified.default_backend.as_str()
                )));
            }
        }
        let mut alias_names: Vec<String> = Vec::new();
        for (name, target) in &unified.aliases {
            let alias_key = format!("unified.aliases.{name}");
            let name = name.trim();
            if name.is_empty()
                || name.len() > MAX_MODEL_REF_LEN
                || name.chars().any(|c| c.is_whitespace() || c.is_control())
            {
                issues.push(Issue::error(format!(
                    "{alias_key}: alias names are 1 to {MAX_MODEL_REF_LEN} bytes with no whitespace or control characters"
                )));
            }
            if provider_id(name).is_some() {
                issues.push(Issue::error(format!(
                    "{alias_key}: an alias may not be named after a backend"
                )));
            }
            let lower = name.to_ascii_lowercase();
            if alias_names.contains(&lower) {
                issues.push(Issue::error(format!(
                    "{alias_key} is a duplicate of another alias differing only in case"
                )));
            }
            alias_names.push(lower);
            match ModelRef::parse(target) {
                Err(err) => issues.push(Issue::error(format!("{alias_key}: {err}"))),
                Ok(target) => {
                    if let Some(backend) = target.backend
                        && !self.backends.get(backend).enabled
                    {
                        issues.push(Issue::error(format!(
                            "{alias_key} points at {backend}, which is disabled"
                        )));
                    }
                    if unified
                        .aliases
                        .keys()
                        .any(|other| other.trim().eq_ignore_ascii_case(&target.model))
                    {
                        issues.push(Issue::error(format!(
                            "{alias_key}: an alias may not point at another alias ({:?})",
                            target.model
                        )));
                    }
                }
            }
        }
        let mut prefixes: Vec<String> = Vec::new();
        for (i, route) in unified.routes.iter().enumerate() {
            let prefix = route.model_prefix.trim().to_ascii_lowercase();
            if prefix.is_empty() {
                issues.push(Issue::error(format!(
                    "unified.routes[{i}].model_prefix is empty"
                )));
            }
            if !self.backends.get(route.backend).enabled {
                issues.push(Issue::error(format!(
                    "unified.routes[{i}] names {}, which is disabled",
                    route.backend
                )));
            }
            if prefixes.contains(&prefix) {
                issues.push(Issue::warning(format!(
                    "unified.routes[{i}] repeats model_prefix {:?}; the first entry wins",
                    route.model_prefix
                )));
            }
            prefixes.push(prefix);
        }

        let mut plain: Vec<String> = Vec::new();
        for (backend, cfg) in self.backends.enabled() {
            for addr in &cfg.listen {
                if !addr.ip().is_loopback() && !cfg.tls.enabled_for(*addr) {
                    plain.push(format!("backends.{backend}.listen {addr}"));
                }
            }
        }
        if unified.enabled {
            for addr in &unified.listen {
                if !addr.ip().is_loopback() && !unified.tls.enabled_for(*addr) {
                    plain.push(format!("unified.listen {addr}"));
                }
            }
        }
        if !plain.is_empty() {
            issues.push(Issue::warning(format!(
                "plain HTTP off loopback on {}: bearer tokens and provider keys cross the network unencrypted; set tls = \"on\" in that section and have clients trust tls/ca.pem",
                plain.join(", ")
            )));
        }
        let mut names: Vec<SanName> = Vec::new();
        for (i, name) in self.tls.sans.iter().enumerate() {
            match SanName::parse(name) {
                Err(err) => issues.push(Issue::error(format!("tls.sans[{i}]: {err}"))),
                Ok(parsed) => {
                    if names.contains(&parsed) {
                        issues.push(Issue::warning(format!("tls.sans[{i}] repeats {parsed}")));
                    }
                    names.push(parsed);
                }
            }
        }

        for i in 0..listeners.len() {
            for j in (i + 1)..listeners.len() {
                let (a, b) = (listeners[i].1, listeners[j].1);
                if a.port() == b.port() && ips_overlap(a.ip(), b.ip()) {
                    issues.push(Issue::error(format!(
                        "{} ({a}) and {} ({b}) would bind the same port",
                        listeners[i].0, listeners[j].0
                    )));
                }
            }
        }

        if self.capture.max_body_bytes == 0 {
            issues.push(Issue::error("capture.max_body_bytes must be at least 1"));
        }
        if self.capture.max_buffered_request_bytes < self.capture.max_body_bytes {
            issues.push(Issue::warning(format!(
                "capture.max_buffered_request_bytes ({}) is below capture.max_body_bytes ({}): requests over the buffer cap are refused with 413 before they could be stored",
                self.capture.max_buffered_request_bytes, self.capture.max_body_bytes
            )));
        }

        if self.dlp.max_scan_bytes == 0 {
            issues.push(Issue::error("dlp.max_scan_bytes must be at least 1"));
        }
        for key in self.dlp.backends.keys() {
            if key.parse::<Backend>().is_err() {
                issues.push(Issue::error(format!(
                    "dlp.backends.{key}: unknown backend (expected one of {})",
                    backend_ids()
                )));
            }
        }
        let mut ids = HashSet::new();
        for rule in &self.dlp.custom_rules {
            if rule.id.trim().is_empty() {
                issues.push(Issue::error("dlp.custom_rules: a rule has an empty id"));
                continue;
            }
            if !ids.insert(rule.id.as_str()) {
                issues.push(Issue::error(format!(
                    "dlp.custom_rules: duplicate id {:?}",
                    rule.id
                )));
            }
            if rule.regex.trim().is_empty() {
                issues.push(Issue::error(format!(
                    "dlp.custom_rules.{}: regex is empty",
                    rule.id
                )));
            }
        }
        for (i, entry) in self.dlp.allowlist.iter().enumerate() {
            if entry.rule.is_none() && entry.regex.is_none() && entry.fingerprint.is_none() {
                issues.push(Issue::error(format!(
                    "dlp.allowlist[{i}] needs at least one of rule, regex or fingerprint"
                )));
            }
            if let Some(backend) = &entry.backend
                && backend.parse::<Backend>().is_err()
            {
                issues.push(Issue::error(format!(
                    "dlp.allowlist[{i}].backend {backend:?} is not one of {}",
                    backend_ids()
                )));
            }
        }

        let webhook = &self.alerts.webhook;
        if webhook.enabled {
            match &webhook.url {
                None => issues.push(Issue::error(
                    "alerts.webhook.enabled = true needs alerts.webhook.url",
                )),
                Some(url) if !(url.starts_with("http://") || url.starts_with("https://")) => {
                    issues.push(Issue::error(
                        "alerts.webhook.url must start with http:// or https://",
                    ));
                }
                Some(_) => {}
            }
            if webhook.timeout_secs == 0 {
                issues.push(Issue::error(
                    "alerts.webhook.timeout_secs must be at least 1",
                ));
            }
        }
        if self.alerts.throttle_secs == 0 {
            issues.push(Issue::warning(
                "alerts.throttle_secs = 0 disables throttling; one noisy rule can flood notifications",
            ));
        }

        if self.hardware.sample_interval_secs == 0 {
            issues.push(Issue::error(
                "hardware.sample_interval_secs must be at least 1",
            ));
        }
        if self.log.level.trim().is_empty() {
            issues.push(Issue::error("log.level must not be empty"));
        }
        issues
    }
}

/// The host and port of an `http://` or `https://` URL, with the scheme's default port when none is
/// given. `None` for anything else, including other schemes.
pub fn upstream_host_port(upstream: &str) -> Option<(String, u16)> {
    let (scheme, rest) = upstream.trim().split_once("://")?;
    let default_port = match scheme.to_ascii_lowercase().as_str() {
        "http" => 80,
        "https" => 443,
        _ => return None,
    };
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    if authority.is_empty() {
        return None;
    }
    if let Some(bracketed) = authority.strip_prefix('[') {
        let (host, after) = bracketed.split_once(']')?;
        let port = match after.strip_prefix(':') {
            Some(port) => port.parse().ok()?,
            None => default_port,
        };
        return Some((host.to_string(), port));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() => Some((host.to_string(), port.parse().ok()?)),
        _ => Some((authority.to_string(), default_port)),
    }
}

/// The backend ids as validation messages list them.
fn backend_ids() -> String {
    Backend::ALL
        .iter()
        .map(|b| b.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn is_plain_http(upstream: &str) -> bool {
    upstream.trim().to_ascii_lowercase().starts_with("http://")
}

fn host_is_loopback(host: &str) -> bool {
    host.parse::<IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or_else(|_| host.eq_ignore_ascii_case("localhost"))
}

/// Two bind addresses collide when they are equal or either is the unspecified address of a family that
/// covers the other.
fn ips_overlap(a: IpAddr, b: IpAddr) -> bool {
    if a == b {
        return true;
    }
    let same_family = a.is_ipv4() == b.is_ipv4();
    same_family && (a.is_unspecified() || b.is_unspecified())
}

/// Whether connecting to `host` could reach a socket bound on `ip`.
fn host_overlaps(host: &str, ip: IpAddr) -> bool {
    match host.parse::<IpAddr>() {
        Ok(h) => {
            ips_overlap(h, ip) || (h.is_loopback() && (ip.is_loopback() || ip.is_unspecified()))
        }
        Err(_) => {
            host.eq_ignore_ascii_case("localhost") && (ip.is_loopback() || ip.is_unspecified())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn errors(cfg: &Config) -> Vec<String> {
        cfg.validate()
            .into_iter()
            .filter(|i| i.level == IssueLevel::Error)
            .map(|i| i.message)
            .collect()
    }

    #[test]
    fn an_empty_file_is_all_defaults() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
    }

    #[test]
    fn the_sample_config_matches_the_defaults() {
        let parsed = Config::parse(SAMPLE).expect("sample parses");
        assert_eq!(
            parsed,
            Config::default(),
            "config.sample.toml drifted from Config::default()"
        );
    }

    #[test]
    fn the_defaults_validate_cleanly() {
        assert!(
            Config::default().validate().is_empty(),
            "{:?}",
            Config::default().validate()
        );
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let err =
            Config::parse("[dashboard]\nlisten = \"127.0.0.1:1\"\nlisten_port = 2\n").unwrap_err();
        assert!(err.to_string().contains("listen_port"), "{err}");
        let err = Config::parse("[backends.ollama]\nupstreem = \"http://x\"\n").unwrap_err();
        assert!(err.to_string().contains("upstreem"), "{err}");
    }

    #[test]
    fn a_partial_backend_table_keeps_that_backends_own_defaults() {
        let cfg = Config::parse("[backends.ollama]\ncapture_bodies = false\n").unwrap();
        assert!(!cfg.backends.ollama.capture_bodies);
        assert_eq!(
            cfg.backends.ollama.listen,
            vec![SocketAddr::from(([127, 0, 0, 1], 11435))]
        );
        assert_eq!(cfg.backends.ollama.upstream, "http://127.0.0.1:11434");
        assert_eq!(
            cfg.backends.lmstudio,
            BackendConfig::defaults_for(Backend::LmStudio)
        );
    }

    #[test]
    fn listeners_accept_several_addresses() {
        let cfg = Config::parse(
            "[backends.ollama]\nlisten = [\"0.0.0.0:11435\", \"127.0.0.1:11400\"]\nupstream = \"http://127.0.0.1:11434\"\n",
        )
        .unwrap();
        assert_eq!(cfg.backends.ollama.listen.len(), 2);
        assert!(errors(&cfg).is_empty(), "{:?}", errors(&cfg));
    }

    #[test]
    fn a_listener_equal_to_its_upstream_is_an_error() {
        let cfg = Config::parse(
            "[backends.lmstudio]\nlisten = [\"0.0.0.0:31337\"]\nupstream = \"http://127.0.0.1:31337\"\n",
        )
        .unwrap();
        let errs = errors(&cfg);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("proxy to itself"), "{errs:?}");
        let cfg = Config::parse(
            "[backends.lmstudio]\nlisten = [\"127.0.0.1:1235\"]\nupstream = \"http://localhost:1235\"\n",
        )
        .unwrap();
        assert!(errors(&cfg)[0].contains("proxy to itself"));
    }

    #[test]
    fn two_listeners_on_one_port_are_an_error() {
        let cfg = Config::parse("[backends.lmstudio]\nlisten = [\"127.0.0.1:11435\"]\n").unwrap();
        let errs = errors(&cfg);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("same port"), "{errs:?}");
        let cfg = Config::parse("[dashboard]\nlisten = \"0.0.0.0:11435\"\n").unwrap();
        assert!(errors(&cfg)[0].contains("same port"));
        let cfg = Config::parse("[unified]\nlisten = [\"0.0.0.0:8787\"]\n").unwrap();
        let errs = errors(&cfg);
        assert!(
            errs.iter()
                .any(|e| e.contains("unified.listen") && e.contains("same port")),
            "{errs:?}"
        );
    }

    #[test]
    fn cloud_providers_default_to_no_listener_and_https_upstreams() {
        let cfg = Config::default();
        for backend in Backend::CLOUD_PROVIDERS {
            let b = cfg.backends.get(backend);
            assert!(b.enabled, "{backend}");
            assert!(b.listen.is_empty(), "{backend}");
            assert!(
                b.upstream.starts_with("https://"),
                "{backend}: {}",
                b.upstream
            );
        }
        for backend in Backend::CLOUD_PROVIDERS {
            assert!(
                cfg.backends.get(backend).rewrite_include_usage,
                "{backend}: usage comes only with stream_options"
            );
        }
        let cfg =
            Config::parse("[backends.gemini]\nupstream = \"http://gemini.example.net\"\n").unwrap();
        let errs = errors(&cfg);
        assert!(
            errs.iter()
                .any(|e| e.contains("backends.gemini.upstream") && e.contains("unencrypted")),
            "{errs:?}"
        );
        let cfg = Config::parse("[backends.gemini]\nupstream = \"http://127.0.0.1:9\"\n").unwrap();
        assert!(
            errors(&cfg).is_empty(),
            "loopback http is fine for tests: {:?}",
            errors(&cfg)
        );
        let cfg = Config::parse("[backends.openai]\nlms_bin = \"/x\"\n").unwrap();
        assert!(
            cfg.validate()
                .iter()
                .any(|i| i.message.contains("only apply to LM Studio"))
        );
    }

    #[test]
    fn an_empty_listen_needs_the_unified_listener() {
        let cfg = Config::parse("[backends.ollama]\nlisten = []\n").unwrap();
        assert!(errors(&cfg).is_empty(), "{:?}", errors(&cfg));
        let cfg =
            Config::parse("[backends.ollama]\nlisten = []\n[unified]\nenabled = false\n").unwrap();
        let errs = errors(&cfg);
        assert!(
            errs.iter().any(|e| e.contains("backends.ollama.listen")),
            "{errs:?}"
        );
        let cfg = Config::parse("[unified]\nenabled = false\n").unwrap();
        assert!(errors(&cfg).is_empty(), "{:?}", errors(&cfg));
        assert!(
            cfg.validate().iter().any(|i| i.level == IssueLevel::Warning
                && i.message
                    .contains("backends.anthropic is enabled but reachable from no listener")),
            "{:?}",
            cfg.validate()
        );
        let cfg = Config::parse("[unified]\nlisten = []\n").unwrap();
        assert!(errors(&cfg)[0].contains("unified.listen must name"));
    }

    #[test]
    fn the_unified_listener_is_checked_against_every_upstream_and_its_default() {
        let cfg = Config::parse("[unified]\nlisten = [\"127.0.0.1:11434\"]\n").unwrap();
        let errs = errors(&cfg);
        assert!(
            errs.iter().any(|e| e.contains("unified.listen")
                && e.contains("backends.ollama.upstream")
                && e.contains("proxy to itself")),
            "{errs:?}"
        );
        let cfg = Config::parse("[backends.lmstudio]\nenabled = false\n").unwrap();
        let errs = errors(&cfg);
        assert!(
            errs.iter()
                .any(|e| e.contains("unified.default_backend") && e.contains("disabled")),
            "{errs:?}"
        );
        let cfg = Config::parse(
            "[backends.lmstudio]\nenabled = false\n[unified]\ndefault_backend = \"ollama\"\n",
        )
        .unwrap();
        assert!(errors(&cfg).is_empty(), "{:?}", errors(&cfg));
    }

    #[test]
    fn aliases_and_routes_are_validated() {
        let cfg = Config::parse(
            "[unified.aliases]\nfast = \"ollama/gemma4:12b-mlx\"\nsmart = \"claude-opus-5-5\"\nopenai = \"gpt-5\"\nchain = \"fast\"\nbad = \"openai/\"\noff = \"openai/gpt-5\"\nFAST = \"ollama/x\"\n\"two words\" = \"x\"\n[backends.openai]\nenabled = false\n[[unified.routes]]\nmodel_prefix = \"mistral-\"\nbackend = \"ollama\"\n[[unified.routes]]\nmodel_prefix = \"\"\nbackend = \"openai\"\n[[unified.routes]]\nmodel_prefix = \"Mistral-\"\nbackend = \"lmstudio\"\n",
        )
        .unwrap();
        let issues = cfg.validate();
        let errs = errors(&cfg);
        let has = |needle: &str| errs.iter().any(|e| e.contains(needle));
        assert!(
            has("unified.aliases.openai: an alias may not be named after a backend"),
            "{errs:?}"
        );
        assert!(
            has("unified.aliases.chain: an alias may not point at another alias"),
            "{errs:?}"
        );
        assert!(
            has("unified.aliases.bad: `openai/` names a provider but no model"),
            "{errs:?}"
        );
        assert!(
            has("unified.aliases.off points at openai, which is disabled"),
            "{errs:?}"
        );
        assert!(has("differing only in case"), "{errs:?}");
        assert!(has("unified.aliases.two words: alias names"), "{errs:?}");
        assert!(has("unified.routes[1].model_prefix is empty"), "{errs:?}");
        assert!(
            has("unified.routes[1] names openai, which is disabled"),
            "{errs:?}"
        );
        assert!(
            issues.iter().any(|i| i.level == IssueLevel::Warning
                && i.message.contains("unified.routes[2] repeats")),
            "{issues:?}"
        );
        assert!(
            !has("unified.aliases.fast:"),
            "the duplicate note is the only complaint about fast: {errs:?}"
        );
        assert!(!has("unified.aliases.smart"), "{errs:?}");
        let cfg = Config::parse("[unified.aliases]\nfast = \"ollama/gemma4:12b-mlx\"\n[[unified.routes]]\nmodel_prefix = \"mistral-\"\nbackend = \"ollama\"\n").unwrap();
        assert!(errors(&cfg).is_empty(), "{:?}", errors(&cfg));
        assert_eq!(cfg.unified.routes[0].backend, Backend::Ollama);
    }

    #[test]
    fn auth_off_is_only_allowed_on_loopback() {
        let cfg = Config::parse("[dashboard]\nauth = false\n").unwrap();
        assert!(errors(&cfg).is_empty());
        let cfg = Config::parse("[dashboard]\nauth = false\nlisten = \"0.0.0.0:8787\"\n").unwrap();
        assert!(errors(&cfg)[0].contains("dashboard.auth"));
    }

    #[test]
    fn tls_off_on_the_network_warns_but_runs() {
        let cfg = Config::parse("[dashboard]\ntls = \"off\"\nlisten = \"0.0.0.0:8787\"\n").unwrap();
        let issues = cfg.validate();
        assert!(errors(&cfg).is_empty());
        assert!(
            issues
                .iter()
                .any(|i| i.level == IssueLevel::Warning && i.message.contains("tls"))
        );
    }

    #[test]
    fn tls_names_are_checked_and_plain_http_off_loopback_warns_once() {
        let config = Config::parse(
            "[backends.ollama]\nlisten = [\"0.0.0.0:11435\"]\n[unified]\nlisten = [\"0.0.0.0:4000\"]\n[tls]\nsans = [\"gateway.lan\", \"192.168.1.10\", \"bad_name\", \"Gateway.lan\"]\n",
        )
        .unwrap();
        assert_eq!(config.backends.ollama.tls, TlsMode::Off);
        assert_eq!(config.unified.tls, TlsMode::Off);
        let issues = config.validate();
        let errors: Vec<_> = issues
            .iter()
            .filter(|i| i.level == IssueLevel::Error)
            .map(|i| i.message.as_str())
            .collect();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].starts_with("tls.sans[2]: "), "{errors:?}");
        let warnings: Vec<_> = issues
            .iter()
            .filter(|i| i.level == IssueLevel::Warning)
            .map(|i| i.message.as_str())
            .collect();
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("tls.sans[3] repeats gateway.lan")),
            "{warnings:?}"
        );
        let plain: Vec<_> = warnings
            .iter()
            .filter(|w| w.starts_with("plain HTTP off loopback on "))
            .collect();
        assert_eq!(
            plain.len(),
            1,
            "one line names every listener: {warnings:?}"
        );
        assert!(plain[0].contains("backends.ollama.listen 0.0.0.0:11435"));
        assert!(plain[0].contains("unified.listen 0.0.0.0:4000"));

        let config = Config::parse(
            "[backends.ollama]\nlisten = [\"0.0.0.0:11435\"]\ntls = \"on\"\n[unified]\nlisten = [\"0.0.0.0:4000\"]\ntls = \"auto\"\n",
        )
        .unwrap();
        assert_eq!(config.backends.ollama.tls, TlsMode::On);
        assert_eq!(config.unified.tls, TlsMode::Auto);
        assert!(
            !config
                .validate()
                .iter()
                .any(|i| i.message.starts_with("plain HTTP off loopback")),
            "TLS on both: nothing to warn about"
        );
        assert!(Config::parse("[tls]\nnames = []\n").is_err(), "unknown key");
    }

    #[test]
    fn tls_auto_follows_the_bind_address() {
        assert!(!TlsMode::Auto.enabled_for("127.0.0.1:8787".parse().unwrap()));
        assert!(!TlsMode::Auto.enabled_for("[::1]:8787".parse().unwrap()));
        assert!(TlsMode::Auto.enabled_for("0.0.0.0:8787".parse().unwrap()));
        assert!(TlsMode::Auto.enabled_for("192.168.1.10:8787".parse().unwrap()));
        assert!(TlsMode::On.enabled_for("127.0.0.1:8787".parse().unwrap()));
        assert!(!TlsMode::Off.enabled_for("0.0.0.0:8787".parse().unwrap()));
    }

    #[test]
    fn the_webhook_needs_a_url_when_enabled() {
        let cfg = Config::parse("[alerts.webhook]\nenabled = true\n").unwrap();
        assert!(errors(&cfg)[0].contains("alerts.webhook.url"));
        let cfg = Config::parse("[alerts.webhook]\nenabled = true\nurl = \"ftp://x\"\n").unwrap();
        assert!(errors(&cfg)[0].contains("http://"));
        let cfg = Config::parse("[alerts.webhook]\nenabled = true\nurl = \"https://x\"\n").unwrap();
        assert!(errors(&cfg).is_empty());
    }

    #[test]
    fn dlp_tables_are_checked_for_backend_names_and_rule_ids() {
        let cfg = Config::parse("[dlp.backends.vllm]\nrequest_action = \"record\"\n").unwrap();
        assert!(errors(&cfg)[0].contains("dlp.backends.vllm"));
        let cfg = Config::parse(
            "[[dlp.custom_rules]]\nid = \"a\"\nregex = \"x\"\n[[dlp.custom_rules]]\nid = \"a\"\nregex = \"y\"\n",
        )
        .unwrap();
        assert!(errors(&cfg)[0].contains("duplicate id"));
        let cfg = Config::parse("[[dlp.allowlist]]\nscope = \"request\"\n").unwrap();
        assert!(errors(&cfg)[0].contains("dlp.allowlist[0]"));
        let cfg =
            Config::parse("[dlp.rules.email]\naction = \"record\"\nredact = false\n").unwrap();
        assert_eq!(cfg.dlp.rules["email"].action, Some(DlpAction::Record));
        assert!(errors(&cfg).is_empty());
    }

    #[test]
    fn pricing_overrides_are_kept_as_a_table_for_the_pricing_module() {
        let cfg = Config::parse(
            "[pricing.providers.google.models.gemini-3-1-pro]\ninput_per_mtok_usd = 4.0\noutput_per_mtok_usd = 18.0\n",
        )
        .unwrap();
        let table = cfg.pricing.expect("pricing table");
        assert!(table.contains_key("providers"));
    }

    #[test]
    fn upstream_urls_yield_host_and_port() {
        assert_eq!(
            upstream_host_port("http://127.0.0.1:31337"),
            Some(("127.0.0.1".into(), 31337))
        );
        assert_eq!(
            upstream_host_port("http://localhost/"),
            Some(("localhost".into(), 80))
        );
        assert_eq!(
            upstream_host_port("https://lm.example.net/v1"),
            Some(("lm.example.net".into(), 443))
        );
        assert_eq!(
            upstream_host_port("http://[::1]:1234/x"),
            Some(("::1".into(), 1234))
        );
        assert_eq!(
            upstream_host_port("http://user:pw@host:9/"),
            Some(("host".into(), 9))
        );
        assert_eq!(upstream_host_port("ftp://host"), None);
        assert_eq!(upstream_host_port("host:1234"), None);
        assert_eq!(upstream_host_port("http://"), None);
        assert_eq!(upstream_host_port("http://host:notaport"), None);
    }

    #[test]
    fn overlapping_binds_are_detected_per_family() {
        assert!(ips_overlap(
            "0.0.0.0".parse().unwrap(),
            "192.168.1.2".parse().unwrap()
        ));
        assert!(ips_overlap(
            "127.0.0.1".parse().unwrap(),
            "127.0.0.1".parse().unwrap()
        ));
        assert!(!ips_overlap(
            "127.0.0.1".parse().unwrap(),
            "127.0.0.2".parse().unwrap()
        ));
        assert!(!ips_overlap(
            "0.0.0.0".parse().unwrap(),
            "::1".parse().unwrap()
        ));
        assert!(host_overlaps("localhost", "0.0.0.0".parse().unwrap()));
        assert!(!host_overlaps("lm.example.net", "0.0.0.0".parse().unwrap()));
    }
}
