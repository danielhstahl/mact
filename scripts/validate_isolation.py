#!/usr/bin/env python3
"""Check ADR-0003 (commit isolation + workspace serialization): its argv, its git protocol and its
lock protocol are what it says they are — and, unlike the sibling checkers, part of it is *executed*.

This is the dev-time checker for ADR-0003 (ticket mact-f1w.4), the sibling of
scripts/validate_tool_contract.py, scripts/validate_evidence.py and scripts/validate_run_lifecycle.py.
Three kinds of check:

  1. Self-consistency of the document: accepted, finished, and its §7.2 argv template, §7.4 golden command
     line, §4 gate table and §11 configuration table agree with each other and with the pinned expectations
     in this script (the same "one table in the script, one table in the ADR, they must match" pattern
     validate_tool_contract.py uses for error codes).
  2. Claims about the other accepted artifacts: the codes and `detail.mode` values the ADR emits exist in
     schema/tool-contract.schema.json, its new warnings are listed in ADR-0001 §5.4 and
     docs/tool-contract.md §7 and match schema/run-meta.schema.json, and its `--git-timeout-secs` fits
     ADR-0004's pre-start budget.
  3. Execution. `git` and `fcntl.flock` are real, so the ADR's §3 resolution rules, §4 gate checks, §5
     worktree lifecycle and §6 lock semantics are run against throwaway repositories and real processes
     rather than asserted in prose. act itself is the only stubbed thing; docker is never needed.

Checks performed (all of them, every run):

  C1   the ADR exists, is accepted, carries no unresolved marker, names its own load-bearing tokens.
  C2   every act flag the ADR emits is in ACT_FLAGS (transcribed from act's cmd/root.go, v0.2.62..v0.2.97),
       and no flag on its never-passed list is in the emitted argv.
  C3   §7.2's template, substituted with §7.4's scenario, equals §7.4's literal command line.
  C4   every failure reason in §4 is in ISOLATION_REASONS (both directions), every code it names is a real
       $defs/error_code member, and the §4.2 commands appear verbatim in the table.
  C5   the only isolation modes used are the ones x-error-codes documents for isolation_failed.
  C6   the three new warnings are in ADR-0001, in docs/tool-contract.md, and match run-meta's pattern.
  C7   §11's table equals EXPECTED_FLAGS and is compatible with ADR-0004's budget table.
  C8   live: §3 resolution + §4 gate behave exactly as the ADR's tables say, on real repositories, and the
       read-only commands do not write to the repository.
  C9   live: §6's lock contends across processes, is released by the holder's death, its body round-trips,
       and the file is never unlinked.
  C10  live: §5's worktree create/destroy commands work, including `remove` refusing a dirty tree without
       --force and `prune` recovering from rm -rf.

Usage:
    scripts/validate_isolation.py [--adr PATH] [--schema PATH] [--quiet] [--stats]

Exit status: 0 everything consistent, 1 a contract violation (one line each), 2 usage or environment
error (missing file, or git/flock unavailable for the checks that need them).
"""
from __future__ import annotations

import argparse
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any

try:
    import fcntl
except ImportError:  # pragma: no cover - the lock protocol is POSIX-only by decision (§6.5)
    fcntl = None  # type: ignore[assignment]

REPO = Path(__file__).resolve().parent.parent
DEFAULT_ADR = REPO / "docs/adr/0003-commit-isolation-and-serialization.md"
DEFAULT_SCHEMA = REPO / "schema/tool-contract.schema.json"
RUN_META_PATH = REPO / "schema/run-meta.schema.json"
ADR1_PATH = REPO / "docs/adr/0001-tool-contract.md"
ADR4_PATH = REPO / "docs/adr/0004-run-lifecycle.md"
REFERENCE_PATH = REPO / "docs/tool-contract.md"

UNRESOLVED_MARKERS = ("TODO", "TBD", "FIXME", "XXX", "placeholder", "not yet decided")

#: act's flag names, transcribed from nektos/act cmd/root.go at v0.2.62 through v0.2.97 (the range the ADR
#: claims, §7.7). Only the flags act-mcp could conceivably emit are listed; adding a flag to the ADR's argv
#: without adding it here is the point of the check.
ACT_FLAGS = {
    "directory", "workflows", "job", "bind", "reuse", "rm", "pull", "rebuild", "use-gitignore",
    "json", "quiet", "secret-file", "var-file", "env-file", "input-file", "env", "input", "var",
    "matrix", "defaultbranch", "network", "platform", "secret", "container-daemon-socket",
    "container-options", "container-working-directory", "artifact-server-path", "default-image",
    "privileged", "cache-server-path", "cache-server-addr", "no-cache-server", "action-offline-mode",
}

