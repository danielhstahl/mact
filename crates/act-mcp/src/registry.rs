//! The run supervisor: who owns an act run, and how a caller talks to one it cannot see.
//!
//! ```text
//!   run_workflow (.14)          run_status / cancel_run (.11)
//!        │ register(plan)                │ status / list / cancel
//!        ▼                               ▼
//!   ┌────────────────────── RunSupervisor ──────────────────────┐
//!   │ runs: RwLock<HashMap<RunId, Slot>>   workspaces: Mutex<…> │
//!   │ policy: RunPolicy                    shutdown: token       │
//!   └───────┬──────────────────────────────────────────┬─────────┘
//!           │ one detached task per run                │ watch<Arc<RunRecord>>
//!           ▼                                          ▼
//!   registry::task::drive                      waiters (wait:true) and pollers
//!   prepare → exec act → wait → harvest → publish → meta.json
//! ```
//!
//! This is ADR-0004 option B made concrete: **a run belongs to the supervisor, never to the
//! request that started it.** `wait: true` is a subscription to a run's terminal transition
//! and nothing more, so closing the HTTP connection removes a waiter and changes nothing
//! else (§5.3's "transport death is never cancellation", and the ticket's headline test).
//!
//! ## The four things this module owns
//!
//! 1. **The state machine.** `RunState` and its wire projection live in `registry::state`; a
//!    run's task
//!    is the only writer, and every transition is logged at `info` (ADR-0004 §17.1's rules).
//! 2. **Single-flight** (ADR-0003 §6): the supervisor is the only code that takes the
//!    workspace lock, it takes it at registration and releases it at §3.4 step 6, and a
//!    second run in a locked workspace is answered `workspace_busy` in microseconds — there
//!    is no queue (§8), so `phase: "queued"` means "pre-spawn", never "waiting for a turn".
//! 3. **The budgets** (§11): the pre-start budget, the harvest budget, `--cancel-wait-secs`
//!    and the shutdown grace are enforced here; the act budget (`timeout_secs`) and the
//!    process-group teardown belong to the runner (mact-f1w.8), which is why this module
//!    never sends a signal itself.
//! 4. **Retention** (§7.2/§7.3): run state is memory-only, bounded by count and age, and
//!    `.evidence/runs/<run_id>/` is never pruned here. An evicted id answers
//!    `run_not_found`, exactly like one from before a restart — the durable answer to
//!    "what happened" is the run's own directory, which `run_status` deliberately does not
//!    read (it must stay non-blocking).
//!
//! ## What the wiring ticket (mact-f1w.14) plugs in
//!
//! [`crate::registry::Pipeline`] is the seam: tree materialisation (ADR-0003 worktrees), the category
//! extractors, and per-run cleanup. The supervisor owns ordering, budgets and reporting;
//! the pipeline owns the category-specific knowledge. [`crate::registry::NoEvidence`] is the
//! default and
//! produces no evidence, which is also the correct answer for an unknown category.

mod lock;
mod state;
mod task;

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use thiserror::Error;
use tokio::sync::{watch, RwLock};
use tokio_util::sync::CancellationToken;

use crate::runner::{RunId, RunSpec, Runner, RunnerLimits};

pub use lock::{
    canonical_workspace, lock_path, process_is_alive, read_body, Acquisition, HeldLock, LockBody,
    LockUnavailable, LOCK_FILE_NAME, UNREADABLE,
};
pub use state::{
    CancelReceipt, Phase, RunError, RunIdentity, RunRecord, RunState, RunSummary, RunView,
    WireStatus, WARNING_ALREADY_TERMINAL, WARNING_PARTIAL_EVIDENCE_DROPPED,
    WARNING_PAYLOAD_CAP_EXCEEDED,
};
/// The boxed-future alias the [`Pipeline`] trait returns, exported so an out-of-module pipeline
/// (mact-f1w.14's) can name the return type without depending on the private `task` module.
pub use task::BoxFuture;
pub use task::{Harvest, NoEvidence, Pipeline, PipelineError, RunContext};

/// ADR-0004 §11: `--max-concurrent-runs`, a limit and never a semaphore anyone waits on.
pub const DEFAULT_MAX_CONCURRENT_RUNS: usize = 2;
/// ADR-0004 §11: `--prestart-timeout-secs`.
pub const DEFAULT_PRESTART_TIMEOUT_SECS: u64 = 120;
/// ADR-0004 §11: `--harvest-timeout-secs`.
pub const DEFAULT_HARVEST_TIMEOUT_SECS: u64 = 120;
/// ADR-0004 §11: `--cancel-wait-secs`, and its upper bound.
pub const DEFAULT_CANCEL_WAIT_SECS: u64 = 30;
/// ADR-0004 §11: `--cancel-wait-secs` upper bound.
pub const MAX_CANCEL_WAIT_SECS: u64 = 300;
/// ADR-0004 §11: `--shutdown-grace-secs`.
pub const DEFAULT_SHUTDOWN_GRACE_SECS: u64 = 30;
/// ADR-0004 §11: `--run-retention-count`.
pub const DEFAULT_RUN_RETENTION_COUNT: usize = 200;
/// ADR-0004 §11: `--run-retention-mins`.
pub const DEFAULT_RUN_RETENTION_MINS: u64 = 120;
/// ADR-0004 §7.3/§11: `run_status`'s listing cap, which the retention count may never
/// undercut (otherwise a full page would imply an eviction that never happened).
pub const LISTING_CAP: usize = 50;
/// ADR-0004 §7.4: `run_not_found`'s `detail.known_runs` is the newest 20 ids at most.
pub const KNOWN_RUNS_CAP: usize = 20;
/// ADR-0004 §4.2/§17.2: the per-category `timeout_secs` defaults.
pub const DEFAULT_COVERAGE_TIMEOUT_SECS: u64 = 900;
/// ADR-0004 §17.2: benchmark and every unknown category.
pub const DEFAULT_BENCHMARK_TIMEOUT_SECS: u64 = 1800;
/// ADR-0004 §17.2: `--max-timeout-secs` default ceiling.
pub const DEFAULT_MAX_TIMEOUT_SECS: u64 = 21_600;
/// ADR-0004 §4.3: the compiled outer limit a ceiling may be raised to.
pub const COMPILED_MAX_TIMEOUT_SECS: u64 = 86_400;

