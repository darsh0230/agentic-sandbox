use anyhow::{Context, Result, bail};
use tokio::sync::mpsc;

use crate::shutdown::{ShutdownPhase, ShutdownSignal};
use crate::types::{AgentRunEvent, CleanupPolicy};

#[derive(Debug, Clone, Copy)]
pub struct ExitStatus {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RunTermination {
    Exited,
    TimedOut,
    ClientDisconnected,
    ServiceShutdown(ShutdownSignal),
    ForcedShutdown(ShutdownSignal),
}

pub(super) fn handle_supervisor_shutdown_phase(
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

pub(super) async fn await_while_connected<T>(
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

pub(super) fn shutdown_error(phase: ShutdownPhase) -> anyhow::Error {
    match phase {
        ShutdownPhase::Graceful { signal, .. } | ShutdownPhase::Forced { signal } => {
            anyhow::anyhow!("agentic sandbox is shutting down after {}", signal.name())
        }
        ShutdownPhase::Running => anyhow::anyhow!("agentic sandbox shutdown channel closed"),
    }
}

pub(super) fn should_preserve_workspace(
    policy: CleanupPolicy,
    termination: RunTermination,
    exit_code: Option<i32>,
) -> bool {
    matches!(policy, CleanupPolicy::PreserveWorkspaceForDebug)
        || (matches!(policy, CleanupPolicy::PreserveWorkspaceOnFailure)
            && (termination != RunTermination::Exited || exit_code != Some(0)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shutdown::{DEFAULT_SHUTDOWN_GRACE, ShutdownCoordinator};

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
}
