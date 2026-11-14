//! The run state machine, exactly as [ADR-0004 §17.1](../../docs/adr/0004-run-lifecycle.md)
//! enumerates it, plus the two wire shapes every control tool answers with.
//!
//! Two levels, on purpose (§3.1): the twelve internal states exist because teardown,
//! extraction and publication have different failure modes; the wire only ever shows
//! `pending`, `running`, and the five terminal words `.evidence` uses. The projection
//! below is the *only* place that mapping is written down, so `run_status`, `cancel_run`
//! and `run_workflow` (mact-f1w.14) cannot disagree about what a run looks like.
//!
//! ```text
//!   Queued ──────────► PreparingTree ─────────► ActStarting ─────────► ActRunning
//!     │  cancel/error      │ cancel/error          │ cancel / spawn fail    │
//!     ▼                    ▼                       ▼                        │ exit ┌────────────┐
//!   Cancelled / Error   Cancelled / Error      Cancelled / Error            ├─────►│ Harvesting │
//!                                                                        cancel    └─────┬──────┘
//!                                                                             │         ▼ Publishing
//!                                                                        Cancelling  (label) terminal
//!                                                                             │
//!                                             Cancelled (caller: extraction skipped)
//!                                             Harvesting (deadline/shutdown: extraction attempted)
//! ```
//!
//! The rules that make this testable rather than racy (§3.5):
//!
//! * **one writer.** Only a run's own task calls [`RunRecord::transition`]. `cancel_run`,
//!   the deadline and the shutdown sweep set flags ([`RunRecord::note_cancel_requested`])
//!   or ask the runner to terminate; they never write a state.
//! * **terminal is final** (T20): [`RunState::is_terminal`] states refuse every
//!   transition, and a `cancel_run` that arrives late is served from the stored state.
//! * **the projection cannot drift**: `terminal ⇔ status ∈ {success, failed, cancelled,
//!   timed_out, error}`, which is also what `schema/tool-contract.schema.json` enforces.

use std::fmt;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::runner::{KillReport, RunStatus, TerminateInitiator};

/// The five terminal status words (ADR-0001 §3.2, byte-identical to `.evidence`'s
/// `run_status`), plus the two non-terminal ones only `run_status`/`wait: false` can show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireStatus {
    /// Registered but act has not started (`queued`/`preparing_tree`/`starting_act`).
    Pending,
    /// act is alive, or the run is in teardown/extraction/publish.
    Running,
    /// act exited 0.
    Success,
    /// act exited non-zero.
    Failed,
    /// A caller took the run down.
    Cancelled,
    /// The `timeout_secs` deadline expired.
    TimedOut,
    /// The server could not run the job to completion.
    Error,
}

impl WireStatus {
    /// The wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Success => "success",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed_out",
            Self::Error => "error",
        }
    }

    /// Whether this status ends a run.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Pending | Self::Running)
    }

    /// Lift one of the runner's five terminal statuses.
    #[must_use]
    pub const fn from_run_status(status: RunStatus) -> Self {
        match status {
            RunStatus::Success => Self::Success,
            RunStatus::Failed => Self::Failed,
            RunStatus::Cancelled => Self::Cancelled,
            RunStatus::TimedOut => Self::TimedOut,
            RunStatus::Error => Self::Error,
        }
    }
}

impl fmt::Display for WireStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The eight `phase` members of `$defs/phase`. No ninth member exists (ADR-0004 §16.8):
/// `phase` is display-only, which is what lets a phase be instantaneous or slow without
/// the wire contract changing (§3.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Registered: id allocated, workspace lock held, run dir created.
    Queued,
    /// ADR-0003 tree materialisation.
    PreparingTree,
    /// `exec` of act in flight.
    StartingAct,
    /// act alive, deadline armed.
    Running,
    /// Teardown in progress — **non-terminal** (ADR-0004 §16.1).
    Cancelling,
    /// Extractors running.
    ExtractingEvidence,
    /// Evidence publish + `meta.json`.
    PublishingEvidence,
    /// Terminal.
    Done,
}

