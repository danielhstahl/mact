#!/usr/bin/env bash
#
# scripts/mcp_run.sh — run one job through the MCP action runner, and fail unless it really ran.
#
# This is the runner side of the coverage and benchmark jobs. The workflow YAML starts `act-mcp
# serve`, calls this script once, and gets one exit code that means: the runner was reached, the
# runner advertises that job, the run reached a terminal state successfully, and it published
# evidence. Every one of those is a question the server itself answers (`list_tasks`, `run_status`,
# `run_workflow`'s `evidence_files`), so the assertion is about the MCP contract and not about a
# docker log somebody has to read afterwards.
#
# Why a script and not YAML: the repository's convention (docs/ci.md) is that a check lives where a
# human can run it and a test can drive it. `--check` below does exactly that against a stub server,
# with no act, no docker and no network — so the driver is covered by every CI run and only the run
# underneath it needs a container.
#
# Usage:
#   scripts/mcp_run.sh --workflow dogfood.yml --task coverage --category coverage \
#                      --commit "$(git rev-parse HEAD)" --evidence-kind coverage
#   scripts/mcp_run.sh --check        # self-test against a stub; needs python3 and nothing else
#
# The five things it fails on, each named in its own line:
#
#   unreachable     the endpoint never answered `list_tasks`
#   not-advertised  `list_tasks` does not offer that workflow/task with that `category_hint`. This is
#                   the failure a job that "works when I run it by hand" has: the runner cannot be
#                   asked to run it, so nothing downstream can depend on it.
#   not-success     the run reached a terminal state that is not `success`; the run's `log_tail` and,
#                   when it is on disk, `.evidence/runs/<run_id>/act.jsonl` are printed
#   no-evidence     the run succeeded and published no records of the requested kind — a green job
#                   with no numbers is the exact state `fathom doctor` warns about ("the run existed
#                   and the columns stay unknown"), so the job asserts it instead
#   runs-on-unmapped
#                   the run succeeded, but act stamped `act_runs_on_unmapped`: it had no image for the
#                   job's `runs-on` label and skipped that leg, exiting 0 having run nothing. Observed
#                   on an Apple-silicon host with a `ubuntu-24.04` matrix: all five legs skipped, exit
#                   code 0, zero steps run. `--allow-unmapped-runs-on` accepts it when the skipped legs
#                   are the point.
#
# Exit status: 0 everything above passed; 1 any of them failed; 2 bad usage or a missing dependency.
#
# Dependencies: curl, python3 (for JSON only — deliberately no jq), and scripts/mcp_call.sh beside
# it, which is the client that actually speaks MCP 2026-07-28 to `act-mcp serve`.

set -o errexit -o nounset -o pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
mcp_call=$script_dir/mcp_call.sh

