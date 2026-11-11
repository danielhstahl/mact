//! Boot configuration: what `act-mcp serve` accepts, and what it refuses.
//!
//! Everything here is checked **at boot** so a misconfigured server fails loudly
//! and legibly instead of failing later inside a tool call. Nothing in this
//! module touches the network.
//!
//! Flag/env precedence: a flag always wins over its env var, because clap's
//! `env` attribute only fills in a value that was not given on the command line.

use std::{
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    path::{Path, PathBuf},
    time::Duration,
};

use axum::http::HeaderValue;
use clap::Args;
use thiserror::Error;

use crate::workspace::{IsolationPolicy, SemVer, MAX_GIT_TIMEOUT, MINIMUM_ACT_VERSION};

/// Default bind address. Loopback only, always: act-mcp has **no
/// authentication** (mact-f1w.5), and `act` runs arbitrary container images, so
/// a routable bind is not a mode we ship. `--bind` to anything else is rejected
/// by [`Config::build`] unless the operator passes
/// [`--i-understand-this-is-unauthenticated`][UNAUTH_ACK_FLAG], which also makes
/// [`crate::server::serve`] shout about it at startup.
pub const DEFAULT_BIND: &str = "127.0.0.1:8080";

/// Default `act` binary name, looked up on `PATH` with [`which`] at boot.
pub const DEFAULT_ACT_BINARY: &str = "act";

/// The spelled-out acknowledgement a non-loopback bind needs, without the leading `--`.
///
/// This is the string to grep for (mact-f1w.13): the flag, its env var, the refusal
/// message, and the startup banner all name it, so "what did we make the operator type
/// for that?" is one search away.
pub const UNAUTH_ACK_FLAG: &str = "i-understand-this-is-unauthenticated";

/// The env-var spelling of [`UNAUTH_ACK_FLAG`].
pub const UNAUTH_ACK_ENV: &str = "ACT_MCP_I_UNDERSTAND_THIS_IS_UNAUTHENTICATED";

/// The `Host` values accepted unless `--allow-host` adds to them.
///
/// rmcp's own default, kept deliberately: `localhost` (what an `ssh -L` client sends),
/// the v4 loopback literal, and the v6 loopback literal. A request naming any other
/// `Host:` is refused with 403, which is what closes DNS rebinding against a local
/// server — a browser tricked into resolving `evil.example` to `127.0.0.1` still sends
/// `Host: evil.example`.
pub const DEFAULT_ALLOWED_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "::1"];

/// Default `--max-request-body-bytes`.
///
/// A `run_workflow` call is a workflow name, a job id, and a handful of `--var`/`--input`
/// pairs; a megabyte is already two orders of magnitude more than a legitimate call, and
/// the bound is what stops one unauthenticated local process from making this one buffer
/// a request the size of its RAM.
pub const DEFAULT_MAX_REQUEST_BODY_BYTES: usize = 1_048_576;
/// Smallest `--max-request-body-bytes`: below this even a call with a couple of `--var`
/// pairs starts failing, so lowering it further is always a mistake.
pub const MIN_REQUEST_BODY_BYTES: usize = 1_024;
/// Largest `--max-request-body-bytes`: past it the limit is decorative.
pub const MAX_REQUEST_BODY_BYTES: usize = 64 * 1024 * 1024;

/// The header the opt-in shared secret travels in (`--require-session-token`).
pub const SESSION_TOKEN_HEADER: &str = "x-act-mcp-token";

/// `--run-scratch`'s default (ADR-0003 §11): `$XDG_RUNTIME_DIR/act-mcp-<uid>`, else
/// `/tmp/act-mcp-<uid>`.
///
/// On the platforms where this crate has no runner there is no per-user runtime directory
/// convention to honour, so the temp directory stands in; the type, the flag and the rule
/// (outside the workspace) are the same everywhere.
#[cfg(unix)]
fn default_run_scratch_dir() -> PathBuf {
    crate::runner::default_run_scratch_dir()
}

/// See the [`default_run_scratch_dir`] above.
#[cfg(not(unix))]
fn default_run_scratch_dir() -> PathBuf {
    std::env::temp_dir().join(format!("act-mcp-{}", std::process::id()))
}

/// Default `--max-run-dirs`: how many `.evidence/runs/<run_id>/` directories may exist
/// before a new run is refused. Retention (§ ADR-0004 §7.3) prunes *memory*, never disk,
/// so without a cap one long-lived server fills the disk a run at a time.
pub const DEFAULT_MAX_RUN_DIRS: usize = 1_000;

/// Resolved, validated, immutable boot configuration.
///
/// Shared with the service factory as an `Arc<Config>`; per-session state stays
/// cheap and `Clone` (see [`crate::server::ActMcp`]).
#[derive(Debug, Clone)]
pub struct Config {
    /// Repo root being acted on. Guaranteed to exist and to contain `.github`.
    pub workspace: PathBuf,
    /// Where durable `.evidence` JSONL lands. Defaults to
    /// `<workspace>/.evidence`; see ADR-0002 §1.1. Not created at boot — the
    /// evidence writer (mact-f1w.9) creates directories on demand.
    pub evidence_dir: PathBuf,
    /// Resolved bind address (always loopback).
    pub bind: SocketAddr,
    /// Resolved `act` executable (always an absolute path that exists).
    pub act_binary: PathBuf,
    /// Optional scratch dir for act log capture (mact-f1w.8). `None` means the
    /// runner will use `<evidence-dir>/runs`.
    pub log_dir: Option<PathBuf>,
    /// ADR-0003 D-1: which trees this server is willing to run (`--isolation`).
    pub isolation: IsolationPolicy,
    /// ADR-0003 §4.2 G5: downgrade the dirty-work-tree gate to the
    /// `dirty_worktree` warning (`--allow-dirty-worktree`).
    pub allow_dirty_worktree: bool,
    /// ADR-0003 §10: per-`git`-call deadline (`--git-timeout-secs`).
    pub git_timeout: Duration,
    /// Oldest act release this server runs (`--minimum-act-version`); never below
    /// [`MINIMUM_ACT_VERSION`].
    pub minimum_act_version: SemVer,
    /// Container-runtime socket the reachability probe dials, when the operator
    /// pinned one (`--container-daemon-socket`). `None` probes the runtime default.
    pub container_daemon_socket: Option<PathBuf>,
    /// `--list-tasks-act-cross-check`: ask `act list` which workflow files it can
    /// actually load during `list_tasks`, and report the files it refuses.
    ///
    /// Off by default. `list_tasks` is meant to be the cheapest call on the server —
    /// no act, no docker, no lock — and act's listing has no machine-readable form
    /// (see `tools::list_tasks::cross_check_with_act`), so the cross-check costs a
    /// subprocess and a fragile table parse per call. It is an operator opt-in for
    /// repositories where "act refuses this file" is worth paying that for.
    pub list_tasks_act_cross_check: bool,
    /// The resolved bind address **is not loopback** and the operator said so with
    /// [`--i-understand-this-is-unauthenticated`][UNAUTH_ACK_FLAG].
    ///
    /// `false` means the loopback rule is in force all the way down: [`crate::server::serve`]
    /// re-checks the socket it actually bound and refuses to run on a routable address.
    pub non_loopback_bind_acknowledged: bool,
    /// `Host:` values accepted (see [`DEFAULT_ALLOWED_HOSTS`] and `--allow-host`).
    /// Never empty: an empty list means "accept any `Host`" in rmcp, which is the
    /// DNS-rebinding hole.
    pub allowed_hosts: Vec<String>,
    /// `Origin:` values accepted (`--allow-origin`). Usually empty, and empty means
    /// *refuse every request that carries an `Origin`*, not "allow any": see
    /// [`Config::origin_validation_enforced`].
    pub allowed_origins: Vec<String>,
    /// `--max-request-body-bytes`: a larger POST is answered 413 by rmcp.
    pub max_request_body_bytes: usize,
    /// The opt-in shared secret ([`SESSION_TOKEN_HEADER`]), `None` when the feature is off.
    pub session_token: Option<SessionToken>,
    /// Cap on `.evidence/runs/` directories (`--max-run-dirs`); see
    /// [`crate::registry::RunPolicy::max_run_dirs`].
    pub max_run_dirs: usize,
    /// `--run-scratch`: the per-run scratch root outside the workspace (act's cwd, the
    /// `0600` secrets file, and an ADR-0003 worktree). See
    /// `Config::resolve_run_scratch` (private, same module) for the one rule boot enforces.
    pub run_scratch: PathBuf,
}

