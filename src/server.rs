//! The MCP service and the axum mount that serves it.
//!
//! The service holds one [`Wiring`] per process and no per-session state: `list_tasks`
//! (mact-f1w.7) reads files, `run_status`/`cancel_run` (mact-f1w.11) go to the supervisor, and
//! `run_workflow` (mact-f1w.14) drives the whole pipeline through it — validate, isolate, run,
//! extract, publish, report. That sharing is the whole point — a run started by one request is
//! pollable and cancellable from any other, and a session going away changes nothing about it.
//! What else has to be right here is the shell everything else hangs on: the pinned protocol
//! revision, the loopback-only bind, the shared immutable [`Config`], and a cancellation token
//! that actually drains.

use std::{borrow::Cow, net::SocketAddr, path::Path, sync::Arc, time::Duration};

use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse as _, Response},
};
use rmcp::{
    handler::server::wrapper::{Json, Parameters},
    model::{
        CancelledNotificationParam, Implementation, JsonObject, ProtocolVersion,
        ServerCapabilities, ServerConfig,
    },
    service::{NotificationContext, RequestContext, RoleServer},
    tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    },
    ServerHandler,
};
use tokio_util::sync::CancellationToken;
use tower_http::trace::TraceLayer;

use crate::config::{Config, SESSION_TOKEN_HEADER};
use crate::registry::RunSupervisor;
use crate::tools::{
    cancel_run, list_tasks, run_status, run_workflow, run_workflow::Wiring, ToolError,
};

/// Server name reported in `serverInfo` / `discover` responses.
pub const SERVER_NAME: &str = "act-mcp";

/// The path the MCP endpoint is mounted at.
pub const MCP_ROUTE: &str = "/mcp";

/// The SSE keep-alive interval for the streams this server does open.
///
/// Not left at rmcp's default by accident. With `json_response = true` a normal tool call
/// is a plain JSON response and there is no stream to keep alive; a stream appears only
/// when a handler notifies mid-call. For a single local client the interval is not about
/// beating an idle proxy timeout (there is no proxy on loopback) — it is how a half-closed
/// client is noticed at all: the keep-alive write fails, the stream ends, and the session
/// and its buffers go instead of lingering until the run's own timeout. 15 s is rmcp's own
/// number, written down here rather than inherited.
pub const SSE_KEEP_ALIVE: Duration = Duration::from_secs(15);

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
/// Cheap to clone and constructed per request by the service factory, so the only thing it holds
/// is one `Arc` to the process-wide [`Wiring`] (config, supervisor, validator, idempotency
/// table). Per-session state is `Clone` by construction; the immutable boot config and every
/// piece of shared mutable state are `Arc`s inside that one struct, which is what makes a run
/// started by this request visible to every later one.
#[derive(Debug, Clone)]
pub struct ActMcp {
    wiring: Arc<Wiring>,
}

impl ActMcp {
    /// Wrap the shared runtime for one session.
    ///
    /// Called by the service factory per request, so this stays cheap: it clones one `Arc`.
    pub fn new(wiring: Arc<Wiring>) -> Self {
        Self { wiring }
    }

    /// The shared runtime: config, supervisor, validator and the idempotency table.
    pub fn wiring(&self) -> &Arc<Wiring> {
        &self.wiring
    }

    /// The shared boot config. Tool handlers read workspace, evidence-dir and act-binary paths
    /// through this.
    pub fn config(&self) -> &Arc<Config> {
        &self.wiring.config
    }

    /// The process-wide run supervisor: every session shares it, which is what lets one request
    /// poll or cancel a run another request started.
    pub fn supervisor(&self) -> &Arc<RunSupervisor> {
        &self.wiring.supervisor
    }
}

