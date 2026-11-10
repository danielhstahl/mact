#!/usr/bin/env python3
"""Check ADR-0004 (run lifecycle): its numbers are self-consistent and its schema claims are true.

This is the dev-time checker for ADR-0004 (ticket mact-f1w.3), the sibling of
scripts/validate_tool_contract.py and scripts/validate_evidence.py. The ADR is prose; this file is
what makes three things enforceable before the Rust server exists:

  1. The ADR is finished: present, marked accepted, free of unresolved-marker text (the same rule
     validate_tool_contract.py applies to the tool contract).
  2. The claims ADR-0004 §16 makes about schema/tool-contract.schema.json are actually true —
     in particular that `run_view` cannot express a non-terminal `cancelled` (which is why the
     mid-kill window projects to `running` + `phase: "cancelling"`), that it can express that
     window at all, and that `cancel_run_result` can express every row of the ADR's §5.2 table.
     If someone relaxes one of those subschemas, this fails and the ADR's §16 has to be re-read.
  3. Every default in ADR-0004 §11 equals EXPECTED below, appears again in the §17 copy-verbatim
     block, and is compatible with the bound in the schema it has to fit inside (term grace vs
     `kill.took_ms`, ring bytes vs `log_tail_bytes`, retention count vs the listing cap, the
     86400 s timeout ceiling vs `duration_ms`). Per-category timeout defaults are read out of §4.2.

Checks performed (all of them, every run):

  C1  ADR-0004 exists, states `status: accepted`, carries no unresolved marker.
  C2  status vocabulary: 2 non-terminal + 5 terminal, matching x-run-status, schema/run-meta and
      schema/evidence; no `succeeded` anywhere.
  C3  §16.1 representability: run_view rejects a terminal status with terminal:false, and accepts
      status:running + phase:cancelling + terminal:false (the pinned mid-kill projection).
  C4  §3.2 projects exactly the seven internal non-terminal states onto phase values that exist in
      $defs/phase — the ADR adds no phase member.
  C5  §5.2 rows validate against cancel_run_result (mid-kill, pre-spawn `kill.method:"none"`,
      post-exit no-op, already-terminal), and already_terminal still forbids `kill`.
  C6  §11's defaults table parses, has exactly EXPECTED's flags, and agrees with EXPECTED.
  C7  EXPECTED satisfies the schema bounds it must sit inside (§4.3, §4.6, §5.2, §7.1, §7.3).
  C8  §4.2's per-category defaults are 900 / 1800 / 1800 and match EXPECTED.
  C9  the §17 copy-verbatim block quotes every flag and default a wiring ticket is told to paste,
      and ADR-0001 + docs/tool-contract.md both list the two warning codes §5.6 adds.

Usage:
    scripts/validate_run_lifecycle.py [--adr PATH] [--schema PATH] [--quiet] [--stats]

Exit status: 0 everything consistent, 1 a contract violation (one line each), 2 usage or
environment error (missing file, or jsonschema absent for the checks that need it).
"""
from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path
from typing import Any

try:
    from jsonschema import Draft202012Validator
except ModuleNotFoundError:  # pragma: no cover - environment, not contract
    Draft202012Validator = None  # type: ignore[assignment]
    SKIPPED = (
        "jsonschema is not installed: the schema-representability checks (C3, C5) were skipped "
        "(pip install jsonschema); every other check ran."
    )
else:
    SKIPPED = ""

REPO = Path(__file__).resolve().parent.parent
DEFAULT_ADR = REPO / "docs/adr/0004-run-lifecycle.md"
DEFAULT_SCHEMA = REPO / "schema/tool-contract.schema.json"
ADR1_PATH = REPO / "docs/adr/0001-tool-contract.md"
REFERENCE_PATH = REPO / "docs/tool-contract.md"
RUN_META_PATH = REPO / "schema/run-meta.schema.json"
EVIDENCE_PATH = REPO / "schema/evidence.schema.json"

UNRESOLVED_MARKERS = ("TODO", "TBD", "FIXME", "XXX", "placeholder", "not yet decided")

#: The §11 configuration surface, as ADR-0004 owns it. A change here and a change to the ADR table
#: must land together, exactly like the error-code table in validate_tool_contract.py.
EXPECTED: dict[str, dict[str, Any]] = {
    "--max-concurrent-runs": {"default": 2, "min": 1},
    "--max-timeout-secs": {"default": 21600, "min": 1, "max": 86400},
    "--prestart-timeout-secs": {"default": 120, "min": 1},
    "--term-grace-secs": {"default": 20, "min": 1, "max": 240},
    "--harvest-timeout-secs": {"default": 120, "min": 1},
    "--cancel-wait-secs": {"default": 30, "min": 1, "max": 300},
    "--shutdown-grace-secs": {"default": 30, "min": 1},
    "--run-retention-count": {"default": 200, "min": 50},
    "--run-retention-mins": {"default": 120, "min": 1},
    "--log-tail-ring-bytes": {"default": 65536},
    "--max-run-log-bytes": {"default": 67108864, "min": 1048576},
}

