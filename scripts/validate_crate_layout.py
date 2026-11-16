#!/usr/bin/env python3
"""Check ADR-0005 (fathom's crate layout and dependency rules): the workspace, the dependency
directions and the shared package policy are what the ADR says they are — read from the manifests,
not from prose.

This is the dev-time checker for ADR-0005 (ticket mact-by1.1), the sibling of
scripts/validate_tool_contract.py, scripts/validate_evidence.py, scripts/validate_run_lifecycle.py
and scripts/validate_isolation.py. Where those read a JSON Schema, this one reads `Cargo.toml`,
because ADR-0005's load-bearing artifacts *are* Cargo manifests: `[workspace] members`,
`[workspace.package]`, `[workspace.dependencies]`, `[workspace.lints]` and each crate's dependency
tables. It needs no third-party python package (tomllib is stdlib since 3.11) and no `cargo`
invocation, so it runs in the same CI step as its siblings.

It is written to pass in BOTH states of the repository: the single-crate state it was authored in
(act-mcp alone, before mact-by1.5 does the split) and the three-crate state ADR-0005 decides on. That
is deliberate — the rules bind the migration commit instead of being invented by it, and a half-done
split fails here rather than in a release build. `scripts/crate_layout_drill.sh` proves that half of
its life: it builds a throwaway three-crate workspace in a scratch directory, checks this script
passes it, then breaks one rule at a time and checks the right check fires.

Checks performed (all of them, every run):

  C1   the ADR: exists, has front matter (id/title/status/ticket/date), is accepted, carries no
       unresolved marker, states every load-bearing token of its decision, marks its rejected options
       rejected, and references only check ids this script implements.
  C2   workspace shape: exactly one Cargo.lock; a virtual root manifest when a workspace exists;
       every member exists, every `crates/*` crate is a member, `resolver = "2"`, no `default-members`
       narrowing; path dependencies stay inside the member set; no `[patch]`/`[replace]` in a member.
  C3   MSRV floor: the floor the ADR states equals the manifest floor, no package is below it, and a
       package that does not inherit it is a member raising its own (never the shared floor down).
  C4   `publish = false` on every package (ADR-0005 §8).
  C5   `act-evidence`'s dependency budget is the closed list in §6, `schemars`/`tempfile` are optional
       and feature-gated, and none of the named-forbidden crates appear in any of its tables.
  C6   dependency direction: act-mcp depends on neither fathom nor a UI crate; fathom depends on
       neither act-mcp nor the server's transport/async stack — dev-dependencies included (P4).
  C7   shared package policy: `version`/`edition` are set once in `[workspace.package]` and inherited;
       any dependency listed in `[workspace.dependencies]` is referenced as `workspace = true`.
  C8   lints: `unsafe_code = "deny"` is workspace-wide (or crate-root level, pre-split), every member
       opts in with `[lints] workspace = true`, and `missing_docs` is NOT promoted to the workspace.
  C9   one toolchain file and one `deny.toml` for the whole workspace, neither duplicated in a member.
  C10  names (P6): act-mcp keeps lib `act_mcp` + bin `act-mcp`; fathom has lib `fathom` + bin `fathom`.
  C11  one copy of each schema file, at `schema/`, and the `include_str!` that embeds it resolves to
       that copy rather than a second one.
  C12  the documentation wiring this ADR's deliverables promise: the "consumers of `act-evidence`"
       note in docs/evidence-format.md, the ADR in README's list, and this script wired into ci.sh.
  C13  one copy of the golden evidence tree: exactly one `testdata/expected/benchmark.jsonl` (§9's
       no-second-copy rule), because a duplicated fixture is a contract with two truths.

Usage:
    scripts/validate_crate_layout.py [--adr PATH] [--quiet] [--stats]

Exit status: 0 everything consistent, 1 a contract violation (one line each), 2 usage or environment
error (a missing manifest, unreadable TOML, or a repository with no Cargo.toml at all).
"""
from __future__ import annotations

import argparse
import re
import sys
import tomllib
from pathlib import Path
from typing import Any

