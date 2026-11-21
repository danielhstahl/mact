---
id: ADR-0006
title: the .fathom cache root, fathom.toml, and the metric cache store
status: accepted
ticket: mact-by1.2
date: 2026-11-16
---

# ADR-0006: the `.fathom` cache root, `fathom.toml`, and the metric cache store

- **Status:** Accepted
- **Date:** 2026-11-16
- **Ticket:** mact-by1.2
- **Decision:** **Plain files, no engine.** The cache root is `<workspace>/.fathom`, overridable by
  `--cache-dir` then `$FATHOM_DIR`, and it is **disposable**: delete it and the next `fathom index`
  rebuilds everything, which is the same promise `.evidence/` makes through its `README.md` stub
  (ADR-0002 §1.4). Inside it, Fathom owns exactly one subtree — `.fathom/index/` — holding
  **atomically-published JSON**: one row per commit under `rows/`, and a content-addressed block
  store under `blocks/` keyed on `(analyzer id, analyzer version, dependency digest, config digest,
  input kind, input hash)`. **No sqlite, no embedded key-value engine, no C toolchain** — the access
  pattern is digest→value point lookups and the ordering comes from `git log`, so a B-tree engine
  buys capability this product does not use at the price of the one dependency class this repo has
  twice said no to. `fathom.toml` sits at the workspace root, is discovered root-ward, is
  `deny_unknown_fields`, and every key is declared by the analyzer that consumes it so a config edit
  invalidates exactly the affected rows and blocks — a noise-glob change recomputes diffstats
  without re-analysing a single blob. Version mismatch ⇒ **refuse and name the file to delete**,
  never migrate blind. One writer per cache root under `flock(2)` on `<root>/fathom.lock`, mirroring
  `.act-mcp.lock`; readers take no lock because they read immutable, atomically-renamed files.
- **Reconciled, not inherited:** the `.fathom/` already sitting in this repo — `.fathom/index.sqlite3`
  (2,248,704 bytes, WAL mode, `schema_version` 11) and 17 blobs under `.fathom/objects/` — is
  **another program's store and is not adopted**, table for table, in any form. §2 names what it is,
  proves it is not ours, and states what happens to it; §12 deletes the `.gitignore` whitelist
  entries (`reviews/`, `ratchet.toml`) that belonged to it and to nothing of Fathom's.
