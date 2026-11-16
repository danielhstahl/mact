#!/usr/bin/env python3
"""Check ADR-0008 (the per-commit metric model): the ADR, schema/metric-model.json, the reference
implementation, the golden vectors in tests/testdata/metrics and the sibling docs say one thing — read
from those artifacts, not from prose.

This is the dev-time checker for ADR-0008 (ticket mact-by1.4), the sibling of
scripts/validate_tool_contract.py, scripts/validate_evidence.py, scripts/validate_run_lifecycle.py,
scripts/validate_isolation.py, scripts/validate_crate_layout.py, scripts/validate_fathom_layout.py and
scripts/validate_mi_definition.py. ADR-0008's load-bearing artifacts are a *selection policy* and an
*aggregation*, so this checker answers the question that matters for a rule set four renderers share:
**would two implementations pick the same records and print the same number?**

Checks performed (all of them, every run):

  C1   the ADR: exists, has front matter (id/title/status/ticket/date), is accepted, is this ticket's,
       carries no unresolved marker, decides every question the ticket asked (a phrase checklist), and
       cites only check ids this script implements.
  C2   the ADR's `metric-model` and `metric-glyphs` blocks equal `schema/metric-model.json` field for
       field — both directions, no key invented on either side — and every pinned value agrees with the
       normative field it names. The duplication is on purpose; the checker is what keeps it honest.
  C3   the vocabularies are closed and documented: every drop reason belongs to a declared stage and
       appears in the ADR's reason table, every gap code and caveat code appears in the ADR, and the
       reference implementation names no code the model does not declare.
  C4   the row shape: every field the derivation emits — over every golden vector and the real sample
       tree — is declared in `row_shape`, the fields ADR-0006 §6.4 already names are still spelled the
       same there, and no derived value anywhere is null.
  C5   the golden vectors: every required case exists, every expected file exists with a hand-written
       note and a matching model_version, every fixture record is a well-formed envelope, and
       re-deriving every fixture equals its expected file.
  C6   the ADR's worked example (§3.8) re-derives exactly from a freshly built sample tree, so the
       numbers quoted in the document cannot rot.
  C7   the recipes win: the reference's per-run coverage totals and per-group benchmark medians equal
       the *recorded jq output* in docs/evidence-format.md §12 recipes 2 and 6 for the same tree —
       a second implementation, in another language, that this ADR claims to agree with.
  C8   arithmetic and state properties over every derived row: a pooled rate lies between the rates it
       pooled; a vacuous row has no percent; a zero is not unknown; a unitless metric is never
       comparable; a group's records agree on their unit; every gap, caveat and state is declared.
  C9   the `runs/` refusal: the model forbids the index from reading it, the reference reads none, and
       the ADR states the advisory policy in as many words.
  C10  documentation wiring: the ADR is linked from README's ADR list, named in docs/ci.md and
       docs/testing.md, this script is wired into scripts/ci.sh's schema step, and ADR-0006 and
       ADR-0007 still point here for what they deferred.
  C11  the checker itself is not a lie: --self-test builds a scratch copy of the artifacts, breaks one
       rule at a time, and asserts that the check it names is the one that fires.

Usage:
    scripts/validate_metric_model.py [--adr PATH] [--model PATH] [--vectors PATH] [--root PATH]
                                     [--quiet] [--stats] [--self-test]

Exit status: 0 everything consistent, 1 a contract violation (one line each), 2 usage or environment
error (a missing ADR, unreadable JSON, no sample tree).
"""
from __future__ import annotations

import argparse
import json
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent
sys.path.insert(0, str(HERE))

import metric_model_reference as mmr  # noqa: E402

DEFAULT_ADR = REPO / "docs/adr/0008-per-commit-metric-model.md"
DEFAULT_MODEL = REPO / "schema/metric-model.json"
DEFAULT_VECTORS = REPO / "tests/testdata/metrics"
DEFAULT_FORMAT_DOC = REPO / "docs/evidence-format.md"
TICKET = "mact-by1.4"
CHECK_IDS = [f"C{i}" for i in range(1, 12)]

# C1: every question the ticket asked, and a string that only appears in the ADR if it was answered.
# Each entry is (label, [any-of strings]); at least one string must appear in the ADR text.
REQUIRED_DECISIONS: list[tuple[str, list[str]]] = [
    ("coverage rate sums numerators and denominators", ["numerators and denominators"]),
    ("never averages per-file percentages", ["mean of per-file percentages", "average of per-file"]),
    ("newest run per (kind, workflow, task)", ["(kind, workflow, task)"]),
    ("newest is defined via run_id sortability", ["lexical maximum", "run_id"]),
    ("three coverage runs", ["three coverage runs"]),
    ("two coverage workflows / tasks", ["two coverage workflows"]),
    ("sum of file records is not an invariant", ["sum(file records) == total"]),
    ("found == 0 is vacuous, not 0 %", ["vacuous"]),
    ("one glyph for vacuous in the timeline", ["`—`"]),
    ("percent_only excluded from the plotted number", ["percent_only"]),
    ("low_confidence excluded and caveated", ["low_confidence"]),
    ("run_status values that feed the number", ["run_status_eligible", "Only `success` feeds"]),
    ("excluded runs still shown somewhere", ["excluded_runs"]),
    ("benchmark identity keeps sample", ["(commit, kind, run_id, name, labels, sample)"]),
    ("median over samples within the newest run", ["median"]),
    ("min/max spread over retained runs", ["min`/`max", "min and max over every retained run"]),
    ("tie rule for an even sample count", ["mean of the two middle values"]),
    ("missing-value rule", ["absent at this commit is absent", "group absent at this commit"]),
    ("unit differing or absent is never compared", ["not comparable", "never comparable"]),
    ("act_log rows are context", ["act_log"]),
    ("step_duration_s absent on act 0.2.62..0.2.75", ["0.2.76"]),
    ("headline metrics default to everything", ["allow-all", "benchmarks.allow"]),
    ("durable signal is the envelope run_status", ["run_status` field on every"]),
    ("four row states", ["`no_evidence`"]),
    ("mixed state defined", ["`mixed`"]),
    ("no evidence renders unknown, not passed", ["never \"passed\"", "never as a pass"]),
    ("meta.json is advisory only", ["advisory"]),
    ("meta.json cannot break a render", ["break a render"]),
    ("unknown vs zero vs empty", ["unknown", "vacuous", "zero"]),
    ("rows keyed by the full 40-hex SHA", ["40-hex", "40 hex"]),
    ("display truncates to 7", ["7"]),
    ("root commit has no delta", ["root commit"]),
    ("merge commit uses the first parent", ["first parent"]),
]


