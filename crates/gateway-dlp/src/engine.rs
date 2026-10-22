//! The engine: scan text with every rule, walk JSON bodies leaf by leaf so findings carry a JSON path,
//! match known secrets in every encoding, apply the allowlist, resolve policy, and produce the findings
//! (fingerprint and masked preview, never the value), the block decision and the redacted copy.

use chrono::Utc;
use gateway_core::config::{AllowlistEntry, AllowlistScope, DlpConfig};
use gateway_core::types::{
    Backend, Direction, DlpAction, Finding, FindingCategory, ResponseAction, Severity,
};
use regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::entropy::shannon_bits_per_char;
use crate::known_secrets::KnownSecrets;
use crate::policy::{EffectivePolicy, Policy};
use crate::redact::{placeholder, redact_spans, redact_values};
use crate::rules::{CompiledRule, KNOWN_SECRET_RULE, Rule, RuleSet, builtin_rules};

/// A PEM block is extended at most this far past its BEGIN line when no END line follows.
const PEM_MAX: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub rule_id: String,
    pub category: FindingCategory,
    pub severity: Severity,
    pub start: usize,
    pub end: usize,
    pub value: String,
}

#[derive(Debug, Clone)]
pub struct ScanInput {
    pub request_id: Uuid,
    pub backend: Backend,
    pub is_cloud: bool,
    pub direction: Direction,
}

#[derive(Debug, Clone, Default)]
pub struct Outcome {
    pub findings: Vec<Finding>,
    /// The rule that blocked the request, when one did.
    pub blocked_by: Option<String>,
    /// The redacted copy to store; `None` when nothing needed redacting.
    pub redacted: Option<Vec<u8>>,
    pub allowlisted: usize,
    pub scan_truncated: bool,
}

#[derive(Debug)]
struct CompiledAllow {
    rule: Option<String>,
    regex: Option<Regex>,
    fingerprint: Option<String>,
    scope: AllowlistScope,
    backend: Option<Backend>,
}

#[derive(Debug)]
pub struct Engine {
    enabled: bool,
    rules: RuleSet,
    known: KnownSecrets,
    policy: Policy,
    allowlist: Vec<CompiledAllow>,
    max_scan_bytes: usize,
}

/// Dummy keys local clients send to servers that want a key but check none.
const DEFAULT_ALLOWLIST_VALUES: &str = r"(?i)^(sk-)?(lm-?studio|ollama|no-?key|not-needed|none|null|x+|dummy|placeholder|changeme|example|test|secret)$";

impl Engine {
    pub fn from_config(config: &DlpConfig, known: KnownSecrets) -> anyhow::Result<Engine> {
        let mut rules = builtin_rules();
        for custom in &config.custom_rules {
            if rules.iter().any(|r| r.id == custom.id) {
                anyhow::bail!(
                    "dlp.custom_rules: id {:?} is a built-in rule; override it under [dlp.rules.{}] instead",
                    custom.id,
                    custom.id
                );
            }
            rules.push(Rule::from_custom(custom));
        }
        let policy = Policy::new(config);
        let enabled_for = |r: &Rule| {
            config
                .rules
                .get(&r.id)
                .and_then(|o| o.enabled)
                .unwrap_or(true)
        };
        let rules = RuleSet::compile(rules, enabled_for)?;
        let mut allowlist = vec![CompiledAllow {
            rule: None,
            regex: Some(Regex::new(DEFAULT_ALLOWLIST_VALUES).expect("static regex")),
            fingerprint: None,
            scope: AllowlistScope::Both,
            backend: None,
        }];
        for entry in &config.allowlist {
            allowlist.push(compile_allow(entry)?);
        }
        Ok(Engine {
            enabled: config.enabled,
            rules,
            known,
            policy,
            allowlist,
            max_scan_bytes: usize::try_from(config.max_scan_bytes)
                .unwrap_or(usize::MAX)
                .max(1),
        })
    }

