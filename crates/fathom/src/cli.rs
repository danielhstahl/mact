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
//! fathom report [WORKSPACE]              headless table on stdout  (--format, --only-missing, --fail-on)
//! fathom doctor [WORKSPACE]              why there is nothing to show  (--json)
//! ```
//!
//! with the shared plumbing flags (`--evidence-dir`, `--cache-dir`, `--config`, `--from`, `--to`,
//! `--first-parent`, `--limit`, `-v`/`-vv`) accepted on the bare command *and* on every subcommand,
//! because `--cache-dir` is the same question whichever command asks it (ADR-0006 §3.1).
//!
//! What is not here yet, and where it comes from: mact-by1.14's commit detail frame, which is a key
//! inside the TUI (`enter`) rather than a subcommand, and whose key already answers with the ticket
//! that owns it.
//!
//! `fathom tui` is wired for real (mact-by1.9), including the diff view (mact-by1.11) and the benchmark
//! panel (mact-by1.12): `d` or `b` on a row inside the screen, or `--diff REV` / `--bench REV` on the
//! command line, which are also what `--plain --diff REV` and `--plain --bench REV` print to a pipe.
//!
//! `fathom index` is wired for real (mact-by1.10): it takes the cache lock, builds or updates
//! `index/`, and publishes its own counters. It is the only command in this crate that writes.
//!
//! `fathom report` and `fathom doctor` are wired for real (mact-by1.13), and they are the two commands
//! whose exit codes follow CI's 0/1/2 convention instead of the binary's 0/1/3 one — see
//! [`crate::exit`]'s table, [`crate::report`] and [`crate::doctor`].

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::boot::{Access, Boot};
use crate::config::Config;
use crate::exit::Exit;
use crate::history::Query;
use crate::store::{self, Runbook, Store};
use crate::{debug, info, warn, FathomError};

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

    /// Print the timeline as text instead of drawing a TUI (mact-by1.9's escape hatch).
    #[arg(
        long,
        global = true,
        help = "Print the timeline table to stdout instead of taking over the terminal; implied by a non-tty stdout or TERM=dumb (timeline screen only, mact-by1.9)"
    )]
    pub plain: bool,

    /// Turn colour off while keeping the unicode glyphs; `$NO_COLOR` does the same.
    #[arg(
        long,
        global = true,
        help = "No colour, unicode glyphs kept (the no-color.org convention: any non-empty $NO_COLOR does the same). The glyphs always carry every state, so nothing is lost (ADR-0008 §6.1)"
    )]
    pub no_color: bool,

    /// Cap how many commits the timeline shows, newest first.
    #[arg(
        long,
        value_name = "N",
        global = true,
        help = "Show at most N commits, newest first; a view choice, so it changes nothing about what is cached (ADR-0006 §7.4)"
    )]
    pub limit: Option<std::num::NonZeroUsize>,

    /// Open one commit's diff instead of the timeline (mact-by1.11's key, spelled on the command line).
    #[arg(
        long,
        value_name = "REV",
        global = true,
        help = "Open this commit's diff instead of the timeline: a SHA, HEAD, a branch, or a form like HEAD~3. With --plain it prints that diff as text"
    )]
    pub diff: Option<String>,

    /// Open one commit's benchmark panel instead of the timeline (mact-by1.12's key, on the command line).
    #[arg(
        long,
        value_name = "REV",
        global = true,
        help = "Open this commit's benchmark panel instead of the timeline: a SHA, HEAD, a branch, or a form like HEAD~3. With --plain it prints that panel as text"
    )]
    pub bench: Option<String>,

    /// Open one commit's detail view instead of the timeline (mact-by1.14's key, on the command line).
    #[arg(
        long,
        value_name = "REV",
        global = true,
        help = "Open this commit's detail view instead of the timeline — per-file coverage, MI hotspots and the diffstat: a SHA, HEAD, a branch, or a form like HEAD~3. With --plain it prints all three panes as text"
    )]
    pub detail: Option<String>,

    /// The whole-commit patch cap, in bytes.
    #[arg(
        long,
        value_name = "BYTES",
        global = true,
        help = "Stop reading a commit's patch after this many bytes (default 1048576). When it bites the view says so and names this flag, because a silently cut diff reads as a smaller commit"
    )]
    pub max_diff_bytes: Option<std::num::NonZeroUsize>,

    /// The per-file patch cap, in bytes.
    #[arg(
        long,
        value_name = "BYTES",
        global = true,
        help = "Stop reading one file's patch after this many bytes (default 131072), so one generated file cannot eat the whole commit budget and hide the other files"
    )]
    pub max_diff_bytes_per_file: Option<std::num::NonZeroUsize>,

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

