# User Isolation Mode Implementation Plan

## Summary

This repository currently implements a privileged single-container sandbox. Each sandboxed run creates Linux namespaces, bind mounts a synthetic root filesystem, pivots into that root, mounts private temporary filesystems, drops to a slot UID/GID, and installs seccomp before executing the requested command.

That design provides strong per-run filesystem and namespace isolation, but it requires container permissions such as `CAP_SYS_ADMIN` and `seccomp=unconfined`. Those settings are difficult to use safely in Kubernetes because a compromised application receives a much larger kernel attack surface.

Add a second isolation mode named `user` that works inside a normal Docker container without `CAP_SYS_ADMIN`, `privileged: true`, or `seccomp=unconfined`.

The new mode should keep the trusted master process running as root inside the container. The master allocates pre-created Unix users, prepares per-user workspaces, injects files, launches the requested command as the allocated user, and then collects or cleans up the workspace. Isolation in this mode comes from standard Unix file permissions, per-slot users, `no_new_privs`, process groups, and a child seccomp filter.

Keep the existing privileged implementation as `privileged` mode. Add `auto` mode only after both explicit modes are working and tested.

## Goals

- Keep the current privileged namespace implementation available.
- Add a fallback mode that works with a plain Docker command like:

```bash
docker run --rm -it \
  --name pi-sandbox-dev-test \
  -p 127.0.0.1:8080:8080 \
  pi-sandbox:dev-test \
  /usr/local/bin/pi-sandbox-master serve --http 0.0.0.0:8080
```

- Avoid requiring these Docker/Kubernetes settings in user mode:

```text
privileged: true
CAP_SYS_ADMIN
seccomp=unconfined
```

- Keep the master responsible for:

```text
slot allocation
workspace reset
file injection
workspace preservation/cleanup
result collection
timeout handling
process group killing
```

- Prevent sandbox users from writing outside their own workspace when normal Unix permissions allow enforcement.
- Prevent sandbox users from using global `/tmp` as a cross-run shared writable area.
- Keep a restrictive child seccomp policy in user mode.

## Non-Goals

- Do not claim user mode is equivalent to namespace isolation.
- Do not hide the whole container filesystem from sandbox users in user mode.
- Do not implement per-run mount namespaces in user mode.
- Do not implement per-run network namespaces in user mode.
- Do not rely on runtime user creation unless pre-created users are impossible.
- Do not silently downgrade from privileged mode to user mode without clear logs.

## Current Context

Important current files:

```text
src/main.rs
src/master/mod.rs
src/master/allocator.rs
src/master/workspace.rs
src/master/injector.rs
src/master/result_collector.rs
src/sandbox/mod.rs
src/sandbox/launcher.rs
src/sandbox/init.rs
src/sandbox/namespaces.rs
src/sandbox/rootfs.rs
src/sandbox/mounts.rs
src/sandbox/credentials.rs
src/sandbox/seccomp.rs
Dockerfile
docker-compose.yml
ops/entrypoint.sh
```

The current launcher uses `Command::pre_exec` in `src/sandbox/launcher.rs` and calls:

```rust
crate::sandbox::init::prepare_and_drop(&config)
```

That path performs privileged setup:

```text
setpgid
unshare(CLONE_NEWNS | CLONE_NEWIPC | CLONE_NEWUTS)
make mount propagation private
build rootfs with bind mounts and tmpfs mounts
pivot_root
mount private /proc
chdir /home/agent
close file descriptors
set no_new_privs
install seccomp
```

This is why a plain Docker container currently fails with:

```text
unshare sandbox namespaces: EPERM: Operation not permitted
```

The current slot allocator already maps slots to UIDs/GIDs:

```rust
uid = 10_000 + idx
```

This maps naturally to the proposed user mode.

## Isolation Modes

Add an explicit isolation mode enum.

