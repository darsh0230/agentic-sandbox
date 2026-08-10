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
use crate::shutdown::{DEFAULT_SHUTDOWN_GRACE, ShutdownCoordinator, ShutdownPhase, ShutdownSignal};
use crate::types::{AgentRunEvent, AgentRunRequest, AgentRunResult, CleanupPolicy};

use self::allocator::{SlotAllocation, SlotAllocator};
use self::injector::Injector;
use self::result_collector::ResultCollector;
use self::workspace::WorkspaceManager;

// Keep at least one downstream slot available for the reliable lifecycle
// forwarder. Bulk stdout/stderr is explicitly best-effort; run state is not.
const OUTPUT_EVENT_LIFECYCLE_RESERVE: usize = 1;

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
    shutdown: ShutdownCoordinator,
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
        Self::new_with_max_waiting_and_shutdown(
            root,
            slot_count,
            max_waiting,
            isolation_mode,
            proxy,
            ShutdownCoordinator::new(DEFAULT_SHUTDOWN_GRACE),
        )
        .await
    }

    pub async fn new_with_max_waiting_and_shutdown(
        root: PathBuf,
        slot_count: u16,
        max_waiting: usize,
        isolation_mode: IsolationMode,
        proxy: Option<ProxyHandle>,
        shutdown: ShutdownCoordinator,
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
            shutdown,
        })
    }

    pub fn shutdown(&self) -> ShutdownCoordinator {
        self.shutdown.clone()
    }

    pub fn is_accepting_runs(&self) -> bool {
        self.shutdown.phase().is_running()
    }

    pub async fn run_agent(
        &self,
        req: AgentRunRequest,
        events: Option<mpsc::Sender<AgentRunEvent>>,
    ) -> Result<AgentRunResult> {
        let mut shutdown_rx = self.shutdown.subscribe();
        let run_permit = self.shutdown.try_start_run()?;
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
                        tokio::select! {
                            biased;
                            changed = shutdown_rx.changed() => {
                                changed.context("shutdown channel closed")?;
                                return Err(shutdown_error(*shutdown_rx.borrow_and_update()));
                            }
                            result = tx.send(event) => {
                                result.context("client disconnected while entering sandbox wait queue")?;
                            }
                        }
                        tokio::select! {
                            biased;
                            changed = shutdown_rx.changed() => {
                                changed.context("shutdown channel closed")?;
                                return Err(shutdown_error(*shutdown_rx.borrow_and_update()));
                            }
                            lease = waiter.wait() => lease?,
                            _ = tx.closed() => {
                                bail!("client disconnected while waiting for a sandbox slot");
                            }
                        }
                    } else {
                        tokio::select! {
                            biased;
                            changed = shutdown_rx.changed() => {
                                changed.context("shutdown channel closed")?;
                                return Err(shutdown_error(*shutdown_rx.borrow_and_update()));
                            }
                            _ = tx.send(event) => {}
                        }
                        tokio::select! {
                            biased;
                            changed = shutdown_rx.changed() => {
                                changed.context("shutdown channel closed")?;
                                return Err(shutdown_error(*shutdown_rx.borrow_and_update()));
                            }
                            lease = waiter.wait() => lease?,
                        }
                    }
                } else {
                    tokio::select! {
                        biased;
                        changed = shutdown_rx.changed() => {
                            changed.context("shutdown channel closed")?;
                            return Err(shutdown_error(*shutdown_rx.borrow_and_update()));
                        }
                        lease = waiter.wait() => lease?,
                    }
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
            let _run_permit = run_permit;
            master
                .run_agent_in_slot(req, slot_spec, events, shutdown_rx)
                .await
        });
        run_task.await.context("sandbox execution task failed")?
    }

    async fn run_agent_in_slot(
        &self,
        req: AgentRunRequest,
        slot: SlotSpec,
        events: Option<mpsc::Sender<AgentRunEvent>>,
        mut shutdown_rx: tokio::sync::watch::Receiver<ShutdownPhase>,
    ) -> Result<AgentRunResult> {
        await_while_connected(
            self.workspace.reset_workspace(&slot),
            req.kill_on_disconnect,
            events.as_ref(),
            &mut shutdown_rx,
        )
        .await?
        .context("reset workspace")?;
        await_while_connected(
            self.injector.inject(&slot, &req.injected_files),
            req.kill_on_disconnect,
            events.as_ref(),
            &mut shutdown_rx,
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
                &mut shutdown_rx,
            )
            .await?
            .context("inject proxy MITM CA certificate")?;
        }
        await_while_connected(
            self.workspace.chown_workspace(&slot),
            req.kill_on_disconnect,
            events.as_ref(),
            &mut shutdown_rx,
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
            &mut shutdown_rx,
        )
        .await?;
        if !shutdown_rx.borrow().is_running() {
            return Err(shutdown_error(*shutdown_rx.borrow_and_update()));
        }
        let mut child = SandboxLauncher::spawn(config).context("spawn sandbox")?;
        drop(_guard);

        let reliable_events = ReliableEventForwarder::new(events.as_ref());
        let started = AgentRunEvent::Started {
            run_id: req.run_id.clone(),
            slot_id: slot.id.clone(),
            pid: child.pid(),
        };
        if let Some(tx) = &events {
            match tx.try_send(started) {
                Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {}
                Err(mpsc::error::TrySendError::Full(started)) => {
                    reliable_events.send(started);
                }
            }
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
        let mut shutdown_signal = None;
        let termination = 'supervise: loop {
            tokio::select! {
                biased;
                changed = shutdown_rx.changed() => {
                    let phase = if changed.is_err() {
                        ShutdownPhase::Forced {
                            signal: shutdown_signal.unwrap_or(ShutdownSignal::Terminate),
                        }
                    } else {
                        *shutdown_rx.borrow_and_update()
                    };
                    if let Some(termination) = handle_supervisor_shutdown_phase(
                        phase,
                        &mut shutdown_signal,
                        |signal| {
                            info!(
                                run_id = %req.run_id,
                                slot = %slot.id,
                                signal = signal.name(),
                                "forwarding shutdown signal to sandbox process group"
                            );
                            child.send_signal(signal).with_context(|| {
                                format!(
                                    "forward {} to sandbox process group {}",
                                    signal.name(),
                                    child.pid()
                                )
                            })?;
                            Ok(())
                        },
                        |signal| {
                            reliable_events.send(AgentRunEvent::ShuttingDown {
                                signal: signal.as_raw(),
                            });
                        },
                    )? {
                        break 'supervise termination;
                    }
                }
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
                        break match shutdown_signal {
                            Some(signal) => RunTermination::ServiceShutdown(signal),
                            None => RunTermination::Exited,
                        };
                    }
                }
                _ = &mut timeout, if shutdown_signal.is_none() => break RunTermination::TimedOut,
                _ = &mut disconnected, if shutdown_signal.is_none() => {
                    break RunTermination::ClientDisconnected;
                }
            }
        };

        match termination {
            RunTermination::Exited => {
                // The direct child may have left background descendants in its
                // process group. Terminate them before collecting results so
                // they cannot race artifact reads or the next slot owner.
                if let Err(err) = child.kill().await {
                    warn!(run_id = %req.run_id, ?err, "failed to clean sandbox process group");
                }
            }
            RunTermination::TimedOut => {
                warn!(run_id = %req.run_id, "sandbox timed out; killing process tree");
                if let Err(err) = child.kill().await {
                    warn!(run_id = %req.run_id, ?err, "failed to kill timed-out sandbox process group");
                }
                reliable_events.send(AgentRunEvent::TimedOut);
            }
            RunTermination::ClientDisconnected => {
                warn!(run_id = %req.run_id, "client disconnected; killing sandbox process tree");
                if let Err(err) = child.kill().await {
                    warn!(run_id = %req.run_id, ?err, "failed to kill disconnected sandbox process group");
                }
            }
            RunTermination::ServiceShutdown(signal) => {
                info!(
                    run_id = %req.run_id,
                    signal = signal.name(),
                    "sandbox exited after graceful shutdown signal"
                );
                // The direct child has exited. Remove any descendants that did
                // not honor the process-group signal before releasing the slot.
                if let Err(err) = child.kill().await {
                    warn!(run_id = %req.run_id, ?err, "failed to clean gracefully stopped sandbox process group");
                }
            }
            RunTermination::ForcedShutdown(signal) => {
                warn!(
                    run_id = %req.run_id,
                    signal = signal.name(),
                    "sandbox exceeded shutdown grace period; killing process tree"
                );
                if let Err(err) = child.kill().await {
                    warn!(run_id = %req.run_id, ?err, "failed to force-kill sandbox process group");
                }
            }
        }

        let exit = child.wait().await?;
        reliable_events.send(AgentRunEvent::Exited {
            exit_code: exit.code,
            signal: exit.signal,
        });

        let collected = self.collector.collect(&slot, &req.result_policy).await?;
        for file in &collected.files {
            reliable_events.send(AgentRunEvent::ResultFileFound {
                relative_path: file.relative_path.clone(),
                size: file.size,
            });
        }

        let preserve = should_preserve_workspace(req.cleanup_policy, termination, exit.code);
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
    ServiceShutdown(ShutdownSignal),
    ForcedShutdown(ShutdownSignal),
}

