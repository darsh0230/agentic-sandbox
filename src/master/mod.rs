pub mod allocator;
pub mod cleanup;
pub mod injector;
pub mod result_collector;
pub mod supervisor;
pub mod workspace;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use tokio::sync::{Mutex, mpsc};
use tracing::{info, warn};

use crate::api::events::TailBuffer;
use crate::policy::defaults::clean_env_for_slot;
use crate::sandbox::{IsolationMode, OutputStream, SandboxLaunchConfig, SandboxLauncher, SlotSpec};
use crate::types::{AgentRunEvent, AgentRunRequest, AgentRunResult, CleanupPolicy};

use self::allocator::SlotAllocator;
use self::injector::Injector;
use self::result_collector::ResultCollector;
use self::workspace::WorkspaceManager;

#[derive(Clone)]
pub struct Master {
    root: PathBuf,
    allocator: Arc<SlotAllocator>,
    workspace: Arc<WorkspaceManager>,
    injector: Arc<Injector>,
    collector: Arc<ResultCollector>,
    isolation_mode: IsolationMode,
    launcher_lock: Arc<Mutex<()>>,
}

impl Master {
    pub async fn new(
        root: PathBuf,
        slot_count: u16,
        isolation_mode: IsolationMode,
    ) -> Result<Self> {
        WorkspaceManager::prepare_root(&root, isolation_mode).await?;
        let planned_slots = SlotAllocator::planned_slots(&root.join("workspaces"), slot_count)?;
        if isolation_mode == IsolationMode::User {
            validate_user_mode_slots(&planned_slots)?;
        }
        info!(%isolation_mode, slots = slot_count, "sandbox isolation mode configured");
        Ok(Self {
            allocator: Arc::new(SlotAllocator::new(root.join("workspaces"), slot_count)?),
            workspace: Arc::new(WorkspaceManager::new(root.clone())),
            injector: Arc::new(Injector::new()),
            collector: Arc::new(ResultCollector::new()),
            isolation_mode,
            root,
            launcher_lock: Arc::new(Mutex::new(())),
        })
    }

    pub async fn run_agent(
        &self,
        req: AgentRunRequest,
        events: Option<mpsc::Sender<AgentRunEvent>>,
    ) -> Result<AgentRunResult> {
        validate_request(&req, self.isolation_mode)?;
        let slot = self.allocator.allocate().await?;
        let slot_spec = SlotSpec::from(&slot);
        info!(run_id = %req.run_id, slot = %slot.id, isolation_mode = %self.isolation_mode, "allocated sandbox slot");

        let outcome = self.run_agent_in_slot(req, slot_spec, events).await;
        self.allocator.release(slot).await;
        outcome
    }

    async fn run_agent_in_slot(
        &self,
        req: AgentRunRequest,
        slot: SlotSpec,
        events: Option<mpsc::Sender<AgentRunEvent>>,
    ) -> Result<AgentRunResult> {
        self.workspace
            .reset_workspace(&slot)
            .await
            .context("reset workspace")?;
        self.injector
            .inject(&slot, &req.injected_files)
            .await
            .context("inject files")?;
        self.workspace
            .chown_workspace(&slot)
            .await
            .context("chown workspace")?;

        let env = clean_env_for_slot(&req, &slot, self.isolation_mode);
        let config = SandboxLaunchConfig {
            run_id: req.run_id.clone(),
            root: self.root.clone(),
            slot: slot.clone(),
            isolation_mode: self.isolation_mode,
            command: req.command.clone(),
            env,
            injected_readonly_dirs: req.injected_readonly_dirs.clone(),
        };

        let _guard = self.launcher_lock.lock().await;
        let mut child = SandboxLauncher::spawn(config).context("spawn sandbox")?;
        drop(_guard);

        if let Some(tx) = &events {
            let _ = tx
                .send(AgentRunEvent::Started {
                    run_id: req.run_id.clone(),
                    slot_id: slot.id.clone(),
                    pid: child.pid(),
                })
                .await;
        }

        let stdout_max = req
            .limits
            .stdout_max_bytes
            .unwrap_or(128 * 1024)
            .min(8 * 1024 * 1024) as usize;
        let stderr_max = req
            .limits
            .stderr_max_bytes
            .unwrap_or(128 * 1024)
            .min(8 * 1024 * 1024) as usize;
        let mut stdout_tail = TailBuffer::new(stdout_max);
        let mut stderr_tail = TailBuffer::new(stderr_max);

        let timed_out = tokio::time::timeout(std::time::Duration::from_millis(req.timeout_ms), async {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(25));
            loop {
                tokio::select! {
                    chunk = child.next_output() => {
                        if let Some(chunk) = chunk {
                            match chunk.stream {
                                OutputStream::Stdout => {
                                    stdout_tail.push(&chunk.data);
                                    if let Some(tx) = &events { let _ = tx.send(AgentRunEvent::Stdout { data: chunk.data }).await; }
                                }
                                OutputStream::Stderr => {
                                    stderr_tail.push(&chunk.data);
                                    if let Some(tx) = &events { let _ = tx.send(AgentRunEvent::Stderr { data: chunk.data }).await; }
                                }
                            }
                        }
                    }
                    _ = interval.tick() => {
                        if child.try_wait().await?.is_some() {
                            while let Ok(chunk) = tokio::time::timeout(std::time::Duration::from_millis(25), child.next_output()).await {
                                let Some(chunk) = chunk else { break; };
                                match chunk.stream {
                                    OutputStream::Stdout => { stdout_tail.push(&chunk.data); if let Some(tx) = &events { let _ = tx.send(AgentRunEvent::Stdout { data: chunk.data }).await; } }
                                    OutputStream::Stderr => { stderr_tail.push(&chunk.data); if let Some(tx) = &events { let _ = tx.send(AgentRunEvent::Stderr { data: chunk.data }).await; } }
                                }
                            }
                            break;
                        }
                    }
                }
            }
            anyhow::Ok(())
        }).await.is_err();

