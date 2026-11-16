#!/usr/bin/env python3
"""ADR-0008's reference implementation: `.evidence` records in, one commit row out.

This is the oracle mact-by1.6's Rust reader is graded against, in the same role
`scripts/mi_reference.py` plays for ADR-0007: a deliberately simple, dependency-free implementation of
the rules in `schema/metric-model.json`, which is the machine-readable form of
`docs/adr/0008-per-commit-metric-model.md`. It loads that file rather than duplicating it, so the model
has exactly one spelling.

The four stages, named as `schema/metric-model.json` names them:

    walk      which bytes are read at all          (kind/<40-hex>/<run_id>.jsonl; dot-files skipped)
    retain    which records a number may use       (run_status, truncated, source precedence, config)
    select    the newest run per (kind, workflow, task)
    aggregate the arithmetic: pooled coverage rate, per-group benchmark median and spread, action state

Every record it reads is either retained or dropped with exactly one reason code from `drop_reasons`,
and every column with no number carries one reason code from `gap_reasons`. That is what makes a wrong
number answerable, and it is the shape of `--explain`.

Usage:
    scripts/metric_model_reference.py --tree "$EV"                    # one row per commit, JSON
    scripts/metric_model_reference.py --tree "$EV" --commit 2f7c1a4   # one commit (7-hex prefix ok)
    scripts/metric_model_reference.py --tree "$EV" --explain          # + the per-record keep/drop table
    scripts/metric_model_reference.py --tree "$EV" --table            # the timeline columns, ascii
    scripts/metric_model_reference.py --vectors                       # list the golden vector cases
    scripts/metric_model_reference.py --vectors --diff --quiet        # CI form: fail on drift
    scripts/metric_model_reference.py --vectors --write               # regenerate expected/ (deliberate)

Exit status: 0 ok, 1 a golden vector disagreed or the tree could not be read, 2 usage or missing input.
"""
from __future__ import annotations

import argparse
import json
import re
import sys
from dataclasses import dataclass, field
from decimal import Decimal, ROUND_HALF_UP
from pathlib import Path
from typing import Any

HERE = Path(__file__).resolve().parent
REPO = HERE.parent
DEFAULT_MODEL = REPO / "schema" / "metric-model.json"
DEFAULT_VECTORS = REPO / "tests" / "testdata" / "metrics"

# The highest evidence schema_version this reader understands. ADR-0002 section 11: refuse what you
# cannot read — skip and count, never guess.
MAX_SCHEMA_VERSION = 1

SHA_RE = re.compile(r"^[0-9a-f]{40}$")
RUN_ID_RE = re.compile(r"^[0-9]{8}T[0-9]{6}Z-[0-9a-f]{6}$")
REQUIRED_ENVELOPE = (
    "schema_version", "kind", "run_id", "commit", "workflow", "task",
    "category", "run_status", "tool", "started_at", "host",
)


# -------------------------------------------------------------------------------------------- model


class Model:
    """`schema/metric-model.json`, plus the handful of accessors this file needs."""

    def __init__(self, doc: dict[str, Any]):
        self.doc = doc
        self.walk: dict = doc["walk"]
        self.retention: dict = doc["retention"]
        self.selection: dict = doc["selection"]
        self.coverage: dict = doc["coverage"]
        self.benchmarks: dict = doc["benchmarks"]
        self.action: dict = doc["action_success"]
        self.display: dict = doc["display"]
        self.row_key: dict = doc["row_key"]
        self.gap_reasons: dict = doc["gap_reasons"]
        self.caveat_codes: dict = doc["caveat_codes"]
        self.reasons = {r["code"]: r for r in doc["drop_reasons"]}
        self.measure_fields: dict = self.retention["coverage_measures"]
        self.measure_order: list[str] = list(self.retention["measure_order"])
        self.eligible = set(self.retention["run_status_eligible"])
        self.successful = set(self.action["success_statuses"])
        self.precedence = {s: i for i, s in enumerate(self.retention["benchmark_precedence_within_run"])}
        self.suffix: str = self.walk["record_file_suffix"]

    @classmethod
    def load(cls, path: Path) -> "Model":
        with path.open(encoding="utf-8") as fh:
            return cls(json.load(fh))

    def reason(self, code: str) -> str:
        """Identity check: a reason this file uses but the model does not declare is a bug."""
        if code not in self.reasons:
            raise AssertionError(f"drop reason {code!r} is not declared in the model")
        return code

    def caveat(self, code: str, into: list[str]) -> None:
        if code not in into:
            assert code in self.caveat_codes["all"], f"caveat {code!r} is not declared in the model"
            into.append(code)


# -------------------------------------------------------------------------- canonical arithmetic


