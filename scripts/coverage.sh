#!/usr/bin/env bash
#
# scripts/coverage.sh — the coverage job's body, so the workflow says one line and the logic has a
# place to live: a script a human runs, a `--check` a CI job runs even where no toolchain is needed.
#
# It produces an lcov tracefile where the contract says to put one and prints the totals it means.
# The contract (ADR-0002 §3.2) is a filename, not a protocol: act-mcp's extractor looks for
# `$ACTMCP_EVIDENCE_OUT/lcov.info` first and `lcov.info` / `coverage/lcov.info` in the workspace
# after that, so this script writes the first when it is running inside a run and the second when it
# is being run by hand.
#
# Usage:
#   scripts/coverage.sh                       # cargo llvm-cov over the workspace → lcov.info
#   scripts/coverage.sh --fail-under 60       # and exit 1 if line coverage fell below 60%
#   scripts/coverage.sh --check               # self-test: no compiler, no run, no network
#   scripts/coverage.sh --print-out-path      # where the tracefile would go, then exit 0
#   scripts/coverage.sh -- --package fathom   # everything after `--` goes to cargo llvm-cov
#
# Where the tracefile goes, in order:
#   1. --output PATH
#   2. $ACTMCP_EVIDENCE_OUT/lcov.info      (inside an act run: the contract path)
#   3. <repo>/lcov.info                    (by hand: also found by the extractor, ADR-0002 §3.2)
#
# Exit status: 0 produced (and above any --fail-under floor); 1 no tracefile, an unparseable one, or
# a total below the floor; 2 bad usage or a missing tool (`cargo llvm-cov`, rustc).
#
# `cargo llvm-cov` is installed on demand unless --no-install says otherwise, because the runner
# images do not carry it and `cargo install --locked` is how the rest of this repository's pinned
# tools are obtained when a tarball is not available.

set -o errexit -o nounset -o pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)

OUTPUT=
FAIL_UNDER=
NO_INSTALL=0
SELF_CHECK=0
PRINT_PATH=0
EXTRA_ARGS=

usage() {
    cat <<'USAGE'
Usage: scripts/coverage.sh [OPTIONS] [-- CARGO_LLVM_COV_ARGS…]

Collects Rust line and branch coverage for this workspace into an lcov tracefile, and prints the
totals the way the evidence contract reads them.

  -o, --output PATH      where the tracefile goes (default: $ACTMCP_EVIDENCE_OUT/lcov.info, else
                         <repo>/lcov.info)
  -f, --fail-under PCT   exit 1 when line coverage is below PCT (a floor, not a gate policy: the
                         workflow decides whether the floor is part of CI)
      --no-install       refuse to install cargo-llvm-cov; fail if it is not already there
  -p, --print-out-path   print the resolved output path and exit 0
      --check            self-test the parser, the path rules and the floor logic. No compiler, no
                         tests, no network — safe to run anywhere python3 runs
  -h, --help             this text

Everything after `--` is passed to `cargo llvm-cov` verbatim, so a workflow can narrow the run
(`-- --workspace --exclude act-mcp`) without this script growing a flag for each option.

Exit: 0 ok; 1 no/unparseable tracefile or below the floor; 2 usage or missing tool.
USAGE
}

die() {
    printf 'coverage: %s\n' "$*" >&2
    exit 1
}

