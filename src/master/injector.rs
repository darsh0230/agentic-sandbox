use anyhow::{Context, Result, bail};
use std::path::Path;

#[cfg(any(not(target_os = "linux"), test))]
use tokio::fs;
#[cfg(not(target_os = "linux"))]
use tokio::fs::OpenOptions;
#[cfg(not(target_os = "linux"))]
use tokio::io::AsyncWriteExt;

use crate::common::paths::validate_relative_path;
#[cfg(not(target_os = "linux"))]
use crate::master::workspace::chown;
use crate::sandbox::SlotSpec;
use crate::types::InjectedFile;

pub struct Injector;

impl Default for Injector {
    fn default() -> Self {
        Self::new()
    }
}

impl Injector {
    pub fn new() -> Self {
        Self
    }

    pub async fn inject(&self, slot: &SlotSpec, files: &[InjectedFile]) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            let slot = slot.clone();
            let files = files.to_vec();
            tokio::task::spawn_blocking(move || inject_linux(&slot, &files))
                .await
                .context("join injected-file writer")?
        }

        #[cfg(not(target_os = "linux"))]
        {
            for file in files {
                let rel = validate_relative_path(&file.relative_path)?;
                let target = slot.workspace_path.join(&rel);
                if let Some(parent) = target.parent() {
                    prepare_parent_dirs(&slot.workspace_path, &rel, slot.uid, slot.gid).await?;
                    debug_assert!(parent.starts_with(&slot.workspace_path));
                }
                let mut opts = OpenOptions::new();
                opts.write(true).create_new(true);
                #[cfg(unix)]
                {
                    opts.mode(file.mode & 0o777);
                }
                let mut handle = opts
                    .open(&target)
                    .await
                    .with_context(|| format!("create {}", rel.display()))?;
                handle.write_all(&file.content).await?;
                handle.flush().await?;
                chown(&target, slot.uid, slot.gid).await?;
            }
            Ok(())
        }
    }
}

#[cfg(target_os = "linux")]
fn inject_linux(slot: &SlotSpec, files: &[InjectedFile]) -> Result<()> {
    for file in files {
        let rel = validate_relative_path(&file.relative_path)?;
        inject_linux_file(slot, &rel, file).with_context(|| format!("create {}", rel.display()))?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn inject_linux_file(slot: &SlotSpec, rel: &Path, file: &InjectedFile) -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let root = CString::new(slot.workspace_path.as_os_str().as_bytes())?;
    let mut directory = open_directory_path(&root)
        .with_context(|| format!("open workspace {}", slot.workspace_path.display()))?;

    let components = rel.components().collect::<Vec<_>>();
    let (file_component, parent_components) = components
        .split_last()
        .ok_or_else(|| anyhow::anyhow!("injected file path has no filename"))?;
    for component in parent_components {
        let std::path::Component::Normal(name) = component else {
            bail!("injected file path contains a non-normal component");
        };
        let name = CString::new(name.as_bytes())?;
        directory = open_or_create_directory_at(
            std::os::fd::AsRawFd::as_raw_fd(&directory),
            &name,
            slot.uid,
            slot.gid,
        )
        .with_context(|| format!("prepare injected directory {}", name.to_string_lossy()))?;
    }

    let std::path::Component::Normal(file_name) = file_component else {
        bail!("injected file path has no normal filename");
    };
    let file_name = CString::new(file_name.as_bytes())?;
    write_injected_file_at(
        std::os::fd::AsRawFd::as_raw_fd(&directory),
        &file_name,
        file,
        slot.uid,
        slot.gid,
    )
}

#[cfg(target_os = "linux")]
fn open_directory_path(path: &std::ffi::CStr) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;

    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
    }
}

#[cfg(target_os = "linux")]
fn open_directory_at(
    parent: std::os::fd::RawFd,
    name: &std::ffi::CStr,
) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;

    let fd = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
    }
}

#[cfg(target_os = "linux")]
fn open_or_create_directory_at(
    parent: std::os::fd::RawFd,
    name: &std::ffi::CStr,
    uid: u32,
    gid: u32,
) -> Result<std::os::fd::OwnedFd> {
    let directory = match open_directory_at(parent, name) {
        Ok(directory) => directory,
        Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
            let rc = unsafe { libc::mkdirat(parent, name.as_ptr(), 0o700) };
            if rc != 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::EEXIST) {
                    return Err(error.into());
                }
            }
            // Re-open with O_NOFOLLOW after mkdirat. If another process won the
            // race by installing a symlink or non-directory, this fails closed.
            open_directory_at(parent, name)?
        }
        Err(error) => return Err(error.into()),
    };
    set_fd_owner_and_mode(std::os::fd::AsRawFd::as_raw_fd(&directory), uid, gid, 0o700)?;
    Ok(directory)
}

