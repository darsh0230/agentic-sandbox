# Documentation

This directory is the source of truth for Agentic Sandbox documentation. Documents
describe behavior implemented by the current codebase; historical implementation
plans are preserved in Git history rather than kept beside the maintained docs.

## Guides

| Document | Contents |
| --- | --- |
| [Runtime and configuration](runtime.md) | Docker, Compose, CLI flags, environment variables, filesystem layout, and development commands |
| [API reference](api.md) | HTTP and Unix-socket protocols, run requests, events, and results |
| [Architecture](architecture.md) | Components, run lifecycle, slot allocation, and isolation modes |
| [Outbound proxy](proxy.md) | Header rules, upstream routing, global configuration, and HTTPS interception |
| [Security model](security.md) | Trust boundaries, required privileges, known gaps, and deployment guidance |

## Documentation policy

- Keep commands executable against the current repository.
- Describe planned work as a limitation or tracked issue, not as implemented
  behavior.
- Put operational and user-facing behavior here; keep code-specific rationale
  close to the code when it is only relevant to maintainers.
- Update the relevant document in the same change that modifies a public request
  field, CLI flag, environment variable, default path, or security boundary.

