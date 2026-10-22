//! `local-ai-gateway setup check`: is this installation going to work? Configuration issues, file
//! modes, the backends' reachability, whether the gateway's own listeners are already bound (a running
//! service), the telemetry and notification tools, and the service unit. Prints `ok` / `warn` / `fail`
//! lines and exits non-zero on a `fail`.

use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use gateway_core::config::{Config, IssueLevel};
use gateway_core::paths::Paths;
use gateway_core::types::Platform;

use crate::service;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

pub struct Check {
    pub level: Level,
    pub message: String,
}

fn ok(message: impl Into<String>) -> Check {
    Check {
        level: Level::Ok,
        message: message.into(),
    }
}

fn warn(message: impl Into<String>) -> Check {
    Check {
        level: Level::Warn,
        message: message.into(),
    }
}

fn fail(message: impl Into<String>) -> Check {
    Check {
        level: Level::Fail,
        message: message.into(),
    }
}

#[cfg(unix)]
fn mode_of(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .ok()
        .map(|m| m.permissions().mode() & 0o777)
}

#[cfg(not(unix))]
fn mode_of(_path: &Path) -> Option<u32> {
    None
}

/// A file that must be private: missing is fine, 0600 is right, anything looser is a warning.
fn private_file(label: &str, path: &Path) -> Check {
    if !path.exists() {
        return ok(format!("{label}: {} (not created yet)", path.display()));
    }
    match mode_of(path) {
        Some(mode) if mode & 0o077 == 0 => {
            ok(format!("{label}: {} (mode {mode:o})", path.display()))
        }
        Some(mode) => warn(format!(
            "{label}: {} is mode {mode:o}; run chmod 600",
            path.display()
        )),
        None => ok(format!("{label}: {}", path.display())),
    }
}

fn tool_in_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|dir| dir.join(name).is_file()))
        .unwrap_or(false)
}

/// Whether something answers on `addr` right now.
fn port_open(addr: SocketAddr) -> bool {
    TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok()
}

fn upstream_reachable(
    client: &reqwest::blocking::Client,
    upstream: &str,
) -> Result<String, String> {
    let url = format!("{}/", upstream.trim_end_matches('/'));
    match client.get(&url).send() {
        Ok(resp) => Ok(format!("HTTP {}", resp.status().as_u16())),
        Err(err) => Err(err.to_string()),
    }
}