impl Phase {
    /// The wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::PreparingTree => "preparing_tree",
            Self::StartingAct => "starting_act",
            Self::Running => "running",
            Self::Cancelling => "cancelling",
            Self::ExtractingEvidence => "extracting_evidence",
            Self::PublishingEvidence => "publishing_evidence",
            Self::Done => "done",
        }
    }
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The twelve internal states of ADR-0004 §3.2 / §17.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    /// Registered, no process yet.
    Queued,
    /// Tree materialisation (ADR-0003).
    PreparingTree,
    /// `exec` in flight.
    ActStarting,
    /// act alive.
    ActRunning,
    /// SIGTERM → grace → SIGKILL → reap. Non-terminal.
    Cancelling,
    /// act reaped, extractors running.
    Harvesting,
    /// Publishing JSONL + `meta.json`.
    Publishing,
    /// Terminal: act exited 0.
    Success,
    /// Terminal: act exited non-zero.
    Failed,
    /// Terminal: caller cancelled.
    Cancelled,
    /// Terminal: deadline expired.
    TimedOut,
    /// Terminal: the server could not run the job.
    Error,
}

impl RunState {
    /// §17.1's `NON-TERMINAL internal states → wire projection`, and the identity
    /// projection for the five terminal ones.
    #[must_use]
    pub const fn status(self) -> WireStatus {
        match self {
            Self::Queued | Self::PreparingTree | Self::ActStarting => WireStatus::Pending,
            Self::ActRunning | Self::Cancelling | Self::Harvesting | Self::Publishing => {
                WireStatus::Running
            }
            Self::Success => WireStatus::Success,
            Self::Failed => WireStatus::Failed,
            Self::Cancelled => WireStatus::Cancelled,
            Self::TimedOut => WireStatus::TimedOut,
            Self::Error => WireStatus::Error,
        }
    }

    /// The `phase` member this state projects to.
    #[must_use]
    pub const fn phase(self) -> Phase {
        match self {
            Self::Queued => Phase::Queued,
            Self::PreparingTree => Phase::PreparingTree,
            Self::ActStarting => Phase::StartingAct,
            Self::ActRunning => Phase::Running,
            Self::Cancelling => Phase::Cancelling,
            Self::Harvesting => Phase::ExtractingEvidence,
            Self::Publishing => Phase::PublishingEvidence,
            Self::Success | Self::Failed | Self::Cancelled | Self::TimedOut | Self::Error => {
                Phase::Done
            }
        }
    }

    /// Terminal states are the five `.evidence` words; nothing else ends a run.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        self.status().is_terminal()
    }

    /// The TRANSITIONS block of §17.1, as data.
    ///
    /// `Cancelling → Harvesting` is T15 (deadline/shutdown: extraction attempted) and
    /// `Cancelling → Cancelled` is T14 (caller cancel: extraction skipped);
    /// `Publishing → <label>` is T18, so all five terminal states are legal from there.
    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        match self {
            Self::Queued => matches!(next, Self::PreparingTree | Self::Cancelled | Self::Error),
            Self::PreparingTree => {
                matches!(next, Self::ActStarting | Self::Cancelled | Self::Error)
            }
            Self::ActStarting => matches!(next, Self::ActRunning | Self::Cancelled | Self::Error),
            Self::ActRunning => matches!(next, Self::Harvesting | Self::Cancelling),
            Self::Cancelling => matches!(next, Self::Cancelled | Self::Harvesting),
            Self::Harvesting => matches!(next, Self::Publishing | Self::Error),
            Self::Publishing => matches!(
                next,
                Self::Success | Self::Failed | Self::Cancelled | Self::TimedOut | Self::Error
            ),
            // T20: a terminal state never transitions.
            Self::Success | Self::Failed | Self::Cancelled | Self::TimedOut | Self::Error => false,
        }
    }

    /// The states a cancel may still stop (ADR-0004 §5.4: the window closes when act
    /// exits, so harvesting/publishing is a no-op, and a terminal run is `already_terminal`).
    #[must_use]
    pub const fn cancel_window_open(self) -> bool {
        matches!(
            self,
            Self::Queued | Self::PreparingTree | Self::ActStarting | Self::ActRunning
        )
    }
}

impl fmt::Display for RunState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.status().as_str())
    }
}

