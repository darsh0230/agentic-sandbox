#!/usr/bin/env bash
set -euo pipefail

: "${AGENT_API_KEY_NAME:?set AGENT_API_KEY_NAME to the provider credential variable, for example ANTHROPIC_API_KEY}"
: "${AGENT_API_KEY:?set AGENT_API_KEY to the provider credential value}"

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
request_file=$(mktemp "${TMPDIR:-/tmp}/agentic-sandbox-opencode.XXXXXX")
chmod 0600 "$request_file"
trap 'rm -f "$request_file"' EXIT

command=$(cat <<'COMMAND'
mkdir -p output
git init -q
git config user.name "Agentic Sandbox Demo"
git config user.email "demo@example.invalid"
git add calculator.py test_calculator.py
git commit -qm baseline
set +e
if [ -n "${OPENCODE_MODEL:-}" ]; then
  opencode run --auto --model "$OPENCODE_MODEL" "$(cat task.md)"
else
  opencode run --auto "$(cat task.md)"
fi
agent_status=$?
python3 -m unittest -v >output/test-output.txt 2>&1
test_status=$?
git diff --binary >output/agent.patch
printf '{"agent_exit":%s,"tests_exit":%s}\n' "$agent_status" "$test_status" >output/result.json
if [ "$agent_status" -ne 0 ]; then exit "$agent_status"; fi
exit "$test_status"
COMMAND
)

jq -n \
  --arg run_id "opencode-fix-$(date +%s)" \
  --arg command "$command" \
  --arg key_name "$AGENT_API_KEY_NAME" \
  --arg key_value "$AGENT_API_KEY" \
  --arg model "${OPENCODE_MODEL:-}" \
  --rawfile calculator "$script_dir/calculator.py" \
  --rawfile tests "$script_dir/test_calculator.py" \
  --rawfile task "$script_dir/task.md" \
  '{
    run_id: $run_id,
    command: ["bash", "-lc", $command],
    injected_files: [
      {relative_path: "calculator.py", content_utf8: $calculator, mode: 420},
      {relative_path: "test_calculator.py", content_utf8: $tests, mode: 420},
      {relative_path: "task.md", content_utf8: $task, mode: 420}
    ],
    env: [[$key_name, $key_value], ["OPENCODE_MODEL", $model]],
    timeout_ms: 300000,
    result_policy: {
      collect_paths: ["output/agent.patch", "output/test-output.txt", "output/result.json"],
      result_file: "output/result.json"
    }
  }' >"$request_file"

"$script_dir/../run-agent-request.sh" "$request_file" "$script_dir/output"

