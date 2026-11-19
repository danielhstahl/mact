#!/usr/bin/env bash
#
# scripts/bench.sh — the benchmark job's body: measure the numbers this repository is about, and
# write them where the contract says a benchmark payload goes.
#
# What it measures is what the whole project exists to make fast, and nothing else: cold first-screen
# latency (the ADR-0006 §9.1 budget), warm report latency, and what a full index costs. Every metric
# is labelled with the things that would make it move — OS, rustc, profile, fixture size — because
# ADR-0002 §3.3 says an unlabelled number is a regression hunt waiting to happen.
#
# The payload is `benchmark.json` with a `metrics` array (§3.3's "object with a `metrics` array"
# shape, rather than one line per metric, because a metric here is one measurement and its labels, not
# an evidence stream), written to `$ACTMCP_EVIDENCE_OUT/benchmark.json` inside a run and beside the
# repository by hand.
#
# Usage:
#   scripts/bench.sh                       # build release, measure, write benchmark.json
#   scripts/bench.sh --quick               # one iteration each; the pre-merge budget check
#   scripts/bench.sh --iterations 9        # more samples, median reported
#   scripts/bench.sh --no-build            # bench target/release/fathom as it stands
#   scripts/bench.sh --check               # payload self-test: no build, no measurement, no network
#   scripts/bench.sh --validate FILE       # check one payload against §3.3 and report
#   scripts/bench.sh --print-out-path      # where the payload would go, then exit 0
#
# Exit status: 0 measured and wrote a payload that validates; 1 a measurement failed or the payload
# would not be accepted by a runner's extractor; 2 bad usage or a missing tool (cargo, git, python3).
#
# --budget SECONDS_MS is the one gate this script can enforce — the cold first-screen budget the ADR
# sets — and the workflow passes it explicitly rather than inheriting a default that could drift.

set -o errexit -o nounset -o pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)

ITERATIONS=5
QUICK=0
NO_BUILD=0
SELF_CHECK=0
PRINT_PATH=0
VALIDATE=
OUTPUT=
BINARY=
FIXTURE_COMMITS=40
BUDGET=
KEEP=0

usage() {
    cat <<'USAGE'
Usage: scripts/bench.sh [OPTIONS]

Measures fathom's timings and publishes them as an ADR-0002 §3.3 benchmark payload.

  -i, --iterations N     timed repetitions per metric; the median is reported (default 5)
      --quick            one iteration per metric: the shape of the answer, not its precision
      --no-build         use target/release/fathom as it stands instead of rebuilding it
      --binary PATH      measure this binary (implies --no-build)
      --commits N        fixture size: commits in the throwaway repository it indexes (default 40)
      --budget MS        fail if cold first-screen latency exceeds MS (the ADR-0006 §9.1 budget is
                         1000; the workflow names it, so the number lives in one place)
  -o, --output PATH      where benchmark.json goes (default: $ACTMCP_EVIDENCE_OUT/benchmark.json,
                         else <repo>/benchmark.json)
      --validate FILE    check a payload against §3.3 (name/value/unit/labels/size) and exit
  -p, --print-out-path   print the resolved output path and exit 0
      --keep             leave the fixture behind and print its path
      --check            self-test: builds nothing and measures nothing, checks the payload rules
  -h, --help             this text

Exit: 0 ok; 1 a measurement failed or the payload would be refused; 2 usage or missing tool.
USAGE
}

die() {
    printf 'bench: %s\n' "$*" >&2
    exit 1
}

