//! WSDS Local AI Gateway: one binary that proxies LM Studio and Ollama, records every request, scans for
//! secrets and serves a web dashboard. This file wires the pieces in a fixed order: CLI → paths → config
//! → logging → bind every socket → runtime → store and session → tasks → graceful shutdown.

mod alerts;
mod cli;
mod dlp;
mod feed;
mod hot;
mod logging;
mod service;
mod setup;
mod web;

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use clap::Parser;
use gateway_backends::PollerConfig;
use gateway_core::aggregate::Aggregator;
use gateway_core::config::{Config, IssueLevel, SAMPLE};
use gateway_core::events::{self, EventSender, GatewayEvent, publish};
use gateway_core::paths::Paths;
use gateway_core::pricing::PricingTable;
use gateway_core::routing::RoutingTable;
use gateway_core::state::BackendsState;
use gateway_core::store::{Store, StoreHandle};
use gateway_core::types::Platform;
use gateway_hardware::SamplerConfig;
use gateway_hardware::sampler::Latest;
use gateway_proxy::{CapturePolicy, ListenerConfig, ProxyContext};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::cli::{Cli, Command, ConfigAction, SetupAction};
use crate::hot::{CliOverrides, Hot, HotParts};
use crate::web::token::TokenSource;
use crate::web::{AppState, AppStateParts, ListenerStatus};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long open connections get after a shutdown signal before they are cut.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
/// Retention runs this often, and once shortly after start.
const RETENTION_EVERY: Duration = Duration::from_secs(600);

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("local-ai-gateway: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// Everything bound and loaded before the runtime starts.
struct Startup {
    config: Arc<Config>,
    config_path: PathBuf,
    overrides: CliOverrides,
    paths: Paths,
    pricing: Arc<PricingTable>,
    dashboard_listener: Option<std::net::TcpListener>,
    proxy_listeners: Vec<(ListenerConfig, std::net::TcpListener)>,
    token: String,
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let paths = match &cli.data_dir {
        Some(dir) => Paths {
            data_dir: dir.clone(),
            config_dir: dir.clone(),
        },
        None => Paths::from_env()?,
    };
    let config_path = cli.config.clone().unwrap_or_else(|| paths.config_file());

    if let Some(Command::Config {
        action: ConfigAction::Init { force },
    }) = &cli.command
    {
        return config_init(&config_path, *force);
    }

    if let Some(Command::Service {
        action,
        exe,
        dry_run,
    }) = &cli.command
    {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME is not set")?;
        let ctx = service::ServiceContext {
            exe: service::resolve_exe(exe.clone())?,
            config_path: absolute(&config_path),
            data_dir: absolute(&paths.data_dir),
            logs_dir: absolute(&paths.logs_dir()),
            home,
        };
        return service::run(*action, &ctx, *dry_run);
    }

    let mut config = Config::load(&config_path)?;
    let overrides = CliOverrides {
        dashboard_listen: cli.dashboard_listen,
        log_level: cli.log_level.clone(),
    };
    overrides.apply(&mut config);
    let mut issues = config.validate();
    issues.extend(dlp::check_engine(&config.dlp, &paths));
    let pricing = match PricingTable::load(config.pricing.as_ref()) {
        Ok(table) => {
            for key in table.missing_frontier() {
                issues.push(gateway_core::config::Issue {
                    level: IssueLevel::Warning,
                    message: format!(
                        "no price for frontier model {key}; its cost column shows (no rate)"
                    ),
                });
            }
            Some(Arc::new(table))
        }
        Err(err) => {
            issues.push(gateway_core::config::Issue {
                level: IssueLevel::Error,
                message: format!("{err:#}"),
            });
            None
        }
    };

    if let Some(Command::Setup {
        action: SetupAction::Check,
    }) = &cli.command
    {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME is not set")?;
        let checks = setup::run_checks(&config, &config_path, &paths, &home);
        if setup::print(&checks) {
            return Ok(());
        }
        bail!("setup check found failures");
    }

    if cli.check_config {
        for issue in &issues {
            let level = match issue.level {
                IssueLevel::Error => "error",
                IssueLevel::Warning => "warning",
            };
            println!("{level}: {}", issue.message);
        }
        if Config::has_errors(&issues) {
            bail!("configuration has errors ({})", config_path.display());
        }
        println!(
            "ok: {} ({} warning(s))",
            config_path.display(),
            issues.len()
        );
        return Ok(());
    }
    if Config::has_errors(&issues) {
        for issue in issues.iter().filter(|i| i.level == IssueLevel::Error) {
            eprintln!("error: {}", issue.message);
        }
        bail!("configuration has errors; see --check-config");
    }
    let pricing = pricing.expect("no errors means the pricing table loaded");

    let token_path = paths.in_data_dir(&config.dashboard.token_file);
    if cli.print_token {
        let (token, _) = web::token::load_or_create(&token_path)?;
        println!("{}", dashboard_url(&config, &token));
        return Ok(());
    }

    std::fs::create_dir_all(&paths.data_dir)
        .with_context(|| format!("create data dir {}", paths.data_dir.display()))?;
    let _log_guard = logging::init(&config.log, &paths.logs_dir(), cli.headless)?;
    for issue in &issues {
        warn!(message = %issue.message, "configuration warning");
    }
    info!(
        version = VERSION,
        platform = %Platform::current(),
        data_dir = %paths.data_dir.display(),
        config = %config_path.display(),
        "starting local-ai-gateway"
    );

    // Bind every socket before building the runtime so a taken port fails fast, as the monitors did.
    let dashboard_listener = if cli.no_dashboard {
        None
    } else {
        Some(web::bind(config.dashboard.listen)?)
    };
    let mut proxy_listeners = Vec::new();
    for (backend, backend_cfg) in config.backends.enabled() {
        for listener in ListenerConfig::from_backend(backend, backend_cfg) {
            let socket = gateway_proxy::bind(listener.listen)?;
            proxy_listeners.push((listener, socket));
        }
    }
    for listener in ListenerConfig::unified(&config.unified, &config.backends) {
        let socket = gateway_proxy::bind(listener.listen)?;
        proxy_listeners.push((listener, socket));
    }
    let (token, source) = web::token::load_or_create(&token_path)?;
    match source {
        TokenSource::Minted => {
            info!(path = %token_path.display(), "minted a new dashboard access token")
        }
        TokenSource::Env => info!("dashboard access token taken from the environment"),
        TokenSource::File => {}
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    let result = runtime.block_on(serve_all(Startup {
        config: Arc::new(config),
        config_path,
        overrides,
        paths,
        pricing,
        dashboard_listener,
        proxy_listeners,
        token,
    }));
    runtime.shutdown_timeout(Duration::from_secs(3));
    result
}

async fn serve_all(startup: Startup) -> Result<()> {
    let Startup {
        config,
        config_path,
        overrides,
        paths,
        pricing,
        dashboard_listener,
        proxy_listeners,
        token,
    } = startup;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let shutdown_tx = Arc::new(shutdown_tx);
    let (events, _keep_bus_open) = events::channel();

    let store = gateway_core::store::open(&paths.db_file())
        .await
        .context("open store")?;
    let session_id = store
        .writer
        .start_session(VERSION)
        .await
        .context("start session")?;
    info!(session_id, db = %paths.db_file().display(), "session started");

    let backends = Arc::new(BackendsState::new());
    let proxy_ctx = ProxyContext::new(
        Some(store.writer.clone()),
        events.clone(),
        session_id,
        pricing.clone(),
        backends.clone(),
        CapturePolicy::from(&config.capture),
    )?;
    let (engine, engine_issues) = dlp::build_engine(&config.dlp, &paths)?;
    for issue in &engine_issues {
        warn!(message = %issue.message, "dlp warning");
    }
    proxy_ctx.set_scanner(Arc::new(dlp::DlpScanner::new(engine.clone())));
    proxy_ctx.set_routing(Arc::new(RoutingTable::from_config(&config.unified)));
    info!(
        enabled = engine.is_enabled(),
        rules = engine.rules().iter().filter(|r| r.enabled).count(),
        known_secrets = engine.known_secret_count(),
        "dlp engine ready"
    );
    let hot = Hot::new(HotParts {
        config: (*config).clone(),
        config_path,
        paths: paths.clone(),
        overrides,
        engine,
        pricing: pricing.clone(),
        proxy: Some(proxy_ctx.clone()),
        events: events.clone(),
    });
    tokio::spawn(signal_task(shutdown_tx.clone(), hot.clone()));
    tokio::spawn(retention_loop(
        store.writer.clone(),
        hot.clone(),
        shutdown_rx.clone(),
    ));
    tokio::spawn(alerts::run(
        events.subscribe(),
        events.clone(),
        store.writer.clone(),
        hot.alert_settings(),
        shutdown_rx.clone(),
    ));
    let mut listener_statuses = Vec::new();
    let mut proxy_tasks: Vec<JoinHandle<Result<()>>> = Vec::new();
    for (listener, socket) in proxy_listeners {
        let fixed = listener.fixed();
        listener_statuses.push(ListenerStatus {
            listen: listener.listen,
            unified: fixed.is_none(),
            backend: fixed.map(|t| t.backend),
            upstream: fixed.map(|t| t.upstream.clone()),
            targets: listener.backends(),
            bound: true,
        });
        proxy_tasks.push(tokio::spawn(gateway_proxy::serve(
            socket,
            Arc::new(listener),
            proxy_ctx.clone(),
            shutdown_rx.clone(),
        )));
    }
    if proxy_tasks.is_empty() {
        warn!("no backend is enabled: the gateway is running without proxy listeners");
    }

    for (backend, backend_cfg) in config.backends.enabled().filter(|(b, _)| b.is_local()) {
        tokio::spawn(gateway_backends::run(
            PollerConfig {
                backend,
                upstream: backend_cfg.upstream.clone(),
                interval: Duration::from_secs(backend_cfg.poll_interval_secs),
                lms_app_path: backend_cfg.app_path.clone(),
                lms_bin: backend_cfg.lms_bin.clone(),
            },
            backends.clone(),
            events.clone(),
            Some(store.writer.clone()),
            shutdown_rx.clone(),
        ));
    }

    let hardware: Latest = Arc::default();
    if config.hardware.enabled {
        tokio::spawn(gateway_hardware::run(
            SamplerConfig {
                interval: Duration::from_secs(config.hardware.sample_interval_secs),
                gpu_source: config.hardware.gpu_source,
                powermetrics: config.hardware.powermetrics,
            },
            events.clone(),
            Some(store.writer.clone()),
            hardware.clone(),
            shutdown_rx.clone(),
        ));
    }

    let aggregator = Arc::new(Mutex::new(Aggregator::new(Utc::now())));
    tokio::spawn(feed::run(
        events.subscribe(),
        aggregator.clone(),
        events.clone(),
        shutdown_rx.clone(),
    ));

    let mut dashboard: Option<JoinHandle<Result<()>>> = None;
    if let Some(listener) = dashboard_listener {
        let listen = config.dashboard.listen;
        let tls_on = config.dashboard.tls.enabled_for(listen);
        let tls = if tls_on {
            Some(web::tls::load_or_create(&paths.tls_dir(), listen.ip()).await?)
        } else {
            None
        };
        let state = AppState::new(AppStateParts {
            hot: hot.clone(),
            token: token.clone(),
            secure_cookies: tls_on,
            events: events.clone(),
            store: Some(store.clone()),
            backends: backends.clone(),
            hardware: hardware.clone(),
            aggregator: aggregator.clone(),
            proxy: Some(proxy_ctx.clone()),
            listeners: listener_statuses,
            session_id: Some(session_id),
        });
        let app = web::router(state);
        // The URL carries the token, so it goes to the terminal once and never into the log.
        println!("dashboard: {}", dashboard_url(&config, &token));
        info!(%listen, tls = tls_on, auth = config.dashboard.auth, "dashboard listening");
        dashboard = Some(tokio::spawn(web::serve(
            listener,
            tls,
            app,
            shutdown_rx.clone(),
            SHUTDOWN_GRACE,
        )));
    } else {
        info!("dashboard disabled (--no-dashboard)");
    }

    // A proxy listener that dies before shutdown is fatal: clients would be left without a gateway.
    let watcher = {
        let shutdown_tx = shutdown_tx.clone();
        let mut shutdown = shutdown_rx.clone();
        let tasks = std::mem::take(&mut proxy_tasks);
        tokio::spawn(async move {
            if tasks.is_empty() {
                let _ = shutdown.changed().await;
                return Vec::new();
            }
            let mut remaining = tasks;
            loop {
                tokio::select! {
                    _ = shutdown.changed() => return remaining,
                    (outcome, _index, rest) = futures_util::future::select_all(remaining.drain(..)) => {
                        remaining = rest;
                        if !*shutdown.borrow() {
                            report_task("proxy listener", outcome);
                            error!("a proxy listener stopped unexpectedly; shutting down");
                            let _ = shutdown_tx.send(true);
                            return remaining;
                        }
                        if remaining.is_empty() {
                            return remaining;
                        }
                    }
                }
            }
        })
    };

    let mut shutdown = shutdown_rx.clone();
    tokio::select! {
        _ = shutdown.changed() => info!("shutting down"),
        outcome = wait_for(&mut dashboard) => {
            report_task("dashboard", outcome);
            let _ = shutdown_tx.send(true);
            bail!("the dashboard server stopped unexpectedly");
        }
    }

    publish(&events, GatewayEvent::Shutdown);
    let remaining = watcher.await.unwrap_or_default();
    if tokio::time::timeout(
        SHUTDOWN_GRACE + Duration::from_secs(1),
        futures_util::future::join_all(remaining),
    )
    .await
    .is_err()
    {
        warn!("proxy listeners did not stop within the grace period; open streams are cut");
    }
    proxy_ctx.tunnels.close();
    if tokio::time::timeout(SHUTDOWN_GRACE, proxy_ctx.tunnels.wait())
        .await
        .is_err()
    {
        warn!("open WebSocket tunnels did not close within the grace period; cutting them");
    }
    if let Some(task) = dashboard {
        match tokio::time::timeout(SHUTDOWN_GRACE + Duration::from_secs(1), task).await {
            Ok(outcome) => report_task("dashboard", outcome),
            Err(_) => warn!("dashboard did not stop within the grace period"),
        }
    }
    close_store(&store, session_id).await;
    info!("stopped");
    Ok(())
}

async fn close_store(store: &Store, session_id: i64) {
    if let Err(err) = store.writer.end_session(session_id).await {
        warn!(error = %format!("{err:#}"), "could not close the session row");
    }
    if let Err(err) = tokio::time::timeout(Duration::from_secs(5), store.writer.checkpoint()).await
    {
        warn!(error = %err, "store checkpoint did not finish in time");
    }
}

/// Retention shortly after start, then on an interval, until shutdown. The policy is read each run, so a
/// reload takes effect at the next one.
async fn retention_loop(writer: StoreHandle, hot: Arc<Hot>, mut shutdown: watch::Receiver<bool>) {
    let mut next = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        tokio::select! {
            _ = shutdown.changed() => return,
            _ = tokio::time::sleep_until(next) => {}
        }
        match writer.run_retention(hot.retention(), Utc::now()).await {
            Ok(report) => {
                let total = report.bodies_deleted
                    + report.passthrough_records_deleted
                    + report.hardware_samples_deleted
                    + report.alerts_deleted;
                if total > 0 {
                    info!(?report, "retention ran");
                }
            }
            Err(err) => warn!(error = %format!("{err:#}"), "retention failed"),
        }
        next = tokio::time::Instant::now() + RETENTION_EVERY;
    }
}

/// Resolves when the task finishes; pends forever when there is no task.
async fn wait_for(
    task: &mut Option<JoinHandle<Result<()>>>,
) -> Result<Result<()>, tokio::task::JoinError> {
    match task {
        Some(handle) => handle.await,
        None => std::future::pending().await,
    }
}

fn report_task(name: &str, outcome: Result<Result<()>, tokio::task::JoinError>) {
    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(err)) => error!(task = name, error = %format!("{err:#}"), "task failed"),
        Err(join) => error!(task = name, error = %join, "task panicked"),
    }
}

