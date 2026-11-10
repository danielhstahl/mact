//! The execution half of the runner: spawn, tee, wait, and the only signalling code in
//! the codebase.
//!
//! Threading model, because it is what makes cancellation provable:
//!
//! ```text
//!   run task (supervisor's)          log pumps (2)             writer (blocking pool)
//!   ─────────────────────            ─────────────             ────────────────────────
//!   start() ── spawn act             stdout ─┐                 act.jsonl
//!   wait()                            stderr ─┴─ redact ─┬─► ring (64 KiB)
//!      ├─ sleep_until(deadline)                          └─► channel ─► cap 64 MiB
//!      ├─ cancellation.cancelled()                                     + marker
//!      └─ child.wait() → reap → drain pumps → join writer → outcome
//!
//!   cancel_run / deadline / shutdown task
//!   ─────────────────────────────────
//!   handle().terminate(initiator): killpg SIGTERM → poll group liveness through the grace
//!                                  → killpg SIGKILL → poll again → KillReport
//! ```
//!
//! One writer, many producers: the ring and the file are fed by two pumps but drained by
//! one channel, so the on-disk order is arrival order. `terminate` runs on a different
//! task than `wait` — that is the whole point (ADR-0004 §5.1: cancel comes from
//! `cancel_run`, while the run task owns the child), and the two meet only in
//! [`RunShared`]: a `watch` that says "reaped", and a mutex holding the single
//! [`KillReport`].
//!
//! Teardown does **not** wait on that `watch` alone: the task calling `terminate` is often
//! the run task itself (the deadline and the cancellation token are both polled inside
//! `wait`), so a grace period that waited for a reap would wait for itself. It polls
//! [`process::group_is_live`] instead — `/proc`, zombie-aware — which also stops a dead but
//! unreaped act from being escalated to `SIGKILL` and mislabelled `kill`.

use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use chrono::{DateTime, SecondsFormat, Utc};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, BufReader},
    sync::{mpsc, watch, Mutex as AsyncMutex},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use super::{
    argv::build_argv,
    log::{self, fit_log_tail, LogRing, Redactor},
    process, RunFailure, RunSpec, RunStatus, RunnerError, RunnerLimits,
    WARNING_ORPHANED_CONTAINERS, WARNING_PAYLOAD_TRANSPORT_UNAVAILABLE,
    WARNING_PROCESS_GROUP_KILLED, WARNING_TRUNCATED,
};

/// Who asked for the run to be taken down (ADR-0004 §5.1, §17.3).
///
/// The three initiators share one mechanism and differ only in the label they leave
/// behind, which is what keeps `cancelled`, `timed_out` and `error` distinguishable
/// downstream even though a `killpg` looks identical from outside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminateInitiator {
    /// `cancel_run`, or an MCP cancellation notification for a waiting call → `cancelled`.
    Caller,
    /// `timeout_secs` expiry → `timed_out`.
    Deadline,
    /// SIGINT/SIGTERM to act-mcp itself → `error` + `runner_internal_error`.
    Shutdown,
}

impl TerminateInitiator {
    /// The label this initiator stamps on the run.
    #[must_use]
    pub const fn status(self) -> RunStatus {
        match self {
            Self::Caller => RunStatus::Cancelled,
            Self::Deadline => RunStatus::TimedOut,
            Self::Shutdown => RunStatus::Error,
        }
    }

    /// The word ADR-0004 §17.3 uses.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Caller => "caller",
            Self::Deadline => "deadline",
            Self::Shutdown => "shutdown",
        }
    }
}

/// How the process group actually died (ADR-0004 §5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillMethod {
    /// SIGTERM was enough: act's own `--rm` cleanup ran, so no orphaned containers.
    Term,
    /// SIGKILL was needed: act's cleanup did not run, so containers may have survived.
    Kill,
    /// Nothing was signalled — cancelled before `exec`, or the cancel arrived after act
    /// had already exited (§5.4).
    None,
}

impl KillMethod {
    /// The wire spelling (`cancel_run_result.kill.method`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Term => "term",
            Self::Kill => "kill",
            Self::None => "none",
        }
    }
}

/// What a teardown cost and how it was done; `cancel_run` copies this verbatim
/// (ADR-0004 §5.1: mact-f1w.11 "copies it verbatim; it does not invent it").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KillReport {
    /// Which signal finished the job.
    pub method: KillMethod,
    /// First signal sent → reaped, in ms. Absent for `method: "none"`. The schema bound is
    /// 300000 ms, which is why `--term-grace-secs` is bounded at 240 (ADR-0004 §16.3).
    pub took_ms: Option<u64>,
}

impl KillReport {
    /// The report for "there was no process to signal".
    #[must_use]
    pub const fn none() -> Self {
        Self {
            method: KillMethod::None,
            took_ms: None,
        }
    }

