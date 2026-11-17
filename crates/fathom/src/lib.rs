//! `fathom` — one row per commit: coverage, benchmarks and maintainability, from the evidence
//! act-mcp wrote and the history the repository already has.
//!
//! Fathom is the **reader** half of this workspace (ADR-0005). It opens `<workspace>/.evidence`
//! with `act_evidence`'s record types — the same strict deserializer the writer uses — and it
//! never talks to the server: no MCP client, no HTTP, no `tokio`, nothing that can start a run. A
//! timeline you can only read while a server is up is a timeline you cannot read in CI, on a laptop,
//! or on a build agent that mounted the tree read-only.
//!
//! ## Layout
//!
//! * [`cli`] — the argv surface and the dispatch. This is the contract CI pins: subcommands
//!   (`tui`/`index`/`report`, plus bare `fathom [WORKSPACE]`), the shared plumbing flags, and clap's
//!   exit 2 for anything else.
//! * [`boot`] — boot-time validation, in the shape `act_mcp::config` uses: workspace exists and is a
//!   git work tree, evidence root (warn if absent), cache root (create it, write `LAYOUT.md`, probe
//!   it writable). Every refusal is one typed line plus a `fix:`.
//! * [`config`] — `fathom.toml`: root-ward discovery, `deny_unknown_fields`, resolved defaults
//!   (ADR-0006 §7).
//! * [`metrics`] — ADR-0008 executed once: the evidence tree in, one [`metrics::CommitMetrics`] per
//!   SHA out, with the drop log that explains every number (mact-by1.6).
//! * [`mi`] — ADR-0007 executed: per-file Maintainability Index from a `(path, bytes)` blob, and the
//!   LOC-weighted per-commit aggregate with its delta, hotspots and watchlist (mact-by1.7).
//! * [`log`] — stderr-only diagnostics behind a hand-rolled `RUST_LOG`-style filter, because this
//!   crate may not own a subscriber (see rule 1 below).
//! * [`exit`] — what each process status promises.
//!
//! The modules the epic adds beside these:
//!
//! * [`metrics`] — the evidence reader (mact-by1.6): `.evidence` in, one typed row per SHA out.
//! * [`tui`] — the timeline screen (mact-by1.9): the store in, one row per commit on the screen or on
//!   stdout, with the index running on a worker thread.
//! * [`report`] — the headless table (mact-by1.13): the same rows and the same cells on stdout as
//!   json/tsv/csv, with the CI gate (`--fail-on`) and its 0/1/2 exit codes.
//! * [`doctor`] — why there is nothing to show (mact-by1.13): seven checks, each `ok`/`warn`/`fail`
//!   with a `fix:`, and the same in `--json`.
//! * [`git`] — the git CLI itself: opening a repository, the version floor, the streamed
//!   `NUL`-separated calls, and the typed refusals. `git` is a subprocess here, never libgit2
//!   (ADR-0005 §6, and the rationale is in `crates/fathom/Cargo.toml`).
//! * [`history`] — the git reader (mact-by1.8): a rev range in, one entry per commit out with its
//!   per-file change list, `+`/`−` totals and modes.
//! * [`diff`] — one commit's patch (mact-by1.11): `git show` for a single commit, streamed and capped,
//!   with the file list read from [`history`] so the two panes of the diff view cannot disagree.
//! * [`classify`] — the one path classifier ADR-0007 §7.1 shares between the `+`/`−` column and the
//!   MI column, so one row's two columns always describe the same file set.
//!
//! The benchmark panel (mact-by1.12) and the diff view (mact-by1.11) are the sibling screens: `b` or
//! `d` on a row, or `--bench REV` / `--diff REV`, each with its own state machine
//! ([`tui::bench::Panel`], [`tui::diff_view::DiffView`]) and its own `--plain` rendering.
//!
//! Still absent on purpose: the commit detail view (mact-by1.14). A subcommand whose engine is a later
//! ticket exits `3` naming the ticket rather than pretending ([`crate::exit`]).
//!
//! ## Two rules this crate keeps
//!
//! 1. **logs to stderr, frames and data to stdout.** The server's rule is "stdout is sacred"; a TUI
//!    owns the terminal, so fathom's version is stricter about *which* stream is which: stdout is
//!    reserved for what the user asked for — the screen, and `report`'s and `doctor`'s output —
//!    and every diagnostic, warning and `fix:` line goes to stderr. `crates/fathom/tests/cli.rs`
//!    asserts that a refusal and a bad flag leave stdout empty, so `fathom report > timeline.tsv`
//!    cannot be poisoned by a log line. That is also why there is no `tracing` subscriber here
//!    (ADR-0005 §6 forbids one, and [`log`] is the thirty lines that replace it).
//! 2. **fathom reads; it never runs.** Nothing in this crate may depend on `act_mcp` or gain an MCP
//!    client, an HTTP client or an async runtime (ADR-0005 P4/P5, checked by
//!    `scripts/validate_crate_layout.py`). The evidence root is read-only here: the writer owns it,
//!    and the publish-then-rename protocol it uses is what lets a reader that starts mid-run see
//!    whole files or nothing at all — no lock, no socket, no polling.
//!
//! [`crate::boot`] creates exactly one thing in a user's workspace: the cache root of ADR-0006 §4.1,
//! and `LAYOUT.md` inside it. Everything else fathom touches, it only reads.