# ----------------------------------------------------------------------------------------------- utils


def fenced(text: str, info: str) -> str | None:
    """The body of the first ```{info} fenced block, or None."""
    match = re.search(rf"^```{re.escape(info)}\s*\n(.*?)^```", text, re.S | re.M)
    return match.group(1) if match else None


def parse_pins(block: str) -> dict[str, str]:
    """Parse `key = value` lines of a pinned block into a dict."""
    out: dict[str, str] = {}
    for line in block.splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        if "=" not in line:
            out[f"<unparsed:{line}>"] = ""
            continue
        key, value = line.split("=", 1)
        out[key.strip()] = value.strip()
    return out


def front_matter(text: str) -> dict[str, str]:
    match = re.match(r"^---\n(.*?)\n---\n", text, re.S)
    if not match:
        return {}
    out: dict[str, str] = {}
    for line in match.group(1).splitlines():
        if ":" in line:
            key, value = line.split(":", 1)
            out[key.strip()] = value.strip()
    return out


def sample_tree(root: Path | None) -> Path:
    """A freshly built sample tree, in --root's dir or a tempdir (caller cleans up)."""
    if root is not None:
        out = root / "sample-evidence"
    else:
        out = Path(tempfile.mkdtemp(prefix="metric-model-")) / "sample"
    out.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run([sys.executable, str(REPO / "scripts/sample_evidence.py"), "-o", str(out), "-q"],
                   check=True, capture_output=True, text=True)
    return out


def derive_all(model: mmr.Model, vectors: Path, tree: Path) -> dict[str, dict]:
    """Every row this model can derive: all golden vectors plus the sample tree, keyed `case:sha`."""
    rows: dict[str, dict] = {}
    for name, fixture in mmr.vector_cases(vectors):
        for sha, row in mmr.derive_case(fixture, model).items():
            rows[f"{name}:{sha}"] = row
    for sha, row in mmr.derive_tree(mmr.read_tree(tree, model, mmr.Config()), model).items():
        rows[f"sample:{sha}"] = row
    return rows


def walk_values(value):
    """Every value in a nested JSON structure, for the no-nulls and shape checks."""
    if isinstance(value, dict):
        for key, sub in value.items():
            yield key, sub
            yield from walk_values(sub)
    elif isinstance(value, list):
        for sub in value:
            yield from walk_values(sub)


def fmt_number(value) -> str:
    """How jq and this reference both print a JSON number: shortest round-trip, ints without a .0."""
    if isinstance(value, bool):
        return str(value)
    if isinstance(value, int):
        return str(value)
    return repr(value)


# ------------------------------------------------------------------------------------------------
# C1
# ------------------------------------------------------------------------------------------------


def check_adr(adr: str, problems: list[str], stats: dict[str, int]) -> None:
    if adr[:4] != "---\n":
        problems.append("C1 the ADR does not start with front matter")
    fm = front_matter(adr)
    for key in ("id", "title", "status", "ticket", "date"):
        if key not in fm:
            problems.append(f"C1 front matter is missing {key!r}")
    if fm.get("id") != "ADR-0008":
        problems.append(f"C1 front matter id is {fm.get('id')!r}, expected ADR-0008")
    if fm.get("status") != "accepted":
        problems.append(f"C1 status is {fm.get('status')!r}, expected accepted")
    if fm.get("ticket") != TICKET:
        problems.append(f"C1 ticket is {fm.get('ticket')!r}, expected {TICKET}")
    for marker in ("TODO", "TBD", "FIXME", "to be decided", "???", "XXX"):
        if marker in adr:
            problems.append(f"C1 unresolved marker {marker!r} in the ADR")
    for label, needles in REQUIRED_DECISIONS:
        if not any(needle in adr for needle in needles):
            problems.append(f"C1 the ADR does not state: {label}")
    stats["decisions"] = len(REQUIRED_DECISIONS)
    for cid in sorted(set(re.findall(r"\bC(\d+)\b", adr))):
        if f"C{cid}" not in CHECK_IDS:
            problems.append(f"C1 the ADR cites check C{cid}, which this script does not implement")
    for section in ("## 0. The decision", "## 2. The pipeline", "## 3. Coverage", "## 4. Benchmark",
                    "## 5. Action success", "## 6. Cross-cutting", "## Considered alternatives",
                    "## What checks what", "## Consequences", "## Open questions"):
        if section not in adr:
            problems.append(f"C1 the ADR has no {section!r} section")


# ------------------------------------------------------------------------------------------------
# C2
# ------------------------------------------------------------------------------------------------