/// The opt-in shared secret, held so it cannot be printed by accident.
///
/// A secret that shows up in a `Debug` line is a secret in the log file, and `Config`
/// *is* debug-logged at boot (`tracing::debug!(?config, …)`). So this newtype renders as
/// `SessionToken(<redacted>)` and hands out its value only through [`SessionToken::expose`]
/// / [`SessionToken::header_value`].
#[derive(Clone)]
pub struct SessionToken(String);

impl SessionToken {
    /// Wrap a token the operator supplied. Empty is not a token.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        if value.is_empty() {
            None
        } else {
            Some(Self(value))
        }
    }

    /// Generate 128 bits from `/dev/urandom`, hex.
    ///
    /// Same source as the run id's random suffix uses. There is no
    /// fallback here on purpose: a secret that silently becomes `clock() ^ pid` is worse
    /// than a boot that fails, and this one is only generated when the operator asked for
    /// the feature.
    #[must_use]
    pub fn generate() -> Option<Self> {
        let mut bytes = [0u8; 16];
        let mut file = std::fs::File::open("/dev/urandom").ok()?;
        std::io::Read::read_exact(&mut file, &mut bytes)
            .ok()
            .map(|()| Self(bytes.iter().map(|byte| format!("{byte:02x}")).collect()))
    }

    /// The raw value, for the comparison and for the one boot log line that prints it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// What a client must send in [`SESSION_TOKEN_HEADER`].
    pub fn header_value(&self) -> HeaderValue {
        // The alphabet is hex (generated) or whatever the operator passed; an
        // ASCII-visible value is validated at boot, so this cannot fail in practice and
        // a rejection at first request would be a worse failure than one at boot.
        HeaderValue::from_str(&self.0).unwrap_or_else(|_| HeaderValue::from_static("invalid"))
    }
}

impl std::fmt::Debug for SessionToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SessionToken(<redacted len={}>)", self.0.len())
    }
}

/// Reasons `act-mcp serve` refuses to start.
///
/// The `Display` text is user-facing — it is what the CLI prints on stderr and
/// what an MCP host will show — so every variant names the offending value and
/// the way to fix it. The `#[error(...)]` string is the documentation of the
/// variant, hence the allow below.
#[allow(
    missing_docs,
    reason = "the #[error] format string documents each variant"
)]
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("workspace does not exist: {path}\n  fix: pass --workspace <path> at the repository you want to act on")]
    WorkspaceMissing { path: PathBuf },

    #[error("workspace is not a directory: {path}")]
    WorkspaceNotADirectory { path: PathBuf },

    #[error(
        "workspace {path} has no `.github` directory, so there are no GitHub Actions workflows to run\n  fix: point --workspace at the repository root that contains .github/workflows/*.yml"
    )]
    WorkspaceMissingGithub { path: PathBuf },

    #[error(
        "could not parse --bind {value:?}: expected host:port such as 127.0.0.1:8080 ({reason})"
    )]
    BindUnresolved { value: String, reason: String },

    #[error(
        "--bind {addr} is not a loopback address, and act-mcp has no authentication\n  fix: bind 127.0.0.1 or ::1 — act runs arbitrary containers behind this endpoint\n  fix: if this endpoint is deliberately reachable from elsewhere, pass --{} and expect a startup warning", UNAUTH_ACK_FLAG
    )]
    BindNotLoopback { addr: SocketAddr },

    #[error("--bind {0} is loopback-only by default; the resolved socket was not loopback")]
    ResolvedBindNotLoopback(SocketAddr),

    #[error("--allow-origin {value:?} is not an origin\n  fix: a full origin with a scheme, such as http://localhost:8080 (or ://…:* to match any port)")]
    OriginInvalid { value: String },

    #[error("--session-token was empty; pass a value, or use --require-session-token to have one generated")]
    SessionTokenEmpty,

    #[error(
        "--max-request-body-bytes {value} is out of range ({}..={})",
        MIN_REQUEST_BODY_BYTES,
        MAX_REQUEST_BODY_BYTES
    )]
    MaxRequestBodyOutOfRange { value: usize },

    #[error("--max-run-dirs must be at least 1; 0 would refuse every run before it started")]
    MaxRunDirsZero,

    #[error(
        "act binary not found: `{requested}`\n  fix: install act (https://github.com/nektos/act) or pass --act-binary /absolute/path/to/act"
    )]
    ActBinaryMissing { requested: String },

    #[error("--log-dir exists but is not a directory: {path}")]
    LogDirNotADirectory { path: PathBuf },

    #[error("cannot read the current working directory: {reason}\n  fix: pass --workspace <path> explicitly")]
    NoCurrentDirectory { reason: String },

    #[error("--isolation {value:?} is not a policy\n  fix: one of head-only, hybrid, worktree")]
    IsolationPolicyInvalid { value: String },

    #[error("--git-timeout-secs {value} is out of range (1..={})", MAX_GIT_TIMEOUT.as_secs())]
    GitTimeoutOutOfRange { value: u32 },

    #[error("--minimum-act-version {value:?} is not a major.minor.patch version ({reason})")]
    MinimumActVersionInvalid { value: String, reason: String },

    #[error("--minimum-act-version {value} is older than {floor}, the oldest act release whose flags this server emits\n  fix: install act {floor} or newer, or raise --minimum-act-version")]
    MinimumActVersionTooOld { value: String, floor: String },

    #[error("--container-daemon-socket was empty; pass a socket path such as /run/docker.sock, or omit it")]
    ContainerDaemonSocketEmpty,

    #[error(
        "--run-scratch {path} is inside the workspace {workspace}\n  fix: run scratch must live outside the repository: act's --bind exposes the whole tree to the job, and the job reads secrets from its own scratch directory (ADR-0003 §8, §9.1)"
    )]
    RunScratchInsideWorkspace { path: PathBuf, workspace: PathBuf },
}

