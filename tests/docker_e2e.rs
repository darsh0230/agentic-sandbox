use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use tokio::time::{sleep, timeout};
use uuid::Uuid;

const BUILD_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const DOCKER_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const READY_TIMEOUT: Duration = Duration::from_secs(30);
const SCENARIO_TIMEOUT: Duration = Duration::from_secs(90);
const STREAM_TIMEOUT: Duration = Duration::from_secs(30);
const QUEUE_HOLD_SECONDS: f64 = 24.0;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a working Docker daemon and intentionally exercises a 15-second heartbeat"]
async fn docker_backed_http_contract() -> Result<()> {
    let mut harness = DockerHarness::start(IsolationProfile::User).await?;
    let scenario_result = match timeout(SCENARIO_TIMEOUT, run_scenarios(&harness)).await {
        Ok(result) => result,
        Err(_) => Err(anyhow!(
            "Docker E2E scenarios exceeded the {SCENARIO_TIMEOUT:?} hard timeout"
        )),
    };

    if let Err(error) = &scenario_result {
        eprintln!("Docker E2E failure: {error:#}");
        eprintln!("Container logs:\n{}", harness.logs());
    }

    let cleanup_result = harness.cleanup();
    match (scenario_result, cleanup_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(cleanup)) => Err(cleanup.context("Docker E2E cleanup failed")),
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("Docker E2E cleanup also failed: {cleanup:#}")))
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a privileged Linux Docker container"]
async fn docker_backed_privileged_isolation_smoke() -> Result<()> {
    let mut harness = DockerHarness::start(IsolationProfile::Privileged).await?;
    let scenario_result = match timeout(
        Duration::from_secs(30),
        privileged_isolation_scenario(&harness),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(anyhow!("privileged isolation smoke test timed out")),
    };
    if let Err(error) = &scenario_result {
        eprintln!("Privileged Docker E2E failure: {error:#}");
        eprintln!("Container logs:\n{}", harness.logs());
    }
    let cleanup_result = harness.cleanup();
    match (scenario_result, cleanup_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(cleanup)) => Err(cleanup.context("privileged Docker cleanup failed")),
        (Err(error), Err(cleanup)) => Err(error.context(format!(
            "privileged Docker cleanup also failed: {cleanup:#}"
        ))),
    }
}

async fn run_scenarios(harness: &DockerHarness) -> Result<()> {
    auth_scenario(harness)
        .await
        .context("authentication scenario")?;
    smoke_scenario(harness).await.context("smoke scenario")?;
    descendant_cleanup_scenario(harness)
        .await
        .context("process-tree cleanup scenario")?;
    timeout_scenario(harness)
        .await
        .context("timeout and slot-release scenario")?;
    queue_scenario(harness)
        .await
        .context("bounded FIFO queue scenario")?;
    active_disconnect_scenario(harness)
        .await
        .context("active disconnect scenario")?;

    let response = harness
        .health(Some(&harness.token))
        .await
        .context("final health check")?;
    ensure!(
        response.status() == StatusCode::OK,
        "final health check returned {}",
        response.status()
    );
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IsolationProfile {
    User,
    Privileged,
}

impl IsolationProfile {
    fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Privileged => "privileged",
        }
    }
}

struct DockerHarness {
    resources: DockerResources,
    client: Client,
    address: SocketAddr,
    base_url: String,
    token: String,
    run_prefix: String,
}

impl DockerHarness {
    async fn start(isolation: IsolationProfile) -> Result<Self> {
        let unique = Uuid::new_v4().simple().to_string();
        let short = &unique[..12];
        let mut resources = DockerResources {
            image: format!("agentic-sandbox-e2e:{unique}"),
            container: format!("agentic-sandbox-e2e-{unique}"),
            label: format!("agentic-sandbox.e2e={unique}"),
            image_built: false,
            container_created: false,
        };

        if let Err(error) = check_docker() {
            return Err(error.context(
                "Docker E2E was explicitly requested, but the Docker daemon is unavailable",
            ));
        }

        if let Err(error) = resources.build_image() {
            return Err(error.context("build lean Docker E2E image"));
        }
        if let Err(error) = resources.start_container(&format!("e2e-token-{unique}"), isolation) {
            eprintln!("Container logs after failed start:\n{}", resources.logs());
            return Err(error.context("start Docker E2E container"));
        }

        let port = resources
            .mapped_port()
            .context("resolve Docker's dynamic HTTP port")?;
        let address = SocketAddr::from(([127, 0, 0, 1], port));
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .read_timeout(Duration::from_secs(25))
            .pool_max_idle_per_host(0)
            .build()
            .context("build E2E HTTP client")?;
        let mut harness = Self {
            resources,
            client,
            address,
            base_url: format!("http://{address}"),
            token: format!("e2e-token-{unique}"),
            run_prefix: format!("e2e-{short}"),
        };

        if let Err(error) = harness.wait_until_ready().await {
            eprintln!(
                "Container logs after readiness failure:\n{}",
                harness.logs()
            );
            let cleanup = harness.cleanup();
            return match cleanup {
                Ok(()) => Err(error),
                Err(cleanup) => Err(error.context(format!(
                    "cleanup after readiness failure also failed: {cleanup:#}"
                ))),
            };
        }
        Ok(harness)
    }

    fn run_id(&self, suffix: &str) -> String {
        format!("{}-{suffix}", self.run_prefix)
    }

