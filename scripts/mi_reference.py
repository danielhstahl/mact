#!/usr/bin/env python3
"""ADR-0007's reference implementation: the Maintainability Index arithmetic, and nothing else.

This is the oracle named by docs/adr/0007-maintainability-index.md. It is *the* definition of the
arithmetic — every number it can produce is derived from schema/mi-definition.json, which is the
machine-readable form of the ADR — and it exists so that the Rust implementation (mact-by1.7) has
something to be wrong against before anyone looks at a trend line.

    MI_raw = 171 - 5.2*ln(V) - 0.23*G - 16.2*ln(LOC)
    MI     = clamp(MI_raw * 100 / 171, 0, 100)          # then rounded, see `round_storage`

What it deliberately does NOT do: tokenise. Turning a blob into (N, n, decision points, LOC) is the
language front end's job, and it is the *half the ticket pins by hand*: the golden vectors in
tests/testdata/mi/vectors carry hand-computed token tables, and this module re-derives V, G, LOC, MI
and the band from those tables and fails on any disagreement. A stdlib-only Python tokeniser would
be a second front end to keep in agreement, and the ADR does not need one; it needs arithmetic that
cannot drift.

Two rules make the numbers reproducible between two languages (ADR-0007 P6/P7):

  * every operation is an IEEE-754 binary64 operation, `log2` is the platform `log2` (never
    ln(n)/ln(2)), and the stored value is the *rounded* one, so a 1 ULP libm difference cannot
    reach a cache;
  * aggregation accumulates in path order and is a pure function of the commit's own file set, so
    the order commits were indexed in cannot reach the stored digit either.

Public API (imported by scripts/validate_mi_definition.py):

    Definition                 the parsed schema/mi-definition.json
    load_definition(path)      -> Definition
    halstead_volume(op_total, op_unique)         -> float | None
    cyclomatic(decision_counts, base=1)          -> int
    measure(defn, counts)      -> Measurement    # the per-file ladder: status, V, G, LOC, MI, band
    aggregate(defn, files)     -> dict           # LOC-weighted mean, hotspots, sub-threshold set
    commit_row(defn, files, parent_files) -> dict  # aggregate + delta + set transitions
    round_storage(defn, x)     -> float
    band_of(defn, mi)          -> str

CLI:
    scripts/mi_reference.py --explain            # print the constants and the ladder, then exit
    scripts/mi_reference.py --vector FILE ...    # re-derive each golden vector and diff it
    scripts/mi_reference.py --aggregate FILE ... # re-derive each aggregate fixture and diff it
    scripts/mi_reference.py --selftest           # the rounding/clamp/band/aggregation property cases

Exit status: 0 everything matched, 1 a mismatch or a failed property, 2 usage/environment error.
"""
from __future__ import annotations

import argparse
import json
import math
import sys
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

REPO = Path(__file__).resolve().parent.parent
DEFAULT_DEFINITION = REPO / "schema" / "mi-definition.json"


class DefinitionError(RuntimeError):
    """The definition file is missing, unreadable, or internally inconsistent."""


# ---------------------------------------------------------------------------------------------
# the definition
# ---------------------------------------------------------------------------------------------


@dataclass(frozen=True)
class Definition:
    raw: dict[str, Any]

    @property
    def formula(self) -> dict[str, Any]:
        return self.raw["formula"]

    @property
    def precision(self) -> dict[str, Any]:
        return self.raw["precision"]

    @property
    def bands(self) -> dict[str, Any]:
        return self.raw["bands"]

    @property
    def aggregation(self) -> dict[str, Any]:
        return self.raw["aggregation"]

    @property
    def measurability(self) -> dict[str, Any]:
        return self.raw["measurability"]

    @property
    def languages(self) -> dict[str, Any]:
        return self.raw["languages"]

    def language(self, name: str) -> dict[str, Any]:
        try:
            return self.raw["languages"][name]
        except KeyError as err:
            raise DefinitionError(f"no language {name!r} in the definition") from err