/// Why a run whose wire status is `error` ended that way, already in wire shape.
///
/// The `code` is a member of ADR-0001 §5.3's closed set; `detail` is what the runner or
/// the supervisor knows and the message cannot say. Messages never carry a host path
/// (ADR-0001 §5.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunError {
    /// One of the `isError: false` runner codes, or `internal_error`.
    pub code: &'static str,
    /// Operator-readable one-liner.
    pub message: String,
    /// ADR-0001 §5.3's advice column.
    pub retryable: bool,
    /// Structured context; `{}` serializes to an empty object, which the schema allows.
    pub detail: Map<String, Value>,
}

impl RunError {
    /// Build one from its parts.
    #[must_use]
    pub fn new(
        code: &'static str,
        message: impl Into<String>,
        retryable: bool,
        detail: Map<String, Value>,
    ) -> Self {
        Self {
            code,
            message: crate::workspace::truncate_bytes(
                &message.into(),
                crate::workspace::MAX_ERROR_MESSAGE_BYTES,
            ),
            retryable,
            detail,
        }
    }

    /// `runner_internal_error` with `detail.stage` naming the phase that failed — the
    /// shape every supervisor-side failure (§3.3 T3/T6/T17) uses.
    #[must_use]
    pub fn stage(stage: &str, reason: impl Into<String>, retryable: bool) -> Self {
        let reason = reason.into();
        let mut detail = Map::new();
        detail.insert("stage".to_owned(), Value::String(stage.to_owned()));
        detail.insert("reason".to_owned(), Value::String(reason.clone()));
        Self::new("runner_internal_error", reason, retryable, detail)
    }

    /// The `error` member of `run_view`.
    #[must_use]
    pub fn to_value(&self) -> Value {
        serde_json::json!({
            "code": self.code,
            "message": self.message,
            "retryable": self.retryable,
            "detail": Value::Object(self.detail.clone()),
        })
    }
}

/// What a run *is*: the identity tuple ADR-0004 §7.1 stores per run, all of it
/// already validated by mact-f1w.6 (so `commit` is a full SHA and `workflow_file` a
/// workspace-relative path).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunIdentity {
    /// Workflow basename without extension, as `run_workflow` was asked.
    pub workflow: String,
    /// `.github/workflows/<file>`, workspace-relative.
    pub workflow_file: String,
    /// The job id passed to `act -j`.
    pub task: String,
    /// The caller's category (open vocabulary).
    pub category: String,
    /// The 40-hex commit actually executed.
    pub commit: String,
}

impl RunIdentity {
    /// Assemble the identity tuple.
    #[must_use]
    pub fn new(
        workflow: impl Into<String>,
        workflow_file: impl Into<String>,
        task: impl Into<String>,
        category: impl Into<String>,
        commit: impl Into<String>,
    ) -> Self {
        Self {
            workflow: workflow.into(),
            workflow_file: workflow_file.into(),
            task: task.into(),
            category: category.into(),
            commit: commit.into(),
        }
    }
}

/// The registry entry for one run — ADR-0004 §7.1's `RunHandle`.
///
/// Immutable once published: the run's own task owns a mutable working copy and hands an
/// `Arc<RunRecord>` snapshot to the registry and to every `watch` subscriber at each
/// transition. That is why reading run state never blocks a writer and why a caller can
/// hold a snapshot across an `.await` without holding a lock.
#[derive(Debug, Clone)]
pub struct RunRecord {
    /// The run's id (ADR-0002 §1.2 shape).
    pub run_id: String,
    /// Identity tuple.
    pub identity: RunIdentity,
    /// Current state.
    pub state: RunState,
    /// When the run was registered (the retention clock, and the only clock that exists
    /// before act spawns).
    pub registered_at: DateTime<Utc>,
    /// The instant `exec` returned; absent until act spawned (ADR-0004 §4.4).
    pub started_at: Option<DateTime<Utc>>,
    /// The instant the run reached its terminal state.
    pub finished_at: Option<DateTime<Utc>>,
    /// `finished_at - started_at`, or elapsed-so-far once act spawned.
    pub duration_ms: Option<u64>,
    /// act's exit code, when it had one.
    pub exit_code: Option<u32>,
    /// The process group act ran in (equals its pid). Not a wire field — it exists so
    /// shutdown/cancel tests can assert the *group* is gone rather than trusting the
    /// receipt, and so an operator log line can name a pgid.
    pub pgid: Option<i32>,
    /// The *first* cancel request's instant, never overwritten (ADR-0004 §3.5).
    pub cancel_requested_at: Option<DateTime<Utc>>,
    /// Which initiator took the run down, if any.
    pub initiator: Option<TerminateInitiator>,
    /// The kill receipt, once there is one.
    pub kill: Option<KillReport>,
    /// `error` — present exactly when the status is `error`.
    pub error: Option<RunError>,
    /// Published evidence files, evidence-root-relative.
    pub evidence_files: Vec<String>,
    /// Records across those files.
    pub evidence_record_count: usize,
    /// The redacted log tail, already capped to `--log-tail-ring-bytes`.
    pub log_tail: String,
    /// Accumulated warning codes (open vocabulary).
    pub warnings: Vec<String>,
}

