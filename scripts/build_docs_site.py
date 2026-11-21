#!/usr/bin/env python3
"""Build a static HTML site out of docs/ — no build dependency, no network, no state.

Why a script and not a generator
--------------------------------
`docs/` is the reasoning this repository runs on: the ADRs, the CI map, the evidence contract,
the threat model. It is written in Markdown because Markdown is what a reviewer reads in a diff,
and it is *read* in a browser by people who will not clone the tree. Something has to turn one
into the other, and the two ways to do that both cost more than they return:

  * an mdBook/Sphinx/Hugo dependency means a build this repository cannot audit (deny.toml guards
    200 crates of Rust; a Haskell binary fetched off a release page is not covered by any of it),
    that needs the network, that is not installed on a laptop, and that wants the sources rearranged
    (`src/SUMMARY.md`, `_toc.yml`) for a tree whose layout is itself normative (ADR-0006).
  * putting the transformation in the workflow YAML is the one thing this repository never does:
    a check you cannot run locally is a check you cannot debug, and `.github/workflows/*.yml` here
    is setup plus one call into `scripts/`.

So the transformation is this script: Markdown in, HTML site out, standard library only. It is
runnable before a Pages site exists, it is checked by its own `--check`, and the workflow that
publishes the result is the four lines in `.github/workflows/docs.yml`.

What it does
------------
  * Every `*.md` under `--docs-dir` becomes a page at the same relative path with `.html`, so
    `docs/adr/0001-tool-contract.md` becomes `<out>/adr/0001-tool-contract.html`. Structure is
    kept because the cross-links between the ADRs are relative and staying relative is what makes
    the site work at any base path (a project Page, a custom domain, a preview unzipped from the
    job artefact, or `file://` on your own disk) with no `base_url` to configure.
  * A landing page (`index.html`) lists every page with the first paragraph as its summary; ADRs
    show their front-matter `id`, `status` and `date`. A `404.html` carries the same navigation so
    a wrong path on Pages still says where the docs live.
  * Links are rewritten, and that is the part with a job to do: `[ADR-0004 §17.1](adr/0004-run-lifecycle.md)`
    has to become `adr/0004-run-lifecycle.html`, and a link that names a section that does not exist
    is the same rot as a broken intra-doc link in Rust — `doc` is a fatal step in `scripts/ci.sh`
    for exactly that reason. So: `.md` -> `.html`, GitHub-compatible heading anchors are generated
    (the links already in the docs, like `#5-action-success--the-durable-question`, are the test),
    a link into the docs tree that does not resolve is an error, and a link that leaves the docs
    tree (`../README.md`, `../schema/evidence.schema.json`) is rewritten to a `--github-url` blob
    URL when one is given and reported as a warning when it is not.
  * Output is deterministic: sorted paths, no timestamps, no run ids. Two builds of one commit are
    byte-identical, which is what makes "did this change alter the site?" a `diff` and not a judgement.

Markdown coverage is the subset docs/ actually uses, and it is deliberate about the boundaries:
ATX headings, fenced code (``` and ~~~, with the info string kept as a class), GFM tables with
alignment, blockquotes, nested ordered/unordered lists, thematic breaks, YAML front matter, code
spans, links and images, `**strong**`, `*em*`, `_em_`, `~~del~~` and `<autolinks>`. Setext
headings and indented code blocks are not supported: the tree uses neither, and a parser that
silently guesses at four leading spaces is worse than one that says so.

Usage:
    scripts/build_docs_site.py [--docs-dir docs] [--out target/docs-site]
                              [--github-url URL] [--title TITLE] [--check] [--clean]
                              [--strict] [-W N] [--quiet]

    -W/--max-warnings caps the warning list, because "these 48 links leave docs/ and no --github-url
    was given" is one fact repeated, not 48 facts, and a log that shouts is a log nobody reads.

    --check runs the builder's own tests against a fixture tree in a temporary directory: nothing
    here needs the network, a toolchain or even this repository's docs, which is why CI can run it
    on every commit (`scripts/ci.sh docs-site`) and why it is the first step of the publish job.
    One of those cases builds the fixture and then rebuilds it through a symlink to its own root, so
    a path that compares a resolved path against an assembled one fails here on Linux instead of only
    on a Mac, where `$TMPDIR` is symlinked by default (docs/testing.md §1).

Exit status: 0 built (or every self-test passed), 1 a build error or a failed self-test, 2 bad
usage or an unreadable tree. With --strict, a warning (a link that leaves the docs tree with no
--github-url to rewrite it into, an asset in docs/ that no page references) is an error too.
"""
from __future__ import annotations

import argparse
import html
import os
import re
import shutil
import sys
import tempfile
import unicodedata
from dataclasses import dataclass, field
from pathlib import Path, PurePosixPath

REPO = Path(__file__).resolve().parent.parent

# ---------------------------------------------------------------------------
# errors
# ---------------------------------------------------------------------------


class BuildError(Exception):
    """One thing that stopped this page (or the whole build) from being trustworthy."""


# ---------------------------------------------------------------------------
# front matter
#
# ADRs open with a `---` block carrying id/title/status/ticket/date. Only flat `key: value` pairs
# are read, because only flat pairs exist: a parser that grew YAML would grow a dependency.
# ---------------------------------------------------------------------------


def split_front_matter(lines: list[str]) -> tuple[dict[str, str], list[str]]:
    if not lines or lines[0].strip() != "---":
        return {}, lines
    for i in range(1, len(lines)):
        if lines[i].strip() in ("---", "..."):
            meta: dict[str, str] = {}
            for raw in lines[1:i]:
                if not raw.strip() or raw.lstrip().startswith("#"):
                    continue
                if ":" not in raw:
                    continue
                key, _, value = raw.partition(":")
                meta[key.strip()] = value.strip().strip("'\"")
            return meta, lines[i + 1 :]
    raise BuildError("front matter opened with --- and never closed")


# ---------------------------------------------------------------------------
# heading anchors
#
# GitHub's slugger, reimplemented: lowercase, drop inline markup, drop every Unicode punctuation
# and symbol codepoint (keeping - and _), then every space becomes a hyphen. The odd-looking
# results are the point: `## 3. Coverage %` -> `3-coverage-` and
# `## 5. Action Success — the durable question` -> `5-action-success--the-durable-question`
# are the anchors already written into the docs, and an anchor function that "cleaned them up"
# would break every cross-section link in the tree.
# ---------------------------------------------------------------------------

_LINK_FORM = re.compile(r"\[([^\]]*)\]\(([^)]*)\)")
_DROP_MARKUP = (
    (re.compile(r"`+([^`]*)`+"), r"\1"),
    (re.compile(r"!\[([^\]]*)\]\([^)]*\)"), r"\1"),
    (re.compile(r"\*\*(.+?)\*\*"), r"\1"),
    (re.compile(r"__(.+?)__"), r"\1"),
    (re.compile(r"~~(.+?)~~"), r"\1"),
    (re.compile(r"(?<!\w)\*(\S(?:.*?\S)?)\*(?!\w)"), r"\1"),
    (re.compile(r"(?<!\w)_(\S(?:.*?\S)?)_(?!\w)"), r"\1"),
)


def strip_inline_markup(text: str) -> str:
    text = _LINK_FORM.sub(r"\1", text)
    for pattern, repl in _DROP_MARKUP:
        text = pattern.sub(repl, text)
    return text


def slugify(text: str) -> str:
    text = strip_inline_markup(text).strip().lower()
    kept = []
    for ch in text:
        if ch in (" ", "\t"):
            kept.append(" ")
        elif ch in ("-", "_"):
            kept.append(ch)
        else:
            category = unicodedata.category(ch)
            if category[0] in ("L", "N", "M"):
                kept.append(ch)
    # deliberately no trailing strip: `## 3. Coverage %` anchors as `3-coverage-` on GitHub, and the
    # section links already written into docs/ assume exactly that.
    return "".join(kept).replace(" ", "-")


# ---------------------------------------------------------------------------
# block-level patterns
# ---------------------------------------------------------------------------

RE_BLANK = re.compile(r"^\s*$")
RE_FENCE = re.compile(r"^( {0,3})(`{3,}|~{3,})[ \t]*(\S*)[ \t]*(.*)$")
RE_HEADING = re.compile(r"^( {0,3})(#{1,6})[ \t]+(.*?)[ \t]*#*[ \t]*$")
RE_HR = re.compile(r"^ {0,3}([-*_])[ \t]*(?:\1[ \t]*){2,}$")
RE_QUOTE = re.compile(r"^ {0,3}>[ \t]?(.*)$")
RE_LIST_ITEM = re.compile(r"^(\s*)([-*+]|\d{1,9}[.)])([ \t]+)(.*)$")
RE_TABLE_DELIM = re.compile(r"^\s*\|?\s*:?-+:?\s*(?:\|\s*:?-+:?\s*)*\|?\s*$")


# ---------------------------------------------------------------------------
# inline rendering
#
# Three passes, in the order the constructs nest: code spans and links first (they are the two that
# can contain the others' *text* but never their markup), then emphasis as a recursive wrapper so
# that escaping happens exactly once, at the leaves. Escaping late is the whole trick behind "no
# HTML injection from a Markdown file": nothing reaches the output unescaped except the tags this
# function itself emits.
# ---------------------------------------------------------------------------

RE_AUTOLINK = re.compile(r"<(https?://[^\s<>]+|mailto:[^\s<>]+)>")


