#[cfg(target_os = "linux")]
pub fn unshare_sandbox_namespaces() -> anyhow::Result<()> {
    use nix::sched::{CloneFlags, unshare};
    unshare(CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_NEWIPC | CloneFlags::CLONE_NEWUTS)?;
    nix::unistd::sethostname("pi-agent")?;
    Ok(())
}