    /// The `{"method": …, "took_ms": …}` object.
    #[must_use]
    pub fn to_value(&self) -> serde_json::Value {
        match self.took_ms {
            Some(took) => serde_json::json!({ "method": self.method.as_str(), "took_ms": took }),
            None => serde_json::json!({ "method": self.method.as_str() }),
        }
    }
}

/// Everything `.11`/`.14` need to report one finished run.
///
/// The ticket's execution contract (`exit_code`, `status`, `started_at`, `finished_at`,
/// `duration_ms`, `log_path`, `log_tail`, `truncated`) plus what `meta.json` wants
/// (ADR-0002 §1.3: `act.pid`, `act.log_bytes`, `act.log_truncated`), the kill receipt, and
/// the warnings only the runner can know.
#[derive(Debug, Clone)]
pub struct ActRunOutcome {
    /// The run this outcome belongs to.
    pub run_id: String,
    /// One of the five terminal words.
    pub status: RunStatus,
    /// act's `wait(2)` code; absent when it was signalled or never spawned (ADR-0001 §3.1).
    pub exit_code: Option<u32>,
    /// The instant `exec` returned (`run_view.started_at`).
    pub started_at: DateTime<Utc>,
    /// The instant act was reaped.
    pub finished_at: DateTime<Utc>,
    /// Wall-clock ms act actually ran (ADR-0004 §4.4: measured from `started_at`).
    pub duration_ms: u64,
    /// act's pid, for `meta.json.act.pid`.
    pub pid: u32,
    /// The process group that was signalled (equals `pid`: the run is its own group leader).
    pub pgid: i32,
    /// Absolute path of the captured log: `<run dir>/act.jsonl`.
    pub log_path: PathBuf,
    /// Bytes of act's own output written to `act.jsonl` (the marker line is not counted).
    pub log_bytes: u64,
    /// Whether the on-disk log hit `--max-run-log-bytes` (or the drain was abandoned).
    pub log_truncated: bool,
    /// Lines dropped after the cap, for `meta.json` and the truncation marker.
    pub log_dropped_lines: u64,
    /// stdout/stderr split, for debugging a capture problem.
    pub log_stdout_bytes: u64,
    /// See [`Self::log_stdout_bytes`].
    pub log_stderr_bytes: u64,
    /// The redacted, path-substituted tail, up to `--log-tail-ring-bytes`.
    pub log_tail: String,
    /// How the group went down, or `None` when nothing signalled it.
    pub kill: Option<KillReport>,
    /// The initiator that won the race, if anything terminated the run.
    pub initiator: Option<TerminateInitiator>,
    /// Accumulated warning codes (open vocabulary, ADR-0001 §5.4).
    pub warnings: Vec<String>,
    /// Set exactly when `status == Error`.
    pub failure: Option<RunFailure>,
    /// Echoed back: this run was an act `--dryrun`.
    pub dry_run: bool,
}

impl ActRunOutcome {
    /// `log_tail` re-capped to a caller's `log_tail_bytes` (ADR-0001 §2.8: whole lines,
    /// UTF-8 boundary, always the end).
    #[must_use]
    pub fn log_tail_within(&self, max_bytes: usize) -> String {
        fit_log_tail(&self.log_tail, max_bytes)
    }

    /// Whether a warning was stamped.
    #[must_use]
    pub fn has_warning(&self, code: &str) -> bool {
        self.warnings.iter().any(|warning| warning == code)
    }

    /// `started_at`/`finished_at` as the canonical wire timestamp
    /// (`YYYY-MM-DDTHH:MM:SS.mmmZ`, ADR-0001 `$defs/timestamp`).
    #[must_use]
    pub fn timestamp_text(instant: DateTime<Utc>) -> String {
        instant.to_rfc3339_opts(SecondsFormat::Millis, true)
    }
}

/// Shared between the run task and any number of [`RunHandle`]s.
struct RunShared {
    run_id: String,
    pgid: i32,
    limits: Arc<RunnerLimits>,
    log_path: PathBuf,
    /// The redacted tail, bounded (ADR-0004 §7.1).
    ring: Mutex<LogRing>,
    /// Redacts secret values only — the on-disk copy keeps real paths (audit trail).
    disk_redactor: Redactor,
    /// Redacts secrets *and* substitutes `<WORKSPACE>`/`<EVIDENCE>` — the wire copy.
    tail_redactor: Redactor,
    counters: Arc<LogCounters>,
    /// The single kill receipt; first caller writes it, everyone else reads it (§3.5).
    kill: AsyncMutex<Option<KillReport>>,
    /// The first initiator to actually take the run down.
    initiator: Mutex<Option<TerminateInitiator>>,
    /// Set the instant act is reaped.
    exited: watch::Sender<bool>,
    /// Warnings known before act even ran (e.g. `payload_transport_unavailable`).
    warnings: Mutex<Vec<String>>,
}