# each ADR pin -> the normative field in the model it must agree with (dotted path, joined-lists allowed)
PIN_PATHS = {
    "percent_decimals": "coverage.percent_decimals",
    "percent_rounding": "coverage.rounding_flat",
    "percent_input": "coverage.percent_input",
    "run_status_eligible": "retention.run_status_eligible",
    "selection_key": "selection.group_key",
    "newest_by": "selection.newest_by",
    "coverage_run_total": "coverage.per_run_total_source",
    "coverage_pool": "coverage.pool_flat",
    "measure_order": "retention.measure_order",
    "benchmark_group_key": "benchmarks.group_key",
    "benchmark_value": "benchmarks.value",
    "median_even_n": "benchmarks.median_even_n_flat",
    "benchmark_spread": "benchmarks.spread_flat",
    "benchmark_context": "benchmarks.context_sources",
    "action_states": "action_success.states",
    "action_group_key": "action_success.group_key",
}


def lookup(doc: dict, dotted: str):
    node = doc
    for part in dotted.split("."):
        if not isinstance(node, dict) or part not in node:
            return None
        node = node[part]
    return node


def as_pin(value) -> str:
    if isinstance(value, list):
        return ", ".join(str(v) for v in value)
    return str(value)


def check_adr_blocks(adr: str, doc: dict, problems: list[str], stats: dict[str, int]) -> None:
    pins = doc.get("adr_pins", {})
    if not pins:
        problems.append("C2 the model declares no adr_pins block")
        return
    block = fenced(adr, "metric-model")
    if block is None:
        problems.append("C2 the ADR has no ```metric-model block")
        return
    pins_in_adr = parse_pins(block)
    for key, value in pins.items():
        if key == "$comment":
            continue
        if key not in pins_in_adr:
            problems.append(f"C2 the ADR's metric-model block omits {key}")
        elif pins_in_adr[key] != value:
            problems.append(f"C2 {key}: ADR says {pins_in_adr[key]!r}, the model says {value!r}")
    for key in pins_in_adr:
        if key not in pins and not key.startswith("<unparsed"):
            problems.append(f"C2 the ADR's metric-model block invents {key}")
    for key in [k for k in pins_in_adr if not k.startswith("<unparsed")]:
        if key == "$comment":
            continue
        path = PIN_PATHS.get(key)
        if path is None:
            continue  # prose-only pins (vacuous, zero, meta_json) are checked by their own sections
        actual = lookup(doc, path)
        if actual is None:
            problems.append(f"C2 adr_pin {key} points at nothing: {path} is not in the model")
        elif as_pin(actual) != pins[key]:
            problems.append(f"C2 adr_pin {key} says {pins[key]!r} but {path} is {as_pin(actual)!r}")
    stats["pins"] = len([k for k in pins if k != "$comment"])

    glyphs = fenced(adr, "metric-glyphs")
    if glyphs is None:
        problems.append("C2 the ADR has no ```metric-glyphs block")
        return
    parsed = parse_pins(glyphs)
    uni, ascii_ = doc["display"]["unicode"], doc["display"]["ascii"]
    for key, want in sorted(uni.items()):
        if key not in parsed:
            problems.append(f"C2 the ADR glyph block omits {key}")
            continue
        parts = [part.strip() for part in parsed[key].split("|")]
        if len(parts) != 2 or parts[0] != want or parts[1] != ascii_[key]:
            problems.append(f"C2 glyph {key}: ADR {parsed[key]!r}, model {want!r} | {ascii_[key]!r}")
        if want == ascii_[key] and key not in ("caveat_marker", "context"):
            problems.append(f"C2 glyph {key} has no distinct unicode form")
    for key in parsed:
        if key not in uni and not key.startswith("<unparsed"):
            problems.append(f"C2 the ADR glyph block invents {key}")
    if set(uni) != set(ascii_):
        problems.append("C2 the unicode and ascii glyph tables have different keys")
    stats["glyphs"] = len(uni)


# ------------------------------------------------------------------------------------------------
# C3
# ------------------------------------------------------------------------------------------------


def check_vocabularies(adr: str, doc: dict, problems: list[str], stats: dict[str, int]) -> None:
    if doc.get("adr") != "ADR-0008" or doc.get("ticket") != TICKET:
        problems.append(f"C3 the model does not name itself ADR-0008 / {TICKET}")
    stages = doc["pipeline"]["stages"]
    reasons = doc["drop_reasons"]
    codes = [r["code"] for r in reasons]
    if len(set(codes)) != len(codes):
        problems.append("C3 drop_reasons contains a duplicate code")
    for reason in reasons:
        if reason["stage"] not in stages:
            problems.append(f"C3 drop reason {reason['code']} has undeclared stage {reason['stage']!r}")
        if f"`{reason['code']}`" not in adr:
            problems.append(f"C3 the ADR never names the drop reason {reason['code']}")
    table = re.findall(r"^\| `([a-z_]+)` \| (walk|retain|select|aggregate) \|", adr, re.M)
    if {code for code, _ in table} != set(codes):
        missing = set(codes) - {code for code, _ in table}
        extra = {code for code, _ in table} - set(codes)
        if missing:
            problems.append(f"C3 the ADR's reason table is missing {sorted(missing)}")
        if extra:
            problems.append(f"C3 the ADR's reason table invents {sorted(extra)}")
    for code, stage in table:
        declared = next((r["stage"] for r in reasons if r["code"] == code), None)
        if declared and declared != stage:
            problems.append(f"C3 {code}: table says stage {stage!r}, model says {declared!r}")

    for column, codes_declared in doc["gap_reasons"].items():
        if column == "$comment" or column == "priority":
            continue
        for code in codes_declared:
            if f"`{code}`" not in adr:
                problems.append(f"C3 the ADR never names the {column} gap code {code}")

    for key in doc["row_shape"].get("gaps_adr0006", []):
        if f"`{key}`" not in adr:
            problems.append(f"C3 gaps.{key} is ADR-0006's key in an object two ADRs write, and the ADR "
                            f"has to name it for the boundary to be findable")

    caveat_all = doc["caveat_codes"]["all"]
    if len(set(caveat_all)) != len(caveat_all):
        problems.append("C3 caveat_codes.all has duplicates")
    for code in caveat_all:
        if f"`{code}`" not in adr:
            problems.append(f"C3 the ADR never names the caveat code {code}")
    for column in ("coverage", "benchmarks"):
        unknown = set(doc["caveat_codes"][column]) - set(caveat_all)
        if unknown:
            problems.append(f"C3 caveat_codes.{column} lists undeclared codes {sorted(unknown)}")

    source = (HERE / "metric_model_reference.py").read_text(encoding="utf-8")
    used_reasons = set(re.findall(r'model\.reason\("([a-z_]+)"\)', source))
    used_caveats = set(re.findall(r'model\.caveat\("([a-z_]+)"', source))
    undeclared = used_reasons - set(codes)
    if undeclared:
        problems.append(f"C3 the reference uses undeclared drop reasons {sorted(undeclared)}")
    unknown_caveats = used_caveats - set(caveat_all)
    if unknown_caveats:
        problems.append(f"C3 the reference uses undeclared caveats {sorted(unknown_caveats)}")
    unused = set(codes) - used_reasons - {"vacuous"}  # vacuous is a caveat *and* a reason
    if unused:
        problems.append(f"C3 declared drop reasons the reference never emits: {sorted(unused)}")
    stats["reasons"] = len(codes)


