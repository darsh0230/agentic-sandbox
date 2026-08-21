# Changelog

All notable changes to Agentic Sandbox will be documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
the project intends to use [Semantic Versioning](https://semver.org/).

## [Unreleased]

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
