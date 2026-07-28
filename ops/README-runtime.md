# Runtime

Build and run in privileged isolation mode:

```bash
docker build -t pi-sandbox:dev .
docker network create pi-sandbox-net || true
docker volume create pi-sandbox-state || true
docker run --rm -it \
  --name pi-sandbox \
  --network pi-sandbox-net \
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
  --tmpfs /run:rw,nosuid,nodev,size=64m \
  --tmpfs /tmp:rw,nosuid,nodev,size=256m \
  -e PI_SANDBOX_HTTP_TOKEN=change-me-dev-token \
  -p 127.0.0.1:8080:8080 \
  -v pi-sandbox-state:/var/lib/pi-sandbox \
  pi-sandbox:dev \
  /usr/local/bin/pi-sandbox-master serve --isolation privileged --http 0.0.0.0:8080
```

Privileged mode is the default. It uses Linux namespaces, bind mounts, `pivot_root`, private tmpfs locations, per-slot UIDs, and child seccomp. It requires `CAP_SYS_ADMIN` and a setup-compatible container seccomp profile.

Run in user isolation mode without `CAP_SYS_ADMIN`, `privileged: true`, or `seccomp=unconfined`:

```bash
docker run --rm -it \
  --name pi-sandbox \
  -e PI_SANDBOX_ISOLATION=user \
  -e PI_SANDBOX_HTTP_TOKEN=change-me-dev-token \
  -p 127.0.0.1:8080:8080 \
  pi-sandbox:dev \
  /usr/local/bin/pi-sandbox-master serve --http 0.0.0.0:8080
```

User mode keeps the master as root, validates pre-created per-slot users, prepares `0700` slot workspaces, runs commands as the slot UID/GID, uses workspace-local temp/cache directories, sets `no_new_privs`, and installs child seccomp. User mode does not hide the container filesystem, create private mount/network namespaces, mount private `/proc`, or support readonly injected directories.

Open the UI at `http://127.0.0.1:8080` and enter the token value from `PI_SANDBOX_HTTP_TOKEN`.

The default safe control plane is still the Unix socket at `/var/lib/pi-sandbox/state/master.sock`. The browser UI uses the optional development HTTP API. Publish it only on `127.0.0.1` and use a bearer token because agents share the container network namespace and can attempt to connect to master HTTP ports.

Docker Compose starts the UI with:

```bash
docker compose up --build
```

Then open `http://127.0.0.1:8080` and use token `change-me-dev-token`, or override `PI_SANDBOX_HTTP_TOKEN` before starting compose.

Send one JSON request per line:

```json
{"type":"health"}
```

```json
{"type":"run","request":{"run_id":"smoke-001","command":["bash","-lc","whoami; id; mkdir -p output; echo '{\"status\":\"ok\"}' > result.json; echo ok > output/result.txt"],"injected_files":[{"relative_path":"input/task.md","content_utf8":"write ok","mode":420}],"result_policy":{"collect_paths":["output/result.txt"],"result_file":"result.json"}}}
```

## Slot wait queue

When every sandbox slot is busy, run requests wait in FIFO order instead of
failing immediately. The default queue limit is ten waiting requests per slot
(80 with the default eight slots). Set an explicit limit with
`--max-waiting <COUNT>` or `PI_SANDBOX_MAX_WAITING`; use `0` to disable
waiting. Requests beyond the limit receive an error response.

A queued request receives a `waiting_for_slot` event with its position at the
time it entered the queue and the configured limit. HTTP and Unix-socket run
streams both send `{"type":"heartbeat"}` every 15 seconds while waiting and
while running. By default, if a client disconnects while queued, its
reservation is removed so it does not consume queue capacity. If it
disconnects after launch, the master kills and reaps the sandbox process tree,
runs the configured cleanup, and then releases the slot. Set
`"kill_on_disconnect": false` in `AgentRunRequest` to let a queued or running
request continue after its client disconnects. The default is `true`.

## Running your Pi agent

The UI sends `AgentRunRequest.command` exactly as an argv vector. The default command is:

```bash
pi --workspace /home/agent --input /home/agent/input/task.md
```

To run your Pi agent successfully, the `pi` executable must exist inside the runtime image at `/usr/local/bin/pi`, `/usr/bin/pi`, or another path in `PATH`. This repository does not install Pi yet because the installation method/version was not specified.

Recommended options:

1. Add your pinned Pi installation steps to the runtime stage of `Dockerfile`.
2. If Pi is a single binary, add `COPY pi /usr/local/bin/pi` and `RUN chmod 0755 /usr/local/bin/pi`.
3. If Pi is installed with npm/pip/cargo, install it in the Dockerfile with an explicit version and keep global caches out of the final image.

For a quick non-Pi smoke run in the UI, use this command:

```bash
bash -lc 'set -e; whoami; id; mkdir -p output; printf "{\"status\":\"ok\",\"summary\":\"smoke complete\"}" > result.json; echo ok > output/result.txt'
```

For your real Pi run, keep the injected task content in the UI and use:

```bash
pi --workspace "$HOME" --input "$HOME/input/task.md"
```

The agent should write structured output to `$HOME/result.json` and artifacts under `$HOME/output` so the master can collect them. In privileged mode `$HOME` is `/home/agent`; in user mode it is the allocated slot workspace under `/var/lib/pi-sandbox/workspaces`.

## Proxy Header Injection And Upstream Proxying

`AgentRunRequest.inject_headers` can inject headers into matching outbound requests made through the master proxy. Each rule can also attach an upstream proxy for matching requests by setting `http_proxy` and/or `https_proxy`. `override_header_key` and `override_header_value` are optional, but must be provided together when used. Omit them or set them to `null` for proxy-only routing rules. Rules with only `endpoint` and no action are ignored.

```json
{
  "run_id": "proxy-example",
  "command": ["bash", "-lc", "curl https://mimir.example.net/api"],
  "inject_headers": [
    {
      "endpoint": "https://mimir.example.net/*",
      "override_header_key": "Cookie",
      "override_header_value": "lightbox=session-token",
      "http_proxy": "http://utils.outbound.example.net:80",
      "https_proxy": "http://utils.outbound.example.net:80"
    }
  ]
}
```

Global rules use the same fields in the proxy config file:

```json
{
  "global_inject_headers": [
    {
      "endpoint": "https://mimir.example.net/*",
      "override_header_key": "Cookie",
      "override_header_value": "env:MIMIR_LIGHTBOX_COOKIE",
      "https_proxy": "http://utils.outbound.example.net:80"
    }
  ]
}
```

Proxy-only rules are valid:

```json
{
  "inject_headers": [
    {
      "endpoint": "https://mimir.example.net/*",
      "override_header_key": null,
      "override_header_value": null,
      "https_proxy": "http://utils.outbound.example.net:80"
    }
  ]
}
```

Rules are matched against normalized absolute URLs. Global rules apply first, per-run rules apply after them, and the last matching rule with a proxy for the request scheme selects the upstream proxy. HTTPS header injection requires MITM mode; without MITM, only the CONNECT host can be matched and tunneled through a matching `https_proxy`.
