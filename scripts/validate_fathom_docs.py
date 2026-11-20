#!/usr/bin/env python3
"""Check the Fathom documentation surface: every flag, key and exit code is documented exactly once,
and no doc names a flag that does not exist.

This is the dev-time checker for the Fathom docs (ticket mact-by1.15: the `README.md` runbook,
`docs/fathom-metrics.md`, `.fathom/LAYOUT.md`). The ticket's acceptance rule is a *cross-check pass* —
"every flag in `fathom --help`, every key in the help overlay and every exit code appears in exactly one
doc page; no doc names a flag that does not exist" — and a rule written only in prose is a rule that
drifts, so it is a script now, and it runs in `scripts/ci.sh`'s `schema` step beside the other
ADR↔artifact checkers.

Why a script and not a review comment: four artifacts describe this surface and only two of them are
documentation. The argv comes from clap (`crates/fathom/src/cli.rs`), the key surface from the help
overlay (`crates/fathom/src/tui/render.rs`), the exit codes from `crates/fathom/src/exit.rs` — all three
move whenever a ticket wires something — and the prose moves when somebody remembers. So the checker asks
the binary what it accepts (`fathom --help`, five times: bare, `tui`, `index`, `report`, `doctor`) rather
than trusting a transcription of it.

A surface is *documented* where it has a row: a table whose header names the surface (`flag`, `key`,
`code`). Prose may mention `--evidence-dir` or the `d` key as often as it likes; two **rows** are two
copies, and two copies is how a runbook ends up documenting a flag that was renamed a year ago.

Checks performed (all of them, every run):

  C1   the argv. Every long flag `fathom --help` prints — for the bare command and for each of the four
       subcommands — has a row in exactly one doc page's flag table, spelled with its value
       (`--from <REV>`, not `--from`) exactly as help spells it. Conversely, inside the Fathom scope
       (the README's `## Fathom` section and all of `docs/fathom-metrics.md`) no `--flag` may be named —
       in a row or in prose — that no `fathom --help` prints. `--help` and `--version` are clap's and
       exempt; flags of other tools (`act-mcp`'s, cargo's) are out of the Fathom scope by construction.
  C2   the key surface. Every key the help overlay lists, on each of its four screens (timeline, diff,
       detail, bench), has a cell in exactly one doc page's key table. A row whose meaning is "a later
       ticket" is a promise rather than a key and is skipped. The overlay may not contain "not wired
       yet": every key it lists is wired, and an overlay is documentation rendered on a screen — the
       `enter` row promising mact-by1.14 after mact-by1.14 shipped is the case that started this.
  C3   the exit codes. Every code in `crates/fathom/src/exit.rs`'s table is a row in exactly one doc
       page's exit-code table.
  C4   wiring. Every relative link and in-page anchor in the Fathom doc pages resolves (file exists and,
       for Markdown, the heading exists under GitHub's anchor rules); `docs/fathom-metrics.md` is linked
       from the README; `.fathom/LAYOUT.md` names `fathom index` and `fathom doctor`, the two commands
       that fix anything wrong inside it; and the README carries the "everything is empty" entry, since
       that is the first thing every new reader meets.

Usage:
    scripts/validate_fathom_docs.py [--repo PATH] [--quiet] [--stats]

The binary is found at `$FATHOM_BIN`, else `$CARGO_TARGET_DIR`/`$CARGO_BUILD_TARGET`'s debug or release
`fathom`, else `target/debug/fathom`, else `cargo run -q -p fathom --` if cargo is on PATH.

Exit status: 0 everything consistent, 1 a documentation violation (one line each), 2 usage or
environment error (a missing page, or no `fathom` to ask).
"""
from __future__ import annotations

import argparse
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

# The pages this checker reads. The flag, key and exit-code tables live in the README's Fathom section;
# a row appearing in two of these pages is two copies that can disagree.
PAGES = ["README.md", "docs/fathom-metrics.md", "docs/testing.md", ".fathom/LAYOUT.md"]

