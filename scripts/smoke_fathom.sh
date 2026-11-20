#!/usr/bin/env bash
#
# scripts/smoke_fathom.sh — prove a built fathom binary is a runnable binary, and that the two
# headless commands it ships are the ones whose exit codes CI depends on.
#
# `scripts/smoke_binary.sh` is that proof for the server: five things about `act-mcp`, checked by
# running the image rather than compiling it. This is the same argument for `fathom` (mact-by1.16),
# with a second half the server does not need. `fathom` is the crate whose whole job is stdout, so
# "the binary runs" is not only "it answers --help": the two subcommands written *for* a CI step —
# `report` and `doctor` — have to keep the 0/1/2 promise `crates/fathom/src/exit.rs` makes, on a
# tree a real `act` run produced, in the shipped image and not only under `cargo test`.
#
# Three checks, selected with --check:
#
#   artefact   --version says this tree's version (and propagates to a subcommand), --help names
#              every subcommand, a bad flag is refused. What `smoke_binary.sh` does for the server,
#              and what the release job runs on an artefact it is about to publish.
#   report     `fathom report --format json` over `scripts/sample_evidence.py`'s tree: exit 0, the
#              envelope `crates/fathom/tests/golden/report.json` has, an index that lands, a tsv
#              header that does not drift, and `--fail-on` exiting 1 exactly when the gate says
#              `findings`.
#   doctor     `fathom doctor --json`: the exit code is its own worst check (0 ok, 1 warn, 2 fail),
#              every documented check is present, and a directory that is not a work tree is refused
#              with 2 — "could not run", not "nothing to report".
#
# The Rust tests are the precise version of all of this; this script is the version that runs the
# artefact. `cargo test` never loads the linked image, so a binary that builds and cannot run — a
# bad interpreter path, a stripped `include_str!`, a musl image linked against a host libc — is
# caught here and nowhere else.
#
# Usage:
#   scripts/smoke_fathom.sh [OPTIONS] [path-to-binary]
#
# The binary is what is under test. Name it, or leave it off and take the one cargo just built —
# `$CARGO_TARGET_DIR`/`target/debug/fathom`, which is where scripts/ci.sh built it a line earlier —
# because a step printed in docs/ci.md is a command someone copies, and `--check doctor` with no path
# used to answer a usage block and exit 2 with nothing missing but the obvious argument.
#
# Options:
#   -h, --help            this text
#   -q, --quiet           only the verdict
#   --check NAME          artefact (default) | report | doctor | all
#   --report DIR          write DIR/version.txt and DIR/sha256.txt, for CI to attach; also
#                         appends a row to $GITHUB_STEP_SUMMARY when that is set
#   --expect-version VER  require --version to report VER (default: [workspace.package]'s)
#   --evidence DIR        the evidence tree for the report/doctor checks (default: a fresh one
#                         built by scripts/sample_evidence.py in a temporary directory)
#   --cache DIR           the cache root to use (default: a temporary one; it is disposable by
#                         contract, ADR-0006 §3, so this script never reuses yours)
#   --workspace DIR       the workspace to point at (default: this repository)
#
# Exit status: 0 everything checked out, 1 a check failed, 2 usage/environment error.

set -o errexit -o nounset -o pipefail