impl RunRecord {
    /// A freshly registered run: state `Queued`, nothing else known yet.
    #[must_use]
    pub fn new(run_id: impl Into<String>, identity: RunIdentity) -> Self {
        Self {
            run_id: run_id.into(),
            identity,
            state: RunState::Queued,
            registered_at: Utc::now(),
            started_at: None,
            finished_at: None,
            duration_ms: None,
            exit_code: None,
            pgid: None,
            cancel_requested_at: None,
            initiator: None,
            kill: None,
            error: None,
            evidence_files: Vec::new(),
            evidence_record_count: 0,
            log_tail: String::new(),
            warnings: Vec::new(),
        }
    }

    /// Move to `next`, refusing (and logging) anything §17.1 does not list.
    ///
    /// The refusal is not cosmetic: a duplicate or illegal transition means two code
    /// paths think they own one run, which is the bug this state machine exists to make
    /// impossible. It is logged at `warn` and ignored rather than panicking because a
    /// run's bookkeeping must not be able to take the server down.
    pub fn transition(&mut self, next: RunState) -> bool {
        if !self.state.can_transition_to(next) {
            tracing::warn!(
                run_id = %self.run_id,
                from = self.state.phase().as_str(),
                to = next.phase().as_str(),
                "refusing a run transition that ADR-0004 §17.1 does not list"
            );
            return false;
        }
        tracing::info!(
            run_id = %self.run_id,
            status = self.state.status().as_str(),
            phase = self.state.phase().as_str(),
            new_status = next.status().as_str(),
            new_phase = next.phase().as_str(),
            "run transition"
        );
        self.state = next;
        if next.is_terminal() {
            self.finished_at = Some(Utc::now());
        }
        true
    }

    /// Record a cancel request (ADR-0004 §3.5: the *first* instant wins, and this is the
    /// only field a `cancel_run` call may write).
    pub fn note_cancel_requested(&mut self) {
        if self.cancel_requested_at.is_none() {
            self.cancel_requested_at = Some(Utc::now());
        }
    }

    /// Stamp a warning code, keeping the list unique and capped (ADR-0001 §9: 64).
    pub fn add_warning(&mut self, code: &str) {
        if self.warnings.iter().any(|existing| existing == code) {
            return;
        }
        if self.warnings.len() >= MAX_WARNINGS {
            tracing::warn!(
                run_id = %self.run_id,
                warning = code,
                "warnings are at the contract cap; this one is not recorded"
            );
            return;
        }
        self.warnings.push(code.to_owned());
    }

    /// Whether a warning is stamped.
    #[must_use]
    pub fn has_warning(&self, code: &str) -> bool {
        self.warnings.iter().any(|warning| warning == code)
    }

    /// Elapsed wall time, following ADR-0004 §16.5: measured from `started_at`, and
    /// **absent** whenever act never spawned (there is no named origin clock before it).
    #[must_use]
    pub fn elapsed_ms(&self) -> Option<u64> {
        let started = self.started_at?;
        let end = self.finished_at.unwrap_or_else(Utc::now);
        let millis = (end - started).num_milliseconds();
        u64::try_from(millis)
            .ok()
            .filter(|value| *value <= MAX_DURATION_MS)
    }

