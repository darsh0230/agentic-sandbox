pub mod allocator;
pub mod cleanup;
pub mod injector;
pub mod result_collector;
pub mod supervisor;
pub mod workspace;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::sync::{Mutex, mpsc};
use tracing::{info, warn};

use crate::api::events::TailBuffer;
use crate::policy::defaults::clean_env_for_slot;
use crate::proxy::ProxyHandle;
use crate::proxy::mitm::{PRIVILEGED_CA_PATH, WORKSPACE_CA_RELATIVE_PATH};
use crate::sandbox::{IsolationMode, OutputStream, SandboxLaunchConfig, SandboxLauncher, SlotSpec};
use crate::types::{AgentRunEvent, AgentRunRequest, AgentRunResult, CleanupPolicy};

use self::allocator::{SlotAllocation, SlotAllocator};
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
    proxy: Option<ProxyHandle>,
}

impl Master {
    pub async fn new(
        root: PathBuf,
        slot_count: u16,
        isolation_mode: IsolationMode,
        proxy: Option<ProxyHandle>,
    ) -> Result<Self> {
        let max_waiting = usize::from(slot_count)
            .checked_mul(allocator::DEFAULT_MAX_WAITING_PER_SLOT)
            .expect("validated slot count cannot overflow usize");
        Self::new_with_max_waiting(root, slot_count, max_waiting, isolation_mode, proxy).await
    }

    pub async fn new_with_max_waiting(
        root: PathBuf,
        slot_count: u16,
        max_waiting: usize,
        isolation_mode: IsolationMode,
        proxy: Option<ProxyHandle>,
    ) -> Result<Self> {
        WorkspaceManager::prepare_root(&root, isolation_mode).await?;
        let planned_slots = SlotAllocator::planned_slots(&root.join("workspaces"), slot_count)?;
        if isolation_mode == IsolationMode::User {
            validate_user_mode_slots(&planned_slots)?;
        }
        info!(
            %isolation_mode,
            slots = slot_count,
            max_waiting,
            "sandbox isolation mode configured"
        );
        Ok(Self {
            allocator: Arc::new(SlotAllocator::with_max_waiting(
                root.join("workspaces"),
                slot_count,
                max_waiting,
            )?),
            workspace: Arc::new(WorkspaceManager::new(root.clone())),
            injector: Arc::new(Injector::new()),
            collector: Arc::new(ResultCollector::new()),
            isolation_mode,
            root,
            launcher_lock: Arc::new(Mutex::new(())),
            proxy,
        })
    }

    pub async fn run_agent(
        &self,
        req: AgentRunRequest,
        events: Option<mpsc::Sender<AgentRunEvent>>,
    ) -> Result<AgentRunResult> {
        validate_request(&req, self.isolation_mode)?;
        if self.proxy.is_none() && !req.inject_headers.is_empty() {
            bail!("inject_headers requires proxy support; remove --disable-proxy");
        }
        let allocation = self.allocator.begin_allocate()?;
        let slot_lease = match allocation {
            SlotAllocation::Ready(lease) => lease,
            SlotAllocation::Waiting(waiter) => {
                let status = waiter.status();
                let queued_at = Instant::now();
                info!(
                    run_id = %req.run_id,
                    position = status.position,
                    max_waiting = status.max_waiting,
                    "run waiting for sandbox slot"
                );
                let lease = if let Some(tx) = &events {
                    let event = AgentRunEvent::WaitingForSlot {
                        position: status.position,
                        max_waiting: status.max_waiting,
                    };

                    if req.kill_on_disconnect {
                        tx.send(event)
                            .await
                            .context("client disconnected while entering sandbox wait queue")?;
                        tokio::select! {
                            lease = waiter.wait() => lease?,
                            _ = tx.closed() => {
                                bail!("client disconnected while waiting for a sandbox slot");
                            }
                        }
                    } else {
                        let _ = tx.send(event).await;
                        waiter.wait().await?
                    }
                } else {
                    waiter.wait().await?
                };
                info!(
                    run_id = %req.run_id,
                    slot = %lease.slot().id,
                    waited_ms = queued_at.elapsed().as_millis(),
                    "allocated queued sandbox slot"
                );
                lease
            }
        };
        let slot_spec = SlotSpec::from(slot_lease.slot());
        info!(run_id = %req.run_id, slot = %slot_lease.slot().id, isolation_mode = %self.isolation_mode, "allocated sandbox slot");

        // Once a sandbox owns a slot, keep its execution task alive even if the
        // request handler is canceled. Returning the slot while its process is
        // still running could let two runs overlap in the same workspace.
        let master = self.clone();
        let run_task = tokio::spawn(async move {
            let _slot_lease = slot_lease;
            master.run_agent_in_slot(req, slot_spec, events).await
        });
        run_task.await.context("sandbox execution task failed")?
    }