#: ADR-0004 §4.2 (pinned defaults per category) and the §4.6 arithmetic.
CATEGORY_DEFAULTS = {"coverage": 900, "benchmark": 1800, "other": 1800}
WAIT_OVERSHOOT_BUDGET = 300  # seconds the ADR promises a wait:true call may overshoot timeout_secs
LISTING_CAP = 50  # ADR-0001 §9 / run_status listing limit

#: The two members §5.6 adds to the open warnings vocabulary.
NEW_WARNINGS = ("orphaned_containers", "already_terminal")

#: §3.2: internal state -> (wire status, phase). Seven non-terminal rows; terminal ones map to
#: themselves with phase "done".
STATE_PROJECTION = {
    "Queued": ("pending", "queued"),
    "PreparingTree": ("pending", "preparing_tree"),
    "ActStarting": ("pending", "starting_act"),
    "ActRunning": ("running", "running"),
    "Cancelling": ("running", "cancelling"),
    "Harvesting": ("running", "extracting_evidence"),
    "Publishing": ("running", "publishing_evidence"),
}
TERMINAL_STATES = ("Succeeded", "Failed", "Cancelled", "TimedOut", "Error")
TERMINAL_STATUSES = ("success", "failed", "cancelled", "timed_out", "error")
NON_TERMINAL_STATUSES = ("pending", "running")


class Problem(Exception):
    """A contract violation. Collected, not raised, so one run reports all of them."""


def load_json(path: Path) -> Any:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
        sys.stderr.write(f"error: cannot read {path}: {exc}\n")
        raise SystemExit(2)


def load_text(path: Path) -> str:
    if not path.is_file():
        sys.stderr.write(f"error: {path} does not exist\n")
        raise SystemExit(2)
    return path.read_text(encoding="utf-8")


def def_schema(root: dict, name: str) -> dict:
    defs = root.get("$defs", {})
    if name not in defs:
        sys.stderr.write(f"error: {name} is not defined under $defs\n")
        raise SystemExit(2)
    sub = dict(defs[name])
    sub["$defs"] = defs
    return sub


def accepts(schema: dict, instance: dict) -> list[str]:
    if Draft202012Validator is None:
        return []
    return sorted(
        (e.message for e in Draft202012Validator(schema).iter_errors(instance)),
        key=str,
    )


# ---------------------------------------------------------------- C1: the ADR itself is finished

def check_adr_text(adr: str, problems: list[str]) -> None:
    if "status: accepted" not in adr:
        problems.append("ADR-0004: must be marked status: accepted")
    for marker in UNRESOLVED_MARKERS:
        if marker.lower() in adr.lower():
            problems.append(
                f"ADR-0004: contains the unresolved marker {marker!r}; an accepted decision "
                "answers, it does not defer"
            )
    for needed in (
        "run_not_found",
        "process_group_killed",
        "partial_evidence_dropped",
        "orphaned_containers",
        "idempotency_key",
        "Tasks",
    ):
        if needed not in adr:
            problems.append(f"ADR-0004: never mentions {needed!r}")


# ------------------------------------------------------------- C2/C4: vocabulary agreements

