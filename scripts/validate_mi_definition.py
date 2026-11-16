#!/usr/bin/env python3
"""Check ADR-0007 (the Maintainability Index definition): the ADR, schema/mi-definition.json, the
golden vectors in tests/testdata/mi and the sibling ADRs say the same thing — read from those
artifacts, not from prose.

This is the dev-time checker for ADR-0007 (ticket mact-by1.3), the sibling of
scripts/validate_tool_contract.py, scripts/validate_evidence.py, scripts/validate_run_lifecycle.py,
scripts/validate_isolation.py, scripts/validate_crate_layout.py and scripts/validate_fathom_layout.py.
ADR-0007's load-bearing artifacts are a set of numbers and a set of closed enumerations, so this is
the checker that answers the question the ticket actually asks: *would two implementations agree?*

It is a strict superset of `scripts/mi_reference.py --vector`: that command re-derives the numbers,
this script additionally pins the ADR prose to the JSON, pins the vectors against the per-language
enumerations, re-derives every number a second time in 40-digit decimal arithmetic (ADR-0002 §7's
"two engines, one contract" rule, applied to arithmetic instead of to a schema), and refuses a vector
that sits on a rounding or band boundary where a 1 ULP libm difference could move a stored digit.

It needs no third-party package (stdlib `decimal`, `json`, `tomllib`) and no `cargo`.

Checks performed (all of them, every run):

  C1   the ADR: exists, has front matter (id/title/status/ticket/date), is accepted, is the ticket's
       ticket, carries no unresolved marker, states every decision the ticket asked for, and cites
       only check ids this script implements.
  C2   the ADR's `mi-constants` fenced block parses and equals schema/mi-definition.json field for
       field — the formula, the precision rules, the ladder thresholds, the analyzer ids, the
       aggregation rules and the scope defaults are the same numbers in the same file twice on
       purpose, and the checker is what keeps the two copies honest.
  C3   the bands: the ADR `mi-bands` block equals the JSON, the edges named in the prose are the
       edges in the JSON, an exact edge belongs to the better band, the bands are neither config nor
       stored, and the ADR carries the arithmetic that justifies the numbers it declares.
  C4   the measurability ladder: the reason vocabulary is closed, matches ADR-0006's row `counts`
       keys, is presented in the ADR in ladder order, and the "no MI is never 0" rule is stated.
  C5   the language tables: v1 enables exactly the languages the ADR names, every enabled language
       has a golden vector and every table a decision-point rule set, the shared exceptions name
       rules that exist, and the source-extension map agrees with the per-language extension lists.
  C6   the golden vectors: every vector parses and re-derives exactly (occurrence tables, V, G, LOC,
       MI, band), every vector's source file exists, the reformat pair really is a pair of different
       files, no vector sits on a boundary, and the coverage list the ADR promises is satisfied.
  C7   a second, independent implementation of the arithmetic: every ok vector is recomputed in
       40-digit `decimal` and must round to the stored digit. Binary64 and decimal agreeing is the
       cheapest available proof that a port to Rust is not going to move a trend line.
  C8   the aggregation fixtures re-derive, and the aggregation rules hold as properties: LOC-weight,
       worst-first hotspots with the pinned tie-break, the watchlist is exactly the red band, the
       delta is the difference of the two rounded aggregates, and an empty population is unknown.
  C9   the aggregation and front-end rules the ADR states in prose (weight, hotspot count, hotspot
       sort, accumulation order, per-commit population) are the ones in the JSON.
  C10  the front-end decision holds in the dependency graph: no tree-sitter (or any C-backed parser)
       in Cargo.lock, `deny.toml` bans the ones ADR-0007 §6 refuses, and the ADR states the reason in
       terms the last two ADRs used.
  C11  the file selector MI uses is the one the diffstat uses: the noise globs are ADR-0006 §7.2's,
       verbatim, the new `paths.include_tests` key is declared here and consumed by both analyzers,
       and nothing about the MI is configurable in a way that would make two clones disagree.
  C12  documentation wiring: the ADR is linked from README's ADR list, named in docs/ci.md and
       docs/testing.md, and this script is wired into scripts/ci.sh's schema step.
  C13  the checker itself is not a lie: --self-test builds a scratch fixture out of the real
       artifacts, breaks one rule at a time, and asserts the check it names is the one that fires.

Usage:
    scripts/validate_mi_definition.py [--adr PATH] [--definition PATH] [--root PATH]
                                      [--quiet] [--stats] [--self-test]

Exit status: 0 everything consistent, 1 a contract violation (one line each), 2 usage or environment
error (a missing ADR, unreadable JSON, a fixture that cannot be built).
"""
from __future__ import annotations

import argparse
import json
import re
import shutil
import sys
import tempfile
from dataclasses import dataclass
from decimal import Decimal, getcontext
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import mi_reference as mir  # noqa: E402

REPO = Path(__file__).resolve().parent.parent
ADR_NAME = "0007-maintainability-index.md"
DEFAULT_ADR = REPO / "docs" / "adr" / ADR_NAME
ADR0006_NAME = "0006-fathom-workspace-layout.md"
DEFAULT_DEFINITION = REPO / "schema" / "mi-definition.json"
DEFAULT_VECTORS = REPO / "tests" / "testdata" / "mi"
SCRIPT_NAME = "validate_mi_definition.py"

#: Check ids this script implements, so an ADR that cites "C9" cannot cite a check that does not exist.
CHECK_IDS = {f"C{i}" for i in range(1, 14)}

#: What the ticket demanded, transcribed as tokens so "every bullet answered" is a machine's finding
#: rather than a claim. Keys are prose labels used in the failure message.
DECISION_TOKENS = {
    "the formula": "mi_raw = 171 - 5.2*ln(halsteadvolume)",
    "the scaling": "clamp(mi_raw * 100 / 171, 0, 100)",
    "halstead volume definition": "v = n * log2(n)",
    "log2 pinned, not ln/ln2": "log2",
    "cyclomatic definition": "g = 1 + decision points",
    "decision points enumerated per language": "decision_points",
    "early-return guards cost once": "guard clause is an `if`",
    "logical lines defined": "logical line",
    "comment and blank exclusion": "comment",
    "multi-line calls": "logical line",
    "macro and format-string bodies": "token tree",
    "brace-only lines": "{`-only",
    "ln(0) is undefined": "undefined",
    "unknown, never 0": "never 0",
    "per-file unit": "one blob",
    "per-commit is an aggregate": "not a measurement",
    "loc-weighted mean": "loc-weighted mean",
    "delta vs parent": "delta_vs_parent",
    "sub-threshold set transitions": "sub_threshold_added",
    "worst-N hotspots": "hotspot",
    "threshold bands declared here": "not invented in the view layer",
    "shared source classifier": "classifier",
    "noise globs": "paths.noise",
    "tests opt-in": "paths.include_tests",
    "front end chosen": "tree-sitter",
    "front end rejected with reason": "cc",
    "v1 language set": "rust",
    "a scripting language": "python",
    "what other languages show": "unsupported_language",
    "caveat count of unanalysed paths": "unanalysed_paths",
    "analyzer id and version": "analyzer_version",
    "config digest": "config_digest",
    "cache key includes all three": "input_kind",
    "golden vectors in the test data": "tests/testdata/mi",
    "one vector per language": "required_cases",
    "the loc == 0 case": "vacuous",
    "a determinism test": "determinism",
}

