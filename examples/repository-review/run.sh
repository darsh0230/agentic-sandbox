#!/usr/bin/env bash
set -euo pipefail

: "${AGENT_API_KEY_NAME:?set AGENT_API_KEY_NAME to the provider credential variable, for example ANTHROPIC_API_KEY}"
: "${AGENT_API_KEY:?set AGENT_API_KEY to the provider credential value}"

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
request_file=$(mktemp "${TMPDIR:-/tmp}/agentic-sandbox-review.XXXXXX")
chmod 0600 "$request_file"
trap 'rm -f "$request_file"' EXIT

command=$(cat <<'COMMAND'
mkdir -p output
set +e
if [ -n "${PI_MODEL:-}" ]; then
  pi --model "$PI_MODEL" --tools read,grep,find,ls -p "$(cat task.md)" 2>&1 | tee output/review.md
else
  pi --tools read,grep,find,ls -p "$(cat task.md)" 2>&1 | tee output/review.md
fi
agent_status=${PIPESTATUS[0]}
printf '{"agent_exit":%s}\n' "$agent_status" >output/result.json
exit "$agent_status"
COMMAND
)

jq -n \
  --arg run_id "pi-review-$(date +%s)" \
  --arg command "$command" \
  --arg key_name "$AGENT_API_KEY_NAME" \
  --arg key_value "$AGENT_API_KEY" \
  --arg model "${PI_MODEL:-}" \
  --rawfile source "$script_dir/payment.py" \
  --rawfile task "$script_dir/task.md" \
  '{
    run_id: $run_id,
    command: ["bash", "-lc", $command],
    injected_files: [
      {relative_path: "payment.py", content_utf8: $source, mode: 420},
      {relative_path: "task.md", content_utf8: $task, mode: 420}
    ],
    env: [[$key_name, $key_value], ["PI_MODEL", $model]],
    timeout_ms: 300000,
    result_policy: {
      collect_paths: ["output/review.md", "output/result.json"],
      result_file: "output/result.json"
    }
  }' >"$request_file"

"$script_dir/../run-agent-request.sh" "$request_file" "$script_dir/output"

