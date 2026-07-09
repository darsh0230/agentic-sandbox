use std::collections::BTreeMap;

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
];

pub fn clean_env(req: &AgentRunRequest) -> Vec<(String, String)> {
    let mut env = BTreeMap::from([
        ("HOME".to_string(), "/home/agent".to_string()),
        ("USER".to_string(), "agent".to_string()),
        ("LOGNAME".to_string(), "agent".to_string()),
        ("SHELL".to_string(), "/bin/bash".to_string()),
        ("PWD".to_string(), "/home/agent".to_string()),
        (
            "PATH".to_string(),
            "/usr/local/bin:/usr/bin:/bin".to_string(),
        ),
        ("TMPDIR".to_string(), "/tmp".to_string()),
        ("TEMP".to_string(), "/tmp".to_string()),
        ("TMP".to_string(), "/tmp".to_string()),
        (
            "XDG_CACHE_HOME".to_string(),
            "/home/agent/.cache".to_string(),
        ),
        (
            "XDG_CONFIG_HOME".to_string(),
            "/home/agent/.config".to_string(),
        ),
        (
            "XDG_DATA_HOME".to_string(),
            "/home/agent/.local/share".to_string(),
        ),
        (
            "PIP_CACHE_DIR".to_string(),
            "/home/agent/.cache/pip".to_string(),
        ),
        (
            "PYTHONUSERBASE".to_string(),
            "/home/agent/.local".to_string(),
        ),
        (
            "PYTHONPYCACHEPREFIX".to_string(),
            "/home/agent/.cache/pycache".to_string(),
        ),
        (
            "NPM_CONFIG_CACHE".to_string(),
            "/home/agent/.cache/npm".to_string(),
        ),
        (
            "NPM_CONFIG_PREFIX".to_string(),
            "/home/agent/.local".to_string(),
        ),
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
            timeout_ms: 1,
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
}
