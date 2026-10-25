//! The settings that change without a restart: the DLP engine (rules, policy, allowlist, known secrets),
//! the alert sinks, the capture and retention limits and the pricing table. `Hot::apply` validates a whole
//! configuration, swaps those pieces in place and reports which other keys would need a restart. SIGHUP,
//! `POST /api/config/reload` and `PUT /api/config` all end here.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result, bail};
use gateway_core::config::{Config, Issue, IssueLevel};
use gateway_core::events::{EventSender, GatewayEvent, publish};
use gateway_core::paths::Paths;
use gateway_core::pricing::PricingTable;
use gateway_core::routing::RoutingTable;
use gateway_core::store::RetentionPolicy;
use gateway_dlp::Engine;
use gateway_proxy::{CapturePolicy, ProxyContext};
use serde::Serialize;
use tracing::info;

use crate::alerts::{AlertSettings, SharedSettings};
use crate::dlp::{self, DlpScanner};

/// Flags that override the file; re-applied to every reload so a file reload does not look like a change.
#[derive(Debug, Clone, Default)]
pub struct CliOverrides {
    pub dashboard_listen: Option<SocketAddr>,
    pub log_level: Option<String>,
}

impl CliOverrides {
    pub fn apply(&self, config: &mut Config) {
        if let Some(addr) = self.dashboard_listen {
            config.dashboard.listen = addr;
        }
        if let Some(level) = &self.log_level {
            config.log.level = level.clone();
        }
    }
}

pub struct HotParts {
    pub config: Config,
    pub config_path: PathBuf,
    pub paths: Paths,
    pub overrides: CliOverrides,
    pub engine: Arc<Engine>,
    pub pricing: Arc<PricingTable>,
    pub proxy: Option<Arc<ProxyContext>>,
    pub events: EventSender,
}

pub struct Hot {
    /// What the process started with; the non-hot keys are compared against this.
    boot: Arc<Config>,
    config: RwLock<Arc<Config>>,
    engine: RwLock<Arc<Engine>>,
    pricing: RwLock<Arc<PricingTable>>,
    retention: RwLock<RetentionPolicy>,
    alert_settings: SharedSettings,
    config_path: PathBuf,
    paths: Paths,
    overrides: CliOverrides,
    proxy: Option<Arc<ProxyContext>>,
    events: EventSender,
}

/// What a reload did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ApplyReport {
    /// Sections whose new values are in effect.
    pub applied: Vec<String>,
    /// Sections that differ from what the process started with and only change on restart.
    pub restart_required: Vec<String>,
    pub warnings: Vec<String>,
    pub known_secrets: usize,
    pub rules_enabled: usize,
}

impl Hot {
    pub fn new(parts: HotParts) -> Arc<Hot> {
        // The overrides are applied to the boot config too, so a reload of the same file is a no-op.
        let mut config = parts.config;
        parts.overrides.apply(&mut config);
        let config = Arc::new(config);
        let alert_settings = Arc::new(RwLock::new(AlertSettings::from_config(
            &config.alerts,
            &config.dlp,
            &parts.paths,
        )));
        Arc::new(Hot {
            boot: config.clone(),
            retention: RwLock::new(RetentionPolicy::from_config(&config.capture)),
            config: RwLock::new(config),
            engine: RwLock::new(parts.engine),
            pricing: RwLock::new(parts.pricing),
            alert_settings,
            config_path: parts.config_path,
            paths: parts.paths,
            overrides: parts.overrides,
            proxy: parts.proxy,
            events: parts.events,
        })
    }

    /// No proxy, a disabled engine and the baked pricing, for tests.
    #[cfg(test)]
    pub fn standalone(config: Arc<Config>, events: EventSender) -> Arc<Hot> {
        Hot::new(HotParts {
            config: (*config).clone(),
            config_path: PathBuf::from("config.toml"),
            paths: Paths {
                data_dir: PathBuf::from("."),
                config_dir: PathBuf::from("."),
            },
            overrides: CliOverrides::default(),
            engine: Arc::new(Engine::disabled()),
            pricing: Arc::new(PricingTable::baked()),
            proxy: None,
            events,
        })
    }

    pub fn config(&self) -> Arc<Config> {
        self.config
            .read()
            .map(|c| c.clone())
            .unwrap_or_else(|_| self.boot.clone())
    }

    pub fn engine(&self) -> Arc<Engine> {
        self.engine
            .read()
            .map(|e| e.clone())
            .unwrap_or_else(|_| Arc::new(Engine::disabled()))
    }

    pub fn pricing(&self) -> Arc<PricingTable> {
        self.pricing
            .read()
            .map(|p| p.clone())
            .unwrap_or_else(|_| Arc::new(PricingTable::baked()))
    }

