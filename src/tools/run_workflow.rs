//! `run_workflow` — the call every other piece of this server exists to serve.
//!
//! This module is the *wiring*: it validates, isolates, registers, waits, and renders. Each
//! heavy thing it needs already has an owner and is used rather than reimplemented:
//!
//! | step | who actually does it |
//! |---|---|
//! | argument shape | this module (types and bounds), then [`crate::workspace::Validator`] (patterns, gates, probes) |
//! | which tree act runs against | ADR-0003 §4 mode table (the validator's verdict) + [`pipeline`] (`git worktree add`) |
//! | the workspace single-flight lock, `run_id`, budgets, teardown | [`crate::registry::RunSupervisor`] |
//! | spawning act, the deadline, the log, the kill | [`crate::runner`] |
//! | turning a finished run into `.evidence` JSONL | [`crate::evidence::coverage`] / [`crate::evidence::benchmark`] via [`pipeline`] |
//! | publishing + `meta.json` | the supervisor's terminal commit (ADR-0004 §3.4) with [`crate::evidence::EvidenceRoot`] |
//!
//! ## The flow, and where each failure lands
//!
//! ADR-0001 §2.9's order, short-circuiting:
//!
//! 1. [`RunWorkflowArgs::parse`] — `invalid_arguments`, `unknown_argument`. No filesystem, no
//!    subprocess: a bad call is rejected before anything is looked at.
//! 2. `idempotency_key` replay ([`idempotency`]) — replay, or `idempotency_conflict`.
//! 3. [`Validator::validate_async`] — the workspace gates, commit resolution, workflow/task
//!    resolution, environment probes and the isolation gate, in that order, each with its own
//!    ADR-0001 §5.3 code.
//! 4. The tree: `head_only` uses the workspace; `worktree` checks the scratch root is writable
//!    (ADR-0003 §4.4 W3 → `isolation_failed`) and names the operator's branch for
//!    `--defaultbranch` (§5.4) *before* registering, so both failures still mean "nothing was
//!    started". The worktree itself is created after registration, in `preparing_tree`, and a
//!    failure there is `runner_internal_error` `detail.stage: "preparing_tree"` — a run status,
//!    not a call error, exactly as ADR-0003 §4.4 pins the boundary.
//! 5. `supervisor.start(plan)` — `too_many_runs`, `workspace_busy`, or the run dir could not be
//!    created. Nothing after this point is a call error: ADR-0001 §5.1's rule is that a run that
//!    ran and failed is `status: "failed"` in a normal `isError: false` result.
//! 6. `wait: true` (default) waits for the terminal commit, bounded by ADR-0004 §4.6's
//!    worst-case budget, and emits progress notifications on phase change for a client that
//!    asked for them. `wait: false` returns the registered snapshot immediately.
//! 7. The result is `run_view` plus `deduplicated` — and because the terminal commit publishes
//!    evidence and writes `meta.json` *before* a terminal snapshot is observable, a result with
//!    non-empty `evidence_files` means those files are already on disk (ADR-0004 §3.4).
//!
//! ## Categories, and the honest empty answer
//!
//! `category` is an open vocabulary (ADR-0001 §2.4). `benchmark` and `coverage` have extractors;
//! anything else runs, writes **no** evidence file, and says so with `warnings:
//! ["unknown_category"]` plus `evidence_files: []`, so an agent reading an empty scan is not left
//! to guess whether it broke. Adding a category later is additive: one arm in
//! [`pipeline::harvest`] and one extractor.
//!
//! ## Errors
//!
//! Every failure in steps 1–5 is a `isError: true` tool result carrying the ADR-0001 §5.2
//! payload, never a JSON-RPC error (see [`crate::tools`] for why arguments are parsed by hand).

pub mod idempotency;
pub mod pipeline;

use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use rmcp::{
    model::{ProgressNotificationParam, ProgressToken, RequestMetaObject},
    service::{Peer, RoleServer},
};
// SEP-2577 deprecates MCP logging, but the 2026-07-28 revision this server pins still defines
// `notifications/message`, and a client that sent `_meta` with a log level expects an answer in
// the vocabulary it spoke. One `allow` with the reason, rather than three scattered ones.
#[allow(deprecated)]
use rmcp::model::LoggingLevel;
use serde_json::{json, Map, Value};

use rmcp::model::JsonObject;
use tokio_util::sync::CancellationToken;

use crate::{
    config::Config,
    registry::{
        RunIdentity, RunNotFound, RunPlan, RunSupervisor, RunTicket, StartError, WireStatus,
    },
    runner::{default_payload_dir, RunId, RunSpec},
    tools::{
        run_workflow::idempotency::{RawTuple, Replay, ResolvedTuple, Table},
        schemas, ArgumentProblem, ToolError,
    },
    workspace::{IsolationMode, RunRequest, SystemEnvironment, ValidationPolicy, Validator},
};

/// The tool's name.
pub const TOOL: &str = "run_workflow";

/// `tools/list` title (ADR-0001 §1).
pub const TITLE: &str = "Run one workflow job against one commit";

/// How often a waiting call re-reads the run snapshot when nothing published in between.
///
/// Phase changes arrive on the run's watch channel, so this is only the fallback clock for the
/// deadline; 200 ms is far below anything a human notices and far above the cost of reading one
/// `Arc` from memory.
const WAIT_TICK: Duration = Duration::from_millis(200);

/// The ADR-0004 §4.6 slack added to `timeout_secs` for a `wait: true` call's own budget.
///
/// `timeout_secs` is act's budget only; pre-start (`≤120 s`), the SIGTERM grace (`≤240 s`) and
/// harvest (`≤120 s`) are supervisor overhead on top of it. The ADR rounds that to "+300 s at
/// defaults"; the real maximum is computed from the live policy instead, so a server with a
/// raised grace does not have its wait cut short by a constant.
pub fn wait_budget(act: Duration, policy: &crate::registry::RunPolicy) -> Duration {
    act + policy.prestart_timeout
        + policy.term_grace
        + policy.harvest_timeout
        + Duration::from_secs(60)
}

