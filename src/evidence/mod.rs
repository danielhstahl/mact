//! The `.evidence` JSONL contract, as code: record types, the writer, and the checker.
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
//! ## The three guarantees this module exists to make
//!
//! 1. **A record that cannot be misread cannot be written.** [`records`] holds the
//!    envelope and the two `kind` variants, and they are typed tightly enough that a
//!    `null`, an unknown `kind`, a non-finite number, a foreign `schema_version`, a
//!    coverage field on a benchmark line, or a traversing path is *unrepresentable*
//!    rather than merely discouraged.
//! 2. **A reader never sees a half-written run.** [`writer`] publishes one file per run
//!    via temp file + `fsync` + `rename` (ADR-0002 §6.2), so append-only is a property of
//!    the directory, and a crashed run costs exactly one file.
//! 3. **The contract cannot rot silently.** [`validate`] is `act-mcp validate-evidence`:
//!    strict about shape, run in CI over a whole evidence tree, printing
//!    `file:line: message` and exiting non-zero on any bad line.
//!
//! ## Layout it produces (ADR-0002 §1.1)
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
//! The writer creates the root and the `benchmark/`, `coverage/`, `runs/` subtrees on
//! demand; `runs/<run_id>/` and `incoming/` are the runner's and the supervisor's
//! (mact-f1w.8, mact-f1w.11), not this module's.

/// Coverage extraction: a finished run's coverage payload → coverage records.
///
/// POSIX-only for the same reason the runner is: it is built on
/// [`crate::runner::ActRunOutcome`], and the runner, `--bind` and act are all POSIX.
#[cfg(unix)]
pub mod coverage;
pub mod records;
pub mod schema;
pub mod shapes;
pub mod validate;
pub mod writer;

#[cfg(unix)]
pub use coverage::{
    extract, extract_with, CoverageFormat, CoverageSummary, ExtractContext, ExtractError,
    ExtractionOutcome,
};
pub use records::{
    BenchmarkMetric, BenchmarkRecord, BenchmarkSource, Commit, CoverageCounts, CoverageRecord,
    CoverageSource, Envelope, EvidenceRecord, Kind, MetricValue, RelPath, ReportedMeasure, RunId,
    RunRecords, RunStatus, SchemaVersion, Scope, Tool, UtcInstant, SCHEMA_VERSION, TOOL_NAME,
};
pub use schema::{SchemaError, SchemaProblem};
pub use validate::{validate_tree, Problem, Report, Stats, ValidateError, ValidateOptions};
pub use writer::{
    hostname, update_gitignore, EvidenceError, EvidenceRoot, GitignoreOutcome, Published, DIR_MODE,
    FILE_MODE, INCOMING_DIR_MODE,
};
