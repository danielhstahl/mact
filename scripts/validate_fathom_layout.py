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
  C2   `.gitignore`: carries ADR-0006 §12.1's block verbatim and contiguously; no other line in the
       file mentions `.fathom`; every whitelist (`!`) entry in the block names a file in §4.1's tree
       and exists on disk; the block's committed set equals the tree's committed set — no orphaned
       whitelist entry, in either direction.
  C3   `.fathom/LAYOUT.md`: exists, is the only LAYOUT.md in the tree, its tree block equals §4.1's
       tree block (modulo inline comments), and it says "safe to delete" and points at the ADR.
  C4   the engine decision holds: neither `rusqlite` nor `libsqlite3-sys` (nor `redb`/`sled`/`diesel`/
       `sea-orm`) appears in Cargo.lock or in any Cargo.toml, and `deny.toml` bans the two that carry
       the C toolchain.
  C5   the ignore rules ADR-0002 §6.4 and ADR-0003 §6 own survived this ADR's `.gitignore` diff:
       `.evidence/` and `.act-mcp.lock` are still ignored under their marker.
  C6   documentation wiring: the ADR is linked from README's ADR list, named in docs/ci.md and
       docs/testing.md, and this script is wired into scripts/ci.sh.
  C7   no mystery: every entry physically present in this checkout's `.fathom/` is either Fathom-owned
       per §4.1 or named in the ADR's §2 disposition of foreign artifacts (skipped, and says so, when
       `.fathom/` is absent — which it is on a fresh clone, because it is gitignored).
  C8   the store rules are stated as rules: atomic publish (temp + fsync + rename), the `fk1` key
       derivation and its `input_kind` vocabulary, per-analyzer versions, readers-take-no-lock, the
       fail-fast `index_busy`, and a `fix:` line for every refusal.
  C9   `fathom.toml`: §7.2's key table parses, carries the four keys the ticket asked for (noise
       globs, language list, benchmark allow/deny, history depth), every key has a declared consumer
       or is explicitly marked as not an input, and no key is listed twice.
  C10  the checker itself is not a lie: --self-test builds a scratch fixture out of the real
       artifacts, breaks one rule at a time, and asserts the check it names is the one that fires.

Usage:
    scripts/validate_fathom_layout.py [--adr PATH] [--root PATH] [--quiet] [--stats] [--self-test]

Exit status: 0 everything consistent, 1 a contract violation (one line each), 2 usage or environment
error (a missing ADR, unreadable TOML, a fixture that cannot be built).
"""
from __future__ import annotations

import argparse
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

#: ADR-0006 §4.1: what Fathom owns inside the cache root. Anything else is foreign (§4.2).
OWNED_ENTRIES = {"LAYOUT.md", "fathom.lock", "index"}

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
    """C2: `.gitignore` carries §12.1's block and nothing else about `.fathom`."""
    block = adr_gitignore_block(tree.adr)
    start = next(
        (i for i in range(len(gitignore) - len(block) + 1) if gitignore[i : i + len(block)] == block),
        None,
    )
    if start is None:
        problems.append(
            "C2: .gitignore does not contain ADR-0006 §12.1's block verbatim; the block is:\n    "
            + "\n    ".join(block)
        )
        return
    span = set(range(start, start + len(block)))
    for i, line in enumerate(gitignore):
        if ".fathom" in line and i not in span:
            problems.append(
                f"C2: .gitignore line {i + 1} mentions `.fathom` outside ADR-0006's block: {line!r}; "
                "a whitelist entry must be added to §12 and §4.1 in the same change, or deleted"
            )

    # the ticket's acceptance rule, both directions
    whitelist = {re.sub(r"^!/?\.fathom/", "", line).strip("/") for line in block if line.startswith("!")}
    committed = committed_from_tree(adr_tree_block(tree.adr))
    orphans = sorted(whitelist - committed)
    if orphans:
        problems.append(
            f"C2: .gitignore whitelists {orphans} under `.fathom/`, which ADR-0006 §4.1's tree does not "
            "contain; a whitelist entry that names nothing in the layout is dead and must go"
        )
    for entry in sorted(whitelist):
        path = tree.root / ".fathom" / entry
        if not path.exists():
            problems.append(
                f"C2: .gitignore un-ignores .fathom/{entry}, which does not exist on disk; either "
                "commit it or drop the rule (ADR-0006 §12)"
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
    """C7: everything physically in `.fathom/` is owned by the ADR or named by its §2 disposition."""
    root = tree.root / ".fathom"
    entries = sorted(p.name for p in root.iterdir()) if root.is_dir() else []
    if not entries:
        SKIPPED.append("C7: no .fathom/ directory in this checkout (it is gitignored), nothing to reconcile")
        return
    named = backticked(tree.adr)
    stats["entries under .fathom/"] = len(entries)
    for entry in entries:
        if entry in OWNED_ENTRIES:
            continue
        if any(entry == name or entry.startswith(name) for name in named):
            continue
        problems.append(
            f"C7: .fathom/{entry} is neither owned by ADR-0006 §4.1 nor named in its §2 disposition of "
            "foreign artifacts; the whole point of this ADR is that nothing in the cache root is a mystery"
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
    """`git ls-files` answer for one path, or None when git cannot be consulted."""
    try:
        out = subprocess.run(
            ["git", "ls-files", "--", path], cwd=root, capture_output=True, text=True
        )
    except OSError:
        return None
    if out.returncode != 0:
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


#: (mutation, the check ids that must fire — and only those)
MUTATIONS = [
    ("drop-whitelist", {"C2"}),
    ("orphan-whitelist", {"C2"}),
    ("layout-drift", {"C3"}),
    ("sqlite", {"C4"}),
    ("unbanned", {"C4"}),
    ("evidence-rule", {"C5"}),
    ("doc-wiring", {"C6"}),
    ("mystery", {"C7"}),
    ("undecided", {"C1"}),
    ("no-mechanism", {"C8"}),
    ("key-table", {"C9"}),
]


def self_test(adr_path: Path) -> int:
    """C10: build a clean fixture, then break one rule at a time and name the check that catches it."""
    problems = []
    with tempfile.TemporaryDirectory(prefix="fathom-layout-drill-") as tmp:
        base = Path(tmp) / "base"
        build_fixture(base, adr_path)
        for kind, expected in MUTATIONS:
            scratch = Path(tmp) / f"case-{kind}"
            shutil.copytree(base, scratch)
            mutate(kind, scratch)
            SKIPPED.clear()
            found = run_checks(Tree(scratch, scratch / "docs" / "adr" / ADR_NAME), {})
            fired = {p.split(":")[0] for p in found}
            if fired != expected:
                problems.append(
                    f"C10: mutating {kind!r} fired {sorted(fired) or 'nothing'}; this ADR's table says "
                    f"{sorted(expected)}. First lines: "
                    + " | ".join(p.splitlines()[0] for p in found[:3])
                )
            print(f"  drill {kind:<18} fired {', '.join(sorted(fired)) or 'nothing'}")

        SKIPPED.clear()
        clean = run_checks(Tree(base, base / "docs" / "adr" / ADR_NAME), {})
        # git ls-files cannot answer inside a scratch directory, so that one sub-check is expected noise.
        clean = [p for p in clean if not p.startswith("C2: .fathom/LAYOUT.md is not tracked")]
        if clean:
            problems.append(
                "C10: the untouched fixture did not pass:\n    " + "\n    ".join(p.splitlines()[0] for p in clean)
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