/// Every budget this supervisor enforces, with ADR-0004 §11's defaults.
///
/// The runner-owned rows (`--term-grace-secs`, the two log caps) are here too because the
/// supervisor is what builds the [`Runner`]: one place maps CLI numbers into
/// [`RunnerLimits`] and its schema bounds, rather than two.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunPolicy {
    /// `--max-concurrent-runs`: server-wide act processes, across workspaces.
    pub max_concurrent_runs: usize,
    /// `--prestart-timeout-secs`: registration → `exec` (ADR-0004 §4.4).
    pub prestart_timeout: Duration,
    /// `--harvest-timeout-secs`: extraction + publish (§4.5).
    pub harvest_timeout: Duration,
    /// `--term-grace-secs`: SIGTERM before SIGKILL (§5.1; enforced by the runner).
    pub term_grace: Duration,
    /// `--cancel-wait-secs`: how long `cancel_run` may block for the terminal commit (§5.2).
    pub cancel_wait: Duration,
    /// `--shutdown-grace-secs`: §9.2's step-3 window.
    pub shutdown_grace: Duration,
    /// `--run-retention-count`: finished runs kept in memory (§7.3).
    pub retention_count: usize,
    /// `--run-retention-mins`: the age at which a finished run becomes evictable.
    pub retention_age: Duration,
    /// `--log-tail-ring_bytes`, equal to `log_tail_bytes`'s maximum (§7.1).
    pub log_tail_ring_bytes: usize,
    /// `--max-run-log-bytes`, the on-disk `act.jsonl` cap.
    pub max_run_log_bytes: u64,
    /// `--max-timeout-secs`: the effective ceiling for a caller's `timeout_secs` **and**
    /// for every per-category default (§4.3).
    pub max_timeout: Duration,
    /// mact-f1w.13 `--max-run-dirs`: how many `.evidence/runs/<run_id>/` directories may
    /// exist at once.
    ///
    /// Run retention (§7.3) prunes *memory*, and the epic's own rule is that a published
    /// file is never deleted, so nothing else bounds the disk footprint of a long-lived
    /// server. Past the cap a new run is refused with
    /// [`StartError::TooManyRunDirs`] — a typed, explained refusal instead of a machine with
    /// a full filesystem and a run that died halfway.
    pub max_run_dirs: usize,
    /// `--max-payload-keep-bytes`: bytes of job-produced payload kept per run under
    /// `.evidence/runs/<run_id>/payload/` after it has been read.
    ///
    /// The payload a job writes through the bind mount is what a coverage or benchmark
    /// record was computed from, and `source_path` names a scratch file that is otherwise
    /// gone by the time the record is read. Keeping it beside the run makes the record
    /// checkable after the fact; the budget is what stops a 200 MB lcov from being kept for
    /// every run of a long-lived server. `0` keeps none, and the run deletes what it read.
    ///
    /// Retention moves the files *into* `runs/`, which is the directory `max_run_dirs`
    /// pruning already reclaims — so this budget is bounded per run *and* per server.
    pub max_payload_keep_bytes: u64,
    /// `--default-timeout-secs`: per-category overrides; `'*'` is the fallback (§4.3).
    pub default_timeouts: HashMap<String, Duration>,
}

impl Default for RunPolicy {
    fn default() -> Self {
        let mut default_timeouts = HashMap::new();
        default_timeouts.insert(
            "coverage".to_owned(),
            Duration::from_secs(DEFAULT_COVERAGE_TIMEOUT_SECS),
        );
        default_timeouts.insert(
            "benchmark".to_owned(),
            Duration::from_secs(DEFAULT_BENCHMARK_TIMEOUT_SECS),
        );
        default_timeouts.insert(
            "*".to_owned(),
            Duration::from_secs(DEFAULT_BENCHMARK_TIMEOUT_SECS),
        );
        Self {
            max_concurrent_runs: DEFAULT_MAX_CONCURRENT_RUNS,
            prestart_timeout: Duration::from_secs(DEFAULT_PRESTART_TIMEOUT_SECS),
            harvest_timeout: Duration::from_secs(DEFAULT_HARVEST_TIMEOUT_SECS),
            term_grace: Duration::from_secs(crate::runner::DEFAULT_TERM_GRACE_SECS),
            cancel_wait: Duration::from_secs(DEFAULT_CANCEL_WAIT_SECS),
            shutdown_grace: Duration::from_secs(DEFAULT_SHUTDOWN_GRACE_SECS),
            retention_count: DEFAULT_RUN_RETENTION_COUNT,
            retention_age: Duration::from_secs(DEFAULT_RUN_RETENTION_MINS * 60),
            log_tail_ring_bytes: crate::runner::DEFAULT_LOG_TAIL_RING_BYTES,
            max_run_log_bytes: crate::runner::DEFAULT_MAX_RUN_LOG_BYTES,
            max_timeout: Duration::from_secs(DEFAULT_MAX_TIMEOUT_SECS),
            max_run_dirs: crate::config::DEFAULT_MAX_RUN_DIRS,
            max_payload_keep_bytes: crate::config::DEFAULT_MAX_PAYLOAD_KEEP_BYTES,
            default_timeouts,
        }
    }
}

impl RunPolicy {
    /// Check an assembled policy against §11's bounds. See [`PolicyError`] for what each
    /// bound protects; every one of them is a schema bound or a "no flag may produce a
    /// result the contract rejects" case.
    ///
    /// # Errors
    ///
    /// [`PolicyError`] on the first bound violated.
    pub fn validate(&self) -> Result<(), PolicyError> {
        if self.max_concurrent_runs == 0 {
            return Err(PolicyError::MaxConcurrentRuns);
        }
        if self.prestart_timeout.is_zero() {
            return Err(PolicyError::PrestartTimeout);
        }
        if self.harvest_timeout.is_zero() {
            return Err(PolicyError::HarvestTimeout);
        }
        let grace = self.term_grace.as_secs();
        if !(crate::runner::MIN_TERM_GRACE_SECS..=crate::runner::MAX_TERM_GRACE_SECS)
            .contains(&grace)
        {
            return Err(PolicyError::TermGrace {
                value: self.term_grace,
            });
        }
        let wait = self.cancel_wait.as_secs();
        if wait > MAX_CANCEL_WAIT_SECS {
            return Err(PolicyError::CancelWait {
                value: self.cancel_wait,
            });
        }
        // §11's interaction rule: a `cancel_run` that gives up before the grace expires
        // would answer `terminal: false` on every cancel of a stubborn act.
        if self.cancel_wait < self.term_grace {
            return Err(PolicyError::CancelWaitBelowGrace {
                cancel_wait: self.cancel_wait,
                term_grace: self.term_grace,
            });
        }
        if self.shutdown_grace.is_zero() {
            return Err(PolicyError::ShutdownGrace);
        }
        if self.retention_count < LISTING_CAP {
            return Err(PolicyError::RetentionCount {
                value: self.retention_count,
            });
        }
        if self.retention_age.is_zero() {
            return Err(PolicyError::RetentionAge);
        }
        let ceiling = self.max_timeout.as_secs();
        if ceiling == 0 || ceiling > COMPILED_MAX_TIMEOUT_SECS {
            return Err(PolicyError::MaxTimeout {
                value: self.max_timeout,
            });
        }
        // 1 rather than 0: a cap of 0 would refuse every run, including the first one ever.
        if self.max_run_dirs == 0 {
            return Err(PolicyError::MaxRunDirs);
        }
        for (category, timeout) in &self.default_timeouts {
            if timeout.is_zero() || timeout > &self.max_timeout {
                return Err(PolicyError::DefaultTimeout {
                    category: category.clone(),
                    value: *timeout,
                    ceiling: self.max_timeout,
                });
            }
        }
        Ok(())
    }

    /// The limits handed to the runner, validated by the runner itself (it owns the
    /// `kill.took_ms` bound, so it re-checks the combination).
    ///
    /// # Errors
    ///
    /// [`RunnerLimits`] rejects a grace/log-cap combination that could overflow
    /// `kill.took_ms`'s schema bound.
    pub fn runner_limits(&self) -> Result<RunnerLimits, crate::runner::LimitError> {
        RunnerLimits::checked(
            self.term_grace,
            self.log_tail_ring_bytes,
            self.max_run_log_bytes,
        )
    }

    /// The effective `timeout_secs` for a `category` (§4.2's table, §4.3's overrides).
    #[must_use]
    pub fn default_timeout_for(&self, category: &str) -> Duration {
        self.default_timeouts
            .get(category)
            .or_else(|| self.default_timeouts.get("*"))
            .copied()
            .unwrap_or(Duration::from_secs(DEFAULT_BENCHMARK_TIMEOUT_SECS))
    }

