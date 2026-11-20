//! Diagnostics, stderr only, with a hand-rolled `RUST_LOG`-style filter.
//!
//! ## Why this module exists instead of `tracing_subscriber::fmt`
//!
//! ADR-0005 §6 forbids a tracing subscriber and `env_logger` in `crates/fathom`, and the reason is
//! the rule this crate lives by: **logs go to stderr, stdout is data**. A TUI owns the terminal,
//! `fathom report > timeline.tsv` must stay parseable, and a subscriber crate is exactly the
//! dependency whose default writer is "print to stdout" — the one line of configuration that, if
//! missed, corrupts a user's file. So fathom has no subscriber, and thirty-odd lines that cannot
//! write to stdout by construction.
//!
//! ## The filter, which is `RUST_LOG`-style and a documented subset
//!
//! * `-v` raises the default level to `info`, `-vv` to `trace`; with neither, `warn` is the
//!   default, because fathom is a program people look at rather than a service that logs.
//! * `RUST_LOG` overrides the flag entirely, and understands the part of the `env-filter` grammar
//!   that is a promise rather than a parser: comma-separated directives, each either a bare level
//!   (`debug`, which sets the default) or `target_prefix=level` (`fathom::boot=trace`). A directive
//!   that parses to neither is reported on stderr, never silently dropped — a filter that quietly
//!   does nothing is how "no logs" becomes a mystery.
//! * The longest matching target prefix wins, so `fathom=info,fathom::config=trace` means what it
//!   says. `off` silences a target (or everything, bare).
//!
//! Levels are the `log`/`tracing` set — error, warn, info, debug, trace — spelled the same way, so
//! a `RUST_LOG` written for any other Rust program works here unedited.

use std::{
    fmt,
    io::IsTerminal as _,
    sync::atomic::{AtomicU8, Ordering},
};

/// The five levels, ordered by how much they say. `off` is level `0` and is not a variant: it is
/// the absence of a threshold, which keeps "print nothing" out of every comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Level {
    /// Something is wrong and the user should see it without asking.
    Error = 1,
    /// Something is worth seeing and was survivable — a missing `.evidence`, a read-only cache.
    Warn = 2,
    /// What happened, at the granularity of a boot step.
    Info = 3,
    /// Resolved values: which config file, which root, which precedence won.
    Debug = 4,
    /// Everything, including every argv detail.
    Trace = 5,
}

impl Level {
    /// The level names accepted in `RUST_LOG`, plus `off`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "error" => Some(Self::Error),
            "warn" | "warning" => Some(Self::Warn),
            "info" => Some(Self::Info),
            "debug" => Some(Self::Debug),
            "trace" => Some(Self::Trace),
            _ => None,
        }
    }

    /// The four-letter label printed with the line.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Error => "ERROR",
            Self::Warn => "WARN ",
            Self::Info => "INFO ",
            Self::Debug => "DEBUG",
            Self::Trace => "TRACE",
        }
    }

    /// The level names `RUST_LOG` accepts here, `off` first because it is the one that surprises
    /// someone who wrote `RUST_LOG=0`.
    pub const ALL: &'static [&'static str] = &["off", "error", "warn", "info", "debug", "trace"];
}

/// The default level with no `-v` and no `RUST_LOG`: warnings and errors.
const DEFAULT_LEVEL: u8 = Level::Warn as u8;
/// `-v`.
const VERBOSE_LEVEL: u8 = Level::Info as u8;
/// `-vv` and beyond.
const VERY_VERBOSE_LEVEL: u8 = Level::Trace as u8;
/// The largest verbosity this flag distinguishes.
pub const MAX_VERBOSE: u8 = 2;

/// The global threshold, one byte, no lock, no `unsafe`: `0` means "off".
static BASE: AtomicU8 = AtomicU8::new(DEFAULT_LEVEL);
/// Per-target directives, parsed once by [`init`]. A `OnceLock` rather than a `Mutex` because it
/// is written exactly once, before anything can read it, and read-only afterwards.
static DIRECTIVES: std::sync::OnceLock<Vec<(String, u8)>> = std::sync::OnceLock::new();
/// Whether to colourise the level label (only when stderr is a terminal).
static COLOUR: AtomicU8 = AtomicU8::new(0);

/// Install the filter. Called once, before the first refusal can be printed.
///
/// `verbosity` is clap's `-v` count; `RUST_LOG` wins when it is set and non-empty, because an
/// explicit filter is a more specific statement of intent than a flag.
pub fn init(verbosity: u8) {
    if std::io::stderr().is_terminal() {
        COLOUR.store(1, Ordering::Relaxed);
    }

    let base_from_flag = match verbosity.min(MAX_VERBOSE) {
        0 => DEFAULT_LEVEL,
        1 => VERBOSE_LEVEL,
        _ => VERY_VERBOSE_LEVEL,
    };

    let raw = std::env::var("RUST_LOG").unwrap_or_default();
    if raw.trim().is_empty() {
        BASE.store(base_from_flag, Ordering::Relaxed);
        let _ = DIRECTIVES.set(Vec::new());
        return;
    }

    let mut base = base_from_flag;
    let mut directives: Vec<(String, u8)> = Vec::new();
    for token in raw.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        let (target, level_text) = match token.split_once('=') {
            Some((target, level)) => (target.trim(), level.trim()),
            None => ("", token),
        };
        let Some(level) = parse_level_or_off(level_text) else {
            // Never a shrug: an ignored directive is indistinguishable from a filter that matched
            // nothing, so say which token was not understood and what was understood instead.
            eprintln!(
                "fathom: RUST_LOG directive {token:?} is not a level or a target=level pair\n  \
                 fix: levels are {}; a directive may name a target prefix, e.g. fathom::boot=trace",
                Level::ALL.join(", ")
            );
            continue;
        };
        if target.is_empty() {
            base = level;
        } else {
            directives.push((target.to_owned(), level));
        }
    }
    BASE.store(base, Ordering::Relaxed);
    let _ = DIRECTIVES.set(directives);
}