def load_definition(path: Path = DEFAULT_DEFINITION) -> Definition:
    try:
        raw = json.loads(path.read_text(encoding="utf-8"))
    except OSError as err:
        raise DefinitionError(f"cannot read {path}: {err}") from err
    except json.JSONDecodeError as err:
        raise DefinitionError(f"{path} is not JSON: {err}") from err
    for key in ("formula", "precision", "bands", "aggregation", "measurability", "languages"):
        if key not in raw:
            raise DefinitionError(f"{path}: missing required section {key!r}")
    return Definition(raw)


# ---------------------------------------------------------------------------------------------
# the two rounding primitives (ADR-0007 section 3.4)
# ---------------------------------------------------------------------------------------------


def round_half_away(x: float) -> float:
    """Rust's `f64::round`: half away from zero, one binary64 operation each side."""
    if math.isnan(x) or math.isinf(x):
        return x
    return math.copysign(math.floor(abs(x) + 0.5), x)


def round_storage(defn: Definition, x: float) -> float:
    """The stored/compared value: MI_1(x) = round_half_away(x * 10) / 10 for the default one
    decimal. `storage_decimals` is in the definition so a change is a bump, not a surprise."""
    decimals = int(defn.precision["storage_decimals"])
    if decimals <= 0:
        return round_half_away(x)
    factor = 10.0**decimals
    return round_half_away(x * factor) / factor


def round_storage_str(defn: Definition, x: float) -> str:
    """The JSON bytes a value is stored as: repr of the rounded double with trailing zeroes trimmed
    to `storage_decimals` (the definition's `json_float_encoding`)."""
    decimals = int(defn.precision["storage_decimals"])
    text = repr(float(x))
    if "." not in text and "e" not in text and "E" not in text:
        return text
    if decimals > 0 and "e" not in text and "E" not in text:
        whole, _, frac = text.partition(".")
        frac = (frac + "0" * decimals)[:decimals].rstrip("0")
        return f"{whole}.{frac}" if frac else whole
    return text


# ---------------------------------------------------------------------------------------------
# the inputs
# ---------------------------------------------------------------------------------------------


def halstead_volume(op_total: int, op_unique: int) -> float:
    """V = N * log2(n) (ADR-0007 section 3.1).

    `math.log2` is deliberate: the platform log2, not ln(n)/ln(2). The two differ by ULPs and the
    ADR pins which one.
    """
    if op_unique <= 0:
        raise ValueError("Halstead vocabulary n must be positive")
    return float(op_total) * math.log2(float(op_unique))


def cyclomatic(decision_counts: dict[str, int] | None, base: int = 1) -> int:
    """G = 1 + sum(decision points) over the whole file (ADR-0007 section 3.2)."""
    return int(base) + int(sum(int(v) for v in (decision_counts or {}).values()))


@dataclass
class Counts:
    """One file's front-end output: everything the tokeniser produces, before any arithmetic."""

    operator_total: int
    operator_unique: int
    operand_total: int
    operand_unique: int
    decision_counts: dict[str, int] = field(default_factory=dict)
    statements: int = 0
    decision_base: int = 1

    @classmethod
    def from_vector(cls, expect: dict[str, Any]) -> "Counts":
        ops = expect["operators"]
        operands = expect["operands"]
        return cls(
            operator_total=int(ops["total"]),
            operator_unique=int(ops["unique"]),
            operand_total=int(operands["total"]),
            operand_unique=int(operands["unique"]),
            decision_counts={k: int(v) for k, v in (expect.get("decision_points") or {}).items()},
            statements=int(expect["statements"]),
            decision_base=int(expect.get("decision_base", 1)),
        )

    def derived(self) -> dict[str, Any]:
        return {
            "operators": {"total": self.operator_total, "unique": self.operator_unique},
            "operands": {"total": self.operand_total, "unique": self.operand_unique},
            "decision_points": dict(self.decision_counts),
            "statements": self.statements,
        }


