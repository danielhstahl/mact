//! The exit codes `fathom` leaves behind, and what each one promises to a script.
//!
//! A TUI is a program people put in Makefiles and CI steps as soon as it can print a table, so
//! the status is part of the interface and is spelled out here rather than in a match arm:
//!
//! | code | meaning |
//! |---|---|
//! | 0 | the command ran and found nothing to act on |
//! | 1 | a refusal (a typed [`crate::FathomError`] printed on stderr with a `fix:`), or a report gate that tripped, or a `doctor` check that came back `warn` |
//! | 2 | clap's own code for bad argv — and, for the two headless commands below, "could not run / something is broken" |
//! | 3 | the argv is understood and the *feature* is not wired yet, with the ticket that owns it |
//!
//! ## Why `report` and `doctor` use 0/1/2 differently from the rest
//!
//! The interactive commands follow the table above: 1 is a refusal, and a stub is 3. `fathom report
//! --fail-on` and `fathom doctor` are the two commands written *for* a CI step, and the ticket pins
//! the convention: **0 clean, 1 findings, 2 could not run** — the same three meanings
//! `act-mcp validate-evidence` has, so one `-run` line in a pipeline treats both tools alike.
//!
//! | code | `fathom report` | `fathom doctor` |
//! |---|---|---|
//! | 0 | the report was produced; the gate found nothing (`gate.status: off` or `clean`) | every check `ok` |
//! | 1 | `--fail-on` found something ([`Exit::Findings`]) | the worst check was `warn` |
//! | 2 | the report could not be produced ([`Exit::Blocked`]) | some check was `fail` |
//!
//! Both commands still exit 2 for a bad flag, because that is clap's and is never re-implemented
//! here. Code 3 remains what it was for every other subcommand: recognised argv, later ticket.

use std::error::Error;
use std::process::ExitCode;

/// The code `clap` exits with for a bad flag; no code in this crate returns it.
pub const USAGE: u8 = 2;

/// The outcomes this binary can reach on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// The command did its job and had nothing to complain about.
    Success,
    /// A typed refusal was printed to stderr (see [`crate::FathomError`]).
    Failure,
    /// The command ran and its gate or its checks found something to act on — a report finding, a
    /// `doctor` `warn`. The command worked; the repository is what has findings.
    Findings,
    /// The command could not do its job at all (`fathom report` on a workspace git will not answer
    /// for), or `doctor` found a check that is not merely worth a look but broken.
    Blocked,
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
            Self::Findings => 1,
            Self::Blocked => 2,
            Self::NotImplemented => 3,
        }
    }

    /// The same value as a [`std::process::ExitCode`].
    #[must_use]
    pub fn exit_code(self) -> ExitCode {
        ExitCode::from(self.code())
    }
}

/// Print a refusal the way this binary prints every one: the top line, then each `cause`, then the
/// `fix:` line the error already carries.
///
/// It lives here rather than in `main` because two callers need it: the binary, and `fathom report`,
/// which catches a refusal so it can exit `2` instead of `1` (see the table above) after saying what
/// went wrong. Everything goes to stderr — stdout is data, and a report redirected to a file must
/// stay parseable even on the way to a non-zero exit.
pub fn print_refusal(error: &dyn Error) {
    eprintln!("fathom: {error}");
    let mut source = error.source();
    while let Some(cause) = source {
        eprintln!("  caused by: {cause}");
        source = cause.source();
    }
}
