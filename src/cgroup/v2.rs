// Cgroup v2 support is intentionally isolated. Docker deployments often need
// explicit writable cgroup mounts; the supervisor currently relies on process
// group killing and timeouts when cgroups are unavailable.
