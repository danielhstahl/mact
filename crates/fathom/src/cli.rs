//! The `fathom` argv surface, and the dispatch that follows it.
//!
//! This module is the contract mact-by1.16 pins in CI. It is deliberately spelled out with help
//! text a human reads, because a CLI's help is its documentation and `fathom --help` is the first
//! thing anyone reads:
//!
//! ```text
//! fathom [WORKSPACE]                     the TUI (the default mode)
//! fathom tui [WORKSPACE]                 the same thing, spelled out
//! fathom index [WORKSPACE]               build or refresh the metric cache  (--status)
//! fathom report [WORKSPACE]              headless table on stdout
//! ```
//!
//! with the shared plumbing flags (`--evidence-dir`, `--cache-dir`, `--config`, `--from`, `--to`,
//! `--first-parent`, `-v`/`-vv`) accepted on the bare command *and* on every subcommand, because
//! `--cache-dir` is the same question whichever command asks it (ADR-0006 §3.1).
//!
//! What is not here yet, and where it comes from: the frames (mact-by1.9, by1.11, by1.12, by1.14),
//! the store (mact-by1.10), the report and `doctor` (mact-by1.13), the git reader (mact-by1.8).
//! Those subcommands are wired to `not_yet_wired` — they parse, they validate the workspace, and
//! they exit `3` naming the ticket instead of panicking or pretending to have produced a report
//! ([`crate::exit`]).

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::boot::{Access, Boot};
use crate::config::Config;
use crate::exit::Exit;
use crate::{debug, error, info, warn, FathomError};

/// Per-commit repo quality: coverage, benchmarks and maintainability, one row per commit.
#[derive(Parser, Debug)]
#[command(
    name = "fathom",
    version = env!("CARGO_PKG_VERSION"),
    about = "Read a workspace's .evidence and git history and show one row per commit",
    long_about = "Fathom is the reader: it opens `<workspace>/.evidence` (written by act-mcp, \
                  ADR-0002) and the repository's own history, and it never talks to the server — \
                  no MCP client, no HTTP, nothing that can start a run (ADR-0005 §7).\n\n\
                  Logs and warnings go to stderr; stdout is data, so `fathom report > timeline.tsv` \
                  stays parseable. Cache and evidence roots, `fathom.toml` and the metric store are \
                  ADR-0006's; `fathom index --status` prints what this build resolved.",
    propagate_version = true,
    disable_help_subcommand = true,
    term_width = 100
)]
pub struct Cli {
    /// The shared flags. `global = true` on each, so they work as `fathom --cache-dir x tui` and as
    /// `fathom tui --cache-dir x` — one argv spelling for both orders, and clap's own error for a
    /// bad one (exit 2, never re-implemented here).
    #[command(flatten)]
    pub common: CommonArgs,

    /// The workspace to read; the same as `fathom tui [WORKSPACE]`.
    #[arg(value_name = "WORKSPACE")]
    pub workspace: Option<PathBuf>,

    /// The command; bare `fathom [WORKSPACE]` runs the TUI.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// The flags every subcommand shares (ADR-0006 §3.1 and §7.4).
///
/// Two of them are also environment-configurable, and exactly two: `EVIDENCE_DIR` (the same name
/// the server reads, so a reader and a writer pointed at one workspace agree by construction) and
/// `FATHOM_DIR`. Per-key variables (`FATHOM_PATHS_NOISE` and friends) are refused by ADR-0006 §7.1
/// — a value set in a CI job's environment that never reaches the digest of the run that used it is
/// the failure mode that rule exists to prevent.
#[derive(Args, Debug, Clone)]
pub struct CommonArgs {
    /// Evidence root; defaults to the workspace's own `.evidence`, or `$EVIDENCE_DIR`.
    #[arg(
        long,
        value_name = "PATH",
        global = true,
        env = "EVIDENCE_DIR",
        help = "Read .evidence from here instead of <workspace>/.evidence (same flag and env name act-mcp writes with)"
    )]
    pub evidence_dir: Option<PathBuf>,

    /// Cache root; defaults to the workspace's own `.fathom`, or `$FATHOM_DIR`.
    #[arg(
        long,
        value_name = "PATH",
        global = true,
        env = "FATHOM_DIR",
        help = "The metric cache root (ADR-0006 §3): disposable, and never read from fathom.toml"
    )]
    pub cache_dir: Option<PathBuf>,

    /// Oldest commit to include: a rev, a branch, or anything `git rev-parse` accepts.
    #[arg(
        long,
        value_name = "REV",
        global = true,
        help = "Start the window here instead of `history.depth` commits back from --to"
    )]
    pub from: Option<String>,

    /// Newest commit to include; defaults to `HEAD`.
    #[arg(
        long,
        value_name = "REV",
        global = true,
        help = "End the window here (default HEAD)"
    )]
    pub to: Option<String>,

    /// Follow only the first parent (overrides `history.first_parent`).
    #[arg(
        long,
        global = true,
        help = "Walk only first parents; this changes per-commit deltas, so it costs a re-index (ADR-0006 §7.4)"
    )]
    pub first_parent: bool,

    /// `fathom.toml` to read; skips the root-ward search.
    #[arg(
        long,
        value_name = "PATH",
        global = true,
        env = "FATHOM_CONFIG",
        help = "Read this config file instead of searching for fathom.toml; an unreadable file is a refusal, never a fallback"
    )]
    pub config: Option<PathBuf>,

    /// `-v` for info, `-vv` for trace; overridden by $RUST_LOG when that is set.
    #[arg(
        short = 'v',
        long = "verbose",
        action = clap::ArgAction::Count,
        global = true,
        help = "Raise the log level on stderr: -v info, -vv trace (RUST_LOG-style, e.g. RUST_LOG=fathom::config=trace)"
    )]
    pub verbose: u8,
}

