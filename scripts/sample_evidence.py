#!/usr/bin/env python3
"""Build a readable `.evidence` tree from the checked-in e2e goldens.

    scripts/sample_evidence.py [-o OUT] [--no-extra-commits] [-q]

docs/evidence-format.md documents the on-disk contract for whoever writes a
downstream cross-commit scanner, and every `jq` recipe in it is written against a
real tree. Nobody should have to install docker and act to read those recipes, so
this script lays the golden files in `tests/e2e/testdata/expected/` out under the
layout ADR-0002 §1.1 pins — the same shape a real server writes (see
`scripts/mcp_call.sh run_workflow …` in the README runbook).

What it writes:

    <OUT>/benchmark/<full-sha>/<run_id>.jsonl     # copied verbatim from the goldens
    <OUT>/coverage/<full-sha>/<run_id>.jsonl
    <OUT>/README.md                               # the ADR-0002 §1.4 stub

Nothing else. `runs/` and `incoming/` are deliberately **not** simulated: ADR-0002
§1.1 keeps them out of the cross-commit contract ("a scanner walks `benchmark/` and
`coverage/` only"), so a scanner written against this tree cannot accidentally come
to depend on them.

By default it also stages two *later* commits (`--no-extra-commits` turns this off):
copies of the goldens with `commit`, `run_id` and `started_at` re-stamped. One commit
cannot exercise a cross-commit scan — "newest value per metric across commits" is the
scan the contract exists to serve — and re-stamping those three fields is the only way
to get three commits' worth of honest, schema-valid records out of one captured run.
Every other field stays the goldens' own, and the printed manifest says which commits
are synthetic.

The result is contract-valid: `cargo run -- validate-evidence <OUT>` exits 0 on it,
which is what `scripts/evidence_recipes.sh` relies on.

Exit status: 0 wrote the tree, 1 could not (bad fixtures, unwritable OUT).
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import sys
from datetime import datetime, timedelta, timezone
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
GOLDEN_DIR = REPO / "tests" / "e2e" / "testdata" / "expected"
README_STUB = (
    "act-mcp evidence (sample tree, built by scripts/sample_evidence.py from\n"
    "tests/e2e/testdata/expected). Local build output, safe to delete, format\n"
    "defined by docs/adr/0002-evidence-jsonl-contract.md. Do not hand-edit.\n"
)

# Two commits after the fixtures' own, oldest → newest, so `ls` order (ADR-0002 §1.2:
# run_id sorts chronologically) and `git log` order (newest first) disagree in the way
# a real scan has to handle. Any 40-hex string is a legal `commit` as far as the schema
# is concerned; these are chosen to read as obviously synthetic.
EXTRA_COMMITS = [
    ("3a91c7d2e4f608b1c5d7e9f0a2b4c6d8e0f13579", timedelta(days=1)),
    ("4b02d8e3f5a719c2d6e8f0a3b5c7d9e1f2a4b6c8", timedelta(days=2)),
]

COMMIT_RE = re.compile(r"^[0-9a-f]{40}$")
RUN_ID_RE = re.compile(r"^[0-9]{8}T[0-9]{6}Z-[0-9a-f]{6}$")
INSTANT_RE = re.compile(r"^([0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2})\.([0-9]{3})Z$")


def restamp(line: str, commit: str, shift: timedelta) -> tuple[str, str]:
    """Re-attribute one record line to `commit`, `shift` later.

    Returns `(new line, new run_id)`. Only the three fields a re-stamp can be trusted
    to change are touched: `commit` (the join key the file is filed under),
    `started_at`, and the `run_id` that `started_at` implies (ADR-0002 §1.2). Every
    measured field is untouched.
    """
    record = json.loads(line)
    match = INSTANT_RE.match(record["started_at"])
    if not match:
        raise ValueError(f"{record['started_at']}: not the pinned millisecond form")
    when = datetime.strptime(match.group(1), "%Y-%m-%dT%H:%M:%S").replace(
        tzinfo=timezone.utc
    ) + shift
    record["commit"] = commit
    record["started_at"] = f"{when:%Y-%m-%dT%H:%M:%S}.{match.group(2)}Z"
    # Keep the fixture's random suffix so a re-stamped run is still traceable to it.
    record["run_id"] = f"{when:%Y%m%dT%H%M%SZ}-{record['run_id'][-6:]}"
    # Compact, and insertion order is the parsed file's order, so the envelope stays in
    # the pinned key order (ADR-0002 §6.1). This is a fixture builder, not the writer,
    # so no byte-stability claim is made beyond that.
    return json.dumps(record, separators=(",", ":"), ensure_ascii=False), record["run_id"]


def read_golden(golden: Path) -> tuple[str, str, str, list[str]]:
    """`(kind, commit, run_id, lines)` for one golden, or raise.

    One file is exactly one run and one kind (ADR-0002 §6.1), and the layout this
    script derives depends on that, so it is checked rather than assumed.
    """
    lines = [line for line in golden.read_text().splitlines() if line.strip()]
    if not lines:
        raise ValueError(f"{golden}: empty")
    first = json.loads(lines[0])
    kind, commit, run_id = first["kind"], first["commit"], first["run_id"]
    if kind not in ("benchmark", "coverage"):
        raise ValueError(f"{golden}: kind {kind!r} is not filed under a kind directory")
    if not COMMIT_RE.match(commit):
        raise ValueError(f"{golden}: commit {commit!r} is not 40 lowercase hex")
    if not RUN_ID_RE.match(run_id):
        raise ValueError(f"{golden}: run_id {run_id!r} is not the pinned shape")
    for line in lines:
        record = json.loads(line)
        if (record["kind"], record["commit"], record["run_id"]) != (kind, commit, run_id):
            raise ValueError(f"{golden.name}: mixes runs or kinds (ADR-0002 §6.1)")
    return kind, commit, run_id, lines


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Lay the checked-in e2e goldens out as a real .evidence tree.",
    )
    parser.add_argument(
        "-o",
        "--out",
        default=os.environ.get("ACT_MCP_SAMPLE_EVIDENCE")
        or str(Path(os.environ.get("TMPDIR", "/tmp")) / "act-mcp-evidence-demo"),
        help="evidence root to build (default: $TMPDIR/act-mcp-evidence-demo)",
    )
    parser.add_argument(
        "--no-extra-commits",
        action="store_true",
        help="only the goldens' own commit; stage no re-stamped later commits",
    )
    parser.add_argument("-q", "--quiet", action="store_true", help="print the path only")
    args = parser.parse_args()

    out = Path(args.out).expanduser().resolve()
    if not GOLDEN_DIR.is_dir():
        print(f"sample_evidence: no goldens in {GOLDEN_DIR}", file=sys.stderr)
        return 1
    goldens = sorted(GOLDEN_DIR.glob("*.jsonl"))
    if not goldens:
        print(f"sample_evidence: no *.jsonl in {GOLDEN_DIR}", file=sys.stderr)
        return 1

    # (kind, commit, run_id, synthetic, path, body)
    targets: list[tuple[str, str, str, bool, Path, str]] = []
    try:
        for golden in goldens:
            kind, commit, run_id, lines = read_golden(golden)
            targets.append(
                (
                    kind,
                    commit,
                    run_id,
                    False,
                    out / kind / commit / f"{run_id}.jsonl",
                    "".join(f"{line}\n" for line in lines),
                )
            )
            if args.no_extra_commits:
                continue
            for extra_commit, shift in EXTRA_COMMITS:
                stamped = [restamp(line, extra_commit, shift) for line in lines]
                stamped_lines = [new_line for new_line, _run in stamped]
                later_run = stamped[0][1]
                targets.append(
                    (
                        kind,
                        extra_commit,
                        later_run,
                        True,
                        out / kind / extra_commit / f"{later_run}.jsonl",
                        "".join(f"{line}\n" for line in stamped_lines),
                    )
                )
    except ValueError as err:
        print(f"sample_evidence: {err}", file=sys.stderr)
        return 1

    if out.exists():
        # A demo tree is disposable by definition; not clobbering a real evidence root
        # matters more than convenience, so only delete what is obviously ours.
        marker = out / "README.md"
        if marker.exists() and "act-mcp evidence" not in marker.read_text():
            print(
                f"sample_evidence: {out} does not look like a sample tree; pick -o",
                file=sys.stderr,
            )
            return 1
        shutil.rmtree(out)
    for _kind, _commit, _run, _synthetic, path, body in targets:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(body)
    (out / "README.md").write_text(README_STUB)

    if args.quiet:
        print(out)
        return 0

    print(f"evidence root: {out}")
    print(f"from goldens:  {GOLDEN_DIR.relative_to(REPO)}")
    print("commits:")
    for commit in sorted({t[1] for t in targets}):
        synthetic = next(t[3] for t in targets if t[1] == commit)
        note = "re-stamped copy" if synthetic else "the goldens, verbatim"
        runs = sorted(t[2] for t in targets if t[1] == commit)
        print(f"  {commit}  ({note}, {len(runs)} runs)")
        for run_id in runs:
            kind = next(t[0] for t in targets if t[1] == commit and t[2] == run_id)
            print(f"    {kind}/{run_id}.jsonl")
    print("files:")
    for _kind, _commit, _run, _synthetic, path, _body in sorted(
        targets, key=lambda t: t[4].parts
    ):
        print(f"  {path.relative_to(out)}")
    print(f"\nnext: cargo run -- validate-evidence {out} --stats")
    return 0


if __name__ == "__main__":
    sys.exit(main())
