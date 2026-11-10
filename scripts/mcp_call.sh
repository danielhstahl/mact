#!/usr/bin/env bash
#
# scripts/mcp_call.sh — a hand-sized MCP client for this server, so the README's runbook
# is a paste rather than a page of curl.
#
# It speaks exactly what `act-mcp serve` requires at the 2026-07-28 revision: a
# self-contained POST per request (no `initialize`, no session id), the
# `MCP-Protocol-Version` header *and* the matching `params._meta` block, and the
# SEP-2243 `Mcp-Method`/`Mcp-Name` routing headers. `Accept` names both media types
# even though the answer is always one JSON body, because the revision says a client
# must be able to read either (docs/tool-contract.md §1).
#
# Usage:
#   scripts/mcp_call.sh list_tasks                       # no arguments
#   scripts/mcp_call.sh list_tasks '{"workflow":"coverage"}'
#   scripts/mcp_call.sh run_workflow '{"workflow":"coverage","task":"coverage","category":"coverage","commit":"HEAD"}'
#   scripts/mcp_call.sh run_status '{"run_id":"20261110T004203Z-a1b2c3"}'
#   scripts/mcp_call.sh --tools-list                     # tools/list instead of tools/call
#   scripts/mcp_call.sh -u http://127.0.0.1:9000/mcp run_status '{}'
#
# `arguments` is taken verbatim, so keep it one compact JSON object (single-quote it in
# the shell). Every tool argument and its meaning is in docs/tool-contract.md §2.
#
# Output: the raw JSON-RPC response on stdout, as one line. Pipe it to `jq`; the
# README's examples use `| jq .result.structuredContent`.
#
# Exit status: 0 the call answered normally; 1 the tool answered an error
# (`isError: true`) or the endpoint refused the request (non-2xx, or no answer);
# 2 bad usage or no curl.
#
# This is a debugging tool, not a client library: it hardcodes one revision, loopback,
# and no auth, because that is all this server is.

set -o errexit -o nounset -o pipefail

URL=${MCP_URL:-http://127.0.0.1:8080/mcp}
METHOD=tools/call

usage() {
    cat <<'USAGE'
Usage: scripts/mcp_call.sh [-u URL] [--tools-list] [TOOL] [ARGUMENTS-JSON]

Calls one tool on a running `act-mcp serve` over streamable HTTP at MCP 2026-07-28.

  TOOL             one of list_tasks, run_workflow, run_status, cancel_run
  ARGUMENTS-JSON   one compact JSON object (default {})
  -u, --url URL    endpoint (default http://127.0.0.1:8080/mcp, or $MCP_URL)
      --tools-list send tools/list instead of tools/call (no TOOL needed)

Prints the raw JSON-RPC response on stdout; pipe it to jq.
Exit: 0 answered ok, 1 tool error or refused request, 2 bad usage.
USAGE
}

while [[ $# -gt 0 ]]; do
    case $1 in
        -h | --help)
            usage
            exit 0
            ;;
        -u | --url)
            [[ $# -ge 2 ]] || {
                echo "mcp_call: --url needs a value" >&2
                exit 2
            }
            URL=$2
            shift 2
            ;;
        --tools-list)
            METHOD=tools/list
            shift
            ;;
        *) break ;;
    esac
done

command -v curl >/dev/null || {
    echo "mcp_call: curl is required" >&2
    exit 2
}

TOOL=${1:-}
ARGUMENTS=${2:-'{}'}
ROUTING_NAME=
META='"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28",'
META+='"io.modelcontextprotocol/clientInfo":{"name":"mcp_call.sh","version":"0"},'
META+='"io.modelcontextprotocol/clientCapabilities":{}}'

if [[ $METHOD == tools/list ]]; then
    BODY=$(printf '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{%s}}' "$META")
else
    [[ -n $TOOL ]] || {
        echo "mcp_call: give a tool name, or --tools-list (try --help)" >&2
        exit 2
    }
    BODY=$(printf '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"%s","arguments":%s,%s}}' \
        "$TOOL" "$ARGUMENTS" "$META")
    ROUTING_NAME=$TOOL
fi

# The body goes in a file rather than `-d` so a multi-line argument blob cannot turn
# into a header-smuggling or line-breaking request.
BODY_FILE=$(mktemp -t mcp_call_body.XXXXXX)
trap 'rm -f "$BODY_FILE"' EXIT
printf '%s' "$BODY" > "$BODY_FILE"

if ! HTTP_CODE=$(curl -sS -o "$BODY_FILE" -w '%{http_code}' "$URL" \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' \
    -H 'MCP-Protocol-Version: 2026-07-28' \
    -H "Mcp-Method: $METHOD" \
    ${ROUTING_NAME:+-H "Mcp-Name: $ROUTING_NAME"} \
    --data-binary "@$BODY_FILE"); then
    echo "mcp_call: no answer from $URL - is act-mcp serve running there?" >&2
    exit 1
fi

cat "$BODY_FILE"
printf '\n' # one response per line: the response itself has no trailing newline

if [[ $HTTP_CODE != 2* ]]; then
    printf 'mcp_call: HTTP %s from %s (the body above says why)\n' "$HTTP_CODE" "$URL" >&2
    exit 1
fi
if grep -q '"isError":true' "$BODY_FILE"; then
    printf 'mcp_call: the tool answered an error (isError:true) — read error.code above\n' >&2
    exit 1
fi
exit 0
