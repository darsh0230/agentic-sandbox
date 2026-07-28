#[cfg(target_os = "linux")]
use anyhow::Context;
use anyhow::{Result, bail};
use serde_json::Value;
use tokio::fs;
use tokio::io::AsyncReadExt;

use crate::common::paths::validate_relative_path;
use crate::sandbox::SlotSpec;
use crate::types::{CollectedFile, ResultPolicy};

pub struct ResultCollector;

pub const MAX_COLLECT_PATHS: usize = 256;
pub const MAX_COLLECTED_FILE_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_COLLECTED_TOTAL_BYTES: u64 = 64 * 1024 * 1024;

impl Default for ResultCollector {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
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
        if policy.collect_paths.len() > MAX_COLLECT_PATHS {
            bail!("too many result collection paths: max {MAX_COLLECT_PATHS}");
        }
        let mut files = Vec::new();
        let mut total_size = 0_u64;
        let workspace = fs::canonicalize(&slot.workspace_path).await?;
        for path in &policy.collect_paths {
            let rel = validate_relative_path(path)?;
            let full = slot.workspace_path.join(&rel);
            let remaining = MAX_COLLECTED_TOTAL_BYTES.saturating_sub(total_size);
            let max_bytes = remaining.min(MAX_COLLECTED_FILE_BYTES);
            if let Some(content) =
                read_regular_file_inside_workspace(&workspace, &rel, &full, max_bytes).await?
            {
                total_size = total_size
                    .checked_add(content.len() as u64)
                    .ok_or_else(|| anyhow::anyhow!("collected result size overflow"))?;
                files.push(CollectedFile {
                    relative_path: path.clone(),
                    size: content.len() as u64,
                    content,
                });
            }
        }

        let result_json = if let Some(path) = &policy.result_file {
            let rel = validate_relative_path(path)?;
            let full = slot.workspace_path.join(&rel);
            read_regular_file_inside_workspace(&workspace, &rel, &full, MAX_COLLECTED_FILE_BYTES)
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
    relative_path: &std::path::Path,
    display_path: &std::path::Path,
    max_bytes: u64,
) -> Result<Option<Vec<u8>>> {
    #[cfg(target_os = "linux")]
    let file = {
        let workspace = workspace.to_path_buf();
        let relative_path = relative_path.to_path_buf();
        let file = tokio::task::spawn_blocking(move || {
            open_file_beneath_workspace(&workspace, &relative_path)
        })
        .await
        .context("join secure result-file opener")??;
        file.map(fs::File::from_std)
    };

    #[cfg(not(target_os = "linux"))]
    let file = {
        let path = workspace.join(relative_path);
        let resolved = match fs::canonicalize(&path).await {
            Ok(resolved) => resolved,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if !resolved.starts_with(workspace) {
            return Ok(None);
        }

        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        match options.open(&resolved).await {
            Ok(file) => Some(file),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        }
    };

    let Some(file) = file else {
        return Ok(None);
    };
    read_bounded_regular_file(file, display_path, max_bytes).await
}

#[cfg(target_os = "linux")]
fn open_file_beneath_workspace(
    workspace: &std::path::Path,
    relative_path: &std::path::Path,
) -> Result<Option<std::fs::File>> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;

    #[repr(C)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }

    const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
    const RESOLVE_BENEATH: u64 = 0x08;

    let workspace = CString::new(workspace.as_os_str().as_bytes())?;
    let root_fd = unsafe {
        libc::open(
            workspace.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if root_fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let root_fd = unsafe { OwnedFd::from_raw_fd(root_fd) };
    let relative_path = CString::new(relative_path.as_os_str().as_bytes())?;
    let how = OpenHow {
        flags: (libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NONBLOCK) as u64,
        mode: 0,
        resolve: RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS,
    };

    for _ in 0..3 {
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                root_fd.as_raw_fd(),
                relative_path.as_ptr(),
                &how,
                std::mem::size_of::<OpenHow>(),
            )
        };
        if fd >= 0 {
            return Ok(Some(unsafe { std::fs::File::from_raw_fd(fd as i32) }));
        }

        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ENOENT | libc::EXDEV) => return Ok(None),
            Some(libc::EAGAIN) => continue,
            Some(libc::ENOSYS) => {
                bail!("secure result collection requires Linux openat2 support")
            }
            _ => return Err(error.into()),
        }
    }
    bail!("result path changed repeatedly during secure resolution")
}

