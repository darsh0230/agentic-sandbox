# Pi Sandbox: Isolation Modes and Security Analysis

## Overview

`pi-sandbox-master` is a single-container Rust application that runs multiple Pi coding agents in isolated sandboxes. It provides two isolation modes: **privileged** (Linux namespaces + pivot root) and **user** (per-slot Unix users with file permissions). This document explains both modes, the trade-offs of user isolation mode, the ways agent processes can break user isolation, and guidance on root password policy.

---

## Privileged Mode

**Source:** `src/sandbox/init.rs:5-22`

This is the default mode. For each agent run, the master (running as root) forks a child and in `pre_exec` (between fork and exec) performs the following sequence:

1. **`setpgid(0, 0)`** — new process group for clean kill
2. **`unshare(CLONE_NEWNS | CLONE_NEWIPC | CLONE_NEWUTS)`** — private mount, IPC, and UTS namespaces (`src/sandbox/namespaces.rs:4`)
3. **`mount("/", MS_REC | MS_PRIVATE)`** — detach mount tree from host (`src/sandbox/mounts.rs:12`)
4. **Build synthetic rootfs** at `/var/lib/pi-sandbox/roots/<run_id>/` (`src/sandbox/rootfs.rs:12-93`):
   - Read-only bind mounts of `/usr`, `/bin`, `/sbin`, `/lib`, `/lib64`
   - Sanitized `/etc/passwd`, `/etc/group`, `/etc/hosts`, `/etc/nsswitch.conf` — only `root` and `agent` entries
   - Private tmpfs on `/tmp`, `/var/tmp`, `/run`, `/dev/shm` with `nosuid, nodev`
   - Minimal device nodes (`null`, `zero`, `random`, `urandom`)
   - Workspace bind-mounted to `/home/agent` (RW)
5. **`pivot_root`** + `umount2("/.oldroot", MNT_DETACH)` — the old container root is detached (`src/sandbox/rootfs.rs:97-105`)
6. **Private `/proc`** mount with `NOSUID | NODEV | NOEXEC` (`src/sandbox/mounts.rs:70-78`)
7. **`setresgid` / `setresuid`** to slot UID/GID (`src/sandbox/credentials.rs:10-15`)
8. **`PR_CAPBSET_DROP`** for caps 0-63 (`src/sandbox/credentials.rs:30-33`)
9. **`PR_SET_NO_NEW_PRIVS`** — block setuid binaries and file capabilities (`src/sandbox/credentials.rs:22-26`)
10. **Seccomp filter** — deny `mount`, `unshare`, `ptrace`, `bpf`, `bind`, `listen`, `accept`, etc. (`src/sandbox/seccomp.rs:2-40`)

### Required Container Capabilities

From `docker-compose.yml:10-22`:

```yaml
cap_drop: [ALL]
cap_add:  [SYS_ADMIN, SETPCAP, SETUID, SETGID, CHOWN, FOWNER, DAC_OVERRIDE, KILL]
security_opt: [seccomp=unconfined]
```

The agent sees a completely synthetic filesystem. It cannot see other agents, the master's state directory, or the real container `/etc`, `/proc`, `/tmp`. The only writable surface is its own workspace.

---

## User Isolation Mode

**Source:** `src/sandbox/launcher.rs:161-173`

This mode was added to be Kubernetes-safe — no `CAP_SYS_ADMIN`, no `seccomp=unconfined`, no `privileged: true` required.

In `pre_exec`, the child does:

1. `setpgid(0, 0)` — new process group
2. `setgroups(0, null)` — clear supplementary groups
3. `setresgid` / `setresuid` to slot UID/GID
4. `drop_capabilities_best_effort()` — bounding set only
5. `PR_SET_NO_NEW_PRIVS`
6. Seccomp filter (same as privileged mode)

**No `unshare`, no `mount`, no `pivot_root`, no private `/proc`, no synthetic rootfs.**

The agent runs directly inside the container's filesystem. Isolation relies entirely on:

- **Per-slot Unix users** pre-created in the Dockerfile (`Dockerfile:33-39`): `agent0001` (UID 10001) through `agent0064` (UID 10064), shell `/usr/sbin/nologin`
- **File permissions**: workspace dirs are `0700` owned by the slot user (`src/master/workspace.rs:54-56`), parent `workspaces/` dir is `0711` (traversable but not listable)
- **Global `/tmp`, `/var/tmp`, `/dev/shm` are `0755`** — readable but not writable by slot users (`ops/entrypoint.sh:21-23`)
- **Workspace-local temp/cache/config** dirs instead of global ones (`src/policy/defaults.rs:33-37`)

---

## Trade-off: User Isolation Mode vs Privileged Mode

