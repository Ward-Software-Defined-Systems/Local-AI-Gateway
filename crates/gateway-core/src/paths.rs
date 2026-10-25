//! Where the gateway keeps its files.
//!
//! macOS puts everything under `~/Library/Application Support/local-ai-gateway/`. Linux follows XDG: data
//! under `$XDG_DATA_HOME/local-ai-gateway` (default `~/.local/share/...`) and `config.toml` under
//! `$XDG_CONFIG_HOME/local-ai-gateway` (default `~/.config/...`), honouring the variables only when they
//! are absolute paths, as the spec requires. `--data-dir` moves everything, config included, so a scratch
//! run touches nothing of the real installation. Ported from Ollama-Monitor's `config.rs` path logic.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::types::Platform;

/// The directory name used on both platforms.
pub const APP_DIR: &str = "local-ai-gateway";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// Database, token, certificates, logs and the known-secrets file.
    pub data_dir: PathBuf,
    /// Where `config.toml` is looked up by default.
    pub config_dir: PathBuf,
}

impl Paths {
    /// The platform defaults for this process. Fails only when `HOME` is unset.
    pub fn from_env() -> Result<Paths> {
        let home = std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
            .context("HOME is not set; pass --data-dir and --config explicitly")?;
        let xdg_data = std::env::var_os("XDG_DATA_HOME");
        let xdg_config = std::env::var_os("XDG_CONFIG_HOME");
        Ok(Self::resolve(
            Platform::current(),
            &home,
            xdg_data.as_deref(),
            xdg_config.as_deref(),
        ))
    }

    /// Pure resolution, so both platform arms are testable anywhere.
    pub fn resolve(
        platform: Platform,
        home: &Path,
        xdg_data: Option<&OsStr>,
        xdg_config: Option<&OsStr>,
    ) -> Paths {
        match platform {
            Platform::Macos => {
                let dir = home
                    .join("Library")
                    .join("Application Support")
                    .join(APP_DIR);
                Paths {
                    data_dir: dir.clone(),
                    config_dir: dir,
                }
            }
            Platform::Linux | Platform::Other => Paths {
                data_dir: xdg_dir(xdg_data, home, &[".local", "share"]).join(APP_DIR),
                config_dir: xdg_dir(xdg_config, home, &[".config"]).join(APP_DIR),
            },
        }
    }

    /// `--data-dir`: everything, the default config location included, moves there.
    pub fn with_data_dir(self, dir: PathBuf) -> Paths {
        Paths {
            data_dir: dir.clone(),
            config_dir: dir,
        }
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub fn db_file(&self) -> PathBuf {
        self.data_dir.join("gateway.db")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.data_dir.join("logs")
    }

    pub fn tls_dir(&self) -> PathBuf {
        self.data_dir.join("tls")
    }

    /// A config value naming a file: absolute paths are kept, relative ones live in the data dir.
    pub fn in_data_dir(&self, file: &Path) -> PathBuf {
        if file.is_absolute() {
            file.to_path_buf()
        } else {
            self.data_dir.join(file)
        }
    }
}

/// An XDG variable when it is set to an absolute path, otherwise `home/<default parts>`.
fn xdg_dir(var: Option<&OsStr>, home: &Path, default_parts: &[&str]) -> PathBuf {
    match var {
        Some(v) if !v.is_empty() && Path::new(v).is_absolute() => PathBuf::from(v),
        _ => default_parts
            .iter()
            .fold(home.to_path_buf(), |path, part| path.join(part)),
    }
}

/// Writes a file readable by its owner only (mode 0600 on Unix), replacing any existing content. For
/// the access token and private keys.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    std::io::Write::write_all(&mut file, bytes)
        .with_context(|| format!("write {}", path.display()))?;
    #[cfg(unix)]
    {
        // An existing file keeps its old mode on open; tighten it too.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_keeps_data_and_config_in_application_support() {
        let p = Paths::resolve(Platform::Macos, Path::new("/Users/w"), None, None);
        let expected = PathBuf::from("/Users/w/Library/Application Support/local-ai-gateway");
        assert_eq!(p.data_dir, expected);
        assert_eq!(p.config_dir, expected);
        assert_eq!(p.config_file(), expected.join("config.toml"));
        assert_eq!(p.db_file(), expected.join("gateway.db"));
    }

    #[test]
    fn linux_follows_xdg_defaults_when_the_variables_are_unset() {
        let p = Paths::resolve(Platform::Linux, Path::new("/home/w"), None, None);
        assert_eq!(
            p.data_dir,
            PathBuf::from("/home/w/.local/share/local-ai-gateway")
        );
        assert_eq!(
            p.config_dir,
            PathBuf::from("/home/w/.config/local-ai-gateway")
        );
    }

    #[test]
    fn linux_honours_absolute_xdg_variables_and_ignores_relative_ones() {
        let p = Paths::resolve(
            Platform::Linux,
            Path::new("/home/w"),
            Some(OsStr::new("/srv/data")),
            Some(OsStr::new("relative/config")),
        );
        assert_eq!(p.data_dir, PathBuf::from("/srv/data/local-ai-gateway"));
        assert_eq!(
            p.config_dir,
            PathBuf::from("/home/w/.config/local-ai-gateway")
        );
    }

    #[test]
    fn data_dir_override_moves_the_config_too() {
        let p = Paths::resolve(Platform::Linux, Path::new("/home/w"), None, None)
            .with_data_dir(PathBuf::from("/tmp/scratch"));
        assert_eq!(p.config_file(), PathBuf::from("/tmp/scratch/config.toml"));
        assert_eq!(p.logs_dir(), PathBuf::from("/tmp/scratch/logs"));
        assert_eq!(p.tls_dir(), PathBuf::from("/tmp/scratch/tls"));
    }

    #[test]
    fn relative_file_names_live_in_the_data_dir_and_absolute_ones_stay() {
        let p = Paths::resolve(Platform::Macos, Path::new("/Users/w"), None, None);
        assert_eq!(
            p.in_data_dir(Path::new("dashboard.token")),
            PathBuf::from("/Users/w/Library/Application Support/local-ai-gateway/dashboard.token")
        );
        assert_eq!(
            p.in_data_dir(Path::new("/etc/laig/token")),
            PathBuf::from("/etc/laig/token")
        );
    }
}