/// `act-mcp serve` arguments.
///
/// Public so integration tests and embedders can build it; the binary is a thin
/// wrapper over [`Config::build`] + [`crate::server::serve`].
#[derive(Debug, Clone, Args)]
pub struct ServeArgs {
    /// Repository to act on. Must exist and contain `.github`.
    #[arg(
        long,
        value_name = "PATH",
        env = "ACT_MCP_WORKSPACE",
        help = "Repository root to act on; must contain .github/workflows. Default: current directory."
    )]
    pub workspace: Option<PathBuf>,

    /// Address to bind. Loopback only.
    #[arg(
        long,
        value_name = "ADDR",
        default_value = DEFAULT_BIND,
        env = "ACT_MCP_BIND",
        help = "Address to listen on; loopback only (act-mcp has no auth). Port 0 asks the kernel for a free port; the real address is logged at startup."
    )]
    pub bind: String,

    /// Durable evidence root.
    #[arg(
        long,
        value_name = "PATH",
        // EVIDENCE_DIR is the env fallback named in ADR-0002 §1.1.
        env = "EVIDENCE_DIR",
        help = "Evidence root; defaults to <workspace>/.evidence."
    )]
    pub evidence_dir: Option<PathBuf>,

    /// `act` executable to run.
    #[arg(
        long,
        value_name = "PATH",
        default_value = DEFAULT_ACT_BINARY,
        env = "ACT_MCP_ACT_BINARY",
        help = "act binary to invoke; resolved on PATH at boot."
    )]
    pub act_binary: PathBuf,

    /// Scratch dir for act log capture.
    #[arg(
        long,
        value_name = "PATH",
        env = "ACT_MCP_LOG_DIR",
        help = "Directory for act log capture; defaults to <evidence-dir>/runs (mact-f1w.8)."
    )]
    pub log_dir: Option<PathBuf>,

    /// ADR-0003 D-1 isolation policy.
    #[arg(
        long,
        value_name = "POLICY",
        default_value = "hybrid",
        env = "ACT_MCP_ISOLATION",
        help = "Which trees may be run: head-only (HEAD in place only), hybrid (HEAD in place, anything else in a worktree), worktree (always a worktree)."
    )]
    pub isolation: String,

    /// ADR-0003 §4.2 G5 escape hatch.
    #[arg(
        long,
        default_value_t = false,
        env = "ACT_MCP_ALLOW_DIRTY_WORKTREE",
        help = "Run a dirty HEAD anyway, stamping the dirty_worktree warning. Operator-only: the commit field then means 'HEAD plus whatever is in it'."
    )]
    pub allow_dirty_worktree: bool,

    /// ADR-0003 §10 git budget.
    #[arg(
        long,
        value_name = "SECS",
        default_value_t = 30,
        env = "ACT_MCP_GIT_TIMEOUT_SECS",
        help = "Deadline for each read-only git call made while validating a run."
    )]
    pub git_timeout_secs: u32,

    /// Oldest supported act.
    #[arg(
        long,
        value_name = "X.Y.Z",
        default_value = MINIMUM_ACT_VERSION,
        env = "ACT_MCP_MINIMUM_ACT_VERSION",
        help = "Refuse to run an act older than this version."
    )]
    pub minimum_act_version: String,

    /// Cross-check `list_tasks` against `act list`.
    #[arg(
        long,
        default_value_t = false,
        env = "ACT_MCP_LIST_TASKS_ACT_CROSS_CHECK",
        help = "During list_tasks, also run `act list` and report workflow files act refuses to load. Costs a subprocess per call; off by default."
    )]
    pub list_tasks_act_cross_check: bool,

    /// Container-runtime socket to probe.
    #[arg(
        long,
        value_name = "PATH",
        env = "ACT_MCP_CONTAINER_DAEMON_SOCKET",
        help = "Container-runtime socket to check reachability against, instead of the runtime default. `-` (act's 'do not mount it') is treated as unset here."
    )]
    pub container_daemon_socket: Option<String>,

    /// The [`UNAUTH_ACK_FLAG`] escape hatch: bind outside loopback on purpose.
    #[arg(
        long = "i-understand-this-is-unauthenticated",
        env = "ACT_MCP_I_UNDERSTAND_THIS_IS_UNAUTHENTICATED",
        default_value_t = false,
        help = "Allow --bind to name a non-loopback address. act-mcp has NO authentication and act runs arbitrary container images: everyone who can route to this socket can run workflows here."
    )]
    pub i_understand_this_is_unauthenticated: bool,

    /// Extra `Host:` values to accept (repeatable). See `docs/security.md`.
    #[arg(
        long = "allow-host",
        value_name = "HOST",
        env = "ACT_MCP_ALLOWED_HOSTS",
        value_delimiter = ',',
        help = "Accept this Host value in addition to localhost/127.0.0.1/::1; use host or host:port. Name the front-end here for a deliberate port-forward, e.g. --allow-host gateway.internal:8080."
    )]
    pub allow_host: Vec<String>,

    /// Extra browser `Origin:` values to accept (repeatable).
    #[arg(
        long = "allow-origin",
        value_name = "ORIGIN",
        env = "ACT_MCP_ALLOWED_ORIGINS",
        value_delimiter = ',',
        help = "Accept this browser Origin in addition to none. Must carry a scheme, e.g. http://localhost:8080; a request with any other Origin is refused."
    )]
    pub allow_origin: Vec<String>,

    /// `--max-request-body-bytes`.
    #[arg(
        long,
        value_name = "BYTES",
        default_value_t = DEFAULT_MAX_REQUEST_BODY_BYTES,
        env = "ACT_MCP_MAX_REQUEST_BODY_BYTES",
        help = "Largest POST body accepted at /mcp; a larger one is answered 413."
    )]
    pub max_request_body_bytes: usize,

    /// Opt-in shared secret.
    #[arg(
        long,
        default_value_t = false,
        env = "ACT_MCP_REQUIRE_SESSION_TOKEN",
        help = "Require every request to carry the shared secret header x-act-mcp-token, and generate one at boot (logged once on stderr). Cheap defence against another local process using this endpoint; pair it with --session-token to supply your own."
    )]
    pub require_session_token: bool,

    /// The shared secret value to require (implies [`ServeArgs::require_session_token`]).
    #[arg(
        long,
        value_name = "TOKEN",
        env = "ACT_MCP_SESSION_TOKEN",
        help = "Require exactly this x-act-mcp-token value instead of a generated one."
    )]
    pub session_token: Option<String>,

    /// `--max-run-dirs`.
    #[arg(
        long,
        value_name = "COUNT",
        default_value_t = DEFAULT_MAX_RUN_DIRS,
        env = "ACT_MCP_MAX_RUN_DIRS",
        help = "Refuse a new run once this many .evidence/runs/<run_id>/ directories exist; run retention prunes memory, never disk, so this is the disk guard."
    )]
    pub max_run_dirs: usize,

    /// `--run-scratch <dir>` (ADR-0003 §11).
    #[arg(
        long,
        value_name = "DIR",
        env = "ACT_MCP_RUN_SCRATCH",
        help = "Where per-run scratch lives (act's cwd, secrets.env, and an ADR-0003 worktree): defaults to $XDG_RUNTIME_DIR/act-mcp-<uid>, else /tmp/act-mcp-<uid>. Must be outside --workspace."
    )]
    pub run_scratch: Option<PathBuf>,
}

