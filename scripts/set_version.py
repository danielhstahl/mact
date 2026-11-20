#!/usr/bin/env python3
"""Set the workspace version — once, where it is actually written — and prove it took.

This repository is a Cargo workspace (ADR-0005 §8): the version lives exactly once, in
`[workspace.package]` of the root `Cargo.toml`, and every member manifest says
`version.workspace = true`. That is what stops `fathom` and `act-mcp` drifting to unrelated
versions nobody chose, and it is what `cd.yml` relies on for a release: one tag, one version.

This script is that one write, plus the proof it worked:

1. the requested version is a plausible semver (a tag typo should fail here, not in a tarball);
2. the root manifest's `[workspace.package] version` is rewritten in place — the file's comments,
   key order and blank lines are preserved, because that file carries the reasoning for the floor
   and the publish rule and is not a generated file;
3. every member manifest is checked: it either inherits the version or says so with a literal that
   already matches. A member that silently stopped inheriting is a release that ships two versions,
   so that is a failure rather than a surprise;
4. if `cargo` is on PATH, `cargo metadata --no-deps` is asked what each package's version actually
   *is*. That is the end of the story: inheritance is Cargo's mechanic, so Cargo is the referee.

`--check` does all of it without writing, which is what `scripts/ci.sh`'s `schema` step can call
after a release (and what makes this script testable on a dirty tree).

Usage:
    scripts/set_version.py 1.2.3            # write it
    scripts/set_version.py --check 1.2.3    # assert that is the version, write nothing
    scripts/set_version.py --show           # print the current workspace version

Exit status: 0 done, 1 the tree disagrees (or --check found a mismatch), 2 bad usage or unreadable
manifest.
"""
from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
import tomllib
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
ROOT_MANIFEST = REPO / "Cargo.toml"

#: Semver, with an optional `-prerelease` and build metadata. Deliberately stricter than Cargo:
#: this is the version that ends up in a filename (`mact-<target>.tar.gz`) and in `--version`.
VERSION_RE = re.compile(
    r"^(?P<major>0|[1-9][0-9]*)\.(?P<minor>0|[1-9][0-9]*)\.(?P<patch>0|[1-9][0-9]*)"
    r"(?:-(?P<pre>[0-9A-Za-z.-]+))?(?:\+(?P<build>[0-9A-Za-z.-]+))?$"
)

#: The line this script rewrites, and only this one, inside `[workspace.package]`.
VERSION_LINE = re.compile(r'^(version[ ]*=[ ]*)"[^"]*"([ ]*(?:#.*)?)$')


class Problem(Exception):
    """A refusal with the sentence the release log should print."""


def load_manifest(path: Path) -> dict:
    try:
        with path.open("rb") as handle:
            return tomllib.load(handle)
    except FileNotFoundError:
        raise Problem(f"no manifest at {path}")
    except tomllib.TOMLDecodeError as err:
        raise Problem(f"{path} is not valid TOML: {err}")


def member_manifests(root_manifest: dict) -> list[Path]:
    """Every workspace member's manifest, or the root itself when it is a package (pre-split)."""
    members = root_manifest.get("workspace", {}).get("members")
    if not members:
        return [ROOT_MANIFEST]
    return [REPO / str(member) / "Cargo.toml" for member in members]


def current_version(text: str) -> str | None:
    """The `[workspace.package] version`, read from the *text* so comments do not matter."""
    in_section = False
    for line in text.splitlines():
        stripped = line.strip()
        if stripped.startswith("["):
            in_section = stripped == "[workspace.package]"
            continue
        if in_section:
            match = VERSION_LINE.match(line)
            if match:
                return match.group(0).split('"')[1]
    return None


def rewrite(text: str, version: str) -> str:
    """Replace the one `[workspace.package] version` line, leaving every other byte alone."""
    out: list[str] = []
    in_section = False
    replaced = 0
    for line in text.splitlines(keepends=True):
        stripped = line.strip()
        if stripped.startswith("["):
            in_section = stripped == "[workspace.package]"
        if in_section:
            match = VERSION_LINE.match(line.rstrip("\n"))
            if match:
                trailer = match.group(2).strip()
                comment = f"  {trailer}" if trailer else ""
                out.append(f'{match.group(1)}"{version}"{comment}\n')
                replaced += 1
                continue
        out.append(line)
    if replaced != 1:
        raise Problem(
            f"expected exactly one `version = \"…\"` under [workspace.package], replaced {replaced}; "
            "the root manifest is not the shape this script knows"
        )
    return "".join(out)


