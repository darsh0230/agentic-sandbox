#[cfg(target_os = "linux")]
use anyhow::{Context, Result};

#[cfg(target_os = "linux")]
pub fn prepare_and_drop(config: &crate::sandbox::SandboxLaunchConfig) -> Result<()> {
    unsafe {
        libc::setpgid(0, 0);
    }
    crate::sandbox::namespaces::unshare_sandbox_namespaces()
        .context("unshare sandbox namespaces")?;
    crate::sandbox::mounts::make_mounts_private().context("make mount propagation private")?;
    let root = crate::sandbox::rootfs::build_rootfs(config).context("build sandbox rootfs")?;
    crate::sandbox::rootfs::pivot_into_rootfs(&root).context("pivot into sandbox rootfs")?;
    crate::sandbox::mounts::mount_proc().context("mount private proc")?;
    std::env::set_current_dir("/home/agent").context("chdir to /home/agent")?;
    crate::sandbox::fds::close_unneeded_fds().context("close unneeded fds")?;
    crate::sandbox::credentials::drop_to_agent(config.slot.uid, config.slot.gid)
        .context("drop to slot uid/gid")?;
    crate::sandbox::credentials::drop_capabilities_best_effort();
    crate::sandbox::credentials::set_no_new_privs().context("set no_new_privs")?;
    crate::sandbox::seccomp::install_agent_seccomp().context("install agent seccomp")?;
    Ok(())
}
