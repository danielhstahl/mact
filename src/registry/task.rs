//! The run task: ADR-0004 §3.4's terminal commit, driven by one task per run.
//!
//! One task owns one run from `Queued` to its terminal commit, and that ownership is the
//! implementation of "only the run's own task writes state" (§3.5). Everything a caller can
//! do to a run from outside arrives here as a *flag* ([`super::Slot::cancel`], the shutdown
//! token) or as the runner's own teardown receipt, and is sampled at the three points §17.1
//! names: after `exec` returns, on child exit, and before extraction.
//!
//! ```text
//!   Queued ──► PreparingTree ──► ActStarting ──► ActRunning ──► Harvesting ──► Publishing ──► label
//!      │             │                │              │  ▲            │  timeout   │  publish fail
//!      │ cancel      │ cancel         │ cancel       │  └─Cancelling ┘            ▼
//!      ▼             ▼                ▼              └──cancel/deadline/shutdown  Error
//!   Cancelled     Cancelled       Cancelled                                       (meta.json still written)
//! ```
//!
//! The runner is doing the waiting for act, the deadline, and the process-group kill
//! (mact-f1w.8). This task's own waits are the two budgets §11 gives it — the pre-start
//! budget around tree prep and `exec`, and the harvest budget around extraction and
//! publication — plus reacting to cancel/shutdown while act is alive.

use std::{
    io::Write as _,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};

use crate::{
    evidence::{writer::Published, EvidenceRoot, RunRecords},
    registry::{LockBody, RunError, RunState, WireStatus},
    runner::{ActRunHandle, ActRunOutcome, KillReport, RunFailure, RunStatus, TerminateInitiator},
};
use chrono::{SecondsFormat, Utc};
use serde_json::{Map, Value};

use super::{
    Inner, RunPlan, RunRecord, Slot, WARNING_PARTIAL_EVIDENCE_DROPPED, WARNING_PAYLOAD_CAP_EXCEEDED,
};

/// Where a consumed payload file is kept: `runs/<run_id>/payload/`.
const PAYLOAD_DIRNAME: &str = "payload";

/// The budget for a terminal path where nothing was ever read (a cancel, a failed harvest):
/// there is nothing to keep, so nothing is moved.
const NO_RETENTION: u64 = 0;

/// A boxed future, so [`Pipeline`] can be a plain `dyn` trait without a `futures` dependency.
pub type BoxFuture<'a, T> = Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// What a pipeline is allowed to know about a run: identity, paths and budgets, nothing
/// mutable. The record stays the supervisor's.
#[derive(Debug, Clone)]
pub struct RunContext {
    /// The run's id.
    pub run_id: String,
    /// Identity tuple as the wire reports it.
    pub identity: crate::registry::RunIdentity,
    /// The tree act was pointed at (workspace in `head_only`, a worktree in `worktree`).
    pub tree: PathBuf,
    /// The workspace the evidence root belongs to.
    pub workspace: PathBuf,
    /// The evidence root.
    pub evidence_dir: PathBuf,
    /// `.evidence/runs/<run_id>/`.
    pub run_dir: PathBuf,
    /// `$ACTMCP_EVIDENCE_OUT` — the per-run payload scratch.
    pub payload_dir: PathBuf,
    /// The scratch dir act ran in (also the worktree's parent in ADR-0003 worktree mode).
    pub scratch_dir: PathBuf,
    /// The run's act budget, echoed for logging.
    pub timeout: Duration,
}

impl RunContext {
    fn for_plan(plan: &RunPlan) -> Self {
        Self {
            run_id: plan.spec.run_id.as_str().to_owned(),
            identity: plan.identity.clone(),
            tree: plan.spec.tree.clone(),
            workspace: plan.spec.workspace.clone(),
            evidence_dir: plan.spec.evidence_dir.clone(),
            run_dir: plan.spec.run_dir.clone(),
            payload_dir: plan.spec.payload_dir.clone(),
            scratch_dir: plan.spec.scratch_dir.clone(),
            timeout: plan.spec.timeout,
        }
    }
}

/// What a pipeline harvested (§3.4 step 2).
#[derive(Debug, Default)]
pub struct Harvest {
    /// Record batches to publish, one per evidence `kind`. An empty vec is a legitimate
    /// "ran, found nothing" and is what an unknown category always produces.
    pub batches: Vec<RunRecords>,
    /// Warning codes to add to the run (ADR-0002 §5.1: extraction faults are warnings).
    pub warnings: Vec<String>,
    /// Well-formed-but-unusable inputs seen, for `meta.json.evidence[].malformed`.
    pub malformed: usize,
    /// Payload files the extractors consumed; the rest of `incoming/<run_id>/` is kept and
    /// listed in `meta.json` (ADR-0002 §3.4).
    pub consumed: Vec<PathBuf>,
}

/// Why a pipeline step failed. Text is operator-facing and may name paths; the wire gets a
/// sanitised `runner_internal_error` message built by the caller.
#[derive(Debug, Clone)]
pub enum PipelineError {
    /// Tree materialisation failed (ADR-0003 §5.5 → `detail.stage: "preparing_tree"`).
    Tree(String),
    /// Extraction blew up rather than finding nothing (§3.3 T17).
    Harvest(String),
}

impl std::fmt::Display for PipelineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tree(reason) => write!(f, "could not prepare the run's tree: {reason}"),
            Self::Harvest(reason) => write!(f, "evidence extraction failed: {reason}"),
        }
    }
}

impl std::error::Error for PipelineError {}