#: Phrases that mean "this decision is still open", copied from the other ADR checkers.
UNRESOLVED = ("todo:", "tbd", "fixme", "unresolved:", "pick one", "to be decided")

#: The front ends ADR-0007 §6 refuses, and therefore the crates that must never appear.
BANNED_PARSERS = ("tree-sitter", "tree-sitter-rust", "tree-sitter-python", "tree-sitter-language")

#: The six fields of the ADR's canonical constants block. Kept as a constant so the ADR and this
#: script cannot disagree about what the block is allowed to contain.
CONSTANT_KEYS = [
    "definition_version",
    "analyzer",
    "formula",
    "precision",
    "measurability",
    "aggregation",
    "scope",
]

SKIPPED: list[str] = []


# ---------------------------------------------------------------------------------------------
# the artifacts
# ---------------------------------------------------------------------------------------------


@dataclass
class Tree:
    root: Path
    adr: Path
    definition_path: Path

    def __post_init__(self) -> None:
        self.adr_text = self.adr.read_text(encoding="utf-8")
        self.definition = json.loads(self.definition_path.read_text(encoding="utf-8"))
        self.defn = mir.Definition(self.definition)
        self.vector_dir = self.root / "tests/testdata/mi/vectors"
        self.source_dir = self.root / "tests/testdata/mi/src"
        self.aggregate_dir = self.root / "tests/testdata/mi/aggregates"
        self.vectors = load_json_dir(self.vector_dir)
        self.aggregates = load_json_dir(self.aggregate_dir)
        self.adr0006 = read_if_exists(self.root / "docs/adr" / ADR0006_NAME)
        self.deny = read_if_exists(self.root / "deny.toml")
        self.lock = read_if_exists(self.root / "Cargo.lock")
        self.cargo = read_if_exists(self.root / "Cargo.toml")
        self.readme = read_if_exists(self.root / "README.md")
        self.ci_doc = read_if_exists(self.root / "docs/ci.md")
        self.testing_doc = read_if_exists(self.root / "docs/testing.md")
        self.ci_sh = read_if_exists(self.root / "scripts/ci.sh")


def read_if_exists(path: Path) -> str:
    try:
        return path.read_text(encoding="utf-8")
    except OSError:
        return ""


def load_json_dir(directory: Path) -> dict[str, dict]:
    out: dict[str, dict] = {}
    if not directory.is_dir():
        return out
    for path in sorted(directory.glob("*.json")):
        out[path.stem] = json.loads(path.read_text(encoding="utf-8"))
    return out


def fenced(text: str, marker: str) -> str | None:
    """The body of a `<!-- marker:begin --> ... <!-- marker:end -->` block."""
    pattern = re.escape(f"<!-- {marker}:begin -->") + r"(.*?)" + re.escape(f"<!-- {marker}:end -->")
    match = re.search(pattern, text, re.DOTALL)
    return match.group(1) if match else None


def fenced_json(text: str, marker: str, problems: list[str], label: str) -> dict | None:
    body = fenced(text, marker)
    if body is None:
        problems.append(f"{label}: no `<!-- {marker}:begin -->` block; the ADR must carry its own copy "
                        "of the numbers so a reviewer reads them where they are argued")
        return None
    match = re.search(r"```json\s*(.*?)```", body, re.DOTALL)
    if not match:
        problems.append(f"{label}: the {marker} block has no ```json fence")
        return None
    try:
        return json.loads(match.group(1))
    except json.JSONDecodeError as err:
        problems.append(f"{label}: the {marker} block is not JSON: {err}")
        return None


def table_rows(text: str, first_column_startswith: str) -> list[list[str]]:
    rows = []
    for line in text.splitlines():
        line = line.strip()
        if not line.startswith("|"):
            continue
        cells = [cell.strip() for cell in line.strip("|").split("|")]
        if cells and cells[0].lower().startswith(first_column_startswith.lower()):
            rows.append(cells)
    return rows


def backticked(text: str) -> set[str]:
    return {token.lower() for token in re.findall(r"`([^`\n]+)`", text)}


def numbers_in(text: str) -> set[str]:
    return set(re.findall(r"\d+\.\d+|\b\d+\b", text))


# ---------------------------------------------------------------------------------------------
# C1 the ADR
# ---------------------------------------------------------------------------------------------


def check_adr(tree: Tree, problems: list[str], stats: dict[str, int]) -> None:
    adr = tree.adr_text
    front = adr.split("---")
    if len(front) < 3 or "id:" not in front[1]:
        problems.append("C1: no YAML front matter block with id/title/status/ticket/date")
    else:
        fields = dict(re.findall(r"^(id|title|status|ticket|date):\s*(.+)$", front[1], re.MULTILINE))
        for key in ("id", "title", "status", "ticket", "date"):
            if key not in fields:
                problems.append(f"C1: front matter is missing {key}")
        if fields.get("id", "").strip() != "ADR-0007":
            problems.append(f"C1: front matter id is {fields.get('id')!r}, expected ADR-0007")
        if fields.get("status", "").strip().lower() != "accepted":
            problems.append(f"C1: status is {fields.get('status')!r}; an ADR that has not been decided "
                            "cannot pin a metric that goes into a cache")
        if "mact-by1.3" not in fields.get("ticket", ""):
            problems.append(f"C1: front matter ticket is {fields.get('ticket')!r}, expected mact-by1.3")
        if fields.get("date") and not re.fullmatch(r"\d{4}-\d{2}-\d{2}", fields["date"].strip()):
            problems.append(f"C1: front matter date is {fields.get('date')!r}")

    lowered = adr.lower()
    for marker in UNRESOLVED:
        if marker in lowered:
            problems.append(f"C1: unresolved marker {marker!r}: this ADR is the decision, it may not "
                            "leave one open")

    missing = [label for label, token in DECISION_TOKENS.items() if token not in lowered]
    if missing:
        problems.append(
            "C1: the ADR does not state these decisions the ticket asked for: "
            + ", ".join(missing)
            + " (each is looked for as a literal phrase; see DECISION_TOKENS in this script)"
        )
    stats["C1 decisions checked"] = len(DECISION_TOKENS)

    cited = set(re.findall(r"\bC(?:\d{1,2})\b", adr))
    unknown = sorted(cited - CHECK_IDS)
    if unknown:
        problems.append(f"C1: the ADR cites checks this script does not implement: {', '.join(unknown)}")
    stats["C1 checks cited"] = len(cited & CHECK_IDS)

    for section in ("## 0.", "## 1.", "## 2."):
        if section not in adr:
            problems.append(f"C1: expected section {section} (the decision screen, the context, the formula)")


