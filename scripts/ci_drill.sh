#!/usr/bin/env bash
#
# scripts/ci_drill.sh — prove that every check in scripts/ci.sh can actually fail.
#
# A CI step that always passes is worse than no step: it buys a false sense of "covered".
# So each check here gets a deliberate violation injected into a throwaway copy of the
# tree, and the drill asserts that the corresponding `scripts/ci.sh` step exits non-zero
# on it. If someone "fixes" a red build by silencing a lint repo-wide, deleting a schema
# check, or pointing a step at the wrong file, this is where it shows up.
#
# Nothing is modified in the real working tree: every drill copies Cargo.toml, Cargo.lock,
# rust-toolchain.toml, deny.toml, schema/, scripts/, crates/ and docs/ into a temporary
# directory, breaks the copy, and runs the step there. Two consequences worth knowing when a drill
# goes red: the copy is not a git work tree (copying `.git` would be slow, and a drill reading the
# real repository's history would not be a drill), so `scripts/smoke_fathom.sh` falls back to a
# scratch work tree of its own — which is what makes the fathom-report and fathom-doctor drills mean
# something, because their checks pass in the copy until the injected violation breaks one.
#
# Usage:
#   scripts/ci_drill.sh                every drill
#   scripts/ci_drill.sh --list         list the drills
#   scripts/ci_drill.sh --step schema  the drills whose name starts with a step
#   scripts/ci_drill.sh -v             keep the logs and print them
#
# The drill compiles the crate as many times as there are drills, in its own target
# directory (default: $TMPDIR/act-mcp-drill-target, kept between runs). Do not point it at
# the target directory your working tree uses.
#
# Exit status: 0 every violation was caught, 1 something was not, 2 usage error.

set -o errexit -o nounset -o pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd -- "$script_dir/.." && pwd)

usage() {
    sed -n '/^# scripts\/ci_drill.sh/,/^set -o/p' "$0" | grep '^#' | sed 's/^# \{0,1\}//' | head -n -1
}

# ---------------------------------------------------------------------------
# drills: each one is "break X in a copy, then expect ci.sh <step> to fail"
# ---------------------------------------------------------------------------
DRILLS=(
    "fmt-misformatted-code:fmt"
    "clippy-lint:clippy"
    "doc-broken-intra-doc-link:doc"
    "doc-link-to-test-only-module:doc"
    "docs-site-broken-cross-page-link:docs-site"
    "test-failing-assertion:test"
    "tui-snapshot-snapshot-mismatch:tui-snapshot"
    "fathom-report-report-golden-drift:fathom-report"
    "fathom-report-flat-header-drift:fathom-report"
    "fathom-doctor-doctor-exit-code-lie:fathom-doctor"
    "schema-broken-schema-file:schema"
    "schema-bad-evidence-record:schema"
    "schema-undocumented-tool-io-fixture:schema"
    "supply-chain-disallowed-licence:supply-chain"
    "supply-chain-fathom-dependency-banned:supply-chain"
    "supply-chain-fathom-firewall-crossed:supply-chain"
)

drill_list() { printf '%s\n' "${DRILLS[@]}"; }

# Each mutation takes the copy's root as $1 and makes exactly one violation.
mut_fmt_misformatted_code() {
    cat >> "$1/crates/act-mcp/src/lib.rs" <<'RS'

/// Injected by scripts/ci_drill.sh: valid Rust that rustfmt will not accept.
pub fn ci_drill_fmt() -> u32 {
    let  drill = 41;
    drill+1
}
RS
}

mut_clippy_lint() {
    cat >> "$1/crates/act-mcp/src/lib.rs" <<'RS'

/// Injected by scripts/ci_drill.sh: compiles, but trips clippy::needless_return.
#[allow(dead_code)]
fn ci_drill_clippy() -> u32 {
    let value = 1;
    return value;
}
RS
}

mut_doc_broken_intra_doc_link() {
    cat >> "$1/crates/act-mcp/src/lib.rs" <<'RS'

/// Injected by scripts/ci_drill.sh: a link to something that does not exist, which is
/// exactly the rot this step exists to catch ([`ci_drill_no_such_item`]).
#[allow(dead_code)]
pub fn ci_drill_doc() -> u32 {
    1
}
RS
}

