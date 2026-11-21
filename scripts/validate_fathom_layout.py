#!/usr/bin/env python3
"""Check ADR-0006 (the `.fathom` cache root, `fathom.toml` and the metric cache store): the ADR,
`.fathom/LAYOUT.md`, `.gitignore` and the dependency graph say the same thing — read from those
artifacts, not from prose.

This is the dev-time checker for ADR-0006 (ticket mact-by1.2), the sibling of
scripts/validate_tool_contract.py, scripts/validate_evidence.py, scripts/validate_run_lifecycle.py,
scripts/validate_isolation.py and scripts/validate_crate_layout.py. ADR-0006's load-bearing artifacts
are a directory contract and an ignore file, so this is the checker that enforces the ticket's own
acceptance rule: *`.gitignore` and `.fathom/LAYOUT.md` describe the same tree, and no whitelist entry
is orphaned*. It needs no third-party python package (tomllib is stdlib since 3.11) and no `cargo`
invocation; `git ls-files` is used once, for one check, and that check skips itself (loudly) if `git`
is absent.

Why the tree is the thing to check: a cache directory with an undocumented 2.2 MB sqlite file in it is
how ADR-0006 started, and the failure mode of every "layout" document is that the layout drifts from
the file that grants visibility. So the ADR's tree block, the tree block inside `.fathom/LAYOUT.md`,
and the `.fathom` lines of `.gitignore` are compared literally, and a whitelist entry that names no
file in the tree is a build failure rather than another six months of mystery.

Checks performed (all of them, every run):

  C1   the ADR: exists, has front matter (id/title/status/ticket/date), is accepted, carries no
       unresolved marker, states every decision the ticket asked for (root resolution, engine,
       fathom.toml, versioning, failure modes, the disposition of the artifacts already in
       `.fathom/`), marks its rejected options rejected, and cites only check ids this script
       implements.
  C2   `.gitignore`: carries ADR-0006 §12.1's block verbatim and contiguously — with a near-miss
       message naming the line of the block that differs, because "here is the whole block again" is
       not a diagnosis; no other line in the file mentions `.fathom`; no `.fathom` rule is
       *unreachable* (a rule for a path below `.fathom/*` can never fire, since git will not descend
       into a directory it has already ignored — §12 deletes four such rules and says why); every
       whitelist (`!`) entry names an entry of §4.1's tree, and that entry is one git tracks. Git's
       index, not this checkout's disk, is what an ignore rule is about: `.fathom/` is a disposable
       cache that §3.2 invites the user to `rm -rf`, so a tracked-but-absent entry is a note with a
       restore command rather than a verdict; and the block's committed set equals the tree's
       committed set — no orphaned whitelist entry, in either direction.
  C3   `.fathom/LAYOUT.md`: exists, is the only LAYOUT.md in the tree, its tree block equals §4.1's
       tree block (modulo inline comments), and it says "safe to delete" and points at the ADR. When
       git tracks it and the disk does not have it, the failure says so and gives the restore command
       (`fathom`'s boot module `include_str!`s it, so a missing one is not only a contract problem).
  C4   the engine decision holds: neither `rusqlite` nor `libsqlite3-sys` (nor `redb`/`sled`/`diesel`/
       `sea-orm`) appears in Cargo.lock or in any Cargo.toml, and `deny.toml` bans the two that carry
       the C toolchain.
  C5   the ignore rules ADR-0002 §6.4 and ADR-0003 §6 own survived this ADR's `.gitignore` diff:
       `.evidence/` and `.act-mcp.lock` are still ignored under their marker.
  C6   documentation wiring: the ADR is linked from README's ADR list, named in docs/ci.md and
       docs/testing.md, and this script is wired into scripts/ci.sh.
  C7   no mystery, and no obstruction. Every entry physically present in this checkout's `.fathom/`
       is either Fathom-owned per §4.1 or named in the ADR's §2 disposition of foreign artifacts —
       and one that is neither is *named*, not failed, because §4.2 already decided a foreign entry
       stays ("Fathom MUST NOT read it, MUST NOT modify it and MUST NOT delete it") and §10.4 asks
       for "an advisory note rather than a finding", which is what `fathom doctor` prints. A
       gitignored cache root whose contents depend on which other tools a developer has installed is
       the flakiest possible reading of "the tree must match the ADR": same commit, two verdicts.
       What does fail is §4.2's other half, the part that is not advisory — a foreign entry that
       *obstructs* a name Fathom owns: `index` a regular file, `LAYOUT.md` a directory, a store path
       symlinked out of the cache root. Skipped, and says so, when `.fathom/` is absent.
  C8   the store rules are stated as rules: atomic publish (temp + fsync + rename), the `fk1` key
       derivation and its `input_kind` vocabulary, per-analyzer versions, readers-take-no-lock, the
       fail-fast `index_busy`, and a `fix:` line for every refusal.
  C9   `fathom.toml`: §7.2's key table parses, carries the four keys the ticket asked for (noise
       globs, language list, benchmark allow/deny, history depth), every key has a declared consumer
       or is explicitly marked as not an input, and no key is listed twice.
  C10  the checker itself is not a lie: --self-test builds a scratch fixture out of the real
       artifacts, breaks one rule at a time, and asserts the check it names is the one that fires —
       as a finding, or as the advisory note §4.2 asks for, and nothing else fires.

Findings print as `FAIL` and decide the exit status; advisories print as `NOTE` and do not — the
same split ADR-0006 §4.2 gives `fathom doctor`, whose exit code CI gates on.

Usage:
    scripts/validate_fathom_layout.py [--adr PATH] [--root PATH] [--quiet] [--stats] [--self-test]

Exit status: 0 everything consistent, 1 a contract violation (one line each), 2 usage or environment
error (a missing ADR, unreadable TOML, a fixture that cannot be built).
"""
from __future__ import annotations