# ---------------------------------------------------------------------------------------------
# C2 the ADR's constants block against the JSON
# ---------------------------------------------------------------------------------------------


def canonical_constants(definition: dict) -> dict:
    """The exact subset the ADR is required to carry, built from the JSON so the two cannot drift by
    an omitted key: adding a key to the JSON makes the ADR's block wrong until it is updated."""
    precision_keys = ("storage_decimals", "rounding", "ln", "log2", "arithmetic", "boundary_slack",
                      "volume_basis")
    aggregation_keys = ("weight", "formula", "hotspots_default", "hotspot_sort", "accumulation_order",
                        "sub_threshold_set", "parent", "delta_rule", "recompute_rule", "empty_population")
    measurability_keys = ("min_loc", "min_operator_unique", "ordered_reasons")
    scope_keys = ("source_extensions", "precedence", "binary_rule")
    return {
        "definition_version": definition["definition_version"],
        "analyzer": definition["analyzer"],
        "formula": definition["formula"],
        "precision": {k: definition["precision"][k] for k in precision_keys if k in definition["precision"]},
        "measurability": {k: definition["measurability"][k] for k in measurability_keys
                          if k in definition["measurability"]},
        "aggregation": {k: definition["aggregation"][k] for k in aggregation_keys
                        if k in definition["aggregation"]},
        "scope": {k: definition["scope"][k] for k in scope_keys if k in definition["scope"]},
    }


def check_constants_block(tree: Tree, problems: list[str], stats: dict[str, int]) -> None:
    block = fenced_json(tree.adr_text, "mi-constants", problems, "C2")
    if block is None:
        return
    want = canonical_constants(tree.definition)
    if set(block) - set(CONSTANT_KEYS):
        problems.append(f"C2: the mi-constants block carries keys outside the agreed set: "
                        f"{sorted(set(block) - set(CONSTANT_KEYS))}")
    for key in CONSTANT_KEYS:
        if key not in block:
            problems.append(f"C2: the mi-constants block is missing {key!r}")
    diff = deep_diff(want, block)
    for line in diff:
        problems.append(f"C2: the ADR's constants block and schema/mi-definition.json disagree: {line}")
    if not diff:
        stats["C2 fields compared"] = sum(1 for _ in walk(want))
    analyzer = tree.definition["analyzer"]
    for key in ("tokenize", "score"):
        spec = analyzer[key]
        if spec["id"].lower() not in tree.adr_text.lower():
            problems.append(f"C2: analyzer id {spec['id']!r} is never named in the ADR")
        if str(spec["version"]) == "":
            problems.append(f"C2: analyzer {spec['id']} has no version")
    for kind in ("blob", "path", "blob_path", "tree", "commit", "evidence"):
        declared = {analyzer["tokenize"]["input_kind"], analyzer["score"]["input_kind"]}
        if kind in declared and kind not in {"blob", "path", "blob_path"}:
            problems.append(f"C2: MI analyzers may only be keyed by content or by content-and-path; "
                            f"{spec['input_kind']!r} is neither")


def deep_diff(want: object, got: object, path: str = "") -> list[str]:
    """Every leaf where two JSON trees differ, as a path."""
    if isinstance(want, dict) and isinstance(got, dict):
        problems = []
        for key in sorted(set(want) | set(got)):
            if key not in got:
                problems.append(f"{path}/{key}: in the JSON, missing from the ADR")
            elif key not in want:
                problems.append(f"{path}/{key}: in the ADR, not in the JSON")
            else:
                problems.extend(deep_diff(want[key], got[key], f"{path}/{key}"))
        return problems
    if isinstance(want, list) and isinstance(got, list):
        if want != got:
            return [f"{path}: {want!r} != {got!r}"]
        return []
    if want != got:
        return [f"{path}: {want!r} != {got!r}"]
    return []


def walk(value: object):
    if isinstance(value, dict):
        for inner in value.values():
            yield from walk(inner)
    elif isinstance(value, list):
        for inner in value:
            yield from walk(inner)
    else:
        yield value


# ---------------------------------------------------------------------------------------------
# C3 the bands
# ---------------------------------------------------------------------------------------------


def check_bands(tree: Tree, problems: list[str], stats: dict[str, int]) -> None:
    bands = tree.definition["bands"]
    block = fenced_json(tree.adr_text, "mi-bands", problems, "C3")
    if block is not None and block != bands:
        for line in deep_diff(bands, block):
            problems.append(f"C3: the ADR's bands block and the JSON disagree: {line}")

    if bands.get("is_config") is not False:
        problems.append("C3: bands must not be configuration; a per-user threshold would make two "
                        "clones of one repository disagree about a measurement")
    if bands.get("stored_in_cache") is not False:
        problems.append("C3: bands must not be stored: changing an edge must not invalidate rows whose "
                        "numbers did not move (ADR-0006 §6.3)")
    if bands.get("edge_ownership") != "better band":
        problems.append(f"C3: edge ownership is {bands.get('edge_ownership')!r}; the better band owns "
                        "an exact edge so that a value cannot be red on one side of a rounding")
    amber = float(bands["names_by_threshold"]["amber_below"])
    red = float(bands["names_by_threshold"]["red_below"])
    if not red < amber:
        problems.append("C3: the red edge must be below the amber edge")
    for value in (amber, red):
        if abs(value * 10 - round(value * 10)) > 1e-12:
            problems.append(f"C3: band edge {value} is finer than the stored decimal; a file could "
                            "land on an edge no stored value can ever equal")
    # the ADR must argue the numbers, not merely print them
    prose = tree.adr_text.lower()
    for needle in ("updated mi", "calibrat"):
        if needle not in prose:
            problems.append(f"C3: the ADR declares band edges but never {needle!r}: the ticket suggested "
                            "85/65 and any deviation has to be argued, not asserted")
    for value in (amber, red):
        text = f"{value:g}"
        if text not in tree.adr_text:
            problems.append(f"C3: band edge {text} is in the JSON and not in the ADR prose")
    if mir.band_of(tree.defn, amber) != "green" or mir.band_of(tree.defn, red) != "amber":
        problems.append("C3: band_of() disagrees with the declared edge ownership")
    if mir.band_of(tree.defn, red - 0.1) != "red":
        problems.append("C3: a value just under the red edge is not red")
    stats["C3 band edges"] = 2


