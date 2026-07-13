use std::path::{Path, PathBuf};

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
            chown(&dir, slot.uid, slot.gid).await?;
        }
        if fs::try_exists(&tombstone).await? {
            fs::remove_dir_all(tombstone).await?;
        }
        Ok(())
    }

    pub async fn chown_workspace(&self, slot: &SlotSpec) -> Result<()> {
        chown_recursive(&slot.workspace_path, slot.uid, slot.gid).await
    }

    pub async fn cleanup_rootfs(&self, run_id: &str) -> Result<()> {
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