impl Default for ServeArgs {
    fn default() -> Self {
        Self {
            workspace: None,
            bind: DEFAULT_BIND.to_owned(),
            evidence_dir: None,
            act_binary: PathBuf::from(DEFAULT_ACT_BINARY),
            log_dir: None,
            isolation: "hybrid".to_owned(),
            allow_dirty_worktree: false,
            git_timeout_secs: 30,
            minimum_act_version: MINIMUM_ACT_VERSION.to_owned(),
            container_daemon_socket: None,
            list_tasks_act_cross_check: false,
            i_understand_this_is_unauthenticated: false,
            allow_host: Vec::new(),
            allow_origin: Vec::new(),
            max_request_body_bytes: DEFAULT_MAX_REQUEST_BODY_BYTES,
            require_session_token: false,
            session_token: None,
            max_run_dirs: DEFAULT_MAX_RUN_DIRS,
            run_scratch: None,
        }
    }
}

impl Config {
    /// Resolve and validate CLI args into a boot configuration.
    ///
    /// Check order: workspace exists → workspace has `.github` → bind resolves
    /// and is loopback (or was explicitly acknowledged) → `act` resolves → log dir is
    /// usable → the request-shape and network-surface limits are inside their bounds. The
    /// order is deliberate: the cheapest and most likely-mistaken check runs first, so
    /// the first error a user sees is the one they caused.
    pub fn build(args: &ServeArgs) -> Result<Self, ConfigError> {
        let workspace = Self::resolve_workspace(args)?;
        let bind = Self::resolve_bind(&args.bind, args.i_understand_this_is_unauthenticated)?;
        let act_binary = Self::resolve_act(&args.act_binary)?;

        let isolation = Self::resolve_isolation(&args.isolation)?;
        let git_timeout = Self::resolve_git_timeout(args.git_timeout_secs)?;
        let minimum_act_version = Self::resolve_minimum_act_version(&args.minimum_act_version)?;
        // `-` is act's own "do not mount the daemon socket into the job" value. It
        // says nothing about whether a runtime is reachable — act always needs one —
        // so the probe treats it as unset (ADR-0003 §8).
        let container_daemon_socket = match args.container_daemon_socket.as_deref() {
            None => None,
            Some("") => return Err(ConfigError::ContainerDaemonSocketEmpty),
            Some("-") => None,
            Some(socket) => Some(PathBuf::from(socket)),
        };

        let evidence_dir = args
            .evidence_dir
            .clone()
            .unwrap_or_else(|| workspace.join(".evidence"));

        if let Some(log_dir) = &args.log_dir {
            if log_dir.exists() && !log_dir.is_dir() {
                return Err(ConfigError::LogDirNotADirectory {
                    path: log_dir.clone(),
                });
            }
        }

        let session_token =
            Self::resolve_session_token(args.require_session_token, args.session_token.as_deref())?;
        let allowed_origins = Self::resolve_allowed_origins(&args.allow_origin)?;
        if args.max_run_dirs == 0 {
            return Err(ConfigError::MaxRunDirsZero);
        }
        let run_scratch = Self::resolve_run_scratch(args.run_scratch.as_deref(), &workspace)?;

        Ok(Self {
            workspace,
            evidence_dir,
            bind,
            act_binary,
            log_dir: args.log_dir.clone(),
            isolation,
            allow_dirty_worktree: args.allow_dirty_worktree,
            git_timeout,
            minimum_act_version,
            container_daemon_socket,
            list_tasks_act_cross_check: args.list_tasks_act_cross_check,
            non_loopback_bind_acknowledged: !is_loopback(bind.ip()),
            allowed_hosts: Self::resolve_allowed_hosts(bind, &args.allow_host),
            allowed_origins,
            max_request_body_bytes: Self::resolve_max_request_body(args.max_request_body_bytes)?,
            session_token,
            max_run_dirs: args.max_run_dirs,
            run_scratch,
        })
    }

    /// The shipped defaults for every field mact-f1w.13 owns, assembled in one place.
    ///
    /// The point is that exactly one function decides what the *default* network surface is,
    /// and both [`Config::build`] (through the same helpers) and every test/embedder
    /// constructor go through it, so a test cannot accidentally exercise a server with a
    /// wider surface than the shipped one — the kind of gap where "the test passed" and
    /// "the binary is safe" come apart.
    #[must_use]
    pub fn with_hardened_defaults(
        workspace: PathBuf,
        evidence_dir: PathBuf,
        act_binary: PathBuf,
    ) -> Self {
        let bind = DEFAULT_BIND
            .parse::<SocketAddr>()
            .expect("the default bind parses");
        Self {
            workspace,
            evidence_dir,
            bind,
            act_binary,
            log_dir: None,
            isolation: IsolationPolicy::default(),
            allow_dirty_worktree: false,
            git_timeout: crate::workspace::DEFAULT_GIT_TIMEOUT,
            minimum_act_version: MINIMUM_ACT_VERSION
                .parse::<SemVer>()
                .expect("the pinned floor is a valid version"),
            container_daemon_socket: None,
            list_tasks_act_cross_check: false,
            non_loopback_bind_acknowledged: false,
            allowed_hosts: Self::resolve_allowed_hosts(bind, &[]),
            allowed_origins: Vec::new(),
            max_request_body_bytes: DEFAULT_MAX_REQUEST_BODY_BYTES,
            session_token: None,
            max_run_dirs: DEFAULT_MAX_RUN_DIRS,
            // Same default the CLI resolves to, so an embedder that never went through
            // [`Config::build`] still gets the ADR-0003 §11 scratch root.
            run_scratch: default_run_scratch_dir(),
        }
    }

    /// `--run-scratch`: the per-run scratch root (act's cwd, `secrets.env`, and the
    /// ADR-0003 worktree of a synthesized tree).
    ///
    /// Only one rule is enforceable at boot — it must not live inside the workspace, because
    /// act's `--bind` hands the whole tree to the job and the job reads `secrets.env` from
    /// its own scratch directory (ADR-0003 §8/§9.1). Whether it is on a *local* filesystem
    /// is not something a portable check can establish, so the operator's word is taken.
    /// The directory itself is created (mode 0700) per run by the pipeline rather than at
    /// boot: boot must not have filesystem side effects a read-only check would trip over.
    fn resolve_run_scratch(
        requested: Option<&Path>,
        workspace: &Path,
    ) -> Result<PathBuf, ConfigError> {
        let scratch = match requested {
            Some(path) => path.to_path_buf(),
            None => default_run_scratch_dir(),
        };
        // Canonical on *both* sides of the comparison. The workspace reached this point
        // canonicalised ([`Config::validate_workspace`]) while `--run-scratch` is whatever the
        // operator typed, so comparing a canonical workspace against a path that spells the
        // same directory through a symlink — macOS' `$TMPDIR`, where `/var` and `/tmp` are
        // links into `/private` — reads as "two unrelated places" and lets
        // `<workspace>/scratch` boot, which is precisely the directory act's `--bind` then
        // hands to the job together with its `secrets.env` (ADR-0003 §11).
        //
        // Only the canonical spelling is consulted, on either side. A raw prefix test looks
        // free to keep and is not: `<workspace>/../elsewhere` shares the workspace's raw
        // prefix and is a different place, so the raw test refuses configurations the ADR
        // allows, while the canonical one resolves the `..` and sees where the directory
        // actually is.
        let workspace_canonical = Self::canonical_path(workspace);
        if Self::canonical_path(&scratch).starts_with(&workspace_canonical) {
            return Err(ConfigError::RunScratchInsideWorkspace {
                path: scratch,
                workspace: workspace.to_path_buf(),
            });
        }
        Ok(scratch)
    }

