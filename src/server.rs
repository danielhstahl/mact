//! The MCP service and the axum mount that serves it.
//!
//! The service holds one [`RunSupervisor`] per process and no per-session state: `list_tasks`
//! (mact-f1w.7) reads files, and `run_status`/`cancel_run` (mact-f1w.11) go to the supervisor,
//! which owns every run. That sharing is the whole point — a run started by one request is
//! pollable and cancellable from any other, and a session going away changes nothing about it.
//! What else has to be right here is the shell everything else hangs on: the pinned protocol
//! revision, the loopback-only bind, the shared immutable [`Config`], and a cancellation token
//! that actually drains.

use std::{borrow::Cow, net::SocketAddr, sync::Arc, time::Duration};

use rmcp::{
    handler::server::wrapper::{Json, Parameters},
    model::{Implementation, JsonObject, ProtocolVersion, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    },
    ServerHandler,
};
use tokio_util::sync::CancellationToken;
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;

use crate::config::Config;
use crate::registry::RunSupervisor;
use crate::tools::{cancel_run, list_tasks, run_status, ToolError};

/// Server name reported in `serverInfo` / `discover` responses.
pub const SERVER_NAME: &str = "act-mcp";

/// Path the MCP endpoint is mounted at.
pub const MCP_ROUTE: &str = "/mcp";

/// The one protocol revision this server speaks.
///
/// `2026-07-28` is the revision whose lifecycle is "no `initialize`, no
/// session": every request is self-contained (see rmcp's
/// `ProtocolVersion::NO_INITIALIZE`). That is what makes an act run a plain
/// request instead of a session that has to outlive a browser tab, and it is why
/// [`StreamableHttpServerConfig::legacy_session_mode`] stays off. Everything is
/// delegated to rmcp — nothing here hand-rolls negotiation.
pub const SUPPORTED_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::V_2026_07_28;

/// Per-session MCP service.
///
/// Cheap to clone and constructed per request by the service factory, so the
/// only thing it holds is a shared `Arc<Config>`. Per-session state is `Clone`
/// by construction; the immutable boot config is shared, not copied.
#[derive(Debug, Clone)]
pub struct ActMcp {
    config: Arc<Config>,
    supervisor: Arc<RunSupervisor>,
}

impl ActMcp {
    /// Wrap the shared boot config for one session.
    ///
    /// Called by the service factory per request, so this stays cheap: it clones
    /// an `Arc`, nothing else.
    pub fn new(config: Arc<Config>, supervisor: Arc<RunSupervisor>) -> Self {
        Self { config, supervisor }
    }

    /// The shared boot config. Tool handlers (mact-f1w.7+) read workspace,
    /// evidence-dir and act-binary paths through this.
    pub fn config(&self) -> &Arc<Config> {
        &self.config
    }

    /// The process-wide run supervisor: every session shares it, which is what lets one
    /// request poll or cancel a run another request started.
    pub fn supervisor(&self) -> &Arc<RunSupervisor> {
        &self.supervisor
    }
}

