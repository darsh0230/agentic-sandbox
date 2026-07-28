use std::collections::BTreeMap;

use crate::sandbox::{IsolationMode, SlotSpec};
use crate::types::AgentRunRequest;

const DENY_EXACT: &[&str] = &[
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "PWD",
    "TMPDIR",
    "TEMP",
    "TMP",
    "PATH",
    "XDG_CACHE_HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "PIP_CACHE_DIR",
    "PYTHONUSERBASE",
    "PYTHONPYCACHEPREFIX",
    "NPM_CONFIG_CACHE",
    "NPM_CONFIG_PREFIX",
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "DOCKER_HOST",
    "KUBECONFIG",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
    "SSL_CERT_FILE",
    "REQUESTS_CA_BUNDLE",
    "NODE_EXTRA_CA_CERTS",
    "GIT_SSL_CAINFO",
];

pub fn clean_env(req: &AgentRunRequest) -> Vec<(String, String)> {
    clean_env_with_base(req, "/home/agent", "/tmp", "agent")
}

pub fn clean_env_for_slot(
    req: &AgentRunRequest,
    slot: &SlotSpec,
    isolation_mode: IsolationMode,
) -> Vec<(String, String)> {
    match isolation_mode {
        IsolationMode::Privileged => clean_env(req),
        IsolationMode::User => {
            let workspace = slot.workspace_path.to_string_lossy();
            let tmp = slot.workspace_path.join("tmp");
            let tmp = tmp.to_string_lossy();
            clean_env_with_base(req, &workspace, &tmp, &slot.id)
        }
    }
}

fn clean_env_with_base(
    req: &AgentRunRequest,
    home: &str,
    tmp: &str,
    username: &str,
) -> Vec<(String, String)> {
    let cache = format!("{home}/.cache");
    let local = format!("{home}/.local");
    let mut env = BTreeMap::from([
        ("HOME".to_string(), home.to_string()),
        ("USER".to_string(), username.to_string()),
        ("LOGNAME".to_string(), username.to_string()),
        ("SHELL".to_string(), "/bin/bash".to_string()),
        ("PWD".to_string(), home.to_string()),
        (
            "PATH".to_string(),
            "/usr/local/bin:/usr/bin:/bin".to_string(),
        ),
        ("TMPDIR".to_string(), tmp.to_string()),
        ("TEMP".to_string(), tmp.to_string()),
        ("TMP".to_string(), tmp.to_string()),
        ("XDG_CACHE_HOME".to_string(), cache.clone()),
        ("XDG_CONFIG_HOME".to_string(), format!("{home}/.config")),
        ("XDG_DATA_HOME".to_string(), format!("{local}/share")),
        ("PIP_CACHE_DIR".to_string(), format!("{cache}/pip")),
        ("PYTHONUSERBASE".to_string(), local.clone()),
        (
            "PYTHONPYCACHEPREFIX".to_string(),
            format!("{cache}/pycache"),
        ),
        ("NPM_CONFIG_CACHE".to_string(), format!("{cache}/npm")),
        ("NPM_CONFIG_PREFIX".to_string(), local),
    ]);

    for (key, value) in &req.env {
        if is_allowed_env_key(key) {
            env.insert(key.clone(), value.clone());
        }
    }
    env.into_iter().collect()
}