# ---------------------------------------------------------------------------------------------
# C4 the measurability ladder
# ---------------------------------------------------------------------------------------------


def check_ladder(tree: Tree, problems: list[str], stats: dict[str, int]) -> None:
    definition = tree.definition
    reasons = [row["id"] for row in definition["status_reasons"]]
    ordered = definition["measurability"]["ordered_reasons"]
    if sorted(reasons) != sorted(ordered):
        problems.append(f"C4: the reason table and the ladder order disagree: {sorted(reasons)} vs {ordered}")
    if not ordered or ordered[-1] != "ok":
        problems.append("C4: the ladder must end in `ok`, which is the only verdict that produces a number")
    if definition["measurability"]["min_loc"] != 1:
        problems.append("C4: min_loc must be 1: LOC == 0 has no MI, LOC == 1 is the smallest file that "
                        "has one (ln(1) is 0, which is defined)")
    if definition["measurability"]["min_operator_unique"] != 2:
        problems.append("C4: min_operator_unique must be 2, because log2(1) is 0 and log2(0) is undefined")
    in_caveat = {row["id"] for row in definition["status_reasons"] if row.get("in_caveat_count")}
    expected_caveat = {"unsupported_language", "parse_error", "vacuous", "oversized"}
    if in_caveat != expected_caveat:
        problems.append(f"C4: the caveat-count set is {sorted(in_caveat)}, expected "
                        f"{sorted(expected_caveat)} (these are the paths the view must announce)")
    # ADR-0006's row shape already names six counts; MI must use exactly those names.
    counts_row = re.search(r'"counts":\s*\{([^}]*)\}', tree.adr0006 or "")
    if counts_row:
        keys = set(re.findall(r'"(\w+)":', counts_row.group(1)))
        unknown = sorted(keys - set(reasons))
        if unknown:
            problems.append(f"C4: ADR-0006's row `counts` names {unknown}, which this ADR's reason "
                            "vocabulary does not produce; the row and the analyzer must use one vocabulary")
        stats["C4 counts reconciled with ADR-0006"] = len(keys)
    else:
        SKIPPED.append("C4: ADR-0006's `counts` row: not found, skipped")
    # and the ADR must present them in ladder order
    lowered = tree.adr_text.lower()
    ladder_line = " \u2192 ".join(ordered)
    if ladder_line.lower() not in lowered:
        problems.append(f"C4: the ADR must state the ladder in one line, exactly: {ladder_line}")
    for reason in ordered:
        if not re.search(r"\b" + re.escape(reason) + r"\b", lowered):
            problems.append(f"C4: reason {reason!r} is never named in the ADR")
    if "never 0" not in lowered:
        problems.append("C4: the ADR must say out loud that a missing MI is never rendered as 0")


# ---------------------------------------------------------------------------------------------
# C5 the language tables
# ---------------------------------------------------------------------------------------------


def check_languages(tree: Tree, problems: list[str], stats: dict[str, int]) -> None:
    definition = tree.definition
    languages = definition["languages"]
    enabled = sorted(name for name, table in languages.items() if table.get("enabled_in_v1"))
    if not enabled:
        problems.append("C5: no language is enabled; there would be nothing to measure")
    required_table_keys = (
        "operator_keywords", "operand_keywords", "operator_punctuation", "ignored_punctuation",
        "decision_points", "statements", "name_rules_ordered", "ambiguities", "extensions",
    )
    for name, table in languages.items():
        for key in required_table_keys:
            if key not in table:
                problems.append(f"C5: language {name!r} has no {key!r}")
        overlap = set(table.get("operator_keywords", [])) & set(table.get("operand_keywords", []))
        if overlap:
            problems.append(f"C5: {name}: {sorted(overlap)} is both an operator and an operand keyword")
        clash = set(table.get("operator_punctuation", [])) & set(table.get("ignored_punctuation", []))
        if clash:
            problems.append(f"C5: {name}: {sorted(clash)} is both operator punctuation and ignored")
        for pattern in table.get("operator_punctuation", []) + table.get("ignored_punctuation", []):
            if not any(not ch.isalnum() and ch != "_" for ch in pattern):
                problems.append(f"C5: {name}: {pattern!r} is listed as punctuation but is not")
        rules = table.get("decision_points", {}).get("rules", [])
        if not rules:
            problems.append(f"C5: {name} enumerates no decision points, so G would be 1 everywhere")
        for rule in rules:
            if "id" not in rule or "weight" not in rule:
                problems.append(f"C5: {name}: decision rule {rule} needs an id and a weight")
            if "token" not in rule and "structural" not in rule:
                problems.append(f"C5: {name}: decision rule {rule.get('id')} matches neither a token "
                                "nor a structural rule")
            allowed = (list(table.get("operator_keywords", [])) + list(table.get("operator_punctuation", []))
                       + list(table.get("contextual_keywords_are_operands", []))
                       + list(table.get("soft_keywords", [])))
            if rule.get("token") and rule["token"] not in allowed and rule.get("condition") != "value_preceded":
                problems.append(f"C5: {name}: decision rule {rule.get('id')} names the token "
                                f"{rule['token']!r}, which the enumerations do not call an operator")
    # extensions and the scope map must be the same map
    scope_map = {ext: lang for ext, lang in definition["scope"]["source_extensions"].items()}
    for name, table in languages.items():
        for ext in table.get("extensions", []):
            if scope_map.get(ext) != name:
                problems.append(f"C5: extension {ext!r} maps to {scope_map.get(ext)!r} in "
                                f"`scope.source_extensions` but to {name!r} in the language table")
    for ext, lang in scope_map.items():
        if lang not in enabled:
            problems.append(f"C5: `scope.source_extensions` routes {ext!r} to {lang!r}, which is not "
                            "enabled in v1; an extension is either measurable or not source at all")
    # every enabled language needs vectors
    for name in enabled:
        if not any(vector.get("language") == name for vector in tree.vectors.values()):
            problems.append(f"C5: {name} is enabled and has no golden vector")
    # shared exceptions must name rules that exist
    ids = {f"{lang}:{rule['id']}" for lang, table in languages.items() for rule in
           table.get("decision_points", {}).get("rules", [])}
    for exception in definition["shared"]["exceptions"]:
        for target in exception.get("applies", []):
            if target in ("rust", "python"):
                if target not in languages:
                    problems.append(f"C5: shared exception {exception['id']} applies to unknown {target}")
                continue
            if target not in ids and target.split(":")[0] in languages:
                problems.append(f"C5: shared exception {exception['id']} applies to "
                                f"{target!r}, which is not a rule of that language")
    stats["C5 languages enabled"] = len(enabled)