| Property | Privileged | User |
|---|---|---|
| Synthetic rootfs (hides container FS) | Yes | **No** — agent sees real container filesystem |
| Private mount namespace | Yes | **No** |
| Private `/proc` | Yes | **No** — shared container `/proc` |
| Private IPC/UTS namespace | Yes | **No** |
| Private tmpfs `/tmp` | Yes | **No** — global `/tmp` at `0755` (not writable) |
| Read-only bind-mounted resources | Yes | **No** — `injected_readonly_dirs` rejected (`src/master/mod.rs:251-253`) |
| Per-slot UID/GID | Yes | Yes |
| `no_new_privs` + seccomp | Yes | Yes |
| Requires `CAP_SYS_ADMIN` | Yes | **No** |
| Requires `seccomp=unconfined` | Yes | **No** |
| Kubernetes-safe | Difficult | Yes |

### The Fundamental Trade-off

User mode trades filesystem hiding and namespace isolation for deployability. The agent can **read** almost everything in the container (`/etc/passwd`, `/usr/bin/*`, `/proc/*`, all world-readable files). It just can't **write** outside its workspace. The design doc explicitly states:

> *"This is acceptable only if the product accepts that user mode is weaker than privileged mode."*
> — `user-isolation-mode-implementation-plan.md:709`

---

## How Agent Processes Can Break User Isolation

The following are concrete ways the isolation in user mode can be bypassed or weakened, based on the codebase.

### 1. `/proc` is shared — information leak to other agents

In user mode, there's no private `/proc` mount. An agent can:

- **Read other agents' process arguments**: `/proc/<pid>/cmdline` and `/proc/<pid>/comm` are world-readable.
- **See PIDs of all processes** in the container, including the master process.
- **Read `/proc/<pid>/maps`**, `/proc/<pid>/status` for other processes — some of these are world-readable.

Privileged mode avoids this by mounting a private `/proc` after `pivot_root`.

### 2. No PID namespace — agents can see and potentially signal each other

Neither mode creates a `CLONE_NEWPID` namespace (`src/sandbox/namespaces.rs:4` only has `NEWNS | NEWIPC | NEWUTS`). In user mode, an agent can see other agents' PIDs. While a non-root user typically cannot `kill` processes owned by other UIDs, there are edge cases:

- If two runs happen to share the same slot UID (after a workspace reset), one run's leftover processes could be signaled by a new run in the same slot.
- Shared process groups or sessions could be exploitable.

### 3. World-readable container files expose the master binary, installed tools, and configuration

The agent in user mode can read:

- `/usr/local/bin/pi-sandbox-master` — the master binary itself
- The Node.js `pi-coding-agent` package and all its code
- `/etc/passwd` — reveals all 64 pre-created agent usernames and UIDs
- Any secrets accidentally placed in the image
- `/etc/hosts`, `/etc/resolv.conf` — container networking details

In privileged mode, only the sanitized `/etc/passwd` with `root` and `agent` entries is visible.

### 4. Global `/tmp` at `0755` — cross-run information channel

While not writable (`0755`), `/tmp` is readable by all slot users. If the master or another tool writes temporary files to `/tmp` (e.g., during file injection or result collection), an agent could read them. The design plan warns about this:

> *"Do not use global /tmp for sandbox run data in user mode."*
> — `user-isolation-mode-implementation-plan.md:260-266`

However, the current code doesn't create a master-private temp directory. The master could leak data through `/tmp` if any library it uses writes there.

### 5. `/dev/shm` at `0755` — readable shared memory

Same issue as `/tmp`. Any process in the container that uses POSIX shared memory (`/dev/shm/*`) exposes it to all slot users.

### 6. Capability drop is incomplete — bounding set only

`drop_capabilities_best_effort()` (`src/sandbox/credentials.rs:30-33`) only drops the **bounding set** via `PR_CAPBSET_DROP`. It does NOT drop the **effective**, **permitted**, or **inheritable** capability sets via `capset()`. The implementation plan acknowledges this gap. In practice, after `setresuid` to a non-zero UID, the kernel clears most effective/permitted caps automatically — but this is a kernel-version-dependent behavior, not an explicit guarantee. A setuid binary (if one existed and `no_new_privs` failed) could still exploit this.

### 7. FD leak — `close_unneeded_fds` is a no-op stub

`src/sandbox/fds.rs:2-5`:

```rust
pub fn close_unneeded_fds() -> anyhow::Result<()> {
    // tokio/std process setup already gives the child only requested stdio plus
    // close-on-exec descriptors. Keep this hook for a future close_range allowlist.
    Ok(())
}
```

While `tokio::process::Command` sets `CLOEXEC` on its internal fds, the master process may have other open file descriptors (log files, the Unix socket, state files) that aren't `CLOEXEC`. If any leak into the child before `exec`, the agent could read master state, the API socket, or the bearer token. This is only called in privileged mode (`src/sandbox/init.rs:16`), not in user mode at all.

### 8. Symlink attacks during workspace reset

`src/master/workspace.rs:42-43` renames the old workspace to a tombstone, then creates a fresh one. Between the rename and the new `create_dir_all`, if a slot user had created a symlink at the workspace path pointing elsewhere — the master is root and the rename would fail on a symlink. But if a malicious process from a previous run in the same slot is still alive (no PID namespace), it could race the workspace reset.

### 9. Seccomp is a deny-list, not an allow-list

