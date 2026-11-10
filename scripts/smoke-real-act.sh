#!/usr/bin/env bash
#
# scripts/smoke-real-act.sh — prove the pipeline against real act on a real container
# runtime, and capture what act's `--json` log actually looks like.
#
# Everything else in this repository's testing story runs against a stub (docs/testing.md §1),
# which is why the assumptions about act are the dangerous ones: argv acceptance, the `--json`
# field names the extractors read, whether `--bind` really makes a job-written file visible on
# the host, how long a trivial job takes, whether `--job` drags a `needs:` chain with it. Those
# are third-party behaviours, and a stub can never contradict an assumption about them. This
# script can, and that is its whole job.
#
# It prints PASS / FAIL / SKIP per step with the evidence paths, then a summary of wall time and
# byte sizes per run — the numbers ADR-0004 §4.2's timeout defaults were reasoned about rather
# than measured against. Exit 0 means every hard step passed.
#
# ─────────────────────────────────────────────────────────────────────────────────────────
# WHAT IT DOES
#
#   prerequisites   git/curl/jq/python3, act present and ≥ --min-act-version, the container
#                   runtime answering the *same* probe the server uses (`docker info`), free
#                   disk for an image pull, the act-mcp binary running, and — needing no
#                   daemon at all — real act accepting every flag src/runner/argv.rs emits
#   fixture         copy tests/realdata/fixture into a throwaway git repo and commit it
#   boot            `act-mcp serve --bind 127.0.0.1:0` against that repo; the port is read
#                   back out of the listening line, exactly as tests/common reads it
#   list_tasks      both workflows and both job ids are visible
#   run ×2          run_workflow(coverage) then run_workflow(benchmark), wait:true, timed
#   run_status      each run is terminal and agrees with what run_workflow returned
#   payload         the files the *job* wrote inside the container are here on the host
#   log             runs/<run_id>/act.jsonl exists and contains the job's own stdout
#   shape           scripts/realdata_report.py compares the log against
#                   tests/realdata/golden/act-shape.json and prints every divergence
#   validate        `act-mcp validate-evidence` over the produced tree, exit 0
#   records         the values the fixture hard-codes are the values the records carry
#   expected        optional byte-for-byte diff against tests/realdata/expected
#   actrc           (real mode) a workspace `.actrc` pinning an unpullable image is inert —
#                   act reads `.actrc` from its cwd, which is the run scratch (ADR-0003 F2/F3)
#   worktree        (real mode; --skip-worktree to skip) the same run under
#                   --isolation worktree below $XDG_RUNTIME_DIR — ADR-0003 V1
#   cancel          (--with-cancel) start then cancel_run and look for orphaned containers:
#                   ADR-0003 V3, plus the `docker ps` filter ADR-0004 §11.2 owes the runbook
#   summary         the per-run timing/size table against the ADR-0004 defaults
#
# ─────────────────────────────────────────────────────────────────────────────────────────
# MODES
#
#   (default)       real act, real container runtime. This is the deliverable.
#   --stub          the identical steps against tests/e2e/testdata/bin/{act,docker}: no
#                   docker, no network, a few seconds. It proves THIS SCRIPT's plumbing (boot,
#                   every MCP call, the evidence machinery, the summary) and nothing about act
#                   — so it is wired into `cargo test --test realdata_smoke`, and a red --stub
#                   run means the smoke is broken rather than that act changed. The fixture's
#                   value assertions are real-mode only: the stub's payload comes from
#                   tests/e2e/testdata/payload, not from this fixture's steps.
#   --prereq-only   run the prerequisite report and stop.
#
# ─────────────────────────────────────────────────────────────────────────────────────────
# WHAT A DIVERGENCE MEANS, AND WHAT TO DO WITH IT
#
# tests/realdata/golden/act-shape.json records what this crate's extractors assume about act's
# `--json` log, field by field, with the provenance of every claim. When a live act contradicts
# one of those claims the script prints
#
#     DIVERGENCE — a field something in this crate reads did not hold up
#
# with the field, the extractor that reads it, and a ready-to-run `bd create` line. Open that
# ticket. Do not edit act-shape.json to match the log before reading the extractor: the file
# exists so that a silent third-party change becomes a named gap instead of a plausible number.
# Two gaps are already recorded there from source inspection (act emits `executionTime` only
# from v0.2.76, and `Output`/`Error` are not act's fields at all); the script prints them when
# a capture triggers them. A divergence does not fail the run unless --strict-shape is given:
# it is a follow-up ticket, not a broken build.
#
# The golden log: --capture-golden copies the coverage run's act.jsonl to
# tests/realdata/golden/act.jsonl plus a provenance sidecar. Nothing is committed here yet
# because nothing has been *captured* yet — tests/realdata/golden/README.md is the policy, and
# .github/workflows/nightly-real-act.yml runs this script nightly with --capture-golden and
# uploads the result as an artefact for a human to review and commit.
#
# ─────────────────────────────────────────────────────────────────────────────────────────
# Usage: scripts/smoke-real-act.sh [OPTIONS]
#
#   --bin PATH         act-mcp binary (default: $CARGO_TARGET_DIR|target/debug/act-mcp,
#                      then target/release/act-mcp)
#   --act PATH         act binary (default: `act` on PATH; also $ACT_BINARY)
#   --out DIR          where the throwaway workspace and scratch go (default: mktemp -d)
#   --keep             keep --out and print it instead of deleting it
#   --isolation POL    isolation policy for the main runs (default: hybrid)
#   --min-act-version V lowest act version to accept (default: 0.2.62, this server's floor)
#   --need-mb N        free disk required at --out (default: 4096)
#   --timeout-secs N   pass timeout_secs to run_workflow (default: do not pass it, so the run
#                      is measured against the shipped per-category defaults)
#   --skip-worktree    skip the --isolation worktree step (real mode only step)
#   --with-cancel      also run the cancel / orphaned-container step
#   --strict-shape     make an act --json shape divergence fail the run
#   --capture-golden   write the captured act --json log (and provenance) into the golden dir
#   --golden-dir DIR   golden directory (default: tests/realdata/golden)
#   --write-expected   write tests/realdata/expected from this run (real mode; human review!)
#   -q, --quiet        step verdicts and the summary only
#   --no-colour        no ANSI
#   -h, --help         this text
#
# Exit: 0 every hard step passed · 1 a step failed · 2 the script could not run.

set -o errexit -o nounset -o pipefail