# ------------------------------------------------------------------------------------------------
# C4
# ------------------------------------------------------------------------------------------------


def check_row_shape(rows: dict[str, dict], doc: dict, adr0006: str, problems: list[str]) -> None:
    shape = doc["row_shape"]
    # ADR-0006 section 6.4 pins the *names* of the fields this ADR fills in; renaming one is a breaking
    # change to a row schema this ADR does not own, so the model has to keep declaring every one of them.
    pinned_by_adr0006 = {"coverage": {"percent", "found", "hit", "runs"},
                         "benchmarks": {"name", "unit", "value", "min", "max", "samples", "run_id",
                                        "flags"},
                         "row": {"action_success", "gaps"}}
    for section, names in pinned_by_adr0006.items():
        missing = names - set(shape.get(section, []))
        if missing:
            problems.append(f"C4 ADR-0006 §6.4 names {sorted(missing)} under {section}; the model no "
                            f"longer declares them, which renames a row schema this ADR does not own")
    for key, sub in shape.items():
        if key == "$comment":
            continue
        if key == "nulls":
            continue
        if isinstance(sub, str):
            continue

    def keys_of(row: dict, path: str) -> set[str]:
        if path == "row":
            return set(row)
        if path == "coverage":
            return set(row.get("coverage") or {})
        if path == "coverage_secondary":
            return {k for s in (row.get("coverage") or {}).get("secondary", []) for k in s}
        if path == "benchmarks":
            return {k for b in row.get("benchmarks") or [] for k in b}
        if path == "action_detail":
            return {k for a in row.get("action_detail") or [] for k in a}
        if path == "excluded_runs":
            return {k for e in row.get("excluded_runs") or [] for k in e}
        if path == "gaps":
            return set(row.get("gaps") or {})
        return set()

    for path, declared in shape.items():
        if path in ("$comment", "nulls") or not isinstance(declared, list):
            continue
        seen: set[str] = set()
        for row in rows.values():
            seen |= keys_of(row, path)
        extra = seen - set(declared)
        if extra:
            problems.append(f"C4 derived rows emit undeclared {path} fields {sorted(extra)}")

    for where, required in (("coverage", ("percent", "found", "hit", "runs")),
                            ("action_success", ("action_success",)),
                            ("gaps", ("gaps",))):
        for field in required:
            if f'"{field}"' not in adr0006:
                problems.append(f"C4 ADR-0006 §6.4 no longer shows {where}'s {field!r} field")

    for name, row in rows.items():
        for key, value in walk_values(row):
            if value is None:
                problems.append(f"C4 {name}: row contains a null under {key!r}")
        if "coverage" in row:
            found = row["coverage"]["found"]
            if found <= 0:
                problems.append(f"C4 {name}: percent present with found={found}")


# ------------------------------------------------------------------------------------------------
# C5
# ------------------------------------------------------------------------------------------------


def check_vectors(model: mmr.Model, vectors: Path, doc: dict, problems: list[str],
                  stats: dict[str, int]) -> None:
    required = doc["vectors"]["required_cases"]
    fixture_dir = vectors / "fixtures"
    present = {p.name for p in fixture_dir.iterdir() if p.is_dir()} if fixture_dir.is_dir() else set()
    for case in required:
        if case not in present:
            problems.append(f"C5 golden vector {case} has no fixture directory")
    for case in sorted(present - set(required)):
        problems.append(f"C5 fixture {case} is not in the model's required_cases")
    stats["vectors"] = len(required)

    expected_dir = vectors / "expected"
    for case in sorted(required):
        expected_path = expected_dir / f"{case}.json"
        if not expected_path.is_file():
            problems.append(f"C5 {case} has no expected file")
            continue
        doc_expected = json.loads(expected_path.read_text(encoding="utf-8"))
        if not doc_expected.get("notes", "").strip():
            problems.append(f"C5 {case}'s expected file carries no hand-written note")
        if doc_expected.get("model_version") != doc["model_version"]:
            problems.append(f"C5 {case}'s expected file was written for model_version "
                            f"{doc_expected.get('model_version')}, not {doc['model_version']}")
        derived = mmr.derive_case(fixture_dir / case, model)
        if derived != doc_expected.get("rows"):
            problems.append(f"C5 {case}: the reference disagrees with its expected file")

    # every fixture record has to be a well-formed envelope, apart from the one fixture whose whole
    # point is that it is not
    for case in sorted(present):
        if case == "dirty_tree":
            continue
        for path in sorted((fixture_dir / case).rglob("*.jsonl")):
            if path.name.startswith("."):
                continue
            for n, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
                if not line.strip():
                    continue
                raw = json.loads(line)
                problem = mmr.envelope_problem(raw, path.parent.parent.name)
                if problem:
                    problems.append(f"C5 {case} fixture {path.name}:{n} is malformed: {problem}")