    async fn wait_until_ready(&self) -> Result<()> {
        let deadline = Instant::now() + READY_TIMEOUT;
        let mut last_error = None;
        while Instant::now() < deadline {
            match self.health(Some(&self.token)).await {
                Ok(response) if response.status() == StatusCode::OK => {
                    let body = response
                        .bytes()
                        .await
                        .context("read health response during readiness")?;
                    let value: Value = serde_json::from_slice(&body)
                        .context("decode health response during readiness")?;
                    ensure!(
                        value == json!({"status": "ok"}),
                        "unexpected health payload: {value}"
                    );
                    return Ok(());
                }
                Ok(response) => {
                    last_error = Some(format!("HTTP {}", response.status()));
                }
                Err(error) => {
                    last_error = Some(format!("{error:#}"));
                }
            }
            sleep(Duration::from_millis(100)).await;
        }
        bail!(
            "container did not become healthy within {READY_TIMEOUT:?}; last error: {}",
            last_error.unwrap_or_else(|| "no response".to_string())
        )
    }

    async fn health(&self, token: Option<&str>) -> Result<reqwest::Response> {
        let mut request = self.client.get(format!("{}/api/health", self.base_url));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        timeout(Duration::from_secs(4), request.send())
            .await
            .context("health request timed out")?
            .context("send health request")
    }

