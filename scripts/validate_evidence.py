#!/usr/bin/env python3
"""Validate .evidence JSONL (and run manifests) against the JSON Schema 2020-12 contract.

This is the dev-time reference checker for ADR-0002. It exists so the contract can be
enforced before the Rust side exists, and so schema/evidence.schema.json is exercised by
something other than the code that wrote it. Once `act-mcp validate-evidence` (ticket
mact-f1w.9) lands, CI may call either one; this script must stay honest about the same
rules.

Usage:
    scripts/validate_evidence.py [PATH ...] [--schema S] [--meta-schema M] [--stats]

With no PATH it walks ./tests/e2e/testdata/expected.

PATH may be a .jsonl file, a meta.json file, or a directory (walked recursively; only
`*.jsonl` and `meta.json` are picked up, dot-files and `.tmp` files are skipped because
those are in-flight temp files per ADR-0002 section 6).

Exit status: 0 all good, 1 validation errors (printed as path:line: message),
2 usage/environment error (e.g. missing jsonschema).
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Iterable, Iterator, NamedTuple

try:
    from jsonschema import Draft202012Validator
    from jsonschema.exceptions import SchemaError
except ModuleNotFoundError:  # pragma: no cover - environment problem, not a contract problem
    sys.stderr.write(
        "error: python package 'jsonschema' is required (pip install jsonschema)\n"
    )
    raise SystemExit(2)

RECORD_KINDS = ("benchmark", "coverage")
TMP_SUFFIXES = (".tmp",)
KIND_DIRS = ("benchmark", "coverage", "runs", "incoming")


class Problem(NamedTuple):
    path: str
    line: int | None
    message: str

    def __str__(self) -> str:
        loc = f"{self.path}:{self.line}" if self.line else self.path
        return f"{loc}: {self.message}"


def load_schema(path: Path) -> Draft202012Validator:
    """Compile a schema and prove it is a legal 2020-12 schema before using it."""
    try:
        raw = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise SystemExit(f"error: cannot read schema {path}: {exc}")
    try:
        Draft202012Validator.check_schema(raw)
    except SchemaError as exc:
        raise SystemExit(f"error: {path} is not a valid 2020-12 schema: {exc.message}")
    return Draft202012Validator(raw)


def discover(paths: Iterable[Path]) -> list[Path]:
    out: list[Path] = []
    for root in paths:
        if root.is_file():
            out.append(root)
        elif root.is_dir():
            for cand in sorted(root.rglob("*")):
                if cand.is_file() and cand.suffix in (".jsonl", ".json"):
                    out.append(cand)
    return out


def jsonl_candidates(paths: Iterable[Path]) -> Iterator[Path]:
    """Yield evidence JSONL files; skip dot-files, temp files and foreign names."""
    for path in discover(paths):
        if path.name.startswith(".") or path.name.endswith(TMP_SUFFIXES):
            continue
        if path.suffix == ".jsonl":
            yield path


def meta_candidates(paths: Iterable[Path]) -> Iterator[Path]:
    for path in discover(paths):
        if path.name.startswith("."):
            continue
        if path.name == "meta.json":
            yield path


def format_error(err) -> str:
    where = "/".join(str(p) for p in err.absolute_path) or "<root>"
    return f"{where}: {err.message}"


def check_document(validator: Draft202012Validator, doc, label: str, line: int | None):
    for err in sorted(validator.iter_errors(doc), key=lambda e: str(e.absolute_path)):
        yield Problem(label, line, format_error(err))


def layout_problems(path: Path, doc: dict, lineno: int) -> list[Problem]:
    """Rules the on-disk layout imposes but the record schema cannot see.

    .evidence/<kind>/<full-sha>/<run_id>.jsonl is the only shape we guarantee, so if a
    file *looks* like it, it must be internally consistent: the directory must name the
    commit every record carries, and the filename must name the run every record carries.
    """
    problems: list[Problem] = []
    parts = path.parts
    if len(parts) >= 3 and parts[-3] in KIND_DIRS and parts[-2] != "incoming":
        sha_dir, stem = parts[-3], path.stem
        if len(sha_dir) == 40:
            commit = doc.get("commit")
            if commit is not None and commit != sha_dir:
                problems.append(
                    Problem(
                        str(path),
                        lineno,
                        f"record commit {commit} does not match directory <{sha_dir}>",
                    )
                )
        if stem != "act" and doc.get("run_id") not in (None, stem):
            problems.append(
                Problem(
                    str(path),
                    lineno,
                    f"record run_id {doc.get('run_id')!r} does not match filename {path.name!r}",
                )
            )
    return problems


def check_jsonl(path: Path, validator: Draft202012Validator, stats: dict) -> list[Problem]:
    """Validate one evidence file: byte rules first, then one schema check per record."""
    problems: list[Problem] = []
    raw = path.read_bytes()

    if raw.startswith(b"\xef\xbb\xbf"):
        problems.append(Problem(str(path), 0, "file starts with a UTF-8 BOM"))
    if b"\r" in raw:
        problems.append(Problem(str(path), 0, "file contains CR; evidence is LF-only"))
    if raw and not raw.endswith(b"\n"):
        problems.append(
            Problem(str(path), 0, "file does not end with LF; a missing final newline means a torn write")
        )
    if b"\x00" in raw:
        problems.append(Problem(str(path), 0, "file contains a NUL byte"))
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError as exc:
        problems.append(Problem(str(path), 0, f"file is not valid UTF-8: {exc}"))
        return problems

    run_ids: set[str] = set()
    for lineno, line in enumerate(text.split("\n")[:-1] if text.endswith("\n") else text.split("\n"), 1):
        if line == "":
            problems.append(Problem(str(path), lineno, "blank line"))
            continue
        try:
            doc = json.loads(line, parse_constant=_reject_constant)
        except json.JSONDecodeError as exc:
            problems.append(Problem(str(path), lineno, f"not a JSON value: {exc.msg}"))
            continue
        if not isinstance(doc, dict):
            problems.append(Problem(str(path), lineno, "line is not a JSON object"))
            continue

        before = len(problems)
        problems.extend(check_document(validator, doc, str(path), lineno))
        problems.extend(layout_problems(path, doc, lineno))
        if len(problems) == before:
            kind = doc.get("kind")
            stats.setdefault(kind, {})
            key = doc.get("commit", "?")
            stats[kind][key] = stats[kind].get(key, 0) + 1
        if isinstance(doc.get("run_id"), str):
            run_ids.add(doc["run_id"])

    if len(run_ids) > 1:
        problems.append(
            Problem(
                str(path),
                0,
                f"one file must carry exactly one run; found run_ids {sorted(run_ids)}",
            )
        )
    return problems


def _reject_constant(name: str) -> float:
    raise ValueError(f"non-finite number literal {name!r} is not valid JSON")


def check_meta(path: Path, validator: Draft202012Validator) -> list[Problem]:
    try:
        doc = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
        return [Problem(str(path), None, f"cannot parse manifest: {exc}")]
    return list(check_document(validator, doc, str(path), None))


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(
        prog="validate_evidence.py",
        description="Validate .evidence JSONL and run manifests against the ADR-0002 schema.",
    )
    parser.add_argument("paths", nargs="*", default=["tests/e2e/testdata/expected"], type=Path)
    parser.add_argument("--schema", type=Path, default=Path("schema/evidence.schema.json"))
    parser.add_argument("--meta-schema", type=Path, default=Path("schema/run-meta.schema.json"))
    parser.add_argument("--stats", action="store_true", help="print per-kind / per-commit counts")
    args = parser.parse_args(argv)

    validator = load_schema(args.schema)
    meta_validator = load_schema(args.meta_schema)
    files = list(jsonl_candidates(args.paths))
    metas = list(meta_candidates(args.paths))
    if not files and not metas:
        sys.stderr.write(f"error: nothing to validate under {args.paths}\n")
        return 2

    stats: dict = {}
    problems: list[Problem] = []
    for path in files:
        problems.extend(check_jsonl(path, validator, stats))
    for path in metas:
        problems.extend(check_meta(path, meta_validator))

    for problem in problems:
        print(str(problem), file=sys.stderr)

    if args.stats:
        for kind in sorted(stats):
            for commit, count in sorted(stats[kind].items()):
                print(f"{kind}\t{commit}\t{count}")

    checked = len(files) + len(metas)
    if problems:
        print(f"FAIL {len(problems)} problem(s) in {checked} file(s)")
        return 1
    print(f"OK {checked} file(s), {sum(sum(c.values()) for c in stats.values())} record(s) valid")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