# ---------------------------------------------------------------------------------------------
# C6 the golden vectors
# ---------------------------------------------------------------------------------------------


def check_vectors(tree: Tree, problems: list[str], stats: dict[str, int]) -> None:
    if not tree.vectors:
        problems.append("C6: no golden vectors under tests/testdata/mi/vectors; the ADR would be "
                        "asserting numbers nobody can re-derive")
        return
    for name, vector in sorted(tree.vectors.items()):
        for key in ("vector", "language", "source", "expect"):
            if key not in vector:
                problems.append(f"C6: vector {name} has no {key!r}")
        source = (tree.vector_dir / vector.get("source", "@nowhere")).resolve()
        if not source.is_file():
            problems.append(f"C6: vector {name} points at {vector.get('source')}, which does not exist")
        if vector.get("sources_equal"):
            pair = [tree.source_dir / p for p in vector["sources_equal"]]
            if len(pair) != 2 or not all(p.is_file() for p in pair):
                problems.append(f"C6: vector {name} claims sources_equal {vector['sources_equal']} "
                                "and one of them is missing")
            elif len(pair) == 2 and pair[0].read_bytes() == pair[1].read_bytes():
                problems.append(f"C6: {vector['sources_equal'][0]} and {vector['sources_equal'][1]} are "
                                "byte-identical, so the pair proves nothing about formatting invariance")
        for problem in mir.check_vector(tree.defn, vector):
            problems.append(f"C6: vector {name}: {problem}")
    stats["C6 vectors"] = len(tree.vectors)

    ok_vectors = [v for v in tree.vectors.values() if v["expect"].get("status") == "ok"]
    statuses = {v["expect"].get("status") for v in tree.vectors.values()}
    # the coverage list the ADR promises
    if "vacuous" not in statuses:
        problems.append("C6: no vacuous vector: the LOC == 0 case is the one the ticket named, and the "
                        "one an analyzer quietly gets wrong by writing 0")
    if not any(v["expect"].get("status") == "parse_error" for v in tree.vectors.values()):
        problems.append("C6: no parse_error vector")
    if not any(v["expect"].get("loc") == 1 for v in ok_vectors):
        problems.append("C6: no LOC == 1 vector: ln(1) == 0 is the boundary between defined and "
                        "undefined and must be pinned")
    pairs = [v for v in tree.vectors.values() if v["expect"].get("also_holds_for")]
    if not pairs:
        problems.append("C6: no reformat pair: formatting-invariance is the property that makes a "
                        "`cargo fmt` commit cost zero on the trend line")
    for language in {v["language"] for v in tree.vectors.values()}:
        kinds = {tuple(sorted((v["expect"].get("decision_points") or {}).keys()))
                 for v in tree.vectors.values() if v["language"] == language and v["expect"].get("status") == "ok"}
        union = set().union(*kinds) if kinds else set()
        declared = {rule["id"] for rule in tree.defn.language(language)["decision_points"]["rules"]}
        # `if_comprehension` is declared so a vector can name it; it is a zero-weight note, not a rule
        unused = {rule for rule in declared - union if tree.defn.language(language) and
                  next(r["weight"] for r in tree.defn.language(language)["decision_points"]["rules"]
                       if r["id"] == rule) != 0}
        if unused:
            problems.append(f"C6: {language}: decision rules {sorted(unused)} are declared and never "
                            "exercised by any vector")
        unknown = sorted(union - declared)
        if unknown:
            problems.append(f"C6: {language}: vectors use decision points {unknown} that the table "
                            "does not declare")


# ---------------------------------------------------------------------------------------------
# C7 the same arithmetic, a second time, in decimal
# ---------------------------------------------------------------------------------------------


def decimal_mi(defn: mir.Definition, volume: float, g: int, loc: int) -> Decimal:
    """The formula again, in 40-digit decimal: a second implementation of the arithmetic, so that
    agreement is a finding rather than a tautology."""
    getcontext().prec = 40
    formula = defn.formula
    v = Decimal(repr(volume))
    raw = (Decimal(str(formula["a"]))
           - Decimal(str(formula["b_volume"])) * v.ln()
           - Decimal(str(formula["c_cyclomatic"])) * Decimal(g)
           - Decimal(str(formula["d_loc"])) * Decimal(loc).ln())
    scaled = raw * Decimal(str(formula["scale_numerator"])) / Decimal(str(formula["scale_denominator"]))
    clamped = min(max(scaled, Decimal(str(formula["clamp_min"]))), Decimal(str(formula["clamp_max"])))
    decimals = int(defn.precision["storage_decimals"])
    factor = Decimal(10) ** decimals
    sign = -1 if clamped < 0 else 1
    return Decimal(sign) * ((abs(clamped) * factor).to_integral_value(rounding="ROUND_HALF_UP") / factor)


def check_second_opinion(tree: Tree, problems: list[str], stats: dict[str, int]) -> None:
    for name, vector in sorted(tree.vectors.items()):
        if vector["expect"].get("status") != "ok":
            continue
        want = decimal_mi(tree.defn, vector["expect"]["volume"], int(vector["expect"]["cyclomatic"]),
                          int(vector["expect"]["loc"]))
        got = Decimal(repr(float(vector["expect"]["mi"])))
        if want != got:
            problems.append(f"C7: vector {name}: decimal arithmetic says {want}, the binary64 oracle "
                            f"says {got}; the two implementations of the formula disagree, so no port "
                            "can be trusted to agree with either")
            continue
        stats["C7 decimal cross-checks"] = stats.get("C7 decimal cross-checks", 0) + 1


# ---------------------------------------------------------------------------------------------
# C8 the aggregation fixtures
# ---------------------------------------------------------------------------------------------