ESCAPE_BASE = 0xE000  # a Unicode private-use codepoint, out of the way of every real character


def encode_escape(char: str) -> str:
    """A backslash escape becomes one character that no markup rule can mistake for markup."""
    return chr(ESCAPE_BASE + ord(char))


def decode_escapes(text: str) -> str:
    out = []
    for char in text:
        code = ord(char)
        out.append(chr(code - ESCAPE_BASE) if ESCAPE_BASE <= code <= ESCAPE_BASE + 0xFF else char)
    return "".join(out)


def esc(text: str) -> str:
    return html.escape(decode_escapes(text), quote=False)


def esc_attr(text: str) -> str:
    return html.escape(decode_escapes(text), quote=True)


def _skip_backticks(text: str, i: int) -> int:
    """Index just past a backtick run, so bracket matching does not count a `[` inside `code`."""
    j = i
    n = len(text)
    while j < n and text[j] == "`":
        j += 1
    run = text[i:j]
    k = text.find(run, j)
    while k != -1 and k + len(run) < n and text[k + len(run)] == "`":
        k = text.find(run, k + 1)
    return len(text) if k == -1 else k + len(run)


def _find_close(text: str, i: int, opener: str, closer: str) -> int:
    depth = 0
    j = i
    n = len(text)
    while j < n:
        ch = text[j]
        if ch == "\\":
            j += 2
            continue
        if ch == "`":
            j = _skip_backticks(text, j)
            continue
        if ch == opener:
            depth += 1
        elif ch == closer:
            depth -= 1
            if depth == 0:
                return j
        j += 1
    return -1


def _find_paren_close(text: str, i: int) -> int:
    depth = 0
    j = i
    n = len(text)
    while j < n:
        ch = text[j]
        if ch == "\\":
            j += 2
            continue
        if ch == "`":
            j = _skip_backticks(text, j)
            continue
        if ch == "(":
            depth += 1
        elif ch == ")":
            depth -= 1
            if depth == 0:
                return j
        j += 1
    return -1


def _split_link_destination(raw: str) -> tuple[str, str]:
    raw = raw.strip()
    if raw.startswith("<"):
        end = raw.find(">")
        if end != -1:
            return raw[1:end], raw[end + 1 :].strip()
    parts = raw.split(None, 1)
    if not parts:
        return "", ""
    if len(parts) == 1:
        return parts[0], ""
    return parts[0], parts[1].strip().strip("\"'")


# Emphasis, innermost-last: each rule wraps what the rules after it produced, and the base case is
# the escaped leaf. Every rule may match across a line break, because a paragraph reaches this
# function with its source newlines in it and a bold phrase that wraps in the file is one emphasis
# run, not a leaked pair of asterisks; and every rule sees the placeholder characters that code
# spans and link destinations became, so bold text containing either stays bold.
# The `(?<![0-9A-Za-z_])` guard on the underscore rules is what keeps ACTMCP_EVIDENCE_OUT an
# identifier instead of an em.
EM_RULES: list[tuple[re.Pattern[str], str]] = [
    (re.compile(r"\*\*(?=\S)([\s\S]+?)(?<=\S)\*\*"), "<strong>%s</strong>"),
    (re.compile(r"__(?=\S)([\s\S]+?)(?<=\S)__"), "<strong>%s</strong>"),
    (re.compile(r"~~(?=\S)([\s\S]+?)(?<=\S)~~"), "<del>%s</del>"),
    (re.compile(r"(?<![0-9A-Za-z_*])\*(?=\S)([^*]+?)(?<=\S)\*(?!\*)"), "<em>%s</em>"),
    (re.compile(r"(?<![0-9A-Za-z_])_(?=\S)([^_]+?)(?<=\S)_(?![0-9A-Za-z_])"), "<em>%s</em>"),
]


def render_emphasis(text: str, depth: int = 0) -> str:
    if depth >= len(EM_RULES):
        return esc(text)
    pattern, wrapper = EM_RULES[depth]
    out: list[str] = []
    pos = 0
    for match in pattern.finditer(text):
        out.append(render_emphasis(text[pos : match.start()], depth + 1))
        out.append(wrapper % render_emphasis(match.group(1), depth + 1))
        pos = match.end()
    out.append(render_emphasis(text[pos: ], depth + 1))
    return "".join(out)


def render_inline(text: str, ctx: "PageContext") -> str:
    """One line (or one joined paragraph) of inline Markdown to HTML.

    The order is the point. Code spans, link destinations and autolinks are lifted out into
    placeholder characters *before* emphasis runs, because `**bold with `code` and a [link](x)
    inside**` is a single emphasis run over text this function must not cut into pieces: a renderer
    that applies emphasis to the plain runs between code spans turns half of docs/ into literal
    asterisks. Whatever is left is emphasis and text, and text is escaped at the leaves.
    """
    slots: dict[str, str] = {}
    text = _unhide_escapes(text)
    hidden = _hide_code_spans(text, slots)
    hidden = _hide_links_and_autolinks(hidden, slots, ctx)
    rendered = render_emphasis(hidden)
    return _refill(rendered, slots)


SLOT_BASE = 0xF000  # the other private-use plane: escapes live at 0xE000, slots at 0xF000


def _slot(slots: dict[str, str], html_text: str) -> str:
    """One placeholder character for a run of markup that emphasis must treat as a single letter."""
    if len(slots) >= 0x8FF:  # a paragraph with 2 303 spans is not a paragraph; render it anyway
        return html_text
    mark = chr(SLOT_BASE + len(slots))
    slots[mark] = html_text
    return mark


def _refill(text: str, slots: dict[str, str]) -> str:
    """Put the lifted markup back. A link's label can hold a code span's placeholder, so one pass is
    not always enough; the loop is bounded because a slot value can only ever get shallower."""
    for _ in range(8):
        if not any(mark in text for mark in slots):
            break
        text = "".join(slots.get(ch, ch) for ch in text)
    return text


def _hide_code_spans(text: str, slots: dict[str, str]) -> str:
    out: list[str] = []
    i = 0
    n = len(text)
    while i < n:
        if text[i] == "`":
            j = i
            while j < n and text[j] == "`":
                j += 1
            run = text[i:j]
            k = text.find(run, j)
            while k != -1 and k + len(run) < n and text[k + len(run)] == "`":
                k = text.find(run, k + 1)
            if k != -1:
                content = text[j:k]
                if len(content) >= 2 and content.startswith(" ") and content.endswith(" "):
                    content = content[1:-1]
                out.append(_slot(slots, "<code>%s</code>" % esc(content)))
                i = k + len(run)
                continue
        out.append(text[i])
        i += 1
    return "".join(out)


def _hide_links_and_autolinks(text: str, slots: dict[str, str], ctx: "PageContext") -> str:
    out: list[str] = []
    i = 0
    n = len(text)
    while i < n:
        ch = text[i]
        if ch == "!":
            image = _image(text, i)
            if image is not None:
                alt_raw, dest, end = image
                src, title = _split_link_destination(dest)
                out.append(_slot(slots, '<img src="%s" alt="%s"%s>'
                                 % (esc_attr(ctx.resolve(src, text)), esc_attr(strip_inline_markup(alt_raw)),
                                    ' title="%s"' % esc_attr(title) if title else "")))
                i = end
                continue
        if ch == "[":
            link = _link(text, i)
            if link is not None:
                label, dest, end = link
                href, title = _split_link_destination(dest)
                # The label is inline Markdown of its own: `[**strong** `code`](x)` has to keep the
                # strong, and the destination must not be re-read as markup by anything after this.
                anchor = '<a href="%s"%s>%s</a>' % (esc_attr(ctx.resolve(href, text)),
                                                    ' title="%s"' % esc_attr(title) if title else "",
                                                    render_inline(label, ctx))
                out.append(_slot(slots, anchor))
                i = end
                continue
        if ch == "<":
            match = RE_AUTOLINK.match(text, i)
            if match:
                target = match.group(1)
                out.append(_slot(slots, '<a href="%s">%s</a>' % (esc_attr(target), esc(target))))
                i = match.end()
                continue
        out.append(ch)
        i += 1
    return "".join(out)


# Backslash escapes are resolved first of all, into characters no markup rule can read as markup:
# `\*not em\*` has to come out as `*not em*`, and an emphasis pass that ran over the unescaped
# asterisks would produce an em instead.
ESCAPABLE = "\\`*_{}[]()#+-.!~<>\"'"


def _unhide_escapes(text: str) -> str:
    out = []
    i = 0
    n = len(text)
    while i < n:
        if text[i] == "\\" and i + 1 < n and text[i + 1] in ESCAPABLE:
            out.append(encode_escape(text[i + 1]))
            i += 2
            continue
        out.append(text[i])
        i += 1
    return "".join(out)


def _link(text: str, i: int):
    close = _find_close(text, i, "[", "]")
    if close == -1 or close + 1 >= len(text) or text[close + 1] != "(":
        return None
    end = _find_paren_close(text, close + 1)
    if end == -1:
        return None
    return text[i + 1 : close], text[close + 2 : end], end + 1


def _image(text: str, i: int):
    if i + 1 >= len(text) or text[i + 1] != "[":
        return None
    close = _find_close(text, i + 1, "[", "]")
    if close == -1 or close + 1 >= len(text) or text[close + 1] != "(":
        return None
    end = _find_paren_close(text, close + 1)
    if end == -1:
        return None
    return text[i + 2 : close], text[close + 2 : end], end + 1