/// `off` in addition to [`Level::parse`]'s names.
fn parse_level_or_off(text: &str) -> Option<u8> {
    if text.eq_ignore_ascii_case("off") {
        return Some(0);
    }
    Level::parse(text).map(|level| level as u8)
}

/// Which threshold applies to `target`: the longest matching directive, else `base`.
///
/// Pure, so the precedence rule has a test that does not depend on process-global state. It picks
/// the longest match rather than the first, so the rule does not depend on the order the directives
/// were written in.
fn threshold_for(base: u8, directives: &[(String, u8)], target: &str) -> u8 {
    directives
        .iter()
        .filter(|(prefix, _)| target.starts_with(prefix.as_str()))
        .max_by_key(|(prefix, _)| prefix.len())
        .map_or(base, |(_, level)| *level)
}

/// Is `level` for `target` going to be printed?
#[must_use]
pub fn enabled(level: Level, target: &str) -> bool {
    let base = BASE.load(Ordering::Relaxed);
    let threshold = DIRECTIVES
        .get()
        .map_or(base, |directives| threshold_for(base, directives, target));
    (level as u8) <= threshold
}

/// Write one line to stderr. There is no `stdout` branch in this function, and that is the rule.
pub fn emit(level: Level, target: &str, args: fmt::Arguments<'_>) {
    if !enabled(level, target) {
        return;
    }
    let colour = match COLOUR.load(Ordering::Relaxed) {
        0 => String::new(),
        _ => match level {
            Level::Error => "\x1b[31m".to_owned(),
            Level::Warn => "\x1b[33m".to_owned(),
            _ => "\x1b[2m".to_owned(),
        },
    };
    let reset = if colour.is_empty() { "" } else { "\x1b[0m" };
    let target = if target.is_empty() {
        String::new()
    } else {
        format!(" {target}")
    };
    // `eprintln!` and nothing else: stdout is reserved for frames and for `report`'s table.
    eprintln!("{colour}fathom [{}]{}{reset} {args}", level.label(), target);
}

/// One line at `error`.
#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => { $crate::log::emit($crate::log::Level::Error, module_path!(), format_args!($($arg)*)) };
}

/// One line at `warn` — the level a user sees by default.
#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => { $crate::log::emit($crate::log::Level::Warn, module_path!(), format_args!($($arg)*)) };
}

/// One line at `info` (`-v`).
#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => { $crate::log::emit($crate::log::Level::Info, module_path!(), format_args!($($arg)*)) };
}

/// One line at `debug`.
#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => { $crate::log::emit($crate::log::Level::Debug, module_path!(), format_args!($($arg)*)) };
}

/// One line at `trace` (`-vv`).
#[macro_export]
macro_rules! trace {
    ($($arg:tt)*) => { $crate::log::emit($crate::log::Level::Trace, module_path!(), format_args!($($arg)*)) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_names_are_the_log_crate_spelling() {
        assert_eq!(Level::parse("debug"), Some(Level::Debug));
        assert_eq!(Level::parse(" WARN "), Some(Level::Warn));
        assert_eq!(Level::parse("chatty"), None);
        // The ordering the filter compares on.
        assert!(Level::Trace > Level::Debug);
        assert!(Level::Debug > Level::Warn);
        assert!(Level::Warn > Level::Error);
    }

    #[test]
    fn the_longest_target_prefix_wins() {
        let directives = vec![
            ("fathom".to_owned(), Level::Info as u8),
            ("fathom::boot".to_owned(), Level::Trace as u8),
        ];
        assert_eq!(
            threshold_for(Level::Warn as u8, &directives, "fathom::boot"),
            Level::Trace as u8
        );
        assert_eq!(
            threshold_for(Level::Warn as u8, &directives, "fathom::config"),
            Level::Info as u8
        );
        // No match at all: the base (from `-v`, or from a bare `RUST_LOG` level) applies.
        assert_eq!(
            threshold_for(Level::Warn as u8, &directives, "other"),
            Level::Warn as u8
        );
    }

    #[test]
    fn off_is_zero_and_silences_a_target() {
        let directives = vec![("fathom::git".to_owned(), 0)];
        assert_eq!(
            threshold_for(Level::Trace as u8, &directives, "fathom::git"),
            0
        );
        assert!(!enabled_at(0, Level::Error));
        assert!(enabled_at(Level::Trace as u8, Level::Trace));
    }

    /// The comparison [`enabled`] makes, exposed for the test above.
    fn enabled_at(threshold: u8, level: Level) -> bool {
        (level as u8) <= threshold
    }
}