    /// The `timeout_secs` bound to advertise, which is the effective ceiling (§4.3: a
    /// client must learn the truth from `tools/list`, never discover it at call time).
    #[must_use]
    pub fn timeout_bound(&self) -> Duration {
        self.max_timeout
    }
}

/// Reasons a [`RunPolicy`] is unusable. Each variant names the bound and why it exists.
#[allow(missing_docs, reason = "the #[error] string documents each variant")]
#[derive(Debug, Error)]
pub enum PolicyError {
    #[error("--max-concurrent-runs must be at least 1")]
    MaxConcurrentRuns,
    #[error("--prestart-timeout-secs must be at least 1")]
    PrestartTimeout,
    #[error("--harvest-timeout-secs must be at least 1")]
    HarvestTimeout,
    #[error("--term-grace-secs {value:?} is out of range (1..=240 s: kill.took_ms is schema-bounded at 300000 ms)")]
    TermGrace { value: Duration },
    #[error("--cancel-wait-secs {value:?} exceeds the 300 s bound")]
    CancelWait { value: Duration },
    #[error("--cancel-wait-secs {cancel_wait:?} must be >= --term-grace-secs {term_grace:?}, or every cancel of a stubborn act answers terminal:false")]
    CancelWaitBelowGrace {
        cancel_wait: Duration,
        term_grace: Duration,
    },
    #[error("--shutdown-grace-secs must be at least 1")]
    ShutdownGrace,
    #[error("--run-retention-count {value} may not be below the run_status listing cap ({LISTING_CAP}), or a full page would imply an eviction that did not happen")]
    RetentionCount { value: usize },
    #[error("--run-retention-mins must be at least 1")]
    RetentionAge,
    #[error("--max-timeout-secs {value:?} is out of range (1..={COMPILED_MAX_TIMEOUT_SECS} s compiled limit)")]
    MaxTimeout { value: Duration },
    #[error("--max-run-dirs must be at least 1; 0 would refuse every run before it started")]
    MaxRunDirs,
    #[error("--default-timeout-secs {category}={value:?} is not inside 1..=--max-timeout-secs ({ceiling:?})")]
    DefaultTimeout {
        category: String,
        value: Duration,
        ceiling: Duration,
    },
}

/// One run, fully described: the identity the wire reports plus the [`RunSpec`] the runner
/// spawns. mact-f1w.14 builds this after validation and isolation; the supervisor never
/// resolves a workflow, a commit or a tree.
///
/// The `run_id` comes from [`RunId::generate`] and must already be inside `spec` (in
/// `spec.run_id`, `spec.run_dir` and the payload dir), because the ADR-0002 path layout is
/// keyed by it.
#[derive(Debug, Clone)]
pub struct RunPlan {
    /// The identity `run_view` reports.
    pub identity: RunIdentity,
    /// What to spawn, with the paths already derived from `spec.run_id`.
    pub spec: RunSpec,
    /// Warning codes the *gate* decided before the run existed, stamped onto the record as
    /// soon as it is registered.
    ///
    /// These are codes the validator knew at resolution time — `isolation_worktree`,
    /// `dirty_worktree`, a runner label with no image — which nothing downstream can
    /// re-derive: by the time the pipeline runs, the tree is just a directory and the act
    /// child has its own reasons. ADR-0003 §5.4 says the run *carries* `isolation_worktree`,
    /// and this field is the carrying.
    pub warnings: Vec<String>,
}

impl RunPlan {
    /// Pair an identity with a spec.
    #[must_use]
    pub fn new(identity: RunIdentity, spec: RunSpec) -> Self {
        Self {
            identity,
            spec,
            warnings: Vec::new(),
        }
    }

    /// Attach the gate's warning codes to the plan (see [`RunPlan::warnings`]).
    #[must_use]
    pub fn carrying(mut self, warnings: &[String]) -> Self {
        self.warnings = warnings.to_vec();
        self
    }

    /// The run's id.
    #[must_use]
    pub fn run_id(&self) -> &str {
        self.spec.run_id.as_str()
    }
}

/// A run this process has no record of (§7.4): never seen, evicted, or from before a
/// restart. Both control tools answer `run_not_found`, and the message points at the
/// durable trace rather than promising to read it.
#[derive(Debug, Clone)]
pub struct RunNotFound {
    /// The id that was asked for.
    pub run_id: String,
    /// The newest ids this process knows, newest first (≤ [`KNOWN_RUNS_CAP`]).
    pub known_runs: Vec<String>,
}

impl RunNotFound {
    /// The `detail` object: `{run_id, known_runs}` (§17.3).
    #[must_use]
    pub fn detail(&self) -> Map<String, Value> {
        let mut detail = Map::new();
        detail.insert("run_id".to_owned(), Value::String(self.run_id.clone()));
        detail.insert(
            "known_runs".to_owned(),
            Value::Array(
                self.known_runs
                    .iter()
                    .take(KNOWN_RUNS_CAP)
                    .map(|run_id| Value::String(run_id.clone()))
                    .collect(),
            ),
        );
        detail
    }

    /// The `error.message`, which names the durable trace because that is the caller's
    /// only remaining source of truth after a restart.
    #[must_use]
    pub fn message(&self) -> String {
        format!(
            "no run {} is known to this server process; its durable trace is \
             .evidence/runs/{}/ (run_status does not read it)",
            self.run_id, self.run_id
        )
    }
}

/// Why `run_workflow` could not register a run (ADR-0001 §5.3's precondition codes).
#[derive(Debug, Clone)]
pub enum StartError {
    /// §9.2 step 1: the run closed for new work after its shutdown signal.
    ShuttingDown,
    /// The workspace single-flight lock is held (§8): no queue, and `detail` names the run
    /// that is going so the caller can poll it or cancel it.
    WorkspaceBusy {
        /// The holder's published body (L1's own record, or L2's lock file body).
        body: LockBody,
    },
    /// The lock file could not be opened at all — a server misconfiguration (ADR-0003 §6.3).
    LockUnavailable(LockUnavailable),
    /// `--max-concurrent-runs` is reached across workspaces.
    TooManyRuns {
        /// The configured limit.
        limit: usize,
        /// How many runs were active when the call arrived.
        active: usize,
    },
    /// `.evidence/runs/<run_id>/` could not be created, so `Queued` would have been a lie.
    RunDir {
        /// The path that failed.
        path: PathBuf,
        /// The OS's words.
        reason: String,
    },
    /// mact-f1w.13: `.evidence/runs/` already holds `--max-run-dirs` run directories.
    ///
    /// Nothing in the epic deletes a published run dir (ADR-0002 §6.2), so this is the disk
    /// guard that replaces deletion: refuse the run, name the directory to prune, and leave
    /// the machine alone.
    TooManyRunDirs {
        /// The configured cap.
        limit: usize,
        /// How many run directories were there when the request arrived.
        existing: usize,
        /// The directory to prune (or the operator raises `--max-run-dirs`).
        path: PathBuf,
    },
    /// The plan's `run_id` is not the pinned shape (defence in-depth: the id is
    /// server-generated, so this is a bug report, not a user error).
    BadRunId(String),
}