def check_aggregates(tree: Tree, problems: list[str], stats: dict[str, int]) -> None:
    if not tree.aggregates:
        problems.append("C8: no aggregate fixtures; the per-commit rules would be untestable")
        return
    for name, fixture in sorted(tree.aggregates.items()):
        for problem in mir.check_aggregate(tree.defn, fixture):
            problems.append(f"C8: aggregate fixture {name}: {problem}")
        red = float(tree.definition["bands"]["names_by_threshold"]["red_below"])
        derived = mir.commit_row(tree.defn, fixture["files"], fixture.get("parent_files"))
        want_watch = sorted((r["path"] for r in fixture["files"]
                             if r.get("status", "ok") == "ok" and float(r["mi"]) < red))
        if derived["sub_threshold_set"] != want_watch:
            problems.append(f"C8: fixture {name}: the watchlist is not exactly the set below the red edge")
        hotspots = [h["mi"] for h in derived["hotspots"]]
        if hotspots != sorted(hotspots):
            problems.append(f"C8: fixture {name}: hotspots are not worst-first")
        if len(hotspots) > int(tree.definition["aggregation"]["hotspots_default"]):
            problems.append(f"C8: fixture {name}: more hotspots than the declared worst-N")
        if derived["loc"] != sum(int(r["loc"]) for r in fixture["files"] if r.get("status") == "ok"):
            problems.append(f"C8: fixture {name}: LOC is not the sum of the files that were counted")
        stats["C8 fixtures"] = len(tree.aggregates)
    # the empty population, which has no fixture file because it is a property, not a case
    empty = mir.aggregate(tree.defn, [{"path": "README.md", "status": "excluded"}])
    if empty["aggregate"] is not None:
        problems.append("C8: a commit with nothing measurable must have an unknown aggregate, not a number")
    # and the order-independence the ADR claims
    files = tree.aggregates[sorted(tree.aggregates)[0]]["files"]
    if mir.aggregate(tree.defn, files)["aggregate"] != mir.aggregate(tree.defn, list(reversed(files)))["aggregate"]:
        problems.append("C8: the aggregate depends on the order the rows arrive in; the ADR says it may not")


# ---------------------------------------------------------------------------------------------
# C9 the aggregation rules in prose
# ---------------------------------------------------------------------------------------------


def check_aggregation_prose(tree: Tree, problems: list[str], stats: dict[str, int]) -> None:
    aggregation = tree.definition["aggregation"]
    if aggregation["weight"] != "loc":
        problems.append("C9: the aggregate must be LOC-weighted; that is the ticket's own remedy for a "
                        "12-line module outvoting a 900-line one")
    if "loc-weighted mean" not in tree.adr_text.lower():
        problems.append("C9: the ADR must state the weight in words, not only in the JSON")
    if aggregation["parent"] != "git parent #1 (first parent), whatever the walk mode":
        problems.append("C9: the parent a delta is computed against must be pinned to first parent; "
                        "a merge with no rule silently compares against the wrong commit")
    if "pure function of the commit" not in aggregation["recompute_rule"]:
        problems.append("C9: the recompute rule must say the aggregate is a pure function of the commit's "
                        "own file set, or the stored digit depends on indexing order")
    for phrase in ("hotspot", "watchlist", "delta_vs_parent", "unanalysed_paths"):
        if phrase not in tree.adr_text.lower():
            problems.append(f"C9: the ADR never mentions {phrase!r}, which the aggregate is required to report")
    if tree.definition["bands"]["names_by_threshold"]["red_below"] != \
            mir._resolve(tree.defn, aggregation["sub_threshold_set"]["below"]):
        problems.append("C9: the watchlist edge and the red edge are spelled separately")
    stats["C9 aggregation rules"] = len(aggregation)


# ---------------------------------------------------------------------------------------------
# C10 the front-end decision, as a supply-chain fact
# ---------------------------------------------------------------------------------------------


def check_front_end(tree: Tree, problems: list[str], stats: dict[str, int]) -> None:
    lock = tree.lock or ""
    cargo = tree.cargo or ""
    deny = tree.deny or ""
    for crate in BANNED_PARSERS:
        if re.search(rf'^name = "{re.escape(crate)}"$', lock, re.MULTILINE):
            problems.append(f"C10: {crate} is in Cargo.lock; ADR-0007 §6 is the decision that it is not "
                            "allowed in, because its C runtime would make `cc` a build requirement on "
                            "every cd.yml target")
        if f'name = "{crate}"' not in deny:
            problems.append(f"C10: deny.toml does not ban {crate}, so it can creep back in through a "
                            "transitive dependency on a branch that skipped a check")
        if crate in cargo:
            problems.append(f"C10: Cargo.toml depends on {crate}")
    for needle in ("cc", "musl", "grammar", "error recovery"):
        if needle not in tree.adr_text.lower():
            problems.append(f"C10: the ADR refuses tree-sitter without saying {needle!r}; §6's whole "
                            "argument is the same one ADR-0003 and ADR-0006 made, and it has to be "
                            "written down to be re-usable")
    if "tree-sitter" not in tree.adr_text.lower():
        problems.append("C10: the ADR never names the front end it rejected")
    stats["C10 banned parsers"] = len(BANNED_PARSERS)


# ---------------------------------------------------------------------------------------------
# C11 the shared file selector
# ---------------------------------------------------------------------------------------------


def check_scope(tree: Tree, problems: list[str], stats: dict[str, int]) -> None:
    scope = tree.definition["scope"]
    if scope["precedence"] != "exclude > noise > (source AND an enabled language)":
        problems.append("C11: glob precedence must be pinned in one sentence; ADR-0006 §7.2 already "
                        "pinned it and the two must not drift")
    # ADR-0006 §7.2 owns the noise list; MI must use it verbatim, which is the ticket's "same set of
    # files" requirement.
    row = None
    for cells in table_rows(tree.adr0006 or "", "| `paths.noise`"):
        row = cells
    if row is None:
        for line in (tree.adr0006 or "").splitlines():
            if line.strip().startswith("| `paths.noise`"):
                row = [cell.strip() for cell in line.strip().strip("|").split("|")]
                break
    if row is None:
        SKIPPED.append("C11: ADR-0006's `paths.noise` row: not found, skipped")
    else:
        default_cell = row[2] if len(row) > 2 else ""
        globs = re.findall(r'"([^"]+)"', default_cell)
        if not globs:
            globs = [g for g in re.findall(r"`([^`]+)`", default_cell) if "," not in g]
        if sorted(globs) != sorted(scope["noise_globs"]):
            problems.append("C11: MI's noise globs are not ADR-0006 §7.2's defaults verbatim; the +/- "
                            "column and the MI column would then describe different sets of files, which "
                            "is the one thing the ticket forbids")
            problems.append(f"C11:   ADR-0006: {sorted(globs)}")
            problems.append(f"C11:   ADR-0007: {sorted(scope['noise_globs'])}")
        else:
            stats["C11 noise globs shared with ADR-0006"] = len(globs)
    tests = scope["tests_default"]
    if tests["key"] != "paths.include_tests":
        problems.append("C11: the tests opt-in key must be `paths.include_tests`, declared once and "
                        "shared with the diffstat")
    if tests["default"] is not False:
        problems.append("C11: tests are opt-in for MI per the ticket, so the default must be false — "
                        "and it must be stated, not hidden, which is what the caveat line is for")
    if key_row(tree.adr_text, "paths.include_tests") is None:
        problems.append("C11: `paths.include_tests` is not in the ADR's declared-key table; a config key "
                        "that no ADR declares is a key nobody may read (ADR-0006 §7.3)")
    for key in ("languages.enabled", "index.file_size_cap_mib", "paths.noise", "paths.exclude"):
        if key not in tree.adr_text:
            problems.append(f"C11: the ADR must name the config key {key!r} it depends on, and the "
                            "analyzer that reads it")
    if "not config" not in tree.adr_text.lower() and "is_config" not in tree.adr_text:
        problems.append("C11: the ADR must say which of its numbers are deliberately not configurable")


