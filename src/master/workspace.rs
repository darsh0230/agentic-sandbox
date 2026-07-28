use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
use anyhow::Context;
use anyhow::Result;
use tokio::fs;

use crate::sandbox::{IsolationMode, SlotSpec};

pub struct WorkspaceManager {
    root: PathBuf,
}

impl WorkspaceManager {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub async fn prepare_root(root: &Path, isolation_mode: IsolationMode) -> Result<()> {
        for dir in [
            "state",
            "workspaces",
            "roots",
            "injected",
            "results",
            "tombstones",
        ] {
            fs::create_dir_all(root.join(dir)).await?;
        }
        let root_mode = match isolation_mode {
            IsolationMode::Privileged => 0o700,
            IsolationMode::User => 0o711,
        };
        set_mode(root, root_mode).await?;
        set_mode(&root.join("workspaces"), 0o711).await?;
        Ok(())
    }

    pub async fn reset_workspace(&self, slot: &SlotSpec) -> Result<()> {
        let tombstone =
            self.root
                .join("tombstones")
                .join(format!("{}-{}", slot.id, uuid::Uuid::new_v4()));
        if fs::try_exists(&slot.workspace_path).await? {
            fs::rename(&slot.workspace_path, &tombstone).await?;
        }
        fs::create_dir_all(&slot.workspace_path).await?;
        for dir in [
            slot.workspace_path.clone(),
            slot.workspace_path.join("tmp"),
            slot.workspace_path.join(".cache"),
            slot.workspace_path.join(".config"),
            slot.workspace_path.join(".local"),
            slot.workspace_path.join(".local/share"),
        ] {
            fs::create_dir_all(&dir).await?;
            set_mode(&dir, 0o700).await?;
        }
        // Keep the new tree root-owned until its complete layout exists, then
        // transfer it through directory handles. This prevents another process
        // with the slot UID from swapping a partially initialized component.
        chown_recursive(&slot.workspace_path, slot.uid, slot.gid).await?;
        if fs::try_exists(&tombstone).await? {
            fs::remove_dir_all(tombstone).await?;
        }
        Ok(())
    }

    pub async fn chown_workspace(&self, slot: &SlotSpec) -> Result<()> {
        chown_recursive(&slot.workspace_path, slot.uid, slot.gid).await
    }

    pub async fn cleanup_rootfs(&self, run_id: &str) -> Result<()> {
        crate::common::paths::validate_run_id(run_id)?;
        let rootfs = self.root.join("roots").join(run_id);
        if fs::try_exists(&rootfs).await? {
            fs::remove_dir_all(rootfs).await?;
        }
        Ok(())
    }
}

async fn set_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).await?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

pub async fn chown(path: &Path, uid: u32, gid: u32) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let c_path = CString::new(path.as_os_str().as_bytes())?;
        let rc = unsafe { libc::lchown(c_path.as_ptr(), uid, gid) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (path, uid, gid);
    Ok(())
}