# ---------------------------------------------------------------------------
# the page and site model
# ---------------------------------------------------------------------------


@dataclass
class Doc:
    """One Markdown source, its place in the site, and what it anchors."""

    src: Path                      # absolute path of the .md file
    out_rel: PurePosixPath         # site-relative output path, e.g. adr/0001.html
    title: str
    meta: dict[str, str]
    lines: list[str]
    anchors: set[str] = field(default_factory=set)
    summary: str = ""


@dataclass
class Site:
    docs_dir: Path
    out_dir: Path
    repo_root: Path = REPO
    github_url: str | None = None
    docs: list[Doc] = field(default_factory=list)
    by_src: dict[Path, Doc] = field(default_factory=dict)
    anchors: dict[str, set[str]] = field(default_factory=dict)
    warnings: list[str] = field(default_factory=list)
    errors: list[str] = field(default_factory=list)

    def warn(self, message: str) -> None:
        if message not in self.warnings:
            self.warnings.append(message)

    def error(self, message: str) -> None:
        if message not in self.errors:
            self.errors.append(message)


@dataclass
class PageContext:
    """The render state for one page: its own anchor counters and its link resolver."""

    site: Site
    doc: Doc
    counts: dict[str, int] = field(default_factory=dict)
    headings: list[tuple[int, str, str]] = field(default_factory=list)

    def anchor_for(self, text: str, level: int = 1) -> str:
        base = slugify(text) or "section"
        seen = self.counts.get(base, 0)
        self.counts[base] = seen + 1
        anchor = base if seen == 0 else f"{base}-{seen}"
        self.doc.anchors.add(anchor)
        if level in (2, 3):
            self.headings.append((level, strip_inline_markup(text), anchor))
        return anchor

    def _where(self, href: str) -> str:
        try:
            # Relative to the tree being built, not to the module's own repository: a finding a
            # reader has to expand an absolute temporary path to read is a finding they skip, and the
            # two spellings of a macOS temporary directory would make the same finding print two ways.
            rel = self.doc.src.relative_to(self.site.repo_root)
        except ValueError:
            rel = self.doc.src
        for number, line in enumerate(self.doc.lines, start=1):
            if href in line:
                return f"{rel}:{number}"
        return str(rel)

    def resolve(self, href: str, _context: str = "") -> str:
        """Rewrite one link destination, and fail the build if a docs-internal link lies."""
        from urllib.parse import quote, unquote

        raw = href.strip()
        if not raw:
            return raw
        if re.match(r"^[a-zA-Z][a-zA-Z0-9+.\-]*:", raw) or raw.startswith("//") or raw.startswith("/"):
            return raw
        path_part, sep, frag = raw.partition("#")
        target = unquote(path_part)

        if not path_part:
            if frag and frag not in self.doc.anchors:
                self.site.error(f"{self._where(raw)}: anchor #{frag} is not a section of {self.doc.src.name}")
            return raw

        try:
            resolved = (self.doc.src.parent / target).resolve()
        except OSError:
            return raw

        inside_docs = _is_within(resolved, self.site.docs_dir)
        markdown_target = target.endswith(".md") or (not resolved.suffix and resolved.with_suffix(".md").is_file())

        if markdown_target:
            wanted = resolved if target.endswith(".md") else resolved.with_suffix(".md")
            doc = self.site.by_src.get(wanted)
            if doc is None:
                if inside_docs:
                    self.site.error(f"{self._where(raw)}: {path_part} does not exist in docs/")
                    return raw
                return self._github_raw(raw, resolved, path_part, sep, frag)
            if frag and frag not in self.site.anchors.get(str(doc.out_rel), set()):
                self.site.error(
                    f"{self._where(raw)}: #{frag} is not a heading anchor of {path_part} "
                    f"(anchors look like: {', '.join(sorted(self.site.anchors.get(str(doc.out_rel), set()))[:3])})"
                )
            prefix = quote(str(_relative(doc.out_rel, self.doc.out_rel.parent)))
            return prefix + (sep + frag if frag else "")

        if inside_docs:
            if resolved.is_file():
                return quote(str(_relative(PurePosixPath(resolved.relative_to(self.site.docs_dir)),
                                           self.doc.out_rel.parent))) + (sep + frag if frag else "")
            self.site.error(f"{self._where(raw)}: {path_part} does not exist in docs/")
            return raw

        return self._github_raw(raw, resolved, path_part, sep, frag)

    def _github_raw(self, raw, resolved, path_part, sep, frag) -> str:
        if self.site.github_url:
            try:
                rel = resolved.relative_to(self.site.repo_root)
            except ValueError:
                self.site.warn(f"{self._where(raw)}: {path_part} is outside the repository; left as written")
                return raw
            from urllib.parse import quote

            return f"{self.site.github_url.rstrip('/')}/{quote(str(rel))}" + (sep + frag if frag else "")
        self.site.warn(f"{self._where(raw)}: {path_part} leaves docs/ and no --github-url was given")
        return raw


def _is_within(path: Path, parent: Path) -> bool:
    try:
        Path(path).relative_to(parent)
        return True
    except ValueError:
        return False


def _relative(target: PurePosixPath, from_dir: PurePosixPath) -> PurePosixPath:
    """`a/b.html` seen from the directory `x/y/` is `../../a/b.html`, without ever escaping upward."""
    target_parts = list(target.parts)
    base_parts = [p for p in from_dir.parts if p != "."]
    while base_parts and target_parts and base_parts[0] == target_parts[0]:
        base_parts.pop(0)
        target_parts.pop(0)
    return PurePosixPath(*([".."] * len(base_parts) + target_parts) or ["."])


# ---------------------------------------------------------------------------
# block-level parsing
#
# A line-based recursive-descent parser over the constructs docs/ uses. It is not a CommonMark
# implementation and does not pretend to be one; what it guarantees is narrower and is the thing
# that matters here: a document that renders to something other than what a reviewer read in the
# diff is a bug, and an unterminated fence is an error rather than a silent swallow of the rest of
# the file.
# ---------------------------------------------------------------------------


def collect_headings(lines: list[str]) -> list[tuple[int, str]]:
    """Headings in document order, fences respected (a `#` inside a code block is not a heading)."""
    found: list[tuple[int, str]] = []
    fence: str | None = None
    for line in lines:
        fence_match = RE_FENCE.match(line)
        if fence is None and fence_match:
            fence = fence_match.group(2)
            continue
        if fence is not None:
            if fence_match and fence_match.group(2)[0] == fence[0] and len(fence_match.group(2)) >= len(fence):
                fence = None
            continue
        heading = RE_HEADING.match(line)
        if heading:
            found.append((len(heading.group(2)), heading.group(3)))
    return found


def _split_row(line: str) -> list[str]:
    line = line.strip()
    if line.startswith("|"):
        line = line[1:]
    if line.endswith("|") and not line.endswith("\\|"):
        line = line[:-1]
    cells = re.split(r"(?<!\\)\|", line)
    return [cell.strip().replace("\\|", "|") for cell in cells]


def _parse_fence(lines: list[str], start: int) -> tuple[str, int]:
    match = RE_FENCE.match(lines[start])
    assert match
    indent, marker, info = len(match.group(1)), match.group(2), match.group(3)
    close = re.compile(r"^ {0,%d}%s{%d,}[ \t]*$" % (indent, re.escape(marker[0]), len(marker)))
    body: list[str] = []
    index = start + 1
    while index < len(lines) and not close.match(lines[index]):
        line = lines[index]
        body.append(line[indent:] if line[:indent].strip() == "" else line)
        index += 1
    if index >= len(lines):
        raise BuildError(f"code fence opened with {marker}{info} and never closed")
    language = info.split()[0] if info.split() else ""
    code = esc("\n".join(body))
    if language:
        return f'<pre class="code"><code class="language-{esc_attr(language)}">{code}</code></pre>', index + 1
    return f'<pre class="code"><code>{code}</code></pre>', index + 1


def _same_list_kind(marker: str, ordered: bool) -> bool:
    return ordered == (marker[-1] in ".)")


def _list_marker_kind(marker: str) -> tuple[bool, int]:
    if marker[-1] in ".)":
        return True, int(marker[:-1])
    return False, 1


def _is_block_start(line: str) -> bool:
    return bool(RE_FENCE.match(line) or RE_HEADING.match(line) or RE_HR.match(line)
                or RE_QUOTE.match(line) or RE_LIST_ITEM.match(line))