/// The advertised description — the only documentation an agent reads (ADR-0001 §1).
pub const DESCRIPTION: &str =
    "Run one job of one GitHub Actions workflow in this server's workspace \
with act, against one commit, and return the terminal result. Arguments: `workflow` \
(`coverage`, `coverage.yml` or `.github/workflows/coverage.yml`), `task` (the job id exactly as \
`act -j` takes it - call list_tasks first), `category` and `commit` (7..=40 hex, resolved locally, \
never fetched) are required; optional `wait` (default true), `timeout_secs` (act's wall-clock \
budget and the wait: defaults coverage 900, benchmark 1800, anything else 1800), \
`log_tail_bytes` (0..=65536, default 4096), `inputs`, `secrets` (never argv, never logged) and \
`idempotency_key` (a retry of the same key + same tuple returns the remembered run instead of \
starting a second one). \
EVIDENCE: on completion a run whose `category` is `coverage` writes one append-only JSONL file to \
`.evidence/coverage/<full-sha>/<run_id>.jsonl` (lcov.info / coverage-summary.json from the job, \
scraped percentage only as a last resort) and one whose `category` is `benchmark` writes \
`.evidence/benchmark/<full-sha>/<run_id>.jsonl` (the job's own metrics, `ACTMCP_BENCHMARK` \
markers, and act's step/job timings). Every run also writes `.evidence/runs/<run_id>/act.jsonl` \
and, once terminal, `.evidence/runs/<run_id>/meta.json`; those two are audit paths and are NOT in \
`evidence_files`. Any OTHER category is accepted (the vocabulary is open so new categories are \
additive), runs normally and writes NO evidence file: expect `evidence_files: []`, \
`evidence_record_count: 0` and `warnings: [unknown_category]`. A job that wants to publish \
numbers under a known category writes them to `$ACTMCP_EVIDENCE_OUT` inside the job. \
RESULT: the `run_view` - `status` (`success` | `failed` | `cancelled` | `timed_out` | `error` | \
`pending` | `running`), `terminal`, `phase`, resolved `workflow`/`workflow_file`/`task`/`category` \
and the full `commit`, `started_at`/`finished_at`/`duration_ms`, act's `exit_code`, \
`evidence_files` + `evidence_record_count`, redacted `log_tail`, `warnings`, and `error` when the \
status is `error`. A workflow that RAN and failed is a normal result with `status: failed` \
(`isError: false`) - `isError: true` means no run was started or controlled, and then \
`structuredContent.error.code` is one of: `invalid_arguments`, `unknown_argument`, \
`workspace_not_found`, `not_a_git_repository`, `workflows_dir_missing`, `act_missing`, \
`docker_unreachable`, `workflow_not_found`, `ambiguous_workflow`, `task_not_found`, \
`commit_unknown`, `commit_ambiguous`, `isolation_failed`, `workspace_busy`, `too_many_runs`, \
`idempotency_conflict`, `internal_error`. One run at a time per workspace and there is no queue: \
a second call while one is in flight is `workspace_busy` naming the run that is going, which you \
can poll with run_status or stop with cancel_run. `wait: false` returns as soon as the run is \
registered and the run keeps going; `$/cancel-request` on a waiting call cancels the run.";

/// The argument names, in schema order.
pub const ARGUMENT_NAMES: [&str; 10] = [
    "workflow",
    "task",
    "category",
    "commit",
    "wait",
    "timeout_secs",
    "log_tail_bytes",
    "inputs",
    "secrets",
    "idempotency_key",
];

/// The default `log_tail_bytes` (ADR-0001 §9).
pub const DEFAULT_LOG_TAIL_BYTES: usize = 4096;
/// `timeout_secs`'s compiled upper bound as advertised (ADR-0001 §9).
pub const MAX_TIMEOUT_SECS: u64 = crate::registry::DEFAULT_MAX_TIMEOUT_SECS;
/// `inputs`/`secrets` entry cap (ADR-0001 §9).
pub const MAX_MAP_ENTRIES: usize = 32;
/// `inputs` per-value byte cap.
pub const MAX_INPUT_VALUE_BYTES: usize = 4096;
/// `secrets` per-value byte cap.
pub const MAX_SECRET_VALUE_BYTES: usize = 8192;
/// `idempotency_key` bound and pattern (ADR-0001 §9).
pub const MAX_IDEMPOTENCY_KEY_BYTES: usize = 64;

/// Validated `run_workflow` arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunWorkflowArgs {
    /// Which workflow file, as typed (resolution is the validator's job).
    pub workflow: String,
    /// The job id — exactly what `act -j` receives.
    pub task: String,
    /// What the run is for (open vocabulary, shape-checked).
    pub category: String,
    /// The commit to attribute the run to, as typed.
    pub commit: String,
    /// Block for the terminal result (`true`, ADR-0001 §2.6) or return once registered.
    pub wait: bool,
    /// Act's budget and the wait's bound; `None` takes the per-category default.
    pub timeout_secs: Option<Duration>,
    /// How much log tail to return.
    pub log_tail_bytes: usize,
    /// `act --input`, sorted by key (a `BTreeMap` is the sort).
    pub inputs: BTreeMap<String, String>,
    /// The `--secret-file` passthrough; values are never echoed anywhere.
    pub secrets: BTreeMap<String, String>,
    /// Caller-side dedup token (ADR-0001 §8).
    pub idempotency_key: Option<String>,
}