Suggested type:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationMode {
    Privileged,
    User,
}
```

Optionally add this input enum later:

```rust
pub enum RequestedIsolationMode {
    Privileged,
    User,
    Auto,
}
```

Recommended first implementation:

```text
Only implement explicit privileged and user modes first.
Add auto mode after tests exist for both paths.
```

## CLI And Environment Configuration

Add an argument to both `serve` and `run-json`:

```text
--isolation privileged|user
```

Support an environment variable:

```text
PI_SANDBOX_ISOLATION=privileged
PI_SANDBOX_ISOLATION=user
```

Recommended default during migration:

```text
privileged
```

Recommended later default for Kubernetes-safe deployments:

```text
user
```

If `auto` is added later, require loud logs like:

```text
privileged sandbox setup unavailable; falling back to user isolation mode
```

Do not make fallback invisible. Operators must know which isolation boundary they are relying on.

## Dockerfile Changes

Pre-create sandbox users in the image. Prefer deterministic users over runtime user creation.

Example target users:

```text
agent0001 uid=10001 gid=10001
agent0002 uid=10002 gid=10002
...
agent0064 uid=10064 gid=10064
```

The number should match or exceed the default `--slots` value. A practical first value is 64.

The Dockerfile can use a loop during build. Keep users shell-restricted if possible, but remember the process will execute requested commands after `setuid`, so shell value is not the main control.

Suggested requirements:

```text
create group per slot
create user per slot
no password
no login password
home can be nonexistent or under /nonexistent
UID/GID must match SlotAllocator
```

Also make global temp directories non-writable by sandbox users:

```text
chmod 0755 /tmp
chmod 0755 /var/tmp
```

Consider `/dev/shm` carefully. It is usually `1777`. In plain Docker mode it may remain writable unless changed. If workloads do not need shared memory, make it non-writable too:

```text
chmod 0755 /dev/shm || true
```

Create all application root directories as root-owned:

```text
/var/lib/pi-sandbox root:root 0700
/var/lib/pi-sandbox/state root:root 0700
/var/lib/pi-sandbox/results root:root 0700
/var/lib/pi-sandbox/roots root:root 0700
/var/lib/pi-sandbox/injected root:root 0700
/var/lib/pi-sandbox/tombstones root:root 0700
/var/lib/pi-sandbox/workspaces root:root 0711
```

The `0711` mode lets slot users traverse to their own workspace by exact path but does not let them list sibling workspaces.

## Entrypoint Changes

Update `ops/entrypoint.sh` to enforce directory permissions on startup. This matters when `/var/lib/pi-sandbox` is a Docker volume because image-time ownership and modes may not apply to a fresh volume.

Keep:

```sh
chmod 0700 "$PI_SANDBOX_ROOT"
chmod 0711 "$PI_SANDBOX_ROOT/workspaces" || chmod 0700 "$PI_SANDBOX_ROOT/workspaces"
```

Add or verify:

```sh
chmod 0755 /tmp || true
chmod 0755 /var/tmp || true
chmod 0755 /dev/shm || true
```

If `/tmp` must remain writable for the master, use a master-private temp directory instead:

```text
/var/lib/pi-sandbox/master-tmp root:root 0700
```

Do not use global `/tmp` for sandbox run data in user mode.

## Slot Allocation

`src/master/allocator.rs` already creates slot IDs and UID/GID pairs.

Keep the mapping:

```text
slot-0001 -> uid/gid 10001
slot-0002 -> uid/gid 10002
```

Add startup validation in user mode:

```text
for each configured slot:
  verify UID exists in /etc/passwd
  verify GID exists in /etc/group