def _collect_list(lines: list[str], start: int) -> tuple[list[list[str]], bool, int, bool, int]:
    first = RE_LIST_ITEM.match(lines[start])
    assert first
    base_indent = len(first.group(1))
    ordered, start_number = _list_marker_kind(first.group(2))
    items: list[list[str]] = []
    current: list[str] | None = None
    content_indent = 0
    loose = False
    index = start
    while index < len(lines):
        line = lines[index]
        if RE_BLANK.match(line):
            probe = index
            while probe < len(lines) and RE_BLANK.match(lines[probe]):
                probe += 1
            if probe >= len(lines):
                index = probe
                break
            nxt = lines[probe]
            nxt_marker = RE_LIST_ITEM.match(nxt)
            nxt_indent = len(nxt) - len(nxt.lstrip())
            if nxt_marker and nxt_indent <= base_indent + 3 and _same_list_kind(nxt_marker.group(2), ordered):
                loose = True
                index = probe
                continue
            if current is not None and nxt_indent >= content_indent:
                loose = True
                current.append("")
                index = probe
                continue
            index = probe
            break
        marker = RE_LIST_ITEM.match(line)
        indent = len(line) - len(line.lstrip())
        # Indentation decides before the marker does: a marker at or past the current item's content
        # column is a *nested* list, and only a shallower one is a sibling. Getting this order wrong
        # turns every nested list in docs/ into a flat one.
        if current is not None and indent >= content_indent:
            current.append(line[content_indent:])
            index += 1
            continue
        if marker and indent <= base_indent + 3 and _same_list_kind(marker.group(2), ordered):
            content_indent = min(indent + len(marker.group(2)) + len(marker.group(3)), indent + 4)
            current = [marker.group(4)]
            items.append(current)
            index += 1
            continue
        if current is not None and not marker and not _is_block_start(line):
            current.append(line.strip())
            index += 1
            continue
        break
    return items, ordered, start_number, loose, index


def _unwrap_paragraph(parts: list[str]) -> list[str]:
    """In a tight list item the leading paragraph is not wrapped; `<p>` would add a margin the
    CommonMark rendering does not have, and a list item whose first block is a `<p>` reads as a
    loose list to every reader, including the CSS."""
    if parts and parts[0].startswith("<p>") and parts[0].endswith("</p>") and parts[0].count("</p>") == 1:
        return [parts[0][3:-4]] + parts[1:]
    return parts


def render_blocks(lines: list[str], ctx: PageContext) -> list[str]:
    parts: list[str] = []
    index = 0
    count = len(lines)
    while index < count:
        line = lines[index]
        if RE_BLANK.match(line):
            index += 1
            continue
        if RE_FENCE.match(line):
            block, index = _parse_fence(lines, index)
            parts.append(block)
            continue
        heading = RE_HEADING.match(line)
        if heading:
            level = len(heading.group(2))
            text = heading.group(3)
            anchor = ctx.anchor_for(text, level)
            parts.append(f'<h{level} id="{esc_attr(anchor)}">{render_inline(text, ctx)}</h{level}>')
            index += 1
            continue
        if RE_HR.match(line):
            parts.append("<hr>")
            index += 1
            continue
        if RE_QUOTE.match(line):
            quoted: list[str] = []
            while index < count:
                match = RE_QUOTE.match(lines[index])
                if match:
                    quoted.append(match.group(1))
                    index += 1
                elif not RE_BLANK.match(lines[index]) and quoted and not _is_block_start(lines[index]):
                    quoted.append(lines[index].strip())
                    index += 1
                else:
                    break
            parts.append("<blockquote>%s</blockquote>" % "".join(render_blocks(quoted, ctx)))
            continue
        if RE_LIST_ITEM.match(line):
            items, ordered, start_number, loose, index_after = _collect_list(lines, index)
            index = index_after
            tag = "ol" if ordered else "ul"
            body: list[str] = []
            for item in items:
                rendered = render_blocks(item, ctx)
                if not loose:
                    rendered = _unwrap_paragraph(rendered)
                body.append("<li>%s</li>" % "".join(rendered))
            start = f' start="{start_number}"' if ordered and start_number != 1 else ""
            parts.append(f"<{tag}{start}>{''.join(body)}</{tag}>")
            continue
        if "|" in line and index + 1 < count and "|" in lines[index + 1] and RE_TABLE_DELIM.match(lines[index + 1]):
            header = _split_row(line)
            aligns = []
            for cell in _split_row(lines[index + 1]):
                cell = cell.strip()
                if cell.startswith(":") and cell.endswith(":"):
                    aligns.append("center")
                elif cell.endswith(":"):
                    aligns.append("right")
                else:
                    aligns.append("")
            index += 2
            rows: list[list[str]] = []
            while index < count and not RE_BLANK.match(lines[index]) and "|" in lines[index]:
                rows.append(_split_row(lines[index]))
                index += 1
            head_cells = []
            for position, cell in enumerate(header):
                align = aligns[position] if position < len(aligns) else ""
                style = f' style="text-align:{align}"' if align else ""
                head_cells.append(f"<th{style}>{render_inline(cell, ctx)}</th>")
            row_html = []
            for row in rows:
                cells = []
                for position, cell in enumerate(row):
                    align = aligns[position] if position < len(aligns) else ""
                    style = f' style="text-align:{align}"' if align else ""
                    cells.append(f"<td{style}>{render_inline(cell, ctx)}</td>")
                row_html.append("<tr>%s</tr>" % "".join(cells))
            parts.append("<table><thead><tr>%s</tr></thead><tbody>%s</tbody></table>"
                         % ("".join(head_cells), "".join(row_html)))
            continue
        paragraph: list[str] = []
        while index < count:
            candidate = lines[index]
            if RE_BLANK.match(candidate):
                break
            if paragraph and _starts_new_block(candidate):
                break
            if not paragraph and RE_TABLE_DELIM.match(candidate):
                break
            if len(paragraph) == 0 and "|" in candidate and index + 1 < count and "|" in lines[index + 1] \
                    and RE_TABLE_DELIM.match(lines[index + 1]):
                break
            paragraph.append(candidate)
            index += 1
        parts.append("<p>%s</p>" % render_inline("\n".join(paragraph), ctx))
    return parts


def _starts_new_block(line: str) -> bool:
    """A paragraph ends where a block-level construct begins; setext is deliberately not one."""
    return bool(RE_FENCE.match(line) or RE_HEADING.match(line) or RE_HR.match(line) or RE_QUOTE.match(line)
                or RE_LIST_ITEM.match(line))


# ---------------------------------------------------------------------------
# the page chrome
#
# One stylesheet, one template, three columns that collapse to one. Nothing here is loaded from
# anywhere else: no font, no script, no CDN. A docs site that phones home to a third party is a
# privacy statement nobody asked it to make, and one that needs JavaScript to render a table of
# contents is a site that breaks in the browser of whoever is reading it on a locked-down laptop.
# ---------------------------------------------------------------------------

CSS = """
:root {
  --bg: #ffffff; --ink: #1f2328; --faint: #57606a; --line: #d8dee4; --band: #f6f8fa;
  --code-bg: #f6f8fa; --link: #0b5cad; --accent: #0b5cad;
  --mono: ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, monospace;
}
@media (prefers-color-scheme: dark) {
  :root {
    --bg: #0d1117; --ink: #e6edf3; --faint: #9198a1; --line: #30363d; --band: #161b22;
    --code-bg: #161b22; --link: #6cb6ff; --accent: #6cb6ff;
  }
}
* { box-sizing: border-box; }
html { -webkit-text-size-adjust: 100%; }
body {
  margin: 0; background: var(--bg); color: var(--ink);
  font: 16px/1.65 -apple-system, BlinkMacSystemFont, "Segoe UI", Helvetica, Arial, sans-serif;
}
.skip { position: absolute; left: -999px; }
.skip:focus { left: 0.5rem; top: 0.5rem; background: var(--band); padding: .5rem 1rem; z-index: 9; }
.masthead {
  display: flex; align-items: baseline; gap: 1rem; flex-wrap: wrap;
  padding: .85rem 1.25rem; border-bottom: 1px solid var(--line); background: var(--band);
}
.masthead .brand { font-weight: 700; letter-spacing: .01em; font-size: 1.05rem; }
.masthead-nav { margin-left: auto; display: flex; gap: 1rem; align-items: baseline; }
.masthead-note { color: var(--faint); font-size: .8rem; }
a { color: var(--link); text-decoration: none; }
a:hover { text-decoration: underline; }
.sheet {
  display: grid; grid-template-columns: 17rem minmax(0, 1fr) 16rem; gap: 2.5rem;
  max-width: 82rem; margin: 0 auto; padding: 1.5rem 1.5rem 4rem;
}
.rail { font-size: .86rem; line-height: 1.45; }
.rail .group { margin-bottom: 1.4rem; }
.rail .group > h2 {
  font-size: .72rem; text-transform: uppercase; letter-spacing: .08em; color: var(--faint);
  margin: 0 0 .4rem; font-weight: 700;
}
.rail ul { list-style: none; margin: 0; padding: 0; }
.rail li { margin: 0 0 .15rem; }
.rail a { display: block; padding: .2rem .45rem; border-radius: 6px; color: var(--ink); }
.rail a:hover { background: var(--band); }
.rail li.here > a { background: var(--band); font-weight: 600; box-shadow: inset 2px 0 0 var(--accent); }
main { min-width: 0; }
article { max-width: 46rem; }
article > h1:first-child { margin-top: 0; }
h1, h2, h3, h4 { line-height: 1.25; font-weight: 700; margin: 2rem 0 .6rem; scroll-margin-top: 1rem; }
h1 { font-size: 1.7rem; }
h2 { font-size: 1.28rem; border-bottom: 1px solid var(--line); padding-bottom: .3rem; }
h3 { font-size: 1.06rem; }
h4 { font-size: .98rem; }
p, ul, ol, table, blockquote { margin: 0 0 1rem; }
ul, ol { padding-left: 1.4rem; }
li { margin: 0 0 .35rem; }
li > ul, li > ol { margin: .35rem 0 0; }
code {
  font: .87em var(--mono); background: var(--code-bg); padding: .12em .35em;
  border-radius: 4px; border: 1px solid var(--line);
}
pre.code {
  background: var(--code-bg); border: 1px solid var(--line); border-radius: 8px;
  padding: .85rem 1rem; overflow-x: auto; margin: 0 0 1rem; line-height: 1.5;
}
pre.code code { background: none; border: 0; padding: 0; font-size: .84rem; white-space: pre; }
blockquote {
  border-left: 4px solid var(--line); padding: .1rem 0 .1rem 1rem; color: var(--faint);
}
blockquote > :last-child { margin-bottom: 0; }
hr { border: 0; border-top: 1px solid var(--line); margin: 2rem 0; }
table { border-collapse: collapse; display: block; overflow-x: auto; max-width: 100%; font-size: .92rem; }
th, td { border: 1px solid var(--line); padding: .45rem .7rem; vertical-align: top; text-align: left; }
th { background: var(--band); font-weight: 700; }
tbody tr:nth-child(even) { background: color-mix(in srgb, var(--band) 55%, transparent); }
img { max-width: 100%; }
.onside { font-size: .82rem; }
.onside h2 {
  font-size: .72rem; text-transform: uppercase; letter-spacing: .08em; color: var(--faint);
  border: 0; margin: 0 0 .4rem; padding: 0;
}
.onside ul { list-style: none; margin: 0; padding: 0; }
.onside li { margin: 0; }
.onside a { display: block; padding: .15rem 0; color: var(--faint); }
.onside a:hover { color: var(--link); }
.onside .l2 { font-weight: 600; }
.onside .l3 { padding-left: .9rem; }
.badges { display: flex; gap: .4rem; flex-wrap: wrap; margin: 0 0 1rem; }
.badge {
  font: 600 .72rem/1.6 var(--mono); text-transform: uppercase; letter-spacing: .04em;
  border: 1px solid var(--line); border-radius: 999px; padding: .05rem .55rem; color: var(--faint);
}
footer.page-foot { margin-top: 3rem; padding-top: 1rem; border-top: 1px solid var(--line);
  color: var(--faint); font-size: .8rem; }
.cards { display: grid; gap: 1rem; margin: 0 0 2rem; padding: 0; list-style: none; }
.cards li {
  border: 1px solid var(--line); border-radius: 10px; padding: .9rem 1.1rem; background: var(--band);
}
.cards h3 { margin: 0 0 .35rem; font-size: 1.02rem; }
.cards p { margin: 0; color: var(--faint); font-size: .88rem; }
.lede { font-size: 1.05rem; color: var(--faint); max-width: 46rem; }
@media (max-width: 1080px) {
  .sheet { grid-template-columns: 15rem minmax(0, 1fr); }
  .onside { display: none; }
}
@media (max-width: 760px) {
  .sheet { grid-template-columns: minmax(0, 1fr); gap: 1.5rem; }
  .rail { border-bottom: 1px solid var(--line); padding-bottom: 1rem; }
}
"""

