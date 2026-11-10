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
//! * [`workspace`] — the pre-start validation gate: turn untrusted `run_workflow`
//!   arguments plus the environment into a [`workspace::ValidatedRunTarget`], or
//!   into one typed ADR-0001 error. Nothing here talks to the network or spawns act.
//! * [`workflow_yaml`] — the minimal YAML view of a workflow file (declared name
//!   and job ids) that validation needs.
//! * [`server`] — the [`server::ActMcp`] service and the axum mount.
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

// Production code never needs `unsafe`; the integration tests use `libc::kill`
// to deliver SIGTERM, which is why this is a crate attribute rather than a
// `[lints]` entry in Cargo.toml.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod config;
pub mod fixtures;
pub mod server;
pub mod tools;
pub mod workflow_yaml;
pub mod workflows;
pub mod workspace;

pub use config::{Config, ConfigError, ServeArgs};
pub use server::ActMcp;
pub use workspace::{RunRequest, ValidatedRunTarget, ValidationError, ValidationPolicy, Validator};