usage() {
    cat <<'USAGE'
Usage: scripts/smoke_fathom.sh [OPTIONS] [path-to-binary]

Prove a built fathom binary is a runnable binary: it exists, answers --version with the version
this tree declares, answers --help and names all four subcommands, refuses an unknown flag, and —
with --check report and --check doctor — keeps the headless exit-code contract (0 clean, 1
findings, 2 could not run) on a real evidence tree.

With no path, the binary is $CARGO_TARGET_DIR (else this repository's target)/debug/fathom — the
one scripts/ci.sh just built — and a missing default says how to build it instead of printing this.

Options:
  -h, --help            this text
  -q, --quiet           only the verdict
  --check NAME          artefact (default) | report | doctor | all
  --report DIR          write DIR/version.txt and DIR/sha256.txt, for CI to attach;
                        also appends a row to $GITHUB_STEP_SUMMARY when it is set
  --expect-version VER  require --version to report VER (default: [workspace.package]'s)
  --evidence DIR        evidence tree for report/doctor (default: scripts/sample_evidence.py's)
  --cache DIR           cache root to use (default: a temporary one)
  --workspace DIR       the workspace to point at (default: this repository)

Exit status: 0 everything checked out, 1 a check failed, 2 usage/environment error.
USAGE
}

quiet=0
check=artefact
report_dir=""
expect_version=""
evidence=""
cache_dir=""
workspace=""
workspace_explicit=0
binary=""
binary_defaulted=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        -h | --help)
            usage
            exit 0
            ;;
        -q | --quiet) quiet=1 ;;
        --check)
            check=${2:?"--check needs a name (artefact, report, doctor or all)"}
            case "$check" in
                artefact | report | doctor | all) ;;
                *)
                    echo "smoke_fathom: unknown --check '$check' (try --help)" >&2
                    exit 2
                    ;;
            esac
            shift
            ;;
        --report) report_dir=${2:?"--report needs a directory"}; shift ;;
        --expect-version) expect_version=${2:?"--expect-version needs a version"}; shift ;;
        --evidence) evidence=${2:?"--evidence needs a directory"}; shift ;;
        --cache) cache_dir=${2:?"--cache needs a directory"}; shift ;;
        --workspace)
            workspace=${2:?"--workspace needs a directory"}
            workspace_explicit=1
            shift
            ;;
        -*)
            echo "smoke_fathom: unknown option '$1' (try --help)" >&2
            exit 2
            ;;
        *)
            if [[ -n "$binary" ]]; then
                echo "smoke_fathom: more than one binary given ('$binary' and '$1')" >&2
                exit 2
            fi
            binary=$1
            ;;
    esac
    shift
done

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd -- "$script_dir/.." && pwd)
py=${PYTHON:-python3}

if [[ -z "$binary" ]]; then
    # The default is the point: `fathom-doctor` is documented as `scripts/smoke_fathom.sh --check
    # doctor`, a line a person pastes, and demanding the path made the documented form exit 2.
    binary=${CARGO_TARGET_DIR:-$repo_root/target}/debug/fathom
    binary_defaulted=1
    if [[ ! -e "$binary" ]]; then
        printf 'smoke_fathom: no binary named, and the default is not there: %s\n  fix: cargo build -p fathom --bin fathom, or name the binary to test as a positional argument\n' \
            "$binary" >&2
        exit 2
    fi
fi

say() { [[ "$quiet" == 1 ]] || printf '  %s\n' "$*"; }
fail() { printf 'smoke_fathom: %s\n' "$*" >&2; exit 1; }
note() { printf '  note: %s\n' "$*"; }

# Temporary everything: the sample tree, the cache root, and every file the checks capture. The
# cache root is disposable by ADR-0006 §3, and this script has no business writing into the
# workspace's own `.fathom` or into a developer's evidence root.
work=$(mktemp -d "${TMPDIR:-/tmp}/fathom-smoke.XXXXXX")
trap 'rm -rf "$work"' EXIT
[[ -n "$cache_dir" ]] || cache_dir=$work/cache
[[ -n "$workspace" ]] || workspace=$repo_root

# ---------------------------------------------------------------------------
# 0. there is a binary, and it is executable
# ---------------------------------------------------------------------------
[[ -f "$binary" ]] || fail "no file at $binary (build it first: cargo build -p fathom --bin fathom)"
[[ -x "$binary" ]] || fail "$binary is not executable"
size=$(wc -c < "$binary" | tr -d ' ')
if [[ "$size" -lt 100000 ]]; then
    fail "$binary is $size bytes: too small to be a linked fathom"
fi
say "binary   $binary ($size bytes)"
if [[ "$binary_defaulted" == 1 ]]; then
    say "         defaulted from \${CARGO_TARGET_DIR:-$repo_root/target}; name a path to test another build"
fi