async fn read_bounded_regular_file(
    file: fs::File,
    display_path: &std::path::Path,
    max_bytes: u64,
) -> Result<Option<Vec<u8>>> {
    // Inspect and read the same open handle. Checking a path and reopening it
    // would let a concurrent sandbox process replace or grow the artifact
    // between the size check and the actual read.
    let metadata = file.metadata().await?;
    if !metadata.is_file() {
        return Ok(None);
    }
    if metadata.len() > max_bytes {
        bail!(
            "result file {} is {} bytes; limit is {} bytes",
            display_path.display(),
            metadata.len(),
            max_bytes
        );
    }

    let capacity = usize::try_from(metadata.len().min(max_bytes)).unwrap_or(usize::MAX);
    let mut content = Vec::with_capacity(capacity);
    let mut limited = file.take(max_bytes.saturating_add(1));
    limited.read_to_end(&mut content).await?;
    if content.len() as u64 > max_bytes {
        bail!(
            "result file {} exceeded the {} byte limit while being read",
            display_path.display(),
            max_bytes
        );
    }
    Ok(Some(content))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ResultPolicy;

    fn slot(root: &std::path::Path) -> SlotSpec {
        SlotSpec {
            id: "slot-test".into(),
            uid: 0,
            gid: 0,
            workspace_path: root.into(),
        }
    }

    #[tokio::test]
    async fn collects_regular_files_in_policy_order_and_parses_result_json() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.bin"), [0_u8, 0xff, 1])
            .await
            .unwrap();
        fs::create_dir(dir.path().join("nested")).await.unwrap();
        fs::write(dir.path().join("nested/b.txt"), b"second")
            .await
            .unwrap();
        fs::write(dir.path().join("result.json"), br#"{"ok":true,"n":2}"#)
            .await
            .unwrap();

        let collected = ResultCollector::new()
            .collect(
                &slot(dir.path()),
                &ResultPolicy {
                    collect_paths: vec!["nested/b.txt".into(), "a.bin".into()],
                    result_file: Some("result.json".into()),
                },
            )
            .await
            .unwrap();

        assert_eq!(
            collected
                .files
                .iter()
                .map(|file| file.relative_path.as_str())
                .collect::<Vec<_>>(),
            ["nested/b.txt", "a.bin"]
        );
        assert_eq!(collected.files[0].content, b"second");
        assert_eq!(collected.files[0].size, 6);
        assert_eq!(collected.files[1].content, [0, 0xff, 1]);
        assert_eq!(
            collected.result_json,
            Some(serde_json::json!({"ok": true, "n": 2}))
        );
    }

    #[tokio::test]
    async fn omits_missing_directories_and_invalid_or_disabled_result_json() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("directory")).await.unwrap();
        fs::write(dir.path().join("invalid.json"), b"not-json")
            .await
            .unwrap();
        let collector = ResultCollector::new();

        let collected = collector
            .collect(
                &slot(dir.path()),
                &ResultPolicy {
                    collect_paths: vec!["missing".into(), "directory".into()],
                    result_file: Some("invalid.json".into()),
                },
            )
            .await
            .unwrap();
        assert!(collected.files.is_empty());
        assert_eq!(collected.result_json, None);

        let disabled = collector
            .collect(
                &slot(dir.path()),
                &ResultPolicy {
                    collect_paths: vec![],
                    result_file: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(disabled.result_json, None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fifo_collection_is_nonblocking_and_omitted() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("artifact.fifo");
        let fifo_path = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) }, 0);

        let collected = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            ResultCollector::new().collect(
                &slot(dir.path()),
                &ResultPolicy {
                    collect_paths: vec!["artifact.fifo".into()],
                    result_file: None,
                },
            ),
        )
        .await
        .expect("opening a FIFO for collection must never wait for a writer")
        .unwrap();

        assert!(collected.files.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn allows_internal_symlinks_but_rejects_workspace_escape() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("inside"), b"inside")
            .await
            .unwrap();
        fs::write(outside.path().join("secret"), b"outside")
            .await
            .unwrap();
        symlink("inside", workspace.path().join("inside-link")).unwrap();
        symlink(
            outside.path().join("secret"),
            workspace.path().join("outside-link"),
        )
        .unwrap();
        symlink("missing", workspace.path().join("dangling")).unwrap();

        let collected = ResultCollector::new()
            .collect(
                &slot(workspace.path()),
                &ResultPolicy {
                    collect_paths: vec![
                        "inside-link".into(),
                        "outside-link".into(),
                        "dangling".into(),
                    ],
                    result_file: None,
                },
            )
            .await
            .unwrap();

        assert_eq!(collected.files.len(), 1);
        assert_eq!(collected.files[0].relative_path, "inside-link");
        assert_eq!(collected.files[0].content, b"inside");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn secure_opener_rejects_an_outside_ancestor_symlink() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), b"outside").unwrap();
        symlink(outside.path(), workspace.path().join("swapped")).unwrap();

        let opened =
            open_file_beneath_workspace(workspace.path(), std::path::Path::new("swapped/secret"))
                .unwrap();
        assert!(
            opened.is_none(),
            "RESOLVE_BENEATH must reject an ancestor that points outside"
        );
    }

    #[tokio::test]
    async fn rejects_files_over_the_per_file_limit_before_reading() {
        let dir = tempfile::tempdir().unwrap();
        let file = fs::File::create(dir.path().join("large")).await.unwrap();
        file.set_len(MAX_COLLECTED_FILE_BYTES + 1).await.unwrap();

        let error = ResultCollector::new()
            .collect(
                &slot(dir.path()),
                &ResultPolicy {
                    collect_paths: vec!["large".into()],
                    result_file: None,
                },
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("limit is 16777216 bytes"));
    }

    #[tokio::test]
    async fn bounded_reader_uses_the_original_open_handle_after_path_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("artifact");
        fs::write(&path, b"original").await.unwrap();
        let file = fs::OpenOptions::new().read(true).open(&path).await.unwrap();

        fs::rename(&path, dir.path().join("old-artifact"))
            .await
            .unwrap();
        fs::write(&path, b"replacement").await.unwrap();

        let content = read_bounded_regular_file(file, &path, 8)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(content, b"original");
    }

    #[tokio::test]
    async fn bounded_reader_rejects_content_larger_than_its_read_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("artifact");
        fs::write(&path, b"12345").await.unwrap();
        let file = fs::OpenOptions::new().read(true).open(&path).await.unwrap();

        let error = read_bounded_regular_file(file, &path, 4).await.unwrap_err();
        assert!(error.to_string().contains("limit is 4 bytes"));
    }

    #[tokio::test]
    async fn rejects_excessive_collection_path_counts() {
        let dir = tempfile::tempdir().unwrap();
        let error = ResultCollector::new()
            .collect(
                &slot(dir.path()),
                &ResultPolicy {
                    collect_paths: vec!["missing".into(); MAX_COLLECT_PATHS + 1],
                    result_file: None,
                },
            )
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "too many result collection paths: max 256"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn propagates_symlink_loop_errors() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        symlink("loop", dir.path().join("loop")).unwrap();
        let error = ResultCollector::new()
            .collect(
                &slot(dir.path()),
                &ResultPolicy {
                    collect_paths: vec!["loop".into()],
                    result_file: None,
                },
            )
            .await
            .unwrap_err();
        let io_error = error
            .downcast_ref::<std::io::Error>()
            .expect("symlink loop should remain an I/O error");
        assert_eq!(io_error.raw_os_error(), Some(libc::ELOOP), "{error:#}");
    }
}
