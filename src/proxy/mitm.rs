use std::collections::HashMap;
use std::io::{BufReader, Cursor};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair,
    KeyUsagePurpose, SanType,
};
use rustls::RootCertStore;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use tokio::fs;
use tokio_rustls::{TlsAcceptor, TlsConnector};
use tracing::info;

pub const WORKSPACE_CA_RELATIVE_PATH: &str = ".pi/proxy-ca.pem";
pub const PRIVILEGED_CA_PATH: &str = "/home/agent/.pi/proxy-ca.pem";

pub struct MitmConfig {
    pub authority: MitmAuthority,
}

pub struct MitmAuthority {
    ca: Certificate,
    ca_cert_pem: String,
    leaf_cache: RwLock<HashMap<String, LeafCertificate>>,
}

#[derive(Clone)]
pub struct LeafCertificate {
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
}

impl MitmConfig {
    pub async fn load_or_create(ca_cert_path: PathBuf, ca_key_path: PathBuf) -> Result<Self> {
        let authority = MitmAuthority::load_or_create(&ca_cert_path, &ca_key_path).await?;
        Ok(Self { authority })
    }
}

impl From<MitmConfig> for MitmAuthority {
    fn from(config: MitmConfig) -> Self {
        config.authority
    }
}

impl MitmAuthority {
    async fn load_or_create(ca_cert_path: &Path, ca_key_path: &Path) -> Result<Self> {
        let cert_exists = fs::try_exists(ca_cert_path).await?;
        let key_exists = fs::try_exists(ca_key_path).await?;
        match (cert_exists, key_exists) {
            (true, true) => Self::load(ca_cert_path, ca_key_path).await,
            (false, false) => Self::create(ca_cert_path, ca_key_path).await,
            _ => bail!(
                "proxy MITM CA cert and key must both exist or both be absent: cert={}, key={}",
                ca_cert_path.display(),
                ca_key_path.display()
            ),
        }
    }

    async fn load(ca_cert_path: &Path, ca_key_path: &Path) -> Result<Self> {
        validate_private_file_permissions(ca_key_path).with_context(|| {
            format!(
                "invalid proxy CA key permissions: {}",
                ca_key_path.display()
            )
        })?;
        let ca_cert_pem = fs::read_to_string(ca_cert_path)
            .await
            .with_context(|| format!("read proxy CA certificate: {}", ca_cert_path.display()))?;
        let key_pem = fs::read_to_string(ca_key_path)
            .await
            .with_context(|| format!("read proxy CA key: {}", ca_key_path.display()))?;
        validate_ca_material(&ca_cert_pem, &key_pem)?;
        let key = KeyPair::from_pem(&key_pem).context("parse proxy CA key")?;
        let params = ca_params(key);
        let ca = Certificate::from_params(params).context("load proxy CA certificate")?;
        let authority = Self {
            ca,
            ca_cert_pem,
            leaf_cache: RwLock::new(HashMap::new()),
        };
        authority
            .validate_leaf_trust("agentic-sandbox-ca-validation.invalid")
            .await
            .context("loaded proxy CA is incompatible with generated interception certificates")?;
        info!(ca_cert = %ca_cert_path.display(), ca_key = %ca_key_path.display(), "proxy MITM CA loaded");
        Ok(authority)
    }

    async fn create(ca_cert_path: &Path, ca_key_path: &Path) -> Result<Self> {
        if let Some(parent) = ca_cert_path.parent() {
            fs::create_dir_all(parent).await?;
            set_mode(parent, 0o700).await?;
        }
        if let Some(parent) = ca_key_path.parent() {
            fs::create_dir_all(parent).await?;
            set_mode(parent, 0o700).await?;
        }
        let key = KeyPair::generate(&rcgen::PKCS_ECDSA_P256_SHA256)?;
        let ca = Certificate::from_params(ca_params(key)).context("generate proxy MITM CA")?;
        let ca_cert_pem = ca
            .serialize_pem()
            .context("serialize proxy CA certificate")?;
        let ca_key_pem = ca.serialize_private_key_pem();
        write_private(ca_cert_path, ca_cert_pem.as_bytes(), 0o600).await?;
        write_private(ca_key_path, ca_key_pem.as_bytes(), 0o600).await?;
        info!(ca_cert = %ca_cert_path.display(), ca_key = %ca_key_path.display(), "proxy MITM CA generated");
        Ok(Self {
            ca,
            ca_cert_pem,
            leaf_cache: RwLock::new(HashMap::new()),
        })
    }

    pub fn ca_cert_pem(&self) -> &str {
        &self.ca_cert_pem
    }