    /// An engine that finds nothing: `dlp.enabled = false`.
    pub fn disabled() -> Engine {
        let mut engine = Engine::from_config(&DlpConfig::default(), KnownSecrets::default())
            .expect("defaults compile");
        engine.enabled = false;
        engine
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn rules(&self) -> &[CompiledRule] {
        self.rules.rules()
    }

    pub fn known_secret_count(&self) -> usize {
        self.known.len()
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// Every rule and known-secret match in `text`, overlapping matches resolved to the most severe (then
    /// longest). Pure: no policy, no allowlist.
    pub fn scan_text(&self, text: &str) -> Vec<Match> {
        if !self.enabled || text.is_empty() {
            return Vec::new();
        }
        let lower = text.to_ascii_lowercase();
        let mut matches = Vec::new();
        for compiled in self.rules.candidates(text) {
            let rule = &compiled.rule;
            if !rule.keywords.is_empty()
                && !rule.keywords.iter().any(|k| lower.contains(k.as_str()))
            {
                continue;
            }
            for caps in compiled.regex.captures_iter(text) {
                let Some(group) = caps.get(rule.capture_group) else {
                    continue;
                };
                let (mut start, mut end) = (group.start(), group.end());
                let mut value = group.as_str();
                if rule.extend_to_end_marker {
                    end = pem_end(text, end);
                    value = &text[start..end];
                }
                if let Some(min) = rule.min_entropy
                    && shannon_bits_per_char(value) < min
                {
                    continue;
                }
                if let Some(validator) = rule.validator
                    && !validator.accepts(value)
                {
                    continue;
                }
                if start == end {
                    continue;
                }
                // Trim stray quotes a loose group may have swallowed.
                while value.ends_with(['"', '\'']) && end > start + 1 {
                    end -= 1;
                    value = &text[start..end];
                }
                while value.starts_with(['"', '\'']) && end > start + 1 {
                    start += 1;
                    value = &text[start..end];
                }
                matches.push(Match {
                    rule_id: rule.id.clone(),
                    category: rule.category,
                    severity: rule.severity,
                    start,
                    end,
                    value: value.to_string(),
                });
            }
        }
        for known in self.known.find_all(text) {
            matches.push(Match {
                rule_id: KNOWN_SECRET_RULE.to_string(),
                category: FindingCategory::KnownSecret,
                severity: Severity::Critical,
                start: known.start,
                end: known.end,
                value: text[known.start..known.end].to_string(),
            });
        }
        dedupe_overlaps(matches)
    }

    /// A request body: JSON bodies are walked leaf by leaf (findings carry the JSON path and the stored
    /// copy is re-serialised with the leaves redacted); anything else is scanned as text.
    pub fn scan_request(&self, input: &ScanInput, body: &[u8]) -> Outcome {
        if !self.enabled || body.is_empty() {
            return Outcome::default();
        }
        let (slice, truncated) = if body.len() > self.max_scan_bytes {
            (
                &body[..floor_char_boundary(body, self.max_scan_bytes)],
                true,
            )
        } else {
            (body, false)
        };
        let text = String::from_utf8_lossy(slice);
        let mut outcome = Outcome {
            scan_truncated: truncated,
            ..Default::default()
        };
        if !truncated
            && let Ok(mut json) = serde_json::from_str::<Value>(&text)
            && (json.is_object() || json.is_array())
        {
            let mut leaves = Vec::new();
            collect_leaves(&json, "$".to_string(), &mut leaves);
            let mut replacements: Vec<(String, Vec<(String, String)>)> = Vec::new();
            for (path, leaf) in leaves {
                let mut per_leaf = Vec::new();
                for m in self.scan_text(&leaf) {
                    let Some((finding, policy)) = self.finding_for(input, &m, &path, None) else {
                        outcome.allowlisted += 1;
                        continue;
                    };
                    if policy.redact {
                        per_leaf.push((
                            m.value.clone(),
                            placeholder(&m.rule_id, &finding.fingerprint),
                        ));
                    }
                    push_finding(&mut outcome, finding, policy, input.direction);
                }
                if !per_leaf.is_empty() {
                    replacements.push((path, per_leaf));
                }
            }
            if !replacements.is_empty() {
                for (path, values) in &replacements {
                    replace_leaf(&mut json, path, values);
                }
                outcome.redacted = Some(json.to_string().into_bytes());
            }
            return outcome;
        }
        let mut spans = Vec::new();
        for m in self.scan_text(&text) {
            let Some((finding, policy)) = self.finding_for(input, &m, "raw", Some(m.start as u64))
            else {
                outcome.allowlisted += 1;
                continue;
            };
            if policy.redact {
                spans.push((
                    m.start,
                    m.end,
                    placeholder(&m.rule_id, &finding.fingerprint),
                ));
            }
            push_finding(&mut outcome, finding, policy, input.direction);
        }
        if !spans.is_empty() {
            outcome.redacted = Some(redact_spans(&text, &spans).into_bytes());
        }
        outcome
    }

    /// A response: the reassembled reply text is scanned; the raw stream copy has each value replaced
    /// where it occurs whole, and falls back to the redacted reply text when a value was split across
    /// chunks, so no secret is ever stored.
    pub fn scan_response(&self, input: &ScanInput, text: &str, raw: &[u8]) -> Outcome {
        if !self.enabled || text.is_empty() {
            return Outcome::default();
        }
        let (slice, truncated) = if text.len() > self.max_scan_bytes {
            (&text[..floor_str_boundary(text, self.max_scan_bytes)], true)
        } else {
            (text, false)
        };
        let mut outcome = Outcome {
            scan_truncated: truncated,
            ..Default::default()
        };
        let mut values = Vec::new();
        for m in self.scan_text(slice) {
            let Some((finding, policy)) =
                self.finding_for(input, &m, "reply", Some(m.start as u64))
            else {
                outcome.allowlisted += 1;
                continue;
            };
            if policy.redact {
                values.push((
                    m.value.clone(),
                    placeholder(&m.rule_id, &finding.fingerprint),
                ));
            }
            push_finding(&mut outcome, finding, policy, input.direction);
        }
        if !values.is_empty() {
            let raw_text = String::from_utf8_lossy(raw);
            let (redacted_raw, all_found) = redact_values(&raw_text, &values);
            if all_found {
                outcome.redacted = Some(redacted_raw.into_bytes());
            } else {
                let (redacted_reply, _) = redact_values(text, &values);
                outcome.redacted = Some(redacted_reply.into_bytes());
            }
        }
        outcome
    }

    /// Resolves policy for a match and builds the finding, or `None` when the allowlist removes it.
    fn finding_for(
        &self,
        input: &ScanInput,
        m: &Match,
        location: &str,
        offset: Option<u64>,
    ) -> Option<(Finding, EffectivePolicy)> {
        let fingerprint = fingerprint(&m.value);
        if self.allowlisted(m, &fingerprint, input) {
            return None;
        }
        let policy = match self.rules.get(&m.rule_id) {
            Some(compiled) => self
                .policy
                .resolve(&compiled.rule, input.backend, input.is_cloud),
            None => self
                .policy
                .resolve(&known_secret_rule(), input.backend, input.is_cloud),
        };
        if !policy.enabled {
            return None;
        }
        let action = match input.direction {
            Direction::Request => policy.request_action,
            Direction::Response => match policy.response_action {
                ResponseAction::Record => DlpAction::Record,
                ResponseAction::Alert => DlpAction::Alert,
            },
        };
        Some((
            Finding {
                request_id: input.request_id,
                direction: input.direction,
                rule_id: m.rule_id.clone(),
                category: m.category,
                severity: policy.severity,
                location: location.to_string(),
                offset,
                length: u32::try_from(m.end - m.start).unwrap_or(u32::MAX),
                fingerprint,
                preview: preview(&m.value, &m.rule_id),
                action,
                created_at: Utc::now(),
            },
            policy,
        ))
    }

    fn allowlisted(&self, m: &Match, fingerprint: &str, input: &ScanInput) -> bool {
        self.allowlist.iter().any(|entry| {
            let scope_ok = match entry.scope {
                AllowlistScope::Both => true,
                AllowlistScope::Request => input.direction == Direction::Request,
                AllowlistScope::Response => input.direction == Direction::Response,
            };
            scope_ok
                && entry.backend.is_none_or(|b| b == input.backend)
                && entry.rule.as_deref().is_none_or(|r| r == m.rule_id)
                && entry
                    .fingerprint
                    .as_deref()
                    .is_none_or(|f| fingerprint.starts_with(f))
                && entry.regex.as_ref().is_none_or(|r| r.is_match(&m.value))
                && (entry.rule.is_some() || entry.fingerprint.is_some() || entry.regex.is_some())
        })
    }
}

fn push_finding(
    outcome: &mut Outcome,
    finding: Finding,
    policy: EffectivePolicy,
    direction: Direction,
) {
    if outcome.findings.iter().any(|f| {
        f.rule_id == finding.rule_id
            && f.fingerprint == finding.fingerprint
            && f.location == finding.location
    }) {
        return;
    }
    if direction == Direction::Request
        && policy.request_action == DlpAction::Block
        && outcome.blocked_by.is_none()
    {
        outcome.blocked_by = Some(finding.rule_id.clone());
    }
    outcome.findings.push(finding);
}

fn known_secret_rule() -> Rule {
    Rule {
        id: KNOWN_SECRET_RULE.into(),
        description: "a value from the known-secrets file".into(),
        category: FindingCategory::KnownSecret,
        severity: Severity::Critical,
        pattern: String::new(),
        capture_group: 0,
        keywords: Vec::new(),
        min_entropy: None,
        validator: None,
        default_action: None,
        redact_by_default: true,
        builtin: true,
        extend_to_end_marker: false,
    }
}

fn compile_allow(entry: &AllowlistEntry) -> anyhow::Result<CompiledAllow> {
    Ok(CompiledAllow {
        rule: entry.rule.clone(),
        regex: match &entry.regex {
            Some(r) => {
                Some(Regex::new(r).map_err(|e| anyhow::anyhow!("dlp.allowlist regex {r:?}: {e}"))?)
            }
            None => None,
        },
        fingerprint: entry.fingerprint.clone().map(|f| f.to_ascii_lowercase()),
        scope: entry.scope,
        backend: match &entry.backend {
            Some(b) => Some(
                b.parse::<Backend>()
                    .map_err(|e| anyhow::anyhow!("dlp.allowlist backend: {e}"))?,
            ),
            None => None,
        },
    })
}

/// The first 32 hex characters of the SHA-256 of the value.
pub fn fingerprint(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    digest.iter().take(16).map(|b| format!("{b:02x}")).collect()
}

/// `AKIA…MPLE` for values of twelve characters or more, `****` below; PEM blocks show their markers only.
pub fn preview(value: &str, _rule_id: &str) -> String {
    if value.starts_with("-----BEGIN") {
        let first = value.lines().next().unwrap_or("-----BEGIN");
        let last = value
            .lines()
            .last()
            .filter(|l| l.starts_with("-----END"))
            .unwrap_or("…");
        return format!("{first}…{last}");
    }
    let chars: Vec<char> = value.chars().collect();
    if chars.len() >= 12 {
        let head: String = chars[..4].iter().collect();
        let tail: String = chars[chars.len() - 4..].iter().collect();
        format!("{head}…{tail}")
    } else {
        "****".to_string()
    }
}

/// Where a PEM block ends: the end of its `-----END ... -----` line, else at most `PEM_MAX` bytes on.
fn pem_end(text: &str, from: usize) -> usize {
    let rest = &text[from..];
    if let Some(pos) = rest.find("-----END") {
        let line_end = rest[pos..]
            .find('\n')
            .map(|n| pos + n)
            .unwrap_or(rest.len());
        return from + line_end;
    }
    from + floor_str_boundary(rest, PEM_MAX.min(rest.len()))
}

/// Overlapping matches collapse to the most severe, then the longest.
fn dedupe_overlaps(mut matches: Vec<Match>) -> Vec<Match> {
    matches.sort_by(|a, b| {
        a.start
            .cmp(&b.start)
            .then_with(|| b.severity.cmp(&a.severity))
            .then_with(|| (b.end - b.start).cmp(&(a.end - a.start)))
    });
    let mut kept: Vec<Match> = Vec::new();
    for m in matches {
        if let Some(last) = kept.last_mut()
            && m.start < last.end
        {
            let better = m.severity > last.severity
                || (m.severity == last.severity && (m.end - m.start) > (last.end - last.start));
            if better {
                *last = m;
            }
            continue;
        }
        kept.push(m);
    }
    kept
}

fn collect_leaves(value: &Value, path: String, out: &mut Vec<(String, String)>) {
    match value {
        Value::String(s) => out.push((path, s.clone())),
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                collect_leaves(item, format!("{path}[{i}]"), out);
            }
        }
        Value::Object(map) => {
            for (k, v) in map {
                let key = if k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                    format!("{path}.{k}")
                } else {
                    format!("{path}[\"{}\"]", k.replace('"', "\\\""))
                };
                collect_leaves(v, key, out);
            }
        }
        _ => {}
    }
}