REPO = Path(__file__).resolve().parent.parent
DEFAULT_ADR = REPO / "docs/adr/0005-fathom-crate-layout-and-dependencies.md"
EVIDENCE_FORMAT_DOC = REPO / "docs/evidence-format.md"
README = REPO / "README.md"
CI_SCRIPT = REPO / "scripts/ci.sh"

# --- the tables this ADR pins, transcribed so a disagreement is a finding -----------------------

#: ADR-0005 §6: the closed dependency budget for `crates/act-evidence`. Anything the contract crate
#: needs to read and validate `.evidence`, and nothing that makes it a second server or a second UI.
EVIDENCE_ALLOWED = {
    "serde",
    "serde_json",
    "chrono",
    "thiserror",
    "jsonschema",
    "schemars",  # only as an optional dependency behind the `json-schema` feature
    "tempfile",  # only as an optional dependency behind the `test-support` feature
}
#: The ones §6 names as out. Written out rather than implied, because "everything else is out" is a
#: rule nobody can check without a list of what people actually reach for.
EVIDENCE_FORBIDDEN = {
    "rmcp",
    "axum",
    "tower",
    "tower-http",
    "tokio",
    "reqwest",
    "clap",
    "tracing",
    "tracing-subscriber",
    "libc",
    "which",
    "yaml-rust2",
    "regex",
    "ratatui",
    "crossterm",
}
#: §6: nothing in a fathom tree may talk to a server, own a runtime, or borrow the server's crate.
FATHOM_FORBIDDEN = {
    "act-mcp",
    "act_mcp",
    "act-evidence-server",
    "rmcp",
    "axum",
    "tower",
    "tower-http",
    "tokio",
    "tokio-util",
    "reqwest",
    "hyper",
    "ureq",
    "git2",
    "libgit2-sys",
    # §6: fathom draws to a TTY and its headless `report` owes a parseable stdout; diagnostics are a
    # file or `doctor` output, not a subscriber wired into the binary's dependency tree.
    "tracing",
    "tracing-subscriber",
    "env_logger",
}
#: §6 / §16: the server's tree stays free of the UI's tree.
ACT_MCP_FORBIDDEN = {
    "fathom",
    "ratatui",
    "crossterm",
    "ratatui-core",
    "reedline",
}
#: §8: the floor today, set by rmcp 3.9's own manifest.
DEFAULT_FLOOR = (1, 88)
SCHEMA_FILES = ("evidence.schema.json", "run-meta.schema.json", "tool-contract.schema.json")
#: C1: tokens the decision has to state out loud, or the ADR has not decided them.
LOAD_BEARING_TOKENS = (
    "crates/act-evidence",
    "crates/act-mcp",
    "crates/fathom",
    "virtual root",
    "publish = false",
    "1.88",
    "rmcp",
    "axum",
    "tokio",
    "ratatui",
    "crossterm",
    "missing_docs",
    "unsafe_code",
    "json-schema",
    "schemars",
    "no new tool",
    "MUST NOT",
)

SKIPPED: list[str] = []


# --- loading -----------------------------------------------------------------------------------


def load_text(path: Path) -> str:
    try:
        return path.read_text(encoding="utf-8")
    except OSError as err:  # pragma: no cover - the message is the finding
        raise SystemExit(f"cannot read {path}: {err}")


def load_manifest(path: Path) -> dict[str, Any]:
    try:
        with path.open("rb") as handle:
            return tomllib.load(handle)
    except OSError as err:
        raise SystemExit(f"cannot read {path}: {err}")
    except tomllib.TOMLDecodeError as err:
        raise SystemExit(f"{path} is not valid TOML: {err}")


def cargo_directories(root: Path) -> list[Path]:
    """Cargo package directories present in the tree (target/ and vendored trees excluded)."""
    found = [root]
    crates = root / "crates"
    if crates.is_dir():
        found.extend(sorted(p.parent for p in crates.glob("*/Cargo.toml")))
    return found


