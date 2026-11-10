//! `act-mcp` CLI: `act-mcp serve --workspace <path>`, `act-mcp validate-evidence [paths…]`.
//!
//! Deliberately thin: parse args → run → report. Errors are printed to stderr with a
//! `fix:` hint and exit non-zero; stdout is never written to by *logs*. The one
//! exception is `validate-evidence`, which is a checker: its findings go to stderr like
//! a compiler's, and its one-line summary and `--stats` table go to stdout so a CI step
//! can `validate-evidence | tee` them.

use std::{
    io::IsTerminal as _,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
};

use anyhow::Context as _;
use clap::{Args, Parser, Subcommand};
use tokio_util::sync::CancellationToken;

use act_mcp::{
    config::ServeArgs,
    evidence::{self, EvidenceRoot},
    registry::RunSupervisor,
    server, Config,
};

#[derive(Parser)]
#[command(
    name = "act-mcp",
    version = env!("CARGO_PKG_VERSION"),
    about = "MCP server (protocol 2026-07-28) that runs GitHub Actions workflows with act",
    long_about = "Runs nektos/act workflows for an MCP client over streamable HTTP on loopback, \
                  pinning MCP protocol revision 2026-07-28 and writing .evidence JSONL.\n\n\
                  There is no authentication: the server binds loopback only, and binding anywhere \
                  else needs --i-understand-this-is-unauthenticated.",
    propagate_version = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the MCP endpoint (streamable HTTP at /mcp).
    Serve(ServeArgs),

    /// Validate .evidence JSONL against the ADR-0002 contract (exit 1 on any bad line).
    ValidateEvidence(ValidateEvidenceArgs),
}

/// `act-mcp validate-evidence` arguments.
///
/// The exit codes are the Python checker's, so CI can point at either implementation:
/// `0` clean, `1` findings, `2` the command could not run (nothing matched the paths,
/// or a schema was unreadable).
#[derive(Debug, Clone, Args)]
struct ValidateEvidenceArgs {
    /// Files or directories to check. Defaults to the evidence root.
    #[arg(value_name = "PATH")]
    paths: Vec<PathBuf>,

    /// Evidence root to fall back to when no PATH is given; ADR-0002 §1.1's `--evidence-dir`.
    #[arg(
        long,
        value_name = "PATH",
        env = "EVIDENCE_DIR",
        help = "Evidence root used when no PATH is given; defaults to ./.evidence."
    )]
    evidence_dir: Option<PathBuf>,

    /// Record schema to validate against instead of the one built into the binary.
    #[arg(
        long,
        value_name = "PATH",
        help = "Validate records against this schema file rather than the embedded schema/evidence.schema.json."
    )]
    schema: Option<PathBuf>,

    /// `meta.json` schema to validate against instead of the embedded one.
    #[arg(
        long,
        value_name = "PATH",
        help = "Validate runs/<run_id>/meta.json against this schema file rather than the embedded schema/run-meta.schema.json."
    )]
    meta_schema: Option<PathBuf>,

    /// Print per-kind, per-commit record counts.
    #[arg(long, help = "Print counts per kind and commit after the findings.")]
    stats: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    // Tracing first, so a failure during boot still has a log line if RUST_LOG
    // is on. Everything goes to stderr: stdout must stay empty (see lib.rs).
    init_tracing();

    let Cli { command } = Cli::parse();
    let result = match command {
        Command::Serve(args) => run_serve(args).await.map(|()| ExitCode::SUCCESS),
        Command::ValidateEvidence(args) => run_validate_evidence(&args),
    };

    match result {
        // `validate-evidence` decides its own status (0 clean, 1 findings, 2 could not
        // run); `serve` only ever reaches here with SUCCESS.
        Ok(code) => code,
        Err(err) => {
            // Print the chain rather than the Debug blob: the top line is the
            // failure, each cause below it is context, and `ConfigError` already
            // carries its own `fix:` line.
            eprintln!("act-mcp: {err}");
            for cause in err.chain().skip(1) {
                eprintln!("  caused by: {cause}");
            }
            ExitCode::FAILURE
        }
    }
}