fn is_allowed_env_key(key: &str) -> bool {
    if key.is_empty() || key.contains('=') || key.as_bytes().contains(&0) {
        return false;
    }
    if DENY_EXACT.contains(&key) {
        return false;
    }
    !(key.starts_with("AWS_") || key.starts_with("GCP_") || key.starts_with("AZURE_"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::AgentRunRequest;

    fn request_with_env(env: Vec<(String, String)>) -> AgentRunRequest {
        let mut request: AgentRunRequest =
            serde_json::from_str(r#"{"run_id":"r","command":["true"]}"#).unwrap();
        request.env = env;
        request
    }

    fn value<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
        env.iter()
            .find_map(|(candidate, value)| (candidate == key).then_some(value.as_str()))
    }

    #[test]
    fn blocks_every_reserved_and_seeded_environment_key() {
        let marker = "request-controlled";
        let request = request_with_env(
            DENY_EXACT
                .iter()
                .map(|key| ((*key).to_string(), marker.to_string()))
                .collect(),
        );

        let env = clean_env(&request);
        for key in DENY_EXACT {
            assert_ne!(value(&env, key), Some(marker), "{key} was overridden");
        }

        assert_eq!(value(&env, "HOME"), Some("/home/agent"));
        assert_eq!(value(&env, "XDG_CACHE_HOME"), Some("/home/agent/.cache"));
        assert_eq!(value(&env, "NPM_CONFIG_PREFIX"), Some("/home/agent/.local"));
        assert_eq!(value(&env, "PATH"), Some("/usr/local/bin:/usr/bin:/bin"));
        assert_eq!(value(&env, "LD_PRELOAD"), None);
        assert_eq!(value(&env, "HTTP_PROXY"), None);
    }

    #[test]
    fn filters_cloud_prefixes_and_malformed_names() {
        let request = request_with_env(vec![
            ("AWS_SECRET_ACCESS_KEY".into(), "x".into()),
            ("GCP_TOKEN".into(), "x".into()),
            ("AZURE_CLIENT_SECRET".into(), "x".into()),
            ("".into(), "x".into()),
            ("BAD=NAME".into(), "x".into()),
            ("BAD\0NAME".into(), "x".into()),
            ("AWS".into(), "allowed".into()),
            ("aws_secret".into(), "case-sensitive".into()),
            ("SAFE".into(), "ok".into()),
        ]);

        let env = clean_env(&request);
        for key in [
            "AWS_SECRET_ACCESS_KEY",
            "GCP_TOKEN",
            "AZURE_CLIENT_SECRET",
            "",
            "BAD=NAME",
            "BAD\0NAME",
        ] {
            assert_eq!(value(&env, key), None, "{key:?}");
        }
        assert_eq!(value(&env, "AWS"), Some("allowed"));
        assert_eq!(value(&env, "aws_secret"), Some("case-sensitive"));
        assert_eq!(value(&env, "SAFE"), Some("ok"));
    }

    #[test]
    fn allowed_duplicates_use_the_last_value_and_output_is_sorted() {
        let request = request_with_env(vec![
            ("Z_SAFE".into(), "first".into()),
            ("A_SAFE".into(), "a".into()),
            ("Z_SAFE".into(), "last".into()),
        ]);

        let env = clean_env(&request);
        assert_eq!(value(&env, "Z_SAFE"), Some("last"));
        assert_eq!(
            env.iter().filter(|(key, _)| key == "Z_SAFE").count(),
            1,
            "environment keys must be unique"
        );
        assert!(
            env.windows(2).all(|pair| pair[0].0 < pair[1].0),
            "BTreeMap output should remain deterministic"
        );
    }

    #[test]
    fn user_mode_uses_workspace_for_every_runtime_directory() {
        let req = request_with_env(vec![
            ("HOME".into(), "/bad".into()),
            ("XDG_CACHE_HOME".into(), "/also-bad".into()),
            ("NPM_CONFIG_PREFIX".into(), "/also-bad".into()),
            ("SAFE".into(), "ok".into()),
        ]);
        let slot = SlotSpec {
            id: "slot-0001".into(),
            uid: 10001,
            gid: 10001,
            workspace_path: "/var/lib/agentic-sandbox/workspaces/slot-0001".into(),
        };

        let env = clean_env_for_slot(&req, &slot, IsolationMode::User);
        let home = "/var/lib/agentic-sandbox/workspaces/slot-0001";
        for key in ["HOME", "PWD"] {
            assert_eq!(value(&env, key), Some(home), "{key}");
        }
        for key in ["TMPDIR", "TMP", "TEMP"] {
            assert_eq!(
                value(&env, key),
                Some("/var/lib/agentic-sandbox/workspaces/slot-0001/tmp"),
                "{key}"
            );
        }
        assert_eq!(value(&env, "USER"), Some("slot-0001"));
        assert_eq!(value(&env, "LOGNAME"), Some("slot-0001"));
        assert_eq!(
            value(&env, "XDG_CACHE_HOME"),
            Some("/var/lib/agentic-sandbox/workspaces/slot-0001/.cache")
        );
        assert_eq!(
            value(&env, "XDG_CONFIG_HOME"),
            Some("/var/lib/agentic-sandbox/workspaces/slot-0001/.config")
        );
        assert_eq!(
            value(&env, "XDG_DATA_HOME"),
            Some("/var/lib/agentic-sandbox/workspaces/slot-0001/.local/share")
        );
        assert_eq!(
            value(&env, "PIP_CACHE_DIR"),
            Some("/var/lib/agentic-sandbox/workspaces/slot-0001/.cache/pip")
        );
        assert_eq!(
            value(&env, "PYTHONUSERBASE"),
            Some("/var/lib/agentic-sandbox/workspaces/slot-0001/.local")
        );
        assert_eq!(
            value(&env, "PYTHONPYCACHEPREFIX"),
            Some("/var/lib/agentic-sandbox/workspaces/slot-0001/.cache/pycache")
        );
        assert_eq!(
            value(&env, "NPM_CONFIG_CACHE"),
            Some("/var/lib/agentic-sandbox/workspaces/slot-0001/.cache/npm")
        );
        assert_eq!(
            value(&env, "NPM_CONFIG_PREFIX"),
            Some("/var/lib/agentic-sandbox/workspaces/slot-0001/.local")
        );
        assert_eq!(value(&env, "SAFE"), Some("ok"));
    }

    #[test]
    fn privileged_mode_uses_sandbox_paths_even_for_a_host_slot() {
        let req = request_with_env(Vec::new());
        let slot = SlotSpec {
            id: "slot-0001".into(),
            uid: 10001,
            gid: 10001,
            workspace_path: "/host/workspaces/slot-0001".into(),
        };

        let env = clean_env_for_slot(&req, &slot, IsolationMode::Privileged);
        assert_eq!(value(&env, "HOME"), Some("/home/agent"));
        assert_eq!(value(&env, "TMPDIR"), Some("/tmp"));
        assert_eq!(value(&env, "USER"), Some("agent"));
    }
}