# ---------------------------------------------------------------------------
# 1. --version, and it is the version this tree says it is
# ---------------------------------------------------------------------------
# One version for one repo (ADR-0005 §8): the root manifest's [workspace.package] is where the
# version is written, `cd.yml` sets it once per release with scripts/set_version.py, and every
# member inherits it — so this is the same expectation smoke_binary.sh has of act-mcp, and a
# fathom that reports a different version than the server is the drift that ADR forbids.
if [[ -z "$expect_version" ]]; then
    expect_version=$(sed -n '/^\[workspace\.package\]/,/^\[/p' "$repo_root/Cargo.toml" \
        | sed -n 's/^version *= *"\([^"]*\)".*/\1/p' | head -1)
    if [[ -z "$expect_version" ]]; then
        fail "no version found: no [workspace.package] version in $repo_root/Cargo.toml"
    fi
fi
version_output=$("$binary" --version 2>&1 | head -1) || fail "--version exited non-zero: $version_output"
if [[ "$version_output" != "fathom $expect_version" ]]; then
    fail "--version reported '$version_output', expected 'fathom $expect_version'"
fi
say "--version  $version_output"

# ---------------------------------------------------------------------------
# 2. --help works and names the surface the runbook documents
# ---------------------------------------------------------------------------
# The four subcommands are the whole argv surface `scripts/validate_fathom_docs.py` diffs the
# runbook against; a fifth appearing here is a surface nobody documented, and one disappearing is a
# documented flag the binary refuses. Either way the checker in `schema` is the verdict; this only
# proves the shipped image agrees with it.
help_output=$("$binary" --help 2>&1) || fail "--help exited non-zero: $help_output"
for needle in tui index report doctor; do
    if ! grep -q -- "$needle" <<<"$help_output"; then
        fail "--help does not mention the '$needle' subcommand"
    fi
done
say "--help     $(wc -l <<<"$help_output" | tr -d ' ') lines, names tui, index, report and doctor"

# `--version` on a subcommand is how clap's `propagate_version` shows up; `report` is the one a CI
# step is most likely to be written against, so that is the one checked.
if ! "$binary" report --version >/dev/null 2>&1; then
    fail "report --version did not work (is propagate_version still set?)"
fi
for subcommand in tui index report doctor; do
    "$binary" "$subcommand" --help >/dev/null 2>&1 || fail "$subcommand --help exited non-zero"
done

# ---------------------------------------------------------------------------
# 3. a bad flag is refused, not ignored
# ---------------------------------------------------------------------------
if "$binary" --definitely-not-a-flag >/dev/null 2>&1; then
    fail "an unknown flag was accepted: argument parsing is not validating anything"
fi
code=0
"$binary" report --definitely-not-a-flag >/dev/null 2>&1 || code=$?
if [[ "$code" != 2 ]]; then
    fail "report with an unknown flag exited $code, expected clap's 2"
fi
say "bad flag   refused as it must be (2, clap's own code)"

# ---------------------------------------------------------------------------
# the evidence tree the headless checks read
# ---------------------------------------------------------------------------
# `scripts/sample_evidence.py` lays the checked-in e2e goldens out as a three-commit `.evidence`
# tree — records a real `act` run wrote, re-stamped onto synthetic commits. It is the same tree
# the report golden is built from, so this smoke and that golden ask the same question of the same
# bytes; only this one asks it of the linked image rather than of the library.
evidence_ready=0
prepare_evidence() {
    [[ "$evidence_ready" == 1 ]] && return 0
    evidence_ready=1
    if [[ -z "$evidence" ]]; then
        evidence=$("$script_dir/sample_evidence.py" -o "$work/evidence" --quiet) \
            || fail "scripts/sample_evidence.py could not lay a tree (see its message above)"
        [[ -d "$evidence" ]] || fail "sample_evidence.py reported '$evidence', which is not a directory"
    fi
    [[ -d "$evidence" ]] || fail "--evidence $evidence is not a directory"
    say "evidence $evidence"
}