impl RunShared {
    fn mark_exited(&self) {
        self.exited.send_replace(true);
    }

    fn is_exited(&self) -> bool {
        *self.exited.borrow()
    }

    fn add_warning(&self, code: &str) {
        let mut warnings = lock(&self.warnings);
        if !warnings.iter().any(|warning| warning == code) {
            warnings.push(code.to_owned());
        }
    }

    fn warnings(&self) -> Vec<String> {
        lock(&self.warnings).clone()
    }

    /// The tail as the wire wants it: whole lines from the end, capped in bytes.
    fn log_tail(&self, max_bytes: usize) -> String {
        let bytes = lock(&self.ring).tail_whole(max_bytes);
        fit_log_tail(&String::from_utf8_lossy(&bytes), max_bytes)
    }

    fn initiator(&self) -> Option<TerminateInitiator> {
        *lock(&self.initiator)
    }

    fn record_initiator(&self, initiator: TerminateInitiator) {
        let mut slot = lock(&self.initiator);
        if slot.is_none() {
            *slot = Some(initiator);
        }
    }
}

/// Poison-tolerant lock: none of these locks spans an `.await`, and a panic in a log pump
/// must not turn a teardown into a panic cascade.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Log accounting, shared with the writer on the blocking pool.
#[derive(Debug, Default)]
struct LogCounters {
    bytes_written: AtomicU64,
    dropped_lines: AtomicU64,
    truncated: AtomicBool,
    stdout_bytes: AtomicU64,
    stderr_bytes: AtomicU64,
}

/// A run whose act process is alive — or has just become dead and is waiting to be reaped.
///
/// Owned by exactly one task, the run's own supervisor task, which is what makes "only the
/// run task writes run state" (ADR-0004 §3.5) structural rather than a rule to remember.
/// Clone a [`RunHandle`] for anyone else who needs to watch or stop it.
#[must_use = "a spawned act must be waited for; dropping this handle kills it (kill_on_drop)"]
pub struct RunningActRun {
    shared: Arc<RunShared>,
    child: tokio::process::Child,
    argv: Vec<String>,
    pid: u32,
    started_at: DateTime<Utc>,
    started_instant: Instant,
    timeout: Duration,
    cancellation: CancellationToken,
    pumps: Vec<JoinHandle<()>>,
    log_tx: Option<mpsc::Sender<Vec<u8>>>,
    writer: Option<JoinHandle<()>>,
    secret_file: PathBuf,
    dry_run: bool,
}

impl RunningActRun {
    /// A cheap handle for watching and stopping: what `cancel_run` holds.
    #[must_use]
    pub fn handle(&self) -> RunHandle {
        RunHandle {
            shared: Arc::clone(&self.shared),
        }
    }

    /// act's pid (also the process group id).
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// The process group id teardown signals.
    #[must_use]
    pub fn pgid(&self) -> i32 {
        self.shared.pgid
    }

    /// The argv act was spawned with, with every redaction needle already replaced:
    /// declared secrets are absent by construction (they are in the secret file, never argv),
    /// and a `--var`/`--input`/`--env` value whose name says secret is masked here too
    /// (mact-f1w.13). `meta.json.act.argv` records exactly this (ADR-0002 §1.3), so the
    /// durable copy of the command line cannot become the place a credential survives.
    #[must_use]
    pub fn argv(&self) -> &[String] {
        &self.argv
    }

    /// The instant `exec` returned (`run_view.started_at`).
    #[must_use]
    pub fn started_at(&self) -> DateTime<Utc> {
        self.started_at
    }

    /// Whether act is still alive.
    #[must_use]
    pub fn is_running(&self) -> bool {
        !self.shared.is_exited()
    }