    async fn open_run(&self, request: Value) -> Result<NdjsonResponse> {
        let request_body = serde_json::to_vec(&request).context("serialize run request")?;
        let response = timeout(
            Duration::from_secs(5),
            self.client
                .post(format!("{}/api/run", self.base_url))
                .bearer_auth(&self.token)
                .header("Accept", "application/x-ndjson")
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(request_body)
                .send(),
        )
        .await
        .context("run request timed out before response headers")?
        .context("send run request")?;

        let status = response.status();
        if status != StatusCode::OK {
            let body = response.text().await.unwrap_or_default();
            bail!("run request returned HTTP {status}: {body}");
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        ensure!(
            content_type.starts_with("application/x-ndjson"),
            "unexpected run response content type {content_type:?}"
        );
        Ok(NdjsonResponse {
            response,
            buffered: Vec::new(),
        })
    }

    fn open_raw_run(&self, request: &Value) -> Result<RawNdjsonResponse> {
        RawNdjsonResponse::connect(self.address, &self.token, request)
    }

    fn logs(&self) -> String {
        self.resources.logs()
    }

    fn cleanup(&mut self) -> Result<()> {
        self.resources.cleanup(true)
    }
}

async fn auth_scenario(harness: &DockerHarness) -> Result<()> {
    let missing = harness.health(None).await?;
    ensure!(
        missing.status() == StatusCode::UNAUTHORIZED,
        "missing token returned {}, expected 401",
        missing.status()
    );
    ensure!(
        missing
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .and_then(|value| value.to_str().ok())
            == Some("Bearer"),
        "401 response did not advertise Bearer authentication"
    );

    let wrong = harness.health(Some("definitely-wrong")).await?;
    ensure!(
        wrong.status() == StatusCode::UNAUTHORIZED,
        "wrong token returned {}, expected 401",
        wrong.status()
    );

    let valid = harness.health(Some(&harness.token)).await?;
    ensure!(
        valid.status() == StatusCode::OK,
        "valid token returned {}",
        valid.status()
    );
    let body = valid.bytes().await?;
    let body: Value = serde_json::from_slice(&body)?;
    ensure!(body == json!({"status": "ok"}), "health payload was {body}");
    Ok(())
}

async fn smoke_scenario(harness: &DockerHarness) -> Result<()> {
    let run_id = harness.run_id("smoke");
    let command = r#"mkdir -p output
printf 'stdout:%s:' "$E2E_VALUE"
cat input/task.txt
printf 'stderr-line\n' >&2
printf 'artifact-data' > output/artifact.txt
printf '%s' '{"ok":true,"source":"e2e"}' > result.json"#;
    let request = json!({
        "run_id": run_id,
        "command": ["sh", "-c", command],
        "injected_files": [{
            "relative_path": "input/task.txt",
            "content_utf8": "injected-data\n",
            "mode": 420
        }],
        "env": [["E2E_VALUE", "expected"]],
        "timeout_ms": 10_000,
        "result_policy": {
            "collect_paths": ["output/artifact.txt"],
            "result_file": "result.json"
        }
    });

    let transcript = run_to_completion(harness, request, STREAM_TIMEOUT).await?;
    ensure!(
        transcript.has_event("started"),
        "smoke run omitted started event: {:?}",
        transcript.values()
    );
    ensure!(
        transcript.has_event("exited"),
        "smoke run omitted exited event: {:?}",
        transcript.values()
    );
    ensure!(
        transcript.has_event("result_file_found"),
        "smoke run omitted result_file_found event: {:?}",
        transcript.values()
    );

    let stdout_events = transcript.event_bytes("stdout")?;
    let stderr_events = transcript.event_bytes("stderr")?;
    ensure!(
        String::from_utf8_lossy(&stdout_events).contains("stdout:expected:injected-data"),
        "unexpected streamed stdout: {:?}",
        String::from_utf8_lossy(&stdout_events)
    );
    ensure!(
        String::from_utf8_lossy(&stderr_events).contains("stderr-line"),
        "unexpected streamed stderr: {:?}",
        String::from_utf8_lossy(&stderr_events)
    );

    let result = transcript.result()?;
    ensure!(
        result["run_id"] == run_id,
        "result run_id mismatch: {result}"
    );
    ensure!(result["exit_code"] == 0, "smoke command failed: {result}");
    ensure!(
        result["workspace_preserved"] == false,
        "default cleanup unexpectedly preserved the workspace: {result}"
    );
    ensure!(
        result["result_json"] == json!({"ok": true, "source": "e2e"}),
        "unexpected structured result: {}",
        result["result_json"]
    );
    ensure!(
        String::from_utf8_lossy(&json_byte_array(&result["stdout_tail"])?)
            .contains("stdout:expected:injected-data"),
        "stdout tail did not contain command output"
    );
    ensure!(
        String::from_utf8_lossy(&json_byte_array(&result["stderr_tail"])?).contains("stderr-line"),
        "stderr tail did not contain command output"
    );

    let files = result["collected_files"]
        .as_array()
        .context("collected_files was not an array")?;
    ensure!(files.len() == 1, "expected one collected file: {files:?}");
    ensure!(
        files[0]["relative_path"] == "output/artifact.txt",
        "unexpected artifact path: {}",
        files[0]
    );
    ensure!(
        json_byte_array(&files[0]["content"])? == b"artifact-data",
        "unexpected artifact content: {}",
        files[0]
    );
    ensure!(files[0]["size"] == 13, "unexpected artifact size");
    Ok(())
}

async fn descendant_cleanup_scenario(harness: &DockerHarness) -> Result<()> {
    let run_id = harness.run_id("descendant-cleanup");
    let marker = format!("/tmp/{run_id}-leaked");
    let ready = format!(".{run_id}-descendant-ready");
    let descendant_script = r#"#!/bin/sh
marker=$1
ready=$2
trap '' HUP TERM
if setsid sh -c 'trap "" HUP TERM; printf escaped > "$1"; sleep 1; printf leaked > "$2"' sh "$ready" "$marker"; then
    :
else
    printf blocked > "$ready"
    sleep 1
    printf leaked > "$marker"
fi
"#;
    let command = format!(
        "test -w /tmp; printf writable > '{marker}'; rm -f '{marker}' '{ready}'; \
         nohup sh descendant-cleanup.sh '{marker}' '{ready}' \
             </dev/null >/dev/null 2>&1 & \
         while test ! -e '{ready}'; do :; done; \
         status=$(cat '{ready}'); \
         rm -f '{ready}'; \
         case \"$status\" in blocked|escaped) ;; *) exit 92;; esac; \
         printf parent-exited"
    );
    let transcript = run_to_completion(
        harness,
        json!({
            "run_id": run_id,
            "command": ["sh", "-c", command],
            "injected_files": [{
                "relative_path": "descendant-cleanup.sh",
                "content_utf8": descendant_script,
                "mode": 384
            }],
            "timeout_ms": 10_000,
            "result_policy": {"collect_paths": [], "result_file": null}
        }),
        Duration::from_secs(8),
    )
    .await?;
    ensure!(
        transcript.result()?["exit_code"] == 0,
        "background-parent command failed: {}",
        transcript.result()?
    );

    sleep(Duration::from_millis(1_500)).await;
    let probe = run_to_completion(
        harness,
        json!({
            "run_id": harness.run_id("descendant-cleanup-probe"),
            "command": [
                "sh",
                "-c",
                format!("if test -e '{marker}'; then cat '{marker}'; rm -f '{marker}'; exit 91; fi")
            ],
            "timeout_ms": 10_000,
            "result_policy": {"collect_paths": [], "result_file": null}
        }),
        Duration::from_secs(8),
    )
    .await?;
    ensure!(
        probe.result()?["exit_code"] == 0,
        "a background descendant survived its completed run: {}",
        probe.result()?
    );
    Ok(())
}

async fn privileged_isolation_scenario(harness: &DockerHarness) -> Result<()> {
    let transcript = run_to_completion(
        harness,
        json!({
            "run_id": harness.run_id("privileged-smoke"),
            "command": [
                "sh",
                "-c",
                "printf 'pwd=%s\\nhome=%s\\nuid=%s\\nhost=%s\\n' \"$PWD\" \"$HOME\" \"$(id -u)\" \"$(cat /proc/sys/kernel/hostname)\"; test \"$PWD\" = /home/agent; test \"$HOME\" = /home/agent; test \"$(id -u)\" = 10001; test \"$(cat /proc/sys/kernel/hostname)\" = pi-agent"
            ],
            "timeout_ms": 15_000,
            "result_policy": {"collect_paths": [], "result_file": null}
        }),
        Duration::from_secs(20),
    )
    .await?;
    ensure!(
        transcript.has_event("started") && transcript.has_event("exited"),
        "privileged run omitted lifecycle events: {:?}",
        transcript.values()
    );
    let result = transcript.result()?;
    ensure!(
        result["exit_code"] == 0,
        "privileged isolation assertions failed: {result}"
    );
    let stdout_bytes = json_byte_array(&result["stdout_tail"])?;
    let stdout = String::from_utf8_lossy(&stdout_bytes);
    for expected in [
        "pwd=/home/agent",
        "home=/home/agent",
        "uid=10001",
        "host=pi-agent",
    ] {
        ensure!(
            stdout.contains(expected),
            "privileged stdout omitted {expected:?}: {stdout:?}"
        );
    }
    Ok(())
}

async fn timeout_scenario(harness: &DockerHarness) -> Result<()> {
    let request = json!({
        "run_id": harness.run_id("timeout"),
        "command": ["sh", "-c", "sleep 30"],
        "timeout_ms": 350,
        "result_policy": {"collect_paths": [], "result_file": null}
    });
    let started_at = Instant::now();
    let transcript = run_to_completion(harness, request, Duration::from_secs(8)).await?;
    ensure!(
        started_at.elapsed() < Duration::from_secs(6),
        "timed-out command did not terminate promptly"
    );
    ensure!(
        transcript.has_event("timed_out"),
        "timeout run omitted timed_out event: {:?}",
        transcript.values()
    );
    let result = transcript.result()?;
    ensure!(
        result["exit_code"].as_i64() != Some(0) || !result["signal"].is_null(),
        "timed-out command reported success: {result}"
    );

    quick_probe(harness, "timeout-probe")
        .await
        .context("slot was not reusable after timeout")?;
    Ok(())
}

async fn queue_scenario(harness: &DockerHarness) -> Result<()> {
    let holder_id = harness.run_id("queue-holder");
    let holder_request = basic_sleep_request(&holder_id, QUEUE_HOLD_SECONDS, None);
    let mut holder = harness.open_run(holder_request).await?;
    let mut holder_lines = Vec::new();
    collect_until(
        &mut holder,
        &mut holder_lines,
        Duration::from_secs(8),
        "holder started event",
        |value| is_event(value, "started"),
    )
    .await?;

    let first_id = harness.run_id("queue-first");
    let second_id = harness.run_id("queue-second");
    let (first, first_lines) = enqueue_waiter(harness, &first_id, 1).await?;
    let (second, second_lines) = enqueue_waiter(harness, &second_id, 2).await?;

    let dropped_id = harness.run_id("queue-dropped");
    let dropped_request = basic_sleep_request(&dropped_id, 0.25, None);
    let mut dropped = harness.open_raw_run(&dropped_request)?;
    let dropped_waiting = dropped.wait_for(
        Duration::from_secs(8),
        "queued raw request waiting event",
        |value| is_event(value, "waiting_for_slot"),
    )?;
    assert_queue_status(&dropped_waiting, 3)?;

    let overflow = run_to_completion(
        harness,
        basic_sleep_request(&harness.run_id("queue-overflow"), 0.1, None),
        Duration::from_secs(5),
    )
    .await?;
    let overflow_error = overflow.error_message()?;
    ensure!(
        overflow_error.contains("sandbox wait queue is full"),
        "unexpected overflow error: {overflow_error}"
    );

    dropped.disconnect();

    let replacement_deadline = Instant::now() + Duration::from_secs(8);
    let mut attempt = 0_u32;
    let (replacement_id, replacement, replacement_lines) = loop {
        ensure!(
            Instant::now() < replacement_deadline,
            "queued disconnect did not free queue capacity within 8 seconds"
        );
        attempt += 1;
        let run_id = harness.run_id(&format!("queue-replacement-{attempt}"));
        let mut stream = harness
            .open_run(basic_sleep_request(&run_id, 0.25, None))
            .await?;
        let first_line = stream.next(Duration::from_secs(3)).await?;
        if is_event(&first_line.value, "waiting_for_slot") {
            assert_queue_status(&first_line.value, 3)?;
            break (run_id, stream, vec![first_line]);
        }
        if first_line.value["type"] == "error"
            && first_line.value["message"]
                .as_str()
                .unwrap_or_default()
                .contains("sandbox wait queue is full")
        {
            sleep(Duration::from_millis(100)).await;
            continue;
        }
        bail!(
            "replacement request returned an unexpected first line: {}",
            first_line.value
        );
    };

    let holder_task =
        tokio::spawn(async move { drain_to_terminal(holder, holder_lines, STREAM_TIMEOUT).await });
    let first_task =
        tokio::spawn(async move { drain_to_terminal(first, first_lines, STREAM_TIMEOUT).await });
    let second_task =
        tokio::spawn(async move { drain_to_terminal(second, second_lines, STREAM_TIMEOUT).await });
    let replacement_task = tokio::spawn(async move {
        drain_to_terminal(replacement, replacement_lines, STREAM_TIMEOUT).await
    });

    let (holder_transcript, queued_transcripts) = timeout(Duration::from_secs(35), async move {
        let holder_transcript = holder_task.await.context("join holder stream task")??;
        let first_transcript = first_task.await.context("join first waiter task")??;
        let second_transcript = second_task.await.context("join second waiter task")??;
        let replacement_transcript = replacement_task
            .await
            .context("join replacement waiter task")??;
        Ok::<_, anyhow::Error>((
            holder_transcript,
            vec![
                (first_id, first_transcript),
                (second_id, second_transcript),
                (replacement_id, replacement_transcript),
            ],
        ))
    })
    .await
    .context("queue streams exceeded their hard timeout")??;

    ensure!(
        holder_transcript.result()?["exit_code"] == 0,
        "queue holder failed: {}",
        holder_transcript.result()?
    );
    let first_transcript = &queued_transcripts[0].1;
    let heartbeat_index = first_transcript
        .line_index(|value| value["type"] == "heartbeat")
        .context("first queued request received no heartbeat")?;
    let started_index = first_transcript
        .line_index(|value| is_event(value, "started"))
        .context("first queued request received no started event")?;
    ensure!(
        heartbeat_index < started_index,
        "first queued heartbeat arrived only after allocation: {:?}",
        first_transcript.values()
    );

    let mut previous_start = None;
    for (expected_id, transcript) in &queued_transcripts {
        let started = transcript
            .first_event("started")
            .with_context(|| format!("{expected_id} omitted started event"))?;
        ensure!(
            started.value["event"]["run_id"] == *expected_id,
            "started event used the wrong run_id: {}",
            started.value
        );
        if let Some(previous) = previous_start {
            ensure!(
                started.received_at > previous,
                "queued runs did not start in FIFO order: {:?}",
                queued_transcripts
                    .iter()
                    .map(|(id, transcript)| (id, transcript.first_event("started")))
                    .collect::<Vec<_>>()
            );
        }
        previous_start = Some(started.received_at);
        ensure!(
            transcript.result()?["exit_code"] == 0,
            "queued run failed: {}",
            transcript.result()?
        );
    }
    Ok(())
}

async fn active_disconnect_scenario(harness: &DockerHarness) -> Result<()> {
    let default_id = harness.run_id("disconnect-default");
    let default_request = basic_sleep_request(&default_id, 20.0, None);
    let mut default_stream = harness.open_raw_run(&default_request)?;
    default_stream.wait_for(
        Duration::from_secs(8),
        "default-disconnect started event",
        |value| is_event(value, "started"),
    )?;
    let default_disconnected_at = Instant::now();
    default_stream.disconnect();

    let default_probe = probe_transcript(harness, "disconnect-default-probe").await?;
    let default_probe_started = default_probe
        .first_event("started")
        .context("default disconnect probe omitted started event")?;
    ensure!(
        default_probe_started
            .received_at
            .duration_since(default_disconnected_at)
            < Duration::from_secs(5),
        "default-disconnected run did not release its slot promptly"
    );
    ensure!(
        default_probe.result()?["exit_code"] == 0,
        "default disconnect probe failed"
    );

    let continue_id = harness.run_id("disconnect-continue");
    let continue_request = basic_sleep_request(&continue_id, 2.5, Some(false));
    let mut continue_stream = harness.open_raw_run(&continue_request)?;
    continue_stream.wait_for(
        Duration::from_secs(8),
        "disconnect opt-out started event",
        |value| is_event(value, "started"),
    )?;
    let continue_disconnected_at = Instant::now();
    continue_stream.disconnect();

    let continue_probe = probe_transcript(harness, "disconnect-continue-probe").await?;
    ensure!(
        continue_probe.has_event("waiting_for_slot"),
        "opt-out probe never queued: {:?}",
        continue_probe.values()
    );
    let continue_probe_started = continue_probe
        .first_event("started")
        .context("opt-out probe omitted started event")?;
    let waited = continue_probe_started
        .received_at
        .duration_since(continue_disconnected_at);
    ensure!(
        waited >= Duration::from_millis(1_800),
        "kill_on_disconnect=false released its slot too early after {waited:?}"
    );
    ensure!(
        waited < Duration::from_secs(8),
        "kill_on_disconnect=false did not release its slot after natural completion: {waited:?}"
    );
    ensure!(
        continue_probe.result()?["exit_code"] == 0,
        "opt-out disconnect probe failed"
    );
    Ok(())
}

async fn enqueue_waiter(
    harness: &DockerHarness,
    run_id: &str,
    expected_position: usize,
) -> Result<(NdjsonResponse, Vec<ObservedLine>)> {
    let mut stream = harness
        .open_run(basic_sleep_request(run_id, 0.25, None))
        .await?;
    let mut lines = Vec::new();
    let index = collect_until(
        &mut stream,
        &mut lines,
        Duration::from_secs(8),
        "waiting_for_slot event",
        |value| is_event(value, "waiting_for_slot"),
    )
    .await?;
    assert_queue_status(&lines[index].value, expected_position)?;
    Ok((stream, lines))
}

fn assert_queue_status(value: &Value, expected_position: usize) -> Result<()> {
    ensure!(
        value["event"]["position"].as_u64() == Some(expected_position as u64),
        "expected queue position {expected_position}, got {value}"
    );
    ensure!(
        value["event"]["max_waiting"].as_u64() == Some(3),
        "server did not report max_waiting=3: {value}"
    );
    Ok(())
}

fn basic_sleep_request(run_id: &str, seconds: f64, kill_on_disconnect: Option<bool>) -> Value {
    let mut request = json!({
        "run_id": run_id,
        "command": [
            "sh",
            "-c",
            format!("sleep {seconds:.3}; printf '%s\\n' \"$E2E_RUN_ID\"")
        ],
        "env": [["E2E_RUN_ID", run_id]],
        "timeout_ms": ((seconds + 10.0) * 1_000.0).ceil() as u64,
        "result_policy": {"collect_paths": [], "result_file": null}
    });
    if let Some(value) = kill_on_disconnect {
        request["kill_on_disconnect"] = Value::Bool(value);
    }
    request
}

async fn quick_probe(harness: &DockerHarness, suffix: &str) -> Result<()> {
    let transcript = probe_transcript(harness, suffix).await?;
    ensure!(
        transcript.result()?["exit_code"] == 0,
        "probe failed: {}",
        transcript.result()?
    );
    Ok(())
}

async fn probe_transcript(harness: &DockerHarness, suffix: &str) -> Result<Transcript> {
    let run_id = harness.run_id(suffix);
    run_to_completion(
        harness,
        json!({
            "run_id": run_id,
            "command": ["sh", "-c", "printf 'probe-ok\\n'"],
            "timeout_ms": 10_000,
            "result_policy": {"collect_paths": [], "result_file": null}
        }),
        Duration::from_secs(12),
    )
    .await
}

async fn run_to_completion(
    harness: &DockerHarness,
    request: Value,
    deadline: Duration,
) -> Result<Transcript> {
    let stream = harness.open_run(request).await?;
    drain_to_terminal(stream, Vec::new(), deadline).await
}

async fn drain_to_terminal(
    mut stream: NdjsonResponse,
    mut lines: Vec<ObservedLine>,
    max_duration: Duration,
) -> Result<Transcript> {
    let deadline = Instant::now() + max_duration;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .context("NDJSON stream did not produce a terminal line before its deadline")?;
        let line = stream.next(remaining).await?;
        let terminal = matches!(line.value["type"].as_str(), Some("result" | "error"));
        lines.push(line);
        if terminal {
            return Ok(Transcript { lines });
        }
    }
}

