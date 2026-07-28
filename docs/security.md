# Security model

Agentic Sandbox executes untrusted commands inside a trusted container control
plane. It provides useful process and filesystem hardening, but it is not yet a
complete hostile-multitenant sandbox. Evaluate the controls against your threat
model before deployment.

## Trust boundaries

Trusted:

- the container image and host;
- the `agentic-sandbox` control-plane process;
- API clients permitted to submit run requests;
- global proxy configuration and CA key material.

Untrusted:

- request commands and arguments;
- injected file contents;
- agent-created workspace files;
- child stdout, stderr, result JSON, and artifacts;
- outbound destinations.

API clients are powerful: they choose executable argv, environment entries that
pass policy, input files, collection paths, and—in privileged mode—host directory
sources for read-only bind mounts. Do not expose the API to untrusted tenants
without an authorization layer that constrains those fields.

## Privileged mode

Privileged mode creates mount, IPC, and UTS namespaces and enters a synthetic root
filesystem. The child receives a private workspace and temporary filesystems,
changes to a slot UID/GID, enables `no_new_privs`, and loads a seccomp deny-list.

It requires the trusted control plane to hold powerful container capabilities,
including `CAP_SYS_ADMIN` for mount setup. A control-plane compromise can therefore
be severe. The development Compose profile also disables the container-wide
seccomp and AppArmor policies so setup syscalls work.

Privileged mode does not create:

- a PID namespace;
- a network namespace;
- a user namespace;
- a cgroup namespace.

Mounting a new `/proc` does not isolate process visibility when the PID namespace
is shared. Agents may observe other processes in the container subject to normal
kernel permission checks.

## User mode

User mode changes to a dedicated slot UID/GID and relies primarily on Unix
permissions. It avoids mount privileges, but:

- the child sees the container's root filesystem;
- `/proc`, `/tmp`, `/dev/shm`, IPC, hostname, PID, and network state are shared;
- world-readable image content is visible;
- read-only host-directory injection is unavailable;
- other processes running under the same UID could access a reused slot.

Use user mode for cooperative workloads where reduced container privilege matters
more than filesystem confidentiality. Do not treat it as a strong boundary
between mutually hostile tenants.

## Child hardening

Both Linux modes:

- remove supplementary groups and switch real, effective, and saved UID/GID;
- request capability bounding-set drops;
- set `PR_SET_NO_NEW_PRIVS`;
- install a seccomp filter that denies mount/namespace, tracing, kernel-management,
  inbound-listener, and several other high-risk syscalls;
- run the child in a new process group, then deny `setsid` and `setpgid`, so
  timeout, disconnect, and normal-exit cleanup can signal the whole group.

Known limitations:

- Capability bounding-set drops are best-effort and errors are ignored.
- Seccomp defaults to allow and denies a finite syscall list. New or omitted
  syscalls remain available.
- `close_unneeded_fds` is currently a no-op that relies on standard close-on-exec
  behavior.
- Process-group killing still does not have the containment and accounting
  guarantees of a cgroup; keep container-level PID limits and monitoring in
  place.
- The child can fork and create outbound sockets.

## Resource limits

The request schema includes memory, PID-count, and CPU quota fields, but cgroup v2
support is currently a placeholder and these three limits are not applied.

The implemented limits are:

- wall-clock timeout;
- bounded stdout and stderr tails in the final result, each capped at 8 MiB;
- a 2 MiB request limit on both control APIs;
- at most 256 collected paths, 16 MiB per collected file or structured result,
  and 64 MiB of collected files in total;
- bounded slot concurrency and wait queue.

Streaming output is not a total-output quota. Apply container-level resource
limits and storage quotas in deployments.

## Outbound network and proxy

Both modes share the container network namespace. Child seccomp blocks server
socket operations such as `bind`, `listen`, and `accept`, but permits outbound
`connect`. Agents can:

- reach any destination allowed to the container;
- bypass the provided proxy with direct sockets;
- probe services on the container network;
- attempt to reach the development HTTP API over loopback.

The run-scoped proxy protects header-rule association and avoids placing configured
global secrets directly in the child environment. It does not force traffic
through the proxy.

HTTPS MITM stores a CA private key in control-plane state, injects the CA certificate
into the run, and lets the proxy inspect plaintext application traffic. Protect
the state directory, rotate compromised CA material, and avoid reusing a
development CA in production.

## Control APIs

Prefer the Unix socket and protect its parent directory. The server does not
currently verify Unix peer credentials.

The HTTP interface is explicitly a development API:

- bearer token is optional unless configured;
- traffic is plaintext;
- there are no scopes or tenant separation;
- there is no built-in rate limit;
- both HTTP and Unix requests are capped at 2 MiB;
- authorization grants the ability to execute arbitrary image commands.

Keep it bound to `127.0.0.1`, configure a strong token, and place production access
behind a trusted TLS and authorization gateway. Agents share loopback with the
control plane, so the bearer token must never be available to a child.

## Files and environment

Workspace injection validates relative paths and rejects traversal. Workspace
reset uses a tombstone rename before creating a fresh slot directory, and Linux
ownership transfer walks held directory descriptors without following symlinks.
Result collection also validates requested relative paths and uses Linux
`openat2(RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS)` so a concurrent ancestor
symlink swap cannot escape the workspace.

The environment starts from a controlled baseline and filters common cloud,
loader, container-control, and proxy variables. This reduces accidental leakage
but cannot discover every application-specific secret. Run the control plane with
the minimum environment it needs.

In privileged mode, `injected_readonly_dirs[].source_path` is selected by the API
caller and opened by the root control plane. Only trusted callers should be able to
use this field, or an outer authorization layer should restrict sources to an
explicit allow-list.

## Container guidance

- Do not set a root password or enable interactive login in the image.
- Pin base-image digests, Rust dependencies, and globally installed agent tools for
  release builds.
- Replace the unconfined seccomp and AppArmor settings with reviewed
  control-plane profiles.
- Drop every container capability that the selected isolation mode does not need.
- Apply container-level CPU, memory, PID, and filesystem limits.
- Restrict egress at the container or orchestrator network layer.
- Run only one trust domain per Agentic Sandbox instance.
- Treat preserved workspaces and collected artifacts as sensitive data.

## Current security priorities

Before positioning the project as a hostile-workload sandbox, prioritize:

1. cgroup v2 enforcement and reliable process-tree cleanup;
2. PID and network isolation, or clearly enforced egress policy;
3. an allow-list child seccomp profile;
4. fail-closed capability handling and explicit FD closing;
5. hardened API authentication, authorization, and request limits;
6. broader adversarial Linux isolation coverage beyond the current Docker E2E
   smoke, queue, timeout, disconnect, and process-tree cases.

Security-sensitive changes should update this document in the same pull request.
