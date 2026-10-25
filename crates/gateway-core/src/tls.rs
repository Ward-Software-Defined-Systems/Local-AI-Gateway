//! The gateway's own certificate authority and the server certificate it issues. One CA per data
//! directory (`tls/ca.pem` and `tls/ca-key.pem`), made on first use and valid ten years: an operator imports
//! `ca.pem` once into their machines and applications, and every certificate the gateway serves chains to
//! it. The server certificate (`tls/cert.pem` and `tls/key.pem`) is reissued at every start for the names
//! the listeners answer to (`localhost`, the loopback addresses, the host name, the configured `[tls]
//! sans` and the specific addresses of TLS listeners), so a name added to the configuration is on the
//! certificate after a restart and nothing is renewed by hand. Clients trust the CA, never the leaf. The
//! CA's name is fixed because the issuer name on every leaf must match it byte for byte; signing with a
//! reloaded CA rebuilds those parameters around the stored key. Private keys are written with mode 0600.

use std::fmt;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail, ensure};
use chrono::{Datelike, Days, NaiveDate, Utc};
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, Ia5String, IsCa, KeyPair, KeyUsagePurpose, SanType, SerialNumber,
};
use rustls_pki_types::CertificateDer;
use rustls_pki_types::pem::PemObject;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::paths::write_private;

pub const CA_CERT_FILE: &str = "ca.pem";
pub const CA_KEY_FILE: &str = "ca-key.pem";
pub const CERT_FILE: &str = "cert.pem";
pub const KEY_FILE: &str = "key.pem";

const CA_COMMON_NAME: &str = "Local AI Gateway CA";
const SERVER_COMMON_NAME: &str = "Local AI Gateway";
const CA_VALIDITY_DAYS: u64 = 3652;
/// Apple platforms refuse a TLS server certificate valid for more than 825 days, imported CA or not. The
/// leaf is reissued at every start, so the limit only matters to a gateway that runs for years unrestarted.
pub const SERVER_VALIDITY_DAYS: u64 = 820;

/// A name a server certificate can be valid for.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SanName {
    /// A DNS name in lower case, optionally `*.`-prefixed.
    Dns(String),
    Ip(IpAddr),
}

impl SanName {
    /// An IP address, else a DNS name: up to 253 characters in dot-separated labels of 1 to 63 letters,
    /// digits and hyphens that neither start nor end with a hyphen, with an optional leading `*.`.
    pub fn parse(text: &str) -> Result<SanName, String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("a name must not be empty".to_string());
        }
        if let Ok(ip) = text.parse::<IpAddr>() {
            return Ok(SanName::Ip(ip));
        }
        let invalid = || format!("{text:?} is neither an IP address nor a DNS name");
        if text.len() > 253 {
            return Err(invalid());
        }
        let labels = text.strip_prefix("*.").unwrap_or(text);
        if labels.is_empty() {
            return Err(invalid());
        }
        for label in labels.split('.') {
            let shape = !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                && !label.starts_with('-')
                && !label.ends_with('-');
            if !shape {
                return Err(invalid());
            }
        }
        Ok(SanName::Dns(text.to_ascii_lowercase()))
    }

    fn san_type(&self) -> Result<SanType> {
        Ok(match self {
            SanName::Dns(name) => SanType::DnsName(
                Ia5String::try_from(name.as_str()).map_err(|err| anyhow!("{name:?}: {err}"))?,
            ),
            SanName::Ip(ip) => SanType::IpAddress(*ip),
        })
    }
}

impl fmt::Display for SanName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SanName::Dns(name) => f.write_str(name),
            SanName::Ip(ip) => write!(f, "{ip}"),
        }
    }
}

/// Whether `load_or_create` found the CA or made it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    Created,
    Loaded,
}

/// The gateway's certificate authority: the stored certificate and the key that signs with it.
pub struct Authority {
    dir: PathBuf,
    cert_pem: String,
    cert_der: Vec<u8>,
    key: KeyPair,
}

