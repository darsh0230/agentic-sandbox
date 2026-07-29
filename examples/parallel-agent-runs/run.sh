#!/usr/bin/env bash
set -euo pipefail

: "${AGENT_API_KEY_NAME:?set AGENT_API_KEY_NAME to the provider credential variable, for example ANTHROPIC_API_KEY}"
: "${AGENT_API_KEY:?set AGENT_API_KEY to the provider credential value}"

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
run_count=${RUN_COUNT:-4}

if ! [[ $run_count =~ ^[1-8]$ ]]; then
  echo "RUN_COUNT must be an integer from 1 through 8" >&2
  exit 2
fi

request_dir=$(mktemp -d "${TMPDIR:-/tmp}/agentic-sandbox-parallel.XXXXXX")
chmod 0700 "$request_dir"
trap 'rm -rf "$request_dir"' EXIT

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

pids=()
for run_number in $(seq 1 "$run_count"); do
  request_file=$request_dir/request-$run_number.json
  task="Review sample.py with emphasis on concern #$run_number: correctness, API behavior, typing, and test coverage. Do not edit files. Return concise Markdown."

  jq -n \
    --arg run_id "parallel-$run_number-$(date +%s)" \
    --arg command "$command" \
    --arg key_name "$AGENT_API_KEY_NAME" \
    --arg key_value "$AGENT_API_KEY" \
    --arg model "${PI_MODEL:-}" \
    --rawfile source "$script_dir/sample.py" \
    --arg task "$task" \
    '{
      run_id: $run_id,
      command: ["bash", "-lc", $command],
      injected_files: [
        {relative_path: "sample.py", content_utf8: $source, mode: 420},
        {relative_path: "task.md", content_utf8: $task, mode: 420}
      ],
      env: [[$key_name, $key_value], ["PI_MODEL", $model]],
      timeout_ms: 300000,
      result_policy: {
        collect_paths: ["output/review.md", "output/result.json"],
        result_file: "output/result.json"
      }
    }' >"$request_file"

  "$script_dir/../run-agent-request.sh" \
    "$request_file" \
    "$script_dir/output/run-$run_number" &
  pids+=("$!")
done

status=0
for pid in "${pids[@]}"; do
  if ! wait "$pid"; then
    status=1
  fi
done

exit "$status"

