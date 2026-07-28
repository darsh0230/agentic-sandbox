pub mod config;
pub mod headers;
pub mod matcher;
pub mod mitm;
pub mod server;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use rand::distributions::{Alphanumeric, DistString};
use rand::rngs::OsRng;
use tracing::info;

use crate::types::HeaderOverrideRule;

pub use config::{ProxyConfig, load_global_config};
pub use matcher::EndpointMatcher;

#[derive(Debug, Clone)]
pub struct ResolvedHeaderOverrideRule {
    pub endpoint: EndpointMatcher,
    pub header_key: Option<http::HeaderName>,
    pub header_value: Option<http::HeaderValue>,
    pub http_proxy: Option<String>,
    pub https_proxy: Option<String>,
    pub source: HeaderRuleSource,
}

#[derive(Debug, Clone)]
pub enum HeaderRuleSource {
    Global,
    Run { run_id: String },
}

#[derive(Debug, Clone)]
pub struct ProxyRunContext {
    pub run_id: String,
    pub slot_id: String,
    pub rules: Vec<ResolvedHeaderOverrideRule>,
}

#[derive(Clone)]
pub struct ProxyHandle {
    listen_addr: SocketAddr,
    global_rules: Arc<Vec<ResolvedHeaderOverrideRule>>,
    contexts: Arc<RwLock<HashMap<String, ProxyRunContext>>>,
    mitm: Option<Arc<mitm::MitmAuthority>>,
}

pub struct ProxyRunRegistration {
    token: String,
    listen_addr: SocketAddr,
    contexts: Arc<RwLock<HashMap<String, ProxyRunContext>>>,
}

impl ProxyHandle {
    pub async fn start(config: ProxyConfig) -> Result<Self> {
        let listener = tokio::net::TcpListener::bind(config.listen_addr).await?;
        let listen_addr = listener.local_addr()?;
        let contexts = Arc::new(RwLock::new(HashMap::new()));
        let handle = Self {
            listen_addr,
            global_rules: Arc::new(config.global_rules),
            contexts,
            mitm: config.mitm.map(|config| Arc::new(config.authority)),
        };
        server::spawn(handle.clone(), listener).await?;
        info!(listen_addr = %handle.listen_addr, global_rules = handle.global_rules.len(), "proxy server started");
        Ok(handle)
    }

    pub fn listen_addr(&self) -> SocketAddr {
        self.listen_addr
    }

    pub fn global_rules(&self) -> &[ResolvedHeaderOverrideRule] {
        &self.global_rules
    }

    pub fn run_context(&self, token: &str) -> Option<ProxyRunContext> {
        self.contexts.read().ok()?.get(token).cloned()
    }

    pub fn mitm(&self) -> Option<Arc<mitm::MitmAuthority>> {
        self.mitm.clone()
    }

    pub fn ca_cert_pem(&self) -> Option<&str> {
        self.mitm.as_ref().map(|authority| authority.ca_cert_pem())
    }

    pub fn register_run(
        &self,
        run_id: &str,
        slot_id: &str,
        rules: &[HeaderOverrideRule],
    ) -> Result<ProxyRunRegistration> {
        let resolved = headers::resolve_rules(
            rules,
            HeaderRuleSource::Run {
                run_id: run_id.to_string(),
            },
        )
        .context("validate per-run proxy header rules")?;
        let token = Alphanumeric.sample_string(&mut OsRng, 48);
        let context = ProxyRunContext {
            run_id: run_id.to_string(),
            slot_id: slot_id.to_string(),
            rules: resolved,
        };
        self.contexts
            .write()
            .expect("proxy context lock poisoned")
            .insert(token.clone(), context);
        info!(
            run_id,
            slot_id,
            per_run_rules = rules.len(),
            "proxy run context registered"
        );
        Ok(ProxyRunRegistration {
            token,
            listen_addr: self.listen_addr,
            contexts: self.contexts.clone(),
        })
    }
}

impl ProxyRunRegistration {
    pub fn env(&self) -> Vec<(String, String)> {
        let proxy_url = format!("http://run:{}@{}", self.token, self.listen_addr);
        vec![
            ("HTTP_PROXY".to_string(), proxy_url.clone()),
            ("HTTPS_PROXY".to_string(), proxy_url.clone()),
            ("ALL_PROXY".to_string(), proxy_url),
            (
                "NO_PROXY".to_string(),
                "127.0.0.1,localhost,::1".to_string(),
            ),
        ]
    }
}

impl Drop for ProxyRunRegistration {
    fn drop(&mut self) {
        if let Ok(mut contexts) = self.contexts.write()
            && let Some(context) = contexts.remove(&self.token)
        {
            info!(run_id = %context.run_id, slot_id = %context.slot_id, "proxy run context unregistered");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle() -> ProxyHandle {
        ProxyHandle {
            listen_addr: "127.0.0.1:18080".parse().unwrap(),
            global_rules: Arc::new(Vec::new()),
            contexts: Arc::new(RwLock::new(HashMap::new())),
            mitm: None,
        }
    }

    #[test]
    fn registration_creates_scoped_proxy_environment_and_drop_expires_token() {
        let handle = handle();
        let registration = handle.register_run("run-1", "slot-1", &[]).unwrap();
        let token = registration.token.clone();

        assert_eq!(token.len(), 48);
        assert!(token.bytes().all(|byte| byte.is_ascii_alphanumeric()));
        let context = handle.run_context(&token).unwrap();
        assert_eq!(context.run_id, "run-1");
        assert_eq!(context.slot_id, "slot-1");
        assert!(context.rules.is_empty());
        assert_eq!(
            registration.env(),
            vec![
                (
                    "HTTP_PROXY".into(),
                    format!("http://run:{token}@127.0.0.1:18080"),
                ),
                (
                    "HTTPS_PROXY".into(),
                    format!("http://run:{token}@127.0.0.1:18080"),
                ),
                (
                    "ALL_PROXY".into(),
                    format!("http://run:{token}@127.0.0.1:18080"),
                ),
                ("NO_PROXY".into(), "127.0.0.1,localhost,::1".into()),
            ]
        );

        drop(registration);
        assert!(handle.run_context(&token).is_none());
    }

    #[test]
    fn invalid_registration_does_not_create_a_context() {
        let handle = handle();
        let invalid = HeaderOverrideRule {
            endpoint: "http://example.com/*".into(),
            override_header_key: Some("Host".into()),
            override_header_value: Some("forbidden".into()),
            http_proxy: None,
            https_proxy: None,
        };

        assert!(handle.register_run("run-1", "slot-1", &[invalid]).is_err());
        assert!(handle.contexts.read().unwrap().is_empty());
    }

    #[test]
    fn registrations_use_distinct_tokens_and_drop_independently() {
        let handle = handle();
        let first = handle.register_run("run-1", "slot-1", &[]).unwrap();
        let second = handle.register_run("run-2", "slot-2", &[]).unwrap();
        assert_ne!(first.token, second.token);

        let first_token = first.token.clone();
        let second_token = second.token.clone();
        drop(first);
        assert!(handle.run_context(&first_token).is_none());
        assert!(handle.run_context(&second_token).is_some());
        drop(second);
        assert!(handle.run_context(&second_token).is_none());
    }
}
