use std::convert::Infallible;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use anyhow::Result;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use futures::Stream;
use tokio::sync::{mpsc, watch};
use tokio::time::MissedTickBehavior;
use tokio_stream::wrappers::ReceiverStream;
use tracing::info;

use crate::api::events::RUN_STREAM_HEARTBEAT_INTERVAL;
use crate::master::Master;
use crate::shutdown::{wait_for_force, wait_for_shutdown};
use crate::types::{AgentRunEvent, AgentRunRequest};

const INDEX_HTML: &str = include_str!("../../ui/index.html");
const APP_CSS: &str = include_str!("../../ui/app.css");
const APP_JS: &str = include_str!("../../ui/app.js");
const MAX_HTTP_RUN_REQUEST_BYTES: usize = 2 * 1024 * 1024;
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

struct RunBodyStream {
    inner: ReceiverStream<String>,
    disconnected_tx: watch::Sender<bool>,
}

impl Stream for RunBodyStream {
    type Item = std::result::Result<Bytes, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner)
            .poll_next(cx)
            .map(|line| line.map(|line| Ok(Bytes::from(line))))
    }
}

impl Drop for RunBodyStream {
    fn drop(&mut self) {
        let _ = self.disconnected_tx.send(true);
    }
}

pub async fn serve_http(config: HttpConfig, master: Master) -> Result<()> {
    let shutdown = master.shutdown();
    let mut graceful_rx = shutdown.subscribe();
    let mut force_rx = shutdown.subscribe();
    let state = AppState {
        master,
        token: config.token.map(Arc::from),
    };
    let app = router(state);

    let listener = tokio::net::TcpListener::bind(config.addr).await?;
    info!(addr = %config.addr, "development HTTP UI listening");
    let server = async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                wait_for_shutdown(&mut graceful_rx).await;
            })
            .await
    };
    tokio::pin!(server);
    tokio::select! {
        result = &mut server => result?,
        _ = wait_for_force(&mut force_rx) => {
            // Dropping the server future closes any connection that did not
            // finish within the process-wide grace period.
        }
    }
    shutdown.wait_for_idle().await;
    Ok(())
}

fn router(state: AppState) -> Router {
    let api = Router::new()
        .route("/api/health", get(health))
        .route(
            "/api/run",
            post(run_agent).layer(DefaultBodyLimit::max(MAX_HTTP_RUN_REQUEST_BYTES)),
        )
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_api_auth,
        ));

    Router::new()
        .route("/", get(index))
        .route("/app.css", get(css))
        .route("/app.js", get(js))
        .merge(api)
        .with_state(state)
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

async fn health(State(state): State<AppState>) -> Response {
    if state.master.is_accepting_runs() {
        Json(HealthResponse { status: "ok" }).into_response()
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(HealthResponse {
                status: "shutting_down",
            }),
        )
            .into_response()
    }
}

async fn run_agent(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    if !state.master.is_accepting_runs() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "agentic sandbox is shutting down and is not accepting new runs",
        )
            .into_response();
    }
    if !has_json_content_type(&headers) {
        return (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Content-Type must be application/json",
        )
            .into_response();
    }
    let request: AgentRunRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("invalid JSON run request: {error}"),
            )
                .into_response();
        }
    };

    let (line_tx, line_rx) = mpsc::channel::<String>(128);
    let (event_tx, mut event_rx) = mpsc::channel::<AgentRunEvent>(128);
    let (events_drained_tx, events_drained_rx) = tokio::sync::oneshot::channel::<()>();
    let (done_tx, done_rx) = watch::channel(false);
    let (disconnected_tx, disconnected_rx) = watch::channel(false);
    let master = state.master.clone();

    let heartbeat_line_tx = line_tx.clone();
    let heartbeat_disconnected_tx = disconnected_tx.clone();
    let heartbeat_task = tokio::spawn(async move {
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
                        break;
                    };
                    if send_line(&event_line_tx, &RunLine::Event { event: &event })
                        .await
                        .is_err()
                    {
                        let _ = event_disconnected_tx.send(true);
                        break;
                    }
                }
                changed = event_disconnected_rx.changed() => {
                    if changed.is_err() || *event_disconnected_rx.borrow() {
                        break;
                    }
                }
            }
        }
        let _ = events_drained_tx.send(());
    });

    let result_tx = line_tx.clone();
    tokio::spawn(async move {
        let result = master.run_agent(request, Some(event_tx)).await;
        // The master can finish after enqueueing its final lifecycle events.
        // Wait until the forwarding task has drained them so the terminal line
        // can never overtake `exited` or `result_file_found`.
        let _ = events_drained_rx.await;
        // Stop and join the heartbeat producer before adding the terminal
        // record. This makes `result`/`error` the final NDJSON line even when
        // the line channel was previously backpressured.
        let _ = done_tx.send(true);
        let _ = heartbeat_task.await;
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
    });
    drop(line_tx);

    let stream = RunBodyStream {
        inner: ReceiverStream::new(line_rx),
        disconnected_tx,
    };
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

