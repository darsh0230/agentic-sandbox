# Parallel Pi runs

This example submits four independent repository-review requests concurrently.
The streamed `started` events show which reusable slots were allocated, and each
run receives its own clean workspace and result directory.

```bash
AGENT_API_KEY_NAME=ANTHROPIC_API_KEY \
AGENT_API_KEY='your-provider-key' \
./examples/parallel-agent-runs/run.sh
```

This example makes four model calls and may incur four times the per-run provider
cost. Reduce `RUN_COUNT` if needed.