    /// Wait for act, driving the deadline and the cancellation token, and return the
    /// terminal outcome.
    ///
    /// This is the only place act is reaped. It returns only after the log pumps and the
    /// writer are done (each bounded by `RunnerLimits::log_drain`), so the caller can
    /// publish evidence and write `meta.json` knowing `act.jsonl` is complete — that is
    /// ADR-0004 §3.4's ordering, and the reason `wait()` is not just `child.wait()`.
    pub async fn wait(self) -> ActRunOutcome {
        let RunningActRun {
            shared,
            mut child,
            pid,
            started_at,
            started_instant,
            timeout,
            cancellation,
            mut pumps,
            mut log_tx,
            mut writer,
            secret_file,
            dry_run,
            ..
        } = self;
        let handle = RunHandle {
            shared: Arc::clone(&shared),
        };

        // ADR-0004 §4.1: the deadline is absolute and armed once, so a starved tokio
        // worker cannot steal seconds from the run.
        let deadline_at = tokio::time::Instant::now() + timeout;
        let mut wait = std::pin::pin!(child.wait());
        let mut deadline = std::pin::pin!(tokio::time::sleep_until(deadline_at));
        let mut cancelled = std::pin::pin!(cancellation.cancelled());
        let (mut deadline_armed, mut cancel_armed) = (true, true);

        let status = loop {
            tokio::select! {
                biased;
                status = &mut wait => break status,
                _ = &mut deadline, if deadline_armed => {
                    deadline_armed = false;
                    handle.terminate(TerminateInitiator::Deadline).await;
                }
                _ = &mut cancelled, if cancel_armed => {
                    cancel_armed = false;
                    handle.terminate(TerminateInitiator::Caller).await;
                }
            }
        };

        let finished_at = Utc::now();
        let finished_instant = Instant::now();
        // Announce the reap first: a concurrent `terminate` is parked on this watch,
        // deciding between "term" and "kill".
        shared.mark_exited();
        // Secrets have done their job (ADR-0003 §9.1: unlinked as soon as the exit status
        // is known). The scratch *directory* stays for the supervisor, because in worktree
        // mode it also holds the worktree (ADR-0004 §3.4 step 6).
        if let Err(error) = tokio::fs::remove_file(&secret_file).await {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(
                    path = %secret_file.display(),
                    "could not unlink the run's secret file: {error}"
                );
            }
        }

        let exit = match status {
            Ok(exit) => exit,
            Err(error) => {
                tracing::error!(run_id = %shared.run_id, "waiting for act failed: {error}");
                std::process::ExitStatus::default()
            }
        };

        // Drain the capture: EOF should already be here; the bound exists because act's
        // grandchildren can hold the pipes open after act itself died.
        drop(log_tx.take());
        let drained = tokio::time::timeout(shared.limits.log_drain, async {
            for pump in pumps.iter_mut() {
                let _ = pump.await;
            }
        })
        .await;
        if drained.is_err() {
            tracing::warn!(
                run_id = %shared.run_id,
                grace = ?shared.limits.log_drain,
                "log pumps did not drain in time; act.jsonl may be missing its last lines"
            );
            for pump in pumps.iter() {
                pump.abort();
            }
            shared.add_warning(WARNING_TRUNCATED);
        }
        if let Some(handle) = writer.as_mut() {
            if tokio::time::timeout(shared.limits.log_drain, handle)
                .await
                .is_err()
            {
                tracing::warn!(run_id = %shared.run_id, "log writer did not finish; abandoning it");
                shared.add_warning(WARNING_TRUNCATED);
            }
        }

        let counters = &shared.counters;
        let log_bytes = counters.bytes_written.load(Ordering::Relaxed);
        let log_dropped_lines = counters.dropped_lines.load(Ordering::Relaxed);
        let log_truncated = counters.truncated.load(Ordering::Relaxed);
        if log_truncated {
            shared.add_warning(WARNING_TRUNCATED);
        }

        let kill = *shared.kill.lock().await;
        let initiator = shared.initiator();
        let code = exit.code();
        let signal = signal_of(&exit);
        let exit_code = code.and_then(|number| u32::try_from(number).ok());
        // The label. An initiator decides it (first one wins, ADR-0004 §3.5); otherwise
        // act's own exit does, and "killed by a signal we did not send at no deadline" is
        // act crashing, not the workflow failing (T10).
        let (status, failure) = match initiator {
            Some(TerminateInitiator::Caller) => (RunStatus::Cancelled, None),
            Some(TerminateInitiator::Deadline) => (RunStatus::TimedOut, None),
            Some(TerminateInitiator::Shutdown) => {
                (RunStatus::Error, Some(RunFailure::ServerShutdown))
            }
            None => match code {
                Some(0) => (RunStatus::Success, None),
                Some(_) => (RunStatus::Failed, None),
                None => (RunStatus::Error, Some(RunFailure::ActCrashed { signal })),
            },
        };

        let outcome = ActRunOutcome {
            run_id: shared.run_id.clone(),
            status,
            exit_code,
            started_at,
            finished_at,
            duration_ms: u64::try_from((finished_instant - started_instant).as_millis())
                .unwrap_or(u64::MAX),
            pid,
            pgid: shared.pgid,
            log_path: shared.log_path.clone(),
            log_bytes,
            log_truncated,
            log_dropped_lines,
            log_stdout_bytes: counters.stdout_bytes.load(Ordering::Relaxed),
            log_stderr_bytes: counters.stderr_bytes.load(Ordering::Relaxed),
            log_tail: shared.log_tail(shared.limits.log_tail_ring_bytes),
            kill,
            initiator,
            warnings: shared.warnings(),
            failure,
            dry_run,
        };
        tracing::info!(
            run_id = %outcome.run_id,
            status = outcome.status.as_str(),
            exit_code = ?outcome.exit_code,
            duration_ms = outcome.duration_ms,
            kill = ?outcome.kill.map(|report| report.method.as_str()),
            "act run finished"
        );
        outcome
    }
}