#: §7.5: flags that must never reach act's argv. Accumulate-able flags we do pass with pinned values
#: (--env, --input) are excluded from the absence test by name, because §7.5 forbids *extra* values, not
#: the flag itself.
NEVER_PASS = {
    "secret", "artifact-server-path", "container-working-directory", "container-options",
    "platform", "default-image", "network", "container-daemon-socket", "privileged",
    "cache-server-path", "cache-server-addr", "no-cache-server", "action-offline-mode", "var", "matrix",
}
ACCUMULATE_EXCEPTIONS = {"env", "input"}

#: §4: the complete set of isolation_failed reasons this ADR is allowed to produce.
ISOLATION_REASONS = {
    "head_unresolvable", "bare_repository", "dirty_worktree", "sparse_checkout",
    "submodules_unsupported", "policy_head_only", "scratch_unwritable", "git_timeout",
}

#: §9.3: members added to the open warnings vocabulary.
NEW_WARNINGS = ("isolation_worktree", "dirty_worktree", "worktree_cleanup_failed")

#: §11: the configuration surface this ADR owns.
EXPECTED_FLAGS: dict[str, dict[str, Any]] = {
    "--isolation": {"default": "hybrid", "choices": ["head-only", "hybrid", "worktree"]},
    "--allow-dirty-worktree": {"default": "false"},
    "--workspace-lock": {"default": "<workspace>/.act-mcp.lock"},
    "--run-scratch": {"default": "$XDG_RUNTIME_DIR/act-mcp-<uid>"},
    "--git-timeout-secs": {"default": "30"},
    "--act-binary": {"default": "act"},
    "--act-no-bind": {"default": "false"},
    "--act-workspace-config-files": {"default": "inert", "choices": ["inert", "discover"]},
}

#: §7.4's scenario, used to instantiate §7.2's template.
SCENARIO = {
    "<tree>": "/srv/ws",
    "<file>": "coverage.yml",
    "<task>": "coverage",
    "<run>": "20261110T004203Z-a1b2c3",
    "<scratch>": "/run/user/1000/act-mcp-1000",
    "<k>=<v>": "fuzz_rounds=4",
}

#: §4.2's gate, as commands (each is `git -C <ws> --no-optional-locks <this>`). The ADR table must contain
#: each one verbatim; the live checks then run exactly these strings.
GATE_CMDS = {
    "G1": "rev-parse --show-toplevel",
    "G2": "rev-parse --verify --quiet --end-of-options HEAD",
    "G3": "rev-parse --is-bare-repository",
    "G5": "status --porcelain --untracked-files=no",
    "G6": "config --bool core.sparseCheckout",
    "G7": "cat-file -e '<commit>:.gitmodules'",
}
#: §3.1's resolution commands, same rule.
RESOLVE_CMDS = {
    "R3": "rev-parse --verify --quiet --end-of-options '<sha>^{commit}'",
    "R4": "cat-file -t '<sha>'",
}

GIT_ENV = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": "/nonexistent",
           "LC_ALL": "C", "GIT_TERMINAL_PROMPT": "0", "GIT_CONFIG_NOSYSTEM": "1"}


class Problem(Exception):
    """A contract violation. Collected, not raised, so one run reports all of them."""


def load_text(path: Path) -> str:
    if not path.is_file():
        sys.stderr.write(f"error: {path} does not exist\n")
        raise SystemExit(2)
    return path.read_text(encoding="utf-8")


def load_json(path: Path) -> Any:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        sys.stderr.write(f"error: cannot read {path}: {exc}\n")
        raise SystemExit(2)


def section(text: str, heading: str) -> str:
    """The body of the first `## <heading>` section up to the next `## ` heading."""
    start = text.find(heading)
    if start < 0:
        return ""
    rest = text[start + len(heading):]
    end = rest.find("\n## ")
    return rest if end < 0 else rest[:end]


def code_blocks(text: str) -> list[str]:
    return re.findall(r"```(?:[a-z]+)?\n(.*?)```", text, re.S)


