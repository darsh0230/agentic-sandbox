pub mod allocator;
pub mod cleanup;
pub mod events;
pub mod injector;
pub mod result_collector;
pub mod runner;
pub mod supervisor;
pub mod validation;
pub mod workspace;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use tokio::sync::{Mutex, mpsc};
use tracing::info;

use crate::proxy::ProxyHandle;
use crate::sandbox::{IsolationMode, SlotSpec};
use crate::shutdown::{DEFAULT_SHUTDOWN_GRACE, ShutdownCoordinator};
use crate::types::{AgentRunEvent, AgentRunRequest, AgentRunResult};

use self::allocator::{SlotAllocation, SlotAllocator};
use self::injector::Injector;
use self::result_collector::ResultCollector;
use self::supervisor::shutdown_error;
use self::validation::{validate_request, validate_user_mode_slots};
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
}

#[cfg(test)]
mod tests {
    use crate::types::AgentRunRequest;

    // These are used only by the host-mode integration tests below, which are
    // compiled out on Linux (the Linux path is covered by tests/docker_e2e.rs).
    #[cfg(all(unix, not(target_os = "linux")))]
    use super::*;
    #[cfg(all(unix, not(target_os = "linux")))]
    use crate::shutdown::ShutdownSignal;

    pub(super) fn request() -> AgentRunRequest {
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

    #[cfg(all(unix, not(target_os = "linux")))]
    #[tokio::test]
    async fn service_shutdown_signal_reaches_the_sandbox_process_group() {
        let root = tempfile::tempdir().unwrap();
        let shutdown = ShutdownCoordinator::new(std::time::Duration::from_secs(2));
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

        let result = tokio::time::timeout(std::time::Duration::from_secs(2), run_task)
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

        let result = tokio::time::timeout(std::time::Duration::from_secs(2), run_task)
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

        let result = tokio::time::timeout(std::time::Duration::from_secs(2), run_task)
            .await
            .expect("opted-out run should finish normally")
            .unwrap()
            .unwrap();

        assert_eq!(result.exit_code, Some(0));
    }
}