fn handle_supervisor_shutdown_phase(
    phase: ShutdownPhase,
    forwarded_signal: &mut Option<ShutdownSignal>,
    mut forward_signal: impl FnMut(ShutdownSignal) -> Result<()>,
    mut emit_shutting_down: impl FnMut(ShutdownSignal),
) -> Result<Option<RunTermination>> {
    let signal = match phase {
        ShutdownPhase::Running => return Ok(None),
        ShutdownPhase::Graceful { signal, .. } | ShutdownPhase::Forced { signal } => signal,
    };

    // A watch receiver observes only the latest value. If Graceful changes to
    // Forced before this supervisor is polled, preserve the required ordering:
    // forward the original graceful signal and publish its lifecycle event
    // before allowing the caller to enter forced process-tree cleanup.
    if forwarded_signal.is_none() {
        forward_signal(signal)?;
        *forwarded_signal = Some(signal);
        emit_shutting_down(signal);
    }

    Ok(matches!(phase, ShutdownPhase::Forced { .. })
        .then_some(RunTermination::ForcedShutdown(signal)))
}

struct ReliableEventForwarder {
    tx: Option<mpsc::UnboundedSender<AgentRunEvent>>,
}

impl ReliableEventForwarder {
    fn new(output: Option<&mpsc::Sender<AgentRunEvent>>) -> Self {
        let Some(output) = output else {
            return Self { tx: None };
        };
        let output = output.clone();
        let (tx, mut rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            // This queue only carries the finite lifecycle/artifact event set,
            // never unbounded process output. Forward sequentially to preserve
            // TimedOut -> Exited -> ResultFileFound ordering without holding a
            // sandbox slot when a response consumer is temporarily slow.
            while let Some(event) = rx.recv().await {
                if output.send(event).await.is_err() {
                    break;
                }
            }
        });
        Self { tx: Some(tx) }
    }

    fn send(&self, event: AgentRunEvent) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(event);
        }
    }
}

