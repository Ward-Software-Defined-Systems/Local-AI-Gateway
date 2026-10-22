//! tracing setup: a daily-rotated file under the logs directory, kept for `log.max_files` days, plus a
//! stderr layer when `--headless` is given or stderr is a terminal. No request or response body, header
//! value or access token may ever reach either sink; that is an invariant every module keeps, not a
//! filter applied here.

use std::io::IsTerminal;
use std::path::Path;

use anyhow::{Context, Result};
use gateway_core::config::LogConfig;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, fmt};

/// Keeps the background log writer alive; dropping it flushes and stops it.
pub struct LogGuard(#[allow(dead_code, reason = "held only for its Drop")] WorkerGuard);

pub fn init(config: &LogConfig, logs_dir: &Path, force_stderr: bool) -> Result<LogGuard> {
    std::fs::create_dir_all(logs_dir)
        .with_context(|| format!("create logs dir {}", logs_dir.display()))?;
    let filter = EnvFilter::try_new(&config.level)
        .with_context(|| format!("invalid log filter {:?}", config.level))?;
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("local-ai-gateway")
        .filename_suffix("log")
        .max_log_files(config.max_files.max(1))
        .build(logs_dir)
        .context("open log file")?;
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let file_layer = fmt::layer().with_writer(writer).with_ansi(false);
    // Colour only on a real terminal: launchd and systemd capture stderr into files.
    let stderr_is_tty = std::io::stderr().is_terminal();
    let stderr_layer = (force_stderr || stderr_is_tty).then(|| {
        fmt::layer()
            .with_writer(std::io::stderr)
            .with_ansi(stderr_is_tty)
            .with_target(false)
    });
    tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(stderr_layer)
        .init();
    Ok(LogGuard(guard))
}
