#!/usr/bin/env python3
"""scripts/realdata_report.py — the two JSON jobs scripts/smoke-real-act.sh delegates.

Both are analysis of files a real-act smoke run produced, and both are easier to get right —
and to read — in Python than in shell. Nothing here talks to a server, to act, or to a
container runtime: the inputs are ordinary files on disk, which is what makes them runnable
(and testable) without docker.

    shape     compare a captured `act --json` log against tests/realdata/golden/act-shape.json
    expected  normalise the records a run published and diff them against committed goldens
    normalise print the normalised form of one evidence file (the diff's primitive)

WHY normalisation exists at all, and its one rule: a `.evidence` record carries facts about
the machine that produced it (run id, commit, instants, hostname, durations, pids) that
cannot be pinned without lying. So **nothing is deleted and nothing is compared loosely**:
every volatile value is replaced by a named placeholder, the list of placeholders is right
here, and everything else is compared byte for byte. It is the same rule and the same
placeholders as `tests/e2e/normalize.rs`, so a reader who knows one knows the other.

    {RUN_ID}   a run id (`YYYYMMDDTHHMMSSZ-xxxxxx`)          {COMMIT}  a full 40-hex commit
    {TS}       an RFC 3339 instant                           {HOST}    the reporting host
    {NUMBER}   the numeric value of duration/pid/size keys

Exit status: 0 the check passed, 3 the check ran and found a divergence (only `shape`, and
only a divergence in a field something actually depends on), 2 bad usage, 1 the files were
unreadable.

Dependencies: the standard library only.
"""

from __future__ import annotations

import argparse
import io
import json
import re
import sys
import difflib
import tempfile
from pathlib import Path
from typing import Any, Iterable

# ---------------------------------------------------------------- the volatile shapes

RUN_ID_RE = re.compile(r"\b\d{8}T\d{6}Z-[0-9a-f]{6}\b")
INSTANT_RE = re.compile(r"\b\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z\b")
COMMIT_RE = re.compile(r"\b[0-9a-f]{40}\b")

#: Keys whose *number* describes this process rather than the contract. Deliberately short:
#: coverage counts, benchmark values and exit codes are data and stay compared.
NUMERIC_KEYS = ("pid", "duration_ms", "took_ms", "log_bytes", "log_stderr_bytes", "size_bytes")
NUMERIC_RES = [
    re.compile(r'("%s"\s*:\s*)-?\d+' % key) for key in NUMERIC_KEYS
]


def normalise(text: str) -> str:
    """Every volatile value in `text` becomes its placeholder."""
    text = INSTANT_RE.sub("{TS}", text)
    text = RUN_ID_RE.sub("{RUN_ID}", text)
    text = COMMIT_RE.sub("{COMMIT}", text)
    text = re.sub(r'("host"\s*:\s*)"[^"]*"', r'\1"{HOST}"', text)
    for pattern in NUMERIC_RES:
        text = pattern.sub(r"\1{NUMBER}", text)
    return text


# ---------------------------------------------------------------------- shape analysis

KIND_PRIMITIVES = {
    "string": str,
    "integer": int,
    "number": (int, float),
    "boolean": bool,
    "null": type(None),
    "object": dict,
    "array": list,
}


def json_type(value: Any) -> str:
    """The JSON type name of one decoded value (`bool` before `int`: Python's allergy)."""
    if value is None:
        return "null"
    if isinstance(value, bool):
        return "boolean"
    if isinstance(value, int):
        return "integer"
    if isinstance(value, float):
        return "number"
    if isinstance(value, str):
        return "string"
    if isinstance(value, list):
        return "array"
    if isinstance(value, dict):
        return "object"
    return "unknown"


class Observation:
    """What a capture actually did with one field."""

    def __init__(self, name: str) -> None:
        self.name = name
        self.types: set[str] = set()
        self.count = 0
        self.samples: list[str] = []
        self.assumed_value_hits: set[str] = set()

    def add(self, value: Any, value_limit: int) -> None:
        self.count += 1
        self.types.add(json_type(value))
        if len(self.samples) < value_limit:
            text = json.dumps(value, ensure_ascii=False)
            self.samples.append(text if len(text) <= 60 else text[:57] + "...")
        if isinstance(value, str):
            self.assumed_value_hits.add(value)

    @property
    def types_text(self) -> str:
        return ",".join(sorted(self.types)) if self.types else "—"