/// The category-specific half of a run, supplied by mact-f1w.14.
///
/// Every method has a default so a pipeline implements only what it has opinions about, and
/// every method is called from inside a budget the supervisor owns: a pipeline that hangs
/// produces a timed-out run, not a wedged server.
pub trait Pipeline: Send + Sync + 'static {
    /// ADR-0003 tree materialisation (worktree mode). Default: the tree in `plan` is the
    /// tree to run, which is the `head_only` answer.
    fn prepare(&self, _context: &RunContext) -> BoxFuture<'_, Result<(), PipelineError>> {
        Box::pin(async { Ok(()) })
    }

    /// §3.4 step 2: turn a finished run into record batches. The `label` is the terminal
    /// status the run is about to take, because a `timed_out` run's records must say so
    /// (ADR-0004 §6.2).
    fn harvest(
        &self,
        _context: &RunContext,
        _outcome: &ActRunOutcome,
        _label: WireStatus,
    ) -> BoxFuture<'_, Result<Harvest, PipelineError>> {
        Box::pin(async { Ok(Harvest::default()) })
    }

    /// §3.4 step 6 extras (worktree removal and friends). Runs after the terminal commit.
    fn release(&self, _context: &RunContext) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}

/// The default pipeline: no extractors, so every run publishes no evidence and its
/// `meta.json` says so with an empty `evidence[]`.
///
/// That is not a stub — it is the correct behaviour for a category with no extractor
/// (ADR-0001 §2.4: an unknown category "runs and produces no evidence"), and it is what
/// lets the supervisor be tested end to end with a stub `act` and no docker.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoEvidence;

impl Pipeline for NoEvidence {}