PAGE = """<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="generator" content="scripts/build_docs_site.py">
<title>{title} &middot; {site_title}</title>
<link rel="stylesheet" href="{css_href}">
</head>
<body>
<a class="skip" href="#main">Skip to content</a>
<header class="masthead">
<a class="brand" href="{root}index.html">{site_title}</a>
<nav class="masthead-nav">{repo_link}{note}</nav>
</header>
<div class="sheet">
<nav class="rail" aria-label="Documentation">
{nav}
</nav>
<main id="main">
<article>
{badges}{article}
</article>
<footer class="page-foot">{footer}</footer>
</main>
<aside class="onside" aria-label="On this page">
{toc}
</aside>
</div>
</body>
</html>
"""


def _nav_html(site: "Site", current: PurePosixPath, prefix: str) -> str:
    groups: dict[str, list[Doc]] = {}
    for doc in site.docs:
        parent = str(doc.out_rel.parent)
        groups.setdefault(parent if parent != "." else "", []).append(doc)

    def label_for(directory: str) -> str:
        if directory == "":
            return "Guides"
        if directory == "adr":
            return "Architecture decisions"
        return directory.replace("-", " ").title()

    order = sorted(groups, key=lambda key: (key != "", key))
    out = []
    for directory in order:
        out.append(f'<div class="group"><h2>{esc(label_for(directory))}</h2><ul>')
        for doc in groups[directory]:
            here = ' class="here"' if doc.out_rel == current else ""
            out.append('<li%s><a href="%s">%s</a></li>'
                       % (here, esc_attr(prefix + quote_posix(doc.out_rel)), esc(_rail_label(doc))))
        out.append("</ul></div>")
    return "\n".join(out)


def _rail_label(doc: Doc) -> str:
    """The sidebar wants something short. An ADR gets its `id` plus the first clause of its title,
    whether the title came from front matter or from the document's own H1."""
    title = doc.meta.get("title") or doc.title
    if doc.meta.get("id"):
        rest = re.sub(r"^ADR-\d+\s*:?[\s-]*", "", title, flags=re.I)
        rest = re.split(r"\s+[—-]\s+", rest, maxsplit=1)[0]
        return f"{doc.meta['id']} · {rest}".strip() or doc.meta["id"]
    clause = re.split(r"\s+—\s+", title, maxsplit=1)[0]
    if len(clause) >= 12:
        return clause
    return title if len(title) <= 58 else title[:57].rstrip() + "…"


def quote_posix(path: PurePosixPath) -> str:
    from urllib.parse import quote

    return quote(str(path))


def _toc_html(headings: list[tuple[int, str, str]]) -> str:
    if not headings:
        return ""
    out = ['<h2>On this page</h2><ul>']
    for level, text, anchor in headings:
        out.append(f'<li class="l{level}"><a href="#{esc_attr(anchor)}">{esc(text)}</a></li>')
    out.append("</ul>")
    return "".join(out)


# ---------------------------------------------------------------------------
# site discovery and build
# ---------------------------------------------------------------------------


def anchors_of(lines: list[str]) -> set[str]:
    """The anchors a page will have — computed the same way the renderer computes them, because a
    cross-page `#section` check that disagrees with the renderer is a broken-link report that lies."""
    counts: dict[str, int] = {}
    anchors: set[str] = set()
    for _level, text in collect_headings(lines):
        base = slugify(text) or "section"
        seen = counts.get(base, 0)
        counts[base] = seen + 1
        anchors.add(base if seen == 0 else f"{base}-{seen}")
    return anchors


def _summary_of(lines: list[str]) -> str:
    paragraph: list[str] = []
    index = 0
    while index < len(lines):
        line = lines[index]
        if RE_BLANK.match(line):
            if paragraph:
                break
            index += 1
            continue
        if _starts_new_block(line) or "|" in line:
            if paragraph:
                break
            index += 1
            continue
        paragraph.append(line)
        index += 1
    text = strip_inline_markup(" ".join(paragraph))
    text = re.sub(r"\s+", " ", text).strip()
    return (text[:220] + "…") if len(text) > 221 else text


def discover_markdown(docs_dir: Path) -> list[Path]:
    found = []
    for path in sorted(docs_dir.rglob("*.md")):
        if any(part.startswith(".") for part in path.relative_to(docs_dir).parts):
            continue
        if path.is_symlink() and not path.exists():
            continue
        found.append(path)
    return found