need() {
    command -v "$1" >/dev/null 2>&1 || {
        printf 'bench: %s is required\n' "$1" >&2
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
        -p | --print-out-path)
            PRINT_PATH=1
            shift
            ;;
        --quick)
            QUICK=1
            shift
            ;;
        --no-build)
            NO_BUILD=1
            shift
            ;;
        --binary)
            [[ $# -ge 2 ]] || die "$1 needs a value"
            BINARY=$2
            NO_BUILD=1
            shift 2
            ;;
        --iterations | -i)
            [[ $# -ge 2 ]] || die "$1 needs a value"
            ITERATIONS=$2
            shift 2
            ;;
        --commits)
            [[ $# -ge 2 ]] || die "$1 needs a value"
            FIXTURE_COMMITS=$2
            shift 2
            ;;
        --budget)
            [[ $# -ge 2 ]] || die "$1 needs a value"
            BUDGET=$2
            shift 2
            ;;
        -o | --output)
            [[ $# -ge 2 ]] || die "$1 needs a value"
            OUTPUT=$2
            shift 2
            ;;
        --validate)
            [[ $# -ge 2 ]] || die "$1 needs a value"
            VALIDATE=$2
            shift 2
            ;;
        --keep)
            KEEP=1
            shift
            ;;
        *)
            printf 'bench: unknown argument: %s\n' "$1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

((QUICK)) && ITERATIONS=1

repo_root() {
    local root
    root=$(git rev-parse --show-toplevel 2>/dev/null || true)
    [[ -n $root ]] || root=$(cd -- "$script_dir/.." && pwd)
    printf '%s\n' "$root"
}

out_path() {
    if [[ -n $OUTPUT ]]; then
        printf '%s\n' "$OUTPUT"
    elif [[ -n ${ACTMCP_EVIDENCE_OUT:-} ]]; then
        printf '%s/benchmark.json\n' "$ACTMCP_EVIDENCE_OUT"
    else
        printf '%s/benchmark.json\n' "$(repo_root)"
    fi
}

# now_ms — wall clock in milliseconds, from python3 rather than `date +%s%N`: GNU date has it, BSD
# date prints a literal `N`, and this script is meant to run on both.
now_ms() {
    python3 -c 'import time; print(int(time.monotonic() * 1000))'
}

# median OF FILE — the middle of the samples in FILE, which is what a benchmark reports rather than a
# mean: one scheduler hiccup on the fifth run should not move the published number.
median() {
    sort -n "$1" | awk '{ a[NR] = $1 } END {
        if (NR == 0) exit 1
        if (NR % 2) print a[(NR + 1) / 2]
        else printf "%.1f\n", (a[NR / 2] + a[NR / 2 + 1]) / 2
    }'
}

# ------------------------------------------------------------------ the payload rules

# validate_payload FILE — ADR-0002 §3.3, checked before publication rather than discovered when a
# runner's extractor answers `invalid_json`. The rules are §3.3's, not a subset: two required fields,
# a finite number, the label bounds, and the 2000-byte budget per record.
validate_payload() {
    local file=$1
    [[ -s $file ]] || {
        printf 'bench: %s is empty or missing\n' "$file" >&2
        return 1
    }
    PAYLOAD=$file python3 - <<'PY'
import json
import math
import os
import re
import sys

NAME = re.compile(r"^[A-Za-z0-9](?:[A-Za-z0-9._-]{0,62}[A-Za-z0-9])?$")
path = os.environ["PAYLOAD"]
try:
    with open(path, encoding="utf-8") as handle:
        raw = handle.read()
except OSError as exc:
    print("bench: %s" % exc, file=sys.stderr)
    raise SystemExit(1)
try:
    body = json.loads(raw)
except json.JSONDecodeError as exc:
    print("bench: %s is not JSON (%s)" % (path, exc), file=sys.stderr)
    raise SystemExit(1)

# §3.3 accepts an object with a `metrics` array, or a bare top-level array.
metrics = body.get("metrics") if isinstance(body, dict) else body
if not isinstance(metrics, list):
    print(
        "bench: %s must be an object with a `metrics` array, or a bare array (§3.3)" % path,
        file=sys.stderr,
    )
    raise SystemExit(1)
if not 1 <= len(metrics) <= 64:
    print(
        "bench: %s holds %d metrics; the cap is 64 and an empty payload is dropped by the runner"
        % (path, len(metrics)),
        file=sys.stderr,
    )
    raise SystemExit(1)

problems = []
for index, metric in enumerate(metrics):
    where = "metrics[%d]" % index
    if not isinstance(metric, dict):
        problems.append("%s is not an object" % where)
        continue
    name = metric.get("name")
    if not isinstance(name, str) or not NAME.match(name):
        problems.append("%s `name` %r is not 1-64 of [A-Za-z0-9._-], not starting/ending on a dash" % (where, name))
    value = metric.get("value")
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(float(value)):
        problems.append("%s `value` %r is not a finite number (null means failed, not zero)" % (where, value))
    unit = metric.get("unit")
    if unit is not None and (not isinstance(unit, str) or not 1 <= len(unit) <= 16):
        problems.append("%s `unit` %r must be 1-16 characters" % (where, unit))
    labels = metric.get("labels", {})
    if not isinstance(labels, dict) or len(labels) > 8:
        problems.append("%s `labels` must be an object of at most 8 pairs" % where)
    else:
        for key, item in labels.items():
            if not isinstance(key, str) or not 1 <= len(key) <= 32:
                problems.append("%s label key %r must be 1-32 characters" % (where, key))
            if not isinstance(item, str) or not 1 <= len(item) <= 32:
                problems.append("%s label %r value %r must be a 1-32 character string" % (where, key, item))
    extra = metric.get("extra")
    if extra is not None and not isinstance(extra, dict):
        problems.append("%s `extra` must be an object" % where)
    one = json.dumps(metric, separators=(",", ":"))
    if len(one.encode("utf-8")) > 2000:
        problems.append("%s serializes to %d bytes; the record budget is 2000" % (where, len(one.encode("utf-8"))))

if problems:
    for problem in problems[:10]:
        print("bench: %s" % problem, file=sys.stderr)
    raise SystemExit(1)
print("bench: %s: %d metric(s), all conforming to ADR-0002 §3.3" % (path, len(metrics)))
PY
}

# ------------------------------------------------------------------ the measurements

# make_fixture N — a throwaway repository with N commits, in the shape a real one has: a couple of
# touched files per commit, so a commit is a diff to read and not an empty event. The store is left
# absent; `cmd index` builds it, which is itself one of the measurements.
make_fixture() {
    local commits=$1 dir=$2
    git init -q "$dir" >/dev/null
    git -C "$dir" -c user.name=bench -c user.email=bench@example.com \
        -c commit.gpgsign=false config user.name bench
    local i
    for ((i = 0; i < commits; i++)); do
        mkdir -p "$dir/src"
        printf 'export const item%d = %d;\n' "$i" "$i" >"$dir/src/item$i.js"
        printf 'item %d\n' "$i" >>"$dir/README.md"
        git -C "$dir" add -A >/dev/null
        git -C "$dir" -c user.name=bench -c user.email=bench@example.com \
            -c commit.gpgsign=false commit -q -m "bench: commit $i" >/dev/null
    done
}

# time_it OUT_FILE COMMAND… — one timed run; the elapsed milliseconds are appended to OUT_FILE and the
# command's own stdout and stderr go to the log, so a failing measurement is visible.
time_it() {
    local file=$1
    shift
    local start end
    start=$(now_ms)
    "$@"
    end=$(now_ms)
    printf '%s\n' "$((end - start))" >>"$file"
}

# run_metric NAME FILE UNIT — median of a file of samples into the metrics list.
METRICS=()
# metric_json NAME VALUE UNIT — one metric as one JSON object, labelled with every fact about this run
# that would make the number move. ADR-0002 §3.3 asks for exactly that ("label anything else that changes
# what the number means") and caps it at eight pairs, which is why the labels are the four that matter and
# not every environment variable in the container.
metric_json() {
    METRIC_NAME=$1 METRIC_VALUE=$2 METRIC_UNIT=$3         OS_LABEL="$OS_LABEL" RUST_LABEL="$RUST_LABEL" PROFILE_LABEL="$PROFILE_LABEL"         FIXTURE_LABEL="$FIXTURE_LABEL" ITERATIONS_LABEL="$ITERATIONS" python3 -c '
import json
import os
print(json.dumps({
    "name": os.environ["METRIC_NAME"],
    "value": float(os.environ["METRIC_VALUE"]),
    "unit": os.environ["METRIC_UNIT"],
    "labels": {
        "os": os.environ["OS_LABEL"],
        "rust": os.environ["RUST_LABEL"],
        "profile": os.environ["PROFILE_LABEL"],
        "fixture": os.environ["FIXTURE_LABEL"],
        "iterations": os.environ["ITERATIONS_LABEL"],
    },
}))
'
}

# run_metric NAME SAMPLE_FILE UNIT — the median of a file of millisecond samples, published as a metric.
run_metric() {
    local name=$1 file=$2 unit=$3
    local value
    value=$(median "$file") || die "no samples for $name"
    METRICS+=("$(metric_json "$name" "$value" "$unit")")
    printf 'bench: %-36s %10s %s
' "$name" "$value" "$unit"
}

# run_derived_metric NAME VALUE UNIT — a number computed from the samples rather than one of them. It is
# labelled the same way, because a derived number inherits the honesty of the numbers it came from.
run_derived_metric() {
    METRICS+=("$(metric_json "$1" "$2" "$3")")
    printf 'bench: %-36s %10s %s
' "$1" "$2" "$3"
}

write_payload() {
    local file=$1
    mkdir -p "$(dirname -- "$file")"
    # A file rather than a pipe, and an explicit check afterwards: a payload that could not be written must
    # not be reported as a measurement, and the validation below is what turns "the file exists" into
    # "a runner would keep it".
    OUT=$file
    export OUT
    METRICS_JSON=$(printf '%s\n' "${METRICS[@]}" | python3 -c '
import json
import sys
print(json.dumps([json.loads(line) for line in sys.stdin if line.strip()], indent=2))
')
    export METRICS_JSON
    python3 -c '
import json
import os
with open(os.environ["OUT"], "w", encoding="utf-8") as handle:
    json.dump({"metrics": json.loads(os.environ["METRICS_JSON"])}, handle, indent=2)
    handle.write("\n")
'
    unset METRICS_JSON
    validate_payload "$file" || die "the payload it just wrote would be refused by a runner"
}

# ------------------------------------------------------------------ self-test

self_check() {
    need python3
    local dir
    dir=$(mktemp -d -t bench-check.XXXXXX)
    # shellcheck disable=SC2064
    trap "rm -rf '$dir'" EXIT
    local failures=0

    # 1. a payload that conforms, with the same shape this script writes.
    cat >"$dir/good.json" <<'JSON'
{
  "metrics": [
    {
      "name": "fathom.report.cold_ms",
      "value": 182.4,
      "unit": "ms",
      "labels": { "os": "linux-x86_64", "rust": "1.96.0", "profile": "release", "fixture": "commits=40" }
    },
    { "name": "fathom.index.full_seconds", "value": 0.42, "unit": "s" }
  ]
}
JSON
    if ! validate_payload "$dir/good.json" >/dev/null; then
        printf 'FAIL a conforming payload was refused\n'
        failures=$((failures + 1))
    fi

    # 2. each refusal §3.3 defines. A benchmark job that publishes a payload the runner drops would
    # be a green job that produces nothing, which is the failure this whole file exists to prevent.
    bad() {
        local what=$1 json=$2
        printf '%s\n' "$json" >"$dir/bad.json"
        if validate_payload "$dir/bad.json" >/dev/null 2>&1; then
            printf 'FAIL %s was accepted\n' "$what"
            failures=$((failures + 1))
        else
            printf 'ok   refused: %s\n' "$what"
        fi
    }
    bad "a metric with no value" '{"metrics":[{"name":"x"}]}'
    bad "a null value" '{"metrics":[{"name":"x","value":null}]}'
    bad "a NaN value" '{"metrics":[{"name":"x","value":[1,2]}]}'
    bad "an empty metrics array" '{"metrics":[]}'
    bad "a name that is not a metric name" '{"metrics":[{"name":"-bad-","value":1}]}'
    bad "a unit over sixteen characters" "{\"metrics\":[{\"name\":\"x\",\"value\":1,\"unit\":\"$(printf 'u%.0s' {1..17})\"}]}"
    bad "nine labels" '{"metrics":[{"name":"x","value":1,"labels":{"a":"1","b":"2","c":"3","d":"4","e":"5","f":"6","g":"7","h":"8","i":"9"}}]}'
    bad "a non-string label value" '{"metrics":[{"name":"x","value":1,"labels":{"a":5}}]}'
    bad "a 65-character name" "{\"metrics\":[{\"name\":\"$(printf 'n%.0s' {1..65})\",\"value\":1}]}"
    bad "a record over the 2000-byte budget" "{\"metrics\":[{\"name\":\"x\",\"value\":1,\"extra\":{\"blob\":\"$(printf 'e%.0s' {1..2100})\"}}]}"
    bad "a top-level object with no metrics array" '{"rows":[]}'
    bad "not JSON at all" '{'

    # 3. the bare-array shape §3.3 also accepts.
    printf '[{"name":"x","value":1}]\n' >"$dir/array.json"
    validate_payload "$dir/array.json" >/dev/null || {
        printf 'FAIL the bare-array shape was refused\n'
        failures=$((failures + 1))
    }

    # 4. the median, including the even-count case that must average the middle pair.
    printf '3\n1\n2\n' >"$dir/odd"
    printf '10\n20\n30\n40\n' >"$dir/even"
    [[ $(median "$dir/odd") == 2 ]] || {
        printf 'FAIL median of 3,1,2 should be 2\n'
        failures=$((failures + 1))
    }
    [[ $(median "$dir/even") == 25.0 ]] || {
        printf 'FAIL median of 10,20,30,40 should be 25.0\n'
        failures=$((failures + 1))
    }
    printf 'ok   median, including the even-count middle pair\n'

    if ((failures)); then
        printf 'bench --check: %s check(s) failed\n' "$failures" >&2
        return 1
    fi
    printf 'bench --check: §3.3 conformance covers every bound the contract states, in both directions\n'
}

# ------------------------------------------------------------------ main

if ((SELF_CHECK)); then
    self_check
    exit $?
fi

if [[ -n $VALIDATE ]]; then
    need python3
    validate_payload "$VALIDATE"
    exit $?
fi

if ((PRINT_PATH)); then
    out_path
    exit 0
fi

need python3
need git
need awk

ROOT=$(repo_root)
if [[ -z $BINARY ]]; then
    BINARY=$ROOT/target/release/fathom
fi

if ((!NO_BUILD)); then
    printf 'bench: building release (pass --no-build to reuse what is there)\n'
    (cd "$ROOT" && cargo build --release --bin fathom)
elif [[ ! -x $BINARY ]]; then
    die "$BINARY is not there; drop --no-build or pass --binary"
fi

OS_LABEL=$(uname -s | tr '[:upper:]' '[:lower:]')-$(uname -m)
RUST_LABEL=$(rustc --version 2>/dev/null | awk '{print $2}' || echo unknown)
# The profile is read off the binary rather than asserted, because `--binary` accepts anything and ADR-0002
# §3.3's whole point in asking for labels is that a debug timing and a release timing are different facts
# that must never be averaged together.
case $BINARY in
    */debug/* | *debug*) PROFILE_LABEL=debug ;;
    *) PROFILE_LABEL=release ;;
esac
FIXTURE_LABEL="commits=$FIXTURE_COMMITS"

WORK=$(mktemp -d -t fathom-bench.XXXXXX)
FIXTURE=$WORK/repo
# shellcheck disable=SC2064
cleanup() {
    if ((KEEP)); then
        printf 'bench: fixture kept at %s\n' "$FIXTURE"
    else
        rm -rf "$WORK"
    fi
}
trap cleanup EXIT

printf 'bench: fixture %s commits, %s iterations, %s\n' "$FIXTURE_COMMITS" "$ITERATIONS" "$OS_LABEL"
make_fixture "$FIXTURE_COMMITS" "$FIXTURE"

# What gets measured, and why these and not more. ADR-0006 §9.1 budgets the TUI's first frame, and the
# TUI's first frame is `report`'s read-and-analyse path behind a terminal; the flags ADR-0006 §9.1 sketches
# for a fixed-frame timing harness are not in this build, so this measures the path itself rather than
# pretending to measure the frame, and says so in the metric names. Each metric is one `fathom` invocation
# end to end, which is the unit a developer feels: process start, config, store open, analysis, render-free
# output.
#
# Measurement noise is left where a developer would notice it: the binary's warnings go to the log file,
# and the timing includes everything between fork and exit, because pretending the process costs nothing
# would make the number useless for comparing against a budget.
: >"$WORK/index.samples"
cold_index() {
    rm -rf "$FIXTURE/.fathom"
    "$BINARY" index "$FIXTURE" >>"$WORK/measure.log" 2>&1
}
for ((i = 0; i < ITERATIONS; i++)); do time_it "$WORK/index.samples" cold_index; done
run_metric fathom.index.full_ms "$WORK/index.samples" ms

# Cold report: no store, so the reader falls back to the direct git path and must still be quick. This is
# the §9.1 budget's path — first screen, nothing cached, nothing to hit — measured through the command that
# can be run without a terminal.
: >"$WORK/cold.samples"
cold_report() {
    rm -rf "$FIXTURE/.fathom"
    "$BINARY" report "$FIXTURE" >>"$WORK/measure.log" 2>&1
}
for ((i = 0; i < ITERATIONS; i++)); do time_it "$WORK/cold.samples" cold_report; done
run_metric fathom.report.cold_ms "$WORK/cold.samples" ms

# Warm report: the store exists and each run re-reads it, so warm and *not cached* — the distinction
# ADR-0006 §2.1 is careful about, since a hit cache answers from precomputed blocks and a warm store still
# pays the read.
"$BINARY" index "$FIXTURE" >>"$WORK/measure.log" 2>&1
: >"$WORK/warm.samples"
for ((i = 0; i < ITERATIONS; i++)); do
    time_it "$WORK/warm.samples" "$BINARY" report "$FIXTURE" >>"$WORK/measure.log" 2>&1
done
run_metric fathom.report.warm_ms "$WORK/warm.samples" ms

# What the reader pays just to find out what it already knows: `index --status` opens the store, reads the
# head row and the layout, and answers. It is the cheapest store read there is, which makes it the number
# that moves when the open path regresses.
: >"$WORK/status.samples"
for ((i = 0; i < ITERATIONS; i++)); do
    time_it "$WORK/status.samples" "$BINARY" index --status "$FIXTURE" >>"$WORK/measure.log" 2>&1
done
run_metric fathom.store.status_ms "$WORK/status.samples" ms

# Index throughput, derived from the cold-index samples: commits per second is what a background index has
# to fit inside the time a developer is willing to wait, and it is the only metric here that is a rate
# rather than a duration, which is why it is labelled with the fixture it came from.
rate=$(awk -v c="$FIXTURE_COMMITS" '{ sum += $1; n += 1 } END {
    if (n && sum > 0) printf "%.1f", c * 1000.0 * n / sum; else print 0
}' "$WORK/index.samples")
run_derived_metric fathom.index.commits_per_second "$rate" count

OUT=$(out_path)
write_payload "$OUT"
printf 'bench: wrote %s\n' "$OUT"

# The one gate: the budget the ADR names, if the caller named one.
if [[ -n $BUDGET ]]; then
    cold=$(median "$WORK/cold.samples")
    if awk -v v="$cold" -v b="$BUDGET" 'BEGIN { exit !(v + 0 > b + 0) }'; then
        die "cold report took ${cold}ms, over the ${BUDGET}ms budget"
    fi
    printf 'bench: cold report %sms is inside the %sms budget\n' "$cold" "$BUDGET"
fi
