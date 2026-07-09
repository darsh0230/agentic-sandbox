#[cfg(target_os = "linux")]
use std::path::Path;

#[cfg(target_os = "linux")]
use anyhow::Result;

#[cfg(target_os = "linux")]
use nix::mount::{MsFlags, mount};

#[cfg(target_os = "linux")]
pub fn make_mounts_private() -> Result<()> {
    mount::<str, str, str, str>(None, "/", None, MsFlags::MS_REC | MsFlags::MS_PRIVATE, None)?;
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn bind_ro(src: &Path, dst: &Path, recursive: bool, executable: bool) -> Result<()> {
    let bind_flags = if recursive {
        MsFlags::MS_BIND | MsFlags::MS_REC
    } else {
        MsFlags::MS_BIND
    };
    mount(Some(src), dst, None::<&str>, bind_flags, None::<&str>)?;
    let mut remount = MsFlags::MS_BIND
        | MsFlags::MS_REMOUNT
        | MsFlags::MS_RDONLY
        | MsFlags::MS_NOSUID
        | MsFlags::MS_NODEV;
    if !executable {
        remount |= MsFlags::MS_NOEXEC;
    }
    mount::<str, Path, str, str>(None, dst, None, remount, None)?;
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn bind_rw(src: &Path, dst: &Path, recursive: bool) -> Result<()> {
    let flags = if recursive {
        MsFlags::MS_BIND | MsFlags::MS_REC
    } else {
        MsFlags::MS_BIND
    };
    mount(Some(src), dst, None::<&str>, flags, None::<&str>)?;
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn mount_tmpfs(dst: &Path, options: &str) -> Result<()> {
    let mut flags = MsFlags::empty();
    let mut data = Vec::new();
    for option in options.split(',').filter(|option| !option.is_empty()) {
        match option {
            "nosuid" => flags |= MsFlags::MS_NOSUID,
            "nodev" => flags |= MsFlags::MS_NODEV,
            "noexec" => flags |= MsFlags::MS_NOEXEC,
            other => data.push(other),
        }
    }
    let data = data.join(",");
    let data = if data.is_empty() {
        None
    } else {
        Some(data.as_str())
    };
    mount(Some("tmpfs"), dst, Some("tmpfs"), flags, data)?;
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn mount_proc() -> Result<()> {
    mount(
        Some("proc"),
        "/proc",
        Some("proc"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
        None::<&str>,
    )?;
    Ok(())
}