/// The subcommands. All five are recognised and wired; the ones whose engine is a later ticket exit
/// [`crate::exit::Exit::NotImplemented`] after validating the workspace, because a stub that exits 0
/// would let CI assert a report was produced.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// The timeline TUI: one row per commit (the default when no subcommand is given).
    Tui(TuiArgs),

    /// Build or refresh the metric cache in the cache root's `index/` directory.
    #[command(about = "Build or refresh the metric cache in <cache root>/index")]
    Index(IndexArgs),

    /// Print the timeline as a table on stdout, no TTY needed.
    Report(ReportArgs),

    /// Explain why the timeline is empty: workspace, evidence, workflows, cache, languages, config.
    Doctor(DoctorArgs),
}

/// `fathom tui [WORKSPACE]`.
#[derive(Args, Debug)]
pub struct TuiArgs {
    /// The workspace to read; defaults to the current directory.
    #[arg(value_name = "WORKSPACE")]
    pub workspace: Option<PathBuf>,
}

/// `fathom index [WORKSPACE] [--status] [--rebuild]`.
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

    /// Throw the store away and build it again from nothing (ADR-0006 §3.2: the store is disposable).
    #[arg(
        long,
        help = "Delete <cache root>/index and rebuild it; nothing else under the cache root is touched"
    )]
    pub rebuild: bool,
}

/// `fathom report [WORKSPACE]` — the timeline, machine-readable (mact-by1.13).
///
/// The window flags (`--from`, `--to`, `--limit`) and the rev-selection flags are the shared ones, and
/// deliberately so: `fathom report --from v1.2` must mean the window `fathom index --from v1.2` built,
/// or the report would be answering a different question than the index was asked.
#[derive(Args, Debug)]
pub struct ReportArgs {
    /// The workspace to report on; defaults to the current directory.
    #[arg(value_name = "WORKSPACE")]
    pub workspace: Option<PathBuf>,

    /// The output format.
    #[arg(
        long,
        value_name = "FMT",
        default_value = "json",
        help = "json (the stable, versioned envelope), tsv (the timeline's own columns, ascii glyphs) or csv; stdout carries the data and nothing else"
    )]
    pub format: crate::report::Format,

    /// Only commits with no evidence — the queue a runner works through.
    #[arg(
        long,
        help = "Only commits with no evidence at all (no coverage percent, no benchmark group, no action verdict), including commits the index never reached: the list an act-mcp sweep would consume"
    )]
    pub only_missing: bool,

    /// The CI gate; repeat the flag to ask more than one question.
    #[arg(
        long,
        value_name = "WHAT",
        value_delimiter = ',',
        help = "Exit 1 when this is true of a commit in the window (repeat or comma-separate): coverage-drop, mi-drop, benchmark-regression, action-failure. 0 clean, 1 findings, 2 could not run — the same codes act-mcp validate-evidence uses"
    )]
    pub fail_on: Vec<crate::report::FailOn>,
}

/// `fathom doctor [WORKSPACE] [--json]` — the explanation command (mact-by1.13).
#[derive(Args, Debug)]
pub struct DoctorArgs {
    /// The workspace to explain; defaults to the current directory.
    #[arg(value_name = "WORKSPACE")]
    pub workspace: Option<PathBuf>,

    /// The same checks as one JSON document.
    #[arg(
        long,
        help = "Print the checks as JSON (name, status, summary, details, fixes) instead of the text report; the exit code is the worst check either way"
    )]
    pub json: bool,
}

/// What the argv asked for, after the default subcommand is filled in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Draw the timeline — the default mode, and the one `fathom` with no subcommand means (mact-by1.9).
    Tui,
    /// Write the store: the walk, the derive, the metric cache (mact-by1.10).
    Index,
    /// Answer questions about the workspace and the store; the one `index` mode that writes nothing.
    IndexStatus,
    /// Print the table, machine-readable (mact-by1.13).
    Report,
    /// Explain why there is nothing to show (mact-by1.13).
    Doctor,
}

impl Mode {
    /// The access this mode needs from the cache root (ADR-0006 §10.1).
    #[must_use]
    pub const fn access(self) -> Access {
        match self {
            Self::Index => Access::Write,
            Self::Tui | Self::IndexStatus | Self::Report | Self::Doctor => Access::Read,
        }
    }