# ------------------------------------------------------------------------------------------------
# C6
# ------------------------------------------------------------------------------------------------


def check_worked_example(adr: str, model: mmr.Model, tree: Path, problems: list[str]) -> None:
    block = fenced(adr, "sample-row")
    if block is None:
        problems.append("C6 the ADR has no ```sample-row block")
        return
    pins = parse_pins(block)
    read = mmr.read_tree(tree, model, mmr.Config())
    sha = pins.get("commit", "")
    if sha not in read.retained and sha not in read.readable:
        problems.append(f"C6 the worked example's commit {sha!r} is not in the sample tree")
        return
    row = mmr.derive_row(sha, read, model)
    cov = row.get("coverage") or {}
    benches = row.get("benchmarks") or []

    def want(key: str, actual) -> None:
        if key not in pins:
            problems.append(f"C6 the worked example omits {key}")
        elif pins[key] != str(actual):
            problems.append(f"C6 worked example {key}: ADR {pins[key]!r}, derived {str(actual)!r}")

    want("coverage.percent", cov.get("percent"))
    want("coverage.hit", cov.get("hit"))
    want("coverage.found", cov.get("found"))
    want("coverage.measure", cov.get("measure"))
    want("coverage.run_count", len(cov.get("runs", [])))
    want("benchmarks.groups", len(benches))
    want("action_success", row["action_success"])
    want("excluded_runs", len(row.get("excluded_runs", [])))
    want("caveats", "; ".join(row.get("caveats", [])) or "none")
    want("gaps", "; ".join(f"{k} {v}" for k, v in sorted((row.get("gaps") or {}).items())) or "none")
    secondary = pins.get("coverage.secondary", "")
    derived_secondary = "; ".join(
        f"{s['measure']} {s['percent']}" for s in cov.get("secondary", [])) or "none"
    if secondary != derived_secondary:
        problems.append(f"C6 worked example coverage.secondary: ADR {secondary!r}, "
                        f"derived {derived_secondary!r}")

    def named(key: str, prefix: str, actual) -> None:
        value = pins.get(key, "")
        if not value.startswith(prefix):
            problems.append(f"C6 worked example {key} should name {prefix!r}")
            return
        if actual is not None and str(actual) not in value:
            problems.append(f"C6 worked example {key}: ADR {value!r}, derived {actual!r}")

    named("benchmarks.median", "tokenizer_throughput",
          fmt_number(next((b["value"] for b in benches
                           if b["name"] == "tokenizer_throughput"), None)))
    named("benchmarks.samples", "tokenizer_throughput",
          next((b["samples"] for b in benches if b["name"] == "tokenizer_throughput"), None))
    spread = next((f"[{fmt_number(b['min'])}, {fmt_number(b['max'])}]" for b in benches
                   if b["name"] == "tokenizer_throughput"), None)
    named("benchmarks.min/max", "tokenizer_throughput", spread)


# ------------------------------------------------------------------------------------------------
# C7
# ------------------------------------------------------------------------------------------------


def recipe_block(doc: str, heading_prefix: str) -> list[str]:
    """The first fenced output block under the §12 recipe whose heading starts with the prefix."""
    match = re.search(rf"^### {re.escape(heading_prefix)}.*?^```sh.*?^\`\`\`.*?^```(.*?)^```",
                      doc, re.S | re.M)
    if not match:
        return []
    return [line for line in match.group(1).strip().splitlines() if line.strip()]


def check_recipes(model: mmr.Model, tree: Path, format_doc: Path, problems: list[str],
                  stats: dict[str, int]) -> None:
    doc = format_doc.read_text(encoding="utf-8")
    first_commit = sorted(p.name for p in (tree / "coverage").iterdir() if p.is_dir())[0]
    read = mmr.read_tree(tree, model, mmr.Config())
    selected, _ = mmr.select(read.retained[first_commit], model)
    eligible, _ = mmr.split_scraped_coverage(selected, model)
    totals = mmr.coverage_run_totals(eligible, model)

    rows = recipe_block(doc, "2 ·")
    if len(rows) < 2:
        problems.append("C7 could not read recipe 2's recorded output from docs/evidence-format.md")
    else:
        stats["recipe2"] = len(rows)
        for line in rows:
            run_id, status, source, hit, found, pct = line.split("\t")
            mine = totals.get(run_id, {}).get("lines")
            if mine is None:
                problems.append(f"C7 recipe 2 lists run {run_id} with no lines total in this tree")
                continue
            if int(mine[1]) != int(found) or int(mine[0]) != int(hit):
                problems.append(f"C7 recipe 2 {run_id}: recipe says hit/found {hit}/{found}, "
                                f"the reference says {mine[0]}/{mine[1]}")
            rate = mmr.round_half_away(100 * int(hit) / int(found), 2)
            if f"{rate:.2f}" != pct:
                problems.append(f"C7 recipe 2 {run_id}: recipe prints {pct}, reference prints "
                                f"{rate:.2f}")
        if len(rows) != len({line.split('\t')[0] for line in rows}):
            problems.append("C7 recipe 2's recorded output has a duplicate run")

    rows = recipe_block(doc, "6 ·")
    if len(rows) < 2:
        problems.append("C7 could not read recipe 6's recorded output from docs/evidence-format.md")
    else:
        stats["recipe6"] = len(rows)
        benches = mmr.benchmarks_of(selected, read.retained[first_commit], model)["rows"]
        index = {(b["name"], mmr.canon(b["labels"])): b for b in benches}
        if len(index) != len(rows):
            problems.append(f"C7 recipe 6 prints {len(rows)} groups, the reference derived "
                            f"{len(index)}")
        for line in rows:
            name, labels, n, median, unit = line.split("\t")
            group = index.get((name, labels))
            if group is None:
                problems.append(f"C7 recipe 6 group {name} {labels} is not one the reference derived")
                continue
            if group["samples"] != int(n):
                problems.append(f"C7 recipe 6 {name}: recipe says n={n}, reference says "
                                f"samples={group['samples']}")
            if fmt_number(group["value"]) != median:
                problems.append(f"C7 recipe 6 {name}: recipe prints median {median}, reference "
                                f"prints {fmt_number(group['value'])}")
            if (group.get("unit") or "unit_missing") != unit:
                problems.append(f"C7 recipe 6 {name}: recipe prints unit {unit!r}, reference "
                                f"prints {group.get('unit')!r}")