/// SIGINT and SIGTERM stop the gateway. SIGHUP reloads `config.toml` and the known-secrets file; it is
/// never treated as "terminal gone" the way the TUIs did.
async fn signal_task(shutdown: Arc<watch::Sender<bool>>, hot: Arc<Hot>) {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut int), Ok(mut hup)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
        signal(SignalKind::hangup()),
    ) else {
        error!("could not install signal handlers; stop the process with SIGKILL if needed");
        return;
    };
    loop {
        tokio::select! {
            _ = term.recv() => { info!("SIGTERM received"); break; }
            _ = int.recv() => { info!("SIGINT received"); break; }
            _ = hup.recv() => match hot.reload_from_disk() {
                Ok(report) => info!(
                    applied = ?report.applied,
                    restart_required = ?report.restart_required,
                    warnings = report.warnings.len(),
                    "SIGHUP: configuration reloaded"
                ),
                Err(err) => warn!(error = %format!("{err:#}"), "SIGHUP: configuration not reloaded"),
            },
        }
    }
    let _ = shutdown.send(true);
}

/// Service units must not depend on the working directory.
fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

fn config_init(path: &Path, force: bool) -> Result<()> {
    if path.exists() && !force {
        bail!(
            "{} already exists; pass --force to overwrite it",
            path.display()
        );
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    std::fs::write(path, SAMPLE).with_context(|| format!("write {}", path.display()))?;
    println!("wrote {}", path.display());
    Ok(())
}

/// The URL a browser should open, with the token when auth is on. An unspecified bind address is shown as
/// loopback, since that is what works from the same machine.
fn dashboard_url(config: &Config, token: &str) -> String {
    let listen = config.dashboard.listen;
    let scheme = if config.dashboard.tls.enabled_for(listen) {
        "https"
    } else {
        "http"
    };
    let mut url = format!(
        "{scheme}://{}:{}/",
        display_host(listen.ip()),
        listen.port()
    );
    if config.dashboard.auth {
        url.push_str("?token=");
        url.push_str(token);
    }
    url
}

fn display_host(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) if v4.is_unspecified() => Ipv4Addr::LOCALHOST.to_string(),
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) if v6.is_unspecified() => format!("[{}]", Ipv6Addr::LOCALHOST),
        IpAddr::V6(v6) => format!("[{v6}]"),
    }
}

#[allow(
    dead_code,
    reason = "the event sender type is re-exported for the API modules' signatures"
)]
type Events = EventSender;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dashboard_url_follows_tls_auth_and_bind_address() {
        let mut config = Config::default();
        assert_eq!(
            dashboard_url(&config, "t0ken"),
            "http://127.0.0.1:8787/?token=t0ken"
        );
        config.dashboard.listen = "0.0.0.0:9000".parse().unwrap();
        assert_eq!(
            dashboard_url(&config, "t0ken"),
            "https://127.0.0.1:9000/?token=t0ken"
        );
        config.dashboard.listen = "[::]:9000".parse().unwrap();
        assert_eq!(
            dashboard_url(&config, "t0ken"),
            "https://[::1]:9000/?token=t0ken"
        );
        config.dashboard.listen = "192.168.1.5:9000".parse().unwrap();
        config.dashboard.auth = false;
        assert_eq!(dashboard_url(&config, "t0ken"), "https://192.168.1.5:9000/");
    }

    #[test]
    fn config_init_refuses_to_overwrite_without_force() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("config.toml");
        config_init(&path, false).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), SAMPLE);
        assert!(config_init(&path, false).is_err());
        config_init(&path, true).unwrap();
    }
}