class Package:
    """One Cargo package: its manifest, the fields that matter, and where its sources live."""

    def __init__(self, directory: Path, workspace: dict[str, Any] | None) -> None:
        self.dir = directory
        self.manifest = load_manifest(directory / "Cargo.toml")
        self.rel = directory.relative_to(REPO).as_posix() or "."
        self.workspace = workspace or {}
        self.pkg = self.manifest.get("package", {})
        self.name = self.pkg.get("name", "")

    def inherits(self, field: str) -> bool:
        """Does this `[package]` field say `field.workspace = true`?

        TOML parses that dotted key as a nested table (`{ "version": { "workspace": true } }`), which
        is the spelling Cargo's `? = true` inheritance actually arrives as.
        """
        value = self.pkg.get(field)
        return isinstance(value, dict) and value.get("workspace") is True

    def inherited(self, field: str) -> Any:
        """The effective value of a `[package]` field, following `field.workspace = true`."""
        if self.inherits(field):
            return self.workspace.get("package", {}).get(field)
        return self.pkg.get(field)

    def deps(self) -> list[tuple[str, str, dict[str, Any]]]:
        """Every declared dependency as (crate name, table it came from, the entry).

        A renamed dependency (`new = { package = "old" }`) is reported under `old`, because that is
        the crate a forbidden-crate rule has to look at.
        """
        out: list[tuple[str, str, dict[str, Any]]] = []
        tables: list[tuple[str, dict[str, Any]]] = [
            (top, self.manifest[top])
            for top in ("dependencies", "dev-dependencies", "build-dependencies")
            if isinstance(self.manifest.get(top), dict)
        ]
        for target, sub in self.manifest.get("target", {}).items():
            if isinstance(sub, dict):
                tables.extend(
                    (f"target.{target}.{top}", sub[top])
                    for top in ("dependencies", "dev-dependencies", "build-dependencies")
                    if isinstance(sub.get(top), dict)
                )
        for where, table in tables:
            for dep_name, entry in table.items():
                entry = entry if isinstance(entry, dict) else {"version": entry}
                out.append((str(entry.get("package", dep_name)), where, entry))
        return out

    def lib_source(self) -> Path | None:
        declared = self.manifest.get("lib", {}).get("path")
        candidate = self.dir / declared if declared else self.dir / "src/lib.rs"
        return candidate if candidate.is_file() else None

    def bin_names(self) -> list[str]:
        """Bin target names, honouring `[[bin]] name =` and the `src/main.rs` default."""
        names = [str(b.get("name", "")) for b in self.manifest.get("bin", []) if isinstance(b, dict)]
        if not names and (self.dir / "src/main.rs").is_file():
            names.append(str(self.pkg.get("name", "")))
        return [n for n in names if n]

    def lib_name(self) -> str:
        """The lib target's crate name, with Cargo's own dash-to-underscore default applied."""
        declared = self.manifest.get("lib", {}).get("name")
        return str(declared or str(self.pkg.get("name", "")).replace("-", "_"))


def version_tuple(text: Any) -> tuple[int, ...] | None:
    if not isinstance(text, str):
        return None
    parts = text.split(".")
    try:
        return tuple(int(part) for part in parts[:2])
    except ValueError:
        return None


def grep_sources(root: Path, needle: re.Pattern[str]) -> list[Path]:
    """Rust sources of every package in the tree (the package `src/` dirs, nothing vendored)."""
    hits: list[Path] = []
    for package_dir in cargo_directories(root):
        source_dir = package_dir / "src"
        if not source_dir.is_dir():
            continue
        for path in sorted(source_dir.rglob("*.rs")):
            if needle.search(load_text(path)):
                hits.append(path)
    return hits


# --- the checks --------------------------------------------------------------------------------


def check_adr(adr: str, problems: list[str], stats: dict[str, int]) -> None:
    """C1: the document itself."""
    lowered = adr.lower()
    if "status: accepted" not in adr:
        problems.append("ADR-0005 is not marked accepted; a decided decision is a decided one")
    for field in ("id: ADR-0005", "title:", "ticket: mact-by1.1", "date:"):
        if field not in adr:
            problems.append(f"ADR-0005 front matter is missing {field!r}")
    for marker in ("TODO", "TBD", "FIXME", "to be decided"):
        if marker.lower() in adr.lower():
            problems.append(f"ADR-0005 carries an unresolved marker: {marker!r}")
    missing = [token for token in LOAD_BEARING_TOKENS if token.lower() not in lowered]
    if missing:
        problems.append(f"ADR-0005 never states: {', '.join(missing)}")
    rejected = len(re.findall(r"\*\*Rejected", adr)) + len(re.findall(r"^- \*\*Rejected", adr))
    if rejected < 4:
        problems.append(
            f"ADR-0005 marks {rejected} alternatives rejected; the ticket asks for each option above "
            "to be recorded, the rejected ones marked rejected with a reason"
        )
    for claim in sorted({int(n) for n in re.findall(r"\bC(\d{1,2})\b", adr)}):
        if claim not in CHECK_IDS:
            problems.append(f"ADR-0005 cites check C{claim}, which this script does not implement")
    stats["checks cited by the ADR"] = len({int(n) for n in re.findall(r"\bC(\d{1,2})\b", adr)})