async fn collect_until(
    stream: &mut NdjsonResponse,
    lines: &mut Vec<ObservedLine>,
    max_duration: Duration,
    description: &str,
    predicate: impl Fn(&Value) -> bool,
) -> Result<usize> {
    let deadline = Instant::now() + max_duration;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .with_context(|| {
                format!(
                    "timed out waiting for {description}; observed={:?}",
                    lines.iter().map(|line| &line.value).collect::<Vec<_>>()
                )
            })?;
        let line = stream.next(remaining).await?;
        let matched = predicate(&line.value);
        let terminal = matches!(line.value["type"].as_str(), Some("result" | "error"));
        lines.push(line);
        if matched {
            return Ok(lines.len() - 1);
        }
        if terminal {
            bail!(
                "stream terminated before {description}; observed={:?}",
                lines.iter().map(|line| &line.value).collect::<Vec<_>>()
            );
        }
    }
}

#[derive(Debug)]
struct ObservedLine {
    received_at: Instant,
    value: Value,
}

struct NdjsonResponse {
    response: reqwest::Response,
    buffered: Vec<u8>,
}

impl NdjsonResponse {
    async fn next(&mut self, max_wait: Duration) -> Result<ObservedLine> {
        let deadline = Instant::now() + max_wait;
        loop {
            if let Some(newline) = self.buffered.iter().position(|byte| *byte == b'\n') {
                let mut raw = self.buffered.drain(..=newline).collect::<Vec<_>>();
                while matches!(raw.last(), Some(b'\n' | b'\r')) {
                    raw.pop();
                }
                if raw.is_empty() {
                    continue;
                }
                let value: Value =
                    serde_json::from_slice(&raw).context("decode streamed NDJSON line")?;
                ensure!(value.is_object(), "NDJSON line was not an object: {value}");
                return Ok(ObservedLine {
                    received_at: Instant::now(),
                    value,
                });
            }

            let remaining = deadline
                .checked_duration_since(Instant::now())
                .context("timed out waiting for the next NDJSON line")?;
            match timeout(remaining, self.response.chunk())
                .await
                .context("timed out reading the NDJSON response body")??
            {
                Some(chunk) => self.buffered.extend_from_slice(&chunk),
                None if self.buffered.is_empty() => {
                    bail!("NDJSON response ended without a terminal line")
                }
                None => {
                    let raw = std::mem::take(&mut self.buffered);
                    let value: Value = serde_json::from_slice(&raw)
                        .context("decode unterminated final NDJSON line")?;
                    return Ok(ObservedLine {
                        received_at: Instant::now(),
                        value,
                    });
                }
            }
        }
    }
}

