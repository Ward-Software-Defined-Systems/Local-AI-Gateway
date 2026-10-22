//! The dashboard access token: 128 bits as 32 lowercase hex characters, minted on first start into
//! `dashboard.token` (mode 0600) or taken from `LAIG_DASHBOARD_TOKEN`. `access.rs` compares it in
//! constant time. It is never written to the log; `--print-token` is the way to see it again.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};

pub const ENV: &str = "LAIG_DASHBOARD_TOKEN";
pub const LEN: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSource {
    Env,
    File,
    Minted,
}

/// The environment wins, then the file, else a new token is minted and written.
pub fn load_or_create(path: &Path) -> Result<(String, TokenSource)> {
    if let Ok(token) = std::env::var(ENV)
        && !token.is_empty()
    {
        ensure!(
            is_valid(&token),
            "{ENV} must be {LEN} lowercase hex characters"
        );
        return Ok((token, TokenSource::Env));
    }
    if path.exists() {
        let token = std::fs::read_to_string(path)
            .with_context(|| format!("read {}", path.display()))?
            .trim()
            .to_string();
        if is_valid(&token) {
            return Ok((token, TokenSource::File));
        }
        bail!(
            "{} does not hold a valid token; delete it to mint a new one",
            path.display()
        );
    }
    let token = generate()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    write_private(path, token.as_bytes())?;
    Ok((token, TokenSource::Minted))
}

/// 16 bytes from `/dev/urandom` as hex. A failed read is an error, never a weaker source.
pub fn generate() -> Result<String> {
    let mut bytes = [0u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .context("read /dev/urandom")?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

pub fn is_valid(token: &str) -> bool {
    token.len() == LEN
        && token
            .bytes()
            .all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
}

/// Writes a file readable by its owner only (mode 0600 on Unix), replacing any existing content.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    file.write_all(bytes)
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
    fn a_generated_token_is_thirty_two_lowercase_hex_characters_and_fresh_each_time() {
        let a = generate().unwrap();
        let b = generate().unwrap();
        assert!(is_valid(&a), "{a}");
        assert!(is_valid(&b), "{b}");
        assert_ne!(a, b);
    }

    #[test]
    fn validation_rejects_other_shapes() {
        assert!(is_valid("0123456789abcdef0123456789abcdef"));
        assert!(!is_valid("0123456789ABCDEF0123456789ABCDEF"), "uppercase");
        assert!(!is_valid("0123456789abcdef0123456789abcde"), "short");
        assert!(!is_valid("0123456789abcdef0123456789abcdefg"), "long");
        assert!(!is_valid(""));
    }

    #[test]
    fn the_token_file_is_minted_once_then_reused_with_owner_only_permissions() {
        // Not read from the environment here: the test must not depend on LAIG_DASHBOARD_TOKEN.
        if std::env::var_os(ENV).is_some_and(|v| !v.is_empty()) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deeper").join("dashboard.token");
        let (first, source) = load_or_create(&path).unwrap();
        assert_eq!(source, TokenSource::Minted);
        let (second, source) = load_or_create(&path).unwrap();
        assert_eq!(source, TokenSource::File);
        assert_eq!(first, second);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "mode {mode:o}");
        }
        std::fs::write(&path, "not a token\n").unwrap();
        assert!(load_or_create(&path).is_err());
    }
}