def check_workspace_shape(root_manifest: dict, root: Path, members: list[Package], problems: list[str]) -> None:
    """C2: one lockfile, a virtual root, members and crates/ agreeing, no escaping paths."""
    locks = [p for p in REPO.rglob("Cargo.lock") if "target" not in p.parts]
    if [str(p.relative_to(REPO)) for p in locks] != ["Cargo.lock"]:
        problems.append(
            f"C2: expected exactly one Cargo.lock at the repository root, found "
            f"{[str(p.relative_to(REPO)) for p in locks] or 'none'}"
        )
    workspace = root_manifest.get("workspace")
    if workspace is None:
        if (REPO / "crates").is_dir() and list((REPO / "crates").glob("*/Cargo.toml")):
            problems.append("C2: crates/* packages exist but the root manifest declares no [workspace]")
        return
    if "package" in root_manifest:
        problems.append("C2: the root manifest is both [package] and [workspace]; §3 pins a virtual root")
    if str(workspace.get("resolver", "")) != "2":
        problems.append('C2: [workspace] resolver must be "2"')
    if "default-members" in workspace:
        problems.append(
            "C2: [workspace] sets default-members; §3 leaves the default so `cargo build` at the root "
            "still covers every member (selective release building is `-p`, not a narrowed default)"
        )
    declared = [str(m) for m in workspace.get("members", [])]
    listed = {p.dir.relative_to(REPO).as_posix() for p in members}
    for missing in sorted(set(declared) - listed):
        problems.append(f"C2: member {missing!r} is listed but has no Cargo.toml")
    on_disk = {
        p.relative_to(REPO).as_posix()
        for p in cargo_directories(REPO)
        if p != REPO
    }
    for orphan in sorted(on_disk - set(declared)):
        problems.append(
            f"C2: {orphan}/Cargo.toml exists but is not a member; no orphan crates, and every crate "
            "under crates/ is in the members list (§3)"
        )
    for package in members:
        for name, where, entry in package.deps():
            path = entry.get("path")
            if path is not None:
                resolved = (package.dir / str(path)).resolve()
                if resolved not in {m.dir.resolve() for m in members}:
                    problems.append(
                        f"C2: {package.name}'s dependency {name!r} points at {path}, outside the member set"
                    )
            for forbidden in ("git", "git-ref", "rev", "branch"):
                if forbidden in entry:
                    problems.append(
                        f"C2: {package.name}'s dependency {name!r} is a git dependency ({forbidden}); "
                        "one lockfile, one registry story (§3)"
                    )
        for section in ("patch", "replace"):
            if section in package.manifest and package.dir != REPO:
                problems.append(f"C2: {package.rel} declares [{section}]; workspace-wide source rules stay in the root")


def check_msrv(members: list[Package], floor: tuple[int, int] | None, problems: list[str]) -> None:
    """C3: the ADR's floor is the manifest's floor, and nobody drags it around."""
    if floor is None:
        problems.append("C3: could not read the rust-version floor the ADR states")
        return
    for package in members:
        effective = package.inherited("rust-version")
        got = version_tuple(effective)
        if got is None:
            problems.append(f"C3: {package.rel} declares no usable rust-version")
            continue
        if got < floor:
            problems.append(
                f'C3: {package.rel} rust-version {effective!r} is below ADR-0005\'s floor "{floor[0]}.{floor[1]}"'
            )
        workspace_floor = version_tuple(package.workspace.get("package", {}).get("rust-version"))
        if workspace_floor is not None and workspace_floor < floor:
            problems.append(
                f"C3: [workspace.package] rust-version is below ADR-0005's floor; the shared floor never goes down (§8)"
            )
        if package.name == "act-mcp" and got > floor:
            problems.append(
                f"C3: act-mcp's rust-version {effective!r} is above the shared floor; the UI's dependency "
                "tree must not raise the server's floor (§8)"
            )


