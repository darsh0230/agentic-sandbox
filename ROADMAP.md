# Roadmap

Agentic Sandbox is an experimental agent execution runtime. The near-term goal is
to make its isolation claims measurable, its distribution reproducible, and its
first-run agent experience easy to verify.

This roadmap is directional, not a delivery commitment.

## Now: trustworthy evaluation

- Publish signed, tagged `linux/amd64` images to GHCR.
- Pin base-image digests and OpenCode/Pi package versions.
- Add a short, reproducible coding-agent demo and recording.
- Align the browser upload limit with the HTTP request limit.
- Expand adversarial tests for filesystem collection, process cleanup, and
  environment filtering.
- Document release, rollback, and compatibility procedures.

## Next: stronger containment

- Enforce cgroup v2 CPU, memory, and process-count limits.
- Add reliable process-tree accounting and cleanup.
- Add PID isolation and a documented network-isolation/egress strategy.
- Replace the child seccomp deny-list with a reviewed allow-list.
- Fail closed on capability drops and explicitly close inherited file
  descriptors.
- Add scoped API authorization and production gateway guidance.

## Later: broader distribution

- Publish and continuously test `linux/arm64` images.
- Add SDKs or generated clients for the streaming API.
- Add metrics, tracing examples, and deployment health guidance.
- Define stable API compatibility and supported release windows.
- Seek an independent security review after the containment milestones land.

## Good first issue candidates

These are candidates for maintainers to create and label on GitHub:

1. **Add a health-check command example.** Add a small script under `examples/`
   that checks authentication and prints the server status, with tests for its
   argument validation.
2. **Check local Markdown links in CI.** Add a dependency-light check that fails
   when a relative documentation link points to a missing repository path.
3. **Document proxy configuration recipes.** Add copyable examples for a header
   override, an upstream HTTP proxy, and HTTPS MITM with explicit warnings.
4. **Add an image tool-version smoke test.** Verify `opencode --version`,
   `pi --version`, `git --version`, and `rg --version` in the Docker E2E image.
5. **Improve browser request-size feedback.** Read the server's 2 MiB limit in
   the UI copy and reject oversized JSON before submission.
