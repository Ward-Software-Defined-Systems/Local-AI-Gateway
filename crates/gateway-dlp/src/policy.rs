//! Policy resolution: what to do with a finding, from the rule's own default through the configuration's
//! layers. Stricter always wins for cloud-routed requests, so the cloud setting can tighten a per-rule
//! override but never loosen it.

use gateway_core::config::DlpConfig;
use gateway_core::types::{Backend, DlpAction, FindingCategory, ResponseAction, Severity};

use crate::rules::Rule;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EffectivePolicy {
    pub enabled: bool,
    pub severity: Severity,
    pub request_action: DlpAction,
    pub response_action: ResponseAction,
    pub redact: bool,
}

#[derive(Debug, Clone)]
pub struct Policy {
    config: DlpConfig,
}

fn rank(action: DlpAction) -> u8 {
    match action {
        DlpAction::Record => 0,
        DlpAction::Alert => 1,
        DlpAction::Block => 2,
    }
}

fn stricter(a: DlpAction, b: DlpAction) -> DlpAction {
    if rank(b) > rank(a) { b } else { a }
}

impl Policy {
    pub fn new(config: &DlpConfig) -> Self {
        Self {
            config: config.clone(),
        }
    }

    pub fn config(&self) -> &DlpConfig {
        &self.config
    }

    pub fn resolve(&self, rule: &Rule, backend: Backend, is_cloud: bool) -> EffectivePolicy {
        let overrides = self.config.rules.get(&rule.id);
        let backend_overrides = self.config.backends.get(backend.as_str());
        let enabled = overrides.and_then(|o| o.enabled).unwrap_or(true);
        let severity = overrides.and_then(|o| o.severity).unwrap_or(rule.severity);

        let mut request_action = rule.default_action.unwrap_or(self.config.default_action);
        if let Some(action) = backend_overrides.and_then(|b| b.request_action) {
            request_action = action;
        }
        if let Some(action) = overrides.and_then(|o| o.action) {
            request_action = action;
        }
        if is_cloud {
            let cloud = if rule.category == FindingCategory::Pii {
                self.config.cloud.pii_action
            } else {
                self.config.cloud.request_action
            };
            request_action = stricter(request_action, cloud);
        }

        let mut response_action = backend_overrides
            .and_then(|b| b.response_action)
            .unwrap_or(self.config.response_action);
        if let Some(action) = overrides.and_then(|o| o.action) {
            response_action = match action {
                DlpAction::Record => ResponseAction::Record,
                DlpAction::Alert | DlpAction::Block => ResponseAction::Alert,
            };
        }

        let redact = self.config.redact_stored_copies
            && overrides
                .and_then(|o| o.redact)
                .unwrap_or(rule.redact_by_default);
        EffectivePolicy {
            enabled,
            severity,
            request_action,
            response_action,
            redact,
        }
    }
}

#[cfg(test)]
mod tests {
    use gateway_core::config::Config;

    use super::*;
    use crate::rules::builtin_rules;

    fn rule(id: &str) -> Rule {
        builtin_rules().into_iter().find(|r| r.id == id).unwrap()
    }

    #[test]
    fn defaults_alert_on_secrets_record_low_pii_and_block_secrets_bound_for_the_cloud() {
        let policy = Policy::new(&Config::default().dlp);
        let aws = policy.resolve(&rule("aws-access-key-id"), Backend::Ollama, false);
        assert_eq!(aws.request_action, DlpAction::Alert);
        assert_eq!(aws.response_action, ResponseAction::Alert);
        assert!(aws.redact);
        assert!(aws.enabled);
        let email = policy.resolve(&rule("email"), Backend::Ollama, false);
        assert_eq!(email.request_action, DlpAction::Record);
        assert!(!email.redact);
        let cloud_aws = policy.resolve(&rule("aws-access-key-id"), Backend::Ollama, true);
        assert_eq!(cloud_aws.request_action, DlpAction::Block);
        let cloud_email = policy.resolve(&rule("email"), Backend::Ollama, true);
        assert_eq!(
            cloud_email.request_action,
            DlpAction::Alert,
            "PII on the cloud path alerts"
        );
    }

    #[test]
    fn overrides_layer_and_cloud_only_tightens() {
        let cfg = Config::parse(
            "[dlp]\ndefault_action = \"record\"\n[dlp.backends.lmstudio]\nrequest_action = \"alert\"\n[dlp.rules.github-token]\naction = \"block\"\nseverity = \"low\"\nredact = false\n[dlp.rules.email]\nenabled = false\n[dlp.cloud]\nrequest_action = \"alert\"\n",
        )
        .unwrap();
        let policy = Policy::new(&cfg.dlp);
        let aws_ollama = policy.resolve(&rule("aws-access-key-id"), Backend::Ollama, false);
        assert_eq!(
            aws_ollama.request_action,
            DlpAction::Record,
            "the global default"
        );
        let aws_lms = policy.resolve(&rule("aws-access-key-id"), Backend::LmStudio, false);
        assert_eq!(
            aws_lms.request_action,
            DlpAction::Alert,
            "the backend override"
        );
        let gh = policy.resolve(&rule("github-token"), Backend::Ollama, true);
        assert_eq!(
            gh.request_action,
            DlpAction::Block,
            "the per-rule block beats a looser cloud setting"
        );
        assert_eq!(gh.severity, Severity::Low);
        assert!(!gh.redact);
        assert_eq!(gh.response_action, ResponseAction::Alert);
        assert!(
            !policy
                .resolve(&rule("email"), Backend::Ollama, false)
                .enabled
        );
        let cfg = Config::parse("[dlp]\nredact_stored_copies = false\n").unwrap();
        assert!(
            !Policy::new(&cfg.dlp)
                .resolve(&rule("aws-access-key-id"), Backend::Ollama, false)
                .redact
        );
    }
}
