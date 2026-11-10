//! `act-mcp` — an HTTP MCP server that runs GitHub Actions workflows with
//! [`nektos/act`] and records what they produced as `.evidence` JSONL.
//!
//! This crate is the shell: an [rmcp] streamable-HTTP server pinned to protocol
//! revision `2026-07-28`, bound to loopback, with no authentication (local-only
//! by design — see [`config::DEFAULT_BIND`] and ADR-0003). Tool logic, the act
//! runner and the evidence writer are later tickets in the same epic; the
//! protocol/transport decisions that live here are settled here.
//!
//! ## Layout
//!
//! * [`config`] — CLI/env surface and every boot-time validation, so "does this
//!   workspace make sense" is answered before anything is spawned.
//! * [`evidence`] — the `.evidence` JSONL contract as code: the record types, the
//!   atomic append-only writer, the `validate-evidence` checker, and the coverage
//!   extractor that turns a finished run's payload into records (ADR-0002).
//! * [`workspace`] — the pre-start validation gate: turn untrusted `run_workflow`
//!   arguments plus the environment into a [`workspace::ValidatedRunTarget`], or
//!   into one typed ADR-0001 error. Nothing here talks to the network or spawns act.
//! * [`workflow_yaml`] — the minimal YAML view of a workflow file (declared name
//!   and job ids) that validation needs.
//! * [`server`] — the [`server::ActMcp`] service and the axum mount.
//! * [`tools`] — one module per MCP tool: its advertised schemas, its argument parsing, and
//!   the ADR-0001 §5 error mapping shared by all of them.
//! * [`registry`] — the run supervisor (ADR-0004): the run state machine, the in-memory run
//!   registry with its retention sweep, the workspace run lock, and the act pipeline seam
//!   that `run_workflow` drives. `run_status` and `cancel_run` read it.
//! * [`runner`] — the act process runner: the pure argv builder, the log tee with secret
//!   redaction and its two byte budgets, the process-group timeout/cancel teardown, and
//!   the terminal [`runner::ActRunOutcome`] the supervisor turns into evidence.
//! * [`fixtures`] — throwaway-git-repository builders shared by the unit tests and
//!   the integration tests (one definition, so the e2e suite reuses the same ones).
//!
//! ## Two rules this crate keeps
//!
//! 1. **stdout is sacred.** Logs go to stderr only. The stdout stream must stay
//!    empty so a supervisor can pipe this process without parsing its chatter;
//!    there is a test asserting exactly that (`tests/boot.rs`).
//! 2. **one protocol revision.** Negotiation, discovery and session handling are
//!    rmcp's job; this crate only narrows the supported set to
//!    [`server::SUPPORTED_PROTOCOL_VERSION`] and does not hand-roll any part of
//!    it.
//!
//! [`nektos/act`]: https://github.com/nektos/act

// Production code is audited for `unsafe` rather than forbidden from it, because exactly
// two modules need it, each with a `// SAFETY:` note at the call:
//   * `runner::process` has to reach `killpg(2)` and `prctl(2)`, for which `std` has no
//     safe API;
//   * `evidence::writer` has to reach `gethostname(2)` for the record envelope's `host`
//     field, which `std` does not expose at all (ADR-0002 §2.2 names that call).
// `deny` rather than `allow` at the crate root means any *new* unsafe outside those two
// modules still fails a build. The integration tests are their own crates and use
// `libc::kill` freely.
#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod config;
pub mod evidence;
pub mod fixtures;
/// The run supervisor: state machine, registry, workspace lock and the control tools'
/// backing logic (ADR-0004).
///
/// POSIX-only for the same reason [`runner`] is: the workspace lock is `flock`, the
/// teardown signals process groups, and both are the supervisor's only OS interaction.
#[cfg(unix)]
pub mod registry;
pub mod server;
pub mod tools;
pub mod workflow_yaml;
pub mod workflows;
pub mod workspace;

/// The act process runner.
///
/// POSIX-only, and deliberately so: process groups, `--bind`, `flock` and act itself are
/// all POSIX, and the ADR-0003 checker already treats the lock protocol as "POSIX-only by
/// decision". A non-POSIX build of this crate therefore has no runner to offer rather than
/// a silently weaker one.
#[cfg(unix)]
pub mod runner;

pub use config::{Config, ConfigError, ServeArgs};
pub use evidence::{EvidenceError, EvidenceRoot};
pub use server::ActMcp;
pub use workspace::{RunRequest, ValidatedRunTarget, ValidationError, ValidationPolicy, Validator};
