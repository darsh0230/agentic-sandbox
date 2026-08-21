use anyhow::{Context, Result, bail};

use crate::sandbox::IsolationMode;
use crate::types::AgentRunRequest;

use super::result_collector;

pub(super) fn validate_request(req: &AgentRunRequest, isolation_mode: IsolationMode) -> Result<()> {
    crate::common::paths::validate_run_id(&req.run_id)?;
    if req.command.is_empty()
        || req
            .command
            .iter()
            .any(|part| part.is_empty() || part.as_bytes().contains(&0))
    {
        bail!("command must contain non-empty strings without NUL bytes");
    }
    for file in &req.injected_files {
        crate::common::paths::validate_relative_path(&file.relative_path)?;
        if file.mode & !0o777 != 0 {
            bail!("invalid mode for {}", file.relative_path);
        }
    }
    for dir in &req.injected_readonly_dirs {
        crate::common::paths::validate_sandbox_absolute_path(&dir.source_path)
            .context("invalid readonly directory source path")?;
        crate::common::paths::validate_sandbox_absolute_path(&dir.sandbox_path)?;
        if !dir.readonly {
            bail!("injected directories must be readonly in this implementation");
        }
    }
    if req.result_policy.collect_paths.len() > result_collector::MAX_COLLECT_PATHS {
        bail!(
            "too many result collection paths: max {}",
            result_collector::MAX_COLLECT_PATHS
        );
    }
    for path in &req.result_policy.collect_paths {
        crate::common::paths::validate_relative_path(path)
            .context("invalid result collection path")?;
    }
    if let Some(path) = &req.result_policy.result_file {
        crate::common::paths::validate_relative_path(path).context("invalid result file path")?;
    }
    if isolation_mode == IsolationMode::User && !req.injected_readonly_dirs.is_empty() {
        bail!("readonly injected directories require privileged isolation mode");
    }
    crate::proxy::headers::resolve_rules(
        &req.inject_headers,
        crate::proxy::HeaderRuleSource::Run {
            run_id: req.run_id.clone(),
        },
    )?;
    Ok(())
}

pub(super) fn validate_user_mode_slots(slots: &[super::allocator::Slot]) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        for slot in slots {
            if unsafe { libc::getpwuid(slot.uid) }.is_null() {
                bail!(
                    "user isolation mode requires pre-created uid {} for {}",
                    slot.uid,
                    slot.id
                );
            }
            if unsafe { libc::getgrgid(slot.gid) }.is_null() {
                bail!(
                    "user isolation mode requires pre-created gid {} for {}",
                    slot.gid,
                    slot.id
                );
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = slots;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::master::tests::request;
    use crate::types::{InjectedDir, InjectedFile};

    #[test]
    fn user_mode_rejects_readonly_injected_dirs() {
        let mut req = request();
        req.injected_readonly_dirs.push(InjectedDir {
            source_path: "/fixtures".into(),
            sandbox_path: "/opt/fixtures".into(),
            readonly: true,
        });

        assert!(validate_request(&req, IsolationMode::Privileged).is_ok());
        let err = validate_request(&req, IsolationMode::User).unwrap_err();
        assert!(
            err.to_string()
                .contains("readonly injected directories require privileged isolation mode")
        );
    }

    #[test]
    fn validates_run_id_command_and_file_mode_boundaries() {
        for run_id in ["", ".", "..", "../escape", "a/b", "a\0b"] {
            let mut req = request();
            req.run_id = run_id.into();
            assert!(
                validate_request(&req, IsolationMode::Privileged).is_err(),
                "unsafe run id was accepted: {run_id:?}"
            );
        }
        for run_id in ["r", "run-001", "job_1.2"] {
            let mut req = request();
            req.run_id = run_id.into();
            assert!(
                validate_request(&req, IsolationMode::Privileged).is_ok(),
                "safe run id was rejected: {run_id:?}"
            );
        }

        for command in [vec![], vec!["".into()], vec!["echo".into(), "a\0b".into()]] {
            let mut req = request();
            req.command = command;
            assert!(validate_request(&req, IsolationMode::Privileged).is_err());
        }

        for mode in [0, 0o600, 0o755, 0o777] {
            let mut req = request();
            req.injected_files.push(InjectedFile {
                relative_path: "input".into(),
                content: vec![],
                mode,
            });
            assert!(
                validate_request(&req, IsolationMode::Privileged).is_ok(),
                "ordinary permission mode was rejected: {mode:o}"
            );
        }
        for mode in [0o1000, 0o4000, 0o10000, u32::MAX] {
            let mut req = request();
            req.injected_files.push(InjectedFile {
                relative_path: "input".into(),
                content: vec![],
                mode,
            });
            assert!(
                validate_request(&req, IsolationMode::Privileged).is_err(),
                "special or out-of-range mode was accepted: {mode:o}"
            );
        }
    }

    #[test]
    fn validates_all_paths_before_allocating_a_slot() {
        for path in ["", ".", "../escape", "/absolute", "a\0b"] {
            let mut req = request();
            req.result_policy.collect_paths = vec![path.into()];
            assert!(
                validate_request(&req, IsolationMode::Privileged).is_err(),
                "unsafe collection path was accepted: {path:?}"
            );

            let mut req = request();
            req.result_policy.result_file = Some(path.into());
            assert!(
                validate_request(&req, IsolationMode::Privileged).is_err(),
                "unsafe result file was accepted: {path:?}"
            );
        }

        let mut req = request();
        req.injected_readonly_dirs.push(InjectedDir {
            source_path: "relative".into(),
            sandbox_path: "/opt/fixtures".into(),
            readonly: true,
        });
        assert!(validate_request(&req, IsolationMode::Privileged).is_err());

        let mut req = request();
        req.injected_readonly_dirs.push(InjectedDir {
            source_path: "/fixtures".into(),
            sandbox_path: "/".into(),
            readonly: true,
        });
        assert!(validate_request(&req, IsolationMode::Privileged).is_err());

        let mut req = request();
        req.result_policy.collect_paths =
            vec!["artifact".into(); result_collector::MAX_COLLECT_PATHS + 1];
        assert!(validate_request(&req, IsolationMode::Privileged).is_err());
    }
}
