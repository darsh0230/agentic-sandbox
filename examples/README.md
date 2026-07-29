# Examples

These examples exercise coding agents already installed in the Agentic Sandbox
runtime image.

| Example | Agent | Demonstrates |
| --- | --- | --- |
| [`opencode-fix-issue/`](opencode-fix-issue/) | OpenCode | Inject a failing repository, fix it, test it, and collect a patch |
| [`pi-build-api/`](pi-build-api/) | Pi | Build and test a small API from a task file |
| [`repository-review/`](repository-review/) | Pi | Run a read-only review and collect the Markdown report |
| [`parallel-agent-runs/`](parallel-agent-runs/) | Pi | Submit several independent runs concurrently |

## Prerequisites

Start one Compose profile from the repository root:

```bash
docker compose --profile user up agentic-sandbox-user
```

Each agent example requires credentials for a model provider. Pass the exact
environment-variable name expected by that provider separately from the value:

```bash
export AGENT_API_KEY_NAME=ANTHROPIC_API_KEY
export AGENT_API_KEY='your-provider-key'
```

This indirection also supports providers that use a different key name. OpenCode
and Pi choose from the providers available to them; the example README explains
the optional model selector.

The scripts:

- create request JSON in a mode-`0600` temporary file;
- validate its required fields before sending it;
- send the provider key only in the run's `env` field;
- connect only to the loopback development API by default;
- remove the temporary request on exit; and
- save collected text artifacts under the example's ignored `output/`
  directory.

The development HTTP API is plaintext. Do not point these scripts at an untrusted
or non-loopback endpoint, and do not use production credentials for public demos.
Agent calls may incur provider charges.

Set `AGENTIC_SANDBOX_DRY_RUN=1` with a dummy credential to validate request
generation without contacting the service or a model provider.

Override `AGENTIC_SANDBOX_URL` or `AGENTIC_SANDBOX_HTTP_TOKEN` only when you have
intentionally changed the local service configuration.
