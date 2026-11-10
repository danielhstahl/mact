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
# rust-toolchain.toml, deny.toml, schema/, scripts/, src/ and tests/ into a temporary
# directory, breaks the copy, and runs the step there.
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
    "test-failing-assertion:test"
    "schema-broken-schema-file:schema"
    "schema-bad-evidence-record:schema"
    "schema-undocumented-tool-io-fixture:schema"
    "supply-chain-disallowed-licence:supply-chain"
)

drill_list() { printf '%s\n' "${DRILLS[@]}"; }

# Each mutation takes the copy's root as $1 and makes exactly one violation.
mut_fmt_misformatted_code() {
    cat >> "$1/src/lib.rs" <<'RS'

/// Injected by scripts/ci_drill.sh: valid Rust that rustfmt will not accept.
pub fn ci_drill_fmt() -> u32 {
    let  drill = 41;
    drill+1
}
RS
}

mut_clippy_lint() {
    cat >> "$1/src/lib.rs" <<'RS'

/// Injected by scripts/ci_drill.sh: compiles, but trips clippy::needless_return.
#[allow(dead_code)]
fn ci_drill_clippy() -> u32 {
    let value = 1;
    return value;
}
RS
}

mut_doc_broken_intra_doc_link() {
    cat >> "$1/src/lib.rs" <<'RS'

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
    cat >> "$1/src/lib.rs" <<'RS'

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

mut_test_failing_assertion() {
    cat >> "$1/src/lib.rs" <<'RS'

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
        >> "$1/tests/e2e/testdata/expected/benchmark.jsonl"
}

mut_schema_undocumented_tool_io_fixture() {
    # Delete the README row that names one tool-io fixture. The contract checker refuses an
    # undocumented fixture, because a contract rule nobody can look up is a rule that will be
    # broken by accident; this proves that refusal is fatal and not advice.
    python3 - "$1/tests/e2e/testdata/README.md" <<'PY'
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
# machinery
# ---------------------------------------------------------------------------
copy_repo() {
    local dest=$1
    mkdir -p "$dest"
    local item
    for item in Cargo.toml Cargo.lock rust-toolchain.toml deny.toml schema scripts src tests docs; do
        [[ -e "$repo_root/$item" ]] && cp -a "$repo_root/$item" "$dest/"
    done
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