/// The runner handle for live log tails (see [`super::RunSupervisor::status`]).
pub(crate) fn live_handle(runner: &Arc<Mutex<Option<ActRunHandle>>>) -> Option<ActRunHandle> {
    runner
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// Drive one run to its terminal commit.
///
/// Panics are caught at the `JoinHandle` and reported: a run whose task died must not stay
/// `running` forever, and it must release the workspace lock, because a workspace wedged by
/// a panic is indistinguishable from a workspace with a run in it.
pub(crate) async fn drive(inner: Arc<Inner>, plan: RunPlan, slot: Arc<Slot>) {
    let run_id = plan.spec.run_id.as_str().to_owned();
    let context = RunContext::for_plan(&plan);
    let outcome = run(&inner, &plan, &slot, &context).await;
    if let Err(error) = outcome {
        tracing::error!(run_id = %run_id, "the run task failed: {error}");
    }
    // §3.4 step 6, on every path including a panic-free early return: the worktree and the
    // lock go after the terminal commit, never before it.
    release(&inner, &slot, &context).await;
    drop(plan);
    inner.prune().await;
}

/// Run everything §3.4 asks of the run task, returning only when the terminal commit and
/// the release are done.
async fn run(
    inner: &Arc<Inner>,
    plan: &RunPlan,
    slot: &Arc<Slot>,
    context: &RunContext,
) -> Result<(), String> {
    let policy = &inner.policy;
    let mut record = slot.current().as_ref().clone();
    let prestart_deadline = tokio::time::Instant::now() + policy.prestart_timeout;
    let prestart_left = || prestart_deadline.saturating_duration_since(tokio::time::Instant::now());

    // ── Queued ────────────────────────────────────────────────────────────────────────
    // A cancel that lands before anything started is T2: nothing was signalled, so the
    // receipt is `kill: {method: "none"}`, and the run still writes a `meta.json` (§3.4).
    if slot.cancel.is_cancelled() {
        return commit_without_act(inner, slot, context, &mut record, RunState::Cancelled, None)
            .await;
    }
    record.transition(RunState::PreparingTree);
    publish(inner, slot, &mut record);
    rewrite_lock(inner, slot, &record);

    // ── PreparingTree (T1 → T4/T5/T6) ────────────────────────────────────────────────
    match tokio::time::timeout(prestart_left(), inner.pipeline.prepare(context)).await {
        Err(_elapsed) => {
            let error = RunError::stage(
                "preparing_tree",
                "the pre-start budget expired before act could be spawned",
                true,
            );
            return commit_without_act(
                inner,
                slot,
                context,
                &mut record,
                RunState::Error,
                Some(error),
            )
            .await;
        }
        Ok(Err(PipelineError::Tree(reason))) | Ok(Err(PipelineError::Harvest(reason))) => {
            tracing::warn!(run_id = %record.run_id, "tree preparation failed: {reason}");
            let error = RunError::stage("preparing_tree", reason, true);
            return commit_without_act(
                inner,
                slot,
                context,
                &mut record,
                RunState::Error,
                Some(error),
            )
            .await;
        }
        Ok(Ok(())) => {}
    }
    if slot.cancel.is_cancelled() {
        slot.note_cancel();
        return commit_without_act(inner, slot, context, &mut record, RunState::Cancelled, None)
            .await;
    }

    // ── ActStarting (T4 → T7/T8/T9) ──────────────────────────────────────────────────
    record.transition(RunState::ActStarting);
    publish(inner, slot, &mut record);
    rewrite_lock(inner, slot, &record);
    let budget = prestart_left();
    if budget.is_zero() {
        let error = RunError::stage(
            "starting_act",
            "the pre-start budget expired before act could be spawned",
            true,
        );
        return commit_without_act(
            inner,
            slot,
            context,
            &mut record,
            RunState::Error,
            Some(error),
        )
        .await;
    }
    let running = match tokio::time::timeout(budget, inner.runner.start(plan.spec.clone())).await {
        Err(_elapsed) => {
            let error = RunError::stage(
                "starting_act",
                "act could not be spawned within the pre-start budget",
                true,
            );
            return commit_without_act(
                inner,
                slot,
                context,
                &mut record,
                RunState::Error,
                Some(error),
            )
            .await;
        }
        Ok(Err(spawn_failure)) => {
            tracing::warn!(run_id = %record.run_id, "act could not be spawned: {spawn_failure}");
            return commit_without_act(
                inner,
                slot,
                context,
                &mut record,
                RunState::Error,
                Some(spawn_error(&spawn_failure)),
            )
            .await;
        }
        Ok(Ok(running)) => running,
    };

    let argv = running.argv().to_vec();
    record.started_at = Some(running.started_at());
    // Recorded at spawn, not at reap: `run_status` and the shutdown sweep need to be able to
    // name the process group while act is still up (and a run cancelled before this point has
    // no pgid at all, which is what `kill: {method: "none"}` means).
    record.pgid = Some(running.pgid());
    *slot
        .runner
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(running.handle());

    // ── ActRunning ────────────────────────────────────────────────────────────────────
    record.transition(RunState::ActRunning);
    publish(inner, slot, &mut record);
    rewrite_lock(inner, slot, &record);

    let handle = running.handle();
    let act_budget = plan.spec.timeout;
    let mut wait = std::pin::pin!(running.wait());
    let mut cancel_armed = true;
    let mut shutdown_armed = true;
    let outcome = loop {
        tokio::select! {
            biased;
            outcome = &mut wait => break outcome,
            // `spec.cancellation` is *not* armed (see `RunSupervisor::start`), so this branch
            // is the only teardown this run can get while act is alive: awaiting `terminate`
            // here parks `wait()`, which is safe only because nothing else holds that mutex.
            _ = slot.cancel.cancelled(), if cancel_armed => {
                cancel_armed = false;
                // T11: the *request* is ours to record; the state change and the signal
                // happen here, in the run's own task (§3.5).
                slot.note_cancel();
                if record.state == RunState::ActRunning {
                    record.transition(RunState::Cancelling);
                    publish(inner, slot, &mut record);
                }
                handle.terminate(TerminateInitiator::Caller).await;
            }
            _ = inner.shutdown.cancelled(), if shutdown_armed => {
                shutdown_armed = false;
                if record.state == RunState::ActRunning {
                    record.transition(RunState::Cancelling);
                    publish(inner, slot, &mut record);
                }
                handle.terminate(TerminateInitiator::Shutdown).await;
            }
        }
    };
    let _ = act_budget; // the deadline itself belongs to the runner (§4.1)
    finish_act(inner, slot, context, &mut record, &argv, &outcome).await
}

/// Everything after act has been reaped: the label, the harvest, the publish, the manifest,
/// and the registry write — in §3.4's order, in one code path.
#[allow(clippy::too_many_lines, reason = "this is §3.4 written top to bottom")]
async fn finish_act(
    inner: &Arc<Inner>,
    slot: &Arc<Slot>,
    context: &RunContext,
    record: &mut RunRecord,
    argv: &[String],
    outcome: &ActRunOutcome,
) -> Result<(), String> {
    let policy = &inner.policy;
    let label = WireStatus::from_run_status(outcome.status);
    // A teardown that the runner performed on its own (the deadline, or a cancel that fired
    // while this task was between states) still has to show as `Cancelling` first: the kill
    // happened, and the wire must not pretend the run went straight from act to a label.
    if record.state == RunState::ActRunning && outcome.initiator.is_some() {
        record.transition(RunState::Cancelling);
        publish(inner, slot, record);
    }
    record.exit_code = outcome.exit_code;
    record.pgid = Some(outcome.pgid);
    record.kill = outcome.kill;
    record.initiator = outcome.initiator;
    record.log_tail = crate::runner::fit_log_tail(&outcome.log_tail, policy.log_tail_ring_bytes);
    for warning in &outcome.warnings {
        record.add_warning(warning);
    }
    if outcome.status == RunStatus::Error {
        record.error = outcome.failure.as_ref().map(run_failure_error);
    }

    // ── A caller cancel skips extraction, deliberately (§6.2) ─────────────────────────
    if label == WireStatus::Cancelled {
        let incoming = clean_payload(context, &[], NO_RETENTION).await;
        let temp_left = staged_publish_temp(context);
        if incoming.had_files || temp_left {
            record.add_warning(WARNING_PARTIAL_EVIDENCE_DROPPED);
        }
        write_manifest(
            context,
            record,
            WireStatus::Cancelled,
            Some(outcome),
            argv,
            &[],
            &incoming,
            0,
        )?;
        force_terminal(record, RunState::Cancelled);
        publish(inner, slot, record);
        return Ok(());
    }

    // ── Harvesting (T10/T15 → T16/T17) ────────────────────────────────────────────────
    record.transition(RunState::Harvesting);
    publish(inner, slot, record);
    let harvest = match tokio::time::timeout(
        policy.harvest_timeout,
        inner.pipeline.harvest(context, outcome, label),
    )
    .await
    {
        Err(_elapsed) => {
            tracing::warn!(
                run_id = %record.run_id,
                budget = ?policy.harvest_timeout,
                "evidence extraction did not finish inside the harvest budget"
            );
            let error = RunError::stage(
                "extracting_evidence",
                "evidence extraction did not finish inside the harvest budget",
                true,
            );
            let incoming = clean_payload(context, &[], NO_RETENTION).await;
            write_manifest(
                context,
                record,
                WireStatus::Error,
                Some(outcome),
                argv,
                &[],
                &incoming,
                0,
            )?;
            force_terminal(record, RunState::Error);
            record.error = Some(error);
            publish(inner, slot, record);
            return Ok(());
        }
        Ok(Err(PipelineError::Harvest(reason))) | Ok(Err(PipelineError::Tree(reason))) => {
            tracing::warn!(run_id = %record.run_id, "evidence extraction failed: {reason}");
            let mut error = RunError::stage("extracting_evidence", reason, true);
            error.detail.insert(
                "stage".to_owned(),
                Value::String("extracting_evidence".to_owned()),
            );
            record.error = Some(error);
            let incoming = clean_payload(context, &[], NO_RETENTION).await;
            write_manifest(
                context,
                record,
                WireStatus::Error,
                Some(outcome),
                argv,
                &[],
                &incoming,
                0,
            )?;
            force_terminal(record, RunState::Error);
            publish(inner, slot, record);
            return Ok(());
        }
        Ok(Ok(harvest)) => harvest,
    };
    for warning in &harvest.warnings {
        record.add_warning(warning);
    }

    // ── Publishing (T18/T19) ──────────────────────────────────────────────────────────
    record.transition(RunState::Publishing);
    publish(inner, slot, record);
    let published = match publish_batches(context, &harvest.batches).await {
        Ok(published) => published,
        Err(failure) => {
            // A published file is never rolled back (§6.4): whatever did land stands and is
            // reported, and the run is `error` because the manifest cannot promise what it
            // cannot vouch for.
            record.error = Some(failure);
            let incoming =
                clean_payload(context, &harvest.consumed, policy.max_payload_keep_bytes).await;
            if incoming.dropped > 0 {
                record.add_warning(WARNING_PAYLOAD_CAP_EXCEEDED);
            }
            write_manifest(
                context,
                record,
                WireStatus::Error,
                Some(outcome),
                argv,
                &[],
                &incoming,
                harvest.malformed,
            )?;
            force_terminal(record, RunState::Error);
            publish(inner, slot, record);
            return Ok(());
        }
    };
    let relative = published
        .iter()
        .filter_map(|entry| entry.relative.clone())
        .collect::<Vec<_>>();
    record.evidence_record_count = published.iter().map(|entry| entry.records).sum();
    record.evidence_files = relative;

    let incoming = clean_payload(context, &harvest.consumed, policy.max_payload_keep_bytes).await;
    if incoming.dropped > 0 {
        record.add_warning(WARNING_PAYLOAD_CAP_EXCEEDED);
    }
    let manifest_result = write_manifest(
        context,
        record,
        label,
        Some(outcome),
        argv,
        &published,
        &incoming,
        harvest.malformed,
    );
    if let Err(reason) = manifest_result {
        // T19: `meta.json` failing is `evidence_write_failed`, the JSONL stays, and the run
        // is terminal — ADR-0002 §1.3's "no meta.json ⇒ suspect that directory".
        tracing::error!(run_id = %record.run_id, "could not write meta.json: {reason}");
        record.error = Some(RunError::new(
            "evidence_write_failed",
            "act finished but the run's manifest could not be written",
            false,
            Map::from_iter([(
                "path".to_owned(),
                Value::String(format!("{}/meta.json", context.run_dir.display())),
            )]),
        ));
        force_terminal(record, RunState::Error);
    } else {
        force_terminal(record, terminal_state(label));
    }
    publish(inner, slot, record);
    Ok(())
}

/// Commit a run that never got an act process: `Queued`/`PreparingTree`/`ActStarting` to
/// `Cancelled` or `Error`, with a `meta.json` (§3.4 step 4: *every* terminal run writes one,
/// including one cancelled before `act` was spawned).
async fn commit_without_act(
    inner: &Arc<Inner>,
    slot: &Arc<Slot>,
    context: &RunContext,
    record: &mut RunRecord,
    terminal: RunState,
    error: Option<RunError>,
) -> Result<(), String> {
    if terminal == RunState::Cancelled {
        // No process ever existed, so the receipt is `kill: {method: "none"}` (§16.2) —
        // present, not absent, and never a made-up `took_ms`.
        record.kill = Some(KillReport::none());
        record.initiator = Some(TerminateInitiator::Caller);
        record.cancel_requested_at = Some(record.cancel_requested_at.unwrap_or_else(Utc::now));
    }
    record.error = error;
    // Payload scratch is cleaned on every terminal path (§6.3), even a pre-spawn cancel.
    let incoming = clean_payload(context, &[], NO_RETENTION).await;
    if incoming.had_files && terminal == RunState::Cancelled {
        record.add_warning(WARNING_PARTIAL_EVIDENCE_DROPPED);
    }
    write_manifest(
        context,
        record,
        terminal.status(),
        None,
        &[],
        &[],
        &incoming,
        0,
    )?;
    force_terminal(record, terminal);
    publish(inner, slot, record);
    Ok(())
}

/// Publish the snapshot and keep the workspace lock body honest (§6.3: the body is
/// rewritten whenever the holder's phase changes).
fn publish(inner: &Arc<Inner>, slot: &Arc<Slot>, record: &mut RunRecord) {
    rewrite_lock(inner, slot, record);
    slot.publish(record.clone());
}

fn rewrite_lock(inner: &Arc<Inner>, slot: &Arc<Slot>, record: &RunRecord) {
    let body = LockBody::for_run(
        &record.run_id,
        record.state.phase().as_str(),
        record.started_at,
    );
    let mut workspaces = inner
        .workspaces
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((_, held)) = workspaces.get_mut(&slot.workspace) {
        held.rewrite(&body);
    }
}

/// §3.4 step 6: pipeline cleanup (worktree removal), then the workspace lock. After this a
/// second `run_workflow` for this workspace is acceptable again.
async fn release(inner: &Arc<Inner>, slot: &Arc<Slot>, context: &RunContext) {
    inner.pipeline.release(context).await;
    let released = inner
        .workspaces
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(&slot.workspace);
    if let Some((run_id, _)) = released {
        tracing::debug!(run_id = %run_id, "released the workspace run lock");
    }
}

/// The terminal state a label names.
fn terminal_state(label: WireStatus) -> RunState {
    match label {
        WireStatus::Success => RunState::Success,
        WireStatus::Failed => RunState::Failed,
        WireStatus::Cancelled => RunState::Cancelled,
        WireStatus::TimedOut => RunState::TimedOut,
        WireStatus::Error | WireStatus::Pending | WireStatus::Running => RunState::Error,
    }
}

/// Commit a terminal state, repairing (loudly) a transition §17.1 refused.
///
/// A refused transition means two code paths thought they owned one run. Failing to go
/// terminal would leave the run `running` forever — unrecoverable for the caller and
/// wedging the workspace — so the state is forced and the bug is shouted about.
fn force_terminal(record: &mut RunRecord, state: RunState) {
    record.error = record.error.take().filter(|_| state == RunState::Error);
    if !record.transition(state) {
        tracing::error!(
            run_id = %record.run_id,
            from = record.state.phase().as_str(),
            to = state.phase().as_str(),
            "forcing a terminal state after a refused transition: ADR-0004 §17.1 was violated \
             upstream, which is a bug worth a ticket"
        );
        record.state = state;
        record.finished_at = Some(Utc::now());
    }
}

/// Map a runner spawn failure onto its `run_view.error` (ADR-0001 §5.3's runner rows).
fn spawn_error(failure: &crate::runner::RunnerError) -> RunError {
    match failure {
        crate::runner::RunnerError::Spawn {
            program, reason, ..
        } => RunError::new(
            "act_spawn_failed",
            format!("act (`{program}`) could not be started: {reason}"),
            true,
            Map::from_iter([
                ("reason".to_owned(), Value::String(reason.clone())),
                ("stage".to_owned(), Value::String("starting_act".to_owned())),
            ]),
        ),
        other => RunError::stage("starting_act", other.to_string(), true),
    }
}

/// Map the runner's terminal failure (`act_crashed`, server shutdown) onto `run_view.error`.
fn run_failure_error(failure: &RunFailure) -> RunError {
    RunError::new(
        failure.code(),
        failure.message(),
        true,
        failure.detail().as_object().cloned().unwrap_or_default(),
    )
}

/// One published evidence file, in the spellings `run_view` and `meta.json` want.
#[derive(Debug, Clone)]
struct PublishedEntry {
    kind: &'static str,
    relative: Option<String>,
    records: usize,
    truncated: bool,
}

/// §3.4 step 3: publish every batch through the ADR-0002 writer (temp file → `fsync` →
/// `rename`), which is what makes "the run is terminal" and "the file is there" one question.
async fn publish_batches(
    context: &RunContext,
    batches: &[RunRecords],
) -> Result<Vec<PublishedEntry>, RunError> {
    if batches.is_empty() {
        return Ok(Vec::new());
    }
    let root = EvidenceRoot::new(&context.evidence_dir).map_err(|error| {
        RunError::new(
            "evidence_write_failed",
            format!("the evidence root could not be prepared: {error}"),
            false,
            Map::from_iter([(
                "path".to_owned(),
                Value::String(context.evidence_dir.display().to_string()),
            )]),
        )
    })?;
    let mut published = Vec::new();
    for batch in batches {
        if batch.is_empty() {
            continue;
        }
        match root.publish(batch).await {
            Ok(Some(entry)) => published.push(PublishedEntry {
                kind: kind_word(&entry),
                relative: evidence_relative(&root, &context.workspace, &entry.path),
                records: entry.records,
                truncated: entry.truncated,
            }),
            // An empty batch publishes nothing; that is ADR-0002's "ran, found nothing".
            Ok(None) => {}
            Err(error) => {
                return Err(evidence_failure(&error));
            }
        }
    }
    Ok(published)
}

fn kind_word(entry: &Published) -> &'static str {
    match entry.kind {
        crate::evidence::Kind::Benchmark => "benchmark",
        crate::evidence::Kind::Coverage => "coverage",
    }
}