impl StartError {
    /// The ADR-0001 §5.3 code and `retryable` column for this refusal.
    #[must_use]
    pub fn code(&self) -> (&'static str, bool) {
        match self {
            // §9.2: "the closed error set has no shutting_down code"; `retryable: true` is
            // the honest advice, because after the restart the call may well succeed.
            Self::ShuttingDown => ("internal_error", true),
            Self::WorkspaceBusy { .. } => ("workspace_busy", true),
            Self::LockUnavailable { .. } => ("internal_error", true),
            Self::TooManyRuns { .. } => ("too_many_runs", true),
            Self::RunDir { .. } => ("runner_internal_error", true),
            // The same wire code as the in-flight cap: from the caller's side both are
            // "this server will not take another run right now", and both are retryable
            // once the operator prunes or a run finishes.
            Self::TooManyRunDirs { .. } => ("too_many_runs", true),
            Self::BadRunId(_) => ("internal_error", false),
        }
    }

    /// The `error.message`.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::ShuttingDown => {
                "act-mcp is shutting down and will not start new runs; retry after it restarts"
                    .to_owned()
            }
            Self::WorkspaceBusy { body } => format!(
                "this workspace already has a run in flight ({}), and there is no queue: poll it \
                 with run_status or cancel it with cancel_run",
                body.run_id
            ),
            Self::LockUnavailable(error) => format!(
                "the workspace run lock could not be taken, which is a server misconfiguration: \
                 {error}"
            ),
            Self::TooManyRuns { limit, active } => format!(
                "{active} act runs are already active and the server limit is {limit}; there is \
                 no queue, so retry when one finishes"
            ),
            Self::RunDir { path, reason } => format!(
                "the run's evidence directory {} could not be created: {reason}",
                path.display()
            ),
            Self::TooManyRunDirs {
                limit,
                existing,
                path,
            } => format!(
                "{} already holds {existing} run directories and the limit is {limit}; run \
                 retention never deletes published evidence, so prune that directory (or raise \
                 --max-run-dirs) before another run starts",
                path.display()
            ),
            Self::BadRunId(value) => format!("`{value}` is not a valid run_id (ADR-0002 §1.2)"),
        }
    }

    /// The `error.detail`.
    #[must_use]
    pub fn detail(&self) -> Map<String, Value> {
        match self {
            Self::ShuttingDown => {
                let mut detail = Map::new();
                detail.insert(
                    "reason".to_owned(),
                    Value::String("shutting_down".to_owned()),
                );
                detail
            }
            Self::WorkspaceBusy { body } => body.to_detail(),
            Self::LockUnavailable(error) => {
                let mut detail = Map::new();
                detail.insert(
                    "reason".to_owned(),
                    Value::String("workspace_lock_unavailable".to_owned()),
                );
                detail.insert(
                    "path".to_owned(),
                    Value::String(error.path.display().to_string()),
                );
                detail.insert("os_error".to_owned(), Value::String(error.os_error.clone()));
                detail
            }
            Self::TooManyRuns { limit, active } => {
                let mut detail = Map::new();
                detail.insert("limit".to_owned(), Value::from(*limit));
                detail.insert("active".to_owned(), Value::from(*active));
                detail
            }
            Self::TooManyRunDirs {
                limit,
                existing,
                path,
            } => {
                let mut detail = Map::new();
                detail.insert("limit".to_owned(), Value::from(*limit));
                detail.insert("existing".to_owned(), Value::from(*existing));
                // Which directory to prune is the actionable half of the answer; ADR-0001 §9
                // keeps the absolute path in the local log rather than on the wire, and the
                // runs dir is `<evidence>/runs` either way.
                detail.insert(
                    "runs_dir".to_owned(),
                    Value::String(path.display().to_string()),
                );
                detail
            }
            Self::RunDir { path, reason } => {
                let mut detail = Map::new();
                detail.insert("stage".to_owned(), Value::String("queued".to_owned()));
                detail.insert("path".to_owned(), Value::String(path.display().to_string()));
                detail.insert("reason".to_owned(), Value::String(reason.clone()));
                detail
            }
            Self::BadRunId(value) => {
                let mut detail = Map::new();
                detail.insert("run_id".to_owned(), Value::String(value.clone()));
                detail
            }
        }
    }
}

/// Everything a poller or a `wait: true` caller needs about a run, read without blocking.
#[derive(Debug, Clone)]
pub struct Listing {
    /// The page: newest first, capped by the caller's `limit`.
    pub runs: Vec<RunSummary>,
    /// Non-terminal runs in this process, whether or not they made the page.
    pub active_count: usize,
    /// True when older finished runs were dropped (page truncation or retention, §7.3).
    pub truncated: bool,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let held = self
            .workspaces
            .lock()
            .map(|workspaces| workspaces.keys().count())
            .unwrap_or(0);
        f.debug_struct("Inner")
            .field("max_concurrent_runs", &self.policy.max_concurrent_runs)
            .field("workspace_locks_held", &held)
            .field("shutting_down", &self.shutting_down.load(Ordering::Relaxed))
            .finish()
    }
}

/// A run the supervisor has accepted: the id, and a subscription to its terminal commit.
///
/// Dropping a ticket removes a waiter and nothing else — that is the property ADR-0004 §4
/// cares about, and `tests/run_supervisor.rs` asserts it.
#[derive(Clone)]
pub struct RunTicket {
    run_id: String,
    receiver: watch::Receiver<Arc<RunRecord>>,
}

impl RunTicket {
    /// The allocated run id.
    #[must_use]
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// The record as it stands right now.
    #[must_use]
    pub fn record(&self) -> Arc<RunRecord> {
        self.receiver.borrow().clone()
    }

    /// Wait for this run's terminal commit (ADR-0004 §3.4 step 7: waiters are notified
    /// last, so a terminal record implies the evidence and `meta.json` are on disk).
    ///
    /// There is no timeout here on purpose: `wait: true`'s bound is
    /// `timeout_secs + prestart + grace + harvest` (§4.6), which the caller knows from the
    /// numbers it passed, and a supervisor-imposed cut-off would invent a second, smaller
    /// budget the contract does not describe.
    #[must_use]
    pub async fn wait(&self) -> Arc<RunRecord> {
        let mut receiver = self.receiver.clone();
        loop {
            if receiver.borrow().state.is_terminal() {
                return receiver.borrow().clone();
            }
            if receiver.changed().await.is_err() {
                // The run task is gone. Report the last state we ever saw rather than
                // hanging: a dropped task is already a logged warning.
                return receiver.borrow().clone();
            }
        }
    }

    /// [`Self::wait`] with a deadline, for a request that has its own budget.
    #[must_use]
    pub async fn wait_within(&self, budget: Duration) -> Option<Arc<RunRecord>> {
        tokio::time::timeout(budget, self.wait()).await.ok()
    }
}

impl std::fmt::Debug for RunTicket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunTicket")
            .field("run_id", &self.run_id)
            .field("state", &self.receiver.borrow().state)
            .finish()
    }
}

/// One run's registry slot: the watch that publishes its snapshots, the task that owns it,
/// and the cancel request channel.
struct Slot {
    record: watch::Sender<Arc<RunRecord>>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    cancel: CancellationToken,
    /// The first cancel request's instant, set by whoever asked (ADR-0004 §3.5: a cancel is
    /// a *request*; only the run task writes state, so the flag lives beside the slot and is
    /// merged into every published snapshot).
    cancel_requested_at: Mutex<Option<DateTime<Utc>>>,
    workspace: PathBuf,
    /// The runner handle, filled the instant act spawns. `run_status` reads its live log
    /// tail through it; the *kill* path does not, because only the run's own task may
    /// signal (ADR-0004 §3.5) — it cancels `cancel` and lets the task do the signalling.
    runner: Arc<Mutex<Option<crate::runner::ActRunHandle>>>,
}