# The workspace `report` and `doctor` read. fathom is a reader of *history*: with no work tree there
# is nothing to report, and a smoke that reported "0 commits, exit 0" would be a smoke of nothing. So
# an explicit --workspace has to be a work tree, and an implicit one falls back to a scratch repository
# of one empty commit when this script is run outside a checkout (a copied tree in scripts/ci_drill.sh,
# an unpacked release tarball) rather than silently proving nothing.
workspace_ready=0
prepare_workspace() {
    [[ "$workspace_ready" == 1 ]] && return 0
    workspace_ready=1
    if git -C "$workspace" rev-parse --git-dir >/dev/null 2>&1; then
        say "workspace $workspace"
        return 0
    fi
    if [[ "$workspace_explicit" == 1 ]]; then
        fail "--workspace $workspace is not a git work tree; fathom reports on history, so it must be one"
    fi
    command -v git >/dev/null 2>&1 || fail "no git on PATH, and no work tree to point fathom at"
    workspace=$work/workspace
    mkdir -p "$workspace"
    git -C "$workspace" init -q --initial-branch=main "$workspace" >/dev/null 2>&1 \
        || git -C "$workspace" init -q "$workspace" >/dev/null 2>&1 \
        || fail "could not create a scratch work tree in $workspace"
    git -C "$workspace" -c user.name='fathom smoke' -c user.email='fathom-smoke@example.invalid' \
        commit -q --allow-empty -m 'fathom smoke workspace' \
        || fail "could not commit in the scratch work tree"
    note "no work tree above $script_dir; using a scratch one at $workspace"
    say "workspace $workspace (scratch)"
}

# Run the binary with the roots pinned, capture stdout, and hand back the exit code. `report` and
# `doctor` are the two subcommands whose *code* is the contract, so nothing here may use `|| true`
# and then guess.
run_fathom() {
    local out=$1
    shift
    local code=0
    "$binary" "$@" --evidence-dir "$evidence" --cache-dir "$cache_dir" "$workspace" >"$out" 2>"$out.err" || code=$?
    return "$code"
}