import argparse
import os
import re
import shutil
import subprocess
import tempfile
import tomllib
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
ADR_NAME = "0006-fathom-workspace-layout.md"
DEFAULT_ADR = REPO / "docs/adr" / ADR_NAME
LAYOUT_RELPATH = Path(".fathom/LAYOUT.md")

#: ADR-0006 §5: the engines this ADR refuses. The first two carry the C toolchain and are additionally
#: banned in deny.toml; the rest are rejected-but-not-banned, so only the lockfile check applies.
BANNED_ENGINES = ("rusqlite", "libsqlite3-sys")
DENIED_ENGINES = ("redb", "sled", "diesel", "sea-orm")

#: ADR-0006 §4.1: what Fathom owns inside the cache root. Anything else is foreign (§4.2). Used
#: where §4.1's tree block cannot be parsed (§4.1's shapes are preferred, see `owned_paths`).
OWNED_ENTRIES = {"LAYOUT.md", "fathom.lock", "index"}

#: A `.fathom` ignore rule naming a path *below* the cache root. git does not descend into a
#: directory it has already ignored, so while `.fathom/*` (or `.fathom/`) is in force such a rule can
#: match nothing — ADR-0006 §12's words are "it is not 'harmless', it is *unreachable* — and an
#: unreachable rule is the one that misleads the next reader".
NESTED_RULE = re.compile(r"^\s*!?\.?/?\.fathom/(?P<rest>[^ ]+)\s*$")

#: The decision list of the ticket, transcribed as tokens so "every bullet answered" is a fact a
#: machine can re-check rather than a claim. Keys are prose labels for the failure message.
DECISION_TOKENS = {
    "root default": "<workspace>/.fathom",
    "root flag": "--cache-dir",
    "root env": "$fathom_dir",
    "precedence": "precedence",
    "safe to delete": "safe to delete",
    "store engine decision": "no sqlite",
    "no c toolchain": "c toolchain",
    "blobs/blocks dir": "blocks/",
    "fathom.toml": "fathom.toml",
    "root-ward discovery": "root-ward",
    "noise globs": "paths.noise",
    "language list": "languages.enabled",
    "benchmark allow list": "benchmarks.allow",
    "benchmark deny list": "benchmarks.deny",
    "history depth": "history.depth",
    "config digest": "config_digest",
    "digest invalidation": "invalidates exactly",
    "meta table": "meta.json",
    "schema_version guard": "schema_version",
    "refuse not migrate": "migrate blind",
    "read-only workspace": "permission denied",
    "no space": "no space left",
    "lock file": "fathom.lock",
    "lock precedent": ".act-mcp.lock",
    "concurrency refusal": "index_busy",
    "which file to delete": "rm -rf",
    "layout deliverable": "layout.md",
    "the foreign store": "index.sqlite3",
    "dead whitelist reviews": "reviews/",
    "dead whitelist ratchet": "ratchet.toml",
}

#: ADR-0006 §6–§10: the mechanisms, as opposed to the decisions, that the later tickets must not
#: improvise. Checked as tokens for the same reason.
MECHANISM_TOKENS = {
    "atomic publish temp": ".tmp",
    "fsync": "fsync",
    "rename": "rename()",
    "immutable rows": "never edited in place",
    "key derivation": "fk1(",
    "input kinds": "input_kind",
    "analyzer versions": "analyzer_version",
    "row version": "row_version",
    "readers unlocked": "readers take no lock",
    "lock never unlinked": "never unlinked",
    "fail fast": "fail-fast",
    "fix line": "fix:",
    "refusal table": "### 10.5",
}

#: The four `fathom.toml` features the ticket named as the minimum.
REQUIRED_CONFIG_KEYS = ("paths.noise", "languages.enabled", "benchmarks.allow",
                        "benchmarks.deny", "history.depth")

SKIPPED: list[str] = []

#: What the checker saw and is not charging the build for (ADR-0006 §4.2's advisory notes). Printed as
#: `NOTE` and never part of the exit status: the point is that nothing in the cache root is a mystery,
#: not that every mystery is somebody's mistake.
NOTES: list[str] = []


class Tree:
    """The artifacts this checker reads, rooted somewhere. `--self-test` points it at a fixture."""

    def __init__(self, root: Path, adr_path: Path) -> None:
        self.root = root
        self.adr_path = adr_path
        self.adr = self._read(adr_path)
        self.layout = self._read(root / LAYOUT_RELPATH, required=False)
        self.gitignore = self._read(root / ".gitignore", required=False)
        self.lock = self._read(root / "Cargo.lock", required=False)
        self.denies = self._read_toml(root / "deny.toml")
        self.cargo_tomls = sorted(
            p for p in self.root.glob("**/Cargo.toml")
            if "target" not in p.parts and ".fathom" not in p.parts
        )

    def _read(self, path: Path, required: bool = True) -> str:
        if not path.is_file():
            if required:
                raise SystemExit(f"no {path} — run this from the act-mcp checkout, or pass --root")
            SKIPPED.append(f"{path.relative_to(self.root)} is absent")
            return ""
        return path.read_text(encoding="utf-8")

    def _read_toml(self, path: Path) -> dict:
        if not path.is_file():
            SKIPPED.append(f"{path.relative_to(self.root)} is absent")
            return {}
        return tomllib.loads(path.read_text(encoding="utf-8"))

    def rel(self, path: Path) -> str:
        try:
            return str(path.relative_to(self.root))
        except ValueError:
            return str(path)


def fenced_blocks(text: str) -> list[str]:
    """Every ``` fenced block, in order, without the fences."""
    blocks, current, inside = [], [], False
    for line in text.splitlines():
        if line.startswith("```"):
            if inside:
                blocks.append("\n".join(current))
                current = []
            inside = not inside
            continue
        if inside:
            current.append(line)
    return blocks


