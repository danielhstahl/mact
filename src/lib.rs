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
//! * [`server`] — the [`server::ActMcp`] service and the axum mount.
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
pub mod server;

pub use config::{Config, ConfigError, ServeArgs};
pub use server::ActMcp;
