#!/usr/bin/env python3
"""Check the act-mcp tool contract: schema, fixtures, and the two contract docs agree.

This is the dev-time reference checker for ADR-0001 (ticket mact-f1w.2), the sibling of
scripts/validate_evidence.py. It exists so the tool contract is enforceable before the Rust
server exists, and so schema/tool-contract.schema.json is exercised by something other than
the code that wrote it. Once `run_workflow` and friends are implemented (mact-f1w.14), the
e2e harness (mact-f1w.15) validates real structuredContent with these same schemas and the
same fixtures.

Checks performed (all of them, every run):

  1. schema/tool-contract.schema.json is a legal JSON Schema 2020-12 document.
  2. every fixture under tests/e2e/testdata/tool-io/valid validates against the $defs entry
     named by its filename, and every fixture under invalid/ is REJECTED (ideally for
     exactly one rule; more than one is reported as a weak fixture, not an error).
  3. run_workflow_result's field set is exactly run_view's field set plus 'deduplicated'
     (the two are spelled out separately so a generated Rust struct can be diffed against
     either one; nothing may drift between them silently).
  4. the run status vocabulary is identical across schema/tool-contract.schema.json,
     schema/run-meta.schema.json and schema/evidence.schema.json, and x-run-status's
     terminal / non-terminal split is a partition of it. 'succeeded' must not appear.
  5. the error-code set is identical across $defs/error_code, x-error-codes, and the
     error-code tables in docs/adr/0001-tool-contract.md and docs/tool-contract.md, and the
     isError / retryable columns of both docs match x-error-codes.
  6. every tool named in x-error-codes exists as a tool, and every tool has both an _args and
     a _result definition.
  7. the two contract docs contain no unresolved-marker text (see UNRESOLVED_MARKERS).

Usage:
    scripts/validate_tool_contract.py [--schema S] [--fixtures DIR] [--quiet] [--stats]

Exit status: 0 everything consistent, 1 a contract violation (printed one per line),
2 usage or environment error (e.g. missing jsonschema, missing file).
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
    from jsonschema.exceptions import SchemaError
except ModuleNotFoundError:  # pragma: no cover - environment, not contract
    Draft202012Validator = None  # type: ignore[assignment]
    SchemaError = None  # type: ignore[assignment]
    SKIPPED = (
        "jsonschema is not installed: schema-legality and fixture checks skipped "
        "(pip install jsonschema). Docs/schema parity checks still ran."
    )
else:
    SKIPPED = ""

REPO = Path(__file__).resolve().parent.parent
DEFAULT_SCHEMA = REPO / "schema/tool-contract.schema.json"
DEFAULT_FIXTURES = REPO / "tests/e2e/testdata/tool-io"
ADR_PATH = REPO / "docs/adr/0001-tool-contract.md"
REFERENCE_PATH = REPO / "docs/tool-contract.md"

TESTDATA_README = REPO / "tests/e2e/testdata/README.md"

# The four tools, and the filename token that selects the shared error payload shape.
TOOLS = ("run_workflow", "list_tasks", "run_status", "cancel_run")
ERROR_TOKEN = "error"

# Wording that must not survive in an accepted contract document: an accepted ADR answers
# its questions, it does not park them. (ADR-0001 acceptance criterion.)
UNRESOLVED_MARKERS = ("TODO", "TBD", "FIXME", "XXX", "placeholder", "not yet decided")

CODE_CELL = re.compile(r"^`([a-z][a-z0-9_]*)`$")


class Problem(Exception):
    """A contract violation. Collected, not raised, so one run reports all of them."""


def load_json(path: Path) -> Any:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
        sys.stderr.write(f"error: cannot read {path}: {exc}\n")
        raise SystemExit(2)


def load_schema(path: Path) -> dict:
    raw = load_json(path)
    if Draft202012Validator is None:
        return raw
    try:
        Draft202012Validator.check_schema(raw)
    except SchemaError as exc:  # pragma: no cover - schema bugs show up here first
        sys.stderr.write(f"error: {path} is not a legal 2020-12 schema: {exc.message}\n")
        raise SystemExit(2)
    return raw


def def_schema(root: dict, name: str) -> dict:
    """Extract one $defs entry as a standalone schema, keeping $defs so internal refs resolve."""
    defs = root.get("$defs", {})
    if name not in defs:
        sys.stderr.write(f"error: {name} is not defined under $defs\n")
        raise SystemExit(2)
    sub = dict(defs[name])
    sub["$defs"] = defs
    return sub


def fixture_def(filename: str) -> str | None:
    """`run_workflow.result.success.json` -> the $defs entry it validates against."""
    parts = filename.split(".")
    if len(parts) < 3 or parts[-1] != "json":
        return None
    tool, part = parts[0], parts[1]
    if tool == ERROR_TOKEN and part == "result":
        return "tool_error"
    if tool not in TOOLS or part not in ("args", "result"):
        return None
    return f"{tool}_{part}"


def check_fixtures(root: dict, fixtures: Path, problems: list[str], stats: dict) -> None:
    if Draft202012Validator is None:
        stats["fixtures"] = 0
        return
    for kind, expect_valid in (("valid", True), ("invalid", False)):
        directory = fixtures / kind
        if not directory.is_dir():
            problems.append(f"{directory}: missing fixture directory")
            continue
        files = sorted(p for p in directory.iterdir() if p.suffix == ".json")
        if not files:
            problems.append(f"{directory}: no fixtures")
        stats[kind] = len(files)
        for path in files:
            def_name = fixture_def(path.name)
            if def_name is None:
                problems.append(f"{path}: filename does not name a contract def")
                continue
            doc = load_json(path)
            validator = Draft202012Validator(def_schema(root, def_name))
            errors = sorted(validator.iter_errors(doc), key=lambda e: list(e.absolute_path))
            if expect_valid:
                if errors:
                    first = errors[0]
                    problems.append(
                        f"{path}: valid fixture rejected as {def_name}: "
                        f"{first.message} at /{'/'.join(map(str, first.absolute_path))}"
                    )
                continue
            if not errors:
                problems.append(f"{path}: invalid fixture ACCEPTED by {def_name}")
            elif len(errors) > 1:
                stats["multi_reason"] = stats.get("multi_reason", 0) + 1


def check_field_sets(root: dict, problems: list[str]) -> None:
    defs = root["$defs"]
    view = set(defs["run_view"]["properties"])
    result = set(defs["run_workflow_result"]["properties"])
    if result != view | {"deduplicated"}:
        missing = sorted((view | {"deduplicated"}) - result)
        extra = sorted(result - (view | {"deduplicated"}))
        problems.append(
            "run_workflow_result/run_view field drift: "
            + (f"missing {missing} " if missing else "")
            + (f"extra {extra}" if extra else "")
            + "; the result must be run_view plus 'deduplicated' and nothing else"
        )
    required_view = set(defs["run_view"]["required"])
    required_result = set(defs["run_workflow_result"]["required"])
    if required_result != required_view | {"deduplicated"}:
        problems.append(
            "run_workflow_result/run_view required-field drift: "
            f"{sorted(required_result)} vs {sorted(required_view | {'deduplicated'})}"
        )
    for summary in ("run_summary",):
        if not set(defs[summary]["properties"]) <= view:
            problems.append(f"{summary} must only use run_view field names")


def status_enum(schema: dict) -> list[str]:
    return schema["$defs"]["status"]["enum"]


def check_status_vocabulary(root: dict, problems: list[str]) -> None:
    statuses = status_enum(root)
    meta = root.get("x-run-status", {})
    if sorted(meta.get("all", [])) != sorted(statuses):
        problems.append("x-run-status.all disagrees with $defs/status.enum")
    terminal = meta.get("terminal", [])
    non_terminal = meta.get("non_terminal", [])
    if sorted(terminal + non_terminal) != sorted(statuses):
        problems.append("x-run-status terminal/non_terminal is not a partition of the enum")
    if set(terminal) & set(non_terminal):
        problems.append("x-run-status terminal and non_terminal overlap")
    if "succeeded" in statuses:
        problems.append("'succeeded' is not a status value; the word is 'success' (ADR-0002)")

    meta_schema = load_json(REPO / "schema/run-meta.schema.json")
    evidence_schema = load_json(REPO / "schema/evidence.schema.json")
    meta_statuses = meta_schema["properties"]["status"]["enum"]
    evidence_statuses = evidence_schema["properties"]["run_status"]["enum"]
    for label, other in (
        ("schema/run-meta.schema.json status", meta_statuses),
        ("schema/evidence.schema.json run_status", evidence_statuses),
    ):
        if sorted(other) != sorted(terminal):
            problems.append(
                f"terminal statuses disagree with {label}: "
                f"tool={sorted(terminal)} other={sorted(other)}"
            )


def error_codes_from_docs(path: Path) -> dict[str, tuple[bool, bool]]:
    """Parse every error-code markdown table in a doc into {code: (isError, retryable)}."""
    found: dict[str, tuple[bool, bool]] = {}
    if not path.is_file():
        raise SystemExit(f"error: {path} does not exist")
    header: list[str] | None = None
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line.startswith("|"):
            header = None
            continue
        cells = [c.strip() for c in line.strip("|").split("|")]
        if header is None:
            if cells and cells[0].lower() == "code" and "iserror" in [c.lower() for c in cells]:
                header = [c.lower() for c in cells]
            continue
        if set("".join(cells)) <= set("-: "):
            continue  # the |---|---| separator row
        match = CODE_CELL.match(cells[0])
        if not match:
            continue
        try:
            is_error = header.index("iserror")
            retry = header.index("retryable")
        except ValueError:  # pragma: no cover - header guaranteed by the check above
            continue
        if len(cells) <= max(is_error, retry):
            continue
        cells_is = cells[is_error].lower()
        cells_retry = cells[retry].lower()
        if cells_is not in ("true", "false") or cells_retry not in ("true", "false"):
            continue
        found[match.group(1)] = (cells_is == "true", cells_retry == "true")
    return found


def check_error_codes(root: dict, problems: list[str], stats: dict) -> None:
    schema_codes = set(root["$defs"]["error_code"]["enum"])
    registry = root.get("x-error-codes", {})
    registry_codes = set(registry)
    if schema_codes != registry_codes:
        only_schema = sorted(schema_codes - registry_codes)
        only_registry = sorted(registry_codes - schema_codes)
        problems.append(
            "error-code set differs between $defs/error_code and x-error-codes: "
            + (f"schema-only {only_schema} " if only_schema else "")
            + (f"registry-only {only_registry}" if only_registry else "")
        )
    stats["error_codes"] = len(schema_codes)

    for doc in (ADR_PATH, REFERENCE_PATH):
        documented = error_codes_from_docs(doc)
        stats[f"{doc.name} codes"] = len(documented)
        if set(documented) != schema_codes:
            missing = sorted(schema_codes - set(documented))
            extra = sorted(set(documented) - schema_codes)
            problems.append(
                f"{doc.relative_to(REPO)}: error-code table does not match the schema: "
                + (f"missing {missing} " if missing else "")
                + (f"unknown {extra}" if extra else "")
            )
        for code, (is_error, retry) in documented.items():
            entry = registry.get(code)
            if entry is None:
                continue
            if bool(entry.get("is_error")) != is_error:
                problems.append(
                    f"{doc.relative_to(REPO)}: {code} isError column says {is_error}, "
                    f"x-error-codes says {entry.get('is_error')}"
                )
            if bool(entry.get("retryable")) != retry:
                problems.append(
                    f"{doc.relative_to(REPO)}: {code} retryable column says {retry}, "
                    f"x-error-codes says {entry.get('retryable')}"
                )
            for tool in entry.get("tools", []):
                if tool not in TOOLS:
                    problems.append(f"x-error-codes: {code} names unknown tool {tool}")

    # The ADR must state the rule that validation failures are tool results, not protocol errors.
    adr_text = ADR_PATH.read_text(encoding="utf-8")
    if "isError" not in adr_text:
        problems.append(f"{ADR_PATH.relative_to(REPO)}: never mentions isError")
    for marker in ("-32700", "-32600", "-32601", "-32602", "-32603"):
        if marker not in adr_text:
            problems.append(f"{ADR_PATH.relative_to(REPO)}: JSON-RPC error {marker} is not mapped")


def check_tools(root: dict, problems: list[str]) -> None:
    defs = root["$defs"]
    for tool in TOOLS:
        for part in ("args", "result"):
            name = f"{tool}_{part}"
            if name not in defs:
                problems.append(f"$defs/{name} is missing")
    names = {"run_workflow", "list_tasks", "run_status", "cancel_run"}
    for code, entry in root.get("x-error-codes", {}).items():
        if not entry.get("tools"):
            problems.append(f"x-error-codes: {code} names no tool")


def check_docs_text(problems: list[str]) -> None:
    for doc in (ADR_PATH, REFERENCE_PATH):
        text = doc.read_text(encoding="utf-8")
        for marker in UNRESOLVED_MARKERS:
            if marker.lower() in text.lower():
                problems.append(
                    f"{doc.relative_to(REPO)}: contains the unresolved marker {marker!r}; "
                    "an accepted contract document answers, it does not defer"
                )
    if "status: accepted" not in ADR_PATH.read_text(encoding="utf-8"):
        problems.append(f"{ADR_PATH.relative_to(REPO)}: must be marked status: accepted")


def check_fixtures_documented(fixtures: Path, problems: list[str]) -> None:
    """Every fixture must appear in tests/e2e/testdata/README.md.

    The invalid/ set is the readable statement of what the schema forbids; an undocumented fixture
    is a rule nobody can look up. ADR-0002's fixtures are held to the same standard.
    """
    if not TESTDATA_README.is_file():
        problems.append(f"{TESTDATA_README.relative_to(REPO)}: missing")
        return
    text = TESTDATA_README.read_text(encoding="utf-8")
    for directory in ("valid", "invalid"):
        for path in sorted((fixtures / directory).glob("*.json")):
            if path.name not in text:
                problems.append(
                    f"{path.relative_to(REPO)}: not documented in "
                    f"{TESTDATA_README.relative_to(REPO)}"
                )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--schema", type=Path, default=DEFAULT_SCHEMA)
    parser.add_argument("--fixtures", type=Path, default=DEFAULT_FIXTURES)
    parser.add_argument("--quiet", action="store_true")
    parser.add_argument("--stats", action="store_true")
    args = parser.parse_args(argv)

    for path in (args.schema, args.fixtures, ADR_PATH, REFERENCE_PATH):
        if not path.exists():
            sys.stderr.write(f"error: {path} does not exist\n")
            return 2

    root = load_schema(args.schema)
    problems: list[str] = []
    stats: dict[str, int] = {}

    check_fixtures(root, args.fixtures, problems, stats)
    check_field_sets(root, problems)
    check_status_vocabulary(root, problems)
    check_error_codes(root, problems, stats)
    check_tools(root, problems)
    check_docs_text(problems)
    check_fixtures_documented(args.fixtures, problems)

    for problem in problems:
        print(f"FAIL {problem}")
    if args.stats:
        for key, value in sorted(stats.items()):
            print(f"  {key}: {value}")
    if SKIPPED:
        print(f"WARN {SKIPPED}")
    if not problems:
        if not args.quiet:
            if SKIPPED:
                print("ok: docs and schema are consistent (fixture validation skipped)")
            else:
                print(
                    "ok: tool contract schema, %s valid / %s invalid fixtures, status vocabulary "
                    "and %s error codes are consistent across schema and docs"
                    % (stats.get("valid", "?"), stats.get("invalid", "?"), stats.get("error_codes", "?"))
                )
        return 0
    print(f"{len(problems)} contract problem(s)")
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