mut_doc_link_to_test_only_module() {
    # The subtler rot, and the one that actually happened (mact-b81): a doc comment that links
    # to an item which exists only under `#[cfg(test)]`. The code compiles, `cargo test` is
    # happy — rustdoc is the only tool that does not build cfg(test) code, so the doc step is
    # the only place the link can show up as dead. This drill is the proof that it stays an
    # error instead of quietly turning the sentence into a reference to nothing.
    cat >> "$1/crates/act-mcp/src/lib.rs" <<'RS'

/// Injected by scripts/ci_drill.sh: a link to a module `#[cfg(test)]` hides from rustdoc
/// ([`ci_drill_test_only_module`]).
#[allow(dead_code)]
pub fn ci_drill_doc_test_only() -> u32 {
    1
}

#[cfg(test)]
mod ci_drill_test_only_module {
    #[allow(dead_code)]
    pub fn pinned_by_tests() {}
}
RS
}

mut_docs_site_broken_cross_page_link() {
    # The Markdown mirror of the drill above, and the same class of rot: a link that reads correctly
    # and goes nowhere. It is a *section* link rather than a file link because the file exists, so
    # nothing but the anchor check can notice, which is exactly why the step exists.
    printf '\nSee [the section that is not there](adr/0001-tool-contract.md#the-heading-that-was-never-written).\n' \
        >>"$1/docs/ci.md"
}

mut_test_failing_assertion() {
    cat >> "$1/crates/act-mcp/src/lib.rs" <<'RS'

#[cfg(test)]
mod ci_drill_tests {
    #[test]
    fn ci_drill_must_fail() {
        assert!(false, "injected by scripts/ci_drill.sh");
    }
}
RS
}

mut_schema_broken_schema_file() {
    # Not a JSON syntax error (that would only prove it can read a file): a schema-shaped
    # document the 2020-12 engine refuses to compile.
    python3 - "$1/schema/evidence.schema.json" <<'PY'
import json
import sys

path = sys.argv[1]
schema = json.loads(open(path, encoding="utf-8").read())
schema["patternProperties"] = 42
open(path, "w", encoding="utf-8").write(json.dumps(schema, indent=2))
PY
}

mut_schema_bad_evidence_record() {
    # A record that satisfies nothing: a JSONL line missing the required envelope fields.
    printf '%s\n' '{"schema_version":1,"kind":"benchmark"}' \
        >> "$1/crates/act-mcp/tests/e2e/testdata/expected/benchmark.jsonl"
}

mut_schema_undocumented_tool_io_fixture() {
    # Delete the README row that names one tool-io fixture. The contract checker refuses an
    # undocumented fixture, because a contract rule nobody can look up is a rule that will be
    # broken by accident; this proves that refusal is fatal and not advice.
    python3 - "$1/crates/act-mcp/tests/e2e/testdata/README.md" <<'PY'
import sys

path = sys.argv[1]
text = open(path, encoding="utf-8").read()
lines = [line for line in text.splitlines(keepends=True)
         if "run_workflow.result.status-succeeded.json" not in line]
if len(lines) == len(text.splitlines(keepends=True)):
    sys.exit("drill setup: the fixture row this drill deletes is not in the README")
open(path, "w", encoding="utf-8").write("".join(lines))
PY
}

mut_supply_chain_disallowed_licence() {
    # Drop MIT from the allow list: the graph is full of MIT crates, so the licences check
    # must now refuse the build. Proves the check reads deny.toml and the graph, and that
    # it is fatal rather than informational.
    python3 - "$1/deny.toml" <<'PY'
import sys

path = sys.argv[1]
text = open(path, encoding="utf-8").read()
if '"MIT",' not in text:
    sys.exit("the drill expected MIT in deny.toml's allow list; the drill is out of date")
open(path, "w", encoding="utf-8").write(text.replace('    "MIT",\n', '', 1))
PY
}

# ---------------------------------------------------------------------------
# the fathom steps (mact-by1.16)
# ---------------------------------------------------------------------------
mut_tui_snapshot_snapshot_mismatch() {
    # One character inside a drawn golden, and only inside the golden: the fixture builds the subject
    # lower-case (`tui_render.rs`'s `populated()`), and the first copy of that subject in the file is
    # the expected screen. So the code still draws the timeline it always drew and the snapshot no
    # longer says what the screen says — which is the shape of a real snapshot failure, whether someone
    # widened a column by accident or is trying to slip a redesign past review.
    python3 - "$1/crates/fathom/tests/tui_render.rs" <<'PY'
import sys

path = sys.argv[1]
text = open(path, encoding="utf-8").read()
needle = "registry: sweep evicted r"
if needle not in text:
    sys.exit("drill setup: the timeline subject this drill edits is not in tui_render.rs")
# The expected screen is the first copy in the file; the fixture's own subject is further down.
open(path, "w", encoding="utf-8").write(text.replace(needle, "registry: sweep evicted R", 1))
PY
}

