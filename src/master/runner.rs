use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::api::events::TailBuffer;
use crate::policy::defaults::clean_env_for_slot;
use crate::proxy::mitm::{PRIVILEGED_CA_PATH, WORKSPACE_CA_RELATIVE_PATH};
use crate::sandbox::{IsolationMode, SandboxLaunchConfig, SandboxLauncher, SlotSpec};
use crate::shutdown::{ShutdownPhase, ShutdownSignal};
use crate::types::{AgentRunEvent, AgentRunRequest, AgentRunResult};

use super::Master;
use super::events::{ReliableEventForwarder, forward_output};
use super::supervisor::{
    RunTermination, await_while_connected, handle_supervisor_shutdown_phase,
    should_preserve_workspace, shutdown_error,
};

impl Master {
    pub(super) async fn run_agent_in_slot(
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

        // Bounds only the child's execution. Once the child exits, event flush,
        // result collection, and workspace cleanup run to completion (outside
        // this budget) so a finished run still delivers its terminal result.
        let execution_deadline = tokio::time::sleep(Duration::from_millis(req.timeout_ms));
        tokio::pin!(execution_deadline);
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
                _ = &mut execution_deadline, if shutdown_signal.is_none() => break RunTermination::TimedOut,
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
        // Drain ShuttingDown/Exited/ResultFileFound into the channel before it
        // closes, so a fast shutdown cannot truncate the client's event stream.
        reliable_events.flush().await;

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
