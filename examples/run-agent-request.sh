#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 REQUEST_JSON OUTPUT_DIRECTORY" >&2
  exit 2
fi

request_file=$1
output_dir=$2
api_url=${AGENTIC_SANDBOX_URL:-http://127.0.0.1:8080/api/run}
http_token=${AGENTIC_SANDBOX_HTTP_TOKEN:-change-me-dev-token}
response_file=$output_dir/response.ndjson

if [[ ! -f $request_file ]]; then
  echo "request file not found: $request_file" >&2
  exit 2
fi

jq -e '
  type == "object" and
  (.run_id | type == "string" and length > 0) and
  (.command | type == "array" and length > 0) and
  (.env | type == "array")
' "$request_file" >/dev/null

if [[ ${AGENTIC_SANDBOX_DRY_RUN:-0} == 1 ]]; then
  echo "validated request: $(jq -r '.run_id' "$request_file")"
  exit 0
fi

mkdir -p "$output_dir"
chmod 0700 "$output_dir"

curl --fail-with-body --silent --show-error --no-buffer \
  -H "Authorization: Bearer $http_token" \
  -H 'Content-Type: application/json' \
  --data-binary "@$request_file" \
  "$api_url" |
  tee "$response_file" |
  jq --unbuffered -r '
    if .type == "event" and .event.type == "started" then
      "[started] \(.event.run_id) on \(.event.slot_id)"
    elif .type == "event" and .event.type == "waiting_for_slot" then
      "[queued] position \(.event.position)"
    elif .type == "event" and (.event.type == "stdout" or .event.type == "stderr") then
      (.event.data | if type == "string" then . else implode end)
    elif .type == "event" and .event.type == "result_file_found" then
      "[artifact] \(.event.relative_path) (\(.event.size) bytes)"
    elif .type == "event" and .event.type == "exited" then
      "[exited] code=\(.event.exit_code) signal=\(.event.signal)"
    elif .type == "result" then
      "[result] exit=\(.result.exit_code) workspace_preserved=\(.result.workspace_preserved)"
    elif .type == "error" then
      "[error] \(.message)"
    else
      empty
    end
  '

result_line=$(jq -c 'select(.type == "result")' "$response_file" | tail -n 1)
if [[ -z $result_line ]]; then
  echo "the response did not contain a final result" >&2
  exit 1
fi

jq '.result.result_json' <<<"$result_line" >"$output_dir/result.json"

while IFS= read -r artifact; do
  relative_path=$(jq -r '.relative_path' <<<"$artifact")
  case $relative_path in
    "" | /* | ../* | */../* | */..)
      echo "refusing unsafe artifact path: $relative_path" >&2
      exit 1
      ;;
  esac
  local_path=${relative_path#output/}
  destination=$output_dir/$local_path
  mkdir -p "$(dirname "$destination")"
  jq -r '.content | if type == "string" then . else implode end' <<<"$artifact" >"$destination"
done < <(jq -c '.result.collected_files[]?' <<<"$result_line")

exit_code=$(jq -r '.result.exit_code // 1' <<<"$result_line")
if [[ $exit_code != 0 ]]; then
  echo "agent run exited with code $exit_code; collected diagnostics are in $output_dir" >&2
  exit "$exit_code"
fi

echo "artifacts saved under $output_dir"