    pub fn certificate_for_host(&self, host: &str) -> Result<LeafCertificate> {
        let host = normalize_certificate_host(host)?;
        if let Ok(cache) = self.leaf_cache.read()
            && let Some(cert) = cache.get(&host)
        {
            return Ok(cert.clone());
        }

        let mut params = CertificateParams::new(Vec::new());
        params.not_before = (SystemTime::now() - Duration::from_secs(300)).into();
        params.not_after = (SystemTime::now() + Duration::from_secs(60 * 60 * 24 * 30)).into();
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, host.clone());
        params.subject_alt_names = vec![match host.parse::<IpAddr>() {
            Ok(address) => SanType::IpAddress(address),
            Err(_) => SanType::DnsName(host.clone()),
        }];
        let cert =
            Certificate::from_params(params).context("generate proxy MITM leaf certificate")?;
        let cert_der = cert
            .serialize_der_with_signer(&self.ca)
            .context("sign proxy MITM leaf certificate")?;
        let key_der = cert.serialize_private_key_der();
        let leaf = LeafCertificate { cert_der, key_der };
        self.leaf_cache
            .write()
            .expect("proxy MITM leaf cache lock poisoned")
            .insert(host, leaf.clone());
        Ok(leaf)
    }

    async fn validate_leaf_trust(&self, host: &str) -> Result<()> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let leaf = self.certificate_for_host(host)?;
        let server_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(leaf.cert_der)],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf.key_der)),
            )
            .context("build proxy leaf TLS configuration")?;

        let mut roots = RootCertStore::empty();
        let mut reader = BufReader::new(Cursor::new(self.ca_cert_pem.as_bytes()));
        for certificate in rustls_pemfile::certs(&mut reader) {
            roots
                .add(certificate.context("parse loaded proxy CA certificate")?)
                .context("add loaded proxy CA certificate to trust store")?;
        }
        let client_config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();

        let (client_io, server_io) = tokio::io::duplex(16 * 1024);
        let accept = TlsAcceptor::from(Arc::new(server_config)).accept(server_io);
        let server_name =
            ServerName::try_from(host.to_string()).context("build proxy CA validation hostname")?;
        let connect = TlsConnector::from(Arc::new(client_config)).connect(server_name, client_io);
        let (server, client) = tokio::join!(accept, connect);
        client.context("generated proxy leaf is not trusted by the loaded proxy CA")?;
        server.context("proxy CA validation server handshake failed")?;
        Ok(())
    }
}

fn normalize_certificate_host(host: &str) -> Result<String> {
    let host = match host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
    {
        Some(host) => host,
        None => host,
    };
    if host.is_empty() {
        bail!("proxy MITM certificate host must not be empty");
    }
    Ok(host.to_ascii_lowercase())
}

fn validate_ca_material(ca_cert_pem: &str, key_pem: &str) -> Result<()> {
    let mut cert_reader = BufReader::new(Cursor::new(ca_cert_pem.as_bytes()));
    let certs = rustls_pemfile::certs(&mut cert_reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("parse proxy CA certificate")?;
    if certs.is_empty() {
        bail!("proxy CA certificate file contains no certificates");
    }

    let mut key_reader = BufReader::new(Cursor::new(key_pem.as_bytes()));
    let key = rustls_pemfile::private_key(&mut key_reader)
        .context("parse proxy CA key")?
        .ok_or_else(|| anyhow::anyhow!("proxy CA key file contains no private key"))?;
    rustls::sign::CertifiedKey::from_der(certs, key, &rustls::crypto::ring::default_provider())
        .context("proxy CA certificate and key do not match")?;
    Ok(())
}

fn ca_params(key_pair: KeyPair) -> CertificateParams {
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::CrlSign,
    ];
    params.not_before = (SystemTime::now() - Duration::from_secs(300)).into();
    params.not_after = (SystemTime::now() + Duration::from_secs(60 * 60 * 24 * 365 * 5)).into();
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, "agentic-sandbox proxy MITM CA");
    params.key_pair = Some(key_pair);
    params
}

async fn write_private(path: &Path, content: &[u8], mode: u32) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(mode);
    let mut file = options.open(path).await?;
    file.write_all(content).await?;
    file.flush().await?;
    set_mode(path, mode).await?;
    Ok(())
}

async fn set_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).await?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