fn should_preserve_workspace(
    policy: CleanupPolicy,
    termination: RunTermination,
    exit_code: Option<i32>,
) -> bool {
    matches!(policy, CleanupPolicy::PreserveWorkspaceForDebug)
        || (matches!(policy, CleanupPolicy::PreserveWorkspaceOnFailure)
            && (termination != RunTermination::Exited || exit_code != Some(0)))
}

async fn await_while_connected<T>(
    operation: impl std::future::Future<Output = T>,
    kill_on_disconnect: bool,
    events: Option<&mpsc::Sender<AgentRunEvent>>,
    shutdown_rx: &mut tokio::sync::watch::Receiver<ShutdownPhase>,
) -> Result<T> {
    if !shutdown_rx.borrow().is_running() {
        return Err(shutdown_error(*shutdown_rx.borrow_and_update()));
    }
    tokio::pin!(operation);
    if kill_on_disconnect && let Some(tx) = events {
        tokio::select! {
            biased;
            changed = shutdown_rx.changed() => {
                changed.context("shutdown channel closed")?;
                Err(shutdown_error(*shutdown_rx.borrow_and_update()))
            }
            result = &mut operation => Ok(result),
            _ = tx.closed() => bail!("client disconnected before sandbox started"),
        }
    } else {
        tokio::select! {
            biased;
            changed = shutdown_rx.changed() => {
                changed.context("shutdown channel closed")?;
                Err(shutdown_error(*shutdown_rx.borrow_and_update()))
            }
            result = &mut operation => Ok(result),
        }
    }
}