    /// Whether a refusal in this mode exits `2` ("could not run") rather than `1`.
    ///
    /// The two headless commands, and only them: they exist to be run by a script, and their 0/1/2
    /// convention is `act-mcp validate-evidence`'s ([`crate::exit`]). `doctor` never raises a refusal
    /// of its own — it turns one into a `fail` check — but a workspace that never booted at all has no
    /// checks to hang the refusal on.
    #[must_use]
    pub const fn blocks_on_error(self) -> bool {
        matches!(self, Self::Report | Self::Doctor)
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

    let (workspace_argument, mode, rebuild) = resolve_request(cli);
    let workspace = match Boot::resolve_workspace(workspace_argument.as_deref()) {
        Ok(workspace) => workspace,
        Err(reason) => return refuse(mode, reason.into()),
    };
    // `doctor` is the command you run when the workspace is wrong, so a boot refusal cannot be allowed
    // to end it: the refusal becomes its report, and the exit code is still 2 ([`crate::exit`]).
    let boot = match Boot::validate(&workspace, &cli.common, mode.access()) {
        Ok(boot) => boot,
        Err(reason) if mode == Mode::Doctor => {
            let report = crate::doctor::unusable(&workspace, &reason.to_string());
            print!("{}", report.render(doctor_json(cli)));
            return Ok(report.exit());
        }
        Err(reason) => return refuse(mode, reason.into()),
    };

    // A `fathom.toml` this build does not understand is a refusal for every command except `doctor`,
    // which reports it as a failed check and carries on with the defaults — saying which in the check.
    let (mut config, config_error) = match Config::discover(
        cli.common.config.as_deref(),
        &boot.workspace,
        boot.git_root.as_deref(),
    ) {
        Ok(config) => (config, None),
        Err(reason) if mode == Mode::Doctor => (Config::default(), Some(reason.to_string())),
        Err(reason) => return refuse(mode, reason.into()),
    };
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

    match &cli.command {
        Some(Command::Report(args)) => crate::report::run(&boot, &config, &cli.common, args)
            .or_else(|reason| refuse(Mode::Report, reason)),
        Some(Command::Doctor(args)) => {
            let input = crate::doctor::Input {
                boot: &boot,
                config: &config,
                config_error,
            };
            let report = crate::doctor::run(&input);
            print!("{}", report.render(args.json));
            Ok(report.exit())
        }
        _ => match mode {
            Mode::IndexStatus => {
                print_status(&boot, &config, &cli.common);
                Ok(Exit::Success)
            }
            Mode::Index => index(&boot, &config, &cli.common, rebuild).map(|()| Exit::Success),
            Mode::Tui => crate::tui::run(&boot, &config, &cli.common),
            Mode::Report | Mode::Doctor => unreachable!("handled above: both have their own argv"),
        },
    }
}

/// Print a refusal on stderr and return the exit code this mode gives one.
///
/// Every command but the two headless ones keeps the binary's convention (a refusal is 1, and `main`
/// prints it); `report` and `doctor` say the same thing and exit 2, because that is what "could not
/// run" means to a CI step in this pipeline ([`crate::exit`]).
fn refuse(mode: Mode, error: FathomError) -> Result<Exit, FathomError> {
    if mode.blocks_on_error() {
        crate::exit::print_refusal(&error);
        return Ok(Exit::Blocked);
    }
    Err(error)
}

/// Whether `fathom doctor` was asked for JSON, without threading the subcommand's fields through
/// [`resolve_request`] for a flag only one mode reads.
fn doctor_json(cli: &Cli) -> bool {
    matches!(&cli.command, Some(Command::Doctor(args)) if args.json)
}

/// `fathom index [WORKSPACE] [--rebuild]`: build the store, and say on stderr what it cost.
///
/// Nothing here goes to stdout. `index` has no data output — its product is the store — and the rule
/// that a redirect of `fathom report` must stay parseable is kept by never writing data to anything
/// but the store (ADR-0005 §6; `tests/cli.rs` asserts the split).
fn index(
    boot: &Boot,
    config: &Config,
    common: &CommonArgs,
    rebuild: bool,
) -> Result<(), FathomError> {
    let query = window(config, common);
    let trigger = if rebuild { "rebuild" } else { "manual" };
    let mut indexer = store::Indexer::new(boot, config.clone(), query, trigger)?;
    let run = if rebuild {
        indexer.rebuild()?
    } else {
        indexer.run()?
    };
    info!(
        "{} {}: {} commits seen, {} rows written ({} new), cache {}/{} hits, {} blobs read, {} \
         analyses, {} bytes published in {} ms",
        run.kind,
        trigger,
        run.commits_seen,
        run.rows_written,
        run.commits_added,
        run.cache_hits,
        run.cache_lookups,
        run.blobs_read,
        run.analyses,
        run.bytes_written,
        run.duration_ms
    );
    if run.rows_pruned > 0 || run.blocks_pruned > 0 {
        info!(
            "pruned {} rows and {} blocks outside the walk set",
            run.rows_pruned, run.blocks_pruned
        );
    }
    Ok(())
}

/// Which workspace and which mode the argv asked for, plus whether `index` was told to rebuild.
fn resolve_request(cli: &Cli) -> (Option<PathBuf>, Mode, bool) {
    let top = cli.workspace.clone();
    match &cli.command {
        None => (top, Mode::Tui, false),
        Some(Command::Tui(args)) => (pick(args.workspace.clone(), &top, "tui"), Mode::Tui, false),
        Some(Command::Index(args)) => (
            pick(args.workspace.clone(), &top, "index"),
            if args.status {
                Mode::IndexStatus
            } else {
                Mode::Index
            },
            args.rebuild,
        ),
        Some(Command::Report(args)) => (
            pick(args.workspace.clone(), &top, "report"),
            Mode::Report,
            false,
        ),
        Some(Command::Doctor(args)) => (
            pick(args.workspace.clone(), &top, "doctor"),
            Mode::Doctor,
            false,
        ),
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
    lines.extend(store_status(boot));

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

/// The diff view's byte caps, from the two flags (mact-by1.11).
///
/// A view limit rather than a `fathom.toml` key, for the reason every view limit in this crate has:
/// ADR-0006 §7.2 declares the config keys a reader may read, and a value that changes what a screen shows
/// without changing any digest belongs on the command line, where the person looking at the screen can see
/// that they set it.
#[must_use]
pub fn diff_limits(common: &CommonArgs) -> crate::diff::Limits {
    crate::diff::Limits::from_args(
        common.max_diff_bytes.map(std::num::NonZeroUsize::get),
        common
            .max_diff_bytes_per_file
            .map(std::num::NonZeroUsize::get),
    )
}

/// The window `fathom.toml` describes with `--from`/`--to` applied on top (§11.1's flags).
///
/// `--from` and `--to` are the walk's window, not a filter applied afterwards: a `--from` older than
/// `history.depth` widens the walk rather than being clipped to it, because a user naming a commit
/// means that commit, and an index that silently started somewhere else would publish rows for a
/// window nobody asked for.
pub fn window(config: &Config, common: &CommonArgs) -> Query {
    let mut query = Query::from_config(config);
    if let Some(from) = &common.from {
        query.from = Some(from.clone());
        // An explicit floor is also the depth cap's replacement: naming both ends of a range and then
        // being cut to 500 commits by a config default would be a surprise. The number is git's own
        // `int` ceiling rather than `usize::MAX`, because it is handed to `--max-count=` verbatim.
        query.depth = usize::try_from(i32::MAX).unwrap_or(usize::MAX);
    }
    if let Some(to) = &common.to {
        query.to = to.clone();
    }
    if common.first_parent {
        // Applied here as well as to the config in [`Cli::run`], because a caller that builds a window
        // directly (the report, a test) must not be able to ask for first parents and get a full walk:
        // which parent a delta was measured against decides what the word "drop" means (§7.4).
        query.first_parent = true;
    }
    query
}

/// What the store, if there is one, says about itself and about its last run (§6.2's "`last` plus the
/// row and block counts").
///
/// A refusal to open the store is printed as a line rather than raised: `--status` is a reader, and its
/// whole job is to tell you what is there — including "there is a store there and it is not mine".
fn store_status(boot: &Boot) -> Vec<(String, String)> {
    match Store::open(&boot.cache.path) {
        Ok(Some(store)) => {
            let book = Runbook::read(&store.index);
            let run = book.map_or_else(store::Run::default, |book| book.last);
            store.status_lines(&run)
        }
        Ok(None) => vec![("store".to_owned(), "absent".to_owned())],
        Err(reason) => vec![
            ("store".to_owned(), "refused".to_owned()),
            (
                "store.refusal".to_owned(),
                reason.to_string().replace('\n', " "),
            ),
        ],
    }
}
