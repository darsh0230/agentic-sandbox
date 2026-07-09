use anyhow::Result;
use serde_json::Value;
use tokio::fs;

use crate::common::paths::validate_relative_path;
use crate::sandbox::SlotSpec;
use crate::types::{CollectedFile, ResultPolicy};

pub struct ResultCollector;

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
        for path in &policy.collect_paths {
            let rel = validate_relative_path(path)?;
            let full = slot.workspace_path.join(&rel);
            if let Ok(metadata) = fs::symlink_metadata(&full).await {
                if metadata.is_file() {
                    let content = fs::read(&full).await?;
                    files.push(CollectedFile {
                        relative_path: path.clone(),
                        size: content.len() as u64,
                        content,
                    });
                }
            }
        }

        let result_json = if let Some(path) = &policy.result_file {
            let rel = validate_relative_path(path)?;
            let full = slot.workspace_path.join(rel);
            match fs::read(&full).await {
                Ok(bytes) => serde_json::from_slice(&bytes).ok(),
                Err(_) => None,
            }
        } else {
            None
        };

        Ok(CollectedResults { files, result_json })
    }
}
