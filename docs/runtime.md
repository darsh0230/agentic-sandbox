# Runtime and configuration

The supported deployment target is a Linux container on kernel 5.6 or newer.
Secure artifact resolution uses `openat2`; startup or collection fails closed when
that syscall is unavailable. macOS and other Unix hosts can compile and run most
tests, but they do not execute the Linux isolation setup.

## Docker Compose

The Compose configuration exposes separate profiles so user mode does not inherit
privileged-only container capabilities. Start exactly one profile:

```bash
# Lower container privileges; weaker sandbox isolation
docker compose --profile user up agentic-sandbox-user

# Mount, IPC, and UTS namespace isolation; requires SYS_ADMIN
docker compose --profile privileged up agentic-sandbox-privileged
```

Compose uses `ghcr.io/darsh0230/agentic-sandbox:v0.1.0` by default. Override the
image with `AGENTIC_SANDBOX_IMAGE`. If the image is unavailable, Compose falls
back to the checked-in build definition. To force a build of the current
checkout:

```bash
docker compose --profile user up --build agentic-sandbox-user
```

Open <http://127.0.0.1:8080> and use `change-me-dev-token`. Override the token
before any shared deployment:

```bash
AGENTIC_SANDBOX_HTTP_TOKEN='replace-with-a-secret' \
  docker compose --profile user up agentic-sandbox-user
```

The checked-in Compose file uses `change-me-dev-token` only as a fallback and
passes through `AGENTIC_SANDBOX_HTTP_TOKEN`, `AGENTIC_SANDBOX_RUST_LOG`, and
`AGENTIC_SANDBOX_HTTP_PORT` when they are set in the shell. Both profiles share
the state volume and host port, so do not start them simultaneously. User mode
omits `SYS_ADMIN` and the unconfined container seccomp/AppArmor overrides;
privileged mode enables them for namespace and mount setup.

## Direct Docker: privileged mode

```bash
docker network create agentic-sandbox-net
docker volume create agentic-sandbox-state

docker run --rm -it \
  --name agentic-sandbox \
  --network agentic-sandbox-net \
  --cap-drop=ALL \
  --cap-add=SYS_ADMIN \
  --cap-add=SETPCAP \
  --cap-add=SETUID \
  --cap-add=SETGID \
  --cap-add=CHOWN \
  --cap-add=FOWNER \
  --cap-add=DAC_OVERRIDE \
  --cap-add=KILL \
  --security-opt seccomp=unconfined \
  --security-opt apparmor=unconfined \
  --tmpfs /run:rw,nosuid,nodev,size=64m \
  --tmpfs /tmp:rw,nosuid,nodev,size=256m \
  -e AGENTIC_SANDBOX_HTTP_TOKEN=replace-with-a-secret \
  -p 127.0.0.1:8080:8080 \
  -v agentic-sandbox-state:/var/lib/agentic-sandbox \
  ghcr.io/darsh0230/agentic-sandbox:v0.1.0 \
  agentic-sandbox serve --isolation privileged --http 0.0.0.0:8080
```

The unconfined seccomp and AppArmor settings are development conveniences because
Docker's default profiles can block namespace and mount setup. For production,
replace them with reviewed control-plane profiles that permit only the required
setup operations, including `unshare`, `mount`, `umount2`, and `pivot_root`. Do
not block the API listener syscalls at container level; the child installs its own
filter after dropping privileges.

## Direct Docker: user mode

```bash
docker volume create agentic-sandbox-state

docker run --rm -it \
  --name agentic-sandbox-user \
  --cap-drop=ALL \
  --cap-add=SETPCAP \
  --cap-add=SETUID \
  --cap-add=SETGID \
  --cap-add=CHOWN \
  --cap-add=FOWNER \
  --cap-add=DAC_OVERRIDE \
  --cap-add=KILL \
  -e AGENTIC_SANDBOX_ISOLATION=user \
  -e AGENTIC_SANDBOX_HTTP_TOKEN=replace-with-a-secret \
  -p 127.0.0.1:8080:8080 \
  -v agentic-sandbox-state:/var/lib/agentic-sandbox \
  ghcr.io/darsh0230/agentic-sandbox:v0.1.0 \
  agentic-sandbox serve --http 0.0.0.0:8080
```

