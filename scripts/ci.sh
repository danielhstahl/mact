#!/usr/bin/env bash
#
# scripts/ci.sh — the whole act-mcp check, in the order CI runs it, on your machine.
#
# The GitHub workflow (.github/workflows/ci.yml) calls this script and nothing else; the
# reasoning for every check lives here (and in the files it calls) so that a red build and
# a green `scripts/ci.sh` are the same fact. Repo convention: logic in scripts, not in the
# YAML.
#
# What it checks, in order:
#
#   fmt          rustfmt, in --check mode. Formatting is a machine's job: `cargo fmt --all`.
#   clippy       `cargo clippy --all-targets --all-features -- -D warnings`. Warnings are
#                errors, so a new clippy is a code change someone reads, not noise nobody
#                reads. Lint suppresses carry a `reason =` saying why.
#   doc          `cargo doc --no-deps` with rustdoc warnings fatal. A broken intra-doc link
#                is a broken reference into the reasoning this crate's docs exist to carry,
#                so it is an error, not a warning.
#   test         `cargo test --all-features`. The whole contract: unit tests, the tool
#                suites, the protocol conformance matrix and tests/e2e (real server, stub
#                act). Nothing in here needs docker or act — if a test needs them it is the
#                wrong test, and it belongs behind `#[ignore]` with the reason in its
#                `reason =` (see docs/testing.md).
#   schema       the JSON Schema half of ADR-0002: every file in schema/ has to be a legal
#                schema for the dialect it declares, `schema/evidence.schema.json` has to
#                compile in the engine the binary embeds, and the committed e2e evidence
#                tree has to validate against it (the e2e suite re-validates the *trees it
#                generates*; this validates the ones in the repo, so the check runs without
#                running the suite). Then the two ADR↔schema checkers docs/testing.md calls
#                CI's (`validate_run_lifecycle.py`, `validate_isolation.py`), and python's
#                own jsonschema over the same tree as a second, independent implementation.
#                `validate_tool_contract.py` is deliberately not here yet: docs/ci.md says
#                what it is waiting on.
#   supply-chain cargo-deny: licences, duplicate/wildcard policy and sources always, RUSTSEC
#                advisories when the network is allowed (`--offline` skips exactly that
#                part and says so). `cargo tree -d` is printed alongside as a note.
#   release      `cargo build --release` plus scripts/smoke_binary.sh: the shipped binary
#                must answer --version and --help, must reject a bad flag, and must run
#                validate-evidence for real. A binary that builds but cannot run is not a
#                release.
#
# Usage and the full flag list: `scripts/ci.sh --help`. CI runs
# `scripts/ci.sh --keep-going fmt clippy doc test schema supply-chain` and a separate
# release job runs `scripts/ci.sh release` per target.

set -o errexit -o nounset -o pipefail

# The canonical order, and the order the header documents.
STEPS_ALL=(fmt clippy doc test schema supply-chain release)

usage() {
    cat <<'USAGE'
Usage: scripts/ci.sh [OPTIONS] [STEP ...]

Runs the act-mcp checks. With no STEP, runs every step in the canonical order
(fmt clippy doc test schema supply-chain release). With STEPs, runs exactly those,
in canonical order.

Options:
  -h, --help          this text
  -l, --list          list the step names and exit
  -k, --keep-going    run every selected step even if one fails, then fail at the end.
                      This is what CI does: one red step must not hide the others.
      --offline       skip the checks that need the network. Exactly one check is affected:
                      `cargo deny check advisories` (the RUSTSEC database fetch). Licences,
                      bans and sources still run in the supply-chain step.
      --target TRIPLE pass --target TRIPLE to cargo for the release step (the release job
                      cross-builds one binary per platform this project ships).
  -q, --quiet         only the step headers, the failures and the summary

Environment:
  CARGO_TARGET_DIR    honoured as always. On a bind-mounted checkout rustc can take SIGBUS
                      while linking into ./target (docs/testing.md) — point it at /tmp.
  PYTHON              the interpreter used by the schema step (default: python3). The
                      step's second-opinion checks need the `jsonschema` package; without
                      it they are reported as skipped, and the hard checks (the Rust
                      engine the binary embeds) still run.
  CARGO_DENY_VERSION  the version scripts/install_ci_tools.sh installs.

Exit status:
  0 every selected step passed   1 a step failed   2 bad usage or a missing tool

Examples:
  scripts/ci.sh                          everything, stop at the first failure
  scripts/ci.sh fmt clippy               the two things a pre-commit hook wants
  scripts/ci.sh --keep-going             what CI runs, locally
  scripts/ci.sh --offline --quiet test   just the tests, no network, terse
  scripts/ci.sh --target aarch64-unknown-linux-gnu release
USAGE
}