# ------------------------------------------------------------------------------------------------
# C8
# ------------------------------------------------------------------------------------------------


def check_properties(model: mmr.Model, rows: dict[str, dict], tree: Path, doc: dict,
                     problems: list[str], stats: dict[str, int]) -> None:
    gap_codes = set(doc["gap_reasons"]["coverage"]) | set(doc["gap_reasons"]["benchmarks"])
    caveat_codes = set(doc["caveat_codes"]["all"])
    states = set(doc["action_success"]["states"])
    seen_states: set[str] = set()
    stats["rows"] = len(rows)
    for name, row in rows.items():
        seen_states.add(row["action_success"])
        if row["action_success"] not in states:
            problems.append(f"C8 {name}: action_success {row['action_success']!r} is not declared")
        for column, code in (row.get("gaps") or {}).items():
            if code not in gap_codes:
                problems.append(f"C8 {name}: gap code {code!r} for {column} is not declared")
            if code == "vacuous" and "vacuous" not in row.get("caveats", []):
                problems.append(f"C8 {name}: gap vacuous without the caveat")
        if "coverage" in row and row["gaps"].get("coverage"):
            problems.append(f"C8 {name}: has a coverage number and a coverage gap")
        for caveat in row.get("caveats", []):
            if caveat not in caveat_codes:
                problems.append(f"C8 {name}: caveat {caveat!r} is not declared")
        for bench in row.get("benchmarks", []):
            if "unit" not in bench and bench["comparable"]:
                problems.append(f"C8 {name}: {bench['name']} has no unit and is comparable")
            if bench["min"] > bench["value"] or bench["value"] > bench["max"]:
                problems.append(f"C8 {name}: {bench['name']} value {bench['value']} is outside "
                                f"[{bench['min']}, {bench['max']}]")
            if bench["samples"] < 1 or bench["run_count"] < 1:
                problems.append(f"C8 {name}: {bench['name']} reports an empty population")
        if row["action_success"] == "no_evidence" and ("coverage" in row or "benchmarks" in row):
            problems.append(f"C8 {name}: no_evidence but a number appeared")
    for state in sorted(states - seen_states):
        problems.append(f"C8 declared action state {state!r} is produced by no vector: add the "
                        f"fixture or drop the state")

    # the pooled bound: a size-weighted mean lies between the rates it pooled
    read = mmr.read_tree(tree, model, mmr.Config())
    for sha in sorted(read.retained):
        selected, _ = mmr.select(read.retained[sha], model)
        eligible, _ = mmr.split_scraped_coverage(selected, model)
        totals = mmr.coverage_run_totals(eligible, model)
        rates = [v[0] / v[1] for t in totals.values() for v in t.values() if v[1] > 0]
        pooled_hits = sum(v[0] for t in totals.values() for v in t.values())
        pooled_found = sum(v[1] for t in totals.values() for v in t.values())
        row = mmr.derive_row(sha, read, model)
        if "coverage" not in row or not rates or pooled_found <= 0:
            continue
        pooled = row["coverage"]["hit"] / row["coverage"]["found"]
        if not (min(rates) - 1e-9 <= pooled <= max(rates) + 1e-9):
            problems.append(f"C8 {sha[:7]}: pooled rate {pooled:.4f} is outside the pooled runs' "
                            f"[{min(rates):.4f}, {max(rates):.4f}]")


# ------------------------------------------------------------------------------------------------
# C9
# ------------------------------------------------------------------------------------------------


def check_runs_refusal(adr: str, doc: dict, tree: Path, model: mmr.Model,
                       problems: list[str]) -> None:
    policy = doc["action_success"]["meta_json_policy"]
    if policy["index_path_reads_runs"] is not False:
        problems.append("C9 the model does not forbid the index from reading runs/")
    if doc["walk"]["never_walk"] != ["runs", "incoming"]:
        problems.append(f"C9 walk.never_walk is {doc['walk']['never_walk']!r}, expected runs and incoming")
    read = mmr.read_tree(tree, model, mmr.Config())
    for drop in read.drops:
        if drop.where.startswith("runs/") or drop.where.startswith("incoming/"):
            problems.append(f"C9 the reference touched {drop.where}")
    for sha, recs in read.retained.items():
        for rec in recs:
            if rec.path.startswith(("runs/", "incoming/")):
                problems.append(f"C9 {sha[:7]}: a record was read from {rec.path}")
    if not read.files:
        problems.append("C9 the sample tree produced nothing to check")
    for phrase in ("the index never reads `runs/`", "advisory", "can never break a render",
                   "never read by the index"):
        if phrase not in adr:
            problems.append(f"C9 the ADR does not state the runs/ policy: {phrase!r}")


# ------------------------------------------------------------------------------------------------
# C10
# ------------------------------------------------------------------------------------------------


