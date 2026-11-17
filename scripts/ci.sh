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
#   build        `cargo build --workspace --all-features`, then a name-check that both shipped
#                binaries (`act-mcp`, `fathom`) exist. `clippy` and `test` compile the workspace
#                too, but as their own targets; this is the step that says "the workspace builds,
#                and it builds the two programs it ships", which is the fact ADR-0005 §3 exists to
#                keep independent and the cheapest place to notice a binary stopped linking.
#   doc          `cargo doc --no-deps` with rustdoc warnings fatal. A broken intra-doc link
#                is a broken reference into the reasoning this crate's docs exist to carry,
#                so it is an error, not a warning.
#   test         `cargo test --all-features`. The whole contract: unit tests, the tool
#                suites, the protocol conformance matrix and crates/act-mcp/tests/e2e (real server, stub
#                act). Nothing in here needs docker or act — if a test needs them it is the
#                wrong test, and it belongs behind `#[ignore]` with the reason in its
#                `reason =` (see docs/testing.md).
#   tui-snapshot the fathom screens, drawn: the ratatui `TestBackend` goldens in
#                crates/fathom/tests/tui_{render,detail,detail_store,diff,bench,store}.rs. They are
#                already inside `test`, and this step repeats them on purpose: a snapshot mismatch is
#                a design change wearing a test failure's clothes, and it should name itself in a CI
#                log (and rerun in half a second locally) instead of scrolling past with 900 lines.
#   fathom-report the report golden and the report smoke, in that order: `cargo test -p fathom --test
#                report` (the two envelope goldens in crates/fathom/tests/golden/, the report-vs-
#                `--plain` cell equality and the exit codes) and then `scripts/smoke_fathom.sh --check
#                report`, which runs the built binary over the tree `scripts/sample_evidence.py` lays
#                and checks the envelope, the tsv header, that an index lands, and that `--fail-on`
#                exits 1 exactly when its own gate says `findings`. The golden is the numbers; the
#                smoke is the artefact.
#   fathom-doctor `scripts/smoke_fathom.sh --check doctor`: `fathom doctor --json`'s exit code is its
#                worst check (0 ok, 1 warn, 2 fail), every documented check is present, and a
#                directory that is not a work tree is refused with 2. `doctor` is a command whose
#                whole output is a status code, so the code is the contract and it is smoke-tested,
#                not trusted.
#   schema       the JSON Schema contract, both accepted contracts. ADR-0002 first: every file
#                in schema/ has to be a legal schema for the dialect it declares,
#                `schema/evidence.schema.json` has to compile in the engine the binary
#                embeds, and the committed e2e evidence tree has to validate against it (the
#                e2e suite re-validates the *trees it generates*; this validates the ones in
#                the repo, so the check runs without running the suite). Then the five
#                ADR↔contract checkers docs/testing.md calls CI's — ADR-0001's tool contract
#                (`validate_tool_contract.py`), ADR-0004's run lifecycle
#                (`validate_run_lifecycle.py`), ADR-0003's isolation protocol
#                (`validate_isolation.py`), ADR-0005's crate layout
#                (`validate_crate_layout.py`, which reads Cargo.toml rather than a schema) and
#                ADR-0006's `.fathom` layout (`validate_fathom_layout.py`, which reads
#                .gitignore and .fathom/LAYOUT.md) and ADR-0007's Maintainability Index
#                (`validate_mi_definition.py`, which reads schema/mi-definition.json, the golden
#                vectors in tests/testdata/mi and deny.toml, and re-derives every number twice)
#                and ADR-0008's per-commit metric model (`validate_metric_model.py`, which reads
#                schema/metric-model.json, re-derives the golden vectors in tests/testdata/metrics,
#                and diffs the reference implementation against the recorded jq-recipe output in
#                docs/evidence-format.md §12) and the Fathom documentation surface
#                (`validate_fathom_docs.py`, which asks the built `fathom --help` what it accepts and
#                fails when a flag, key or exit code is undocumented, documented twice, or invented)
#                — and python's own jsonschema over the same
#                evidence tree as a second, independent implementation.
#   supply-chain cargo-deny: licences, duplicate/wildcard policy and sources always, RUSTSEC
#                advisories when the network is allowed (`--offline` skips exactly that
#                part and says so). `cargo tree -d` is printed alongside as a note. Then
#                `scripts/check_dependency_firewalls.sh`, for the two things deny.toml cannot say: no
#                crate from one side of ADR-0005's graph reaches the other (transitively included,
#                which is the only kind that sneaks in past a manifest checker), and nothing in the
#                resolved graph of any shipped target needs a C toolchain (ADR-0003, ADR-0006 §5,
#                ADR-0007 §6).
#   release-workspace `cargo build --release --workspace`: the release profile over every member, which
#                is what the release job runs before it packages anything. A release-only failure — an
#                optimisation-time ICE, a `cfg(not(debug_assertions))` arm that does not compile — is
#                red here rather than in the middle of an artefact build.
#   release      the shipped artefacts, one at a time: `cargo build --release -p <crate> --bin <bin>`
#                plus that binary's smoke (`scripts/smoke_binary.sh` for `act-mcp`,
#                `scripts/smoke_fathom.sh` for `fathom`): the binary must answer --version and --help,
#                must refuse a bad flag, and must do its real work on the fixtures it is given. A
#                binary that builds but cannot run is not a release. Selective (`-p X --bin X`) and
#                one artefact at a time on purpose: one artefact is one binary, and a broken TUI must
#                never block shipping the server (ADR-0005 §3) — which is why `cd.yml` builds, smokes
#                and uploads `act-mcp` and `fathom` in separate steps with `--artefact`.
#
# Usage and the full flag list: `scripts/ci.sh --help`. CI runs
# `scripts/ci.sh --keep-going fmt clippy build doc test tui-snapshot fathom-report fathom-doctor
# schema supply-chain` in the check job; the release job runs
# `scripts/ci.sh --keep-going supply-chain release-workspace` and then `scripts/ci.sh release` per
# target.