#[cfg(target_os = "linux")]
fn write_injected_file_at(
    parent: std::os::fd::RawFd,
    name: &std::ffi::CStr,
    injected: &InjectedFile,
    uid: u32,
    gid: u32,
) -> Result<()> {
    use std::io::Write;
    use std::os::fd::{AsRawFd, FromRawFd};

    let mode = injected.mode & 0o777;
    let fd = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            mode,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut handle = unsafe { std::fs::File::from_raw_fd(fd) };
    handle.write_all(&injected.content)?;
    handle.flush()?;
    set_fd_owner_and_mode(handle.as_raw_fd(), uid, gid, mode)?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn set_fd_owner_and_mode(fd: std::os::fd::RawFd, uid: u32, gid: u32, mode: u32) -> Result<()> {
    if unsafe { libc::fchown(fd, uid, gid) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    if unsafe { libc::fchmod(fd, mode) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
async fn prepare_parent_dirs(root: &Path, rel: &Path, uid: u32, gid: u32) -> Result<()> {
    let root_metadata = fs::symlink_metadata(root).await?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        bail!("workspace root must be a real directory");
    }

    let mut current = root.to_path_buf();
    if let Some(parent) = rel.parent() {
        for component in parent.components() {
            current.push(component);
            match fs::symlink_metadata(&current).await {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    bail!(
                        "injected file parent must not be a symlink: {}",
                        current.display()
                    );
                }
                Ok(metadata) if !metadata.is_dir() => {
                    bail!(
                        "injected file parent must be a directory: {}",
                        current.display()
                    );
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    fs::create_dir(&current).await?;
                }
                Err(error) => return Err(error.into()),
            }
            chown(&current, uid, gid).await?;
            set_dir_mode(&current, 0o700).await?;
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
async fn set_dir_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).await?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::InjectedFile;

    fn ids() -> (u32, u32) {
        #[cfg(unix)]
        {
            (unsafe { libc::geteuid() }, unsafe { libc::getegid() })
        }
        #[cfg(not(unix))]
        {
            (0, 0)
        }
    }

    fn slot(root: &Path) -> SlotSpec {
        let (uid, gid) = ids();
        SlotSpec {
            id: "slot-test".into(),
            uid,
            gid,
            workspace_path: root.into(),
        }
    }

    fn file(path: &str, content: &[u8], mode: u32) -> InjectedFile {
        InjectedFile {
            relative_path: path.into(),
            content: content.into(),
            mode,
        }
    }

    #[tokio::test]
    async fn injects_nested_binary_and_empty_files_without_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let injector = Injector::new();
        let slot = slot(dir.path());
        injector
            .inject(
                &slot,
                &[
                    file("nested/data.bin", &[0, 1, 0xff], 0o640),
                    file("empty", &[], 0o600),
                ],
            )
            .await
            .unwrap();

        assert_eq!(
            fs::read(dir.path().join("nested/data.bin")).await.unwrap(),
            [0, 1, 0xff]
        );
        assert!(fs::read(dir.path().join("empty")).await.unwrap().is_empty());

        let error = injector
            .inject(&slot, &[file("nested/data.bin", b"replacement", 0o600)])
            .await
            .unwrap_err();
        assert!(error.to_string().contains("create nested/data.bin"));
        assert_eq!(
            fs::read(dir.path().join("nested/data.bin")).await.unwrap(),
            [0, 1, 0xff]
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(dir.path().join("nested"))
                    .await
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(dir.path().join("nested/data.bin"))
                    .await
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o640
            );
        }
    }

    #[tokio::test]
    async fn rejects_invalid_paths_before_writing() {
        let dir = tempfile::tempdir().unwrap();
        let injector = Injector::new();
        let slot = slot(dir.path());

        for path in ["", ".", "../escape", "/absolute", "a\0b"] {
            assert!(
                injector
                    .inject(&slot, &[file(path, b"x", 0o600)])
                    .await
                    .is_err(),
                "unsafe path was accepted: {path:?}"
            );
        }
        assert!(
            fs::read_dir(dir.path())
                .await
                .unwrap()
                .next_entry()
                .await
                .unwrap()
                .is_none()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_parent_symlinks_without_writing_outside_workspace() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), workspace.path().join("link")).unwrap();
        let injector = Injector::new();

        let error = injector
            .inject(
                &slot(workspace.path()),
                &[file("link/escaped", b"secret", 0o600)],
            )
            .await
            .unwrap_err();

        assert!(
            error.to_string().contains("link"),
            "unexpected error: {error:#}"
        );
        assert!(!outside.path().join("escaped").exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn held_directory_fd_cannot_be_redirected_by_a_symlink_swap() {
        use std::ffi::CString;
        use std::os::fd::AsRawFd;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root_name = CString::new(workspace.path().as_os_str().as_bytes()).unwrap();
        let root_fd = open_directory_path(&root_name).unwrap();
        let parent_name = CString::new("parent").unwrap();
        let (uid, gid) = ids();
        let parent_fd =
            open_or_create_directory_at(root_fd.as_raw_fd(), &parent_name, uid, gid).unwrap();

        std::fs::rename(
            workspace.path().join("parent"),
            workspace.path().join("moved-parent"),
        )
        .unwrap();
        symlink(outside.path(), workspace.path().join("parent")).unwrap();

        let injected = file("parent/child", b"safe", 0o600);
        write_injected_file_at(
            parent_fd.as_raw_fd(),
            &CString::new("child").unwrap(),
            &injected,
            uid,
            gid,
        )
        .unwrap();

        assert_eq!(
            std::fs::read(workspace.path().join("moved-parent/child")).unwrap(),
            b"safe"
        );
        assert!(!outside.path().join("child").exists());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn rejects_a_symlink_at_the_final_file_component() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("target");
        fs::write(&outside_file, b"original").await.unwrap();
        symlink(&outside_file, workspace.path().join("target")).unwrap();

        let error = Injector::new()
            .inject(
                &slot(workspace.path()),
                &[file("target", b"replacement", 0o600)],
            )
            .await
            .unwrap_err();

        assert!(error.to_string().contains("create target"));
        assert_eq!(fs::read(outside_file).await.unwrap(), b"original");
    }

    #[tokio::test]
    async fn file_directory_collisions_preserve_existing_data() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("parent"), b"original")
            .await
            .unwrap();
        let injector = Injector::new();

        assert!(
            injector
                .inject(&slot(dir.path()), &[file("parent/child", b"x", 0o600)])
                .await
                .is_err()
        );
        assert_eq!(
            fs::read(dir.path().join("parent")).await.unwrap(),
            b"original"
        );
    }
}