/// The watch-and-stop handle `cancel_run` and the shutdown sweep hold.
#[derive(Clone)]
pub struct RunHandle {
    shared: Arc<RunShared>,
}

impl RunHandle {
    /// The run's id.
    #[must_use]
    pub fn run_id(&self) -> &str {
        &self.shared.run_id
    }

    /// The process group that teardown signals.
    #[must_use]
    pub fn pgid(&self) -> i32 {
        self.shared.pgid
    }

    /// Whether act is still alive.
    #[must_use]
    pub fn is_running(&self) -> bool {
        !self.shared.is_exited()
    }

    /// The current tail (redacted, path-substituted), capped in bytes. This is what lets
    /// `run_status` answer a tail without reading the file (ADR-0004 §7.1).
    #[must_use]
    pub fn log_tail(&self, max_bytes: usize) -> String {
        self.shared.log_tail(max_bytes)
    }

    /// The kill receipt, if a teardown already happened.
    #[must_use]
    pub async fn kill_report(&self) -> Option<KillReport> {
        *self.shared.kill.lock().await
    }

    /// Warnings accumulated so far.
    #[must_use]
    pub fn warnings(&self) -> Vec<String> {
        self.shared.warnings()
    }

    /// The initiator that has taken (or asked for) this run down, if any.
    #[must_use]
    pub fn initiator(&self) -> Option<TerminateInitiator> {
        self.shared.initiator()
    }

    /// Take the run down: `killpg(SIGTERM)` → `--term-grace-secs` → `killpg(SIGKILL)` →
    /// await the reap, and report how it went (ADR-0004 §5.1).
    ///
    /// This is **the** signalling code: nothing else in this codebase sends a signal to act.
    /// It is idempotent and race-proof by construction:
    ///
    /// * the first caller owns the teardown and writes the single [`KillReport`]; later
    ///   callers are handed that same report (ADR-0004 §3.5), so a `cancel_run` racing the
    ///   deadline produces one receipt and one label;
    /// * if act is already reaped, nothing is signalled and nothing is relabelled — a cancel
    ///   first observed at or after act's exit is a no-op (§5.4);
    /// * `method` is never absent when a process existed, and is `none` when none did.
    pub async fn terminate(&self, initiator: TerminateInitiator) -> KillReport {
        let shared = self.shared.as_ref();
        let mut guard = shared.kill.lock().await;
        if let Some(report) = *guard {
            return report;
        }
        // Subscribe before deciding, so a reap landing between the check and the SIGTERM
        // cannot be missed.
        let mut exited = shared.exited.subscribe();
        if *exited.borrow_and_update() {
            // §5.4: act already exited. Deliberately not recorded as an initiator and
            // deliberately not stored, so the run keeps the label it earned.
            return KillReport::none();
        }
        if shared.pgid <= 0 {
            // No process ever existed for this run — a cancel before `exec` (§17.3).
            let report = KillReport::none();
            *guard = Some(report);
            return report;
        }

        shared.record_initiator(initiator);
        let started = Instant::now();
        let mut method = KillMethod::Term;
        match process::signal_group(shared.pgid, libc::SIGTERM) {
            Ok(true) => tracing::info!(
                run_id = %shared.run_id,
                pgid = shared.pgid,
                initiator = initiator.as_str(),
                "SIGTERM sent to the act process group"
            ),
            Ok(false) => tracing::debug!(
                run_id = %shared.run_id,
                pgid = shared.pgid,
                "process group already gone before SIGTERM"
            ),
            Err(error) => tracing::warn!(
                run_id = %shared.run_id,
                pgid = shared.pgid,
                "could not signal the act process group: {error}"
            ),
        }

        let grace = shared.limits.term_grace;
        if !wait_for_group_death(&mut exited, shared.pgid, grace).await {
            // The grace expired, so act's own cleanup is not happening: escalate. This is
            // the path that may leave containers behind, because act's `--rm` teardown is
            // exactly what SIGTERM was meant to let it do (ADR-0004 §5.5).
            method = KillMethod::Kill;
            tracing::warn!(
                run_id = %shared.run_id,
                pgid = shared.pgid,
                grace = ?grace,
                "act ignored SIGTERM for the whole grace; signalling the group with SIGKILL"
            );
            let _ = process::signal_group(shared.pgid, libc::SIGKILL);
            if !wait_for_group_death(&mut exited, shared.pgid, shared.limits.reap_wait).await {
                tracing::warn!(
                    run_id = %shared.run_id,
                    pgid = shared.pgid,
                    grace = ?shared.limits.reap_wait,
                    "act's group was still running after SIGKILL; the §9.2 sweep is next"
                );
            }
            shared.add_warning(WARNING_PROCESS_GROUP_KILLED);
            shared.add_warning(WARNING_ORPHANED_CONTAINERS);
        }

        // `kill.took_ms` is schema-bounded at 300000; the limits guarantee the sum fits and
        // this clamps against a scheduler stall, so the field can never be a violation.
        let took_ms = Some(elapsed_ms(started).min(300_000));
        let report = KillReport { method, took_ms };
        *guard = Some(report);
        report
    }
}