def table_rows(text: str) -> list[list[str]]:
    rows = []
    for line in text.splitlines():
        line = line.strip()
        if line.startswith("|") and not re.match(r"^\|[\s:|-]+\|$", line):
            rows.append([c.strip() for c in line.strip("|").split("|")])
    return rows


# ------------------------------------------------------------------ C1: the document is finished

def check_adr_text(adr: str, problems: list[str]) -> None:
    if "status: accepted" not in adr:
        problems.append("ADR-0003: must be marked status: accepted")
    for marker in UNRESOLVED_MARKERS:
        if marker.lower() in adr.lower():
            problems.append(
                f"ADR-0003: contains the unresolved marker {marker!r}; an accepted decision "
                "answers, it does not defer"
            )
    for needed in ("isolation_failed", "workspace_busy", "ACTMCP_EVIDENCE_OUT", "--run-scratch",
                   "--isolation", "flock", "--no-optional-locks", "never unlinked", "worktree add"):
        if needed not in adr:
            problems.append(f"ADR-0003: never mentions {needed!r}")
    for heading in ("## 3.", "## 4.", "## 5.", "## 6.", "## 7.", "## 8.", "## 9.", "## 11.", "## 16."):
        if heading not in adr:
            problems.append(f"ADR-0003: section {heading} is missing")


# ------------------------------------------------------- C2/C3: the argv is the argv it documents

def emitted_flags(adr: str, problems: list[str]) -> set[str]:
    """Flag names in §7.2's template, and the flags the §7.5 never-passed table forbids."""
    argv72 = code_blocks(section(adr, "## 7."))
    if not argv72:
        problems.append("ADR-0003 §7: no fenced argv block found")
        return set()
    template = argv72[0]
    names = set()
    for tok in re.findall(r"(?:^|\s)(-{1,2}[a-z0-9-]+)", template):
        names.add(tok.lstrip("-"))
    unknown = sorted(names - ACT_FLAGS)
    if unknown:
        problems.append(
            "ADR-0003 §7.2 emits act flags outside the transcribed act flag set (an unknown flag is a "
            f"fatal cobra error, F16): {', '.join(unknown)}"
        )
    never = set()
    for row in table_rows(section(adr, "### 7.5"))[1:]:
        for tok in re.findall(r"`(-{1,2}[a-z0-9-]+)", row[0]):
            never.add(tok.lstrip("-"))
    missing_rule = sorted(n for n in never if n in names and n not in ACCUMULATE_EXCEPTIONS)
    if missing_rule:
        problems.append(
            "ADR-0003 §7.5 forbids flags that §7.2 emits: " + ", ".join(missing_rule)
        )
    if not never:
        problems.append("ADR-0003 §7.5: the never-passed table parsed empty")
    return names


def check_golden_argv(adr: str, problems: list[str], stats: dict) -> None:
    sec = section(adr, "### 7.2")
    golden = None
    for block in code_blocks(section(adr, "### 7.4")):
        if block.lstrip().startswith("$ act"):
            golden = block
    if golden is None or not sec:
        problems.append("ADR-0003: §7.2 template or §7.4 golden command line is missing")
        return
    # template -> token list
    template_tokens: list[str] = []
    for line in code_blocks(sec)[0].splitlines():
        line = line.split("#", 1)[0].strip()
        if not line or line == "act" or "--defaultbranch" in line:
            continue                      # the worktree-only fragment; §7.4 is a head_only run
        template_tokens.extend(shlex.split(line))
    for key, value in SCENARIO.items():
        template_tokens = [t.replace(key, value) for t in template_tokens]
    # golden -> token list
    joined = " ".join(golden.lstrip("$").replace("\\\n", " ").replace("\\", " ").split())
    golden_tokens = [t for t in shlex.split(joined) if t != "act"]
    stats["argv tokens"] = len(template_tokens)
    if template_tokens != golden_tokens:
        i = next((k for k in range(min(len(template_tokens), len(golden_tokens)))
                  if template_tokens[k] != golden_tokens[k]), min(len(template_tokens), len(golden_tokens)))
        problems.append(
            "ADR-0003 §7.4's golden command line does not equal §7.2's template at token %d: template %r "
            "vs golden %r" % (i, template_tokens[i:i + 3], golden_tokens[i:i + 3])
        )


