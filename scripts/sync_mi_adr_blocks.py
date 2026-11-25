#!/usr/bin/env python3
"""Regenerate the two normative fenced blocks in ADR-0007 from schema/mi-definition.json.

The ADR is required to carry the constants and the bands *verbatim*, so a reader can recompute every
stored number from the ADR alone (ADR-0007 §0, §5.1), and `validate_mi_definition.py` fails the run
on any leaf that differs. Copying them by hand is how that drifts, so they are generated: run this
after any change to the JSON, then commit both.

`--check` reports drift without writing, which is what `scripts/ci.sh` uses: a regenerated file and a
regenerated schema landing in separate commits would otherwise pass.
"""
from __future__ import annotations

import argparse
import difflib
import json
import pathlib
import re
import sys

HERE = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import validate_mi_definition as check  # noqa: E402

ADR = check.DEFAULT_ADR
DEFINITION = HERE.parent / "schema" / "mi-definition.json"
LABELS = ("mi-constants", "mi-bands")


def render(text: str) -> str:
    definition = json.loads(DEFINITION.read_text())
    payloads = {
        "mi-constants": check.canonical_constants(definition),
        "mi-bands": definition["bands"],
    }
    for label in LABELS:
        body = json.dumps(payloads[label], indent=2, ensure_ascii=False)
        block = f"<!-- {label}:begin -->\n```json\n{body}\n```\n<!-- {label}:end -->"
        pattern = re.compile(
            rf"<!-- {re.escape(label)}:begin -->.*?<!-- {re.escape(label)}:end -->", re.S
        )
        if not pattern.search(text):
            raise SystemExit(f"{ADR}: no {label} block to replace")
        text = pattern.sub(lambda _m: block, text, count=1)
    return text


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="report drift and exit 1")
    args = parser.parse_args()
    original = ADR.read_text()
    rendered = render(original)
    if rendered == original:
        print(f"{ADR.name}: mi-constants and mi-bands match schema/mi-definition.json")
        return 0
    if args.check:
        sys.stderr.write(
            "".join(
                difflib.unified_diff(
                    original.splitlines(keepends=True),
                    rendered.splitlines(keepends=True),
                    fromfile=str(ADR) + " (committed)",
                    tofile=str(ADR) + " (regenerated)",
                )
            )[:4000]
        )
        print(
            "FAIL: the ADR's normative blocks do not match schema/mi-definition.json; run "
            "scripts/sync_mi_adr_blocks.py",
            file=sys.stderr,
        )
        return 1
    ADR.write_text(rendered)
    print(f"regenerated mi-constants and mi-bands in {ADR.name}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