/// The workspace-relative spelling `run_view.evidence_files` advertises
/// (`.evidence/coverage/<sha>/<run>.jsonl`); for an evidence root outside the workspace
/// there is no honest workspace-relative path, so the root-relative one is used.
fn evidence_relative(root: &EvidenceRoot, workspace: &Path, path: &Path) -> Option<String> {
    root.relative_path(workspace, path).or_else(|| {
        path.strip_prefix(root.path())
            .ok()
            .map(|relative| relative.to_string_lossy().replace('\\', "/"))
    })
}

fn evidence_failure(error: &crate::evidence::EvidenceError) -> RunError {
    use crate::evidence::EvidenceError as Evidence;
    let code = match error.code() {
        "evidence_path_exists" => "evidence_path_exists",
        "evidence_path_escape" => "evidence_path_escape",
        _ => "evidence_write_failed",
    };
    let path = match error {
        Evidence::Io { path, .. }
        | Evidence::Canonical { path, .. }
        | Evidence::PathEscape { path, .. }
        | Evidence::PathExists { path } => path.display().to_string(),
        other => other.to_string(),
    };
    RunError::new(
        code,
        error.to_string(),
        false,
        Map::from_iter([("path".to_owned(), Value::String(path))]),
    )
}

/// What became of the per-run payload scratch (§6.3), as `meta.json`'s `incoming` and
/// `payload` members.
#[derive(Debug, Default)]
struct IncomingMeta {
    had_files: bool,
    kept: Vec<(String, u64)>,
    removed_dir: bool,
    /// The budget that was in force, echoed into `meta.json` so a reader can tell a kept
    /// payload from a run configured to keep nothing.
    cap_bytes: u64,
    /// Consumed payload kept under `runs/<run_id>/payload/`, as `(file name, bytes)`.
    retained: Vec<(String, u64)>,
    /// Total size of [`Self::retained`], so the budget check is one addition.
    retained_bytes: u64,
    /// Consumed files the budget would not fit (or that could not be moved), so they were
    /// deleted after all — the count behind `payload.dropped` and
    /// [`WARNING_PAYLOAD_CAP_EXCEEDED`].
    dropped: usize,
}