pub fn run_checks(config: &Config, config_path: &Path, paths: &Paths, home: &Path) -> Vec<Check> {
    let mut out = Vec::new();
    let platform = Platform::current();
    out.push(ok(format!("platform: {platform}")));

    // Configuration.
    if config_path.exists() {
        out.push(ok(format!("config: {}", config_path.display())));
    } else {
        out.push(warn(format!(
            "config: {} is missing; running on defaults (run `config init`)",
            config_path.display()
        )));
    }
    let mut issues = config.validate();
    issues.extend(crate::dlp::check_engine(&config.dlp, paths));
    for issue in issues {
        match issue.level {
            IssueLevel::Error => out.push(fail(format!("config: {}", issue.message))),
            IssueLevel::Warning => out.push(warn(format!("config: {}", issue.message))),
        }
    }

    // Files.
    if paths.data_dir.exists() {
        let probe = paths.data_dir.join(".write-check");
        match std::fs::write(&probe, b"ok") {
            Ok(()) => {
                let _ = std::fs::remove_file(&probe);
                out.push(ok(format!(
                    "data dir: {} (writable)",
                    paths.data_dir.display()
                )));
            }
            Err(err) => out.push(fail(format!(
                "data dir: {} is not writable ({err})",
                paths.data_dir.display()
            ))),
        }
    } else {
        out.push(ok(format!(
            "data dir: {} (created on first start)",
            paths.data_dir.display()
        )));
    }
    out.push(private_file(
        "token file",
        &paths.in_data_dir(&config.dashboard.token_file),
    ));
    out.push(private_file("tls key", &paths.tls_dir().join("key.pem")));
    out.push(private_file(
        "known secrets",
        &paths.in_data_dir(&config.dlp.known_secrets_file),
    ));

    // Backends and listeners.
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .ok();
    for (backend, cfg) in config.backends.iter() {
        let name = backend.display_name();
        if !cfg.enabled {
            out.push(ok(format!("{name}: disabled")));
            continue;
        }
        match client
            .as_ref()
            .map(|c| upstream_reachable(c, &cfg.upstream))
        {
            Some(Ok(status)) => out.push(ok(format!(
                "{name}: upstream {} answers ({status})",
                cfg.upstream
            ))),
            Some(Err(err)) => out.push(warn(format!(
                "{name}: upstream {} is not answering ({err})",
                cfg.upstream
            ))),
            None => out.push(warn(format!(
                "{name}: could not build an HTTP client to probe {}",
                cfg.upstream
            ))),
        }
        for addr in &cfg.listen {
            if port_open(*addr) {
                out.push(ok(format!(
                    "{name}: listener {addr} is bound (a gateway is running)"
                )));
            } else {
                out.push(ok(format!(
                    "{name}: listener {addr} is free (no gateway running on it)"
                )));
            }
        }
    }
    let dash = config.dashboard.listen;
    if port_open(dash) {
        out.push(ok(format!(
            "dashboard: {dash} is bound (a gateway is running); `--print-token` prints the URL"
        )));
    } else {
        out.push(ok(format!("dashboard: {dash} is free")));
    }

    // Tools.
    match platform {
        Platform::Macos => {
            out.push(
                if tool_in_path("ioreg") || Path::new("/usr/sbin/ioreg").exists() {
                    ok("gpu telemetry: ioreg present")
                } else {
                    warn("gpu telemetry: ioreg not found; GPU utilisation will be n/a")
                },
            );
            out.push(
                if tool_in_path("osascript") || Path::new("/usr/bin/osascript").exists() {
                    ok("desktop notifications: osascript present")
                } else {
                    warn("desktop notifications: osascript not found")
                },
            );
        }
        Platform::Linux => {
            out.push(if tool_in_path("nvidia-smi") {
                ok("gpu telemetry: nvidia-smi present")
            } else {
                warn("gpu telemetry: nvidia-smi not found; GPU utilisation and VRAM will be n/a")
            });
            out.push(if tool_in_path("notify-send") {
                ok("desktop notifications: notify-send present")
            } else {
                warn("desktop notifications: notify-send not found (headless server?); use the webhook sink for notifications")
            });
        }
        Platform::Other => out.push(warn(
            "unsupported platform: GPU telemetry, notifications and the service manager are off",
        )),
    }
    if config.alerts.webhook.enabled {
        match &config.alerts.webhook.url {
            Some(url) => out.push(ok(format!("webhook: {url}"))),
            None => out.push(fail("webhook: enabled without a url")),
        }
    }

    // Service.
    match service::unit_path(platform, home) {
        Some(unit) if unit.exists() => {
            out.push(ok(format!("service: installed ({})", unit.display())))
        }
        Some(unit) => out.push(ok(format!(
            "service: not installed (`service install` writes {})",
            unit.display()
        ))),
        None => {}
    }
    out
}

/// Prints every check; `Ok(false)` when any failed.
pub fn print(checks: &[Check]) -> bool {
    let mut failed = false;
    for check in checks {
        let tag = match check.level {
            Level::Ok => "ok  ",
            Level::Warn => "warn",
            Level::Fail => {
                failed = true;
                "FAIL"
            }
        };
        println!("{tag}  {}", check.message);
    }
    !failed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_default_config_in_a_temp_dir_has_no_failures() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            data_dir: dir.path().join("data"),
            config_dir: dir.path().to_path_buf(),
        };
        let mut config = Config::default();
        // Nothing listens on these during tests; the probes must not hang.
        config.backends.lmstudio.upstream = "http://127.0.0.1:9".into();
        config.backends.ollama.upstream = "http://127.0.0.1:9".into();
        let checks = run_checks(&config, &dir.path().join("config.toml"), &paths, dir.path());
        assert!(
            checks.iter().all(|c| c.level != Level::Fail),
            "{:?}",
            checks.iter().map(|c| &c.message).collect::<Vec<_>>()
        );
        assert!(
            checks
                .iter()
                .any(|c| c.message.contains("is missing; running on defaults"))
        );
        assert!(checks.iter().any(|c| c.message.contains("not answering")));
        assert!(print(&checks));
    }

    #[test]
    fn a_loose_token_file_warns_and_a_bad_rule_fails() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            data_dir: dir.path().to_path_buf(),
            config_dir: dir.path().to_path_buf(),
        };
        let token = dir.path().join("dashboard.token");
        std::fs::write(&token, "x".repeat(32)).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        let mut config =
            Config::parse("[[dlp.custom_rules]]\nid = \"bad\"\nregex = \"(\"\n").unwrap();
        config.backends.lmstudio.enabled = false;
        config.backends.ollama.enabled = false;
        let checks = run_checks(&config, &dir.path().join("config.toml"), &paths, dir.path());
        assert!(
            checks
                .iter()
                .any(|c| c.level == Level::Fail && c.message.contains("regex"))
        );
        #[cfg(unix)]
        assert!(
            checks
                .iter()
                .any(|c| c.level == Level::Warn && c.message.contains("chmod 600"))
        );
        assert!(!print(&checks));
    }
}