mut_fathom_report_report_golden_drift() {
    # A number in the report golden, nudged by one — the drift a downstream dashboard notices first
    # and CI should notice second: the envelope still parses, the field is still there and still an
    # integer, and it now describes something the tool did not compute. A surgical text edit, not a
    # json.load/dump: the golden is compared as text, so reformatting the file would make the drill
    # pass for a formatting reason instead of the semantic one.
    python3 - "$1/crates/fathom/tests/golden/report.json" <<'PY'
import re
import sys

path = sys.argv[1]
text = open(path, encoding="utf-8").read()
match = re.search(r'("with_evidence": )([0-9]+)', text)
if not match:
    sys.exit("drill setup: report.json has no counts.with_evidence to drift")
value = int(match.group(2))
open(path, "w", encoding="utf-8").write(
    text[:match.start()] + match.group(1) + str(value + 1) + text[match.end():]
)
PY
}

mut_fathom_report_flat_header_drift() {
    # The half the golden cannot see: the tsv header is a machine-readable contract (`cut -f4` is
    # written against it), and crates/fathom/tests/report.rs pins the *count* of columns and the last
    # two names, so renaming a middle column passes every Rust test and breaks every consumer.
    # `smoke_fathom.sh --check report` is the one that pins the whole header; this is its proof.
    python3 - "$1/crates/fathom/src/report.rs" <<'PY'
import sys

path = sys.argv[1]
text = open(path, encoding="utf-8").read()
needle = '"sha", "age", "message", "coverage", "churn",'
if needle not in text:
    sys.exit("drill setup: report::COLUMNS is not spelled the way this drill expects")
open(path, "w", encoding="utf-8").write(
    text.replace(needle, '"sha", "age", "message", "coverage", "churned",', 1)
)
PY
}

mut_fathom_doctor_doctor_exit_code_lie() {
    # `doctor`'s whole product is its exit code, so sever the code from the checks and require the
    # smoke to notice. Everything else still prints — same statuses, same fixes, same JSON — which is
    # what makes this a lie rather than a crash: the command now reports `warn` and exits 0, so a CI
    # step written as `fathom doctor || exit 1` would go green on a broken workspace.
    python3 - "$1/crates/fathom/src/doctor.rs" <<'PY'
import sys

path = sys.argv[1]
text = open(path, encoding="utf-8").read()
needle = "    pub fn exit(&self) -> Exit {\n        self.status.exit()\n    }"
if needle not in text:
    sys.exit("drill setup: doctor's Report::exit is no longer the one-line derivation this drill replaces")
open(path, "w", encoding="utf-8").write(
    text.replace(
        needle,
        "    pub fn exit(&self) -> Exit {\n        let _ = self;\n        Exit::Success\n    }",
        1,
    )
)
PY
}

# ---------------------------------------------------------------------------
# the fathom half of the supply-chain step (mact-by1.16)
# ---------------------------------------------------------------------------
mut_supply_chain_fathom_dependency_banned() {
    # Ban a crate only fathom's tree carries. The MIT drill above proves cargo-deny reads *a* graph;
    # this proves it reads *fathom's*, which is the difference between this epic's dependencies being
    # gated and being assumed. `ratatui` is the one to name: nothing in the server's tree reaches it
    # (ADR-0005 §6 forbids it there), so if the workspace were not in cargo-deny's view this ban would
    # change nothing.
    cat >> "$1/deny.toml" <<'TOML'

# injected by scripts/ci_drill.sh — a crate only the fathom tree carries.
[[bans.deny]]
name = "ratatui"
reason = "drill: proves cargo-deny checks fathom's graph and not only the server's"
TOML
}

mut_supply_chain_fathom_firewall_crossed() {
    # The transitive edge a manifest checker cannot see. `schemars` is already in
    # [workspace.dependencies] (act-mcp needs it for the tool schemas) and
    # `scripts/validate_crate_layout.py` C5/C6 read manifests, so declaring it here is the smallest
    # way to put the server's schema engine into fathom's release build through the front door; the
    # firewall check reads `cargo tree`, which is the only place ADR-0005 §11's "a standalone fathom
    # build carries no schemars" can be checked at all.
    python3 - "$1/crates/fathom/Cargo.toml" <<'PY'
import sys

path = sys.argv[1]
text = open(path, encoding="utf-8").read()
needle = "serde_json = { workspace = true }"
if needle not in text:
    sys.exit("drill setup: crates/fathom/Cargo.toml does not declare serde_json as this drill expects")
open(path, "w", encoding="utf-8").write(
    text.replace(needle, needle + "\n# injected by scripts/ci_drill.sh\nschemars = { workspace = true }", 1)
)
PY
}