def check_publish(members: list[Package], problems: list[str]) -> None:
    """C4: publish = false everywhere (§8)."""
    for package in members:
        if package.inherited("publish") is not False:
            problems.append(
                f'C4: {package.rel} is publishable; §8 pins publish = false for all three crates '
                "(set it in [workspace.package] and inherit it)"
            )


def check_evidence_budget(members: list[Package], problems: list[str], stats: dict[str, int]) -> None:
    """C5: `act-evidence`'s closed dependency budget (§6)."""
    crate = next((p for p in members if p.name == "act-evidence"), None)
    if crate is None:
        SKIPPED.append("C5/C6 evidence-crate budget and fathom rules: no crates/act-evidence yet (mact-by1.5)")
        return
    listed = {}
    for name, where, entry in crate.deps():
        listed.setdefault(name, []).append((where, entry))
    for name in sorted(set(listed) - EVIDENCE_ALLOWED):
        problems.append(
            f"C5: crates/act-evidence depends on {name!r}, which is not in ADR-0005 §6's budget "
            "(adding a dependency to the contract crate means editing this ADR)"
        )
    for name in sorted(EVIDENCE_FORBIDDEN):
        if name in listed:
            problems.append(f"C5: crates/act-evidence depends on the forbidden crate {name!r} (§6)")
    for gated, feature in (("schemars", "json-schema"), ("tempfile", "test-support")):
        if gated not in listed:
            continue
        for where, entry in listed[gated]:
            if entry.get("optional") is not True:
                problems.append(
                    f"C5: {gated} must be an optional dependency of act-evidence, gated on the "
                    f"'{feature}' feature, so a reader's build need not carry it (found in {where})"
                )
        if feature not in crate.manifest.get("features", {}):
            problems.append(f"C5: crates/act-evidence declares no `{feature}` feature (§6)")
    stats["act-evidence dependencies"] = len(listed)


def check_direction(members: list[Package], problems: list[str]) -> None:
    """C6: the two edges that may not exist, dev-dependencies included (P4)."""
    for package in members:
        forbidden = {
            "act-mcp": ACT_MCP_FORBIDDEN,
            "fathom": FATHOM_FORBIDDEN,
        }.get(package.name, set())
        if not forbidden:
            continue
        for name, where, _entry in package.deps():
            if name in forbidden:
                problems.append(
                    f"C6: {package.name} depends on {name!r} in {where}; ADR-0005 §6 forbids it in any "
                    "table, dev-dependencies included"
                )
    if not any(p.name == "fathom" for p in members):
        SKIPPED.append("C6 fathom half: no crates/fathom yet (mact-by1.5)")


def check_shared_package_policy(root_manifest: dict, members: list[Package], problems: list[str]) -> None:
    """C7: version/edition inherited, [workspace.dependencies] used consistently (§8)."""
    workspace = root_manifest.get("workspace")
    if workspace is None:
        return
    shared = workspace.get("package", {})
    for field in ("version", "edition", "rust-version", "publish"):
        if field not in shared:
            problems.append(f"C7: [workspace.package] does not set {field!r}; §8 sets it once")
    for package in members:
        for field in ("version", "edition"):
            if not package.inherits(field):
                problems.append(
                    f"C7: {package.rel} sets its own {field}; one repo, one version, one edition (§8)"
                )
        shared_deps = workspace.get("dependencies", {})
        for name, where, entry in package.deps():
            if name in shared_deps and entry.get("workspace") is not True:
                problems.append(
                    f"C7: {package.rel} declares {name!r} in {where} with its own source while "
                    "[workspace.dependencies] pins it; a version is declared once (§8)"
                )