def section(adr: str, marker: str) -> str:
    """The ADR text following a heading marker, up to the next heading of the same or higher rank."""
    tail = adr.split(marker, 1)[1]
    match = re.search(r"^#{2,4} ", tail, re.M)
    return tail[: match.start()] if match else tail


def tree_of(block: str) -> list[str]:
    """Normalise a tree block for comparison: no inline `#` comments, no trailing blanks."""
    lines = []
    for line in block.splitlines():
        line = re.split(r"\s+#", line, maxsplit=1)[0].rstrip()
        if line.strip():
            lines.append(line.rstrip())
    return lines


def adr_tree_block(adr: str) -> list[str]:
    """§4.1's tree, which is the one contract both documents must copy."""
    blocks = fenced_blocks(section(adr, "### 4.1 The layout"))
    if not blocks:
        raise SystemExit("ADR-0006 §4.1 has no fenced tree block; nothing to compare against")
    return tree_of(blocks[0])


def adr_gitignore_block(adr: str) -> list[str]:
    """§12.1's pinned `.gitignore` block, verbatim."""
    blocks = fenced_blocks(section(adr, "### 12.1 This repository"))
    if not blocks:
        raise SystemExit("ADR-0006 §12.1 has no fenced .gitignore block; nothing to compare against")
    return blocks[0].splitlines()


def norm_ignore(line: str) -> str:
    """A `.gitignore` line as matched: a BOM and trailing whitespace are noise, nothing else is."""
    return line.lstrip("\ufeff").rstrip()


def find_block(lines: list[str], block: list[str]) -> int | None:
    """Where `block` starts inside `lines`: exact, then tolerant of line-ending and trailing-space noise.

    Requiring a byte-exact copy is ADR-0006 §12's rule, but an editor that strips one trailing space
    is not a contract change, and failing on it trains people to ignore this checker.
    """
    for haystack, needle in (
        (lines, block),
        ([norm_ignore(x) for x in lines], [norm_ignore(x) for x in block]),
    ):
        for i in range(len(haystack) - len(needle) + 1):
            if haystack[i : i + len(needle)] == needle:
                return i
    return None


def block_near_miss(lines: list[str], block: list[str]) -> str:
    """One clause saying *how* the block is missing; an absent block and an edited one need different fixes."""
    clean = [norm_ignore(x) for x in lines]
    want = [norm_ignore(x) for x in block]
    if not [x for x in want if x not in clean]:
        return ("every line of it is present, but not in one contiguous run in the order §12.1 "
                "writes them")
    best, best_score = None, -1
    for i in range(len(clean) - len(want) + 1):
        score = sum(1 for a, b in zip(clean[i : i + len(want)], want) if a == b)
        if score > best_score:
            best, best_score = i, score
    if best is None or best_score < 1:
        return f"none of its {len(want)} lines appears in the file at all"
    gap = next((k for k in range(len(want)) if clean[best + k] != want[k]), 0)
    return (f"the closest run starts at line {best + 1} and matches {best_score} of {len(want)} "
            f"lines; line {best + gap + 1} reads {clean[best + gap]!r} where §12.1 has {want[gap]!r}")


def unreachable_rule(line: str, whitelist: set[str]) -> bool:
    """True when `.fathom/*` already shadows this rule, so git can never reach it.

    A rule below the cache root can only matter under a directory some `!` rule un-ignores — which is
    why §12.1 deletes `.fathom/**/*.tmp` (nothing below the root is visible, so the temporaries of a
    whitelist that does not exist cannot leak) and keeps `!.fathom/LAYOUT.md`.
    """
    if line.lstrip().startswith(("!", "#")):
        return False
    match = NESTED_RULE.match(line)
    if not match:
        return False
    rest = match.group("rest")
    if "/" not in rest and not rest.startswith("**/"):
        return False  # `.fathom/x` — the directory-level rule does not shadow a one-level name
    return rest.split("/")[0] not in whitelist


def owned_paths(adr: str) -> dict[str, str]:
    """Every path §4.1's tree names, mapped to the shape it must have: "file" or "directory".

    Derived from the ADR's own tree block rather than hardcoded here, so amending §4.1 amends the
    ownership rule by itself — and the obstruction half of C7 stays honest about which names Fathom
    actually needs.
    """
    out: dict[str, str] = {}
    stack: list[str] = []
    for line in adr_tree_block(adr):
        match = re.match(r"^([\s│]*)([├└])── (.+)$", line)
        if not match:
            continue
        depth = len(match.group(1).replace("│", " ")) // 4 + 1
        rest = match.group(3).strip()
        name = re.split(r"[/\s]", rest, maxsplit=1)[0]
        stack = stack[: max(depth - 1, 0)]
        stack.append(name)
        out["/".join(stack)] = "directory" if rest.startswith(name + "/") else "file"
    return out or {name: ("directory" if name == "index" else "file") for name in OWNED_ENTRIES}


def inside(path: Path, root: Path) -> bool:
    """Whether `path` resolves to somewhere under `root` (a store that escapes its root is not
    disposable any more — ADR-0006 §4.2 refuses symlinks for exactly that reason)."""
    try:
        Path(path).resolve().relative_to(root.resolve())
        return True
    except (OSError, ValueError):
        return False


def committed_from_tree(tree: list[str]) -> set[str]:
    """The top-level names §4.1's tree commits — i.e. the ones at the cache root, not under index/."""
    names = set()
    for line in tree:
        if re.match(r"^[├└]── ", line):
            name = re.split(r"[/\s]", line[4:].strip(), maxsplit=1)[0]
            names.add(name)
    return names


