//! `local-ai-gateway service …`: a launchd agent on macOS, a systemd user unit on Linux. The unit text is
//! generated here (pure, tested) and written next to the platform's other user units; the lifecycle
//! commands shell out to `launchctl` or `systemctl --user`. The service runs the same binary with
//! explicit `--config` and `--data-dir`, so it depends on no environment.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use gateway_core::types::Platform;

use crate::cli::ServiceAction;

pub const LAUNCHD_LABEL: &str = "io.wsds.local-ai-gateway";
pub const SYSTEMD_UNIT: &str = "local-ai-gateway.service";

/// Everything the unit text needs.
#[derive(Debug, Clone)]
pub struct ServiceContext {
    pub exe: PathBuf,
    pub config_path: PathBuf,
    pub data_dir: PathBuf,
    pub logs_dir: PathBuf,
    pub home: PathBuf,
}

/// Where the unit file lives for `platform`.
pub fn unit_path(platform: Platform, home: &Path) -> Option<PathBuf> {
    match platform {
        Platform::Macos => Some(
            home.join("Library")
                .join("LaunchAgents")
                .join(format!("{LAUNCHD_LABEL}.plist")),
        ),
        Platform::Linux => Some(
            home.join(".config")
                .join("systemd")
                .join("user")
                .join(SYSTEMD_UNIT),
        ),
        Platform::Other => None,
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The launchd agent: restart on failure only (a clean exit after `service stop` stays stopped), ten
/// seconds to exit (the gateway's own grace is five), output to the logs directory.
pub fn launchd_plist(ctx: &ServiceContext) -> String {
    let args = [
        ctx.exe.display().to_string(),
        "--config".to_string(),
        ctx.config_path.display().to_string(),
        "--data-dir".to_string(),
        ctx.data_dir.display().to_string(),
    ]
    .iter()
    .map(|a| format!("    <string>{}</string>", xml_escape(a)))
    .collect::<Vec<_>>()
    .join("\n");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
{args}
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>ThrottleInterval</key>
  <integer>5</integer>
  <key>ExitTimeOut</key>
  <integer>10</integer>
  <key>ProcessType</key>
  <string>Adaptive</string>
  <key>StandardOutPath</key>
  <string>{out}</string>
  <key>StandardErrorPath</key>
  <string>{err}</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>LAIG_LOG</key>
    <string>info</string>
  </dict>
</dict>
</plist>
"#,
        label = LAUNCHD_LABEL,
        out = xml_escape(&ctx.logs_dir.join("service.out.log").display().to_string()),
        err = xml_escape(&ctx.logs_dir.join("service.err.log").display().to_string()),
    )
}

fn systemd_quote(path: &Path) -> String {
    let s = path.display().to_string();
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._-:+@%".contains(c))
    {
        s
    } else {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

/// The systemd user unit. `loginctl enable-linger` keeps it running on a headless server with no login
/// session; the README says so.
pub fn systemd_unit(ctx: &ServiceContext) -> String {
    format!(
        "[Unit]\n\
         Description=WSDS Local AI Gateway\n\
         Documentation=https://github.com/Ward-Software-Defined-Systems/Local-AI-Gateway\n\
         After=network-online.target\n\
         \n\
         [Service]\n\
         ExecStart={exe} --config {config} --data-dir {data}\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         TimeoutStopSec=15\n\
         KillSignal=SIGTERM\n\
         Environment=LAIG_LOG=info\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exe = systemd_quote(&ctx.exe),
        config = systemd_quote(&ctx.config_path),
        data = systemd_quote(&ctx.data_dir),
    )
}

/// The unit text for the current platform.
pub fn unit_text(platform: Platform, ctx: &ServiceContext) -> Result<String> {
    match platform {
        Platform::Macos => Ok(launchd_plist(ctx)),
        Platform::Linux => Ok(systemd_unit(ctx)),
        Platform::Other => {
            bail!("service management is available on macOS (launchd) and Linux (systemd) only")
        }
    }
}

/// The binary the service should run: `--exe` when given, else this executable. A build directory is
/// a warning, not an error: `cargo build` replaces the file under the running service.
pub fn resolve_exe(explicit: Option<PathBuf>) -> Result<PathBuf> {
    let exe = match explicit {
        Some(path) => path,
        None => std::env::current_exe().context("locate this executable")?,
    };
    let exe = exe.canonicalize().unwrap_or(exe);
    if exe.components().any(|c| c.as_os_str() == "target") {
        eprintln!(
            "warning: {} is inside a build directory; copy the binary somewhere stable (for example ~/.local/bin) and pass --exe",
            exe.display()
        );
    }
    Ok(exe)
}

fn run_cmd(program: &str, args: &[String]) -> Result<std::process::Output> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("run {program}"))?;
    Ok(output)
}

fn print_output(output: &std::process::Output) {
    let out = String::from_utf8_lossy(&output.stdout);
    let err = String::from_utf8_lossy(&output.stderr);
    for line in out.lines().chain(err.lines()) {
        if !line.trim().is_empty() {
            println!("  {line}");
        }
    }
}

fn ensure_ok(name: &str, output: std::process::Output) -> Result<()> {
    if output.status.success() {
        print_output(&output);
        Ok(())
    } else {
        print_output(&output);
        bail!("{name} failed with {}", output.status)
    }
}

fn uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

pub fn run(action: ServiceAction, ctx: &ServiceContext, dry_run: bool) -> Result<()> {
    let platform = Platform::current();
    let Some(path) = unit_path(platform, &ctx.home) else {
        bail!("service management is available on macOS (launchd) and Linux (systemd) only");
    };
    match action {
        ServiceAction::Install => {
            let text = unit_text(platform, ctx)?;
            if dry_run {
                println!("# would write {}", path.display());
                print!("{text}");
                return Ok(());
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
            }
            std::fs::create_dir_all(&ctx.logs_dir)
                .with_context(|| format!("create {}", ctx.logs_dir.display()))?;
            std::fs::write(&path, &text).with_context(|| format!("write {}", path.display()))?;
            println!("wrote {}", path.display());
            match platform {
                Platform::Macos => {
                    let domain = format!("gui/{}", uid());
                    let output = run_cmd(
                        "launchctl",
                        &[
                            "bootstrap".into(),
                            domain.clone(),
                            path.display().to_string(),
                        ],
                    )?;
                    if output.status.success() {
                        print_output(&output);
                    } else {
                        // Already loaded: replace it.
                        let _ = run_cmd(
                            "launchctl",
                            &["bootout".into(), format!("{domain}/{LAUNCHD_LABEL}")],
                        );
                        ensure_ok(
                            "launchctl bootstrap",
                            run_cmd(
                                "launchctl",
                                &[
                                    "bootstrap".into(),
                                    domain.clone(),
                                    path.display().to_string(),
                                ],
                            )?,
                        )?;
                    }
                    println!("installed and started {LAUNCHD_LABEL} (launchd, {domain})");
                }
                Platform::Linux => {
                    ensure_ok(
                        "systemctl daemon-reload",
                        run_cmd("systemctl", &["--user".into(), "daemon-reload".into()])?,
                    )?;
                    ensure_ok(
                        "systemctl enable --now",
                        run_cmd(
                            "systemctl",
                            &[
                                "--user".into(),
                                "enable".into(),
                                "--now".into(),
                                SYSTEMD_UNIT.into(),
                            ],
                        )?,
                    )?;
                    println!("installed and started {SYSTEMD_UNIT} (systemd --user)");
                    println!(
                        "note: on a server without a login session run `loginctl enable-linger $USER` once so the unit survives logout"
                    );
                }
                Platform::Other => unreachable!("checked above"),
            }
            println!("logs: {}", ctx.logs_dir.display());
            Ok(())
        }
        ServiceAction::Uninstall => {
            if dry_run {
                println!("# would stop the service and remove {}", path.display());
                return Ok(());
            }
            match platform {
                Platform::Macos => {
                    let domain = format!("gui/{}", uid());
                    let output = run_cmd(
                        "launchctl",
                        &["bootout".into(), format!("{domain}/{LAUNCHD_LABEL}")],
                    )?;
                    print_output(&output);
                }
                Platform::Linux => {
                    let output = run_cmd(
                        "systemctl",
                        &[
                            "--user".into(),
                            "disable".into(),
                            "--now".into(),
                            SYSTEMD_UNIT.into(),
                        ],
                    )?;
                    print_output(&output);
                }
                Platform::Other => unreachable!("checked above"),
            }
            if path.exists() {
                std::fs::remove_file(&path)
                    .with_context(|| format!("remove {}", path.display()))?;
                println!("removed {}", path.display());
            } else {
                println!("{} was not installed", path.display());
            }
            if platform == Platform::Linux {
                let _ = run_cmd("systemctl", &["--user".into(), "daemon-reload".into()]);
            }
            Ok(())
        }
        ServiceAction::Status => {
            if !path.exists() {
                println!("not installed ({} is missing)", path.display());
                return Ok(());
            }
            println!("unit: {}", path.display());
            match platform {
                Platform::Macos => {
                    let domain = format!("gui/{}", uid());
                    let output = run_cmd(
                        "launchctl",
                        &["print".into(), format!("{domain}/{LAUNCHD_LABEL}")],
                    )?;
                    if output.status.success() {
                        let text = String::from_utf8_lossy(&output.stdout);
                        for line in text.lines() {
                            let t = line.trim();
                            if t.starts_with("state =")
                                || t.starts_with("pid =")
                                || t.starts_with("last exit code")
                                || t.starts_with("runs =")
                            {
                                println!("  {t}");
                            }
                        }
                    } else {
                        println!("  not loaded (run `service install` or `service start`)");
                    }
                }
                Platform::Linux => {
                    let output = run_cmd(
                        "systemctl",
                        &[
                            "--user".into(),
                            "status".into(),
                            "--no-pager".into(),
                            SYSTEMD_UNIT.into(),
                        ],
                    )?;
                    print_output(&output);
                }
                Platform::Other => unreachable!("checked above"),
            }
            Ok(())
        }
        ServiceAction::Start | ServiceAction::Stop | ServiceAction::Restart => {
            if dry_run {
                println!("# would {action:?} the service");
                return Ok(());
            }
            if !path.exists() {
                bail!("not installed: run `local-ai-gateway service install` first");
            }
            match platform {
                Platform::Macos => {
                    let target = format!("gui/{}/{LAUNCHD_LABEL}", uid());
                    let args: Vec<String> = match action {
                        ServiceAction::Start => vec!["kickstart".into(), target],
                        ServiceAction::Stop => vec!["kill".into(), "SIGTERM".into(), target],
                        _ => vec!["kickstart".into(), "-k".into(), target],
                    };
                    ensure_ok("launchctl", run_cmd("launchctl", &args)?)?;
                }
                Platform::Linux => {
                    let verb = match action {
                        ServiceAction::Start => "start",
                        ServiceAction::Stop => "stop",
                        _ => "restart",
                    };
                    ensure_ok(
                        "systemctl",
                        run_cmd(
                            "systemctl",
                            &["--user".into(), verb.into(), SYSTEMD_UNIT.into()],
                        )?,
                    )?;
                }
                Platform::Other => unreachable!("checked above"),
            }
            println!("{action:?} sent");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ServiceContext {
        ServiceContext {
            exe: PathBuf::from("/opt/laig/bin/local-ai-gateway"),
            config_path: PathBuf::from("/home/u/.config/local-ai-gateway/config.toml"),
            data_dir: PathBuf::from("/home/u/.local/share/local-ai-gateway"),
            logs_dir: PathBuf::from("/home/u/.local/share/local-ai-gateway/logs"),
            home: PathBuf::from("/home/u"),
        }
    }

    #[test]
    fn the_launchd_plist_runs_the_binary_with_explicit_paths_and_restarts_on_failure_only() {
        let plist = launchd_plist(&ctx());
        assert!(plist.contains("<string>io.wsds.local-ai-gateway</string>"));
        assert!(plist.contains(
            "<string>/opt/laig/bin/local-ai-gateway</string>\n    <string>--config</string>"
        ));
        assert!(plist.contains("<key>SuccessfulExit</key>\n    <false/>"));
        assert!(
            plist.contains("<integer>10</integer>"),
            "ExitTimeOut above the 5 s grace"
        );
        assert!(plist.contains("/logs/service.err.log</string>"));
        let odd = ServiceContext {
            exe: PathBuf::from("/odd <path>&/bin"),
            ..ctx()
        };
        assert!(launchd_plist(&odd).contains("<string>/odd &lt;path&gt;&amp;/bin</string>"));
    }

    #[test]
    fn the_systemd_unit_is_a_user_unit_with_a_stop_timeout_above_the_grace() {
        let unit = systemd_unit(&ctx());
        assert!(unit.contains("ExecStart=/opt/laig/bin/local-ai-gateway --config /home/u/.config/local-ai-gateway/config.toml --data-dir /home/u/.local/share/local-ai-gateway\n"));
        assert!(unit.contains("TimeoutStopSec=15"));
        assert!(unit.contains("WantedBy=default.target"));
        let spaced = ServiceContext {
            data_dir: PathBuf::from("/home/u/My Data"),
            ..ctx()
        };
        assert!(systemd_unit(&spaced).contains("--data-dir \"/home/u/My Data\""));
    }

    #[test]
    fn unit_paths_follow_the_platform() {
        let home = Path::new("/home/u");
        assert_eq!(
            unit_path(Platform::Macos, home).unwrap(),
            PathBuf::from("/home/u/Library/LaunchAgents/io.wsds.local-ai-gateway.plist")
        );
        assert_eq!(
            unit_path(Platform::Linux, home).unwrap(),
            PathBuf::from("/home/u/.config/systemd/user/local-ai-gateway.service")
        );
        assert!(unit_path(Platform::Other, home).is_none());
    }
}