# --------------------------------------------------------------------------- options
MODE=real
BIN=${ACT_MCP_BIN:-}
ACT=${ACT_BINARY:-}
OUT=${SMOKE_OUT:-}
KEEP=0
ISOLATION=hybrid
MIN_ACT_VERSION=0.2.62
NEED_MB=4096
TIMEOUT_SECS=
SKIP_WORKTREE=0
WITH_CANCEL=0
STRICT_SHAPE=0
CAPTURE_GOLDEN=0
GOLDEN_DIR=
WRITE_EXPECTED=0
QUIET=0

# The header comment *is* this script's documentation; print it rather than maintain a second
# copy that will disagree with it within one edit.
usage() { awk 'NR > 2 && /^#/ { sub(/^# ?/, ""); print; next } NR > 2 { exit }' "${BASH_SOURCE[0]}"; }

while [[ $# -gt 0 ]]; do
    case "$1" in
        --stub) MODE=stub ;;
        --prereq-only) MODE=prereq ;;
        --bin) BIN=${2:?--bin needs a value}; shift ;;
        --act) ACT=${2:?--act needs a value}; shift ;;
        --out) OUT=${2:?--out needs a value}; shift ;;
        --keep) KEEP=1 ;;
        --isolation) ISOLATION=${2:?--isolation needs a value}; shift ;;
        --min-act-version) MIN_ACT_VERSION=${2:?}; shift ;;
        --need-mb) NEED_MB=${2:?}; shift ;;
        --timeout-secs) TIMEOUT_SECS=${2:?}; shift ;;
        --skip-worktree) SKIP_WORKTREE=1 ;;
        --with-cancel) WITH_CANCEL=1 ;;
        --strict-shape) STRICT_SHAPE=1 ;;
        --capture-golden) CAPTURE_GOLDEN=1 ;;
        --golden-dir) GOLDEN_DIR=${2:?}; shift ;;
        --write-expected) WRITE_EXPECTED=1 ;;
        -q | --quiet) QUIET=1 ;;
        --no-colour) NO_COLOR=1 ;;
        -h | --help) usage; exit 0 ;;
        *)
            printf 'smoke-real-act: unknown option %s (try --help)\n' "$1" >&2
            exit 2
            ;;
    esac
    shift
done

# --------------------------------------------------------------------------- where we are
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
if repo_root=$(git -C "$script_dir" rev-parse --show-toplevel 2>/dev/null); then
    :
else
    repo_root=$(cd -- "$script_dir/.." && pwd)
fi
fixture_dir=$repo_root/tests/realdata/fixture
GOLDEN_DIR=${GOLDEN_DIR:-$repo_root/tests/realdata/golden}
expected_dir=$repo_root/tests/realdata/expected
report=$repo_root/scripts/realdata_report.py
mcp_client=$repo_root/scripts/mcp_call.sh

if [[ -z "$BIN" ]]; then
    for candidate in \
        "${CARGO_TARGET_DIR:-$repo_root/target}/debug/act-mcp" \
        "${CARGO_TARGET_DIR:-$repo_root/target}/release/act-mcp" \
        "$repo_root/target/release/act-mcp"; do
        if [[ -x "$candidate" ]]; then BIN=$candidate; break; fi
    done
fi

if [[ -t 1 && -z "${NO_COLOR:-}" ]]; then
    c_bold=$'\033[1m'; c_red=$'\033[31m'; c_green=$'\033[32m'; c_yellow=$'\033[33m'; c_off=$'\033[0m'
else
    c_bold='' c_red='' c_green='' c_yellow='' c_off=''
fi

# --------------------------------------------------------------------------- state
declare -a step_status=()
declare -a step_label=()
declare -a step_note=()
failed=0
diverged=0
# bash 3.2 (the one macOS ships) has no negative array index, so the last verdict is kept in
# a variable instead of read back off the list.
LAST_STEP=

declare -A RUN_ID=()          # run id per category
declare -A RUN_WALL=()        # wall-clock seconds of the run_workflow call
declare -A RUN_VIEW=()        # the structuredContent, one line of JSON
declare -A RUN_LOG=()         # runs/<id>/act.jsonl
declare -A RUN_LOG_BYTES=()
declare -A RUN_EVIDENCE_BYTES=()

# Every server this script started, so none of them outlives it.
declare -a server_pids=()
URL=

cleanup() {
    local pid
    for pid in ${server_pids[@]+"${server_pids[@]}"}; do
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
    if [[ -n "$OUT" && $KEEP -eq 0 && -d "$OUT" && "$OUT" != "/" ]]; then
        rm -rf "$OUT"
    fi
}
trap cleanup EXIT

say() { [[ $QUIET -eq 1 ]] || printf '%s\n' "$*"; }
detail() { [[ $QUIET -eq 1 ]] || printf '         %s\n' "$*"; }
note() { printf '         %sfix:%s %s\n' "$c_yellow" "$c_off" "$*"; }

# run_step LABEL FUNCTION [args…] — exactly one PASS/FAIL line per step.
run_step() {
    local label=$1; shift
    say "${c_bold}── $label${c_off}"
    if "$@"; then
        LAST_STEP=pass; step_status+=("pass"); step_label+=("$label"); step_note+=("")
        printf '       %sPASS%s %s\n' "$c_green" "$c_off" "$label"
    else
        local status=$?
        LAST_STEP=fail; step_status+=("fail"); step_label+=("$label"); step_note+=("")
        printf '       %sFAIL%s %s (step exit %s)\n' "$c_red" "$c_off" "$label" "$status"
        failed=1
    fi
}

skip_step() {
    LAST_STEP=skip
    step_status+=("skip"); step_label+=("$1"); step_note+=("$2")
    say "${c_bold}── $1${c_off}"
    printf '       %sSKIP%s %s — %s\n' "$c_yellow" "$c_off" "$1" "$2"
}

seconds_now() { date +%s; }
file_size() { stat -c %s "$1" 2>/dev/null || stat -f %z "$1"; }