def read_log_lines(path: Path, limit: int | None) -> list[Any]:
    """Every line of a captured log, decoded where it decodes.

    A line that is not a JSON object is kept as a `str` so the report can count it: act's
    own stderr, a torn write, or a run captured without `--json` all look like that, and
    "N of M lines were JSON objects" is a fact the smoke should print rather than hide.
    """
    lines: list[Any] = []
    with path.open("r", encoding="utf-8", errors="replace") as handle:
        for number, raw in enumerate(handle, start=1):
            if limit is not None and number > limit:
                break
            text = raw.strip()
            if not text:
                continue
            try:
                lines.append(json.loads(text))
            except json.JSONDecodeError:
                lines.append(text)
    return lines


def observe(lines: Iterable[Any], sample_limit: int) -> tuple[dict[str, Observation], int, int]:
    """Per-field observations over the JSON-object lines of a capture."""
    observations: dict[str, Observation] = {}
    objects = 0
    non_objects = 0
    for line in lines:
        if not isinstance(line, dict):
            non_objects += 1
            continue
        objects += 1
        for key, value in line.items():
            observations.setdefault(key, Observation(key)).add(value, sample_limit)
    return observations, objects, non_objects


def type_matches(observed: Observation, expected: list[str]) -> bool:
    """Whether every observed JSON type is allowed by the manifest.

    `integer` in the manifest also accepts `number`: act is not obliged to keep a duration
    whole across a formatter change, and a float nanosecond count is not a contract break.
    """
    allowed = set(expected)
    for seen in observed.types:
        if seen in allowed:
            continue
        if seen == "number" and "integer" in allowed:
            continue
        return False
    return True


def cmd_shape(args: argparse.Namespace) -> int:
    """The divergence report: what the manifest claims, what the log did."""
    manifest_path = Path(args.manifest)
    capture_path = Path(args.capture)
    try:
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as err:
        print(f"realdata_report: cannot read the manifest {manifest_path}: {err}", file=sys.stderr)
        return 1
    if not capture_path.exists():
        print(f"realdata_report: no capture to read at {capture_path}", file=sys.stderr)
        return 1

    lines = read_log_lines(capture_path, args.max_lines)
    observations, objects, non_objects = observe(lines, args.samples)

    fields: dict[str, dict] = manifest.get("fields", {})
    hard: list[str] = []
    notes: list[str] = []
    rows: list[tuple[str, str, str, str, str]] = []

    # 1. every field the manifest names, checked against what was seen.
    for name, spec in sorted(fields.items()):
        observed = observations.get(name)
        expected = spec.get("type")
        expected_list = [expected] if isinstance(expected, str) else list(expected or [])
        assumed = spec.get("assumed_by") or []
        group = spec.get("group", "?")

        if observed is None:
            seen = "0"
            types = "—"
            if spec.get("group") == "phantom":
                # A field that was never act's, listed so the report stays honest. Its absence
                # is the expected answer, so it is a note about our dead code, not a divergence
                # act caused — and a nightly that is red until somebody deletes a fallback in
                # src/ would be a nightly nobody trusts.
                verdict = "absent (as expected: never act's)"
                notes.append(
                    f"`{name}` is still absent, as it has always been: act never emitted it, so "
                    "the read of it in the extractor is dead code. That is a cleanup for "
                    "this crate, not a change in act."
                )
            elif assumed or spec.get("required"):
                # A field something reads (or every line must have), that never appeared.
                verdict = "MISSING"
                why = "; ".join(assumed) if assumed else "the manifest marks it required"
                hard.append(f"`{name}` never appears in the captured log — {why}")
            else:
                verdict = "absent (optional, nobody depends on it)"
        else:
            seen = str(observed.count)
            types = observed.types_text
            if expected_list and not type_matches(observed, expected_list):
                verdict = f"TYPE: {types} ≠ {','.join(expected_list)}"
                if assumed:
                    hard.append(
                        f"`{name}` arrived as {types}, the manifest says "
                        f"{','.join(expected_list)}, and act_log depends on it"
                    )
                else:
                    notes.append(verdict + f" (`{name}`, unclaimed: ignored)")
                    verdict = f"type {types} (manifest {','.join(expected_list)}), unclaimed"
            elif spec.get("values") and observed.assumed_value_hits:
                allowed = set(spec["values"])
                unexpected = sorted(observed.assumed_value_hits - allowed)
                if unexpected:
                    notes.append(
                        f"`{name}` carried {unexpected}, outside the manifest's "
                        f"{sorted(allowed)}"
                    )
                verdict = f"observed {spec.get('values')}"
            else:
                verdict = "observed"

        rows.append((name, group, seen, types, verdict))

    # 2. fields a live act emitted that the manifest never heard of.
    for name in sorted(set(observations) - set(fields)):
        observed = observations[name]
        rows.append((name, "unclaimed", str(observed.count), observed.types_text, "NEW"))
        notes.append(
            f"`{name}` appeared ({observed.count}×, {observed.types_text}) and no manifest "
            "field claims it; the extractors ignore unknown fields, so this is a documentation "
            "gap, not a break — add it to act-shape.json once a human agrees it is real."
        )

    width = max([len(r[0]) for r in rows] + [len("field")])
    total = objects + non_objects
    print(f"act --json shape report — {capture_path}")
    print(
        f"  {total} line(s): {objects} JSON object(s), {non_objects} not a JSON object"
        + (", %d scanned" % len(lines) if args.max_lines else "")
    )
    print(f"  manifest: {manifest_path}")
    print(f"  {'field'.ljust(width)}  group        seen     types            verdict")
    for name, group, seen, types, verdict in rows:
        print(
            f"  {name.ljust(width)}  {group[:12].ljust(12)}  {seen.rjust(6)}  "
            f"{types[:16].ljust(16)}  {verdict}"
        )

    if hard:
        print("")
        print("DIVERGENCE — a field something in this crate reads did not hold up:")
        for finding in hard:
            print(f"  ✗ {finding}")
    for note in notes:
        print(f"  · {note}")

    for gap in manifest.get("known_gaps", []):
        # A known gap is only worth printing when this capture actually ran into it, i.e. one
        # of the fields it names is missing from the log.
        names = [part.strip().strip("`") for part in gap.get("field", "").split("/")]
        absent = [name for name in names if name not in observations]
        if absent:
            print(f"  · known gap {gap['id']} — triggered here ({', '.join(absent)} absent):")
            print(f"      {gap['gap']}")
            print(f"      what to do: {gap['what_to_do']}")

    if args.json_out:
        payload = {
            "capture": str(capture_path),
            "manifest": str(manifest_path),
            "lines_total": total,
            "lines_json_objects": objects,
            "lines_not_json_objects": non_objects,
            "fields": {
                name: {
                    "count": obs.count,
                    "types": sorted(obs.types),
                    "samples": obs.samples,
                }
                for name, obs in sorted(observations.items())
            },
            "divergences": hard,
            "notes": notes,
        }
        Path(args.json_out).write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")

    if hard and args.strict:
        print("  (--strict-shape: the divergences above are a failure)")
        return 3
    return 0