impl Slot {
    fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Record that a cancel was requested, returning the first request's instant.
    fn note_cancel(&self) -> DateTime<Utc> {
        let mut guard = Self::lock(&self.cancel_requested_at);
        if guard.is_none() {
            *guard = Some(Utc::now());
        }
        guard.expect("just set")
    }

    /// Publish a snapshot, merging the cancel flag the run task does not own.
    fn publish(&self, record: RunRecord) {
        let mut record = record;
        if record.cancel_requested_at.is_none() {
            record.cancel_requested_at = *Self::lock(&self.cancel_requested_at);
        }
        let _ = self.record.send_replace(Arc::new(record));
    }

    /// The current snapshot.
    fn current(&self) -> Arc<RunRecord> {
        self.record.borrow().clone()
    }

    /// Wait for the terminal commit, up to `budget`. `true` when it arrived.
    async fn wait_terminal(&self, budget: Duration) -> bool {
        let mut receiver = self.record.subscribe();
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            if self.record.borrow().state.is_terminal() {
                return true;
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return false;
            }
            if tokio::time::timeout(remaining, receiver.changed())
                .await
                .is_err()
            {
                return false;
            }
        }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        // Eviction must not orphan a task: a run whose registry entry is gone has nobody who
        // can answer `run_status` or `cancel_run` for it, so an unfinished one is stopped
        // rather than left to run unwatched. Retention never reaches this for a live run
        // (a non-terminal run is never evicted), so in practice this fires only when the
        // whole supervisor is going away.
        if let Some(task) = Self::lock(&self.task).as_ref() {
            if !task.is_finished() {
                self.cancel.cancel();
                task.abort();
            }
        }
    }
}

/// The shared state behind every clone of [`RunSupervisor`].
///
/// `Debug` is manual and small: the registry holds log tails, and printing them from a
/// `{:?}` in an unrelated log line would be a secret-redaction hole by accident.
struct Inner {
    policy: RunPolicy,
    runner: Runner,
    pipeline: Arc<dyn Pipeline>,
    runs: RwLock<HashMap<String, Arc<Slot>>>,
    /// ADR-0003 §6.1's L1 and L2 together: the canonical workspace path maps to the run
    /// that holds it and to the descriptor that holds the `flock`.
    workspaces: Mutex<HashMap<PathBuf, (String, HeldLock)>>,
    /// §5.3: `request key → run_id` for `wait: true` calls, so a cancellation notification
    /// naming a request can reach the run it is waiting on.
    waiters: Mutex<HashMap<String, String>>,
    shutting_down: AtomicBool,
    shutdown: CancellationToken,
}

/// The supervisor: register runs, report them, cancel them, and take them down on shutdown.
///
/// Cheap to clone (one `Arc`), so the MCP service holds one per process and every session
/// sees the same set of runs.
#[derive(Debug, Clone)]
pub struct RunSupervisor {
    inner: Arc<Inner>,
}

impl RunSupervisor {
    /// Build a supervisor with ADR-0004 §11's defaults and no evidence pipeline.
    ///
    /// # Errors
    ///
    /// [`PolicyError`] if the defaults were modified out of their bounds.
    pub fn new() -> Result<Self, PolicyError> {
        Self::with_policy(RunPolicy::default())
    }

    /// Build a supervisor from a policy, with the default no-evidence pipeline.
    ///
    /// # Errors
    ///
    /// [`PolicyError`] on a policy out of §11's bounds, or a runner-limit combination the
    /// runner rejects (a grace so long that `kill.took_ms` could exceed its schema bound).
    pub fn with_policy(policy: RunPolicy) -> Result<Self, PolicyError> {
        Self::build(policy, Arc::new(NoEvidence)).map_err(|error| match error {
            StartBuildError::Policy(policy_error) => policy_error,
            StartBuildError::Limits(limit) => PolicyError::TermGrace {
                value: Duration::from_secs(limit_term_grace_secs(&limit)),
            },
        })
    }

    /// Build a supervisor with an evidence pipeline (this is what mact-f1w.14 calls).
    ///
    /// # Errors
    ///
    /// As [`RunSupervisor::with_policy`].
    pub fn build(policy: RunPolicy, pipeline: Arc<dyn Pipeline>) -> Result<Self, StartBuildError> {
        policy.validate().map_err(StartBuildError::Policy)?;
        let limits = policy.runner_limits().map_err(StartBuildError::Limits)?;
        let runner = Runner::new(limits).map_err(StartBuildError::Limits)?;
        Ok(Self::from_runner(policy, runner, pipeline))
    }

    /// Assemble from an already-built runner (the seam tests use to shorten the grace).
    #[must_use]
    pub fn from_runner(policy: RunPolicy, runner: Runner, pipeline: Arc<dyn Pipeline>) -> Self {
        Self {
            inner: Arc::new(Inner {
                policy,
                runner,
                pipeline,
                runs: RwLock::new(HashMap::new()),
                workspaces: Mutex::new(HashMap::new()),
                waiters: Mutex::new(HashMap::new()),
                shutting_down: AtomicBool::new(false),
                shutdown: CancellationToken::new(),
            }),
        }
    }

    /// The enforced policy.
    #[must_use]
    pub fn policy(&self) -> &RunPolicy {
        &self.inner.policy
    }

    /// Whether §9.2's step 1 has happened (new runs are refused).
    #[must_use]
    pub fn is_shutting_down(&self) -> bool {
        self.inner.shutting_down.load(Ordering::SeqCst)
    }

    /// Allocate the next `run_id` (the caller feeds it into the spec it hands back).
    #[must_use]
    pub fn new_run_id(&self) -> RunId {
        RunId::generate()
    }

    /// A waitable handle for a run this process already has, `None` for an unknown id.
    ///
    /// This is what lets an `idempotency_key` replay wait on a run another request started:
    /// the run belongs to the supervisor (ADR-0004 §2), so any caller that knows its id can
    /// subscribe to its terminal commit. Unlike [`Self::status`] this never answers
    /// `run_not_found` — an unknown id is simply `None`, and the caller decides whether that is
    /// an error (it is for `run_status`, and for `run_workflow` it means "start a fresh run").
    #[must_use]
    pub async fn ticket(&self, run_id: &str) -> Option<RunTicket> {
        let slot = { self.inner.runs.read().await.get(run_id).cloned() }?;
        Some(RunTicket {
            run_id: run_id.to_owned(),
            receiver: slot.record.subscribe(),
        })
    }
}

/// How many run directories `runs_root` holds (the `--max-run-dirs` check, mact-f1w.13).
///
/// One `read_dir`, no recursion: the cap counts the runs this server has accumulated, and a
/// checker that walked every run's contents would be the expensive thing a request has to
/// wait for. A missing or unreadable root counts as zero — a first run has no directory yet,
/// and "could not count them" must not read as "at the cap".
fn count_run_dirs(runs_root: &std::path::Path) -> usize {
    match std::fs::read_dir(runs_root) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .filter(|entry| entry.path().is_dir())
            .count(),
        Err(_) => 0,
    }
}