def key_row(adr: str, key: str) -> list[str] | None:
    for cells in table_rows(adr, f"`{key}`"):
        return cells
    return None


# ---------------------------------------------------------------------------------------------
# C12 documentation wiring
# ---------------------------------------------------------------------------------------------


def check_wiring(tree: Tree, problems: list[str], stats: dict[str, int]) -> None:
    if SCRIPT_NAME not in (tree.ci_sh or ""):
        problems.append("C12: scripts/ci.sh does not run scripts/" + SCRIPT_NAME + "; an ADR whose "
                        "checker nothing runs rots the way ADR-0006 §2 describes")
    if SCRIPT_NAME not in (tree.ci_doc or ""):
        problems.append(f"C12: docs/ci.md does not name {SCRIPT_NAME}")
    if SCRIPT_NAME not in (tree.testing_doc or ""):
        problems.append(f"C12: docs/testing.md does not name {SCRIPT_NAME}")
    if ADR_NAME not in (tree.readme or ""):
        problems.append(f"C12: README's ADR list does not link {ADR_NAME}")
    if "ADR-0007" not in (tree.ci_doc or ""):
        problems.append("C12: docs/ci.md does not name ADR-0007, so a reader cannot find why the "
                        "checker exists")
    if "mi_reference.py" not in (tree.ci_doc or "") + (tree.testing_doc or ""):
        problems.append("C12: nothing documents scripts/mi_reference.py as the oracle the Rust port is "
                        "checked against")
    if "ADR-0006" not in tree.adr_text or "ADR-0005" not in tree.adr_text:
        problems.append("C12: this ADR must cite the ADRs whose decisions it inherits (ADR-0005's "
                        "dependency budget, ADR-0006's cache keys and config table)")
    stats["C12 wired documents"] = 5


# ---------------------------------------------------------------------------------------------
# the run
# ---------------------------------------------------------------------------------------------


def run_checks(tree: Tree, stats: dict[str, int]) -> list[str]:
    problems: list[str] = []
    check_adr(tree, problems, stats)
    check_constants_block(tree, problems, stats)
    check_bands(tree, problems, stats)
    check_ladder(tree, problems, stats)
    check_languages(tree, problems, stats)
    check_vectors(tree, problems, stats)
    check_second_opinion(tree, problems, stats)
    check_aggregates(tree, problems, stats)
    check_aggregation_prose(tree, problems, stats)
    check_front_end(tree, problems, stats)
    check_scope(tree, problems, stats)
    check_wiring(tree, problems, stats)
    return problems


# ---------------------------------------------------------------------------------------------
# C13 prove the checks fire
# ---------------------------------------------------------------------------------------------


def build_fixture(dest: Path, adr: Path, definition: Path) -> Path:
    """A scratch repository whose artifacts are the real ones, so a mutation isolates one rule."""
    (dest / "docs" / "adr").mkdir(parents=True, exist_ok=True)
    (dest / "schema").mkdir(parents=True, exist_ok=True)
    shutil.copy2(adr, dest / "docs" / "adr" / ADR_NAME)
    shutil.copy2(REPO / "docs" / "adr" / ADR0006_NAME, dest / "docs" / "adr" / ADR0006_NAME)
    shutil.copy2(definition, dest / "schema" / "mi-definition.json")
    shutil.copytree(REPO / "tests/testdata/mi", dest / "tests" / "testdata" / "mi")
    for name in ("deny.toml", "Cargo.lock", "Cargo.toml", "README.md", ".gitignore"):
        if (REPO / name).is_file():
            shutil.copy2(REPO / name, dest / name)
    (dest / "docs" / "ci.md").write_text(f"scripts/{SCRIPT_NAME}.py ADR-0007 scripts/mi_reference.py\n",
                                         encoding="utf-8")
    (dest / "docs" / "testing.md").write_text(f"{SCRIPT_NAME}.py mi_reference.py\n", encoding="utf-8")
    (dest / "scripts").mkdir(parents=True, exist_ok=True)
    (dest / "scripts" / "ci.sh").write_text(f"python3 scripts/{SCRIPT_NAME}.py\n", encoding="utf-8")
    return dest


