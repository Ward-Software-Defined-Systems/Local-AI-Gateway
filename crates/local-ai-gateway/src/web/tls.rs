//! Optional HTTPS for the dashboard. `tls = "auto"` turns it on for non-loopback binds, where the token
//! and captured prompts would otherwise cross the network in the clear (the headless Linux server case),
//! and leaves loopback on plain HTTP so the single-machine case shows no certificate warning. The
//! certificate is self-signed (rcgen), minted once into `<data_dir>/tls/` with SANs for localhost, the
//! loopback addresses and the bind address, and served through axum-server's rustls acceptor.

use std::net::IpAddr;
use std::path::Path;

use anyhow::{Context, Result};
use axum_server::tls_rustls::RustlsConfig;
use tracing::info;

use super::token::write_private;

/// rustls refuses to guess between the providers that reqwest and axum-server bring in; ring is the one
/// this binary uses everywhere. Safe to call more than once.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Loads `cert.pem` and `key.pem` from `dir`, minting them first when either is missing.
pub async fn load_or_create(dir: &Path, bind: IpAddr) -> Result<RustlsConfig> {
    install_crypto_provider();
    let cert = dir.join("cert.pem");
    let key = dir.join("key.pem");
    if !(cert.is_file() && key.is_file()) {
        mint(dir, bind)?;
        info!(path = %dir.display(), "minted a self-signed TLS certificate for the dashboard");
    }
    RustlsConfig::from_pem_file(&cert, &key)
        .await
        .with_context(|| format!("load TLS certificate from {}", dir.display()))
}

/// Writes a fresh self-signed certificate and key (mode 0600) into `dir`.
pub fn mint(dir: &Path, bind: IpAddr) -> Result<()> {
    let mut names = vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ];
    if !bind.is_unspecified() && !bind.is_loopback() {
        names.push(bind.to_string());
    }
    let mut params = rcgen::CertificateParams::new(names).context("certificate parameters")?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "Local AI Gateway");
    params.not_before = rcgen::date_time_ymd(2026, 1, 1);
    params.not_after = rcgen::date_time_ymd(2036, 1, 1);
    let key_pair = rcgen::KeyPair::generate().context("generate TLS key pair")?;
    let cert = params
        .self_signed(&key_pair)
        .context("self-sign TLS certificate")?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    write_private(&dir.join("key.pem"), key_pair.serialize_pem().as_bytes())?;
    write_private(&dir.join("cert.pem"), cert.pem().as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_certificate_is_minted_once_and_then_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let tls_dir = dir.path().join("tls");
        load_or_create(&tls_dir, "192.168.1.10".parse().unwrap())
            .await
            .unwrap();
        let cert = std::fs::read_to_string(tls_dir.join("cert.pem")).unwrap();
        assert!(cert.starts_with("-----BEGIN CERTIFICATE-----"), "{cert}");
        let key = std::fs::read_to_string(tls_dir.join("key.pem")).unwrap();
        assert!(key.contains("PRIVATE KEY"), "{key}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(tls_dir.join("key.pem"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "mode {mode:o}");
        }
        // A second load reuses the files instead of minting again.
        load_or_create(&tls_dir, "192.168.1.10".parse().unwrap())
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(tls_dir.join("cert.pem")).unwrap(),
            cert
        );
    }
}