/// How often teardown re-checks whether act's process group is gone.
const GRACE_POLL: Duration = Duration::from_millis(25);

/// Wait up to `budget` for the run's process group to stop being live.
///
/// Either event counts: the run task reporting act reaped (`exited`), or `/proc` saying
/// nothing in the group is running any more. The second is the one that matters, because
/// the run task is usually the caller of `terminate` and cannot report a reap it has not
/// yet performed.
async fn wait_for_group_death(
    exited: &mut watch::Receiver<bool>,
    pgid: i32,
    budget: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if *exited.borrow_and_update() || !process::group_is_live(pgid) {
            return true;
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return false;
        }
        tokio::time::sleep((deadline - now).min(GRACE_POLL)).await;
    }
}

fn elapsed_ms(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// The signal that killed `exit`, if a signal did it.
#[cfg(unix)]
fn signal_of(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

/// Spawn act and start capturing its log.
///
/// Split out from `Runner::start` so the failure paths are all in one place and the public
/// entry point stays a one-liner.
pub(super) async fn start(
    spec: RunSpec,
    limits: Arc<RunnerLimits>,
) -> Result<RunningActRun, RunnerError> {
    spec.validate()?;
    prepare_directories(&spec)?;
    write_run_files(&spec)?;

    let argv = build_argv(&spec);
    let mut command = tokio::process::Command::new(&spec.act_binary);
    command
        .args(&argv[1..])
        // ADR-0003 §7.6: act's cwd is the per-run scratch dir, never the workspace, so
        // act's `.actrc`/project-file lookup cannot reach the repository (F2/F3).
        .current_dir(&spec.scratch_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // Last-resort net (ADR-0004 §15): the real guarantees are the group kill here and
        // §9.2's sweep, but a leaked handle must still not orphan act.
        .kill_on_drop(true);
    // Own process group, so teardown signals act *and its children* (ADR-0004 §5.1).
    command.process_group(0);
    process::set_pdeathsig_on_parent_death(&mut command);

    // "act-mcp's own environment, unscrubbed" (ADR-0003 §7.6) plus the operator seam, plus
    // GITHUB_TOKEN in the *process* env — never argv — for image and action pulls
    // (ADR-0001 §2.7).
    for (key, value) in &spec.process_env {
        command.env(key, value);
    }
    if let Some(token) = spec.secrets.get("GITHUB_TOKEN") {
        command.env("GITHUB_TOKEN", token);
    }

    let started_at = Utc::now();
    let started_instant = Instant::now();
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            // Nothing was spawned, so nothing will ever reach the unlink at the end of
            // `wait()`: remove the secret file here or a failed spawn leaves a plaintext
            // secret on disk until §9.2's sweep.
            if let Err(cleanup) = std::fs::remove_file(spec.secret_file_path()) {
                if cleanup.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(
                        path = %spec.secret_file_path().display(),
                        "could not unlink the secret file of a run that never started: {cleanup}"
                    );
                }
            }
            tracing::error!(
                run_id = %spec.run_id,
                program = %spec.act_binary.display(),
                "act could not be spawned: {error}"
            );
            return Err(RunnerError::Spawn {
                run_id: spec.run_id.to_string(),
                program: spec.act_binary.display().to_string(),
                reason: error.to_string(),
            });
        }
    };
    let pid = child.id().unwrap_or(0);
    // `process_group(0)` makes the child the group leader, so pgid == pid.
    let pgid = pid as i32;
    tracing::info!(
        run_id = %spec.run_id,
        pid,
        pgid,
        job = %spec.job,
        dry_run = spec.dry_run,
        "act spawned"
    );

    let secret_values: Vec<String> = spec.redaction_needles();
    let (exited, _) = watch::channel(false);
    let shared = Arc::new(RunShared {
        run_id: spec.run_id.to_string(),
        pgid,
        limits: Arc::clone(&limits),
        log_path: spec.run_dir.join("act.jsonl"),
        ring: Mutex::new(LogRing::new(limits.log_tail_ring_bytes)),
        disk_redactor: Redactor::secrets(secret_values.iter().map(String::as_str)),
        tail_redactor: Redactor::secrets_and_paths(
            secret_values.iter().map(String::as_str),
            &spec.workspace,
            &spec.evidence_dir,
        ),
        counters: Arc::new(LogCounters::default()),
        kill: AsyncMutex::new(None),
        initiator: Mutex::new(None),
        exited,
        warnings: Mutex::new(if spec.bind {
            Vec::new()
        } else {
            vec![WARNING_PAYLOAD_TRANSPORT_UNAVAILABLE.to_owned()]
        }),
    });

    let (log_tx, log_rx) = mpsc::channel::<Vec<u8>>(256);
    let writer = spawn_writer(
        shared.log_path.clone(),
        log_rx,
        Arc::clone(&shared.counters),
        limits.max_run_log_bytes,
    );
    let mut pumps = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        pumps.push(tokio::spawn(pump(
            stdout,
            StreamKind::Stdout,
            Arc::clone(&shared),
            log_tx.clone(),
        )));
    }
    if let Some(stderr) = child.stderr.take() {
        pumps.push(tokio::spawn(pump(
            stderr,
            StreamKind::Stderr,
            Arc::clone(&shared),
            log_tx.clone(),
        )));
    }

    Ok(RunningActRun {
        shared,
        child,
        // The argv handed back for `meta.json.act.argv` is the *redacted* one: act's argv
        // carries every `--var`/`--input`/`--env` value, and a value whose name says secret is
        // exactly what must not end up in a durable file under `.evidence` (mact-f1w.13). The
        // process was spawned with `argv` above; only this record is filtered, so the audit
        // trail says what we ran without storing what we were paid to keep out.
        argv: {
            let redactor = Redactor::secrets(secret_values.iter().map(String::as_str));
            argv.iter()
                .map(|argument| redactor.apply_str(argument).into_owned())
                .collect()
        },
        pid,
        started_at,
        started_instant,
        timeout: spec.timeout,
        cancellation: spec.cancellation.clone(),
        pumps,
        // `log_tx` itself is the handle's sender: `wait()` drops it, which closes the
        // writer's stream once the pumps have dropped theirs too.
        log_tx: Some(log_tx),
        writer: Some(writer),
        secret_file: spec.secret_file_path(),
        dry_run: spec.dry_run,
    })
}