# `version_lt 0.2.68 0.2.76` is true when the first is older.
version_lt() {
    [[ "$1" != "$2" ]] && [[ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | head -n1)" == "$1" ]]
}

# --------------------------------------------------------------------------- MCP plumbing
# mcp TOOL [ARGUMENTS-JSON] — one tools/call through scripts/mcp_call.sh, response stored
# under $OUT/responses and printed. Non-zero when the endpoint refused or the tool answered
# isError:true; the body is printed either way, because the body is the finding.
mcp() {
    local tool=$1 arguments=${2:-'{}'}
    mkdir -p "$OUT/responses"
    local name=$tool
    [[ $tool == --tools-list ]] && name=tools_list
    local file
    file=$(mktemp "$OUT/responses/${name}-XXXXXX.json")
    local rc=0
    if [[ $tool == --tools-list ]]; then
        "$mcp_client" --url "$URL" --tools-list >"$file" 2>"$file.err" || rc=$?
    else
        "$mcp_client" --url "$URL" "$tool" "$arguments" >"$file" 2>"$file.err" || rc=$?
    fi
    if [[ $rc -ne 0 ]]; then
        printf 'mcp %s: %s (full response in %s)\n' "$tool" "$(cat "$file.err")" "$file" >&2
    fi
    cat "$file"
    return $rc
}

# last_response NAME — the newest stored response for a tool.
last_response() {
    local name=${1:-}
    [[ $name == --tools-list ]] && name=tools_list
    ls -1t "$OUT/responses/${name}"-*.json 2>/dev/null | head -n1
}

# content FILE — its structuredContent, or whatever error came back.
content() { jq -c '.result.structuredContent // .error // .' "$1" 2>/dev/null; }

# --------------------------------------------------------------------------- steps
check_tool() {
    if command -v "$1" >/dev/null 2>&1; then
        detail "$1 → $(command -v "$1")"
        return 0
    fi
    detail "$1 is missing — $2"
    return 1
}

step_prerequisites() {
    local ok=0
    [[ -x "$BIN" ]] || { note "no act-mcp binary; build one (cargo build) or pass --bin PATH"; return 1; }
    for tool in git curl jq python3; do
        check_tool "$tool" "this script needs it to drive and read the server" || ok=1
    done
    [[ -r "$report" ]] || { note "scripts/realdata_report.py is missing"; return 1; }
    detail "act-mcp → $BIN ($("$BIN" --version 2>&1 | head -n1))"

    if [[ $MODE == stub ]]; then
        detail "mode --stub: act is tests/e2e/testdata/bin/act and docker is the stub beside it"
        ACT=$repo_root/tests/e2e/testdata/bin/act
        PATH="$repo_root/tests/e2e/testdata/bin:$PATH"
        export PATH
    else
        ACT=${ACT:-$(command -v act || true)}
        if [[ -z "$ACT" || ! -x "$ACT" ]]; then
            note "no act on PATH (<https://github.com/nektos/act#installation>), and --act was not given."
            note "no container runtime either? \`scripts/smoke-real-act.sh --stub\` runs the same steps without either."
            return 1
        fi
    fi

    local act_version
    act_version=$("$ACT" --version 2>&1 | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -n1 || true)
    if [[ -z "$act_version" ]]; then
        detail "'$ACT' --version did not look like act: $("$ACT" --version 2>&1 | head -n1)"
        return 1
    fi
    ACT_VERSION=$act_version
    export ACT_VERSION ACT
    detail "act → $ACT (version $ACT_VERSION)"
    if version_lt "$ACT_VERSION" "$MIN_ACT_VERSION"; then
        detail "act $ACT_VERSION is below --min-act-version $MIN_ACT_VERSION, which is this server's floor"
        return 1
    fi

    # argv acceptance first, because it needs no daemon and is the fact most likely to be wrong:
    # every flag src/runner/argv.rs emits must exist in this act, or every run dies as a cobra
    # error that looks like a failing workflow.
    check_argv || return 1

    # The runtime, probed the way the server probes it (ADR-0001 §5.3): `docker info` through
    # the client on PATH. If this fails every run_workflow would answer docker_unreachable,
    # so say it here rather than twenty seconds and one image pull later.
    local runtime_version
    runtime_version=$(docker info --format '{{.ServerVersion}}' 2>/dev/null || true)
    if [[ -z "$runtime_version" ]]; then
        detail "nothing answered \`docker info --format '{{.ServerVersion}}'\` (DOCKER_HOST=${DOCKER_HOST:-unset})"
        note "start docker, or a podman socket at the docker path — or use --stub to exercise this script without one"
        return 1
    fi
    detail "container runtime → docker $runtime_version (the server's own probe)"
    if [[ $MODE != stub ]]; then
        detail "images present locally: $(docker images -q 2>/dev/null | sort -u | wc -l | tr -d ' ') — act pulls on first use"
        local rc_file
        for rc_file in "${XDG_CONFIG_HOME:-$HOME/.config}/act/actrc" "$HOME/.actrc"; do
            [[ -f "$rc_file" ]] && detail "act config $rc_file: $(tr '\n' ' ' <"$rc_file")"
        done
    fi

    local avail_mb
    avail_mb=$(df -Pk "$OUT" | awk 'NR==2 {print int($4/1024)}')
    detail "free disk at $OUT: ${avail_mb} MB (--need-mb is ${NEED_MB})"
    if (( avail_mb < NEED_MB )); then
        detail "not room for a container image; pass --need-mb $avail_mb only if you mean it"
        return 1
    fi
    return $ok
}

# argv acceptance, which needs no daemon: every flag src/runner/argv.rs emits must exist in
# this act, or every run dies as a cobra error that looks like a failing workflow.
check_argv() {
    if [[ $MODE == stub ]]; then
        detail "argv acceptance against the stub is tests/e2e scenario 12; the real check runs in real mode"
        return 0
    fi
    local help_text missing=() flag help_file="$OUT/help.txt"
    [[ -n $ACT ]] || {
        detail "no act to check flags against"
        return 1
    }
    [[ -s $help_file ]] || "$ACT" --help >"$help_file" 2>&1 || true
    help_text=$(<"$help_file")
    if [[ -z "$help_text" ]]; then
        detail "act --help produced nothing to read; cannot check flags against it"
        return 1
    fi
    # The names are the ones src/runner/argv.rs emits, minus the leading dashes. An unknown
    # flag is a cobra error that arrives looking like a failing workflow, and `--use-gitignore`
    # is the sharp one: it exists only from act v0.2.80, and argv.rs emits it whenever the
    # workspace carries no .actrc — which is the state this fixture is in.
    for flag in directory workflows job bind reuse rm pull rebuild use-gitignore json quiet \
        secret-file var-file env-file input-file env input var matrix eventpath dryrun defaultbranch; do
        grep -qE -- "(^|[[:space:]])--${flag}([[:space:]]|=)" <<<"$help_text" || missing+=("$flag")
    done
    if (( ${#missing[@]} > 0 )); then
        detail "act ${ACT_VERSION:-?} does not document: ${missing[*]} — every run would fail before a container starts"
        return 1
    fi
    detail "act ${ACT_VERSION:-?} documents every flag src/runner/argv.rs emits (--directory … --defaultbranch)"
}

step_fixture() {
    mkdir -p "$OUT/repo"
    cp -R "$fixture_dir/." "$OUT/repo/"
    (
        cd "$OUT/repo" || exit 1
        git init -q -b main .
        git config user.name act-mcp-smoke
        git config user.email act-mcp-smoke@example.invalid
        git config commit.gpgsign false
        git add -A
        git commit -qm "realdata smoke fixture"
    )
    detail "workspace $OUT/repo at $(git -C "$OUT/repo" rev-parse --short HEAD), from $fixture_dir"
    detail "workflows: $(cd "$OUT/repo/.github/workflows" && ls | tr '\n' ' ')"
    [[ -f "$OUT/repo/.github/workflows/coverage.yml" ]] || { detail "the fixture lost its workflows"; return 1; }
    local junk
    for junk in .actrc .env .vars .input; do
        [[ -e "$OUT/repo/$junk" ]] && { detail "fixture carries $junk: act would read it from the run's cwd (ADR-0003 F2/F3)"; return 1; }
    done
    return 0
}

# start_server LOG SCRATCH ISOLATION [ENV=VALUE…] — boot on port 0, set URL and SERV_PID.
start_server() {
    local log=$1 scratch=$2 isolation=$3; shift 3
    local envvars=("$@")
    mkdir -p "$scratch"
    # Nothing ambient may decide what the server does (tests/common's list), and act's own
    # config lookup starts in the process cwd, so the cwd is this run's directory (F2).
    ( cd "$OUT" \
        && exec env -u ACT_MCP_WORKSPACE -u ACT_MCP_BIND -u ACT_MCP_ACT_BINARY \
            -u ACT_MCP_EVIDENCE_DIR -u EVIDENCE_DIR -u ACT_MCP_ISOLATION \
            -u ACT_MCP_MINIMUM_ACT_VERSION -u ACT_MCP_LIST_TASKS_ACT_CROSS_CHECK \
            -u ACT_MCP_ALLOW_DIRTY_WORKTREE -u RUST_LOG \
            ${envvars[@]+"${envvars[@]}"} \
            "$BIN" serve \
            --workspace "$OUT/repo" \
            --act-binary "$ACT" \
            --run-scratch "$scratch" \
            --bind 127.0.0.1:0 \
            --isolation "$isolation" ) >"$log" 2>&1 &
    local pid=$!
    server_pids+=("$pid")
    local attempt=0 port=
    while (( attempt < 300 )); do
        port=$(grep -oE 'bind=127\.0\.0\.1:[0-9]+' "$log" 2>/dev/null | head -n1 | cut -d: -f2 || true)
        [[ -n "$port" ]] && break
        kill -0 "$pid" 2>/dev/null || break
        sleep 0.1
        attempt=$((attempt + 1))
    done
    if [[ -z "$port" ]]; then
        detail "the server exited or never bound; its log:"
        [[ -s $log ]] && sed 's/^/         | /' "$log" | tail -n 30
        return 1
    fi
    URL=http://127.0.0.1:$port/mcp
    SERV_PID=$pid
    SERV_LOG=$log
    detail "pid $pid at $URL (log $log)"
    detail "evidence root: $OUT/repo/.evidence"
}

stop_server() {
    local pid=${1:-${SERV_PID:-}}
    [[ -n "$pid" ]] || return 0
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
}

step_boot() {
    local env=()
    if [[ $MODE == stub ]]; then
        # The stub is steered by environment (the runner hands its own env to act). `all`
        # drops every payload fixture, so both runs have something to extract.
        env=(
            "STUB_PAYLOAD_DIR=$repo_root/tests/e2e/testdata/payload"
            "STUB_PAYLOAD=all"
            "STUB_BENCHMARK_MARKER=1"
            "STUB_LOG=$repo_root/tests/e2e/testdata/logs/benchmark-success.jsonl"
            "STUB_ARGV_FILE=$OUT/stub-argv.txt"
        )
    fi
    SERV_PID=
    start_server "$OUT/server.log" "$OUT/scratch" "$ISOLATION" ${env[@]+"${env[@]}"}
}

step_tools_list() {
    local rc=0
    mcp --tools-list >/dev/null || rc=$?
    [[ $rc -eq 0 ]] || return 1
    local tools
    tools=$(jq -r '.result.tools[].name' "$(last_response --tools-list)" | sort | tr '\n' ' ')
    detail "tools/list → $tools"
    local want
    for want in list_tasks run_workflow run_status cancel_run; do
        grep -qw "$want" <<<"$tools" || { detail "no $want in the advertised set"; return 1; }
    done
}

step_list_tasks() {
    local rc=0
    mcp list_tasks '{"include_steps":true}' >/dev/null || rc=$?
    if [[ $rc -ne 0 ]]; then
        detail "list_tasks answered an error: $(content "$(last_response list_tasks)")"
        return 1
    fi
    jq -c '.result.structuredContent' "$(last_response list_tasks)" >"$OUT/list_tasks.json"
    local workflows jobs hints
    workflows=$(jq -r '[.workflows[].name] | join(" ")' "$OUT/list_tasks.json")
    jobs=$(jq -r '[.workflows[].jobs[].id] | join(" ")' "$OUT/list_tasks.json")
    hints=$(jq -r '[.workflows[].jobs[] | "\(.id)=\(.category_hint // "?")"] | join(" ")' "$OUT/list_tasks.json")
    detail "workflows: $workflows"
    detail "jobs: $jobs"
    detail "category hints: $hints"
    local ok=0 want
    for want in coverage benchmark; do
        grep -qw "$want" <<<"$workflows" || { detail "workflow '$want' is not listed"; ok=1; }
    done
    # The needs-chain job has to be enumerable too, or `-j warmup` could not be asked for.
    grep -qw warmup <<<"$jobs" || { detail "job 'warmup' is not listed"; ok=1; }
    return $ok
}

# call_run LABEL WORKFLOW TASK CATEGORY — one timed run_workflow, and its measurements.
call_run() {
    local label=$1 workflow=$2 task=$3 category=$4
    # `commit` is a commit abbreviation (7–40 hex), not a ref: HEAD is resolved here, in the
    # workspace the server is about to gate against, so the run is pinned to the SHA the
    # evidence will be keyed by.
    local sha args
    sha=$(git -C "$OUT/repo" rev-parse HEAD)
    args=$(printf '{"workflow":"%s","task":"%s","category":"%s","commit":"%s"}' \
        "$workflow" "$task" "$category" "$sha")
    [[ -n "$TIMEOUT_SECS" ]] && args=${args%\}},\"timeout_secs\":$TIMEOUT_SECS\}
    local start end rc=0
    start=$(seconds_now)
    mcp run_workflow "$args" >/dev/null || rc=$?
    end=$(seconds_now)
    RUN_WALL[$label]=$((end - start))
    local file
    file=$(last_response run_workflow)
    if [[ $rc -ne 0 ]]; then
        detail "run_workflow(category=$category) answered an error after ${RUN_WALL[$label]}s: $(content "$file")"
        diagnose_run_error "$file"
        return 1
    fi
    RUN_VIEW[$label]=$(content "$file")
    RUN_ID[$label]=$(jq -r '.result.structuredContent.run_id // empty' "$file")
    local status duration
    status=$(jq -r '.result.structuredContent.status // "?"' "$file")
    duration=$(jq -r '.result.structuredContent.duration_ms // "?"' "$file")
    detail "$label: status=$status wall=${RUN_WALL[$label]}s act.duration_ms=$duration run_id=${RUN_ID[$label]:-?}"
    detail "evidence_files: $(jq -c '.result.structuredContent.evidence_files' "$file")"
    local warnings
    warnings=$(jq -c '.result.structuredContent.warnings // []' "$file")
    [[ "$warnings" == "[]" ]] || detail "warnings: $warnings"
    [[ "$status" == "success" ]] || { detail "status was $status, not success"; return 1; }
    [[ -n "${RUN_ID[$label]}" ]] || { detail "no run_id came back"; return 1; }
    local log="$OUT/repo/.evidence/runs/${RUN_ID[$label]}/act.jsonl"
    [[ -f "$log" ]] || { detail "no captured act log at $log"; return 1; }
    RUN_LOG[$label]=$log
    RUN_LOG_BYTES[$label]=$(file_size "$log")
    local records
    records=$(jq -r '.result.structuredContent.evidence_files[0] // empty' "$file")
    RUN_EVIDENCE_BYTES[$label]=0
    [[ -n "$records" && -f "$OUT/repo/$records" ]] && RUN_EVIDENCE_BYTES[$label]=$(file_size "$OUT/repo/$records")
}

# diagnose_run_error FILE — name the two failure shapes this script can explain better than
# the server can, because they are about the fixture and the gates rather than about act.
diagnose_run_error() {
    local file=$1
    local code reason
    code=$(jq -r '.result.structuredContent.error.code // ""' "$file")
    reason=$(jq -r '.result.structuredContent.error.detail.reason // ""' "$file")
    case "$code/$reason" in
        isolation_failed/dirty_worktree)
            note "this workspace is a fresh commit of tests/realdata/fixture, so any dirt is the server's own:"
            note "ADR-0002 §6.4 appends .evidence/ and .act-mcp.lock to the repo's .gitignore. If .gitignore is tracked and did not already list them, that append is a tracked modification and ADR-0003 G5 refuses every later run."
            note "the fixture commits those patterns for exactly that reason (see tests/realdata/fixture/.gitignore)."
            detail "git status --porcelain --untracked-files=no:"
            git -C "$OUT/repo" status --porcelain --untracked-files=no | sed 's/^/         | /'
            ;;
        */docker_unreachable|isolation_failed/docker_unreachable)
            note "the runtime stopped answering between the prerequisite check and now. Re-run --prereq-only to see it say so directly."
            ;;
    esac
}

step_run_coverage() { call_run coverage coverage coverage coverage; }

step_run_benchmark() {
    call_run benchmark benchmark benchmark benchmark || return 1
    if [[ $MODE == real ]]; then
        detail "the needs: chain ran: the fixture's benchmark job fails its first step unless act ran 'warmup' first"
    else
        detail "(a stub runs no chain — this is the assumption only a real run can settle)"
    fi
}

step_run_status() {
    local ok=0 label
    for label in coverage benchmark; do
        local id=${RUN_ID[$label]:-}
        if [[ -z "$id" ]]; then detail "$label produced no run id"; ok=1; continue; fi
        local rc=0
        mcp run_status "{\"run_id\":\"$id\"}" >/dev/null || rc=$?
        if [[ $rc -ne 0 ]]; then detail "run_status($id) answered an error"; ok=1; continue; fi
        local view status
        view=$(content "$(last_response run_status)")
        status=$(jq -r '.run.status // .status // "?"' <<<"$view")
        detail "run_status($id) → $status"
        [[ "$status" == "success" ]] || { detail "expected terminal success: $view"; ok=1; }
        local meta="$OUT/repo/.evidence/runs/$id/meta.json"
        if [[ ! -f "$meta" ]]; then detail "no meta.json at $meta (ADR-0002 §8 writes it last)"; ok=1; continue; fi
        detail "meta.json: status=$(jq -r .status "$meta") act.log=$(jq -r '.act.log // "?"' "$meta") warnings=$(jq -c '.warnings // []' "$meta")"
    done
    return $ok
}

# ADR-0003 F8 / V1: under --bind the container path IS the host path, so what the job wrote
# under the workspace is there on the host. The payload channel (ADR-0002 §3.1) stands on it,
# and no stub can test it.
step_payload_crossed_the_bind() {
    local ok=0 label
    for label in coverage benchmark; do
        local id=${RUN_ID[$label]:-}
        [[ -n "$id" ]] || { ok=1; continue; }
        local meta="$OUT/repo/.evidence/runs/$id/meta.json"
        [[ -f "$meta" ]] || { detail "$label has no meta.json"; ok=1; continue; }
        local records warnings
        records=$(jq -r '[.evidence[].records] | add // 0' "$meta")
        warnings=$(jq -c '[.warnings[] | if type == "object" then .code else . end]' "$meta")
        detail "$label: published records=$records warnings=$warnings"
        if (( records == 0 )); then
            detail "$label published nothing — the job's payload never reached the host (or was never written)"
            ok=1
        fi
        local incoming
        incoming=$(jq -r '.incoming.dir // empty' "$meta")
        [[ -n "$incoming" ]] && detail "$label payload dir was $incoming (host side of the bind mount)"
    done
    [[ $MODE == stub ]] && detail "(against the stub this proves the plumbing only; F8/V1 needs real act)"
    return $ok
}

step_log_captured() {
    local label=$1
    local log=${RUN_LOG[$label]:-}
    [[ -n "$log" && -f "$log" ]] || { detail "no log to read for $label"; return 1; }
    local total objects
    total=$(wc -l <"$log" | tr -d ' ')
    objects=$(grep -c '^{' "$log" || true)
    detail "$label: $log — $total line(s), $objects of them JSON objects"
    # ADR-0003 V2: does --quiet=false keep step output in the JSON stream? The fixture's own
    # marker is the probe, and the in-band marker channel depends on the answer.
    if ! grep -q 'ACTMCP_SMOKE_MARKER\|ACTMCP_BENCHMARK' "$log"; then
        detail "$label: none of the job's stdout reached the JSON log — V2 fails (--quiet=false did not keep it)"
        return 1
    fi
    if grep -q '"raw_output"' "$log"; then
        detail "$label: the job's stdout is in the log on raw_output lines (V2 holds)"
    else
        detail "$label: the marker is in the log but no line carried raw_output:true — it arrived some other way"
    fi
    return 0
}

step_shape() {
    local label=$1
    local log=${RUN_LOG[$label]:-}
    [[ -n "$log" && -f "$log" ]] || { detail "nothing to shape-check for $label"; return 1; }
    local args=(shape --capture "$log" --manifest "$GOLDEN_DIR/act-shape.json"
                --json-out "$OUT/shape-$label.json")
    [[ $STRICT_SHAPE -eq 1 ]] && args+=(--strict)
    local rc=0
    python3 "$report" "${args[@]}" | sed 's/^/       /' || rc=${PIPESTATUS[0]}
    if [[ $rc -eq 3 ]]; then
        if [[ $MODE == stub ]]; then
            note "the report above describes the STUB's log fixture (tests/e2e/testdata/logs), not act."
            note "It is still the same checker a real run uses; run without --stub to learn about act."
            return 0
        fi
        diverged=1
        note "SHAPE DIVERGENCE ($label). File the follow-up before touching the manifest:"
        detail "  bd create 'real act --json log diverges from tests/realdata/golden/act-shape.json' \\"
        detail "    -d 'captured log: $(basename "$log"); the shape report is in this smoke run's output' -p 2"
        return 3
    elif [[ $rc -ne 0 ]]; then
        detail "the shape checker could not run (exit $rc)"
        return 1
    fi
    return 0
}

step_capture_golden() {
    [[ $CAPTURE_GOLDEN -eq 1 ]] || { detail "not asked for (--capture-golden)"; return 0; }
    local log=${RUN_LOG[coverage]:-}
    [[ -n "$log" && -f "$log" ]] || { detail "no coverage log to capture"; return 1; }
    mkdir -p "$GOLDEN_DIR"
    cp "$log" "$GOLDEN_DIR/act.jsonl"
    SMOKE_OUT=$OUT python3 - "$GOLDEN_DIR/act.jsonl.provenance.json" <<'PY'
import datetime, json, os, sys
payload = {
    "captured_at": datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds"),
    "captured_by": "scripts/smoke-real-act.sh --capture-golden",
    "act_version": os.environ.get("ACT_VERSION", "unknown"),
    "act_binary": os.environ.get("ACT_BIN", "unknown"),
    "workspace": os.environ.get("SMOKE_OUT", "unknown"),
    "host": os.uname().nodename,
    "read_me_first": "tests/realdata/golden/README.md — a capture is a candidate golden, review it before committing",
}
with open(sys.argv[1], "w", encoding="utf-8") as handle:
    json.dump(payload, handle, indent=2)
    handle.write("\n")
PY
    detail "wrote $GOLDEN_DIR/act.jsonl ($(file_size "$GOLDEN_DIR/act.jsonl") bytes) + act.jsonl.provenance.json"
    detail "review both, then commit them. The policy is tests/realdata/golden/README.md."
}

step_validate_evidence() {
    local out rc=0
    out=$("$BIN" validate-evidence --stats "$OUT/repo/.evidence" 2>&1) || rc=$?
    sed 's/^/         | /' <<<"$out" | tail -n 25
    if [[ $rc -ne 0 ]]; then
        detail "validate-evidence exited $rc: the tree this run wrote does not satisfy ADR-0002"
        return 1
    fi
    detail "validate-evidence is clean over $OUT/repo/.evidence"
}

step_record_values() {
    local ok=0 label
    if [[ $MODE == stub ]]; then
        for label in coverage benchmark; do
            local id=${RUN_ID[$label]:-}
            [[ -n "$id" ]] || continue
            local file
            file=$(find "$OUT/repo/.evidence/$label" -name "$id.jsonl" 2>/dev/null | head -n1)
            if [[ -z "$file" || ! -s "$file" ]]; then detail "$label wrote no records"; ok=1; continue; fi
            detail "$label: $(wc -l <"$file" | tr -d ' ') record(s) in $(basename "$file") (values belong to the stub payload, not this fixture)"
        done
        return $ok
    fi

    local cov_id=${RUN_ID[coverage]:-} bench_id=${RUN_ID[benchmark]:-}
    local cov_file bench_file
    cov_file=$(find "$OUT/repo/.evidence/coverage" -name "$cov_id.jsonl" 2>/dev/null | head -n1)
    bench_file=$(find "$OUT/repo/.evidence/benchmark" -name "$bench_id.jsonl" 2>/dev/null | head -n1)
    if [[ -z "$cov_file" || -z "$bench_file" ]]; then detail "expected one coverage and one benchmark file"; return 1; fi

    # coverage: the fixture's tracefile has two SF blocks → one total-scope record + two.
    local totals files found hit bfound bhit
    totals=$(jq -s '[.[] | select(.scope=="total")] | length' "$cov_file")
    files=$(jq -s '[.[] | select(.scope=="file")] | length' "$cov_file")
    detail "coverage records: $totals total-scope, $files file-scope (want 1 and 2)"
    [[ "$totals" == "1" && "$files" == "2" ]] || ok=1
    found=$(jq -s '[.[] | select(.scope=="total")][0].lines_found' "$cov_file")
    hit=$(jq -s '[.[] | select(.scope=="total")][0].lines_hit' "$cov_file")
    bfound=$(jq -s '[.[] | select(.scope=="total")][0].branches_found' "$cov_file")
    bhit=$(jq -s '[.[] | select(.scope=="total")][0].branches_hit' "$cov_file")
    detail "coverage totals: lines $hit/$found, branches $bhit/$bfound (the fixture's arithmetic says 4/6 and 1/2)"
    [[ "$found" == "6" && "$hit" == "4" && "$bfound" == "2" && "$bhit" == "1" ]] || ok=1

    # benchmark: three producers, and the fixture's own values.
    local names source_list latency
    names=$(jq -sr '[.[].name] | join(" ")' "$bench_file")
    source_list=$(jq -sr '[.[].source] | unique | join(",")' "$bench_file")
    detail "benchmark: sources=[$source_list] names=[$names]"
    local want
    for want in smoke_throughput smoke_startup_ms smoke_latency_ms smoke_queue_depth; do
        grep -qw "$want" <<<"$names" || { detail "metric '$want' is missing — its producer did not fire"; ok=1; }
    done
    grep -q job_file <<<"$source_list" || { detail "no job_file record: benchmark.json never crossed the bind mount"; ok=1; }
    grep -q in_band_stdout <<<"$source_list" || { detail "no in_band_stdout record: the in-band marker channel is dead (V2)"; ok=1; }
    if [[ -n "${ACT_VERSION:-}" ]] && ! version_lt "$ACT_VERSION" 0.2.76; then
        grep -q act_log <<<"$source_list" || {
            detail "act $ACT_VERSION emits executionTime, yet no act_log record appeared — step timings went unread"
            ok=1
        }
    else
        detail "act ${ACT_VERSION:-?} predates v0.2.76: it emits no step timings, so no act_log records are expected"
    fi
    latency=$(jq -sr '[.[] | select(.name=="smoke_latency_ms")][0].value // "none"' "$bench_file")
    detail "smoke_latency_ms = $latency (the fixture printed 12.5)"
    [[ "$latency" == "12.5" ]] || ok=1
    return $ok
}

step_expected_records() {
    local args=(expected --produced "$OUT/repo/.evidence" --expected "$expected_dir")
    if [[ $WRITE_EXPECTED -eq 1 ]]; then
        if [[ $MODE == stub ]]; then
            detail "--write-expected against the stub would commit stub-derived goldens; refusing"
            return 0
        fi
        args+=(--write)
    fi
    local rc=0
    python3 "$report" "${args[@]}" | sed 's/^/       /' || rc=${PIPESTATUS[0]}
    return $rc
}

# ADR-0003 §7.1 rests on a fact about act (F2): act reads `.actrc` from its *process cwd*, and
# the runner starts act in the run's scratch directory — never in the workspace. So a
# repository cannot configure our run by dropping an `.actrc` next to its workflows. That is
# worth proving rather than trusting, and it is provable safely: pin `ubuntu-latest` in a
# workspace `.actrc` to an image that cannot be pulled. If act read it the run would fail on
# the pull; if the defence holds the run is untroubled. Cheap in a warm workspace, and it is the
# §12.3 question the ADR asks.
step_workspace_actrc_is_inert() {
    [[ $MODE == real ]] || { detail "F2/F3 are real-act facts; --stub skips the trap"; return 0; }
    printf '%s\n' '-P ubuntu-latest=act-mcp-smoke.invalid/does-not-exist:latest' >"$OUT/repo/.actrc"
    detail "planted $OUT/repo/.actrc pinning ubuntu-latest to an unpullable image"
    local rc=0
    call_run actrc coverage coverage coverage || rc=$?
    if [[ $rc -ne 0 ]]; then
        note "act applied the workspace .actrc: ADR-0003 §7.1's defence against a repository configuring our run has stopped holding (F2/F3)."
        return 1
    fi
    detail "the run ignored it — act's cwd is the run's scratch dir, so a workspace .actrc is inert (F2, ADR-0003 §7.1)"
    rm -f "$OUT/repo/.actrc"
    return 0
}

# ADR-0003 V1: a worktree under the runtime directory, bind-mounted at its own path.
step_worktree_isolation() {
    [[ $MODE == real ]] || { detail "V1 is a real-act claim; --stub skips it"; return 0; }
    local scratch=${XDG_RUNTIME_DIR:-/tmp}/act-mcp-smoke-worktree-$$
    detail "second server: --isolation worktree --run-scratch $scratch"
    local pid_before=${#server_pids[@]}
    if ! start_server "$OUT/server-worktree.log" "$scratch" worktree; then
        return 1
    fi
    local wt_url=$URL wt_pid=${server_pids[pid_before]}
    local args
    args=$(printf '{"workflow":"coverage","task":"coverage","category":"coverage","commit":"%s"}' \
        "$(git -C "$OUT/repo" rev-parse HEAD)")
    [[ -n "$TIMEOUT_SECS" ]] && args=${args%\}},\"timeout_secs\":$TIMEOUT_SECS\}
    local rc=0 out
    out=$(mcp run_workflow "$args") || rc=$?
    if [[ $rc -ne 0 ]]; then
        detail "the worktree run answered an error: $(content "$(last_response run_workflow)")"
        stop_server "$wt_pid"
        return 1
    fi
    local view records
    view=$(content "$(last_response run_workflow)")
    records=$(jq -r '.evidence_record_count // 0' <<<"$view")
    detail "worktree run $(jq -r .run_id <<<"$view"): status=$(jq -r .status <<<"$view") records=$records warnings=$(jq -c .warnings <<<"$view")"
    stop_server "$wt_pid"
    if [[ "$records" == "0" ]]; then
        detail "it published nothing: the daemon could not bind a worktree under $scratch — ADR-0003 V1 does not hold here"
        return 1
    fi
    detail "V1 holds: a worktree under $scratch was bind-mounted at the same path and the payload came back"
    rm -rf "$scratch"
}

# ADR-0003 V3 and the container filter ADR-0004 §11.2 still owes the runbook.
step_cancel_leaves_nothing() {
    [[ $WITH_CANCEL -eq 1 ]] || { detail "not asked for (--with-cancel)"; return 0; }
    [[ $MODE == real ]] || { detail "cancellation needs real act"; return 0; }
    local rc=0
    local cancel_args
    cancel_args=$(printf '{"workflow":"coverage","task":"coverage","category":"coverage","commit":"%s","wait":false}' \
        "$(git -C "$OUT/repo" rev-parse HEAD)")
    mcp run_workflow "$cancel_args" >/dev/null || rc=$?
    if [[ $rc -ne 0 ]]; then
        detail "the wait:false call failed: $(content "$(last_response run_workflow)")"
        return 1
    fi
    local id
    id=$(jq -r '.result.structuredContent.run_id // empty' "$(last_response run_workflow)")
    detail "started $id with wait:false; cancelling after 2s so act is mid-pull or mid-step"
    sleep 2
    mcp cancel_run "{\"run_id\":\"$id\"}" >/dev/null || { detail "cancel_run answered an error"; return 1; }
    local status=? attempt=0
    while (( attempt < 120 )); do
        mcp run_status "{\"run_id\":\"$id\"}" >/dev/null || true
        status=$(content "$(last_response run_status)" | jq -r '.run.status // .status // "?"')
        case "$status" in success | failed | cancelled | timed_out | error) break ;; esac
        sleep 0.5
        attempt=$((attempt + 1))
    done
    detail "run $id reached $status"
    [[ "$status" == "cancelled" ]] || { detail "expected cancelled"; return 1; }
    # act names its containers createContainerName("act", …), so this is the filter that
    # finds them — the one docs/security.md's orphan recipe needs (ADR-0004 §11.2).
    local leftovers
    leftovers=$(docker ps -a --filter name=act --format '{{.ID}} {{.Names}} {{.Status}}' 2>/dev/null | tr '\n' ';')
    detail "docker ps -a --filter name=act → ${leftovers:-none}"
    [[ -n "$leftovers" ]] && detail "act containers exist after a cancelled run: read that against --rm's contract (ADR-0003 F4/V3) before calling it a leak"
    return 0
}