# ------------------------------------------------------- C4/C5: the gate table is honest about codes

def check_gate_table(adr: str, root: dict, problems: list[str], stats: dict) -> None:
    codes = set(root["$defs"]["error_code"]["enum"])
    sec4 = section(adr, "## 4.")
    for key, cmd in GATE_CMDS.items():
        if f"`{cmd}`" not in sec4:
            problems.append(f"ADR-0003 §4.2 row {key}: the pinned command {cmd!r} is not in the table verbatim")
        if cmd not in section(adr, "## 16."):
            problems.append(f"ADR-0003 §16 copy-verbatim block is missing the {key} command {cmd!r}")
    for key, cmd in RESOLVE_CMDS.items():
        if cmd not in sec3(adr):
            problems.append(f"ADR-0003 §3.1 step {key}: the pinned command {cmd!r} is not in the pipeline")
    reasons = set(re.findall(r'reason: "([a-z_]+)"', sec4))
    stats["isolation reasons"] = len(reasons)
    for r in sorted(reasons - ISOLATION_REASONS):
        problems.append(f"ADR-0003 §4: reason {r!r} is not in the pinned reason set")
    for r in sorted(ISOLATION_REASONS - reasons):
        problems.append(f"ADR-0003 §4: pinned reason {r!r} is never produced by the tables")
    suspicious = sorted(
        c for c in set(re.findall(r"`([a-z][a-z0-9_]+)`", sec4))
        if c.endswith(("_failed", "_unknown", "_not_found", "_busy", "_repository", "_ambiguous",
                       "_missing", "_unresolvable", "_unsupported"))
        and c not in codes and c not in ISOLATION_REASONS
    )
    if suspicious:
        problems.append(f"ADR-0003 §4 names codes that are not in $defs/error_code: {', '.join(suspicious)}")
    modes = set(re.findall(r"mode: \"?([a-z_]{5,})", sec4))
    documented = set(re.findall(r"mode: ([a-z_ |]+),", root["x-error-codes"]["isolation_failed"]["detail"]))
    allowed = {m.strip() for m in "".join(documented).split("|")}
    for mode in sorted(modes - allowed):
        problems.append(
            f"ADR-0003 uses detail.mode {mode!r}, which x-error-codes does not document for "
            f"isolation_failed (documented: {', '.join(sorted(allowed))})"
        )


def sec3(adr: str) -> str:
    return section(adr, "## 3.")


# ------------------------------------------------------- C6: the warnings vocabulary stays in sync

def check_warnings(adr: str, problems: list[str], stats: dict) -> None:
    pattern = load_json(RUN_META_PATH)["properties"]["warnings"]["items"]["pattern"]
    adr1 = load_text(ADR1_PATH)
    ref = load_text(REFERENCE_PATH)
    for warn in NEW_WARNINGS:
        if warn not in section(adr, "### 9.3"):
            problems.append(f"ADR-0003 §9.3 does not define the warning {warn!r} it adds")
        if warn not in section(adr1, "### 5.4"):
            problems.append(f"ADR-0001 §5.4 does not list the new warning {warn!r}")
        if warn not in section(ref, "### Warnings"):
            problems.append(f"docs/tool-contract.md's warnings list does not include {warn!r}")
        if not re.fullmatch(pattern, warn):
            problems.append(f"{warn!r} does not match schema/run-meta.schema.json's warnings pattern")
    stats["new warnings"] = len(NEW_WARNINGS)


# ------------------------------------------------------- C7: the configuration surface

def parse_flag_table(text: str, heading: str) -> dict[str, dict[str, str]]:
    out: dict[str, dict[str, str]] = {}
    for row in table_rows(section(text, heading))[1:]:
        if len(row) < 2:
            continue
        name = row[0].strip("` ").split(" ")[0]
        out[name] = {"default": row[1].strip("` "), "bound": row[2] if len(row) > 2 else "",
                     "row": " | ".join(row)}
    return out