impl Authority {
    /// Loads the CA from `dir`, minting it when neither file exists. One file without the other is an
    /// error rather than a silent replacement: clients may trust the certificate that is there.
    pub fn load_or_create(dir: &Path) -> Result<(Authority, Provenance)> {
        let cert_path = dir.join(CA_CERT_FILE);
        let key_path = dir.join(CA_KEY_FILE);
        match (cert_path.is_file(), key_path.is_file()) {
            (true, true) => {
                let cert_pem = std::fs::read_to_string(&cert_path)
                    .with_context(|| format!("read {}", cert_path.display()))?;
                let cert_der = CertificateDer::from_pem_slice(cert_pem.as_bytes())
                    .map_err(|err| {
                        anyhow!("{} is not a PEM certificate: {err}", cert_path.display())
                    })?
                    .to_vec();
                let key_pem = std::fs::read_to_string(&key_path)
                    .with_context(|| format!("read {}", key_path.display()))?;
                let key = KeyPair::from_pem(&key_pem)
                    .map_err(|err| anyhow!("{} is not a PEM key: {err}", key_path.display()))?;
                Ok((
                    Authority {
                        dir: dir.to_path_buf(),
                        cert_pem,
                        cert_der,
                        key,
                    },
                    Provenance::Loaded,
                ))
            }
            (false, false) => {
                let key = KeyPair::generate().context("generate the CA key")?;
                let mut params = ca_params();
                let today = Utc::now().date_naive();
                let (y, m, d) = ymd(today - Days::new(1));
                params.not_before = rcgen::date_time_ymd(y, m, d);
                let (y, m, d) = ymd(today + Days::new(CA_VALIDITY_DAYS));
                params.not_after = rcgen::date_time_ymd(y, m, d);
                params.serial_number = Some(fresh_serial());
                let cert = params
                    .self_signed(&key)
                    .context("sign the CA certificate")?;
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("create {}", dir.display()))?;
                write_private(&key_path, key.serialize_pem().as_bytes())?;
                std::fs::write(&cert_path, cert.pem())
                    .with_context(|| format!("write {}", cert_path.display()))?;
                Ok((
                    Authority {
                        dir: dir.to_path_buf(),
                        cert_pem: cert.pem(),
                        cert_der: cert.der().to_vec(),
                        key,
                    },
                    Provenance::Created,
                ))
            }
            (cert, _) => {
                let (present, missing) = if cert {
                    (CA_CERT_FILE, CA_KEY_FILE)
                } else {
                    (CA_KEY_FILE, CA_CERT_FILE)
                };
                bail!(
                    "{} exists but {} is missing in {}; move the remaining file away to mint a new CA (clients that trust the old one must import the new ca.pem)",
                    present,
                    missing,
                    dir.display()
                )
            }
        }
    }

    pub fn cert_pem(&self) -> &str {
        &self.cert_pem
    }

    pub fn cert_path(&self) -> PathBuf {
        self.dir.join(CA_CERT_FILE)
    }

    /// SHA-256 of the certificate's DER, as `AB:CD:…`, the form keychains and browsers show.
    pub fn fingerprint_sha256(&self) -> String {
        Sha256::digest(&self.cert_der)
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(":")
    }

    /// A server certificate for `names`, with a fresh key, signed by this CA.
    pub fn issue_server(&self, names: &[SanName]) -> Result<ServerCertificate> {
        ensure!(
            !names.is_empty(),
            "a server certificate needs at least one name"
        );
        // The issuer only lends its name, key identifier and key: the same fixed parameters the stored
        // certificate was made from, so the leaf's issuer field matches it.
        let issuer: Certificate = ca_params()
            .self_signed(&self.key)
            .context("rebuild the CA parameters")?;
        let key = KeyPair::generate().context("generate the server key")?;
        let mut params = CertificateParams::default();
        params.subject_alt_names = names
            .iter()
            .map(SanName::san_type)
            .collect::<Result<Vec<_>>>()?;
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, SERVER_COMMON_NAME);
        params.is_ca = IsCa::ExplicitNoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.use_authority_key_identifier_extension = true;
        let today = Utc::now().date_naive();
        let not_after = today + Days::new(SERVER_VALIDITY_DAYS);
        let (y, m, d) = ymd(today - Days::new(1));
        params.not_before = rcgen::date_time_ymd(y, m, d);
        let (y, m, d) = ymd(not_after);
        params.not_after = rcgen::date_time_ymd(y, m, d);
        params.serial_number = Some(fresh_serial());
        let cert = params
            .signed_by(&key, &issuer, &self.key)
            .context("sign the server certificate")?;
        Ok(ServerCertificate {
            cert_pem: cert.pem(),
            key_pem: key.serialize_pem(),
            names: names.to_vec(),
            not_after,
        })
    }
}