/// Clean `.evidence/incoming/<run_id>/` on **every** terminal path (§6.3).
///
/// A file the extractor *read* is moved to `runs/<run_id>/payload/` while it fits
/// `cap_bytes`, and deleted once it does not: it is the file the published record was
/// computed from, so keeping it is what makes a coverage record checkable after the run,
/// and `runs/` is the one place retention already reclaims. Anything the extractor did not
/// read is **kept where it is** and listed with its size — an unparsed payload is the only
/// clue to a job that wrote the wrong file name. The directory goes when it is empty.
///
/// `cap_bytes == 0` keeps nothing, which is what a disk-tight deployment asks for.
async fn clean_payload(context: &RunContext, consumed: &[PathBuf], cap_bytes: u64) -> IncomingMeta {
    let mut meta = IncomingMeta {
        cap_bytes,
        ..IncomingMeta::default()
    };
    let mut entries = match tokio::fs::read_dir(&context.payload_dir).await {
        Ok(entries) => entries,
        Err(_) => return meta,
    };
    let consumed: Vec<PathBuf> = consumed
        .iter()
        .map(|path| path.canonicalize().unwrap_or_else(|_| path.clone()))
        .collect();
    let keep_dir = context.run_dir.join(PAYLOAD_DIRNAME);
    let mut survivors = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let size = entry
            .metadata()
            .await
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        meta.had_files = true;
        let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
        let is_file = entry
            .metadata()
            .await
            .map(|metadata| metadata.is_file())
            .unwrap_or(false);
        if is_file && consumed.contains(&canonical) {
            let fits = cap_bytes > 0 && meta.retained_bytes.saturating_add(size) <= cap_bytes;
            if fits {
                match retain_file(&path, &keep_dir).await {
                    Ok(()) => {
                        let name = path
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_else(|| path.display().to_string());
                        meta.retained.push((name, size));
                        meta.retained_bytes += size;
                        continue;
                    }
                    Err(reason) => {
                        // Failing to keep a payload is never a reason to lose a manifest:
                        // say so, and let the file go the way it went before.
                        tracing::warn!(
                            run_id = %context.run_id,
                            path = %path.display(),
                            "could not keep the run's payload: {reason}"
                        );
                        meta.dropped += 1;
                    }
                }
            } else if cap_bytes > 0 {
                meta.dropped += 1;
            }
            // Read and finished with (ADR-0002 §3.4).
            if tokio::fs::remove_file(&path).await.is_err() {
                survivors.push((path, size));
            }
            continue;
        }
        survivors.push((path, size));
    }
    for (path, size) in survivors {
        meta.kept.push((path.display().to_string(), size));
    }
    // Only remove the directory if it is empty; kept-for-debugging files stay put (§6.3).
    // `remove_dir` only succeeds on an empty directory, and short-circuiting here is the
    // point: a payload we deliberately kept must stay on disk.
    if meta.kept.is_empty() && tokio::fs::remove_dir(&context.payload_dir).await.is_ok() {
        meta.removed_dir = true;
    }
    meta
}

