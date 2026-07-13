use anyhow::Result;
use serde_json::Value;
use tokio::fs;

use crate::common::paths::validate_relative_path;
use crate::sandbox::SlotSpec;
use crate::types::{CollectedFile, ResultPolicy};

pub struct ResultCollector;

impl Default for ResultCollector {
    fn default() -> Self {
        Self::new()
    }
}

pub struct CollectedResults {
    pub files: Vec<CollectedFile>,
    pub result_json: Option<Value>,
}

impl ResultCollector {
    pub fn new() -> Self {
        Self
    }

    pub async fn collect(
        &self,
        slot: &SlotSpec,
        policy: &ResultPolicy,
    ) -> Result<CollectedResults> {
        let mut files = Vec::new();
        let workspace = fs::canonicalize(&slot.workspace_path).await?;
        for path in &policy.collect_paths {
            let rel = validate_relative_path(path)?;
            let full = slot.workspace_path.join(&rel);
            if let Some(content) = read_regular_file_inside_workspace(&workspace, &full).await? {
                files.push(CollectedFile {
                    relative_path: path.clone(),
                    size: content.len() as u64,
                    content,
                });
            }
        }

        let result_json = if let Some(path) = &policy.result_file {
            let rel = validate_relative_path(path)?;
            let full = slot.workspace_path.join(rel);
            read_regular_file_inside_workspace(&workspace, &full)
                .await?
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        } else {
            None
        };

        Ok(CollectedResults { files, result_json })
    }
}

async fn read_regular_file_inside_workspace(
    workspace: &std::path::Path,
    path: &std::path::Path,
) -> Result<Option<Vec<u8>>> {
    let Ok(resolved) = fs::canonicalize(path).await else {
        return Ok(None);
    };
    if !resolved.starts_with(workspace) {
        return Ok(None);
    }
    let metadata = fs::metadata(&resolved).await?;
    if !metadata.is_file() {
        return Ok(None);
    }
    Ok(Some(fs::read(resolved).await?))
}