        if timed_out {
            warn!(run_id = %req.run_id, "sandbox timed out; killing process tree");
            let _ = child.kill().await;
            if let Some(tx) = &events {
                let _ = tx.send(AgentRunEvent::TimedOut).await;
            }
        }

        let exit = child.wait().await?;
        if let Some(tx) = &events {
            let _ = tx
                .send(AgentRunEvent::Exited {
                    exit_code: exit.code,
                    signal: exit.signal,
                })
                .await;
        }

        let collected = self.collector.collect(&slot, &req.result_policy).await?;
        if let Some(tx) = &events {
            for file in &collected.files {
                let _ = tx
                    .send(AgentRunEvent::ResultFileFound {
                        relative_path: file.relative_path.clone(),
                        size: file.size,
                    })
                    .await;
            }
        }

        let preserve = matches!(req.cleanup_policy, CleanupPolicy::PreserveWorkspaceForDebug)
            || (matches!(
                req.cleanup_policy,
                CleanupPolicy::PreserveWorkspaceOnFailure
            ) && (timed_out || exit.code != Some(0)));
        if !preserve {
            self.workspace
                .reset_workspace(&slot)
                .await
                .context("cleanup workspace")?;
            self.workspace.cleanup_rootfs(&req.run_id).await.ok();
        }

        Ok(AgentRunResult {
            run_id: req.run_id,
            slot_id: slot.id,
            exit_code: exit.code,
            signal: exit.signal,
            stdout_tail: stdout_tail.into_vec(),
            stderr_tail: stderr_tail.into_vec(),
            collected_files: collected.files,
            result_json: collected.result_json,
            workspace_preserved: preserve,
        })
    }
}

fn validate_request(req: &AgentRunRequest, isolation_mode: IsolationMode) -> Result<()> {
    if req.run_id.is_empty()
        || req.run_id.as_bytes().contains(&0)
        || req.run_id.contains('/')
        || req.run_id.contains("..")
    {
        bail!("run_id must be a non-empty path-safe identifier");
    }
    if req.command.is_empty()
        || req
            .command
            .iter()
            .any(|part| part.is_empty() || part.as_bytes().contains(&0))
    {
        bail!("command must contain non-empty strings without NUL bytes");
    }
    for file in &req.injected_files {
        crate::common::paths::validate_relative_path(&file.relative_path)?;
        if file.mode & 0o7777 != file.mode {
            bail!("invalid mode for {}", file.relative_path);
        }
    }
    for dir in &req.injected_readonly_dirs {
        crate::common::paths::validate_sandbox_absolute_path(&dir.sandbox_path)?;
        if !dir.readonly {
            bail!("injected directories must be readonly in this implementation");
        }
    }
    if isolation_mode == IsolationMode::User && !req.injected_readonly_dirs.is_empty() {
        bail!("readonly injected directories require privileged isolation mode");
    }
    Ok(())
}

fn validate_user_mode_slots(slots: &[self::allocator::Slot]) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        for slot in slots {
            if unsafe { libc::getpwuid(slot.uid) }.is_null() {
                bail!(
                    "user isolation mode requires pre-created uid {} for {}",
                    slot.uid,
                    slot.id
                );
            }
            if unsafe { libc::getgrgid(slot.gid) }.is_null() {
                bail!(
                    "user isolation mode requires pre-created gid {} for {}",
                    slot.gid,
                    slot.id
                );
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = slots;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AgentRunRequest, InjectedDir};

    fn request() -> AgentRunRequest {
        AgentRunRequest {
            run_id: "r".into(),
            command: vec!["true".into()],
            injected_files: vec![],
            injected_readonly_dirs: vec![],
            env: vec![],
            timeout_ms: 1,
            limits: Default::default(),
            result_policy: Default::default(),
            cleanup_policy: Default::default(),
        }
    }

    #[test]
    fn user_mode_rejects_readonly_injected_dirs() {
        let mut req = request();
        req.injected_readonly_dirs.push(InjectedDir {
            source_path: "/fixtures".into(),
            sandbox_path: "/opt/fixtures".into(),
            readonly: true,
        });

        assert!(validate_request(&req, IsolationMode::Privileged).is_ok());
        let err = validate_request(&req, IsolationMode::User).unwrap_err();
        assert!(
            err.to_string()
                .contains("readonly injected directories require privileged isolation mode")
        );
    }
}