#[cfg(unix)]
fn validate_private_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let metadata = std::fs::metadata(path)?;
    let current_uid = unsafe { libc::geteuid() };
    if metadata.uid() != 0 && metadata.uid() != current_uid {
        bail!("owner uid must be root or current master uid");
    }
    if metadata.mode() & 0o077 != 0 {
        bail!("mode must not allow group or world access; use 0600");
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_private_file_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn assert_certificate_valid_for(authority: &MitmAuthority, host: &str) {
        authority.validate_leaf_trust(host).await.unwrap();
    }

    #[tokio::test]
    async fn creates_ca_and_leaf_certificate() {
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("proxy-ca.pem");
        let key_path = dir.path().join("proxy-ca-key.pem");

        let config = MitmConfig::load_or_create(cert_path.clone(), key_path.clone())
            .await
            .unwrap();
        assert!(config.authority.ca_cert_pem().contains("BEGIN CERTIFICATE"));
        let leaf = config
            .authority
            .certificate_for_host("Example.COM")
            .unwrap();
        assert!(!leaf.cert_der.is_empty());
        assert!(!leaf.key_der.is_empty());
        assert!(cert_path.exists());
        assert!(key_path.exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            assert_eq!(
                std::fs::metadata(&cert_path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(&key_path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[tokio::test]
    async fn generated_certificates_validate_for_dns_and_ip_hosts() {
        let dir = tempfile::tempdir().unwrap();
        let config = MitmConfig::load_or_create(
            dir.path().join("proxy-ca.pem"),
            dir.path().join("proxy-ca-key.pem"),
        )
        .await
        .unwrap();

        assert_certificate_valid_for(&config.authority, "example.com").await;
        assert_certificate_valid_for(&config.authority, "127.0.0.1").await;
        assert_certificate_valid_for(&config.authority, "::1").await;
    }

    #[tokio::test]
    async fn loaded_ca_with_an_incompatible_subject_is_rejected_at_startup() {
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("custom-ca.pem");
        let key_path = dir.path().join("custom-ca-key.pem");

        let key = KeyPair::generate(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let mut params = ca_params(key);
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, "custom test signing authority");
        let ca = Certificate::from_params(params).unwrap();
        write_private(&cert_path, ca.serialize_pem().unwrap().as_bytes(), 0o600)
            .await
            .unwrap();
        write_private(&key_path, ca.serialize_private_key_pem().as_bytes(), 0o600)
            .await
            .unwrap();

        let error = MitmConfig::load_or_create(cert_path, key_path)
            .await
            .err()
            .expect("a CA whose subject cannot match generated leaf issuers must be rejected");
        assert!(
            format!("{error:#}").contains("generated proxy leaf is not trusted"),
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn rejects_matching_key_certificate_without_ca_constraints() {
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("not-ca.pem");
        let key_path = dir.path().join("not-ca-key.pem");
        let certificate =
            Certificate::from_params(CertificateParams::new(vec!["not-ca.example".into()]))
                .unwrap();
        write_private(
            &cert_path,
            certificate.serialize_pem().unwrap().as_bytes(),
            0o600,
        )
        .await
        .unwrap();
        write_private(
            &key_path,
            certificate.serialize_private_key_pem().as_bytes(),
            0o600,
        )
        .await
        .unwrap();

        let error = MitmConfig::load_or_create(cert_path, key_path)
            .await
            .err()
            .expect("non-CA material should be rejected");
        assert!(
            format!("{error:#}").contains("generated proxy leaf is not trusted"),
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn certificate_cache_normalizes_case_and_ipv6_brackets() {
        let dir = tempfile::tempdir().unwrap();
        let config = MitmConfig::load_or_create(
            dir.path().join("proxy-ca.pem"),
            dir.path().join("proxy-ca-key.pem"),
        )
        .await
        .unwrap();

        let upper = config
            .authority
            .certificate_for_host("Example.COM")
            .unwrap();
        let lower = config
            .authority
            .certificate_for_host("example.com")
            .unwrap();
        assert_eq!(upper.cert_der, lower.cert_der);
        assert_eq!(upper.key_der, lower.key_der);

        let bracketed = config.authority.certificate_for_host("[::1]").unwrap();
        let plain = config.authority.certificate_for_host("::1").unwrap();
        assert_eq!(bracketed.cert_der, plain.cert_der);
        assert!(config.authority.certificate_for_host("").is_err());
    }

    #[tokio::test]
    async fn rejects_partial_or_malformed_ca_material() {
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("proxy-ca.pem");
        let key_path = dir.path().join("proxy-ca-key.pem");
        write_private(&cert_path, b"not a certificate", 0o600)
            .await
            .unwrap();

        assert!(
            MitmConfig::load_or_create(cert_path.clone(), key_path.clone())
                .await
                .is_err()
        );

        let generated_dir = tempfile::tempdir().unwrap();
        let generated_cert = generated_dir.path().join("proxy-ca.pem");
        let generated_key = generated_dir.path().join("proxy-ca-key.pem");
        MitmConfig::load_or_create(generated_cert.clone(), generated_key.clone())
            .await
            .unwrap();
        std::fs::write(&generated_cert, "not a certificate").unwrap();
        assert!(
            MitmConfig::load_or_create(generated_cert, generated_key)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn rejects_mismatched_ca_certificate_and_key() {
        let first = tempfile::tempdir().unwrap();
        let first_cert = first.path().join("proxy-ca.pem");
        let first_key = first.path().join("proxy-ca-key.pem");
        MitmConfig::load_or_create(first_cert.clone(), first_key.clone())
            .await
            .unwrap();

        let second = tempfile::tempdir().unwrap();
        let second_cert = second.path().join("proxy-ca.pem");
        let second_key = second.path().join("proxy-ca-key.pem");
        MitmConfig::load_or_create(second_cert, second_key.clone())
            .await
            .unwrap();

        std::fs::copy(second_key, &first_key).unwrap();
        assert!(
            MitmConfig::load_or_create(first_cert, first_key)
                .await
                .is_err()
        );
    }
}
