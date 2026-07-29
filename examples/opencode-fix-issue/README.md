# OpenCode: fix a failing test

This example injects a tiny Python repository with a broken `divide` function,
asks OpenCode to fix the implementation, runs the tests, and collects both the
patch and test output.

From the repository root:

```bash
AGENT_API_KEY_NAME=ANTHROPIC_API_KEY \
AGENT_API_KEY='your-provider-key' \
./examples/opencode-fix-issue/run.sh
```

OpenCode can use other providers. Set `AGENT_API_KEY_NAME` to that provider's
documented environment variable. To select a model explicitly:

```bash
OPENCODE_MODEL=provider/model \
AGENT_API_KEY_NAME=PROVIDER_API_KEY \
AGENT_API_KEY='your-provider-key' \
./examples/opencode-fix-issue/run.sh
```

The script saves `agent.patch`, `test-output.txt`, the structured `result.json`,
and the raw NDJSON response under `output/`.