set -o errexit -o nounset -o pipefail

# The canonical order, and the order the header documents. `build` is workspace-wide and comes right
# after clippy so a binary that does not link is red before anything pays for rustdoc; the three
# fathom steps sit after `test` (whose suite contains them) so a fathom regression names the step it
# belongs to; `release-workspace` precedes `release` because the artefacts are packaged from a tree
# that already survived the release profile.
STEPS_ALL=(fmt clippy build doc test tui-snapshot fathom-report fathom-doctor schema
           supply-chain release-workspace release)

usage() {
    cat <<'USAGE'
Usage: scripts/ci.sh [OPTIONS] [STEP ...]

Runs the act-mcp workspace's checks — the server, the contract crate and fathom. With no STEP,
runs every step in the canonical order (fmt clippy build doc test tui-snapshot fathom-report
fathom-doctor schema supply-chain release-workspace release). With STEPs, runs exactly those,
in canonical order.

Options:
  -h, --help          this text
  -l, --list          list the step names and exit
  -k, --keep-going    run every selected step even if one fails, then fail at the end.
                      This is what CI does: one red step must not hide the others.
      --offline       skip the checks that need the network. Exactly one check is affected:
                      `cargo deny check advisories` (the RUSTSEC database fetch). Licences,
                      bans and sources still run in the supply-chain step.
      --target TRIPLE pass --target TRIPLE to cargo for the release steps (the release job
                      cross-builds one binary per platform this project ships).
      --artefact NAME which shipped binary the release step builds and smokes: `act-mcp`, `fathom`,
                      or the default of both. Repeatable, and cd.yml uses it to build, smoke and
                      upload each artefact in its own step so a fathom that will not build cannot
                      hold up the server (ADR-0005 §3).
  -q, --quiet         only the step headers, the failures (with the output that caused them) and
                      the summary

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
  scripts/ci.sh tui-snapshot fathom-report    the two fathom golden steps, nothing else
  scripts/ci.sh --target aarch64-unknown-linux-gnu release
  scripts/ci.sh --artefact act-mcp release    ship-the-server release, TUI breakage aside
USAGE
}

