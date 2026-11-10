#!/usr/bin/env bash
#
# scripts/evidence_recipes.sh — every jq recipe in docs/evidence-format.md, run in order.
#
# The recipes in that document are the contract seen from the reader's side, so they are
# also contract tests of clarity: a recipe that does not parse, or that prints nothing, is
# a document that lies about the data. This script is the copy of them that CI-less humans
# can re-run:
#
#   scripts/evidence_recipes.sh                       # a tree built from the e2e goldens
#   scripts/evidence_recipes.sh /path/to/.evidence    # a real evidence root
#   scripts/evidence_recipes.sh <root> --repo <repo>  # also run the git-log join (recipe 12)
#   scripts/evidence_recipes.sh --no-setup            # do not rebuild the sample tree first
#
# With no path it calls scripts/sample_evidence.py, which lays the checked-in goldens
# under tests/e2e/testdata/expected/ out as a real tree (three commits, five runs). A
# real root works the same way — the scanner-visible half of it is all these recipes read.
# Recipe 12 joins the tree with `git log`, so it needs a repository whose commits the tree
# is keyed by: pass it with --repo and it runs, pass none and it says so and skips.
#
# Needs jq (verified against 1.7.1). Exit status: 0 if every recipe ran and the tree
# validated, 1 otherwise.

set -o errexit -o nounset -o pipefail

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo=$(cd -- "$here/.." && pwd)
setup=1
root=
git_repo=

usage() {
    cat <<'USAGE'
Usage: scripts/evidence_recipes.sh [EVIDENCE_ROOT] [--repo GIT_REPO] [--no-setup]

Runs every jq recipe in docs/evidence-format.md, in order, against EVIDENCE_ROOT
(default: $TMPDIR/act-mcp-evidence-demo, built by scripts/sample_evidence.py).

  --repo GIT_REPO   also run the git-log join recipe against this repository
  --no-setup        do not rebuild the sample tree first
  -h, --help        this text

Needs jq. Exit: 0 all recipes ran and the tree validated, 1 otherwise.
USAGE
}

while [[ $# -gt 0 ]]; do
    case $1 in
        -h | --help)
            usage
            exit 0
            ;;
        --no-setup)
            setup=0
            shift
            ;;
        --repo)
            [[ $# -ge 2 ]] || {
                echo "evidence_recipes: --repo needs a path" >&2
                exit 1
            }
            git_repo=$2
            shift 2
            ;;
        *)
            root=$1
            shift
            ;;
    esac
done

command -v jq >/dev/null || {
    echo "evidence_recipes: jq is required (verified against jq 1.7.1)" >&2
    exit 1
}

if [[ -z $root ]]; then
    if [[ $setup == 1 ]]; then
        "$repo/scripts/sample_evidence.py" --quiet
    fi
    root=${TMPDIR:-/tmp}/act-mcp-evidence-demo
fi
EV=$(cd -- "$root" && pwd)

# The scanner-visible records and nothing else: ADR-0002 §1.1 keeps `runs/` and
# `incoming/` out of the contract, and a `*/*/*.jsonl` glob would walk straight into
# runs/<run_id>/act.jsonl (act's own log, which is not a record).
mapfile -t RECORDS < <(find "$EV"/benchmark "$EV"/coverage -name '*.jsonl' 2>/dev/null | sort)

hdr() {
    # Bold on a terminal, plain when piped or pasted into a document.
    if [[ -t 1 ]]; then
        printf '\n\033[1m# %s\033[0m\n' "$*"
    else
        printf '\n# %s\n' "$*"
    fi
}

hdr "evidence root under test"
echo "$EV"

# --- 1. what a scanner walks -------------------------------------------------------
# Only benchmark/ and coverage/ are the cross-commit contract (ADR-0002 §1.1).
hdr "1. commits and runs, in ls order (== chronological, because run_id sorts)"
find "$EV"/benchmark "$EV"/coverage -name '*.jsonl' 2>/dev/null | sed "s|^$EV/||" | sort

