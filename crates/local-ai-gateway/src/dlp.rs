//! The bridge between the DLP engine and the proxy, and the known-secrets file. The engine is pure; this
//! module gives it the file system (the known-secrets file, with its mode check) and adapts its outcome to
//! the proxy's `RequestScanner` trait.

use std::path::Path;
use std::sync::Arc;

use gateway_core::config::{DlpConfig, Issue, IssueLevel};
use gateway_core::paths::Paths;
use gateway_dlp::Engine;
use gateway_dlp::engine::ScanInput;
use gateway_dlp::known_secrets::KnownSecrets;
use gateway_proxy::scan::{Decision, RequestScanner, ScanContext, ScanOutcome};

/// The engine behind the proxy's scanner slot; swapped whole on reload.
pub struct DlpScanner {
    engine: Arc<Engine>,
}

impl DlpScanner {
    pub fn new(engine: Arc<Engine>) -> Self {
        Self { engine }
    }
}

fn input(ctx: &ScanContext) -> ScanInput {
    ScanInput {
        request_id: ctx.request_id,
        backend: ctx.backend,
        is_cloud: ctx.is_cloud,
        direction: ctx.direction,
    }
}

impl RequestScanner for DlpScanner {
    fn scan_request(&self, ctx: &ScanContext, body: &[u8]) -> ScanOutcome {
        let out = self.engine.scan_request(&input(ctx), body);
        ScanOutcome {
            findings: out.findings,
            decision: match out.blocked_by {
                Some(rule_id) => Decision::Block { rule_id },
                None => Decision::Allow,
            },
            redacted: out.redacted,
        }
    }

    fn scan_response(&self, ctx: &ScanContext, text: &str, raw: &[u8]) -> ScanOutcome {
        let out = self.engine.scan_response(&input(ctx), text, raw);
        ScanOutcome {
            findings: out.findings,
            decision: Decision::Allow,
            redacted: out.redacted,
        }
    }
}

fn issue(level: IssueLevel, message: String) -> Issue {
    Issue { level, message }
}

/// The known-secrets file: missing is fine (no entries), a loose mode is a warning (the values stay
/// usable), an unparsable file is a warning and no entries. Values never reach the log.
pub fn load_known_secrets(path: &Path) -> (KnownSecrets, Vec<Issue>) {
    let mut issues = Vec::new();
    if !path.exists() {
        return (KnownSecrets::default(), issues);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            let mode = meta.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                issues.push(issue(
                    IssueLevel::Warning,
                    format!(
                        "{} is mode {mode:o}; it holds secret values and should be 0600 (chmod 600)",
                        path.display()
                    ),
                ));
            }
        }
    }
    match std::fs::read_to_string(path) {
        Ok(text) => match KnownSecrets::parse(&text) {
            Ok(set) => (set, issues),
            Err(err) => {
                issues.push(issue(
                    IssueLevel::Warning,
                    format!(
                        "{} could not be parsed ({err}); no known secrets are loaded",
                        path.display()
                    ),
                ));
                (KnownSecrets::default(), issues)
            }
        },
        Err(err) => {
            issues.push(issue(
                IssueLevel::Warning,
                format!(
                    "{} could not be read ({err}); no known secrets are loaded",
                    path.display()
                ),
            ));
            (KnownSecrets::default(), issues)
        }
    }
}

/// Compiles the engine for `dlp`, reading the known-secrets file from the data dir.
pub fn build_engine(dlp: &DlpConfig, paths: &Paths) -> anyhow::Result<(Arc<Engine>, Vec<Issue>)> {
    let (known, issues) = load_known_secrets(&paths.in_data_dir(&dlp.known_secrets_file));
    let engine = Engine::from_config(dlp, known)?;
    Ok((Arc::new(engine), issues))
}

/// The engine's view of a configuration as `--check-config` issues: a custom rule or allowlist regex that
/// does not compile is an error, the known-secrets file's mode a warning.
pub fn check_engine(dlp: &DlpConfig, paths: &Paths) -> Vec<Issue> {
    match build_engine(dlp, paths) {
        Ok((_, issues)) => issues,
        Err(err) => vec![issue(IssueLevel::Error, format!("{err:#}"))],
    }
}

#[cfg(test)]
mod tests {
    use gateway_core::config::Config;

    use super::*;

    #[test]
    fn a_missing_known_secrets_file_is_fine_and_a_loose_mode_warns() {
        let dir = tempfile::tempdir().unwrap();
        let (set, issues) = load_known_secrets(&dir.path().join("none.toml"));
        assert!(set.is_empty() && issues.is_empty());
        let path = dir.path().join("known-secrets.toml");
        std::fs::write(
            &path,
            "[[secret]]\nkind = \"vault\"\nvalue = \"hunter2hunter2\"\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            let (set, issues) = load_known_secrets(&path);
            assert_eq!(set.len(), 1);
            assert_eq!(issues.len(), 1);
            assert!(issues[0].message.contains("0600"), "{}", issues[0].message);
            assert!(
                !issues[0].message.contains("hunter2"),
                "values never appear in messages"
            );
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            let (_, issues) = load_known_secrets(&path);
            assert!(issues.is_empty());
        }
        std::fs::write(&path, "not toml at all = = =").unwrap();
        let (set, issues) = load_known_secrets(&path);
        assert!(set.is_empty());
        assert!(
            issues
                .iter()
                .any(|i| i.message.contains("could not be parsed"))
        );
    }

    #[test]
    fn a_bad_custom_rule_is_a_check_error() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            data_dir: dir.path().to_path_buf(),
            config_dir: dir.path().to_path_buf(),
        };
        let cfg = Config::parse("[[dlp.custom_rules]]\nid = \"bad\"\nregex = \"(\"\n").unwrap();
        let issues = check_engine(&cfg.dlp, &paths);
        assert!(
            issues.iter().any(|i| i.level == IssueLevel::Error),
            "{issues:?}"
        );
        assert!(check_engine(&Config::default().dlp, &paths).is_empty());
    }

    #[test]
    fn the_scanner_blocks_when_the_engine_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            data_dir: dir.path().to_path_buf(),
            config_dir: dir.path().to_path_buf(),
        };
        let (engine, _) = build_engine(&Config::default().dlp, &paths).unwrap();
        let scanner = DlpScanner::new(engine);
        let ctx = ScanContext {
            request_id: uuid::Uuid::nil(),
            backend: gateway_core::types::Backend::Ollama,
            api_format: gateway_core::types::ApiFormat::OpenAiChat,
            model: None,
            is_cloud: true,
            direction: gateway_core::types::Direction::Request,
        };
        let out = scanner.scan_request(
            &ctx,
            br#"{"messages":[{"role":"user","content":"AKIAIOSFODNN7EXAMPLE"}]}"#,
        );
        assert!(
            matches!(out.decision, Decision::Block { ref rule_id } if rule_id == "aws-access-key-id")
        );
        assert!(out.redacted.is_some());
        let reply = scanner.scan_response(&ctx, "nothing", b"nothing");
        assert_eq!(reply.decision, Decision::Allow);
        assert!(reply.findings.is_empty());
    }
}