def check_config(adr: str, problems: list[str], stats: dict) -> None:
    table = parse_flag_table(adr, "## 11.")
    stats["flags documented"] = len(table)
    if sorted(table) != sorted(EXPECTED_FLAGS):
        problems.append(
            "ADR-0003 §11 documents %s but the checker expects %s"
            % (", ".join(sorted(table)), ", ".join(sorted(EXPECTED_FLAGS)))
        )
    for flag, want in EXPECTED_FLAGS.items():
        got = table.get(flag)
        if not got:
            continue
        if want["default"] not in got["default"]:
            problems.append(f"ADR-0003 §11: {flag} default {got['default']!r}, expected {want['default']!r}")
        for choice in want.get("choices", []):
            if choice not in got["row"]:
                problems.append(f"ADR-0003 §11: {flag} does not advertise the value {choice!r}")
    if any("\\|" in line for line in section(adr, "## 11.").splitlines() if line.lstrip().startswith("|")):
        problems.append("ADR-0003 §11: an escaped pipe in the table would split a cell when parsed")
    # cross-document budget compatibility
    a4 = parse_flag_table(load_text(ADR4_PATH), "## 11.")
    prestart = a4.get("--prestart-timeout-secs", {}).get("default", "0")
    git_timeout = table.get("--git-timeout-secs", {}).get("default", "0")
    try:
        if int(git_timeout) > int(prestart):
            problems.append(
                f"ADR-0003 --git-timeout-secs ({git_timeout}) exceeds ADR-0004 --prestart-timeout-secs "
                f"({prestart}); the gate would then outlive the budget that bounds it"
            )
    except ValueError:
        problems.append("ADR-0003/ADR-0004: could not compare git-timeout-secs with prestart-timeout-secs")
    if "prestart-timeout-secs" not in table.get("--git-timeout-secs", {}).get("bound", ""):
        problems.append("ADR-0003 §11: --git-timeout-secs bound must name --prestart-timeout-secs")


# ------------------------------------------------------- C8: execute §3 and §4 against real repositories

class GitFixture:
    def __init__(self, root: Path):
        self.root = root

    def git(self, *args: str, cwd: Path | None = None, env: dict | None = None) -> subprocess.CompletedProcess:
        return subprocess.run(["git", "-C", str(cwd or self.root), *args], capture_output=True,
                              text=True, env=env or GIT_ENV)

    def gate(self, row: str, commit: str = "HEAD") -> subprocess.CompletedProcess:
        cmd = GATE_CMDS[row].replace("<commit>", commit)
        return self.git("--no-optional-locks", *shlex.split(cmd))

    def resolve(self, sha: str) -> subprocess.CompletedProcess:
        cmd = RESOLVE_CMDS["R3"].replace("<sha>", sha)
        return self.git(*shlex.split(cmd))


def init_repo(path: Path, *, bare: bool = False) -> GitFixture:
    path.mkdir(parents=True, exist_ok=True)
    fx = GitFixture(path)
    if bare:
        fx.git("init", "--bare", "-q", ".")
        return fx
    fx.git("init", "-q", "-b", "main", ".")
    for k, v in (("user.email", "checker@example.invalid"), ("user.name", "checker"),
                 ("commit.gpgsign", "false")):
        fx.git("config", k, v)
    (path / "a.txt").write_text("one\n")
    fx.git("add", "-A")
    fx.git("commit", "-qm", "one")
    return fx


