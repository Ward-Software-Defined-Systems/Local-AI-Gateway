//! The rule model and the built-in ruleset. A rule is a regex (crate syntax, no look-around), an optional
//! capture group holding the secret, optional keyword and entropy gates, and an optional validator. The
//! set is compiled into one `RegexSet` for the first pass; only rules that hit run their own regex.

use gateway_core::config::CustomRule;
use gateway_core::types::{DlpAction, FindingCategory, Severity};
use regex::{Regex, RegexSet, RegexSetBuilder};

use crate::validators::Validator;

#[derive(Debug, Clone)]
pub struct Rule {
    pub id: String,
    pub description: String,
    pub category: FindingCategory,
    pub severity: Severity,
    pub pattern: String,
    /// Which capture group holds the secret; 0 is the whole match.
    pub capture_group: usize,
    /// Lowercase literals, any of which must appear in the text for the regex to run.
    pub keywords: Vec<String>,
    pub min_entropy: Option<f32>,
    pub validator: Option<Validator>,
    /// `None` follows `dlp.default_action`; PII rules carry their own defaults.
    pub default_action: Option<DlpAction>,
    pub redact_by_default: bool,
    pub builtin: bool,
    /// PEM blocks: the match extends to the matching `END` line.
    pub extend_to_end_marker: bool,
}

