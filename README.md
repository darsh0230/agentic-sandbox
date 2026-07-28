# Agentic Sandbox

Agentic Sandbox runs coding-agent commands in reusable, isolated workspaces inside a
single Linux container. A Rust control plane accepts run requests, allocates a slot,
injects input files, streams output, collects declared artifacts, and cleans up the
workspace.

> [!WARNING]
> This project is under active development. Read the
> [security model](docs/security.md) before using it with untrusted workloads.
> User isolation mode is a filesystem-permissions boundary, and neither isolation
> mode currently enforces CPU, memory, or process-count limits.

## Features

- Bounded FIFO scheduling across reusable sandbox slots
- Privileged isolation using mount, IPC, and UTS namespaces plus `pivot_root`
- Lower-privilege user isolation using a dedicated UID/GID per active slot
- Unix-socket control API and an optional browser/HTTP development API
- Streaming stdout, stderr, lifecycle events, and 15-second heartbeats
- Input-file injection and explicit result/artifact collection
- Per-run timeouts, disconnect cancellation, and cleanup policies
- Run-scoped outbound proxy credentials, header overrides, upstream proxy routing,
  and optional HTTPS interception

## Quick start

Requirements:

- Docker with Compose
- A Linux container host for the isolation features

Start exactly one isolation profile.

User mode runs the control plane without `SYS_ADMIN`:

```bash
docker compose --profile user up --build agentic-sandbox-user
```

Privileged mode enables mount, IPC, and UTS namespace isolation:

```bash
docker compose --profile privileged up --build agentic-sandbox-privileged
```

Open <http://127.0.0.1:8080> and use the development token
`change-me-dev-token`.

The Compose file is intended for local evaluation. Change the token before sharing
the service, keep the HTTP listener bound to loopback, do not start both profiles
at once because they share a port and state volume, and do not treat the browser API
as a hardened production control plane.

To stop the service:

```bash
docker compose --profile user down
# or: docker compose --profile privileged down
```

## Minimal API example

With the Compose service running:

```bash
curl --no-buffer \
  -H 'Authorization: Bearer change-me-dev-token' \
  -H 'Content-Type: application/json' \
  --data '{
    "run_id": "hello-001",
    "command": [
      "bash",
      "-lc",
      "mkdir -p output; echo hello; printf \"{\\\"status\\\":\\\"ok\\\"}\" > result.json; echo done > output/result.txt"
    ],
    "result_policy": {
      "collect_paths": ["output/result.txt"],
      "result_file": "result.json"
    }
  }' \
  http://127.0.0.1:8080/api/run
```

The response is newline-delimited JSON containing lifecycle events, output,
heartbeats, and a final result.

## Documentation

All detailed documentation lives in [`docs/`](docs/README.md):

- [Runtime and configuration](docs/runtime.md)
- [API reference](docs/api.md)
- [Architecture](docs/architecture.md)
- [Outbound proxy](docs/proxy.md)
- [Security model and known limitations](docs/security.md)

## Development

The host build works on non-Linux systems for development and unit tests, but the
real sandbox setup is Linux-specific.

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
docker compose config --quiet
```

The proxy integration tests bind loopback sockets and may require local-network
permission in restricted development environments.

The Docker-backed E2E suite is intentionally ignored by the default test run. It
builds a lean Linux image, owns disposable user- and privileged-isolation
containers, and validates HTTP authentication, artifacts, timeouts, queueing,
heartbeats, disconnect cancellation, process-tree cleanup, and slot reuse:

```bash
cargo test --test docker_e2e -- --ignored --test-threads=1
```

This command requires a working Docker daemon and can take over a minute because
one queue test waits for the production 15-second heartbeat.

## Vibe-coded project

This project is vibe coded: its code and documentation were developed
iteratively with AI coding assistants under human direction and review. That
history is disclosed for transparency. As with any security-sensitive software,
review and test the implementation carefully before relying on it in production.

## License

Agentic Sandbox is available under the [MIT License](LICENSE).