@dataclass
class Measurement:
    status: str
    loc: int
    volume: float | None = None
    volume_operand_basis: bool = False
    cyclomatic: int | None = None
    mi_raw: float | None = None
    mi: float | None = None
    band: str | None = None
    reason: str | None = None

    def to_json(self) -> dict[str, Any]:
        out: dict[str, Any] = {"status": self.status, "loc": self.loc}
        if self.reason:
            out["reason"] = self.reason
        if self.volume is not None:
            out["volume"] = self.volume
        if self.volume_operand_basis:
            out["volume_operand_basis"] = True
        if self.cyclomatic is not None:
            out["cyclomatic"] = self.cyclomatic
        if self.mi_raw is not None:
            out["mi_raw"] = self.mi_raw
        if self.mi is not None:
            out["mi"] = self.mi
        if self.band is not None:
            out["band"] = self.band
        return out


# ---------------------------------------------------------------------------------------------
# the per-file ladder (ADR-0007 section 3.6)
# ---------------------------------------------------------------------------------------------


def measure(defn: Definition, counts: Counts, status: str = "ok") -> Measurement:
    """Run one file down the ladder: reason precedence first, then the arithmetic, then rounding.

    `status` may arrive already non-`ok` (unsupported_language, oversized, parse_error, ...); the
    ladder never upgrades it. A `vacuous` verdict is produced here, from LOC and the vocabulary,
    because it is a property of the counts and not of the reader.
    """
    if status != "ok":
        return Measurement(status=status, loc=counts.statements, reason=status)

    loc = counts.statements
    if loc < int(defn.measurability["min_loc"]) or counts.operator_unique < int(
        defn.measurability["min_operator_unique"]
    ):
        # ln(0) is not a number and there is no other sensible one: no MI. Never 0. (section 3.6)
        return Measurement(status="vacuous", loc=loc, reason="vacuous")

    volume_operand_basis = defn.raw.get("precision", {}).get("volume_basis", "operators") == "operands"
    if volume_operand_basis:
        total, unique = counts.operand_total, counts.operand_unique
    else:
        total, unique = counts.operator_total, counts.operator_unique
    volume = halstead_volume(total, unique)
    if volume < 1.0:
        # With n >= 2 and N >= 1 this cannot happen; it is here so that a future change to the
        # vocabulary rule cannot hand ln() a non-positive argument unnoticed.
        return Measurement(status="vacuous", loc=loc, reason="vacuous")

    g = cyclomatic(counts.decision_counts, counts.decision_base)
    f = defn.formula
    mi_raw = (
        float(f["a"])
        - float(f["b_volume"]) * math.log(volume)
        - float(f["c_cyclomatic"]) * float(g)
        - float(f["d_loc"]) * math.log(float(loc))
    )
    scaled = mi_raw * float(f["scale_numerator"]) / float(f["scale_denominator"])
    clamped = min(max(scaled, float(f["clamp_min"])), float(f["clamp_max"]))
    mi = round_storage(defn, clamped)
    return Measurement(
        status="ok",
        loc=loc,
        volume=volume,
        volume_operand_basis=volume_operand_basis,
        cyclomatic=g,
        mi_raw=mi_raw,
        mi=mi,
        band=band_of(defn, mi),
    )


def band_of(defn: Definition, mi: float) -> str:
    """green / amber / red, with an exact edge owned by the better band (section 5)."""
    bands = defn.bands
    if mi < float(bands["names_by_threshold"]["red_below"]):
        return "red"
    if mi < float(bands["names_by_threshold"]["amber_below"]):
        return "amber"
    return "green"


# ---------------------------------------------------------------------------------------------
# the commit aggregate (ADR-0007 section 4)
# ---------------------------------------------------------------------------------------------


def _resolve(defn: Definition, dotted: str) -> float:
    """Resolve `"bands.red_below"` style references inside the definition, so one edge is spelled once."""
    node: Any = defn.raw
    for part in dotted.split("."):
        node = node[part]
    return float(node)