async fn run_serve(args: ServeArgs) -> anyhow::Result<()> {
    let config = Arc::new(Config::build(&args)?);
    tracing::debug!(?config, "configuration accepted");

    // One supervisor per process: it owns every run, so a run outlives the request that
    // started it and is visible to every session (ADR-0004 §2, D-1). Built from the boot
    // config so `--max-run-dirs` (mact-f1w.13) reaches the cap that actually enforces it.
    let supervisor = std::sync::Arc::new(
        RunSupervisor::with_policy(config.run_policy())
            .context("the built-in run-lifecycle defaults are unusable")?,
    );

    let shutdown = CancellationToken::new();
    install_signal_handlers(&shutdown);

    server::serve(config, supervisor, shutdown)
        .await
        .context("server terminated with an error")
}

/// `act-mcp validate-evidence`: walk the tree, check every line, report, and decide the
/// exit code here so the process status is the contract (ADR-0002 §7 makes this command
/// the strict reader, and a guard nobody can branch on is not a guard).
fn run_validate_evidence(args: &ValidateEvidenceArgs) -> anyhow::Result<ExitCode> {
    let paths = if args.paths.is_empty() {
        // No PATH: the evidence root, which is `--evidence-dir` (clap already folded
        // `$EVIDENCE_DIR` into it) and otherwise `.evidence` in the current directory.
        let root = EvidenceRoot::resolve(Path::new("."), args.evidence_dir.as_deref())?;
        vec![root.path().to_path_buf()]
    } else {
        args.paths.clone()
    };

    let options = evidence::ValidateOptions {
        paths,
        schema: args.schema.as_deref(),
        meta_schema: args.meta_schema.as_deref(),
    };
    let report = match evidence::validate_tree(&options) {
        Ok(report) => report,
        Err(evidence::ValidateError::NothingToValidate { paths }) => {
            // Exit 2, the Python checker's "the command was asked about nothing".
            eprintln!(
                "act-mcp: nothing to validate under {}\n  fix: pass a path, or run the server once to create the evidence tree",
                paths
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            return Ok(ExitCode::from(2));
        }
        Err(other) => {
            // Everything else that stopped the command from running is exit 2 too: a
            // checker that exits 1 says "the data is bad", which would be a lie here.
            eprintln!("act-mcp: validate-evidence could not run: {other}");
            return Ok(ExitCode::from(2));
        }
    };

    // Findings on stderr (the compiler convention); the summary and the stats table on
    // stdout, so `validate-evidence --stats > counts.tsv` does the obvious thing.
    for problem in &report.problems {
        eprintln!("{problem}");
    }
    if args.stats {
        print!("{}", report.stats.to_string_pretty());
    }
    println!("{}", report.summary());
    Ok(ExitCode::from(report.exit_code()))
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};

    let filter = EnvFilter::try_from_default_env()
        // Default to info, but keep rmcp at warn: under 2026-07-28 every request
        // builds a per-request service, and rmcp logs one INFO line per
        // handshake — that is `run_status` polling spam by default. Set
        // RUST_LOG=rmcp=info (or =debug) when you want the transport chatter.
        .unwrap_or_else(|_| EnvFilter::new("info,rmcp=warn"));

    fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .with_target(true)
        .init();
}

/// Cancel `token` on ctrl-c (SIGINT) or SIGTERM.
///
/// This is the only thing that stops the process: [`server::serve`] owns the
/// drain window and the session teardown that follows it, so a signal means
/// "stop accepting, then shut everything down" rather than an instant kill. A
/// handler that fails to install is logged and left alone — a missing SIGTERM
/// handler must not read as a shutdown request.
fn install_signal_handlers(token: &CancellationToken) {
    let interrupt = token.clone();
    tokio::spawn(async move {
        match tokio::signal::ctrl_c().await {
            Ok(()) => {
                tracing::info!("received SIGINT (ctrl-c); shutting down");
                interrupt.cancel();
            }
            Err(err) => tracing::warn!(
                %err,
                "could not install the SIGINT handler; ctrl-c will not stop this server"
            ),
        }
    });

    let terminate = token.clone();
    tokio::spawn(async move {
        if await_sigterm().await {
            tracing::info!("received SIGTERM; shutting down");
            terminate.cancel();
        }
    });
}

/// Wait for SIGTERM. Always pending on non-unix platforms, where there is no
/// SIGTERM; ctrl-c still works there.
#[cfg(unix)]
async fn await_sigterm() -> bool {
    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(mut stream) => stream.recv().await.is_some(),
        Err(err) => {
            tracing::warn!(%err, "could not install the SIGTERM handler");
            false
        }
    }
}

#[cfg(not(unix))]
async fn await_sigterm() -> bool {
    std::future::pending::<()>().await;
    false
}
