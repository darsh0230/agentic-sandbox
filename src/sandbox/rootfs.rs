#[cfg(target_os = "linux")]
use std::fs;
#[cfg(target_os = "linux")]
use std::os::unix::fs::symlink;
#[cfg(target_os = "linux")]
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
use anyhow::{Context, Result};

#[cfg(target_os = "linux")]
pub fn build_rootfs(config: &crate::sandbox::SandboxLaunchConfig) -> Result<PathBuf> {
    let root = config.root.join("roots").join(&config.run_id);
    if root.exists() {
        fs::remove_dir_all(&root).ok();
    }
    for dir in [
        "usr",
        "etc",
        "home/agent",
        "tmp",
        "var/tmp",
        "run",
        "dev/shm",
        "proc",
    ] {
        fs::create_dir_all(root.join(dir))?;
    }
    crate::sandbox::mounts::bind_rw(&root, &root, false)?;

    bind_ro_or_symlink(Path::new("/usr"), &root.join("usr"), true)?;
    for path in ["/bin", "/sbin", "/lib", "/lib64"] {
        bind_ro_or_symlink(
            Path::new(path),
            &root.join(path.trim_start_matches('/')),
            true,
        )
        .ok();
    }

    write_etc(&root, config.slot.uid, config.slot.gid)?;
    bind_if_exists(
        Path::new("/etc/resolv.conf"),
        &root.join("etc/resolv.conf"),
        false,
        false,
    )?;
    bind_if_exists(Path::new("/etc/ssl"), &root.join("etc/ssl"), true, false).ok();
    bind_if_exists(
        Path::new("/etc/ca-certificates"),
        &root.join("etc/ca-certificates"),
        true,
        false,
    )
    .ok();

    crate::sandbox::mounts::mount_tmpfs(&root.join("tmp"), "mode=1777,nosuid,nodev")?;
    crate::sandbox::mounts::mount_tmpfs(&root.join("var/tmp"), "mode=1777,nosuid,nodev")?;
    crate::sandbox::mounts::mount_tmpfs(&root.join("run"), "mode=0755,nosuid,nodev,noexec")?;
    crate::sandbox::mounts::mount_tmpfs(&root.join("dev/shm"), "mode=1777,nosuid,nodev,noexec")?;

    for dev in ["null", "zero", "random", "urandom"] {
        let dst = root.join("dev").join(dev);
        fs::File::create(&dst)?;
        crate::sandbox::mounts::bind_rw(&Path::new("/dev").join(dev), &dst, false)?;
    }
    symlink("/proc/self/fd", root.join("dev/fd")).ok();
    symlink("/proc/self/fd/0", root.join("dev/stdin")).ok();
    symlink("/proc/self/fd/1", root.join("dev/stdout")).ok();
    symlink("/proc/self/fd/2", root.join("dev/stderr")).ok();

    crate::sandbox::mounts::bind_rw(&config.slot.workspace_path, &root.join("home/agent"), true)?;
    symlink("/home/agent", root.join("workspace")).ok();

    for dir in &config.injected_readonly_dirs {
        let relative = dir.sandbox_path.trim_start_matches('/');
        let dst = root.join(relative);
        fs::create_dir_all(&dst)?;
        crate::sandbox::mounts::bind_ro(Path::new(&dir.source_path), &dst, true, false)?;
    }
    Ok(root)
}

#[cfg(target_os = "linux")]
pub fn pivot_into_rootfs(root: &Path) -> Result<()> {
    let oldroot = root.join(".oldroot");
    fs::create_dir_all(&oldroot)?;
    nix::unistd::pivot_root(root, &oldroot).context("pivot_root")?;
    std::env::set_current_dir("/")?;
    nix::mount::umount2("/.oldroot", nix::mount::MntFlags::MNT_DETACH).ok();
    fs::remove_dir("/.oldroot").ok();
    Ok(())
}

#[cfg(target_os = "linux")]
fn bind_ro_or_symlink(src: &Path, dst: &Path, executable: bool) -> Result<()> {
    let meta = fs::symlink_metadata(src)?;
    if meta.file_type().is_symlink() {
        let target = fs::read_link(src)?;
        if dst.exists() {
            fs::remove_dir_all(dst)
                .or_else(|_| fs::remove_file(dst))
                .ok();
        }
        symlink(target, dst)?;
    } else {
        fs::create_dir_all(dst)?;
        crate::sandbox::mounts::bind_ro(src, dst, true, executable)?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn bind_if_exists(src: &Path, dst: &Path, recursive: bool, executable: bool) -> Result<()> {
    if !src.exists() {
        return Ok(());
    }
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    if src.is_dir() {
        fs::create_dir_all(dst)?;
    } else {
        fs::File::create(dst)?;
    }
    crate::sandbox::mounts::bind_ro(src, dst, recursive, executable)
}

#[cfg(target_os = "linux")]
fn write_etc(root: &Path, uid: u32, gid: u32) -> Result<()> {
    fs::write(
        root.join("etc/passwd"),
        format!(
            "root:x:0:0:root:/root:/usr/sbin/nologin\nagent:x:{uid}:{gid}:Agent:/home/agent:/bin/bash\n"
        ),
    )?;
    fs::write(
        root.join("etc/group"),
        format!("root:x:0:\nagent:x:{gid}:\n"),
    )?;
    fs::write(
        root.join("etc/hosts"),
        "127.0.0.1 localhost\n::1 localhost ip6-localhost\n",
    )?;
    fs::write(
        root.join("etc/nsswitch.conf"),
        "passwd: files\ngroup: files\nhosts: files dns\n",
    )?;
    Ok(())
}