struct Transcript {
    lines: Vec<ObservedLine>,
}

impl Transcript {
    fn values(&self) -> Vec<&Value> {
        self.lines.iter().map(|line| &line.value).collect()
    }

    fn has_event(&self, event_type: &str) -> bool {
        self.lines
            .iter()
            .any(|line| is_event(&line.value, event_type))
    }

    fn first_event(&self, event_type: &str) -> Option<&ObservedLine> {
        self.lines
            .iter()
            .find(|line| is_event(&line.value, event_type))
    }

    fn line_index(&self, predicate: impl Fn(&Value) -> bool) -> Option<usize> {
        self.lines.iter().position(|line| predicate(&line.value))
    }

    fn result(&self) -> Result<&Value> {
        self.lines
            .iter()
            .find_map(|line| (line.value["type"] == "result").then_some(&line.value["result"]))
            .context("stream did not contain a result line")
    }

    fn error_message(&self) -> Result<&str> {
        self.lines
            .iter()
            .find_map(|line| {
                (line.value["type"] == "error")
                    .then(|| line.value["message"].as_str())
                    .flatten()
            })
            .context("stream did not contain an error line")
    }

    fn event_bytes(&self, event_type: &str) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        for line in &self.lines {
            if is_event(&line.value, event_type) {
                output.extend(json_byte_array(&line.value["event"]["data"])?);
            }
        }
        Ok(output)
    }
}