/// The tools this server advertises.
///
/// `list_tasks` (mact-f1w.7), `run_status` and `cancel_run` (mact-f1w.11) are here;
/// `run_workflow` joins with mact-f1w.14. ADR-0001 §1 pins the names, titles and
/// annotations each of them must carry.
#[tool_router]
impl ActMcp {
    /// Enumerate this workspace's workflows and job ids (ADR-0001 §6.1).
    ///
    /// The body is deliberately thin — parse, read on the blocking pool, optionally
    /// cross-check against act, wrap in `Json` — because every decision that shapes
    /// the answer lives in [`list_tasks`], so the contract has one home and its unit
    /// tests do not need a server.
    #[tool(
        name = "list_tasks",
        title = "List workflows and job ids in this workspace",
        description = list_tasks::DESCRIPTION,
        input_schema = list_tasks::input_schema(),
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        ),
    )]
    async fn list_tasks(
        &self,
        Parameters(arguments): Parameters<JsonObject>,
    ) -> Result<Json<list_tasks::ListTasksResult>, ToolError> {
        let args = list_tasks::ListTasksArgs::parse(Some(&arguments))?;
        let config = Arc::clone(self.config());
        // Reading a workflows directory is a handful of small file reads — except on
        // a cold network mount, where `readdir` alone is seconds. Either way it does
        // not belong on an I/O worker, so it goes to the blocking pool exactly like
        // validation does (`Validator::validate_async`). Taking no workspace lock is
        // what makes waiting here safe while a benchmark holds the workspace.
        let mut result =
            tokio::task::spawn_blocking(move || list_tasks::list_tasks(&config, &args))
                .await
                .map_err(|join_error| {
                    ToolError::new(
                        list_tasks::TOOL,
                        "internal_error",
                        format!("the listing did not complete: {join_error}"),
                        true,
                        serde_json::Map::new(),
                    )
                })??;
        if self.config().list_tasks_act_cross_check {
            list_tasks::cross_check_with_act(self.config(), &mut result).await;
        }
        Ok(Json(result))
    }

    /// Poll one run, or list this server's recent runs (ADR-0001 §6.2, ADR-0004 §10).
    ///
    /// Purely a read of supervisor state: no lock, no disk, no act, no wait. That is the
    /// contract (`run_status` is the progress channel for a server with no streams), so the
    /// handler's only jobs are argument validation and error mapping.
    #[tool(
        name = "run_status",
        title = "Poll a run, or list this server's recent runs",
        description = run_status::DESCRIPTION,
        input_schema = run_status::input_schema(),
        output_schema = run_status::output_schema(),
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        ),
    )]
    async fn run_status(
        &self,
        Parameters(arguments): Parameters<JsonObject>,
    ) -> Result<Json<serde_json::Value>, ToolError> {
        let args = run_status::RunStatusArgs::parse(Some(&arguments))?;
        let result = run_status::run(self.supervisor(), &args)
            .await
            .map_err(|not_found| run_status::run_not_found(&not_found))?;
        Ok(Json(result))
    }

    /// Cancel a run (ADR-0001 §6.3, ADR-0004 §5.2).
    ///
    /// The only control tool that can block, and its block is bounded by
    /// `--cancel-wait-secs`. Annotated `idempotent_hint = true` because §5.2 makes a repeat
    /// cancel a report of what was already true rather than a second teardown.
    #[tool(
        name = "cancel_run",
        title = "Cancel a run by id (idempotent)",
        description = cancel_run::DESCRIPTION,
        input_schema = cancel_run::input_schema(),
        output_schema = cancel_run::output_schema(),
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        ),
    )]
    async fn cancel_run(
        &self,
        Parameters(arguments): Parameters<JsonObject>,
    ) -> Result<Json<serde_json::Value>, ToolError> {
        let args = cancel_run::CancelRunArgs::parse(Some(&arguments))?;
        let receipt = cancel_run::run(self.supervisor(), &args)
            .await
            .map_err(|not_found| cancel_run::run_not_found(&not_found))?;
        Ok(Json(receipt.to_value()))
    }
}

#[tool_handler]
impl ServerHandler for ActMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(SUPPORTED_PROTOCOL_VERSION)
            .with_server_info(Implementation::new(SERVER_NAME, env!("CARGO_PKG_VERSION")))
            .with_instructions(format!(
                "act-mcp {} — runs GitHub Actions workflows via act and writes .evidence JSONL.\n\
                 Workspace: {} (evidence: {})\n\
                 Start with list_tasks: it enumerates workflow files, their job ids, and an advisory \n\
                 benchmark/coverage hint, and is safe to call at any time (read-only, no run lock).",
                env!("CARGO_PKG_VERSION"),
                self.config.workspace.display(),
                self.config.evidence_dir.display(),
            ))
    }

    /// Expose exactly one revision.
    ///
    /// rmcp uses this list for three things at once: what `server/discover`
    /// advertises, what `initialize` may agree to, and what a per-request
    /// `_meta` protocol version is validated against — so narrowing it is the
    /// whole of "only negotiates 2026-07-28". A client that asks for an older
    /// revision is refused rather than downgraded.
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(std::slice::from_ref(&SUPPORTED_PROTOCOL_VERSION))
    }
}

/// Build the streamable-HTTP service for one server instance.
///
/// `cancellation_token` ends live sessions and the streams they hold; [`serve`]
/// hands it the session token it cancels after the drain window. `supervisor` is shared by
/// every session in the process — the runs belong to it, not to any session.
pub fn mcp_service(
    config: Arc<Config>,
    supervisor: Arc<RunSupervisor>,
    cancellation_token: CancellationToken,
) -> StreamableHttpService<ActMcp, LocalSessionManager> {
    StreamableHttpService::new(
        move || Ok(ActMcp::new(Arc::clone(&config), Arc::clone(&supervisor))),
        Arc::new(LocalSessionManager::default()),
        http_config(cancellation_token),
    )
}