    /// The `run_view` (ADR-0001 §3.1) of this record, with the log tail re-capped to what
    /// the caller asked for.
    #[must_use]
    pub fn view(&self, log_tail_bytes: usize) -> RunView {
        RunView {
            run_id: self.run_id.clone(),
            status: self.state.status(),
            phase: self.state.phase(),
            identity: self.identity.clone(),
            started_at: self.started_at,
            finished_at: self.finished_at,
            duration_ms: self.elapsed_ms(),
            exit_code: if self.state.status().is_terminal() {
                self.exit_code
            } else {
                None
            },
            evidence_files: self.evidence_files.clone(),
            evidence_record_count: self.evidence_record_count,
            log_tail: crate::runner::fit_log_tail(&self.log_tail, log_tail_bytes),
            warnings: self.warnings.clone(),
            error: self.error.clone(),
        }
    }

    /// The compact `run_summary` for `run_status` listing mode.
    #[must_use]
    pub fn summary(&self) -> RunSummary {
        RunSummary {
            run_id: self.run_id.clone(),
            status: self.state.status(),
            phase: self.state.phase(),
            identity: self.identity.clone(),
            started_at: self.started_at,
            duration_ms: self.elapsed_ms(),
        }
    }
}

/// `warnings` items per run (ADR-0001 §9).
pub const MAX_WARNINGS: usize = 64;
/// `duration_ms`'s schema bound (72 h).
pub const MAX_DURATION_MS: u64 = 259_200_000;

/// `run_view` — the canonical view of one run.
///
/// A typed struct rather than a hand-built `json!` so the absence rules the schema
/// enforces (terminal ⇒ `finished_at`; non-terminal ⇒ no `finished_at`, no `exit_code`,
/// no `error`; `error` ⇔ `status: "error"`) are structural: the fields are `Option`, and
/// [`RunView::to_value`] is the only place they become JSON.
#[derive(Debug, Clone)]
pub struct RunView {
    /// The run's id.
    pub run_id: String,
    /// Wire status.
    pub status: WireStatus,
    /// Display-only phase.
    pub phase: Phase,
    /// Identity tuple.
    pub identity: RunIdentity,
    /// When act spawned.
    pub started_at: Option<DateTime<Utc>>,
    /// When the run ended.
    pub finished_at: Option<DateTime<Utc>>,
    /// Wall duration / elapsed-so-far.
    pub duration_ms: Option<u64>,
    /// act's exit code.
    pub exit_code: Option<u32>,
    /// Published evidence files.
    pub evidence_files: Vec<String>,
    /// Records across them.
    pub evidence_record_count: usize,
    /// The capped, redacted log tail.
    pub log_tail: String,
    /// Warning codes.
    pub warnings: Vec<String>,
    /// The `error` payload, when the status is `error`.
    pub error: Option<RunError>,
}

impl RunView {
    /// Render `run_view`.
    ///
    /// Note the two ADR-0004 §16.1 rules baked in here: a mid-kill run is `running` +
    /// `cancelling` (a `run_view` cannot express a non-terminal `cancelled`), and a
    /// terminal run always carries `finished_at`.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut view = Map::new();
        view.insert("run_id".to_owned(), Value::String(self.run_id.clone()));
        view.insert(
            "status".to_owned(),
            Value::String(self.status.as_str().to_owned()),
        );
        view.insert(
            "terminal".to_owned(),
            Value::Bool(self.status.is_terminal()),
        );
        view.insert(
            "phase".to_owned(),
            Value::String(self.phase.as_str().to_owned()),
        );
        view.insert(
            "workflow".to_owned(),
            Value::String(self.identity.workflow.clone()),
        );
        view.insert(
            "workflow_file".to_owned(),
            Value::String(self.identity.workflow_file.clone()),
        );
        view.insert("task".to_owned(), Value::String(self.identity.task.clone()));
        view.insert(
            "category".to_owned(),
            Value::String(self.identity.category.clone()),
        );
        view.insert(
            "commit".to_owned(),
            Value::String(self.identity.commit.clone()),
        );
        insert_timestamp(&mut view, "started_at", self.started_at);
        insert_timestamp(&mut view, "finished_at", self.finished_at);
        if let Some(duration) = self.duration_ms {
            view.insert("duration_ms".to_owned(), Value::from(duration));
        }
        if let Some(code) = self.exit_code {
            view.insert("exit_code".to_owned(), Value::from(code));
        }
        view.insert(
            "evidence_files".to_owned(),
            Value::Array(
                self.evidence_files
                    .iter()
                    .take(MAX_EVIDENCE_FILES)
                    .map(|path| Value::String(path.clone()))
                    .collect(),
            ),
        );
        view.insert(
            "evidence_record_count".to_owned(),
            Value::from(self.evidence_record_count),
        );
        view.insert("log_tail".to_owned(), Value::String(self.log_tail.clone()));
        view.insert(
            "warnings".to_owned(),
            Value::Array(
                self.warnings
                    .iter()
                    .map(|warning| Value::String(warning.clone()))
                    .collect(),
            ),
        );
        // `error` is present exactly when the status says `error`; the schema forbids the
        // combination any other way (run_view's allOf).
        if let Some(error) = self
            .error
            .as_ref()
            .filter(|_| self.status == WireStatus::Error)
        {
            view.insert("error".to_owned(), error.to_value());
        }
        Value::Object(view)
    }
}

