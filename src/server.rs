//! The MCP service and the axum mount that serves it.
//!
//! The service is intentionally empty in this ticket: no tool logic and no `act`
//! invocation (those are mact-f1w.7 … mact-f1w.14). What has to be right here is
//! the shell everything else hangs on — the pinned protocol revision, the
//! loopback-only bind, the shared immutable [`Config`], and a cancellation token
//! that actually drains.

use std::{borrow::Cow, net::SocketAddr, sync::Arc, time::Duration};

use rmcp::{
    model::{Implementation, ProtocolVersion, ServerCapabilities, ServerConfig},
    tool_handler, tool_router,
    transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    },
    ServerHandler,
};
use tokio_util::sync::CancellationToken;
use tower::ServiceBuilder;
use tower_http::trace::TraceLayer;

use crate::config::Config;

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
}

impl ActMcp {
    /// Wrap the shared boot config for one session.
    ///
    /// Called by the service factory per request, so this stays cheap: it clones
    /// an `Arc`, nothing else.
    pub fn new(config: Arc<Config>) -> Self {
        Self { config }
    }

    /// The shared boot config. Tool handlers (mact-f1w.7+) read workspace,
    /// evidence-dir and act-binary paths through this.
    pub fn config(&self) -> &Arc<Config> {
        &self.config
    }
}

/// Empty tool router: `tools/list` answers a valid, empty envelope.
///
/// `allow_empty` is deliberate — the impl has no `#[tool]` fns until
/// `list_tasks` (mact-f1w.7) and `run_workflow` (mact-f1w.14) land.
#[tool_router(allow_empty)]
impl ActMcp {}

#[tool_handler]
impl ServerHandler for ActMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(SUPPORTED_PROTOCOL_VERSION)
            .with_server_info(Implementation::new(SERVER_NAME, env!("CARGO_PKG_VERSION")))
            .with_instructions(format!(
                "act-mcp {} — runs GitHub Actions workflows via act and writes .evidence JSONL.\n\
                 Workspace: {} (evidence: {})\n\
                 No tools are registered yet; tools/list is empty in this build.",
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
/// hands it the session token it cancels after the drain window.
pub fn mcp_service(
    config: Arc<Config>,
    cancellation_token: CancellationToken,
) -> StreamableHttpService<ActMcp, LocalSessionManager> {
    StreamableHttpService::new(
        move || Ok(ActMcp::new(Arc::clone(&config))),
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
pub fn router(config: Arc<Config>, cancellation_token: CancellationToken) -> axum::Router {
    axum::Router::new()
        .nest_service(MCP_ROUTE, mcp_service(config, cancellation_token))
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
pub async fn serve(config: Arc<Config>, shutdown: CancellationToken) -> anyhow::Result<()> {
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
    let app = router(Arc::clone(&config), sessions.clone());
    let drain_sessions = sessions.clone();

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown.cancelled().await;
            tracing::info!(
                drain_budget = ?SHUTDOWN_DRAIN_BUDGET,
                "shutdown signal received; draining in-flight requests"
            );
            tokio::time::sleep(SHUTDOWN_DRAIN_BUDGET).await;
            tracing::debug!("end of drain window; ending live sessions");
            drain_sessions.cancel();
        })
        .await?;

    // `serve` can also return on an internal error with sessions still alive.
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