fn is_event(value: &Value, event_type: &str) -> bool {
    value["type"] == "event" && value["event"]["type"] == event_type
}

fn json_byte_array(value: &Value) -> Result<Vec<u8>> {
    value
        .as_array()
        .context("expected a JSON byte array")?
        .iter()
        .map(|byte| {
            byte.as_u64()
                .filter(|byte| *byte <= u8::MAX as u64)
                .map(|byte| byte as u8)
                .context("JSON byte array contained a non-byte value")
        })
        .collect()
}

struct RawNdjsonResponse {
    socket: TcpStream,
    reader: BufReader<TcpStream>,
    chunked: bool,
    decoded: Vec<u8>,
    finished: bool,
}

impl RawNdjsonResponse {
    fn connect(address: SocketAddr, token: &str, request: &Value) -> Result<Self> {
        let body = serde_json::to_vec(request)?;
        let mut socket = TcpStream::connect_timeout(&address, Duration::from_secs(5))
            .with_context(|| format!("connect raw HTTP client to {address}"))?;
        socket.set_nodelay(true)?;
        socket.set_read_timeout(Some(Duration::from_secs(8)))?;
        socket.set_write_timeout(Some(Duration::from_secs(5)))?;
        let request_head = format!(
            "POST /api/run HTTP/1.1\r\n\
             Host: {address}\r\n\
             Authorization: Bearer {token}\r\n\
             Accept: application/x-ndjson\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\
             \r\n",
            body.len()
        );
        socket.write_all(request_head.as_bytes())?;
        socket.write_all(&body)?;
        socket.flush()?;

        let reader_socket = socket.try_clone()?;
        let mut reader = BufReader::new(reader_socket);
        let mut status_line = String::new();
        reader.read_line(&mut status_line)?;
        let status = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|value| value.parse::<u16>().ok())
            .with_context(|| format!("invalid raw HTTP status line {status_line:?}"))?;
        ensure!(status == 200, "raw run request returned {status_line:?}");