The seccomp filter uses `ScmpAction::Allow` as the default and adds `Errno(EPERM)` rules for specific syscalls (`src/sandbox/seccomp.rs:5`). This means any syscall NOT in the deny list is allowed. A new dangerous syscall added to the kernel would be allowed by default. An allow-list (default deny) would be more secure but harder to maintain.

Notable syscalls NOT blocked:

- `clone` / `clone3` — an agent can fork new processes
- `socket` / `connect` — outbound network is fully open
- `open` / `openat` — can open any world-readable file
- `ioctl` — broad attack surface
- `mmap` / `mprotect` — can manipulate memory

### 10. Outbound network is unrestricted

The seccomp filter blocks `bind`, `listen`, `accept`, `accept4` — preventing inbound servers. But `connect` is allowed. An agent can:

- Make outbound HTTP/TCP connections to any reachable address
- Exfiltrate data to external servers
- Probe the container network (other containers, metadata services)
- Connect to the master's HTTP API at `127.0.0.1:8080` if it knows the bearer token

### 11. `injected_readonly_dirs` rejected but no alternative

In privileged mode, read-only resources are bind-mounted read-only (`src/sandbox/rootfs.rs:86-92`). In user mode, this is rejected (`src/master/mod.rs:251-253`). If someone tries to provide resources via file copy with `chmod 0444`, the slot user owns the writable parent directory and can delete/replace the "readonly" files. The design plan explicitly warns about this (`user-isolation-mode-implementation-plan.md:540-542`).

### 12. Cgroup resource limits are not enforced

`src/cgroup/v2.rs` is an empty stub. `ResourceLimits` (memory, PIDs, CPU) are parsed from requests but never applied (`src/master/mod.rs:118-127`). An agent can consume unlimited memory, fork-bomb, or peg CPU. Only `tokio::time::timeout` and `kill(-pid, SIGKILL)` provide any limit.

### 13. No cgroup also means no `/proc` cgroup isolation

Without a cgroup namespace or private `/proc`, an agent can read `/proc/cgroups`, `/proc/self/cgroup`, and understand the container's resource hierarchy.

---

## Should Root Have a Password Set?

**No.** Here is why based on the code:

1. **The master process runs as root** and always will. It needs root to call `setresuid`, `setresgid`, `chown`, and (in privileged mode) `unshare`/`mount`/`pivot_root`. The Dockerfile doesn't set a root password.

2. **The 64 agent users have shell `/usr/sbin/nologin`** and no password (`Dockerfile:33-39`). They can never log in interactively. They're only used as UID/GID targets for `setresuid`.

3. **Setting a root password would create an attack surface**: In user mode, agents can see `/etc/passwd` and `/etc/shadow` (if world-readable). If root has a password hash in `/etc/shadow`, and the file is somehow readable (misconfigured permissions, a tool that reads it), a compromised agent could attempt offline cracking. More importantly, `su` or `login` become possible attack vectors.

4. **`PR_SET_NO_NEW_PRIVS` is the defense, not passwords**. Even if a setuid binary like `su` exists in the container, `no_new_privs` (`src/sandbox/credentials.rs:22-26`) prevents the agent from using it to escalate. But this is defense-in-depth — a root password adds no value and only adds risk.

5. **The synthetic `/etc/passwd` in privileged mode** (`src/sandbox/rootfs.rs:143-148`) already writes `root:x:0:0:root:/root:/usr/sbin/nologin` — the `x` means "password in shadow" but there's no `/etc/shadow` in the synthetic rootfs. Root effectively has no login path.

6. **In the container's real `/etc/passwd`** (visible to agents in user mode), the Dockerfile doesn't modify root's entry. The default Debian image typically has `root:x:0:0:root:/root:/bin/bash`. The shell is `/bin/bash` but there's no password — `useradd` for the agent users uses `--shell /usr/sbin/nologin` but root is untouched.

### Recommendation

Ensure root has no password (`passwd -l root` or `usermod -p '!' root` in the Dockerfile), and ideally set root's shell to `/usr/sbin/nologin` as well. The master doesn't need an interactive shell — it runs as a binary. But even this is defense-in-depth; the real protections are `no_new_privs`, seccomp, and the capability drop.

---

## Summary

**Privileged mode** provides strong isolation via namespaces + pivot root + private `/proc` + hidden filesystem, but requires `CAP_SYS_ADMIN` and `seccomp=unconfined` — risky in Kubernetes.

**User mode** provides weak isolation via Unix permissions only. Agents can read the entire container filesystem, see all processes via shared `/proc`, and have unrestricted outbound network. The isolation boundary is "can't write outside your workspace" — not "can't see anything outside your workspace."

**Root should NOT have a password set.** The master needs root for `setuid`/`chown`/`mount` operations, but interactive root login is never needed. A root password would only add attack surface in user mode where `/etc/shadow` is potentially accessible.

The most significant ways isolation can break in user mode are: shared `/proc` (information leak), unrestricted outbound network (data exfiltration), incomplete capability drop (bounding set only), the no-op FD cleanup stub, and the deny-list seccomp approach. These are all documented gaps in the design plan.
