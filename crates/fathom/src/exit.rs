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
use std::io::{self, Write};
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

/// Why a write to a data stream failed, and which of the two very different facts it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteFailure {
    /// The reader hung up (`EPIPE`): a `| head`, a pager that quit, a `less` the user left. Every number
    /// was derived and every gate ran; the only thing missing is somebody at the other end. That is a
    /// normal thing to do to this binary, so the caller keeps the exit code it had already earned.
    ReaderGone,
    /// The stream is broken in a way that *is* a fault — a full disk, a closed descriptor, a read-only
    /// redirect. The payload is the operating system's own sentence, already one line.
    Broken(String),
}

impl WriteFailure {
    /// Whether the write stopped because the reader left rather than because anything broke.
    #[must_use]
    pub const fn reader_gone(&self) -> bool {
        matches!(self, Self::ReaderGone)
    }
}

/// Write `text` to `out`, and tell a closed reader apart from a broken stream.
///
/// This exists because `print!` panics on a write error, and because Rust ignores `SIGPIPE` at startup,
/// so the first write to a pipe nobody is reading is an `EPIPE` — not a signal, an `Err`. A tool whose
/// own contract is *stdout is data* (ADR-0005 §6) gets asked to be piped first and read second:
/// `fathom report --format tsv | head -1` is one of the first things a reader types, and before this
/// function it was a core dump with a `101` no table in this repository promises.
///
/// `flush` is part of the write: stdout is line-buffered, and the error a closed reader returns from the
/// flush is the same fact arriving one call later.
pub fn write_data(out: &mut impl Write, text: &str) -> Result<(), WriteFailure> {
    out.write_all(text.as_bytes())
        .and_then(|()| out.flush())
        .map_err(|reason| {
            if reason.kind() == io::ErrorKind::BrokenPipe {
                WriteFailure::ReaderGone
            } else {
                WriteFailure::Broken(one_line(&reason.to_string()))
            }
        })
}

/// [`write_data`] against this process's stdout, which is where every command's data goes.
///
/// A reader that hung up is `Ok(())`, quietly: `fathom report --fail-on coverage-drop | head -1` must not
/// turn a finding into a crash *or* into a clean run, so the caller keeps its own verdict and says nothing
/// about a pipe it was not asked to keep open. Anything else is a refusal in this binary's usual shape —
/// one line and a `fix:` — and the caller maps it the way it maps every other refusal.
pub fn print_data(text: &str) -> Result<(), crate::FathomError> {
    let mut out = io::stdout().lock();
    match write_data(&mut out, text) {
        Ok(()) | Err(WriteFailure::ReaderGone) => Ok(()),
        Err(WriteFailure::Broken(reason)) => Err(crate::FathomError::Output(format!(
            "cannot write to stdout: {reason}\n  fix: a closed reader is never this error — a `| head` \
             and a pager are both fine. Something else is wrong with where stdout points: check the \
             redirect, the disk and the descriptor"
        ))),
    }
}

/// The first line of a multi-line message, trimmed: an operating system's sentence belongs on the same
/// line as the word that introduced it.
#[must_use]
fn one_line(text: &str) -> String {
    text.lines().next().unwrap_or(text).trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sink whose writes always fail with one kind, which is the only way to meet `EPIPE` on a machine
    /// that is not currently being piped into `head`.
    struct Fails(io::ErrorKind);

    impl Write for Fails {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(self.0, "the sink is like this today"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_write_that_fits_goes_out_whole() {
        let mut out = Vec::new();
        assert!(write_data(&mut out, "sha\tage\n").is_ok());
        assert_eq!(String::from_utf8(out).expect("ascii"), "sha\tage\n");
    }

    #[test]
    fn a_reader_that_hung_up_is_not_a_broken_stream() {
        let mut out = Fails(io::ErrorKind::BrokenPipe);
        let why = write_data(&mut out, "data").expect_err("the sink always fails");
        assert_eq!(why, WriteFailure::ReaderGone);
        assert!(why.reader_gone());
    }

    #[test]
    fn anything_else_keeps_the_operating_systems_sentence() {
        let mut out = Fails(io::ErrorKind::StorageFull);
        let why = write_data(&mut out, "data").expect_err("the sink always fails");
        assert!(
            matches!(&why, WriteFailure::Broken(reason) if reason.contains("sink")),
            "{why:?}"
        );
        assert!(!why.reader_gone());
    }

    #[test]
    fn a_failure_with_no_sentence_of_its_own_still_names_what_happened() {
        // `ErrorKind::StorageFull`'s `to_string` is one line on linux; a kind with a two-line message
        // must not push the refusal's `fix:` onto the second row.
        let mut out = Fails(io::ErrorKind::Other);
        assert!(matches!(
            write_data(&mut out, "data"),
            Err(WriteFailure::Broken(_))
        ));
    }
}