def check_wiring(problems: list[str]) -> None:
    readme = (REPO / "README.md").read_text(encoding="utf-8")
    if "0008-per-commit-metric-model.md" not in readme:
        problems.append("C10 README's ADR list does not link this ADR")
    for doc in ("docs/ci.md", "docs/testing.md"):
        if "validate_metric_model.py" not in (REPO / doc).read_text(encoding="utf-8"):
            problems.append(f"C10 {doc} does not name scripts/validate_metric_model.py")
    ci = (REPO / "scripts/ci.sh").read_text(encoding="utf-8")
    if "validate_metric_model.py" not in ci:
        problems.append("C10 scripts/ci.sh does not run scripts/validate_metric_model.py")
    for sibling, path in (("ADR-0006", "docs/adr/0006-fathom-workspace-layout.md"),
                          ("ADR-0007", "docs/adr/0007-maintainability-index.md")):
        text = (REPO / path).read_text(encoding="utf-8")
        if "ADR-0008" not in text:
            problems.append(f"C10 {sibling} no longer defers anything to ADR-0008")
    schema = REPO / "schema/metric-model.json"
    if not schema.is_file():
        problems.append("C10 schema/metric-model.json is missing")


# ------------------------------------------------------------------------------------------------
# C11 self-test
# ------------------------------------------------------------------------------------------------


def self_test(adr_path: Path, model_path: Path, vectors: Path) -> int:
    """Break one rule at a time in a scratch copy; the check named has to be the one that fires."""
    import validate_metric_model as own  # noqa: PLC0415

    with tempfile.TemporaryDirectory(prefix="metric-model-selftest-") as tmp:
        root = Path(tmp)
        adr_copy = root / "adr.md"
        model_copy = root / "model.json"
        vec_copy = root / "vectors"
        shutil.copy(adr_path, adr_copy)
        shutil.copy(model_path, model_copy)
        shutil.copytree(vectors, vec_copy)
        tree = sample_tree(root)
        base_adr = adr_copy.read_text(encoding="utf-8")
        base_model_text = model_copy.read_text(encoding="utf-8")
        adr0006 = (REPO / "docs/adr/0006-fathom-workspace-layout.md").read_text(encoding="utf-8")

        def run() -> dict[str, list[str]]:
            """Re-run every check against the scratch copies and bucket the findings by check id."""
            model = mmr.Model.load(model_copy)
            doc = json.loads(model_copy.read_text(encoding="utf-8"))
            adr_text = adr_copy.read_text(encoding="utf-8")
            problems: list[str] = []
            own.check_adr(adr_text, problems, {})
            own.check_adr_blocks(adr_text, doc, problems, {})
            own.check_vocabularies(adr_text, doc, problems, {})
            own.check_runs_refusal(adr_text, doc, tree, model, problems)
            try:
                rows = derive_all(model, vec_copy, tree)
            except Exception as exc:  # noqa: BLE001 - a model this broken must be *named*, not swallowed
                problems.append(f"C8 derivation crashed: {type(exc).__name__}: {exc}")
                rows = {}
            if rows:
                own.check_row_shape(rows, doc, adr0006, problems)
                own.check_properties(model, rows, tree, doc, problems, {})
                own.check_worked_example(adr_text, model, tree, problems)
            try:
                own.check_vectors(model, vec_copy, doc, problems, {})
            except Exception as exc:  # noqa: BLE001
                problems.append(f"C5 the vectors crashed: {type(exc).__name__}: {exc}")
            try:
                own.check_recipes(model, tree, DEFAULT_FORMAT_DOC, problems, {})
            except Exception as exc:  # noqa: BLE001
                problems.append(f"C7 the recipe comparison crashed: {type(exc).__name__}: {exc}")
            found: dict[str, list[str]] = {cid: [] for cid in CHECK_IDS}
            for problem in problems:
                cid = problem.split()[0]
                if cid in found:
                    found[cid].append(problem)
            return found

        def reset() -> None:
            adr_copy.write_text(base_adr, encoding="utf-8")
            model_copy.write_text(base_model_text, encoding="utf-8")

        def mutate_adr(old: str, new: str = "removed decision") -> None:
            assert old in base_adr, old
            adr_copy.write_text(base_adr.replace(old, new, 1), encoding="utf-8")

        def mutate_model(path: list[str], value) -> None:
            doc = json.loads(base_model_text)
            node = doc
            for part in path[:-1]:
                node = node[part]
            if value is _REMOVE:
                del node[path[-1]]
            else:
                node[path[-1]] = value
            model_copy.write_text(json.dumps(doc), encoding="utf-8")

        cases: list[tuple[str, str]] = []

        def expect(cid: str, label: str, broken) -> None:
            reset()
            broken()
            fired = run()
            hits = sorted(k for k, v in fired.items() if v)
            if cid and cid in hits:
                cases.append((label, "ok"))
            elif cid == "" and not hits:
                cases.append((label, "ok"))
            else:
                cases.append((label, f"FAILED: wanted {cid or 'nothing'}, got {hits or 'nothing'}"))

        expect("", "baseline: a clean copy raises nothing", lambda: None)
        expect("C1", "wrong ticket in the ADR front matter",
               lambda: mutate_adr("ticket: mact-by1.4", "ticket: mact-by1.9"))
        expect("C3", "the model names a different ticket",
               lambda: mutate_model(["ticket"], "mact-by1.9"))
        expect("C1", "a decision the ADR no longer states",
               lambda: mutate_adr("two coverage workflows"))
        expect("C1", "an unresolved marker",
               lambda: mutate_adr("## 1. Context", "## 1. Context\n\nTODO: finish this.\n"))
        expect("C2", "the pinned decimals disagree with the model",
               lambda: mutate_model(["adr_pins", "percent_decimals"], "2"))
        expect("C2", "the glyph table disagrees with the ADR",
               lambda: mutate_model(["display", "unicode", "mixed"], "X"))
        expect("C2", "a pin points at a field that no longer exists",
               lambda: mutate_model(["coverage", "pool_flat"], "different arithmetic"))
        expect("C3", "a declared drop reason vanished from the model",
               lambda: mutate_model(["drop_reasons"], [r for r in
                                                       json.loads(base_model_text)["drop_reasons"]
                                                       if r["code"] != "vacuous"]))
        expect("C3", "a gap key that is ADR-0006's and the ADR stops naming",
               lambda: mutate_model(["row_shape", "gaps_adr0006"],
                                    json.loads(base_model_text)["row_shape"]["gaps_adr0006"]
                                    + ["no_blobs"]))
        expect("C3", "a caveat the reference emits is not declared",
               lambda: mutate_model(["caveat_codes", "all"],
                                    [c for c in json.loads(base_model_text)["caveat_codes"]["all"]
                                     if c != "context_only"]))
        expect("C4", "a derived field is not declared in row_shape",
               lambda: mutate_model(["row_shape", "benchmarks"],
                                    [b for b in json.loads(base_model_text)["row_shape"]["benchmarks"]
                                     if b != "samples"]))
        expect("C5", "a golden vector with no note and empty rows",
               lambda: (vec_copy / "expected" / "vacuous_only.json").write_text(json.dumps(
                   {"case": "vacuous_only", "notes": "", "model_version": 1, "rows": {}})))
        expect("C5", "a required case with no fixture",
               lambda: mutate_model(["vectors", "required_cases"],
                                    json.loads(base_model_text)["vectors"]["required_cases"]
                                    + ["no_such_case"]))
        expect("C6", "the worked example block is gone",
               lambda: mutate_adr("```sample-row", "```nope-row"))
        expect("C6", "the worked example's percent is wrong",
               lambda: mutate_adr("coverage.percent    = 86.7", "coverage.percent    = 91.0"))
        expect("C7", "the count pair the recipes read is no longer the one used",
               lambda: mutate_model(["retention", "coverage_measures", "lines"],
                                    ["lines_hit_x", "lines_found_x"]))
        expect("C8", "an action state no vector ever produces",
               lambda: mutate_model(["action_success", "states"],
                                    json.loads(base_model_text)["action_success"]["states"]
                                    + ["unknowable"]))
        expect("C9", "the model stops refusing to read runs/",
               lambda: mutate_model(["action_success", "meta_json_policy",
                                     "index_path_reads_runs"], True))
        cases.append(_unparseable_case(mmr, model_copy, base_model_text))
        reset()

    width = max(len(label) for label, _ in cases)
    failures = 0
    for label, outcome in cases:
        if outcome != "ok":
            failures += 1
        print(f"  {label:<{width}}  {outcome}")
    print(f"self-test: {len(cases) - failures}/{len(cases)} broke the way they should")
    return 1 if failures else 0


