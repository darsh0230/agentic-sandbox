#[cfg(target_os = "linux")]
pub fn install_agent_seccomp() -> anyhow::Result<()> {
    use libseccomp::{ScmpAction, ScmpFilterContext, ScmpSyscall};

    let mut ctx = ScmpFilterContext::new_filter(ScmpAction::Allow)?;
    for name in [
        "mount",
        "umount2",
        "pivot_root",
        "chroot",
        "setns",
        "unshare",
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
        "bind",
        "listen",
        "accept",
        "accept4",
    ] {
        if let Ok(syscall) = ScmpSyscall::from_name(name) {
            ctx.add_rule(ScmpAction::Errno(libc::EPERM), syscall)?;
        }
    }
    ctx.load()?;
    Ok(())
}
