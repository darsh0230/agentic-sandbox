use tokio::sync::mpsc;

use crate::api::events::TailBuffer;
use crate::sandbox::OutputStream;
use crate::types::AgentRunEvent;

// Keep at least one downstream slot available for the reliable lifecycle
// forwarder. Bulk stdout/stderr is explicitly best-effort; run state is not.
pub(super) const OUTPUT_EVENT_LIFECYCLE_RESERVE: usize = 1;

/// Forwards the finite lifecycle/artifact event set into the (bounded) run
/// event channel without ever blocking the supervisor on a slow consumer.
/// When a run has no event channel this is the `Inactive` no-op variant.
pub(super) enum ReliableEventForwarder {
    Inactive,
    Active {
        tx: mpsc::UnboundedSender<AgentRunEvent>,
        task: tokio::task::JoinHandle<()>,
    },
}

impl ReliableEventForwarder {
    pub(super) fn new(output: Option<&mpsc::Sender<AgentRunEvent>>) -> Self {
        let Some(output) = output else {
            return Self::Inactive;
        };
        let output = output.clone();
        let (tx, mut rx) = mpsc::unbounded_channel();
        // Never carries unbounded process output; sequential forwarding
        // preserves TimedOut -> Exited -> ResultFileFound ordering.
        let task = tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                if output.send(event).await.is_err() {
                    break;
                }
            }
        });
        Self::Active { tx, task }
    }

    pub(super) fn send(&self, event: AgentRunEvent) {
        if let Self::Active { tx, .. } = self {
            let _ = tx.send(event);
        }
    }

    /// Close the queue and wait until buffered lifecycle events reach the
    /// downstream channel, so a run that ends right after shutdown cannot
    /// close the channel while `ShuttingDown`/`Exited` are still in flight.
    pub(super) async fn flush(self) {
        if let Self::Active { tx, task } = self {
            drop(tx);
            let _ = task.await;
        }
    }
}

pub(super) async fn forward_output(
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

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
    async fn flush_delivers_lifecycle_events_before_the_forwarder_returns() {
        // A slow consumer must not cause lifecycle events to be lost when the
        // forwarder is flushed: the queue blocks on a full downstream channel
        // until the reader makes room, preserving order through shutdown.
        let (tx, mut rx) = mpsc::channel(1);
        tx.send(AgentRunEvent::Stdout {
            data: b"occupied".into(),
        })
        .await
        .unwrap();
        let forwarder = ReliableEventForwarder::new(Some(&tx));
        forwarder.send(AgentRunEvent::ShuttingDown {
            signal: libc::SIGTERM,
        });
        forwarder.send(AgentRunEvent::Exited {
            exit_code: Some(0),
            signal: None,
        });

        // flush() must not return until both lifecycle events are handed to the
        // (initially full) downstream channel.
        let flush = tokio::spawn(async move { forwarder.flush().await });
        tokio::task::yield_now().await;
        assert!(!flush.is_finished());

        assert!(matches!(
            rx.recv().await,
            Some(AgentRunEvent::Stdout { .. })
        ));
        assert!(matches!(
            rx.recv().await,
            Some(AgentRunEvent::ShuttingDown {
                signal
            }) if signal == libc::SIGTERM
        ));
        assert!(matches!(
            rx.recv().await,
            Some(AgentRunEvent::Exited {
                exit_code: Some(0),
                signal: None
            })
        ));
        tokio::time::timeout(Duration::from_secs(1), flush)
            .await
            .expect("flush did not complete after the consumer drained the queue")
            .unwrap();
    }
}
