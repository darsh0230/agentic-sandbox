use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::types::HeaderOverrideRule;

use super::{HeaderRuleSource, ResolvedHeaderOverrideRule, headers};

pub const DEFAULT_PROXY_CONFIG_PATH: &str = "/var/lib/agentic-sandbox/state/proxy-headers.json";
pub const DEFAULT_PROXY_CA_CERT_PATH: &str = "/var/lib/agentic-sandbox/state/proxy-ca.pem";
pub const DEFAULT_PROXY_CA_KEY_PATH: &str = "/var/lib/agentic-sandbox/state/proxy-ca-key.pem";

pub struct ProxyConfig {
    pub listen_addr: SocketAddr,
    pub global_rules: Vec<ResolvedHeaderOverrideRule>,
    pub mitm: Option<super::mitm::MitmConfig>,
}

#[derive(Debug, Deserialize)]
struct RawProxyConfig {
    #[serde(default)]
    global_inject_headers: Vec<HeaderOverrideRule>,
}

pub async fn load_global_config(
    path: Option<PathBuf>,
    listen_addr: SocketAddr,
) -> Result<ProxyConfig> {
    let explicit = path.is_some();
    let path = path.unwrap_or_else(|| PathBuf::from(DEFAULT_PROXY_CONFIG_PATH));
    if !path.exists() {
        if explicit {
            bail!("proxy config does not exist: {}", path.display());
        }
        return Ok(ProxyConfig {
            listen_addr,
            global_rules: Vec::new(),
            mitm: None,
        });
    }
    validate_permissions(&path)
        .with_context(|| format!("invalid proxy config permissions: {}", path.display()))?;
    let body = tokio::fs::read(&path)
        .await
        .with_context(|| format!("read proxy config: {}", path.display()))?;
    let mut raw: RawProxyConfig = serde_json::from_slice(&body)
        .with_context(|| format!("parse proxy config: {}", path.display()))?;
    resolve_env_values(&mut raw.global_inject_headers)?;
    let global_rules = headers::resolve_rules(&raw.global_inject_headers, HeaderRuleSource::Global)
        .context("validate global proxy header rules")?;
    Ok(ProxyConfig {
        listen_addr,
        global_rules,
        mitm: None,
    })
}

fn resolve_env_values(rules: &mut [HeaderOverrideRule]) -> Result<()> {
    for (index, rule) in rules.iter_mut().enumerate() {
        let Some(value) = rule.override_header_value.as_mut() else {
            continue;
        };
        if let Some(name) = value.strip_prefix("env:") {
            *value = resolve_env(name, index)?;
        } else if value.starts_with("${") && value.ends_with('}') {
            let name = &value[2..value.len() - 1];
            *value = resolve_env(name, index)?;
        }
    }
    Ok(())
}

fn resolve_env(name: &str, index: usize) -> Result<String> {
    if name.is_empty() || name.contains('=') || name.as_bytes().contains(&0) {
        bail!("rule {index} references invalid env var name");
    }
    let value = std::env::var(name)
        .with_context(|| format!("rule {index} references missing env var {name}"))?;
    if value.is_empty() {
        bail!("rule {index} references empty env var {name}");
    }
    Ok(value)
}

#[cfg(unix)]
fn validate_permissions(path: &Path) -> Result<()> {
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
fn validate_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listen_addr() -> SocketAddr {
        "127.0.0.1:18080".parse().unwrap()
    }

    fn write_secure(path: &Path, body: &str) {
        std::fs::write(path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    #[test]
    fn resolves_env_forms_and_literals() {
        unsafe {
            std::env::set_var("PROXY_TEST_TOKEN", "secret");
        }
        let mut rules = vec![
            HeaderOverrideRule {
                endpoint: "http://a/*".into(),
                override_header_key: Some("X-A".into()),
                override_header_value: Some("env:PROXY_TEST_TOKEN".into()),
                http_proxy: None,
                https_proxy: None,
            },
            HeaderOverrideRule {
                endpoint: "http://a/*".into(),
                override_header_key: Some("X-B".into()),
                override_header_value: Some("${PROXY_TEST_TOKEN}".into()),
                http_proxy: None,
                https_proxy: None,
            },
            HeaderOverrideRule {
                endpoint: "http://a/*".into(),
                override_header_key: Some("X-C".into()),
                override_header_value: Some("literal".into()),
                http_proxy: None,
                https_proxy: None,
            },
        ];
        resolve_env_values(&mut rules).unwrap();
        assert_eq!(rules[0].override_header_value.as_deref(), Some("secret"));
        assert_eq!(rules[1].override_header_value.as_deref(), Some("secret"));
        assert_eq!(rules[2].override_header_value.as_deref(), Some("literal"));
    }

    #[test]
    fn missing_env_fails() {
        unsafe {
            std::env::remove_var("PROXY_TEST_MISSING");
        }
        let mut rules = vec![HeaderOverrideRule {
            endpoint: "http://a/*".into(),
            override_header_key: Some("X-A".into()),
            override_header_value: Some("env:PROXY_TEST_MISSING".into()),
            http_proxy: None,
            https_proxy: None,
        }];
        assert!(resolve_env_values(&mut rules).is_err());
    }

    #[test]
    fn rejects_invalid_env_references_without_lookup() {
        for value in ["env:", "${}", "${BAD=NAME}", "env:BAD\0NAME"] {
            let mut rules = vec![HeaderOverrideRule {
                endpoint: "http://a/*".into(),
                override_header_key: Some("X-A".into()),
                override_header_value: Some(value.into()),
                http_proxy: None,
                https_proxy: None,
            }];
            assert!(
                resolve_env_values(&mut rules).is_err(),
                "{value:?} should be rejected"
            );
        }
    }

    #[tokio::test]
    async fn explicit_missing_config_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.json");

        let err = load_global_config(Some(path.clone()), listen_addr())
            .await
            .err()
            .expect("missing explicit config should fail");

        assert!(
            err.to_string()
                .contains(&format!("proxy config does not exist: {}", path.display()))
        );
    }

    #[tokio::test]
    async fn loads_secure_config_and_defaults_missing_rule_list() {
        let dir = tempfile::tempdir().unwrap();
        let empty_path = dir.path().join("empty.json");
        write_secure(&empty_path, "{}");
        let empty = load_global_config(Some(empty_path), listen_addr())
            .await
            .unwrap();
        assert_eq!(empty.listen_addr, listen_addr());
        assert!(empty.global_rules.is_empty());

        let populated_path = dir.path().join("populated.json");
        write_secure(
            &populated_path,
            r#"{"global_inject_headers":[{"endpoint":"http://example.com/*","override_header_key":"X-Test","override_header_value":"value"}]}"#,
        );
        let populated = load_global_config(Some(populated_path), listen_addr())
            .await
            .unwrap();
        assert_eq!(populated.global_rules.len(), 1);
    }

    #[tokio::test]
    async fn malformed_config_reports_parse_context() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        write_secure(&path, "{not json");

        let err = load_global_config(Some(path.clone()), listen_addr())
            .await
            .err()
            .expect("malformed config should fail");

        assert!(format!("{err:#}").contains(&format!("parse proxy config: {}", path.display())));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_group_or_world_readable_config() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("insecure.json");
        std::fs::write(&path, "{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let err = load_global_config(Some(path), listen_addr())
            .await
            .err()
            .expect("insecure config should fail");

        assert!(format!("{err:#}").contains("mode must not allow group or world access"));
    }
}
