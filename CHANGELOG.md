# Changelog

All notable changes to Agentic Sandbox will be documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
the project intends to use [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Security

- Hardened the runtime image against scan findings (STRYKEA ECR-2026-12):
  apply all published Debian bookworm security updates at build time
  (`apt-get upgrade`), upgrade npm to 12.2.1 (fixes bundled `tar` and `pacote`
  CVEs) and swap its bundled `undici` (6.29.0 → 6.29.1) and `ip-address`
  (10.7.2 → 10.7.4), pin `@earendil-works/pi-coding-agent` (1.4.0) and
  `opencode-ai` (1.19.5), override `pi-coding-agent`'s pinned `undici`
  (→ 8.12.3) and bundled `@vitest/mocker` (→ 4.1.12) with patched releases,
  and pin pip packages with fixed releases (`certifi>=2026.10.18`,
  `urllib3>=2.8.0`, `requests==2.34.2`).
- Slot cleanup now SIGKILLs the child process group and then sweeps every
  remaining process owned by the slot's dedicated UID, which agents cannot
  change. Sweep signals are pidfd-pinned so PID reuse cannot redirect a kill
  onto an unrelated process. Slots are swept again before being leased to the
  next run, and at master startup for stale processes left by a previous
  instance.

### Fixed

- Slot agents that isolate spawned commands in their own session or process
  group (for example the ante agent runtime) can spawn subprocesses again:
  `setsid` and `setpgid` are no longer denied by the agent seccomp filter.

## [0.2.0] - 2026-08-21

### Added

- Graceful SIGINT/SIGTERM draining with exact-signal forwarding to active slot
  process groups and bounded SIGKILL escalation.

### Changed

- Byte payloads on the wire (`injected_files[].content`, stdout/stderr event
  `data`, `stdout_tail`/`stderr_tail`, and `collected_files[].content`) are now
  encoded as raw UTF-8 strings when valid UTF-8 and as JSON byte arrays only
  for binary content. Requests still accept both shapes; response-reading
  clients must handle both.
- Raised the run request limit on both control APIs from 2 MiB to 32 MiB.

### Security

- The 32 MiB run request limit raises the per-request memory ceiling on both
  control APIs, which still have no built-in rate limiting. Keep them bound to
  loopback or behind the trusted authenticated gateway described in
  `docs/security.md`.

## [0.1.0] - 2026-07-29

### Added

- Rust control plane with bounded FIFO scheduling and reusable workspaces.
- Privileged and user isolation modes.
- Streaming Unix-socket and development HTTP APIs.
- Input injection, structured result parsing, and artifact collection.
- Run-scoped outbound proxy credentials and optional HTTPS interception.
- Docker Compose profiles and Docker-backed E2E coverage.
- OpenCode and Pi Coding Agent in the runtime image.
- Agent-specific examples and GHCR release automation.

### Security

- Documented current isolation boundaries and production-hardening gaps.

[Unreleased]: https://github.com/darsh0230/agentic-sandbox/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/darsh0230/agentic-sandbox/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/darsh0230/agentic-sandbox/releases/tag/v0.1.0