impl RunWorkflowArgs {
    /// Validate `params.arguments` by hand — see [`crate::tools`] for why.
    ///
    /// Every problem with the call is collected, so one round trip fixes all of them
    /// (ADR-0001 §5.3). Unknown keys are refused first, because a caller who misspelled
    /// `commit` must not get a run of a different commit.
    ///
    /// # Errors
    ///
    /// [`ToolError`] (`unknown_argument` / `invalid_arguments`). Secret *values* never appear
    /// in a problem's `reason`: the key is named, the value is counted.
    pub fn parse(arguments: Option<&JsonObject>) -> Result<Self, ToolError> {
        crate::tools::reject_unknown_arguments(TOOL, arguments, &ARGUMENT_NAMES)?;
        let empty = JsonObject::new();
        let arguments = arguments.unwrap_or(&empty);
        let mut problems: Vec<ArgumentProblem> = Vec::new();
        let mut text = |key: &'static str, expected: &'static str| -> Option<String> {
            match arguments.get(key) {
                None => {
                    problems.push(ArgumentProblem {
                        path: key,
                        reason: "required".to_owned(),
                        expected,
                    });
                    None
                }
                Some(Value::String(value)) => Some(value.clone()),
                Some(other) => {
                    problems.push(ArgumentProblem {
                        path: key,
                        reason: format!("expected a string, got {}", kind_of(other)),
                        expected,
                    });
                    None
                }
            }
        };

        let workflow = text(
            "workflow",
            "a workflow name (`coverage`, `coverage.yml`) or a path under `.github/workflows/`",
        )
        .unwrap_or_default();
        let task = text(
            "task",
            "a job id declared in the workflow file, exactly what act -j receives",
        )
        .unwrap_or_default();
        let category = text("category", crate::tools::category::EXPECTED).unwrap_or_default();
        let commit = text(
            "commit",
            "7 to 40 hex digits, either case, of a commit this clone already has",
        )
        .unwrap_or_default();
        // The commit's shape is checked here as well as by the validator (ADR-0001 §2.5), because
        // a non-hex string can never resolve and the caller deserves it in the same batch as
        // their other mistakes — the validator short-circuits and would report only the first.
        if !commit.is_empty() && !is_commit_abbreviation(&commit) {
            problems.push(ArgumentProblem {
                path: "commit",
                reason: format!("`{commit}` is not a commit abbreviation"),
                expected: "7 to 40 hex digits, either case",
            });
        }
        // The schema advertises that `..` is not expressible in a workflow or task name
        // (ADR-0001 §2.2, §2.3), and the validator enforces it again later. Enforcing it here
        // too is not duplication: a traversal can never resolve, so a caller should learn that
        // before anything looks at a filesystem, in the same batch as their other mistakes.
        for (name, value) in [("workflow", workflow.as_str()), ("task", task.as_str())] {
            if !value.is_empty() && value.contains("..") {
                problems.push(ArgumentProblem {
                    path: name,
                    reason: "two consecutive dots are not allowed anywhere in a name".to_owned(),
                    expected: "a name or workspace-relative path with no `..` segment",
                });
            }
        }
        // The category has a Rust-side check of its own because it is the one open vocabulary
        // in this contract, and the shape is what keeps it out of every path this tool builds.
        if !category.is_empty() {
            if let Err(problem) = crate::tools::category::validate(&category) {
                problems.push(problem);
            }
        }

        let mut wait = true;
        match arguments.get("wait") {
            None | Some(Value::Null) => {}
            Some(Value::Bool(value)) => wait = *value,
            Some(other) => problems.push(ArgumentProblem {
                path: "wait",
                reason: format!("expected a boolean, got {}", kind_of(other)),
                expected: "true (wait for the terminal result) or false (return once registered)",
            }),
        }

        let mut timeout_secs = None;
        match arguments.get("timeout_secs") {
            None | Some(Value::Null) => {}
            Some(Value::Number(number)) => match number.as_u64() {
                Some(value) if (1..=MAX_TIMEOUT_SECS).contains(&value) => {
                    timeout_secs = Some(Duration::from_secs(value));
                }
                other => problems.push(ArgumentProblem {
                    path: "timeout_secs",
                    reason: match other {
                        Some(value) => format!("{value} is outside the allowed range"),
                        None => "not a positive integer".to_owned(),
                    },
                    expected: "an integer number of seconds, 1..=21600 (defaults: coverage 900, \
                               benchmark 1800, any other category 1800)",
                }),
            },
            Some(other) => problems.push(ArgumentProblem {
                path: "timeout_secs",
                reason: format!("expected an integer, got {}", kind_of(other)),
                expected: "an integer number of seconds, 1..=21600",
            }),
        }

        let mut log_tail_bytes = DEFAULT_LOG_TAIL_BYTES;
        match arguments.get("log_tail_bytes") {
            None | Some(Value::Null) => {}
            Some(Value::Number(number)) => match number.as_u64() {
                Some(value) if value <= crate::runner::MAX_LOG_TAIL_BYTES as u64 => {
                    // Safe: the bound above is the platform-independent 65536.
                    log_tail_bytes = value as usize;
                }
                other => problems.push(ArgumentProblem {
                    path: "log_tail_bytes",
                    reason: match other {
                        Some(value) => format!("{value} exceeds the 65536 byte bound"),
                        None => "not a non-negative integer".to_owned(),
                    },
                    expected: "0..=65536 (0 returns no log at all)",
                }),
            },
            Some(other) => problems.push(ArgumentProblem {
                path: "log_tail_bytes",
                reason: format!("expected an integer, got {}", kind_of(other)),
                expected: "0..=65536",
            }),
        }

        let inputs = parse_map(
            arguments.get("inputs"),
            "inputs",
            MAX_INPUT_VALUE_BYTES,
            &mut problems,
        );
        let secrets = parse_map(
            arguments.get("secrets"),
            "secrets",
            MAX_SECRET_VALUE_BYTES,
            &mut problems,
        );

        let mut idempotency_key = None;
        match arguments.get("idempotency_key") {
            None | Some(Value::Null) => {}
            Some(Value::String(value)) if is_idempotency_key(value) => {
                idempotency_key = Some(value.clone());
            }
            Some(Value::String(value)) => problems.push(ArgumentProblem {
                path: "idempotency_key",
                reason: format!("{value:?} is not a key"),
                expected: "1..=64 bytes of `[A-Za-z0-9._:-]`",
            }),
            Some(other) => problems.push(ArgumentProblem {
                path: "idempotency_key",
                reason: format!("expected a string, got {}", kind_of(other)),
                expected: "1..=64 bytes of `[A-Za-z0-9._:-]`",
            }),
        }

        if !problems.is_empty() {
            return Err(ToolError::invalid_arguments(TOOL, problems));
        }
        Ok(Self {
            workflow,
            task,
            category,
            commit,
            wait,
            timeout_secs,
            log_tail_bytes,
            inputs,
            secrets,
            idempotency_key,
        })
    }

    /// The step-3 identity pair (ADR-0001 §8): what the caller typed, commit lowercased.
    #[must_use]
    pub fn raw_tuple(&self) -> RawTuple {
        RawTuple::new(&self.workflow, &self.task, &self.category, &self.commit)
    }
}

/// Everything a call needs that outlives the request: the process-wide collaborators.
///
/// One per process, built at boot and shared by every session — which is the whole point: a run
/// started by one request is pollable and cancellable from any other, and two requests carrying
/// the same `idempotency_key` must see the same table.
pub struct Wiring {
    /// The immutable boot configuration.
    pub config: Arc<Config>,
    /// The process-wide run supervisor.
    pub supervisor: Arc<RunSupervisor>,
    /// The validation gate, with its 30 s probe cache.
    pub validator: Arc<Validator>,
    /// `idempotency_key → run` (ADR-0001 §8).
    pub idempotency: Arc<Table>,
}

impl Wiring {
    /// Build the runtime for a boot configuration: a supervisor whose pipeline runs the real
    /// extractors and the real worktree commands, plus the validator the tool calls.
    ///
    /// # Errors
    ///
    /// [`crate::registry::StartBuildError`] if the assembled run policy is out of ADR-0004 §11's
    /// bounds or the runner's process-group limits could not be built (neither is possible for a
    /// configuration `Config::build` accepted).
    pub fn build(config: Arc<Config>) -> Result<Arc<Self>, crate::registry::StartBuildError> {
        Self::with_policy(Arc::clone(&config), config.run_policy())
    }

    /// [`Wiring::build`] with a run policy the caller chose, keeping the real pipeline.
    ///
    /// The seam for a test (or an embedder) that wants a shorter term grace than the shipped
    /// one without giving up the extractors and worktree commands the default supervisor would
    /// otherwise run.
    pub fn with_policy(
        config: Arc<Config>,
        policy: crate::registry::RunPolicy,
    ) -> Result<Arc<Self>, crate::registry::StartBuildError> {
        let pipeline = Arc::new(pipeline::EvidencePipeline::new(config.git_timeout));
        let supervisor = Arc::new(RunSupervisor::build(policy, pipeline)?);
        Ok(Arc::new(Self::with_supervisor(config, supervisor)))
    }

    /// Assemble around an already-built supervisor.
    ///
    /// The supervisor keeps whatever pipeline it was built with, so a supervisor from
    /// [`RunSupervisor::new`] publishes no evidence at all — right for the run-control tools'
    /// tests, wrong for anything that wants the extraction path. [`Self::build`] and
    /// [`Self::with_policy`] are the paths that install [`pipeline::EvidencePipeline`].
    #[must_use]
    pub fn with_supervisor(config: Arc<Config>, supervisor: Arc<RunSupervisor>) -> Self {
        let validator = Arc::new(Validator::new(
            ValidationPolicy::from_config(&config),
            Arc::new(SystemEnvironment),
        ));
        Self {
            config,
            supervisor,
            validator,
            idempotency: Arc::new(Table::new()),
        }
    }
}