def aggregate(defn: Definition, files: list[dict[str, Any]]) -> dict[str, Any]:
    """LOC-weighted mean over the files that produced a measurement, plus the sets and hotspots.

    `files` is a list of `{path, status, loc, mi}` in any order; the accumulation order is pinned to
    path bytes ascending, so the stored digit does not depend on the order rows were written in.
    """
    agg_rules = defn.aggregation
    hotspots_n = int(agg_rules["hotspots_default"])
    watch_below = _resolve(defn, agg_rules["sub_threshold_set"]["below"])

    counts: dict[str, int] = {}
    for row in files:
        status = str(row.get("status", "ok"))
        counts[status] = counts.get(status, 0) + 1

    usable = [r for r in files if str(r.get("status", "ok")) == "ok"]
    usable.sort(key=lambda r: str(r["path"]).encode("utf-8"))

    total_loc = 0
    weighted = 0.0
    for row in usable:
        loc = int(row["loc"])
        total_loc += loc
        weighted += float(row["mi"]) * float(loc)

    out: dict[str, Any] = {
        "files": len(usable),
        "loc": total_loc,
        "counts": dict(sorted(counts.items())),
    }
    unanalysed = sum(
        n for status, n in counts.items() if status not in _non_caveat_statuses(defn)
    )
    out["unanalysed_paths"] = unanalysed

    if not usable or total_loc <= 0:
        out["aggregate"] = None
        out["aggregate_basis"] = "no measurable file in scope"
        out["hotspots"] = []
        out["sub_threshold_set"] = []
        out["amber_paths"] = []
        return out

    out["aggregate"] = round_storage(defn, weighted / float(total_loc))
    out["amber_paths"] = sorted(
        (str(r["path"]) for r in usable if float(r["mi"]) < float(defn.bands["names_by_threshold"]["amber_below"])),
        key=lambda p: p.encode("utf-8"),
    )
    out["sub_threshold_set"] = sorted(
        (str(r["path"]) for r in usable if float(r["mi"]) < watch_below),
        key=lambda p: p.encode("utf-8"),
    )
    ranked = sorted(
        usable,
        key=lambda r: (float(r["mi"]), -int(r["loc"]), str(r["path"]).encode("utf-8")),
    )
    out["hotspots"] = [
        {"path": str(r["path"]), "mi": float(r["mi"]), "loc": int(r["loc"])} for r in ranked[:hotspots_n]
    ]
    out["band"] = band_of(defn, out["aggregate"])
    return out


def _non_caveat_statuses(defn: Definition) -> set[str]:
    return {
        str(row["id"])
        for row in defn.raw["status_reasons"]
        if not row.get("in_caveat_count")
    }


def commit_row(defn: Definition, files: list[dict[str, Any]], parent_files: list[dict[str, Any]] | None) -> dict[str, Any]:
    """The row's `mi` section: the aggregate, the delta against parent #1, and the sub-threshold
    transitions (ADR-0007 section 4.3)."""
    row = aggregate(defn, files)
    if parent_files is None:
        row["delta_vs_parent"] = None
        row["sub_threshold_added"] = []
        row["sub_threshold_removed"] = []
        return row
    parent = aggregate(defn, parent_files)
    if row["aggregate"] is None or parent["aggregate"] is None:
        row["delta_vs_parent"] = None
    else:
        row["delta_vs_parent"] = round_storage(defn, float(row["aggregate"]) - float(parent["aggregate"]))
    here = set(row["sub_threshold_set"])
    there = set(parent["sub_threshold_set"])
    encode = lambda paths: sorted(paths, key=lambda p: p.encode("utf-8"))  # noqa: E731
    row["sub_threshold_added"] = encode(here - there)
    row["sub_threshold_removed"] = encode(there - here)
    return row


# ---------------------------------------------------------------------------------------------
# vector / fixture re-derivation
# ---------------------------------------------------------------------------------------------


