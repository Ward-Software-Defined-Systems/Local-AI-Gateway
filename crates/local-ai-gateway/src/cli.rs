//! The command line. Flags override `config.toml`, which overrides the defaults; `LAIG_*` environment
//! variables stand in for the flags (clap's `env`), which is how the launchd plist and the systemd unit
//! configure the service.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "local-ai-gateway",
    version,
    about = "WSDS Local AI Gateway: a monitoring, inspection and DLP reverse proxy for LM Studio and Ollama with a web dashboard"
)]
pub struct Cli {
    /// Path to config.toml (default: <data dir>/config.toml; a missing file means all defaults)
    #[arg(long, env = "LAIG_CONFIG", value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Directory for the database, token, certificates, logs and (unless --config is given) config.toml.
    /// Defaults to the platform data directory.
    #[arg(long, env = "LAIG_DATA_DIR", value_name = "DIR")]
    pub data_dir: Option<PathBuf>,

    /// Dashboard listen address, overriding dashboard.listen
    #[arg(long, env = "LAIG_DASHBOARD_LISTEN", value_name = "IP:PORT")]
    pub dashboard_listen: Option<SocketAddr>,

    /// Run without the web dashboard
    #[arg(long)]
    pub no_dashboard: bool,

    /// Also log to stderr (the log file is always written)
    #[arg(long)]
    pub headless: bool,

    /// Log filter in tracing syntax, overriding log.level (e.g. "info,gateway_proxy=debug")
    #[arg(long, env = "LAIG_LOG", value_name = "FILTER")]
    pub log_level: Option<String>,

    /// Print the dashboard URL with its access token and exit
    #[arg(long)]
    pub print_token: bool,

    /// Validate the configuration, print every issue and exit (status 0 when there are no errors)
    #[arg(long)]
    pub check_config: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Manage config.toml
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Install or control the background service (launchd agent on macOS, systemd user unit on Linux)
    Service {
        #[command(subcommand)]
        action: ServiceAction,
        /// The binary the service runs (default: this executable; copy it somewhere stable first)
        #[arg(long, value_name = "PATH", global = true)]
        exe: Option<PathBuf>,
        /// Print what would be written or run without doing it
        #[arg(long, global = true)]
        dry_run: bool,
    },
    /// Check an installation: configuration, file modes, backends, listeners, tools, service
    Setup {
        #[command(subcommand)]
        action: SetupAction,
    },
}

#[derive(Debug, Clone, Copy, Subcommand, PartialEq, Eq)]
pub enum ServiceAction {
    /// Write the unit and start the service at login
    Install,
    /// Stop the service and remove the unit
    Uninstall,
    /// Show whether the service is loaded and running
    Status,
    Start,
    Stop,
    Restart,
}

#[derive(Debug, Clone, Copy, Subcommand, PartialEq, Eq)]
pub enum SetupAction {
    /// Run every check and exit non-zero on a failure
    Check,
}

#[derive(Debug, Subcommand)]
pub enum ConfigAction {
    /// Write a commented config.toml holding every default (refuses to overwrite unless --force)
    Init {
        #[arg(long)]
        force: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_parse_and_subcommands_nest() {
        let cli = Cli::try_parse_from(["laig", "--headless", "--dashboard-listen", "127.0.0.1:1"])
            .unwrap();
        assert!(cli.headless);
        assert_eq!(cli.dashboard_listen, Some("127.0.0.1:1".parse().unwrap()));
        let cli = Cli::try_parse_from(["laig", "config", "init", "--force"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Config {
                action: ConfigAction::Init { force: true }
            })
        ));
        assert!(Cli::try_parse_from(["laig", "--dashboard-listen", "nope"]).is_err());
        let cli = Cli::try_parse_from(["laig", "service", "install", "--dry-run", "--exe", "/x"])
            .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Service {
                action: ServiceAction::Install,
                dry_run: true,
                ..
            })
        ));
        let cli = Cli::try_parse_from(["laig", "setup", "check"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Setup {
                action: SetupAction::Check
            })
        ));
    }
}