# ---------------------------------------------------------------------------
# options
# ---------------------------------------------------------------------------
KEEP_GOING=0
OFFLINE=0
QUIET=0
SELECTED=()
# The shipped binaries, in the order the release step builds them: the server first, because nothing
# that happens to `fathom` afterwards may stop it (ADR-0005 §3).
ARTEFACTS_ALL=(act-mcp fathom)
ARTEFACTS=()

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
        --artefact)
            if [[ $# -lt 2 ]]; then
                echo "ci.sh: --artefact needs a name (${ARTEFACTS_ALL[*]})" >&2
                exit 2
            fi
            case " ${ARTEFACTS_ALL[*]} " in
                *" $2 "*) ARTEFACTS+=("$2") ;;
                *)
                    echo "ci.sh: unknown artefact '$2' (one of: ${ARTEFACTS_ALL[*]})" >&2
                    exit 2
                    ;;
            esac
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
if [[ ${#ARTEFACTS[@]} -eq 0 ]]; then
    ARTEFACTS=("${ARTEFACTS_ALL[@]}")
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

# The integration tests and their fixtures live in the crate since the workspace split
# (ADR-0005 §9: Cargo looks for `tests/` next to the package manifest), while `schema/` and
# `scripts/` stayed at the root — so anything that names a test fixture goes through this.
crate_tests="$repo_root/crates/act-mcp/tests"

# Where a binary of ours ends up, which is `target/[<triple>/]<profile>/<name>` and depends on the
# --target option the release job passes. Both helpers echo a path; neither checks it, because the
# step that cares says why when the file is not there.
debug_bin() { printf '%s/%sdebug/%s\n' "$target_dir" "${CARGO_BUILD_TARGET:+$CARGO_BUILD_TARGET/}" "$1"; }
release_bin() { printf '%s/%srelease/%s\n' "$target_dir" "${CARGO_BUILD_TARGET:+$CARGO_BUILD_TARGET/}" "$1"; }

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

step_build() {
    # Workspace-wide, every feature, both binaries. `clippy` and `test` compile the same graph, but
    # they compile it as *their* targets and neither of them promises that a shipped binary linked:
    # clippy stops at metadata for most crates, and `cargo test` builds the binary only to run its
    # integration tests, which a broken `[[bin]]` path or a `required-features` mistake can survive.
    # This step is also where "the workspace has two programs" (ADR-0005 §3) is checked as a fact: a
    # third member that stopped being a member, or a binary that silently disappeared, is a red here
    # with the binary's name in it.
    cargo build --workspace --all-features
    local artefact bin
    for artefact in "${ARTEFACTS_ALL[@]}"; do
        bin=$(debug_bin "$artefact")
        if [[ ! -x "$bin" ]]; then
            echo "build: --workspace is green but there is no runnable $bin" >&2
            return 1
        fi
        echo "--- built $bin"
    done
}

step_test() {
    # No --ignored: the ignored tests are the ones that wait on wall-clock time (or need a
    # container daemon), and CI has neither the minutes nor the daemon.
    RUST_BACKTRACE=1 cargo test --all-features
}

step_tui_snapshot() {
    # The screens, drawn into ratatui's TestBackend and compared against a golden written out in the
    # test file (see crates/fathom/tests/tui_render.rs's header: no PTY, no process, no timing, and
    # deliberately no colour, because a golden that fails when a palette is re-tuned tests the wrong
    # thing). `test` already runs these; the point of naming them is that a snapshot mismatch is a
    # user-visible design change, and a step whose whole name is the thing that changed is a step a
    # reviewer reads. Six suites: the timeline, the detail frame (twice: synthetic and off the real
    # store), the diff view, the benchmark panel, and the responsive loop's own drawing.
    cargo test -p fathom \
        --test tui_render \
        --test tui_detail \
        --test tui_detail_store \
        --test tui_diff \
        --test tui_bench \
        --test tui_store
}

step_fathom_report() {
    # Two halves, in the order that fails cheapest. The goldens first:
    # `crates/fathom/tests/golden/report.json` is the hand-built fixture (unknowns, gap codes, a
    # mixed action verdict) and `report-sample-tree.json` is `scripts/sample_evidence.py`'s tree
    # re-stamped onto the fixture's commits, so a renamed or dropped envelope field is a failing test
    # rather than a broken dashboard downstream (ADR-0006 §10.1). Then the same shape asked of the
    # built binary over the sample tree itself — `cargo test` never loads the linked image, so
    # "the artefact reports" is a separate fact, and `--fail-on`'s 0/1 agreement with its own
    # `gate.status` is the part a CI step is written against.
    cargo test -p fathom --test report
    cargo build -p fathom --bin fathom
    local fbin
    fbin=$(debug_bin fathom)
    if [[ ! -x "$fbin" ]]; then
        echo "fathom-report: expected a built fathom at $fbin and did not find one" >&2
        return 1
    fi
    echo "--- scripts/smoke_fathom.sh --check report ($fbin)"
    "$script_dir/smoke_fathom.sh" --check report "$fbin"
}

step_fathom_doctor() {
    # `doctor`'s whole output is a status code and a reason, so the code is the interface: 0 clean,
    # 1 findings, 2 could not run — the same three meanings `act-mcp validate-evidence` has, which is
    # what lets one pipeline line treat both tools alike. The smoke asserts the code is *derived* from
    # the checks the command just reported (and that a directory which is not a work tree is 2, not 0),
    # which is a promise about the binary rather than about this repository's setup — what the checks
    # say about a runner with no act-mcp on it is legitimately a `warn`.
    cargo build -p fathom --bin fathom
    local fbin
    fbin=$(debug_bin fathom)
    if [[ ! -x "$fbin" ]]; then
        echo "fathom-doctor: expected a built fathom at $fbin and did not find one" >&2
        return 1
    fi
    echo "--- scripts/smoke_fathom.sh --check doctor ($fbin)"
    "$script_dir/smoke_fathom.sh" --check doctor "$fbin"
}

step_schema() {
    # The engine that matters is the one the binary embeds: `schema/evidence.schema.json`
    # is `include_str!`-ed into act-mcp, so checking those bytes is what
    # `act-mcp validate-evidence` does in production. `--schema`/`--meta-schema` name the
    # files on disk so this step fails on the file, not only on the embedded copy.
    # Selective building, and not only for speed: the schema step is about the server's binary, and
    # since ADR-0005 a broken TUI must never be able to fail the contract checker. `-p act-mcp
    # --bin act-mcp` is the same spelling the release step uses.
    cargo build -p act-mcp --bin act-mcp
    local bin
    if [[ -n "${CARGO_BUILD_TARGET:-}" ]]; then
        bin="$target_dir/$CARGO_BUILD_TARGET/debug/act-mcp"
    else
        bin="$target_dir/debug/act-mcp"
    fi
    if [[ ! -x "$bin" ]]; then
        echo "schema: expected a built binary at $bin and did not find one" >&2
        return 1
    fi

    echo "--- act-mcp validate-evidence (jsonschema engine embedded in the binary)"
    "$bin" validate-evidence \
        --schema schema/evidence.schema.json \
        --meta-schema schema/run-meta.schema.json \
        "$crate_tests/e2e/testdata/expected"

    local py=${PYTHON:-python3}

    # 2. the ADR↔contract checkers docs/testing.md names as CI's: no python packages needed,
    #    milliseconds each, and they fail when an accepted ADR and the artifact implementing it
    #    stop agreeing with one another. validate_tool_contract.py belongs here rather than
    #    with the jsonschema checks below because the half of it that matters most — ADR-0001's
    #    error-code and status tables, the schema, and the fixture index in
    #    crates/act-mcp/tests/e2e/testdata/README.md — needs no package at all; it skips its own
    #    fixture-validating half, and says so, when jsonschema is absent.
    #    validate_crate_layout.py is the odd one out and belongs here anyway: it reads
    #    Cargo.toml/Cargo.lock instead of schema/*.json, because ADR-0005's artifacts *are*
    #    manifests, and it is what stops the act-mcp ↔ fathom dependency edge from creeping in.
    #    validate_fathom_layout.py is the other one: ADR-0006's artifacts are a directory contract
    #    and an ignore file, so it diffs .gitignore, .fathom/LAYOUT.md and the ADR against each other
    #    and fails on an orphaned whitelist entry (the state this repo was in when ADR-0006 was written).
    #    validate_mi_definition.py is the third one of that kind: ADR-0007's artifacts are numbers and
    #    closed enumerations, so it diffs the ADR's constants block against schema/mi-definition.json,
    #    re-derives every golden vector in tests/testdata/mi twice (binary64 through
    #    scripts/mi_reference.py, then 40-digit decimal in the checker itself), and fails if the
    #    tree-sitter ban that the ADR's front-end decision depends on has been lifted. No third-party
    #    package, no cargo, milliseconds.
    #    validate_metric_model.py is the fourth: ADR-0008's artifacts are a selection policy and an
    #    aggregation, so it diffs the ADR's pinned blocks against schema/metric-model.json, re-derives
    #    every golden vector in tests/testdata/metrics through scripts/metric_model_reference.py (the
    #    oracle mact-by1.6 is graded against), and cross-checks that implementation against the *jq
    #    recipes' recorded output* in docs/evidence-format.md — two languages, one answer, or the build
    #    says which record each side picked.
    #    validate_fathom_docs.py is the fifth, and the only one that checks prose: the Fathom runbook,
    #    docs/fathom-metrics.md and .fathom/LAYOUT.md must describe the tool that ships, so it asks
    #    `fathom --help` (five invocations: bare and each subcommand) what the argv actually is and fails
    #    when a flag, a help-overlay key or an exit code is undocumented, documented in two pages, or
    #    named in a page and absent from the binary. Hence the fathom build here and not just act-mcp:
    #    the checker reads the binary, not clap's source.
    echo "--- ADR <-> schema checkers ($py)"
    "$py" scripts/validate_tool_contract.py
    "$py" scripts/validate_run_lifecycle.py
    "$py" scripts/validate_isolation.py
    "$py" scripts/validate_crate_layout.py
    "$py" scripts/validate_fathom_layout.py
    "$py" scripts/validate_mi_definition.py
    "$py" scripts/validate_metric_model.py
    cargo build -p fathom --bin fathom
    local fbin
    if [[ -n "${CARGO_BUILD_TARGET:-}" ]]; then
        fbin="$target_dir/$CARGO_BUILD_TARGET/debug/fathom"
    else
        fbin="$target_dir/debug/fathom"
    fi
    if [[ ! -x "$fbin" ]]; then
        echo "schema: expected a built fathom at $fbin and did not find one" >&2
        return 1
    fi
    FATHOM_BIN="$fbin" "$py" scripts/validate_fathom_docs.py

    # 3. python's jsonschema as a second, independent implementation of the same contract
    #    (ADR-0002 §7 wants a disagreement between two engines to show up here rather than
    #    as a client bug report). Skipped loudly when the package is missing: these two are
    #    second opinions, and the hard checks above are the verdict.
    if ! "$py" -c 'import jsonschema' >/dev/null 2>&1; then
        note "$py has no 'jsonschema' package: skipping the two second-opinion checks below"
        note "and the fixture-validating half of validate_tool_contract.py above."
        note "the hard checks still ran; install it with scripts/install_ci_tools.sh"
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
        "$crate_tests/e2e/testdata/expected"
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

    # The two promises deny.toml cannot make, because both are about the shape of the graph rather
    # than about which crates are in it: that ADR-0005's forbidden edges stay absent *transitively*
    # (a manifest checker cannot see `fathom` acquiring `schemars` through `act-evidence`'s
    # `json-schema` feature — the manifest says only `act-evidence = { workspace = true }`), and that
    # nothing in the resolved graph of a shipped target invokes a C compiler, which three ADRs
    # (0003's libgit2, 0006 §5's sqlite, 0007 §6's tree-sitter) each refused for that reason. Named a
    # step of its own rather than folded in because its failure message names a crate and a fix.
    echo "--- dependency firewalls (scripts/check_dependency_firewalls.sh)"
    "$script_dir/check_dependency_firewalls.sh"
}

step_release_workspace() {
    # The release profile over every member, before any artefact is packaged. Optimisation changes
    # what compiles (a `cfg(not(debug_assertions))` arm, a const-eval loop that only blows up
    # unoptimised), and this is the job that ships binaries, so it is the job that pays for knowing.
    cargo build --release --workspace
    local artefact bin
    for artefact in "${ARTEFACTS_ALL[@]}"; do
        bin=$(release_bin "$artefact")
        if [[ ! -x "$bin" ]]; then
            echo "release-workspace: --release --workspace is green but there is no $bin" >&2
            return 1
        fi
        echo "--- built $bin"
    done
}

step_release() {
    local build_target=()
    if [[ -n "${CARGO_BUILD_TARGET:-}" ]]; then
        build_target=(--target "$CARGO_BUILD_TARGET")
    fi

    # One artefact at a time, server first. Each is `-p <crate> --bin <bin>`, never `--workspace`
    # (ADR-0005 §3): the tarball is one renamed binary, and `cd.yml` runs this step once per artefact
    # with `--artefact` so that a `fathom` which will not link cannot hold up the server that already
    # built, smoked and uploaded. `release-workspace` above is where the whole workspace is built in
    # release; this step is where a binary is *proved runnable*, which is the half `cargo build` stops
    # short of — the linker being happy is not the same fact as `--version`, `--help`, a refused flag
    # and the embedded data still being in the image.
    local artefact bin report_dir
    for artefact in "${ARTEFACTS[@]}"; do
        bin=$(release_bin "$artefact")
        echo "--- cargo build --release -p $artefact --bin $artefact"
        cargo build --release -p "$artefact" --bin "$artefact" ${build_target[@]+"${build_target[@]}"}

        # Where the smoke scripts drop --version/sha256 for the artifact steps to upload. One
        # directory per artefact, because two binaries share one `release/` directory and one
        # `dist/` would be one artefact's sha256 quietly overwritten by the other's.
        report_dir=$(dirname "$bin")/dist/$artefact
        case "$artefact" in
            act-mcp)
                "$script_dir/smoke_binary.sh" --report "$report_dir" \
                    --fixture "$crate_tests/e2e/testdata/expected" "$bin"
                ;;
            fathom)
                # `--check all`: the artefact answers its argv *and* keeps the headless exit-code
                # contract on a tree a real act run produced. A release image that prints a table and
                # exits 0 when its gate found something is worse than no gate.
                "$script_dir/smoke_fathom.sh" --check all --report "$report_dir" "$bin"
                ;;
        esac
    done
}

# What each step wants, printed when it fails so the log answers "now what".
step_fix_hint() {
    case "$1" in
        fmt)          echo "cargo fmt --all" ;;
        clippy)       echo "cargo clippy --all-targets --all-features, then read the lints; suppress one only with a reason" ;;
        build)        echo "cargo build --workspace --all-features; both act-mcp and fathom must produce a binary" ;;
        doc)          echo "the failing link is named above; point it at a real path or make it a code span" ;;
        test)         echo "cargo test --all-features -- --nocapture <the failing test>; docs/testing.md explains the suites" ;;
        tui-snapshot) echo "cargo test -p fathom --test tui_render; a mismatch is a screen you changed on purpose or a bug in the screen — read the diff, never re-record blind" ;;
        fathom-report) echo "cargo test -p fathom --test report, then scripts/smoke_fathom.sh --check report <bin>; FATHOM_UPDATE_GOLDEN=1 rewrites the goldens and you must read what it wrote" ;;
        fathom-doctor) echo "scripts/smoke_fathom.sh --check doctor <bin> prints every check and the code it derived from them; crates/fathom/src/exit.rs is the table" ;;
        schema)       echo "act-mcp validate-evidence <path> prints every finding; ADR-0002 §7 is the contract" ;;
        supply-chain) echo "scripts/install_ci_tools.sh installs cargo-deny; deny.toml holds the policy, and scripts/check_dependency_firewalls.sh --help says which firewall broke" ;;
        release-workspace) echo "cargo build --release --workspace" ;;
        release)      echo "each binary must run --help and --version; scripts/smoke_binary.sh <path> and scripts/smoke_fathom.sh <path> reproduce them alone" ;;
        *)            echo "" ;;
    esac
}


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
    # `-q` means "do not make me read this on a green run", not "throw the evidence away". In quiet mode
    # the step's output goes to a temporary file and is printed when — and only when — the step fails. A
    # quiet log that says `test FAILED (11s)` and nothing else sends someone into a 65-second rerun to
    # find one test name, which is how a flake stays a mystery forever.
    step_log=""
    if [[ "$QUIET" == 1 ]]; then
        step_log=$(mktemp 2>/dev/null || true)
        if [[ -n "$step_log" ]]; then
            ( set -e; "$step_fn" ) >"$step_log" 2>&1
            status=$?
        else
            # No scratch file, no capture: the old discard-everything behaviour beats no build at all.
            ( set -e; "$step_fn" ) >/dev/null 2>&1
            status=$?
        fi
    else
        ( set -e; "$step_fn" )
        status=$?
    fi
    set -e
    elapsed=$(( $(date +%s) - step_start ))

    if [[ $status -eq 0 ]]; then
        summary_status+=("pass")
        summary_line+=("$(printf '%-18s %6ss' "$step" "$elapsed")")
        if [[ "$QUIET" == 0 ]]; then
            printf '%s  %s passed%s (%ss)\n' "$c_green" "$step" "$c_off" "$elapsed"
        fi
    else
        failed=1
        summary_status+=("fail")
        summary_line+=("$(printf '%-18s %6ss' "$step" "$elapsed")")
        printf '%s  %s FAILED%s (%ss, exit %s)\n' "$c_red" "$step" "$c_off" "$elapsed" "$status"
        hint=$(step_fix_hint "$step")
        if [[ -n "$hint" ]]; then
            fix "$hint"
        fi
        # The evidence last: the step header and the fix line are what a reader acts on, and the output
        # is what they then want. Tail-first, because a cargo or python failure ends with the thing that
        # failed.
        if [[ -n "$step_log" && -s "$step_log" ]]; then
            printf '  ---- %s output, last 200 of %s lines ----\n' "$step" "$(wc -l <"$step_log")"
            tail -n 200 "$step_log"
            printf '  ---- end of %s output ----\n' "$step"
        fi
        if [[ -n "$in_gh_actions" ]]; then
            printf '::error::ci step %s failed\n' "$step"
        fi
    fi
    if [[ -n "$step_log" ]]; then
        rm -f "$step_log"
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