async fn chown_recursive(root: &Path, uid: u32, gid: u32) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let root = root.to_path_buf();
        tokio::task::spawn_blocking(move || chown_recursive_linux(&root, uid, gid))
            .await
            .context("join secure workspace ownership traversal")?
    }

    #[cfg(not(target_os = "linux"))]
    {
        let mut stack = vec![root.to_path_buf()];
        while let Some(path) = stack.pop() {
            chown(&path, uid, gid).await?;
            let metadata = fs::symlink_metadata(&path).await?;
            if metadata.is_dir() {
                let mut entries = fs::read_dir(&path).await?;
                while let Some(entry) = entries.next_entry().await? {
                    stack.push(entry.path());
                }
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn chown_recursive_linux(root: &Path, uid: u32, gid: u32) -> Result<()> {
    use std::ffi::CString;
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;

    let root = CString::new(root.as_os_str().as_bytes())?;
    let fd = unsafe {
        libc::open(
            root.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    chown_directory_fd(unsafe { OwnedFd::from_raw_fd(fd) }, uid, gid)
}

#[cfg(target_os = "linux")]
fn chown_directory_fd(directory: std::os::fd::OwnedFd, uid: u32, gid: u32) -> Result<()> {
    use std::ffi::{CStr, CString};
    use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};

    struct DirectoryStream(*mut libc::DIR);

    impl DirectoryStream {
        fn fd(&self) -> std::os::fd::RawFd {
            unsafe { libc::dirfd(self.0) }
        }
    }

    impl Drop for DirectoryStream {
        fn drop(&mut self) {
            unsafe {
                libc::closedir(self.0);
            }
        }
    }

    let raw_fd = directory.into_raw_fd();
    let stream = unsafe { libc::fdopendir(raw_fd) };
    if stream.is_null() {
        let error = std::io::Error::last_os_error();
        drop(unsafe { OwnedFd::from_raw_fd(raw_fd) });
        return Err(error.into());
    }
    let stream = DirectoryStream(stream);

    loop {
        unsafe {
            *libc::__errno_location() = 0;
        }
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            let errno = unsafe { *libc::__errno_location() };
            if errno != 0 {
                return Err(std::io::Error::from_raw_os_error(errno).into());
            }
            break;
        }

        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if matches!(name.to_bytes(), b"." | b"..") {
            continue;
        }
        let name = CString::new(name.to_bytes())?;
        let parent_fd = stream.fd();
        let child_fd = unsafe {
            libc::openat(
                parent_fd,
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if child_fd >= 0 {
            chown_directory_fd(unsafe { OwnedFd::from_raw_fd(child_fd) }, uid, gid)?;
            continue;
        }

        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ENOENT) => continue,
            Some(libc::ENOTDIR | libc::ELOOP) => {}
            _ => return Err(error.into()),
        }

        if unsafe {
            libc::fchownat(
                parent_fd,
                name.as_ptr(),
                uid,
                gid,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENOENT) {
                continue;
            }
            return Err(error.into());
        }
    }

    // Transfer each directory only after its descendants. Until the root is
    // transferred, the slot UID cannot reach or mutate any completed subtree,
    // so a same-UID process cannot redirect the traversal between entries.
    if unsafe { libc::fchown(stream.fd(), uid, gid) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn slot(path: PathBuf) -> SlotSpec {
        let (uid, gid) = ids();
        SlotSpec {
            id: "slot-0001".into(),
            uid,
            gid,
            workspace_path: path,
        }
    }

    #[tokio::test]
    async fn prepare_root_creates_the_complete_layout() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("state-root");
        WorkspaceManager::prepare_root(&root, IsolationMode::User)
            .await
            .unwrap();

        for name in [
            "state",
            "workspaces",
            "roots",
            "injected",
            "results",
            "tombstones",
        ] {
            assert!(root.join(name).is_dir(), "missing {name}");
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&root).await.unwrap().permissions().mode() & 0o777,
                0o711
            );
            assert_eq!(
                fs::metadata(root.join("workspaces"))
                    .await
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o711
            );
        }
    }

    #[tokio::test]
    async fn reset_workspace_removes_stale_content_and_is_idempotent() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("root");
        WorkspaceManager::prepare_root(&root, IsolationMode::User)
            .await
            .unwrap();
        let manager = WorkspaceManager::new(root.clone());
        let slot = slot(root.join("workspaces/slot-0001"));

        manager.reset_workspace(&slot).await.unwrap();
        fs::write(slot.workspace_path.join("stale"), b"old")
            .await
            .unwrap();
        fs::create_dir_all(slot.workspace_path.join("old/tree"))
            .await
            .unwrap();
        manager.reset_workspace(&slot).await.unwrap();
        manager.reset_workspace(&slot).await.unwrap();

        assert!(!slot.workspace_path.join("stale").exists());
        assert!(!slot.workspace_path.join("old").exists());
        for relative in ["", "tmp", ".cache", ".config", ".local", ".local/share"] {
            assert!(
                slot.workspace_path.join(relative).is_dir(),
                "missing workspace directory {relative:?}"
            );
        }
        assert!(
            fs::read_dir(root.join("tombstones"))
                .await
                .unwrap()
                .next_entry()
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn cleanup_rootfs_is_scoped_validated_and_idempotent() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("root");
        WorkspaceManager::prepare_root(&root, IsolationMode::User)
            .await
            .unwrap();
        let manager = WorkspaceManager::new(root.clone());
        fs::create_dir_all(root.join("roots/run-1/nested"))
            .await
            .unwrap();
        fs::write(root.join("roots/keep"), b"keep").await.unwrap();

        manager.cleanup_rootfs("run-1").await.unwrap();
        manager.cleanup_rootfs("run-1").await.unwrap();
        assert!(!root.join("roots/run-1").exists());
        assert_eq!(fs::read(root.join("roots/keep")).await.unwrap(), b"keep");

        for unsafe_id in ["", ".", "..", "../root", "nested/run"] {
            assert!(
                manager.cleanup_rootfs(unsafe_id).await.is_err(),
                "unsafe cleanup identifier was accepted: {unsafe_id:?}"
            );
            assert_eq!(fs::read(root.join("roots/keep")).await.unwrap(), b"keep");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn recursive_chown_does_not_follow_directory_symlinks() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("sentinel"), b"outside")
            .await
            .unwrap();
        symlink(outside.path(), root.path().join("link")).unwrap();
        let (uid, gid) = ids();

        chown_recursive(root.path(), uid, gid).await.unwrap();

        assert_eq!(
            fs::read(outside.path().join("sentinel")).await.unwrap(),
            b"outside"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn held_directory_traversal_cannot_be_redirected_after_a_path_swap() {
        use std::os::fd::{FromRawFd, OwnedFd};
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::{MetadataExt, symlink};

        let parent = tempfile::tempdir().unwrap();
        let workspace = parent.path().join("workspace");
        let moved = parent.path().join("moved-workspace");
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(workspace.join("nested")).unwrap();
        std::fs::write(workspace.join("nested/artifact"), b"inside").unwrap();
        std::fs::write(outside.path().join("sentinel"), b"outside").unwrap();

        let workspace_c = std::ffi::CString::new(workspace.as_os_str().as_bytes()).unwrap();
        let fd = unsafe {
            libc::open(
                workspace_c.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        assert!(fd >= 0, "{}", std::io::Error::last_os_error());
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };

        std::fs::rename(&workspace, &moved).unwrap();
        symlink(outside.path(), &workspace).unwrap();

        let current_uid = unsafe { libc::geteuid() };
        let current_gid = unsafe { libc::getegid() };
        let (target_uid, target_gid) = if current_uid == 0 {
            (12_345, 12_345)
        } else {
            (current_uid, current_gid)
        };
        let outside_uid = std::fs::metadata(outside.path().join("sentinel"))
            .unwrap()
            .uid();

        chown_directory_fd(fd, target_uid, target_gid).unwrap();

        assert_eq!(
            std::fs::read(moved.join("nested/artifact")).unwrap(),
            b"inside"
        );
        assert_eq!(
            std::fs::read(outside.path().join("sentinel")).unwrap(),
            b"outside"
        );
        assert_eq!(
            std::fs::metadata(outside.path().join("sentinel"))
                .unwrap()
                .uid(),
            outside_uid,
            "the swapped-in outside tree must never be traversed"
        );
        if current_uid == 0 {
            assert_eq!(
                std::fs::metadata(moved.join("nested/artifact"))
                    .unwrap()
                    .uid(),
                target_uid
            );
        }
    }
}