/// `run_summary` — the listing entry (no log, no evidence, on purpose).
#[derive(Debug, Clone)]
pub struct RunSummary {
    /// The run's id.
    pub run_id: String,
    /// Wire status.
    pub status: WireStatus,
    /// Display-only phase.
    pub phase: Phase,
    /// Identity tuple.
    pub identity: RunIdentity,
    /// When act spawned.
    pub started_at: Option<DateTime<Utc>>,
    /// Duration/elapsed, when act spawned.
    pub duration_ms: Option<u64>,
}

impl RunSummary {
    /// Render `run_summary`.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut summary = Map::new();
        summary.insert("run_id".to_owned(), Value::String(self.run_id.clone()));
        summary.insert(
            "status".to_owned(),
            Value::String(self.status.as_str().to_owned()),
        );
        summary.insert(
            "terminal".to_owned(),
            Value::Bool(self.status.is_terminal()),
        );
        summary.insert(
            "phase".to_owned(),
            Value::String(self.phase.as_str().to_owned()),
        );
        summary.insert(
            "workflow".to_owned(),
            Value::String(self.identity.workflow.clone()),
        );
        summary.insert("task".to_owned(), Value::String(self.identity.task.clone()));
        summary.insert(
            "category".to_owned(),
            Value::String(self.identity.category.clone()),
        );
        summary.insert(
            "commit".to_owned(),
            Value::String(self.identity.commit.clone()),
        );
        insert_timestamp(&mut summary, "started_at", self.started_at);
        if let Some(duration) = self.duration_ms {
            summary.insert("duration_ms".to_owned(), Value::from(duration));
        }
        Value::Object(summary)
    }
}

/// `evidence_files` items per run (ADR-0001 §9).
pub const MAX_EVIDENCE_FILES: usize = 8;

/// `cancel_run_result` — a receipt for one cancel request, not a second run view.
///
/// The two views of the mid-kill moment (ADR-0004 §16.1) are deliberate: `run_status`
/// says `running`/`cancelling`/`terminal: false` because it is the polling surface, and
/// this says `cancelled`/`terminal: false` because it is the answer to "did my cancel
/// take". §5.2's table is implemented by [`crate::registry::RunSupervisor::cancel`].
#[derive(Debug, Clone)]
pub struct CancelReceipt {
    /// The run's id.
    pub run_id: String,
    /// Status the receipt reports (ADR-0004 §5.2's second column).
    pub status: WireStatus,
    /// Whether the run is terminal as of this reply.
    pub terminal: bool,
    /// Whether the run was already terminal when the call arrived (idempotent no-op).
    pub already_terminal: bool,
    /// The first cancel request's instant; absent exactly when `already_terminal`.
    pub cancel_requested_at: Option<DateTime<Utc>>,
    /// The kill receipt; absent for `already_terminal`, for a teardown still in flight,
    /// and for a no-op cancel past the cancel window.
    pub kill: Option<KillReport>,
    /// Warnings, which carry `already_terminal` on the two no-op rows of §5.2.
    pub warnings: Vec<String>,
}