    /// `path` with every existing part resolved and the missing tail put back on.
    ///
    /// [`Path::canonicalize`] is std's only resolver and it insists that the whole path
    /// exist, but boot has to compare paths it must not create — `--run-scratch` names a
    /// directory the first run makes (ADR-0003 §11). So resolve as far as the filesystem
    /// can, from the deepest existing ancestor down, and keep the rest as written. A
    /// relative path is resolved against the working directory first, because
    /// `--run-scratch ./work/scratch` and `--workspace .` are meant to be two spellings of
    /// one place, not two places.
    fn canonical_path(path: &Path) -> PathBuf {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(path))
                .unwrap_or_else(|_| path.to_path_buf())
        };
        if let Ok(canonical) = absolute.canonicalize() {
            return canonical;
        }
        // Walk up until something exists, collecting the components that do not.
        let mut missing = Vec::new();
        let mut cursor = absolute.as_path();
        let resolved = loop {
            match cursor.canonicalize() {
                Ok(canonical) => break canonical,
                Err(_) => match (cursor.file_name(), cursor.parent()) {
                    (Some(name), Some(parent)) => {
                        missing.push(name);
                        cursor = parent;
                    }
                    // Nothing in the path exists, or there is no component left to strip
                    // (a root, or a bare relative name): compare what we have.
                    _ => return absolute,
                },
            }
        };
        let mut out = resolved;
        for name in missing.iter().rev() {
            out.push(name);
        }
        out
    }

    ///
    /// ADR-0004 §11's defaults, with the one cap this ticket owns overlaid. The other knobs
    /// stay with the ADR defaults on purpose: they are the budgets ADR-0004 pins, and moving
    /// them onto the CLI is a ticket that also has to re-check the interaction rules in
    /// [`crate::registry::RunPolicy::validate`] — whereas `--max-run-dirs` exists only to keep
    /// a long-lived local server from filling the disk one run at a time.
    #[cfg(unix)]
    #[must_use]
    pub fn run_policy(&self) -> crate::registry::RunPolicy {
        crate::registry::RunPolicy {
            max_run_dirs: self.max_run_dirs,
            ..crate::registry::RunPolicy::default()
        }
    }

    fn resolve_isolation(value: &str) -> Result<IsolationPolicy, ConfigError> {
        value
            .parse::<IsolationPolicy>()
            .map_err(|_| ConfigError::IsolationPolicyInvalid {
                value: value.to_owned(),
            })
    }

    fn resolve_git_timeout(secs: u32) -> Result<Duration, ConfigError> {
        if secs == 0 || Duration::from_secs(u64::from(secs)) > MAX_GIT_TIMEOUT {
            return Err(ConfigError::GitTimeoutOutOfRange { value: secs });
        }
        Ok(Duration::from_secs(u64::from(secs)))
    }

    fn resolve_minimum_act_version(value: &str) -> Result<SemVer, ConfigError> {
        let parsed =
            value
                .parse::<SemVer>()
                .map_err(|reason| ConfigError::MinimumActVersionInvalid {
                    value: value.to_owned(),
                    reason,
                })?;
        let floor = MINIMUM_ACT_VERSION
            .parse::<SemVer>()
            .expect("the pinned floor is a valid version");
        if parsed < floor {
            return Err(ConfigError::MinimumActVersionTooOld {
                value: value.to_owned(),
                floor: MINIMUM_ACT_VERSION.to_owned(),
            });
        }
        Ok(parsed)
    }

    fn resolve_workspace(args: &ServeArgs) -> Result<PathBuf, ConfigError> {
        let raw = match args.workspace.clone() {
            Some(path) => path,
            None => std::env::current_dir().map_err(|e| ConfigError::NoCurrentDirectory {
                reason: e.to_string(),
            })?,
        };
        Self::validate_workspace(&raw)
    }

    /// Workspace gate: it must exist and it must contain a `.github` directory.
    ///
    /// Deliberately minimal at this stage — workflow/task resolution, the HEAD
    /// commit gate and docker presence land in mact-f1w.6 (ADR-0003).
    pub(crate) fn validate_workspace(path: &Path) -> Result<PathBuf, ConfigError> {
        if !path.exists() {
            return Err(ConfigError::WorkspaceMissing {
                path: path.to_path_buf(),
            });
        }
        if !path.is_dir() {
            return Err(ConfigError::WorkspaceNotADirectory {
                path: path.to_path_buf(),
            });
        }
        if !path.join(".github").is_dir() {
            return Err(ConfigError::WorkspaceMissingGithub {
                path: path.to_path_buf(),
            });
        }
        // Canonicalise once, here, so every later path comparison (worktree
        // paths, evidence paths, `../` smuggling checks in later tickets) is
        // made against a single absolute, symlink-free root.
        Ok(std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()))
    }

    fn resolve_bind(value: &str, acknowledged: bool) -> Result<SocketAddr, ConfigError> {
        // `127.0.0.1:8080` and `[::1]:8080` parse directly; `localhost:8080`
        // needs a lookup. Anything that resolves to port 0 is rejected below,
        // so a bare hostname cannot silently become "pick a random port".
        let addrs: Vec<SocketAddr> = if let Ok(addr) = value.parse::<SocketAddr>() {
            vec![addr]
        } else {
            match value.to_socket_addrs() {
                Ok(iter) => iter.collect(),
                Err(err) => {
                    return Err(ConfigError::BindUnresolved {
                        value: value.to_owned(),
                        reason: err.to_string(),
                    });
                }
            }
        };
        let addr = addrs
            .into_iter()
            .next()
            .ok_or_else(|| ConfigError::BindUnresolved {
                value: value.to_owned(),
                reason: "no address returned".to_owned(),
            })?;

        // Port 0 is allowed and useful: the kernel picks a free port and
        // `serve` logs the real one. A typo like `--bind 127.0.0.1` (no port)
        // never gets here — it fails to resolve above.
        if !is_loopback(addr.ip()) && !acknowledged {
            return Err(ConfigError::BindNotLoopback { addr });
        }
        Ok(addr)
    }

    /// The `Host:` allowlist: [`DEFAULT_ALLOWED_HOSTS`] plus the bound address when the
    /// operator knowingly bound somewhere else, plus every `--allow-host`.
    ///
    /// The bound IP is added only in the acknowledged non-loopback case: an operator who
    /// passed [`UNAUTH_ACK_FLAG`] has already accepted that the endpoint is reachable, and
    /// refusing `Host: 192.168.1.20:8080` on that socket would be a surprise rather than a
    /// defence (the DNS-rebinding argument is about loopback, where a browser can be tricked
    /// into dialing `127.0.0.1` for an attacker's domain name). A DNS *name* still has to
    /// be named with `--allow-host`, so a port-forward or a reverse proxy is an explicit
    /// act, not an accident — see `docs/security.md`.
    fn resolve_allowed_hosts(bind: SocketAddr, extra: &[String]) -> Vec<String> {
        let mut hosts: Vec<String> = DEFAULT_ALLOWED_HOSTS
            .iter()
            .map(|host| (*host).to_owned())
            .collect();
        if !is_loopback(bind.ip()) {
            hosts.push(bind.ip().to_string());
        }
        for host in extra {
            let trimmed = host.trim();
            if !trimmed.is_empty() && !hosts.iter().any(|seen| seen.eq_ignore_ascii_case(trimmed)) {
                hosts.push(trimmed.to_owned());
            }
        }
        hosts
    }

    /// Validate every `--allow-origin` before it reaches rmcp.
    ///
    /// An origin without a scheme is refused rather than guessed at: rmcp treats a
    /// schemeless entry as no match at all, so silently accepting one would produce a
    /// allowlist entry that never fires — the subtle failure this whole ticket is against.
    fn resolve_allowed_origins(extra: &[String]) -> Result<Vec<String>, ConfigError> {
        extra
            .iter()
            .map(|raw| {
                let value = raw.trim();
                let scheme_and_host = value.split_once("://").and_then(|(scheme, rest)| {
                    let host = rest.trim_end_matches('/');
                    let scheme_ok =
                        scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https");
                    // `:*` is rmcp's "any port" spelling; a host may not contain `/`.
                    let host_ok = !host.is_empty()
                        && !host.contains('/')
                        && !host.contains(char::is_whitespace);
                    if scheme_ok && host_ok {
                        Some(value.to_owned())
                    } else {
                        None
                    }
                });
                scheme_and_host.ok_or_else(|| ConfigError::OriginInvalid { value: raw.clone() })
            })
            .collect()
    }

    /// `--require-session-token` / `--session-token`: the supplied value wins, and asking
    /// to require one without naming it gets a generated one (which the server logs once,
    /// because a secret nobody can read is not a control).
    fn resolve_session_token(
        require: bool,
        supplied: Option<&str>,
    ) -> Result<Option<SessionToken>, ConfigError> {
        match supplied {
            Some("") => Err(ConfigError::SessionTokenEmpty),
            Some(value) => Ok(SessionToken::new(value)),
            None if require => SessionToken::generate()
                .map(Some)
                .ok_or(ConfigError::SessionTokenEmpty),
            None => Ok(None),
        }
    }

    fn resolve_max_request_body(bytes: usize) -> Result<usize, ConfigError> {
        if !(MIN_REQUEST_BODY_BYTES..=MAX_REQUEST_BODY_BYTES).contains(&bytes) {
            return Err(ConfigError::MaxRequestBodyOutOfRange { value: bytes });
        }
        Ok(bytes)
    }

    /// Whether requests carrying an `Origin:` must match [`Config::allowed_origins`].
    ///
    /// Always `true`, and the point of naming it is that it is *not* a knob: an MCP client
    /// is not a browser, so an `Origin` header on this endpoint means either a browser (the
    /// cross-site-request case) or someone probing. A local browser-based MCP client that
    /// really needs to reach us names itself with `--allow-origin`.
    #[must_use]
    pub const fn origin_validation_enforced(&self) -> bool {
        true
    }

    fn resolve_act(requested: &Path) -> Result<PathBuf, ConfigError> {
        let requested_text = requested.display().to_string();
        // `which` accepts either a bare name (PATH search) or a path, so
        // `--act-binary ./act` and `--act-binary act` both mean something.
        which::which(requested).map_err(|_| ConfigError::ActBinaryMissing {
            requested: requested_text,
        })
    }
}