/// The key stays out of debug output; the fingerprint identifies the CA.
impl fmt::Debug for Authority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Authority")
            .field("dir", &self.dir)
            .field("fingerprint_sha256", &self.fingerprint_sha256())
            .finish_non_exhaustive()
    }
}

/// A freshly issued server certificate and its key, both PEM.
#[derive(Debug, Clone)]
pub struct ServerCertificate {
    pub cert_pem: String,
    pub key_pem: String,
    pub names: Vec<SanName>,
    pub not_after: NaiveDate,
}

impl ServerCertificate {
    /// Writes `cert.pem` and `key.pem` (mode 0600) into `dir`, replacing the previous pair.
    pub fn write_to(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        write_private(&dir.join(KEY_FILE), self.key_pem.as_bytes())?;
        std::fs::write(dir.join(CERT_FILE), &self.cert_pem)
            .with_context(|| format!("write {}", dir.join(CERT_FILE).display()))?;
        Ok(())
    }
}

/// The names every server certificate carries: `localhost`, both loopback addresses and the host name
/// (when it is a usable DNS name).
pub fn default_names(host_name: Option<&str>) -> Vec<SanName> {
    let mut names = vec![
        SanName::Dns("localhost".to_string()),
        SanName::Ip(IpAddr::from([127, 0, 0, 1])),
        SanName::Ip(IpAddr::from([0, 0, 0, 0, 0, 0, 0, 1])),
    ];
    if let Some(parsed) = host_name.and_then(|h| SanName::parse(h).ok())
        && !names.contains(&parsed)
    {
        names.push(parsed);
    }
    names
}