        let mut chunked = false;
        loop {
            let mut line = String::new();
            ensure!(
                reader.read_line(&mut line)? != 0,
                "raw HTTP response ended inside headers"
            );
            if line == "\r\n" || line == "\n" {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("transfer-encoding")
                && value
                    .split(',')
                    .any(|coding| coding.trim().eq_ignore_ascii_case("chunked"))
            {
                chunked = true;
            }
        }

        Ok(Self {
            socket,
            reader,
            chunked,
            decoded: Vec::new(),
            finished: false,
        })
    }

    fn wait_for(
        &mut self,
        max_duration: Duration,
        description: &str,
        predicate: impl Fn(&Value) -> bool,
    ) -> Result<Value> {
        let deadline = Instant::now() + max_duration;
        let mut observed = Vec::new();
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .with_context(|| {
                    format!("timed out waiting for {description}; observed={observed:?}")
                })?;
            self.reader
                .get_mut()
                .set_read_timeout(Some(remaining.min(Duration::from_secs(2))))?;
            let value = self.next()?;
            if predicate(&value) {
                return Ok(value);
            }
            if matches!(value["type"].as_str(), Some("result" | "error")) {
                bail!(
                    "raw response terminated before {description}; terminal={value}; observed={observed:?}"
                );
            }
            observed.push(value);
        }
    }

    fn next(&mut self) -> Result<Value> {
        loop {
            if let Some(newline) = self.decoded.iter().position(|byte| *byte == b'\n') {
                let mut raw = self.decoded.drain(..=newline).collect::<Vec<_>>();
                while matches!(raw.last(), Some(b'\n' | b'\r')) {
                    raw.pop();
                }
                if raw.is_empty() {
                    continue;
                }
                let value: Value =
                    serde_json::from_slice(&raw).context("decode raw NDJSON line")?;
                ensure!(value.is_object(), "raw NDJSON line was not an object");
                return Ok(value);
            }
            ensure!(!self.finished, "raw response ended without a terminal line");
            self.read_body_chunk()?;
        }
    }

    fn read_body_chunk(&mut self) -> Result<()> {
        if !self.chunked {
            let mut raw = Vec::new();
            let read = self.reader.read_until(b'\n', &mut raw)?;
            if read == 0 {
                self.finished = true;
            } else {
                self.decoded.extend(raw);
            }
            return Ok(());
        }

        let mut size_line = String::new();
        ensure!(
            self.reader.read_line(&mut size_line)? != 0,
            "chunked response ended before a chunk size"
        );
        let size_text = size_line
            .trim_end_matches(['\r', '\n'])
            .split(';')
            .next()
            .unwrap_or_default();
        let size = usize::from_str_radix(size_text, 16)
            .with_context(|| format!("invalid HTTP chunk size {size_line:?}"))?;
        ensure!(
            size <= 4 * 1024 * 1024,
            "unreasonably large HTTP chunk: {size}"
        );
        if size == 0 {
            loop {
                let mut trailer = String::new();
                if self.reader.read_line(&mut trailer)? == 0 || trailer == "\r\n" || trailer == "\n"
                {
                    break;
                }
            }
            self.finished = true;
            return Ok(());
        }

        let start = self.decoded.len();
        self.decoded.resize(start + size, 0);
        self.reader.read_exact(&mut self.decoded[start..])?;
        let mut delimiter = [0_u8; 2];
        self.reader.read_exact(&mut delimiter)?;
        ensure!(delimiter == *b"\r\n", "HTTP chunk was not followed by CRLF");
        Ok(())
    }

    fn disconnect(self) {
        let _ = self.socket.shutdown(Shutdown::Both);
    }
}

struct DockerResources {
    image: String,
    container: String,
    label: String,
    image_built: bool,
    container_created: bool,
}

impl DockerResources {
    fn build_image(&mut self) -> Result<()> {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let dockerfile = manifest_dir.join("tests/e2e/Dockerfile");
        let mut command = Command::new("docker");
        command
            .arg("build")
            .arg("--file")
            .arg(&dockerfile)
            .arg("--tag")
            .arg(&self.image)
            .arg("--label")
            .arg(&self.label)
            .arg("--progress")
            .arg("plain")
            .arg(&manifest_dir);
        checked_command(command, BUILD_TIMEOUT, "docker build")?;
        self.image_built = true;
        Ok(())
    }

    fn start_container(&mut self, token: &str, isolation: IsolationProfile) -> Result<()> {
        // `docker run` performs create and start as separate daemon operations.
        // Claim cleanup ownership before invoking it so an OCI start failure
        // cannot strand a uniquely named container in the "created" state.
        self.container_created = true;
        let mut command = Command::new("docker");
        command
            .arg("run")
            .arg("--detach")
            .arg("--name")
            .arg(&self.container)
            .arg("--label")
            .arg(&self.label)
            .arg("--cap-drop")
            .arg("ALL")
            .arg("--cap-add")
            .arg("SETPCAP")
            .arg("--cap-add")
            .arg("SETUID")
            .arg("--cap-add")
            .arg("SETGID")
            .arg("--cap-add")
            .arg("CHOWN")
            .arg("--cap-add")
            .arg("FOWNER")
            .arg("--cap-add")
            .arg("DAC_OVERRIDE")
            .arg("--cap-add")
            .arg("KILL");
        if isolation == IsolationProfile::Privileged {
            command
                .arg("--cap-add")
                .arg("SYS_ADMIN")
                .arg("--security-opt")
                .arg("seccomp=unconfined")
                .arg("--security-opt")
                .arg("apparmor=unconfined");
        }
        command
            .arg("--tmpfs")
            .arg("/run:rw,nosuid,nodev,size=64m")
            .arg("--tmpfs")
            .arg("/tmp:rw,nosuid,nodev,size=128m")
            .arg("--tmpfs")
            .arg("/var/lib/agentic-sandbox:rw,nosuid,nodev,size=128m,mode=0711")
            .arg("--publish")
            .arg("127.0.0.1::8080")
            .arg(&self.image)
            .arg("/usr/local/bin/agentic-sandbox")
            .arg("serve")
            .arg("--root")
            .arg("/var/lib/agentic-sandbox")
            .arg("--slots")
            .arg("1")
            .arg("--max-waiting")
            .arg("3")
            .arg("--socket")
            .arg("/var/lib/agentic-sandbox/state/master.sock")
            .arg("--disable-proxy")
            .arg("--http")
            .arg("0.0.0.0:8080")
            .arg("--http-token")
            .arg(token)
            .arg("--isolation")
            .arg(isolation.as_str());
        checked_command(command, DOCKER_COMMAND_TIMEOUT, "docker run")?;
        Ok(())
    }

