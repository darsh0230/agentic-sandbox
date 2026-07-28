#[cfg(target_os = "linux")]
use std::path::Path;

#[cfg(target_os = "linux")]
use anyhow::Result;

#[cfg(target_os = "linux")]
use nix::mount::{MsFlags, mount};

#[cfg(target_os = "linux")]
fn bind_mount_flags(recursive: bool) -> MsFlags {
    if recursive {
        MsFlags::MS_BIND | MsFlags::MS_REC
    } else {
        MsFlags::MS_BIND
    }
}

#[cfg(target_os = "linux")]
fn readonly_bind_remount_flags(executable: bool) -> MsFlags {
    let mut flags = MsFlags::MS_BIND
        | MsFlags::MS_REMOUNT
        | MsFlags::MS_RDONLY
        | MsFlags::MS_NOSUID
        | MsFlags::MS_NODEV;
    if !executable {
        flags |= MsFlags::MS_NOEXEC;
    }
    flags
}

#[cfg(target_os = "linux")]
fn parse_tmpfs_options(options: &str) -> (MsFlags, Option<String>) {
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
    let data = (!data.is_empty()).then(|| data.join(","));
    (flags, data)
}

#[cfg(target_os = "linux")]
pub fn make_mounts_private() -> Result<()> {
    mount::<str, str, str, str>(None, "/", None, MsFlags::MS_REC | MsFlags::MS_PRIVATE, None)?;
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn bind_ro(src: &Path, dst: &Path, recursive: bool, executable: bool) -> Result<()> {
    let bind_flags = bind_mount_flags(recursive);
    mount(Some(src), dst, None::<&str>, bind_flags, None::<&str>)?;
    let remount = readonly_bind_remount_flags(executable);
    mount::<str, Path, str, str>(None, dst, None, remount, None)?;
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn bind_rw(src: &Path, dst: &Path, recursive: bool) -> Result<()> {
    let flags = bind_mount_flags(recursive);
    mount(Some(src), dst, None::<&str>, flags, None::<&str>)?;
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn mount_tmpfs(dst: &Path, options: &str) -> Result<()> {
    let (flags, data) = parse_tmpfs_options(options);
    mount(Some("tmpfs"), dst, Some("tmpfs"), flags, data.as_deref())?;
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

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use nix::mount::MsFlags;

    use super::{bind_mount_flags, parse_tmpfs_options, readonly_bind_remount_flags};

    #[test]
    fn bind_flags_only_add_recursion_when_requested() {
        assert_eq!(bind_mount_flags(false), MsFlags::MS_BIND);
        assert_eq!(bind_mount_flags(true), MsFlags::MS_BIND | MsFlags::MS_REC);
    }

    #[test]
    fn readonly_bind_flags_always_apply_basic_hardening() {
        let required = MsFlags::MS_BIND
            | MsFlags::MS_REMOUNT
            | MsFlags::MS_RDONLY
            | MsFlags::MS_NOSUID
            | MsFlags::MS_NODEV;

        let executable = readonly_bind_remount_flags(true);
        assert!(executable.contains(required));
        assert!(!executable.contains(MsFlags::MS_NOEXEC));

        let non_executable = readonly_bind_remount_flags(false);
        assert!(non_executable.contains(required | MsFlags::MS_NOEXEC));
    }

    #[test]
    fn tmpfs_options_separate_security_flags_from_mount_data() {
        let (flags, data) = parse_tmpfs_options("nosuid,size=64m,,nodev,mode=0700,noexec");

        assert_eq!(
            flags,
            MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC
        );
        assert_eq!(data.as_deref(), Some("size=64m,mode=0700"));
    }

    #[test]
    fn tmpfs_options_omit_empty_mount_data() {
        let (flags, data) = parse_tmpfs_options("nosuid,nodev");

        assert_eq!(flags, MsFlags::MS_NOSUID | MsFlags::MS_NODEV);
        assert_eq!(data, None);
    }
}
