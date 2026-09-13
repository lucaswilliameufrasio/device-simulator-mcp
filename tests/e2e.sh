#!/usr/bin/env bash
set -euo pipefail

binary="${1:-target/release/device-simulator-mcp}"

if [[ -z "${DEVICE_PLATFORM:-}" ]]; then
  printf 'DEVICE_PLATFORM must be ios or android\n' >&2
  exit 2
fi

call_tool() {
  local request_id="$1"
  local tool_name="$2"
  local arguments="$3"

  printf '%s\n' \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"device-simulator-e2e","version":"0.1.1"}}}' \
    '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
    "{\"jsonrpc\":\"2.0\",\"id\":${request_id},\"method\":\"tools/call\",\"params\":{\"name\":\"${tool_name}\",\"arguments\":${arguments}}}" |
    "$binary" |
    jq --exit-status --argjson request_id "$request_id" \
      'select(.id == $request_id) | .result.isError == false' >/dev/null
}

call_tool 2 device_start '{}'
call_tool 3 device_status '{}'
call_tool 4 device_capture '{"name":"e2e"}'
call_tool 5 device_tap '{"x":0.01,"y":0.01}'
call_tool 6 device_swipe '{"x1":0.01,"y1":0.01,"x2":0.02,"y2":0.02}'
call_tool 7 device_type '{"text":"e2e test"}'
call_tool 8 device_stop '{}'

printf 'E2E passed for %s\n' "$DEVICE_PLATFORM"
