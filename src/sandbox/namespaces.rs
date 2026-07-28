#[cfg(target_os = "linux")]
use nix::sched::CloneFlags;

#[cfg(target_os = "linux")]
const SANDBOX_HOSTNAME: &str = "pi-agent";

#[cfg(target_os = "linux")]
fn sandbox_namespace_flags() -> CloneFlags {
    CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_NEWIPC | CloneFlags::CLONE_NEWUTS
}

#[cfg(target_os = "linux")]
pub fn unshare_sandbox_namespaces() -> anyhow::Result<()> {
    nix::sched::unshare(sandbox_namespace_flags())?;
    nix::unistd::sethostname(SANDBOX_HOSTNAME)?;
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use nix::sched::CloneFlags;

    use super::sandbox_namespace_flags;

    #[test]
    fn sandbox_namespace_policy_is_explicit_about_shared_boundaries() {
        let flags = sandbox_namespace_flags();

        assert!(flags.contains(CloneFlags::CLONE_NEWNS));
        assert!(flags.contains(CloneFlags::CLONE_NEWIPC));
        assert!(flags.contains(CloneFlags::CLONE_NEWUTS));
        assert!(!flags.contains(CloneFlags::CLONE_NEWPID));
        assert!(!flags.contains(CloneFlags::CLONE_NEWNET));
        assert!(!flags.contains(CloneFlags::CLONE_NEWUSER));
    }
}