/// The subcommands. All three are recognised today; two of them print
/// [`crate::exit::Exit::NotImplemented`] after validating the workspace, because their engines are
/// later tickets and a stub that exits 0 would let CI assert a report was produced.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// The timeline TUI: one row per commit (the default when no subcommand is given).
    Tui(TuiArgs),

    /// Build or refresh the metric cache in the cache root's `index/` directory.
    #[command(about = "Build or refresh the metric cache in <cache root>/index")]
    Index(IndexArgs),

    /// Print the timeline as a table on stdout, no TTY needed.
    Report(ReportArgs),
}

/// `fathom tui [WORKSPACE]`.
#[derive(Args, Debug)]
pub struct TuiArgs {
    /// The workspace to read; defaults to the current directory.
    #[arg(value_name = "WORKSPACE")]
    pub workspace: Option<PathBuf>,
}

/// `fathom index [WORKSPACE] [--status]`.
#[derive(Args, Debug)]
pub struct IndexArgs {
    /// The workspace to index; defaults to the current directory.
    #[arg(value_name = "WORKSPACE")]
    pub workspace: Option<PathBuf>,

    /// Report the state of the store and of what this build resolved, and write nothing.
    #[arg(
        long,
        help = "Print the store and configuration state as key<TAB>value lines and exit 0"
    )]
    pub status: bool,
}

/// `fathom report [WORKSPACE]`.
#[derive(Args, Debug)]
pub struct ReportArgs {
    /// The workspace to report on; defaults to the current directory.
    #[arg(value_name = "WORKSPACE")]
    pub workspace: Option<PathBuf>,
}

/// What the argv asked for, after the default subcommand is filled in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Draw the timeline. This ticket cannot: no frame is drawn before mact-by1.9.
    Tui,
    /// Write the store. This ticket cannot: the store is mact-by1.10's.
    Index,
    /// Answer questions about the workspace and the store; the one `index` mode that works today.
    IndexStatus,
    /// Print the table. This ticket cannot: the table is mact-by1.13's.
    Report,
}

impl Mode {
    /// The access this mode needs from the cache root (ADR-0006 §10.1).
    #[must_use]
    pub const fn access(self) -> Access {
        match self {
            Self::Index => Access::Write,
            Self::Tui | Self::IndexStatus | Self::Report => Access::Read,
        }
    }

    /// The ticket that fills this mode in, for the `not yet wired` line.
    #[must_use]
    pub const fn owner(self) -> &'static str {
        match self {
            Self::Tui => "mact-by1.9 (the timeline screen, ratatui)",
            Self::Index => "mact-by1.10 (the metric cache store)",
            Self::IndexStatus => "mact-by1.10 (the metric cache store)",
            Self::Report => "mact-by1.13 (the headless report)",
        }
    }
}