# --- 2. coverage totals for one commit --------------------------------------------
# Percentages are recomputed from the raw counts (ADR-0002 §4.3); `percent_only`
# records are excluded because they carry no counts to recompute from.
COMMIT=$(ls "$EV"/coverage | head -1)
hdr "2. every coverage total for one commit (rate recomputed, percent-only excluded)"
echo "commit $COMMIT"
jq -r 'def pct($hit; $found): if ($found // 0) == 0 then 100 else ((10000 * $hit / $found) | round) / 100 end;
       select(.scope == "total" and .reported_percent == null)
       | [.run_id, .run_status, .source, ((.lines_hit // 0) | tostring),
          ((.lines_found // 0) | tostring), (pct(.lines_hit // 0; .lines_found // 0) | tostring)] | @tsv' \
    "$EV"/coverage/"$COMMIT"/*.jsonl

# --- 3. per-file coverage, ranked --------------------------------------------------
# A zero denominator is vacuous: ranked out, not printed as 0% (ADR-0002 §4.3).
hdr "3. per-file line coverage, ranked, empty files excluded"
jq -r 'select(.scope == "file" and (.lines_found // 0) > 0)
       | [(((10000 * .lines_hit / .lines_found) | round) / 100 | tostring), .path] | @tsv' \
    "$EV"/coverage/"$COMMIT"/*.jsonl | sort -rn

# --- 4. newest value per metric across commits ------------------------------------
# `run_id` is time-sortable, so max_by(.run_id) is the newest measurement. Group by the
# ADR's identity, not by name alone: `step_duration_s` is one metric with many label sets.
hdr "4. newest benchmark value per (metric, labels) across every commit in the tree"
jq -rs 'group_by([.name, (.labels // {})]) | map(max_by(.run_id))
        | sort_by([.name, ((.labels // {}) | tojson)]) | .[]
        | [.name, ((.labels // {}) | tojson), (.value | tostring),
           (.unit // "unit_missing"), .run_id] | @tsv' "$EV"/benchmark/*/*.jsonl

# --- 5. the spread per commit ------------------------------------------------------
# Non-success runs are filtered: they are data, but not a benchmark to regress against.
hdr "5. per-metric, per-commit spread (n, min, max) — successful runs only"
jq -rs '[ group_by([.name, (.labels // {})])[]
         | { metric: .[0].name,
             unit: (.[0].unit // "unit_missing"),
             commits: ([.[] | select(.run_status == "success")]
                       | [ group_by(.commit)[]
                          | { commit: (.[0].commit[0:7]), n: length,
                              min: (map(.value) | min), max: (map(.value) | max) } ] ) } ]
        | .[]
        | [ .metric, .unit,
            (.commits | map("\(.commit): [\(.min),\(.max)] x\(.n)") | join("  ")) ] | @tsv' \
    "$EV"/benchmark/*/*.jsonl

# --- 6. percentiles are the scanner's job ------------------------------------------
hdr "6. median across samples, within one commit (act-mcp never aggregates)"
jq -rs 'def median: sort | length as $n
        | if $n == 0 then null elif ($n % 2) == 1 then .[($n - 1) / 2]
          else (.[($n / 2) - 1] + .[$n / 2]) / 2 end;
        [.[] | select(.run_status == "success")] | group_by([.name, (.labels // {})])[]
        | [.[0].name, ((.[0].labels // {}) | tojson), (length | tostring),
           ((map(.value) | median) | tostring), (.[0].unit // "unit_missing")] | @tsv' \
    "$EV"/benchmark/"$COMMIT"/*.jsonl

# --- 7. advisory records -----------------------------------------------------------
hdr "7. every advisory / last-resort record (the flags a consumer must be able to filter)"
jq -r 'select((.flags // []) | any(. == "percent_only" or . == "low_confidence"
                                   or . == "unit_missing" or . == "truncated"))
       | [.run_id, .kind, .source, ((.flags // []) | join("+")), (.name // .path // "-")] | @tsv' \
    "${RECORDS[@]}"

# --- 8. the identity the ADR pins ---------------------------------------------------
hdr "8. records whose identity key repeats (empty output = the writer kept its promise)"
jq -r '[.commit, .run_id, .kind, (.name // .scope // "-"), ((.labels // {}) | tojson),
        ((.sample // 0) | tostring), (.path // "-")] | @tsv' "${RECORDS[@]}" \
    | sort | uniq -d
echo "(nothing above = no repeats)"

# --- 9. run status is a filter, not a suppression -----------------------------------
hdr "9. run statuses in the tree, and the files that came from a non-success run"
jq -r '.run_status' "${RECORDS[@]}" | sort | uniq -c
jq -r 'select(.run_status != "success")
       | [.run_status, .commit[0:7], .run_id, input_filename] | @tsv' "${RECORDS[@]}" | sort -u

# --- 10. version policy -------------------------------------------------------------
hdr "10. schema_version census, and the strict reader (skip what you cannot read)"
jq -r '.schema_version' "${RECORDS[@]}" | sort | uniq -c
jq -c 'select(.schema_version != 1)' "${RECORDS[@]}"
echo "(nothing above = every record is a version this reader understands)"

# --- 11. the join from git log -------------------------------------------------------
# The scan the full-SHA keying exists for. It needs commits that exist, so it only runs
# with --repo: the sample tree's SHAs are fixtures, not commits in any repository. The
# document spells REPO/EV as literals; here they come from the flags.
hdr "11. the same join from git log (needs --repo <repo>)"
if [[ -z $git_repo ]]; then
    echo "skipped: pass --repo /path/to/the/repo whose commits this tree is keyed by"
else
    REPO=$(cd -- "$git_repo" && pwd)
    EV=$REPO/.evidence
    echo "repo $REPO · evidence root $EV"
    git -C "$REPO" log --format=%H | while read -r sha; do
        for kind in benchmark coverage; do
            d=$EV/$kind/$sha
            [[ -d $d ]] || continue
            printf '%s %s %s file(s), %s record(s)\n' \
                "${sha:0:7}" "$kind" "$(find "$d" -name '*.jsonl' | wc -l)" \
                "$(find "$d" -name '*.jsonl' -exec cat {} + | wc -l)"
        done
    done
fi

# --- 12. the schema, from the binary that ships it -----------------------------------
hdr "12. validate-evidence over the same tree"
if [[ -x "$repo/target/release/act-mcp" ]]; then
    "$repo/target/release/act-mcp" validate-evidence "$EV" --stats
elif [[ -x "$repo/target/debug/act-mcp" ]]; then
    "$repo/target/debug/act-mcp" validate-evidence "$EV" --stats
else
    (cd "$repo" && cargo run --quiet -- validate-evidence "$EV" --stats)
fi

printf '\nall recipes ran\n'