# ---------------------------------------------------------------------------
# 4. `fathom report --format json`: the envelope, and the gate's exit code
# ---------------------------------------------------------------------------
check_report_surface() {
    prepare_evidence
    prepare_workspace
    mkdir -p "$cache_dir"

    local out=$work/report.json
    local code=0
    run_fathom "$out" report --format json || code=$?
    if [[ "$code" != 0 ]]; then
        fail "report --format json exited $code on a valid tree (expected 0): $(tail -3 "$out.err")"
    fi
    # The envelope `crates/fathom/tests/golden/report.json` pins, restated here as a *shape* so a
    # renamed field fails here too — but this check is deliberately weaker than the golden: the
    # commits in this workspace are this repository's, not the fixture's, so only the envelope is
    # comparable. The numbers are the golden's job.
    "$py" - "$out" "$expect_version" <<'PY' || fail "the report envelope is not what the golden envelope is"
import json
import sys

path, version = sys.argv[1], sys.argv[2]
document = json.loads(open(path, encoding="utf-8").read())

def need(cond, what):
    if not cond:
        raise SystemExit(f"envelope: {what}")

need(document.get("schema_version") == 1, "schema_version is not 1 (ADR-0006 §10.1 froze it)")
need(document.get("fathom_version") == version, f"fathom_version is not {version!r}")
need(isinstance(document.get("workspace"), str) and document["workspace"], "workspace is empty")
for section in ("window", "store", "counts", "gate"):
    need(isinstance(document.get(section), dict), f"'{section}' is missing or not an object")
need(isinstance(document.get("commits"), list), "'commits' is missing or not a list")
need(document["counts"].get("commits") == len(document["commits"]),
     "counts.commits disagrees with the rows in commits[]")
need(document["gate"].get("fail_on") == [], "gate.fail_on is not empty for a run with no --fail-on")
for row in document["commits"]:
    for key in ("sha", "short", "subject", "indexed", "action", "coverage", "churn", "mi", "bench",
                "display", "gaps", "caveats"):
        need(key in row, f"commit {row.get('short', '?')} has no '{key}'")
    for cell in ("coverage", "churn", "mi", "bench", "action"):
        need(cell in row["display"], f"commit {row.get('short', '?')} has no display.{cell}")
print(f"  envelope   {len(document['commits'])} commit(s), store.state={document['store']['state']}, "
      f"counts={document['counts']['with_evidence']} with evidence")
PY

    # The index is the only write path, and a report whose store stayed `absent` would still exit 0
    # (every cell unknown is an honest answer) — so prove the write happened instead of assuming it.
    local code=0
    run_fathom "$work/index.out" index --limit 5 || code=$?
    if [[ "$code" != 0 ]]; then
        fail "fathom index exited $code on a sample tree (expected 0): $(tail -3 "$work/index.out")"
    fi
    code=0
    run_fathom "$out" report --format json || code=$?
    if [[ "$code" != 0 ]]; then
        fail "report --format json exited $code after an index (expected 0): $(tail -3 "$out.err")"
    fi
    local state
    state=$("$py" -c 'import json,sys; print(json.load(open(sys.argv[1]))["store"]["state"])' "$out") \
        || fail "could not read store.state back from $out"
    if [[ "$state" == "absent" ]]; then
        fail "the index ran and store.state is still 'absent': the cache root the report reads " \
            "($cache_dir) is not the one the index wrote"
    fi
    say "index      store.state=$state after \`fathom index --limit 5\`"

    # The flat formats: the header is a contract, because a downstream `cut -f` is written against
    # it. Same columns `fathom tui --plain` prints — ADR-0006 §10.1 forbids a second formatter.
    code=0
    run_fathom "$work/report.tsv" report --format tsv || code=$?
    if [[ "$code" != 0 ]]; then
        fail "report --format tsv exited $code on a valid tree (expected 0)"
    fi
    local header want
    header=$("$py" - "$work/report.tsv" <<'PY'
import sys

print(open(sys.argv[1], encoding="utf-8").readline().rstrip("\n"))
PY
    )
    want=$'sha\tage\tmessage\tcoverage\tchurn\tmi\tbench\taction\tindexed\tgaps'
    if [[ "$header" != "$want" ]]; then
        fail "report --format tsv header is '$header', expected '$want'"
    fi
    say "tsv        header is the timeline's own columns"

    # The gate: `--fail-on` exits 1 when it found something and 0 when it did not, and the only
    # honest way to test that on a tree this script does not control is to ask the report what it
    # thinks it found and require the code to agree. Both answers come from one run.
    for gate in coverage-drop mi-drop benchmark-regression action-failure; do
        code=0
        run_fathom "$work/gate.json" report --format json --fail-on "$gate" || code=$?
        if [[ "$code" != 0 && "$code" != 1 ]]; then
            fail "--fail-on $gate exited $code; the gate's codes are 0 (clean) and 1 (findings) only"
        fi
        "$py" - "$work/gate.json" "$gate" "$code" <<'PY' || fail "--fail-on disagreed with its own gate"
import json
import sys

path, gate, code = sys.argv[1], sys.argv[2], int(sys.argv[3])
document = json.load(open(path, encoding="utf-8"))
status = document["gate"]["status"]
findings = document["gate"]["findings"]
if status not in ("off", "clean", "findings"):
    raise SystemExit(f"--fail-on {gate}: gate.status is {status!r}, not off/clean/findings")
if (code == 1) != (status == "findings"):
    raise SystemExit(
        f"--fail-on {gate}: exit {code} but gate.status is {status!r} "
        f"({len(findings)} finding(s)) — the gate and the exit code disagree"
    )
PY
        say "gate       --fail-on $gate → exit $code (gate.status $("$py" -c \
            'import json,sys; print(json.load(open(sys.argv[1]))["gate"]["status"])' "$work/gate.json"))"
    done
}

# ---------------------------------------------------------------------------
# 5. `fathom doctor`: the exit code is the worst check, never a guess
# ---------------------------------------------------------------------------
check_doctor_surface() {
    prepare_evidence
    prepare_workspace
    mkdir -p "$cache_dir"

    local out=$work/doctor.json
    local code=0
    run_fathom "$out" doctor --json || code=$?
    case "$code" in
        0 | 1 | 2) ;;
        *) fail "fathom doctor --json exited $code; its contract is 0 (ok), 1 (warn), 2 (fail)" ;;
    esac
    # Two independent halves of one promise: every documented check answered, and the process code
    # derived from the checks rather than from the boot path that happened to be reached. What the
    # checks *say* about this repository is nobody's golden — a runner without act-mcp on it, or a
    # workspace whose workflows publish no evidence, is a legitimate `warn`.
    "$py" - "$out" "$code" <<'PY' || fail "doctor's exit code is not its worst check"
