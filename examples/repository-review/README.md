# Pi: review a repository

This example gives Pi only read-oriented tools, streams its review to stdout, and
collects the same Markdown output as an artifact.

```bash
AGENT_API_KEY_NAME=ANTHROPIC_API_KEY \
AGENT_API_KEY='your-provider-key' \
./examples/repository-review/run.sh
```

The fixture deliberately contains issues for the agent to find. It is not
production code.

