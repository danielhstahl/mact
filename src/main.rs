//! `act-mcp` CLI: `act-mcp serve --workspace <path>`.
//!
//! Deliberately thin: parse args → validate them ([`Config::build`]) → serve.
//! Errors are printed to stderr with a `fix:` hint and exit non-zero; stdout is
//! never written to.

use std::{io::IsTerminal as _, process::ExitCode, sync::Arc};

use anyhow::Context as _;
use clap::{Parser, Subcommand};
use tokio_util::sync::CancellationToken;

use act_mcp::{config::ServeArgs, server, Config};

#[derive(Parser)]
#[command(
    name = "act-mcp",
    version = env!("CARGO_PKG_VERSION"),
    about = "MCP server (protocol 2026-07-28) that runs GitHub Actions workflows with act",
    long_about = "Runs nektos/act workflows for an MCP client over streamable HTTP on loopback, \
                  pinning MCP protocol revision 2026-07-28 and writing .evidence JSONL.\n\n\
                  There is no authentication: the server refuses to bind anywhere but loopback.",
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
}

#[tokio::main]
async fn main() -> ExitCode {
    // Tracing first, so a failure during boot still has a log line if RUST_LOG
    // is on. Everything goes to stderr: stdout must stay empty (see lib.rs).
    init_tracing();

    let Cli { command } = Cli::parse();
    let result = match command {
        Command::Serve(args) => run_serve(args).await,
    };

    if let Err(err) = result {
        // Print the chain rather than the Debug blob: the top line is the
        // failure, each cause below it is context, and `ConfigError` already
        // carries its own `fix:` line.
        eprintln!("act-mcp: {err}");
        for cause in err.chain().skip(1) {
            eprintln!("  caused by: {cause}");
        }
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

async fn run_serve(args: ServeArgs) -> anyhow::Result<()> {
    let config = Arc::new(Config::build(&args)?);
    tracing::debug!(?config, "configuration accepted");

    let shutdown = CancellationToken::new();
    install_signal_handlers(&shutdown);

    server::serve(config, shutdown)
        .await
        .context("server terminated with an error")
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