/// Walks to the leaf at `path` (the shape `collect_leaves` produces) and replaces values in it.
fn replace_leaf(value: &mut Value, path: &str, replacements: &[(String, String)]) {
    let mut current = value;
    let mut rest = path.strip_prefix('$').unwrap_or(path);
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix('.') {
            let end = after.find(['.', '[']).unwrap_or(after.len());
            let key = &after[..end];
            rest = &after[end..];
            match current.get_mut(key) {
                Some(next) => current = next,
                None => return,
            }
        } else if let Some(after) = rest.strip_prefix("[\"") {
            let Some(close) = after.find("\"]") else {
                return;
            };
            let key = after[..close].replace("\\\"", "\"");
            rest = &after[close + 2..];
            match current.get_mut(&key) {
                Some(next) => current = next,
                None => return,
            }
        } else if let Some(after) = rest.strip_prefix('[') {
            let Some(close) = after.find(']') else { return };
            let Ok(index) = after[..close].parse::<usize>() else {
                return;
            };
            rest = &after[close + 1..];
            match current.get_mut(index) {
                Some(next) => current = next,
                None => return,
            }
        } else {
            return;
        }
    }
    if let Value::String(s) = current {
        let (redacted, _) = redact_values(s, replacements);
        *s = redacted;
    }
}

