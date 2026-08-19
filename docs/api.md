# API reference

Agentic Sandbox exposes a Unix-socket API by default and can optionally expose a
development HTTP API. Both accept the same `AgentRunRequest` and stream
newline-delimited JSON.

## HTTP API

Start it with `agentic-sandbox serve --http 127.0.0.1:8080`. If
`--http-token` or `AGENTIC_SANDBOX_HTTP_TOKEN` is set, all API requests require:

```http
Authorization: Bearer <token>
```

| Method | Path | Response |
| --- | --- | --- |
| `GET` | `/api/health` | HTTP 200 `{"status":"ok"}`; HTTP 503 while shutting down |
| `POST` | `/api/run` | `application/x-ndjson` stream |
| `GET` | `/` | Embedded development UI |

Example:

```bash
curl --no-buffer \
  -H 'Authorization: Bearer change-me-dev-token' \
  -H 'Content-Type: application/json' \
  --data @request.json \
  http://127.0.0.1:8080/api/run
```

HTTP stream lines wrap run events:

```jsonl
{"type":"event","event":{"type":"started","run_id":"run-001","slot_id":"slot-0001","pid":123}}
{"type":"event","event":{"type":"stdout","data":[104,101,108,108,111,10]}}
{"type":"heartbeat"}
{"type":"result","result":{"run_id":"run-001","slot_id":"slot-0001","exit_code":0}}
```

The examples abbreviate the final result. Actual result objects include every field
documented below.

The HTTP API is a development interface. It has bearer-token authentication but no
TLS, rate limiting, or authorization scopes. `POST /api/run` requires
`Content-Type: application/json` and enforces a 2 MiB body limit. Authentication
is checked before the request body is buffered. Keep it on loopback or place it
behind a trusted authenticated gateway.

## Unix-socket API

The default socket is `/var/lib/agentic-sandbox/state/master.sock`. Send exactly
one JSON request followed by a newline per connection:

```json
{"type":"health"}
```

```json
{"type":"run","request":{"run_id":"run-001","command":["/usr/bin/id"]}}
```

Health returns one line:

```json
{"type":"health","status":"ok"}
```

Run lifecycle events are emitted directly on the Unix stream, followed by a
wrapped final result or error:

```jsonl
{"type":"started","run_id":"run-001","slot_id":"slot-0001","pid":123}
{"type":"stdout","data":[104,101,108,108,111,10]}
{"type":"result","result":{"run_id":"run-001","slot_id":"slot-0001","exit_code":0}}
```

Protect access to the socket with parent-directory ownership and permissions. The
server currently relies on filesystem access control and does not inspect peer
credentials.

## Run request

Minimal request:

```json
{
  "run_id": "run-001",
  "command": ["bash", "-lc", "echo hello"]
}
```

Full example:

```json
{
  "run_id": "run-001",
  "command": ["bash", "-lc", "cat input/task.md; echo '{\"ok\":true}' > result.json"],
  "injected_files": [
    {
      "relative_path": "input/task.md",
      "content_utf8": "Complete this task",
      "mode": 420
    }
  ],
  "injected_readonly_dirs": [],
  "env": [["SAFE_NAME", "value"]],
  "inject_headers": [],
  "timeout_ms": 300000,
  "kill_on_disconnect": true,
  "limits": {
    "memory_max_bytes": null,
    "pids_max": null,
    "cpu_quota": null,
    "stdout_max_bytes": 131072,
    "stderr_max_bytes": 131072
  },
  "result_policy": {
    "collect_paths": [],
    "result_file": "result.json"
  },
  "cleanup_policy": "DeleteWorkspaceAfterRun"
}
```

### Fields