# ---------------------------------------------------------------------------
# machinery
# ---------------------------------------------------------------------------
copy_repo() {
    local dest=$1
    mkdir -p "$dest"
    local item
    for item in Cargo.toml Cargo.lock rust-toolchain.toml deny.toml schema scripts crates docs .gitignore; do
        [[ -e "$repo_root/$item" ]] && cp -a "$repo_root/$item" "$dest/"
    done
    # `.fathom/LAYOUT.md` is `include_str!`-ed by fathom's boot module (ADR-0006 §4.1), so a copy
    # without it cannot build the crate at all — and a step that fails because of a missing input is a
    # drill that "catches" every violation, including none of them. Only that one file: the rest of
    # `.fathom` is somebody's 14 MB metric cache, which is an output, not an input to a compile.
    if [[ -f "$repo_root/.fathom/LAYOUT.md" ]]; then
        mkdir -p "$dest/.fathom"
        cp -a "$repo_root/.fathom/LAYOUT.md" "$dest/.fathom/"
    fi
    # Cargo calls a tree "fresh" by mtime and `cp -a` preserves mtimes, so a copy can be handed an
    # artefact compiled from a different tree — one whose `env!("CARGO_MANIFEST_DIR")` points at a
    # directory that no longer exists, or (worse, because it is quiet) at the real repository. A drill
    # whose tests never run against the copy is a drill that proves nothing, so make the copy's own
    # member manifests newer than anything in the shared target directory: every workspace crate
    # recompiles from the copy, and every baked path points inside it. Dependencies keep their mtimes,
    # so the expensive part is still paid once per target directory rather than once per drill.
    find "$dest/crates" -mindepth 2 -maxdepth 2 -name Cargo.toml -exec touch {} +
    chmod -R u+w "$dest"
}

verbose=0
selected=()

while [[ $# -gt 0 ]]; do
    case "$1" in
        -h | --help)
            usage
            exit 0
            ;;
        -l | --list)
            drill_list
            exit 0
            ;;
        -v | --verbose) verbose=1 ;;
        --step)
            step=${2:?"--step needs a step name"}
            found=0
            for entry in "${DRILLS[@]}"; do
                if [[ "${entry#*:}" == "$step" ]]; then
                    selected+=("$entry")
                    found=1
                fi
            done
            [[ $found == 1 ]] || {
                echo "ci_drill: no drill for step '$step' (try --list)" >&2
                exit 2
            }
            shift
            ;;
        *)
            echo "ci_drill: unknown argument '$1' (try --help)" >&2
            exit 2
            ;;
    esac
    shift
done

if [[ ${#selected[@]} -eq 0 ]]; then
    selected=("${DRILLS[@]}")
fi

work=$(mktemp -d "${TMPDIR:-/tmp}/act-mcp-drill.XXXXXX")
trap 'rm -rf "$work"' EXIT

# The drill compiles the crate once per drill. It uses its own target directory, kept
# between runs so the second run is warm, and it must stay separate from your working tree's:
# two source trees with the same package name and version sharing one target directory can
# make cargo call the other tree's artefact fresh, which is a whole afternoon of debugging
# you do not need. Set CARGO_TARGET_DIR only to move it, not to share it.
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-${TMPDIR:-/tmp}/act-mcp-drill-target}
mkdir -p "$CARGO_TARGET_DIR"

echo "act-mcp CI drill — every violation below must be caught"
echo "work dir: $work (removed on exit), target: $CARGO_TARGET_DIR"
echo

failed=0
declare -a lines=()
for entry in "${selected[@]}"; do
    name=${entry%%:*}
    step=${entry#*:}
    mutate="mut_${name//-/_}"

    dir="$work/$name"
    copy_repo "$dir"
    "$mutate" "$dir"

    start=$(date +%s)
    status=0
    (cd "$dir" && ./scripts/ci.sh --quiet "$step") > "$work/$name.log" 2>&1 || status=$?
    elapsed=$(( $(date +%s) - start ))

    if [[ $status -eq 0 ]]; then
        # The violation slipped through: the step is not doing its job.
        failed=1
        printf '  \033[31m%-38s step=%-13s NOT CAUGHT\033[0m (%ss)\n' "$name" "$step" "$elapsed"
    else
        printf '  \033[32m%-38s step=%-13s caught\033[0m (%ss, exit %s)\n' "$name" "$step" "$elapsed" "$status"
    fi
    lines+=("$name")
    if [[ $verbose == 1 || $status -eq 0 ]]; then
        sed 's/^/      | /' "$work/$name.log" | tail -n 25
    fi
done

echo
if [[ $failed -ne 0 ]]; then
    echo "DRILL FAILED — a violation got through; the step named above is not a guard."
    exit 1
fi
echo "DRILL OK — ${#selected[@]} violation(s), all caught."