# ---------------------------------------------------------------------- expected goldens

def evidence_files(directory: Path) -> list[Path]:
    """The contract half of an evidence root: `benchmark/` and `coverage/`, nothing else.

    `runs/<run_id>/act.jsonl` is `.jsonl` too and is NOT the contract (docs/evidence-format.md
    §1: walk benchmark/ and coverage/ and nothing else), so a glob over the whole root would
    quietly turn a raw act log into an "evidence file".
    """
    kinds = ("benchmark", "coverage")
    found: list[Path] = []
    for kind in kinds:
        root = directory / kind
        if root.is_dir():
            found.extend(sorted(p for p in root.rglob("*.jsonl") if p.is_file()))
    return sorted(found)


def kind_of(path: Path, produced: Path) -> str:
    """The first component under the root, which is the kind (ADR-0002 §1.1)."""
    parts = path.relative_to(produced).parts
    return parts[0] if parts else "records"


def cmd_normalise(args: argparse.Namespace) -> int:
    path = Path(args.file)
    try:
        print(normalise(path.read_text(encoding="utf-8")), end="")
    except OSError as err:
        print(f"realdata_report: {err}", file=sys.stderr)
        return 1
    return 0


def cmd_expected(args: argparse.Namespace) -> int:
    """Write or check the normalised goldens for the records a smoke run published."""
    produced = Path(args.produced)
    expected_dir = Path(args.expected)
    if not produced.is_dir():
        print(f"realdata_report: no produced evidence directory at {produced}", file=sys.stderr)
        return 1
    files = evidence_files(produced)
    if not files:
        print(f"realdata_report: no .jsonl files under {produced}", file=sys.stderr)
        return 1

    if args.write:
        expected_dir.mkdir(parents=True, exist_ok=True)
        seen: dict[str, int] = {}
        for path in files:
            seen[kind_of(path, produced)] = seen.get(kind_of(path, produced), 0) + 1
        for kind, count in sorted(seen.items()):
            if count > 1:
                print(
                    f"  WARNING: {count} {kind} files under {produced}; the golden is one file "
                    "per kind, so the last one wins. Point --produced at one run's tree.",
                    file=sys.stderr,
                )
        for path in files:
            target = expected_dir / f"{kind_of(path, produced)}.jsonl"
            target.write_text(normalise(path.read_text(encoding="utf-8")), encoding="utf-8")
            print(f"  wrote {target} ({sum(1 for _ in target.open())} normalised line(s))")
        print(
            "  NOTE: these are goldens now. Read them before committing: a golden that was "
            "written by the code under test and never read by a human is a snapshot, not a test.",
            file=sys.stderr,
        )
        return 0

    have = {p.stem for p in expected_dir.glob("*.jsonl")} if expected_dir.is_dir() else set()
    missing = sorted({kind_of(p, produced) for p in files} - have)
    if missing or not expected_dir.is_dir():
        names = ", ".join(missing) or "(no expected dir)"
        print(f"realdata_report: no committed expected records for: {names}")
        print(
            "  that is the state of the repo until a real-act run has produced them — see "
            "tests/realdata/expected/README.md. The smoke's own value assertions still ran; "
            "the byte-for-byte diff starts on the next run after someone runs --write-expected.",
            file=sys.stderr,
        )
        return 0

    failed = 0
    for path in files:
        kind = kind_of(path, produced)
        expected = expected_dir / f"{kind}.jsonl"
        got = normalise(path.read_text(encoding="utf-8")).splitlines()
        want = expected.read_text(encoding="utf-8").splitlines()
        if got == want:
            print(f"  ✓ {kind}: {len(got)} record(s) match {expected} byte for byte (after normalisation)")
            continue
        failed += 1
        print(f"  ✗ {kind}: differs from {expected}")
        for line in difflib.unified_diff(want, got, fromfile=str(expected), tofile=str(path), lineterm=""):
            print(f"      {line}")
    return 3 if failed else 0


