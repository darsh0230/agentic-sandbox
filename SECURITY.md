# Security policy

## Project status

Agentic Sandbox is experimental and has not received an independent external
security audit. It provides isolation-focused controls, but it is not yet a
complete hostile-multitenant sandbox.

Before deployment, read the full [security model](docs/security.md), including
the shared PID and network boundaries, unenforced CPU/memory/PID limits, child
seccomp limitations, and development HTTP API constraints.

## Reporting a vulnerability

Please report suspected vulnerabilities privately through
[GitHub Security Advisories](https://github.com/darsh0230/agentic-sandbox/security/advisories/new).

Include, where possible:

- the affected commit, tag, and isolation mode;
- the host kernel, container runtime, and relevant configuration;
- a minimal reproduction or proof of concept;
- the expected and observed security boundary;
- the potential impact; and
- any suggested mitigation.

Do not include live credentials, private workloads, or third-party data. Please
do not open a public GitHub issue until the report has been reviewed and a
coordinated disclosure plan has been agreed.

There is currently no guaranteed response or remediation SLA. Maintainers will
acknowledge reports and coordinate fixes as availability permits.

## Supported versions

Until the first tagged release is published, only the latest commit on the
default branch receives security fixes. A version support table will be added
when stable release lines exist.