impl std::fmt::Debug for Wiring {
    /// Names the configuration that matters and nothing else.
    ///
    /// Hand-written rather than derived: the supervisor, validator and idempotency table are
    /// stateful and would dump every run's log tail into a `Debug` line (and into any assertion
    /// failure that prints a `Wiring`), which is exactly how a secret that reached a log line ends
    /// up in a bug report.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Wiring")
            .field("workspace", &self.config.workspace)
            .field("evidence_dir", &self.config.evidence_dir)
            .field("run_scratch", &self.config.run_scratch)
            .field("act_binary", &self.config.act_binary)
            .field("isolation", &self.config.isolation)
            .finish_non_exhaustive()
    }
}

/// One call's transport-side facts: who to notify, and which cancellation to honour.
#[derive(Clone)]
pub struct Call {
    /// Where phase changes go, if anywhere.
    pub progress: Progress,
    /// Cancelled when the client sends `$/cancel-request` for *this* request.
    pub cancel: CancellationToken,
    /// This request's key in the supervisor's waiter map (ADR-0004 §5.3).
    pub request_key: String,
}

impl Call {
    /// A call with no notifications and no cancellation — what a direct (non-MCP) caller gets.
    #[must_use]
    pub fn silent(request_key: impl Into<String>) -> Self {
        Self {
            progress: Progress::silent(),
            cancel: CancellationToken::new(),
            request_key: request_key.into(),
        }
    }
}

/// Phase-change notifications, when the client asked for them.
///
/// Two channels, both best-effort and both gated on something the *client* sent: progress
/// notifications only for a request that carried `_meta.progressToken`, logging notifications
/// only for one that carried a `_meta` log level. A client that declared neither gets nothing,
/// which is what "harmless if it didn't" means: a notification with no token is not a progress
/// report, it is a protocol violation.
#[derive(Clone, Default)]
#[allow(deprecated)] // `log_level` is `LoggingLevel`; see the import for why we still answer it
pub struct Progress {
    peer: Option<Peer<RoleServer>>,
    token: Option<ProgressToken>,
    log_level: Option<LoggingLevel>,
    /// Monotonic counter: MCP progress values must not go backwards.
    counter: Arc<AtomicU64>,
}

impl Progress {
    /// The notifications a request's `_meta` asked for.
    #[must_use]
    pub fn for_request(peer: &Peer<RoleServer>, meta: &RequestMetaObject) -> Self {
        Self {
            peer: Some(peer.clone()),
            token: meta.get_progress_token(),
            log_level: meta.log_level(),
            counter: Arc::new(AtomicU64::new(0)),
        }
    }

    /// A sink that drops everything.
    #[must_use]
    pub fn silent() -> Self {
        Self::default()
    }

    /// Whether this call wants any notifications at all (a wait can then skip the tick).
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.token.is_some() || self.log_level.is_some()
    }

    /// Report one phase change.
    #[allow(
        deprecated,
        reason = "SEP-2577 deprecates MCP logging, but the 2026-07-28 revision this server pins \
                  still defines `notifications/cancelled` and logging notifications; a client that \
                  sent `_meta` with a log level expects an answer in the vocabulary it spoke"
    )]
    pub async fn phase(&self, run_id: &str, status: &str, phase: &str) {
        let message = format!("{status}/{phase}");
        if let (Some(peer), Some(token)) = (&self.peer, &self.token) {
            let progress = self.counter.fetch_add(1, Ordering::Relaxed) as f64 + 1.0;
            let params = ProgressNotificationParam::new(token.clone(), progress)
                .with_message(format!("run {run_id}: {message}"));
            if let Err(error) = peer.notify_progress(params).await {
                // A notification can never fail a call: the run and its result are the answer,
                // and a client that stopped reading is not a reason to lose one.
                tracing::debug!(run_id = %run_id, "progress notification not delivered: {error}");
            }
        }
        if let (Some(peer), Some(level)) = (&self.peer, self.log_level) {
            let params = rmcp::model::LoggingMessageNotificationParam::new(
                level,
                json!({ "run_id": run_id, "status": status, "phase": phase }),
            )
            .with_logger(TOOL);
            if let Err(error) = peer.notify_logging_message(params).await {
                tracing::debug!(run_id = %run_id, "log notification not delivered: {error}");
            }
        }
    }
}

