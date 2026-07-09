use anyhow::{Context, Result};
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;

use crate::common::paths::validate_relative_path;
use crate::master::workspace::chown;
use crate::sandbox::SlotSpec;
use crate::types::InjectedFile;

pub struct Injector;

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