/// The tools this server advertises.
///
/// All four of ADR-0001's: `list_tasks` (mact-f1w.7), `run_status` and `cancel_run`
/// (mact-f1w.11), and `run_workflow` (mact-f1w.14). ADR-0001 §1 pins the names, titles and
/// annotations each of them must carry, and `tests/tool_schema_parity.rs` is what keeps the
/// advertised `inputSchema`/`outputSchema` identical to the checked-in contract.
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

    /// Run one workflow job against one commit (ADR-0001 §2, §3, §4).
    ///
    /// The headline tool, and the reason the other three exist: validate → isolate → run →
    /// extract → publish → structured result. All of that lives in
    /// [`crate::tools::run_workflow::call`]; this handler is the transport seam — argument
    /// parsing, the notification sink the request's `_meta` asked for, and the cancellation token
    /// that ties a `$/cancel-request` to the run this request is waiting on.
    #[tool(
        name = "run_workflow",
        title = "Run one workflow job against one commit",
        description = run_workflow::DESCRIPTION,
        input_schema = run_workflow::input_schema(),
        output_schema = run_workflow::output_schema(),
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        ),
    )]
    async fn run_workflow(
        &self,
        Parameters(arguments): Parameters<JsonObject>,
        context: RequestContext<RoleServer>,
    ) -> Result<Json<serde_json::Value>, ToolError> {
        let args = run_workflow::RunWorkflowArgs::parse(Some(&arguments))?;
        let call = run_workflow::Call {
            // Progress only for a request that carried a progress token, logging only for one
            // that carried a log level: both are the client's choice, expressed per request in
            // the sessionless 2026-07-28 revision.
            progress: run_workflow::Progress::for_request(&context.peer, &context.meta),
            cancel: context.ct.clone(),
            request_key: request_key(&context.id),
        };
        let result = run_workflow::call(Arc::clone(self.wiring()), args, call).await?;
        Ok(Json(result))
    }
}

/// The `instructions` field an agent reads before it calls anything.
///
/// The per-tool descriptions each describe one tool; this is the only place the
/// *sequence* is stated, and it is written for the caller that has not read the source:
/// what to call first, what the paths in the answers mean, and — the question that decides
/// whether a run is worth starting at all — which categories write evidence and where it
/// lands. Kept as a pure function of three values so a test can pin it without booting a
/// server (the `server::tests` module, which is `#[cfg(test)]` and so not a linkable path).
#[must_use]
pub fn server_instructions(version: &str, workspace: &Path, evidence_dir: &Path) -> String {
    format!(
        "act-mcp {version} — runs one job of one GitHub Actions workflow with act, and writes what the run produced as `.evidence` JSONL. Workspace {workspace}; evidence root {evidence} (every path a tool returns is relative to it).\n\
         Use the tools in this order:\n\
         1. list_tasks — workflow names, their job ids (the `task` argument) and an advisory benchmark/coverage category hint. Read-only, takes no run lock, safe to call at any time.\n\
         2. run_workflow(workflow, task, category, commit) — starts a run and by default waits for the terminal result; `wait: false` returns a `run_id` immediately, which you poll with run_status and stop with cancel_run. One run at a time per workspace and there is no queue: a second call answers `workspace_busy` naming the run in flight. A workflow that ran and failed is a normal answer (`status: failed`), not an error.\n\
         3. Read the evidence. A terminal run's `evidence_files` lists what it wrote: `category: \"benchmark\"` → benchmark/<commit>/<run_id>.jsonl (one metric per line), `category: \"coverage\"` → coverage/<commit>/<run_id>.jsonl (one source file or the run total per line). Any other category runs normally and writes NO evidence file (`evidence_files: []`). Every run also writes runs/<run_id>/act.jsonl and, once terminal, runs/<run_id>/meta.json; those two are audit paths and are never in `evidence_files`.\n\
         Evidence is append-only and never aggregated — no mean, no percentile, no dedup across runs, those are the consumer's job — it stores raw counts so percentages are recomputed, and a published file is never rewritten.",
        workspace = workspace.display(),
        evidence = evidence_dir.display(),
    )
}