def backticked(adr: str) -> set[str]:
    """Everything the ADR names in backticks that looks like a single path component: the foreign
    artifact naming test.

    A name qualifies when it is a plain component — `index.sqlite3`, `objects/`, `ratchet.toml` — so
    globs (`index.sqlite3-*`), placeholders (`rows/<xx>`) and empty spans cannot make a foreign entry
    look accounted for. A trailing slash is stripped, so `objects/` names the entry `objects`, and a
    named file covers its sidecars (`index.sqlite3` covers `index.sqlite3-wal`) because §2.2 disposes
    of the store, not of one of its files.
    """
    names = set()
    for match in re.findall(r"`([^`\n]+)`", adr):
        raw = match.strip()
        name = raw.rstrip("/")
        if not name or len(name) < 3 or any(bad in name for bad in ("<", "*", " ", "|", "/")):
            continue  # a glob, a path with a placeholder, or an empty span: not a name to match on
        if "." in name or raw.endswith("/"):
            names.add(name)
    return names


# ---------------------------------------------------------------------------------------------
# the checks
# ---------------------------------------------------------------------------------------------

def check_adr(adr: str, problems: list[str], stats: dict[str, int]) -> None:
    """C1: the document itself, and the ticket's decision list."""
    if "status: accepted" not in adr:
        problems.append("C1: ADR-0006 is not marked accepted; a decided decision is a decided one")
    for field in ("id: ADR-0006", "title:", "ticket: mact-by1.2", "date:"):
        if field not in adr:
            problems.append(f"C1: ADR-0006 front matter is missing {field!r}")
    for marker in ("TODO", "TBD", "FIXME", "to be decided"):
        if marker.lower() in adr.lower():
            problems.append(f"C1: ADR-0006 carries an unresolved marker: {marker!r}")
    lowered = adr.lower()
    missing = [label for label, token in DECISION_TOKENS.items() if token.lower() not in lowered]
    if missing:
        problems.append("C1: ADR-0006 never states: " + ", ".join(missing))
    rejected = len(re.findall(r"\*\*Rejected\*\*", adr))
    if rejected < 5:
        problems.append(
            f"C1: ADR-0006 marks {rejected} alternatives rejected; the ticket asks for the options "
            "above to be recorded and the rejected ones marked rejected with a reason"
        )
    for claim in sorted({int(n) for n in re.findall(r"(?<![A-Za-z])C(\d{1,2})\b", adr)}):
        if claim not in CHECK_IDS:
            problems.append(f"C1: ADR-0006 cites check C{claim}, which this script does not implement")
    stats["decision tokens checked"] = len(DECISION_TOKENS)


def check_gitignore(tree: Tree, gitignore: list[str], problems: list[str]) -> None:
    """C2: `.gitignore` carries §12.1's block and nothing else about `.fathom`.

    Every sub-check runs even after one has failed: a drifted ignore file usually has more than one
    problem, and a checker that stops at the first turns one edit into three build runs.
    """
    block = adr_gitignore_block(tree.adr)
    start = find_block(gitignore, block)
    # The lines the block accounts for. When the block is present this is its span; when it is not,
    # every line of it still gets the benefit of the doubt, so a mangled block is reported once — as
    # "the block does not match", with the line that differs — and not also as a dozen stray mentions.
    if start is None:
        wanted = {norm_ignore(line) for line in block}
        span = {i for i, line in enumerate(gitignore) if norm_ignore(line) in wanted}
    else:
        span = set(range(start, start + len(block)))
    # The whitelist, read from `.gitignore` rather than from the block, so an entry smuggled in
    # *outside* the block is judged on its merits too, not only reported as a stray mention.
    whitelist = {
        re.sub(r"^!\s*/?\.fathom/", "", line.strip()).strip("/")
        for line in gitignore
        if line.strip().startswith("!") and ".fathom" in line
    }

    if start is None:
        problems.append(
            "C2: .gitignore does not contain ADR-0006 §12.1's block verbatim — "
            + block_near_miss(gitignore, block)
            + ". The block is:\n    "
            + "\n    ".join(block)
        )

    for i, line in enumerate(gitignore):
        if ".fathom" not in line or i in span:
            continue
        if unreachable_rule(line, whitelist):
            problems.append(
                f"C2: .gitignore line {i + 1} can never fire: {line!r} names a path *below* "
                "`.fathom/*`, and git does not descend into a directory it has already ignored, so "
                "there is nothing under it for this rule to act on. ADR-0006 §12: 'an unreachable "
                "rule is the one that misleads the next reader' — delete it, or un-ignore its parent "
                "here and add that path to §4.1 in the same change"
            )
            continue
        problems.append(
            f"C2: .gitignore line {i + 1} mentions `.fathom` outside ADR-0006's block: {line!r}; "
            "a whitelist entry must be added to §12 and §4.1 in the same change, or deleted"
        )

    # the ticket's acceptance rule, both directions
    committed = committed_from_tree(adr_tree_block(tree.adr))
    orphans = sorted(whitelist - committed)
    if orphans:
        problems.append(
            f"C2: .gitignore whitelists {orphans} under `.fathom/`, which ADR-0006 §4.1's tree does not "
            "contain; a whitelist entry that names nothing in the layout is dead and must go"
        )
    # An ignore rule governs what git may *commit*, so git's index is the oracle for whether a
    # whitelist entry is live — not this checkout's disk, which §3.2 invites anyone to `rm -rf`.
    for entry in sorted(whitelist):
        tracked = git_tracked(tree.root, f".fathom/{entry}")
        on_disk = (tree.root / ".fathom" / entry).exists()
        if tracked is False:
            problems.append(
                f"C2: .gitignore un-ignores .fathom/{entry} and git tracks nothing under it: the rule "
                "grants visibility to a path nothing is committed at. Either commit the file or drop "
                "the rule (ADR-0006 §12)"
            )
        elif tracked is None and not on_disk:
            # Nothing else to ask: with no git to consult, the file itself is the only witness left.
            problems.append(
                f"C2: .gitignore un-ignores .fathom/{entry}, which does not exist on disk; either "
                "commit it or drop the rule (ADR-0006 §12)"
            )
        elif tracked and not on_disk:
            NOTES.append(
                f"C2: git tracks .fathom/{entry} and this checkout does not have it — the cache root "
                f"is disposable (§3.2), so `git checkout -- .fathom/{entry}` puts the on-disk "
                "contract back; the rule itself is live and C3 is the check that says whether the "
                "missing file matters"
            )

    # an owned-but-never-committed top-level name is fine (index/, fathom.lock); an entry in the tree
    # that the ignore block whitelists nothing for and that git can see is not.
    visible = committed - whitelist - {"index"}
    for name in sorted(visible):
        if name not in {"fathom.lock"} and (tree.root / ".fathom" / name).exists():
            problems.append(
                f"C2: .fathom/{name} is in ADR-0006 §4.1's tree but is neither committed nor ignored "
                "by §12.1's block"
            )