# ---------------------------------------------------------------------------
# options
# ---------------------------------------------------------------------------
KEEP_GOING=0
OFFLINE=0
QUIET=0
SELECTED=()

while [[ $# -gt 0 ]]; do
    case "$1" in
        -h | --help)
            usage
            exit 0
            ;;
        -l | --list)
            printf '%s\n' "${STEPS_ALL[@]}"
            exit 0
            ;;
        -k | --keep-going) KEEP_GOING=1 ;;
        --offline) OFFLINE=1 ;;
        -q | --quiet) QUIET=1 ;;
        --target)
            if [[ $# -lt 2 ]]; then
                echo "ci.sh: --target needs a triple (e.g. x86_64-unknown-linux-gnu)" >&2
                exit 2
            fi
            CARGO_BUILD_TARGET="$2"
            export CARGO_BUILD_TARGET
            shift
            ;;
        --) ;;
        -*)
            echo "ci.sh: unknown option '$1' (try --help)" >&2
            exit 2
            ;;
        *)
            case " ${STEPS_ALL[*]} " in
                *" $1 "*) SELECTED+=("$1") ;;
                *)
                    echo "ci.sh: unknown step '$1' (try --list)" >&2
                    exit 2
                    ;;
            esac
            ;;
    esac
    shift
done