    async fn run_agent_in_slot(
        &self,
        req: AgentRunRequest,
        slot: SlotSpec,
        events: Option<mpsc::Sender<AgentRunEvent>>,
    ) -> Result<AgentRunResult> {
        await_while_connected(
            self.workspace.reset_workspace(&slot),
            req.kill_on_disconnect,
            events.as_ref(),
        )
        .await?
        .context("reset workspace")?;
        await_while_connected(
            self.injector.inject(&slot, &req.injected_files),
            req.kill_on_disconnect,
            events.as_ref(),
        )
        .await?
        .context("inject files")?;
        if let Some(proxy) = &self.proxy
            && let Some(ca_cert_pem) = proxy.ca_cert_pem()
        {
            await_while_connected(
                self.injector.inject(
                    &slot,
                    &[crate::types::InjectedFile {
                        relative_path: WORKSPACE_CA_RELATIVE_PATH.to_string(),
                        content: ca_cert_pem.as_bytes().to_vec(),
                        mode: 0o600,
                    }],
                ),
                req.kill_on_disconnect,
                events.as_ref(),
            )
            .await?
            .context("inject proxy MITM CA certificate")?;
        }
        await_while_connected(
            self.workspace.chown_workspace(&slot),
            req.kill_on_disconnect,
            events.as_ref(),
        )
        .await?
        .context("chown workspace")?;

        let mut env = clean_env_for_slot(&req, &slot, self.isolation_mode);
        let _proxy_guard = if let Some(proxy) = &self.proxy {
            let registration = proxy
                .register_run(&req.run_id, &slot.id, &req.inject_headers)
                .context("register proxy run context")?;
            env.extend(registration.env());
            if proxy.ca_cert_pem().is_some() {
                env.extend(proxy_ca_env(&slot, self.isolation_mode));
            }
            Some(registration)
        } else {
            None
        };
        let config = SandboxLaunchConfig {
            run_id: req.run_id.clone(),
            root: self.root.clone(),
            slot: slot.clone(),
            isolation_mode: self.isolation_mode,
            command: req.command.clone(),
            env,
            injected_readonly_dirs: req.injected_readonly_dirs.clone(),
        };

        let _guard = await_while_connected(
            self.launcher_lock.lock(),
            req.kill_on_disconnect,
            events.as_ref(),
        )
        .await?;
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

        let timeout = tokio::time::sleep(Duration::from_millis(req.timeout_ms));
        tokio::pin!(timeout);
        let disconnect_events = req.kill_on_disconnect.then(|| events.clone()).flatten();
        let disconnected = async move {
            match disconnect_events {
                Some(tx) => tx.closed().await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(disconnected);

        let mut interval = tokio::time::interval(Duration::from_millis(25));
        let mut output_open = true;
        let termination = loop {
            tokio::select! {
                chunk = child.next_output(), if output_open => {
                    match chunk {
                        Some(chunk) => {
                            forward_output(
                                chunk,
                                &mut stdout_tail,
                                &mut stderr_tail,
                                events.as_ref(),
                            ).await;
                        }
                        None => output_open = false,
                    }
                }
                _ = interval.tick() => {
                    if child.try_wait().await?.is_some() {
                        while let Ok(chunk) = tokio::time::timeout(
                            Duration::from_millis(25),
                            child.next_output(),
                        ).await {
                            let Some(chunk) = chunk else { break; };
                            forward_output(
                                chunk,
                                &mut stdout_tail,
                                &mut stderr_tail,
                                events.as_ref(),
                            ).await;
                        }
                        break RunTermination::Exited;
                    }
                }
                _ = &mut timeout => break RunTermination::TimedOut,
                _ = &mut disconnected => break RunTermination::ClientDisconnected,
            }
        };

        match termination {
            RunTermination::Exited => {}
            RunTermination::TimedOut => {
                warn!(run_id = %req.run_id, "sandbox timed out; killing process tree");
                let _ = child.kill().await;
                if let Some(tx) = &events {
                    let _ = tx.send(AgentRunEvent::TimedOut).await;
                }
            }
            RunTermination::ClientDisconnected => {
                warn!(run_id = %req.run_id, "client disconnected; killing sandbox process tree");
                let _ = child.kill().await;
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
            ) && (termination != RunTermination::Exited || exit.code != Some(0)));
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RunTermination {
    Exited,
    TimedOut,
    ClientDisconnected,
}

async fn await_while_connected<T>(
    operation: impl std::future::Future<Output = T>,
    kill_on_disconnect: bool,
    events: Option<&mpsc::Sender<AgentRunEvent>>,
) -> Result<T> {
    if kill_on_disconnect && let Some(tx) = events {
        tokio::select! {
            result = operation => Ok(result),
            _ = tx.closed() => bail!("client disconnected before sandbox started"),
        }
    } else {
        Ok(operation.await)
    }
}

async fn forward_output(
    chunk: crate::sandbox::OutputChunk,
    stdout_tail: &mut TailBuffer,
    stderr_tail: &mut TailBuffer,
    events: Option<&mpsc::Sender<AgentRunEvent>>,
) {
    let event = match chunk.stream {
        OutputStream::Stdout => {
            stdout_tail.push(&chunk.data);
            AgentRunEvent::Stdout { data: chunk.data }
        }
        OutputStream::Stderr => {
            stderr_tail.push(&chunk.data);
            AgentRunEvent::Stderr { data: chunk.data }
        }
    };
    if let Some(tx) = events {
        let _ = tx.send(event).await;
    }
}

fn proxy_ca_env(slot: &SlotSpec, isolation_mode: IsolationMode) -> Vec<(String, String)> {
    let ca_path = match isolation_mode {
        IsolationMode::Privileged => PRIVILEGED_CA_PATH.to_string(),
        IsolationMode::User => slot
            .workspace_path
            .join(WORKSPACE_CA_RELATIVE_PATH)
            .to_string_lossy()
            .into_owned(),
    };
    vec![
        ("SSL_CERT_FILE".to_string(), ca_path.clone()),
        ("REQUESTS_CA_BUNDLE".to_string(), ca_path.clone()),
        ("NODE_EXTRA_CA_CERTS".to_string(), ca_path.clone()),
        ("GIT_SSL_CAINFO".to_string(), ca_path),
    ]
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
    crate::proxy::headers::resolve_rules(
        &req.inject_headers,
        crate::proxy::HeaderRuleSource::Run {
            run_id: req.run_id.clone(),
        },
    )?;
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
            kill_on_disconnect: true,
            limits: Default::default(),
            result_policy: Default::default(),
            cleanup_policy: Default::default(),
            inject_headers: vec![],
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

    #[tokio::test]
    async fn prelaunch_work_is_canceled_when_the_client_disconnects() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);

        let err = await_while_connected(std::future::pending::<()>(), true, Some(&tx))
            .await
            .unwrap_err();

        assert_eq!(
            err.to_string(),
            "client disconnected before sandbox started"
        );
    }

    #[tokio::test]
    async fn prelaunch_work_continues_when_disconnect_killing_is_disabled() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);

        let value = await_while_connected(async { 42 }, false, Some(&tx))
            .await
            .unwrap();

        assert_eq!(value, 42);
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    #[tokio::test]
    async fn disconnect_kills_and_reaps_a_running_sandbox() {
        let root = tempfile::tempdir().unwrap();
        let master = Master::new(root.path().to_path_buf(), 1, IsolationMode::User, None)
            .await
            .unwrap();
        let mut req = request();
        req.run_id = "disconnect-kills".into();
        req.command = vec!["/bin/sleep".into(), "10".into()];
        req.timeout_ms = 10_000;
        req.result_policy.result_file = None;

        let (tx, mut rx) = mpsc::channel(16);
        let run_task = tokio::spawn({
            let master = master.clone();
            async move { master.run_agent(req, Some(tx)).await }
        });
        loop {
            match rx.recv().await {
                Some(AgentRunEvent::Started { .. }) => break,
                Some(_) => {}
                None => panic!("run ended before the sandbox started"),
            }
        }
        drop(rx);

        let result = tokio::time::timeout(Duration::from_secs(2), run_task)
            .await
            .expect("disconnected run should be killed promptly")
            .unwrap()
            .unwrap();

        assert_ne!(result.exit_code, Some(0));
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    #[tokio::test]
    async fn disconnect_opt_out_allows_a_running_sandbox_to_finish() {
        let root = tempfile::tempdir().unwrap();
        let master = Master::new(root.path().to_path_buf(), 1, IsolationMode::User, None)
            .await
            .unwrap();
        let mut req = request();
        req.run_id = "disconnect-continues".into();
        req.command = vec!["/bin/sleep".into(), "0.1".into()];
        req.timeout_ms = 2_000;
        req.kill_on_disconnect = false;
        req.result_policy.result_file = None;

        let (tx, mut rx) = mpsc::channel(16);
        let run_task = tokio::spawn({
            let master = master.clone();
            async move { master.run_agent(req, Some(tx)).await }
        });
        loop {
            match rx.recv().await {
                Some(AgentRunEvent::Started { .. }) => break,
                Some(_) => {}
                None => panic!("run ended before the sandbox started"),
            }
        }
        drop(rx);

        let result = tokio::time::timeout(Duration::from_secs(2), run_task)
            .await
            .expect("opted-out run should finish normally")
            .unwrap()
            .unwrap();

        assert_eq!(result.exit_code, Some(0));
    }
}
