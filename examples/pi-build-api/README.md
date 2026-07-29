# Pi: build a small API

This example asks Pi to build a dependency-free Python JSON API, write tests, and
run them. The generated source, tests, patch, and test output are collected before
the workspace is deleted.

From the repository root:

```bash
AGENT_API_KEY_NAME=ANTHROPIC_API_KEY \
AGENT_API_KEY='your-provider-key' \
./examples/pi-build-api/run.sh
```

Set `PI_MODEL=provider/model` to select a model explicitly. See Pi's provider
documentation for the key variable and model identifier used by your account.