def check_status_vocabulary(root: dict, adr: str, problems: list[str], stats: dict) -> None:
    defs = root["$defs"]
    statuses = defs["status"]["enum"]
    meta = root.get("x-run-status", {})
    if sorted(meta.get("terminal", [])) != sorted(TERMINAL_STATUSES):
        problems.append("x-run-status terminal split is not the five words ADR-0004 §3.3 enumerates")
    if sorted(meta.get("non_terminal", [])) != sorted(NON_TERMINAL_STATUSES):
        problems.append("x-run-status non-terminal split is not pending/running")
    if sorted(statuses) != sorted(TERMINAL_STATUSES + NON_TERMINAL_STATUSES):
        problems.append("$defs/status is not the seven values ADR-0004 §3.2 projects onto")
    for path, label in ((RUN_META_PATH, "run-meta status"), (EVIDENCE_PATH, "evidence run_status")):
        other = load_json(path)
        field = "status" if "run-meta" in path.name else "run_status"
        values = other["properties"][field]["enum"]
        if sorted(values) != sorted(TERMINAL_STATUSES):
            problems.append(f"ADR-0004 terminal statuses disagree with {label}: {sorted(values)}")
    vocab = [statuses, meta.get("all", []), meta.get("terminal", []), meta.get("non_terminal", [])]
    if any("succeeded" in values for values in vocab):
        problems.append(
            "'succeeded' is a status value somewhere in the tool contract; the word is 'success'"
        )
    stats["statuses"] = len(statuses)

    phases = defs["phase"]["enum"]
    used = sorted({p for _, (_, p) in STATE_PROJECTION.items()} | {"done"})
    if not set(used) <= set(phases):
        problems.append(
            "ADR-0004 §3.2 uses phase values that are not in $defs/phase: "
            + str(sorted(set(used) - set(phases)))
        )
    stats["phases"] = len(phases)
    for state in list(STATE_PROJECTION) + list(TERMINAL_STATES):
        if state not in adr:
            problems.append(f"ADR-0004: internal state {state!r} is not named in the ADR text")
    for status in sorted({status for status, _ in STATE_PROJECTION.values()}):
        if status not in statuses:
            problems.append(f"ADR-0004 projects onto unknown status {status!r}")


# ------------------------------------------------------- C3/C5: the §16 representability claims

def check_representability(root: dict, problems: list[str]) -> None:
    if Draft202012Validator is None:
        return
    rv = def_schema(root, "run_view")
    cr = def_schema(root, "cancel_run_result")
    rs = def_schema(root, "run_status_result")

    base = {
        "run_id": "20261110T011507Z-d4e5f6",
        "status": "running",
        "terminal": False,
        "workflow": "coverage",
        "workflow_file": ".github/workflows/coverage.yml",
        "task": "coverage",
        "category": "coverage",
        "commit": "2f7c1a4b9e0d3c6a5b8f1e2d4c6a8b0f3e5d7c91",
        "evidence_files": [],
        "evidence_record_count": 0,
        "log_tail": "",
        "warnings": [],
    }
    stamp = "2026-11-10T01:15:07.004Z"

    # §16.1: a non-terminal terminal-status is inexpressible in run_view, which is the whole reason
    # the mid-kill window is `running` + `phase: "cancelling"`.
    midkill = dict(
        base,
        status="cancelled",
        terminal=False,
        phase="cancelling",
        started_at=stamp,
        finished_at="2026-11-10T01:15:31.900Z",
    )
    if not accepts(rv, midkill):
        problems.append(
            "§16.1 is stale: run_view now ACCEPTS a terminal status with terminal:false, so the "
            "mid-kill projection pinned in ADR-0004 §3.2/§16.1 needs re-reading"
        )
    if accepts(rv, dict(base, phase="cancelling", started_at=stamp, duration_ms=5000)):
        problems.append("§3.2 is stale: run_view rejects running + phase:cancelling (mid-kill)")
    terminal_ok = accepts(
        rv,
        dict(
            base,
            status="cancelled",
            terminal=True,
            phase="done",
            started_at=stamp,
            finished_at="2026-11-10T01:15:31.900Z",
            duration_ms=24896,
        ),
    )
    if terminal_ok:
        problems.append(f"§3.2 is stale: run_view rejects a terminal cancelled run: {terminal_ok[0]}")

    # §16.5: duration_ms is absent while pending (no started_at).
    if accepts(rv, dict(base, status="pending", phase="preparing_tree")):
        problems.append("§3.2/§16.5 is stale: run_view rejects a pending run with no started_at")

    # §4.6: an error-status run may still carry act's exit code.
    err = accepts(
        rv,
        dict(
            base,
            status="error",
            terminal=True,
            phase="done",
            started_at=stamp,
            finished_at="2026-11-10T01:15:31.900Z",
            duration_ms=24896,
            exit_code=1,
            error={
                "code": "runner_internal_error",
                "message": "extraction exceeded the harvest budget",
                "retryable": True,
                "detail": {"stage": "extracting_evidence"},
            },
        ),
    )
    if err:
        problems.append(f"§3.3/T17 is stale: run_view rejects error + exit_code: {err[0]}")

    # §5.2 rows.
    cancel_id = "20261110T011507Z-d4e5f6"
    requested = "2026-11-10T01:15:31.900Z"
    rows = {
        "mid-kill (cancelled, not yet terminal)": {
            "run_id": cancel_id,
            "status": "cancelled",
            "terminal": False,
            "already_terminal": False,
            "cancel_requested_at": requested,
            "warnings": [],
        },
        "pre-spawn cancel (kill.method none)": {
            "run_id": cancel_id,
            "status": "cancelled",
            "terminal": True,
            "already_terminal": False,
            "cancel_requested_at": requested,
            "kill": {"method": "none", "took_ms": 0},
            "warnings": [],
        },
        "cancel after act exited (no-op)": {
            "run_id": cancel_id,
            "status": "running",
            "terminal": False,
            "already_terminal": False,
            "cancel_requested_at": requested,
            "warnings": ["already_terminal"],
        },
        "already terminal": {
            "run_id": cancel_id,
            "status": "success",
            "terminal": True,
            "already_terminal": True,
            "warnings": ["already_terminal"],
        },
    }
    for label, payload in rows.items():
        errors = accepts(cr, payload)
        if errors:
            problems.append(f"§5.2 row rejected by cancel_run_result — {label}: {errors[0]}")
    if not accepts(cr, dict(rows["already terminal"], kill={"method": "term"})):
        problems.append(
            "cancel_run_result no longer forbids `kill` on an already_terminal run; §5.2's "
            "already-terminal row relies on that"
        )

    if accepts(rs, {"runs": [], "active_count": 0, "truncated": False}):
        problems.append("§7.2 is stale: an empty run_status listing is not a valid result")