# Where "do not name a flag that does not exist" applies. The act-mcp half of the README documents the
# server's flags, and docs/testing.md quotes cargo's; both are out of scope on purpose.
FATHOM_SCOPE = {"README.md": "## Fathom", "docs/fathom-metrics.md": None}

# clap's own flags, present on every command and documented by `--help` itself.
EXEMPT_FLAGS = {"--help", "--version"}

# Keys the help overlay does not list but the screens answer. `backspace` edits the filter the overlay
# documents under `/`; it belongs in the key table because the table documents the surface.
EXEMPT_KEYS = {"backspace"}

# A row whose meaning is somebody else's ticket is a promise, not a key of this screen.
PROMISE = "later ticket"

# An overlay label spells some keys as words; the key table may use either spelling.
KEY_ALIASES = {"up": {"up", "↑"}, "down": {"down", "↓"}}

# Which table header column names which surface. A table is a flag table when its header row contains a
# cell exactly `flag`, and so on — checked on the header, so the data rows never have to be guessed at.
SURFACE_COLUMNS = {"flag": "flags", "key": "keys", "code": "exit codes"}


# ---------------------------------------------------------------------------------------- the tool


def fathom_command(repo: Path) -> list[str]:
    """How to invoke the binary to ask it what it accepts.

    `$FATHOM_BIN` wins (that is what `scripts/ci.sh` sets, because it knows its own target dir); then the
    usual places, honouring `CARGO_TARGET_DIR` and `CARGO_BUILD_TARGET` the same way `ci.sh` resolves its
    `act-mcp` path; then `cargo run`, which is slower and correct.
    """
    env = os.environ.get("FATHOM_BIN")
    if env:
        return [env]
    target_root = Path(os.environ.get("CARGO_TARGET_DIR", repo / "target"))
    triple = os.environ.get("CARGO_BUILD_TARGET", "")
    for profile in ("debug", "release"):
        candidate = target_root / Path(triple) / profile / "fathom" if triple else target_root / profile / "fathom"
        if candidate.exists():
            return [str(candidate)]
    if shutil.which("cargo"):
        return ["cargo", "run", "--quiet", "-p", "fathom", "--"]
    sys.exit("no fathom binary: set $FATHOM_BIN, build one, or put cargo on PATH")


def help_text(repo: Path, subcommand: list[str]) -> str:
    cmd = fathom_command(repo) + subcommand + ["--help"]
    proc = subprocess.run(cmd, capture_output=True, text=True, check=False)
    if proc.returncode != 0:
        sys.exit(f"`{' '.join(cmd)}` exited {proc.returncode}: {proc.stderr.strip()}")
    return proc.stdout


def flags_from_help(text: str) -> dict[str, set[str]]:
    """`{flag_name: {every spelling help prints it with}}`, both help layouts handled.

    The wide help puts `--from <REV>` on its own line and the compact help inline; a regex over the
    whole text sees both. A flag with a value must be documented with it, because a runbook that writes
    `--from` drops the half a reader gets wrong.
    """
    found: dict[str, set[str]] = {}
    for match in re.finditer(r"(?<![\w-])(--[a-z][a-z0-9-]*)\s*(?:<([A-Z_][A-Z0-9_]*)>)?", text):
        flag, value = match.group(1), match.group(2)
        if flag in EXEMPT_FLAGS:
            continue
        found.setdefault(flag, set()).add(f"{flag} <{value}>" if value else flag)
    return found


