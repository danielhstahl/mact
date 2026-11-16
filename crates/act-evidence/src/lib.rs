//! `act-evidence` — the `.evidence` JSONL contract, as code: record types, shape predicates,
//! the embedded schemas, and the tree validator.
//!
//! Everything here is an implementation of
//! [`docs/adr/0002-evidence-jsonl-contract.md`](../../docs/adr/0002-evidence-jsonl-contract.md),
//! which is the normative half of the contract; `schema/evidence.schema.json` is the
//! machine-readable half. The files on disk are this project's public API — another tool
//! scans them across many commits, and once it does, a shape change is a breaking change —
//! so the three artifacts (ADR, schema, code) are meant to say one thing. When they do
//! not, that is a bug in whichever is wrongest, to be fixed in the same change
//! (ADR-0002 §7).
//!
//! ## Why a crate of its own
//!
//! This crate is the **only** implementation of reading `.evidence` (ADR-0005 P2). Both halves of
//! the workspace depend on it and neither may bypass it:
//!
//! ```text
//!    act-mcp ───────▶  act-evidence  ◀─────── fathom
//!  (server, writer,      (the contract)     (reader, TUI, report)
//!        CLI)
//! ```
//!
//! `act-mcp` re-exports all of this as `act_mcp::evidence`, so paths written before the split keep
//! resolving. New code in either consumer should name `act_evidence::` directly.
//!
//! The dependency budget is closed and short (serde, serde_json, chrono, thiserror, jsonschema,
//! and optionally `schemars` behind the non-default `json-schema` feature): no rmcp, axum, tower,
//! tokio, clap, tracing, `libc`, `which`, `yaml-rust2` or `regex` (ADR-0005 §6, enforced by
//! `scripts/validate_crate_layout.py` check C5). Two consequences of that budget are load-bearing:
//!
//! * **the writer is not here.** Publishing evidence means `temp` + `fsync` + `rename` and a
//!   `gethostname(2)` call for the envelope's `host`; a reader must not be able to write, so
//!   `writer.rs` stays in `act-mcp` along with the extractors built on act's own run outcome.
//! * **there is no second schema.** Both schemas are `include_str!`-ed from `schema/` at the
//!   repository root — exactly one copy of each file exists there (check C11), and the compiled
//!   validator is shared by the writer, the checker and the reader.
//!
//! ## The three guarantees this crate exists to make
//!
//! 1. **A record that cannot be misread cannot be written.** [`records`] holds the
//!    envelope and the two `kind` variants, and they are typed tightly enough that a
//!    `null`, an unknown `kind`, a non-finite number, a foreign `schema_version`, a
//!    coverage field on a benchmark line, or a traversing path is *unrepresentable*
//!    rather than merely discouraged.
//! 2. **A reader never sees a half-written run.** That is the *writer's* promise — ADR-0002 §6.2:
//!    one file per run, published via temp file + `fsync` + `rename` — and this crate is what makes
//!    good on the read half: a file is whole or absent, and [`validate`] says so per line.
//! 3. **The contract cannot rot silently.** [`validate`] is `act-mcp validate-evidence`:
//!    strict about shape, run in CI over a whole evidence tree, printing
//!    `file:line: message` and exiting non-zero on any bad line.
//!
//! ## Layout it describes (ADR-0002 §1.1)
//!
//! ```text
//! <workspace>/.evidence/
//! ├── README.md                                   # stub, written once
//! ├── benchmark/<full-sha>/<run_id>.jsonl         # the contract
//! ├── coverage/<full-sha>/<run_id>.jsonl          # the contract
//! ├── runs/<run_id>/meta.json                     # terminal-state manifest (not written here)
//! ├── runs/<run_id>/act.jsonl                     # act's own log (not written here)
//! └── incoming/<run_id>/                          # per-run payload scratch (not written here)
//! ```
//!
//! The writer (`act_mcp::evidence::writer`) creates the root and the `benchmark/`, `coverage/`,
//! `runs/` subtrees on demand; `runs/<run_id>/` and `incoming/` are the runner's and the
//! supervisor's (mact-f1w.8, mact-f1w.11), and neither is part of the read contract.

// The contract crate owns no `unsafe`: the only `unsafe` in this area of the tree was
// `gethostname` in the *writer*, and that stayed in `act-mcp` (ADR-0005 §6). The workspace lint
// already denies it for every target; the attribute is repeated at the crate root so the promise
// travels with the source rather than with a manifest three directories up.
#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod records;
pub mod schema;
pub mod shapes;
pub mod validate;

pub use records::{
    BenchmarkMetric, BenchmarkRecord, BenchmarkSource, Commit, CoverageCounts, CoverageRecord,
    CoverageSource, Envelope, EvidenceRecord, Kind, MetricValue, RelPath, ReportedMeasure, RunId,
    RunRecords, RunStatus, SchemaVersion, Scope, Tool, UtcInstant, SCHEMA_VERSION, TOOL_NAME,
};
pub use schema::{SchemaError, SchemaProblem};
pub use validate::{validate_tree, Problem, Report, Stats, ValidateError, ValidateOptions};