// The workspace lint denies `unsafe_code` for every target (ADR-0005 §8); the attribute is repeated
// here so the promise is visible in the file that keeps it. Fathom has no audited call sites, unlike
// act-mcp's runner and writer: git is a subprocess (mact-by1.8), not libgit2, and the store is files
// (ADR-0006 §5), not sqlite.
#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod boot;
pub mod classify;
pub mod cli;
pub mod config;
pub mod diff;
pub mod doctor;
pub mod exit;
pub mod git;
pub mod history;
pub mod log;
pub mod metrics;
pub mod mi;
pub mod report;
pub mod store;
pub mod tui;

use thiserror::Error;

pub use boot::{Boot, BootError};
pub use cli::Cli;
pub use config::{Config, ConfigError};
pub use exit::Exit;
pub use metrics::{CommitMetrics, Reader, Readset};
pub use store::StoreError;

/// The one error type `main` prints: the two refusal families, unchanged.
///
/// `main` prints the top line and each `cause`, and each variant already carries its own `fix:` —
/// the same shape `act-mcp`'s CLI uses, so one habit covers both binaries in this workspace.
#[derive(Debug, Error)]
pub enum FathomError {
    /// `fathom.toml` could not be found, read, or parsed (see [`crate::config`]).
    #[error(transparent)]
    Config(#[from] ConfigError),

    /// The workspace, the evidence root or the cache root refused the command (see [`crate::boot`]).
    #[error(transparent)]
    Boot(#[from] BootError),

    /// The metric store refused the command: a foreign fingerprint, a busy cache root, a publish that
    /// failed (see [`crate::store`]). Each message is already one line plus a `fix:` (ADR-0006 §10).
    #[error(transparent)]
    Store(#[from] StoreError),

    /// The screen could not be drawn: the terminal refused to be taken over, or would not give back what
    /// it took (mact-by1.9). Carries its own `fix:` line, which is always `--plain`, because a broken
    /// terminal is not a reason to lose the table.
    #[error("{0}")]
    Terminal(String),

    /// A commit's diff could not be read (see [`crate::diff`], mact-by1.11): git does not know the
    /// revision, the repository is unreadable, or a `paths.*` glob will not compile. Every variant
    /// already carries its `fix:` line.
    #[error(transparent)]
    Diff(#[from] crate::diff::Error),

    /// `git` refused a question a headless command had to ask (mact-by1.13): `report` cannot enumerate
    /// its window without it, and `doctor` reports the refusal as a failed check instead of raising
    /// it. Each [`crate::git::GitError`] already carries its own `fix:` line.
    #[error(transparent)]
    Git(#[from] crate::git::GitError),
}