def check_vector(defn: Definition, vector: dict[str, Any]) -> list[str]:
    """Re-derive a golden vector and report every disagreement with what the file claims."""
    problems: list[str] = []
    expect = vector["expect"]

    if not vector.get("derived", True):
        # A gap vector: the front end stopped before it could count anything. What is checkable is
        # that it stopped, that the reason is a real one, and that it promises no number.
        status = expect.get("status")
        if status == "ok":
            problems.append("derived is false but status is ok")
        if status not in defn.measurability["ordered_reasons"]:
            problems.append(f"status {status!r} is not a reason the ladder can produce")
        for key in ("mi", "volume", "cyclomatic", "loc", "statements", "band"):
            if expect.get(key) is not None:
                problems.append(f"a {status} file must carry no {key}, found {expect[key]!r}")
        return problems

    counts = Counts.from_vector(expect)

    # 1. the token tables have to be internally consistent before they can be trusted to pin V
    for label, key in (("operators", "operator_total"), ("operands", "operand_total")):
        table = expect.get(label, {})
        occurrences = table.get("occurrences")
        if occurrences is None:
            problems.append(f"{label}: no occurrences table, so total/unique cannot be re-derived")
            continue
        total = sum(int(v) for v in occurrences.values())
        unique = len(occurrences)
        if int(table["total"]) != total:
            problems.append(f"{label}.total claims {table['total']}, occurrences sum to {total}")
        if int(table["unique"]) != unique:
            problems.append(f"{label}.unique claims {table['unique']}, occurrences have {unique} distinct keys")
        if key == "operator_total":
            for name in occurrences:
                problem = _operator_contradiction(defn, vector["language"], name)
                if problem:
                    problems.append(f"operator {name!r}: {problem}")

    # 2. V, G and LOC, from the tables, by the pinned rules
    if expect["decision_points"] is not None:
        for name in expect["decision_points"]:
            if name not in _decision_ids(defn, vector["language"]):
                problems.append(f"decision point {name!r} is not a rule of language {vector['language']!r}")
    claimed_volume = expect.get("volume")
    derived_volume = halstead_volume(counts.operator_total, counts.operator_unique) if counts.operator_unique else None
    if claimed_volume is not None and derived_volume is not None and float(claimed_volume) != derived_volume:
        problems.append(f"volume claims {claimed_volume!r}, V = N*log2(n) is {derived_volume!r}")
    if claimed_volume is None and expect.get("status") == "ok":
        problems.append("status is ok, so volume must be present")
    derived_g = cyclomatic(counts.decision_counts, counts.decision_base)
    if int(expect["cyclomatic"]) != derived_g:
        problems.append(f"cyclomatic claims {expect['cyclomatic']}, 1 + decision points is {derived_g}")
    decision_sum = int(sum(int(v) for v in counts.decision_counts.values()))
    if decision_sum != derived_g - counts.decision_base:
        problems.append("decision point sum does not match cyclomatic")
    if int(expect["loc"]) != int(expect["statements"]):
        problems.append(f"loc claims {expect['loc']}, statements count is {expect['statements']}")

    # 3. MI, from V/G/LOC, by the pinned arithmetic
    measurement = measure(defn, counts)
    claimed = expect.get("mi")
    if measurement.status != "ok":
        if claimed is not None:
            problems.append(f"status is {measurement.status}, so mi must be absent, not {claimed!r}")
        if expect.get("status") != measurement.status:
            problems.append(f"status claims {expect.get('status')}, the ladder says {measurement.status}")
        return problems
    if claimed is None:
        problems.append("the ladder measures this file, but the vector has no mi")
        return problems
    if float(claimed) != measurement.mi:
        problems.append(f"mi claims {float(claimed)!r}, the formula gives {measurement.mi!r}")
    if expect.get("mi_raw") is not None:
        want, got = float(expect["mi_raw"]), float(measurement.mi_raw)
        if abs(got - want) > 1e-9 * max(1.0, abs(want)):
            problems.append(
                f"MI_raw claims {want!r}, the formula gives {got!r}; a different libm gets 1e-9 of "
                "relative room here and none at all in the stored value"
            )
    if expect.get("band") != measurement.band:
        problems.append(f"band claims {expect.get('band')!r}, the bands give {measurement.band!r}")
    if abs(mi_raw_boundary_distance(defn, measurement) - 1.0) > 1e-9:
        problems.append(
            f"mi {measurement.mi!r} sits within the boundary slack of a rounding or band edge; "
            "a 1 ULP libm difference could move the stored digit, so this fixture is not allowed"
        )
    return problems


def mi_raw_boundary_distance(defn: Definition, measurement: Measurement) -> float:
    """1.0 when the value is comfortably away from every boundary that could change what is stored
    or displayed, else the smallest distance (ADR-0007 section 3.4's hygiene rule)."""
    mi = float(measurement.mi)
    slack = float(defn.precision["boundary_slack"])
    clamped = min(max(float(measurement.mi_raw) * 100.0 / 171.0, 0.0), 100.0)
    distances = [
        abs(clamped * 10.0 - round_half_away(clamped * 10.0)),
        abs(mi - float(defn.bands["names_by_threshold"]["amber_below"])),
        abs(mi - float(defn.bands["names_by_threshold"]["red_below"])),
    ]
    smallest = min(distances)
    return 1.0 if smallest > slack else smallest


