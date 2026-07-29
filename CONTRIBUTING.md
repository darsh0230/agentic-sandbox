# Contributing

Thanks for helping improve Agentic Sandbox. The project is experimental and
security-sensitive, so small, reviewable changes with tests and explicit threat
model notes are preferred.

## Before opening a pull request

1. Check the [roadmap](ROADMAP.md) and existing GitHub issues.
2. For a substantial feature or isolation-model change, open an issue first so
   the design and security consequences can be discussed.
3. Do not open a public issue for a suspected vulnerability. Follow
   [SECURITY.md](SECURITY.md) instead.

## Local checks

The host build supports development and most tests on non-Linux systems, but the
real isolation setup and Docker E2E suite require Linux containers.

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets
docker compose --profile user config --quiet
docker compose --profile privileged config --quiet
```

With a Docker daemon available:

```bash
cargo test --locked --test docker_e2e -- --ignored --test-threads=1
```

The E2E suite owns disposable images, containers, networks, and volumes. It can
take over a minute because it waits through a production heartbeat interval.

## Pull request expectations

- Explain the user-visible outcome and why the change is needed.
- Add or update tests for behavior changes.
- Update documentation when the API, configuration, isolation boundary, or known
  limitations change.
- Keep credentials, provider tokens, generated agent output, and local state out
  of commits.
- Call out new capabilities, syscalls, mounts, environment variables, network
  access, or trusted inputs in the pull request description.

Security-sensitive changes should update `docs/security.md` in the same pull
request.

## Good first contributions

The [good first issue candidates](ROADMAP.md#good-first-issue-candidates) are
intentionally bounded tasks. Before starting one, check GitHub to make sure
someone has not already claimed it. Maintainers can convert these candidates
into labeled issues as the project is prepared for wider contribution.