def build_site(
    docs_dir: Path,
    out_dir: Path,
    site_title: str = "act-mcp",
    github_url: str | None = None,
    repo_url: str | None = None,
    footer_note: str = "",
    lede: str = "",
    repo_root: Path | None = None,
    clean: bool = False,
    strict: bool = False,
    quiet: bool = False,
) -> tuple[int, list[str], list[str]]:
    """Build the site. Returns (pages written, errors, warnings); errors also raise nothing — the
    caller decides whether a red build is fatal, because --check wants to report and carry on."""
    docs_dir = docs_dir.resolve()
    out_dir = out_dir.resolve()
    # Canonicalise the repository root too, or the two halves of every comparison disagree by
    # construction. A link's target is resolved (`(doc.src.parent / target).resolve()`), so a root that
    # arrives as the *other* spelling of the same directory makes `relative_to` raise ValueError, and
    # ValueError is read as "this link leaves the repository". That is not hypothetical: on macOS
    # `$TMPDIR` lives under `/var`, and `/var` is a symlink to `/private/var`, so `tempfile`'s own path
    # and its resolved form are two names for one place — which is why `--check` was green on Linux and
    # red on a Mac (docs/testing.md §1 states the hazard for the Rust tests; this is the builder's half
    # of the same rule). One spelling in, one spelling everywhere.
    repo_root = (repo_root or REPO).resolve()
    site = Site(docs_dir=docs_dir, out_dir=out_dir, repo_root=repo_root, github_url=github_url)

    sources = discover_markdown(docs_dir)
    if not sources:
        site.error(f"no Markdown found under {docs_dir}")
        return 0, site.errors, site.warnings

    # pass 1: what exists, what it is called, and what it anchors — every cross-page link needs this
    # table before a single link can be checked.
    for path in sources:
        try:
            text = path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError) as exc:
            site.error(f"{path}: cannot be read as UTF-8 text ({exc})")
            continue
        try:
            meta, body = split_front_matter(text.splitlines())
        except BuildError as exc:
            site.error(f"{path}: {exc}")
            continue
        out_rel = PurePosixPath(str(path.relative_to(docs_dir))[:-3] + ".html")
        headings = collect_headings(body)
        h1 = next((strip_inline_markup(t) for level, t in headings if level == 1), "")
        title = h1 or meta.get("title") or path.stem
        doc = Doc(src=path.resolve(), out_rel=out_rel, title=title, meta=meta, lines=body,
                  anchors=anchors_of(body), summary=_summary_of(body))
        site.docs.append(doc)
        site.by_src[doc.src] = doc
        site.anchors[str(out_rel)] = doc.anchors
    site.docs.sort(key=lambda doc: str(doc.out_rel))

    if site.errors:
        return 0, site.errors, site.warnings

    if clean and out_dir.exists():
        if out_dir == docs_dir or out_dir == site.repo_root or docs_dir in out_dir.parents:
            site.error(f"refusing to --clean {out_dir}: it is not a build output directory")
            return 0, site.errors, site.warnings
        shutil.rmtree(out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)

    written = 0
    for doc in site.docs:
        ctx = PageContext(site=site, doc=doc)
        try:
            body_html = "".join(render_blocks(doc.lines, ctx))
        except BuildError as exc:
            site.error(f"{doc.src}: {exc}")
            continue
        prefix = _prefix_for(doc.out_rel)
        badges = _badges_html(doc.meta)
        footer = footer_note or "Generated from <code>docs/</code> by <code>scripts/build_docs_site.py</code>."
        page = PAGE.format(
            title=esc(doc.title),
            site_title=esc(site_title),
            css_href=prefix + "assets/docs.css",
            root=prefix,
            repo_link=(f'<a href="{esc_attr(repo_url)}">repository</a>' if repo_url else ""),
            note=f'<span class="masthead-note">{esc(footer_note)}</span>' if footer_note else "",
            nav=_nav_html(site, doc.out_rel, prefix),
            badges=badges,
            article=body_html,
            toc=_toc_html(ctx.headings),
            footer=footer,
        )
        _write(out_dir / str(doc.out_rel), page)
        written += 1

    index = _index_page(site, site_title, repo_url, footer_note, lede)
    if index is not None:
        _write(out_dir / "index.html", index)
        written += 1
    _write(out_dir / "404.html", _not_found_page(site, site_title, repo_url, footer_note, lede))
    written += 1
    _write(out_dir / "assets" / "docs.css", CSS)
    _write(out_dir / ".nojekyll", "")
    written += 1

    for path in sorted(docs_dir.rglob("*")):
        if path.is_dir() or path.suffix.lower() == ".md":
            continue
        if any(part.startswith(".") for part in path.relative_to(docs_dir).parts):
            continue
        relative = path.relative_to(docs_dir)
        target = out_dir / str(relative)
        if target.suffix.lower() == ".html":
            site.warn(f"{relative}: a .html file in docs/ would be overwritten by a generated page")
            continue
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(path, target)
        written += 1

    if strict and site.warnings:
        for warning in site.warnings:
            site.error(f"--strict: {warning}")

    if not quiet:
        print(f"built {written} file(s) into {out_dir} from {len(site.docs)} page(s)")
    return written, site.errors, site.warnings


def _prefix_for(out_rel: PurePosixPath) -> str:
    depth = len([part for part in out_rel.parts[:-1] if part != "."])
    return "../" * depth


def _badges_html(meta: dict[str, str]) -> str:
    keys = [key for key in ("id", "status", "date") if meta.get(key)]
    if not keys:
        return ""
    return '<div class="badges">%s</div>' % "".join(
        f'<span class="badge">{esc(meta[key])}</span>' for key in keys)


def _cards(docs: list[Doc], prefix: str) -> str:
    out = ['<ul class="cards">']
    for doc in docs:
        out.append("<li><h3><a href=\"%s\">%s</a></h3>%s<p>%s</p></li>"
                   % (esc_attr(prefix + quote_posix(doc.out_rel)), esc(doc.title),
                      _badges_html(doc.meta), esc(doc.summary) or "<em>no summary</em>"))
    out.append("</ul>")
    return "".join(out)


def _index_page(site: "Site", site_title: str, repo_url: str | None, footer_note: str, lede: str) -> str:
    guides = [doc for doc in site.docs
              if str(doc.out_rel.parent) == "." and doc.out_rel != PurePosixPath("index.html")]
    adrs = [doc for doc in site.docs if str(doc.out_rel.parent) == "adr"]
    other = [doc for doc in site.docs if doc not in guides and doc not in adrs]
    article = [f"<h1>{esc(site_title)} documentation</h1>"]
    article.append('<p class="lede">%s</p>' % esc(lede or
                   "The reasoning this repository runs on: the decision records, the contracts, "
                   "the CI map. Written in docs/, published as static HTML."))
    if guides:
        article.append("<h2>Guides</h2>")
        article.append(_cards(guides, ""))
    if adrs:
        article.append("<h2>Architecture decision records</h2>")
        article.append(_cards(adrs, ""))
    if other:
        article.append("<h2>More</h2>")
        article.append(_cards(other, ""))
    return PAGE.format(
        title=f"{site_title} documentation",
        site_title=esc(site_title),
        css_href="assets/docs.css",
        root="",
        repo_link=(f'<a href="{esc_attr(repo_url)}">repository</a>' if repo_url else ""),
        note=f'<span class="masthead-note">{esc(footer_note)}</span>' if footer_note else "",
        nav=_nav_html(site, PurePosixPath("index.html"), ""),
        badges="",
        article="".join(article),
        toc="",
        footer=footer_note or "Generated from <code>docs/</code> by <code>scripts/build_docs_site.py</code>.",
    )


def _not_found_page(site: "Site", site_title: str, repo_url: str | None, footer_note: str, lede: str) -> str:
    article = ("<h1>Page not found</h1><p class=\"lede\">That path is not part of the published "
               "docs. The index lists everything that is.</p><p><a href=\"index.html\">Back to the "
               "documentation index</a></p>")
    return PAGE.format(
        title=f"not found · {site_title}",
        site_title=esc(site_title),
        css_href="assets/docs.css",
        root="",
        repo_link=(f'<a href="{esc_attr(repo_url)}">repository</a>' if repo_url else ""),
        note=f'<span class="masthead-note">{esc(footer_note)}</span>' if footer_note else "",
        nav=_nav_html(site, PurePosixPath("404.html"), ""),
        badges="",
        article=article,
        toc="",
        footer=footer_note or "Generated from <code>docs/</code> by <code>scripts/build_docs_site.py</code>.",
    )


