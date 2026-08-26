#[cfg(target_os = "linux")]
use libseccomp::{ScmpAction, ScmpFilterContext, ScmpSyscall};

#[cfg(target_os = "linux")]
const DENIED_SYSCALLS: &[&str] = &[
    "mount",
    "umount2",
    "pivot_root",
    "chroot",
    "setns",
    "unshare",
    // `setsid` and `setpgid` are intentionally allowed: agent runtimes place
    // each spawned command in its own session/process group. Cleanup cannot
    // rely on process-group signaling alone; the UID-scoped reaper
    // (`sandbox::reaper`) covers descendants that detach.
    "ptrace",
    "bpf",
    "perf_event_open",
    "keyctl",
    "add_key",
    "request_key",
    "reboot",
    "kexec_load",
    "init_module",
    "finit_module",
    "delete_module",
    "swapon",
    "swapoff",
    "open_by_handle_at",
    "fanotify_init",
    "quotactl",
    "acct",
];

#[cfg(target_os = "linux")]
fn build_agent_seccomp_filter() -> anyhow::Result<ScmpFilterContext> {
    let mut ctx = ScmpFilterContext::new_filter(ScmpAction::Allow)?;
    for name in DENIED_SYSCALLS {
        if let Ok(syscall) = ScmpSyscall::from_name(name) {
            ctx.add_rule(ScmpAction::Errno(libc::EPERM), syscall)?;
        }
    }
    Ok(ctx)
}

#[cfg(target_os = "linux")]
pub fn install_agent_seccomp() -> anyhow::Result<()> {
    let ctx = build_agent_seccomp_filter()?;
    ctx.load()?;
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::collections::HashSet;

    use libseccomp::ScmpSyscall;

    use super::{DENIED_SYSCALLS, build_agent_seccomp_filter};

    #[test]
    fn session_and_process_group_syscalls_stay_allowed_for_agent_runtimes() {
        // Denying these made agent subprocess spawning fail outright.
        for syscall in ["setsid", "setpgid"] {
            assert!(
                !DENIED_SYSCALLS.contains(&syscall),
                "{syscall} must stay allowed; cleanup is the UID reaper's job"
            );
        }
    }

    #[test]
    fn denied_syscall_policy_has_no_duplicate_rules() {
        let unique = DENIED_SYSCALLS.iter().copied().collect::<HashSet<_>>();

        assert_eq!(unique.len(), DENIED_SYSCALLS.len());
    }

    #[test]
    fn core_isolation_and_listener_syscalls_resolve_on_linux() {
        for name in [
            "mount", "umount2", "unshare", "ptrace", "bpf", "bind", "listen", "accept",
        ] {
            assert!(
                ScmpSyscall::from_name(name).is_ok(),
                "required seccomp syscall name {name:?} did not resolve"
            );
        }
    }

    #[test]
    fn seccomp_filter_can_be_built_without_loading_it() {
        build_agent_seccomp_filter().unwrap();
    }
}