impl CancelReceipt {
    /// Render `cancel_run_result`.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut receipt = Map::new();
        receipt.insert("run_id".to_owned(), Value::String(self.run_id.clone()));
        receipt.insert(
            "status".to_owned(),
            Value::String(self.status.as_str().to_owned()),
        );
        receipt.insert("terminal".to_owned(), Value::Bool(self.terminal));
        receipt.insert(
            "already_terminal".to_owned(),
            Value::Bool(self.already_terminal),
        );
        // The schema requires `cancel_requested_at` iff `already_terminal` is false, and
        // forbids `kill` alongside `already_terminal: true`.
        if !self.already_terminal {
            insert_timestamp(
                &mut receipt,
                "cancel_requested_at",
                Some(self.cancel_requested_at.unwrap_or_else(Utc::now)),
            );
            if let Some(kill) = self.kill {
                receipt.insert("kill".to_owned(), kill.to_value());
            }
        }
        receipt.insert(
            "warnings".to_owned(),
            Value::Array(
                self.warnings
                    .iter()
                    .take(MAX_WARNINGS)
                    .map(|warning| Value::String(warning.clone()))
                    .collect(),
            ),
        );
        Value::Object(receipt)
    }
}

/// Warning stamped when a `cancel_run` arrived that could do nothing (ADR-0004 §5.6).
pub const WARNING_ALREADY_TERMINAL: &str = "already_terminal";
/// Warning stamped when a caller cancel discarded a staged payload (ADR-0004 §6.2).
pub const WARNING_PARTIAL_EVIDENCE_DROPPED: &str = "partial_evidence_dropped";
/// Warning stamped when a run read more payload than `--max-payload-keep-bytes` would fit,
/// so part of it was deleted instead of kept under `runs/<run_id>/payload/ (ADR-0002 §3.4).
pub const WARNING_PAYLOAD_CAP_EXCEEDED: &str = "payload_cap_exceeded";