fn shutdown_error(phase: ShutdownPhase) -> anyhow::Error {
    match phase {
        ShutdownPhase::Graceful { signal, .. } | ShutdownPhase::Forced { signal } => {
            anyhow::anyhow!("agentic sandbox is shutting down after {}", signal.name())
        }
        ShutdownPhase::Running => anyhow::anyhow!("agentic sandbox shutdown channel closed"),
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
        // Output delivery is best-effort. A slow client must not be able to
        // stall execution or consume the slot reserved for lifecycle events.
        if tx.capacity() > OUTPUT_EVENT_LIFECYCLE_RESERVE {
            let _ = tx.try_send(event);
        }
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
    crate::common::paths::validate_run_id(&req.run_id)?;
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
        if file.mode & !0o777 != 0 {
            bail!("invalid mode for {}", file.relative_path);
        }
    }
    for dir in &req.injected_readonly_dirs {
        crate::common::paths::validate_sandbox_absolute_path(&dir.source_path)
            .context("invalid readonly directory source path")?;
        crate::common::paths::validate_sandbox_absolute_path(&dir.sandbox_path)?;
        if !dir.readonly {
            bail!("injected directories must be readonly in this implementation");
        }
    }
    if req.result_policy.collect_paths.len() > result_collector::MAX_COLLECT_PATHS {
        bail!(
            "too many result collection paths: max {}",
            result_collector::MAX_COLLECT_PATHS
        );
    }
    for path in &req.result_policy.collect_paths {
        crate::common::paths::validate_relative_path(path)
            .context("invalid result collection path")?;
    }
    if let Some(path) = &req.result_policy.result_file {
        crate::common::paths::validate_relative_path(path).context("invalid result file path")?;
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
    use crate::types::{AgentRunRequest, InjectedDir, InjectedFile};

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

    #[test]
    fn validates_run_id_command_and_file_mode_boundaries() {
        for run_id in ["", ".", "..", "../escape", "a/b", "a\0b"] {
            let mut req = request();
            req.run_id = run_id.into();
            assert!(
                validate_request(&req, IsolationMode::Privileged).is_err(),
                "unsafe run id was accepted: {run_id:?}"
            );
        }
        for run_id in ["r", "run-001", "job_1.2"] {
            let mut req = request();
            req.run_id = run_id.into();
            assert!(
                validate_request(&req, IsolationMode::Privileged).is_ok(),
                "safe run id was rejected: {run_id:?}"
            );
        }

        for command in [vec![], vec!["".into()], vec!["echo".into(), "a\0b".into()]] {
            let mut req = request();
            req.command = command;
            assert!(validate_request(&req, IsolationMode::Privileged).is_err());
        }

        for mode in [0, 0o600, 0o755, 0o777] {
            let mut req = request();
            req.injected_files.push(InjectedFile {
                relative_path: "input".into(),
                content: vec![],
                mode,
            });
            assert!(
                validate_request(&req, IsolationMode::Privileged).is_ok(),
                "ordinary permission mode was rejected: {mode:o}"
            );
        }
        for mode in [0o1000, 0o4000, 0o10000, u32::MAX] {
            let mut req = request();
            req.injected_files.push(InjectedFile {
                relative_path: "input".into(),
                content: vec![],
                mode,
            });
            assert!(
                validate_request(&req, IsolationMode::Privileged).is_err(),
                "special or out-of-range mode was accepted: {mode:o}"
            );
        }
    }

    #[test]
    fn validates_all_paths_before_allocating_a_slot() {
        for path in ["", ".", "../escape", "/absolute", "a\0b"] {
            let mut req = request();
            req.result_policy.collect_paths = vec![path.into()];
            assert!(
                validate_request(&req, IsolationMode::Privileged).is_err(),
                "unsafe collection path was accepted: {path:?}"
            );

            let mut req = request();
            req.result_policy.result_file = Some(path.into());
            assert!(
                validate_request(&req, IsolationMode::Privileged).is_err(),
                "unsafe result file was accepted: {path:?}"
            );
        }

        let mut req = request();
        req.injected_readonly_dirs.push(InjectedDir {
            source_path: "relative".into(),
            sandbox_path: "/opt/fixtures".into(),
            readonly: true,
        });
        assert!(validate_request(&req, IsolationMode::Privileged).is_err());

        let mut req = request();
        req.injected_readonly_dirs.push(InjectedDir {
            source_path: "/fixtures".into(),
            sandbox_path: "/".into(),
            readonly: true,
        });
        assert!(validate_request(&req, IsolationMode::Privileged).is_err());

        let mut req = request();
        req.result_policy.collect_paths =
            vec!["artifact".into(); result_collector::MAX_COLLECT_PATHS + 1];
        assert!(validate_request(&req, IsolationMode::Privileged).is_err());
    }

    #[test]
    fn proxy_ca_environment_is_isolation_aware() {
        let slot = SlotSpec {
            id: "slot-0001".into(),
            uid: 10_001,
            gid: 10_001,
            workspace_path: "/srv/workspaces/slot-0001".into(),
        };
        let privileged = proxy_ca_env(&slot, IsolationMode::Privileged);
        assert_eq!(privileged.len(), 4);
        assert!(
            privileged
                .iter()
                .all(|(_, value)| value == PRIVILEGED_CA_PATH)
        );

        let user = proxy_ca_env(&slot, IsolationMode::User);
        let expected = "/srv/workspaces/slot-0001/.pi/proxy-ca.pem";
        assert_eq!(
            user.iter()
                .map(|(_, value)| value.as_str())
                .collect::<Vec<_>>(),
            vec![expected; 4]
        );
        assert_eq!(
            user.iter().map(|(key, _)| key.as_str()).collect::<Vec<_>>(),
            vec![
                "SSL_CERT_FILE",
                "REQUESTS_CA_BUNDLE",
                "NODE_EXTRA_CA_CERTS",
                "GIT_SSL_CAINFO"
            ]
        );
    }

    #[test]
    fn cleanup_policy_preservation_matrix_is_complete() {
        for (policy, termination, exit_code, expected) in [
            (
                CleanupPolicy::DeleteWorkspaceAfterRun,
                RunTermination::Exited,
                Some(0),
                false,
            ),
            (
                CleanupPolicy::DeleteWorkspaceAfterRun,
                RunTermination::TimedOut,
                None,
                false,
            ),
            (
                CleanupPolicy::PreserveWorkspaceForDebug,
                RunTermination::Exited,
                Some(0),
                true,
            ),
            (
                CleanupPolicy::PreserveWorkspaceOnFailure,
                RunTermination::Exited,
                Some(0),
                false,
            ),
            (
                CleanupPolicy::PreserveWorkspaceOnFailure,
                RunTermination::Exited,
                Some(1),
                true,
            ),
            (
                CleanupPolicy::PreserveWorkspaceOnFailure,
                RunTermination::Exited,
                None,
                true,
            ),
            (
                CleanupPolicy::PreserveWorkspaceOnFailure,
                RunTermination::TimedOut,
                None,
                true,
            ),
            (
                CleanupPolicy::PreserveWorkspaceOnFailure,
                RunTermination::ClientDisconnected,
                None,
                true,
            ),
            (
                CleanupPolicy::PreserveWorkspaceOnFailure,
                RunTermination::ServiceShutdown(ShutdownSignal::Terminate),
                Some(0),
                true,
            ),
            (
                CleanupPolicy::PreserveWorkspaceOnFailure,
                RunTermination::ForcedShutdown(ShutdownSignal::Terminate),
                None,
                true,
            ),
        ] {
            assert_eq!(
                should_preserve_workspace(policy, termination, exit_code),
                expected,
                "{policy:?}, {termination:?}, {exit_code:?}"
            );
        }
    }

    #[tokio::test]
    async fn output_backpressure_never_blocks_tail_collection() {
        let (tx, mut rx) = mpsc::channel(1);
        tx.send(AgentRunEvent::TimedOut).await.unwrap();
        let mut stdout = TailBuffer::new(3);
        let mut stderr = TailBuffer::new(3);

        tokio::time::timeout(
            Duration::from_millis(100),
            forward_output(
                crate::sandbox::OutputChunk {
                    stream: OutputStream::Stdout,
                    data: b"abcd".to_vec(),
                },
                &mut stdout,
                &mut stderr,
                Some(&tx),
            ),
        )
        .await
        .expect("a full client channel must not block execution");

        assert_eq!(stdout.into_vec(), b"bcd");
        assert!(stderr.into_vec().is_empty());
        assert!(matches!(rx.recv().await, Some(AgentRunEvent::TimedOut)));
    }

    #[tokio::test]
    async fn output_flood_preserves_downstream_lifecycle_capacity() {
        let (tx, mut rx) = mpsc::channel(3);
        let mut stdout = TailBuffer::new(32);
        let mut stderr = TailBuffer::new(32);
        for byte in 0..10 {
            forward_output(
                crate::sandbox::OutputChunk {
                    stream: OutputStream::Stdout,
                    data: vec![byte],
                },
                &mut stdout,
                &mut stderr,
                Some(&tx),
            )
            .await;
        }

        assert_eq!(tx.capacity(), OUTPUT_EVENT_LIFECYCLE_RESERVE);
        assert_eq!(stdout.into_vec(), (0..10).collect::<Vec<_>>());
        assert!(matches!(
            rx.recv().await,
            Some(AgentRunEvent::Stdout { .. })
        ));
        assert!(matches!(
            rx.recv().await,
            Some(AgentRunEvent::Stdout { .. })
        ));
    }

    #[tokio::test]
    async fn reliable_events_survive_a_saturated_downstream_channel_in_order() {
        let (tx, mut rx) = mpsc::channel(1);
        tx.send(AgentRunEvent::Stdout {
            data: b"already queued".to_vec(),
        })
        .await
        .unwrap();
        let forwarder = ReliableEventForwarder::new(Some(&tx));
        forwarder.send(AgentRunEvent::TimedOut);
        forwarder.send(AgentRunEvent::Exited {
            exit_code: None,
            signal: Some(libc::SIGKILL),
        });
        forwarder.send(AgentRunEvent::ResultFileFound {
            relative_path: "result.json".into(),
            size: 2,
        });
        drop(forwarder);
        drop(tx);

        assert!(matches!(
            rx.recv().await,
            Some(AgentRunEvent::Stdout { .. })
        ));
        assert!(matches!(rx.recv().await, Some(AgentRunEvent::TimedOut)));
        assert!(matches!(
            rx.recv().await,
            Some(AgentRunEvent::Exited {
                exit_code: None,
                signal: Some(libc::SIGKILL)
            })
        ));
        assert!(matches!(
            rx.recv().await,
            Some(AgentRunEvent::ResultFileFound { .. })
        ));
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn prelaunch_work_is_canceled_when_the_client_disconnects() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let shutdown = ShutdownCoordinator::new(DEFAULT_SHUTDOWN_GRACE);
        let mut shutdown_rx = shutdown.subscribe();

        let err = await_while_connected(
            std::future::pending::<()>(),
            true,
            Some(&tx),
            &mut shutdown_rx,
        )
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
        let shutdown = ShutdownCoordinator::new(DEFAULT_SHUTDOWN_GRACE);
        let mut shutdown_rx = shutdown.subscribe();

        let value = await_while_connected(async { 42 }, false, Some(&tx), &mut shutdown_rx)
            .await
            .unwrap();

        assert_eq!(value, 42);
    }

    #[tokio::test]
    async fn prelaunch_work_without_an_event_stream_completes() {
        let shutdown = ShutdownCoordinator::new(DEFAULT_SHUTDOWN_GRACE);
        let mut shutdown_rx = shutdown.subscribe();
        let value = await_while_connected(async { 42 }, true, None, &mut shutdown_rx)
            .await
            .unwrap();
        assert_eq!(value, 42);
    }

    #[tokio::test]
    async fn prelaunch_work_is_canceled_when_service_shutdown_begins() {
        let shutdown = ShutdownCoordinator::new(DEFAULT_SHUTDOWN_GRACE);
        let mut shutdown_rx = shutdown.subscribe();
        shutdown.begin(ShutdownSignal::Terminate);

        let err =
            await_while_connected(std::future::pending::<()>(), false, None, &mut shutdown_rx)
                .await
                .unwrap_err();

        assert_eq!(
            err.to_string(),
            "agentic sandbox is shutting down after SIGTERM"
        );
    }

    #[test]
    fn coalesced_forced_shutdown_forwards_graceful_signal_and_event_before_force() {
        #[derive(Debug, PartialEq, Eq)]
        enum Effect {
            SignalForwarded(ShutdownSignal),
            EventEmitted(ShutdownSignal),
        }

        let shutdown = ShutdownCoordinator::new(DEFAULT_SHUTDOWN_GRACE);
        let mut shutdown_rx = shutdown.subscribe();
        assert!(matches!(
            shutdown.begin(ShutdownSignal::Interrupt),
            crate::shutdown::BeginShutdown::Started { .. }
        ));
        assert!(shutdown.force());

        // The supervisor has not observed either transition yet, so watch
        // exposes only the latest Forced state here.
        let phase = *shutdown_rx.borrow_and_update();
        assert!(matches!(phase, ShutdownPhase::Forced { .. }));

        let effects = std::cell::RefCell::new(Vec::new());
        let mut forwarded_signal = None;
        let termination = handle_supervisor_shutdown_phase(
            phase,
            &mut forwarded_signal,
            |signal| {
                effects.borrow_mut().push(Effect::SignalForwarded(signal));
                Ok(())
            },
            |signal| {
                effects.borrow_mut().push(Effect::EventEmitted(signal));
            },
        )
        .unwrap();

        assert_eq!(
            effects.into_inner(),
            vec![
                Effect::SignalForwarded(ShutdownSignal::Interrupt),
                Effect::EventEmitted(ShutdownSignal::Interrupt),
            ]
        );
        assert_eq!(forwarded_signal, Some(ShutdownSignal::Interrupt));
        assert_eq!(
            termination,
            Some(RunTermination::ForcedShutdown(ShutdownSignal::Interrupt))
        );
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    #[tokio::test]
    async fn service_shutdown_signal_reaches_the_sandbox_process_group() {
        let root = tempfile::tempdir().unwrap();
        let shutdown = ShutdownCoordinator::new(Duration::from_secs(2));
        let master = Master::new_with_max_waiting_and_shutdown(
            root.path().to_path_buf(),
            1,
            1,
            IsolationMode::User,
            None,
            shutdown.clone(),
        )
        .await
        .unwrap();
        let mut req = request();
        req.run_id = "shutdown-signal".into();
        req.command = vec![
            "/bin/sh".into(),
            "-c".into(),
            "trap 'printf handled-term; exit 0' TERM; printf handler-ready; while :; do :; done"
                .into(),
        ];
        req.timeout_ms = 10_000;
        req.result_policy.result_file = None;

        let (tx, mut rx) = mpsc::channel(16);
        let run_task = tokio::spawn({
            let master = master.clone();
            async move { master.run_agent(req, Some(tx)).await }
        });
        let mut started = false;
        let mut handler_ready = false;
        while !started || !handler_ready {
            match rx.recv().await {
                Some(AgentRunEvent::Started { .. }) => started = true,
                Some(AgentRunEvent::Stdout { data }) => {
                    handler_ready |= String::from_utf8_lossy(&data).contains("handler-ready");
                }
                Some(_) => {}
                None => panic!("run ended before the sandbox started"),
            }
        }
        shutdown.begin(ShutdownSignal::Terminate);

        let result = tokio::time::timeout(Duration::from_secs(2), run_task)
            .await
            .expect("sandbox did not handle SIGTERM")
            .unwrap()
            .unwrap();
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.signal, None);
        assert!(
            String::from_utf8_lossy(&result.stdout_tail).contains("handled-term"),
            "slot SIGTERM handler did not produce its marker"
        );
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