fn floor_char_boundary(bytes: &[u8], at: usize) -> usize {
    let mut i = at.min(bytes.len());
    while i > 0 && (bytes[i] & 0xC0) == 0x80 {
        i -= 1;
    }
    i
}

fn floor_str_boundary(s: &str, at: usize) -> usize {
    let mut i = at.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use gateway_core::config::Config;

    use super::*;

    fn engine() -> Engine {
        Engine::from_config(&Config::default().dlp, KnownSecrets::default()).unwrap()
    }

    fn input(direction: Direction, is_cloud: bool) -> ScanInput {
        ScanInput {
            request_id: Uuid::nil(),
            backend: Backend::Ollama,
            is_cloud,
            direction,
        }
    }

    /// Every built-in rule with a vector that must match and one that must not.
    #[test]
    fn builtin_rules_match_their_vectors_and_ignore_their_negatives() {
        let cases: Vec<(&str, String, &str)> = vec![
            ("aws-access-key-id", "key AKIAIOSFODNN7EXAMPLE here".into(), "AKIA123"),
            ("aws-secret-access-key", "aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(), "aws secret = aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            ("github-token", format!("ghp_{}", "aB3dE6fG9hJ2kL5mN8pQ1rS4tU7vW0xYz1234567"), "ghp_short"),
            ("github-fine-grained-pat", format!("github_pat_{}_{}", "a".repeat(22), "b".repeat(59)), "github_pat_short"),
            ("gitlab-token", "glpat-abcdefghijklmnopqrstuvwx".into(), "glpat-short"),
            ("slack-token", "xoxb-123456789012-abcdefghijklmnop".into(), "xoxz-nope"),
            ("slack-webhook", "https://hooks.slack.com/services/T00000000/B00000000/XXXXXXXXXXXXXXXXXXXXXXXX".into(), "https://hooks.slack.com/x"),
            ("stripe-live-key", "sk_live_abcdefghijklmnopqrstuvwxyz".into(), "sk_live_short"),
            ("stripe-test-key", "sk_test_abcdefghijklmnopqrstuvwxyz".into(), "sk_test_short"),
            ("google-api-key", "AIzaSyA1234567890abcdefghijklmnopqrstuv".into(), "AIzaShort"),
            ("gcp-service-account", r#"{"type":"service_account","private_key_id":"0123456789abcdef0123456789abcdef01234567"}"#.into(), r#"{"private_key_id":"0123456789abcdef0123456789abcdef01234567"}"#),
            ("anthropic-api-key", "sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789".into(), "sk-ant-short"),
            ("openai-api-key", format!("sk-proj-{}T3BlbkFJ{}", "a1B2c3D4e5F6g7H8i9J0k1L2", "m3N4o5P6q7R8s9T0u1V2w3X4"), "sk-proj-nope"),
            ("openai-api-key-legacy", "sk-aB3dE6fG9hJ2kL5mN8pQ1rS4tU7vW0xYz12345678abcdefg".into(), "sk-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            ("huggingface-token", "hf_abcdefghijklmnopqrstuvwxyz1234567890".into(), "hf_short"),
            ("npm-token", format!("npm_{}", "aB3dE6fG9hJ2kL5mN8pQ1rS4tU7vW0xYz123"), "npm_short"),
            ("npm-auth-token", "//registry.npmjs.org/:_authToken=npm_aB3dE6fG9hJ2kL5mN8pQ1rS4tU7v".into(), "_authToken=short"),
            ("pypi-token", format!("pypi-AgEIcHlwaS5vcmc{}", "aB3dE6fG9hJ2kL5mN8pQ1rS4tU7vW0xYz1234567890abcdefghijklmnop"), "pypi-short"),
            ("twilio-credentials", "twilio sid AC0123456789abcdef0123456789abcdef".into(), "AC0123456789abcdef0123456789abcdef"),
            ("sendgrid-api-key", format!("SG.{}.{}", "aB3dE6fG9hJ2kL5mN8pQ1r", "aB3dE6fG9hJ2kL5mN8pQ1rS4tU7vW0xYz1234567890"), "SG.short.short"),
            ("mailgun-api-key", "mailgun key-0123456789abcdef0123456789abcdef".into(), "key-0123456789abcdef0123456789abcdef"),
            ("azure-storage-key", format!("AccountKey={}==", "a".repeat(86)), "AccountKey=short"),
            ("azure-connection-string", "DefaultEndpointsProtocol=https;AccountName=acct;AccountKey=x".into(), "AccountName=acct"),
            ("private-key-pem", "-----BEGIN RSA PRIVATE KEY-----\nMIIE\n-----END RSA PRIVATE KEY-----".into(), "-----BEGIN CERTIFICATE-----"),
            ("ssh-private-key", "-----BEGIN OPENSSH PRIVATE KEY-----\nb3Blbn\n-----END OPENSSH PRIVATE KEY-----".into(), "-----BEGIN PUBLIC KEY-----"),
            ("age-secret-key", format!("AGE-SECRET-KEY-1{}", "QZ8X".repeat(15).chars().take(58).collect::<String>()), "AGE-SECRET-KEY-1short"),
            ("jwt", "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U".into(), "eyJub3QiOiJqd3QifQ.eyJub3QiOiJqd3QifQ.c2lnbmF0dXJlc2lnbmF0dXJl"),
            ("generic-secret-assignment", "password = \"c0rr3ct-h0rse-b4ttery\"".into(), "password = \"aaaaaaaa\""),
            ("database-url-credentials", "postgres://app:s3cretPassw0rd@db.internal:5432/app".into(), "postgres://db.internal/app"),
            ("basic-auth-url", "https://user:p4ssw0rd@example.com/path".into(), "https://example.com/path"),
            ("bearer-token", "Authorization: Bearer aB3dE6fG9hJ2kL5mN8pQ1rS4tU7v".into(), "Bearer aaaaaaaaaaaaaaaaaaaaaaaa"),
            ("telegram-bot-token", "123456789:AAHfiqksKZ8WmR2zSjiQ7_v4TMAKdiHm9T0".into(), "123:short"),
            ("digitalocean-token", format!("dop_v1_{}", "0123456789abcdef".repeat(4)), "dop_v1_short"),
            ("email", "mail me at someone@example.org please".into(), "not an email @ all"),
            ("phone", "call +14155552671 now".into(), "call 12 now"),
            ("us-ssn", "ssn 123-45-6789".into(), "000-00-0000"),
            ("credit-card", "card 4111 1111 1111 1111".into(), "card 4111 1111 1111 1112"),
            ("iban", "GB82WEST12345698765432".into(), "GB82WEST12345698765433"),
        ];
        let e = engine();
        for (rule, positive, negative) in cases {
            let found = e.scan_text(&positive);
            assert!(
                found.iter().any(|m| m.rule_id == rule),
                "{rule} should match {positive:?}; got {found:?}"
            );
            let found = e.scan_text(negative);
            assert!(
                !found.iter().any(|m| m.rule_id == rule),
                "{rule} must not match {negative:?}; got {found:?}"
            );
        }
    }

    #[test]
    fn overlapping_matches_keep_the_most_severe() {
        let found = engine().scan_text("api_key = AKIAIOSFODNN7EXAMPLE");
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].rule_id, "aws-access-key-id");
        assert_eq!(found[0].value, "AKIAIOSFODNN7EXAMPLE");
    }

    #[test]
    fn json_requests_carry_paths_block_on_cloud_and_redact_the_stored_copy() {
        let e = engine();
        let body = br#"{"model":"deepseek-v4-pro:cloud","messages":[{"role":"system","content":"be nice"},{"role":"user","content":"my key is AKIAIOSFODNN7EXAMPLE ok"}]}"#;
        let local = e.scan_request(&input(Direction::Request, false), body);
        assert_eq!(local.findings.len(), 1);
        let f = &local.findings[0];
        assert_eq!(f.rule_id, "aws-access-key-id");
        assert_eq!(f.location, "$.messages[1].content");
        assert_eq!(f.preview, "AKIA…MPLE");
        assert_eq!(f.fingerprint.len(), 32);
        assert_eq!(f.action, DlpAction::Alert);
        assert_eq!(f.severity, Severity::Critical);
        assert!(local.blocked_by.is_none());
        let stored = String::from_utf8(local.redacted.clone().unwrap()).unwrap();
        assert!(!stored.contains("AKIAIOSFODNN7EXAMPLE"));
        assert!(stored.contains("[REDACTED:aws-access-key-id:"));
        let json: Value = serde_json::from_str(&stored).expect("still JSON");
        assert_eq!(json["messages"][0]["content"], "be nice");
        let cloud = e.scan_request(&input(Direction::Request, true), body);
        assert_eq!(cloud.blocked_by.as_deref(), Some("aws-access-key-id"));
        assert_eq!(cloud.findings[0].action, DlpAction::Block);
    }

    #[test]
    fn raw_requests_get_offsets_and_pii_is_recorded_without_redaction() {
        let e = engine();
        let out = e.scan_request(
            &input(Direction::Request, false),
            b"contact someone@example.org about AKIAIOSFODNN7EXAMPLE",
        );
        let ids: Vec<&str> = out.findings.iter().map(|f| f.rule_id.as_str()).collect();
        assert!(
            ids.contains(&"email") && ids.contains(&"aws-access-key-id"),
            "{ids:?}"
        );
        let email = out.findings.iter().find(|f| f.rule_id == "email").unwrap();
        assert_eq!(email.action, DlpAction::Record);
        assert_eq!(email.location, "raw");
        assert_eq!(email.offset, Some(8));
        let stored = String::from_utf8(out.redacted.unwrap()).unwrap();
        assert!(
            stored.contains("someone@example.org"),
            "PII is not redacted by default"
        );
        assert!(!stored.contains("AKIAIOSFODNN7EXAMPLE"));
    }

    #[test]
    fn responses_are_scanned_and_split_secrets_fall_back_to_the_reply_text() {
        let e = engine();
        let raw = b"data: {\"choices\":[{\"delta\":{\"content\":\"key AKIAIOSF\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"ODNN7EXAMPLE\"}}]}\n\n";
        let out = e.scan_response(
            &input(Direction::Response, false),
            "key AKIAIOSFODNN7EXAMPLE",
            raw,
        );
        assert_eq!(out.findings.len(), 1);
        assert_eq!(out.findings[0].direction, Direction::Response);
        assert_eq!(out.findings[0].location, "reply");
        assert_eq!(out.findings[0].action, DlpAction::Alert);
        let stored = String::from_utf8(out.redacted.unwrap()).unwrap();
        assert!(!stored.contains("ODNN7EXAMPLE"), "{stored}");
        assert!(
            stored.starts_with("key [REDACTED:"),
            "falls back to the reply text: {stored}"
        );
        let whole =
            b"data: {\"choices\":[{\"delta\":{\"content\":\"key AKIAIOSFODNN7EXAMPLE\"}}]}\n\n";
        let out = e.scan_response(
            &input(Direction::Response, false),
            "key AKIAIOSFODNN7EXAMPLE",
            whole,
        );
        let stored = String::from_utf8(out.redacted.unwrap()).unwrap();
        assert!(
            stored.starts_with("data: "),
            "the raw stream is kept when the value sits whole: {stored}"
        );
        assert!(!stored.contains("AKIAIOSFODNN7EXAMPLE"));
    }

    #[test]
    fn allowlists_and_known_secrets_apply() {
        let cfg = Config::parse(
            "[[dlp.allowlist]]\nrule = \"aws-access-key-id\"\nregex = \"EXAMPLE$\"\n",
        )
        .unwrap();
        let known = KnownSecrets::from_entries([(
            "vault-root".to_string(),
            "hunter2-hunter2-hunter2".to_string(),
        )]);
        let e = Engine::from_config(&cfg.dlp, known).unwrap();
        let out = e.scan_request(
            &input(Direction::Request, false),
            b"AKIAIOSFODNN7EXAMPLE and hunter2-hunter2-hunter2",
        );
        assert_eq!(out.allowlisted, 1);
        assert_eq!(out.findings.len(), 1);
        assert_eq!(out.findings[0].rule_id, "known-secret");
        assert_eq!(out.findings[0].category, FindingCategory::KnownSecret);
        assert_eq!(out.findings[0].severity, Severity::Critical);
        let dummy = e.scan_request(
            &input(Direction::Request, false),
            br#"{"api_key":"sk-lm-studio","token":"not-needed"}"#,
        );
        assert!(
            dummy.findings.is_empty(),
            "local dummy keys are allowlisted: {:?}",
            dummy.findings
        );
        let disabled = Engine::disabled();
        assert!(
            disabled
                .scan_request(&input(Direction::Request, true), b"AKIAIOSFODNN7EXAMPLE")
                .findings
                .is_empty()
        );
    }

    #[test]
    fn pem_blocks_extend_to_their_end_line_and_preview_shows_the_markers() {
        let text = "before\n-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n-----END OPENSSH PRIVATE KEY-----\nafter";
        let found = engine().scan_text(text);
        assert_eq!(found.len(), 1);
        assert!(
            found[0]
                .value
                .ends_with("-----END OPENSSH PRIVATE KEY-----")
        );
        assert_eq!(
            preview(&found[0].value, "ssh-private-key"),
            "-----BEGIN OPENSSH PRIVATE KEY-----…-----END OPENSSH PRIVATE KEY-----"
        );
        assert_eq!(preview("short", "x"), "****");
        assert_eq!(fingerprint("a").len(), 32);
    }

    #[test]
    fn oversized_bodies_are_scanned_up_to_the_cap() {
        let cfg = Config::parse("[dlp]\nmax_scan_bytes = 64\n").unwrap();
        let e = Engine::from_config(&cfg.dlp, KnownSecrets::default()).unwrap();
        let body = format!("{} AKIAIOSFODNN7EXAMPLE", "x".repeat(100));
        let out = e.scan_request(&input(Direction::Request, false), body.as_bytes());
        assert!(out.scan_truncated);
        assert!(out.findings.is_empty(), "the key sits past the cap");
    }

    #[test]
    fn custom_rules_join_the_set_and_cannot_shadow_builtins() {
        let cfg = Config::parse("[[dlp.custom_rules]]\nid = \"acme-ticket\"\nregex = \"\\\\bACME-[0-9]{6}\\\\b\"\nseverity = \"high\"\naction = \"block\"\n").unwrap();
        let e = Engine::from_config(&cfg.dlp, KnownSecrets::default()).unwrap();
        let out = e.scan_request(&input(Direction::Request, false), b"see ACME-123456");
        assert_eq!(out.blocked_by.as_deref(), Some("acme-ticket"));
        assert_eq!(out.findings[0].category, FindingCategory::Custom);
        let shadow =
            Config::parse("[[dlp.custom_rules]]\nid = \"email\"\nregex = \"x\"\n").unwrap();
        assert!(Engine::from_config(&shadow.dlp, KnownSecrets::default()).is_err());
    }
}