def overlay_keys(repo: Path) -> dict[str, set[str]]:
    """`{screen: {key, …}}`, read from the `("key", "meaning")` rows of the help overlay source.

    An overlay is a screen, not a command, so its source is the key surface. Each of the four screens
    has exactly one `for (key, what) in […]` table and each block ends at its `draw_help_box`.
    """
    source = (repo / "crates/fathom/src/tui/render.rs").read_text(encoding="utf-8")
    markers = {
        "timeline": 'for (key, what) in [\n        ("j / k, up / down"',
        "diff": "if matches!(app.screen, Screen::Diff(_))",
        "detail": "fn detail_help",
        "bench": "fn bench_help",
    }
    out: dict[str, set[str]] = {}
    for screen, marker in markers.items():
        start = source.find(marker)
        if start < 0:
            sys.exit(f"C2: cannot find the {screen!r} help block in crates/fathom/src/tui/render.rs")
        end = source.find("draw_help_box", start)
        block = source[start : end if end > 0 else len(source)]
        keys: set[str] = set()
        for row in re.finditer(r'^\s*\(\s*"([^"]+)"\s*,\s*"([^"]*)"', block, re.MULTILINE):
            label, meaning = row.group(1), row.group(2)
            if PROMISE in meaning:
                continue
            for chunk in label.split(","):
                for part in chunk.split("/"):
                    part = part.strip()
                    if not part or (not part[:1].isalnum() and part not in {"/", "?"}):
                        continue
                    keys.add("ctrl-c" if part == "Ctrl-C" else part.lower())
        out[screen] = keys
    return out


# --------------------------------------------------------------------------------------- the pages


def read_pages(repo: Path) -> dict[str, str]:
    pages = {}
    for page in PAGES:
        path = repo / page
        if not path.exists():
            sys.exit(f"missing doc page: {page}")
        pages[page] = path.read_text(encoding="utf-8")
    return pages


def scope(text: str, heading: str | None) -> str:
    """The part of a page the flag-existence rule applies to (`None` is all of it)."""
    if heading is None:
        return text
    start = text.find(f"\n{heading}")
    if start < 0:
        return ""
    rest = text[start:]
    end = rest.find("\n## ", 1)
    return rest[: end if end > 0 else len(rest)]


def strip_fences(text: str) -> str:
    """Drop fenced blocks: a `--flag` in a shell example is a usage, not a definition."""
    return re.sub(r"```.*?```", "", text, flags=re.DOTALL)


def tables(text: str) -> list[tuple[list[str], list[list[str]]]]:
    """Every markdown table as `(lowercased header cells, data rows)`, fenced blocks excluded."""
    out: list[tuple[list[str], list[list[str]]]] = []
    block: list[list[str]] = []
    in_fence = False

    def flush() -> None:
        if len(block) >= 2:
            out.append(([cell.strip().lower() for cell in block[0]], block[1:]))
        block.clear()

    for line in text.splitlines():
        if line.lstrip().startswith("```"):
            in_fence = not in_fence
            flush()
            continue
        stripped = line.strip()
        if in_fence or not stripped.startswith("|"):
            flush()
            continue
        cells = [cell.strip() for cell in stripped.strip("|").split("|")]
        if any(re.fullmatch(r":?-{2,}:?", cell) for cell in cells if cell):
            continue
        block.append(cells)
    flush()
    return out

def surface_cells(pages: dict[str, str], column: str) -> dict[str, list[tuple[str, str]]]:
    """`{token: [(page, cell), …]}` for every cell of the table column that names a surface.

    `column` is `"flag"`, `"key"` or `"code"` — the header cell that makes a table the flag table, the
    key table or the exit-code table. Each backticked span in that column is a token; keys are matched
    case-insensitively (`G` and `g` are the same key), flags and codes are matched as written.
    """
    out: dict[str, list[tuple[str, str]]] = {}
    for page, text in pages.items():
        for header, rows in tables(text):
            index = next(
                (
                    position
                    for position, cell in enumerate(header)
                    if cell.strip().strip("*` ") == column
                ),
                None,
            )
            if index is None or not rows:
                # Not a surface table: its header names no surface column.
                continue
            for row in rows:
                if len(row) <= index:
                    continue
                for span in re.findall(r"`([^`]+)`", row[index]):
                    parts = [part for part in re.split(r"[\s,]+", span) if part]
                    for position, part in enumerate(parts):
                        if column == "flag" and part.startswith("--"):
                            # `fathom report --format <FMT>` names one flag with its value: keep the
                            # command prefix out of the name and the value with the flag it belongs to.
                            value = parts[position + 1] if position + 1 < len(parts) else ""
                            spelled = f"{part} {value}" if re.fullmatch(r"<\w+>", value) else part
                            out.setdefault(part, []).append((page, spelled))
                            continue
                        token = part.lower() if column == "key" else part
                        out.setdefault(token, []).append((page, part))
    return out


