pub mod events;
pub mod http;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::time::MissedTickBehavior;
use tracing::{error, info};

use crate::api::events::RUN_STREAM_HEARTBEAT_INTERVAL;
use crate::master::Master;
use crate::types::{AgentRunEvent, AgentRunRequest};

const MAX_API_REQUEST_BYTES: usize = 2 * 1024 * 1024;

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
    remove_stale_socket(&socket).await?;

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

async fn remove_stale_socket(socket: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::FileTypeExt;

    let metadata = match tokio::fs::symlink_metadata(socket).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_socket() {
        bail!(
            "refusing to replace non-socket API path: {}",
            socket.display()
        );
    }

    match UnixStream::connect(socket).await {
        Ok(_) => bail!("API socket is already in use: {}", socket.display()),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            ) =>
        {
            match tokio::fs::remove_file(socket).await {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error.into()),
            }
        }
        Err(error) => Err(error.into()),
    }
}

async fn handle_client(stream: UnixStream, master: Arc<Master>) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let request = read_api_request(&mut reader).await?;

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

async fn read_api_request<R>(reader: &mut R) -> Result<ApiRequest>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut line = String::new();
    let mut limited = reader.take((MAX_API_REQUEST_BYTES + 1) as u64);
    let bytes_read = limited.read_line(&mut line).await?;
    if bytes_read == 0 {
        bail!("API request ended before a JSON line was received");
    }
    if bytes_read > MAX_API_REQUEST_BYTES {
        bail!("API request exceeds {MAX_API_REQUEST_BYTES} bytes");
    }
    if !line.ends_with('\n') {
        bail!("API request must end with a newline");
    }
    Ok(serde_json::from_str(&line)?)
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
    use std::path::Path;
    use std::time::Duration;

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    use super::{
        ApiRequest, MAX_API_REQUEST_BYTES, read_api_request, remove_stale_socket,
        stream_run_events_with_interval, write_json_line,
    };
    use crate::types::AgentRunEvent;

    async fn parse_request_bytes(bytes: &[u8]) -> anyhow::Result<ApiRequest> {
        let capacity = bytes.len().max(1) + 1;
        let (mut client, server) = tokio::io::duplex(capacity);
        client.write_all(bytes).await.unwrap();
        client.shutdown().await.unwrap();
        let mut reader = BufReader::new(server);
        read_api_request(&mut reader).await
    }

    fn socket_path(root: &Path, name: &str) -> std::path::PathBuf {
        root.join(format!("{name}.sock"))
    }

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

    #[tokio::test]
    async fn unix_disconnect_closes_the_run_event_receiver() {
        let (client, server) = tokio::io::duplex(1024);
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let stream_task = tokio::spawn(stream_run_events_with_interval(
            server,
            rx,
            Duration::from_millis(5),
        ));

        drop(client);
        tokio::time::timeout(Duration::from_millis(100), tx.closed())
            .await
            .expect("event receiver should close after the heartbeat write fails");

        assert!(stream_task.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn unix_request_reader_requires_one_bounded_newline_terminated_object() {
        assert!(matches!(
            parse_request_bytes(b"{\"type\":\"health\"}\n")
                .await
                .unwrap(),
            ApiRequest::Health
        ));
        for invalid in [
            b"".as_slice(),
            b"{\"type\":\"health\"}".as_slice(),
            b"not-json\n".as_slice(),
            b"{\"type\":\"unknown\"}\n".as_slice(),
        ] {
            assert!(
                parse_request_bytes(invalid).await.is_err(),
                "invalid request was accepted: {:?}",
                String::from_utf8_lossy(invalid)
            );
        }

        let mut oversized = vec![b' '; MAX_API_REQUEST_BYTES];
        oversized.push(b'\n');
        let error = parse_request_bytes(&oversized).await.unwrap_err();
        assert!(error.to_string().contains("exceeds"));
    }

    #[tokio::test]
    async fn socket_startup_refuses_regular_files_and_live_listeners() {
        let root = tempfile::tempdir().unwrap();
        let regular = socket_path(root.path(), "regular");
        tokio::fs::write(&regular, b"do not delete").await.unwrap();
        let error = remove_stale_socket(&regular).await.unwrap_err();
        assert!(error.to_string().contains("non-socket"));
        assert_eq!(tokio::fs::read(&regular).await.unwrap(), b"do not delete");

        let live = socket_path(root.path(), "live");
        let _listener = tokio::net::UnixListener::bind(&live).unwrap();
        let error = remove_stale_socket(&live).await.unwrap_err();
        assert!(error.to_string().contains("already in use"));
        assert!(live.exists());
    }

    #[tokio::test]
    async fn socket_startup_removes_only_stale_socket_nodes() {
        let root = tempfile::tempdir().unwrap();
        let stale = socket_path(root.path(), "stale");
        let listener = tokio::net::UnixListener::bind(&stale).unwrap();
        drop(listener);
        assert!(stale.exists());

        remove_stale_socket(&stale).await.unwrap();
        assert!(!stale.exists());
        remove_stale_socket(&stale).await.unwrap();
    }

    #[tokio::test]
    async fn json_line_writer_flushes_stable_framing() {
        let (client, mut server) = tokio::io::duplex(128);
        let task = tokio::spawn(async move {
            let mut client = client;
            write_json_line(
                &mut client,
                &serde_json::json!({"type": "health", "status": "ok"}),
            )
            .await
            .unwrap();
        });
        let mut line = String::new();
        BufReader::new(&mut server)
            .read_line(&mut line)
            .await
            .unwrap();
        task.await.unwrap();
        assert_eq!(line, "{\"status\":\"ok\",\"type\":\"health\"}\n");
    }
}
