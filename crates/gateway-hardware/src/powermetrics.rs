//! Optional macOS telemetry: `sudo -n /usr/bin/powermetrics --samplers cpu_power,gpu_power,ane_power -i
//! <ms>` as a long-lived child, parsed line by line for `ANE Power` (and GPU active residency, used only
//! when `ioreg` gave nothing). It needs root; `-n` never prompts, so a service relies on a sudoers rule
//! such as `<user> ALL=(root) NOPASSWD: /usr/bin/powermetrics`. The child gets no stdin and its own process
//! group; on shutdown it receives SIGTERM, which sudo relays so nothing lingers as root. If it exits on
//! its own the readings clear and it is not restarted. Ported from LMS-Monitor's `powermetrics.rs`.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tracing::{info, trace, warn};

pub const PROGRAM: &str = "/usr/bin/powermetrics";

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Readings {
    pub gpu_util_pct: Option<f32>,
    pub ane_power_mw: Option<f32>,
}

pub type Shared = Arc<Mutex<Readings>>;

pub fn command(interval: Duration) -> Command {
    let mut cmd = Command::new("sudo");
    cmd.arg("-n")
        .arg(PROGRAM)
        .arg("--samplers")
        .arg("cpu_power,gpu_power,ane_power")
        .arg("-i")
        .arg(interval.as_millis().to_string())
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    cmd
}

/// The running child and the readings it feeds.
pub struct Handle {
    pub readings: Shared,
    child: Child,
}

impl Handle {
    /// SIGTERM to sudo (relayed to powermetrics), then a short wait.
    pub async fn terminate(mut self) {
        if let Some(pid) = self.child.id() {
            // SAFETY: pid is a live child's OS process id from tokio.
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
        }
        let _ = tokio::time::timeout(Duration::from_secs(2), self.child.wait()).await;
    }
}

/// Spawns the child and the task that parses its output. `None`, with a warning, when it cannot start.
pub fn spawn(interval: Duration) -> Option<Handle> {
    let mut child = match command(interval).spawn() {
        Ok(child) => child,
        Err(err) => {
            warn!(error = %err, "could not start sudo powermetrics; ANE power will be n/a");
            return None;
        }
    };
    let stdout = child.stdout.take()?;
    let readings: Shared = Arc::default();
    let shared = readings.clone();
    tokio::spawn(async move {
        let mut state = State::default();
        let mut lines = BufReader::new(stdout).lines();
        let ended = loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    trace!(target: "gateway_hardware::powermetrics", "{line}");
                    if state.apply_line(&line) {
                        *shared.lock().await = state.readings;
                    }
                }
                Ok(None) => break "powermetrics exited".to_string(),
                Err(err) => break format!("powermetrics read failed: {err}"),
            }
        };
        *shared.lock().await = Readings::default();
        info!("{ended}; ANE power will show n/a (sudo -n needs a NOPASSWD rule for a service)");
    });
    Some(Handle { readings, child })
}

#[derive(Debug, Default)]
pub struct State {
    pub readings: Readings,
}

impl State {
    /// Returns whether the line carried a reading.
    pub fn apply_line(&mut self, line: &str) -> bool {
        if let Some(pct) = parse_gpu_active_residency(line) {
            self.readings.gpu_util_pct = Some(pct);
            true
        } else if let Some(mw) = parse_ane_power(line) {
            self.readings.ane_power_mw = Some(mw);
            true
        } else {
            false
        }
    }
}

/// `GPU HW active residency:   12.34% (...)` or the older `GPU active residency: 9.5%`.
fn parse_gpu_active_residency(line: &str) -> Option<f32> {
    let l = line.trim();
    if !l.starts_with("GPU") || !l.to_ascii_lowercase().contains("active residency") {
        return None;
    }
    let after_colon = l.split_once(':')?.1;
    after_colon.trim().split('%').next()?.trim().parse().ok()
}

/// `ANE Power: 12 mW`.
fn parse_ane_power(line: &str) -> Option<f32> {
    let l = line.trim();
    if !l.to_ascii_lowercase().starts_with("ane power") {
        return None;
    }
    l.split_once(':')?.1.split_whitespace().next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gpu_residency_and_ane_lines() {
        let mut state = State::default();
        assert!(state.apply_line("GPU HW active residency:   12.34% (338 MHz: 1%)"));
        assert!(state.apply_line("ANE Power: 25 mW"));
        assert!(state.apply_line("GPU active residency: 9.5%"));
        assert!(!state.apply_line("System Average frequency: 1234 MHz"));
        assert!(!state.apply_line("CPU Power: 1500 mW"));
        assert!((state.readings.gpu_util_pct.unwrap() - 9.5).abs() < 1e-3);
        assert!((state.readings.ane_power_mw.unwrap() - 25.0).abs() < 1e-3);
        assert!((parse_ane_power("ANE Power:0 mW").unwrap()).abs() < 1e-3);
    }

    #[test]
    fn the_command_is_non_interactive_and_absolute() {
        let cmd = command(Duration::from_secs(2));
        let std_cmd = cmd.as_std();
        assert_eq!(std_cmd.get_program(), "sudo");
        let args: Vec<String> = std_cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args[0], "-n");
        assert_eq!(args[1], PROGRAM);
        assert_eq!(args.last().unwrap(), "2000");
    }
}