def check_aggregate(defn: Definition, fixture: dict[str, Any]) -> list[str]:
    problems: list[str] = []
    files = fixture["files"]
    parent = fixture.get("parent_files")
    derived = commit_row(defn, files, parent)
    expect = fixture["expect"]
    for key, want in expect.items():
        got = derived.get(key)
        if _normalise(want) != _normalise(got):
            problems.append(f"{key}: fixture claims {_brief(want)}, derived {_brief(got)}")
    return problems


def _normalise(value: Any) -> Any:
    if isinstance(value, float):
        return value
    if isinstance(value, dict):
        return {k: _normalise(v) for k, v in sorted(value.items())}
    if isinstance(value, list):
        return [_normalise(v) for v in value]
    if isinstance(value, int) and not isinstance(value, bool):
        return float(value)
    return value


def _brief(value: Any) -> str:
    text = json.dumps(value, sort_keys=True)
    return text if len(text) <= 220 else text[:217] + "..."


def _operator_contradiction(defn: Definition, language: str, name: str) -> str | None:
    """Why an operator token cannot be an operator, or None if the closed enumerations allow it.

    Identifier-shaped operator names (the R2/R2m call-shaped rule) cannot be enumerated, so they are
    allowed through; what this catches is the real contradiction: a token the definition calls an
    operand or calls ignored showing up in an operator table.
    """
    lang = defn.language(language)
    if name in lang["ignored_punctuation"]:
        return "is in the language's ignored punctuation, so it may not be an operator"
    if name in lang["operand_keywords"]:
        return "is one of the language's operand keywords, so it may not be an operator"
    if name in lang["operator_punctuation"] or name in lang["operator_keywords"]:
        return None
    if not any(ch.isalnum() or ch == "_" for ch in name):
        return "is punctuation that the language does not list as an operator"
    return None


def _decision_ids(defn: Definition, language: str) -> set[str]:
    return {str(rule["id"]) for rule in defn.language(language)["decision_points"]["rules"]}


def _operator_is_defined(defn: Definition, language: str, name: str) -> bool:
    return _operator_contradiction(defn, language, name) is None


# ---------------------------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------------------------


def explain(defn: Definition) -> int:
    f = defn.formula
    print(f"{defn.raw.get("adr", "ADR-0007")} / {defn.raw.get("ticket", "")} — the pinned definition")
    print()
    print(f"  {f['raw']}")
    print(f"  {f['scaled']}")
    print(f"  {f['halstead_volume']}   ({defn.precision['log2']})")
    print(f"  {f['cyclomatic']}")
    print()
    print(f"  constants: a={f['a']} b={f['b_volume']} c={f['c_cyclomatic']} d={f['d_loc']}")
    print(f"  scale: {f['scale_numerator']}/{f['scale_denominator']}  clamp: [{f['clamp_min']}, {f['clamp_max']}]")
    print(f"  stored at {defn.precision['storage_decimals']} decimal(s), {defn.precision['rounding']}")
    print(f"  bands: < {defn.bands['names_by_threshold']['red_below']} red, "
          f"< {defn.bands['names_by_threshold']['amber_below']} amber, else green "
          f"({defn.bands['edge_ownership']} owns an edge)")
    print(f"  weight: {defn.aggregation['weight']}, hotspots: worst {defn.aggregation['hotspots_default']}, "
          f"sub-threshold set: < {defn.aggregation['sub_threshold_set_below']}")
    print(f"  reasons, in ladder order: {', '.join(defn.measurability['ordered_reasons'])}")
    print()
    print("  worked example: N=56 n=16 ops, 2 ifs + 3 match arms + 1 `?`, 15 statements:")
    m = measure(defn, Counts(56, 16, 74, 20, {"if": 2, "match_arm": 3, "question": 1}, 15))
    print(f"    V = {m.volume!r}  G = {m.cyclomatic}  MI_raw = {m.mi_raw!r}  MI = {m.mi} ({m.band})")
    return 0