def cmd_selftest(args: argparse.Namespace) -> int:
    """Check this tool against synthetic captures, so CI can trust the checker itself.

    The point is not to re-assert act's shape (only act can) but to pin the *logic*: that a
    depended-on field going missing is a divergence, that a phantom field going missing is not,
    that an unclaimed new field is reported, that a type change is caught, and that
    `normalise` placeholders are what the goldens claim they are. Nothing here needs a run.
    """
    repo = Path(__file__).resolve().parent.parent
    manifest = repo / "tests" / "realdata" / "golden" / "act-shape.json"
    failures: list[str] = []

    def check(name: str, ok: bool) -> None:
        print(f"  {'✓' if ok else '✗'} {name}")
        if not ok:
            failures.append(name)

    # --- normalise: the placeholders the goldens promise, and nothing looser than that.
    sample = (
        '{"run_id":"20261110T004203Z-a1b2c3",'
        '"commit":"2f7c1a4b9e0d3c6a5b8f1e2d4c6a8b0f3e5d7c91",'
        '"started_at":"2026-11-10T01:15:07.004Z","host":"build-box-07",'
        '"duration_ms":61233,"exit_code":0,"lines_hit":2199}'
    )
    text = normalise(sample)
    check(
        "normalise replaces run id, commit, instant, host and duration",
        all(
            token in text
            for token in ("{RUN_ID}", "{COMMIT}", "{TS}", "{HOST}", '"duration_ms":{NUMBER}')
        ),
    )
    check(
        "normalise leaves the data alone (exit_code, lines_hit)",
        '"exit_code":0' in text and '"lines_hit":2199' in text,
    )

    # --- shape: synthetic captures through the real manifest.
    def report(entries: list, manifest_path: Path | None = None) -> tuple[int, str]:
        import contextlib

        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "act.jsonl"
            body = "\n".join(
                item if isinstance(item, str) else json.dumps(item, separators=(",", ":"))
                for item in entries
            )
            path.write_text(body + "\n", encoding="utf-8")
            buffer = io.StringIO()
            options = argparse.Namespace(
                capture=str(path), manifest=str(manifest_path or manifest), json_out=None,
                max_lines=None, samples=1, strict=True,
            )
            with contextlib.redirect_stdout(buffer):
                code = cmd_shape(options)
            return code, buffer.getvalue()

    def entry(**fields: object) -> dict:
        line = {"level": "info", "msg": "m", "time": "2026-11-10T00:00:00Z"}
        line.update(fields)
        return line

    jobish = dict(job="coverage", jobID="coverage", dryrun=False, matrix=None)
    complete = [
        entry(**jobish),
        entry(**jobish, step="s", stepID=["1"], stage="main", raw_output=True,
              msg='ACTMCP_BENCHMARK {"name":"x","value":1}'),
        entry(**jobish, step="s", stepID=["1"], stage="main", stepResult="success",
              executionTime=1200000000, msg="  \u2705  Success - Main s [1.2s]"),
        entry(job="coverage", jobID="coverage", jobResult="success", msg="\U0001f3c1  Job succeeded"),
        entry(brand_new_field={"a": 1}, msg="act noticed something new"),
    ]
    code, out = report(complete)
    check("a complete capture raises no divergence", code == 0)
    check("an unclaimed field is reported as NEW", "brand_new_field" in out and "NEW" in out)

    # A `phantom` row is a field this crate once mistook for one of act's. The committed
    # manifest has none since mact-dfu deleted the `Output`/`Error` rows together with the
    # read that made them look claimed (see its `resolved_gaps`), so this branch is pinned
    # with a manifest of its own: what is under test is the checker's rule — an absent
    # phantom is a note about our dead code, an absent depended-on field is a divergence —
    # and that rule must not quietly rot into "always red" or "never red".
    with tempfile.TemporaryDirectory() as tmp:
        ghostly = json.loads(manifest.read_text(encoding="utf-8"))
        ghostly["fields"]["ghost_output"] = {
            "group": "phantom",
            "type": "string",
            "required": False,
            "assumed_by": ["a read no released act can satisfy"],
        }
        ghost_manifest = Path(tmp) / "ghost-shape.json"
        ghost_manifest.write_text(json.dumps(ghostly), encoding="utf-8")

        code, out = report(complete, ghost_manifest)
        check(
            "a phantom field's absence is a note, not a divergence",
            "ghost_output" in out and "never act's" in out and code == 0
            and "DIVERGENCE" not in out,
        )
        settled = [
            {k: v for k, v in e.items() if k != "brand_new_field"} for e in complete
        ] + [entry(ghost_output="surprise")]
        code, out = report(settled, ghost_manifest)
        check(
            "a phantom field that does appear is judged on its type, not as something new",
            code == 0 and "NEW" not in out,
        )

    def without(field: str, entries: list) -> list:
        return [{k: v for k, v in e.items() if k != field} for e in entries]

    code, out = report(without("executionTime", complete))
    check("a log with no executionTime IS a divergence (act < v0.2.76)",
          code == 3 and "executionTime" in out and "DIVERGENCE" in out)

    mistyped = [{**e, "executionTime": "1.2s"} if "executionTime" in e else e for e in complete]
    code, out = report(mistyped)
    check("executionTime arriving as a string is a divergence", code == 3 and "TYPE" in out)

    code, out = report(["this is not json at all"])
    check("a log that is not JSON at all is counted, not crashed on",
          "not a JSON object" in out and code == 3)

    print(f"realdata_report selftest: {len(failures)} failure(s)")
    return 1 if failures else 0


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(
        prog="scripts/realdata_report.py",
        description=__doc__.splitlines()[0],
        epilog=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    sub = parser.add_subparsers(dest="command", required=True)

    shape = sub.add_parser("shape", help="compare a captured act --json log against the shape manifest")
    shape.add_argument("--capture", required=True, help="a captured act --json log (act.jsonl)")
    shape.add_argument("--manifest", required=True, help="tests/realdata/golden/act-shape.json")
    shape.add_argument("--json-out", help="also write the machine-readable report here")
    shape.add_argument("--max-lines", type=int, default=None, help="scan at most N lines")
    shape.add_argument("--samples", type=int, default=3, help="sample values kept per field")
    shape.add_argument(
        "--strict",
        action="store_true",
        help="exit 3 when a depended-on field diverged (default: report and exit 0)",
    )
    shape.set_defaults(func=cmd_shape)

    expected = sub.add_parser("expected", help="diff produced records against committed goldens")
    expected.add_argument("--produced", required=True, help="the run's evidence root (<ws>/.evidence)")
    expected.add_argument("--expected", required=True, help="tests/realdata/expected")
    expected.add_argument(
        "--write", action="store_true", help="write the goldens instead of checking them"
    )
    expected.set_defaults(func=cmd_expected)

    normalise_cmd = sub.add_parser("normalise", help="print one evidence file with volatile fields replaced")
    normalise_cmd.add_argument("file")
    normalise_cmd.set_defaults(func=cmd_normalise)

    selftest = sub.add_parser(
        "selftest",
        help="check this checker against synthetic captures (no run, no act, no docker needed)",
    )
    selftest.set_defaults(func=cmd_selftest)

    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