def _write(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with open(path, "w", encoding="utf-8", newline="\n") as handle:
        handle.write(content if content.endswith("\n") or content == "" else content + "\n")


# ---------------------------------------------------------------------------
# command line
# ---------------------------------------------------------------------------


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="scripts/build_docs_site.py",
        description=__doc__.splitlines()[0],
        epilog=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("--docs-dir", default=str(REPO / "docs"), help="Markdown tree to publish (default: docs/)")
    parser.add_argument("--out", default=str(REPO / "target" / "docs-site"),
                       help="output directory (default: target/docs-site, which is gitignored)")
    parser.add_argument("--title", default="act-mcp", help="site title shown in the masthead and <title>")
    parser.add_argument("--github-url", default=None,
                       help="blob base URL, e.g. https://github.com/owner/repo/blob/main; links that "
                            "leave docs/ are rewritten through it instead of left broken")
    parser.add_argument("--repo-url", default=None, help="repository URL for the masthead link")
    parser.add_argument("--footer-note", default="", help="one line added to every page footer and masthead")
    parser.add_argument("--lede", default="", help="the sentence under the index page title")
    parser.add_argument("--check", action="store_true", help="run the builder's own tests and build nothing")
    parser.add_argument("--clean", action="store_true", help="remove the output directory first")
    parser.add_argument("--strict", action="store_true", help="treat warnings as errors")
    parser.add_argument("-W", "--max-warnings", type=int, default=20, metavar="N",
                       help="print at most N warnings, then a count of the rest (0 to print none; default 20)")
    parser.add_argument("-q", "--quiet", action="store_true", help="only the findings, not the summary line")
    args = parser.parse_args(argv)

    if args.check:
        return run_self_test()

    docs_dir = Path(args.docs_dir)
    if not docs_dir.is_dir():
        print(f"no such docs directory: {docs_dir}", file=sys.stderr)
        return 2
    _written, errors, warnings = build_site(
        docs_dir=docs_dir,
        out_dir=Path(args.out),
        site_title=args.title,
        github_url=args.github_url,
        repo_url=args.repo_url,
        footer_note=args.footer_note,
        lede=args.lede,
        clean=args.clean,
        strict=args.strict,
        quiet=args.quiet,
    )
    if args.max_warnings == 0:
        # Silence is a decision with a cost, so the count survives even when the list does not.
        if warnings:
            print(f"warning: {len(warnings)} warning(s) suppressed by -W 0", file=sys.stderr)
    else:
        for index, warning in enumerate(warnings):
            if index >= args.max_warnings:
                print(f"warning: ... and {len(warnings) - index} more (the links that leave docs/; "
                      f"--github-url rewrites them)", file=sys.stderr)
                break
            print(f"warning: {warning}", file=sys.stderr)
    for error in errors:
        print(f"error: {error}", file=sys.stderr)
    return 1 if errors else 0


# ---------------------------------------------------------------------------
# self-test
#
# `--check` is the step `scripts/ci.sh docs-site` runs, so it has to prove the same things the other
# `--check` flags in this directory prove: that the parser renders what it claims, that the link
# checker fails when the thing it checks is false, and that a build is reproducible. The fixture
# tree is written into a temporary directory so the test does not depend on docs/ existing or on
# having the network, and it is small enough to read here.
# ---------------------------------------------------------------------------


def run_self_test() -> int:
    fixture = _SelfTest()
    return fixture.run()


class _SelfTest:
    def __init__(self) -> None:
        self.failures: list[str] = []
        self.passes = 0

    def check(self, name: str, condition: bool, detail: str = "") -> None:
        if condition:
            self.passes += 1
        else:
            self.failures.append(f"{name}: {detail or 'expected true'}")

    def equal(self, name: str, got, want) -> None:
        if got == want:
            self.passes += 1
        else:
            self.failures.append(f"{name}:\n    got  {got!r}\n    want {want!r}")

    def run(self) -> int:
        self.slug_cases()
        self.inline_cases()
        self.block_cases()
        self.build_cases()
        self.link_cases()
        self.build_error_cases()
        self.path_spelling_cases()
        for failure in self.failures:
            print(f"FAIL {failure}", file=sys.stderr)
        print(f"{'FAIL' if self.failures else 'OK'} docs site builder: "
              f"{self.passes} check(s) passed, {len(self.failures)} failure(s)")
        return 1 if self.failures else 0

    # -- anchors ---------------------------------------------------------------
    def slug_cases(self) -> None:
        # Every one of these is an anchor that already appears in docs/, which is the only reason
        # the odd trailing and doubled hyphens are correct rather than sloppy.
        cases = {
            "plain heading": "plain-heading",
            "3. Coverage %": "3-coverage-",
            "5. Action Success — the durable question": "5-action-success--the-durable-question",
            "7.2 The keys": "72-the-keys",
            "6. Glyphs, caveats, and gaps": "6-glyphs-caveats-and-gaps",
            "`fathom report` output": "fathom-report-output",
            "**Bold** heading": "bold-heading",
            "a `snake_case` name": "a-snake_case-name",
            "§three": "three",
            "MiXeD Case Heading": "mixed-case-heading",
        }
        for text, want in cases.items():
            self.equal(f"slugify({text!r})", slugify(text), want)

    # -- inline ----------------------------------------------------------------
    def inline_cases(self) -> None:
        doc = Doc(src=Path("/x/docs/a.md"), out_rel=PurePosixPath("a.html"), title="a", meta={}, lines=[])
        other = Doc(src=Path("/x/docs/other.md"), out_rel=PurePosixPath("other.html"), title="other",
                    meta={}, lines=[], anchors={"sec"})
        site = Site(docs_dir=Path("/x/docs"), out_dir=Path("/x/out"), repo_root=Path("/x"))
        for page in (doc, other):
            site.by_src[page.src] = page
            site.docs.append(page)
            site.anchors[str(page.out_rel)] = page.anchors
        doc.anchors.add("sec")
        ctx = PageContext(site=site, doc=doc)
        render = lambda text: render_inline(text, ctx)  # noqa: E731
        self.equal("escape raw html", render("a <b>tag</b> & an &"), "a &lt;b&gt;tag&lt;/b&gt; &amp; an &amp;")
        self.equal("code span wins over markup", render("`a **b** <c>`"), "<code>a **b** &lt;c&gt;</code>")
        self.equal("code span trims one space", render("` a `"), "<code>a</code>")
        self.equal("double backtick span", render("``a`b``"), "<code>a`b</code>")
        self.equal("strong", render("**bold**"), "<strong>bold</strong>")
        self.equal("em", render("a *word* b"), "a <em>word</em> b")
        self.equal("underscore emphasis is not intra-word", render("ACTMCP_EVIDENCE_OUT"), "ACTMCP_EVIDENCE_OUT")
        self.equal("underscore emphasis between words", render("a _word_ b"), "a <em>word</em> b")
        self.equal("strike", render("~~gone~~"), "<del>gone</del>")
        self.equal("nested emphasis", render("**a *b* c**"), "<strong>a <em>b</em> c</strong>")
        self.equal("star math is not emphasis", render("5.2 * 3 / 4"), "5.2 * 3 / 4")
        self.equal("backslash escape", render(r"\*not em\*"), "*not em*")
        self.equal("autolink", render("<https://example.com/x>"),
                   '<a href="https://example.com/x">https://example.com/x</a>')
        self.equal("inline link text", render("[label](other.md)"), '<a href="other.html">label</a>')
        self.equal("link with code label", render("[`code`](other.md)"),
                   '<a href="other.html"><code>code</code></a>')
        self.equal("link keeps its title", render('[x](other.md "t")'), '<a href="other.html" title="t">x</a>')
        self.equal("emphasis spans a code span", render("**before `code` after**"),
                   "<strong>before <code>code</code> after</strong>")
        self.equal("emphasis spans a link", render("**before [label](other.md) after**"),
                   '<strong>before <a href="other.html">label</a> after</strong>')
        self.equal("code inside a link label", render("[`code` here](other.md)"),
                   '<a href="other.html"><code>code</code> here</a>')
        self.equal("external untouched", render("[x](https://example.com/a.md)"),
                   '<a href="https://example.com/a.md">x</a>')
        self.equal("anchor only", render("[x](#sec)"), '<a href="#sec">x</a>')
        self.equal("image", render("![alt](pic.png)"), '<img src="pic.png" alt="alt">')

    # -- blocks ----------------------------------------------------------------
    def block_cases(self) -> None:
        doc = Doc(src=Path("/x/docs/a.md"), out_rel=PurePosixPath("a.html"), title="a", meta={}, lines=[])
        site = Site(docs_dir=Path("/x/docs"), out_dir=Path("/x/out"), repo_root=Path("/x"))
        site.by_src[doc.src] = doc
        site.docs.append(doc)
        ctx = PageContext(site=site, doc=doc)

        def blocks(text: str) -> str:
            return "".join(render_blocks(text.splitlines(), ctx))

        self.equal("heading anchor", blocks("# Title\n\n## Section Name"),
                   '<h1 id="title">Title</h1><h2 id="section-name">Section Name</h2>')
        self.equal("duplicate heading anchors get numbered", blocks("## Same\n\n## Same"),
                   '<h2 id="same">Same</h2><h2 id="same-1">Same</h2>')
        self.equal("fence keeps language and escapes",
                   blocks("```sh\ncd <dir> && make\n```"),
                   '<pre class="code"><code class="language-sh">cd &lt;dir&gt; &amp;&amp; make</code></pre>')
        self.equal("fence without language", blocks("```\nplain\n```"),
                   '<pre class="code"><code>plain</code></pre>')
        self.equal("heading inside a fence is not a heading", blocks("```\n# not a heading\n```"),
                   '<pre class="code"><code># not a heading</code></pre>')
        self.equal("table with alignment",
                   blocks("| a | b: |\n|---|---:|\n| 1 | 2 |"),
                   '<table><thead><tr><th>a</th><th style="text-align:right">b:</th></tr></thead>'
                   '<tbody><tr><td>1</td><td style="text-align:right">2</td></tr></tbody></table>')
        self.equal("tight list unwraps paragraphs", blocks("- one\n- two"),
                   "<ul><li>one</li><li>two</li></ul>")
        self.equal("nested list", blocks("- one\n  - deep\n    deeper\n- two"),
                   "<ul><li>one<ul><li>deep\ndeeper</li></ul></li><li>two</li></ul>")
        self.equal("ordered list keeps its start", blocks("3. three\n4. four"),
                   '<ol start="3"><li>three</li><li>four</li></ol>')
        self.equal("loose list keeps paragraphs", blocks("- one\n\n- two"),
                   "<ul><li><p>one</p></li><li><p>two</p></li></ul>")
        self.equal("list item with a fence", blocks("- run:\n\n  ```sh\n  make\n  ```\n"),
                   "<ul><li><p>run:</p><pre class=\"code\"><code class=\"language-sh\">make</code></pre></li></ul>")
        self.equal("blockquote nests", blocks("> **a**\n> b\n>\n> - x"),
                   "<blockquote><p><strong>a</strong>\nb</p><ul><li>x</li></ul></blockquote>")
        self.equal("thematic break", blocks("a\n\n---\n\nb"), "<p>a</p><hr><p>b</p>")
        self.equal("paragraph joins lines", blocks("one\ntwo"), "<p>one\ntwo</p>")
        matter = split_front_matter(["---", "id: ADR-0001", "status: accepted", "---", "# t"])
        self.equal("front matter splits", matter, ({"id": "ADR-0001", "status": "accepted"}, ["# t"]))
        self.equal("front matter absent", split_front_matter(["# t"]), ({}, ["# t"]))

    # -- a whole site ----------------------------------------------------------
    FIXTURE = {
        "docs/guide.md": """# Guide title

Intro paragraph pointing at [the page](sub/page.md#deep-section) and at [a section](#more-sections).

## More sections

```sh
echo "hi"
```

| a | b |
|---|---:|
| 1 | [deep](sub/page.md#deep-section) |

- one
- two

  nested under two:

    ```sh
    make
    ```

## Another section

See [the ADR](adr/0001-one.md#the-decision) and [outside](../outside.md) and [the asset](asset.txt).
""",
        "docs/sub/page.md": """# A page in a directory

## Deep section

Text with a <script>alert(1)</script> that must never survive.

[back to the guide](../guide.md#more-sections)
""",
        "docs/adr/0001-one.md": """---
id: ADR-0001
status: accepted
date: 2026-11-20
---

# ADR-0001: the one

## The decision

Decided.
""",
        "docs/asset.txt": "not markdown, copied verbatim\n",
        "outside.md": "# outside\n",
    }

    def _write_fixture(self, root: Path, overrides: dict[str, str] | None = None) -> Path:
        files = dict(self.FIXTURE)
        files.update(overrides or {})
        for relative, content in files.items():
            path = root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(content, encoding="utf-8")
        return root

    def build_cases(self) -> None:
        with tempfile.TemporaryDirectory(prefix="docs-site-check-") as tmp:
            root = self._write_fixture(Path(tmp))
            out = root / "out"
            written, errors, warnings = build_site(
                docs_dir=root / "docs", out_dir=out, github_url="https://example.com/o/r/blob/main",
                repo_url="https://example.com/o/r", repo_root=root, quiet=True)
            self.equal("fixture builds clean", errors, [])
            self.equal("no warnings on the good fixture", warnings, [])
            self.check("wrote something", written > 0, f"written={written}")
            for relative in ("index.html", "404.html", "guide.html", "sub/page.html",
                             "adr/0001-one.html", "assets/docs.css", ".nojekyll", "asset.txt"):
                self.check(f"{relative} exists", (out / relative).is_file(), f"missing {relative}")
            guide = (out / "guide.html").read_text(encoding="utf-8")
            self.check("relative .md link became .html", 'href="sub/page.html#deep-section"' in guide,
                       guide[max(0, guide.find("the page") - 60): guide.find("the page") + 40])
            self.check("asset link kept and rewritten", 'href="asset.txt"' in guide, "")
            self.check("outside link rewritten to the blob url",
                       'href="https://example.com/o/r/blob/main/outside.md"' in guide, "")
            self.check("page title from the h1", "<title>Guide title &middot; act-mcp</title>" in guide, "")
            self.check("sidebar marks the current page", 'class="here"' in guide, "")
            self.check("toc lists h2s", '#more-sections' in guide and "Another section" in guide, "")
            sub = (out / "sub" / "page.html").read_text(encoding="utf-8")
            self.check("script tag is escaped", "<script>alert(1)</script>" not in sub, "")
            self.check("css path is relative to depth",
                       '<link rel="stylesheet" href="../assets/docs.css">' in sub, "")
            adr = (out / "adr" / "0001-one.html").read_text(encoding="utf-8")
            self.check("front matter renders badges", "ADR-0001" in adr and "accepted" in adr, "")
            index = (out / "index.html").read_text(encoding="utf-8")
            for label in ("Guide title", "A page in a directory", "ADR-0001"):
                self.check(f"index lists {label}", label in index, "")
            self.check("index carries a summary", "Intro paragraph pointing at" in index, "")
            # determinism: two builds of one tree are byte-identical, which is what makes
            # "did this change alter the site?" a diff rather than a judgement call.
            out2 = root / "out2"
            build_site(docs_dir=root / "docs", out_dir=out2, github_url="https://example.com/o/r/blob/main",
                       repo_url="https://example.com/o/r", repo_root=root, quiet=True)
            self.equal("build is deterministic", _tree_bytes(out), _tree_bytes(out2))

    # -- link checking ---------------------------------------------------------
    def link_cases(self) -> None:
        bad = {
            "docs/broken-file.md": "See [nowhere](does-not-exist.md).\n",
            "docs/broken-anchor.md": "See [nowhere near](guide.md#not-a-heading).\n",
            "docs/broken-self.md": "See [itself](#no-such-anchor).\n",
        }
        with tempfile.TemporaryDirectory(prefix="docs-site-check-") as tmp:
            root = self._write_fixture(Path(tmp), bad)
            _written, errors, warnings = build_site(docs_dir=root / "docs", out_dir=root / "out",
                                                   repo_root=root, quiet=True)
            missing = [error for error in errors if "does-not-exist" in error]
            self.equal("a docs-internal link to a missing file is an error", len(missing), 1)
            self.equal("a cross-page anchor that is not a heading is an error",
                       len([e for e in errors if "not-a-heading" in e]), 1)
            self.equal("an in-page anchor that is not a heading is an error",
                       len([e for e in errors if "no-such-anchor" in e]), 1)
            self.check("the error names the file and line",
                       any(re.search(r"docs/broken-file\.md:\d+", error) for error in errors), str(errors))
        with tempfile.TemporaryDirectory(prefix="docs-site-check-") as tmp:
            root = self._write_fixture(Path(tmp))
            _written, errors, warnings = build_site(docs_dir=root / "docs", out_dir=root / "out",
                                                   repo_root=root, quiet=True)
            self.equal("without --github-url a link leaving docs/ is a warning, not an error",
                       len(warnings), 1)
            self.check("the warning names the link", "outside.md" in warnings[0], warnings[0])
            _written, strict_errors, _warnings = build_site(docs_dir=root / "docs", out_dir=root / "out2",
                                                           repo_root=root, quiet=True, strict=True)
            self.check("--strict turns that warning into an error", bool(strict_errors), "")

    def build_error_cases(self) -> None:
        with tempfile.TemporaryDirectory(prefix="docs-site-check-") as tmp:
            root = self._write_fixture(Path(tmp), {"docs/open-fence.md": "# t\n\n```sh\nnever closed\n"})
            _written, errors, _warnings = build_site(docs_dir=root / "docs", out_dir=root / "out",
                                                    repo_root=root, quiet=True)
            self.check("an unterminated fence is an error",
                       any("never closed" in error for error in errors), str(errors))
        with tempfile.TemporaryDirectory(prefix="docs-site-check-") as tmp:
            root = self._write_fixture(Path(tmp), {"docs/open-front.md": "---\nid: X\n# no closing matter\n"})
            _written, errors, _warnings = build_site(docs_dir=root / "docs", out_dir=root / "out",
                                                    repo_root=root, quiet=True)
            self.check("unterminated front matter is an error",
                       any("front matter" in error for error in errors), str(errors))
        with tempfile.TemporaryDirectory(prefix="docs-site-check-") as tmp:
            root = Path(tmp)
            (root / "empty-docs").mkdir()
            _written, errors, _warnings = build_site(docs_dir=root / "empty-docs", out_dir=root / "out",
                                                    repo_root=root, quiet=True)
            self.check("an empty docs tree is an error, not an empty site", bool(errors), str(errors))
        # a fence that closes is the control for the case above: same shape, no finding
        with tempfile.TemporaryDirectory(prefix="docs-site-check-") as tmp:
            root = self._write_fixture(Path(tmp), {"docs/closed.md": "# t\n\n```sh\nok\n```\n"})
            _written, errors, _warnings = build_site(docs_dir=root / "docs", out_dir=root / "out",
                                                    repo_root=root, quiet=True)
            self.equal("a closed fence builds clean", errors, [])

    def path_spelling_cases(self) -> None:
        """One directory, two spellings — the macOS `$TMPDIR` trap, made reproducible on Linux.

        On macOS `$TMPDIR` lives under `/var`, and `/var` is a symlink to `/private/var`, so every
        temporary directory has two names for one place and a path assembled from `$TMPDIR` is not
        `==` the same path resolved. `Path.relative_to` across the two spellings raises ValueError, and
        the builder reads a ValueError as "this link leaves the repository" — so a link that was
        plainly inside the tree was reported as outside it, and only on the Mac. docs/testing.md §1
        calls out exactly this hazard for the Rust tests; this is the builder's own half of the rule:
        resolve consistently at the boundary, and prove it here rather than waiting for a laptop.
        """
        with tempfile.TemporaryDirectory(prefix="docs-site-check-") as tmp:
            real = self._write_fixture(Path(tmp) / "real")
            link = Path(tmp) / "link"
            try:
                os.symlink(real, link)
            except (OSError, NotImplementedError):  # pragma: no cover - a Windows box without the privilege
                self.check("symlinked root spelling (no symlink support on this box)", True, "skipped")
                return
            url = "https://example.com/o/r/blob/main"
            for label, root, out in (("real", real, real / "out-real"), ("symlinked", link, real / "out-link")):
                _written, errors, warnings = build_site(
                    docs_dir=root / "docs", out_dir=out, github_url=url,
                    repo_url="https://example.com/o/r", repo_root=root, quiet=True)
                self.equal(f"{label} spelling of the root builds clean", errors, [])
                self.equal(f"{label} spelling of the root earns no warning", warnings, [])
            guide = (real / "out-link" / "guide.html").read_text(encoding="utf-8")
            self.check(
                "the link that leaves docs/ is rewritten, not warned about, through the symlinked spelling",
                'href="https://example.com/o/r/blob/main/outside.md"' in guide,
                "the blob url is missing; the builder thought an inside link was outside the repository")
            self.equal("both spellings write the same files",
                       sorted(_tree_bytes(real / "out-real")), sorted(_tree_bytes(real / "out-link")))
            both = _tree_bytes(real / "out-real"), _tree_bytes(real / "out-link")
            self.equal("both spellings write the same bytes",
                       sorted(name for name in both[0] if both[0][name] != both[1].get(name)), [])


def _tree_bytes(root: Path) -> dict[str, bytes]:
    """Every file under a built tree, keyed by relative path — the shape determinism is checked in."""
    out: dict[str, bytes] = {}
    for path in sorted(root.rglob("*")):
        if path.is_file():
            out[str(path.relative_to(root))] = path.read_bytes()
    return out


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
