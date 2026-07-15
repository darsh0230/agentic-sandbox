use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair,
    KeyUsagePurpose, SanType,
};
use tokio::fs;
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
        let key = KeyPair::from_pem(&key_pem).context("parse proxy CA key")?;
        let params = ca_params(key);
        let ca = Certificate::from_params(params).context("load proxy CA certificate")?;
        info!(ca_cert = %ca_cert_path.display(), ca_key = %ca_key_path.display(), "proxy MITM CA loaded");
        Ok(Self {
            ca,
            ca_cert_pem,
            leaf_cache: RwLock::new(HashMap::new()),
        })
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
        let host = host.to_ascii_lowercase();
        if let Ok(cache) = self.leaf_cache.read()
            && let Some(cert) = cache.get(&host)
        {
            return Ok(cert.clone());
        }

        let mut params = CertificateParams::new(vec![host.clone()]);
        params.not_before = (SystemTime::now() - Duration::from_secs(300)).into();
        params.not_after = (SystemTime::now() + Duration::from_secs(60 * 60 * 24 * 30)).into();
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, host.clone());
        params.subject_alt_names = vec![SanType::DnsName(host.clone())];
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
        .push(DnType::CommonName, "pi-sandbox proxy MITM CA");
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
    }
}