/// Run `run_workflow`.
///
/// The returned value is `run_view` plus `deduplicated`; the `Err` half is always a
/// precondition failure (nothing was started), never a run that went badly.
///
/// # Errors
///
/// The ADR-0001 §5.3 precondition codes: `invalid_arguments`, `unknown_argument`, the workspace,
/// resolution and probe codes from the validator, `isolation_failed` (including the scratch-root
/// check this ticket owns), `workspace_busy`, `too_many_runs`, `idempotency_conflict`, and
/// `internal_error` for anything unforeseen before a run existed.
pub async fn call(
    wiring: Arc<Wiring>,
    args: RunWorkflowArgs,
    call: Call,
) -> Result<Value, ToolError> {
    // ── step 3: idempotency replay, before anything expensive ─────────────────────────
    let raw = args.raw_tuple();
    if let Some(key) = args.idempotency_key.as_deref() {
        if let Some(remembered) = wiring.idempotency.recall_raw(key, &raw) {
            if let Some(ticket) = replay(&wiring, key, &remembered.run_id).await {
                return finish(&wiring, &args, &call, &ticket, true).await;
            }
        }
    }

    // ── steps 4–8: the validator (gates, resolution, probes, isolation) ───────────────
    let request = RunRequest::new(
        args.workflow.clone(),
        args.task.clone(),
        args.category.clone(),
        args.commit.clone(),
    );
    let target = Arc::clone(&wiring.validator)
        .validate_async(request)
        .await
        .map_err(|error| ToolError::from_validation(TOOL, &error))?;
    let resolved = ResolvedTuple::from_target(&target);

    // The deferred half of the replay check: the key may be known with a differently-typed
    // spelling of the same request, which only the resolved identity can tell apart from a real
    // conflict (ADR-0001 §8).
    if let Some(key) = args.idempotency_key.as_deref() {
        match wiring.idempotency.recall_identity(key, &raw, &resolved) {
            Some(Replay::Match(remembered)) => {
                if let Some(ticket) = replay(&wiring, key, &remembered.run_id).await {
                    return finish(&wiring, &args, &call, &ticket, true).await;
                }
            }
            Some(Replay::Conflict { remembered }) => {
                return Err(idempotency_conflict(key, &remembered, &resolved));
            }
            None => {}
        }
    }

    // ── `timeout_secs`: the caller's, or ADR-0004 §4.2's per-category default ─────────
    let policy = wiring.supervisor.policy();
    let timeout = match args.timeout_secs {
        Some(secs) => secs,
        None => policy.default_timeout_for(&args.category),
    };
    if timeout > policy.max_timeout {
        return Err(ToolError::invalid_arguments(
            TOOL,
            vec![ArgumentProblem {
                path: "timeout_secs",
                reason: format!(
                    "{timeout:?} is above this server's ceiling of {:?}",
                    policy.max_timeout
                ),
                expected: "1..=--max-timeout-secs seconds (21600 unless the operator lowered it)",
            }],
        ));
    }

    // ── the tree this run measures (ADR-0003 §4/§5) ───────────────────────────────────
    let run_id: RunId = wiring.supervisor.new_run_id();
    let run_scratch = wiring.config.run_scratch.join(run_id.as_str());
    let (tree, default_branch) = match target.mode {
        IsolationMode::HeadOnly => (target.workspace_root.clone(), None),
        IsolationMode::Worktree => {
            // ADR-0003 §4.4 W3, pre-registration: an unwritable scratch root is
            // `isolation_failed`, because nothing has started yet. Once registered, the same
            // failure would be `runner_internal_error` — a worse answer to the same question.
            if let Err(reason) = ensure_scratch_usable(&wiring.config.run_scratch) {
                return Err(isolation_failed_scratch(&target, &reason));
            }
            let branch =
                pipeline::default_branch(&target.workspace_root, wiring.config.git_timeout).await;
            (run_scratch.join(pipeline::WORKTREE_DIR), branch)
        }
    };

    let mut spec = RunSpec::new(
        run_id.clone(),
        wiring.config.act_binary.clone(),
        tree.clone(),
        target.workflow_in(&tree),
        target.job_id.clone(),
    );
    // The evidence root is never rewritten to the tree (ADR-0003 §5.3): durable numbers outlive
    // a worktree that `release` deletes.
    spec.workspace = target.workspace_root.clone();
    spec.evidence_dir = wiring.config.evidence_dir.clone();
    spec.run_dir = wiring
        .config
        .evidence_dir
        .join("runs")
        .join(run_id.as_str());
    spec.scratch_dir = run_scratch;
    spec.payload_dir = default_payload_dir(&tree, run_id.as_str());
    spec.timeout = timeout;
    spec.default_branch = default_branch;
    spec.inputs = args.inputs.clone();
    spec.secrets = args.secrets.clone();
    // `spec.cancellation` stays a fresh, never-armed token on purpose: ADR-0004 §3.5 lets only
    // the run's own task signal, and `RunSupervisor::start` explains the deadlock that
    // arming both cancel paths would create.

    let identity = RunIdentity::new(
        target.workflow_display.clone(),
        target.workflow_relpath.clone(),
        target.job_id.clone(),
        target.category.clone(),
        target.commit_full_sha.clone(),
    );
    // ── steps 9–10: capacity, the workspace lock, `run_id`, registration ─────────────
    let ticket = wiring
        .supervisor
        .start(RunPlan::new(identity, spec))
        .await
        .map_err(start_error)?;

    if let Some(key) = args.idempotency_key.as_deref() {
        wiring
            .idempotency
            .remember(key, raw, resolved, ticket.run_id());
    }
    // Keys must not outlive the runs they point at (ADR-0004 §7.3 evicts runs).
    let evicted = wiring.supervisor.prune().await;
    if !evicted.is_empty() {
        wiring.idempotency.forget_runs(&evicted);
    }
    finish(&wiring, &args, &call, &ticket, false).await
}

/// Resolve a remembered run into something waitable, dropping the key if the run is gone.
///
/// A key whose run has been evicted (or predates a restart) must not answer `run_not_found` —
/// ADR-0001 §8 makes a replay *safe*, and "your key is stale, here is a fresh run" is the safe
/// answer. So the key is forgotten and the caller proceeds as a fresh call.
async fn replay(wiring: &Wiring, key: &str, run_id: &str) -> Option<RunTicket> {
    match wiring.supervisor.ticket(run_id).await {
        Some(ticket) => Some(ticket),
        None => {
            tracing::debug!(
                run_id = %run_id,
                idempotency_key = %key,
                "the remembered run is no longer known; starting a fresh one"
            );
            wiring.idempotency.forget(key);
            None
        }
    }
}

/// Wait (or not) for the run, then render the result.
async fn finish(
    wiring: &Wiring,
    args: &RunWorkflowArgs,
    call: &Call,
    ticket: &RunTicket,
    deduplicated: bool,
) -> Result<Value, ToolError> {
    // ADR-0004 §5.3: a waiting request is registered so a `$/cancel-request` naming *this*
    // request can find the run behind it. Dropping the guard (including by the request future
    // being dropped) removes the mapping and changes nothing about the run — transport death is
    // never cancellation.
    let waiter = wiring
        .supervisor
        .wait_on(&call.request_key, ticket.run_id());
    let record = if args.wait {
        wait_for_terminal(wiring, args, call, ticket).await
    } else {
        ticket.record()
    };
    drop(waiter);
    Ok(render(
        &record,
        args.log_tail_bytes,
        deduplicated,
        &args.category,
    ))
}

/// ADR-0004 §3.4 step 7 from the caller's side: sample the run's snapshots until it is terminal.
///
/// Bounded by [`wait_budget`] rather than by `timeout_secs` alone, because the supervisor's own
/// overhead (pre-start, grace, harvest) sits on top of act's budget and is not a bug. Expiry is
/// not an error: the current snapshot is returned, non-terminal, and `run_status` stays the way
/// to keep watching — the alternative is a tool call that cannot answer at all.
///
/// The snapshot is read from the in-memory record every [`WAIT_TICK`], not from the watch
/// channel: a `watch::Receiver` marks values as seen, so a subscriber that reads the current
/// value and *then* waits can miss a publication that lands in between and sleep until its
/// deadline for a run that finished a minute ago. A 200 ms read of one `Arc` is cheaper than that
/// bug, and nothing about it blocks a worker.
async fn wait_for_terminal(
    wiring: &Wiring,
    args: &RunWorkflowArgs,
    call: &Call,
    ticket: &RunTicket,
) -> Arc<crate::registry::RunRecord> {
    let policy = wiring.supervisor.policy();
    let act_budget = args
        .timeout_secs
        .unwrap_or_else(|| policy.default_timeout_for(&args.category));
    let deadline = tokio::time::Instant::now() + wait_budget(act_budget, policy);
    let mut last: Option<(String, String)> = None;
    let mut cancel_armed = true;
    loop {
        let record = ticket.record();
        if call.progress.is_enabled() {
            let phase = (
                record.state.status().as_str().to_owned(),
                record.state.phase().as_str().to_owned(),
            );
            if last.as_ref() != Some(&phase) {
                last = Some(phase.clone());
                call.progress
                    .phase(ticket.run_id(), phase.0.as_str(), phase.1.as_str())
                    .await;
            }
        }
        if record.state.is_terminal() {
            return record;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            tracing::warn!(
                run_id = %ticket.run_id(),
                budget = ?wait_budget(act_budget, policy),
                "a wait:true call reached its budget before the run's terminal commit; returning \
                 the current snapshot, which run_status will keep updating"
            );
            return record;
        }
        tokio::select! {
            biased;
            // ADR-0001 §4: cancelling the *request* means stop the work. The supervisor's cancel
            // path does it (bounded by `--cancel-wait-secs`); this branch then keeps waiting for
            // the terminal commit the teardown will publish.
            _ = call.cancel.cancelled(), if cancel_armed => {
                cancel_armed = false;
                if wiring
                    .supervisor
                    .cancel_waiting_request(&call.request_key)
                    .await
                    .is_none()
                {
                    // Already terminal by the time the notification landed: ADR-0004 §5.4 makes a
                    // cancel after act exits a reported no-op, so there is nothing to do but say
                    // so in the result.
                    tracing::debug!(
                        run_id = %ticket.run_id(),
                        "cancellation arrived after the run finished"
                    );
                }
            }
            _ = tokio::time::sleep(std::cmp::min(remaining, WAIT_TICK)) => {}
        }
    }
}