def run_vectors(defn: Definition, paths: list[Path]) -> int:
    failures = 0
    for path in paths:
        vector = json.loads(path.read_text(encoding="utf-8"))
        problems = check_vector(defn, vector)
        expect = vector["expect"]
        status = expect.get("status")
        mi = expect.get("mi")
        if problems:
            failures += 1
            print(f"FAIL {path}")
            for problem in problems:
                print(f"     {problem}")
        else:
            print(f"ok   {path}  [{status} loc={expect['loc']} mi={mi}]")
    if failures:
        print(f"\n{failures} vector(s) disagree with the definition", file=sys.stderr)
    return 1 if failures else 0


def run_aggregates(defn: Definition, paths: list[Path]) -> int:
    failures = 0
    for path in paths:
        fixture = json.loads(path.read_text(encoding="utf-8"))
        problems = check_aggregate(defn, fixture)
        if problems:
            failures += 1
            print(f"FAIL {path}")
            for problem in problems:
                print(f"     {problem}")
        else:
            derived = commit_row(defn, fixture["files"], fixture.get("parent_files"))
            print(f"ok   {path}  [aggregate={derived['aggregate']} loc={derived['loc']} files={derived['files']}]")
    if failures:
        print(f"\n{failures} aggregate fixture(s) disagree with the definition", file=sys.stderr)
    return 1 if failures else 0