def check_lints(root_manifest: dict, members: list[Package], problems: list[str]) -> None:
    """C8: what is workspace-wide is workspace-wide, and what is not stays at the crate root (§8)."""
    if "workspace" not in root_manifest:
        crate_roots = [p.lib_source() for p in members]
        for source in [s for s in crate_roots if s is not None]:
            text = load_text(source)
            if "#![deny(unsafe_code)]" not in text and "unsafe_code" not in text:
                problems.append(f"C8: {source.relative_to(REPO)} does not deny unsafe_code")
            if "missing_docs" not in text:
                problems.append(f"C8: {source.relative_to(REPO)} does not warn missing_docs")
        return
    lints = root_manifest.get("workspace", {}).get("lints", {})
    if str(lints.get("rust", {}).get("unsafe_code", "")) != "deny":
        problems.append('C8: [workspace.lints.rust] must set unsafe_code = "deny" (§8)')
    if "missing_docs" in lints.get("rust", {}):
        problems.append(
            "C8: missing_docs is promoted to [workspace.lints]; §8 keeps it a crate attribute in each "
            "lib.rs, because [lints] applies to every target and `-D warnings` would demand docs on "
            "test scaffolding"
        )
    for package in members:
        if package.manifest.get("lints", {}).get("workspace") is not True:
            problems.append(f"C8: {package.rel} has no [lints] workspace = true (§8)")
        source = package.lib_source()
        if source is not None and "missing_docs" not in load_text(source):
            problems.append(
                f"C8: {source.relative_to(REPO)} carries no missing_docs attribute; §8 says the "
                "documentation promise stays at the crate root, where the public API lives"
            )


def check_singletons(members: list[Package], problems: list[str]) -> None:
    """C9: one toolchain, one cargo-deny config (P1)."""
    for filename in ("rust-toolchain.toml", "deny.toml"):
        copies = [p for p in REPO.rglob(filename) if "target" not in p.parts]
        if [str(p.relative_to(REPO)) for p in copies] != [filename]:
            problems.append(
                f"C9: expected exactly one {filename} at the root, found "
                f"{[str(p.relative_to(REPO)) for p in copies] or 'none'}"
            )


def check_names(members: list[Package], problems: list[str]) -> None:
    """C10: package/lib/bin names (P6)."""
    expectations = {
        "act-mcp": ("act_mcp", "act-mcp"),
        "fathom": ("fathom", "fathom"),
        "act-evidence": ("act_evidence", None),
    }
    for package in members:
        if package.name not in expectations:
            continue
        want_lib, want_bin = expectations[package.name]
        got_lib = package.lib_name()
        if got_lib != want_lib:
            problems.append(f"C10: {package.name}'s lib target is {got_lib!r}, ADR-0005 §8 pins {want_lib!r}")
        if want_bin is None:
            if package.bin_names():
                problems.append(f"C10: {package.name} must be a library only (§8), it declares a bin")
            continue
        bins = package.bin_names()
        if want_bin not in bins:
            problems.append(
                f"C10: {package.name}'s bins are {bins}; ADR-0005 §8 pins a bin named {want_bin!r} "
                "(the release artefact and smoke_binary.sh name it)"
            )


def check_schema_copies(root: Path, problems: list[str]) -> None:
    """C11: one copy of each schema, at schema/, and the embedding source resolves to it (§4, §9)."""
    for filename in SCHEMA_FILES:
        copies = [p for p in REPO.rglob(filename) if "target" not in p.parts]
        placed = [str(p.relative_to(REPO)) for p in copies]
        if placed != [f"schema/{filename}"]:
            problems.append(f"C11: expected exactly one {filename} at schema/{filename}, found {placed or 'none'}")
    embeds = grep_sources(
        root, re.compile(r'include_str!\("([^"]*schema/([a-z-]+\.schema\.json))"\)')
    )
    if not embeds:
        problems.append("C11: no source embeds schema/*.schema.json with include_str!; who pins the schema bytes?")
    for source in embeds:
        text = load_text(source)
        for relative, filename in re.findall(r'include_str!\("([^"]*schema/([a-z-]+\.schema\.json))"\)', text):
            resolved = (source.parent / relative).resolve()
            if resolved != (REPO / "schema" / filename).resolve():
                problems.append(
                    f"C11: {source.relative_to(REPO)} embeds {relative}, which resolves to "
                    f"{resolved.relative_to(REPO)} rather than schema/{filename}"
                )