/// Which of act's two pipes a line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamKind {
    Stdout,
    Stderr,
}

/// Read one pipe line by line: redact, tee to the ring, hand the disk copy to the writer.
///
/// Line-oriented because `act --json` is a line protocol and because `log_tail` is defined
/// in lines. Memory is bounded twice: the ring by bytes, one line by `MAX_LOG_LINE_BYTES`
/// (a job `cat`ting a huge file must not become our buffer size).
///
/// Both streams feed one `act.jsonl` in arrival order, and act's JSON log has been observed
/// on both fds across versions (logrus defaults to stderr; `--json` output has appeared on
/// stdout), so splitting them would mean guessing which file the extractors (.10/.12) must
/// read. ADR-0002 §1.3 names exactly one file for act's stream, so that is the file both
/// pipes go to; the per-stream byte counters keep them distinguishable in `meta.json`.
///
/// A fill is *not* a line: `fill_buf` may hand back half a JSON record, or three records and
/// a fragment of a fourth. The loop below keeps the fragment and reads on — emitting a
/// newline-less buffer as if it were a line is how a log line ends up glued to the next one
/// in `act.jsonl` (there is an on-disk-cap test that catches exactly that).
async fn pump<R: AsyncRead + Unpin>(
    stream: R,
    kind: StreamKind,
    shared: Arc<RunShared>,
    log_tx: mpsc::Sender<Vec<u8>>,
) {
    let mut reader = BufReader::new(stream);
    let mut line: Vec<u8> = Vec::with_capacity(8 * 1024);
    loop {
        let filled = match reader.fill_buf().await {
            Ok(filled) => filled,
            Err(error) => {
                tracing::debug!(run_id = %shared.run_id, "log pipe read failed: {error}");
                break;
            }
        };
        if filled.is_empty() {
            // EOF. A last line with no newline is still act's output and still belongs in
            // act.jsonl, so it is emitted rather than dropped.
            if !line.is_empty() {
                emit(&shared, &log_tx, &line, kind).await;
                line.clear();
            }
            break;
        }
        let mut consumed = 0;
        while consumed < filled.len() {
            match filled[consumed..].iter().position(|byte| *byte == b'\n') {
                Some(offset) => {
                    let end = consumed + offset + 1;
                    line.extend_from_slice(&filled[consumed..end]);
                    consumed = end;
                    emit(&shared, &log_tx, &line, kind).await;
                    line.clear();
                }
                None => {
                    line.extend_from_slice(&filled[consumed..]);
                    consumed = filled.len();
                }
            }
            // A line longer than the cap is split rather than accumulated without bound.
            // act does not do this (its JSON lines are bounded), but the pump must not be
            // the place a hostile or buggy child can grow our heap.
            if line.len() >= super::MAX_LOG_LINE_BYTES {
                emit(&shared, &log_tx, &line, kind).await;
                line.clear();
            }
        }
        reader.consume(consumed);
    }
}