/// `run_view` + `deduplicated`, with the two warnings only the tool is allowed to add.
fn render(
    record: &crate::registry::RunRecord,
    log_tail_bytes: usize,
    deduplicated: bool,
    category: &str,
) -> Value {
    let view = record.view(log_tail_bytes);
    let mut value = view.to_value();
    let Some(object) = value.as_object_mut() else {
        return value;
    };
    let mut warnings: Vec<String> = record.warnings.clone();
    for code in tool_warnings(&view, category) {
        if !warnings.iter().any(|held| held == code) {
            warnings.push(code.to_owned());
        }
    }
    // ADR-0001 §9 caps `warnings` at 64 unique entries; the record caps them too, and these two
    // additions must not be the thing that breaks the cap.
    warnings.truncate(64);
    object.insert(
        "warnings".to_owned(),
        Value::Array(warnings.into_iter().map(Value::String).collect()),
    );
    object.insert("deduplicated".to_owned(), Value::Bool(deduplicated));
    value
}

/// The warnings this tool stamps, which no lower layer can know:
///
/// * `unknown_category` — the honest "we wrote nothing, and here is why" (ADR-0001 §2.4). Added
///   even for a `wait: false` or a pre-spawn cancel, where the pipeline never ran to add it.
/// * `exit_nonzero` — the contract's own example of a failed run carries it (docs §3.3): the
///   status already says `failed`, and this makes the *reason* greppable.
fn tool_warnings(view: &crate::registry::RunView, category: &str) -> Vec<&'static str> {
    let mut codes = Vec::new();
    if let Some(code) = pipeline::unknown_category_warning(category) {
        codes.push(code);
    }
    if view.status == WireStatus::Failed {
        codes.push(WARNING_EXIT_NONZERO);
    }
    codes
}

/// Warning stamped on a run whose act exited non-zero (ADR-0001 §5.4's vocabulary).
pub const WARNING_EXIT_NONZERO: &str = "exit_nonzero";

/// A `StartError` becomes the tool error it names: the codes and `retryable` column are already
/// in the supervisor's table (ADR-0001 §5.3), so this is a copy, not a decision.
fn start_error(error: StartError) -> ToolError {
    let (code, retryable) = error.code();
    ToolError::new(TOOL, code, error.message(), retryable, error.detail())
}

/// `idempotency_conflict` (ADR-0001 §5.3: `detail.idempotency_key`, `detail.run_id`,
/// `detail.previous`, `detail.requested`).
fn idempotency_conflict(
    key: &str,
    remembered: &idempotency::Remembered,
    requested: &ResolvedTuple,
) -> ToolError {
    let mut detail = Map::new();
    detail.insert("idempotency_key".to_owned(), json!(key));
    detail.insert("run_id".to_owned(), json!(remembered.run_id));
    detail.insert("previous".to_owned(), remembered.resolved.to_value());
    detail.insert("requested".to_owned(), requested.to_value());
    ToolError::new(
        TOOL,
        "idempotency_conflict",
        format!(
            "idempotency_key `{key}` already belongs to run {}, which asked for a different \
             (workflow, task, category, commit); a key means one request for as long as that run \
             is remembered",
            remembered.run_id
        ),
        false,
        detail,
    )
}

/// ADR-0003 §4.4 W3: is the scratch root actually usable?
///
/// `mkdir -p` then a write probe: "the directory exists" and "a worktree can be created in it"
/// are different claims on a read-only or full filesystem, and this is the last moment where the
/// answer can still be `isolation_failed` rather than a run that died.
fn ensure_scratch_usable(scratch_root: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(scratch_root).map_err(|reason| reason.to_string())?;
    let probe = scratch_root.join(".act-mcp-write-probe");
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(reason) => Err(reason.to_string()),
    }
}

/// The `isolation_failed` payload for an unusable scratch root, shaped like the validator's own
/// (`detail.mode: "worktree"`, `reason: "scratch_unwritable"`).
fn isolation_failed_scratch(
    target: &crate::workspace::ValidatedRunTarget,
    reason: &str,
) -> ToolError {
    let mut detail = Map::new();
    detail.insert("mode".to_owned(), json!(target.mode.as_str()));
    detail.insert("requested".to_owned(), json!(target.commit_full_sha));
    detail.insert("head".to_owned(), Value::Null);
    detail.insert("reason".to_owned(), json!("scratch_unwritable"));
    detail.insert("os_error".to_owned(), json!(reason));
    detail.insert(
        "path".to_owned(),
        json!(crate::runner::default_run_scratch_dir()
            .display()
            .to_string()),
    );
    ToolError::new(
        TOOL,
        "isolation_failed",
        format!(
            "the run scratch root is not writable, so no tree could be prepared for {}: {reason}",
            target.commit_full_sha
        ),
        true,
        detail,
    )
}

/// `inputs` / `secrets`: an object of string→string within the caps of ADR-0001 §9.
///
/// A secret's *value* is never quoted back — the key is an identifier and safe to name, the value
/// is the thing the caller trusted us with.
fn parse_map(
    value: Option<&Value>,
    path: &'static str,
    max_value_bytes: usize,
    problems: &mut Vec<ArgumentProblem>,
) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    let Some(value) = value else {
        return map;
    };
    let Value::Object(entries) = value else {
        problems.push(ArgumentProblem {
            path,
            reason: format!(
                "expected an object of string to string, got {}",
                kind_of(value)
            ),
            expected: "an object mapping names to string values",
        });
        return map;
    };
    if entries.len() > MAX_MAP_ENTRIES {
        problems.push(ArgumentProblem {
            path,
            reason: format!(
                "{} entries, over the {MAX_MAP_ENTRIES}-entry cap",
                entries.len()
            ),
            expected: "at most 32 entries",
        });
    }
    for (key, value) in entries {
        if !is_env_key(key) {
            problems.push(ArgumentProblem {
                path,
                reason: format!("`{key}` is not a name this map accepts"),
                expected: "a key matching `^[A-Za-z_][A-Za-z0-9_]{0,127}$`",
            });
            continue;
        }
        let Some(text) = value.as_str() else {
            problems.push(ArgumentProblem {
                path,
                reason: format!("`{key}` is not a string"),
                expected: "every value must be a string",
            });
            continue;
        };
        if text.len() > max_value_bytes {
            // Size only: the value stays unquoted, which is the whole point of `secrets`.
            problems.push(ArgumentProblem {
                path,
                reason: format!(
                    "`{key}` is {} bytes, over the {max_value_bytes}-byte cap",
                    text.len()
                ),
                expected: "a value within the per-map byte cap",
            });
            continue;
        }
        map.insert(key.clone(), text.to_owned());
    }
    map
}