This mode does not need `CAP_SYS_ADMIN` or unconfined container security profiles,
but it is a substantially weaker isolation boundary. See
[Security](security.md#user-mode).

## CLI and environment

`agentic-sandbox serve` options:

| CLI | Environment | Default |
| --- | --- | --- |
| `--root <PATH>` | `AGENTIC_SANDBOX_ROOT` | `/var/lib/agentic-sandbox` |
| `--slots <COUNT>` | — | `8` |
| `--max-waiting <COUNT>` | `AGENTIC_SANDBOX_MAX_WAITING` | `10 × slots` |
| `--shutdown-grace-ms <MILLISECONDS>` | `AGENTIC_SANDBOX_SHUTDOWN_GRACE_MS` | `25000` |
| `--socket <PATH>` | — | `/var/lib/agentic-sandbox/state/master.sock` |
| `--config <PATH>` | — | none; currently accepted and logged but not loaded |
| `--http <ADDR>` | `AGENTIC_SANDBOX_HTTP_ADDR` | disabled |
| `--http-token <TOKEN>` | `AGENTIC_SANDBOX_HTTP_TOKEN` | none |
| `--isolation <MODE>` | `AGENTIC_SANDBOX_ISOLATION` | `privileged` |
| `--proxy-listen <ADDR>` | `AGENTIC_SANDBOX_PROXY_LISTEN` | `127.0.0.1:18080` |
| `--proxy-config <PATH>` | `AGENTIC_SANDBOX_PROXY_CONFIG` | optional default path |
| `--disable-proxy` | `AGENTIC_SANDBOX_DISABLE_PROXY` | `false` |
| `--enable-proxy-mitm` | `AGENTIC_SANDBOX_ENABLE_PROXY_MITM` | `false` |
| `--proxy-ca-cert <PATH>` | `AGENTIC_SANDBOX_PROXY_CA_CERT` | `state/proxy-ca.pem` |
| `--proxy-ca-key <PATH>` | `AGENTIC_SANDBOX_PROXY_CA_KEY` | `state/proxy-ca-key.pem` |

Use `--max-waiting 0` to reject requests immediately when all slots are occupied.

The shutdown grace applies to both `serve` and `run-json`. The first SIGINT or
SIGTERM stops run admission and forwards the same signal to every active slot
process group. A second shutdown signal or expiry of the grace period sends
SIGKILL to remaining groups. Set the value below the outer container or service
manager's termination deadline so result collection and workspace cleanup retain
time to finish.

## Kubernetes termination

The supplied image declares `STOPSIGNAL SIGTERM` and keeps Tini as PID 1. During
normal Pod termination, Tini forwards SIGTERM to `agentic-sandbox`; the service
then drains its listeners and forwards SIGTERM to every active slot process
group. No `preStop` hook is required for signal propagation.

Set the Pod grace period longer than the internal grace. With the default 25
second service grace, a value such as 40 seconds leaves time for result
collection, workspace cleanup, and container exit:

```yaml
spec:
  terminationGracePeriodSeconds: 40
  containers:
    - name: agentic-sandbox
      image: ghcr.io/darsh0230/agentic-sandbox:v0.1.0
      args:
        - /usr/local/bin/agentic-sandbox
        - serve
        - --http
        - 0.0.0.0:8080
        - --shutdown-grace-ms
        - "25000"
```

Use `args`, as above, to retain the image's Tini entrypoint. If a deployment
overrides `command`, it must provide an equivalent init and signal-forwarding
chain.

For user isolation, the control plane starts as root, changes each slot to its
dedicated UID/GID, and needs these capabilities. `KILL` is required for both
graceful forwarding and forced cleanup:

```yaml
securityContext:
  capabilities:
    drop: ["ALL"]
    add:
      - SETPCAP
      - SETUID
      - SETGID
      - CHOWN
      - FOWNER
      - DAC_OVERRIDE
      - KILL
env:
  - name: AGENTIC_SANDBOX_ISOLATION
    value: user
```

Privileged isolation additionally requires `SYS_ADMIN` and container security
profiles that allow its namespace and mount setup. The simplest Kubernetes
configuration is `securityContext.privileged: true`; a hardened deployment
should instead grant only the documented capabilities and reviewed seccomp and
AppArmor permissions, including `KILL`.

When draining begins, `/api/health` returns HTTP 503 with
`{"status":"shutting_down"}`, new runs are rejected, and queued or pre-launch
runs are canceled. Existing run streams remain open until their slots exit or
the grace period expires. Kubernetes also marks terminating Pod endpoints not
ready, but the application state protects traffic arriving over existing or
non-Service connections.

The proxy config defaults to
`/var/lib/agentic-sandbox/state/proxy-headers.json`. A missing default file is
allowed; a missing explicitly supplied file is an error.

## Installed agent tools

The runtime image installs:

- `@earendil-works/pi-coding-agent` (pinned via `PI_CODING_AGENT_VERSION`)
- `opencode-ai` (pinned via `OPENCODE_VERSION`)

Tool and npm versions are pinned in the Dockerfile, and vulnerable transitive
dependencies are overridden with patched releases; bump the corresponding
`ARG` values to update. Base-image digests are still unpinned; pin them before
producing reproducible or security-sensitive releases. Agentic Sandbox
executes the request's argv and does not require either tool; a plain shell
command is sufficient for smoke testing.

Non-interactive agent commands currently use these forms:

```bash
opencode run --auto "Fix the failing tests"
pi -p "Fix the failing tests"
```

Both tools require a configured model provider. The
[agent examples](../examples/README.md) show how a trusted local client can pass a
provider credential in a run request without committing it to disk.

## Published images

The `Publish container image` workflow runs for tags matching `v*.*.*`. A tag
such as `v0.1.0` publishes:

- `ghcr.io/darsh0230/agentic-sandbox:v0.1.0`;
- `ghcr.io/darsh0230/agentic-sandbox:0.1.0`;
- `ghcr.io/darsh0230/agentic-sandbox:0.1`;
- `ghcr.io/darsh0230/agentic-sandbox:latest` for non-prerelease tags; and
- an immutable commit-SHA tag.

After the image succeeds, the workflow creates a GitHub release with generated
release notes unless that tag already has a release. The current workflow
publishes `linux/amd64` only and attaches BuildKit provenance and an SBOM.
Multi-architecture publishing remains a roadmap item.

## Logging

Logs use `tracing_subscriber` and honor `RUST_LOG`. The default filter is:

```text
agentic_sandbox=info,info
```

Proxy logs include method, redacted target URL, rule match counts, status, and
duration. They do not intentionally log injected header values or run proxy
tokens.

## Development workflow

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
docker compose config --quiet
```

To inspect the CLI:

```bash
cargo run -- --help
cargo run -- serve --help
```

Run the self-contained Linux E2E suite to exercise both isolation modes. The
test builds `tests/e2e/Dockerfile`, uses unique container and image names, maps a
dynamic loopback port, captures logs on failure, and removes its resources:

```bash
cargo test --test docker_e2e -- --ignored --test-threads=1
```

The E2E target is ignored during ordinary `cargo test` runs because it requires a
Docker daemon, privileged container capabilities, and a deliberate wait through
the production 15-second heartbeat interval. Non-Linux unit tests use a
lightweight launcher fallback and cannot validate namespace, mount, credential,
or seccomp setup.