/// The axum [`Router`] holding the endpoint.
///
/// Kept separate from [`serve`] so tests can mount the identical stack on an
/// ephemeral port without going through the CLI.
///
/// The only layer so far is request tracing (`RUST_LOG=debug` turns it on).
/// mact-f1w.13 adds the Host/Origin guards here.
pub fn router(
    config: Arc<Config>,
    supervisor: Arc<RunSupervisor>,
    cancellation_token: CancellationToken,
) -> axum::Router {
    axum::Router::new()
        .nest_service(
            MCP_ROUTE,
            mcp_service(config, supervisor, cancellation_token),
        )
        .layer(ServiceBuilder::new().layer(TraceLayer::new_for_http()))
}

fn http_config(cancellation_token: CancellationToken) -> StreamableHttpServerConfig {
    // Defaults are right for us (allowed_hosts stays loopback-only:
    // localhost/127.0.0.1/::1), with two deliberate changes:
    //
    // * legacy_session_mode = false. The pinned revision has no sessions, and a
    //   server that could also speak the old session handshake is a second
    //   behaviour to test for no benefit. (--allow-legacy-sessions was dropped:
    //   with one supported version there is nothing for it to unlock.)
    // * json_response = true. Stateless request/response tools answer with
    //   application/json instead of an SSE stream, which keeps a plain client
    //   working; rmcp still falls back to SSE if a handler ever streams
    //   notifications mid-call, which is what run_workflow will want later.
    StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_cancellation_token(cancellation_token)
}

/// Bind, log the address, and serve until `shutdown` is cancelled.
///
/// Shutdown is two-phase, because the ticket's budget is "a couple of seconds"
/// and a half-open SSE stream must not be able to hold the process open:
///
/// 1. `shutdown` cancels (ctrl-c / SIGTERM, wired in the binary) → axum stops
///    accepting connections and in-flight handlers get [`SHUTDOWN_DRAIN_BUDGET`]
///    to finish on their own.
/// 2. After that budget the session token is cancelled, which tears down live
///    MCP sessions, their SSE streams, and (from mact-f1w.8/.11 onward) the act
///    children hanging off them.
///
/// The session token is deliberately *not* a child of `shutdown`: if it were, the
/// signal would kill live sessions at the same instant the listener stopped and
/// phase 1 would be a lie.
pub async fn serve(
    config: Arc<Config>,
    supervisor: Arc<RunSupervisor>,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    let local = listener.local_addr()?;
    let url = endpoint_url(local);
    tracing::info!(
        %url,
        bind = %local,
        workspace = %config.workspace.display(),
        evidence_dir = %config.evidence_dir.display(),
        act_binary = %config.act_binary.display(),
        protocol_version = %SUPPORTED_PROTOCOL_VERSION,
        "act-mcp listening"
    );

    let sessions = CancellationToken::new();
    let app = router(
        Arc::clone(&config),
        Arc::clone(&supervisor),
        sessions.clone(),
    );
    let drain_sessions = sessions.clone();
    let sweep_supervisor = Arc::clone(&supervisor);

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown.cancelled().await;
            // ADR-0004 §9.2 steps 1–2, in this order: refuse new runs and ask every active
            // one to come down *concurrently* (separate process groups, so wall time is one
            // grace period rather than N) before the HTTP side starts counting its own
            // budget. A `wait: true` request is released by its run's terminal commit, which
            // is why the sweep runs first and the answers arrive normally.
            sweep_supervisor.begin_shutdown();
            tracing::info!(
                drain_budget = ?SHUTDOWN_DRAIN_BUDGET,
                "shutdown signal received; draining in-flight requests"
            );
            tokio::time::sleep(SHUTDOWN_DRAIN_BUDGET).await;
            // Step 3: the bounded window for the terminal commits (evidence publish and
            // `meta.json` included), so a run that can finish cleanly gets to.
            sweep_supervisor.drain().await;
            tracing::debug!("end of drain window; ending live sessions");
            drain_sessions.cancel();
        })
        .await?;

    // `serve` can also return on an internal error with sessions still alive, and with runs
    // that were never asked to stop. Both mean the same thing now: sweep, then leave.
    supervisor.shutdown().await;
    sessions.cancel();
    tracing::info!("act-mcp stopped");
    Ok(())
}

/// How long in-flight requests get to finish after a shutdown signal before
/// their sessions are cancelled.
const SHUTDOWN_DRAIN_BUDGET: Duration = Duration::from_secs(1);

/// The URL a client should dial. Loopback, so plain HTTP is fine (and TLS here
/// would only hide that there is no authentication).
pub fn endpoint_url(bind: SocketAddr) -> String {
    let host = match bind.ip() {
        std::net::IpAddr::V4(v4) => v4.to_string(),
        // Bracket v6 or the URL is ambiguous about where the host ends.
        std::net::IpAddr::V6(v6) => format!("[{v6}]"),
    };
    format!("http://{host}:{}{MCP_ROUTE}", bind.port())
}