class _Remove:
    pass


_REMOVE = _Remove()


def _unparseable_case(mmr_module, model_copy: Path, base_model_text: str) -> tuple[str, str]:
    """A model file that is not JSON at all has to be an environment error, not a silent pass."""
    model_copy.write_text("{ not json", encoding="utf-8")
    try:
        mmr_module.Model.load(model_copy)
        return ("unreadable model json", "FAILED: it parsed")
    except Exception:  # noqa: BLE001
        return ("unreadable model json", "ok")
    finally:
        model_copy.write_text(base_model_text, encoding="utf-8")


# ----------------------------------------------------------------------------------------------- main


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(prog="validate_metric_model.py",
                                 description="check ADR-0008 against its artifacts")
    ap.add_argument("--adr", default=str(DEFAULT_ADR))
    ap.add_argument("--model", default=str(DEFAULT_MODEL))
    ap.add_argument("--vectors", default=str(DEFAULT_VECTORS))
    ap.add_argument("--root", default=None, help="scratch dir for the sample tree (default: a tempdir)")
    ap.add_argument("--quiet", action="store_true")
    ap.add_argument("--stats", action="store_true")
    ap.add_argument("--self-test", action="store_true", dest="self_test")
    args = ap.parse_args(argv)

    if args.self_test:
        return self_test(Path(args.adr), Path(args.model), Path(args.vectors))

    adr_path = Path(args.adr)
    if not adr_path.is_file():
        print(f"no such ADR: {adr_path}", file=sys.stderr)
        return 2
    adr = adr_path.read_text(encoding="utf-8")
    try:
        model = mmr.Model.load(Path(args.model))
    except Exception as exc:  # noqa: BLE001
        print(f"cannot read {args.model}: {exc}", file=sys.stderr)
        return 2
    doc = model.doc
    scratch = Path(args.root) if args.root else Path(tempfile.mkdtemp(prefix="metric-model-"))
    tree = sample_tree(scratch)
    vectors = Path(args.vectors)

    problems: list[str] = []
    stats: dict[str, int] = {}
    rows = derive_all(model, vectors, tree)
    check_adr(adr, problems, stats)
    check_adr_blocks(adr, doc, problems, stats)
    check_vocabularies(adr, doc, problems, stats)
    check_row_shape(rows, doc,
                    (REPO / "docs/adr/0006-fathom-workspace-layout.md").read_text(encoding="utf-8"),
                    problems)
    check_vectors(model, vectors, doc, problems, stats)
    check_worked_example(adr, model, tree, problems)
    check_recipes(model, tree, DEFAULT_FORMAT_DOC, problems, stats)
    check_properties(model, rows, tree, doc, problems, stats)
    check_runs_refusal(adr, doc, tree, model, problems)
    check_wiring(problems)

    for problem in problems:
        print(problem)
    if args.stats:
        for key, value in sorted(stats.items()):
            print(f"  {key}: {value}")
    if not args.quiet:
        print(f"{'FAIL' if problems else 'OK'} metric model: {len(rows)} row(s) derived, "
              f"{len(problems)} finding(s)")
    if args.root is None:
        shutil.rmtree(scratch, ignore_errors=True)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
