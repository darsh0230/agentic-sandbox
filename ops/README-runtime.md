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
{"type":"run","request":{"run_id":"smoke-001","command":["bash","-lc","whoami; id; mkdir -p .pi output; echo '{\"status\":\"ok\"}' > .pi/result.json; echo ok > output/result.txt"],"injected_files":[{"relative_path":"input/task.md","content_utf8":"write ok","mode":420}],"result_policy":{"collect_paths":["output/result.txt"],"result_file":".pi/result.json"}}}
```

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
bash -lc 'set -e; whoami; id; mkdir -p .pi output; printf "{\"status\":\"ok\",\"summary\":\"smoke complete\"}" > .pi/result.json; echo ok > output/result.txt'
```

For your real Pi run, keep the injected task content in the UI and use:

```bash
pi --workspace "$HOME" --input "$HOME/input/task.md"
```

The agent should write structured output to `$HOME/.pi/result.json` and artifacts under `$HOME/output` so the master can collect them. In privileged mode `$HOME` is `/home/agent`; in user mode it is the allocated slot workspace under `/var/lib/pi-sandbox/workspaces`.
