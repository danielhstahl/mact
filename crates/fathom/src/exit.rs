//! The exit codes `fathom` leaves behind, and what each one promises to a script.
//!
//! A TUI is a program people put in Makefiles and CI steps as soon as it can print a table, so
//! the status is part of the interface and is spelled out here rather than in a match arm:
//!
//! | code | meaning |
//! |---|---|
//! | 0 | the command ran (`fathom index --status` answering a question about the store) |
//! | 1 | a refusal: a typed [`crate::FathomError`] was printed on stderr with a `fix:` line |
//! | 2 | the argv was not valid — clap's own code, and clap owns the message (never re-implemented) |
//! | 3 | the argv is understood and the *feature* is not wired yet, with the ticket that owns it |
//!
//! Code 3 is the interesting one. A stub that exits 0 would let a CI step assert that `fathom
//! report` produced a report; a panic would be worse. `3` says "your command line is correct,
//! this build stops here, and here is the ticket that fills it in" — which is what lets
//! mact-by1.16 land a golden argv test before mact-by1.13 writes the table.

use std::process::ExitCode;

/// What `clap` exits with for a bad flag (ADR-0005/this ticket's argv contract).
pub const USAGE: u8 = 2;

/// The three outcomes this binary can reach on its own; `2` belongs to clap and is never returned
/// by any code in this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// The command did its job.
    Success,
    /// A typed refusal was printed to stderr (see [`crate::FathomError`]).
    Failure,
    /// The subcommand is recognised, the workspace validated, and the feature is a later ticket.
    NotImplemented,
}

impl Exit {
    /// The process status for this outcome.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Success => 0,
            Self::Failure => 1,
            Self::NotImplemented => 3,
        }
    }

    /// The same value as a [`std::process::ExitCode`].
    #[must_use]
    pub fn exit_code(self) -> ExitCode {
        ExitCode::from(self.code())
    }
}
