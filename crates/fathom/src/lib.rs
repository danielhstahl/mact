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
//! * [`log`] — stderr-only diagnostics behind a hand-rolled `RUST_LOG`-style filter, because this
//!   crate may not own a subscriber (see rule 1 below).
//! * [`exit`] — what each process status promises.
//!
//! The modules the epic adds live beside these: the git reader (mact-by1.8), the MI analyser
//! (mact-by1.7), the store (mact-by1.10), the screens (mact-by1.9/.11/.12/.14), `report` and
//! `doctor` (mact-by1.13). None of them exist yet, and the subcommands that will call them already
//! parse and validate: they exit `3` with the ticket that owns them rather than pretending.
//!
//! ## Two rules this crate keeps
//!
//! 1. **logs to stderr, frames and data to stdout.** The server's rule is "stdout is sacred"; a TUI
//!    owns the terminal, so fathom's version is stricter about *which* stream is which: stdout is
//!    reserved for what the user asked for — the screen, and `report`'s table — and every
//!    diagnostic, warning and `fix:` line goes to stderr. `crates/fathom/tests/cli.rs` asserts that
//!    `report` and the not-yet-wired commands leave stdout empty, so `fathom report > timeline.tsv`
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
pub mod cli;
pub mod config;
pub mod exit;
pub mod log;

use thiserror::Error;

pub use boot::{Boot, BootError};
pub use cli::Cli;
pub use config::{Config, ConfigError};
pub use exit::Exit;

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
}