def canon(value: Any) -> str:
    """Canonical JSON for a labels object: keys ascending, no insignificant whitespace (ADR-0002 6.1)."""
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False)


def round_half_away(value: float, decimals: int) -> float:
    """Round half away from zero at `decimals`, going through the double's shortest repr.

    Going through `repr` instead of scaling in binary is what keeps 86.85 from deciding its own rounding
    by accident of representation. This is the only rounding coverage gets (ADR-0008 3.4).
    """
    quantum = Decimal(1).scaleb(-decimals)
    return float(Decimal(repr(value)).quantize(quantum, rounding=ROUND_HALF_UP))


def median(values: list[float]) -> float:
    """Median, ascending; even n is the mean of the two middle values — recipe 6's rule, pinned."""
    if not values:
        raise ValueError("median of an empty population")
    ordered = sorted(values)
    n = len(ordered)
    if n % 2:
        return ordered[(n - 1) // 2]
    return (ordered[n // 2 - 1] + ordered[n // 2]) / 2


def significant(value: float, digits: int) -> str:
    """Render at `digits` significant digits, half away from zero, trailing zeros trimmed."""
    if value == 0:
        return "0"
    dec = Decimal(repr(value))
    quantum = Decimal(1).scaleb(dec.adjusted() - (digits - 1))
    out = format(dec.quantize(quantum, rounding=ROUND_HALF_UP), "f")
    if "." in out:
        out = out.rstrip("0").rstrip(".")
    return out


# ------------------------------------------------------------------------------------- record view


@dataclass
class Rec:
    """One evidence record as read, plus where it came from."""

    path: str
    line: int
    raw: dict[str, Any]

    @property
    def kind(self) -> str:
        return self.raw["kind"]

    @property
    def run_id(self) -> str:
        return self.raw["run_id"]

    @property
    def workflow(self) -> str:
        return self.raw["workflow"]

    @property
    def task(self) -> str:
        return self.raw["task"]

    @property
    def category(self) -> str:
        return self.raw["category"]

    @property
    def run_status(self) -> str:
        return self.raw["run_status"]

    @property
    def flags(self) -> list[str]:
        return list(self.raw.get("flags") or [])

    @property
    def source(self) -> str:
        return str(self.raw.get("source", ""))

    @property
    def handle(self) -> str:
        """Short human handle for --explain."""
        if self.kind == "coverage":
            return str(self.raw.get("path") or self.raw.get("name") or self.raw.get("scope"))
        return str(self.raw.get("name"))

    @property
    def group(self) -> tuple:
        """Benchmark group key: (name, canon(labels), unit-or-None). Unit is part of the identity."""
        return (self.raw.get("name"), canon(self.raw.get("labels") or {}), self.raw.get("unit"))

    @property
    def selection_key(self) -> tuple:
        return (self.kind, self.workflow, self.task)

    def count_pairs(self, model: Model) -> dict[str, tuple[int, int]]:
        """Complete (hit, found) pairs this coverage record carries, by measure."""
        out: dict[str, tuple[int, int]] = {}
        for measure, (hit_key, found_key) in model.measure_fields.items():
            hit, found = self.raw.get(hit_key), self.raw.get(found_key)
            if isinstance(hit, int) and not isinstance(hit, bool) \
               and isinstance(found, int) and not isinstance(found, bool):
                out[measure] = (hit, found)
        return out


@dataclass
class Drop:
    sha: str
    where: str
    reason: str
    detail: str
    run_id: str = ""
    status: str = ""


@dataclass
class Config:
    """The two `fathom.toml` keys this derivation reads (ADR-0006 7.2). Deny wins."""

    allow: tuple[str, ...] = ()
    deny: tuple[str, ...] = ()

    def keeps(self, name: str) -> bool:
        if name in self.deny:
            return False
        if self.allow and name not in self.allow:
            return False
        return True


@dataclass
class Tree:
    """What one walk of one evidence root produced."""

    retained: dict[str, list[Rec]] = field(default_factory=dict)     # usable for a number
    readable: dict[str, list[Rec]] = field(default_factory=dict)     # any readable record, any status
    unreadable: dict[str, int] = field(default_factory=dict)         # records present but unreadable
    drops: list[Drop] = field(default_factory=list)
    files: int = 0
    records: int = 0

    def drop(self, sha: str, where: str, reason: str, detail: str,
             run_id: str = "", status: str = "") -> None:
        self.drops.append(Drop(sha, where, reason, detail, run_id, status))

    def drops_for(self, sha: str) -> list[Drop]:
        return [d for d in self.drops if d.sha == sha]


# -------------------------------------------------------------------------------------------------
# stages 1 + 2: walk and retain
# -------------------------------------------------------------------------------------------------


def envelope_problem(raw: Any, expect_kind: str) -> str | None:
    """None when the parsed line is a usable envelope of `expect_kind`, else why it is not."""
    if not isinstance(raw, dict):
        return "line is not a JSON object"
    for name in REQUIRED_ENVELOPE:
        if name not in raw:
            return f"envelope field {name!r} is absent"
    if isinstance(raw["schema_version"], bool) or not isinstance(raw["schema_version"], int):
        return "schema_version is not an integer"
    if raw["kind"] not in ("benchmark", "coverage"):
        return f"unknown kind {raw['kind']!r}"
    if raw["kind"] != expect_kind:
        return f"kind {raw['kind']!r} inside a {expect_kind}/ directory"
    if not isinstance(raw["run_status"], str) or not isinstance(raw["run_id"], str):
        return "run_status / run_id must be strings"
    if not isinstance(raw["tool"], dict):
        return "tool is not an object"
    if raw["kind"] == "coverage":
        scope = raw.get("scope")
        if scope not in ("total", "file"):
            return "coverage scope is not total|file"
        if scope == "file" and not isinstance(raw.get("path"), str):
            return "a file record carries no path"
        if scope == "total" and "path" in raw:
            return "a total record must not carry path"
    else:
        if not isinstance(raw.get("name"), str):
            return "benchmark record has no name"
        value = raw.get("value")
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            return "benchmark record has no numeric value"
    return None


def read_tree(root: Path, model: Model, cfg: Config) -> Tree:
    """Walk `root`; retain what a number may be built from and drop everything else with a reason."""
    tree = Tree()
    for kind in model.walk["kinds"]:
        kind_dir = root / kind
        if not kind_dir.is_dir():
            continue
        for sha_dir in sorted(p for p in kind_dir.iterdir() if p.is_dir()):
            sha = sha_dir.name
            if not SHA_RE.match(sha):
                tree.drop("", str(sha_dir), model.reason("directory_not_sha"),
                          f"expected 40 lowercase hex, got {sha!r}")
                continue
            for fpath in sorted(sha_dir.iterdir()):
                if not fpath.is_file():
                    continue
                rel = fpath.relative_to(root).as_posix()
                if fpath.name.startswith("."):
                    tree.drop(sha, rel, model.reason("temp_file"),
                              "a dot-file is a crashed publish, never a record")
                    continue
                if not fpath.name.endswith(model.suffix):
                    tree.drop(sha, rel, model.reason("not_a_record_path"), "not a record file")
                    continue
                tree.files += 1
                text = fpath.read_text(encoding="utf-8", errors="replace")
                if text and not text.endswith("\n"):
                    tree.drop(sha, rel, model.reason("malformed"), "last byte is not LF: torn write")
                for n, line in enumerate(text.splitlines(), 1):
                    if not line.strip():
                        continue
                    tree.records += 1
                    where = f"{rel}:{n}"
                    try:
                        raw = json.loads(line)
                    except json.JSONDecodeError as exc:
                        tree.drop(sha, where, model.reason("malformed"), f"not strict JSON: {exc.msg}")
                        tree.unreadable[sha] = tree.unreadable.get(sha, 0) + 1
                        continue
                    version = raw.get("schema_version") if isinstance(raw, dict) else None
                    if isinstance(version, int) and not isinstance(version, bool) \
                       and version > MAX_SCHEMA_VERSION:
                        tree.drop(sha, where, model.reason("schema_unsupported"),
                                  f"schema_version {version} is newer than this reader's {MAX_SCHEMA_VERSION}")
                        tree.unreadable[sha] = tree.unreadable.get(sha, 0) + 1
                        continue
                    problem = envelope_problem(raw, kind)
                    if problem:
                        tree.drop(sha, where, model.reason("malformed"), problem)
                        tree.unreadable[sha] = tree.unreadable.get(sha, 0) + 1
                        continue
                    if raw["commit"] != sha:
                        tree.drop(sha, where, model.reason("commit_mismatch"),
                                  f"record commit {raw['commit']!r} != directory {sha!r}")
                        tree.unreadable[sha] = tree.unreadable.get(sha, 0) + 1
                        continue
                    if not RUN_ID_RE.match(raw["run_id"]):
                        tree.drop(sha, where, model.reason("bad_run_id"), f"run_id {raw['run_id']!r}")
                        tree.unreadable[sha] = tree.unreadable.get(sha, 0) + 1
                        continue
                    rec = Rec(rel, n, raw)
                    # A readable record belongs to the commit's action history whatever its run_status:
                    # action success asks "did a run happen here", not "is this number usable".
                    tree.readable.setdefault(sha, []).append(rec)
                    if rec.run_status not in model.eligible:
                        tree.drop(sha, where, model.reason("run_status_excluded"),
                                  f"run_status {rec.run_status!r}", rec.run_id, rec.run_status)
                        continue
                    if "truncated" in rec.flags:
                        tree.drop(sha, where, model.reason("run_truncated"),
                                  "the run hit the metric cap; what survived is not the run",
                                  rec.run_id, rec.run_status)
                        continue
                    if rec.kind == "benchmark" and not cfg.keeps(rec.raw["name"]):
                        tree.drop(sha, where, model.reason("config_filtered"),
                                  f"{rec.raw['name']!r} excluded by benchmarks.allow/deny",
                                  rec.run_id, rec.run_status)
                        continue
                    tree.retained.setdefault(sha, []).append(rec)
        apply_source_precedence(tree, model, kind)
    return tree


def apply_source_precedence(tree: Tree, model: Model, kind: str) -> None:
    """Within one run and one group the highest-precedence source wins (ADR-0002 5.1's order).

    The key carries `sample`: two samples of one (name, labels) in one run are two measurements, not one
    measurement seen twice, and a precedence rule that collapsed them would throw away the noise estimate.
    """
    if kind != "benchmark":
        return
    for sha, recs in list(tree.retained.items()):
        best: dict[tuple, Rec] = {}
        keep: list[Rec] = []
        for rec in recs:
            if rec.kind != "benchmark":
                keep.append(rec)
                continue
            key = (rec.run_id, rec.group, rec.raw.get("sample"))
            rank = model.precedence.get(rec.source, len(model.precedence))
            current = best.get(key)
            if current is None:
                best[key] = rec
                keep.append(rec)
                continue
            if rank >= model.precedence.get(current.source, len(model.precedence)):
                tree.drop(sha, f"{rec.path}:{rec.line}", model.reason("source_shadowed"),
                          f"{rec.source} shadowed by {current.source} for the same measurement",
                          rec.run_id, rec.run_status)
                continue
            keep.remove(current)
            tree.drop(sha, f"{current.path}:{current.line}", model.reason("source_shadowed"),
                      f"{current.source} superseded within the run by {rec.source}",
                      current.run_id, current.run_status)
            best[key] = rec
            keep.append(rec)
        tree.retained[sha] = keep


# -------------------------------------------------------------------------------------------------
# stage 3: select
# -------------------------------------------------------------------------------------------------


def select(recs: list[Rec], model: Model) -> tuple[list[Rec], list[Rec]]:
    """Newest run per (kind, workflow, task); the rest are superseded, not discarded."""
    groups: dict[tuple, list[Rec]] = {}
    for rec in recs:
        groups.setdefault(rec.selection_key, []).append(rec)
    selected: list[Rec] = []
    superseded: list[Rec] = []
    for key in sorted(groups, key=lambda k: [str(x) for x in k]):
        newest = max(rec.run_id for rec in groups[key])
        for rec in groups[key]:
            (selected if rec.run_id == newest else superseded).append(rec)
    return selected, superseded


# -------------------------------------------------------------------------------------------------
# stage 4: coverage
# -------------------------------------------------------------------------------------------------


def split_scraped_coverage(selected: list[Rec], model: Model,
                           log=None) -> tuple[list[Rec], list[str]]:
    """Coverage records a number may be built from, with the scraped ones set aside as caveats."""
    caveats: list[str] = []
    eligible: list[Rec] = []
    for rec in selected:
        if rec.kind != "coverage":
            continue
        scraped: list[str] = []
        if "percent_only" in rec.flags:
            scraped.append(model.reason("percent_only"))
            model.caveat("percent_only", caveats)
        if "low_confidence" in rec.flags:
            scraped.append(model.reason("low_confidence"))
            model.caveat("low_confidence", caveats)
        if scraped:
            if log:
                log(rec, scraped[0],
                    f"scraped evidence (reported_percent {rec.raw.get('reported_percent')} of "
                    f"{rec.raw.get('reported_measure')}): a caveat on the row, never a plotted number")
            continue
        eligible.append(rec)
    return eligible, caveats


def coverage_run_totals(eligible: list[Rec], model: Model, log=None) -> dict[str, dict[str, list]]:
    """One total per (run, measure): the run's own total record if it has one, else its file records.

    `sum(file records) == total` is not an invariant — ADR-0002 4.2 puts instrumented files outside the
    workspace in the total and in no file record — so a total comes from the total, and file records are
    summed only for a run that published none.
    """
    totals: dict[str, dict[str, list]] = {}
    for rec in eligible:
        pairs = rec.count_pairs(model)
        if not pairs:
            if log:
                log(rec, model.reason("no_counts"), "no complete count pair and no reported_percent")
            continue
        run = totals.setdefault(rec.run_id, {})
        for measure, (hit, found) in pairs.items():
            if rec.raw["scope"] == "total":
                run[measure] = [hit, found, "scope_total"]
            else:
                prev = run.get(measure)
                if prev and prev[2] == "scope_total":
                    if log:
                        log(rec, model.reason("file_under_total"),
                            f"the run published its own {measure} total, which is that run's number")
                    continue       # a run's own total is that run's total (ADR-0002 4.3)
                base = prev or [0, 0, "sum_of_file_records"]
                run[measure] = [int(base[0]) + hit, int(base[1]) + found, "sum_of_file_records"]
    return totals


def coverage_of(selected: list[Rec], model: Model, tree: Tree, sha: str) -> dict[str, Any]:
    """Pooled coverage for one commit: {block, caveats}. Drops are logged to the tree."""
    caveats: list[str] = []

    def log(rec: Rec, reason: str, detail: str) -> None:
        tree.drop(sha, f"{rec.path}:{rec.line}", reason, detail, rec.run_id, rec.run_status)

    eligible, scraped_caveats = split_scraped_coverage(selected, model, log)
    caveats.extend(scraped_caveats)

    if not eligible:
        return {"block": None, "caveats": caveats}

    totals = coverage_run_totals(eligible, model, log)
    sum_runs = {r for r, t in totals.items()
                if any(v[2] == "sum_of_file_records" for v in t.values())}
    vacuous_runs = {r for r, t in totals.items() if any(int(v[1]) == 0 for v in t.values())}
    if sum_runs:
        model.caveat("total_from_file_sum", caveats)
    if vacuous_runs:
        model.caveat("vacuous", caveats)

    pooled: dict[str, dict[str, Any]] = {}
    for run_id, run in totals.items():
        for measure, (hit, found, _src) in run.items():
            bucket = pooled.setdefault(measure, {"hit": 0, "found": 0, "runs": []})
            bucket["hit"] += int(hit)
            bucket["found"] += int(found)
            if run_id not in bucket["runs"]:
                bucket["runs"].append(run_id)

    present = [m for m in model.measure_order if m in pooled]
    primary = next((m for m in present if pooled[m]["found"] > 0), None)
    if primary is None:
        return {"block": None, "caveats": caveats}

    dec = int(model.coverage["percent_decimals"])
    main = pooled[primary]
    block: dict[str, Any] = {
        "percent": round_half_away(100 * main["hit"] / main["found"], dec),
        "found": main["found"],
        "hit": main["hit"],
        "measure": primary,
        "runs": sorted(main["runs"]),
    }
    secondary = []
    for measure in present:
        bucket = pooled[measure]
        if measure == primary or bucket["found"] <= 0:
            continue
        secondary.append({
            "measure": measure,
            "percent": round_half_away(100 * bucket["hit"] / bucket["found"], dec),
            "found": bucket["found"],
            "hit": bucket["hit"],
        })
    if secondary:
        block["secondary"] = secondary
        model.caveat("secondary_measures", caveats)
    for rec in eligible:
        pairs = rec.count_pairs(model)
        if pairs and primary not in pairs:
            log(rec, model.reason("measure_not_primary"),
                f"measures {'/'.join(sorted(pairs))}; this commit's primary measure is {primary}")
            model.caveat("measure_not_primary", caveats)
    return {"block": block, "caveats": caveats}


# -------------------------------------------------------------------------------------------------
# stage 4: benchmarks
# -------------------------------------------------------------------------------------------------


def benchmarks_of(selected: list[Rec], retained: list[Rec], model: Model) -> dict[str, Any]:
    caveats: list[str] = []
    groups: dict[tuple, dict[str, list[Rec]]] = {}
    for rec in selected:
        if rec.kind == "benchmark":
            groups.setdefault(rec.group, {}).setdefault(rec.run_id, []).append(rec)

    spread: dict[tuple, dict[str, Any]] = {}
    for rec in retained:
        if rec.kind != "benchmark":
            continue
        bucket = spread.setdefault(rec.group, {"values": [], "runs": [], "sources": []})
        bucket["values"].append(rec.raw["value"])
        if rec.run_id not in bucket["runs"]:
            bucket["runs"].append(rec.run_id)
        if rec.source not in bucket["sources"]:
            bucket["sources"].append(rec.source)

    rows: list[dict[str, Any]] = []
    for key in sorted(groups, key=lambda k: (str(k[0]), k[1], str(k[2]))):
        name, labels_json, unit = key
        by_run = groups[key]
        newest = max(by_run)
        newest_recs = by_run[newest]
        values = [r.raw["value"] for r in newest_recs]
        # the spread deliberately spans every retained run, superseded ones included
        s = spread.get(key) or {"values": values, "runs": [newest], "sources": []}
        sources = sorted({r.source for r in newest_recs})
        row: dict[str, Any] = {
            "name": name,
            "labels": json.loads(labels_json) or {},
            "comparable": unit is not None,
            "context": all(r.source in model.benchmarks["context_sources"] for r in newest_recs),
            "value": median(values),
            "samples": len(newest_recs),
            "min": min(s["values"]),
            "max": max(s["values"]),
            "run_count": len(s["runs"]),
            "run_id": newest,
            "source": sources[0],
            "flags": sorted({f for r in newest_recs for f in r.flags}),
        }
        if unit is not None:
            # absent, never null: ADR-0002 2.1 and ADR-0006 6.3 both forbid a null in a stored row
            row["unit"] = unit
        else:
            model.caveat("unit_missing", caveats)
        if len(s["sources"]) > 1:
            model.caveat("source_changed", caveats)
        if row["context"]:
            model.caveat("context_only", caveats)
        rows.append(row)
    return {"rows": rows, "caveats": caveats}


# -------------------------------------------------------------------------------------------------
# stage 4: action success
# -------------------------------------------------------------------------------------------------


def action_of(readable: list[Rec], model: Model) -> dict[str, Any]:
    """One of the four states, from the envelope's run_status on every readable record for the SHA."""
    if not readable:
        return {"state": "no_evidence", "detail": []}
    groups: dict[tuple, list[Rec]] = {}
    for rec in readable:
        groups.setdefault((rec.kind, rec.workflow, rec.task), []).append(rec)
    detail: list[dict[str, Any]] = []
    verdicts: list[bool] = []
    for key in sorted(groups, key=lambda k: [str(x) for x in k]):
        recs = groups[key]
        newest = max(rec.run_id for rec in recs)
        winner = next(r for r in recs if r.run_id == newest)
        ok = winner.run_status in model.successful
        verdicts.append(ok)
        detail.append({
            "kind": key[0], "workflow": winner.workflow, "task": winner.task,
            "category": winner.category, "status": winner.run_status,
            "run_id": newest, "ok": ok,
        })
    if all(verdicts):
        state = "success"
    elif not any(verdicts):
        state = "failed"
    else:
        state = "mixed"
    return {"state": state, "detail": detail}


# -------------------------------------------------------------------------------------------------
# the row
# -------------------------------------------------------------------------------------------------


def derive_row(sha: str, tree: Tree, model: Model) -> dict[str, Any]:
    retained = tree.retained.get(sha, [])
    readable = tree.readable.get(sha, [])
    selected, superseded = select(retained, model)
    cov = coverage_of(selected, model, tree, sha)
    bench = benchmarks_of(selected, retained, model)
    action = action_of(readable, model)

    caveats: list[str] = []
    for code in cov["caveats"] + bench["caveats"]:
        model.caveat(code, caveats)

    excluded: list[dict[str, str]] = []
    seen: set[tuple[str, str]] = set()
    for drop in tree.drops_for(sha):
        if drop.reason in (model.reason("run_status_excluded"), model.reason("run_truncated")):
            key = (drop.run_id, drop.reason)
            if key not in seen:
                seen.add(key)
                excluded.append({"run_id": drop.run_id, "status": drop.status, "reason": drop.reason})
                model.caveat("run_excluded", caveats)
    unreadable = tree.unreadable.get(sha, 0)
    if unreadable:
        model.caveat("unreadable", caveats)

    row: dict[str, Any] = {"commit": sha, "action_success": action["state"]}
    if cov["block"] is not None:
        row["coverage"] = cov["block"]
    if bench["rows"]:
        row["benchmarks"] = bench["rows"]
    if action["detail"]:
        row["action_detail"] = action["detail"]
    if excluded:
        row["excluded_runs"] = sorted(excluded, key=lambda e: (e["run_id"], e["reason"]))
    if superseded:
        for rec in superseded:
            tree.drop(sha, f"{rec.path}:{rec.line}", model.reason("superseded"),
                      "an older run of the same (kind, workflow, task)", rec.run_id, rec.run_status)
        row["superseded_runs"] = sorted({r.run_id for r in superseded})
        model.caveat("superseded_runs", caveats)
    if unreadable:
        row["unreadable_records"] = unreadable
    gaps: dict[str, str] = {}
    if cov["block"] is None:
        gaps["coverage"] = gap_code(model, tree, sha, "coverage", caveats)
    if not bench["rows"]:
        gaps["benchmarks"] = gap_code(model, tree, sha, "benchmarks", caveats)
    row["gaps"] = gaps
    if caveats:
        row["caveats"] = caveats
    return row


def gap_code(model: Model, tree: Tree, sha: str, column: str, caveats: list[str]) -> str:
    """Why a column has no number, from the closed per-column list, first match wins.

    The order is the priority, and it is deliberately "the most specific thing that actually happened"
    first: a commit whose only coverage run was a scraped percentage is `only_percent_only`, not
    `no_records`, because the actionable difference is "go make it write an lcov".
    """
    reasons = {d.reason for d in tree.drops_for(sha)}
    if not tree.readable.get(sha):
        code = "unreadable" if tree.unreadable.get(sha) else "no_evidence"
    elif column == "coverage":
        if {"percent_only", "low_confidence"} & set(caveats):
            code = "only_percent_only"
        elif "vacuous" in caveats:
            code = "vacuous"
        elif {model.reason("run_status_excluded"), model.reason("run_truncated")} & reasons:
            code = "all_runs_excluded"
        elif model.reason("no_counts") in reasons:
            code = "no_counts"
        elif model.reason("measure_not_primary") in reasons:
            code = "no_counts"
        else:
            code = "no_records"
    else:
        if model.reason("config_filtered") in reasons:
            code = "config_filtered"
        elif {model.reason("run_status_excluded"), model.reason("run_truncated")} & reasons:
            code = "all_runs_excluded"
        else:
            code = "no_records"
    assert code in model.gap_reasons[column], f"gap code {code!r} is not declared for {column}"
    return code


def derive_tree(tree: Tree, model: Model) -> dict[str, dict[str, Any]]:
    shas = sorted(set(tree.retained) | set(tree.readable) | set(tree.unreadable))
    return {sha: derive_row(sha, tree, model) for sha in shas}


# -------------------------------------------------------------------------------------------------
# the timeline columns (--table, and the same rules mact-by1.9 renders from)
# -------------------------------------------------------------------------------------------------


def render_columns(row: dict[str, Any], model: Model, *, ascii_only: bool = False) -> dict[str, str]:
    g = model.display["ascii"] if ascii_only else model.display["unicode"]
    unknown, mark = g["unknown"], g["caveat_marker"]
    caveats = row.get("caveats") or []
    out: dict[str, str] = {}

    def qualifies(column: str) -> str:
        """The caveat marker, if this column has a caveat of its own (ADR-0008 6.1)."""
        return mark if set(caveats) & set(model.caveat_codes[column]) else ""

    cov = row.get("coverage")
    if cov is None:
        # an unknown column carries the marker too: the gap code behind it is a caveat's other half
        out["cov"] = unknown + (mark if caveats else "")
    else:
        out["cov"] = f"{cov['percent']:.{int(model.coverage['percent_decimals'])}f}" + qualifies("coverage")
    benches = row.get("benchmarks") or []
    if not benches:
        out["bench"] = unknown + (mark if caveats else "")
    else:
        pick = next((b for b in benches if b["comparable"] and not b["context"]), benches[0])
        prefix = "" if (pick["comparable"] and not pick["context"]) else g["context"]
        text = f"{significant(float(pick['value']), 4)}{pick.get('unit') or ''}"
        extra = f" +{len(benches) - 1}" if len(benches) > 1 else ""
        out["bench"] = f"{(prefix + ' ') if prefix else ''}{text}{extra}" + qualifies("benchmarks")
    out["action"] = g[row["action_success"]]
    return out


# -------------------------------------------------------------------------------------------------
# golden vectors
# -------------------------------------------------------------------------------------------------


def derive_case(fixture: Path, model: Model) -> dict[str, Any]:
    """Derive every row a fixture tree produces.

    A fixture may carry `commits.json`: the SHAs the git walk handed the reader. `.evidence` alone never
    says which commits exist (ADR-0008 section 2.4), so a vector that wants a row for a commit with no
    evidence at all has to say so the way the real indexer does.
    """
    tree = read_tree(fixture, model, Config())
    rows = derive_tree(tree, model)
    known = fixture / "commits.json"
    if known.is_file():
        for sha in json.loads(known.read_text(encoding="utf-8")):
            rows.setdefault(sha, derive_row(sha, tree, model))
        rows = {sha: rows[sha] for sha in sorted(rows)}
    return rows


def vector_cases(vectors: Path) -> list[tuple[str, Path]]:
    return sorted((p.name, p) for p in (vectors / "fixtures").iterdir() if p.is_dir())


# -------------------------------------------------------------------------------------------------
# cli
# -------------------------------------------------------------------------------------------------


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(prog="metric_model_reference.py",
                                 description="ADR-0008's reference implementation")
    ap.add_argument("--model", default=str(DEFAULT_MODEL), help="path to schema/metric-model.json")
    ap.add_argument("--tree", help="an evidence root: the directory holding benchmark/ and coverage/")
    ap.add_argument("--commit", help="one commit: full 40 hex or an unambiguous prefix")
    ap.add_argument("--allow", default="", help="comma-separated benchmark names (benchmarks.allow)")
    ap.add_argument("--deny", default="", help="comma-separated benchmark names (benchmarks.deny)")
    ap.add_argument("--explain", action="store_true", help="also print the per-record keep/drop table")
    ap.add_argument("--table", action="store_true", help="print the timeline columns instead of JSON")
    ap.add_argument("--ascii", action="store_true", dest="ascii_only", help="ascii glyphs only")
    ap.add_argument("--vectors", action="store_true", help="work on tests/testdata/metrics instead of --tree")
    ap.add_argument("--diff", action="store_true", help="with --vectors: compare against expected/")
    ap.add_argument("--write", action="store_true", help="with --vectors: rewrite expected/")
    ap.add_argument("--quiet", action="store_true")
    args = ap.parse_args(argv)

    model = Model.load(Path(args.model))
    cfg = Config(tuple(x for x in args.allow.split(",") if x), tuple(x for x in args.deny.split(",") if x))

    if args.vectors:
        rc = 0
        cases = vector_cases(DEFAULT_VECTORS)
        for name, fixture in cases:
            derived = derive_case(fixture, model)
            expected_path = DEFAULT_VECTORS / "expected" / f"{name}.json"
            if args.write:
                expected_path.parent.mkdir(parents=True, exist_ok=True)
                notes = ""
                if expected_path.is_file():
                    notes = json.loads(expected_path.read_text(encoding="utf-8")).get("notes", "")
                doc = {"case": name, "notes": notes, "model_version": model.doc["model_version"],
                       "rows": derived}
                expected_path.write_text(json.dumps(doc, indent=2, sort_keys=True) + "\n",
                                         encoding="utf-8")
                print(f"wrote {expected_path.relative_to(REPO)}")
                continue
            if not args.diff:
                print(f"{name}: {len(derived)} commit(s)")
                continue
            if not expected_path.is_file():
                print(f"{name}: missing {expected_path.relative_to(REPO)}")
                rc = 1
                continue
            expected = json.loads(expected_path.read_text(encoding="utf-8")).get("rows", {})
            if expected != derived:
                rc = 1
                print(f"{name}: DISAGREES with {expected_path.relative_to(REPO)}")
                for sha in sorted(set(expected) | set(derived)):
                    if expected.get(sha) != derived.get(sha):
                        print(f"  {sha[:7]}")
                        print(f"    expected: {json.dumps(expected.get(sha), sort_keys=True)}")
                        print(f"    derived : {json.dumps(derived.get(sha), sort_keys=True)}")
            elif not args.quiet:
                print(f"{name}: ok ({len(derived)} commit(s))")
        return rc

    if not args.tree:
        print("need --tree or --vectors", file=sys.stderr)
        return 2
    root = Path(args.tree)
    if not root.is_dir():
        print(f"no such evidence root: {root}", file=sys.stderr)
        return 1
    tree = read_tree(root, model, cfg)
    rows = derive_tree(tree, model)
    if not rows:
        # ADR-0008 2.4: rows come from the git walk, and a bare evidence root cannot say which commits
        # exist. Say so instead of printing an empty object and looking like a bug in the derivation.
        print(f"note: no evidence-derived rows under {root}: .evidence never says which commits exist; "
              f"the indexer is handed a commit list (ADR-0008 2.4)", file=sys.stderr)
    shas = sorted(rows)
    if args.commit:
        want = args.commit.lower()
        matches = [s for s in shas if s.startswith(want)]
        if len(matches) != 1:
            print(f"--commit {args.commit!r} matched {len(matches)} commits", file=sys.stderr)
            return 1
        shas = matches
    if args.table:
        width = int(model.row_key["display_length"])
        for sha in shas:
            cols = render_columns(rows[sha], model, ascii_only=args.ascii_only)
            print(f"{sha[:width]}  {cols['cov']:>6}  {cols['bench']:<26} {cols['action']}")
    else:
        print(json.dumps({sha: rows[sha] for sha in shas}, indent=2, sort_keys=True))
    if args.explain:
        counts: dict[str, int] = {}
        for drop in tree.drops:
            counts[drop.reason] = counts.get(drop.reason, 0) + 1
        print(f"# {tree.files} file(s), {tree.records} record(s) walked", file=sys.stderr)
        for reason in sorted(counts):
            print(f"# {reason:<24} {counts[reason]}", file=sys.stderr)
        for drop in tree.drops:
            print(f"# DROP {drop.reason:<22} {drop.where}  {drop.detail}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