need() {
    command -v "$1" >/dev/null 2>&1 || {
        printf 'coverage: %s is required\n' "$1" >&2
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
        -o | --output)
            [[ $# -ge 2 ]] || die "$1 needs a value"
            OUTPUT=$2
            shift 2
            ;;
        -f | --fail-under)
            [[ $# -ge 2 ]] || die "$1 needs a value"
            FAIL_UNDER=$2
            shift 2
            ;;
        --no-install)
            NO_INSTALL=1
            shift
            ;;
        --)
            shift
            EXTRA_ARGS="$*"
            break
            ;;
        *)
            printf 'coverage: unknown argument: %s\n' "$1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

# repo_root — the workspace, when this runs inside one; the script's own directory's parent otherwise.
repo_root() {
    local root
    root=$(git rev-parse --show-toplevel 2>/dev/null || true)
    [[ -n $root ]] || root=$(cd -- "$script_dir/.." && pwd)
    printf '%s\n' "$root"
}

# out_path — the three rules above, in order. An empty $ACTMCP_EVIDENCE_OUT is "not inside a run",
# which is the difference between a job publishing and a developer's `cargo llvm-cov` run writing
# somewhere surprising.
out_path() {
    if [[ -n $OUTPUT ]]; then
        printf '%s\n' "$OUTPUT"
    elif [[ -n ${ACTMCP_EVIDENCE_OUT:-} ]]; then
        printf '%s/lcov.info\n' "$ACTMCP_EVIDENCE_OUT"
    else
        printf '%s/lcov.info\n' "$(repo_root)"
    fi
}

# lcov_totals FILE — `lines_found lines_hit branches_found branches_hit` summed over the tracefile.
#
# lcov counts a line at `DA:` and a branch at `BRDA:`; `LF`/`LH` and `BF`/`BH` are the per-record
# summary the writer emits, and the sum of the pairs is what both `genhtml` and the contract's own
# total record use, so it is what this prints. Branches may be absent (`BRDA:` lines missing), which
# is a real "no branch data" and not a zero to divide by.
lcov_totals() {
    awk -F: '
        /^[ \t]*LF:/ { lines_found += $2 + 0 }
        /^[ \t]*LH:/ { lines_hit   += $2 + 0 }
        /^[ \t]*BF:/ { branches_found += $2 + 0 }
        /^[ \t]*BH:/ { branches_hit   += $2 + 0 }
        END { printf "%d %d %d %d\n", lines_found, lines_hit, branches_found, branches_hit }
    ' "$1"
}

# percent HITS TOTAL — one decimal, or the empty string when TOTAL is 0 (absence, not zero: the
# contract's own rule for a number that could not be derived).
percent() {
    local hits=$1 total=$2
    [[ ${total:-0} -gt 0 ]] || return 0
    awk -v h="$hits" -v t="$total" 'BEGIN { printf "%.1f", 100.0 * h / t }'
}

ensure_llvm_cov() {
    if cargo llvm-cov --version >/dev/null 2>&1; then
        return 0
    fi
    # A missing tool is exit 2, the same code as a missing `cargo`: it is "could not run", and a caller
    # that distinguishes "the numbers are bad" from "there are no numbers and no way to get them" needs
    # the two codes to mean different things.
    if ((NO_INSTALL)); then
        printf 'coverage: cargo-llvm-cov is not installed and --no-install was passed\n' >&2
        printf 'coverage: fix: cargo install cargo-llvm-cov --locked\n' >&2
        exit 2
    fi
    printf 'coverage: installing cargo-llvm-cov (--locked; pass --no-install to forbid this)\n'
    if ! cargo install cargo-llvm-cov --locked >/dev/null 2>&1; then
        printf 'coverage: could not install cargo-llvm-cov (no network? no rustc?)\n' >&2
        exit 2
    fi
    cargo llvm-cov --version >/dev/null 2>&1 || {
        printf 'coverage: cargo-llvm-cov installed but does not run; is ~/.cargo/bin on PATH?\n' >&2
        exit 2
    }
}

# run_coverage — cargo llvm-cov, and the summary line it prints kept for the log.
run_coverage() {
    local out=$1
    ensure_llvm_cov
    mkdir -p "$(dirname -- "$out")"
    local args=(--lcov --output-path "$out")
    if [[ -n $EXTRA_ARGS ]]; then
        # Word-split on purpose: what follows `--` is a list of flags, and a flag with a space in it
        # is not a thing cargo accepts anyway.
        # shellcheck disable=SC2206
        args+=(${EXTRA_ARGS})
    else
        args+=(--workspace)
    fi
    printf 'coverage: cargo llvm-cov %s\n' "${args[*]}"
    cargo llvm-cov "${args[@]}"
}

report_totals() {
    local out=$1
    local totals lines_found lines_hit branches_found branches_hit lines branches
    totals=$(lcov_totals "$out")
    read -r lines_found lines_hit branches_found branches_hit <<<"$totals"
    lines=$(percent "$lines_hit" "$lines_found")
    branches=$(percent "$branches_hit" "$branches_found")
    printf 'coverage: %s\n' "$out"
    printf 'coverage: lines     %s%% (%s/%s)\n' "${lines:-unknown}" "$lines_hit" "$lines_found"
    printf 'coverage: branches  %s%% (%s/%s)\n' "${branches:-unknown}" "$branches_hit" "$branches_found"
    printf 'coverage_percent=%s\n' "${lines:-}"
    printf 'coverage_branches_percent=%s\n' "${branches:-}"
    [[ -n $lines ]] || die "no line coverage in $out — the tracefile is empty or not lcov"
    if [[ -n $FAIL_UNDER ]]; then
        local below
        below=$(awk -v p="$lines" -v floor="$FAIL_UNDER" 'BEGIN { print (p+0 < floor+0) ? "yes" : "no" }')
        if [[ $below == yes ]]; then
            die "line coverage ${lines}% is below the ${FAIL_UNDER}% floor"
        fi
        printf 'coverage: at or above the %s%% floor\n' "$FAIL_UNDER"
    fi
}

# self_check — the parts that are logic rather than compilation, checked with a fixture nobody has to
# build. This is what `scripts/ci.sh coverage` can run on a runner with no act and no docker.
self_check() {
    need python3
    local dir
    dir=$(mktemp -d -t coverage-check.XXXXXX)
    # shellcheck disable=SC2064
    trap "rm -rf '$dir'" EXIT
    local failures=0

    # 1. the parser, on a tracefile whose answer is known by hand: LF 10, LH 4 → 40.0%; BF 4, BH 1.
    cat >"$dir/lcov.info" <<'LCOV'
TN:
SF:/workspace/crates/fathom/src/lib.rs
DA:1,1
DA:2,1
DA:3,0
FNF:3
FNH:2
BRDA:1,0,0,1
BRDA:1,0,1,0
LF:10
LH:4
BF:4
BH:1
end_of_record
SF:/workspace/crates/fathom/src/cli.rs
LF:6
LH:6
end_of_record
LCOV
    local totals lines branches
    totals=$(lcov_totals "$dir/lcov.info")
    [[ $totals == "16 10 4 1" ]] || {
        printf 'FAIL totals: expected "16 10 4 1", got "%s"\n' "$totals"
        failures=$((failures + 1))
    }
    lines=$(percent 4 10)
    [[ $lines == "40.0" ]] || {
        printf 'FAIL percent: expected 40.0, got %s\n' "$lines"
        failures=$((failures + 1))
    }
    [[ -z $(percent 0 0) ]] || {
        printf 'FAIL percent: a zero denominator must be absence, not zero\n'
        failures=$((failures + 1))
    }
    printf 'ok   totals, percent, and absence-for-zero\n'

    # 2. the path rules, asserted through the CLI a job actually calls: --print-out-path with
    # --output, with $ACTMCP_EVIDENCE_OUT, and with neither.
    local want=$dir/explicit/lcov.info
    local got
    got=$("$script_dir/coverage.sh" --print-out-path --output "$want")
    [[ $got == "$want" ]] || {
        printf 'FAIL --output: expected %s, got %s\n' "$want" "$got"
        failures=$((failures + 1))
    }
    got=$(ACTMCP_EVIDENCE_OUT="$dir/incoming/20261119T000000Z-abcdef" \
        "$script_dir/coverage.sh" --print-out-path)
    [[ $got == "$dir/incoming/20261119T000000Z-abcdef/lcov.info" ]] || {
        printf 'FAIL ACTMCP_EVIDENCE_OUT: expected the contract path, got %s\n' "$got"
        failures=$((failures + 1))
    }
    got=$(ACTMCP_EVIDENCE_OUT= "$script_dir/coverage.sh" --print-out-path)
    [[ $got == *"/lcov.info" && $got != "$dir"/* ]] || {
        printf 'FAIL default: expected the repository path, got %s\n' "$got"
        failures=$((failures + 1))
    }
    printf 'ok   --output, $ACTMCP_EVIDENCE_OUT and the repository default\n'

    # 3. the floor decision, which is pure arithmetic: the same comparison `report_totals` makes.
    local below above
    below=$(awk -v p="40.0" -v f="60" 'BEGIN { print (p+0 < f+0) ? "yes" : "no" }')
    above=$(awk -v p="62.5" -v f="60" 'BEGIN { print (p+0 < f+0) ? "yes" : "no" }')
    [[ $below == yes && $above == no ]] || {
        printf 'FAIL floor: 40 vs 60 → %s, 62.5 vs 60 → %s\n' "$below" "$above"
        failures=$((failures + 1))
    }
    printf 'ok   the --fail-under comparison\n'

    # 4. an empty tracefile is a failure, not a 0% coverage run.
    : >"$dir/empty.info"
    if [[ -n $(percent "$(lcov_totals "$dir/empty.info" | cut -d' ' -f2)" \
        "$(lcov_totals "$dir/empty.info" | cut -d' ' -f1)") ]]; then
        printf 'FAIL an empty tracefile produced a percentage instead of absence\n'
        failures=$((failures + 1))
    fi
    printf 'ok   an empty tracefile is absence, not zero\n'

    if ((failures)); then
        printf 'coverage --check: %s check(s) failed\n' "$failures" >&2
        return 1
    fi
    printf 'coverage --check: lcov parsing, output paths and the floor all behave\n'
}

if ((SELF_CHECK)); then
    self_check
    exit $?
fi

if ((PRINT_PATH)); then
    out_path
    exit 0
fi

need cargo
need awk

OUT=$(out_path)
run_coverage "$OUT"
[[ -s $OUT ]] || die "cargo llvm-cov produced no tracefile at $OUT"
report_totals "$OUT"
