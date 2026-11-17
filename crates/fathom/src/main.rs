//! The `fathom` binary: parse argv, hand it to the library, print what came back.
//!
//! Thin on purpose (ADR-0005 §6): everything testable is in `fathom` the lib, so mact-by1.9 can
//! drive a screen and mact-by1.13 can diff a golden report without spawning a process. What this
//! file owns is the two things only a `main` can own — the process exit status, and the shape of a
//! refusal on stderr.
//!
//! A bad flag never reaches the library: `Clap::parse` prints clap's own message and exits `2`,
//! which is the code CI asserts for "your command line is wrong" ([`fathom::exit`]).

use std::process::ExitCode;

use clap::Parser;
use fathom::cli::{self, Cli};
use fathom::exit::print_refusal;
use fathom::Exit;

fn main() -> ExitCode {
    let cli = Cli::parse();

    match cli::run(&cli) {
        Ok(exit) => exit.exit_code(),
        Err(err) => {
            // The chain, not the Debug blob: the top line is the refusal, each cause is context,
            // and every `ConfigError`/`BootError` already carries a `fix:` line of its own. Same
            // printer `fathom report` uses when it needs to say a refusal and still exit 2.
            print_refusal(&err);
            Exit::Failure.exit_code()
        }
    }
}
