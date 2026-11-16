---
id: ADR-0005
title: fathom lives in a Cargo workspace as its own crate and reads evidence through act-mcp's own record types, not a new MCP tool
status: accepted
ticket: mact-by1.1
date: 2026-11-16
---

# ADR-0005: where fathom lives, and what it may depend on

- **Status:** Accepted
- **Date:** 2026-11-16
- **Ticket:** mact-by1.1
- **Decision:** **Option B** — one Cargo workspace, three crates, one lockfile:
  `crates/act-mcp` (the server, writer, validator CLI — surface unchanged),
  `crates/fathom` (the reader/TUI, its own binary, its own dependency tree), and
  `crates/act-evidence` — the lean contract crate that `src/evidence/{records,shapes,schema,validate}.rs`
  become, depended on by **both** of the others. The edge `act-mcp ↔ fathom` does not exist in
  either direction. **Fathom is a reader of `.evidence` files, not a client of the server**: no new
  MCP tool, no MCP client dependency, nothing in fathom that can start a run.
- **Rejected:** a `fathom` `[[bin]]` inside the act-mcp crate (drags ratatui into the server's tree
  and puts a TTY-owning program in the crate whose tests assert stdout stays empty); a separate
  repository (the two halves share the record types and the fixture trees); fathom parsing
  `.evidence` with its own structs (a second implementation of the contract — the failure mode
  ADR-0002 §7 exists to prevent); fathom depending on `act_mcp` wholesale; a fathom-facing MCP tool.
  Each with its reason under [Considered alternatives](#considered-alternatives).
- **Machine-readable form:** none new on purpose. The load-bearing facts of this ADR are already
  expressible in Cargo: `[workspace] members`, `[workspace.package]` (version / edition /
  rust-version / publish), `[workspace.dependencies]`, `[workspace.lints]`, and each crate's
  `[dependencies]` table. Adding a fourth spelling of "who may depend on whom" would be the same
  mistake as a second record schema.
- **Checked by:** [`scripts/validate_crate_layout.py`](../../scripts/validate_crate_layout.py)
  (runs in `scripts/ci.sh`'s `schema` step beside the other ADR checkers), rehearsed against a
  synthetic post-split workspace by
  [`scripts/crate_layout_drill.sh`](../../scripts/crate_layout_drill.sh). It reads the manifests and
  `Cargo.lock`, not prose, and it enforces C1–C13 of this ADR — including in the pre-split
  single-crate state it was written in, so the split cannot land and silently break a rule on the way
  in.
- **Related:** [ADR-0001](0001-tool-contract.md) (the tool surface this ADR refuses to grow),
  [ADR-0002](0002-evidence-jsonl-contract.md) (the contract `act-evidence` implements; §7's
  "update both halves in one change" rule is *why* there is one reader-side implementation),
  [ADR-0003](0003-commit-isolation-and-serialization.md) (the no-libgit2 precedent §6 re-uses),
  [ADR-0004](0004-run-lifecycle.md) (runs outlive requests; fathom reads what they leave behind)
- **Consumers inside the epic:** mact-by1.5 (executes the split and the `fathom` CLI skeleton — the
  first crate split this repo has ever done), mact-by1.6 (the evidence reader: adds `read` to
  `act-evidence`), mact-by1.7 / mact-by1.8 (MI analyser, git reader: fathom-side deps), mact-by1.9 /
  .11 / .12 / .14 (the ratatui screens), mact-by1.10 (the `.fathom` index — ADR-0006's store),
  mact-by1.13 (`fathom report` / `fathom doctor`, headless), mact-by1.15 (docs), mact-by1.16 (CI:
  `-p` targeting, cargo-deny for the fathom tree). ADR-0006 and ADR-0008 own the on-disk `.fathom`
  store and the metric semantics respectively; neither may reopen the dependency edges pinned here.

## 1. Context

Fathom is the downstream cross-commit scanner that
[`docs/evidence-format.md`](../evidence-format.md) was written for and that ADR-0002 §4.4 assigns
dedup, percentiles, outlier rejection and regression detection to. Before any of it is built, four
questions have to be answered once, because every later ticket in the epic picks an answer
differently and two of them are expensive to undo:

1. **Repository shape.** New crate beside the server, another binary inside the server's crate, or a
   second repository?
2. **Where the record types live.** If fathom parses `.evidence` with its own structs, the contract
   has two implementations on the read side, and ADR-0002 §7's anti-rot argument — one ADR, one
   schema, one implementation, all three changed together — dies of exactly the thing it named.
3. **What fathom may *not* depend on.** "It reads the output of the MCP" is one word away from "it
   calls the MCP", and that reading would put a run-triggering client inside a TUI.
4. **The boring half:** package/library/binary names, whether the MSRV floor travels, workspace
   lints, and whether fathom is `publish = false` like act-mcp.

Facts on the table, all of them current as of this ticket:

- act-mcp is **one package** today: root `Cargo.toml`, `lib = act_mcp` (`src/lib.rs`), `bin =
  act-mcp` (`src/main.rs`), `rust-version = "1.88"` (rmcp 3.9's own floor — lowering it breaks the
  2026-07-28 protocol plumbing), `publish = false`, one `Cargo.lock` with 235 packages, one
  `rust-toolchain.toml` pinning 1.99.1.
- `src/evidence/` splits cleanly. `records.rs` (2026 lines), `shapes.rs`, `schema.rs` and
  `validate.rs` import only `serde`, `serde_json`, `chrono`, `thiserror`, `jsonschema`, `schemars`
  and each other. `writer.rs` additionally needs `libc::gethostname` for the envelope's `host`, and
  the two extractors (`evidence/{benchmark,coverage}`) are `#[cfg(unix)]` and built on
  `crate::runner::ActRunOutcome` — the writer's half, not the reader's.
- The strictness fathom needs is already written, twice over and cross-checked: `records.rs` makes a
  `null`, an unknown `kind`, a non-finite `value`, a foreign `schema_version`, a coverage field on a
  benchmark line and a traversing path *unrepresentable*, and `validate.rs` runs the pinned JSON
  Schema on top. Re-implementing that in a second language of structs is the one thing on the table
  that is a plain negative.
- `tests/boot.rs` asserts `assert_eq!(stdout, "", "stdout must stay empty")` four times. A TUI is a
  program whose entire job is writing to stdout and putting the terminal in raw mode.
- The release path is single-binary and single-version: `cd.yml` takes the tag, runs
  `cargo set-version "$version"`, `scripts/ci.sh release`, then tars `target/release/act-mcp` as
  `mact-<target>.tar.gz`. The last four commits on this branch were about getting that version sync
  to stop flaking.
- The epic's non-goals already forbid three things this ADR has to make structural: no new MCP tool,
  no aggregation/dedup/regression logic inside act-mcp, no network in fathom.

## 2. Decision, in one screen

```
     act-mcp ───────▶  crates/act-evidence  ◀─────── fathom
 (server, writer, CLI)     (the contract)      (reader, TUI, report)

     ✘ there is no edge between act-mcp and fathom, in either direction ✘
```

Numbered pins; `MUST` / `MUST NOT` are normative, and each is either enforced by
`scripts/validate_crate_layout.py` (the check is named) or is a judgement call named as such.

| # | pin |
|---|---|
| P1 | A Cargo workspace with a **virtual root manifest** and members `crates/act-evidence`, `crates/act-mcp`, `crates/fathom`. One lockfile, one `target/`, one `deny.toml`, one toolchain file. (C2) |
| P2 | `crates/act-evidence` holds the record types, the shape predicates, the embedded schemas and the tree validator; it is the **only** implementation of reading `.evidence`. (C5, C11) |
| P3 | `crates/act-mcp` keeps the writer, the extractors, the runner, the supervisor and the server; it consumes `act-evidence` and re-exports it as `act_mcp::evidence` so nothing else has to be rewritten. |
| P4 | `crates/fathom` never depends on `act_mcp` (not even as a dev-dependency) and its tree never contains `rmcp`, `axum`, `tower`, `tokio` or an HTTP client. (C6) |
| P5 | The MCP surface stays four tools. Fathom talks to no server. (the tool list is already pinned by `schema/tool-contract.schema.json` / `scripts/validate_tool_contract.py`; C6 keeps the client side out of fathom) |
| P6 | Names: packages `act-evidence` / `act-mcp` / `fathom`; libs `act_evidence` / `act_mcp` / `fathom`; bins `act-mcp` / `fathom`. (C10) |
| P7 | `version`, `edition`, `publish` and `rust-version` are set once in `[workspace.package]`; members inherit. The 1.88 floor stays 1.88 for the server no matter what the UI's dependencies want. (C3, C4, C7) |
| P8 | `[workspace.lints]` carries what is genuinely workspace-wide (`unsafe_code = "deny"`); `missing_docs` stays a crate attribute in each `lib.rs`. (C8) |

## 3. The workspace (P1)

```
Cargo.toml                # [workspace] members + [workspace.package] + [workspace.dependencies] + [workspace.lints]
Cargo.lock                # the only one (C2)
rust-toolchain.toml       # unchanged, workspace-wide: 1.99.1 + clippy + rustfmt
deny.toml                 # unchanged, workspace-wide; cargo-deny resolves the whole workspace
schema/                   # unchanged: evidence.schema.json, run-meta.schema.json, tool-contract.schema.json (C11)
docs/ scripts/            # unchanged
crates/
├── act-evidence/
│   ├── Cargo.toml
│   └── src/lib.rs  records.rs  shapes.rs  schema.rs  validate.rs   # moved, not rewritten
│                    read.rs                                        # added by mact-by1.6 (§7)
├── act-mcp/
│   ├── Cargo.toml
│   ├── src/…                     # as today, minus the four moved files
│   └── tests/                    # moved from ./tests by mact-by1.5 (Cargo requires tests/ in the package)
└── fathom/
    ├── Cargo.toml
    ├── src/main.rs               # thin: parse args, call the lib
    ├── src/lib.rs                # ui/, git/, mi/, index/, report/ as the epic adds them
    └── tests/
```

Rules:

- **Virtual root.** The root `Cargo.toml` has `[workspace]` and no `[package]`. A root that is also a
  package makes `cargo build` at the root mean "build the server" by accident and gives act-mcp a
  privileged position the dependency graph in §2 says it does not have.
- `members = ["crates/act-evidence", "crates/act-mcp", "crates/fathom"]`, `resolver = "2"`, and
  every directory under `crates/` is a member — no orphan crate that `cargo build` at the root never
  sees (C2).
- `default-members` is left at its default (all members), so `cargo build`, `cargo test` and
  `cargo clippy --all-targets --all-features -D warnings` keep covering everything, which is what
  `scripts/ci.sh`'s `fmt`/`clippy`/`test` steps already assume.
- **Selective release building.** `scripts/ci.sh`'s `release` step changes from
  `cargo build --release` to `cargo build --release -p act-mcp --bin act-mcp` (and, when by1.16 adds
  it, a separate `-p fathom --bin fathom` job). A broken TUI must never block shipping the server,
  and the artefact `cd.yml` tars has to stay exactly one binary.
- `/target` in `.gitignore` needs no change: a workspace shares the root `target/`, so `cargo clean`
  still cleans everything and the ignore rule is still anchored correctly.

## 4. What moves into `crates/act-evidence` (P2)

Moved as-is, in the same commit that adds the crate (mact-by1.5), with only `use` paths and the two
`include_str!` depths changing (`../../schema/…` → `../../../schema/…`):

| today | destination | why |
|---|---|---|
| `src/evidence/records.rs` | `act-evidence::records` | the strict types are the contract; both sides need them |
| `src/evidence/shapes.rs` | `act-evidence::shapes` | `records` uses it; hand-written matchers, no deps of its own |
| `src/evidence/schema.rs` | `act-evidence::schema` | embeds `schema/*.json`; one copy of the bytes, one compiled validator (`C11`) |
| `src/evidence/validate.rs` | `act-evidence::validate` | `validate_tree` walks a tree with `std::fs` + the schema only — the walk is exactly what fathom's reader and `fathom doctor` (by1.13) will re-use |
| `src/evidence/writer.rs` | **stays** in `act-mcp` | it publishes files (temp+`fsync`+rename), stamps `host` via `libc::gethostname`, and edits `.gitignore`. A reader must not be able to write |
| `src/evidence/{benchmark,coverage}*` | **stay** in `act-mcp` | extractors are `#[cfg(unix)]` and built on `crate::runner::ActRunOutcome` — act's log, act's payload directory |
| `src/evidence/mod.rs`'s `pub use records::{…}` | stays, re-exporting from `act_evidence` | see §5 |

`act-evidence::SchemaVersion` keeps the reader-side rule ADR-0002 §7 asks of every reader: a record
whose `schema_version` this build does not know is refused, never "upgraded". Fathom inherits that
behaviour by construction rather than by re-implementing it, and inherits ADR-0002 §7's downstream
policy — permissive on *values* (unknown `category` / `flag` / `source` members, unknown fields),
strict on *shape* — because that policy is already what these types enforce.

## 5. What `crates/act-mcp` keeps, and the re-export that makes the move cheap (P3)

`src/evidence/mod.rs` becomes:

```rust
pub use ::act_evidence as evidence;           // records, shapes, schema, validate
pub use ::act_evidence::{validate_tree, …};   // today's re-export list, from its new home
pub mod writer;                               // stays
#[cfg(unix)] pub mod benchmark;               // stays
#[cfg(unix)] pub mod coverage;                // stays
```

so `act_mcp::evidence::records::BenchmarkRecord`, `act_mcp::evidence::Kind`,
`act_mcp::evidence::validate_tree` and every intra-doc link in the epic's tests and docs keep
resolving. That is deliberate: `tests/evidence_contract.rs`, `tests/evidence_golden.rs`,
`src/tools/category.rs` and a dozen doc comments name those paths, and a split whose diff is "plus a
re-export" is a split that can be reviewed in ten minutes. New code in act-mcp should name
`act_evidence::` directly; the alias exists for the migration, not forever.

`ExtractContext`, `EvidenceRoot`, the `validate-evidence` subcommand and `--schema`/`--meta-schema`
flags all stay: the CLI's behaviour, its exit codes and its stdout spelling are unchanged, which is
what `scripts/ci.sh`'s `schema` step and `scripts/validate_evidence.py`'s "two implementations must
agree" check depend on.

## 6. `crates/fathom` (P4, P6)

Package `fathom`, lib `fathom` **and** bin `fathom` (one package, two targets: the bin is
`Clap::parse()` + a call into the lib; the lib holds everything testable — that is how act-mcp is
shaped and how by1.13's headless `fathom report` / `fathom doctor` and by1.9's screen tests need it).

Allowed in `crates/fathom` (the whole list; adding to it is a code review question, not an ADR
question, as long as it does not collide with the right-hand column):

| | |
|---|---|
| `act-evidence` (path) | the contract: records, shapes, schema, validate, read |
| `ratatui`, `crossterm` | the screens |
| `clap` (same version as act-mcp, via `[workspace.dependencies]`) | CLI surface |
| `serde`, `serde_json`, `toml`, `chrono` | `fathom.toml` (ADR-0006), the cache store, timestamps |
| `git` via `std::process::Command` | the diffstat/diff reader (by1.8), with the same reasoning ADR-0003 and act-mcp's `Cargo.toml` already give: act shells out to git, the subcommands we need are CLI subcommands, and libgit2 would add a C build dependency for no gain |
| one store for `.fathom` (ADR-0006's call) | the metric cache |

Forbidden in `crates/fathom`, including in `[dev-dependencies]` (C6):

| forbidden | because |
|---|---|
| `act_mcp` / `act-mcp` | the whole point: fathom reads files, it does not borrow the server's process, runner or lock. If a *test helper* is genuinely shared (a throwaway git repo, a scratch evidence tree) it goes behind `act-evidence`'s `test-support` feature or into a `crates/act-testdata` dev-only crate — it does not become a dev-dependency on the server |
| `rmcp`, `reqwest`, `hyper`, `ureq`, `tower`, `axum` | P5: nothing in fathom speaks to a server or can start a run. Fathom must work with act-mcp not installed, not running, and with no network |
| `tokio` (or any async runtime) | the TUI's work is synchronous file and git reads on a worker thread; a runtime in the binary buys nothing and would be the thin end of "let's call the server". A later ticket that genuinely wants one amends this ADR |
| `tracing` subscribers / `env_logger` | fathom draws to a TTY. Diagnostics go to a file or to stderr in `doctor`, never to stdout it owns; the headless `report` must keep stdout parseable (same rule as act-mcp's, `tests/boot.rs`) |

`act-evidence`'s own dependency budget (C5) is the reverse-side rule that keeps it a contract crate
rather than a second server:

| allowed | why |
|---|---|
| `serde` (derive), `serde_json` | the records |
| `chrono` (`default-features = false, features = ["std", "clock"]`) | the envelope's instants |
| `thiserror` | typed errors, `RecordError::UnsupportedSchemaVersion` etc. |
| `jsonschema` (`default-features = false`) | `schema.rs` / `validate.rs`, and the embedded schemas stay self-contained |
| `schemars` — **optional**, behind a non-default `json-schema` feature | the `JsonSchema` impls on the record types exist to mirror `schema/evidence.schema.json` for the server's tool schemas. `act-mcp` enables the feature; fathom does not |
| `tempfile` — **optional**, behind a non-default `test-support` feature | the shared test-fixture escape hatch named above |

Everything else is out, and the list is closed by enumeration on purpose: `rmcp`, `axum`, `tower`,
`tokio`, `clap`, `tracing`, `libc`, `which`, `yaml-rust2`, `regex` are named in the checker. `libc`
deserves the note — the only `unsafe` in this area today is `gethostname` in the *writer*, which
stays in act-mcp, so the contract crate has no reason to carry it. `regex` is out for the reason
`shapes.rs` already documents: the patterns are few, short and anchored, and the hand-written
matchers plus `tests/evidence_contract.rs` are the guard.

**What "out of the tree" means, honestly.** Feature unification means a whole-workspace build links
one `act-evidence` built with the union of features its members ask for, so `cargo build` at the root
does compile `schemars` for the same target that builds fathom. The rule this ADR states, and the
rule the checker checks (`cargo tree -p fathom -i schemars`, from a `-p fathom` build), is about the
tree each *binary* ships from and the source each crate may name — which is what the `act-evidence`
feature flag actually buys, and what keeps `ratatui` out of the server tree absolutely rather than
cosmetically. The load-bearing half — `rmcp`, `axum`, `tower`, `tokio` never appearing in any fathom
tree, `ratatui`/`crossterm` never appearing in any act-mcp tree — is not affected by unification,
because nothing feature-gates them.

## 7. Fathom is a reader, not a client (P5)

Restated explicitly because "reads the output of the MCP" invites the opposite reading:

- **No new tool.** The server's surface stays `run_workflow` / `list_tasks` / `run_status` /
  `cancel_run` (ADR-0001), and `schema/tool-contract.schema.json` plus
  `scripts/validate_tool_contract.py` are what enumerate it. A `read_evidence` /
  `commit_history` / `regression` tool is rejected on three counts: it moves the scanner's work into
  the writer, which ADR-0002 §4.4 and the epic's own non-goals assign to the scanner; it makes
  history unreadable without a running server; and it would have to answer "which run do I trust for
  this commit", which is a policy question the same ADR refuses to own.
- **No MCP client, no HTTP, no loopback call.** Fathom opens `<workspace>/.evidence/benchmark/` and
  `…/coverage/` with `std::fs`. `runs/` and `incoming/` are not part of the contract
  (ADR-0002 §1) and fathom ignores them except to read a run's `meta.json` as audit context when the
  metric model (ADR-0008) needs it.
- **Concurrency needs no protocol.** A reader that starts mid-run sees whole files or nothing:
  the writer publishes with temp file + `fsync` + `rename` (ADR-0002 §6.2, §8), so a half-written
  run costs one file and never a partial line. That is the entire synchronisation story — no lock,
  no socket, no polling of `run_status`. `fathom doctor` reports a `runs/<run_id>/` with no
  `meta.json` as "suspicious directory" rather than trying to join the server's state.
- **Deferred is not permitted-by-silence.** mact-by1.17 (fathom triggering `run_workflow` to fill
  commits that have no evidence) is exactly what P4/P5 forbid. If it is ever un-deferred it amends
  this ADR first, and the amendment has to keep the rule that survived the discussion: the read path
  stays fully usable with no server present, and any client dependency is opt-in at the Cargo-feature
  level, never in the default tree.

## 8. Shared package policy (P6, P7, P8)

```toml
# Cargo.toml (root, virtual)
[workspace]
resolver = "2"
members = ["crates/act-evidence", "crates/act-mcp", "crates/fathom"]

[workspace.package]
version = "0.0.1"          # one repo, one tag, one version — see below
edition = "2021"
rust-version = "1.88"      # rmcp 3.9's floor; comment explaining it MOVES HERE
publish = false

[workspace.dependencies]
act-evidence = { path = "crates/act-evidence" }   # every shared version is declared here, once
clap = { version = "4", features = ["derive", "env"] }
serde = { version = "1", features = ["derive"] }
# …

[workspace.lints.rust]
unsafe_code = "deny"
```

- **Names (P6).** Packages `act-evidence`, `act-mcp`, `fathom`. Libs `act_evidence`, `act_mcp`,
  `fathom`. Bins `act-mcp`, `fathom`. `act-mcp`'s lib and bin names do not change: `cd.yml`,
  `scripts/smoke_binary.sh`, `scripts/ci.sh`, `docs/ci.md` and the README all name `act-mcp` (and
  the release tarball renames it to `mact`), and churn there buys nothing.
- **Version.** `[workspace.package] version` + `version.workspace = true` in each member. `cd.yml`
  does one `cargo set-version "$tag"` per release, and one version for one repo is what that command
  does when members inherit — which keeps the release-sync behaviour the last four commits
  stabilised, and stops `fathom` and `act-mcp` drifting to unrelated versions nobody chose.
- **`rust-version` floor (P7).** 1.88, set in `[workspace.package]`, inherited by all three. Two
  directions of drag are both refused: the UI's dependencies must not raise the *server's* floor
  (a 1.88 install of act-mcp is a promise the rmcp note in today's `Cargo.toml` is the reason for),
  and the server's floor is not a reason for the UI to compile against a version its own TUI deps
  need more than. If `fathom`'s tree ever needs more, `fathom/Cargo.toml` raises **its own**
  `rust-version` with a literal and a comment naming the dependency that forced it; the workspace
  floor never goes *down* (C3).
- **`publish = false`** for all three (C4), inherited from `[workspace.package]`. Consequence worth
  stating: nothing here is a public API in the crates.io sense, so `act-evidence` may refactor
  freely *within this repo* — but its types describe an on-disk format that is already frozen
  (ADR-0002), so "internal" means "no external consumer to apologise to", not "free to change".
  Publishing `act-evidence` someday (a third tool, outside this repo, reading the same trees) is the
  only publishing case anyone has asked for, and it needs a licence, docs and a name check first.
- **Lints (P8).** `[workspace.lints.rust]` carries exactly one entry: `unsafe_code = "deny"`, which
  is a workspace-wide promise (neither tree wants `unsafe`; act-mcp's two audited call sites —
  `killpg`/`prctl` in the runner, `gethostname` in the writer — keep their local `#[allow]` with its
  `// SAFETY:` note, and a new `unsafe` anywhere else still fails a build). `missing_docs` deliberately
  does **not** move up: `[lints]` applies to *every target in the package*, and `cargo clippy
  --all-targets --all-features -D warnings` turns it into an error, so promoting it would demand doc
  comments on integration-test scaffolding for no benefit. It stays a `#![warn(missing_docs)]` crate
  attribute in each `lib.rs`, where the promise actually lives — a public API's items are documented.
  No clippy lint groups are added at the workspace level either: CI's `-D warnings` on the default set
  is the bar, and a crate that wants `pedantic` opts in on its own.
- **Edition 2021, one toolchain, one lockfile, one `deny.toml`.** `rust-toolchain.toml` (1.99.1)
  covers the workspace; cargo-deny resolves the whole workspace, so one deny.toml gates both trees —
  which is precisely why the *dependency-direction* rules are enforced by a checker and not left to
  taste (§11).

## 9. Shared directories, and the two `.fathom`s

- **`schema/` stays at the repo root.** Three artifacts include it: act-evidence via `include_str!`,
  the python checkers, and `scripts/ci.sh` (`--schema schema/evidence.schema.json`). Exactly one
  copy of each schema file exists, at `schema/` (C11).
- **Integration tests move with their crate**, because Cargo looks for `tests/` next to
  `Cargo.toml`. `tests/` → `crates/act-mcp/tests/`. Tests that build paths from
  `env!("CARGO_MANIFEST_DIR")` (which is most of them) keep working untouched; only a crate moved out
  of the way it is referenced would need edits, and none is.
- **Shared fixture corpora are never duplicated.** `tests/e2e/testdata/expected/` is the golden
  evidence tree; when mact-by1.6 wants it from `act-evidence` too, the tree is **moved** once more
  (to `crates/act-evidence/tests/testdata/`, or to a root-level `testdata/` if the ADR-editing
  person prefers) and referenced from both — never copied (C13: more than one copy of
  `expected/benchmark.jsonl` is a failure). Choosing where is mact-by1.5's call; the no-second-copy
  rule is not.
- **`crates/fathom/` and `.fathom/` are unrelated** and both keep their names. `crates/fathom/` is
  this crate's source. `.fathom/` (already present in this repo, with `index.sqlite3` and
  `objects/`, and `.gitignore`d) is the *data directory a fathom run writes into a user's workspace*
  — ADR-0006's to define (mact-by1.2), including `.fathom/LAYOUT.md`. The naming collision is
  accepted because the crate's name and the product's name are the same word for two different
  things, and `.gitignore`'s `.fathom/*` pattern is root-anchored, so it never touches `crates/`.
  Fathom MUST NOT write into its own crate directory, and MUST NOT decide where `.fathom/` goes by
  any rule ADR-0006 has not stated.

## 10. What must not move into `act-evidence`

The crate is a contract, not a second scanner. The line ADR-0002 §4.4 draws — act-mcp appends, the
scanner thinks — keeps its direction when the scanner gets a shared crate:

| belongs to | not to `act-evidence` |
|---|---|
| fathom (ADR-0008, by1.4/by1.6) | coverage **percentage** from raw counts, benchmark grouping and dedup policy, percentiles, outlier rejection, unit reconciliation, "did this commit regress", what `run_status` means durably per commit |
| fathom (ADR-0007, by1.4/by1.7) | the Maintainability Index formula and its per-commit aggregation |
| fathom (ADR-0006, by1.2/by1.10) | the `.fathom` store, `fathom.toml`, the blob-keyed metric cache |
| act-mcp | the act runner, the workspace lock, the run registry, extraction from act's payload, every MCP tool schema |

`act-evidence::read` (by1.6) may answer "which files exist for this commit, and give me their typed
records, and tell me what you skipped and why" — a *read* with a count of refused records, per
ADR-0002 §7's downstream policy. It may not answer "what is this commit's coverage".

## 11. Verification

| claim | owner | command |
|---|---|---|
| one lockfile, one target dir, every `crates/*` is a member, no member path escapes the workspace | this ADR (C2) | `python3 scripts/validate_crate_layout.py` |
| the four tools are still the whole tool surface (P5) | mact-f1w.1/.17 (pre-existing) | `python3 scripts/validate_tool_contract.py` |
| `act-evidence`'s dependency budget is what §6 says, and it holds no forbidden crate | this ADR (C5) | `cargo tree -p act-evidence -e normal,dev`, plus the checker's own list |
| no rmcp/axum/tokio/ratatui in fathom's tree; no ratatui in act-mcp's tree (P4) | this ADR (C6) | `cargo tree -p fathom -i rmcp --invert` (must be empty), `cargo tree -p act-mcp -i ratatui` (must be empty) |
| `schemars` stays out of a standalone fathom build (§6) | mact-by1.16 | `cargo build -p fathom --release && cargo tree -p fathom -i schemars` |
| floor / inheritance / publish (P7) | this ADR (C3, C4, C7) | `python3 scripts/validate_crate_layout.py` |
| one copy of each schema and of the golden tree (C11, C13) | this ADR | `python3 scripts/validate_crate_layout.py` |
| the split did not change the contract's behaviour | pre-existing | `scripts/ci.sh test schema` (the writer's golden tests and `validate-evidence` are unchanged), plus `scripts/validate_evidence.py` disagreeing with nothing |
| the server still ships as one binary with the same name and version | mact-by1.5 | `scripts/ci.sh release` (now `-p act-mcp --bin act-mcp`) then `scripts/smoke_binary.sh --expect-version $(sed -n 's/^version *= *"\([^"]*\)".*/\1/p' crates/act-mcp/Cargo.toml)` |
| the checker itself is not a lie: every C-check fires when the rule it names is broken | this ADR | `scripts/crate_layout_drill.sh` (builds a throwaway three-crate workspace in `/tmp`, then breaks one rule at a time) |
| the checker runs in CI | this ADR (C12) | `scripts/ci.sh schema` |

The checker runs in `scripts/ci.sh`'s `schema` step, in the company of the other three ADR↔schema
checkers, and passes today — before the split exists — precisely so that mact-by1.5's commit is judged
against this ADR rather than inventing its own answer mid-move.

## 12. Consequences

**Good**

- One strict implementation of the read path. `act-evidence`'s types are the same ones that make a
  `null` or a foreign `schema_version` unrepresentable on the write side, so ADR-0002 §7 stays
  "one ADR, one schema, one implementation" instead of "one ADR, one schema, two struct sets and a
  prayer".
- The server's tree is untouched by the UI's: no ratatui, no crossterm, no store engine, no
  compile-time growth for `act-mcp`, and `tests/boot.rs`'s stdout rule stays a statement about a
  process that does not own a TTY.
- `cargo deny`, `rust-toolchain.toml`, `Cargo.lock` and CI stay single artifacts: one supply-chain
  story for both binaries, one lockfile to review, one toolchain to install.
- fathom is testable as a library (by1.9's screens, by1.13's `report` golden, by1.8's git reader)
  without a `#[path]` hack into a server crate it has no business naming.
- The next time a binary wants to appear, the workspace already answers the question.

**Costs, and who pays them**

- One crate split in a repo that has never done one: `git mv` for `src/evidence/{records,shapes,
  schema,validate}.rs` and `tests/`, plus a root manifest rewrite. mact-by1.5 owns it, and the
  re-export in §5 is what keeps its diff reviewable.
- Path-sensitive scripts and workflows have to learn about `crates/`. Every known case, so nobody
  finds them in CI at 2am:

| thing | today | after the split | owner |
|---|---|---|---|
| `scripts/smoke_binary.sh:118` | reads version from `$repo_root/Cargo.toml` | `crates/act-mcp/Cargo.toml` | by1.5 |
| `scripts/ci.sh` `release` step | `cargo build --release` | `-p act-mcp --bin act-mcp` (+ separate fathom job) | by1.5 / by1.16 |
| `.github/workflows/ci.yml` `paths:` filter | `Cargo.toml` | add `crates/**/Cargo.toml`, `crates/**/src/**` | by1.5 |
| `.github/workflows/cd.yml` | `cargo set-version`, then `cd target/release && mv act-mcp mact` | unchanged commands; `cargo set-version` now sets the shared `workspace.package` version | by1.5 (verify) |
| `scripts/ci_drill.sh:184` copied items | `Cargo.toml Cargo.lock rust-toolchain.toml deny.toml schema scripts src tests docs` | add `crates` | by1.5 |
| `docs/ci.md` MSRV note ("`Cargo.toml`'s `rust-version = "1.88"` is the floor"), README prerequisites row | root manifest | `[workspace.package]` | by1.5 |

- `cargo clippy --all-targets --all-features -D warnings` and `cargo test --all-features` now cover
  three crates, so a release train carries the TUI's lint debt as well as the server's — which is
  the normal price of one CI, and the reason §3 pins selective release building.

## Considered alternatives

- **A — `[[bin]] name = "fathom"` inside the act-mcp crate.** **Rejected.** Ratatui, crossterm and
  whatever store engine ADR-0006 picks land in the same tree as the MCP server, so the server's
  compile time and its advisory exposure grow with a UI's; `tests/boot.rs`'s "stdout must stay empty"
  becomes a rule about a crate that also contains a program whose whole job is stdout and raw-mode
  TTY control; and the lint/error surface is shared (`#![deny(unsafe_code)]` at one crate root, one
  `Cargo.toml`'s features) between two programs with opposite requirements. It is cheapest for about
  a week, and after by1.9 it is permanent.
- **A′ — a `fathom` subcommand of the `act-mcp` binary** (`act-mcp fathom`). Rejected for the same
  reasons, with an extra one: the release artefact is one renamed binary (`mact`), and a subcommand
  makes the *server's* CLI surface carry a reader's flags forever.
- **B′ — workspace with `crates/act-mcp` + `crates/fathom`, no third crate; fathom depends on
  `act_mcp` as a library** (optionally with `default-features = false` to switch rmcp/axum off).
  **Rejected**, and the ticket named the reason: "a fathom that depends on act_mcp wholesale is not
  acceptable." It inverts nothing structurally but makes the whole server — runner, supervisor,
  lock, rmcp, axum — a build dependency of a TUI, in the tree that CI ships and the tree that must
  stay small. Feature-gating the server off is the same crate with the important parts switched out
  by a flag, and a flag is a weaker boundary than a package boundary; it also does not get rid of the
  TTY/stdout-sharing crate root.
- **B″ — workspace, and fathom re-declares its own `.evidence` structs.** **Rejected.** Two
  implementations of one contract on the read side is the specific failure ADR-0002 §7 forbids
  ("inventing a second shape in code is the failure mode this ADR exists to prevent"), and the strict
  deserializer fathom wants — the one rejecting a `null`, a foreign `schema_version`, a non-finite
  value, a coverage field on a benchmark line, a traversing path — already exists and is already
  cross-checked against the schema by `tests/evidence_contract.rs`. Duplicating it means fathom
  silently accepting records act-mcp would refuse, and finding that out from a user's chart.
- **C — a separate repository for fathom.** **Rejected.** The two halves share the record types and
  the fixture trees; a cross-repo pin (path dependency, git dependency, or a published crate) buys
  nothing but friction, and it splits the one lockfile / one CI / one `deny.toml` property that made
  option B better than option A in the first place. The ADR↔schema↔code "change all three together"
  rule of ADR-0002 §7 wants one commit, not a PR pair.
- **A new MCP tool (`read_evidence` / `commit_history`) that fathom calls.** **Rejected**, three
  ways: it moves the scanner's work into the writer, against ADR-0002 §4.4 and the epic's own
  non-goals; it makes the history unreadable when the server is not running, which is most of the
  time; and it would have to own cross-run trust policy, which is ADR-0008's and fathom's.
- **A shared `crates/act-common` grab-bag** (config, logging, error types, CLI helpers) alongside
  `act-evidence`. **Rejected as premature.** There is exactly one genuinely shared thing today: the
  contract. A second crate full of "utilities" is where a dependency edge from fathom to
  act-mcp-shaped code comes back in disguise. If a second shared crate is ever needed it is
  `crates/act-testdata`, dev-only, and it names no server code.

## Open questions (owned elsewhere)

- What `.fathom/` contains, where it lives, and `fathom.toml`'s schema — ADR-0006 (mact-by1.2). This
  ADR only says the crate directory and that data directory are different things (§9).
- Which store engine (and therefore whether `fathom` grows a C dependency — the root already has
  none, and this ADR would like that to stay true) — ADR-0006 / mact-by1.10.
- The MI front end's dependencies (`tree-sitter`? a hand-written tokenizer?) — ADR-0007 (mact-by1.3).
- Coverage-% derivation, benchmark grouping/dedup and durable "action success" — ADR-0008
  (mact-by1.4), in fathom, per §10.
- Whether `fathom` gets its own release artefacts (`mact-tui-<target>.tar.gz`) or rides the server's
  tarball — mact-by1.16.
- Whether `act-evidence` is ever published to crates.io for out-of-repo readers (§8). Deferred;
  `publish = false` until someone asks.

## Implementation status

Written by mact-by1.1 against a tree that was one crate: there was no `crates/`, no `act-evidence`,
and `fathom` was a name in a dependency graph nobody had built. **mact-by1.5 executed §9**, so the
tree is now §3's three crates with one root `Cargo.toml`/`Cargo.lock`, workspace inheritance for the
package fields, one dependency table and `[workspace.lints]`; `scripts/validate_crate_layout.py`
C1–C13 pass and `scripts/crate_layout_drill.sh` breaks each of them on purpose. What differs from
the letter of this ADR, and why — read this section as an amendment, not a footnote:

* **`act-evidence` has one Cargo feature, `json-schema`, default off.** §2 said it "does not need the
  `schemars` dependency at all", which is true of the runtime path — the schema *files* are JSON text
  and nothing here needs Rust codegen to read them. What needs `schemars` is the pair of second
  opinions that prove the Rust types and `schema/evidence.schema.json` still agree: `validate.rs`'s
  `load_schema`/`schema_says_the_same`, which `schema_for!(EvidenceRecord)` against the file, and
  `act-mcp`'s `tests/evidence_contract.rs`, which runs that over the whole golden corpus. So the
  derives and those two functions are behind `#[cfg(feature = "json-schema")]`; a reader that never
  asks the question never links `schemars`, and the C5 drill ("schemars stays optional so a reader's
  build can skip it") still fires. `act-mcp` turns the feature on, which is why the move changed no
  behaviour at all.
* **`validate.rs` crossed whole, not just its tree logic.** The pure half of it *is* the `.jsonl`
  line-reader this ADR exists to share (§1): content-hash dedupe, line-numbered errors, `O_NOFOLLOW`
  against a symlinked record file, size caps, and `create(false)`. Splitting it in two would hand
  fathom a second implementation to write, which is P4's whole reason for existing. Consequence for
  the tickets after this one: mact-by1.7's scanner goes through `act_evidence::validate` and must not
  open the evidence root with its own `File::open` — the "a half-written file is not visible"
  guarantee (§1) lives there, not in the tree walk.
* **`unsafe_code = "deny"` reached the integration tests, and four of them now carry an audited
  `allow`.** §8's workspace lint applies to every target, and the test crates signal process groups
  with `libc::kill`/`killpg`/`getpgid` (`boot.rs`, `hardening.rs`, `runner_execution.rs`,
  `runner_pdeathsig.rs`, plus the shared `tests/common` module the e2e suites include). Each carries
  a `reason =`, per §12's rule about the *form* an exception takes. `unsafe_code` stays `deny`, never
  `forbid`, exactly as §8 says: `evidence::writer`'s `gethostname` audit continues to compile.
* **`missing_docs` did not move into `[workspace.lints]`.** The checker (C8, drilled) requires it to
  stay a per-crate attribute, and the crates keep `#![warn(missing_docs)]` in their `lib.rs`. A
  workspace-wide `missing_docs` would apply to binaries and test crates, where the rule is not wanted;
  the ADR's own §12 argument — an exception must name a line and a reason — cuts against spraying the
  warning over targets that have no public surface.
* **`.fathom/LAYOUT.md` stays at the repository root**, one copy. §9's "copy `.fathom/LAYOUT.md` too"
  was about not *duplicating* the contract; `validate_fathom_layout.py` reads the root path and needed
  no change, and a cache-root contract is not a crate's private detail.
* **CI changed as §9 predicted, in `scripts/ci.sh` rather than only in the workflows.** The `schema`
  step and the `release` step now build `-p act-mcp --bin act-mcp`; the workflow path filters are
  `crates/**` because `src/**` and `tests/**` would have matched nothing and silently stopped running
  CI at all. `cd.yml` no longer installs `cargo-edit`: with inherited versions there is one line to
  set, and `scripts/set_version.py` sets it *and* checks that every member still inherits (C7's rule)
  and that `cargo metadata` agrees. The separate `fathom` release job is still mact-by1.16's.

Not done here, because they are later tickets and this ADR says so: the ratatui front end (by1.6), the
evidence scanner (by1.7), git history and `history.jsonl` (by1.8), MI and Halstead (by1.9), the store
(by1.10–by1.12), `report`'s engine (by1.13). What `fathom` is today: the clap surface above, `fathom.toml`
discovery and refusal (§7.1), the workspace/evidence/cache-root boot rules including `.fathom` creation
and `LAYOUT.md` (§4.1), and `index --status`'s root-precedence output (§10.1). `tui`, `report` and
`index`'s write path validate the workspace — so a bad workspace is refused with exit 1 and a `fix:`
today, in the shape later tickets must keep — and then exit 3 naming the ticket that owns the engine.