    fn mapped_port(&self) -> Result<u16> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut last_error = None;
        while Instant::now() < deadline {
            let mut command = Command::new("docker");
            command.arg("port").arg(&self.container).arg("8080/tcp");
            match checked_command(command, Duration::from_secs(5), "docker port") {
                Ok(output) => {
                    if let Some(port) = parse_mapped_port(&output) {
                        return Ok(port);
                    }
                    last_error = Some(format!("unrecognized mapping {output:?}"));
                }
                Err(error) => last_error = Some(format!("{error:#}")),
            }
            thread::sleep(Duration::from_millis(100));
        }
        bail!(
            "Docker did not report a mapped port: {}",
            last_error.unwrap_or_else(|| "no output".to_string())
        )
    }

    fn logs(&self) -> String {
        if !self.container_created {
            return "(container was not created)".to_string();
        }
        let mut command = Command::new("docker");
        command.arg("logs").arg(&self.container);
        match run_command(command, DOCKER_COMMAND_TIMEOUT) {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                format!("stdout:\n{stdout}\nstderr:\n{stderr}")
            }
            Err(error) => format!("unable to read container logs: {error:#}"),
        }
    }

    fn cleanup(&mut self, strict: bool) -> Result<()> {
        let mut errors = Vec::new();
        if self.container_created {
            let mut command = Command::new("docker");
            command.arg("rm").arg("--force").arg(&self.container);
            if let Err(error) =
                checked_command(command, DOCKER_COMMAND_TIMEOUT, "docker rm --force")
            {
                errors.push(format!("{error:#}"));
            } else {
                self.container_created = false;
            }
        }
        if self.image_built {
            let mut command = Command::new("docker");
            command
                .arg("image")
                .arg("rm")
                .arg("--force")
                .arg(&self.image);
            if let Err(error) = checked_command(command, DOCKER_COMMAND_TIMEOUT, "docker image rm")
            {
                errors.push(format!("{error:#}"));
            } else {
                self.image_built = false;
            }
        }
        if strict && !errors.is_empty() {
            bail!("{}", errors.join("\n"));
        }
        if !strict {
            for error in errors {
                eprintln!("best-effort Docker E2E cleanup failed: {error}");
            }
        }
        Ok(())
    }
}

impl Drop for DockerResources {
    fn drop(&mut self) {
        if thread::panicking() {
            eprintln!("Container logs during panic:\n{}", self.logs());
        }
        let _ = self.cleanup(false);
    }
}

fn check_docker() -> Result<()> {
    let mut command = Command::new("docker");
    command
        .arg("version")
        .arg("--format")
        .arg("{{.Server.Version}}");
    checked_command(command, DOCKER_COMMAND_TIMEOUT, "docker version").map(|_| ())
}

fn parse_mapped_port(output: &str) -> Option<u16> {
    output.lines().find_map(|line| {
        let (host, port) = line.trim().rsplit_once(':')?;
        (host == "127.0.0.1").then(|| port.parse().ok()).flatten()
    })
}

struct CommandOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn checked_command(command: Command, max_duration: Duration, label: &str) -> Result<String> {
    let output = run_command(command, max_duration)?;
    ensure!(
        output.status.success(),
        "{label} failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn run_command(mut command: Command, max_duration: Duration) -> Result<CommandOutput> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().context("spawn subprocess")?;
    let stdout = child.stdout.take().context("capture subprocess stdout")?;
    let stderr = child.stderr.take().context("capture subprocess stderr")?;
    let stdout_thread = thread::spawn(move || {
        let mut bytes = Vec::new();
        BufReader::new(stdout)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let stderr_thread = thread::spawn(move || {
        let mut bytes = Vec::new();
        BufReader::new(stderr)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });

    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().context("poll subprocess")? {
            break status;
        }
        if started.elapsed() >= max_duration {
            let _ = child.kill();
            let _ = child.wait();
            let stdout = stdout_thread
                .join()
                .map_err(|_| anyhow!("stdout reader thread panicked"))??;
            let stderr = stderr_thread
                .join()
                .map_err(|_| anyhow!("stderr reader thread panicked"))??;
            bail!(
                "subprocess exceeded {max_duration:?}\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&stdout),
                String::from_utf8_lossy(&stderr)
            );
        }
        thread::sleep(Duration::from_millis(50));
    };

    let stdout = stdout_thread
        .join()
        .map_err(|_| anyhow!("stdout reader thread panicked"))??;
    let stderr = stderr_thread
        .join()
        .map_err(|_| anyhow!("stderr reader thread panicked"))??;
    Ok(CommandOutput {
        status,
        stdout,
        stderr,
    })
}