/// Why [`RunSupervisor::build`] refused.
#[allow(missing_docs, reason = "the inner variants carry the text")]
#[derive(Debug, Error)]
pub enum StartBuildError {
    #[error(transparent)]
    Policy(#[from] PolicyError),
    #[error(transparent)]
    Limits(#[from] crate::runner::LimitError),
}

fn limit_term_grace_secs(error: &crate::runner::LimitError) -> u64 {
    match error {
        crate::runner::LimitError::TookMsBound { grace, .. } => grace.as_secs(),
        crate::runner::LimitError::TermGrace { value } => value.as_secs(),
        _ => crate::runner::DEFAULT_TERM_GRACE_SECS,
    }
}

impl Default for RunSupervisor {
    fn default() -> Self {
        Self::new().expect("the shipped defaults are valid")
    }
}

impl RunSupervisor {
    /// One run's record, or the `run_not_found` payload for it (§7.4).
    ///
    /// Never blocks on run work: the snapshot is published by the run task on every
    /// transition, so this is one read of one `Arc`.
    ///
    /// # Errors
    ///
    /// [`RunNotFound`] when this process has no such run — never seen, evicted, or from
    /// before a restart.
    pub async fn record(&self, run_id: &str) -> Result<Arc<RunRecord>, RunNotFound> {
        let slot = { self.inner.runs.read().await.get(run_id).cloned() };
        match slot {
            Some(slot) => Ok(slot.record.borrow().clone()),
            None => Err(self.not_found(run_id).await),
        }
    }

    /// `run_status(run_id)` — the `run_view`, with the log tail served from memory
    /// (ADR-0004 §7.1: the tail ring is exactly `log_tail_bytes`'s maximum, so a poll never
    /// reads `act.jsonl`, which is what keeps this call non-blocking).
    ///
    /// # Errors
    ///
    /// [`RunNotFound`] as for [`Self::record`].
    pub async fn status(
        &self,
        run_id: &str,
        log_tail_bytes: usize,
    ) -> Result<RunView, RunNotFound> {
        let slot = { self.inner.runs.read().await.get(run_id).cloned() };
        let slot = match slot {
            Some(slot) => slot,
            None => return Err(self.not_found(run_id).await),
        };
        let record = slot.record.borrow().clone();
        let mut view = record.view(log_tail_bytes);
        // While act is alive the freshest tail is the runner's ring, not the snapshot the
        // run task last published (the run task is parked in `wait()` and cannot write).
        if !record.state.is_terminal() {
            if let Some(handle) = task::live_handle(&slot.runner) {
                view.log_tail = handle.log_tail(log_tail_bytes);
            }
        }
        Ok(view)
    }

    /// `run_status()` listing mode: the newest `limit` runs, newest first.
    ///
    /// An empty listing is a successful `{runs: [], active_count: 0, truncated: false}`
    /// (§17.3), and after a restart that is exactly what it is: run state is memory-only.
    #[must_use]
    pub async fn listing(&self, limit: usize) -> Listing {
        let limit = limit.clamp(1, LISTING_CAP);
        let runs = self.inner.runs.read().await;
        let mut all: Vec<Arc<RunRecord>> = runs
            .values()
            .map(|slot| slot.record.borrow().clone())
            .collect();
        // Newest first. `run_id` is time-first (ADR-0002 §1.2), so id order is registration
        // order; `registered_at` orders by arrival and the id breaks same-second ties.
        all.sort_by(|a, b| {
            b.registered_at
                .cmp(&a.registered_at)
                .then_with(|| b.run_id.cmp(&a.run_id))
        });
        let active_count = all
            .iter()
            .filter(|record| !record.state.is_terminal())
            .count();
        let truncated = all.len() > limit;
        let runs = all
            .into_iter()
            .take(limit)
            .map(|record| record.summary())
            .collect();
        Listing {
            runs,
            active_count,
            truncated,
        }
    }

    /// How many runs are non-terminal right now (`too_many_runs` and `active_count` share
    /// this number so they cannot disagree).
    #[must_use]
    pub async fn active_count(&self) -> usize {
        self.inner
            .runs
            .read()
            .await
            .values()
            .filter(|slot| !slot.record.borrow().state.is_terminal())
            .count()
    }

    /// The `run_not_found` payload: the id asked for plus this process's newest ids, newest
    /// first (§7.4). Listing mode with nothing in it is a success; an unknown id is not.
    async fn not_found(&self, run_id: &str) -> RunNotFound {
        let runs = self.inner.runs.read().await;
        let mut ids: Vec<(DateTime<Utc>, String)> = runs
            .values()
            .map(|slot| {
                let record = slot.record.borrow();
                (record.registered_at, record.run_id.clone())
            })
            .collect();
        ids.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
        RunNotFound {
            run_id: run_id.to_owned(),
            known_runs: ids
                .into_iter()
                .take(KNOWN_RUNS_CAP)
                .map(|(_, id)| id)
                .collect(),
        }
    }
}

impl RunSupervisor {
    /// Register a run and hand its task to the supervisor.
    ///
    /// This is ADR-0004 §3.4's registration half, in the pinned order: refuse a shutdown
    /// server, check the workspace single-flight (L1 then L2), check the server-wide cap,
    /// create `.evidence/runs/<run_id>/`, publish `Queued`, then spawn. `wait: false`
    /// returns here; `wait: true` calls [`RunTicket::wait`] on the returned ticket.
    ///
    /// The run outlives this call and the caller's connection: the task belongs to the
    /// runtime, not to the request.
    ///
    /// # Errors
    ///
    /// [`StartError`] names the precondition that refused the run (`workspace_busy`,
    /// `too_many_runs`, the shutdown refusal, or a lock/run-dir failure).
    pub async fn start(&self, plan: RunPlan) -> Result<RunTicket, StartError> {
        if self.is_shutting_down() {
            return Err(StartError::ShuttingDown);
        }
        let run_id = plan.spec.run_id.as_str().to_owned();
        if !RunId::is_valid_shape(&run_id) {
            return Err(StartError::BadRunId(run_id));
        }
        let workspace = lock::canonical_workspace(&plan.spec.workspace);

        // L1 (ADR-0003 §6.1): a run of *this* process already holds the workspace. The
        // kernel cannot see this case — re-`flock`ing the held descriptor always succeeds —
        // so the registry's own slot map is the only thing that can answer it.
        let held_by_this_process = self
            .inner
            .workspaces
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&workspace)
            .map(|(run_id, _)| run_id.clone());
        // The guard is dropped above before any `.await`, so a busy workspace never blocks
        // the map for the duration of building the reply.
        if let Some(owner) = held_by_this_process {
            let body = match self.record(&owner).await {
                Ok(record) => LockBody::for_run(
                    &record.run_id,
                    record.state.phase().as_str(),
                    record.started_at,
                ),
                Err(_) => LockBody::for_run(&owner, "running", None),
            };
            tracing::info!(
                run_id = %run_id,
                holder = %owner,
                "refusing a second run in a workspace this process already holds"
            );
            return Err(StartError::WorkspaceBusy { body });
        }

        // Server-wide cap. Checked before the lock so a burst against one workspace leaves
        // the lock untouched, and so `active` is read before any run of ours is registered.
        let active = self.active_count().await;
        if active >= self.inner.policy.max_concurrent_runs {
            return Err(StartError::TooManyRuns {
                limit: self.inner.policy.max_concurrent_runs,
                active,
            });
        }

        // The disk-side sibling of that cap (mact-f1w.13): memory retention bounds the run
        // records, this bounds the directories they left behind. Checked before the lock for
        // the same reason as the concurrency cap — a refusal should not touch anything.
        if let Some(runs_root) = plan.spec.run_dir.parent() {
            let limit = self.inner.policy.max_run_dirs;
            let existing = count_run_dirs(runs_root);
            if existing >= limit {
                tracing::warn!(
                    run_id = %run_id,
                    limit,
                    existing,
                    runs_dir = %runs_root.display(),
                    "refusing a run: the evidence runs directory is at its cap"
                );
                return Err(StartError::TooManyRunDirs {
                    limit,
                    existing,
                    path: runs_root.to_path_buf(),
                });
            }
        }

        // L2: the `flock`. Busy is answered in microseconds (§6.5) — there is no queue and
        // no wait, and the body names the holder so "poll it or cancel it" works across
        // server processes.
        let (stale, held) = match lock::acquire(&plan.spec.workspace) {
            Ok(Acquisition::Taken { stale, held }) => (stale, held),
            Ok(Acquisition::Busy { body }) => {
                return Err(StartError::WorkspaceBusy { body });
            }
            Err(unavailable) => {
                tracing::warn!(
                    run_id = %run_id,
                    "the workspace run lock could not be taken: {unavailable}"
                );
                return Err(StartError::LockUnavailable(unavailable));
            }
        };
        if let Some(stale) = &stale {
            tracing::warn!(
                run_id = %run_id,
                previous_run = %stale.run_id,
                previous_pid = stale.pid.unwrap_or_default(),
                lock = %lock::lock_path(&plan.spec.workspace).display(),
                "broke a stale workspace lock: its holder process is gone, and flock \
                 released the lock with it (ADR-0003 §6.5)"
            );
        }

        // `Queued` means "run dir created" (ADR-0004 §17.1), so the directory is made
        // before the state is published, not after.
        if let Err(reason) = std::fs::create_dir_all(&plan.spec.run_dir) {
            return Err(StartError::RunDir {
                path: plan.spec.run_dir.clone(),
                reason: reason.to_string(),
            });
        }

        let mut record = RunRecord::new(run_id.clone(), plan.identity.clone());
        // The gate's codes are on the record from the first publish, so a run that never
        // gets to spawn act still says what the gate noticed about it.
        for warning in &plan.warnings {
            record.add_warning(warning);
        }
        let (sender, receiver) = watch::channel(Arc::new(record));
        let cancel = CancellationToken::new();
        let slot = Arc::new(Slot {
            record: sender,
            task: Mutex::new(None),
            cancel: cancel.clone(),
            cancel_requested_at: Mutex::new(None),
            workspace: workspace.clone(),
            runner: Arc::default(),
        });
        let spec = plan.spec.clone();
        // **The runner's internal cancel path is deliberately left unarmed**, and this is the
        // one place to explain why (mact-f1w.10 owns the termination matrix; see also
        // `tests/run_supervisor.rs::a_run_that_ignores_sigterm_is_escalated_to_sigkill`).
        //
        // `RunHandle::terminate` claims a mutex for the whole teardown (grace wait included),
        // and the runner's *own* `wait()` calls it from inside its select when
        // `spec.cancellation` fires. The run task parks `wait()` while it awaits its own
        // `terminate`, so arming both paths deadlocks: the internal teardown can only advance
        // while `wait()` is polled, and `wait()` is not polled until the external teardown
        // returns, and the external teardown is waiting for the mutex the internal one holds.
        // A run whose act ignored `SIGTERM` then hangs for the whole grace instead of
        // escalating — silently, with the workspace wedged.
        //
        // Keeping the token unarmed also fixes the label: the runner's token path always
        // records `Caller`, but ADR-0004 T13 needs a shutdown kill to say
        // `runner_internal_error` + `detail.reason: "server_shutdown"`, which only this
        // supervisor knows. So the run task's select is the sole caller of `terminate`, and
        // the runner keeps its deadline (T12) and its own token path (used by
        // `tests/runner_execution.rs`, where nothing else is terminating the run).
        let plan = RunPlan {
            identity: plan.identity.clone(),
            spec,
            warnings: plan.warnings.clone(),
        };
        tracing::info!(
            run_id = %run_id,
            workspace = %plan.spec.workspace.display(),
            workflow = %plan.identity.workflow,
            task = %plan.identity.task,
            category = %plan.identity.category,
            "run registered"
        );
        let task = tokio::spawn(task::drive(
            Arc::clone(&self.inner),
            plan,
            Arc::clone(&slot),
        ));
        *Slot::lock(&slot.task) = Some(task);
        self.inner
            .workspaces
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(workspace, (run_id.clone(), held));
        self.inner.runs.write().await.insert(run_id.clone(), slot);
        self.prune().await;
        Ok(RunTicket { run_id, receiver })
    }

    /// `cancel_run(run_id)` — ADR-0004 §5.2's table, row by row, and idempotent: a cancel
    /// for a terminal run reports that and is not an error.
    ///
    /// Only ever *requests* a stop: the flag/token is written here, and the run's own task
    /// performs the teardown and every state change (§3.5). The block is bounded by
    /// `--cancel-wait-secs` (§11), and a second caller does not wait: it is told the first
    /// one owns the teardown.
    ///
    /// # Errors
    ///
    /// [`RunNotFound`] for a run this process does not have (§7.4) — the only error this
    /// tool has.
    pub async fn cancel(&self, run_id: &str) -> Result<CancelReceipt, RunNotFound> {
        let slot = { self.inner.runs.read().await.get(run_id).cloned() };
        let slot = match slot {
            Some(slot) => slot,
            None => return Err(self.not_found(run_id).await),
        };
        let record = slot.current();
        let warnings_of = |record: &RunRecord, add_already_terminal: bool| {
            let mut warnings = record.warnings.clone();
            if add_already_terminal
                && !warnings
                    .iter()
                    .any(|warning| warning == WARNING_ALREADY_TERMINAL)
            {
                warnings.push(WARNING_ALREADY_TERMINAL.to_owned());
            }
            warnings
        };

        // Row: any terminal state → `already_terminal`, the original status, no timestamp,
        // no kill (T20: a terminal run never transitions, so nothing here may change it).
        if record.state.is_terminal() {
            return Ok(CancelReceipt {
                run_id: run_id.to_owned(),
                status: record.state.status(),
                terminal: true,
                already_terminal: true,
                cancel_requested_at: None,
                kill: None,
                warnings: warnings_of(&record, true),
            });
        }

        // Row: `Cancelling` → the first caller owns the teardown, so this caller is
        // answered from state immediately and never starts a second kill (ADR-0001 §6.3).
        if record.state == RunState::Cancelling {
            return Ok(CancelReceipt {
                run_id: run_id.to_owned(),
                status: WireStatus::Cancelled,
                terminal: false,
                already_terminal: false,
                cancel_requested_at: Some(slot.note_cancel()),
                kill: None,
                warnings: warnings_of(&record, false),
            });
        }

        // Row: `Harvesting` / `Publishing` → the cancel window closed when act exited
        // (§5.4). Nothing to stop, the evidence is already determined, so this is a reported
        // no-op rather than a silent one.
        if matches!(record.state, RunState::Harvesting | RunState::Publishing) {
            let requested_at = slot.note_cancel();
            return Ok(CancelReceipt {
                run_id: run_id.to_owned(),
                status: record.state.status(),
                terminal: false,
                already_terminal: false,
                cancel_requested_at: Some(requested_at),
                kill: None,
                warnings: warnings_of(&record, true),
            });
        }

        // Rows: `Queued` / `PreparingTree` / `ActStarting` / `ActRunning` → request the
        // stop, then wait up to `--cancel-wait-secs` for the terminal commit.
        let requested_at = slot.note_cancel();
        slot.publish(slot.current().as_ref().clone());
        slot.cancel.cancel();
        let terminal_in_time = slot.wait_terminal(self.inner.policy.cancel_wait).await;
        let record = slot.current();
        if terminal_in_time && record.state.is_terminal() {
            Ok(CancelReceipt {
                run_id: run_id.to_owned(),
                status: record.state.status(),
                terminal: true,
                already_terminal: false,
                cancel_requested_at: Some(record.cancel_requested_at.unwrap_or(requested_at)),
                // A run that never spawned reports `kill: {method: "none"}` (§16.2); the run
                // task records exactly that, so this is a copy, not a guess.
                kill: record.kill.or(Some(crate::runner::KillReport::none())),
                warnings: warnings_of(&record, false),
            })
        } else {
            // Teardown still running: the receipt is honest about that, and `run_status` is
            // the follow-up (§5.2).
            Ok(CancelReceipt {
                run_id: run_id.to_owned(),
                status: WireStatus::Cancelled,
                terminal: false,
                already_terminal: false,
                cancel_requested_at: Some(record.cancel_requested_at.unwrap_or(requested_at)),
                kill: None,
                warnings: warnings_of(&record, false),
            })
        }
    }
}

/// A `wait: true` request registered with the supervisor (§5.3).
///
/// Holding one of these is what makes an MCP cancellation notification able to find the run
/// a request is waiting on. Dropping the guard removes the mapping, which is also what
/// happens when a request future is dropped — and dropping a request is **not** cancellation
/// (§5.3: "transport death is never cancellation"), so the run keeps going.
#[derive(Debug)]
pub struct RequestWaiter {
    inner: Arc<Inner>,
    key: String,
}

impl Drop for RequestWaiter {
    fn drop(&mut self) {
        self.inner.forget_request(&self.key);
    }
}

impl RunSupervisor {
    /// Register that request `key` is waiting on `run_id`, returning a guard that
    /// unregisters it. Calling this twice for one key replaces the mapping (the newest wait
    /// is the one a cancellation should reach).
    #[must_use]
    pub fn wait_on(&self, key: &str, run_id: &str) -> RequestWaiter {
        self.inner
            .waiters
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(key.to_owned(), run_id.to_owned());
        RequestWaiter {
            inner: Arc::clone(&self.inner),
            key: key.to_owned(),
        }
    }