```

Fail early if users are missing:

```text
user isolation mode requires pre-created uid 10001 for slot-0001
```

Do not create users at runtime in the first implementation. Runtime creation is possible, but it adds extra dependencies and makes failures harder to reason about.

## Workspace Management

Current workspace code already resets and chowns slot workspaces.

In user mode, workspace preparation should be:

```text
rename old workspace to tombstone if it exists
create workspace directory
create workspace/tmp
create workspace/.cache
create workspace/.config
create workspace/.local
chown recursive workspace to slot uid/gid
chmod workspace 0700
chmod workspace/tmp 0700
delete tombstone unless preserving
```

Recommended modes:

```text
workspace root: 0700, owner slot uid/gid
workspace/tmp: 0700, owner slot uid/gid
workspace/.cache: 0700, owner slot uid/gid
workspace/.config: 0700, owner slot uid/gid
workspace/.local: 0700, owner slot uid/gid
```

The master runs as root, so it can always delete, preserve, or inspect workspaces regardless of slot ownership.

## Environment Policy

Current defaults in `src/policy/defaults.rs` assume:

```text
HOME=/home/agent
PWD=/home/agent
TMPDIR=/tmp
```

For user mode, environment values should be workspace-specific:

```text
HOME=/var/lib/pi-sandbox/workspaces/slot-0001
PWD=/var/lib/pi-sandbox/workspaces/slot-0001
TMPDIR=/var/lib/pi-sandbox/workspaces/slot-0001/tmp
TMP=/var/lib/pi-sandbox/workspaces/slot-0001/tmp
TEMP=/var/lib/pi-sandbox/workspaces/slot-0001/tmp
XDG_CACHE_HOME=/var/lib/pi-sandbox/workspaces/slot-0001/.cache
XDG_CONFIG_HOME=/var/lib/pi-sandbox/workspaces/slot-0001/.config
XDG_DATA_HOME=/var/lib/pi-sandbox/workspaces/slot-0001/.local/share
PIP_CACHE_DIR=/var/lib/pi-sandbox/workspaces/slot-0001/.cache/pip
PYTHONUSERBASE=/var/lib/pi-sandbox/workspaces/slot-0001/.local
PYTHONPYCACHEPREFIX=/var/lib/pi-sandbox/workspaces/slot-0001/.cache/pycache
NPM_CONFIG_CACHE=/var/lib/pi-sandbox/workspaces/slot-0001/.cache/npm
NPM_CONFIG_PREFIX=/var/lib/pi-sandbox/workspaces/slot-0001/.local
```

Do not let user-provided environment override these keys.

Implementation approach:

```text
Make clean_env aware of the slot workspace, or add a second function:
clean_env_for_slot(req, slot, mode)
```

Keep filtering sensitive environment variables:

```text
AWS_*
GCP_*
AZURE_*
DOCKER_HOST
KUBECONFIG
LD_PRELOAD
LD_LIBRARY_PATH
```

## Launcher Changes

Modify `src/sandbox/launcher.rs` to branch on isolation mode.

Current privileged setup should remain:

```rust
crate::sandbox::init::prepare_and_drop(&config)
```

Add user-mode setup that does not call:

```text
unshare
mount
pivot_root
mount proc
sethostname
```

User-mode child setup should do:

```text
set current_dir to slot workspace before spawn
pre_exec:
  setpgid(0, 0)
  setgroups([])
  setresgid(slot.gid, slot.gid, slot.gid)
  setresuid(slot.uid, slot.uid, slot.uid)
  drop capabilities best effort
  set no_new_privs
  install child seccomp
```

Order matters:

```text
1. setpgid
2. setgroups
3. setresgid
4. setresuid
5. drop capabilities
6. no_new_privs
7. seccomp
```

Rationale:

```text
setgroups must happen before losing privilege
setgid should happen before setuid
capability dropping should happen after UID/GID transition
no_new_privs should happen before exec
seccomp should be loaded before exec
```

Be careful with `Command::pre_exec`. It runs after fork and before exec. Avoid allocation, logging, filesystem traversal, or complex Rust code there where possible. The existing code already does complex setup in `pre_exec`, but the user-mode path should be simpler.

## Credentials Code

`src/sandbox/credentials.rs` already has useful helpers:

```rust
drop_to_agent(uid, gid)
set_no_new_privs()
drop_capabilities_best_effort()
```

Keep or extend `drop_to_agent` for user mode. It already calls:

```text
setgroups(0, null)
setresgid
setresuid
```

If more explicit naming helps, add:

```rust
drop_to_uid_gid(uid, gid)
```

Avoid duplicating credential logic in the launcher.

## Seccomp Policy

Keep `src/sandbox/seccomp.rs` active in user mode.

The current filter blocks dangerous syscalls after setup:

```text
mount
umount2
pivot_root
chroot
setns
unshare
ptrace
bpf
perf_event_open
keyctl
add_key
request_key
reboot
kexec_load
init_module
finit_module
delete_module
swapon
swapoff
open_by_handle_at
fanotify_init
quotactl
acct
bind
listen
accept
accept4
```

This is valuable in user mode because the child process should not need any of these syscalls.

Review the networking restrictions:

```text
bind/listen/accept/accept4 blocked means inbound servers fail
outbound connect still works
Unix-domain socket servers also fail if they use bind/listen
```

If real workloads require local Unix sockets, revisit the policy. Do not loosen TCP/UDP listening without a clear product decision.

## File Injection

Current `src/master/injector.rs` creates files and then chowns them to the slot UID/GID.

Keep that behavior in user mode.

For injected regular files:

```text
create parent dirs
write file
chmod requested mode
chown file to slot uid/gid
```

Ensure parent directories are also owned by the slot user if they are created during injection. If only the final file is chowned, a parent directory created as root may block the sandbox user from using the path.

## Readonly Injected Directories

Current privileged mode supports `injected_readonly_dirs` through read-only bind mounts.

User mode cannot provide equivalent semantics without `mount`.

Recommended first implementation:

```text
Reject injected_readonly_dirs in user mode with a clear error.
```

Example error:

```text
readonly injected directories require privileged isolation mode
```

Do not copy readonly dirs and claim strong readonly semantics. If the sandbox user owns a writable parent directory, chmod-based readonly copies can often be deleted and replaced.

Possible future implementation:

```text
copy readonly resources to root-owned paths outside the workspace
grant read/execute access only
pass absolute paths through env or arguments
```

But this changes API semantics and should be designed separately.

## Validation Changes

Update request validation in `src/master/mod.rs`.

In user mode:

```text
if injected_readonly_dirs is not empty:
  reject request
