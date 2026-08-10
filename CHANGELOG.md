# Changelog

All notable changes to Agentic Sandbox will be documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
the project intends to use [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- Rust control plane with bounded FIFO scheduling and reusable workspaces.
- Privileged and user isolation modes.
- Streaming Unix-socket and development HTTP APIs.
- Input injection, structured result parsing, and artifact collection.
- Run-scoped outbound proxy credentials and optional HTTPS interception.
- Docker Compose profiles and Docker-backed E2E coverage.
- OpenCode and Pi Coding Agent in the runtime image.
- Agent-specific examples and GHCR release automation.
- Graceful SIGINT/SIGTERM draining with exact-signal forwarding to active slot
  process groups and bounded SIGKILL escalation.

### Security

- Documented current isolation boundaries and production-hardening gaps.

[Unreleased]: https://github.com/darsh0230/agentic-sandbox/commits/main