WORKFLOW=
TASK=
CATEGORY=
COMMIT=
EVIDENCE_KIND=none
URL=${MCP_URL:-http://127.0.0.1:8080/mcp}
EVIDENCE_DIR=
WAIT_SECS=120
TIMEOUT_SECS=2400
POLL_SECS=15
SELF_CHECK=0
ALLOW_UNMAPPED=0

usage() {
    cat <<'USAGE'
Usage: scripts/mcp_run.sh [OPTIONS]

Runs one workflow job through a running `act-mcp serve` and asserts it published evidence.

  --workflow NAME        workflow as the runner takes it: `dogfood.yml` or `dogfood`
  --task ID              the job id (act -j <ID>)
  --category NAME        the runner's category: coverage | benchmark | any other lowercase word
  --commit SHA           7..40 hex commit to run (default: HEAD of the current repository)
  --evidence-kind KIND   coverage | benchmark | none (default none): require at least one record of
                         this kind under the evidence root for the commit the run executed
      --allow-unmapped-runs-on
                         accept a successful run that act stamped act_runs_on_unmapped, i.e. one whose
                         runs-on label act has no image for and therefore skipped (default: refuse)
  --url URL              MCP endpoint (default http://127.0.0.1:8080/mcp, or $MCP_URL)
  --evidence-dir PATH    evidence root to look in (default <repo>/.evidence, or $EVIDENCE_DIR)
  --wait-secs N          how long to wait for the ENDPOINT to answer list_tasks — before any run
                         exists (default 120)
  --timeout-secs N       how long to wait for the run to reach a terminal state (default 2400)
  --poll-secs N          poll interval for run_status (default 15)
      --check            self-test against a stub server: no act, no docker, no network
  -h, --help             this text

Exit: 0 ran and published; 1 unreachable / not advertised / not success / no evidence /
     runs-on unmapped; 2 usage.
USAGE
}

die() {
    printf 'mcp_run: %s\n' "$*" >&2
    exit 1
}

need() {
    command -v "$1" >/dev/null 2>&1 || {
        printf 'mcp_run: %s is required\n' "$1" >&2
        exit 2
    }
}

while [[ $# -gt 0 ]]; do
    case $1 in
        -h | --help)
            usage
            exit 0
            ;;
        --check)
            SELF_CHECK=1
            shift
            ;;
        --allow-unmapped-runs-on)
            ALLOW_UNMAPPED=1
            shift
            ;;
        --workflow)
            [[ $# -ge 2 ]] || die "$1 needs a value"
            WORKFLOW=$2
            shift 2
            ;;
        --task)
            [[ $# -ge 2 ]] || die "$1 needs a value"
            TASK=$2
            shift 2
            ;;
        --category)
            [[ $# -ge 2 ]] || die "$1 needs a value"
            CATEGORY=$2
            shift 2
            ;;
        --commit)
            [[ $# -ge 2 ]] || die "$1 needs a value"
            COMMIT=$2
            shift 2
            ;;
        --evidence-kind)
            [[ $# -ge 2 ]] || die "$1 needs a value"
            EVIDENCE_KIND=$2
            shift 2
            ;;
        --url)
            [[ $# -ge 2 ]] || die "$1 needs a value"
            URL=$2
            shift 2
            ;;
        --evidence-dir)
            [[ $# -ge 2 ]] || die "$1 needs a value"
            EVIDENCE_DIR=$2
            shift 2
            ;;
        --wait-secs)
            [[ $# -ge 2 ]] || die "$1 needs a value"
            WAIT_SECS=$2
            shift 2
            ;;
        --timeout-secs)
            [[ $# -ge 2 ]] || die "$1 needs a value"
            TIMEOUT_SECS=$2
            shift 2
            ;;
        --poll-secs)
            [[ $# -ge 2 ]] || die "$1 needs a value"
            POLL_SECS=$2
            shift 2
            ;;
        *)
            printf 'mcp_run: unknown argument: %s\n' "$1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

need curl
need python3
[[ -f $mcp_call ]] || die "scripts/mcp_call.sh is missing beside this script"
[[ -x $mcp_call ]] || chmod +x "$mcp_call" 2>/dev/null || true

# ------------------------------------------------------------------ JSON over the wire

# json_get PATH — read one dotted path out of a JSON-RPC response on stdin. `null`, a missing path
# and unparseable JSON all print nothing, so `[[ -z … ]]` is the "absent" test everywhere below.
json_get() {
    python3 -c '
import json, sys
path = sys.argv[1].split(".")
try:
    node = json.load(sys.stdin)
except Exception:
    sys.exit(0)
for key in path:
    if isinstance(node, list):
        try:
            node = node[int(key)]
        except (ValueError, IndexError):
            sys.exit(0)
    elif isinstance(node, dict) and key in node:
        node = node[key]
    else:
        sys.exit(0)
if node is None:
    sys.exit(0)
sys.stdout.write(node if isinstance(node, str) else json.dumps(node))
' "$1" || true
}

# mcp TOOL [ARGS-JSON] — one tools/call through scripts/mcp_call.sh, raw JSON-RPC on stdout.
mcp() {
    MCP_URL=$URL "$mcp_call" -u "$URL" "$@"
}

# ------------------------------------------------------------------ the four assertions

TASKS_JSON=
VIEW=
RUN_ID=
RUN_STATUS=

# wait_for_endpoint — `list_tasks` is the cheapest call that proves both that the socket is up and
# that the server resolved a workspace, and a refused connection is retryable, so this is a loop.
wait_for_endpoint() {
    local deadline=$((SECONDS + WAIT_SECS))
    # Each failed attempt's reason is kept rather than dropped into /dev/null. This loop used to
    # turn two different problems into the same line: a server that never started (no `act` on
    # PATH, an unacknowledged routable bind, a workspace it refused) and a server that is merely
    # slow both produced "did not answer list_tasks within 120s" — and the first one was never a
    # problem with the run, which is what a reader then goes looking for.
    local stderr_file last rc
    stderr_file=$(mktemp -t mcp-run-endpoint.XXXXXX)
    last="not attempted"
    while true; do
        rc=0
        TASKS_JSON=$(mcp list_tasks '{}' 2>"$stderr_file") || rc=$?
        if (( rc == 0 )) &&
            [[ -n $(json_get result.structuredContent.workflows_dir <<<"$TASKS_JSON") ]]; then
            rm -f "$stderr_file"
            printf '  endpoint   %s answered list_tasks (%s)\n' "$URL" \
                "$(json_get result.structuredContent.workflows_dir <<<"$TASKS_JSON")"
            return 0
        fi
        last=$(tr '\n' ' ' < "$stderr_file")
        [[ -n ${last// /} ]] || last="no diagnostic (exit $rc)"
        if (( SECONDS >= deadline )); then
            rm -f "$stderr_file"
            die "unreachable: $URL never answered list_tasks within ${WAIT_SECS}s. That is --wait-secs, the budget for the ENDPOINT: no run was submitted. Is act-mcp serve listening there, and did it boot (its own stderr names the refusal)? last attempt said: $last"
        fi
        sleep 2
    done
}

# assert_advertised — the runner must offer this job, with this category. `list_tasks` is the same
# answer any other MCP client gets, so a job that exists only in the YAML and not in the runner's
# answer cannot pass here, which is the difference between "act can run this file" and "the runner
# can be asked to run this job".
assert_advertised() {
    local answer
    answer=$(mcp list_tasks "{\"workflow\":\"$1\"}" 2>/dev/null || true)
    [[ -n $answer ]] || answer=$TASKS_JSON
    if ANSWER=$answer python3 - "$1" "$TASK" "$CATEGORY" <<'PY'
import json
import os
import sys

want_workflow, want_task, want_category = sys.argv[1:4]
body = json.loads(os.environ["ANSWER"])
content = body.get("result", {}).get("structuredContent", {})
workflows = content.get("workflows", [])


def stem(text):
    for suffix in (".yaml", ".yml"):
        if text.endswith(suffix):
            return text[: -len(suffix)]
    return text


wanted = {want_workflow, want_workflow.split("/")[-1], stem(want_workflow.split("/")[-1])}
matches = [wf for wf in workflows if wf.get("name") in wanted]
if not matches:
    print(
        "mcp_run: not-advertised: list_tasks did not offer workflow %r; it offered: %s"
        % (
            want_workflow,
            ", ".join(sorted(str(wf.get("name")) for wf in workflows)) or "(nothing)",
        ),
        file=sys.stderr,
    )
    sys.exit(1)

jobs = matches[0].get("jobs", [])
named = [job for job in jobs if job.get("id") == want_task]
if not named:
    print(
        "mcp_run: not-advertised: workflow %r has no job %r; it has: %s"
        % (
            want_workflow,
            want_task,
            ", ".join(sorted(str(job.get("id")) for job in jobs)) or "(none)",
        ),
        file=sys.stderr,
    )
    sys.exit(1)

hint = named[0].get("category_hint")
if hint != want_category:
    print(
        "mcp_run: not-advertised: job %r advertises category_hint=%r, not %r — the runner decides "
        "that from the job name and its steps, so name the job or the step, or pass the category the "
        "runner derives" % (want_task, hint, want_category),
        file=sys.stderr,
    )
    sys.exit(1)
print("  advertised %s/%s is offered by the runner as category %s" % (want_workflow, want_task, hint))
PY
    then
        return 0
    fi
    exit 1
}

# run_and_wait — start with wait=false and poll, so a long run prints progress instead of one silent
# block (and so `--timeout-secs` can cancel rather than hang). The tool contract's own `wait`
# argument exists for this; using it is what makes the phase visible.
run_and_wait() {
    local commit=$1
    local started
    local started refuse
    started=$(mcp run_workflow "{\"workflow\":\"$WORKFLOW\",\"task\":\"$TASK\",\"category\":\"$CATEGORY\",\"commit\":\"$commit\",\"wait\":false}") || {
        # The tool's own error, named: `docker_unreachable` and `task_not_found` want different fixes, and
        # "the run was refused" is not a sentence that tells anyone which one.
        refuse=$(json_get result.structuredContent.error.message <<<"$started")
        [[ -n $refuse ]] || refuse=$(json_get result.error.message <<<"$started")
        die "run_workflow refused the run${refuse:+: $refuse}"
    }
    RUN_ID=$(json_get result.structuredContent.run_id <<<"$started")
    [[ -n $RUN_ID ]] || die "run_workflow answered no run_id: $started"
    printf '  started    %s/%s [%s] run_id=%s status=%s\n' "$WORKFLOW" "$TASK" "$CATEGORY" \
        "$RUN_ID" "$(json_get result.structuredContent.status <<<"$started")"

    VIEW=$started
    local deadline=$((SECONDS + TIMEOUT_SECS))
    local status terminal phase
    while true; do
        if status=$(mcp run_status "{\"run_id\":\"$RUN_ID\",\"log_tail_bytes\":0}" 2>/dev/null) &&
            [[ -n $(json_get result.structuredContent.run.status <<<"$status") ]]; then
            VIEW=$status
        fi
        status=$(json_get result.structuredContent.run.status <<<"$VIEW")
        terminal=$(json_get result.structuredContent.run.terminal <<<"$VIEW")
        phase=$(json_get result.structuredContent.run.phase <<<"$VIEW")
        printf '  status     %s: %s%s\n' "$RUN_ID" "${status:-unknown}" "${phase:+ ($phase)}"
        [[ $terminal == true ]] && break
        ((SECONDS < deadline)) || {
            mcp cancel_run "{\"run_id\":\"$RUN_ID\"}" >/dev/null 2>&1 || true
            die "not-success: $RUN_ID did not reach a terminal state within ${TIMEOUT_SECS}s (cancelled)"
        }
        sleep "$POLL_SECS"
    done

    RUN_STATUS=$status
    if [[ $RUN_STATUS != success ]]; then
        printf 'mcp_run: not-success: %s finished with status %s\n' "$RUN_ID" "$RUN_STATUS" >&2
        local tail
        tail=$(json_get result.structuredContent.run.log_tail <<<"$VIEW")
        [[ -n $tail ]] && printf '%s\n' "$tail" >&2
        if [[ -n $EVIDENCE_DIR && -d $EVIDENCE_DIR/runs/$RUN_ID ]]; then
            printf -- '--- %s/runs/%s/act.jsonl (last 60 lines)\n' "$EVIDENCE_DIR" "$RUN_ID" >&2
            tail -n 60 "$EVIDENCE_DIR/runs/$RUN_ID/act.jsonl" >&2 || true
        fi
        exit 1
    fi
    printf '  success    %s (%s ms)\n' "$RUN_ID" \
        "$(json_get result.structuredContent.run.duration_ms <<<"$VIEW")"

    # A successful run is not the same as a run that executed steps. act answers 0 when it skipped
    # every matrix leg because no image matches the job's `runs-on` label — which is what a label
    # act's own platform table (plus its actrc `-P` entries) does not know reads like, including
    # `${{ matrix.runner }}`, which act reads as a literal. `act_runs_on_unmapped` is the only place
    # that is visible, and a green evidence job with no steps behind it is worse than a red one.
    local warns
    warns=$(view_field warnings)
    if [[ -n $warns && $warns != "[]" ]]; then printf '  warnings   %s\n' "$warns"; fi
    if [[ $warns == *act_runs_on_unmapped* ]] && ((ALLOW_UNMAPPED == 0)); then
        die "runs-on-unmapped: $RUN_ID succeeded, but act had no image for the job's runs-on label and skipped that leg — a success here can mean zero steps ran. Give the job a label act maps out of the box (ubuntu-latest, macos-latest), map one in actrc with '-P ubuntu-24.04=...', or pass --allow-unmapped-runs-on when skipping them is the point."
    fi
}

# view_field FIELD — the run view lives at `.run` in a run_status answer and at the top level in a
# run_workflow answer; both are legal here, so look in both places.
view_field() {
    local field=$1
    local value
    value=$(json_get "result.structuredContent.run.$field" <<<"$VIEW")
    [[ -n $value ]] || value=$(json_get "result.structuredContent.$field" <<<"$VIEW")
    printf '%s' "$value"
}

# assert_evidence — both halves: what the run said it published, and what is on disk for the commit
# it ran. The second is the one that matters to fathom, which reads the tree and not the response.
assert_evidence() {
    local commit=$1
    printf '  evidence   the run published %s record(s) in %s\n' \
        "$(view_field evidence_record_count)" "$(view_field evidence_files)"
    [[ $EVIDENCE_KIND == none ]] && return 0
    [[ -n $EVIDENCE_DIR ]] || die "--evidence-kind $EVIDENCE_KIND needs an evidence dir (--evidence-dir)"
    local dir=$EVIDENCE_DIR/$EVIDENCE_KIND/$commit
    [[ -d $dir ]] ||
        die "no-evidence: $dir does not exist — the run succeeded and wrote no $EVIDENCE_KIND records"
    local records
    records=$(find "$dir" -name '*.jsonl' -type f -exec cat {} + 2>/dev/null |
        grep -c '[^[:space:]]' || true)
    [[ ${records:-0} -gt 0 ]] ||
        die "no-evidence: $dir holds .jsonl files but not one record in them"
    printf '  evidence   %s %s record(s) under %s\n' "$records" "$EVIDENCE_KIND" "$dir"
}

# ------------------------------------------------------------------ self-test

# self_check — drive the real code path (mcp_call.sh → HTTP → these assertions) against a stub that
# answers the three tools this script uses. Four scenarios, because "the driver works" has to mean
# it also fails when it should: success, a failed run, a successful run that published nothing, and a
# job the runner does not offer.
self_check() {
    need python3
    local dir
    dir=$(mktemp -d -t mcp-run-check.XXXXXX)
    # shellcheck disable=SC2064
    trap "rm -rf '$dir'" EXIT

    cat >"$dir/stub.py" <<'PY'
import http.server
import json
import os
import sys

SCENARIO = os.environ["STUB_SCENARIO"]
RUN_ID = "20261119T000000Z-abcdef"
COMMIT = "0123456789abcdef0123456789abcdef01234567"


def jobs():
    listed = [{"id": "check", "name": "check", "category_hint": "other"}]
    if SCENARIO != "not-advertised":
        listed.insert(0, {"id": "coverage", "name": "Coverage", "category_hint": "coverage"})
    return listed


def run_view(terminal=True):
    view = {
        "run_id": RUN_ID,
        "status": "failed" if SCENARIO == "failed" else "success",
        "terminal": terminal,
        "phase": "done" if terminal else "running",
        "workflow": "dogfood",
        "task": "coverage",
        "category": "coverage",
        "commit": COMMIT,
        "duration_ms": 1234,
        "evidence_files": [],
        "evidence_record_count": 0,
        "log_tail": "step exited 1\n",
    }
    if SCENARIO in ("success", "no-evidence-on-disk", "runs-on-unmapped"):
        view["evidence_files"] = ["/evidence/coverage/%s/%s.jsonl" % (COMMIT, RUN_ID)]
        view["evidence_record_count"] = 3
    if SCENARIO == "runs-on-unmapped":
        # The shape act gives a job whose runs-on label it cannot map: terminal success, and the
        # skip only in the warnings. The run published its records — the leg that published them
        # may be the one you meant, or none of them.
        view["warnings"] = ["act_runs_on_unmapped"]
    return view


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        body = json.loads(self.rfile.read(length) or b"{}")
        name = body.get("params", {}).get("name")
        if name == "list_tasks":
            content = {
                "workflows_dir": ".github/workflows",
                "workflow_count": 1,
                "workflows": [
                    {
                        "name": "dogfood",
                        "file": ".github/workflows/dogfood.yml",
                        "job_count": len(jobs()),
                        "jobs": jobs(),
                    }
                ],
                "warnings": [],
            }
        elif name == "run_workflow":
            content = dict(run_view(terminal=False), deduplicated=False)
        elif name == "run_status":
            content = {"run": run_view(), "warnings": []}
        elif name == "cancel_run":
            content = {"run_id": RUN_ID, "status": "cancelled", "requested": True}
        else:
            content = {"unexpected_tool": name}
        payload = json.dumps(
            {
                "jsonrpc": "2.0",
                "id": body.get("id", 1),
                "result": {"content": [], "structuredContent": content},
            }
        ).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


http.server.HTTPServer(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()
PY

    local failures=0
    # A spread of ports rather than a fixed one, because two check runs on one machine — CI's matrix, or
    # this script and an e2e suite that also binds loopback — should not collide.
    local port=$((12000 + (RANDOM % 20000)))
    local scenario
    for scenario in success failed no-evidence-on-disk not-advertised runs-on-unmapped; do
        STUB_SCENARIO=$scenario python3 "$dir/stub.py" "$port" &
        local stub=$!
        sleep 1

        local commit=0123456789abcdef0123456789abcdef01234567
        # An evidence tree that matches the scenario, so `assert_evidence` has something real to
        # count — or deliberately nothing to count.
        mkdir -p "$dir/$scenario/.evidence/coverage/$commit"
        if [[ $scenario == success || $scenario == runs-on-unmapped ]]; then
            # runs-on-unmapped gets a real evidence tree on purpose: the guard under test is the
            # warning, and a scenario that also failed the evidence check would pass for the
            # wrong reason.
            for n in 1 2 3; do
                printf '{"schema_version":1,"kind":"coverage"}\n' \
                    >"$dir/$scenario/.evidence/coverage/$commit/record-$n.jsonl"
            done
        else
            : >"$dir/$scenario/.evidence/coverage/$commit/empty.jsonl"
        fi

        local want=0
        [[ $scenario == success ]] || want=1
        local out code=0
        out=$(MCP_URL="http://127.0.0.1:$port/mcp" \
            "$script_dir/mcp_run.sh" \
            --workflow dogfood.yml --task coverage --category coverage \
            --commit "$commit" --evidence-kind coverage \
            --evidence-dir "$dir/$scenario/.evidence" \
            --wait-secs 15 --timeout-secs 30 --poll-secs 1 2>&1) || code=$?

        # The opt-out has to work, or the guard is a wall instead of a switch: same stub, same
        # refusal available, one extra flag, and the run is accepted.
        local opt_note=""
        if [[ $scenario == runs-on-unmapped ]]; then
            MCP_URL="http://127.0.0.1:$port/mcp" \
                "$script_dir/mcp_run.sh" \
                --workflow dogfood.yml --task coverage --category coverage \
                --commit "$commit" --evidence-kind coverage \
                --evidence-dir "$dir/$scenario/.evidence" --allow-unmapped-runs-on \
                --wait-secs 15 --timeout-secs 30 --poll-secs 1 >/dev/null 2>&1 ||
                opt_note="--allow-unmapped-runs-on did not accept it"
        fi
        kill "$stub" 2>/dev/null || true
        wait "$stub" 2>/dev/null || true

        local note=""
        [[ $code -eq $want ]] || note="expected exit $want, got $code"
        [[ -z $opt_note ]] || note="$note; $opt_note"
        case $scenario in
            success)
                grep -q 'advertised' <<<"$out" || note="no advertised line: $out"
                grep -q '3 coverage record(s)' <<<"$out" || note="no evidence tally: $out"
                ;;
            failed) grep -q 'not-success' <<<"$out" || note="did not name not-success: $out" ;;
            no-evidence-on-disk)
                grep -q 'no-evidence' <<<"$out" || note="did not name no-evidence: $out"
                ;;
            not-advertised)
                grep -q 'not-advertised' <<<"$out" || note="did not name not-advertised: $out"
                ;;
            runs-on-unmapped)
                grep -q 'runs-on-unmapped' <<<"$out" ||
                    note="did not name runs-on-unmapped: $out"
                ;;
        esac
        if [[ -n $note ]]; then
            printf 'FAIL %-20s %s\n' "$scenario" "$note"
            failures=$((failures + 1))
        else
            printf 'ok   %-20s exit %s\n' "$scenario" "$code"
        fi
        port=$((port + 1))
    done

    # And the case with no stub at all: nothing is listening, which is the failure this script used
    # to report as a run timeout. It must be refused fast, and it must say it is the endpoint.
    local silent_code=0
    local silent
    silent=$("$script_dir/mcp_run.sh" \
        --workflow dogfood.yml --task coverage --category coverage \
        --commit "$commit" --url "http://127.0.0.1:$port/mcp" \
        --wait-secs 2 --timeout-secs 5 2>&1) || silent_code=$?
    if (( silent_code != 1 )) || ! grep -q 'never answered list_tasks' <<<"$silent" ||
        ! grep -q 'ENDPOINT' <<<"$silent"; then
        printf 'FAIL %-20s expected the endpoint refusal, got exit %s: %s\n' \
            endpoint-silent "$silent_code" "$silent"
        failures=$((failures + 1))
    else
        printf 'ok   %-20s refused with no server and said so\n' endpoint-silent
    fi

    ((failures == 0)) || {
        printf 'mcp_run --check: %s scenario(s) failed\n' "$failures" >&2
        return 1
    }
    printf 'mcp_run --check: 6 scenarios went through the real code path and answered as designed\n'
}

# ------------------------------------------------------------------ main

if ((SELF_CHECK)); then
    self_check
    exit $?
fi

[[ -n $WORKFLOW && -n $TASK && -n $CATEGORY ]] || {
    printf 'mcp_run: --workflow, --task and --category are all required (try --help)\n' >&2
    exit 2
}

if [[ -z $COMMIT ]]; then
    COMMIT=$(git rev-parse HEAD 2>/dev/null) ||
        die "--commit is required outside a git repository"
fi
# 7..40 hex, lowercased before the call: the contract refuses a ref name, which is why `HEAD` is
# resolved here rather than passed through (docs/tool-contract.md §3.1).
[[ $COMMIT =~ ^[0-9a-fA-F]{7,40}$ ]] ||
    die "--commit must be 7-40 hex git characters, got '$COMMIT'"
COMMIT=$(printf '%s' "$COMMIT" | tr '[:upper:]' '[:lower:]')

if [[ -z $EVIDENCE_DIR ]]; then
    EVIDENCE_DIR=${EVIDENCE_DIR:-$(git rev-parse --show-toplevel 2>/dev/null || true)}
    [[ -n $EVIDENCE_DIR ]] && EVIDENCE_DIR=$EVIDENCE_DIR/.evidence
fi

printf 'mcp_run %s/%s via %s\n' "$WORKFLOW" "$TASK" "$URL"
wait_for_endpoint
assert_advertised "${WORKFLOW#.github/workflows/}"
run_and_wait "$COMMIT"
if [[ $EVIDENCE_KIND != none ]]; then
    # The commit the run *executed* — the abbreviation is the caller's, and the server may have been
    # asked with any of them.
    executed=$(view_field commit)
    assert_evidence "${executed:-$COMMIT}"
fi
if [[ $EVIDENCE_KIND == none ]]; then
    printf 'mcp_run OK %s/%s ran to success through the MCP runner (this job publishes no evidence)\n' \
        "$WORKFLOW" "$TASK"
else
    printf 'mcp_run OK %s/%s ran through the MCP runner and published %s evidence\n' \
        "$WORKFLOW" "$TASK" "$EVIDENCE_KIND"
fi