#[tool_handler]
impl ServerHandler for ActMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(SUPPORTED_PROTOCOL_VERSION)
            .with_server_info(Implementation::new(SERVER_NAME, env!("CARGO_PKG_VERSION")))
            .with_instructions(server_instructions(
                env!("CARGO_PKG_VERSION"),
                &self.config().workspace,
                &self.config().evidence_dir,
            ))
    }

    /// Honour `$/cancel-request` for a request that is waiting inside `run_workflow`.
    ///
    /// ADR-0004 §5.3's mechanism: the notification names a *request*, the waiter map says which
    /// run that request was waiting on, and the supervisor's cancel path does the rest — for a
    /// caller-initiated cancel the run is torn down (ADR-0004 §5.1) and ends `cancelled` with
    /// extraction skipped (§6.2). A notification naming an unknown, already-finished or
    /// `wait: false` request is ignored, because a notification cannot be answered and ignoring
    /// it is the whole error handling.
    ///
    /// The per-request cancellation token (`context.ct`, which rmcp cancels for the matching
    /// request id) is the primary path and is unambiguous; this map is the ADR's documented
    /// fallback. Its key is the JSON-RPC request id, which in the sessionless 2026-07-28 revision
    /// is per-connection and so can repeat across connections — the worst case is a cancel
    /// landing on another waiting request's run, which is inside this server's existing model
    /// (any local process may cancel any run by id; there is no authentication).
    async fn on_cancelled(
        &self,
        notification: CancelledNotificationParam,
        _context: NotificationContext<RoleServer>,
    ) {
        let Some(request_id) = notification.request_id.as_ref() else {
            return;
        };
        let key = request_key(request_id);
        match self.supervisor().cancel_waiting_request(&key).await {
            Some(receipt) => tracing::info!(
                run_id = %receipt.run_id,
                request = %request_id,
                status = receipt.status.as_str(),
                "a cancellation notification cancelled the run its request was waiting on"
            ),
            None => tracing::debug!(
                request = %request_id,
                "cancellation notification for a request that is not waiting on a run; ignored"
            ),
        }
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
/// `cancellation_token` ends live sessions and the streams they hold; [`serve`] hands it the
/// session token it cancels after the drain window. `wiring` is shared by every session in the
/// process — the runs, the validation gate and the idempotency table belong to it, not to any
/// session, which is why it is passed in rather than assembled per request.
pub fn mcp_service(
    wiring: Arc<Wiring>,
    cancellation_token: CancellationToken,
) -> StreamableHttpService<ActMcp, LocalSessionManager> {
    let http = http_config(Arc::clone(&wiring.config), cancellation_token);
    StreamableHttpService::new(
        move || Ok(ActMcp::new(Arc::clone(&wiring))),
        Arc::new(LocalSessionManager::default()),
        http,
    )
}

/// The waiter-map key for one JSON-RPC request (ADR-0004 §5.3).
///
/// Both halves — registering a waiter inside `run_workflow` and resolving a `$/cancel-request` in
/// [`ActMcp::on_cancelled`] — must build it the same way from the request id, so it is spelled
/// once here. See `on_cancelled` for why the id alone is a good-enough key in a server with no
/// sessions and no authentication.
fn request_key(id: &rmcp::model::RequestId) -> String {
    format!("{id}")
}

/// The axum [`axum::Router`] holding the endpoint.
///
/// Kept separate from [`serve`] so tests can mount the identical stack on an
/// ephemeral port without going through the CLI.
///
/// Two layers, in this order: request tracing (`RUST_LOG=debug`), and — only when the
/// operator turned it on — the shared-secret check in `require_session_token`. Everything
/// else that refuses a request happens inside rmcp's service, because that is where its
/// `Host`/`Origin` allowlists and its body-size limit live; see `http_config`.
pub fn router(wiring: Arc<Wiring>, cancellation_token: CancellationToken) -> axum::Router {
    let router = axum::Router::new().nest_service(
        MCP_ROUTE,
        mcp_service(Arc::clone(&wiring), cancellation_token),
    );
    // The shared-secret gate goes on first (it is the outermost layer because it is the
    // cheapest answer: no session, no parse, no handler for a request that never proves
    // itself). Tracing wraps everything, so a refused request still gets its trace line.
    let router = match wiring.config.session_token.as_ref() {
        Some(token) => router.layer(middleware::from_fn_with_state(
            token.expose().to_owned(),
            require_session_token,
        )),
        None => router,
    };
    router.layer(TraceLayer::new_for_http())
}

/// Refuse any request that does not carry the shared secret in [`SESSION_TOKEN_HEADER`].
///
/// Installed only when `--require-session-token`/`--session-token` is set (see
/// [`crate::config::ServeArgs::require_session_token`] for the decision and its limits).
/// The comparison is `secrets_equal`, and the presented value is never logged: a middleware
/// that prints the secret it is checking would put it in the log file it is meant to keep
/// secrets out of.
// `Result<Response, Response>` is the shape axum's middleware contract asks for; boxing the
// error would be a wrapper nobody unwraps.
#[allow(
    clippy::result_large_err,
    reason = "axum middleware returns a Response either way"
)]
async fn require_session_token(
    State(expected): State<String>,
    request: Request,
    next: Next,
) -> Result<Response, Response> {
    let presented = request
        .headers()
        .get(SESSION_TOKEN_HEADER)
        .and_then(|value| value.to_str().ok());
    match presented {
        Some(given) if secrets_equal(given, &expected) => Ok(next.run(request).await),
        other => {
            // `presented` (not its value) is the whole diagnostic: "no header" and "wrong
            // header" are the same answer to the caller and must stay the same answer here
            // too, or the log becomes a probe oracle.
            tracing::warn!(
                path = %request.uri(),
                header_present = other.is_some(),
                "refused a request that did not carry the required shared secret"
            );
            Err((
                StatusCode::UNAUTHORIZED,
                format!("act-mcp requires the `{SESSION_TOKEN_HEADER}` header on every request\n"),
            )
                .into_response())
        }
    }
}