/// Move one consumed payload file into `keep_dir`, which is `runs/<run_id>/payload/`.
///
/// `rename` is the whole operation on the common path — the evidence root is one filesystem
/// — and the copy-then-remove fallback covers a payload directory mounted from elsewhere. A
/// file that ends up in neither place is an `Err`, never a silent disappearance.
async fn retain_file(path: &Path, keep_dir: &Path) -> Result<(), String> {
    let name = path
        .file_name()
        .ok_or_else(|| "payload file has no file name".to_owned())?;
    tokio::fs::create_dir_all(keep_dir)
        .await
        .map_err(|reason| format!("cannot create {}: {reason}", keep_dir.display()))?;
    let target = keep_dir.join(name);
    if tokio::fs::rename(path, &target).await.is_ok() {
        return Ok(());
    }
    tokio::fs::copy(path, &target)
        .await
        .map_err(|reason| format!("cannot copy to {}: {reason}", target.display()))?;
    tokio::fs::remove_file(path)
        .await
        .map_err(|reason| format!("copied but could not remove {}: {reason}", path.display()))
}

/// Whether a publish temp file for this run still exists — the second half of the
/// `partial_evidence_dropped` condition (§6.2).
fn staged_publish_temp(context: &RunContext) -> bool {
    let run_id = &context.run_id;
    for kind in ["coverage", "benchmark"] {
        let directory = context
            .evidence_dir
            .join(kind)
            .join(context.identity.commit.as_str());
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') && name.contains(run_id) && name.ends_with(".tmp") {
                return true;
            }
        }
    }
    false
}