/// Redact once per copy, push the tail copy to the ring, queue the disk copy.
async fn emit(shared: &RunShared, log_tx: &mpsc::Sender<Vec<u8>>, raw: &[u8], kind: StreamKind) {
    let counters = &shared.counters;
    let counter = match kind {
        StreamKind::Stdout => &counters.stdout_bytes,
        StreamKind::Stderr => &counters.stderr_bytes,
    };
    let _ = counter.fetch_add(raw.len() as u64, Ordering::Relaxed);

    let tail = shared.tail_redactor.apply(raw);
    lock(&shared.ring).push(tail.as_ref());

    let disk = shared.disk_redactor.apply(raw);
    if log_tx.send(disk.to_vec()).await.is_err() {
        tracing::warn!(run_id = %shared.run_id, "log writer is gone; dropping act's output");
    }
}

/// The `act.jsonl` writer, on the blocking pool: it owns the file, enforces the on-disk cap,
/// and writes the truncate-with-marker when the cap bites (ADR-0004 §7.1).
fn spawn_writer(
    path: PathBuf,
    mut rx: mpsc::Receiver<Vec<u8>>,
    counters: Arc<LogCounters>,
    max_bytes: u64,
) -> JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        let file = match std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) => {
                tracing::error!(path = %path.display(), "could not open the run log: {error}");
                return;
            }
        };
        let mut file = std::io::BufWriter::new(file);
        let mut written: u64 = 0;
        let mut dropped: u64 = 0;
        let mut truncated = false;
        while let Some(line) = rx.blocking_recv() {
            let size = line.len() as u64;
            if written.saturating_add(size) > max_bytes {
                truncated = true;
                dropped += 1;
                continue;
            }
            if file.write_all(&line).is_err() {
                tracing::error!(path = %path.display(), "could not write the run log");
                break;
            }
            written += size;
            counters.bytes_written.store(written, Ordering::Relaxed);
        }
        if truncated {
            let marker = log::truncation_marker(written, dropped, max_bytes);
            let _ = file.write_all(marker.as_bytes());
            counters.truncated.store(true, Ordering::Relaxed);
        }
        counters.dropped_lines.store(dropped, Ordering::Relaxed);
        let _ = file.flush();
    })
}

/// Create the run's directories: scratch (`0700`, it holds the secret file), the run dir
/// (evidence layout), and the payload scratch act's job writes into.
fn prepare_directories(spec: &RunSpec) -> Result<(), RunnerError> {
    let fail = |path: &Path, reason: String| RunnerError::Prepare {
        run_id: spec.run_id.to_string(),
        path: path.to_path_buf(),
        reason,
    };
    for directory in [&spec.scratch_dir, &spec.run_dir, &spec.payload_dir] {
        if let Err(error) = std::fs::create_dir_all(directory) {
            return Err(fail(directory, format!("create_dir_all: {error}")));
        }
    }
    // 0700 on the scratch dir regardless of umask — it holds secrets, and `create_dir_all`
    // applies the umask rather than the mode asked for.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&spec.scratch_dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| fail(&spec.scratch_dir, format!("chmod 0700: {error}")))?;
    }
    Ok(())
}

/// Write the two files act's argv points at: the secret file and the inert project file.
fn write_run_files(spec: &RunSpec) -> Result<(), RunnerError> {
    let fail = |path: &Path, reason: String| RunnerError::Prepare {
        run_id: spec.run_id.to_string(),
        path: path.to_path_buf(),
        reason,
    };

    let secret_path = spec.secret_file_path();
    let mut contents = String::new();
    for (key, value) in &spec.secrets {
        contents.push_str(key);
        contents.push('=');
        contents.push_str(value);
        contents.push('\n');
    }
    // ADR-0003 §7.7: GITHUB_TOKEN is always present — the caller's value or an empty one —
    // so act never shells out to the operator's `gh auth token`.
    if !spec.secrets.contains_key("GITHUB_TOKEN") {
        contents.push_str("GITHUB_TOKEN=\n");
    }
    write_private(&secret_path, contents.as_bytes()).map_err(|error| {
        fail(
            &secret_path,
            format!("writing the secret file: {error} (values are never echoed)"),
        )
    })?;

    if spec.project_config_files_inert() {
        // ADR-0003 §7.7: exists, 0 bytes, 0600, inside the 0700 run dir.
        let inert = spec.inert_file_path();
        write_private(&inert, b"").map_err(|error| fail(&inert, error.to_string()))?;
    }
    Ok(())
}

/// Create `path` with mode 0600 and write `contents`, replacing any previous content.
fn write_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)?;
    file.write_all(contents)?;
    file.flush()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}