def check_layout(tree: Tree, problems: list[str], stats: dict[str, int]) -> None:
    """C3: `.fathom/LAYOUT.md` describes the same tree as the ADR."""
    if not tree.layout:
        # Two different states look the same from here: never committed, and committed-but-deleted
        # (which is what `rm -rf .fathom` — the promise §3.2 makes — leaves behind). git tells them
        # apart, and only one of them has a one-command fix.
        if git_tracked(tree.root, str(LAYOUT_RELPATH)):
            problems.append(
                f"C3: git tracks {LAYOUT_RELPATH} and this checkout does not have it; the cache root is "
                f"disposable (§3.2) — `git checkout -- {LAYOUT_RELPATH}` restores it. ADR-0006 §3.2 "
                "makes it the on-disk contract, and `fathom`'s boot module include_str!s it, so the "
                "crate does not build without it either"
            )
        else:
            problems.append("C3: .fathom/LAYOUT.md is missing; ADR-0006 §3.2 makes it the on-disk contract")
        return
    expected = adr_tree_block(tree.adr)
    blocks = [b for b in fenced_blocks(tree.layout) if "├──" in b or "└──" in b]
    if not blocks:
        problems.append("C3: .fathom/LAYOUT.md contains no tree block to compare with ADR-0006 §4.1")
    else:
        got = tree_of(blocks[0])
        if got != expected:
            diff = [
                f"      adr: {a!r}  layout: {b!r}"
                for a, b in zip(expected + [""] * 9, got + [""] * 9)
                if a != b
            ]
            problems.append(
                "C3: .fathom/LAYOUT.md's tree does not equal ADR-0006 §4.1's tree:\n" + "\n".join(diff)
            )
    if "safe to delete" not in tree.layout.lower():
        problems.append("C3: .fathom/LAYOUT.md does not say the cache root is safe to delete (§3.2)")
    if ADR_NAME not in tree.layout:
        problems.append(f"C3: .fathom/LAYOUT.md does not link {ADR_NAME}")
    copies = [
        p for p in tree.root.rglob("LAYOUT.md")
        if "target" not in p.parts and ".fathom/index" not in p.parts
    ]
    stats["copies of LAYOUT.md"] = len(copies)
    if len(copies) != 1:
        problems.append(
            "C3: found %d copies of LAYOUT.md (%s); §3.2 pins one, the one inside the cache root"
            % (len(copies), ", ".join(tree.rel(p) for p in copies))
        )


def check_engine(tree: Tree, problems: list[str]) -> None:
    """C4: no sqlite, no embedded store — the decision, as a dependency fact."""
    packages = set(re.findall(r'^name = "([^"]+)"', tree.lock, re.M))
    found = sorted((packages & set(BANNED_ENGINES)) | (packages & set(DENIED_ENGINES)))
    if found:
        problems.append(
            f"C4: Cargo.lock resolves {found}; ADR-0006 §5 pins the store to plain files — no sqlite "
            "and no embedded engine, so neither the C toolchain nor a second recovery path appears"
        )
    declared = []
    for manifest in tree.cargo_tomls:
        text = manifest.read_text(encoding="utf-8")
        for crate in (*BANNED_ENGINES, *DENIED_ENGINES):
            if re.search(rf'^{re.escape(crate)}\s*=', text, re.M):
                declared.append(f"{tree.rel(manifest)}:{crate}")
    if declared:
        problems.append(f"C4: a Cargo.toml declares a store engine ADR-0006 §5 refuses: {declared}")
    bans = {entry.get("name") for entry in tree.denies.get("bans", {}).get("deny", [])}
    for crate in BANNED_ENGINES:
        if crate not in bans:
            problems.append(
                f"C4: deny.toml does not ban {crate!r}; ADR-0006 §5 makes the no-C-toolchain decision "
                "a supply-chain rule so it cannot be reversed by a quiet Cargo.toml edit"
            )


def check_evidence_rules(tree: Tree, gitignore: list[str], problems: list[str]) -> None:
    """C5: ADR-0002 §6.4's and ADR-0003 §6's ignore rules survived this ADR's diff."""
    for rule in (".evidence/", ".act-mcp.lock"):
        if rule not in gitignore:
            problems.append(
                f"C5: .gitignore no longer ignores {rule}; that is ADR-0002 §6.4 / ADR-0003 §6, not "
                "this ADR's to remove"
            )
    if "act-mcp evidence" not in tree.gitignore:
        problems.append("C5: .gitignore's act-mcp evidence marker comment is gone; ADR-0002 §6.4")