# --------------------------------------------------------------------------------------- the checks


def check_flags(pages: dict[str, str], flags: dict[str, set[str]], problems: list[str], stats: dict) -> None:
    """C1: every real flag has exactly one row, spelled as help spells it; no fake flag in the scope."""
    rows = surface_cells(pages, "flag")
    for name in sorted(flags):
        where = rows.get(name, [])
        if not where:
            problems.append(f"C1: `{name}` is in `fathom --help` and in no doc page's flag table")
            continue
        for page, spelled in where:
            if spelled not in flags[name]:
                wanted = "` or `".join(sorted(flags[name]))
                problems.append(f"C1: {page} documents `{spelled}`; `fathom --help` prints `{wanted}`")
        pages_here = sorted({page for page, _ in where})
        if len(pages_here) > 1:
            problems.append(
                f"C1: `{name}` has a flag-table row in {', '.join(pages_here)}; exactly one page"
            )
    for page, heading in FATHOM_SCOPE.items():
        body = strip_fences(scope(pages[page], heading))
        for match in sorted(set(re.findall(r"(?<![`\w-])(--[a-z][a-z0-9-]*)", body))):
            if match not in flags and match not in EXEMPT_FLAGS:
                problems.append(f"C1: {page} names `{match}`, which no `fathom --help` prints")
    stats["flags"] = len(flags)


def check_keys(pages: dict[str, str], keys: dict[str, set[str]], problems: list[str], stats: dict) -> None:
    """C2: every overlay key has exactly one key-table cell; the overlay itself is not stale."""
    cells = surface_cells(pages, "key")
    for screen, keyset in sorted(keys.items()):
        for key in sorted(keyset):
            if key in EXEMPT_KEYS:
                continue
            found = {page for page, _ in cells.get(key, [])}
            for alias in KEY_ALIASES.get(key, set()):
                found |= {page for page, _ in cells.get(alias, [])}
            if not found:
                problems.append(f"C2: key `{key}` ({screen} help overlay) is in no doc page's key table")
            elif len(found) > 1:
                problems.append(f"C2: key `{key}` has a key-table cell in {', '.join(sorted(found))}")
    overlay = (Path(pages["__repo__"]) / "crates/fathom/src/tui/render.rs").read_text(encoding="utf-8")
    if "not wired yet" in overlay:
        problems.append(
            'C2: the help overlay still says "not wired yet"; every key it lists is wired, and the '
            "overlay is documentation rendered on a screen — say what the key does instead"
        )
    stats["keys"] = sum(len(value) for value in keys.values())


def check_exit_codes(pages: dict[str, str], exit_source: str, problems: list[str], stats: dict) -> None:
    """C3: every code in exit.rs's table is a row in exactly one doc page's exit-code table."""
    codes = set(re.findall(r"^//! \| (\d) \|", exit_source, re.MULTILINE))
    if not codes:
        problems.append("C3: no exit-code table found in crates/fathom/src/exit.rs")
        return
    cells = surface_cells(pages, "code")
    for code in sorted(codes):
        found = sorted({page for page, _ in cells.get(code, [])})
        if not found:
            problems.append(f"C3: exit code {code} is in exit.rs and in no doc page's exit-code table")
        elif len(found) > 1:
            problems.append(f"C3: exit code {code} is documented in {', '.join(found)}; exactly one page")
    stats["exit codes"] = len(codes)


