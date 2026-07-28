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

    #[test]
    fn filters_sensitive_env() {
        let mut req = AgentRunRequest {
            run_id: "r".into(),
            command: vec!["true".into()],
            injected_files: vec![],
            injected_readonly_dirs: vec![],
            env: vec![
                ("AWS_SECRET_ACCESS_KEY".into(), "x".into()),
                ("SAFE".into(), "ok".into()),
            ],
            inject_headers: vec![],
            timeout_ms: 1,
            kill_on_disconnect: true,
            limits: Default::default(),
            result_policy: Default::default(),
            cleanup_policy: Default::default(),
        };
        let env = clean_env(&req);
        assert!(env.iter().any(|(k, v)| k == "SAFE" && v == "ok"));
        assert!(!env.iter().any(|(k, _)| k == "AWS_SECRET_ACCESS_KEY"));
        req.env.push(("PATH".into(), "/bad".into()));
        assert_eq!(
            clean_env(&req).iter().find(|(k, _)| k == "PATH").unwrap().1,
            "/usr/local/bin:/usr/bin:/bin"
        );
    }

    #[test]
    fn user_mode_uses_workspace_env_and_blocks_overrides() {
        let req = AgentRunRequest {
            run_id: "r".into(),
            command: vec!["true".into()],
            injected_files: vec![],
            injected_readonly_dirs: vec![],
            env: vec![("HOME".into(), "/bad".into()), ("SAFE".into(), "ok".into())],
            inject_headers: vec![],
            timeout_ms: 1,
            kill_on_disconnect: true,
            limits: Default::default(),
            result_policy: Default::default(),
            cleanup_policy: Default::default(),
        };
        let slot = SlotSpec {
            id: "slot-0001".into(),
            uid: 10001,
            gid: 10001,
            workspace_path: "/var/lib/pi-sandbox/workspaces/slot-0001".into(),
        };

        let env = clean_env_for_slot(&req, &slot, IsolationMode::User);
        assert_eq!(
            env.iter().find(|(k, _)| k == "HOME").unwrap().1,
            "/var/lib/pi-sandbox/workspaces/slot-0001"
        );
        assert_eq!(
            env.iter().find(|(k, _)| k == "TMPDIR").unwrap().1,
            "/var/lib/pi-sandbox/workspaces/slot-0001/tmp"
        );
        assert_eq!(
            env.iter().find(|(k, _)| k == "TMP").unwrap().1,
            "/var/lib/pi-sandbox/workspaces/slot-0001/tmp"
        );
        assert_eq!(
            env.iter().find(|(k, _)| k == "TEMP").unwrap().1,
            "/var/lib/pi-sandbox/workspaces/slot-0001/tmp"
        );
        assert!(env.iter().any(|(k, v)| k == "SAFE" && v == "ok"));
    }
}