| Field | Required | Default | Notes |
| --- | --- | --- | --- |
| `run_id` | yes | — | One path-safe component, at most 128 bytes; separators, `..`, NUL, and control characters are rejected |
| `command` | yes | — | Exact argv vector; no implicit shell |
| `injected_files` | no | `[]` | Files written below the workspace |
| `injected_readonly_dirs` | no | `[]` | Privileged mode only; host source bound at an absolute sandbox path |
| `env` | no | `[]` | Array of `[name, value]`; sensitive keys are filtered |
| `inject_headers` | no | `[]` | Proxy header or upstream-routing rules |
| `timeout_ms` | no | `300000` | Wall-clock timeout |
| `kill_on_disconnect` | no | `true` | Cancels queued or active work when the stream closes |
| `limits` | no | empty | Output-tail limits work; CPU, memory, and PID limits are not yet enforced |
| `result_policy` | no | see below | Selects files and structured JSON to return |
| `cleanup_policy` | no | `DeleteWorkspaceAfterRun` | Serde enum spelling is case-sensitive |

`injected_files[].content` accepts a JSON byte array. The alias `content_utf8`
accepts a UTF-8 string and is more convenient for text. File modes are numeric;
decimal `420` is octal `0644`.

Unknown fields in the request and its nested request objects are rejected. This
prevents misspelled security or timeout settings from being silently ignored.

The browser currently allows selecting up to 24 MiB of files, but the HTTP
server's JSON body limit is smaller. Treat 2 MiB as the effective total request
limit until the UI and server limits are aligned.

Cleanup policy values:

- `DeleteWorkspaceAfterRun`
- `PreserveWorkspaceForDebug`
- `PreserveWorkspaceOnFailure`

### Result collection

`result_policy.collect_paths` contains workspace-relative file paths to return.
`result_policy.result_file` is parsed as JSON and placed in `result_json`; it
defaults to `result.json`. A missing or invalid JSON file produces `null`. Set the
field to `null` to disable structured-result parsing.

At most 256 paths may be collected. Each collected file and the structured result
file are limited to 16 MiB, and collected files are limited to 64 MiB in total.
Symlinks that resolve outside the workspace are omitted.

The result object contains:

| Field | Meaning |
| --- | --- |
| `run_id`, `slot_id` | Request and allocated slot identifiers |
| `exit_code`, `signal` | Child termination status |
| `stdout_tail`, `stderr_tail` | Bounded byte arrays containing the end of each stream |
| `collected_files` | Requested relative path, bytes, and size |
| `result_json` | Parsed result file, or `null` |
| `workspace_preserved` | Whether cleanup policy retained the slot workspace |

## Run events

Run streams can emit:

- `waiting_for_slot`
- `started`
- `stdout`
- `stderr`
- `agent_event`
- `result_file_found`
- `exited`
- `shutting_down`
- `timed_out`
- `failed`

The current supervisor emits stdout/stderr and lifecycle events; `agent_event` is
part of the wire type but is not currently produced by the launcher.

`shutting_down` is emitted after the service successfully forwards SIGINT or
SIGTERM to the run's process group. Its numeric `signal` field preserves the
forwarded Unix signal, for example:

```json
{"type":"shutting_down","signal":15}
```

The subsequent `exited` event describes how the direct child actually exited. A
child that catches SIGTERM and exits normally can therefore report
`{"exit_code":0,"signal":null}`.

Both APIs send `{"type":"heartbeat"}` every 15 seconds. These keep idle queued or
quiet-running streams observable and also help detect disconnected clients.

After the run's child exits, the server flushes any buffered lifecycle events
(`shutting_down`, `exited`, `result_file_found`) before writing the terminal
`result`/`error` line. The HTTP handler bounds this flush at 2 seconds; on the
rare occasion it is exceeded the server logs a warning and still emits the
terminal line so the response body is complete rather than truncated. The
`timeout_ms` request field bounds only the child's execution—event flush and
result collection always run to completion afterward.

## Direct JSON command

For local debugging without either API:

```bash
agentic-sandbox run-json \
  --root /tmp/agentic-sandbox \
  --request request.json \
  --isolation user
```

This runs with one slot and prints the final result as formatted JSON.
