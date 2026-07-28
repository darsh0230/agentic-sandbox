use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use futures::StreamExt;
use tokio::sync::{mpsc, watch};
use tokio::time::MissedTickBehavior;
use tokio_stream::wrappers::ReceiverStream;
use tracing::info;

use crate::api::events::RUN_STREAM_HEARTBEAT_INTERVAL;
use crate::master::Master;
use crate::types::{AgentRunEvent, AgentRunRequest};

const INDEX_HTML: &str = include_str!("../../ui/index.html");
const APP_CSS: &str = include_str!("../../ui/app.css");
const APP_JS: &str = include_str!("../../ui/app.js");
#[derive(Clone)]
pub struct HttpConfig {
    pub addr: SocketAddr,
    pub token: Option<String>,
}

#[derive(Clone)]
struct AppState {
    master: Master,
    token: Option<Arc<str>>,
}

#[derive(serde::Serialize)]
struct HealthResponse<'a> {
    status: &'a str,
}

#[derive(serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RunLine<'a> {
    Heartbeat,
    Event {
        event: &'a AgentRunEvent,
    },
    Result {
        result: &'a crate::types::AgentRunResult,
    },
    Error {
        message: String,
    },
}

pub async fn serve_http(config: HttpConfig, master: Master) -> Result<()> {
    let state = AppState {
        master,
        token: config.token.map(Arc::from),
    };
    let app = Router::new()
        .route("/", get(index))
        .route("/app.css", get(css))
        .route("/app.js", get(js))
        .route("/api/health", get(health))
        .route("/api/run", post(run_agent))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(config.addr).await?;
    info!(addr = %config.addr, "development HTTP UI listening");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn css() -> Response {
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], APP_CSS).into_response()
}

async fn js() -> Response {
    (
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        APP_JS,
    )
        .into_response()
}

async fn health(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return *response;
    }
    Json(HealthResponse { status: "ok" }).into_response()
}

async fn run_agent(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<AgentRunRequest>,
) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return *response;
    }

    let (line_tx, line_rx) = mpsc::channel::<String>(128);
    let (event_tx, mut event_rx) = mpsc::channel::<AgentRunEvent>(128);
    let (done_tx, done_rx) = watch::channel(false);
    let (disconnected_tx, disconnected_rx) = watch::channel(false);
    let master = state.master.clone();

    let heartbeat_line_tx = line_tx.clone();
    let heartbeat_disconnected_tx = disconnected_tx.clone();
    tokio::spawn(async move {
        send_heartbeats(heartbeat_line_tx, done_rx, heartbeat_disconnected_tx).await;
    });

    let event_line_tx = line_tx.clone();
    let mut event_disconnected_rx = disconnected_rx;
    let event_disconnected_tx = disconnected_tx.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                event = event_rx.recv() => {
                    let Some(event) = event else {
                        return;
                    };
                    if send_line(&event_line_tx, &RunLine::Event { event: &event })
                        .await
                        .is_err()
                    {
                        let _ = event_disconnected_tx.send(true);
                        return;
                    }
                }
                changed = event_disconnected_rx.changed() => {
                    if changed.is_err() || *event_disconnected_rx.borrow() {
                        return;
                    }
                }
            }
        }
    });

    let result_tx = line_tx.clone();
    tokio::spawn(async move {
        let result = master.run_agent(request, Some(event_tx)).await;
        match result {
            Ok(result) => {
                let _ = send_line(&result_tx, &RunLine::Result { result: &result }).await;
            }
            Err(err) => {
                let _ = send_line(
                    &result_tx,
                    &RunLine::Error {
                        message: format!("{err:#}"),
                    },
                )
                .await;
            }
        }
        let _ = done_tx.send(true);
    });
    drop(line_tx);

    let stream =
        ReceiverStream::new(line_rx).map(|line| Ok::<Bytes, Infallible>(Bytes::from(line)));
    (
        [
            (header::CONTENT_TYPE, "application/x-ndjson; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store, no-transform"),
            (header::HeaderName::from_static("x-accel-buffering"), "no"),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}

async fn send_heartbeats(
    tx: mpsc::Sender<String>,
    mut done_rx: watch::Receiver<bool>,
    disconnected_tx: watch::Sender<bool>,
) {
    let mut interval = tokio::time::interval(RUN_STREAM_HEARTBEAT_INTERVAL);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    interval.tick().await;

    loop {
        tokio::select! {
            _ = interval.tick() => {
                if *done_rx.borrow() {
                    return;
                }
                if send_line(&tx, &RunLine::Heartbeat).await.is_err() {
                    let _ = disconnected_tx.send(true);
                    return;
                }
            }
            changed = done_rx.changed() => {
                if changed.is_err() || *done_rx.borrow() {
                    return;
                }
            }
        }
    }
}

async fn send_line<T: serde::Serialize>(tx: &mpsc::Sender<String>, value: &T) -> Result<()> {
    let mut line = serde_json::to_string(value)?;
    line.push('\n');
    tx.send(line).await?;
    Ok(())
}

fn authorize(state: &AppState, headers: &HeaderMap) -> std::result::Result<(), Box<Response>> {
    let Some(token) = &state.token else {
        return Ok(());
    };
    let expected = format!("Bearer {token}");
    match headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
    {
        Some(actual) if constant_time_eq(actual.as_bytes(), expected.as_bytes()) => Ok(()),
        _ => Err(Box::new(
            (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"))],
                "missing or invalid bearer token",
            )
                .into_response(),
        )),
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right.iter())
        .fold(0_u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

#[cfg(test)]
mod tests {
    use super::RunLine;
    use crate::types::AgentRunEvent;

    #[test]
    fn queue_event_uses_http_run_line_envelope() {
        let event = AgentRunEvent::WaitingForSlot {
            position: 3,
            max_waiting: 80,
        };
        let line = serde_json::to_string(&RunLine::Event { event: &event }).unwrap();
        assert_eq!(
            line,
            r#"{"type":"event","event":{"type":"waiting_for_slot","position":3,"max_waiting":80}}"#
        );
    }

    #[test]
    fn heartbeat_has_stable_wire_format() {
        let line = serde_json::to_string(&RunLine::Heartbeat).unwrap();
        assert_eq!(line, r#"{"type":"heartbeat"}"#);
    }
}
