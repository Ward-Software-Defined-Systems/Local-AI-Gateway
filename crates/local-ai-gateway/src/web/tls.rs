//! HTTPS for the dashboard and the proxy listeners, with the certificate the gateway's own certificate
//! authority issues (`gateway_core::tls`). `prepare` makes sure the CA exists, reissues the server
//! certificate for the names the listeners answer to, writes both into `<data_dir>/tls/` and builds the
//! rustls configuration every TLS listener shares. `authority` alone serves the `tls` subcommands and the
//! CA download, so the CA exists from the first start whether or not a listener uses it yet.

use std::path::Path;

use anyhow::{Context, Result};
use axum_server::tls_rustls::RustlsConfig;
use chrono::NaiveDate;
use gateway_core::tls::{Authority, Provenance, SanName};
use serde::Serialize;
use tracing::info;

/// rustls refuses to guess between the providers that reqwest and axum-server bring in; ring is the one
/// this binary uses everywhere. Safe to call more than once.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// The CA in `dir`, minted on first use.
pub fn authority(dir: &Path) -> Result<Authority> {
    let (ca, how) = Authority::load_or_create(dir)?;
    if how == Provenance::Created {
        info!(
            path = %ca.cert_path().display(),
            fingerprint = %ca.fingerprint_sha256(),
            "minted the gateway certificate authority; import ca.pem where clients must trust the gateway"
        );
    }
    Ok(ca)
}

/// What the dashboard and `setup check` show about TLS.
#[derive(Debug, Clone, Serialize)]
pub struct TlsSummary {
    pub ca_file: String,
    pub ca_fingerprint_sha256: String,
    /// The names on the server certificate; empty when no listener serves HTTPS, so none was issued.
    pub names: Vec<String>,
    pub certificate_not_after: Option<NaiveDate>,
}

impl TlsSummary {
    /// The CA alone: no listener serves HTTPS, so no server certificate was issued.
    pub fn ca_only(ca: &Authority) -> Self {
        Self {
            ca_file: ca.cert_path().display().to_string(),
            ca_fingerprint_sha256: ca.fingerprint_sha256(),
            names: Vec::new(),
            certificate_not_after: None,
        }
    }
}

pub struct Prepared {
    pub config: RustlsConfig,
    pub summary: TlsSummary,
}

/// Reissues the server certificate for `names` under the CA in `dir`, writes the pair into `dir` and loads
/// it. Every start gets a fresh certificate: names added to the configuration are on it after a restart
/// and nothing is renewed by hand. Clients trust the CA, never the certificate.
pub async fn prepare(dir: &Path, names: &[SanName]) -> Result<Prepared> {
    install_crypto_provider();
    let ca = authority(dir)?;
    let issued = ca.issue_server(names)?;
    issued.write_to(dir)?;
    let names: Vec<String> = issued.names.iter().map(ToString::to_string).collect();
    info!(
        names = %names.join(", "),
        not_after = %issued.not_after,
        "issued the server certificate under the gateway CA"
    );
    let config = RustlsConfig::from_pem(issued.cert_pem.into_bytes(), issued.key_pem.into_bytes())
        .await
        .context("load the server certificate")?;
    Ok(Prepared {
        config,
        summary: TlsSummary {
            ca_file: ca.cert_path().display().to_string(),
            ca_fingerprint_sha256: ca.fingerprint_sha256(),
            names,
            certificate_not_after: Some(issued.not_after),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_ca_is_minted_once_and_the_certificate_reissued_each_start() {
        let dir = tempfile::tempdir().unwrap();
        let tls_dir = dir.path().join("tls");
        let names = [
            SanName::Dns("localhost".into()),
            SanName::Ip("127.0.0.1".parse().unwrap()),
        ];
        let first = prepare(&tls_dir, &names).await.unwrap();
        assert_eq!(first.summary.names, vec!["localhost", "127.0.0.1"]);
        assert!(first.summary.certificate_not_after.is_some());
        assert_eq!(
            first.summary.ca_file,
            tls_dir.join("ca.pem").display().to_string()
        );
        let ca_pem = std::fs::read_to_string(tls_dir.join("ca.pem")).unwrap();
        let cert_pem = std::fs::read_to_string(tls_dir.join("cert.pem")).unwrap();
        assert!(
            cert_pem.starts_with("-----BEGIN CERTIFICATE-----"),
            "{cert_pem}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for key in ["key.pem", "ca-key.pem"] {
                let mode = std::fs::metadata(tls_dir.join(key))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777;
                assert_eq!(mode, 0o600, "{key} mode {mode:o}");
            }
        }
        let second = prepare(&tls_dir, &names).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(tls_dir.join("ca.pem")).unwrap(),
            ca_pem,
            "the CA is kept"
        );
        assert_ne!(
            std::fs::read_to_string(tls_dir.join("cert.pem")).unwrap(),
            cert_pem,
            "the server certificate is reissued"
        );
        assert_eq!(
            second.summary.ca_fingerprint_sha256,
            first.summary.ca_fingerprint_sha256
        );
        let ca = authority(&tls_dir).unwrap();
        let only = TlsSummary::ca_only(&ca);
        assert!(only.names.is_empty());
        assert_eq!(
            only.ca_fingerprint_sha256,
            first.summary.ca_fingerprint_sha256
        );
    }
}