def selftest(defn: Definition) -> int:
    """The properties the ADR claims about the arithmetic, independent of the fixture files."""
    problems: list[str] = []

    def case(name: str, got: Any, want: Any) -> None:
        if _normalise(got) != _normalise(want):
            problems.append(f"{name}: got {got!r}, want {want!r}")

    # rounding: half away from zero, applied to x*10.0 — which is why the two neighbours below both
    # land on 62.5: 62.449999999999996 * 10.0 is *exactly* 624.5 in binary64. The formula is what
    # has to agree, not a decimal-string intuition about "62.44999 rounds down".
    for x, want in (
        (62.449999999999996, 62.5),
        (62.450000000000003, 62.5),
        (62.449999, 62.4),
        (99.99999, 100.0),
        (0.04, 0.0),
    ):
        case(f"round_storage({x})", round_storage(defn, x), want)
    case("round_storage_str(100.0)", round_storage_str(defn, 100.0), "100")
    case("round_storage_str(62.4)", round_storage_str(defn, 62.4), "62.4")
    case("round_storage_str(71.25)", round_storage_str(defn, round_storage(defn, 71.25)), "71.3")

    # clamps at both ends, and the band edges.
    # The top of the range is not reachable: V >= 1 and LOC >= 1 make every subtracted term
    # non-negative, so MI_raw < 171 and MI < 100 for every measurable file. The clamp stays because
    # the published range is part of the contract, and it is what a leak through `vacuous` would hit.
    tiny = measure(defn, Counts(1, 2, 1, 2, {}, 1))
    case("one-operator one-line file is the reachable ceiling", tiny.mi, 99.9)
    case("reachable ceiling is under 100", tiny.mi < 100.0, True)
    case("top clamp is unreachable", measure(defn, Counts(1, 2, 1, 2, {}, 1)).mi <= 99.9, True)
    huge = measure(defn, Counts(9_000_000, 900_000, 9_000_000, 900_000, {"if": 40_000}, 400_000))
    case("enormous file clamps to 0", huge.mi, 0.0)
    # Placed at the *declared* edges rather than at numbers, because an exact edge belongs to the better
    # band and a self-test that hard-codes the numbers is a second copy of the edges.
    amber_edge = float(defn.bands["names_by_threshold"]["amber_below"])
    red_edge = float(defn.bands["names_by_threshold"]["red_below"])
    case("the amber edge is green", band_of(defn, amber_edge), "green")
    case("one tick under it is amber", band_of(defn, round_storage(defn, amber_edge - 0.1)), "amber")
    case("the red edge is amber", band_of(defn, red_edge), "amber")
    case("one tick under that is red", band_of(defn, round_storage(defn, red_edge - 0.1)), "red")

    # ln(0) is undefined: no number, and never 0
    zero_loc = Counts(12, 6, 20, 11, {"if": 1}, 0)
    case("LOC == 0 is vacuous", measure(defn, zero_loc).status, "vacuous")
    case("LOC == 0 has no mi", measure(defn, zero_loc).mi, None)
    one_vocab = Counts(30, 1, 40, 1, {"if": 1}, 10)
    case("n < 2 is vacuous", measure(defn, one_vocab).status, "vacuous")

    # determinism: the same counts, twice, byte for byte
    counts = Counts(56, 16, 74, 20, {"if": 2, "match_arm": 3, "question": 1}, 15)
    case("two runs agree", json.dumps(measure(defn, counts).to_json()), json.dumps(measure(defn, counts).to_json()))

    # aggregation: the LOC weight must dominate, and the order of the rows must not matter
    files = [
        {"path": "src/mod.rs", "status": "ok", "loc": 12, "mi": 95.0},
        {"path": "src/big.rs", "status": "ok", "loc": 900, "mi": 18.0},
    ]
    agg = aggregate(defn, files)
    case("LOC-weighted mean", agg["aggregate"], 19.0)  # (95*12 + 18*900)/912 = 19.013
    case("a per-file mean would be 56.5, which is the lie this fixes", round_storage(defn, (95.0 + 18.0) / 2.0), 56.5)
    case("LOC total", agg["loc"], 912)
    case("order does not matter", aggregate(defn, list(reversed(files)))["aggregate"], agg["aggregate"])
    case("hotspots worst-first", [h["path"] for h in agg["hotspots"]], ["src/big.rs", "src/mod.rs"])
    case("sub-threshold set", agg["sub_threshold_set"], ["src/big.rs"])
    empty = aggregate(defn, [{"path": "README.md", "status": "excluded", "loc": 0}])
    case("empty population is unknown", empty["aggregate"], None)
    case("empty population is not 0", "aggregate" in empty and empty["aggregate"] is None, True)

    # delta and the watchlist transitions
    # Both parents sit above the red edge but below green, so the transition is a *transition*: big.rs
    # falls into the watchlist by getting worse and old.rs leaves it by going away. Numbers here are
    # relative to the edges for the same reason the band cases above are.
    parent = [
        {"path": "src/big.rs", "status": "ok", "loc": 900, "mi": red_edge + 5.0},
        {"path": "src/old.rs", "status": "ok", "loc": 40, "mi": red_edge - 1.0},
    ]
    row = commit_row(defn, files, parent)
    case("delta vs parent", row["delta_vs_parent"], round_storage(defn, agg["aggregate"] - commit_row(defn, parent, None)["aggregate"]))
    case("watchlist added", row["sub_threshold_added"], ["src/big.rs"])
    case("watchlist removed", row["sub_threshold_removed"], ["src/old.rs"])
    case("no parent means no delta", commit_row(defn, files, None)["delta_vs_parent"], None)

    if problems:
        for problem in problems:
            print(f"FAIL {problem}", file=sys.stderr)
        print(f"\n{len(problems)} self-test case(s) failed", file=sys.stderr)
        return 1
    print(f"ok  mi_reference selftest: {34 - len(problems)} properties hold")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--definition", type=Path, default=DEFAULT_DEFINITION)
    parser.add_argument("--vector", type=Path, nargs="+", default=[])
    parser.add_argument("--aggregate", type=Path, nargs="+", default=[])
    parser.add_argument("--explain", action="store_true")
    parser.add_argument("--selftest", action="store_true")
    args = parser.parse_args(argv)

    try:
        defn = load_definition(args.definition)
    except DefinitionError as err:
        print(f"mi_reference: {err}", file=sys.stderr)
        return 2

    did = False
    rc = 0
    if args.explain:
        did = True
        rc |= explain(defn)
    if args.vector:
        did = True
        rc |= run_vectors(defn, args.vector)
    if args.aggregate:
        did = True
        rc |= run_aggregates(defn, args.aggregate)
    if args.selftest:
        did = True
        rc |= selftest(defn)
    if not did:
        parser.print_help()
        return 2
    return rc


if __name__ == "__main__":
    raise SystemExit(main())