# --------------------------------------------------------------------------- summary
summary() {
    echo
    printf '%ssummary%s\n' "$c_bold" "$c_off"
    local i
    for i in "${!step_label[@]}"; do
        case "${step_status[$i]}" in
            pass) printf '  %s✓%s %s\n' "$c_green" "$c_off" "${step_label[$i]}" ;;
            skip) printf '  %s·%s %s (skipped: %s)\n' "$c_yellow" "$c_off" "${step_label[$i]}" "${step_note[$i]:-}" ;;
            *) printf '  %s✗%s %s\n' "$c_red" "$c_off" "${step_label[$i]}" ;;
        esac
    done

    if (( ${#RUN_ID[@]} > 0 )); then
        echo
        printf '%sper-run cost — what ADR-0004 §4.2 assumed, measured%s\n' "$c_bold" "$c_off"
        printf '  %-10s %8s %11s %12s %13s %14s %s\n' \
            category wall_s act_ms act_log_B evidence_B default_s verdict
        local label default wall act_ms verdict
        for label in coverage benchmark; do
            [[ -n "${RUN_ID[$label]:-}" ]] || continue
            default=$([[ $label == coverage ]] && echo 900 || echo 1800)
            wall=${RUN_WALL[$label]}
            act_ms=$(jq -r '.duration_ms // "?"' <<<"${RUN_VIEW[$label]}")
            verdict=ok
            (( wall > default )) && verdict="EXCEEDED the shipped default"
            printf '  %-10s %8s %11s %12s %13s %14s %s\n' \
                "$label" "$wall" "$act_ms" "${RUN_LOG_BYTES[$label]:-?}" \
                "${RUN_EVIDENCE_BYTES[$label]:-?}" "$default" "$verdict"
        done
        echo
        printf '  The coverage default is 900 s and the benchmark default 1800 s (ADR-0004 §4.2). Most\n  of what you just measured on a cold machine is the image pull, not the job: the second run\n  is the warm number. A fixture this trivial coming anywhere near a default is a finding.\n'
    fi

    if [[ $diverged -eq 1 ]]; then
        echo
        printf '%sact --json shape: DIVERGED%s — the DIVERGENCE block above is the finding, and it\nwants a follow-up ticket before anything in src/evidence changes.\n' "$c_yellow" "$c_off"
    fi

    if [[ $KEEP -eq 1 && -n "$OUT" ]]; then
        echo
        printf '  --keep: everything is still under %s\n     workspace     %s/repo (its .evidence tree is the evidence)\n     server log    %s\n     responses     %s/responses\n' \
            "$OUT" "$OUT" "${SERV_LOG:-none}" "$OUT"
    fi

    echo
    if [[ $failed -ne 0 ]]; then
        printf '%sFAILED%s\n' "$c_red" "$c_off"
        return 1
    fi
    printf '%sPASS%s\n' "$c_green" "$c_off"
}

# --------------------------------------------------------------------------- main
main() {
    OUT=${OUT:-$(mktemp -d -t act-mcp-smoke.XXXXXX)}
    mkdir -p "$OUT"
    OUT=$(cd "$OUT" && pwd)
    say "${c_bold}act-mcp real-act smoke${c_off} — mode $MODE, out $OUT"

    run_step "prerequisites" step_prerequisites
    if [[ $LAST_STEP == fail ]]; then
        echo
        printf 'The prerequisites are half the deliverable: this script refuses to guess about act.\nNo act and no container runtime on this machine? `scripts/smoke-real-act.sh --stub` runs\nthe identical steps against the checked-in stubs (docs/testing.md §7).\n'
        summary
        return 1
    fi
    if [[ $MODE == prereq ]]; then
        summary
        return 0
    fi

    run_step "fixture workspace" step_fixture
    run_step "server boot" step_boot
    if [[ $LAST_STEP == fail ]]; then summary; return 1; fi

    run_step "tools/list" step_tools_list
    run_step "list_tasks" step_list_tasks
    run_step "run_workflow(coverage)" step_run_coverage
    run_step "run_workflow(benchmark)" step_run_benchmark
    run_step "run_status" step_run_status
    run_step "payload crossed the bind mount (F8/V1)" step_payload_crossed_the_bind
    run_step "act log captured (coverage)" step_log_captured coverage
    run_step "act --json shape (coverage)" step_shape coverage
    run_step "act log captured (benchmark)" step_log_captured benchmark
    run_step "act --json shape (benchmark)" step_shape benchmark
    run_step "capture golden" step_capture_golden
    stop_server
    run_step "validate-evidence" step_validate_evidence
    run_step "record values" step_record_values
    run_step "records vs tests/realdata/expected" step_expected_records
    run_step "workspace .actrc is inert (F2/F3)" step_workspace_actrc_is_inert
    if [[ $SKIP_WORKTREE -eq 1 ]]; then
        skip_step "worktree isolation (V1)" "--skip-worktree"
    else
        run_step "worktree isolation (V1)" step_worktree_isolation
    fi
    run_step "cancel leaves nothing (V3)" step_cancel_leaves_nothing

    summary
}

main "$@"