    /// Honour a cancellation notification for request `key` (ADR-0004 §5.3).
    ///
    /// A notification naming an unknown, already-finished or non-waiting request is ignored:
    /// a notification cannot be answered, so ignoring it *is* the error handling.
    ///
    /// Returns the receipt when the request was waiting on a run that could be cancelled.
    ///
    /// # Errors
    ///
    /// [`RunNotFound`] when the mapped run has vanished between the mapping and the cancel.
    pub async fn cancel_waiting_request(&self, key: &str) -> Option<CancelReceipt> {
        let run_id = self
            .inner
            .waiters
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(key)
            .cloned();
        match run_id {
            Some(run_id) => self.cancel(&run_id).await.ok(),
            None => {
                tracing::debug!(request = %key, "cancellation notification for an unknown request; ignored");
                None
            }
        }
    }

    /// §9.2 steps 1–2: stop accepting runs, then ask every active run to come down
    /// **concurrently** (separate process groups, so wall time is one grace period, not N).
    ///
    /// Non-blocking: the runs' own tasks do the signalling and the terminal commits.
    pub fn begin_shutdown(&self) {
        if self.inner.shutting_down.swap(true, Ordering::SeqCst) {
            return;
        }
        tracing::info!("shutdown: no new runs will be accepted; terminating the active ones");
        self.inner.shutdown.cancel();
    }