async fn require_api_auth(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if let Err(response) = authorize(&state, request.headers()) {
        return *response;
    }
    next.run(request).await
}

fn has_json_content_type(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case("application/json"))
}

async fn send_heartbeats(
    tx: mpsc::Sender<String>,
    done_rx: watch::Receiver<bool>,
    disconnected_tx: watch::Sender<bool>,
) {
    send_heartbeats_with_interval(tx, done_rx, disconnected_tx, RUN_STREAM_HEARTBEAT_INTERVAL)
        .await;
}

async fn send_heartbeats_with_interval(
    tx: mpsc::Sender<String>,
    mut done_rx: watch::Receiver<bool>,
    disconnected_tx: watch::Sender<bool>,
    heartbeat_interval: std::time::Duration,
) {
    let mut interval = tokio::time::interval(heartbeat_interval);
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
    let mut difference = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        let left_byte = left.get(index).copied().unwrap_or_default();
        let right_byte = right.get(index).copied().unwrap_or_default();
        difference |= usize::from(left_byte ^ right_byte);
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::{
        AppState, MAX_HTTP_RUN_REQUEST_BYTES, RunBodyStream, RunLine, constant_time_eq,
        has_json_content_type, router, send_heartbeats, send_heartbeats_with_interval, send_line,
    };
    use crate::master::Master;
    use crate::sandbox::IsolationMode;
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

    #[tokio::test]
    async fn dropping_http_response_body_reports_disconnect() {
        let (_line_tx, line_rx) = tokio::sync::mpsc::channel(1);
        let (disconnected_tx, mut disconnected_rx) = tokio::sync::watch::channel(false);
        let stream = RunBodyStream {
            inner: tokio_stream::wrappers::ReceiverStream::new(line_rx),
            disconnected_tx,
        };

        drop(stream);
        disconnected_rx.changed().await.unwrap();

        assert!(*disconnected_rx.borrow());
    }

    #[test]
    fn content_type_accepts_json_parameters_case_insensitively() {
        for value in [
            "application/json",
            "Application/JSON",
            "application/json; charset=utf-8",
        ] {
            let mut headers = axum::http::HeaderMap::new();
            headers.insert(header::CONTENT_TYPE, value.parse().unwrap());
            assert!(has_json_content_type(&headers), "{value}");
        }
        for value in ["text/json", "application/problem+json", "text/plain"] {
            let mut headers = axum::http::HeaderMap::new();
            headers.insert(header::CONTENT_TYPE, value.parse().unwrap());
            assert!(!has_json_content_type(&headers), "{value}");
        }
        assert!(!has_json_content_type(&axum::http::HeaderMap::new()));
    }

    #[test]
    fn token_comparison_handles_equal_unequal_and_binary_values() {
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(b"same", b"same"));
        assert!(!constant_time_eq(b"same", b"dame"));
        assert!(!constant_time_eq(b"short", b"longer"));
        assert!(!constant_time_eq(&[0, 255], &[0, 254]));
    }

    #[tokio::test]
    async fn send_line_appends_exactly_one_newline() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        send_line(
            &tx,
            &RunLine::Error {
                message: "boom".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            rx.recv().await.unwrap(),
            "{\"type\":\"error\",\"message\":\"boom\"}\n"
        );
    }

    #[tokio::test]
    async fn heartbeat_task_stops_without_emitting_after_completion() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let (done_tx, done_rx) = tokio::sync::watch::channel(false);
        done_tx.send(true).unwrap();

        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            send_heartbeats(tx, done_rx, tokio::sync::watch::channel(false).0),
        )
        .await
        .unwrap();
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn joined_heartbeat_task_cannot_emit_after_a_terminal_line() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let (done_tx, done_rx) = tokio::sync::watch::channel(false);
        let heartbeat_task = tokio::spawn(send_heartbeats_with_interval(
            tx.clone(),
            done_rx,
            tokio::sync::watch::channel(false).0,
            std::time::Duration::from_millis(1),
        ));

        assert_eq!(
            rx.recv().await.as_deref(),
            Some("{\"type\":\"heartbeat\"}\n")
        );
        done_tx.send(true).unwrap();
        heartbeat_task.await.unwrap();
        send_line(
            &tx,
            &RunLine::Error {
                message: "terminal".into(),
            },
        )
        .await
        .unwrap();
        drop(tx);

        let mut remaining = Vec::new();
        while let Some(line) = rx.recv().await {
            remaining.push(line);
        }
        assert_eq!(
            remaining.last().map(String::as_str),
            Some("{\"type\":\"error\",\"message\":\"terminal\"}\n")
        );
        assert!(
            remaining[..remaining.len() - 1]
                .iter()
                .all(|line| line == "{\"type\":\"heartbeat\"}\n"),
            "only already-enqueued heartbeats may precede the terminal line"
        );
    }

    async fn test_router(token: Option<&str>) -> (axum::Router, tempfile::TempDir) {
        let root = tempfile::tempdir().unwrap();
        let master = Master::new(
            root.path().to_path_buf(),
            1,
            IsolationMode::Privileged,
            None,
        )
        .await
        .unwrap();
        (
            router(AppState {
                master,
                token: token.map(std::sync::Arc::from),
            }),
            root,
        )
    }

    #[tokio::test]
    async fn health_and_static_routes_have_stable_auth_and_content_types() {
        let (app, _root) = test_router(Some("secret")).await;

        let unauthorized = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            unauthorized
                .headers()
                .get(header::WWW_AUTHENTICATE)
                .unwrap(),
            "Bearer"
        );

        let health = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .header(header::AUTHORIZATION, "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(health.status(), StatusCode::OK);
        assert_eq!(
            health.into_body().collect().await.unwrap().to_bytes(),
            r#"{"status":"ok"}"#
        );

        for (path, expected) in [
            ("/app.css", "text/css; charset=utf-8"),
            ("/app.js", "application/javascript; charset=utf-8"),
        ] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.headers().get(header::CONTENT_TYPE).unwrap(),
                expected
            );
        }
    }

    #[tokio::test]
    async fn draining_service_reports_unready_and_rejects_new_runs() {
        let root = tempfile::tempdir().unwrap();
        let master = Master::new(
            root.path().to_path_buf(),
            1,
            IsolationMode::Privileged,
            None,
        )
        .await
        .unwrap();
        let shutdown = master.shutdown();
        let app = router(AppState {
            master,
            token: Some(std::sync::Arc::from("secret")),
        });
        shutdown.begin(crate::shutdown::ShutdownSignal::Terminate);

        let health = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .header(header::AUTHORIZATION, "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(health.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            health.into_body().collect().await.unwrap().to_bytes(),
            r#"{"status":"shutting_down"}"#
        );

        let run = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/run")
                    .header(header::AUTHORIZATION, "Bearer secret")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(run.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn run_route_authenticates_before_parsing_and_enforces_json_media_type() {
        let (app, _root) = test_router(Some("secret")).await;

        let unauthorized = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/run")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let oversized_body = vec![b' '; MAX_HTTP_RUN_REQUEST_BYTES + 1];
        let oversized_unauthorized = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/run")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(oversized_body.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(oversized_unauthorized.status(), StatusCode::UNAUTHORIZED);

        let oversized_authorized = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/run")
                    .header(header::AUTHORIZATION, "Bearer secret")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(oversized_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(oversized_authorized.status(), StatusCode::PAYLOAD_TOO_LARGE);

        let malformed = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/run")
                    .header(header::AUTHORIZATION, "Bearer secret")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);

        let unsupported = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/run")
                    .header(header::AUTHORIZATION, "Bearer secret")
                    .header(header::CONTENT_TYPE, "text/plain")
                    .body(Body::from(r#"{"run_id":"r","command":["true"]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unsupported.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }
}