def check_doc_wiring(adr_path: Path, adr: str, problems: list[str]) -> None:
    """C12: the documentation this ADR's deliverables promise."""
    doc = load_text(EVIDENCE_FORMAT_DOC)
    if "Consumers of `act-evidence`" not in doc:
        problems.append(
            "C12: docs/evidence-format.md has no 'Consumers of `act-evidence`' note; the ticket asks "
            "for one so the next reader knows a second consumer exists"
        )
    if adr_path.name not in doc:
        problems.append(f"C12: docs/evidence-format.md does not point at {adr_path.name}")
    if adr_path.name not in load_text(README):
        problems.append(f"C12: README's ADR list does not link {adr_path.name}")
    if "validate_crate_layout.py" not in load_text(CI_SCRIPT):
        problems.append("C12: scripts/ci.sh does not run validate_crate_layout.py; this checker must be CI's")
    if "ADR-0005" not in load_text(EVIDENCE_FORMAT_DOC):
        problems.append("C12: docs/evidence-format.md never names ADR-0005")


def check_one_golden_tree(problems: list[str], stats: dict[str, int]) -> None:
    """C13: the shared fixture corpus is never copied (§9)."""
    goldens = [
        p for p in REPO.rglob("benchmark.jsonl")
        if "target" not in p.parts and p.parent.name == "expected"
    ]
    stats["copies of the golden evidence tree"] = len(goldens)
    if len(goldens) != 1:
        problems.append(
            "C13: found %d copies of testdata/expected/benchmark.jsonl (%s); §9 allows one, moved, "
            "never copied" % (len(goldens), ", ".join(str(p.relative_to(REPO)) for p in goldens))
        )


CHECK_IDS = set(range(1, 14))


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=(__doc__ or "").splitlines()[0])
    parser.add_argument("--adr", type=Path, default=DEFAULT_ADR)
    parser.add_argument("--quiet", action="store_true")
    parser.add_argument("--stats", action="store_true")
    args = parser.parse_args(argv)

    root_manifest_path = REPO / "Cargo.toml"
    if not root_manifest_path.is_file():
        raise SystemExit("no Cargo.toml at the repository root; run this from the act-mcp checkout")
    root_manifest = load_manifest(root_manifest_path)
    adr = load_text(args.adr)

    workspace = root_manifest.get("workspace")
    if workspace is None:
        members = [Package(REPO, None)]
        state = "single crate (pre-mact-by1.5)"
    else:
        members = [Package(REPO / str(member), workspace) for member in workspace.get("members", [])]
        members = [m for m in members if (m.dir / "Cargo.toml").is_file()]
        state = "workspace: " + ", ".join(m.name for m in members)

    adr_floor = None
    match = re.search(r'rust-version = "([0-9.]+)"', adr)
    if match:
        adr_floor = version_tuple(match.group(1))
    if adr_floor is None:
        SKIPPED.append("C3 floor: ADR-0005 states no rust-version")
        adr_floor = DEFAULT_FLOOR

    problems: list[str] = []
    stats: dict[str, int] = {}

    check_adr(load_text(args.adr), problems, stats)
    check_workspace_shape(root_manifest, REPO, members, problems)
    check_msrv(members, adr_floor, problems)
    check_publish(members, problems)
    check_evidence_budget(members, problems, stats)
    check_direction(members, problems)
    check_shared_package_policy(root_manifest, members, problems)
    check_lints(root_manifest, members, problems)
    check_singletons(members, problems)
    check_names(members, problems)
    check_schema_copies(REPO, problems)
    check_doc_wiring(args.adr, adr, problems)
    check_one_golden_tree(problems, stats)

    for problem in problems:
        print(f"FAIL {problem}")
    if args.stats:
        for key, value in sorted(stats.items()):
            print(f"  {key}: {value}")
    for note in dict.fromkeys(SKIPPED):
        print(f"WARN {note}")
    if not problems:
        if not args.quiet:
            print(f"ok: ADR-0005 is accepted and the tree agrees with it — current state: {state}")
        return 0
    print(f"{len(problems)} crate-layout problem(s)")
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