```

In privileged mode:

```text
keep current behavior
```

Add mode-aware validation if needed:

```rust
validate_request(&req, self.isolation_mode)?;
```

## Result Collection

`src/master/result_collector.rs` reads files from the workspace as the master process.

This should continue to work because the master remains root.

No major changes are required.

Add symlink safety checks if not already covered. A malicious sandbox user can create symlinks inside its workspace. The collector currently uses `symlink_metadata` for collect paths but then reads the path. Verify whether this can follow a symlink outside the workspace. If it can, fix it before trusting collected files.

Recommended rule:

```text
Collected paths must resolve to regular files inside the slot workspace.
Do not follow symlinks outside the workspace.
```

## Cleanup And Process Killing

The current launcher starts a process group and kills with:

```rust
kill(-pid, SIGKILL)
```

Keep this behavior.

In user mode, the sandbox user should not be able to signal master processes because it is a different UID and lacks privileges.

The master, running as root, can kill the sandbox process group.

## Kubernetes Security Context

User mode should not require `SYS_ADMIN` or `seccomp: Unconfined`.

A suitable Kubernetes direction is:

```yaml
securityContext:
  runAsUser: 0
  allowPrivilegeEscalation: false
  seccompProfile:
    type: RuntimeDefault
```

Do not use:

```yaml
privileged: true
```

Do not add:

```yaml
capabilities:
  add:
    - SYS_ADMIN