def check_doc_wiring(tree: Tree, problems: list[str]) -> None:
    """C6: the documentation this ADR's deliverables promise."""
    for rel, needle, why in (
        ("README.md", ADR_NAME, "README's ADR list must link the ADR"),
        ("docs/ci.md", "validate_fathom_layout.py", "docs/ci.md must name the checker CI runs"),
        ("docs/testing.md", "validate_fathom_layout.py", "docs/testing.md must name the checker CI runs"),
        ("scripts/ci.sh", "validate_fathom_layout.py", "scripts/ci.sh must run this checker"),
    ):
        path = tree.root / rel
        if not path.is_file():
            problems.append(f"C6: {rel} is missing ({why})")
        elif needle not in path.read_text(encoding="utf-8"):
            problems.append(f"C6: {rel} does not mention {needle!r} — {why}")


def check_no_mystery(tree: Tree, problems: list[str], stats: dict[str, int]) -> None:
    """C7: nothing in `.fathom/` is a mystery, and nothing Fathom owns is obstructed.

    Two halves, and only the second one is a verdict. §4.2 decides that a foreign entry *stays* —
    Fathom MUST NOT read, modify or delete it — and §10.4 asks for "an advisory note rather than a
    finding", which is what `fathom doctor` prints on an `ok` `cache` check. A hard failure there
    would also be an unfalsifiable one: `.fathom/` is gitignored, so its contents depend on which
    other tool a developer has run in this checkout, and the same commit would be red on one machine
    and green on another (and green in CI, where the directory holds one tracked file). Naming it is
    the deliverable; failing on it is the flake.

    What fails is §4.2's other half, the part that is not advisory: a foreign entry that *obstructs*
    a name Fathom owns — `index` a regular file, `LAYOUT.md` a directory, a store path symlinked out
    of the cache root. That is the case where "just delete the cache" stops being safe, because the
    bytes behind the name are somebody else's and the name is where Fathom's go.
    """
    root = tree.root / ".fathom"
    entries = sorted(p.name for p in root.iterdir()) if root.is_dir() else []
    if not entries:
        SKIPPED.append("C7: no .fathom/ directory in this checkout (it is gitignored), nothing to reconcile")
        return
    named = backticked(tree.adr)
    stats["entries under .fathom/"] = len(entries)

    # 1. the names §4.1 gives Fathom must have the shape §4.1 gives them, and must not escape the root
    owned = owned_paths(tree.adr)
    stats["paths Fathom owns"] = len(owned)
    for rel, want in sorted(owned.items()):
        path = root / rel
        if not path.is_symlink() and not path.exists():
            continue  # absent is fine: a cold cache has no index/ until the first run writes it
        if path.is_symlink() and not inside(path, root):
            problems.append(
                f"C7: .fathom/{rel} is a symlink to {path.resolve()}, outside the cache root; §4.2 "
                "refuses a path it writes that escapes the root, because a cache whose bytes live "
                "somewhere else is not a cache you are allowed to delete"
            )
            continue
        got = "directory" if path.is_dir() else "file"
        if got != want:
            problems.append(
                f"C7: .fathom/{rel} is a {got}, where ADR-0006 §4.1's tree has a {want}; a name Fathom "
                "owns and cannot use is a boot refusal (§10.4), not something to route around — "
                f"delete .fathom/{rel} if you are sure it is dead, or point --cache-dir / $FATHOM_DIR "
                "somewhere else (§2.2)"
            )

    # 2. every other name at the cache root is named, not failed (§4.2, §10.4)
    for entry in entries:
        if entry in OWNED_ENTRIES:
            continue
        if any(entry == name or entry.startswith(name) for name in named):
            continue
        NOTES.append(
            f"C7: .fathom/{entry} was not written by Fathom and is not one of the foreign artifacts "
            "ADR-0006 §2 disposes of. Per §4.2 Fathom neither reads it nor deletes it, and "
            "`fathom doctor` lists it as an advisory note on an ok `cache` check — this is that note. "
            "The layout is unchanged and nobody owes a fix; a PR that adds a committed file here "
            "instead adds it to §4.1 and §12.1 in the same change"
        )


def check_mechanisms(adr: str, problems: list[str], stats: dict[str, int]) -> None:
    """C8: the store rules are written down as rules."""
    lowered = adr.lower()
    missing = [label for label, token in MECHANISM_TOKENS.items() if token.lower() not in lowered]
    if missing:
        problems.append("C8: ADR-0006 does not pin: " + ", ".join(missing))
    stats["mechanism tokens checked"] = len(MECHANISM_TOKENS)


def check_config_table(adr: str, problems: list[str], stats: dict[str, int]) -> None:
    """C9: §7.2's `fathom.toml` table is complete and every key is claimed."""
    rows = [
        line for line in section(adr, "### 7.2 The keys").splitlines()
        if line.startswith("| `")
    ]
    keys = [line.split("`")[1] for line in rows]
    if not keys:
        problems.append("C9: ADR-0006 §7.2 has no `fathom.toml` key table to check")
        return
    stats["fathom.toml keys"] = len(keys)
    duplicates = sorted({k for k in keys if keys.count(k) > 1})
    if duplicates:
        problems.append(f"C9: §7.2 lists {duplicates} twice")
    for key in REQUIRED_CONFIG_KEYS:
        if key not in keys:
            problems.append(
                f"C9: §7.2 has no `{key}` key; the ticket names it as a minimum (`fathom.toml` at "
                "minimum carries the noise globs, the language list, the benchmark allow/deny list "
                "and history depth)"
            )
    for line in rows:
        cells = [c.strip() for c in line.strip().strip("|").split("|")]
        if len(cells) < 5:
            problems.append(f"C9: §7.2's row for {cells[0] if cells else '?'} is not a 4-column row")
        elif not cells[3] or cells[3] in {"—", "-"}:
            problems.append(
                f"C9: §7.2 gives `{cells[0]}` no declared consumer; every key is either read by a "
                "named analyzer (ADR-0006 §7.3) or explicitly not an input to anything cached"
            )


CHECK_IDS = set(range(1, 11))