/// Parse-free part of running: everything after `Clap::parse()`.
///
/// Order is deliberate: the log filter first (so a boot failure is explainable at `-v`), then the
/// workspace, then the roots, then the config, then the command. A refusal at any step is printed
/// by `main` with its `fix:` line and exits `1`.
///
/// # Errors
///
/// [`crate::FathomError`] for any refusal: a workspace that is not a git work tree, a cache root
/// that cannot be created or written by a writer, a config file that does not parse.
pub fn run(cli: &Cli) -> Result<Exit, FathomError> {
    crate::log::init(cli.common.verbose);

    let (workspace_argument, mode) = resolve_request(cli);
    let workspace = Boot::resolve_workspace(workspace_argument.as_deref())?;
    let boot = Boot::validate(&workspace, &cli.common, mode.access())?;

    let mut config = Config::discover(
        cli.common.config.as_deref(),
        &boot.workspace,
        boot.git_root.as_deref(),
    )?;
    if cli.common.first_parent {
        // The one flag that is also config (§7.4), and the one that costs a re-index: ADR-0008's
        // per-commit delta is measured against a parent, so which parent decides the digest.
        config.history.first_parent = true;
        info!("--first-parent overrides history.first_parent; the `history` section re-derives");
    }

    // Warnings first: they are the reason a column may come back unknown later.
    for warning in &boot.warnings {
        warn!("{}", warning);
    }

    info!(
        "workspace {} (git root {})",
        boot.workspace.display(),
        boot.git_root
            .as_ref()
            .map_or_else(|| "none".to_owned(), |root| root.display().to_string())
    );
    debug!("config {}", config);
    debug!(
        "evidence root {} [{}]",
        boot.evidence.path.display(),
        boot.evidence.state()
    );
    debug!(
        "cache root {} [{}]{}",
        boot.cache.path.display(),
        boot.cache.state(),
        if boot.cache.layout_written {
            ", LAYOUT.md written"
        } else {
            ""
        }
    );
    // The one thing the contract crate is needed for before there is a reader: saying which
    // evidence schema this build refuses to guess about (ADR-0002 §7).
    debug!(
        "this build reads evidence schema v{}",
        act_evidence::SCHEMA_VERSION
    );

    match mode {
        Mode::IndexStatus => {
            print_status(&boot, &config, &cli.common);
            Ok(Exit::Success)
        }
        Mode::Tui | Mode::Index | Mode::Report => {
            not_yet_wired(mode);
            Ok(Exit::NotImplemented)
        }
    }
}

/// Which workspace and which mode the argv asked for.
///
/// A workspace may be given at the top level (`fathom ../repo tui`) or after the subcommand
/// (`fathom tui ../repo`); the subcommand's own argument wins when both are present, and the
/// position of the flag is logged at `-v` so a confused user can see what fathom chose.
fn resolve_request(cli: &Cli) -> (Option<PathBuf>, Mode) {
    let top = cli.workspace.clone();
    match &cli.command {
        None => (top, Mode::Tui),
        Some(Command::Tui(args)) => (pick(args.workspace.clone(), &top, "tui"), Mode::Tui),
        Some(Command::Index(args)) => (
            pick(args.workspace.clone(), &top, "index"),
            if args.status {
                Mode::IndexStatus
            } else {
                Mode::Index
            },
        ),
        Some(Command::Report(args)) => (pick(args.workspace.clone(), &top, "report"), Mode::Report),
    }
}

fn pick(subcommand: Option<PathBuf>, top: &Option<PathBuf>, name: &str) -> Option<PathBuf> {
    if subcommand.is_some() && top.is_some() {
        debug!(
            "`fathom {}` was given a workspace twice; using the one after the subcommand",
            name
        );
    }
    subcommand.or_else(|| top.clone())
}

/// The `key<TAB>value` table `fathom index --status` writes on stdout — the only thing this ticket
/// prints there, and the shape `report` will grow into.
fn print_status(boot: &Boot, config: &Config, common: &CommonArgs) {
    let mut lines = boot.status_lines();
    lines.extend(config.status_lines());
    lines.push((
        "window.from".to_owned(),
        common
            .from
            .clone()
            .unwrap_or_else(|| "(history.depth)".to_owned()),
    ));
    lines.push((
        "window.to".to_owned(),
        common.to.clone().unwrap_or_else(|| "HEAD".to_owned()),
    ));

    // `stdout` is data: nothing else in this crate may write it, and nothing here logs to it
    // (ADR-0005 §6; `crates/fathom/tests/cli.rs` asserts the split for the other commands).
    let mut out = String::new();
    for (key, value) in lines {
        out.push_str(&key);
        out.push('\t');
        out.push_str(&value.replace('\t', " "));
        out.push('\n');
    }
    print!("{out}");
}

/// The `not yet wired` line: stderr, exit 3, and the ticket that owns the rest.
///
/// Not `todo!()`/`unimplemented!()`, which would be a panic in a user's terminal and a core dump in
/// CI: the argv is honoured, the workspace was validated, and what is missing is named with its
/// owner.
fn not_yet_wired(mode: Mode) {
    let name = match mode {
        Mode::Tui => "tui",
        Mode::Index => "index",
        Mode::IndexStatus => "index --status",
        Mode::Report => "report",
    };
    error!(
        "fathom {}: not yet wired — {} lands it. The argv and the boot checks above are real; the \
         output is not.",
        name,
        mode.owner()
    );
    error!(
        "  fix: `fathom index --status [WORKSPACE]` prints what this build already knows (roots, \
         config, store state) as key<TAB>value lines"
    );
}