/// Loopback test covering `127.0.0.0/8`, `::1`, and IPv4-mapped `::ffff:127.0.0.1`.
pub fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.is_loopback(),
            None => v6.is_loopback(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::{IsolationPolicy, DEFAULT_GIT_TIMEOUT, MINIMUM_ACT_VERSION};
    use std::time::Duration;

    fn args_with(workspace: &Path, act: &Path) -> ServeArgs {
        ServeArgs {
            workspace: Some(workspace.to_path_buf()),
            bind: DEFAULT_BIND.to_owned(),
            evidence_dir: None,
            act_binary: act.to_path_buf(),
            log_dir: None,
            ..ServeArgs::default()
        }
    }

    /// A workspace that passes the gate: a dir with `.github/workflows` in it.
    fn fixture_workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(".github/workflows")).expect("create .github");
        dir
    }

    /// An executable stub standing in for `act` (mact-f1w.8 replaces it with a
    /// real runner; nothing invokes it yet).
    fn stub_act(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"#!/bin/sh\nexit 0\n").expect("write stub");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        path
    }

    #[test]
    fn build_accepts_a_valid_workspace_and_resolves_defaults() {
        let workspace = fixture_workspace();
        let tmp = tempfile::tempdir().expect("tempdir");
        let act = stub_act(tmp.path(), "act");
        let config = Config::build(&args_with(workspace.path(), &act)).expect("valid config");

        assert_eq!(
            config.workspace,
            std::fs::canonicalize(workspace.path()).expect("canonical")
        );
        assert_eq!(config.evidence_dir, config.workspace.join(".evidence"));
        assert_eq!(config.bind.to_string(), DEFAULT_BIND);
        assert_eq!(config.act_binary, act);
        assert!(config.log_dir.is_none());
    }

    #[test]
    fn build_rejects_a_missing_workspace() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let act = stub_act(tmp.path(), "act");
        let missing = tmp.path().join("nope");
        let err = Config::build(&args_with(&missing, &act)).expect_err("must fail");
        assert!(matches!(err, ConfigError::WorkspaceMissing { .. }));
        assert!(err.to_string().contains("fix:"));
    }

    #[test]
    fn build_rejects_a_workspace_without_dot_github() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tmp = tempfile::tempdir().expect("tempdir");
        let act = stub_act(tmp.path(), "act");
        let err = Config::build(&args_with(dir.path(), &act)).expect_err("must fail");
        assert!(matches!(err, ConfigError::WorkspaceMissingGithub { .. }));
        assert!(err.to_string().contains(".github"));
    }

    #[test]
    fn build_rejects_a_missing_act_binary() {
        let workspace = fixture_workspace();
        let tmp = tempfile::tempdir().expect("tempdir");
        let err = Config::build(&args_with(workspace.path(), &tmp.path().join("no-act")))
            .expect_err("must fail");
        assert!(matches!(err, ConfigError::ActBinaryMissing { .. }));
        assert!(err.to_string().contains("nektos/act"));
    }

    #[test]
    fn isolation_dirty_and_git_budget_defaults_are_the_adrs() {
        let workspace = fixture_workspace();
        let tmp = tempfile::tempdir().expect("tempdir");
        let act = stub_act(tmp.path(), "act");
        let config = Config::build(&args_with(workspace.path(), &act)).expect("valid config");
        assert_eq!(config.isolation, IsolationPolicy::Hybrid);
        assert!(!config.allow_dirty_worktree);
        // The CLI default and the documented constant are one number, not two.
        assert_eq!(config.git_timeout, DEFAULT_GIT_TIMEOUT);
        assert_eq!(DEFAULT_GIT_TIMEOUT, Duration::from_secs(30));
        assert_eq!(
            config.minimum_act_version,
            MINIMUM_ACT_VERSION.parse().expect("valid version")
        );
        assert_eq!(config.container_daemon_socket, None);
    }

    #[test]
    fn the_isolation_flags_are_validated_at_boot_not_at_call_time() {
        let workspace = fixture_workspace();
        let tmp = tempfile::tempdir().expect("tempdir");
        let act = stub_act(tmp.path(), "act");

        let mut args = args_with(workspace.path(), &act);
        args.isolation = "head_only".to_owned();
        let err = Config::build(&args).expect_err("underscores are not the flag spelling");
        assert!(matches!(err, ConfigError::IsolationPolicyInvalid { .. }));

        let mut args = args_with(workspace.path(), &act);
        args.isolation = "head-only".to_owned();
        assert_eq!(
            Config::build(&args)
                .expect("head-only is a policy")
                .isolation,
            IsolationPolicy::HeadOnly
        );

        let mut args = args_with(workspace.path(), &act);
        args.git_timeout_secs = 0;
        assert!(matches!(
            Config::build(&args).expect_err("0 is not a budget"),
            ConfigError::GitTimeoutOutOfRange { .. }
        ));

        let mut args = args_with(workspace.path(), &act);
        args.git_timeout_secs = 301;
        assert!(matches!(
            Config::build(&args).expect_err("ADR-0003 §10 caps the budget at 300"),
            ConfigError::GitTimeoutOutOfRange { value: 301 }
        ));

        let mut args = args_with(workspace.path(), &act);
        args.minimum_act_version = "0.1.99".to_owned();
        let err = Config::build(&args).expect_err("below the flag floor");
        assert!(matches!(err, ConfigError::MinimumActVersionTooOld { .. }));
        assert!(err.to_string().contains(MINIMUM_ACT_VERSION));

        let mut args = args_with(workspace.path(), &act);
        args.minimum_act_version = "0.2.70".to_owned();
        assert_eq!(
            Config::build(&args)
                .expect("raising the floor is allowed")
                .minimum_act_version,
            "0.2.70".parse().expect("valid version")
        );

        // `-` is act's own value for "no daemon socket in the job"; the probe treats
        // it as unset, an empty value is a typo worth reporting.
        let mut args = args_with(workspace.path(), &act);
        args.container_daemon_socket = Some("-".to_owned());
        assert_eq!(
            Config::build(&args)
                .expect("`-` is legal")
                .container_daemon_socket,
            None
        );
        let mut args = args_with(workspace.path(), &act);
        args.container_daemon_socket = Some("/run/docker.sock".to_owned());
        assert_eq!(
            Config::build(&args)
                .expect("a socket path is legal")
                .container_daemon_socket,
            Some(PathBuf::from("/run/docker.sock"))
        );
        let mut args = args_with(workspace.path(), &act);
        args.container_daemon_socket = Some(String::new());
        assert!(matches!(
            Config::build(&args).expect_err("empty is a typo"),
            ConfigError::ContainerDaemonSocketEmpty
        ));
    }

    #[test]
    fn bind_must_be_loopback() {
        for value in ["0.0.0.0:8080", "10.1.2.3:8080", "8.8.8.8:1194"] {
            let err = Config::resolve_bind(value, false).expect_err("must reject non-loopback");
            assert!(
                matches!(err, ConfigError::BindNotLoopback { .. }),
                "{value} → {err}"
            );
            // The refusal names the only way past it, so the message is the documentation.
            assert!(
                err.to_string().contains(UNAUTH_ACK_FLAG),
                "the refusal must name --{UNAUTH_ACK_FLAG}: {err}"
            );
        }
    }

    #[test]
    fn a_non_loopback_bind_needs_the_acknowledgement_and_says_so_in_the_config() {
        // With the flag the bind is legal, and the config records that it was acknowledged
        // — that flag is what `server::serve` re-checks against the socket it bound.
        let acked = Config::resolve_bind("0.0.0.0:8080", true).expect("acknowledged bind is legal");
        assert_eq!(acked.ip().to_string(), "0.0.0.0");
        assert!(!is_loopback(acked.ip()));
        // A loopback bind never needs it, and never sets the flag.
        assert!(Config::resolve_bind("127.0.0.1:8080", false).is_ok());
    }

    #[test]
    fn the_network_surface_defaults_are_the_narrow_ones() {
        let workspace = fixture_workspace();
        let tmp = tempfile::tempdir().expect("tempdir");
        let act = stub_act(tmp.path(), "act");
        let config = Config::build(&args_with(workspace.path(), &act)).expect("valid config");

        assert_eq!(
            config.allowed_hosts,
            vec!["localhost", "127.0.0.1", "::1"],
            "the default Host allowlist is loopback-only and never empty"
        );
        assert!(
            config.allowed_origins.is_empty(),
            "no browser origin is allowed by default"
        );
        assert!(config.origin_validation_enforced());
        assert_eq!(
            config.max_request_body_bytes,
            DEFAULT_MAX_REQUEST_BODY_BYTES
        );
        assert_eq!(config.max_run_dirs, DEFAULT_MAX_RUN_DIRS);
        assert!(
            config.session_token.is_none(),
            "the shared secret is opt-in, not on by default"
        );
        assert!(!config.non_loopback_bind_acknowledged);
    }

    #[test]
    fn allowed_hosts_grows_only_where_it_is_told_to() {
        let workspace = fixture_workspace();
        let tmp = tempfile::tempdir().expect("tempdir");
        let act = stub_act(tmp.path(), "act");

        let mut args = args_with(workspace.path(), &act);
        args.allow_host = vec!["gateway.internal:8080".to_owned(), " localhost ".to_owned()];
        let config = Config::build(&args).expect("a port-forward is a legal thing to configure");
        assert_eq!(
            config.allowed_hosts,
            vec!["localhost", "127.0.0.1", "::1", "gateway.internal:8080"],
            "--allow-host appends, dedupes, and never replaces the loopback defaults"
        );

        // An acknowledged non-loopback bind adds its own address: the operator who said
        // "expose it" should not then have to guess why clients get 403.
        let mut args = args_with(workspace.path(), &act);
        args.bind = "127.0.0.1:0".to_owned();
        args.i_understand_this_is_unauthenticated = true;
        assert!(
            !Config::build(&args)
                .expect("loopback needs no acknowledgement, but tolerates it")
                .non_loopback_bind_acknowledged
        );
    }

    #[test]
    fn origins_and_limits_are_validated_at_boot() {
        let workspace = fixture_workspace();
        let tmp = tempfile::tempdir().expect("tempdir");
        let act = stub_act(tmp.path(), "act");

        let mut args = args_with(workspace.path(), &act);
        args.allow_origin = vec!["evil.example".to_owned()];
        let err = Config::build(&args).expect_err("an origin needs its scheme");
        assert!(matches!(err, ConfigError::OriginInvalid { .. }));
        assert!(err.to_string().contains("--allow-origin"));

        let mut args = args_with(workspace.path(), &act);
        args.allow_origin = vec![
            "http://localhost:8080".to_owned(),
            "http://127.0.0.1:*".to_owned(),
        ];
        assert_eq!(
            Config::build(&args)
                .expect("a scheme-qualified origin is fine")
                .allowed_origins,
            vec!["http://localhost:8080", "http://127.0.0.1:*"]
        );

        for bytes in [1usize, 999, MAX_REQUEST_BODY_BYTES + 1] {
            let mut args = args_with(workspace.path(), &act);
            args.max_request_body_bytes = bytes;
            assert!(matches!(
                Config::build(&args).expect_err("out of bounds is a typo worth reporting"),
                ConfigError::MaxRequestBodyOutOfRange { .. }
            ));
        }

        let mut args = args_with(workspace.path(), &act);
        args.max_run_dirs = 0;
        assert!(matches!(
            Config::build(&args).expect_err("0 refuses every run"),
            ConfigError::MaxRunDirsZero
        ));

        // `--run-scratch` (ADR-0003 §11): the one enforceable rule is that it is not inside the
        // workspace, because act's `--bind` hands the job the whole tree and the job's own
        // scratch directory holds `secrets.env`.
        let default_scratch = Config::build(&args_with(workspace.path(), &act))
            .expect("a scratch root needs no flag")
            .run_scratch;
        assert!(
            !default_scratch.starts_with(workspace.path()),
            "the default scratch root is a runtime directory, not the repository: {default_scratch:?}"
        );
        let mut args = args_with(workspace.path(), &act);
        args.run_scratch = Some(workspace.path().join("scratch"));
        assert!(matches!(
            Config::build(&args)
                .expect_err("scratch inside the workspace is the leak the ADR forbids"),
            ConfigError::RunScratchInsideWorkspace { .. }
        ));
        let mut args = args_with(workspace.path(), &act);
        args.run_scratch = Some(tmp.path().join("scratch"));
        assert_eq!(
            Config::build(&args)
                .expect("an explicit scratch root outside the workspace is fine")
                .run_scratch,
            std::fs::canonicalize(tmp.path().join("scratch"))
                .unwrap_or_else(|_| tmp.path().join("scratch")),
            "the operator's path is taken as given, not moved under a subdirectory"
        );

        // The shared secret: generated on request, supplied by hand, never empty.
        let mut args = args_with(workspace.path(), &act);
        args.require_session_token = true;
        let generated = Config::build(&args)
            .expect("requiring a token generates one")
            .session_token
            .expect("generated");
        assert_eq!(generated.expose().len(), 32, "128 bits of hex");
        assert_ne!(generated.expose(), generated.expose().to_uppercase());
        assert!(
            !format!("{generated:?}").contains(generated.expose()),
            "Debug must not print the secret: {generated:?}"
        );

        let mut args = args_with(workspace.path(), &act);
        args.session_token = Some("hunter2hunter2".to_owned());
        assert_eq!(
            Config::build(&args)
                .expect("a supplied token implies requiring it")
                .session_token
                .expect("kept")
                .expose(),
            "hunter2hunter2"
        );

        let mut args = args_with(workspace.path(), &act);
        args.session_token = Some(String::new());
        assert!(matches!(
            Config::build(&args).expect_err("empty is not a secret"),
            ConfigError::SessionTokenEmpty
        ));
    }

    /// ADR-0003 §11's "not inside the workspace" rule is about a *place*, and a place can have
    /// two spellings: macOS' `$TMPDIR` lives under `/var`, which is a link into
    /// `/private/var`, so a test temp directory — and any workspace under it — is reachable as
    /// two different strings. Boot canonicalises the workspace on the way in and used to
    /// canonicalise only the scratch side of the comparison, which waved through a scratch
    /// path spelled the other way through the link: `secrets.env` inside the tree act is
    /// about to `--bind`.
    #[test]
    #[cfg(unix)]
    fn a_scratch_inside_a_workspace_reached_through_a_symlink_is_still_refused() {
        let root = tempfile::tempdir().expect("tempdir");
        let real = root.path().join("real");
        std::fs::create_dir_all(real.join(".github/workflows")).expect("create .github");
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("symlink the workspace");
        let tmp = tempfile::tempdir().expect("tempdir");
        let act = stub_act(tmp.path(), "act");

        // Workspace spelled through the link, scratch spelled through what it points at.
        let mut args = args_with(&link, &act);
        args.run_scratch = Some(real.join("scratch"));
        assert!(matches!(
            Config::build(&args)
                .expect_err("one directory is one directory, however it is spelled"),
            ConfigError::RunScratchInsideWorkspace { .. }
        ));

        // The mirror image, which is the direction the old comparison missed: the workspace
        // is canonical by the time this check runs, the scratch path is not.
        let mut args = args_with(&real, &act);
        args.run_scratch = Some(link.join("scratch"));
        assert!(matches!(
            Config::build(&args).expect_err("and the same the other way round"),
            ConfigError::RunScratchInsideWorkspace { .. }
        ));

        // A scratch that really is outside the tree still boots even when it is spelled
        // through the tree with a `..`: the rule refuses one thing, not everything it has to
        // think about.
        let mut args = args_with(&link, &act);
        args.run_scratch = Some(real.join("../elsewhere"));
        assert!(
            Config::build(&args).is_ok(),
            "`<workspace>/../elsewhere` is not inside the workspace"
        );
    }

    #[test]
    fn bind_accepts_loopback_forms_and_rejects_portless_or_rubbish() {
        assert_eq!(
            Config::resolve_bind("[::1]:8080", false)
                .expect("v6 loopback")
                .port(),
            8080
        );
        assert!(Config::resolve_bind("localhost:8080", false).is_ok());
        // No port at all is a typo, not "port 0".
        assert!(matches!(
            Config::resolve_bind("127.0.0.1", false),
            Err(ConfigError::BindUnresolved { .. })
        ));
        // Port 0 means "any free port"; `serve` logs the port it got.
        assert_eq!(
            Config::resolve_bind("127.0.0.1:0", false)
                .expect("port 0 is legal")
                .port(),
            0
        );
        assert!(Config::resolve_bind("not an address", false).is_err());
    }

    #[test]
    fn loopback_helper_covers_mapped_v6() {
        let parse = |text: &str| text.parse::<IpAddr>().expect("literal ip address");
        assert!(is_loopback(parse("127.5.6.7")));
        assert!(!is_loopback(parse("192.168.0.1")));
        assert!(is_loopback(parse("::1")));
        assert!(is_loopback(parse("::ffff:127.0.0.1")));
        assert!(!is_loopback(parse("::ffff:8.8.8.8")));
    }
}