def git_tracked(root: Path, path: str) -> bool | None:
    """`git ls-files` answer for one path, or None when git cannot be consulted *about this tree*.

    "Cannot consult" includes the case that matters for --self-test: a scratch fixture sitting inside
    somebody's repository would otherwise be asked about the wrong repo, and the drill's verdict would
    depend on where /tmp happens to be. Answer only for the tree that is its own top level.
    """
    try:
        top = subprocess.run(
            ["git", "rev-parse", "--show-toplevel"], cwd=root, capture_output=True, text=True
        )
        out = subprocess.run(
            ["git", "ls-files", "--", path], cwd=root, capture_output=True, text=True
        )
    except OSError:
        return None
    if top.returncode != 0 or out.returncode != 0:
        return None
    if Path(top.stdout.strip()).resolve() != Path(root).resolve():
        return None
    return bool(out.stdout.strip())


def run_checks(tree: Tree, stats: dict[str, int]) -> list[str]:
    """Every check, every run. Returns the problems; appends to SKIPPED for what it could not judge."""
    problems: list[str] = []
    gitignore_lines = tree.gitignore.splitlines()
    check_adr(tree.adr, problems, stats)
    check_gitignore(tree, gitignore_lines, problems)
    check_layout(tree, problems, stats)
    check_engine(tree, problems)
    check_evidence_rules(tree, gitignore_lines, problems)
    check_doc_wiring(tree, problems)
    check_no_mystery(tree, problems, stats)
    check_mechanisms(tree.adr, problems, stats)
    check_config_table(tree.adr, problems, stats)

    tracked = git_tracked(tree.root, str(LAYOUT_RELPATH))
    if tracked is None:
        SKIPPED.append("C2/LAYOUT tracked-by-git check: no git, skipped")
    elif not tracked:
        problems.append(
            "C2: .fathom/LAYOUT.md is not tracked by git, so the ignore rule that un-ignores it is "
            "pointless: ADR-0006 §12 commits exactly this one file (git add .fathom/LAYOUT.md)"
        )
    return problems


# ---------------------------------------------------------------------------------------------
# C10: prove the checks fire
# ---------------------------------------------------------------------------------------------

def build_fixture(dest: Path, source_adr: Path) -> Path:
    """A scratch repository whose artifacts are the real ones, so a mutation isolates one rule."""
    dest.mkdir(parents=True, exist_ok=True)
    (dest / "docs" / "adr").mkdir(parents=True, exist_ok=True)
    (dest / ".fathom").mkdir(parents=True, exist_ok=True)
    (dest / "scripts").mkdir(parents=True, exist_ok=True)
    shutil.copy2(source_adr, dest / "docs" / "adr" / ADR_NAME)
    shutil.copy2(REPO / LAYOUT_RELPATH, dest / LAYOUT_RELPATH)
    shutil.copy2(REPO / ".gitignore", dest / ".gitignore")
    shutil.copy2(REPO / "deny.toml", dest / "deny.toml")
    shutil.copy2(REPO / "Cargo.lock", dest / "Cargo.lock")
    (dest / "Cargo.toml").write_text('[workspace]\nresolver = "2"\nmembers = []\n', encoding="utf-8")
    (dest / "README.md").write_text(f"* [docs/adr/{ADR_NAME}](docs/adr/{ADR_NAME})\n", encoding="utf-8")
    (dest / "docs" / "ci.md").write_text("scripts/validate_fathom_layout.py\n", encoding="utf-8")
    (dest / "docs" / "testing.md").write_text("validate_fathom_layout.py\n", encoding="utf-8")
    (dest / "scripts" / "ci.sh").write_text("#!/usr/bin/env bash\npython3 scripts/validate_fathom_layout.py\n", encoding="utf-8")
    return dest


def mutate(kind: str, fixture: Path) -> None:
    """Break exactly one rule of the fixture."""
    def rewrite(rel: str, fn) -> None:
        path = fixture / rel
        path.write_text(fn(path.read_text(encoding="utf-8")), encoding="utf-8")

    if kind == "drop-whitelist":
        rewrite(".gitignore", lambda t: t.replace("!.fathom/LAYOUT.md\n", ""))
    elif kind == "orphan-whitelist":
        rewrite(".gitignore", lambda t: t.replace(".fathom/*\n", ".fathom/*\n!.fathom/reviews/\n"))
    elif kind == "unreachable-rule":
        # The exact regression this checker was written against, and the shape it came in: a
        # `.fathom` rule appended for local tidiness, four lines below the ADR's block, that git can
        # never reach because `.fathom/*` already ignores the directory it lives in.
        rewrite(".gitignore", lambda t: t + "\n.fathom/**/*.tmp\n")
    elif kind == "layout-drift":
        rewrite(str(LAYOUT_RELPATH), lambda t: t.replace("meta.json", "META.json"))
    elif kind == "sqlite":
        rewrite("Cargo.lock", lambda t: t + '\n[[package]]\nname = "rusqlite"\nversion = "0.31.0"\n')
    elif kind == "unbanned":
        rewrite("deny.toml", lambda t: t.replace('name = "rusqlite"', 'name = "not-rusqlite"'))
    elif kind == "evidence-rule":
        rewrite(".gitignore", lambda t: t.replace(".evidence/\n", ""))
    elif kind == "doc-wiring":
        (fixture / "scripts" / "ci.sh").write_text("#!/usr/bin/env bash\necho hi\n", encoding="utf-8")
    elif kind == "mystery":
        (fixture / ".fathom" / "mystery_store.bin").write_bytes(b"\x00" * 16)
    elif kind == "obstruction":
        # §4.2's non-advisory half: a foreign entry sitting on a name Fathom owns. Same entry as
        # `mystery`, different verdict, because now "delete the cache" is somebody else's data.
        (fixture / ".fathom" / "index").write_text("not a store, a file\n", encoding="utf-8")
    elif kind == "symlink-escape":
        elsewhere = fixture.parent / "elsewhere"
        elsewhere.mkdir(exist_ok=True)
        os.symlink(elsewhere, fixture / ".fathom" / "index")
    elif kind == "undecided":
        rewrite("docs/adr/" + ADR_NAME, lambda t: t.replace("- **Status:** Accepted", "- **Status:** Accepted\n\nTODO: pick a root"))
    elif kind == "no-mechanism":
        rewrite("docs/adr/" + ADR_NAME, lambda t: t.replace("rename()", "renamed"))
    elif kind == "key-table":
        rewrite(
            "docs/adr/" + ADR_NAME,
            lambda t: "\n".join(l for l in t.splitlines() if not l.startswith("| `benchmarks.deny`")) + "\n",
        )
    else:  # pragma: no cover - a coding mistake in the table below
        raise SystemExit(f"unknown mutation {kind!r}")