    /// §9.2 step 3: wait up to `--shutdown-grace-secs` for the terminal commits, then report
    /// whatever is still going (their teardown has already escalated to SIGKILL inside the
    /// runner; a run that does not reach `meta.json` leaves the manifest absent, which is
    /// ADR-0002 §1.3's documented "the process died mid-run", not a third state).
    pub async fn drain(&self) {
        let deadline = tokio::time::Instant::now() + self.inner.policy.shutdown_grace;
        loop {
            let active: Vec<String> = {
                let runs = self.inner.runs.read().await;
                runs.iter()
                    .filter(|(_, slot)| !slot.current().state.is_terminal())
                    .map(|(run_id, _)| run_id.clone())
                    .collect()
            };
            if active.is_empty() {
                tracing::info!("shutdown: every run reached its terminal commit");
                return;
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                tracing::warn!(
                    runs = ?active,
                    grace = ?self.inner.policy.shutdown_grace,
                    "shutdown grace expired with runs still in flight; their act process \
                     groups were SIGKILLed and their directories deliberately have no \
                     meta.json"
                );
                return;
            }
            tokio::time::sleep(std::cmp::min(remaining, Duration::from_millis(25))).await;
        }
    }

    /// [`Self::begin_shutdown`] followed by [`Self::drain`] — what `serve` calls on SIGINT
    /// or SIGTERM.
    pub async fn shutdown(&self) {
        self.begin_shutdown();
        self.drain().await;
    }

    /// Retention (§7.3): evict finished runs that are older than the age limit, then the
    /// oldest-finished ones until the count fits. A non-terminal run is **never** evicted,
    /// and the on-disk `.evidence/runs/<run_id>/` is never touched here (disk pruning is an
    /// operator cron, per ADR-0002).
    ///
    /// Returns the evicted ids so the holder of an `idempotency_key → run_id` map (the
    /// `run_workflow` wiring) can drop its entries *with* their runs — a key that outlives
    /// its run would replay into `run_not_found`, which is worse than a fresh run.
    pub async fn prune(&self) -> Vec<String> {
        self.inner.prune().await
    }
}

impl Inner {
    /// The retention sweep itself (see [`RunSupervisor::prune`]).
    async fn prune(&self) -> Vec<String> {
        let policy = &self.policy;
        let age_limit = chrono::Duration::from_std(policy.retention_age)
            .unwrap_or_else(|_| chrono::Duration::hours(2));
        let mut evicted: Vec<String> = Vec::new();
        {
            let mut runs = self.runs.write().await;
            let now = Utc::now();
            // Age limit first: any finished run past the age becomes eligible and goes.
            let stale: Vec<String> = runs
                .iter()
                .filter(|(_, slot)| {
                    let record = slot.current();
                    record
                        .state
                        .is_terminal()
                        .then(|| record.finished_at)
                        .flatten()
                        .is_some_and(|finished| now - finished > age_limit)
                })
                .map(|(run_id, _)| run_id.clone())
                .collect();
            for run_id in stale {
                if runs.remove(&run_id).is_some() {
                    evicted.push(run_id);
                }
            }
            // Count limit second: oldest-finished first, one at a time until the entry fits.
            let mut finished: Vec<(Option<DateTime<Utc>>, String)> = runs
                .iter()
                .filter(|(_, slot)| slot.current().state.is_terminal())
                .map(|(run_id, slot)| (slot.current().finished_at, run_id.clone()))
                .collect();
            while finished.len() > policy.retention_count {
                finished.sort_by_key(|entry| entry.0);
                let Some((_, oldest)) = finished.first().cloned() else {
                    break;
                };
                if runs.remove(&oldest).is_some() {
                    evicted.push(oldest.clone());
                }
                finished.remove(0);
            }
        }
        for run_id in &evicted {
            // ADR-0004 §7.3: memory goes, disk stays. Say so, because the operator's next
            // question is "where are the numbers then".
            tracing::info!(
                run_id = %run_id,
                "evicted run state from memory; .evidence/runs/ keeps the durable record"
            );
        }
        evicted
    }

    fn forget_request(&self, key: &str) {
        self.waiters
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(key);
    }
}
