pub mod events;
pub mod http;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::time::MissedTickBehavior;
use tracing::{error, info};

use crate::api::events::RUN_STREAM_HEARTBEAT_INTERVAL;
use crate::master::Master;
use crate::types::{AgentRunEvent, AgentRunRequest};

#[derive(Debug, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ApiRequest {
    Health,
    Run { request: Box<AgentRunRequest> },
}

#[derive(Debug, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ApiResponse<'a> {
    Health {
        status: &'a str,
    },
    Heartbeat,
    Result {
        result: &'a crate::types::AgentRunResult,
    },
    Error {
        message: String,
    },
}

pub async fn serve_unix(socket: PathBuf, master: Master) -> Result<()> {
    if let Some(parent) = socket.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    match tokio::fs::remove_file(&socket).await {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }

    let listener = UnixListener::bind(&socket)?;
    info!(socket = %socket.display(), "master API listening");
    let master = Arc::new(master);

    loop {
        let (stream, _) = listener.accept().await?;
        let master = Arc::clone(&master);
        tokio::spawn(async move {
            if let Err(err) = handle_client(stream, master).await {
                error!(?err, "client handling failed");
            }
        });
    }
}

async fn handle_client(stream: UnixStream, master: Arc<Master>) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let request: ApiRequest = serde_json::from_str(&line)?;

    match request {
        ApiRequest::Health => {
            write_json_line(&mut writer, &ApiResponse::Health { status: "ok" }).await?
        }
        ApiRequest::Run { request } => {
            let (tx, rx) = tokio::sync::mpsc::channel::<AgentRunEvent>(64);
            let stream_task = tokio::spawn(stream_run_events(writer, rx));
            let result = master.run_agent(*request, Some(tx)).await;
            let mut writer = stream_task.await??;
            match result {
                Ok(result) => {
                    write_json_line(&mut writer, &ApiResponse::Result { result: &result }).await?
                }
                Err(err) => {
                    write_json_line(
                        &mut writer,
                        &ApiResponse::Error {
                            message: format!("{err:#}"),
                        },
                    )
                    .await?
                }
            }
        }
    }
    Ok(())
}

async fn stream_run_events<W>(
    writer: W,
    rx: tokio::sync::mpsc::Receiver<AgentRunEvent>,
) -> Result<W>
where
    W: AsyncWriteExt + Unpin,
{
    stream_run_events_with_interval(writer, rx, RUN_STREAM_HEARTBEAT_INTERVAL).await
}

async fn stream_run_events_with_interval<W>(
    mut writer: W,
    mut rx: tokio::sync::mpsc::Receiver<AgentRunEvent>,
    heartbeat_interval: Duration,
) -> Result<W>
where
    W: AsyncWriteExt + Unpin,
{
    let mut interval = tokio::time::interval(heartbeat_interval);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    interval.tick().await;

    loop {
        tokio::select! {
            event = rx.recv() => {
                let Some(event) = event else {
                    return Ok(writer);
                };
                write_json_line(&mut writer, &event).await?;
            }
            _ = interval.tick() => {
                write_json_line(&mut writer, &ApiResponse::Heartbeat).await?;
            }
        }
    }
}

async fn write_json_line<T: serde::Serialize, W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    value: &T,
) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::{AsyncBufReadExt, BufReader};

    use super::stream_run_events_with_interval;
    use crate::types::AgentRunEvent;

    #[tokio::test]
    async fn unix_run_stream_sends_queue_event_and_heartbeat() {
        let (client, server) = tokio::io::duplex(1024);
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        tx.send(AgentRunEvent::WaitingForSlot {
            position: 2,
            max_waiting: 10,
        })
        .await
        .unwrap();

        let stream_task = tokio::spawn(stream_run_events_with_interval(
            server,
            rx,
            Duration::from_millis(5),
        ));
        let mut reader = BufReader::new(client);
        let mut line = String::new();
        tokio::time::timeout(Duration::from_millis(100), reader.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            line,
            "{\"type\":\"waiting_for_slot\",\"position\":2,\"max_waiting\":10}\n"
        );

        line.clear();
        tokio::time::timeout(Duration::from_millis(100), reader.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(line, "{\"type\":\"heartbeat\"}\n");

        drop(tx);
        stream_task.await.unwrap().unwrap();
    }
}