```

Be careful with dropping all capabilities. The master probably needs default root capabilities such as:

```text
CHOWN
SETUID
SETGID
KILL
FOWNER
DAC_OVERRIDE
SETPCAP
```

Docker default root capabilities typically include these. Kubernetes policies may remove them, so document the required capability set for user mode.

## Security Model Comparison

Privileged mode provides:

```text
private mount namespace
private synthetic rootfs
private tmpfs locations
private proc mount
readonly bind mounts
slot UID/GID drop
child seccomp
stronger per-run filesystem view
```

But privileged mode requires risky container permissions:

```text
CAP_SYS_ADMIN
seccomp=unconfined or a setup-friendly seccomp profile
possibly AppArmor/SELinux relaxations
```

User mode provides:

```text
plain Docker/Kubernetes compatibility
no CAP_SYS_ADMIN
no internal mounts
no internal namespace creation
per-slot Unix users
0700 workspaces
non-writable global /tmp
workspace-local temp/cache/config directories
child no_new_privs
child seccomp
```

User mode does not provide:

```text
hidden root filesystem
private /proc
private /tmp mount
private IPC namespace
readonly bind-mounted resources
mount namespace isolation
full malicious-code containment
```

User mode prevents writes mainly through Unix permissions. Sandbox users may still read world-readable files in the container, such as:

```text
/etc/passwd
/etc/hosts
/usr/bin/*
/proc/*
```

This is acceptable only if the product accepts that user mode is weaker than privileged mode.

## Suggested Code Structure

Suggested new or changed types:

```text
src/sandbox/mod.rs
  IsolationMode
  SandboxLaunchConfig { isolation_mode, ... }

src/master/mod.rs
  Master { isolation_mode, ... }
  Master::new(root, slot_count, isolation_mode)

src/main.rs
  parse --isolation / PI_SANDBOX_ISOLATION

src/sandbox/launcher.rs
  configure_platform_privileged(...)
  configure_platform_user(...)

src/policy/defaults.rs
  clean_env_for_slot(req, slot, isolation_mode)

src/master/workspace.rs
  prepare_slot_workspace(slot, mode)
  validate_slot_users(slots)
```

Keep the first implementation minimal. Do not refactor unrelated code.

## Implementation Steps

1. Add `IsolationMode` and plumb it through `main`, `Master`, and `SandboxLaunchConfig`.
2. Keep current behavior when mode is `Privileged`.
3. Add CLI/env parsing for `--isolation privileged|user`.
4. Pre-create slot users in the Dockerfile.
5. Harden `/tmp`, `/var/tmp`, and optionally `/dev/shm` permissions in Dockerfile and entrypoint.
6. Add startup validation that user-mode slot UIDs/GIDs exist.
7. Update workspace reset to create workspace-local temp/cache/config directories and chown them to the slot user.
8. Update injected file handling so parent directories created during injection are owned by the slot user.
9. Add user-mode launcher path that skips namespace/mount/rootfs setup and runs child as the slot UID/GID.
10. Keep `no_new_privs` and child seccomp in user mode.
11. Make environment defaults workspace-specific in user mode.
12. Reject `injected_readonly_dirs` in user mode.
13. Add tests for user-mode workspace permissions and command execution.
14. Add documentation showing how to run each mode.
15. Only after explicit modes work, consider adding `auto` detection.

## Testing Plan

Unit tests:

```text
parse isolation mode values
reject invalid isolation values
validate user-mode requests reject injected_readonly_dirs
clean_env_for_slot uses workspace paths in user mode
clean_env_for_slot keeps /home/agent paths in privileged mode if needed
```

Integration tests inside Docker:

```text
user mode can run: id -u
id -u returns 10001 for slot-0001
pwd equals the slot workspace
HOME equals the slot workspace
TMPDIR equals workspace/tmp
touch $TMPDIR/file succeeds
touch /tmp/file fails
touch ../slot-0002/file fails
cat /var/lib/pi-sandbox/state/* fails as sandbox user
timeout still kills long-running process
result collection still works
cleanup deletes workspace after successful run
preserve policy keeps workspace after failure if requested
```

Privileged regression tests:

```text
privileged mode still calls namespace/rootfs path
existing smoke tests still pass in privileged Docker Compose setup
readonly injected dirs still work in privileged mode
```

Manual Docker tests:

```bash
docker build -t pi-sandbox:dev-test .
docker run --rm -it \
  --name pi-sandbox-dev-test \
  -e PI_SANDBOX_ISOLATION=user \
  -p 127.0.0.1:8080:8080 \
  pi-sandbox:dev-test \
  /usr/local/bin/pi-sandbox-master serve --http 0.0.0.0:8080
```

Manual privileged test:

```bash
docker compose up --build --force-recreate
```

## Acceptance Criteria

User mode is complete when:

```text
The service starts with a plain docker run command.
No CAP_SYS_ADMIN is required.
No seccomp=unconfined is required.
The master runs as root.
Sandbox commands run as per-slot users.
Each slot user can write only to its own workspace and workspace-local temp dirs.
Sandbox commands cannot write to global /tmp.
Sandbox commands cannot write to another slot workspace.
Sandbox commands cannot write to master state directories.
Timeout and process-group kill still work.
Result collection still works.
Privileged mode still works as before.
The docs clearly explain that user mode is weaker than privileged mode.
```

## Risks And Follow-Ups

Main risks:

```text
Some tools hardcode /tmp and will fail when /tmp is not writable.
Some workloads need local Unix sockets; current seccomp blocks bind/listen.
World-readable files in the container remain readable in user mode.
/proc is shared at the container level in user mode.
Readonly injected directories cannot be implemented with equivalent strength without mounts.
Kubernetes policies that drop SETUID/SETGID/CHOWN will break user mode.
```

Follow-ups after the first implementation:

```text
Add auto mode with explicit logs.
Add symlink-safe result collection if missing.
Add a custom Kubernetes manifest for user mode.
Add a custom seccomp profile for the container itself if needed.
Consider one-pod-per-run architecture for hostile untrusted code.
Consider gVisor, Kata Containers, or Firecracker for stronger isolation boundaries.
```

## Final Guidance For The Coding Agent

Implement this as a minimal dual-mode change. Preserve the current privileged mode and add user mode beside it. Do not replace the existing namespace/rootfs implementation.

Favor small, explicit branches over large abstractions. The important behavioral split is:

```text
privileged mode = current namespace/mount/pivot_root sandbox
user mode = root master plus per-slot Unix users and workspace permissions
```

Make the mode visible in logs at startup and per run. If the mode is user, logs should make clear that filesystem hiding and mount namespace isolation are not active.
