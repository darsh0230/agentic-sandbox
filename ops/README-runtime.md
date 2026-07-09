# Runtime

Build and run:

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
  -v pi-sandbox-state:/var/lib/pi-sandbox \
  pi-sandbox:dev
```

The control plane is a Unix socket at `/var/lib/pi-sandbox/state/master.sock`. Do not publish ports by default because agents share the container network namespace.

Send one JSON request per line:

```json
{"type":"health"}
```

```json
{"type":"run","request":{"run_id":"smoke-001","command":["bash","-lc","whoami; id; mkdir -p .pi output; echo '{\"status\":\"ok\"}' > .pi/result.json; echo ok > output/result.txt"],"injected_files":[{"relative_path":"input/task.md","content_utf8":"write ok","mode":420}],"result_policy":{"collect_paths":["output/result.txt"],"result_file":".pi/result.json"}}}
```