/// The full name list for the server certificate: the defaults, the configured extras and the specific
/// (neither unspecified nor loopback) addresses of the TLS listeners, in that order, without repeats.
pub fn server_names(
    host_name: Option<&str>,
    extra: &[SanName],
    listener_ips: impl IntoIterator<Item = IpAddr>,
) -> Vec<SanName> {
    let mut names = default_names(host_name);
    let listener_names = listener_ips
        .into_iter()
        .filter(|ip| !ip.is_unspecified() && !ip.is_loopback())
        .map(SanName::Ip);
    for name in extra.iter().cloned().chain(listener_names) {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

fn ca_params() -> CertificateParams {
    let mut params = CertificateParams::default();
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, CA_COMMON_NAME);
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    params
}

/// The year, month and day `rcgen::date_time_ymd` takes (rcgen speaks the `time` crate's dates without
/// re-exporting it).
fn ymd(date: NaiveDate) -> (i32, u8, u8) {
    (
        date.year(),
        u8::try_from(date.month()).unwrap_or(1),
        u8::try_from(date.day()).unwrap_or(1),
    )
}

/// rcgen derives a default serial from the public key; an explicit unique one keeps every issued
/// certificate distinct under the same issuer, which browsers insist on.
fn fresh_serial() -> SerialNumber {
    let mut bytes = *Uuid::now_v7().as_bytes();
    bytes[0] &= 0x7f;
    SerialNumber::from_slice(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn the_ca_is_minted_once_then_reloaded_and_its_key_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let tls = dir.path().join("tls");
        let (ca, how) = Authority::load_or_create(&tls).unwrap();
        assert_eq!(how, Provenance::Created);
        assert!(ca.cert_pem().starts_with("-----BEGIN CERTIFICATE-----"));
        assert_eq!(ca.cert_path(), tls.join("ca.pem"));
        let fingerprint = ca.fingerprint_sha256();
        assert_eq!(fingerprint.len(), 95, "{fingerprint}");
        assert!(fingerprint.split(':').all(|p| p.len() == 2));
        #[cfg(unix)]
        {
            assert_eq!(mode(&tls.join("ca-key.pem")), 0o600);
            assert_ne!(
                mode(&tls.join("ca.pem")) & 0o044,
                0,
                "the certificate is public"
            );
        }
        let (again, how) = Authority::load_or_create(&tls).unwrap();
        assert_eq!(how, Provenance::Loaded);
        assert_eq!(again.cert_pem(), ca.cert_pem());
        assert_eq!(again.fingerprint_sha256(), fingerprint);
    }

    #[test]
    fn a_lone_ca_file_is_refused_rather_than_replaced() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ca.pem"), "x").unwrap();
        let err = Authority::load_or_create(dir.path()).unwrap_err();
        assert!(
            format!("{err:#}").contains("ca-key.pem is missing"),
            "{err:#}"
        );
    }

    #[test]
    fn a_server_certificate_is_issued_for_the_names_with_a_private_key() {
        let dir = tempfile::tempdir().unwrap();
        let (ca, _) = Authority::load_or_create(dir.path()).unwrap();
        let names = vec![
            SanName::Ip("127.0.0.1".parse().unwrap()),
            SanName::Dns("gateway.test".into()),
        ];
        let issued = ca.issue_server(&names).unwrap();
        assert_eq!(issued.names, names);
        assert_eq!(
            issued.not_after,
            Utc::now().date_naive() + Days::new(SERVER_VALIDITY_DAYS)
        );
        assert!(issued.cert_pem.starts_with("-----BEGIN CERTIFICATE-----"));
        assert!(issued.key_pem.contains("PRIVATE KEY"));
        let out = dir.path().join("served");
        issued.write_to(&out).unwrap();
        assert_eq!(
            std::fs::read_to_string(out.join("cert.pem")).unwrap(),
            issued.cert_pem
        );
        #[cfg(unix)]
        assert_eq!(mode(&out.join("key.pem")), 0o600);
        let second = ca.issue_server(&names).unwrap();
        assert_ne!(
            second.cert_pem, issued.cert_pem,
            "every issue is a fresh certificate"
        );
        assert!(ca.issue_server(&[]).is_err());
    }

    #[test]
    fn names_parse_as_ip_or_dns_and_the_server_list_has_no_repeats() {
        assert_eq!(
            SanName::parse(" Gateway.LAN "),
            Ok(SanName::Dns("gateway.lan".into()))
        );
        assert_eq!(
            SanName::parse("192.168.1.10"),
            Ok(SanName::Ip("192.168.1.10".parse().unwrap()))
        );
        assert_eq!(SanName::parse("*.lan"), Ok(SanName::Dns("*.lan".into())));
        assert_eq!(
            SanName::parse("::1"),
            Ok(SanName::Ip("::1".parse().unwrap()))
        );
        for bad in [
            "",
            "bad_name",
            "-x.com",
            "a..b",
            "x.com/",
            "*.",
            &"a".repeat(64),
        ] {
            assert!(SanName::parse(bad).is_err(), "{bad:?}");
        }
        assert_eq!(SanName::Dns("a.b".into()).to_string(), "a.b");

        let names = server_names(
            Some("Studio.local"),
            &[
                SanName::Dns("localhost".into()),
                SanName::Dns("gateway.lan".into()),
            ],
            [
                "0.0.0.0".parse().unwrap(),
                "127.0.0.1".parse().unwrap(),
                "192.168.1.10".parse().unwrap(),
                "192.168.1.10".parse().unwrap(),
            ],
        );
        assert_eq!(
            names,
            vec![
                SanName::Dns("localhost".into()),
                SanName::Ip("127.0.0.1".parse().unwrap()),
                SanName::Ip("::1".parse().unwrap()),
                SanName::Dns("studio.local".into()),
                SanName::Dns("gateway.lan".into()),
                SanName::Ip("192.168.1.10".parse().unwrap()),
            ]
        );
        assert_eq!(
            default_names(Some("not a host name")).len(),
            3,
            "an unusable host name is left out"
        );
    }
}