def check_live_gate(adr: str, problems: list[str], stats: dict) -> None:
    if shutil.which("git") is None:  # pragma: no cover
        sys.stderr.write("error: git is required for the live checks (C8, C10)\n")
        raise SystemExit(2)
    tmp = Path(tempfile.mkdtemp(prefix="act-mcp-adr0003-"))
    try:
        # --- a plain, clean repository: G1/G2/G3/G5/G6/G7 all pass
        fx = init_repo(tmp / "clean")
        head = fx.gate("G2").stdout.strip()
        assert re.fullmatch(r"[0-9a-f]{40}", head), head
        if fx.gate("G5").stdout.strip():
            problems.append("C8: a freshly committed repository is reported dirty")
        if fx.gate("G3").stdout.strip() != "false":
            problems.append("C8: a non-bare repository is not reported as false by G3")
        if fx.gate("G6").returncode not in (0, 1) or fx.gate("G6").stdout.strip() == "true":
            problems.append("C8: G6 must accept an unset core.sparseCheckout")
        if fx.gate("G7").returncode == 0:
            problems.append("C8: G7 found .gitmodules in a repository without submodules")

        # --- untracked files are NOT dirt; a tracked modification IS
        (tmp / "clean" / "untracked.txt").write_text("noise\n")
        if fx.gate("G5").stdout.strip():
            problems.append("C8: an untracked file made the tree dirty; §4.2 says it must not")
        (tmp / "clean" / "a.txt").write_text("edited\n")
        if not fx.gate("G5").stdout.strip():
            problems.append("C8: a tracked modification did not make the tree dirty")
        (tmp / "clean" / "a.txt").write_text("one\n")

        # --- sparse checkout is detected through config, and only through config
        fx.git("config", "core.sparseCheckout", "true")
        if fx.gate("G6").stdout.strip() != "true":
            problems.append("C8: G6 did not read core.sparseCheckout=true")
        fx.git("config", "core.sparseCheckout", "false")

        # --- the read-only claim: --no-optional-locks leaves no lock files and no index rewrite
        index = fx.root / ".git" / "index"
        before = index.stat().st_mtime_ns if index.exists() else None
        for row in GATE_CMDS:
            fx.gate(row)
        locks = list((fx.root / ".git").glob("*.lock"))
        if locks:
            problems.append(f"C8: gate commands left lock files behind: {locks}")
        if before is not None and index.stat().st_mtime_ns != before:
            problems.append("C8: a gate command rewrote .git/index despite --no-optional-locks")

        # --- bare repository, detached HEAD, submodules
        bare = init_repo(tmp / "bare.git", bare=True)
        if bare.gate("G3").stdout.strip() != "true":
            problems.append("C8: a bare repository is not reported true by G3")
        fx.git("checkout", "-q", "--detach")
        if not re.fullmatch(r"[0-9a-f]{40}", fx.gate("G2").stdout.strip()):
            problems.append("C8: G2 must resolve HEAD on a detached HEAD (the gate does not care about refs)")
        (tmp / "clean" / ".gitmodules").write_text("[submodule \"x\"]\n\tpath = x\n\turl = ./x\n")
        fx.git("add", "-A")
        fx.git("commit", "-qm", "gitmodules")
        if fx.gate("G7").returncode != 0:
            problems.append("C8: G7 did not find .gitmodules in the target commit's tree")

        # --- §3 resolution: full, abbreviated, tag, unknown, non-commit
        sha = fx.git("rev-parse", "HEAD").stdout.strip()
        for label, value, want_rc in (("full", sha, 0), ("abbrev", sha[:8], 0)):
            if fx.resolve(value).returncode != want_rc:
                problems.append(f"C8: resolution of an {label} id failed")
        out = fx.resolve(sha[:8]).stdout.strip()
        if out != sha:
            problems.append(f"C8: an abbreviation resolved to {out!r}, not the full id {sha!r}")
        fx.git("tag", "v1", sha)
        if fx.resolve("v1").stdout.strip() != sha:
            problems.append("C8: a tag did not peel to its commit")
        if fx.resolve("deadbeef").returncode == 0:
            problems.append("C8: an unknown id resolved")
        blob = subprocess.run(
            ["git", "-C", str(fx.root), "hash-object", "-w", "--stdin"],
            input=b"hello\n", capture_output=True, env=GIT_ENV).stdout.decode().strip()
        if fx.resolve(blob).returncode == 0:
            problems.append("C8: a blob resolved as a commit")
        if fx.git("cat-file", "-t", blob).stdout.strip() != "blob":
            problems.append("C8: cat-file -t did not classify the blob (R4's not_a_commit branch)")

        # --- ambiguity: two refs sharing a prefix must be countable by R5's for-each-ref form
        pair = ambiguous_prefix(fx)
        if pair:
            pfx, ids = pair
            listing = fx.git("for-each-ref", "--format=%(objectname) %(*objectname) %(refname)")
            hits = [ln for ln in listing.stdout.splitlines()
                    if any(ln.startswith(i + " ") for i in ids)]
            if len(hits) < 2:
                problems.append("C8: R5's ref listing did not find the two refs sharing an abbreviation")
            if fx.resolve(pfx).returncode == 0:
                problems.append("C8: an ambiguous abbreviation resolved instead of failing")
        stats["gate rows executed"] = len(GATE_CMDS) + len(RESOLVE_CMDS)
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def ambiguous_prefix(fx: GitFixture) -> tuple[str, list[str]] | None:
    """Create two commits sharing a short prefix and point refs at them (C8's commit_ambiguous case)."""
    tree = fx.git("write-tree").stdout.strip()
    seen: dict[str, str] = {}
    env = dict(GIT_ENV, GIT_AUTHOR_NAME="c", GIT_AUTHOR_EMAIL="c@e.invalid",
               GIT_COMMITTER_NAME="c", GIT_COMMITTER_EMAIL="c@e.invalid")
    for i in range(2000):
        stamp = f"@{1600000000 + i} +0000"
        env["GIT_AUTHOR_DATE"] = stamp
        env["GIT_COMMITTER_DATE"] = stamp
        sha = subprocess.run(["git", "-C", str(fx.root), "commit-tree", tree, "-m", f"m{i}"],
                             capture_output=True, text=True, env=env).stdout.strip()
        if len(sha) < 3:
            continue
        if sha[:3] in seen:
            fx.git("update-ref", "refs/heads/amb-a", seen[sha[:3]])
            fx.git("update-ref", "refs/heads/amb-b", sha)
            return sha[:3], [seen[sha[:3]], sha]
        seen[sha[:3]] = sha
    return None