/// Length-checked, then XOR-folded: no early return on the first differing byte.
///
/// Leaking the length of the secret is accepted — it is fixed at boot and hex, and the
/// attack this guards against is timing a local loopback socket byte by byte.
fn secrets_equal(presented: &str, expected: &str) -> bool {
    let (presented, expected) = (presented.as_bytes(), expected.as_bytes());
    if presented.len() != expected.len() {
        return false;
    }
    let mut difference = 0u8;
    for (given, want) in presented.iter().zip(expected) {
        difference |= given ^ want;
    }
    difference == 0
}

/// rmcp's transport config, assembled from [`Config`].
///
/// The library defaults were chosen for a remote server with browser clients; this is a
/// local, unauthenticated endpoint, so every field that encodes a trust decision is set
/// here from the boot config rather than inherited:
///
/// * `legacy_session_mode = false`. The pinned revision has no sessions, and a server that
///   could also speak the old session handshake is a second behaviour to test for no
///   benefit (`--allow-legacy-sessions` was dropped for the same reason).
/// * `json_response = true`. Stateless request/response tools answer with `application/json`
///   instead of an SSE stream, which keeps a plain client working; rmcp still falls back to
///   SSE if a handler streams notifications mid-call, which is what `run_workflow` will
///   want later.
/// * `allowed_hosts` comes from [`Config::allowed_hosts`] and is **never empty** — an empty
///   allowlist is rmcp's "accept any Host" mode, which is the DNS-rebinding hole.
/// * `allowed_origins` + `enforce_origin_validation`: enforcement is always on, so an empty
///   allowlist refuses every request that carries an `Origin` rather than allowing any of
///   them (see [`Config::origin_validation_enforced`]).
/// * `max_request_body_bytes` from `--max-request-body-bytes`: an oversized POST is
///   answered 413 while the body streams, so it does not matter that HTTP/1.1 chunking or
///   a lying `Content-Length` could otherwise size the buffer.
/// * `stateless_protocol_metadata_required = true`. In the pinned revision the protocol
///   version travels per request (`MCP-Protocol-Version`, plus the `_meta` entry); left at
///   rmcp's default a request that names *no* version is silently read as `2025-03-26`, i.e.
///   this server would answer a dialect it says it does not speak. ADR-0001 §7 and
///   `docs/tool-contract.md` §1 both pin the strict answer — refuse, and say why — and
///   `tests/protocol_conformance.rs` asserts it on the wire rather than in source.
/// * `sse_keep_alive` is [`SSE_KEEP_ALIVE`], explained there.
fn http_config(
    config: Arc<Config>,
    cancellation_token: CancellationToken,
) -> StreamableHttpServerConfig {
    StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_cancellation_token(cancellation_token)
        .with_allowed_hosts(config.allowed_hosts.iter().map(String::as_str))
        .with_allowed_origins(config.allowed_origins.iter().map(String::as_str))
        .enforce_origin_validation()
        .with_max_request_body_bytes(config.max_request_body_bytes)
        .with_stateless_protocol_metadata_required(true)
        .with_sse_keep_alive(Some(SSE_KEEP_ALIVE))
}