#: (mutation, the check ids that must fire as findings, the check ids that must fire as advisories —
#:  and only those). The advisory column is what keeps ADR-0006 §4.2 honest from both sides: a foreign
#:  cache entry is named, and a foreign cache entry that obstructs a Fathom name is a finding.
MUTATIONS = [
    ("drop-whitelist", {"C2"}, set()),
    ("orphan-whitelist", {"C2"}, set()),
    ("unreachable-rule", {"C2"}, set()),
    ("layout-drift", {"C3"}, set()),
    ("sqlite", {"C4"}, set()),
    ("unbanned", {"C4"}, set()),
    ("evidence-rule", {"C5"}, set()),
    ("doc-wiring", {"C6"}, set()),
    ("mystery", set(), {"C7"}),
    ("obstruction", {"C7"}, set()),
    ("symlink-escape", {"C7"}, set()),
    ("undecided", {"C1"}, set()),
    ("no-mechanism", {"C8"}, set()),
    ("key-table", {"C9"}, set()),
]


def self_test(adr_path: Path) -> int:
    """C10: build a clean fixture, break one rule at a time, and name what catches it.

    A finding and an advisory both have to be proved, or §4.2's careful split between "somebody
    else's file" and "somebody else's file on a name of ours" is a comment rather than a contract.
    """
    problems = []
    with tempfile.TemporaryDirectory(prefix="fathom-layout-drill-") as tmp:
        base = Path(tmp) / "base"
        build_fixture(base, adr_path)
        for kind, expected, expected_notes in MUTATIONS:
            scratch = Path(tmp) / f"case-{kind}"
            shutil.copytree(base, scratch)
            mutate(kind, scratch)
            SKIPPED.clear()
            NOTES.clear()
            found = run_checks(Tree(scratch, scratch / "docs" / "adr" / ADR_NAME), {})
            fired = {p.split(":")[0] for p in found}
            noted = {n.split(":")[0] for n in NOTES}
            if fired != expected or noted != expected_notes:
                problems.append(
                    f"C10: mutating {kind!r} fired {sorted(fired) or 'nothing'} "
                    f"(noted {sorted(noted) or 'nothing'}); this ADR's table says "
                    f"{sorted(expected)} (noted {sorted(expected_notes)}). First lines: "
                    + " | ".join(p.splitlines()[0] for p in (found + list(NOTES))[:3])
                )
            print(
                f"  drill {kind:<18} fired {', '.join(sorted(fired)) or 'nothing':<8} "
                f"noted {', '.join(sorted(noted)) or 'nothing'}"
            )

        SKIPPED.clear()
        NOTES.clear()
        clean = run_checks(Tree(base, base / "docs" / "adr" / ADR_NAME), {})
        # A scratch tree under /tmp is not a git top level, so the tracked-by-git sub-checks decline
        # to answer (see git_tracked). If the drill ever runs inside a repository, that one sub-check
        # is still expected noise rather than a finding.
        clean = [p for p in clean if not p.startswith("C2: .fathom/LAYOUT.md is not tracked")]
        if clean or NOTES:
            problems.append(
                "C10: the untouched fixture did not pass:\n    "
                + "\n    ".join(p.splitlines()[0] for p in clean)
                + ("\n    noted: " + " | ".join(n.splitlines()[0] for n in NOTES) if NOTES else "")
            )
    for problem in problems:
        print(f"FAIL {problem}")
    if problems:
        print(f"{len(problems)} self-test failure(s)")
        return 1
    print(f"ok: {len(MUTATIONS)} mutations fired exactly the check each one names, and the clean fixture passed")
    return 0


# ---------------------------------------------------------------------------------------------
# entry point
# ---------------------------------------------------------------------------------------------

def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=(__doc__ or "").splitlines()[0])
    parser.add_argument("--adr", type=Path, default=DEFAULT_ADR)
    parser.add_argument("--root", type=Path, default=REPO)
    parser.add_argument("--quiet", action="store_true")
    parser.add_argument("--stats", action="store_true")
    parser.add_argument("--self-test", action="store_true", dest="self_test")
    args = parser.parse_args(argv)

    if args.self_test:
        return self_test(args.adr if args.adr != DEFAULT_ADR else DEFAULT_ADR)

    if not args.adr.is_file():
        raise SystemExit(f"no ADR at {args.adr}")

    stats: dict[str, int] = {}
    tree = Tree(args.root.resolve(), args.adr)
    problems = run_checks(tree, stats)

    for problem in problems:
        print(f"FAIL {problem}")
    if args.stats:
        for key, value in sorted(stats.items()):
            print(f"  {key}: {value}")
    for note in dict.fromkeys(NOTES):
        print(f"NOTE {note}")
    for note in dict.fromkeys(SKIPPED):
        print(f"WARN {note}")
    if not problems:
        if not args.quiet:
            print(f"ok: ADR-0006 is accepted and .gitignore, .fathom/LAYOUT.md and the graph agree with it")
        return 0
    print(f"{len(problems)} workspace-layout problem(s)")
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