def check_members(version: str) -> list[str]:
    """Members either inherit or already say this version. Returns the findings."""
    findings: list[str] = []
    for manifest in member_manifests(load_manifest(ROOT_MANIFEST)):
        package = load_manifest(manifest).get("package", {})
        inherited = package.get("version")
        name = package.get("name", manifest.parent.name)
        if isinstance(inherited, dict) and inherited.get("workspace") is True:
            continue
        if inherited == version:
            findings.append(f"note: {manifest.relative_to(REPO)} sets its own version {inherited!r}")
            continue
        findings.append(
            f"{manifest.relative_to(REPO)} does not inherit the workspace version "
            f"(it says {inherited!r}); a release that ships two versions is not a release"
        )
    return findings


def cargo_says(version: str) -> list[str]:
    """Ask Cargo what each package's version really is — inheritance is its mechanic, so it referees."""
    if subprocess.run(["bash", "-lc", "command -v cargo"], capture_output=True).returncode != 0:
        return ["note: cargo is not on PATH; the manifest checks are all that ran"]
    try:
        completed = subprocess.run(
            ["cargo", "metadata", "--no-deps", "--format-version", "1"],
            capture_output=True,
            text=True,
            cwd=REPO,
            timeout=600,
        )
    except OSError as err:
        return[f"note: could not run cargo: {err}"]
    if completed.returncode != 0:
        return [f"`cargo metadata` failed: {completed.stderr.strip().splitlines()[-1:]}"]
    findings = []
    for package in json.loads(completed.stdout).get("packages", []):
        if package.get("version") != version:
            findings.append(
                f"cargo reports {package['name']} at {package['version']}, not {version}"
            )
    return findings


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=(__doc__ or "").splitlines()[0])
    parser.add_argument("version", nargs="?", help="the version to set, e.g. 0.1.2")
    parser.add_argument("--check", action="store_true", help="verify without writing")
    parser.add_argument("--show", action="store_true", help="print the current version and exit")
    args = parser.parse_args(argv)

    text = ROOT_MANIFEST.read_text(encoding="utf-8")
    have = current_version(text)
    if have is None:
        raise Problem("no [workspace.package] version in the root manifest")

    if args.show:
        print(have)
        return 0
    if not args.version:
        parser.error("a version is required (or --check / --show)")
    if not VERSION_RE.match(args.version):
        raise Problem(
            f"{args.version!r} is not a version this repo releases (expected major.minor.patch, "
            "optionally -prerelease); tags are cut as `vX.Y.Z`"
        )

    findings = check_members(args.version)
    hard = [line for line in findings if not line.startswith("note:")]
    if args.check:
        if have != args.version:
            hard.append(f"root manifest says {have}, expected {args.version}")
        hard.extend(cargo_says(args.version))
        for line in findings + [x for x in hard]:
            print(line)
        if hard:
            print(f"{len(hard)} version problem(s)")
            return 1
        print(f"ok: every package in the workspace is at {args.version}")
        return 0

    if have != args.version:
        ROOT_MANIFEST.write_text(rewrite(text, args.version), encoding="utf-8")
        print(f"{ROOT_MANIFEST.relative_to(REPO)}: {have} -> {args.version}")
    else:
        print(f"{ROOT_MANIFEST.relative_to(REPO)}: already {args.version}")

    for line in cargo_says(args.version):
        print(line)
    after = current_version(ROOT_MANIFEST.read_text(encoding="utf-8"))
    if after != args.version:
        print(f"the write did not take: the manifest now says {after!r}")
        return 1
    print(f"ok: the workspace is at {args.version}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Problem as problem:
        print(f"set_version: {problem}", file=sys.stderr)
        raise SystemExit(1)