/// §3.4 step 4: write `meta.json` **last**, atomically.
///
/// Every terminal run gets one, including a run cancelled before act was spawned — that is
/// what makes "no `meta.json`" mean exactly one thing (ADR-0002 §1.3). Two members are
/// spelled relative to the evidence root rather than the workspace, because the file lives
/// inside it: `act.log` and `evidence[].path`.
///
/// For a run that never spawned there is no act-spawn instant; `started_at` (required by
/// `schema/run-meta.schema.json`) then carries the registration instant, which is the only
/// honest clock that exists, and `act` is omitted entirely.
#[allow(
    clippy::too_many_arguments,
    reason = "this is meta.json's member list: the record, the label, and the four optional                sections, passed straight through rather than re-derived"
)]
fn write_manifest(
    context: &RunContext,
    record: &RunRecord,
    status: WireStatus,
    outcome: Option<&ActRunOutcome>,
    argv: &[String],
    published: &[PublishedEntry],
    incoming: &IncomingMeta,
    malformed: usize,
) -> Result<(), String> {
    // `status` is passed in, not read off `record`: the manifest is written *before* the
    // terminal transition is published (§3.4 step 5–6), so at this instant the record is
    // still `publishing`, which would render `"status": "running"` and violate both
    // ADR-0002's manifest and `run-meta.schema.json`.
    let status = if status == WireStatus::Pending {
        WireStatus::Error
    } else {
        status
    };
    let mut manifest = Map::new();
    manifest.insert("schema_version".to_owned(), Value::from(1));
    manifest.insert("run_id".to_owned(), Value::String(record.run_id.clone()));
    manifest.insert(
        "status".to_owned(),
        Value::String(status.as_str().to_owned()),
    );
    manifest.insert(
        "workflow".to_owned(),
        Value::String(record.identity.workflow.clone()),
    );
    manifest.insert(
        "task".to_owned(),
        Value::String(record.identity.task.clone()),
    );
    manifest.insert(
        "category".to_owned(),
        Value::String(record.identity.category.clone()),
    );
    manifest.insert(
        "commit".to_owned(),
        Value::String(record.identity.commit.clone()),
    );
    let started = record.started_at.unwrap_or(record.registered_at);
    manifest.insert(
        "started_at".to_owned(),
        Value::String(started.to_rfc3339_opts(SecondsFormat::Millis, true)),
    );
    if let Some(finished) = record.finished_at {
        manifest.insert(
            "finished_at".to_owned(),
            Value::String(finished.to_rfc3339_opts(SecondsFormat::Millis, true)),
        );
    }
    if let Some(duration) = record.elapsed_ms() {
        manifest.insert("duration_ms".to_owned(), Value::from(duration));
    }
    if let Some(code) = record.exit_code {
        manifest.insert("exit_code".to_owned(), Value::from(code));
    }
    manifest.insert(
        "host".to_owned(),
        Value::String(crate::evidence::writer::hostname()),
    );
    if let Some(outcome) = outcome {
        let mut act = Map::new();
        act.insert(
            "argv".to_owned(),
            Value::Array(
                argv.iter()
                    .take(256)
                    .map(|argument| Value::String(argument.chars().take(4096).collect::<String>()))
                    .collect(),
            ),
        );
        act.insert("pid".to_owned(), Value::from(outcome.pid));
        act.insert(
            "log".to_owned(),
            Value::String(format!("runs/{}/act.jsonl", record.run_id)),
        );
        act.insert("log_bytes".to_owned(), Value::from(outcome.log_bytes));
        act.insert(
            "log_truncated".to_owned(),
            Value::Bool(outcome.log_truncated),
        );
        manifest.insert("act".to_owned(), Value::Object(act));
    }
    if context.payload_dir.exists() || !incoming.kept.is_empty() || incoming.removed_dir {
        let mut incoming_member = Map::new();
        incoming_member.insert(
            "dir".to_owned(),
            Value::String(format!("incoming/{}", record.run_id)),
        );
        if !incoming.kept.is_empty() {
            incoming_member.insert(
                "files".to_owned(),
                Value::Array(
                    incoming
                        .kept
                        .iter()
                        .take(64)
                        .map(|(path, bytes)| {
                            Value::Object(Map::from_iter([
                                (
                                    "path".to_owned(),
                                    Value::String(
                                        Path::new(path)
                                            .file_name()
                                            .map(|name| name.to_string_lossy().to_string())
                                            .unwrap_or_else(|| path.clone()),
                                    ),
                                ),
                                ("bytes".to_owned(), Value::from(*bytes)),
                            ]))
                        })
                        .collect(),
                ),
            );
        }
        incoming_member.insert("removed".to_owned(), Value::Bool(incoming.removed_dir));
        manifest.insert("incoming".to_owned(), Value::Object(incoming_member));
    }
    // The payload this run *read*, kept where it can be read again. `source_path` in the
    // published records names the scratch file it came from; this is where that file now is.
    if !incoming.retained.is_empty() || incoming.dropped > 0 {
        let mut payload_member = Map::new();
        payload_member.insert(
            "dir".to_owned(),
            Value::String(format!("runs/{}/{}", record.run_id, PAYLOAD_DIRNAME)),
        );
        payload_member.insert("bytes".to_owned(), Value::from(incoming.retained_bytes));
        payload_member.insert("cap_bytes".to_owned(), Value::from(incoming.cap_bytes));
        if !incoming.retained.is_empty() {
            payload_member.insert(
                "files".to_owned(),
                Value::Array(
                    incoming
                        .retained
                        .iter()
                        .take(64)
                        .map(|(name, bytes)| {
                            Value::Object(Map::from_iter([
                                ("path".to_owned(), Value::String(name.clone())),
                                ("bytes".to_owned(), Value::from(*bytes)),
                            ]))
                        })
                        .collect(),
                ),
            );
        }
        if incoming.dropped > 0 {
            payload_member.insert("dropped".to_owned(), Value::from(incoming.dropped));
        }
        manifest.insert("payload".to_owned(), Value::Object(payload_member));
    }
    manifest.insert(
        "evidence".to_owned(),
        Value::Array(
            published
                .iter()
                .filter_map(|entry| {
                    let path = entry.relative.clone()?;
                    let mut member = Map::new();
                    member.insert("kind".to_owned(), Value::String(entry.kind.to_owned()));
                    member.insert("path".to_owned(), Value::String(path));
                    member.insert("records".to_owned(), Value::from(entry.records));
                    member.insert("truncated".to_owned(), Value::Bool(entry.truncated));
                    if malformed > 0 {
                        member.insert("malformed".to_owned(), Value::from(malformed));
                    }
                    Some(Value::Object(member))
                })
                .collect(),
        ),
    );
    manifest.insert(
        "warnings".to_owned(),
        Value::Array(
            record
                .warnings
                .iter()
                .take(64)
                .map(|warning| Value::String(warning.clone()))
                .collect(),
        ),
    );

    let destination = context.run_dir.join("meta.json");
    let temporary = context
        .run_dir
        .join(format!(".meta.json.{}.tmp", random_suffix()));
    std::fs::create_dir_all(&context.run_dir)
        .map_err(|reason| format!("{}: {reason}", context.run_dir.display()))?;
    let text = serde_json::to_vec_pretty(&Value::Object(manifest))
        .map_err(|reason| format!("meta.json could not be serialized: {reason}"))?;
    let wrote = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(&text)?;
        file.write_all(b"\n")?;
        file.flush()?;
        drop(file);
        std::fs::rename(&temporary, &destination)
    })();
    if let Err(reason) = wrote {
        let _ = std::fs::remove_file(&temporary);
        return Err(format!("{}: {reason}", destination.display()));
    }
    Ok(())
}

