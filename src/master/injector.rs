use anyhow::{Context, Result};
use std::path::Path;

use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;

use crate::common::paths::validate_relative_path;
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
        for file in files {
            let rel = validate_relative_path(&file.relative_path)?;
            let target = slot.workspace_path.join(&rel);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).await?;
                chown_relative_dirs(&slot.workspace_path, &rel, slot.uid, slot.gid).await?;
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

async fn chown_relative_dirs(root: &Path, rel: &Path, uid: u32, gid: u32) -> Result<()> {
    let mut current = root.to_path_buf();
    if let Some(parent) = rel.parent() {
        for component in parent.components() {
            current.push(component);
            chown(&current, uid, gid).await?;
            set_dir_mode(&current, 0o700).await?;
        }
    }
    Ok(())
}

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