import json
import sys

path, code = sys.argv[1], int(sys.argv[2])
document = json.load(open(path, encoding="utf-8"))
rank = {"ok": 0, "warn": 1, "fail": 2}
# Every check `fathom doctor` documents — `crates/fathom/src/doctor.rs`'s list, and the one the
# runbook's doctor section describes. Extra checks are allowed; a missing one is a silent gap.
documented = ("workspace", "evidence", "runs", "workflows", "cache", "languages", "config")
present = {check["name"] for check in document["checks"]}
missing = [name for name in documented if name not in present]
if missing:
    raise SystemExit(f"doctor reported no check named: {', '.join(missing)}")
worst = max(rank[check["status"]] for check in document["checks"])
if worst != code:
    raise SystemExit(
        f"doctor exited {code} but its worst check is "
        f"{[n for n, r in ((c['name'], rank[c['status']]) for c in document['checks']) if r == worst][0]}"
    )
if document["status"] not in ("ok", "warn", "fail"):
    raise SystemExit(f"doctor status is {document['status']!r}")
PY
    local tally
    tally=$("$py" - "$out" <<'PY'
import json
import sys

document = json.load(open(sys.argv[1], encoding="utf-8"))
print(" ".join(f"{c['name']}={c['status']}" for c in document["checks"]))
PY
    )
    say "doctor     exit $code is its worst check ($tally)"

    # A directory that is not a work tree is "could not run", which is 2 — the code that says a
    # pipeline cannot proceed — and never 0, which would make an unreadable workspace look clean.
    local notrepo=$work/not-a-workspace
    mkdir -p "$notrepo"
    code=0
    "$binary" doctor --json --evidence-dir "$evidence" --cache-dir "$cache_dir" "$notrepo" \
        >"$work/notrepo.json" 2>"$work/notrepo.err" || code=$?
    if [[ "$code" != 2 ]]; then
        fail "doctor on a non-workspace exited $code, expected 2 (could not run)"
    fi
    "$py" - "$work/notrepo.json" <<'PY' || fail "doctor's refusal on a non-workspace is not reported as a failure"
import json
import sys

document = json.load(open(sys.argv[1], encoding="utf-8"))
if document["status"] != "fail":
    raise SystemExit(f"status is {document['status']!r} on a directory that is not a work tree")
if not any(check["name"] == "workspace" and check["status"] == "fail" for check in document["checks"]):
    raise SystemExit("no failing 'workspace' check, so the refusal came from somewhere else")
PY
    say "doctor     a directory that is not a work tree is exit 2, with a failing 'workspace' check"
}

if [[ "$check" == report || "$check" == all ]]; then
    check_report_surface
fi
if [[ "$check" == doctor || "$check" == all ]]; then
    check_doctor_surface
fi

# ---------------------------------------------------------------------------
# 6. report for CI (version + digest), attached where CI can see it
# ---------------------------------------------------------------------------
digest=$(sha256sum "$binary" 2>/dev/null | cut -d' ' -f1 || shasum -a 256 "$binary" | cut -d' ' -f1)
if [[ -n "$report_dir" ]]; then
    mkdir -p "$report_dir"
    printf '%s\n' "$version_output" > "$report_dir/version.txt"
    printf '%s  %s\n' "$digest" "$(basename "$binary")" > "$report_dir/sha256.txt"
    say "sha256     $digest"
    say "report     $report_dir/{version.txt,sha256.txt}"
fi
if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
    {
        printf '| fathom | `%s` | %s bytes | `%s` |\n' \
            "$version_output" "$size" "${digest:0:16}…"
    } >> "$GITHUB_STEP_SUMMARY"
fi

exit 0