def slug(heading: str) -> str:
    """GitHub's anchor for a heading: lowercase, drop what is not a word, a space or a hyphen, then
    every space becomes a hyphen. Not collapsing is the whole trick: ` — ` is two spaces and one
    removed character, which is the `--` every anchor in this repo already carries."""
    text = heading.strip().lower()
    text = re.sub(r"[^\w \-]", "", text, flags=re.UNICODE)
    return text.replace(" ", "-")


def markdown_files(repo: Path) -> list[Path]:
    for path in sorted(repo.rglob("*.md")):
        if {"target", "node_modules", ".git"} & set(path.parts):
            continue
        yield path


def check_wiring(repo: Path, pages: dict[str, str], problems: list[str], stats: dict) -> None:
    """C4: links, anchors, and the three things the ticket says a reader must be able to find."""
    anchors: dict[str, set[str]] = {}
    for path in markdown_files(repo):
        try:
            text = path.read_text(encoding="utf-8")
        except UnicodeDecodeError:
            continue
        headings = re.findall(r"^(#{1,6})\s+(.*)$", strip_fences(text), re.MULTILINE)
        anchors[str(path.relative_to(repo))] = {slug(title) for _, title in headings}
    for page, text in pages.items():
        base = (repo / page).parent
        for match in re.finditer(r"\]\(([^)\s]+)\)", strip_fences(text)):
            target = match.group(1)
            if target.startswith(("http://", "https://", "mailto:")):
                continue
            file_part, _, anchor = target.partition("#")
            if file_part:
                resolved = (base / file_part).resolve()
                if not resolved.exists():
                    problems.append(f"C4: {page} links to `{target}`, which does not exist")
                    continue
                key = str(resolved.relative_to(repo))
            else:
                key = page
            if anchor and key in anchors and anchor not in anchors[key]:
                problems.append(f"C4: {page} links to `{target}`; no heading `{anchor}` there")
    if "fathom-metrics.md" not in pages["README.md"]:
        problems.append("C4: README.md never links docs/fathom-metrics.md")
    layout = pages[".fathom/LAYOUT.md"]
    for needle in ("fathom index", "fathom doctor"):
        if needle not in layout:
            problems.append(f"C4: .fathom/LAYOUT.md does not name `{needle}`")
    if "everything is empty" not in pages["README.md"].lower():
        problems.append("C4: README has no 'everything is empty' entry for the empty-timeline case")
    stats["pages"] = len(pages)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--repo", type=Path, default=Path(__file__).resolve().parent.parent)
    parser.add_argument("--quiet", action="store_true")
    parser.add_argument("--stats", action="store_true")
    args = parser.parse_args()

    repo = args.repo.resolve()
    problems: list[str] = []
    stats: dict = {}

    pages = read_pages(repo)
    pages["__repo__"] = str(repo)
    help_all = "\n".join(
        help_text(repo, sub) for sub in ([], ["tui"], ["index"], ["report"], ["doctor"])
    )
    check_flags(pages, flags_from_help(help_all), problems, stats)
    check_keys(pages, overlay_keys(repo), problems, stats)
    check_exit_codes(
        pages, (repo / "crates/fathom/src/exit.rs").read_text(encoding="utf-8"), problems, stats
    )
    check_wiring(repo, pages, problems, stats)

    if args.stats:
        for key, value in stats.items():
            print(f"{key}: {value}")
    if problems:
        for problem in problems:
            print(problem)
        if not args.quiet:
            print(f"\n{len(problems)} problem(s) in the Fathom documentation surface.")
        return 1
    if not args.quiet:
        print(
            f"OK  {stats['flags']} flags, {stats['keys']} overlay keys, {stats['exit codes']} exit "
            f"codes across {stats['pages']} pages: each documented exactly once, none invented."
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
