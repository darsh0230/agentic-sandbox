pub mod events;
pub mod http;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tracing::{error, info};

use crate::master::Master;
use crate::types::{AgentRunEvent, AgentRunRequest};

#[derive(Debug, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ApiRequest {
    Health,
    Run { request: AgentRunRequest },
}

#[derive(Debug, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ApiResponse<'a> {
    Health {
        status: &'a str,
    },
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
            let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentRunEvent>(64);
            let stream_task = tokio::spawn(async move {
                while let Some(event) = rx.recv().await {
                    write_json_line(&mut writer, &event).await?;
                }
                anyhow::Ok(writer)
            });
            let result = master.run_agent(request, Some(tx)).await;
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