/// Bind, log the address, and serve until `shutdown` is cancelled.
///
/// Shutdown is two-phase, because the ticket's budget is "a couple of seconds"
/// and a half-open SSE stream must not be able to hold the process open:
///
/// 1. `shutdown` cancels (ctrl-c / SIGTERM, wired in the binary) → axum stops
///    accepting connections and in-flight handlers get `SHUTDOWN_DRAIN_BUDGET`
///    to finish on their own.
/// 2. After that budget the session token is cancelled, which tears down live
///    MCP sessions, their SSE streams, and (from mact-f1w.8/.11 onward) the act
///    children hanging off them.
///
/// The session token is deliberately *not* a child of `shutdown`: if it were, the
/// signal would kill live sessions at the same instant the listener stopped and
/// phase 1 would be a lie.
pub async fn serve(wiring: Arc<Wiring>, shutdown: CancellationToken) -> anyhow::Result<()> {
    // The body reads the same two handles the wiring was assembled from; taking them apart here
    // keeps `Wiring` the single thing a caller has to build once per process.
    let config = Arc::clone(&wiring.config);
    let supervisor = Arc::clone(&wiring.supervisor);
    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    let local = listener.local_addr()?;
    // The second, independent loopback check: `Config::build` already refused a routable
    // `--bind` without the acknowledgement, and this one checks the address the socket
    // actually got. Between the two, "it is only listening on loopback" is a property of the
    // running process rather than of the argument parser.
    guard_resolved_bind(&config, local)?;
    let url = endpoint_url(local);
    log_startup_surface(&config, local, &url);
    // ADR-0003 §9.2: sweep the run scratch root before anything can be served. Whatever is in it
    // belongs to a process that is gone, and there is no better moment — nothing is live yet, so
    // no lock is needed, and the alternative is that every SIGKILL leaks a checked-out worktree
    // into a runtime directory until the disk fills.
    let debris = run_workflow::pipeline::sweep_scratch_root(
        &config.run_scratch,
        &config.workspace,
        config.git_timeout,
    )
    .await;
    if debris > 0 {
        tracing::info!(
            leftover_run_scratch_dirs = debris,
            scratch_root = %config.run_scratch.display(),
            "cleared the leftover scratch directories of a previous process"
        );
    }
    tracing::info!(
        %url,
        bind = %local,
        workspace = %config.workspace.display(),
        evidence_dir = %config.evidence_dir.display(),
        act_binary = %config.act_binary.display(),
        protocol_version = %SUPPORTED_PROTOCOL_VERSION,
        allowed_hosts = ?config.allowed_hosts,
        allowed_origins = ?config.allowed_origins,
        max_request_body_bytes = config.max_request_body_bytes,
        shared_secret_required = config.session_token.is_some(),
        "act-mcp listening"
    );

    let sessions = CancellationToken::new();
    let app = router(Arc::clone(&wiring), sessions.clone());
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

/// Refuse to run on a socket that is reachable from elsewhere, unless the operator said so.
///
/// The condition is checked against the *bound* address, not the requested one: the
/// difference is exactly the case this guards (a config assembled by an embedder that never
/// went through [`Config::build`], a `--bind` that resolved to something surprising).
///
/// # Errors
///
/// [`crate::config::ConfigError::ResolvedBindNotLoopback`] when the socket is not loopback
/// and [`--i-understand-this-is-unauthenticated`][crate::config::UNAUTH_ACK_FLAG] was not
/// passed.
fn guard_resolved_bind(config: &Config, local: SocketAddr) -> anyhow::Result<()> {
    if crate::config::is_loopback(local.ip()) || config.non_loopback_bind_acknowledged {
        return Ok(());
    }
    Err(
        anyhow::Error::new(crate::config::ConfigError::ResolvedBindNotLoopback(local)).context(
            "refusing to serve: the socket this process bound is reachable from other machines",
        ),
    )
}

/// Say out loud, at startup, what the network surface is.
///
/// Loud on purpose: the whole ticket is that "HTTP but no auth, single local machine" is a
/// claim about the socket, and an operator who widened it must not be able to miss that they
/// did. A loopback boot gets one info line; everything else gets a banner.
fn log_startup_surface(config: &Config, local: SocketAddr, url: &str) {
    if crate::config::is_loopback(local.ip()) {
        tracing::info!(
            bind = %local,
            "bind is loopback-only: only this machine can reach the endpoint (there is no authentication)"
        );
    } else {
        // The acknowledged case. Multi-line because a single WARN line scrolls away.
        tracing::warn!(
            bind = %local,
            %url,
            allowed_hosts = ?config.allowed_hosts,
            concat!(
                "############################################################\n",
                "# act-mcp is LISTENING ON A NON-LOOPBACK ADDRESS and has NO  \n",
                "# AUTHENTICATION. Everyone who can route to this socket can   \n",
                "# run containerised workflows on this machine via act.       \n",
                "# You passed --i-understand-this-is-unauthenticated; this    \n",
                "# line is the record that the exposure was deliberate.       \n",
                "# Requests must still name this socket in Host (see           \n",
                "# --allow-host, docs/security.md) and no browser Origin is    \n",
                "# allowed unless you passed --allow-origin.                   \n",
                "############################################################"
            ),
        );
    }

    if let Some(token) = &config.session_token {
        // Deliberately logged: a secret no client can learn is not a control. It is logged
        // once, at boot, on stderr — the same channel every other startup fact takes.
        tracing::info!(
            header = SESSION_TOKEN_HEADER,
            token = token.expose(),
            "shared secret required on every request (printed once; keep it out of logs you share)"
        );
    }
}

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

#[cfg(test)]
mod tests {
    use super::server_instructions;
    use crate::tools::{cancel_run, list_tasks, run_status, run_workflow};
    use std::path::Path;

    /// The `instructions` text is the only place an agent is told the *sequence* and where
    /// evidence lands, so the parts of it a caller would act on are pinned here rather than
    /// left to whoever last liked the wording. (mact-f1w.18: "the server's instructions field
    /// explains the workflow: list_tasks → run_workflow → read evidence paths".)
    #[test]
    fn instructions_state_the_workflow_and_where_evidence_lands() {
        let text = server_instructions(
            "9.9.9",
            Path::new("/srv/work/repo"),
            Path::new("/srv/work/repo/.evidence"),
        );

        for fact in [
            "9.9.9",
            "/srv/work/repo",
            "/srv/work/repo/.evidence",
            // the order, in order
            "1. list_tasks",
            "2. run_workflow",
            "3. Read the evidence",
            // the three things that change what a caller does next
            "workspace_busy",
            "wait: false",
            "status: failed",
            // what each category writes, and that anything else writes nothing
            "benchmark/<commit>/<run_id>.jsonl",
            "coverage/<commit>/<run_id>.jsonl",
            "writes NO evidence file",
            "runs/<run_id>/meta.json",
            // the two consumer-facing guarantees of ADR-0002 §4.3/§4.4
            "never aggregated",
            "percentages are recomputed",
        ] {
            assert!(
                text.contains(fact),
                "instructions must state {fact:?}, got:\n{text}"
            );
        }
    }

    /// Every tool's description is written for an agent (ADR-0001 §1), which includes saying
    /// what it does to `.evidence` — including "nothing", in `list_tasks`' case.
    #[test]
    fn every_tool_description_names_evidence() {
        for (tool, description) in [
            ("list_tasks", list_tasks::DESCRIPTION),
            ("run_workflow", run_workflow::DESCRIPTION),
            ("run_status", run_status::DESCRIPTION),
            ("cancel_run", cancel_run::DESCRIPTION),
        ] {
            assert!(
                description.len() > 200,
                "{tool}'s description is too short to be a contract"
            );
            assert!(
                description.contains(".evidence"),
                "{tool}'s description must say what it does to `.evidence`"
            );
        }
    }
}
