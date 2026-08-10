use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, bail};
use tokio::sync::watch;
use tokio::time::Instant;

pub const DEFAULT_SHUTDOWN_GRACE_MS: u64 = 25_000;
pub const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_millis(DEFAULT_SHUTDOWN_GRACE_MS);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShutdownSignal {
    Interrupt,
    Terminate,
}

impl ShutdownSignal {
    pub const fn as_raw(self) -> i32 {
        match self {
            Self::Interrupt => libc::SIGINT,
            Self::Terminate => libc::SIGTERM,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Interrupt => "SIGINT",
            Self::Terminate => "SIGTERM",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShutdownPhase {
    Running,
    Graceful {
        signal: ShutdownSignal,
        deadline: Instant,
    },
    Forced {
        signal: ShutdownSignal,
    },
}

impl ShutdownPhase {
    pub const fn is_running(self) -> bool {
        matches!(self, Self::Running)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BeginShutdown {
    Started { deadline: Instant },
    AlreadyStarted,
}

#[derive(Clone)]
pub struct ShutdownCoordinator {
    inner: Arc<ShutdownInner>,
}

struct ShutdownInner {
    grace: Duration,
    state: Mutex<ShutdownState>,
    changes: watch::Sender<ShutdownPhase>,
    active_runs: watch::Sender<usize>,
}

struct ShutdownState {
    phase: ShutdownPhase,
    active_runs: usize,
}

impl ShutdownCoordinator {
    pub fn new(grace: Duration) -> Self {
        let phase = ShutdownPhase::Running;
        let (changes, _) = watch::channel(phase);
        let (active_runs, _) = watch::channel(0);
        Self {
            inner: Arc::new(ShutdownInner {
                grace,
                state: Mutex::new(ShutdownState {
                    phase,
                    active_runs: 0,
                }),
                changes,
                active_runs,
            }),
        }
    }

    pub fn subscribe(&self) -> watch::Receiver<ShutdownPhase> {
        self.inner.changes.subscribe()
    }

    pub fn phase(&self) -> ShutdownPhase {
        self.lock_state().phase
    }

    pub fn try_start_run(&self) -> Result<RunPermit> {
        let mut state = self.lock_state();
        if !state.phase.is_running() {
            bail!("agentic sandbox is shutting down and is not accepting new runs");
        }
        state.active_runs = state
            .active_runs
            .checked_add(1)
            .expect("active run count cannot overflow usize");
        self.inner.active_runs.send_replace(state.active_runs);
        Ok(RunPermit {
            coordinator: self.clone(),
            active: true,
        })
    }

    pub fn begin(&self, signal: ShutdownSignal) -> BeginShutdown {
        let mut state = self.lock_state();
        if !state.phase.is_running() {
            return BeginShutdown::AlreadyStarted;
        }
        let deadline = Instant::now() + self.inner.grace;
        let phase = ShutdownPhase::Graceful { signal, deadline };
        state.phase = phase;
        self.inner.changes.send_replace(phase);
        BeginShutdown::Started { deadline }
    }

    pub fn force(&self) -> bool {
        let mut state = self.lock_state();
        let ShutdownPhase::Graceful { signal, .. } = state.phase else {
            return false;
        };
        let phase = ShutdownPhase::Forced { signal };
        state.phase = phase;
        self.inner.changes.send_replace(phase);
        true
    }

    pub async fn wait_for_idle(&self) {
        let mut active_runs = self.inner.active_runs.subscribe();
        loop {
            if *active_runs.borrow_and_update() == 0 {
                return;
            }
            if active_runs.changed().await.is_err() {
                return;
            }
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, ShutdownState> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

pub struct RunPermit {
    coordinator: ShutdownCoordinator,
    active: bool,
}

impl Drop for RunPermit {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        {
            let mut state = self.coordinator.lock_state();
            state.active_runs = state
                .active_runs
                .checked_sub(1)
                .expect("run permit count must be balanced");
            self.coordinator
                .inner
                .active_runs
                .send_replace(state.active_runs);
        }
    }
}

pub async fn wait_for_shutdown(receiver: &mut watch::Receiver<ShutdownPhase>) -> ShutdownPhase {
    loop {
        let phase = *receiver.borrow_and_update();
        if !phase.is_running() {
            return phase;
        }
        if receiver.changed().await.is_err() {
            return ShutdownPhase::Forced {
                signal: ShutdownSignal::Terminate,
            };
        }
    }
}

pub async fn wait_for_force(receiver: &mut watch::Receiver<ShutdownPhase>) -> ShutdownPhase {
    loop {
        let phase = *receiver.borrow_and_update();
        if matches!(phase, ShutdownPhase::Forced { .. }) {
            return phase;
        }
        if receiver.changed().await.is_err() {
            return ShutdownPhase::Forced {
                signal: ShutdownSignal::Terminate,
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shutdown_is_atomic_with_run_admission_and_waits_for_permits() {
        let shutdown = ShutdownCoordinator::new(Duration::from_secs(5));
        let permit = shutdown.try_start_run().unwrap();
        assert!(matches!(
            shutdown.begin(ShutdownSignal::Terminate),
            BeginShutdown::Started { .. }
        ));
        assert!(shutdown.try_start_run().is_err());

        let waiting = tokio::spawn({
            let shutdown = shutdown.clone();
            async move { shutdown.wait_for_idle().await }
        });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        drop(permit);
        tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("idle waiter was not notified")
            .unwrap();
    }

    #[tokio::test]
    async fn subscribers_observe_the_original_signal_and_forced_escalation() {
        let shutdown = ShutdownCoordinator::new(Duration::from_secs(5));
        let mut receiver = shutdown.subscribe();
        let BeginShutdown::Started { deadline } = shutdown.begin(ShutdownSignal::Interrupt) else {
            panic!("first shutdown request did not start draining");
        };

        assert_eq!(
            wait_for_shutdown(&mut receiver).await,
            ShutdownPhase::Graceful {
                signal: ShutdownSignal::Interrupt,
                deadline,
            }
        );
        assert_eq!(
            shutdown.begin(ShutdownSignal::Terminate),
            BeginShutdown::AlreadyStarted
        );
        assert!(shutdown.force());
        assert_eq!(
            wait_for_force(&mut receiver).await,
            ShutdownPhase::Forced {
                signal: ShutdownSignal::Interrupt,
            }
        );
        assert!(!shutdown.force());
    }
}