if [[ ${#SELECTED[@]} -eq 0 ]]; then
    SELECTED=("${STEPS_ALL[@]}")
fi

# ---------------------------------------------------------------------------
# where we are, and how we talk
# ---------------------------------------------------------------------------
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
if repo_root=$(git -C "$script_dir" rev-parse --show-toplevel 2>/dev/null); then
    : # inside a work tree: the tree root is the project
else
    repo_root=$(cd -- "$script_dir/.." && pwd)
fi
cd "$repo_root"

target_dir=${CARGO_TARGET_DIR:-$repo_root/target}

if [[ -t 1 && -z "${NO_COLOR:-}" ]]; then
    c_bold=$'\033[1m'
    c_red=$'\033[31m'
    c_green=$'\033[32m'
    c_yellow=$'\033[33m'
    c_off=$'\033[0m'
else
    c_bold='' c_red='' c_green='' c_yellow='' c_off=''
fi

# GitHub Actions gets step groups instead of colour; a log a human can scan is the point of
# both.
in_gh_actions=${GITHUB_ACTIONS:-}

group_start() {
    if [[ -n "$in_gh_actions" ]]; then
        printf '::group::%s\n' "$1"
    else
        printf '\n%s==> %s%s\n' "$c_bold" "$1" "$c_off"
    fi
}

group_end() {
    if [[ -n "$in_gh_actions" ]]; then
        printf '::endgroup::\n'
    fi
}

note() { printf '%s  note:%s %s\n' "$c_yellow" "$c_off" "$*"; }
fix() { printf '%s  fix:%s %s\n' "$c_yellow" "$c_off" "$*"; }

# ---------------------------------------------------------------------------
# the steps
# ---------------------------------------------------------------------------
step_fmt() {
    cargo fmt --all -- --check
}

step_clippy() {
    cargo clippy --all-targets --all-features -- -D warnings
}

step_doc() {
    # Warnings fatal, and the intra-doc-link lint named even though `-D warnings` already
    # covers it: the whole reason this step exists is that a link into the crate's own
    # reasoning silently stopped pointing at anything.
    RUSTDOCFLAGS="-D warnings -D rustdoc::broken_intra_doc_links ${RUSTDOCFLAGS:-}" \
        cargo doc --no-deps --all-features
}

step_test() {
    # No --ignored: the ignored tests are the ones that wait on wall-clock time (or need a
    # container daemon), and CI has neither the minutes nor the daemon.
    RUST_BACKTRACE=1 cargo test --all-features
}

step_schema() {
    # The engine that matters is the one the binary embeds: `schema/evidence.schema.json`
    # is `include_str!`-ed into act-mcp, so checking those bytes is what
    # `act-mcp validate-evidence` does in production. `--schema`/`--meta-schema` name the
    # files on disk so this step fails on the file, not only on the embedded copy.
    cargo build
    local bin="$target_dir/debug/act-mcp"
    if [[ ! -x "$bin" ]]; then
        echo "schema: expected a built binary at $bin and did not find one" >&2
        return 1
    fi

    echo "--- act-mcp validate-evidence (jsonschema engine embedded in the binary)"
    "$bin" validate-evidence \
        --schema schema/evidence.schema.json \
        --meta-schema schema/run-meta.schema.json \
        tests/e2e/testdata/expected

    local py=${PYTHON:-python3}

    # 2. the ADR↔schema checkers docs/testing.md names as CI's: no python packages needed,
    #    milliseconds each, and they fail when an accepted ADR and the schema implementing it
    #    stop agreeing with one another.
    echo "--- ADR <-> schema checkers ($py)"
    "$py" scripts/validate_run_lifecycle.py
    "$py" scripts/validate_isolation.py

    # 3. python's jsonschema as a second, independent implementation of the same contract
    #    (ADR-0002 §7 wants a disagreement between two engines to show up here rather than
    #    as a client bug report). Skipped loudly when the package is missing: these two are
    #    second opinions, and the hard checks above are the verdict.
    if ! "$py" -c 'import jsonschema' >/dev/null 2>&1; then
        note "$py has no 'jsonschema' package: skipping the two second-opinion checks below."
        note "the hard checks above still ran; install it with scripts/install_ci_tools.sh"
        note "or point PYTHON at an interpreter that has it."
        return 0
    fi

    # Every file in schema/ must be a legal schema *for the dialect it declares*, which is
    # the check that catches a schema that is valid JSON and nonsense.
    echo "--- schema/*.json against the meta-schema they declare ($py)"
    "$py" - <<'PY'
import json
import pathlib
import sys

from jsonschema import validators

failures = 0
for path in sorted(pathlib.Path("schema").glob("*.json")):
    try:
        document = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as err:
        print(f"{path}: cannot be read as JSON: {err}", file=sys.stderr)
        failures += 1
        continue
    dialect = validators.validator_for(document)
    declared = document.get("$schema", "(no $schema: inferred)")
    try:
        dialect.check_schema(document)
    except Exception as err:  # noqa: BLE001 - the message is the finding
        print(f"{path}: not a legal schema ({declared}): {err}", file=sys.stderr)
        failures += 1
        continue
    print(f"  {path}: legal — {declared}")
sys.exit(1 if failures else 0)
PY

    # And the same evidence tree through python rather than through Rust.
    echo "--- scripts/validate_evidence.py (independent implementation)"
    "$py" scripts/validate_evidence.py \
        --schema schema/evidence.schema.json \
        --meta-schema schema/run-meta.schema.json \
        tests/e2e/testdata/expected
}

step_supply_chain() {
    if command -v cargo-deny >/dev/null 2>&1; then
        echo "--- cargo deny check licenses bans sources (deny.toml)"
        cargo deny check licenses bans sources
        if [[ "$OFFLINE" == 1 ]]; then
            note "--offline: skipped 'cargo deny check advisories' (it fetches the RUSTSEC database)."
        else
            echo "--- cargo deny check advisories (RUSTSEC)"
            cargo deny check advisories
        fi
    elif command -v cargo-audit >/dev/null 2>&1; then
        note "cargo-deny is not installed; falling back to cargo audit (no licence/source check)."
        fix "install the full check with scripts/install_ci_tools.sh"
        cargo audit
    else
        echo "supply-chain: neither cargo-deny nor cargo-audit is installed, and this step is"
        echo "not optional: an unchecked dependency graph is an accepted dependency graph."
        fix "scripts/install_ci_tools.sh installs cargo-deny from its release tarball; deny.toml holds the policy"
        return 1
    fi

    # A note, never a verdict: duplicate versions are normal in a graph this size, and the
    # ones that are *needless* (two majors of a big crate for no reason) are what this is
    # for. cargo-deny's own `bans` check prints the same list as warnings above.
    local dupes
    dupes=$(cargo tree -d --edges normal,build 2>/dev/null | awk '/^[a-z0-9]/ { sub(/ v.*$/, "", $1); print $1 }' | sort -u | tr '\n' ' ')
    if [[ -n "${dupes// /}" ]]; then
        note "dependency graph resolves more than one version of: ${dupes}(cargo tree -d has the tree)"
    else
        note "dependency graph has no duplicated crates."
    fi
}

step_release() {
    local build_target=()
    local bin
    if [[ -n "${CARGO_BUILD_TARGET:-}" ]]; then
        build_target=(--target "$CARGO_BUILD_TARGET")
        bin="$target_dir/$CARGO_BUILD_TARGET/release/act-mcp"
    else
        bin="$target_dir/release/act-mcp"
    fi

    cargo build --release "${build_target[@]}"

    # Where the smoke script drops --version/sha256 for the artifact steps to upload.
    local report_dir
    report_dir=$(dirname "$bin")/dist
    "$script_dir/smoke_binary.sh" --report "$report_dir" --fixture "$repo_root/tests/e2e/testdata/expected" "$bin"
}

# What each step wants, printed when it fails so the log answers "now what".
declare -A STEP_FIX=(
    [fmt]="cargo fmt --all"
    [clippy]="cargo clippy --all-targets --all-features, then read the lints; suppress one only with a reason"
    [doc]="the failing link is named above; point it at a real path or make it a code span"
    [test]="cargo test --all-features -- --nocapture <the failing test>; docs/testing.md explains the suites"
    [schema]="act-mcp validate-evidence <path> prints every finding; ADR-0002 §7 is the contract"
    [supply-chain]="scripts/install_ci_tools.sh installs cargo-deny; deny.toml holds the policy"
    [release]="the binary must run --help and --version; scripts/smoke_binary.sh <path> reproduces it alone"
)

# ---------------------------------------------------------------------------
# run
# ---------------------------------------------------------------------------
declare -a summary_status=()
declare -a summary_line=()
failed=0

if [[ ${#SELECTED[@]} -gt 1 ]]; then
    echo "act-mcp CI — repo $repo_root, toolchain $(rustc -Vv | tr '\n' ' ')"
    echo "steps: ${SELECTED[*]}"
fi

for step in "${SELECTED[@]}"; do
    group_start "ci: $step"
    step_start=$(date +%s)
    # `supply-chain` is the step's name; bash function names spell it `step_supply_chain`.
    step_fn="step_${step//-/_}"
    if ! declare -F "$step_fn" >/dev/null; then
        echo "ci.sh: step '$step' has no implementation ($step_fn is not defined)" >&2
        exit 2
    fi
    # NOTE on plumbing: a step must fail the run when a command *inside* it fails, and an
    # `if cmd` context switches `set -e` off for the whole function body — which would let a
    # failing check in the middle of a step fall on the floor. So each step runs in a
    # subshell that turns errexit back on and is not a condition.
    set +e
    if [[ "$QUIET" == 1 ]]; then
        ( set -e; "$step_fn" ) >/dev/null 2>&1
    else
        ( set -e; "$step_fn" )
    fi
    status=$?
    set -e
    elapsed=$(( $(date +%s) - step_start ))

    if [[ $status -eq 0 ]]; then
        summary_status+=("pass")
        summary_line+=("$(printf '%-14s %6ss' "$step" "$elapsed")")
        if [[ "$QUIET" == 0 ]]; then
            printf '%s  %s passed%s (%ss)\n' "$c_green" "$step" "$c_off" "$elapsed"
        fi
    else
        failed=1
        summary_status+=("fail")
        summary_line+=("$(printf '%-14s %6ss' "$step" "$elapsed")")
        printf '%s  %s FAILED%s (%ss, exit %s)\n' "$c_red" "$step" "$c_off" "$elapsed" "$status"
        if [[ -n "${STEP_FIX[$step]:-}" ]]; then
            fix "${STEP_FIX[$step]}"
        fi
        if [[ -n "$in_gh_actions" ]]; then
            printf '::error::ci step %s failed\n' "$step"
        fi
    fi
    group_end

    if [[ $status -ne 0 && "$KEEP_GOING" == 0 ]]; then
        break
    fi
done

echo
if [[ ${#summary_status[@]} -gt 1 ]]; then
    echo "summary"
    for i in "${!summary_line[@]}"; do
        if [[ "${summary_status[$i]}" == "pass" ]]; then
            printf '  %s%s%s\n' "$c_green" "${summary_line[$i]}" "$c_off"
        else
            printf '  %s%s%s\n' "$c_red" "${summary_line[$i]}" "$c_off"
        fi
    done
fi

if [[ $failed -ne 0 ]]; then
    echo "FAILED"
    exit 1
fi
echo "OK"
