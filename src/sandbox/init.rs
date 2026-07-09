#[cfg(target_os = "linux")]
use anyhow::Result;

#[cfg(target_os = "linux")]
pub fn prepare_and_drop(config: &crate::sandbox::SandboxLaunchConfig) -> Result<()> {
    unsafe {
        libc::setpgid(0, 0);
    }
    crate::sandbox::namespaces::unshare_sandbox_namespaces()?;
    crate::sandbox::mounts::make_mounts_private()?;
    let root = crate::sandbox::rootfs::build_rootfs(config)?;
    crate::sandbox::rootfs::pivot_into_rootfs(&root)?;
    crate::sandbox::mounts::mount_proc()?;
    std::env::set_current_dir("/home/agent")?;
    crate::sandbox::fds::close_unneeded_fds()?;
    crate::sandbox::credentials::drop_to_agent(config.slot.uid, config.slot.gid)?;
    crate::sandbox::credentials::drop_capabilities_best_effort();
    crate::sandbox::credentials::set_no_new_privs()?;
    crate::sandbox::seccomp::install_agent_seccomp()?;
    Ok(())
}
