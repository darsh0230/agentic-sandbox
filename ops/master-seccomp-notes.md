# Master Seccomp Notes

The development container uses `--security-opt seccomp=unconfined` because the trusted master needs namespace and mount setup syscalls that Docker's default profile blocks.

For production, start from Docker's default seccomp profile and add only the setup calls the master needs:

- `unshare`
- `mount`
- `umount2`
- `pivot_root`
- `clone` or `clone3` with namespace flags, if the launcher is changed to use a PID namespace stage

Do not deny `bind`, `listen`, `accept`, or `accept4` at the container level if the trusted master exposes a management API. Those denials are loaded by the per-agent seccomp filter after privilege drop.