impl Rule {
    fn secret(id: &str, description: &str, severity: Severity, pattern: &str) -> Rule {
        Rule {
            id: id.into(),
            description: description.into(),
            category: FindingCategory::Secret,
            severity,
            pattern: pattern.into(),
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

    fn pii(
        id: &str,
        description: &str,
        severity: Severity,
        pattern: &str,
        action: DlpAction,
        redact: bool,
    ) -> Rule {
        Rule {
            category: FindingCategory::Pii,
            default_action: Some(action),
            redact_by_default: redact,
            ..Rule::secret(id, description, severity, pattern)
        }
    }

    fn group(mut self, n: usize) -> Rule {
        self.capture_group = n;
        self
    }

    fn keywords(mut self, words: &[&str]) -> Rule {
        self.keywords = words.iter().map(|w| w.to_ascii_lowercase()).collect();
        self
    }

    fn entropy(mut self, bits: f32) -> Rule {
        self.min_entropy = Some(bits);
        self
    }

    fn validator(mut self, v: Validator) -> Rule {
        self.validator = Some(v);
        self
    }

    fn pem(mut self) -> Rule {
        self.extend_to_end_marker = true;
        self
    }

    pub fn from_custom(custom: &CustomRule) -> Rule {
        Rule {
            id: custom.id.clone(),
            description: custom.description.clone(),
            category: FindingCategory::Custom,
            severity: custom.severity,
            pattern: custom.regex.clone(),
            capture_group: custom.capture_group,
            keywords: custom
                .keywords
                .iter()
                .map(|w| w.to_ascii_lowercase())
                .collect(),
            min_entropy: custom.min_entropy,
            validator: None,
            default_action: custom.action,
            redact_by_default: custom.redact,
            builtin: false,
            extend_to_end_marker: false,
        }
    }
}

/// The id the engine reports for known-secret matches; it is not a regex rule.
pub const KNOWN_SECRET_RULE: &str = "known-secret";

pub fn builtin_rules() -> Vec<Rule> {
    use Severity::{Critical, High, Low, Medium};
    vec![
        Rule::secret("aws-access-key-id", "AWS access key id", Critical, r"\b(?:AKIA|ASIA|AGPA|AIDA|AROA|AIPA|ANPA|ANVA)[A-Z0-9]{16}\b"),
        Rule::secret("aws-secret-access-key", "AWS secret access key beside an AWS keyword", Critical, r#"(?i)aws.{0,20}(?:secret|private).{0,20}['"=:\s]([A-Za-z0-9/+=]{40})\b"#).group(1).entropy(4.0),
        Rule::secret("github-token", "GitHub personal, OAuth, user-to-server, server-to-server or refresh token", Critical, r"\b(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{36,255}\b"),
        Rule::secret("github-fine-grained-pat", "GitHub fine-grained personal access token", Critical, r"\bgithub_pat_[A-Za-z0-9]{22}_[A-Za-z0-9]{59}\b"),
        Rule::secret("gitlab-token", "GitLab personal, runner, pipeline-trigger or deploy token", Critical, r"\bgl(?:pat|rt|ptt|dt)-[A-Za-z0-9_\-]{20,}\b"),
        Rule::secret("slack-token", "Slack bot, user, app or refresh token", High, r"\bxox[baprs]-[A-Za-z0-9\-]{10,}\b"),
        Rule::secret("slack-webhook", "Slack incoming webhook URL", High, r"https://hooks\.slack\.com/services/T[A-Z0-9]+/B[A-Z0-9]+/[A-Za-z0-9]+"),
        Rule::secret("stripe-live-key", "Stripe live secret or restricted key", Critical, r"\b(?:sk|rk)_live_[A-Za-z0-9]{24,}\b"),
        Rule::secret("stripe-test-key", "Stripe test key", Low, r"\b(?:sk|rk)_test_[A-Za-z0-9]{24,}\b"),
        Rule::secret("google-api-key", "Google API key", High, r"\bAIza[0-9A-Za-z_\-]{35}\b"),
        Rule::secret("gcp-service-account", "Google Cloud service account JSON", Critical, r#""private_key_id"\s*:\s*"[a-f0-9]{40}""#).keywords(&["service_account", "\"private_key\""]),
        Rule::secret("anthropic-api-key", "Anthropic API key", Critical, r"\bsk-ant-[A-Za-z0-9_\-]{20,}\b"),
        Rule::secret("openai-api-key", "OpenAI project, service-account or admin key", High, r"\bsk-(?:proj-|svcacct-|admin-)?[A-Za-z0-9_\-]{20,}T3BlbkFJ[A-Za-z0-9_\-]{20,}\b"),
        Rule::secret("openai-api-key-legacy", "OpenAI legacy 48-character key", High, r"\bsk-[A-Za-z0-9]{48}\b").entropy(3.5),
        Rule::secret("huggingface-token", "Hugging Face user access token", High, r"\bhf_[A-Za-z0-9]{30,}\b"),
        Rule::secret("npm-token", "npm granular access token", High, r"\bnpm_[A-Za-z0-9]{36}\b"),
        Rule::secret("npm-auth-token", ".npmrc _authToken", High, r"(?i)_authToken\s*=\s*([A-Za-z0-9_\-]{20,})").group(1),
        Rule::secret("pypi-token", "PyPI API token", High, r"\bpypi-AgEIcHlwaS5vcmc[A-Za-z0-9_\-]{50,}\b"),
        Rule::secret("twilio-credentials", "Twilio account SID or API key SID", Medium, r"\b(?:AC|SK)[a-f0-9]{32}\b").keywords(&["twilio"]),
        Rule::secret("sendgrid-api-key", "SendGrid API key", High, r"\bSG\.[A-Za-z0-9_\-]{22}\.[A-Za-z0-9_\-]{43}\b"),
        Rule::secret("mailgun-api-key", "Mailgun API key beside a Mailgun keyword", Medium, r"\bkey-[a-f0-9]{32}\b").keywords(&["mailgun"]),
        Rule::secret("azure-storage-key", "Azure storage account key", Critical, r"(?i)AccountKey=([A-Za-z0-9+/=]{86,88})").group(1),
        Rule::secret("azure-connection-string", "Azure storage connection string", Critical, r"(?i)DefaultEndpointsProtocol=https?;AccountName=[^;]+;AccountKey="),
        Rule::secret("private-key-pem", "PEM private key block", Critical, r"-----BEGIN (?:RSA |EC |DSA |PGP |ENCRYPTED )?PRIVATE KEY(?: BLOCK)?-----").pem(),
        Rule::secret("ssh-private-key", "OpenSSH private key block", Critical, r"-----BEGIN OPENSSH PRIVATE KEY-----").pem(),
        Rule::secret("age-secret-key", "age identity (secret key)", High, r"\bAGE-SECRET-KEY-1[A-Z0-9]{58}\b"),
        Rule::secret("jwt", "JSON Web Token", Medium, r"\beyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}\b").validator(Validator::JwtHeader),
        Rule::secret("generic-secret-assignment", "password / secret / token / api key assignment with a high-entropy value", Medium, r#"(?i)\b(?:password|passwd|pwd|secret|token|api[_-]?key|access[_-]?key|auth[_-]?token|client[_-]?secret)\b\s*[:=]\s*['"]?([A-Za-z0-9_\-\.\+/=]{8,})"#).group(1).entropy(3.0),
        Rule::secret("database-url-credentials", "database URL with an embedded password", High, r#"\b(?:postgres(?:ql)?|mysql|mongodb(?:\+srv)?|redis|amqp|mssql)://[^\s:/@'"]+:([^\s@'"]+)@[^\s'"/]+"#).group(1),
        Rule::secret("basic-auth-url", "URL with basic-auth credentials", Medium, r#"https?://[^\s:/@'"]+:([^\s@'"]+)@"#).group(1),
        Rule::secret("bearer-token", "Bearer token with a high-entropy value", Medium, r"(?i)\bbearer\s+([A-Za-z0-9_\-\.=+/]{20,})").group(1).entropy(3.0),
        Rule::secret("telegram-bot-token", "Telegram bot token", Medium, r"\b\d{8,10}:[A-Za-z0-9_\-]{35}\b"),
        Rule::secret("digitalocean-token", "DigitalOcean personal access token", High, r"\bdop_v1_[a-f0-9]{64}\b"),
        Rule::pii("email", "email address", Low, r"\b[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}\b", DlpAction::Record, false),
        Rule::pii("phone", "phone number (E.164 or North American)", Low, r"\+[1-9]\d{7,14}\b|\(?\b[2-9]\d{2}\)?[\s.\-]?\d{3}[\s.\-]?\d{4}\b", DlpAction::Record, false),
        Rule::pii("us-ssn", "US social security number", Medium, r"\b(?:0[1-9]\d|[1-5]\d\d|6[0-57-9]\d|66[0-57-9]|7[0-6]\d|77[0-2])-(?:0[1-9]|[1-9]\d)-(?:0[1-9]\d\d|[1-9]\d{3})\b", DlpAction::Alert, true),
        Rule::pii("credit-card", "payment card number (Luhn-valid)", High, r"\b(?:\d[ \-]?){13,19}\b", DlpAction::Alert, true).validator(Validator::Luhn),
        Rule::pii("iban", "IBAN (mod-97 valid)", Medium, r"\b[A-Z]{2}\d{2}[A-Z0-9]{11,30}\b", DlpAction::Alert, true).validator(Validator::IbanMod97),
    ]
}

#[derive(Debug)]
pub struct CompiledRule {
    pub rule: Rule,
    pub regex: Regex,
    pub enabled: bool,
}

/// All rules behind one `RegexSet`.
#[derive(Debug)]
pub struct RuleSet {
    rules: Vec<CompiledRule>,
    set: RegexSet,
}

impl RuleSet {
    pub fn compile(rules: Vec<Rule>, enabled: impl Fn(&Rule) -> bool) -> anyhow::Result<RuleSet> {
        let mut compiled = Vec::with_capacity(rules.len());
        for rule in rules {
            let regex = Regex::new(&rule.pattern)
                .map_err(|e| anyhow::anyhow!("dlp rule {}: {e}", rule.id))?;
            if rule.capture_group > regex.captures_len().saturating_sub(1) {
                anyhow::bail!(
                    "dlp rule {}: capture_group {} does not exist in its regex",
                    rule.id,
                    rule.capture_group
                );
            }
            let enabled = enabled(&rule);
            compiled.push(CompiledRule {
                rule,
                regex,
                enabled,
            });
        }
        let set = RegexSetBuilder::new(compiled.iter().map(|c| {
            if c.enabled {
                c.rule.pattern.clone()
            } else {
                "a^".to_string()
            }
        }))
        .size_limit(10 << 20)
        .dfa_size_limit(2 << 20)
        .build()
        .map_err(|e| anyhow::anyhow!("dlp rule set: {e}"))?;
        Ok(RuleSet {
            rules: compiled,
            set,
        })
    }

    pub fn rules(&self) -> &[CompiledRule] {
        &self.rules
    }

    pub fn get(&self, id: &str) -> Option<&CompiledRule> {
        self.rules.iter().find(|c| c.rule.id == id)
    }

    /// The enabled rules whose pattern occurs somewhere in `text`.
    pub fn candidates(&self, text: &str) -> impl Iterator<Item = &CompiledRule> {
        self.set
            .matches(text)
            .into_iter()
            .map(move |i| &self.rules[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_rule_compiles_and_has_a_unique_id() {
        let rules = builtin_rules();
        assert!(rules.len() >= 36, "{}", rules.len());
        let set = RuleSet::compile(rules.clone(), |_| true).unwrap();
        let mut ids: Vec<&str> = rules.iter().map(|r| r.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), rules.len(), "duplicate ids");
        assert!(set.get("aws-access-key-id").is_some());
        assert!(set.candidates("nothing secret here").next().is_none());
    }

    #[test]
    fn a_disabled_rule_never_becomes_a_candidate_and_bad_custom_rules_fail_loudly() {
        let set = RuleSet::compile(builtin_rules(), |r| r.id != "aws-access-key-id").unwrap();
        assert!(set.candidates("AKIAIOSFODNN7EXAMPLE").next().is_none());
        let bad = CustomRule {
            id: "bad".into(),
            description: String::new(),
            regex: "(unclosed".into(),
            capture_group: 0,
            keywords: vec![],
            min_entropy: None,
            severity: Severity::Low,
            action: None,
            redact: true,
        };
        assert!(RuleSet::compile(vec![Rule::from_custom(&bad)], |_| true).is_err());
        let wrong_group = CustomRule {
            regex: "abc".into(),
            capture_group: 2,
            ..bad
        };
        let err = RuleSet::compile(vec![Rule::from_custom(&wrong_group)], |_| true).unwrap_err();
        assert!(err.to_string().contains("capture_group"), "{err}");
    }
}