    pub fn retention(&self) -> RetentionPolicy {
        self.retention
            .read()
            .map(|r| r.clone())
            .unwrap_or_else(|_| RetentionPolicy::from_config(&self.boot.capture))
    }

    pub fn alert_settings(&self) -> SharedSettings {
        self.alert_settings.clone()
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    /// Every issue with `config`: the structural checks, the pricing table and the DLP engine's compile.
    pub fn check(&self, config: &Config) -> Vec<Issue> {
        let mut issues = config.validate();
        match PricingTable::load(config.pricing.as_ref()) {
            Ok(table) => {
                for key in table.missing_frontier() {
                    issues.push(Issue {
                        level: IssueLevel::Warning,
                        message: format!(
                            "no price for frontier model {key}; its cost column shows (no rate)"
                        ),
                    });
                }
            }
            Err(err) => issues.push(Issue {
                level: IssueLevel::Error,
                message: format!("{err:#}"),
            }),
        }
        issues.extend(dlp::check_engine(&config.dlp, &self.paths));
        issues
    }

    /// Validates and swaps the hot pieces. Errors leave everything as it was.
    pub fn apply(&self, mut config: Config) -> Result<ApplyReport> {
        self.overrides.apply(&mut config);
        let issues = self.check(&config);
        if Config::has_errors(&issues) {
            let errors: Vec<String> = issues
                .into_iter()
                .filter(|i| i.level == IssueLevel::Error)
                .map(|i| i.message)
                .collect();
            bail!("{}", errors.join("; "));
        }
        let warnings: Vec<String> = issues.into_iter().map(|i| i.message).collect();
        let (engine, _) = dlp::build_engine(&config.dlp, &self.paths)?;
        let pricing = Arc::new(PricingTable::load(config.pricing.as_ref())?);

        let current = self.config();
        let mut applied = Vec::new();
        if current.dlp != config.dlp {
            applied.push("dlp".to_string());
        }
        if current.alerts != config.alerts {
            applied.push("alerts".to_string());
        }
        if current.capture != config.capture {
            applied.push("capture".to_string());
        }
        if current.pricing != config.pricing {
            applied.push("pricing".to_string());
        }
        if engine.known_secret_count() != self.engine().known_secret_count() {
            applied.push("known_secrets".to_string());
        }
        if current.unified.routing_part() != config.unified.routing_part() {
            applied.push("unified".to_string());
        }

        if let Some(proxy) = &self.proxy {
            proxy.set_scanner(Arc::new(DlpScanner::new(engine.clone())));
            proxy.set_capture(CapturePolicy::from(&config.capture));
            proxy.set_pricing(pricing.clone());
            proxy.set_routing(Arc::new(RoutingTable::from_config(&config.unified)));
        }
        if let Ok(mut slot) = self.alert_settings.write() {
            *slot = AlertSettings::from_config(&config.alerts, &config.dlp, &self.paths);
        }
        if let Ok(mut slot) = self.retention.write() {
            *slot = RetentionPolicy::from_config(&config.capture);
        }
        if let Ok(mut slot) = self.pricing.write() {
            *slot = pricing;
        }
        let report = ApplyReport {
            applied,
            restart_required: restart_required(&self.boot, &config),
            warnings,
            known_secrets: engine.known_secret_count(),
            rules_enabled: engine.rules().iter().filter(|r| r.enabled).count(),
        };
        if let Ok(mut slot) = self.engine.write() {
            *slot = engine;
        }
        if let Ok(mut slot) = self.config.write() {
            *slot = Arc::new(config);
        }
        info!(
            applied = ?report.applied,
            restart_required = ?report.restart_required,
            known_secrets = report.known_secrets,
            "configuration applied"
        );
        publish(
            &self.events,
            GatewayEvent::ConfigChanged {
                restart_required: report.restart_required.clone(),
            },
        );
        Ok(report)
    }

    /// Re-reads `config.toml` (a missing file is all defaults) and applies it.
    pub fn reload_from_disk(&self) -> Result<ApplyReport> {
        let config = Config::load(&self.config_path)?;
        self.apply(config)
    }

    /// Parses `text`, refuses it on errors, otherwise writes it to `config.toml` atomically and applies it.
    pub fn save_and_apply(&self, text: &str) -> Result<ApplyReport> {
        let mut config = Config::parse(text)?;
        self.overrides.apply(&mut config);
        let issues = self.check(&config);
        if Config::has_errors(&issues) {
            let errors: Vec<String> = issues
                .into_iter()
                .filter(|i| i.level == IssueLevel::Error)
                .map(|i| i.message)
                .collect();
            bail!("{}", errors.join("; "));
        }
        write_atomic(&self.config_path, text)?;
        self.apply(config)
    }
}

/// The sections that only take effect on restart, where `next` differs from `boot`.
pub fn restart_required(boot: &Config, next: &Config) -> Vec<String> {
    let mut out = Vec::new();
    if boot.dashboard != next.dashboard {
        out.push("dashboard".to_string());
    }
    for (backend, cfg) in next.backends.iter() {
        if boot.backends.get(backend) != cfg {
            out.push(format!("backends.{}", backend.as_str()));
        }
    }
    if boot.unified.enabled != next.unified.enabled || boot.unified.listen != next.unified.listen {
        out.push("unified.listen".to_string());
    }
    if boot.unified.tls != next.unified.tls {
        out.push("unified.tls".to_string());
    }
    if boot.tls != next.tls {
        out.push("tls".to_string());
    }
    if boot.hardware != next.hardware {
        out.push("hardware".to_string());
    }
    if boot.log != next.log {
        out.push("log".to_string());
    }
    out
}

/// Write-then-rename, so a crash mid-write never leaves a half file where the configuration was.
pub fn write_atomic(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {} to {}", tmp.display(), path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use gateway_core::events;

    use super::*;

    fn hot_in(dir: &Path) -> Arc<Hot> {
        let (events, _rx) = events::channel();
        let paths = Paths {
            data_dir: dir.to_path_buf(),
            config_dir: dir.to_path_buf(),
        };
        let (engine, _) = dlp::build_engine(&Config::default().dlp, &paths).unwrap();
        Hot::new(HotParts {
            config: Config::default(),
            config_path: dir.join("config.toml"),
            paths,
            overrides: CliOverrides {
                dashboard_listen: Some("127.0.0.1:1".parse().unwrap()),
                log_level: None,
            },
            engine,
            pricing: Arc::new(PricingTable::baked()),
            proxy: None,
            events,
        })
    }

    #[test]
    fn applying_a_dlp_change_swaps_the_engine_and_reports_restart_keys() {
        let dir = tempfile::tempdir().unwrap();
        let hot = hot_in(dir.path());
        let before = hot.engine().rules().iter().filter(|r| r.enabled).count();
        let report = hot
            .save_and_apply(
                "[dlp.rules.email]\nenabled = false\n[backends.ollama]\nlisten = [\"127.0.0.1:2\"]\n[capture]\nretention_days = 3\n\
                 [unified]\nlisten = [\"127.0.0.1:3\"]\n[unified.aliases]\nfast = \"ollama/gemma4:12b-mlx\"\n",
            )
            .unwrap();
        assert_eq!(report.applied, vec!["dlp", "capture", "unified"]);
        assert_eq!(
            report.restart_required,
            vec!["backends.ollama", "unified.listen"],
            "the routing table reloads live; the address needs a restart"
        );
        assert_eq!(
            hot.config().unified.aliases.get("fast").map(String::as_str),
            Some("ollama/gemma4:12b-mlx")
        );
        assert_eq!(report.rules_enabled, before - 1);
        assert_eq!(hot.retention().body_retention, chrono::Duration::days(3));
        assert!(dir.path().join("config.toml").exists());
        assert!(!dir.path().join("config.toml.tmp").exists());
        // The CLI override is re-applied, so the file's dashboard address is not a restart item.
        assert_eq!(hot.config().dashboard.listen.port(), 1);
    }

    #[test]
    fn tls_changes_are_restart_items() {
        let dir = tempfile::tempdir().unwrap();
        let hot = hot_in(dir.path());
        let report = hot
            .save_and_apply("[unified]\ntls = \"on\"\n[tls]\nsans = [\"gateway.lan\"]\n")
            .unwrap();
        assert_eq!(report.restart_required, vec!["unified.tls", "tls"]);
        assert!(report.applied.is_empty(), "{:?}", report.applied);
        assert_eq!(hot.config().tls.sans, vec!["gateway.lan"]);
    }

    #[test]
    fn errors_leave_everything_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let hot = hot_in(dir.path());
        assert!(hot.save_and_apply("[dlp\n").is_err(), "a syntax error");
        assert!(
            hot.save_and_apply("[[dlp.custom_rules]]\nid = \"x\"\nregex = \"(\"\n")
                .is_err(),
            "a bad regex"
        );
        assert!(
            hot.save_and_apply("[dlp]\nmax_scan_bytes = 0\n").is_err(),
            "a validation error"
        );
        assert!(
            !dir.path().join("config.toml").exists(),
            "nothing was written"
        );
        let mut expected = Config::default();
        expected.dashboard.listen = "127.0.0.1:1".parse().unwrap();
        assert_eq!(
            *hot.config(),
            expected,
            "the boot config with the CLI override"
        );
        let report = hot.reload_from_disk().unwrap();
        assert!(report.applied.is_empty(), "{report:?}");
    }
}