- **Rejected:** adopting that sqlite store (and with it its episode/ledger/triage product); sqlite
  with `rusqlite` + `bundled`; `redb`/`sled`; one JSON file for the whole index; a git-style pack
  file; the cache root inside `.evidence/`; config in `.fathom/` rather than at the workspace root;
  layered user/workspace/project config; a migration path on a version mismatch. Each with its reason
  in [Considered alternatives](#considered-alternatives).
- **Machine-readable form:** three artifacts, and only three. `.fathom/LAYOUT.md` is the one-paragraph
  on-disk contract that lives *in* the cache root and is the only file in it that is committed; the
  `.gitignore` block in §12 is the ignore contract; and
  [`scripts/validate_fathom_layout.py`](../../scripts/validate_fathom_layout.py) reads the ADR, the
  two files above, `Cargo.lock` and `deny.toml` and fails when they stop agreeing — including the
  ticket's own acceptance rule that no whitelist entry may be orphaned.
- **Checked by:** [`scripts/validate_fathom_layout.py`](../../scripts/validate_fathom_layout.py)
  (C1–C10, run in `scripts/ci.sh`'s `schema` step beside the other ADR checkers; `--self-test`
  proves each named check fires by breaking one rule at a time in a scratch fixture), plus
  `deny.toml`'s two new `[[bans.deny]]` entries, which make the no-C-store decision a supply-chain
  fact rather than a preference.
- **Related:** [ADR-0002](0002-evidence-jsonl-contract.md) (the root it reads; the atomic-publish
  mechanism §6.1 re-uses verbatim; the gitignore-append rule of §6.4 that §12 mirrors),
  [ADR-0003](0003-commit-isolation-and-serialization.md) (§6's `flock` + never-unlink lock precedent,
  and the no-libgit2 / no-C-dependency precedent §5 re-uses),
  [ADR-0005](0005-fathom-crate-layout-and-dependencies.md) (§9 hands this ADR the `.fathom/`
  directory and forbids the crate from choosing any other; §6's dependency budget is what "one store
  for `.fathom` (ADR-0006's call)" has to fit inside),
  [ADR-0004](0004-run-lifecycle.md) (why a reader never needs a lock: writers publish atomically)
- **Consumers inside the epic:** mact-by1.5 (the `--cache-dir`/`$FATHOM_DIR`/`--config` flags and the
  `fathom.toml` skeleton — it implements §3, §7 and the boot-time refusals of §10), mact-by1.6 (the
  evidence identity ledger of §6.4), mact-by1.7 (the MI analyzer: the declared-key table of §7 and
  the block input kinds of §6.3), mact-by1.8 (the diffstat, which shares `paths.noise` /
  `paths.exclude`), mact-by1.10 (the store itself: §4–§11 in full), mact-by1.13 (`fathom doctor`'s
  cache and config checks, §10), mact-by1.16 (cargo-deny: the bans land here), mact-by1.15 (the
  Fathom runbook: it links this ADR and `.fathom/LAYOUT.md`, it does not restate them).
- **Explicitly not this ADR's:** what a commit's coverage % or benchmark value *means* (ADR-0008),
  the MI formula and its language set (ADR-0007), the TUI's own state (P2 puts it in the user's state
  directory), and whether Fathom ever triggers runs (ADR-0005 P5 forbids it today).

## 0. The decision, in one screen

```
<workspace>/.evidence/    read-only for Fathom     (ADR-0002; act-mcp writes it)
<workspace>/fathom.toml   config, committed        (§7; discovered root-ward)
<workspace>/.fathom/      the cache root, disposable
```

Numbered pins. `MUST` / `MUST NOT` / `SHOULD` are normative. Each is either enforced by
`scripts/validate_fathom_layout.py` (the check is named) or is named as a judgement call.

| # | pin |
|---|---|
| P1 | Cache root: `--cache-dir <path>`, else `$FATHOM_DIR`, else `<workspace>/.fathom`. Never read from `fathom.toml` (§3.1), canonicalised, and never inside the evidence root. |
| P2 | The cache root is **disposable**: safe to delete, rebuilt by `fathom index`, and holds nothing a user would want to keep. No preference, filter or UI state lives there (§4.3). |
| P3 | **No sqlite, no embedded store, no C toolchain.** The store is JSON files published with temp + `fsync` + `rename`, plus a content-addressed block directory. `rusqlite` and `libsqlite3-sys` are banned in `deny.toml` (§5). |
| P4 | Fathom owns exactly the paths in §4.1. It MUST NOT write any other path under the cache root, MUST NOT read foreign files there, and MUST NOT delete one — not during prune, not during `--rebuild` (§4.2). |
| P5 | One row per commit, one block per analyzer result, each published atomically and **never edited in place** — ADR-0002 §6.2's mechanism, re-used rather than re-invented (§6.1). |
| P6 | Key derivation is pinned in §6.3 as `fk1` digests. A key covers every input a value depends on, including the path when the value depends on the path. |
| P7 | Every `fathom.toml` key is declared by the code that reads it, and the digest that guards a cached value covers exactly the declared keys, resolved through defaults (§7). |
| P8 | `meta.json` carries `layout_version`, `schema_version`, the `key_schema` fingerprint and the writer identity. On any mismatch: refuse, name the path, say `--rebuild`. Never migrate blind (§8). |
| P9 | One writer per cache root: `flock(2)` on `<root>/fathom.lock`, never unlinked, fail-fast `index_busy`, no queue. Readers take no lock (§9). |
| P10 | `.gitignore` carries exactly the §12 block — `.fathom/*` and `!.fathom/LAYOUT.md` — and the dead whitelist entries that came with the foreign store are deleted (§12). |
| P11 | The store holds paths, digests and numbers. Never file contents, never a secret, never an absolute host path (the ADR-0002 §2.2 rule, applied here). |
| P12 | Size is bounded by the walk set (`history.depth`) and by unreferenced-block GC, both run only under the lock (§11). |

## 1. Context

Fathom is the cross-commit scanner: it reads `.evidence/` (ADR-0002), reads `git log`, computes MI
(ADR-0007), and shows one row per commit. Every ticket after this one writes bytes somewhere, and the
decisions below are expensive to change once bytes exist:

1. **Where the bytes go**, and what a user is allowed to do with them (delete them? commit them?
   have two Fathoms in one workspace?).
2. **What holds them.** A query engine or a directory of files, and therefore whether this repo's
   first C dependency appears.
3. **Where configuration lives**, and how a config edit invalidates exactly the work it should.
4. **What the `.fathom/` already in this repository is**, because the ticket is right that a mystery
   2.2 MB sqlite file in a new tool's cache directory is not an acceptable starting state.

Facts, all of them re-measured for this ticket rather than remembered:

- `.gitignore` already carries a `.fathom` block with four whitelist/negate lines
  (`.fathom/*`, `!.fathom/reviews/`, `!.fathom/ratchet.toml`, `!.fathom/LAYOUT.md`, plus
  `.fathom/**/*.tmp` and `.fathom/**/*.sqlite3-*`). **None of `reviews/`, `ratchet.toml` or
  `LAYOUT.md` exists** in the worktree and `git ls-files .fathom` is empty, so every one of those
  whitelist entries is aspirational and none is load-bearing today (§12).
- `.fathom/index.sqlite3` is 2,248,704 bytes, `integrity_check` = ok, `journal_mode` = wal,
  `page_size` 4096, 26 user tables plus three triggers and an FTS5 table with its four shadow
  tables. Its `meta` row says `schema_version = 11`, `key_schema = fathom-commits/1`,
  `index_epoch = 1`, `epoch_status = ready`. Its `analyzer` table holds 16 analyzers
  (`ingest.normalize`, `taxonomy.path`, `diff.per_file`, `diff.rename_detect`, `episode.segment`,
  `structure.delta`, `deps.resolve`, `non_code.surface`, `test.surface`, `graph.build`,
  `graph.delta`, `graph.cycles`, `blast_radius`, `findings.detect`, `risk.score`, `delta.compose`),
  `cache` holds 682 rows, `index_run` holds 93 runs, `git_commit` 91 rows, `git_commit_file` 673,
  `graph_snapshot` 86, `blob` 808 (791 `inline`, 17 spilled to files), `episode` 51, and
  `decision_note`, `ledger_entry`, `triage_event`, `review_state` and `session_state` are all empty.
- It is **live on the author's machine and it is not this repo's code**: `index_run` id 93 has
  `started_utc = 1794833121489` = `2026-11-16T12:45:21Z`, which is to the second the author date of
  `HEAD` (`24caeb3`, the ADR-0005 commit). Something hooked the commit and indexed it. No file in
  this repository opens that database; no ADR here defines its schema; no ticket in this epic can
  read it. §2 is its disposition.
- `.fathom/objects/<2 hex>/<64 hex>` holds 17 files (160 KB) whose contents are per-tree file
  inventories of *this* repo — `{"kind":"delta","dep":14,"base":"4e4d759…","files":[["tests/boot.rs",
  33188,13297], …]}`. Same other program: content-addressed spill of its `blob` table, git-object
  directory naming, `sha256` names (64 hex, not git's 40).
- The repo has **zero C build dependencies** and says so twice: ADR-0003 and `Cargo.toml` both refuse
  libgit2 because it "would add a C build dependency for no gain". `cd.yml` builds five targets, two
  of them `*-musl`. `deny.toml` allows only permissive OSI licences and bans by name (it already
  bans `serde_yaml`).
- ADR-0005 §9 already decided the *name*: `.fathom/` is "the data directory a fathom run writes into
  a user's workspace — ADR-0006's to define", `crates/fathom/` is source, and the crate may not pick
  a different location. This ADR therefore does not reopen the root's name, only its contents.
- ADR-0002 §6.2 already specifies, and `tests/` already exercises, the exact mechanism this store
  needs for crash safety: write a temp file in the destination directory, `fsync`, `rename`,
  `fsync` the parent. Re-using it is cheaper than describing a second one.

## 2. The `.fathom/` that is already here — reconciled first

The ticket's framing is correct: this must be answered before anything writes a byte, and "leave the
mystery file" is not an answer. Here is the answer, in three parts.

### 2.1 What it is (observable, not guessed)

`.fathom/index.sqlite3` + `.fathom/objects/` belong to a **different program** — one that indexes a
repository's history into sqlite, splits it into "episodes" with agent-versus-human attribution
(`episode.author_kind`, `attribution_correction`, `episode.boundary_rule ∈
{override,hook,trailer,branch,author,time-gap,message-shape,root}`, `triage_event`,
`ledger_entry`, `decision_note`, `review_state`), and keeps a code graph (`graph_snapshot`,
`graph_node`, `graph_delta`, `graph_chunk`) with cycle and blast-radius analyses. Its vocabulary is
*episodes, ledger entries, decision notes, triage* — a review-and-attribution product. Fathom's
vocabulary is *commits, coverage, MI, benchmarks* (ADR-0008). The overlap is the word "analyzer" and
a shared ancestor's habit of writing dot-directories into whatever workspace it points at, which is
also how `.evidence/` and `.act-mcp.lock` got here (ADR-0002 §6.4, ADR-0003 §6).

### 2.2 The decision: not adopted, not migrated, not deleted by us

| question | answer |
|---|---|
| Does Fathom read it? | **No.** Not now, not through a compatibility shim. `fathom index` never opens `index.sqlite3`, and `fathom doctor` reports it as foreign (§10.4). |
| Does Fathom write into it, or add its own tables beside the existing ones? | **No.** Sharing a file with a live foreign writer is a locking negotiation with a program whose source this repo does not have. |
| Do any of its 26 tables survive into Fathom's store? | **None.** Not because the shapes are wrong — several are good — but because Fathom has no reader for them. Adopting a table without a reader is exactly how `decision_note`, `ledger_entry`, `triage_event` and `review_state` got to be four empty tables in a 2.2 MB file today. The two Fathom concepts that *look* like neighbours are different things: its `cache`/`analyzer` pair keys on tree/blob scopes for a graph product, Fathom's rows key on commits for a metrics product (P6). |
| What about `reviews/` and `ratchet.toml`, which `.gitignore` whitelists? | **Dead here, so the whitelist goes.** Neither exists, neither is tracked, and nothing in this epic has a review/triage surface or a separate ratchet file: Fathom's CI gate is `fathom report --fail-on …` (mact-by1.13) and its baseline lives in `fathom.toml` (§7.2). §12 deletes both entries and says what their owner must do to get them back. |
| Do we delete `index.sqlite3` and `objects/` from this checkout? | **No.** They are another program's live state, and a cache directory is not ours to clean for a tool we do not own (P4). They stay gitignored. Deleting them is that program's business or the user's, and `fathom doctor` names them so it is at least not a mystery (§10.4). |
| And `objects/`, since Fathom wants content-addressed blobs too? | **Different directory.** Fathom's blocks are `.fathom/index/blocks/…`, never `.fathom/objects/…`. Two programs GC-ing into one content-addressed directory would delete each other's blobs — a shared directory makes that a data-loss bug; separate directories make it a non-event. |

## 3. Root resolution

### 3.1 The three roots, and their precedence (P1)

| root | flag | env | default | may `fathom.toml` set it? |
|---|---|---|---|---|
| **cache root** | `--cache-dir <path>` | `$FATHOM_DIR` | `<workspace>/.fathom` | **no** |
| **evidence root** | `--evidence-dir <path>` | `$EVIDENCE_DIR` | `<workspace>/.evidence` | **no** |
| **config** | `--config <path>` | `$FATHOM_CONFIG` | `fathom.toml`, found root-ward (§7.1) | — |

Same three-way shape and the same precedence order as the evidence root's (`--evidence-dir`,
`$EVIDENCE_DIR`, `<workspace>/.evidence`, ADR-0002 §1.1), because mact-by1.5 gives `fathom` the same
shared flags and a reader whose flags disagree with the writer's is a bug report waiting to happen.

- The value is canonicalised (`realpath`) and used as-is; a relative path resolves against the
  current working directory, not against the workspace, so `--cache-dir /tmp/x` and `cd /tmp &&
  fathom --cache-dir x` mean the same thing and say so in `doctor`.
- The cache root MUST NOT be inside the evidence root, and the evidence root MUST NOT be inside the
  cache root. That is a boot-time refusal, not a warning: Fathom reads one and writes the other, and
  nesting them makes "delete the cache" a way to destroy evidence.
- **The cache root is not a `fathom.toml` key on purpose.** The config file is discovered from the
  workspace; letting the config file (or the cache, which is disposable) name the cache root creates
  a loop in which the wrong value hides the file that would fix it. A flag or an environment
  variable is always reachable, and both are printed by `fathom doctor` and recorded in
  `index/meta.json`.
- `$FATHOM_DIR` pointing at a *file*, or at a directory the process cannot create, is a boot-time
  refusal (§10.1) — never a silent fall back to the default, because a silently-ignored environment
  variable turns every cache miss into a mystery.

### 3.2 "Safe to delete, just rebuild" (P2)

The property the evidence root advertises, stated for the cache root, and pinned to the same
three consequences:

1. **Deleting it loses nothing but time.** The store is a function of (`git log`, `.evidence/`,
   `fathom.toml`, the Fathom build). `rm -rf <root>/index` and `fathom index` reproduces every byte
   (§11.3 states the determinism this promise depends on).
2. **Nothing worth keeping lives there.** UI state, pane sizes, filters, per-user overrides and
   saved views go to the user's own state directory (`$XDG_STATE_HOME/fathom/`, else
   `$HOME/.local/state/fathom/`), never to the cache root — otherwise `rm -rf` of a cache costs a
   user their preferences and the property is a lie. mact-by1.9 owns that file; this ADR owns only
   the rule that it is not in `.fathom/`.
3. **The root says so on disk.** `fathom` creates `<root>/LAYOUT.md` if absent (never overwritten),
   the same trick as `.evidence/README.md` (ADR-0002 §1.4): one paragraph, safe to delete, points at
   this ADR, do not hand-edit. In *this* repository that file is committed, which is what makes the
   contract reviewable; §12 is the ignore rule that allows exactly that one file.

## 4. The tree

### 4.1 The layout (the pinned block — `.fathom/LAYOUT.md` must match it)

`<xx>` is the first two hex characters of the key named on that line. `<cache root>` is §3.1's
resolved path; in a workspace it is normally `.fathom`.

```
<cache root>/
├── LAYOUT.md                     # the one committed file: this contract, written if absent
├── fathom.lock                   # flock(2) for this cache root; advisory JSON body; never unlinked
└── index/                        # the store: delete this directory, `fathom index` rebuilds it
    ├── meta.json                 # layout_version, schema_version, key_schema, writer, digests
    ├── run.json                  # counters of the last and previous index run (`index --status`)
    ├── sources.json              # evidence-file identity ledger (path → size, mtime, sha256)
    ├── rows/<xx>/<commit>.json   # one row per commit: history+diffstat, evidence metrics, MI
    └── blocks/<xx>/<key>.json    # content-addressed analyzer results — the metric cache
```

That is the whole tree: seven kinds of path, no cache of a cache, no `tmp/` (temp files are created in
the destination directory and named `.<final-name>.<random>.tmp`, exactly as ADR-0002 §6.2 does, so
they are dot-hidden and live and die beside the file they become).

Directories are created `0755` and files `0644`, with the mode applied by an explicit `fchmod` so a
user's `umask` cannot make a store unreadable by the next process — the same reasoning ADR-0002 §1.1
gives for the evidence root.

### 4.2 Ownership, and what happens on a collision (P4)

- Fathom creates and owns exactly the paths above. `LAYOUT.md` and `fathom.lock` at the root;
  everything else under `index/`.
- Anything else under the cache root — `index.sqlite3`, `index.sqlite3-wal`, `objects/`, a
  `reviews/`, whatever a colleague's tool writes — is **foreign**. Fathom MUST NOT read it, MUST NOT
  modify it and MUST NOT delete it, including during `--rebuild` (which removes `index/` and nothing
  else) and including during prune (§11). A cache directory shared by two programs is fine as long as
  neither touches the other's bytes; the moment one "helpfully" garbage-collects the other, deleting
  the cache becomes data loss.
- A collision is only possible where Fathom needs a name. If one of §4.1's paths exists but is not
  what Fathom expects — `index` is a regular file, `LAYOUT.md` is a directory, `blocks/xx` is a
  symlink out of the root — Fathom refuses at boot with the path and the fix (§10.4). Symlinks are
  refused for every path Fathom writes, exactly as ADR-0002 §6.3 refuses them for evidence paths: a
  store that escapes its root through a symlink is not disposable any more.
- `fathom doctor` lists every entry in the cache root it does not own, as an **advisory note on an `ok`
  `cache` check**, with the line "not written by Fathom; Fathom neither reads nor deletes it". That is the
  anti-mystery mechanism: the ticket's complaint is not that a foreign file exists, it is that nothing said
  what it was. It is deliberately not a `warn`: §4.2 already decided the entry stays — it is another
  program's live state, Fathom neither reads nor deletes it, and a shared cache root is legal — so a `warn`
  would be a finding that nobody reading the report can act on, and `doctor`'s exit code *is* its worst
  check, which is what CI gates on. A foreign entry earns a status only by **obstructing** something Fathom
  needs — `index` a regular file, `blocks/xx` a symlink out of the root — and those are already reported by
  the checks that try to use the path.

### 4.3 What is deliberately not here

| not in the cache root | where it lives | why |
|---|---|---|
| `fathom.toml` | workspace root, committed (§7) | config is a team artifact and a reviewable input to every number; a disposable directory is the wrong home for the thing that decides what a number means |
| UI state, pane sizes, filters, saved views | `$XDG_STATE_HOME/fathom/` | P2 |
| logs | stderr, or `$XDG_STATE_HOME/fathom/logs/` if a file is ever wanted | ADR-0005 §6: fathom draws to a TTY; stdout is data |
| the evidence root | `<workspace>/.evidence` | ADR-0002; Fathom is a reader of it, never a writer into it |
| exported reports | wherever the user redirects `fathom report` | stdout, so no location decision is needed |
| anything derived from a *dirty* working tree | nowhere | a row describes a commit, never the checkout: ADR-0007's "analysis operates on content" and `git ls-tree`, not `std::fs` walks |

## 5. Store engine: files, not sqlite, not an embedded engine (P3)

### 5.1 Why files win here

Write the query set down and the answer stops being close.

| what the product does | access pattern |
|---|---|
| timeline (mact-by1.9) | `git log --first-parent? -n depth --format=%H` gives the ids and their order; then one point lookup per id |
| commit detail (by1.14), diff view (by1.11) | point lookup by commit id |
| benchmark sparkline (by1.12) | the same ids, one window, values already in those rows |
| MI hotspots (by1.7) | inside one commit's row |
| `fathom report` (by1.13) | the same window, streamed |
| the metric cache (by1.10) | "is `f(blobs, analyzer, version, config)` already computed?" — a digest lookup |

There is no ad-hoc query, no join, no range scan, no aggregation over the store: **ordering comes
from `git log` and every other access is a lookup by a key that is already a filename.** A B-tree
index has nothing to index that a path does not already provide. Meanwhile the properties this repo
already pays for come free with files:

- **Crash safety is the same mechanism as the evidence contract.** A row is written to
  `.<name>.<random>.tmp` in `rows/<xx>/`, `fsync`-ed, `rename`-d, parent `fsync`-ed (ADR-0002 §6.2).
  A half-written row is not a thing that exists, so "crash mid-index leaves the store usable"
  (by1.10's requirement) is satisfied by construction instead of by a journal, a WAL, a
  `PRAGMA integrity_check` and a recovery path.
- **Readers need no coordination.** An immutable, atomically-renamed file is a snapshot. The TUI can
  read while `fathom index` writes and sees either the old row or the new one, which is the same
  argument ADR-0005 §7 makes about `.evidence` and the same one ADR-0002 §6.2 makes about runs.
  sqlite gives the same guarantee through WAL — and charges §5.2's whole list for it.
- **Disposability is literal.** `rm -rf index/` is the migration, the repair and the cleanup. There
  is no `-wal`/`-shm` etiquette, no "delete the db but keep the sidecars" trap, and nothing to
  checkpoint. The `.fathom/**/*.sqlite3-*` line in `.gitignore` exists only because some tool's WAL
  leftovers might escape; §12 deletes it because Fathom will not make any.
- **`doctor` can read the store with no engine.** "Which rows exist, which are stale, what is this
  commit's number" are `cat` and `jq` questions. That is worth real money in a tool whose whole job
  is explaining itself.
- **One copy of the mechanism.** ADR-0002 §6.2's publish rule, its mode rules, its symlink rule and
  its tests already exist. A store engine would be a second implementation of all four.

### 5.2 What sqlite would have cost, and why "no" is not free-standing

`rusqlite` with `bundled` (the only sane way to ship sqlite for a binary this repo builds on five
targets) means:

| cost | detail |
|---|---|
| a C toolchain in the build | `cc` becomes a build requirement for `crates/fathom`; `cd.yml` builds `x86_64/aarch64` × `gnu/musl` plus darwin, and musl means `musl-gcc`, i.e. a cross toolchain per target or a build-time failure that looks like a Rust error |
| build and CI time | ~25k lines of C compiled on every clean build of the TUI, on five release jobs, to store ~500 rows |
| the repo's stated policy | ADR-0003 and `Cargo.toml` both refuse libgit2 for exactly this reason ("a C build dependency for no gain"); accepting sqlite here would be the second exception, in the *reader*, where the surface is smaller |
| supply-chain surface | `libsqlite3-sys` build script, bindgen-or-bundled matrix, and a body of C in the process that reads untrusted repositories — the process that also renders a TUI a user runs in a random checkout |
| licences | fine, and worth saying so plainly so the record is honest: `rusqlite`/`libsqlite3-sys` are MIT and `deny.toml`'s allow list already permits MIT; the SQLite *blessing* is public domain and cargo-deny scores the crate, not the amalgamation. **The licence was never the objection; the toolchain was.** |
| a migration story we would have to write | §8's refuse-and-rebuild rule is cheap for files (rebuild) and expensive for a database (dump/replay or accept a schema-version matrix) |

So the pins: **no `rusqlite`, no `libsqlite3-sys`, no `diesel`/`sea-orm`, and no pure-Rust embedded
store (`redb`, `sled`) either** — those last two are rejected on the same analysis minus the C
compiler: they buy a transaction log this design gets from `rename(2)`, and they cost a second
compaction/GC/recovery story. `deny.toml` gains two `[[bans.deny]]` entries (`rusqlite`,
`libsqlite3-sys`) so the decision cannot be reversed by a quiet `Cargo.toml` edit, and
C4 of the checker asserts neither appears in `Cargo.lock`. A pure-Rust embedded store is *rejected*
rather than banned: if one ever wins on evidence, the amendment is a `Cargo.toml` line and an ADR
edit, not a fight with a ban.

The budget that would reopen this (measure before amending): the store's file count and cold-index
wall time against a synthetic ~1000-commit repository and against this one — mact-by1.10's own test.
If plain files blow the budget, the amendment is §11.4's packing escape hatch, not sqlite-by-default.

## 6. The store: rows, blocks, and how they are published

### 6.1 Publication and immutability (P5)

Every file in `index/` — a row, a block, `meta.json`, `run.json`, `sources.json` — is written by
exactly one mechanism, the one ADR-0002 §6.2 already specifies for evidence, and by nothing else:

1. serialise to bytes in memory;
2. write `<dest>/.<name>.<random>.tmp` **in the destination directory**;
3. `fsync` the file;
4. `rename()` it to its final name (same filesystem ⇒ atomic);
5. `fsync` the directory.

Consequences, all of them load-bearing later: no file is ever opened for writing twice; a crash
costs at most one file; `--rebuild` is "remove `index/`, run the cold path"; a rollback is a delete;
and a reader never needs a lock (P9). The store has no append-only file anywhere: JSONL, which is
what appending would want, is ADR-0002's format for *evidence*, and mixing the two formats in one
product would give a maintainer two rules to remember.

### 6.2 `meta.json`, `run.json`, `sources.json`

```jsonc
// index/meta.json — written first, and the store's fingerprint (§8)
{
  "fathom_index": "fathom-index/1",       // key_schema fingerprint; a foreign file fails here
  "layout_version": 1,                     // this ADR's tree
  "schema_version": 1,                     // the row and block shapes below
  "writer": { "name": "fathom", "version": "0.0.1" },
  "root": "/abs/path/to/.fathom",          // where this store says it lives
  "workspace": "/abs/path/to/repo",
  "created_utc": 1794833121, "updated_utc": 1794833121,
  "config": { "path": "/abs/path/to/repo/fathom.toml", "digest": "fk1:…" },  // §7; digest only
  "key_inputs": { "fingerprint_of_f": "fk1" },
  "analyzers": [ { "id": "mi.score", "version": 1, "input_kind": "blob",
                   "config_keys": ["paths.noise","paths.exclude","languages.enabled"] } ]
}
```

`analyzers` is a *recording* of the in-code registry (ADR-0005's "no fourth spelling of a fact"
rule: code is the source of truth, `meta.json` is what a `doctor` or a `jq` sees), and by1.10's test
asserts the two agree. `run.json` is the `index_run` idea, two entries deep:

```jsonc
// index/run.json
{ "last": { "kind": "warm", "trigger": "manual", "status": "ok",
            "started_utc": 0, "finished_utc": 0,
            "commits_seen": 0, "commits_added": 0, "rows_written": 0,
            "cache_lookups": 0, "cache_hits": 0, "cache_misses": 0,
            "evidence_files_parsed": 0, "blocks_pruned": 0, "bytes_written": 0,
            "interrupted": false },         // "error": "<one line>" appears only when one happened
  "previous": { } }
```

`fathom index --status` (mact-by1.10) prints `last` plus the row and block counts; the *counters*
shape is deliberately in the spirit of act-mcp's `index_run` bookkeeping rather than a new
invention, and deliberately small: no `peak_rss_kib` until something measures it.

`sources.json` answers the one genuinely subtle question in by1.10 — *git history is immutable,
evidence is not*. It is a ledger of the evidence files that produced the current rows:

```jsonc
// index/sources.json — one entry per evidence file, sorted by path; the ledger, not a log
{ "files": [
  { "path": ".evidence/coverage/2f…/20261110T004203Z-a1b2c3.jsonl",
    "run_id": "20261110T004203Z-a1b2c3", "commit": "2f…",
    "size": 10480, "mtime_utc": 1794833121, "sha256": "…", "parsed_records": 42 }
] }
```

Identity is the **content digest**; `(size, mtime_utc)` is only the cheap test that decides whether
to re-hash. A row's `sources.evidence_digest` is `fk1( sorted("path\0sha256") )` over exactly the
files that fed it, so a *new* evidence file for an *old* commit changes that commit's
`evidence_digest`, and the next `fathom index` re-derives that one row. `sources.json` is derived
data: absent or older than the last run, it is rebuilt by stat-ing the evidence root, which is why
crashing mid-run costs a re-stat and nothing else.

### 6.3 Keys (P6)

```
fk1(s)    = "fk1:" || lowercase hex sha256(UTF-8 bytes of s)          // no trailing newline
canon(v)  = canonical JSON: object keys in ascending code-point order, no insignificant
            whitespace, no nulls — the ADR-0002 §6.1 rule, re-used rather than re-invented

config_digest(keys) = fk1( "fc1\0" || canon( { "<dotted.key>": resolved_value, … } ) )
                      // only the declared keys, keys sorted ascending, defaults already applied

dep_digest(analyzer) = fk1( canon( sorted( [ "id@version", … ] ) ) )  // transitive analyzer deps; [] allowed

block_key = fk1( "fb1\0" || analyzer_id || "\0" || analyzer_version
                 || "\0" || dep_digest || "\0" || config_digest || "\0" || input_kind
                 || "\0" || input_hash )
            → index/blocks/<block_key[4..6]>/<block_key[4..]>.json    // strip the "fk1:" prefix

input_hash by input_kind:
    blob        fk1(bytes)                                  // content only: path-independent
    path        "p:"  || repo-relative path
    blob_path   "bp:" || repo-relative path || "\0" || fk1(bytes)
    tree        "t:"  || git tree oid
    commit      "c:"  || git commit oid
    evidence    fk1( canon( sorted( [ "path\0sha256", … ] ) ) )
```

Rules that make the derivation trustworthy rather than folklore:

- **The key must cover every input the value depends on.** A value that varies with a file's path
  (language detection, noise classification, a per-path skip) is keyed `path` or `blob_path`, never
  `blob`. ADR-0007 owns where that line falls for MI; the pin here is that the analyzer declares its
  `input_kind` honestly and that a value found under a key that omits one of its inputs is a bug of
  the worst kind — a wrong number that never changes.
- **`analyzer_version` is an integer bumped in code, and every analyzer has a golden block
  fixture** (by1.7 for MI, by1.10 for the rest): the fixture's bytes are part of the test, so an
  output change without a version bump shows up as a fixture diff in the PR that caused it. That is
  the cheapest possible substitute for a schema registry, and it is the rule the foreign store's
  own comment ("bumped in code; CI checks the bump") wished it had.
- Blocks are self-describing: the body repeats `analyzer`, `version`, `input_kind`, `input_hash`,
  `config_digest`, `cost_ms`, `built_utc`, plus `result`. A stray or mismatched block can therefore
  be explained by `cat` — which is what makes §11's GC safe to write.
- A cached **failure** is cached too (`result.error`), the way ADR-0002 keeps a non-`success` run's
  evidence instead of suppressing it: a miss and a "this file cannot be parsed" must not look the
  same to a re-index, or one unparseable file is re-parsed on every run forever.

### 6.4 A row

```jsonc
// index/rows/2f/2f….json
{
  "row_version": 1,
  "commit": "2f…",                                 // 40 lowercase hex, == the filename
  "indexed_utc": 1794833121,
  "sources": {                                     // §7.3: what this row was derived from
    "git": { "tree": "4e…", "parents": ["9a…"], "author_utc": 1794833000 },
    "evidence_digest": "fk1:…", "dep_digest": "fk1:…",
    "config_digest": { "history": "fk1:…", "diffstat": "fk1:…", "evidence": "fk1:…", "mi": "fk1:…" }
  },
  "history": { "subject": "…", "author_name": "…", "author_email": "…", "committer_utc": 0,
               "n_files": 4, "n_add": 120, "n_del": 30, "n_binary": 0,
               "files": [ { "path": "src/config.rs", "add": 90, "del": 12, "kind": "modify" } ],
               "files_truncated": 0, "noise_files": 2, "noise_add": 4000, "noise_del": 4000 },
  "coverage":  { "percent": 82.4, "found": 1200, "hit": 989, "runs": ["2026…-a1b2c3"] },
  "benchmarks": [ { "name": "build_s", "unit": "s", "value": 12.5, "min": 12.1, "max": 13.0,
                    "samples": 3, "run_id": "2026…-a1b2c3", "flags": [] } ],
  "action_success": "success",
  "mi": { "aggregate": 71.4, "loc": 12345, "files": 120, "delta_vs_parent": -0.8,
          "hotspots": [ { "path": "src/runner.rs", "mi": 18.2, "loc": 900, "delta": -3.1 } ],
          "counts": { "ok": 120, "skipped_noise": 2, "unsupported_language": 3,
                      "parse_error": 0, "vacuous": 1, "oversized": 0 },
          "blocks": { "src/config.rs": "…" } },
  "gaps": { "no_evidence": false, "no_mi": false, "unanalysed_paths": 3 }
}
```

The shape rules are the pin; the field lists above are v1's and are owned by the ADRs named:

- **Per-section digests, not one.** `sources.config_digest` has one entry per derivation
  (`history`, `diffstat`, `evidence`, `mi`) so §7.3 can recompute exactly one section of a row and
  reuse the rest. This is the whole reason the cache invalidates *proportionally* instead of globally.
- **Unknown is absent, never zero** — ADR-0002 §2.1 verbatim. `coverage` is absent on a commit with
  no coverage evidence; `gaps` says which column will render `—` and why. The TUI, `fathom report`
  and CI then agree because they read one rule out of one file.
- **No file contents, no absolute paths, no secrets** (P11): paths are repo-relative with forward
  slashes and ADR-0002 §2.2's pattern and length cap; block results are numbers, enums and short
  tokens. If a future feature wants to cache text (a diff summary, an error excerpt) it re-reads
  this pin first, because "the cache might contain a fragment of a private source file" changes what
  `doctor --json` can print and what a bug report can attach.
- **`coverage`/`benchmarks`/`action_success` carry ADR-0008's decisions and `mi` carries ADR-0007's.**
  They may add fields additively; a breaking change bumps `row_version`, and §8's rule then applies
  to it. This ADR pins where the bytes go and how freshness is decided, which is the part every
  consumer needs fixed at the same time.

## 7. `fathom.toml`

### 7.1 Discovery and precedence

```
1. --config <path>            (no search; an unreadable file is a refusal, never "use defaults")
2. $FATHOM_CONFIG
3. <workspace>/fathom.toml, then each parent directory, stopping after the git top level
4. built-in defaults (the right-hand column of the table below)
```

- **One file, no layering.** There is no user-wide file merged with a workspace file. A single source
  is what makes "every key is in the digest" (§7.2) a checkable claim, what makes two developers and
  CI produce identical cache keys, and what keeps `fathom doctor` able to answer "why is this
  number what it is" with one path. A global config is an ADR amendment, not an implementation
  detail — with layering come shadowing rules nobody asked for.
- **Root-ward, and bounded.** The search starts at the workspace directory (so a monorepo member
  can carry its own file) and stops after `git rev-parse --show-toplevel` — or after the filesystem
  root when the workspace is not a git repository. Searching past the repository would let a
  `fathom.toml` in `/home/dev` silently change what a repository's numbers mean.
- A broken file is a refusal, never a fallback: a parse error names the path and the line/column,
  and a silent fallback to defaults would change every cache key while looking like an ordinary
  index run. An *absent* file is not an error: defaults apply, `doctor` says so.
- **Unknown keys are an error** (`#[serde(deny_unknown_fields)]`, mact-by1.5's "unknown keys
  are an error, not a shrug"). A removed key is also an error, with the nearest existing key named —
  that is the difference between a rename that gets noticed and a rename that quietly turns off a
  noise glob. Every key is read through the parsed struct: `fathom.toml` is parsed once, into one
  typed `Config`, and every value an analyzer receives is resolved through defaults first
  (§7.2's digests depend on this).
- **CLI flags override config, and there is no per-key environment variable.** by1.5's "every flag
  overridable by env" is met by `FATHOM_DIR`, `FATHOM_CONFIG`, `EVIDENCE_DIR` and `RUST_LOG`.
  Per-key variables (`FATHOM_PATHS_NOISE`, `FATHOM_LANGUAGES_ENABLED`) are refused deliberately: they
  would be a second, untyped, un-versioned spelling of the config `fathom.toml` exists to be, and a
  value set in a CI job's environment is a value that never reaches the digest of the run that used
  it — the exact failure §7.3 exists to prevent.

### 7.2 The keys

Defaults are the values Fathom uses with no file at all; the `consumed by` column is the
**declared** config-key list of §7.3, and is normative in the sense that code may not read a key it
does not declare.

| key | type | default | consumed by | changes |
|---|---|---|---|---|
| `history.depth` | int ≥ 1 | `500` | the walk; prune (§11) | nothing cached — it is the window, not an input |
| `history.refs` | string[] | `["HEAD"]` | the walk | nothing cached, same reason |
| `history.first_parent` | bool | `false` | `git.walk`, row `history` section | rows' `history` + every parent-relative value (ADR-0008 deltas) |
| `paths.source` | glob[] | `["**/*"]` | MI candidate set, diffstat | rows' `diffstat`, `mi` blocks |
| `paths.noise` | glob[] | `["target/**","node_modules/**","**/*.lock","Cargo.lock","**/generated/**","**/*.min.*",".venv/**","vendor/**"]` | `taxonomy.path` (shared: MI **and** diffstat), `mi.score` | rows' `diffstat` + `mi`; blocks whose key names a noise glob |
| `paths.exclude` | glob[] | `[".git/**",".fathom/**",".evidence/**"]` | every path walk | everything those paths feed |
| `languages.enabled` | string[] | ADR-0007's v1 set | `mi.tokenize`, `taxonomy.path` | `mi` blocks |
| `index.file_size_cap_mib` | int ≥ 1 | `2` | ingest (a larger file is `oversized`, not analysed) | `mi` blocks |
| `benchmarks.allow` | string[] | `[]` (= every metric) | evidence section of a row | rows' `benchmarks` section |
| `benchmarks.deny` | string[] | `[]` (deny wins) | idem | idem |
| `cache.block_cap` | int | `200000` | prune | nothing cached (a policy, not an input) |
| `cache.prune_on_index` | bool | `true` | prune | nothing cached |

Properties worth their place in the document:

- **`paths.noise` really is shared**, which is the ticket's requirement: the same list decides both
  which files MI analyses and which paths' `+`/`-` count toward a row's headline diffstat (the rest
  are still counted, in `history.noise_add`/`noise_del`, so a 4,000-line lockfile never disappears —
  it is *labelled*, which is the difference between noise handling and lying).
- **`[benchmarks]` filters what is kept, not what is plotted.** ADR-0008 owns which metrics are
  comparable and how they are grouped; this list is the user's "drop `job_duration_s` from my
  store". The default is allow-all: Fathom does not hide evidence by default.
- **A glob's semantics are one sentence:** matched against repo-relative paths with forward slashes,
  case-sensitive, `**` crosses `/`, a trailing `/` matches a directory subtree, no negation,
  precedence `exclude` > `noise` > (`source` ∩ an enabled language extension). No negation on
  purpose: order-dependent globs in a config file are a support ticket.
- **`history.depth` is not a cache key, and that is a rule, not an accident:** no cached value may
  depend on the *set* of other commits in the window. A row is a function of its commit, its
  parents, its evidence and the config — which is what makes a depth change cheap, and what stops
  `--from`/`--to` from being config at all (§7.4).
- `doctor` (mact-by1.13) reports unknown keys, globs that match nothing in this repository, and a
  `languages.enabled` entry outside ADR-0007's supported set. Each is a `warn` with the line to fix.

### 7.3 Config digests: a change invalidates exactly the rows and blocks it should (P7)

Three rules, and then the payoff.

1. **Every analyzer declares the keys it reads**, as dotted paths (§7.2's column). A path may name a
   whole table (`paths.noise`) and may not name something its code does not read.
2. **The digest covers only the declared keys, and their resolved values** (§6.3): defaults applied,
   in canonical JSON. So adding a key to `fathom.toml` that an analyzer does not declare does not
   invalidate that analyzer's blocks, and a file that omits a key and a file that states the default
   produce the same digest — otherwise every cache dies the first time someone writes a comment.
3. **The declared list is tested, not trusted** (by1.10's test): for each registered analyzer and
   each key in the workspace's resolved config, flip that key's value alone and assert (a) the
   analyzer's `config_digest` changes iff the key is declared, and (b) the cached value actually
   depends on it. A key read without being declared, or declared but unused, fails that test. It is
   the only way "invalidates exactly the right rows" survives contact with a second analyzer author.

The payoff, on the two cases that matter most:

| change | what dies | what survives |
|---|---|---|
| edit `paths.noise` (add `fixtures/**`) | every row's `diffstat` section, and the blocks of files whose noise classification changed | every `mi`/`tokens` block for a file that was and stays informative — **not one blob is re-tokenised** |
| edit `benchmarks.deny` | every row's `evidence` section (a re-read of `.evidence`, no analysis at all) | every block, and every row's `history` and `mi` sections |
| bump `mi.score`'s `analyzer_version` | the `mi` blocks and `mi` sections of every row | `history`, `evidence`, every other analyzer's blocks |
| edit `history.depth` | nothing cached; the walk set and prune's input change | everything |
| upgrade Fathom (analyzer versions change) | exactly the analyzers whose version moved | the rest, which is the point of per-analyzer versions |

The row-level digests are the four entries of `sources.config_digest` (§6.4); a block carries one
(§6.3), computed from the same rule with that analyzer's declared keys. `doctor` prints the digest
per section and the date of the last run that matched, so "why did this re-index" is answerable
without reading code.

### 7.4 Config, flags and the two things that are never config

`--from`, `--to`, `--limit` and `--format` are **view** choices and `--cache-dir`, `--evidence-dir`,
`--config` are **plumbing**: none of them enters a digest or changes a cached value, precisely because
a cached value may not depend on them (§7.2's last bullet). `--first-parent` is the one flag that is
not in either bucket, and it is worth the exception spelled out: it overrides `history.first_parent`,
which *is* config, because ADR-0008's per-commit delta depends on which parent a row is compared
against. Passing it therefore changes the `history` section's digest and re-derives that section —
intended, visible in `doctor`, and the reason the flag is documented as costing a re-index rather than
free.

## 8. Versioning, and refusing instead of migrating blind (P8)

`index/meta.json` (§6.2) is the guard. Every mismatch below has exactly one answer, and none of them
is "guess":

| mismatch | decision |
|---|---|
| `layout_version` **higher** than this build's | refuse: a newer Fathom wrote this tree. `fix: upgrade fathom, or delete <root>/index` |
| `layout_version` **lower** | refuse: §8.1 says why this is never a migration |
| `schema_version` (row/block shape) differs | refuse, same message |
| `fathom_index` is absent, or not `fathom-index/1` | refuse: something else wrote this directory (§10.4) |
| `meta.json` unreadable/truncated | refuse, and name `rm -rf <root>/index` as the fix — the store's single point of failure, which is why it is 20 lines of JSON |
| no `meta.json`, empty `index/` | this is the cold path, not an error |
| a **row** whose `row_version` this build does not know | that one row is stale: re-derive it. Other rows are unaffected, and `doctor` counts them |
| `writer.version` differs, everything else matches | fine, and expected: re-indexes are incremental across Fathom versions, and the per-analyzer digests decide what actually needs redoing |

### 8.1 Why refuse-and-rebuild, and what an amendment would have to show

Rebuilding is *cheap by construction here*: a cold index is O(commits × changed files), and a rebuild
after a Fathom upgrade is exactly what a user would want anyway. A migration would have to be
correct for every past version, and "correct" here means byte-identical to the rebuild — so the
migration's test is a cold rebuild from a fixture of the old store, which means the old store must
be kept forever as a test fixture (the foreign store is the object lesson: `schema_version = 11`,
i.e. eleven schema changes' worth of migration or eleven abandoned versions of the same fixture).

So: **no migration path exists in v1**, and an amendment that wants one must bring (a) a committed
fixture store at the old version, (b) a test asserting migrated bytes == rebuilt bytes, and (c) a
reason rebuilding is not enough — e.g. a cold index that is unacceptably slow on some class of
repository, which §11.4 is where that evidence would be recorded.


## 9. One writer, no reader lock (P9)

| # | rule |
|---|---|
| L1 | A writer (`index`, `index --rebuild`, prune) takes `flock(open(root/fathom.lock, O_CREAT\|O_RDWR), LOCK_EX \| LOCK_NB)` before it writes a byte. Held for the whole run, released by exit — by `close(2)` if the process dies, which is the entire stale-lock story. |
| L2 | Busy is **fail-fast**: `index_busy`, "retryable", no queue, no `--lock-wait-secs`. This is ADR-0003 §6's decision (D-5/D-6) applied to a cache: a second `fathom index` in one workspace is a mistake worth a message, not a queue worth a scheduler. |
| L3 | The lock file is **never unlinked**, by anyone, ever — including `--rebuild` and prune. Its content is advisory JSON the holder rewrites (`pid`, `hostname`, `started_utc`, `phase`, `commits_done`), which is what lets `fathom doctor` answer "who is indexing this right now, and how far along" from a file. ADR-0003 §6.4's same reasoning: the body is the diagnosis, the kernel is the lock. |
| L4 | **Readers take no lock.** The TUI, `report` and `doctor` open rows and blocks and read them; §6.1's publish rule means each file is whole or absent, and a run that is mid-index means some rows are from the previous run, which is a coherent store, not a broken one. |
| L5 | The lock guards a **cache root**, not a workspace. Two Fathoms on one workspace with different `--cache-dir` values do not contend (they write different stores); two on one cache root, one branch or two, do. |
| L6 | Scope is one writer, not one writer-per-subtree. Rows and blocks are individually atomic, but `run.json`, `sources.json` and prune's decisions are only coherent under a single writer, and a finer lock here would be a lock nobody can reason about. |

`.act-mcp.lock` is the precedent in both shape and message: a dot-file lock at the workspace level,
advisory JSON inside, `flock` for liveness, never unlinked, and a refusal that names the holder. The
only difference is scope, and L5 states it.

## 10. Failure modes, each with the line the user sees

`act-mcp`'s refusals are one line naming the offending value plus a `fix:` that changes it
(`src/config.rs`); Fathom's follow the same shape, and every one of them is a boot-time or
point-of-failure refusal rather than a panic or a silent degradation. "Unknown" stays unknown: a
cache that cannot be written must produce `—` in a column and a `warn` in `doctor`, never a 0.

### 10.1 No write access — a read-only workspace, a read-only mount, a root that cannot be created

```
cannot write the cache root /srv/repo/.fathom: permission denied
  fix: pass --cache-dir <path> on a filesystem you can write (ADR-0006 §3), or
       make the directory writable; Fathom will read an existing store without write access
```

- **`fathom index` refuses.** Indexing needs a writable store; pretending otherwise produces a run
  that "succeeded" having cached nothing.
- **`fathom tui` and `fathom report` proceed read-only** when `index/` exists and its guard passes
  (§8): a build agent that mounted the workspace read-only can still read a store someone else
  built, which is exactly the read/write split the evidence contract has. If the store is absent:
  `no metric cache in this workspace` + `fix: run fathom index …` — and every column renders the
  unknown glyph rather than a zero.
- Boot checks write access with an actual `open(2)`, not `access(2)` (ACLs, read-only mounts and
  squash-on-NFS all make `access` a liar) — and the check is *not* a `write` of a probe file: an
  `O_RDWR` open of `fathom.lock` is the probe, and for a writer it is also where the lock of §9 is
  taken. A reader that only needs read access does not probe for write access at all: it opens rows,
  not the lock.
- A root that does not exist is created (§4.1's modes) along with `LAYOUT.md`; if the parent is
  unwritable that is the same refusal, with the parent named.

### 10.2 Out of space (and out of inodes)

`ENOSPC`/`EDQUOT`/`EFBIG` while publishing a row or block: the temp file is unlinked, the failed
path is recorded in `run.json` (`error`, `interrupted`), the run stops cleanly, and the store is
**exactly as usable as it was before** — §6.1's whole point. `doctor` reports bytes on disk, block
and row counts, and the last run's error.

```
out of space writing the metric cache (index/blocks/2f/…): No space left on device (os error 28)
  fix: free space, or point --cache-dir / $FATHOM_DIR somewhere with room, then run
       fathom index --rebuild (the store is disposable: ADR-0006 §3.2)
```

Deliberately **not** done: automatic pruning-to-make-room mid-run. A cache that silently shrinks
itself under a TUI is a cache whose numbers change depending on disk pressure, and §11's prune runs
only where the user opted in.

### 10.3 A second Fathom on the same cache root

```
another Fathom is indexing this cache root (pid 41234 on daniels-mac-mini.local, phase 'rows',
started 2026-11-16T12:45:21Z): index_busy
  fix: let it finish, or run fathom doctor to see what holds /repo/.fathom/fathom.lock
```

Fail-fast (L2), retryable, and `doctor` answers the "who" from `fathom.lock`'s body (L3). A lock
whose holder is gone is simply unlocked: `flock` died with it, so there is no stale-lock recovery, no
`--force`, and nothing that can break a live writer's run.

### 10.4 A foreign file, or a name already taken

```
/repo/.fathom/index exists but is not a Fathom store (no meta.json)
  fix: this directory belongs to another program. Point --cache-dir / $FATHOM_DIR somewhere else,
       or delete /repo/.fathom/index if you are sure it is dead (ADR-0006 §2.2, §4.2)
```

and the same shape for `index` being a file, `blocks` being a symlink out of the root, `LAYOUT.md`
being a directory, or `meta.json` failing to parse. The rule that generates these messages: Fathom
never guesses about a path it owns but did not write, and never deletes a path it did not write.
`doctor` separately names foreign entries (§4.2) — including the two this repository actually has,
`index.sqlite3` and `objects/` — as an advisory note rather than a finding (§10.4), so the answer to "what
is this 2 MB file" is a command away rather than a git blame.

### 10.5 A damaged store

| damage | behaviour | the thing to delete |
|---|---|---|
| a row that will not parse, or whose `commit` ≠ its filename | that commit is re-derived on the next index; `doctor` counts it as `warn` and names the path | the one file, or nothing (§6.1 makes this rare: a torn file should be impossible) |
| a block that will not parse | it is a miss: recompute and re-publish over it; `doctor` counts it | the one file |
| `meta.json` unreadable | refuse, rebuild | `rm -rf <root>/index` |
| `sources.json` unreadable or older than the last run | rebuilt by stat-ing the evidence root (§6.2) | the one file |
| `run.json` unreadable | it is a report, not an input: start a fresh one, `doctor` notes the gap | the one file |
| `rows/<xx>/` or `blocks/<xx>/` contains an unexpected name | ignored by readers, listed by `doctor`, deleted only by prune's unreferenced pass (§11.2) | nothing by hand |
| a directory that should hold rows is missing | cold path for those commits | nothing |

There is no `fathom doctor --fix` in v1: every repair above is a delete of a named path or a
`fathom index`, and a tool that deletes files as a side effect of a diagnostic command is a tool you
do not run in someone else's checkout.

### 10.6 The other inputs

| case | behaviour |
|---|---|
| no evidence root | normal and legal (mact-by1.5's boot rule: warn, do not fail): coverage/benchmark columns are unknown, `doctor` says where it looked and that `.github/workflows` may have nothing to run |
| evidence root unreadable | refuse with `fix:` naming `--evidence-dir` / `$EVIDENCE_DIR` |
| evidence file unreadable or half-written | that *file* is skipped and counted (ADR-0002 §7's downstream policy: permissive on values, strict on shape), `doctor` names it, and the row that lost it is marked |
| workspace is not a git repo, or has no commits | refuse (by1.5's boot validation), `fix:` names `--workspace`/the path |
| a row's commit is gone (history rewritten, branch deleted) | the row is outside the walk set and gets pruned (§11.2); nothing breaks, because nothing keys off a stale id |
| clock jumps / a row from the future | timestamps are informational and stamped by the writer; nothing orders on them (§6.3's keys contain no clock) |
| `fathom.toml` appears or changes mid-run | the run keeps the config it parsed at boot and stamps its digests with it; the next run picks up the change. Two configs in one run is how a cache gets poisoned |

## 11. Pruning, budgets, and determinism

### 11.1 What bounds the store

| bound | mechanism |
|---|---|
| rows | at most one per commit in the walk set: `history.refs` × `history.depth` × `history.first_parent` (default `HEAD` × 500 × full) |
| blocks | only blobs/paths the walk set touched, per analyzer version, per config digest |
| file count | `cache.block_cap` (default 200 000) triggers prune; buckets are 2-hex-bucketed, so a bucket holds `1/256` of the total (≈ 800 entries at the cap) — the reason git buckets the same way |
| total bytes | whatever the above produces; `doctor` prints it, split rows/blocks |

### 11.2 Prune (P12)

Runs only under the lock, only when `cache.prune_on_index` is true (or `--rebuild`), and only after a
successful index — never as a way to make room for a failed run (§10.2):

1. **rows**: delete a row whose commit is not in the current walk set. This is the bound that makes
   `history.depth` real: raising it grows the store, lowering it shrinks it, and nothing else changes
   (§7.2's "depth is not a cache key").
2. **blocks**: collect the block keys referenced by the rows that survived (`mi.blocks`, and the
   same field in whatever ADR-0007 adds later), and delete unreferenced blocks older than the run.
   Blocks are cheap to recompute and expensive to keep forever, so a block's life is "referenced by a
   live row", and `cache.block_cap` trims the rest least-recently-published-first.
3. `run.json` records what went (`rows_written`, `blocks_pruned`), so `--status` answers "did the
   last run think?"

Prune deletes **only** paths under `index/rows/` and `index/blocks/` matching
`^[0-9a-f]{2}/[0-9a-f]{40}\.json$` and `^[0-9a-f]{2}/[0-9a-f]{64}\.json$`. Anything else is a `warn`
for a human (§10.5), because the cheapest way to lose a user's data with a GC is a bug in a pattern
that matched something else.

### 11.3 Determinism, which is what "just rebuild" means

Two runs with the same (`git log`, evidence, config, Fathom build) produce **byte-identical** files:
keys are digests (§6.3), so the only input is the bytes of a value; row and block JSON is written
with the canonical key order of ADR-0002 §6.1; lists are sorted by `(path, key)`; no wall-clock
value appears in a row's *inputs*, only in `indexed_utc`/`built_utc`/`run.json`, which are excluded
from every key. This is not a nicety: it is what makes a rebuilt store provably the same store, so
§8.1's "rebuild instead of migrate" is testable — a test can compare a rebuilt tree against a
committed fixture, byte for byte, and a golden block fixture (§6.3) is only meaningful because of it.

### 11.4 Budgets, and the evidence that would reopen §5

Owned by mact-by1.10's test; pinned here so the test has something to assert against:

| budget | value | measured how |
|---|---|---|
| warm re-index with no change | 0 rows written, 0 blocks written, 0 blob analyses; one `stat` per evidence file and one read per row | synthetic ~1000-commit repo and this repo, `index` twice, assert on `run.json` counters ("by count, not vibes", per by1.10) |
| incremental cost | O(new commits × files they touched), never O(repo) | the same test, halved repo vs whole |
| cache hit ratio after a warm re-index | 100 % of lookups on the second run, and the run after a `paths.noise` edit must hit every block §7.3 predicts | §7.3's table is the assertion list |
| file count | ≤ `cache.block_cap`, and ≤ 100 000 on the synthetic repo | prune's own test |
| wall clock | no number pinned in this ADR on purpose: it is confident about the access pattern, not about 2026 hardware | by1.10 records the measured figures for the synthetic repo and this one in its PR. If plain files cannot clear an incremental index of ~1000 commits, that measurement is the §5 reopening — and the fix to try first is **packing** (`rows/<shard>.pack`: keys and digests unchanged, same 2-hex bucketing, still no engine), not sqlite |

The last row is the honest version of "no sqlite": this ADR is confident about the access pattern and
not about 2026 hardware, so it names the measurement that would change its mind, and the cheaper
thing to try first. Keys survive packing, which is exactly why §6.3 was worth pinning in an ADR.

## 12. `.gitignore`, `LAYOUT.md`, and the end of the orphaned whitelist (P10)

### 12.1 This repository

The whole diff, replacing the six `.fathom` lines that are there today:

```
# fathom metric cache (local only): docs/adr/0006-fathom-workspace-layout.md
# Everything under .fathom/ is disposable cache; the only committed file is LAYOUT.md, the
# on-disk contract that lives inside the cache root. Any other whitelist entry here must be
# listed in that ADR's §12 or it is dead: scripts/validate_fathom_layout.py says so.
.fathom/*
!.fathom/LAYOUT.md
```

Removed, each for a stated reason rather than by taste:

| removed | why it was there, and why it is dead |
|---|---|
| `!.fathom/reviews/` | a whitelist for a *committed* human-artifact directory in the other program's model (its `decision_note`/`ledger_entry`/`review_state` tables mirror markdown back and forth — every one of them is empty). Fathom has no review surface; the epic has no triage screen. The directory does not exist and no tracked file lives there. |
| `!.fathom/ratchet.toml` | same program (its `graph.cycles` analyzer consumed `ratchet.*` config keys). Fathom's ratchet is `fathom report --fail-on …` (mact-by1.13) and its baseline is `fathom.toml` (§7.2) — one committed config file, not two, and not one hidden inside a cache directory. |
| `.fathom/**/*.tmp` | existed to keep temporaries of a whitelisted `reviews/` out of git. With nothing un-ignored below the root, `.fathom/*` already ignores everything at any depth: once git will not descend into an ignored directory, a deeper pattern cannot un-ignore anything it had missed. It is not "harmless", it is *unreachable* — and an unreachable rule is the one that misleads the next reader. |
| `.fathom/**/*.sqlite3-*` | same story, and now doubly dead: Fathom writes no sqlite (§5), and the foreign `index.sqlite3-wal`/`-shm` are already covered by `.fathom/*`. |

The rule that keeps the two documents honest, and the ticket's acceptance criterion made
executable: **every `.fathom` line in `.gitignore` appears verbatim in the block above, and every
whitelist entry appears in §4.1's tree.** C2 of the checker compares the block literally and the
whitelist set to the tree, and C3 compares the tree inside `.fathom/LAYOUT.md` to §4.1's, so an
orphan is a failing build rather than another six months of mystery. C2 also refuses a rule git can
never reach — a pattern naming a path below `.fathom/*` while the rule for `.fathom/*` itself stands,
which is four of the six lines this section deletes — so a rule that only *looks* like a decision stops
being added to this file. If another program genuinely
wants a committed file under `.fathom/`, the act is: add the line here *and* to §4.1 in the same PR —
one reviewable line, which is how this repo already treats a new licence in `deny.toml`.

### 12.2 Someone else's repository

Symmetric with ADR-0002 §6.4, because a cache directory that dirties the repo it is measuring is the
same bug twice: when Fathom **creates** a cache root inside a git working tree it appends, idempotent
and marker-guarded, never reordering anything, skipped silently if the tree is not a git repo or
`.gitignore` is unwritable, and verified with `git check-ignore`:

```
# fathom cache (local only)
.fathom/
```

Two differences from the writer's version of this rule, both deliberate: the whole directory is
ignored there (an auto-appended block must be the dumbest possible thing; committing `LAYOUT.md` is a
choice a repository makes for itself, as this repo has), and it is skipped when
`git ls-files <root>` is non-empty — a workspace that already tracks something in its own cache
directory has an opinion, and Fathom's append would silently contradict it.

## 13. Verification

| claim | check | how |
|---|---|---|
| the ADR is accepted, complete, and states every token of the ticket's decision list | C1 | `python3 scripts/validate_fathom_layout.py` |
| `.gitignore` carries the §12.1 block verbatim and contiguously — and if it does not, the finding names the line of the block that differs | C2 | same |
| no orphaned whitelist entry: every `!` rule names a top-level entry of §4.1's tree, and git tracks something under it (git's index is what an ignore rule governs; a tracked entry this deleted-cache-root does not have is a `NOTE` with the restore command, not a verdict) | C2 | same |
| no `.fathom` rule is unreachable: a rule naming a path below `.fathom/*`, which git never descends into while the directory-level rule stands, is refused as the dead rule it is (§12) | C2 | same |
| `.fathom/LAYOUT.md` exists, is tracked, and its tree block equals §4.1's | C3 | same |
| `.fathom/LAYOUT.md` is not duplicated anywhere (one copy of the contract's bytes) | C3 | same |
| no sqlite, no embedded store in the dependency graph (`rusqlite`, `libsqlite3-sys`, `redb`, `sled`) and both bans are present in `deny.toml` | C4 | same, reading `Cargo.lock` + `deny.toml` |
| the ignore rules ADR-0002 owns (`.evidence/`, `.act-mcp.lock`) survived this ADR's diff | C5 | same |
| documentation wiring: ADR-0006 in README's ADR list, named in `docs/ci.md` and `docs/testing.md`, and this checker wired into `scripts/ci.sh` | C6 | same |
| a foreign artifact physically present in this checkout is either Fathom-owned per §4.1 or named in §2's disposition — and one that is named by neither is a `NOTE`, because §4.2 already decided it stays and §10.4 asks for an advisory note, not a finding | C7 | same (skipped, loudly, when `.fathom/` is absent, which it is on a fresh clone: it is gitignored) |
| a foreign artifact that *obstructs* a name Fathom owns — `index` a file, `LAYOUT.md` a directory, a store path symlinked out of the cache root — is a finding, because that is the case where deleting the cache stops being safe (§4.2) | C7 | same, comparing each existing §4.1 path against the shape §4.1's tree gives it |
| the store rules are stated as rules: atomic publish, key derivation, refuse-on-version-mismatch, the `fix:` strings, readers-need-no-lock | C8 | same (token assertions on this document, the same technique as `validate_crate_layout.py`'s C1) |
| `fathom.toml`'s key table is complete and every key is claimed by a consumer | C9 | same: the §7.2 table is parsed and every key must appear in the declared-consumer column |
| the checker is not a lie: each named check fires when the rule it names is broken — as a finding, or as the advisory note §4.2 asks for, and nothing else fires | C10 | `python3 scripts/validate_fathom_layout.py --self-test` (builds a scratch fixture, breaks one rule at a time, asserts the named check fires and only that check) — the `scripts/crate_layout_drill.sh` idea, self-contained |
| the code keeps its side of the pins: per-analyzer declared keys are honest (§7.3), golden block fixtures exist (§6.3), warm re-index writes nothing (§11.4) | by1.7 / by1.10's Rust tests | named there; this ADR states what they must assert |
| cargo-deny accepts the new bans | mact-by1.16 | `cargo deny check` in the `supply-chain` step |

## 14. Consequences

**Good**

- A reader that adds no dependency class: `crates/fathom` grows `serde`, `serde_json`, `toml`,
  `globset` and `sha2` (all pure Rust, all inside ADR-0005 §6's budget for it), and the repo's
  no-C-toolchain property survives its first storage decision. Five release targets, one compiler.
- The store is `ls` and `jq`. "What does Fathom think this commit is?" has a `cat` answer, which is
  worth more in a diagnostic tool than any amount of engine sophistication.
- Crash safety, atomicity and mode discipline are re-used from ADR-0002 rather than newly specified —
  one mechanism, one set of tests, one thing for a maintainer to remember.
- Config invalidation is proportional and *testable* (§7.3), which is the difference between an
  index people trust and one they occasionally "just rebuild".
- The workspace's `.fathom/` stops being a mystery: seven named kinds of path in it, everything else
  named as foreign by `doctor`, and the ignore file says the same thing the ADR says.
- Version mismatch has one answer — rebuild — and it is cheap enough that nobody is tempted by a
  migration path in a dark moment.
- The two empty-by-design whitelist entries are gone, so nobody future-proofs a product into `.gitignore`
  again.

**Costs, and who pays**

- No query engine. Anything that wants a *query* over rows — "the worst MI in the last 200 commits",
  "all commits where coverage dropped" — must load the window's rows. At `history.depth = 500` that
  is ~500 small JSON files, so the answer is the timeline's own read path (§0's table); if a real
  need for a cross-commit index ever appears, §11.4's packing hatch or an amended §5 covers it, and
  **the keys survive either way**, which is the property that made deciding keys here worth it.
- Many small files: prune is a directory walk, `doctor`'s counts are `readdir` rather than
  `SELECT count(*)`, and a pathological repository with 10⁵ distinct blobs on a network filesystem
  is where §11.4's measurement gets taken seriously. Buckets and `cache.block_cap` are the mitigation
  named in advance.
- One writer per cache root (§9): two shells running `fathom index` on one workspace get
  `index_busy`, and the fix is to wait. That is the same trade act-mcp made for runs, and here the
  thing serialised is a local cache.
- No migrations (§8.1): a Fathom that changes `schema_version` forces a full re-index on every
  checkout — minutes on a normal repository, which is why §11.4's budget has to stay honest.
- The Rust side still owes the work this ADR describes: by1.10 implements §4–§11, by1.5 implements
  §3 (including the three roots' flags and their refusals) and §7's `Config`, by1.7 declares its keys
  per §7.3, by1.13 implements the `doctor` half of §10. Every `fix:` string above is a test fixture
  by then, and none of them may drift from this page without an edit to it.

## Considered alternatives

- **Adopt the existing `.fathom/index.sqlite3`.** **Rejected** (§2.2). It is another program's live
  store — its newest `index_run` is stamped to the second of this repository's `HEAD` commit, so a
  hook of that program is writing it on the author's machine; Fathom would be sharing a file with a
  writer whose source it does not have, negotiating WAL with a stranger. Its schema is a different
  product's (`episode`, `ledger_entry`, `decision_note`, `triage_event`, `review_state`,
  `attribution_correction` with `author_kind ∈ {agent, human}`): Fathom has no reader for any of it,
  and five of its tables are already empty. It also drags in sqlite, which everything below rejects
  independently. Its `reviews/`/`ratchet.toml` ignore entries go the same way, because they name a
  product this epic does not have.
- **sqlite via `rusqlite` + `bundled`.** **Rejected** (§5.2), and the honest summary is that it solves
  a problem this product does not have — there is no query workload, no join, no range scan, and the
  one ordering that matters comes from `git log` — while adding a C toolchain to five release
  targets (musl included), a build-script and bindgen matrix into the process that parses untrusted
  repositories, a WAL/-shm recovery story, and a migration obligation (§8.1). Its licence would have
  been fine; the toolchain and the machinery were the objection.
- **A pure-Rust embedded store (`redb`, `sled`).** **Rejected**, same analysis minus the C compiler:
  a transaction log and compaction are things `rename(2)` and `rm -rf` already give this design for
  free, and both engines put a second recovery path between "delete the cache" and "the cache is
  gone". Rejected, not banned (§5.2), so the amendment is cheap if this is wrong.
- **JSON + content-addressed blobs, but keeping sqlite-shaped machinery** — a single JSONL append log
  plus a snapshot: **Rejected.** It re-creates the append hazards ADR-0002 §6.2 explicitly removed
  (a torn line, a reader mid-write, an `O_APPEND` across filesystems) in order to avoid… a
  `rename`, which is the mechanism it would have to use anyway to publish the snapshot.
- **One JSON file for the whole index.** **Rejected.** A 500-row store would be fine and a 5 000-row
  store would mean rewriting the world per commit, no incremental write, no crash safety, and no
  "delete the one broken row" (§10.5). It is the same mistake as a single `run_state.json`, and
  act-mcp already learned it in the registry.
- **A git-style pack file.** **Rejected for v1, and named in §11.4 as the escape hatch.** Packing
  pays off when objects vastly outnumber their contents' entropy — millions of loose objects, network
  transfer, delta compression. Here the count is bounded by the walk set, the values are numbers, and
  the cost would be an index-of-the-index plus a repack pass: precisely the machinery §5 chose not to
  have. Keys are unchanged by packing, which is why deferring it is safe rather than expensive.
- **The cache root somewhere else** (`$XDG_CACHE_HOME/fathom/<hash-of-path>`, `.fathom-cache/`,
  `target/fathom`): **Rejected.** ADR-0005 §9 already names it; a cache next to the thing it
  describes is discoverable ("what is this directory?" has a `LAYOUT.md` inside it); a hashed path in
  `$XDG_CACHE_HOME` is invisible to the user who needs to delete it and un-deletable by the person who
  inherited the machine; and `target/` is `cargo clean`'s to remove, which would silently destroy a
  cache that is not a build artefact. `$FATHOM_DIR` exists precisely so a user whose policy is "keep
  caches out of the tree" can say so in one place.
- **`fathom.toml` inside `.fathom/`,** beside the store it configures: **Rejected.** The config is
  committed, reviewable, team-shared input to every number (so it belongs with the code it describes,
  and gets reviewed in the same PR), while the directory it would sit in is gitignored, disposable,
  and possibly not even on the same filesystem (§3.1). The bootstrap loop — config naming the cache
  that contains the config — is the technical reason; discoverability is the human one.
- **Layered config** (user-wide → workspace → flags): **Rejected** (§7.1). One file keeps "what were
  the inputs of this number" a single-path answer, which is exactly what §7.3's digests and
  `doctor`'s "why is this number what it is" need.
- **A migration path on `schema_version` mismatch:** **Rejected** (§8.1). Its test is a byte-comparison
  against a cold rebuild, so it costs the fixture forever, and rebuild is already correct.
- **`fathom doctor --fix`,** which deletes the offending file: **Rejected** (§10.5). A diagnostic that
  deletes files in a user's checkout is a tool you cannot run while you are nervous, and every repair
  here is one `rm` of a named path away.
- **No lock at all, or a bare body-less one** ("it is only a cache", or "one person uses this repo"):
  **Rejected.** P9/L1–L6; the
  `.act-mcp.lock` precedent exists in this very directory listing and its body is what made
  `workspace_busy` answerable across processes.

## Open questions (owned elsewhere)

- The MI formula, its language set, and exactly where a value stops being path-independent — ADR-0007
  (mact-by1.3), subject to §6.3's `input_kind` rule and §7.2's declared keys.
- Coverage-% derivation, benchmark grouping/dedup and durable action success — ADR-0008 (mact-by1.4).
  The row's *fields* are theirs; their digests are §7.3's.
- Whether the store grows a "commits that have no evidence, please measure" queue that by1.17's
  deferred run-triggering feature would consume. §6.4's `gaps.no_evidence` is where it would start;
  acting on it is ADR-0005 P5's amendment.
- Whether `fathom.toml` ever grows a `[fail_on]` section for CI baselines. by1.13 owns `--fail-on`; if
  the flags multiply, the section belongs here (§7.2), not in a `ratchet.toml`.
- Whether `.fathom/LAYOUT.md` should also be regenerated when the ADR's tree changes (it is
  `fathom`-written-if-absent, and C3's equality check makes drift a build failure; deciding to add a
  `--force` writer is a docs ticket's call, mact-by1.15).
- Whether the foreign store in this checkout gets deleted by its owner. Fathom's position is fixed by
  §2.2: untouched, un-read, gitignored, and named by `doctor`.