/// `^[A-Za-z_][A-Za-z0-9_]{0,127}$` — the one name rule `inputs`, `secrets` and act's `--env` share.
fn is_env_key(key: &str) -> bool {
    let bytes = key.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && (bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
        && bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
}

/// `^[0-9a-fA-F]{7,40}$` — the abbreviation rule ADR-0001 §2.5 states, the schema advertises,
/// and `crate::workspace` re-checks before it ever asks git.
fn is_commit_abbreviation(value: &str) -> bool {
    (7..=40).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// `^[A-Za-z0-9._:-]{1,64}$`.
fn is_idempotency_key(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_IDEMPOTENCY_KEY_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
}

fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// The advertised `inputSchema` (the resolved `$defs/run_workflow_args`).
#[must_use]
pub fn input_schema() -> Arc<JsonObject> {
    let schema = json!({
        "description": "tools/call params.arguments for run_workflow. All four identity fields are \
                        required; nothing else may be supplied except the listed optionals.",
        "type": "object",
        "additionalProperties": false,
        "required": ["workflow", "task", "category", "commit"],
        "properties": {
            "workflow": schemas::workflow_name(),
            "task": schemas::task_id(),
            "category": schemas::category(),
            "commit": schemas::commit_input(),
            "wait": {
                "description": "true (default): block until the run reaches a terminal state or \
                                timeout_secs expires, whichever comes first. false: return as soon \
                                as the run is registered, status running. A run outlives the \
                                request either way.",
                "type": "boolean",
                "default": true
            },
            "timeout_secs": {
                "description": "Wall-clock budget for the act run and therefore for the blocking \
                                wait: at expiry the act process group is killed as a group and the \
                                run ends 'timed_out'. Defaults are per category and owned by \
                                ADR-0004 (v1: coverage 900, benchmark 1800, anything else 1800).",
                "type": "integer",
                "minimum": 1,
                "maximum": MAX_TIMEOUT_SECS
            },
            "log_tail_bytes": schemas::log_tail_bytes(),
            "inputs": schemas::input_map(),
            "secrets": schemas::secret_map(),
            "idempotency_key": schemas::idempotency_key()
        }
    });
    arc_object(schema)
}

/// The advertised `outputSchema` (the resolved `$defs/run_workflow_result`: `run_view` plus
/// `deduplicated`).
///
/// Spell-checked against the pinned file by `tests/tool_schema_parity.rs`, which is also what
/// keeps the `status`/`phase` word lists and the terminal-invariants `allOf` from drifting.
#[must_use]
pub fn output_schema() -> Arc<JsonObject> {
    let schema = json!({
        "description": "structuredContent of a successful (isError:false) run_workflow call: \
                        run_view plus 'deduplicated'.",
        "type": "object",
        "additionalProperties": false,
        "required": [
            "run_id",
            "status",
            "terminal",
            "deduplicated",
            "workflow",
            "workflow_file",
            "task",
            "category",
            "commit",
            "evidence_files",
            "evidence_record_count",
            "log_tail",
            "warnings"
        ],
        "properties": {
            "run_id": schemas::run_id(),
            "status": schemas::status(),
            "terminal": { "type": "boolean" },
            "deduplicated": {
                "description": "True when this result is a remembered run found through \
                                idempotency_key rather than a run started by this call.",
                "type": "boolean"
            },
            "phase": schemas::phase(),
            "workflow": schemas::workflow_identity(),
            "workflow_file": schemas::relpath(),
            "task": schemas::task_id(),
            "category": schemas::category(),
            "commit": schemas::commit_full(),
            "started_at": schemas::timestamp(),
            "finished_at": schemas::timestamp(),
            "duration_ms": { "type": "integer", "minimum": 0, "maximum": 259200000 },
            "exit_code": { "type": "integer", "minimum": 0, "maximum": 255 },
            "evidence_files": {
                "type": "array",
                "maxItems": 8,
                "items": schemas::relpath()
            },
            "evidence_record_count": {
                "type": "integer",
                "minimum": 0,
                "maximum": 1000000
            },
            "log_tail": schemas::log_tail(),
            "warnings": schemas::warnings(),
            "error": schemas::error()
        },
        "allOf": schemas::run_view_invariants()
    });
    arc_object(schema)
}

fn arc_object(schema: Value) -> Arc<JsonObject> {
    Arc::new(
        schema
            .as_object()
            .expect("the literal run_workflow schema is an object")
            .clone(),
    )
}

/// The `run_not_found` half of a replay that raced an eviction — kept public so the control
/// tools and this one agree on the wording.
#[must_use]
pub fn run_not_found(not_found: &RunNotFound) -> ToolError {
    ToolError::new(
        TOOL,
        "run_not_found",
        not_found.message(),
        false,
        not_found.detail(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn arguments(value: Value) -> JsonObject {
        value.as_object().expect("object arguments").clone()
    }

    /// The smallest legal call: the four required identity fields, nothing optional.
    fn base() -> Value {
        json!({
            "workflow": "ci",
            "task": "build",
            "category": "coverage",
            "commit": "deadbeef1",
        })
    }

    fn failure(value: Value) -> ToolError {
        RunWorkflowArgs::parse(Some(&arguments(value)))
            .expect_err("these arguments must be refused")
    }

    /// The `detail.errors[]` paths a failure reported.
    fn paths(error: &ToolError) -> Vec<String> {
        error.structured_content()["error"]["detail"]["errors"]
            .as_array()
            .expect("invalid_arguments always carries an errors array")
            .iter()
            .map(|problem| {
                problem["path"]
                    .as_str()
                    .expect("every problem names its path")
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn defaults_are_the_contracts_defaults() {
        let args =
            RunWorkflowArgs::parse(Some(&arguments(base()))).expect("the base call is legal");
        assert!(
            args.wait,
            "ADR-0001 §2.6: a call waits unless it says otherwise"
        );
        assert_eq!(args.log_tail_bytes, DEFAULT_LOG_TAIL_BYTES);
        assert_eq!(
            args.timeout_secs, None,
            "the per-category default is the supervisor's to apply"
        );
        assert!(args.inputs.is_empty() && args.secrets.is_empty());
        assert_eq!(args.idempotency_key, None);
    }

    #[test]
    fn every_missing_field_is_reported_at_once() {
        let error = failure(json!({}));
        assert_eq!(error.code(), "invalid_arguments");
        for required in ["workflow", "task", "category", "commit"] {
            assert!(
                paths(&error).contains(&required.to_owned()),
                "one round trip must report all four: {error:?}"
            );
        }
    }

    #[test]
    fn a_caller_supplied_run_id_is_refused_by_name() {
        let mut value = base();
        value["run_id"] = json!("20261110T000000Z-deadbe");
        let error = failure(value);
        assert_eq!(error.code(), "unknown_argument");
        assert_eq!(
            error.structured_content()["error"]["detail"]["unexpected"],
            json!(["run_id"]),
            "the caller has to be told which key it invented"
        );
    }

    #[test]
    fn shapes_are_checked_before_anything_is_looked_up() {
        for (value, path) in [
            (
                json!({"task": 3, "workflow": "ci", "category": "coverage", "commit": "deadbeef1"}),
                "task",
            ),
            (
                json!({"workflow": "ci", "task": "build", "category": "Coverage", "commit": "deadbeef1"}),
                "category",
            ),
            (
                json!({"workflow": "ci", "task": "build", "category": "coverage", "commit": "deadbe"}),
                "commit",
            ),
            (
                json!({"workflow": "ci", "task": "build", "category": "coverage", "commit": "deadbeefz"}),
                "commit",
            ),
            (
                json!({"workflow": "../escape", "task": "build", "category": "coverage", "commit": "deadbeef1"}),
                "workflow",
            ),
            (
                json!({"workflow": "ci", "task": "build", "category": "coverage", "commit": "deadbeef1", "wait": "yes"}),
                "wait",
            ),
        ] {
            let error = failure(value.clone());
            assert_eq!(error.code(), "invalid_arguments", "{value} must be refused");
            assert!(
                paths(&error).contains(&path.to_owned()),
                "{value} must be reported as `{path}`"
            );
        }
    }

    #[test]
    fn bounds_are_enforced_not_clamped() {
        for (value, path) in [
            (json!({"timeout_secs": 0}), "timeout_secs"),
            (
                json!({"timeout_secs": MAX_TIMEOUT_SECS + 1}),
                "timeout_secs",
            ),
            (json!({"timeout_secs": 1.5}), "timeout_secs"),
            (json!({"log_tail_bytes": 65537}), "log_tail_bytes"),
            (json!({"log_tail_bytes": -1}), "log_tail_bytes"),
            (json!({"idempotency_key": ""}), "idempotency_key"),
            (
                json!({"idempotency_key": "x".repeat(65)}),
                "idempotency_key",
            ),
            (json!({"idempotency_key": "has space"}), "idempotency_key"),
        ] {
            let mut arguments = base();
            for (key, value) in value.as_object().expect("object") {
                arguments[key.clone()] = value.clone();
            }
            let error = failure(arguments);
            assert_eq!(error.code(), "invalid_arguments");
            assert!(
                paths(&error).contains(&path.to_owned()),
                "{value:?} must be reported as `{path}`"
            );
        }
        // The bounds are inclusive at the legal end.
        let legal_arguments = json!({
            "workflow": "ci", "task": "build", "category": "coverage", "commit": "deadbeef1",
            "timeout_secs": MAX_TIMEOUT_SECS, "log_tail_bytes": 0,
            "idempotency_key": "A.b:c-d_9",
        });
        let legal = RunWorkflowArgs::parse(Some(&arguments(legal_arguments)))
            .expect("the inclusive edges are legal");
        assert_eq!(
            legal.timeout_secs,
            Some(Duration::from_secs(MAX_TIMEOUT_SECS))
        );
        assert_eq!(legal.log_tail_bytes, 0);
    }

    #[test]
    fn a_secret_value_is_counted_and_never_quoted_back() {
        let value =
            base_call_with(json!({"secrets": {"TOKEN": "s".repeat(MAX_SECRET_VALUE_BYTES + 1)}}));
        let error = failure(value);
        let payload = error.structured_content().to_string();
        assert_eq!(error.code(), "invalid_arguments");
        assert!(paths(&error).contains(&"secrets".to_owned()));
        assert!(
            !payload.contains("sssss"),
            "an over-long secret must be described by size, never echoed: {payload}"
        );

        // A non-string value is named by key, not by value.
        let error = failure(base_call_with(json!({"secrets": {"TOKEN": 42}})));
        assert!(!error.structured_content().to_string().contains("42"));

        // A key that is not an identifier, and a map that is not a map.
        assert!(
            paths(&failure(base_call_with(json!({"secrets": {"9X": "x"}}))))
                .contains(&"secrets".to_owned())
        );
        assert!(
            paths(&failure(base_call_with(json!({"inputs": "profile=ci"}))))
                .contains(&"inputs".to_owned())
        );

        // The entry cap is a cap, not a performance hint.
        let mut many = serde_json::Map::new();
        for index in 0..=MAX_MAP_ENTRIES {
            many.insert(format!("KEY_{index}"), json!("v"));
        }
        let error = failure(base_call_with(json!({"inputs": many})));
        assert!(paths(&error).contains(&"inputs".to_owned()));
    }

    fn base_call_with(extra: Value) -> Value {
        let mut value = base();
        let (key, map) = extra
            .as_object()
            .expect("one key")
            .iter()
            .next()
            .expect("one key");
        value[key.clone()] = map.clone();
        value
    }

    #[test]
    fn a_name_must_be_an_identifier() {
        assert!(is_env_key("_A9_b"));
        assert!(!is_env_key(""));
        assert!(!is_env_key("9x"));
        assert!(!is_env_key("a-b"));
        assert!(is_env_key(&"a".repeat(128)));
        assert!(!is_env_key(&"a".repeat(129)));
        assert!(is_idempotency_key("2026-11-10T00:00:00Z:build"));
        assert!(!is_idempotency_key("build;rm -rf /"));
    }

    #[test]
    fn the_raw_tuple_normalizes_only_the_commit() {
        let tuple = RawTuple::new("CI", "build", "coverage", "DEADBEEF1");
        assert_eq!(
            tuple.commit, "deadbeef1",
            "hex is case-insensitive by definition"
        );
        assert_eq!(
            tuple.workflow, "CI",
            "a workflow name is case sensitive (ADR-0001 §2.2)"
        );
        assert_eq!(tuple.category, "coverage");
    }

    #[test]
    fn the_wait_budget_is_act_s_budget_plus_the_supervisors_own() {
        let policy = crate::registry::RunPolicy::default();
        let act = Duration::from_secs(60);
        let budget = wait_budget(act, &policy);
        assert!(budget > act + policy.prestart_timeout + policy.term_grace);
        // The ADR's "+300 s at defaults" is this composition, not a constant: prestart 120 +
        // grace 30 + harvest 120 + 60 of slack.
        assert_eq!(
            budget,
            act + policy.prestart_timeout
                + policy.term_grace
                + policy.harvest_timeout
                + Duration::from_secs(60)
        );
    }

    #[test]
    fn the_advertised_schemas_are_the_ones_the_contract_pins() {
        // Cheap guard against a schema function being edited into something the tool router
        // rejects (a non-object, or an object root that is not an object).
        for schema in [input_schema(), output_schema()] {
            assert_eq!(schema.get("type"), Some(&json!("object")));
            assert_eq!(schema.get("additionalProperties"), Some(&json!(false)));
        }
    }
}