/// A short random suffix for the manifest's temp file, so two runs cannot collide even if
/// they somehow share a run directory (they cannot; this is cheap paranoia, same as the
/// evidence writer's temp names).
fn random_suffix() -> String {
    let mut buffer = [0u8; 3];
    if let Ok(mut file) = std::fs::File::open("/dev/urandom") {
        use std::io::Read as _;
        let _ = file.read_exact(&mut buffer);
    }
    buffer.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::{clean_payload, retain_file, RunContext, PAYLOAD_DIRNAME};
    use crate::registry::RunIdentity;
    use std::{fs, path::PathBuf};
    use tempfile::TempDir;

    /// A context whose evidence root is `root/.evidence`, with the run's two directories
    /// (`runs/<run_id>/` and `incoming/<run_id>/`) already present, as production leaves them.
    struct Harness {
        /// Held for the test's lifetime only: dropping it deletes the tree under test.
        _root: TempDir,
        context: RunContext,
    }

    impl Harness {
        fn new() -> Self {
            let root = tempfile::tempdir().expect("tempdir");
            let workspace = root.path().join("workspace");
            let evidence_dir = workspace.join(".evidence");
            let run_dir = evidence_dir.join("runs").join("20261110T011507Z-d4e5f6");
            let payload_dir = evidence_dir
                .join("incoming")
                .join("20261110T011507Z-d4e5f6");
            fs::create_dir_all(&run_dir).expect("run dir");
            fs::create_dir_all(&payload_dir).expect("payload dir");
            let context = RunContext {
                run_id: "20261110T011507Z-d4e5f6".to_owned(),
                identity: RunIdentity::new(
                    "coverage",
                    ".github/workflows/coverage.yml",
                    "coverage",
                    "coverage",
                    "2f7c1a4b9e0d3c6a5b8f1e2d4c6a8b0f3e5d7c91",
                ),
                tree: workspace.clone(),
                workspace: workspace.clone(),
                evidence_dir,
                run_dir,
                payload_dir,
                scratch_dir: root.path().join("scratch"),
                timeout: std::time::Duration::from_secs(60),
            };
            Self {
                _root: root,
                context,
            }
        }

        /// Drop a payload file into the scratch directory, returning the path an extractor
        /// would have handed back as "consumed".
        fn payload(&self, name: &str, body: &str) -> PathBuf {
            let path = self.context.payload_dir.join(name);
            fs::write(&path, body).expect("write payload");
            path
        }

        fn kept(&self, name: &str) -> Option<String> {
            std::fs::read_to_string(self.context.run_dir.join(PAYLOAD_DIRNAME).join(name)).ok()
        }
    }

    /// A file the extractor read is moved under `runs/<run_id>/payload/`, and the file it did
    /// not read stays where it is: that one is evidence about the job, this one is evidence
    /// about the record.
    #[tokio::test]
    async fn a_consumed_payload_moves_next_to_the_run() {
        let harness = Harness::new();
        let consumed = harness.payload("lcov.info", "TN:\nSF:src/a.rs\nend_of_record\n");
        harness.payload("mystery.bin", "not a payload name the extractor knows");

        let meta = clean_payload(&harness.context, &[consumed], 4096).await;

        assert_eq!(
            harness.kept("lcov.info").as_deref(),
            Some("TN:\nSF:src/a.rs\nend_of_record\n")
        );
        assert!(!harness.context.payload_dir.join("lcov.info").exists());
        assert_eq!(meta.retained.len(), 1, "one file kept, named");
        assert_eq!(meta.retained[0].0, "lcov.info");
        assert_eq!(meta.retained_bytes, 30);
        assert_eq!(meta.cap_bytes, 4096);
        assert_eq!(meta.dropped, 0, "the budget fitted it");
        assert_eq!(meta.kept.len(), 1, "the unparsed file is still listed");
        assert!(harness.context.payload_dir.join("mystery.bin").exists());
        assert!(
            !meta.removed_dir,
            "a kept-for-debugging file holds the directory open"
        );
    }

    /// The budget is the whole point of the flag: over it, the run behaves the way it always
    /// did — the payload is deleted, and the manifest says so.
    #[tokio::test]
    async fn a_payload_over_the_budget_is_dropped_and_counted() {
        let harness = Harness::new();
        let consumed = harness.payload("lcov.info", &"SF:src/a.rs\n".repeat(64));

        let meta = clean_payload(&harness.context, &[consumed], 32).await;

        assert!(meta.retained.is_empty(), "nothing over the budget is kept");
        assert_eq!(meta.dropped, 1, "and the manifest admits what went");
        assert!(!harness.context.run_dir.join(PAYLOAD_DIRNAME).exists());
        assert!(
            !harness.context.payload_dir.exists(),
            "and the scratch goes with it"
        );
        assert!(meta.removed_dir);
    }

    /// `--max-payload-keep-bytes 0` is the documented "keep nothing" setting, which is *not*
    /// the same event as a payload being too big: no drop, so no warning.
    #[tokio::test]
    async fn a_zero_budget_keeps_nothing_without_complaining() {
        let harness = Harness::new();
        let consumed = harness.payload("lcov.info", "TN:\nend_of_record\n");

        let meta = clean_payload(&harness.context, &[consumed], 0).await;

        assert!(meta.retained.is_empty());
        assert_eq!(meta.dropped, 0);
        assert_eq!(meta.cap_bytes, 0);
        assert!(!harness.context.payload_dir.join("lcov.info").exists());
    }

    /// A run whose job wrote no payload has no scratch directory at all, and cleanup is not
    /// the place that gets to decide that is an error.
    #[tokio::test]
    async fn a_run_with_no_payload_directory_is_quiet() {
        let harness = Harness::new();
        fs::remove_dir(&harness.context.payload_dir).expect("remove the scratch dir");

        let meta = clean_payload(&harness.context, &[], 4096).await;

        assert!(!meta.had_files);
        assert!(!meta.removed_dir);
        assert!(meta.retained.is_empty() && meta.kept.is_empty());
    }

    /// The copy fallback has to leave exactly one copy: a payload that lands in both places,
    /// or in neither, is a record nothing can be checked against.
    #[tokio::test]
    async fn retain_file_leaves_one_copy_whether_or_not_rename_works() {
        let harness = Harness::new();
        let source = harness.payload("benchmark.json", "{\"metrics\":[]}");
        let keep = harness.context.run_dir.join(PAYLOAD_DIRNAME);

        retain_file(&source, &keep)
            .await
            .expect("retain benchmark.json");

        assert_eq!(
            fs::read_to_string(keep.join("benchmark.json")).expect("kept copy"),
            "{\"metrics\":[]}"
        );
        assert!(!source.exists(), "the scratch copy is gone");
    }
}