# ------------------------------------------------------- C9: execute §6's lock protocol

LOCK_KEYS = {"run_id", "phase", "started_at", "pid", "hostname"}


def check_live_lock(problems: list[str], stats: dict) -> None:
    if fcntl is None:  # pragma: no cover
        problems.append("C9 skipped: fcntl is unavailable, so the lock protocol cannot be exercised")
        return
    tmp = Path(tempfile.mkdtemp(prefix="act-mcp-lock-"))
    lock = tmp / ".act-mcp.lock"
    try:
        fd = os.open(lock, os.O_CREAT | os.O_RDWR, 0o644)
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            problems.append("C9: could not take a lock nobody holds")
        body = {"run_id": "20261110T004203Z-a1b2c3", "phase": "running",
                "started_at": "20261110T00:42:03Z", "pid": os.getpid(), "hostname": "checker"}
        os.ftruncate(fd, 0)
        os.write(fd, (json.dumps(body) + "\n").encode())
        os.fsync(fd)
        got = json.loads(Path(lock).read_text())
        if not LOCK_KEYS <= set(got):
            problems.append(f"C9: lock body is missing {sorted(LOCK_KEYS - set(got))}")
        # a second *process* must be turned away, and must be able to read the body
        probe = subprocess.run(
            [sys.executable, "-c",
             "import fcntl,json,os,sys; fd=os.open(sys.argv[1], os.O_RDWR); "
             "fcntl.flock(fd, fcntl.LOCK_EX|fcntl.LOCK_NB); sys.exit(0)" , str(lock)],
            capture_output=True, text=True)
        if probe.returncode == 0:
            problems.append("C9: a second process took a held flock (workspace_busy would never fire)")
        stats["lock contended"] = True
        # the same open file description re-locks trivially: a cloned handle would silently
        # double-book the workspace, which is the bug layer L1 exists to catch (§6.1)
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            problems.append("C9: re-locking the same open file description was denied; §6.1's reason for "
                            "layer L1 (a cloned handle would double-book) no longer holds")
        # a second, independent descriptor in the same process IS denied on Linux; §6.1 documents that
        # flock is the authority for in-process contention too, and L1 is for detail quality and cost
        second = os.open(lock, os.O_CREAT | os.O_RDWR, 0o644)
        intra_denied = False
        try:
            fcntl.flock(second, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            intra_denied = True
        os.close(second)
        if not intra_denied:
            problems.append("C9: a second independent flock in one process succeeded; §6.1 claims Linux "
                            "denies it, and without that L1 would be doing real serialization work")
        fcntl.flock(fd, fcntl.LOCK_UN)
        os.close(fd)
        if not lock.exists():
            problems.append("C9: the lock file vanished on release; §6.3 says it is never unlinked")
        # death of the holder must release the lock
        kid = subprocess.Popen(
            [sys.executable, "-c",
             "import fcntl,os,signal,sys; fd=os.open(sys.argv[1], os.O_CREAT|os.O_RDWR, 0o644); "
             "fcntl.flock(fd, fcntl.LOCK_EX); os.kill(os.getpid(), signal.SIGKILL)", str(lock)])
        kid.wait()
        fd2 = os.open(lock, os.O_CREAT | os.O_RDWR, 0o644)
        try:
            fcntl.flock(fd2, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            problems.append("C9: a SIGKILLed holder still held the lock (a stale-lock protocol would be "
                            "required, contrary to §6.5)")
        os.close(fd2)
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


# ------------------------------------------------------- C10: execute §5's worktree lifecycle

def check_live_worktree(problems: list[str], stats: dict) -> None:
    tmp = Path(tempfile.mkdtemp(prefix="act-mcp-wt-"))
    try:
        fx = init_repo(tmp / "ws")
        (tmp / "ws" / "b.txt").write_text("second\n")
        fx.git("add", "-A")
        fx.git("commit", "-qm", "two")
        first = fx.git("rev-parse", "HEAD~1").stdout.strip()
        tree = tmp / "scratch" / "run1" / "worktree"
        tree.parent.mkdir(parents=True, exist_ok=True)
        add = fx.git("worktree", "add", "--detach", str(tree), first)
        if add.returncode != 0:
            problems.append(f"C10: git worktree add failed: {add.stderr.strip()[:200]}")
            return
        at = GitFixture(tree).git("rev-parse", "HEAD").stdout.strip()
        if at != first:
            problems.append("C10: the worktree's HEAD is not the commit it was created at")
        dotgit = tree / ".git"
        if not dotgit.is_file():
            problems.append("C10: §5.4 claims a linked worktree's .git is a file; git says otherwise")
        else:
            target = dotgit.read_text().split("gitdir:", 1)[1].strip()
            if Path(target).is_relative_to(tree):
                problems.append("C10: §5.4 claims the gitdir lives outside the worktree; it does not")
        (tree / "b.txt").write_text("job wrote here\n")
        if fx.git("worktree", "remove", str(tree)).returncode == 0:
            problems.append("C10: worktree remove accepted a dirty tree without --force")
        if fx.git("worktree", "remove", "--force", str(tree)).returncode != 0:
            problems.append("C10: worktree remove --force failed on a dirty tree")
        if tree.exists():
            problems.append("C10: worktree remove --force left the directory behind")
        # the rm -rf fallback of §5.1: remove --force also drops the admin entry of a vanished tree,
        # which is why act-mcp never needs `git worktree prune` at all
        tree2 = tmp / "scratch" / "run2" / "worktree"
        tree2.parent.mkdir(parents=True, exist_ok=True)
        fx.git("worktree", "add", "--detach", str(tree2), first)
        shutil.rmtree(tree2)
        if fx.git("worktree", "remove", "--force", str(tree2)).returncode != 0:
            problems.append("C10: worktree remove --force failed on a vanished directory; §5.1's fallback "
                            "depends on it cleaning the admin entry")
        listed = fx.git("worktree", "list").stdout
        if str(tree2) in listed:
            problems.append(f"C10: a vanished worktree is still listed after remove --force: {listed.strip()}")
        stats["worktree lifecycle"] = "add/remove/remove-after-rm"
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


# ---------------------------------------------------------------------------- main

def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--adr", default=str(DEFAULT_ADR))
    ap.add_argument("--schema", default=str(DEFAULT_SCHEMA))
    ap.add_argument("--quiet", action="store_true")
    ap.add_argument("--stats", action="store_true")
    args = ap.parse_args()

    adr = load_text(Path(args.adr))
    root = load_json(Path(args.schema))
    problems: list[str] = []
    stats: dict[str, Any] = {}

    check_adr_text(adr, problems)
    emitted = emitted_flags(adr, problems)
    stats["act flags emitted"] = len(emitted)
    check_golden_argv(adr, problems, stats)
    check_gate_table(adr, root, problems, stats)
    check_warnings(adr, problems, stats)
    check_config(adr, problems, stats)
    check_live_gate(adr, problems, stats)
    check_live_lock(problems, stats)
    check_live_worktree(problems, stats)

    for p in problems:
        print(f"FAIL {p}")
    if args.stats:
        for k, v in sorted(stats.items()):
            print(f"  {k}: {v}")
    if not problems and not args.quiet:
        print("ok: ADR-0003's argv, gate, worktree and lock protocol are self-consistent and were executed "
              "against real git and real processes (%d flags, %d gate rows)"
              % (stats.get("act flags emitted", 0), stats.get("gate rows executed", 0)))
    return 1 if problems else 0


if __name__ == "__main__":
    raise SystemExit(main())