# ------------------------------------------------------- C6/C7/C8/C9: the numbers in §11 and §17

def parse_defaults_table(adr: str) -> dict[str, str]:
    """The §11 configuration table: `| `--flag` | default | ... |` rows."""
    found: dict[str, str] = {}
    for line in adr.splitlines():
        line = line.strip()
        if not line.startswith("|"):
            continue
        cells = [c.strip() for c in line.strip("|").split("|")]
        if len(cells) < 2 or not cells[0].startswith("`--"):
            continue
        flag = cells[0].strip("`").split()[0]
        found[flag] = cells[1]
    return found


def fenced_block(adr: str, header: str) -> str:
    """The first fenced block under a `### <header>` heading (the §17 copy-verbatim blocks)."""
    lines = adr.splitlines()
    try:
        start = next(i for i, line in enumerate(lines) if line.startswith("### ") and header in line)
    except StopIteration:
        return ""
    out: list[str] = []
    inside = False
    for line in lines[start + 1 :]:
        if line.startswith("```"):
            if inside:
                break
            inside = True
            continue
        if inside:
            out.append(line)
    return "\n".join(out)


def check_defaults(adr: str, root: dict, problems: list[str], stats: dict) -> None:
    table = parse_defaults_table(adr)
    stats["flags documented"] = len(table)
    missing = sorted(set(EXPECTED) - set(table))
    extra = sorted(set(table) - set(EXPECTED) - {"--default-timeout-secs"})
    if missing:
        problems.append(f"§11 table is missing flags: {missing}")
    if extra:
        problems.append(f"§11 table documents unknown flags: {extra}")
    for flag, spec in EXPECTED.items():
        cell = table.get(flag, "")
        got = cell.split()[0].replace(",", "").replace("(", "") if cell else ""
        if got != str(spec["default"]):
            problems.append(
                f"§11 {flag} default is {cell!r}, expected {spec['default']} (EXPECTED in "
                "scripts/validate_run_lifecycle.py — change both together with the ADR)"
            )

    # §17.2 must quote each flag and its default, since that block is what gets pasted into tickets.
    block = fenced_block(adr, "17.2")
    if not block:
        problems.append("§17.2 copy-verbatim block is missing")
    for flag, spec in EXPECTED.items():
        hits = [line for line in block.splitlines() if flag in line]
        if not hits:
            problems.append(f"§17.2 does not quote {flag}")
        elif not any(str(spec["default"]) in line for line in hits):
            problems.append(f"§17.2 quotes {flag} without its default {spec['default']}")

    # §4.2 per-category defaults, as stated in the ADR's own table.
    stated = {
        "coverage": r"\|\s*`coverage`\s*\|\s*\*\*(\d+)\*\*",
        "benchmark": r"\|\s*`benchmark`\s*\|\s*\*\*(\d+)\*\*",
        "other": r"anything else[^|]*\|\s*\*\*(\d+)\*\*",
    }
    for category, pattern in stated.items():
        match = re.search(pattern, adr)
        if not match:
            problems.append(f"§4.2 does not state a bold default for the {category} category")
        elif int(match.group(1)) != CATEGORY_DEFAULTS[category]:
            problems.append(
                f"§4.2 states {category} default {match.group(1)}, expected "
                f"{CATEGORY_DEFAULTS[category]} (EXPECTED in this script — change both together)"
            )

    # Bounds in the schema that the defaults must fit inside.
    defs = root["$defs"]
    timeout = defs["run_workflow_args"]["properties"]["timeout_secs"]
    if timeout["maximum"] != EXPECTED["--max-timeout-secs"]["default"]:
        problems.append(
            "timeout_secs schema bound "
            f"{timeout['maximum']} != the default --max-timeout-secs "
            f"{EXPECTED['--max-timeout-secs']['default']} (ADR-0004 §4.1)"
        )
    for category, value in CATEGORY_DEFAULTS.items():
        if not timeout["minimum"] <= value <= timeout["maximum"]:
            problems.append(f"§4.2 {category} default {value} is outside timeout_secs bounds")
    if CATEGORY_DEFAULTS["coverage"] >= CATEGORY_DEFAULTS["benchmark"]:
        problems.append("§4.2: coverage's default must be shorter than benchmark's")

    took = defs["cancel_run_result"]["properties"]["kill"]["properties"]["took_ms"]["maximum"]
    grace = EXPECTED["--term-grace-secs"]
    if (grace["max"] + 60) * 1000 > took:
        problems.append(
            f"--term-grace-secs max {grace['max']} s cannot fit in kill.took_ms's {took} ms "
            "bound (ADR-0004 §16.3)"
        )
    if grace["default"] > EXPECTED["--cancel-wait-secs"]["default"]:
        problems.append("--cancel-wait-secs default must be >= --term-grace-secs default (§11)")

    ring = EXPECTED["--log-tail-ring-bytes"]["default"]
    if ring != defs["log_tail_bytes"]["maximum"]:
        problems.append(
            f"--log-tail-ring-bytes {ring} != log_tail_bytes maximum "
            f"{defs['log_tail_bytes']['maximum']} (ADR-0004 §7.1)"
        )
    if EXPECTED["--run-retention-count"]["default"] < LISTING_CAP:
        problems.append("--run-retention-count default must be >= the run_status listing cap")
    if EXPECTED["--run-retention-count"]["min"] < LISTING_CAP:
        problems.append("--run-retention-count minimum must be >= the run_status listing cap")

    duration = defs["run_view"]["properties"]["duration_ms"]["maximum"]
    if EXPECTED["--max-timeout-secs"]["max"] * 1000 > duration:
        problems.append(
            f"the {EXPECTED['--max-timeout-secs']['max']} s timeout ceiling cannot be expressed by "
            f"duration_ms (max {duration} ms) — ADR-0004 §4.3"
        )
    overshoot = (
        EXPECTED["--prestart-timeout-secs"]["default"]
        + EXPECTED["--term-grace-secs"]["default"]
        + EXPECTED["--harvest-timeout-secs"]["default"]
    )
    if overshoot > WAIT_OVERSHOOT_BUDGET:
        problems.append(
            f"§4.6 promises a {WAIT_OVERSHOOT_BUDGET} s overshoot but the default budgets sum to "
            f"{overshoot}"
        )

    # §5.6: the new warning codes must be pattern-legal and appear in the two contract docs.
    pattern = re.compile(defs["warning_code"]["pattern"])
    for warning in NEW_WARNINGS:
        if not pattern.match(warning) or len(warning) > defs["warning_code"]["maxLength"]:
            problems.append(f"{warning!r} violates $defs/warning_code")
        for doc in (ADR1_PATH, REFERENCE_PATH):
            if warning not in load_text(doc):
                problems.append(
                    f"{doc.name}: §5.6's new warning {warning!r} is missing from it; "
                    "ADR-0004 §16.6 says it is there"
                )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--adr", type=Path, default=DEFAULT_ADR)
    parser.add_argument("--schema", type=Path, default=DEFAULT_SCHEMA)
    parser.add_argument("--quiet", action="store_true")
    parser.add_argument("--stats", action="store_true")
    args = parser.parse_args(argv)

    adr = load_text(args.adr)
    root = load_json(args.schema)
    problems: list[str] = []
    stats: dict[str, int] = {}

    check_adr_text(adr, problems)
    check_status_vocabulary(root, adr, problems, stats)
    check_representability(root, problems)
    check_defaults(adr, root, problems, stats)

    for problem in problems:
        print(f"FAIL {problem}")
    if args.stats:
        for key, value in sorted(stats.items()):
            print(f"  {key}: {value}")
    if SKIPPED:
        print(f"WARN {SKIPPED}")
    if not problems:
        if not args.quiet:
            print(
                "ok: ADR-0004 is accepted, its schema claims hold, and its %d configuration "
                "defaults sit inside the contract bounds" % stats.get("flags documented", 0)
            )
        return 0
    print(f"{len(problems)} run-lifecycle problem(s)")
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