/// `canonical_timestamp` is `$defs/timestamp`: millisecond precision, `Z`, no offset.
fn insert_timestamp(map: &mut Map<String, Value>, key: &str, instant: Option<DateTime<Utc>>) {
    if let Some(instant) = instant {
        map.insert(
            key.to_owned(),
            Value::String(instant.to_rfc3339_opts(SecondsFormat::Millis, true)),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use RunState::{
        ActRunning, ActStarting, Cancelled, Cancelling, Error, Failed, Harvesting, PreparingTree,
        Publishing, Queued, Success, TimedOut,
    };

    /// §13: "every transition in §3.3 reachable and terminal states final", as data.
    #[test]
    fn the_transition_table_is_exactly_adr_0004_17_1() {
        let allowed: &[(RunState, &[RunState])] = &[
            (Queued, &[PreparingTree, Cancelled, Error]),
            (PreparingTree, &[ActStarting, Cancelled, Error]),
            (ActStarting, &[ActRunning, Cancelled, Error]),
            (ActRunning, &[Harvesting, Cancelling]),
            (Cancelling, &[Cancelled, Harvesting]),
            (Harvesting, &[Publishing, Error]),
            (Publishing, &[Success, Failed, Cancelled, TimedOut, Error]),
        ];
        let all = [
            Queued,
            PreparingTree,
            ActStarting,
            ActRunning,
            Cancelling,
            Harvesting,
            Publishing,
            Success,
            Failed,
            Cancelled,
            TimedOut,
            Error,
        ];
        for from in all {
            let expected: Vec<RunState> = allowed
                .iter()
                .find(|(state, _)| *state == from)
                .map(|(_, targets)| targets.to_vec())
                .unwrap_or_default();
            for to in all {
                assert_eq!(
                    from.can_transition_to(to),
                    expected.contains(&to),
                    "{from:?} → {to:?} disagreed with §17.1"
                );
            }
        }
    }

    /// T20, spelled out: no terminal state has any outgoing edge.
    #[test]
    fn terminal_states_are_final() {
        for terminal in [Success, Failed, Cancelled, TimedOut, Error] {
            assert!(terminal.is_terminal());
            assert!(!terminal.cancel_window_open());
            for next in [
                Success, Failed, Cancelled, TimedOut, Error, Queued, Harvesting,
            ] {
                assert!(!terminal.can_transition_to(next), "{terminal:?} → {next:?}");
            }
        }
    }

    /// The §17.1 projection table, row by row.
    #[test]
    fn the_wire_projection_is_pinned() {
        let table: &[(RunState, &str, &str)] = &[
            (Queued, "pending", "queued"),
            (PreparingTree, "pending", "preparing_tree"),
            (ActStarting, "pending", "starting_act"),
            (ActRunning, "running", "running"),
            (Cancelling, "running", "cancelling"),
            (Harvesting, "running", "extracting_evidence"),
            (Publishing, "running", "publishing_evidence"),
            (Success, "success", "done"),
            (Failed, "failed", "done"),
            (Cancelled, "cancelled", "done"),
            (TimedOut, "timed_out", "done"),
            (Error, "error", "done"),
        ];
        for (state, status, phase) in table {
            assert_eq!(state.status().as_str(), *status);
            assert_eq!(state.phase().as_str(), *phase);
        }
    }

    /// A `run_view` cannot lie about terminal/finished_at/error (the schema's allOf).
    #[test]
    fn run_views_satisfy_the_schema_invariants() {
        let mut record = RunRecord::new("20261110T004203Z-a1b2c3", identity());
        let pending = record.view(64);
        let value = pending.to_value();
        assert_eq!(value["status"], "pending");
        assert_eq!(value["terminal"], false);
        assert!(value.get("finished_at").is_none());
        assert!(value.get("exit_code").is_none());
        assert!(value.get("error").is_none());
        assert!(
            value.get("duration_ms").is_none(),
            "no origin clock before act"
        );

        record.started_at = Some(Utc::now());
        record.exit_code = Some(3);
        record.error = Some(RunError::stage(
            "extracting_evidence",
            "extraction hung",
            true,
        ));
        assert!(spawned_to_act_running(&mut record));
        let running = record.view(64).to_value();
        assert_eq!(running["status"], "running");
        assert!(
            running.get("error").is_none(),
            "non-terminal must not carry error"
        );
        assert!(running.get("exit_code").is_none());
        assert!(running["duration_ms"].is_number());

        assert!(record.transition(RunState::Harvesting));
        assert!(record.transition(RunState::Publishing));
        assert!(record.transition(RunState::Error));
        let errored = record.view(64).to_value();
        assert_eq!(errored["status"], "error");
        assert_eq!(errored["terminal"], true);
        assert_eq!(errored["phase"], "done");
        assert!(errored.get("finished_at").is_some());
        assert_eq!(errored["error"]["code"], "runner_internal_error");
        assert_eq!(errored["error"]["detail"]["stage"], "extracting_evidence");
    }

    /// A cancel that arrives after the terminal commit changes nothing (T20).
    #[test]
    fn a_late_cancel_changes_no_state() {
        let mut record = RunRecord::new("20261110T004203Z-a1b2c3", identity());
        assert!(spawned_to_act_running(&mut record));
        assert!(record.transition(RunState::Harvesting));
        assert!(record.transition(RunState::Publishing));
        assert!(record.transition(RunState::Success));
        let before = record.clone();
        assert!(!record.transition(RunState::Cancelled));
        assert_eq!(record.state, before.state);
        assert_eq!(record.warnings, before.warnings);
    }

    /// `cancel_requested_at` keeps the first instant, which is what makes a second
    /// `cancel_run` honest (ADR-0004 §3.5).
    #[tokio::test]
    async fn the_first_cancel_request_owns_the_timestamp() {
        let mut record = RunRecord::new("20261110T004203Z-a1b2c3", identity());
        record.note_cancel_requested();
        let first = record.cancel_requested_at.expect("set");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        record.note_cancel_requested();
        assert_eq!(record.cancel_requested_at.expect("still first"), first);
    }

    /// Walk the legal path from `Queued` to `ActRunning`, which is what "act spawned" means.
    fn spawned_to_act_running(record: &mut RunRecord) -> bool {
        record.started_at = Some(Utc::now());
        record.transition(RunState::PreparingTree)
            && record.transition(RunState::ActStarting)
            && record.transition(RunState::ActRunning)
    }

    fn identity() -> RunIdentity {
        RunIdentity::new(
            "ci",
            ".github/workflows/ci.yml",
            "build",
            "other",
            "2f7c1a4b9e0d3c6a5b8f1e2d4c6a8b0f3e5d7c91",
        )
    }
}