def mutate(kind: str, fixture: Path) -> None:
    """Break exactly one rule of the fixture."""

    def rewrite(rel: str, fn) -> None:
        path = fixture / rel
        path.write_text(fn(path.read_text(encoding="utf-8")), encoding="utf-8")

    def json_rewrite(rel: str, key_path: list[str], value: object) -> None:
        path = fixture / rel
        doc = json.loads(path.read_text(encoding="utf-8"))
        node = doc
        for part in key_path[:-1]:
            node = node[part]
        node[key_path[-1]] = value
        path.write_text(json.dumps(doc, indent=2) + "\n", encoding="utf-8")

    if kind == "constants-drift":
        # a prose field no vector reads: only the ADR/JSON diff can see it
        rewrite("schema/mi-definition.json",
                lambda t: t.replace("IEEE Software 13(5), 1996", "IEEE Software 14(5), 1996"))
    elif kind == "formula-constant":
        # a real constant: the vectors disagree too, and the drill says so
        rewrite("schema/mi-definition.json", lambda t: t.replace('"a": 171.0', '"a": 170.0'))
    elif kind == "band-drift":
        rewrite("schema/mi-definition.json",
                lambda t: t.replace('"amber_below": 50.0', '"amber_below": 46.0'))
    elif kind == "bands-are-config":
        rewrite("schema/mi-definition.json", lambda t: t.replace('"is_config": false', '"is_config": true'))
    elif kind == "reason-ladder":
        rewrite("schema/mi-definition.json",
                lambda t: t.replace('      "vacuous",\n', '', 1))
    elif kind == "adr-phrase":
        # the phrase appears three times, once inside the ADR's own copy of the constants block, so
        # losing it is a three-check event and the drill says so
        rewrite("docs/adr/" + ADR_NAME, lambda t: t.replace("never 0", "never zero"))
    elif kind == "vector-mi":
        json_rewrite("tests/testdata/mi/vectors/rust_basic.json", ["expect", "mi"], 56.2)
    elif kind == "vector-table":
        json_rewrite("tests/testdata/mi/vectors/rust_tiny.json", ["expect", "operators", "total"], 9)
    elif kind == "vector-boundary":
        json_rewrite("tests/testdata/mi/vectors/rust_tiny.json", ["expect", "statements"], 58)
    elif kind == "drop-vacuous-vectors":
        for name in ("rust_comment_only", "python_comment_only", "python_docstring"):
            (fixture / f"tests/testdata/mi/vectors/{name}.json").unlink()
    elif kind == "identical-pair":
        shutil.copy2(fixture / "tests/testdata/mi/src/rust_reformat_a.rs",
                     fixture / "tests/testdata/mi/src/rust_reformat_b.rs")
    elif kind == "aggregate-drift":
        json_rewrite("tests/testdata/mi/aggregates/commit_small.json", ["expect", "aggregate"], 30.0)
    elif kind == "treesitter":
        rewrite("Cargo.lock", lambda t: t + '\n[[package]]\nname = "tree-sitter"\nversion = "0.22.6"\n')
    elif kind == "unbanned":
        rewrite("deny.toml", lambda t: t.replace('name = "tree-sitter"', 'name = "not-tree-sitter"'))
    elif kind == "noise-globs":
        rewrite("docs/adr/" + ADR0006_NAME, lambda t: t.replace('"vendor/**"', '"vendor/**","docs/**"'))
    elif kind == "doc-wiring":
        (fixture / "scripts" / "ci.sh").write_text("#!/usr/bin/env bash\necho hi\n", encoding="utf-8")
    elif kind == "undecided":
        rewrite("docs/adr/" + ADR_NAME, lambda t: t.replace("- **Status:** Accepted",
                                                            "- **Status:** Accepted\n\nTODO: pick the bands"))
    else:  # pragma: no cover - a coding mistake in the table below
        raise SystemExit(f"unknown mutation {kind!r}")


#: (mutation, the check ids that must fire — and only those)
MUTATIONS = [
    ("constants-drift", {"C2"}),
    ("formula-constant", {"C2", "C6", "C7"}),
    ("band-drift", {"C3"}),
    ("bands-are-config", {"C3"}),
    ("reason-ladder", {"C2", "C4", "C6"}),
    ("adr-phrase", {"C1", "C2", "C4"}),
    ("vector-mi", {"C6", "C7"}),
    ("vector-table", {"C6"}),
    ("vector-boundary", {"C6"}),
    ("drop-vacuous-vectors", {"C6"}),
    ("identical-pair", {"C6"}),
    ("aggregate-drift", {"C8"}),
    ("treesitter", {"C10"}),
    ("unbanned", {"C10"}),
    ("noise-globs", {"C11"}),
    ("doc-wiring", {"C12"}),
    ("undecided", {"C1"}),
]


def self_test(adr: Path, definition: Path) -> int:
    """C13: build a clean fixture, then break one rule at a time and name the check that catches it."""
    problems: list[str] = []
    with tempfile.TemporaryDirectory(prefix="mi-definition-drill-") as tmp:
        base = Path(tmp) / "base"
        build_fixture(base, adr, definition)
        SKIPPED.clear()
        clean = run_checks(Tree(base, base / "docs" / "adr" / ADR_NAME, base / "schema" / "mi-definition.json"), {})
        if clean:
            problems.append("C13: the copied artifacts are not clean on their own; the drill would blame "
                            "the mutation for an existing failure: "
                            + " | ".join(clean[:3]))
        for kind, expected in MUTATIONS:
            scratch = Path(tmp) / f"case-{kind}"
            shutil.copytree(base, scratch)
            mutate(kind, scratch)
            SKIPPED.clear()
            found = run_checks(Tree(scratch, scratch / "docs" / "adr" / ADR_NAME,
                                    scratch / "schema" / "mi-definition.json"), {})
            fired = {problem.split(":")[0] for problem in found}
            if fired != expected:
                problems.append(
                    f"C13: mutating {kind!r} fired {sorted(fired) or 'nothing'}; this script's table "
                    f"says {sorted(expected)}. First lines: "
                    + " | ".join(problem.splitlines()[0] for problem in found[:3])
                )
        for note in dict.fromkeys(SKIPPED):
            print(f"WARN {note}")
    if problems:
        for problem in problems:
            print(f"FAIL {problem}")
        print(f"\n{len(problems)} self-test problem(s)", file=sys.stderr)
        return 1
    print(f"ok  {SCRIPT_NAME}: {len(MUTATIONS)} mutations each fired exactly the check that claims them")
    return 0


# ---------------------------------------------------------------------------------------------
# entry point
# ---------------------------------------------------------------------------------------------


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=(__doc__ or "").splitlines()[0])
    parser.add_argument("--adr", type=Path, default=DEFAULT_ADR)
    parser.add_argument("--definition", type=Path, default=DEFAULT_DEFINITION)
    parser.add_argument("--root", type=Path, default=REPO)
    parser.add_argument("--quiet", action="store_true")
    parser.add_argument("--stats", action="store_true")
    parser.add_argument("--self-test", action="store_true", dest="self_test")
    args = parser.parse_args(argv)

    if args.self_test:
        return self_test(DEFAULT_ADR, DEFAULT_DEFINITION)

    if not args.adr.is_file():
        raise SystemExit(f"no ADR at {args.adr} (ADR-0007 has not been written yet?)")
    if not args.definition.is_file():
        raise SystemExit(f"no definition at {args.definition}")

    stats: dict[str, int] = {}
    try:
        tree = Tree(args.root.resolve(), args.adr, args.definition)
    except (OSError, json.JSONDecodeError) as err:
        raise SystemExit(f"cannot read the artifacts: {err}") from err
    problems = run_checks(tree, stats)

    for problem in problems:
        print(f"FAIL {problem}")
    if args.stats:
        for key, value in sorted(stats.items()):
            print(f"  {key}: {value}")
    for note in dict.fromkeys(SKIPPED):
        print(f"WARN {note}")
    if not problems:
        if not args.quiet:
            print("ok: ADR-0007, schema/mi-definition.json, the golden vectors and the dependency graph "
                  "all describe the same Maintainability Index")
        return 0
    print(f"{len(problems)} Maintainability-Index definition problem(s)")
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
