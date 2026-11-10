//! The act process runner — the only place in this codebase that builds or signals an
//! act command line (mact-f1w.8, ADR-0003 §7, ADR-0004 §5).
//!
//! ```text
//! Runner::start(spec) ──► RunningActRun ──► wait() ──► ActRunOutcome
//!                              │
//!                              └── handle() ──► terminate(initiator) → KillReport
//! ```
//!
//! * [`crate::runner::build_argv`] (in the `argv` submodule) turns a
//!   [`crate::runner::RunSpec`] into act's argv — a pure function,
//!   golden-tested against ADR-0003 §7.4.
//! * [`crate::runner::Runner::start`] prepares the run's directories, writes the secret file,
//!   spawns act **in its own process group** with `kill_on_drop`, and starts the log tee.
//! * [`crate::runner::RunningActRun::wait`] owns the wait: it arms the absolute deadline,
//!   watches the cancellation token, reaps act, drains the log, and returns an
//!   [`crate::runner::ActRunOutcome`].
//! * [`crate::runner::ActRunHandle::terminate`] is the codebase's only signalling code
//!   (ADR-0004 §5.1):
//!   `killpg(SIGTERM)` → `--term-grace-secs` → `killpg(SIGKILL)` → reap, and it reports a
//!   [`crate::runner::KillReport`] that `cancel_run` copies verbatim.
//!
//! ## What the runner guarantees
//!
//! 1. **No shell, ever.** `tokio::process::Command` with an argument vector; the only
//!    string built is `KEY=value` *values* inside an argument, never a command line.
//! 2. **The whole tree dies.** `process_group(0)` at spawn, `killpg` at teardown, because
//!    act's `docker` clients and containers are in that group (ADR-0004 §5.1).
//!    `PDEATHSIG` is set as defense-in-depth (see `tests/runner_pdeathsig.rs`).
//! 3. **Secrets are gone before they are stored.** Redaction happens at the log source, so
//!    neither `act.jsonl` nor `log_tail` nor any file a run produces can carry one
//!    (ADR-0001 §2.7), and secret *values* are never argv at all. The needle list is
//!    [`crate::runner::RunSpec::redaction_needles`]: every declared secret, plus anything
//!    whose *name* says
//!    secret among the caller's `--var`/`--input`/`--env`, the operator's act-process env,
//!    and this server's own environment (mact-f1w.13).
//! 4. **Both log bounds are enforced.** `act.jsonl` is capped by `--max-run-log-bytes`
//!    with a truncate-with-marker, and the in-memory tail ring is `--log-tail-ring-bytes`
//!    so `log_tail` is always servable from memory (ADR-0004 §7.1).
//! 5. **A non-zero exit is not an error.** act exiting 7 is a `failed` workflow with
//!    `exit_code: 7`, the same status word the evidence contract uses; only act *itself*
//!    dying (a signal we did not send, at no deadline) is `error` + `act_crashed`.
//! 6. **`act.jsonl` is complete when the outcome is returned.** `wait()` joins the log
//!    pumps and the writer before it answers, so `.11` can publish evidence and write
//!    `meta.json` (ADR-0002 §1.3's "`meta.json` last") without racing the capture.
//!
//! ## What the runner deliberately does *not* do
//!
//! It does not allocate a `run_id`, take the workspace lock, materialise a worktree,
//! harvest evidence, or touch the registry — those are ADR-0004 §3.4's other steps and
//! belong to the supervisor (mact-f1w.11). It also never deletes a published file
//! (ADR-0002 §6.2). It unlinks one thing: the per-run secret file, as soon as act's exit
//! status is known (ADR-0003 §9.1).
//!
//! Platform: POSIX. act, `--bind`, `flock` and process groups are all POSIX, and the
//! crate's own tooling checker says so, so `runner` is compiled on `cfg(unix)` only.

mod argv;
mod log;
/// The crate's audited `unsafe`: `killpg(2)`, `kill(2)`, `prctl(2)`, and the `/proc` reads
/// that tell teardown whether a process group is still executing.
///
/// Public so `tests/runner_pdeathsig.rs` can exercise `PR_SET_PDEATHSIG` through exactly
/// the helper production uses (ADR-0004 §5.1 requires the verification, not an assumption)
/// and so an audit of the unsafe surface is one `grep` away.
pub mod process;
mod run;

pub use argv::{
    argv_carries, argv_contains, build_argv, default_payload_dir, ActEvent, DRYRUN_FLAG,
    EVENTPATH_FLAG,
};
pub use log::{
    fit_log_tail, name_looks_secret, secret_named_values, truncation_marker, LogRing, Redactor,
    MAX_LOG_LINE_BYTES, MAX_LOG_TAIL_BYTES, MIN_HEURISTIC_NEEDLE_BYTES, REDACTION_MARKER,
    SECRET_NAME_MARKERS,
};
pub use process::{default_run_scratch_dir, group_exists};
pub use run::{
    ActRunOutcome, KillMethod, KillReport, RunHandle as ActRunHandle, RunningActRun,
    TerminateInitiator,
};

use std::{
    collections::BTreeMap,
    fmt,
    path::{Path, PathBuf},
    time::Duration,
};

use thiserror::Error;
use tokio_util::sync::CancellationToken;

/// ADR-0004 §11: `--term-grace-secs` default.
pub const DEFAULT_TERM_GRACE_SECS: u64 = 20;
/// ADR-0004 §11: `--term-grace-secs` lower bound (also what keeps `kill.took_ms` small).
pub const MIN_TERM_GRACE_SECS: u64 = 1;
/// ADR-0004 §11 + §16.3: the maximum that keeps `kill.took_ms ≤ 300000`.
pub const MAX_TERM_GRACE_SECS: u64 = 240;
/// ADR-0004 §11: `--log-tail-ring-bytes` default, equal to `log_tail_bytes`'s maximum.
pub const DEFAULT_LOG_TAIL_RING_BYTES: usize = MAX_LOG_TAIL_BYTES;
/// ADR-0004 §11: `--max-run-log-bytes` default (64 MiB).
pub const DEFAULT_MAX_RUN_LOG_BYTES: u64 = 67_108_864;
/// ADR-0004 §11: `--max-run-log-bytes` lower bound (1 MiB).
pub const MIN_RUN_LOG_BYTES: u64 = 1_048_576;

/// Warning stamped when SIGKILL was needed to take the group down (ADR-0004 §5.6).
pub const WARNING_PROCESS_GROUP_KILLED: &str = "process_group_killed";
/// Warning stamped when SIGKILL was needed, so act's own `--rm` cleanup may not have run
/// (ADR-0004 §5.5, §5.6).
pub const WARNING_ORPHANED_CONTAINERS: &str = "orphaned_containers";
/// Warning stamped when the on-disk log hit its byte cap (ADR-0002 §5.1 via ADR-0004 §7.1).
pub const WARNING_TRUNCATED: &str = "truncated";
/// Warning stamped when `--act-no-bind` dropped `--bind` (ADR-0003 §7.7).
pub const WARNING_PAYLOAD_TRANSPORT_UNAVAILABLE: &str = "payload_transport_unavailable";

/// The five terminal status words of ADR-0001 §3.2 / `.evidence`'s `run_status`.
///
/// The runner can only ever produce these five; `pending`/`running` are the supervisor's
/// (ADR-0004 §3.1), and nothing here invents a sixth word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    /// act exited 0.
    Success,
    /// act exited non-zero: the workflow failed, the run did not (acceptance criterion).
    Failed,
    /// A caller took the run down (`initiator: caller`).
    Cancelled,
    /// `timeout_secs` expired and the group was killed.
    TimedOut,
    /// The server could not run the job to completion (`act_crashed`, shutdown).
    Error,
}

impl RunStatus {
    /// The wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed_out",
            Self::Error => "error",
        }
    }

    /// Always true: every status the runner produces is terminal (ADR-0004 §3.2).
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        true
    }
}

impl fmt::Display for RunStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a run that ended in `error` ended that way.
///
/// The `code` strings are members of ADR-0001 §5.3's closed set; the runner produces two
/// of them and the supervisor produces the rest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunFailure {
    /// act died on a signal this process did not send, at no deadline: the *runner* lost
    /// the job rather than the workflow failing (ADR-0004 §3.3 T10).
    ActCrashed {
        /// The delivering signal, when the OS reported one.
        signal: Option<i32>,
    },
    /// The server was shutting down and cut this run short (ADR-0004 §9.2, §17.3).
    ServerShutdown,
}

impl RunFailure {
    /// The `error.code` for this failure.
    pub const fn code(&self) -> &'static str {
        match self {
            Self::ActCrashed { .. } => "act_crashed",
            Self::ServerShutdown => "runner_internal_error",
        }
    }

    /// The `error.detail` this failure contributes, or `None` for an empty object.
    #[must_use]
    pub fn detail(&self) -> serde_json::Value {
        match self {
            Self::ActCrashed { signal } => serde_json::json!({
                "signal": match signal {
                    Some(number) => serde_json::Value::String(
                        process::signal_name(*number).to_owned(),
                    ),
                    None => serde_json::Value::Null,
                },
            }),
            Self::ServerShutdown => serde_json::json!({ "reason": "server_shutdown" }),
        }
    }

    /// The `error.message`, already free of host paths (ADR-0001 §5.2).
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::ActCrashed { signal } => match signal {
                Some(number) => format!(
                    "act was killed by {} before it exited on its own",
                    process::signal_name(*number)
                ),
                None => "act exited without a waitable status".to_owned(),
            },
            Self::ServerShutdown => {
                "act-mcp is shutting down; the run was terminated with the server".to_owned()
            }
        }
    }
}

/// Everything the operator pins that bounds one run's cost (ADR-0004 §11, runner rows).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerLimits {
    /// `--term-grace-secs`: how long SIGTERM gets before SIGKILL.
    pub term_grace: Duration,
    /// `--log-tail-ring-bytes`: in-memory tail budget per run.
    pub log_tail_ring_bytes: usize,
    /// `--max-run-log-bytes`: on-disk `act.jsonl` budget per run.
    pub max_run_log_bytes: u64,
    /// How long `wait()` lets the log pumps drain after act exits. Not a CLI flag:
    /// act's own grandchildren can hold the pipes open, and a hung grandchild must not
    /// decide how long a terminal transition takes (ADR-0004 §4.6's bound).
    pub log_drain: Duration,
    /// How long `terminate()` waits for the reap after SIGKILL, bounding
    /// `kill.took_ms` (ADR-0004 §16.3).
    pub reap_wait: Duration,
}

impl Default for RunnerLimits {
    fn default() -> Self {
        Self {
            term_grace: Duration::from_secs(DEFAULT_TERM_GRACE_SECS),
            log_tail_ring_bytes: DEFAULT_LOG_TAIL_RING_BYTES,
            max_run_log_bytes: DEFAULT_MAX_RUN_LOG_BYTES,
            log_drain: Duration::from_secs(5),
            reap_wait: Duration::from_secs(30),
        }
    }
}

impl RunnerLimits {
    /// The ADR-0004 §11 bounds, checked rather than assumed, because a bound that a flag
    /// can violate is not a bound:
    ///
    /// * `term_grace` in 1..=240 s — above 240 s, `kill.took_ms` could exceed its
    ///   `300000` schema bound (ADR-0004 §16.3);
    /// * `max_run_log_bytes` ≥ 1 MiB;
    /// * `log_tail_ring_bytes` ≤ 65536, because it must stay exactly `log_tail_bytes`'s
    ///   maximum (raising one raises the other, which is a wire-contract change);
    /// * `term_grace + reap_wait` ≤ 295 s, so `kill.took_ms` can never overflow the
    ///   schema bound even with a 5 s slop for signal delivery.
    pub fn checked(
        term_grace: Duration,
        log_tail_ring_bytes: usize,
        max_run_log_bytes: u64,
    ) -> Result<Self, LimitError> {
        let limits = Self {
            term_grace,
            log_tail_ring_bytes,
            max_run_log_bytes,
            ..Self::default()
        };
        limits.validate()?;
        Ok(limits)
    }

    /// Validate an already-built set.
    pub fn validate(&self) -> Result<(), LimitError> {
        let secs = self.term_grace.as_secs();
        if !(MIN_TERM_GRACE_SECS..=MAX_TERM_GRACE_SECS).contains(&secs) {
            return Err(LimitError::TermGrace {
                value: self.term_grace,
            });
        }
        if self.max_run_log_bytes < MIN_RUN_LOG_BYTES {
            return Err(LimitError::LogBytes {
                value: self.max_run_log_bytes,
            });
        }
        if self.log_tail_ring_bytes == 0 || self.log_tail_ring_bytes > MAX_LOG_TAIL_BYTES {
            return Err(LimitError::TailRing {
                value: self.log_tail_ring_bytes,
            });
        }
        if self.term_grace + self.reap_wait > Duration::from_secs(295) {
            return Err(LimitError::TookMsBound {
                grace: self.term_grace,
                reap_wait: self.reap_wait,
            });
        }
        Ok(())
    }
}

/// Reasons a [`RunnerLimits`] is unusable.
#[allow(missing_docs, reason = "the #[error] string documents each variant")]
#[derive(Debug, Error)]
pub enum LimitError {
    #[error("--term-grace-secs {value:?} is out of range (1..=240 s, because kill.took_ms is schema-bounded at 300000 ms)")]
    TermGrace { value: Duration },
    #[error("--max-run-log-bytes {value} is below the 1 MiB floor")]
    LogBytes { value: u64 },
    #[error("--log-tail-ring-bytes {value} is out of range (1..=65536; it must stay equal to log_tail_bytes' maximum)")]
    TailRing { value: usize },
    #[error("term grace {grace:?} + reap wait {reap_wait:?} cannot stay under kill.took_ms's 300000 ms bound")]
    TookMsBound {
        grace: Duration,
        reap_wait: Duration,
    },
}

/// Reasons [`Runner::start`] refused.
///
/// These are *internal* errors with operator-facing text: they may name host paths, so
/// the supervisor must map them onto an ADR-0001 §5.3 code (`act_spawn_failed`,
/// `runner_internal_error`) with a sanitised message rather than echoing them onto the
/// wire (ADR-0001 §5.2 forbids absolute host paths in a tool result).
#[allow(missing_docs, reason = "the #[error] string documents each variant")]
#[derive(Debug, Error)]
pub enum RunnerError {
    #[error("invalid run specification: {field} = {value:?}: {reason}")]
    InvalidSpec {
        field: &'static str,
        value: String,
        reason: String,
    },
    #[error("could not prepare {path} for run {run_id}: {reason}")]
    Prepare {
        run_id: String,
        path: PathBuf,
        reason: String,
    },
    #[error("act could not be spawned for run {run_id} ({program}): {reason}")]
    Spawn {
        run_id: String,
        program: String,
        reason: String,
    },
}

impl RunnerError {
    /// The ADR-0004 §3.3 `error.code` this failure corresponds to.
    ///
    /// The runner does not *publish* the run (that is the supervisor's `publish_run`), but
    /// it is the one that knows which of these three things went wrong, so the mapping
    /// lives here and the supervisor copies it into `run_view.error` verbatim.
    /// `act_not_found` is deliberately absent: a missing `act` binary is `Spawn` with a
    /// `NotFound` os error, and the supervisor decides between the two codes by looking at
    /// whether act was found at all (ADR-0004 §3.3 T7 vs T8).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidSpec { .. } => "validation_failed",
            Self::Prepare { .. } => "internal_error",
            Self::Spawn { .. } => "act_spawn_failed",
        }
    }
}

/// A run identity: `YYYYMMDDTHHMMSSZ-<6 lowercase hex>` (ADR-0002 §1.2).
///
/// A type rather than a `String` because it is spliced into paths (`.evidence/runs/…`,
/// `--run-scratch/<run>/`) and into `meta.json`, so "is this the shape" has to be a
/// compile-time habit, not a per-call check. Holding it also means `ls` order is
/// chronological for free, which is the whole reason the shape is time-first.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RunId(String);

impl RunId {
    /// `20261110T004203Z-a1b2c3` is 23 bytes: 8 date digits, `T`, 6 time digits,
    /// `Z`, `-`, 6 hex.
    pub const LENGTH: usize = 23;

    /// Allocate a fresh id: the current UTC second plus 3 random bytes.
    ///
    /// Uniqueness within a second comes from `/dev/urandom`, not from a counter: a
    /// restart must not be able to replay an id, and a counter that resets on restart
    /// would collide with a run the previous process is (per ADR-0004 §9.3) possibly still
    /// orphaning.
    pub fn generate() -> Self {
        let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
        Self(format!("{stamp}-{}", hex_random3()))
    }

    /// Accept `text` only if it matches the pinned shape.
    pub fn parse(text: &str) -> Result<Self, RunnerError> {
        if Self::is_valid_shape(text) {
            Ok(Self(text.to_owned()))
        } else {
            Err(RunnerError::InvalidSpec {
                field: "run_id",
                value: text.to_owned(),
                reason: format!(
                    "expected YYYYMMDDTHHMMSSZ-<6 lowercase hex> (ADR-0002 §1.2), e.g. {}",
                    "20261110T004203Z-a1b2c3"
                ),
            })
        }
    }

    /// The shape check ADR-0002 §1.2 states as a regex, written by hand so there is no
    /// regex engine on a hot path: 8 digits, `T`, 6 digits, `Z`, `-`, 6 lowercase hex.
    #[must_use]
    pub fn is_valid_shape(text: &str) -> bool {
        let bytes = text.as_bytes();
        if bytes.len() != Self::LENGTH {
            return false;
        }
        let digit = |i: usize| bytes[i].is_ascii_digit();
        let hex = |i: usize| bytes[i].is_ascii_hexdigit() && !bytes[i].is_ascii_uppercase();
        bytes[8] == b'T'
            && bytes[15] == b'Z'
            && bytes[16] == b'-'
            && (0..8).all(digit)
            && (9..15).all(digit)
            && (17..23).all(hex)
    }

    /// The id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for RunId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Three random bytes as 6 hex characters, from `/dev/urandom`.
///
/// Falls back to clock-plus-pid entropy if `/dev/urandom` is unopenable, which on Linux
/// it is not; the fallback keeps `generate()` infallible rather than turning a
/// filesystem surprise into a failed run before it began.
fn hex_random3() -> String {
    let mut buffer = [0u8; 3];
    let filled = match std::fs::File::open("/dev/urandom") {
        Ok(mut file) => std::io::Read::read_exact(&mut file, &mut buffer).is_ok(),
        Err(_) => false,
    };
    if !filled {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.subsec_nanos())
            .unwrap_or(0);
        let mixed = nanos ^ (std::process::id() << 8);
        buffer = [
            (mixed & 0xff) as u8,
            ((mixed >> 8) & 0xff) as u8,
            ((mixed >> 16) & 0xff) as u8,
        ];
    }
    buffer
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
}

/// Whether the repository's own project-config files are hidden from act.
///
/// `--act-workspace-config-files` (ADR-0003 §11): `inert` (the default) points
/// `--var-file`/`--env-file`/`--input-file` at an empty file we own, so a `.env` shipped
/// by the repository cannot inject host state into a measurement; `discover` leaves
/// act's own defaults, for a repository that legitimately depends on them.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ProjectConfigFiles {
    /// Redirect the three project files to our own 0-byte file (ADR-0003 §7.3, F1).
    #[default]
    Inert,
    /// Leave act's `.vars`/`.env`/`.input` lookup alone.
    Discover,
}

/// One act run, fully described before anything is spawned.
///
/// Built by the supervisor (mact-f1w.11/.14) from a validated target plus the operator's
/// configuration. The fields are public rather than behind setters because the whole
/// point is that this is a *description*, not an object with behaviour: [`Runner::start`]
/// reads it and never mutates it, so a caller can log it, compare it, or build two runs
/// from one spec.
///
/// Every path is absolute and must already be usable (or creatable); ADR-0003 §7.1 rule 1
/// is what `validate` enforces.
#[derive(Debug, Clone)]
pub struct RunSpec {
    /// The run identity (never caller-chosen).
    pub run_id: RunId,
    /// The `act` executable (already resolved by `Config`, ADR-0001 §2.9 step 7).
    pub act_binary: PathBuf,
    /// `<tree>`: the workspace (mode `head_only`) or the synthesized worktree (`worktree`).
    pub tree: PathBuf,
    /// `<tree>/.github/workflows/<file>`, as resolved by mact-f1w.6.
    pub workflow_file: PathBuf,
    /// The job id — exactly what `--job` receives (ADR-0001 §2.3).
    pub job: String,
    /// The operator's workspace, for `<WORKSPACE>` substitution in `log_tail`.
    pub workspace: PathBuf,
    /// The evidence root, for `<EVIDENCE>` substitution.
    pub evidence_dir: PathBuf,
    /// `.evidence/runs/<run_id>/` — where `act.jsonl` lands.
    pub run_dir: PathBuf,
    /// `<run-scratch>/<run_id>/` — secret file, inert file, and act's cwd (ADR-0003 §7.6).
    pub scratch_dir: PathBuf,
    /// `ACTMCP_EVIDENCE_OUT`: the per-run payload scratch (ADR-0002 §3.1).
    pub payload_dir: PathBuf,
    /// `timeout_secs` for this run, measured from act's spawn (ADR-0004 §4.1).
    pub timeout: Duration,
    /// Whether to pass `--bind` (`--act-no-bind` clears it, ADR-0003 §7.7).
    pub bind: bool,
    /// `--pull=<bool>`; ADR-0003 §7.3 pins `false` as the default.
    pub pull: bool,
    /// Whether to pass `--use-gitignore` (ADR-0003 F7).
    pub use_gitignore: bool,
    /// `--act-workspace-config-files`.
    pub project_config_files: ProjectConfigFiles,
    /// `--defaultbranch`, worktree mode only (ADR-0003 §5.4 row 2).
    pub default_branch: Option<String>,
    /// `--input k=v` (ADR-0001 §2.7).
    pub inputs: BTreeMap<String, String>,
    /// `--var k=v`.
    pub vars: BTreeMap<String, String>,
    /// `--matrix k:v`.
    pub matrix: BTreeMap<String, String>,
    /// Extra `--env k=v` for the job container, beyond the two `ACTMCP_*` ones.
    pub extra_env: BTreeMap<String, String>,
    /// Secrets. Never argv: written to `secrets.env` (mode 0600) and passed as
    /// `--secret-file` (ADR-0001 §2.7).
    pub secrets: BTreeMap<String, String>,
    /// Extra environment for the **act process itself** (not the job): the operator seam,
    /// and the seam the stub-act tests drive. `GITHUB_TOKEN` is filled from `secrets`
    /// automatically (ADR-0001 §2.7).
    pub process_env: BTreeMap<String, String>,
    /// Opt-in event for act (see the `argv` submodule's module docs).
    pub event: Option<ActEvent>,
    /// `--dryrun`: act validates the workflow and exits.
    pub dry_run: bool,
    /// Cancelling this token takes the run down with `initiator: caller` (ADR-0004 §17.3).
    pub cancellation: CancellationToken,
}

impl RunSpec {
    /// The minimum a run needs, with ADR defaults for everything else.
    #[must_use]
    pub fn new(
        run_id: RunId,
        act_binary: impl Into<PathBuf>,
        tree: impl Into<PathBuf>,
        workflow_file: impl Into<PathBuf>,
        job: impl Into<String>,
    ) -> Self {
        let run = run_id.as_str().to_owned();
        let tree = tree.into();
        let act_binary = act_binary.into();
        let workflow_file = workflow_file.into();
        let workspace = tree.clone();
        let evidence_dir = workspace.join(".evidence");
        let scratch_dir = default_run_scratch_dir().join(&run);
        Self {
            run_id,
            act_binary,
            workflow_file,
            tree: tree.clone(),
            job: job.into(),
            workspace,
            evidence_dir: evidence_dir.clone(),
            run_dir: evidence_dir.join("runs").join(&run),
            scratch_dir,
            payload_dir: default_payload_dir(&tree, &run),
            timeout: Duration::from_secs(1800),
            bind: true,
            pull: false,
            use_gitignore: true,
            project_config_files: ProjectConfigFiles::Inert,
            default_branch: None,
            inputs: BTreeMap::new(),
            vars: BTreeMap::new(),
            matrix: BTreeMap::new(),
            extra_env: BTreeMap::new(),
            secrets: BTreeMap::new(),
            process_env: BTreeMap::new(),
            event: None,
            dry_run: false,
            cancellation: CancellationToken::new(),
        }
    }

    /// `<run-scratch>/<run>/secrets.env` (ADR-0003 §7.2).
    #[must_use]
    pub fn secret_file_path(&self) -> PathBuf {
        self.scratch_dir.join("secrets.env")
    }

    /// Every value this run must not let reach a log, a tool result, or an evidence file.
    ///
    /// Three sources, in three different confidence classes:
    ///
    /// 1. **declared secrets** (`secrets`, i.e. what goes into `secrets.env`) — every value,
    ///    with no length floor, because the caller told us it is a secret;
    /// 2. **caller-supplied values** (`--var`, `--input`, `--matrix`, the job's `--env`) —
    ///    only the pairs whose *key* looks secret ([`name_looks_secret`]), because a `--var`
    ///    is routinely a build flag and masking all of them would mask the log rather than
    ///    the secret (see [`MIN_HEURISTIC_NEEDLE_BYTES`] for the length floor);
    /// 3. **the environment** — the act process env and this server's own `std::env`, again
    ///    name-matched. This is what catches `GITHUB_TOKEN`/`NPM_TOKEN` that the operator
    ///    exported for act's image pulls and never declared to the tool: act-mcp passes its
    ///    own environment through (ADR-0003 §7.6), so a job that `env`s its surroundings
    ///    would otherwise paste the operator's credentials into `act.jsonl`.
    ///
    /// Deliberately *not* included: every value of every map. The redactor runs on every line
    /// of a chatty build, and a needle list built from all env values both masks ordinary
    /// output and makes the cost of a line proportional to the size of the environment.
    #[must_use]
    pub fn redaction_needles(&self) -> Vec<String> {
        let mut needles: Vec<String> = self
            .secrets
            .values()
            .filter(|value| !value.is_empty())
            .cloned()
            .collect();

        let mut name_matched: Vec<&str> = Vec::new();
        for map in [
            &self.vars,
            &self.inputs,
            &self.matrix,
            &self.extra_env,
            &self.process_env,
        ] {
            name_matched.extend(secret_named_values(
                map.iter()
                    .map(|(key, value)| (key.as_str(), value.as_str())),
            ));
        }
        // The server's own environment, read once per run rather than per line.
        let server_env: Vec<(String, String)> = std::env::vars().collect();
        name_matched.extend(secret_named_values(
            server_env
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
        ));
        needles.extend(name_matched.into_iter().map(str::to_owned));

        needles.sort();
        needles.dedup();
        needles
    }

    /// `<run-scratch>/<run>/inert.conf` (ADR-0003 §7.3, §7.7).
    #[must_use]
    pub fn inert_file_path(&self) -> PathBuf {
        self.scratch_dir.join("inert.conf")
    }

    /// Whether the three project-config flags point at our inert file.
    #[must_use]
    pub fn project_config_files_inert(&self) -> bool {
        self.project_config_files == ProjectConfigFiles::Inert
    }

    /// Refuse a spec that cannot be spawned safely, **before** touching the filesystem.
    ///
    /// This is defence-in-depth, not the caller's validation gate (that is mact-f1w.6 and
    /// the tool layer): a `run_id` that is not the pinned shape, a relative path that act
    /// would resolve against an unexpected directory, a workflow file outside the tree,
    /// a secret value with a newline (which would corrupt `secrets.env` into two lines and
    /// silently re-key it), or an env key that could shadow the server's own `ACTMCP_*`
    /// contract.
    pub fn validate(&self) -> Result<(), RunnerError> {
        let bad = |field: &'static str, value: String, reason: String| RunnerError::InvalidSpec {
            field,
            value,
            reason,
        };

        if self.act_binary.as_os_str().is_empty() {
            return Err(bad(
                "act_binary",
                String::new(),
                "must name an executable".to_owned(),
            ));
        }
        for (field, path) in [
            ("tree", &self.tree),
            ("workflow_file", &self.workflow_file),
            ("run_dir", &self.run_dir),
            ("scratch_dir", &self.scratch_dir),
            ("payload_dir", &self.payload_dir),
        ] {
            if !path.is_absolute() {
                return Err(bad(
                    field,
                    path.display().to_string(),
                    "must be absolute (ADR-0003 §7.1 rule 1)".to_owned(),
                ));
            }
        }
        if !under(&self.workflow_file, &self.tree) {
            return Err(bad(
                "workflow_file",
                self.workflow_file.display().to_string(),
                "must live inside the tree passed as --directory".to_owned(),
            ));
        }
        if under(&self.scratch_dir, &self.workspace) {
            // ADR-0003 §11: --run-scratch must not be inside the workspace, because F8
            // makes everything inside the workspace readable by the job — and the job
            // reads secrets that way.
            return Err(bad(
                "scratch_dir",
                self.scratch_dir.display().to_string(),
                "must not be inside the workspace: --bind makes the whole tree readable by the job"
                    .to_owned(),
            ));
        }
        if self.job.is_empty() || self.job.contains(['\n', '\0']) {
            return Err(bad(
                "job",
                self.job.clone(),
                "must be a single-line job id".to_owned(),
            ));
        }
        if self.timeout.is_zero() || self.timeout > Duration::from_secs(86_400) {
            return Err(bad(
                "timeout",
                format!("{:?}", self.timeout),
                "must be in 1..=86400 s (ADR-0004 §4.3)".to_owned(),
            ));
        }
        for (key, value) in &self.secrets {
            if !is_env_key(key) {
                return Err(bad(
                    "secrets",
                    key.clone(),
                    "must be [A-Za-z_][A-Za-z0-9_]*".to_owned(),
                ));
            }
            if value.contains(['\n', '\r', '\0']) {
                return Err(bad(
                    "secrets",
                    key.clone(),
                    "value must be single-line: secrets.env is a KEY=value file".to_owned(),
                ));
            }
        }
        for (map_name, map) in [
            ("extra_env", &self.extra_env),
            ("process_env", &self.process_env),
        ] {
            for (key, value) in map {
                if !is_env_key(key) {
                    return Err(bad(
                        map_name,
                        key.clone(),
                        "must be [A-Za-z_][A-Za-z0-9_]*".to_owned(),
                    ));
                }
                if key.starts_with("ACTMCP_") {
                    return Err(bad(
                        map_name,
                        key.clone(),
                        "ACTMCP_* keys belong to the server (ADR-0002 §3.1)".to_owned(),
                    ));
                }
                if value.contains(['\n', '\0']) {
                    return Err(bad(
                        map_name,
                        key.clone(),
                        "value must be single-line".to_owned(),
                    ));
                }
            }
        }
        for (map_name, map) in [
            ("inputs", &self.inputs),
            ("vars", &self.vars),
            ("matrix", &self.matrix),
        ] {
            for (key, value) in map {
                if key.is_empty() || key.contains(['\n', '\0', '=', ':']) {
                    return Err(bad(
                        map_name,
                        key.clone(),
                        "key must be single-line and free of = and :".to_owned(),
                    ));
                }
                if value.contains(['\n', '\0']) {
                    return Err(bad(
                        map_name,
                        key.clone(),
                        "value must be single-line".to_owned(),
                    ));
                }
            }
        }
        if let Some(ActEvent::Name(name)) = &self.event {
            if !ActEvent::is_valid_name(name) {
                return Err(bad(
                    "event",
                    name.clone(),
                    "not a valid GitHub event name".to_owned(),
                ));
            }
        }
        if let Some(branch) = &self.default_branch {
            if branch.is_empty() || branch.contains(['\n', '\0']) {
                return Err(bad(
                    "default_branch",
                    branch.clone(),
                    "must be a single branch name".to_owned(),
                ));
            }
        }
        Ok(())
    }
}

/// Component-wise prefix test (`candidate` inside `root`), no symlink resolution.
fn under(candidate: &Path, root: &Path) -> bool {
    let root = root.components().collect::<Vec<_>>();
    let candidate = candidate.components().collect::<Vec<_>>();
    !root.is_empty() && candidate.starts_with(&root)
}

/// A name usable as an environment variable / dotenv key.
fn is_env_key(key: &str) -> bool {
    let mut bytes = key.bytes();
    match bytes.next() {
        Some(first) if first.is_ascii_alphabetic() || first == b'_' => {}
        _ => return false,
    }
    bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// The runner itself: a set of limits plus a spawn method. Cheap to clone and share.
#[derive(Debug, Clone)]
pub struct Runner {
    limits: std::sync::Arc<RunnerLimits>,
}

impl Runner {
    /// Build a runner from validated limits.
    pub fn new(limits: RunnerLimits) -> Result<Self, LimitError> {
        limits.validate()?;
        Ok(Self {
            limits: std::sync::Arc::new(limits),
        })
    }

    /// A runner with ADR-0004 §11's defaults.
    #[must_use]
    pub fn with_default_limits() -> Self {
        Self::default()
    }

    /// The limits this runner enforces.
    #[must_use]
    pub fn limits(&self) -> &RunnerLimits {
        &self.limits
    }

    /// Spawn act for `spec` and start capturing its log.
    ///
    /// Returns as soon as `exec` succeeded (ADR-0004 §3.3 T7: `started_at` is this instant,
    /// and `timeout` is measured from it); the wait, the deadline and the teardown are the
    /// returned [`RunningActRun`]'s job.
    pub async fn start(&self, spec: RunSpec) -> Result<RunningActRun, RunnerError> {
        run::start(spec, std::sync::Arc::clone(&self.limits)).await
    }
}

impl Default for Runner {
    fn default() -> Self {
        Self {
            limits: std::sync::Arc::new(RunnerLimits::default()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> RunSpec {
        RunSpec::new(
            RunId::parse("20261110T004203Z-a1b2c3").expect("valid id"),
            "/usr/bin/act",
            "/srv/ws",
            "/srv/ws/.github/workflows/ci.yml",
            "build",
        )
    }

    #[test]
    fn run_id_shape_is_the_adr_regex() {
        for good in [
            "20261110T004203Z-a1b2c3",
            "19700101T000000Z-000000",
            "20991231T235959Z-abcdef",
        ] {
            assert!(RunId::is_valid_shape(good), "{good} should be valid");
            assert!(RunId::parse(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            "20261110T004203Z-a1b2c",
            "20261110T004203Z-a1b2c34",
            "20261110T004203Z-A1B2C3",
            "20261110T004203+0000-a1b2c3",
            "../../etc/passwd",
            "2026-11-10T004203Z-a1b2c3",
            "20261110T00420Z-a1b2c3",
        ] {
            assert!(!RunId::is_valid_shape(bad), "{bad:?} must be refused");
            assert!(matches!(
                RunId::parse(bad),
                Err(RunnerError::InvalidSpec { .. })
            ));
        }
    }

    #[test]
    fn generated_run_ids_parse_and_are_not_repeated() {
        let first = RunId::generate();
        let second = RunId::generate();
        assert!(RunId::is_valid_shape(first.as_str()), "{first}");
        assert!(RunId::is_valid_shape(second.as_str()), "{second}");
        assert_eq!(first.as_str().len(), RunId::LENGTH);
        // Not a guaranteed-unique property (two runs may share a second and three random
        // bytes), but a constant or an empty hex tail would fail immediately here.
        assert_ne!(first, second, "two generated ids were identical");
        assert_ne!(
            &first.as_str()[17..],
            "000000",
            "the hex tail looks like a fallback that never got entropy"
        );
    }

    #[test]
    fn the_redaction_needles_are_the_declared_secrets_plus_everything_named_like_one() {
        let mut spec = spec();
        spec.secrets = BTreeMap::from([
            ("GITHUB_TOKEN".to_owned(), "ghp_declared".to_owned()),
            ("SHORT".to_owned(), "x".to_owned()),
            ("EMPTY".to_owned(), String::new()),
        ]);
        // A caller that passes a credential as a `--var` gets it redacted by name, and one
        // that passes an ordinary build flag does not get its log shredded.
        spec.vars = BTreeMap::from([
            ("NPM_TOKEN".to_owned(), "npm-from-var".to_owned()),
            ("BUILD_MODE".to_owned(), "release-debug".to_owned()),
        ]);
        spec.inputs = BTreeMap::from([("RELEASE_PASSWORD".to_owned(), "p@ssw0rd-long".to_owned())]);
        spec.extra_env =
            BTreeMap::from([("PLAIN_ENV".to_owned(), "not-a-secret-value".to_owned())]);
        // Short values under the name rule are skipped on purpose (see the constant's doc).
        spec.process_env = BTreeMap::from([("API_KEY".to_owned(), "ab".to_owned())]);

        let needles = spec.redaction_needles();
        for wanted in ["ghp_declared", "x", "npm-from-var", "p@ssw0rd-long"] {
            assert!(
                needles.contains(&wanted.to_owned()),
                "{wanted} missing: {needles:?}"
            );
        }
        for not_wanted in ["release-debug", "not-a-secret-value", "ab", ""] {
            assert!(
                !needles.contains(&not_wanted.to_owned()),
                "{not_wanted:?} must not be a needle: {needles:?}"
            );
        }
        // The server's own environment is scanned by name too, and a value taken from it can
        // only be an operator's, so assert through the seam rather than the ambient env.
        std::env::set_var("ACT_MCP_TEST_ONLY_SECRET", "from-the-server-env");
        assert!(spec
            .redaction_needles()
            .contains(&"from-the-server-env".to_owned()));
        std::env::remove_var("ACT_MCP_TEST_ONLY_SECRET");
        // Deterministic and deduped: the same spec twice gives the same list.
        assert_eq!(spec.redaction_needles(), spec.redaction_needles());
    }

    #[test]
    fn defaults_are_the_adrs() {
        let limits = RunnerLimits::default();
        assert_eq!(limits.term_grace, Duration::from_secs(20));
        assert_eq!(limits.log_tail_ring_bytes, 65_536);
        assert_eq!(limits.max_run_log_bytes, 67_108_864);
        limits.validate().expect("defaults validate");

        let spec = spec();
        assert_eq!(spec.timeout, Duration::from_secs(1800));
        assert!(spec.bind);
        assert!(!spec.pull, "ADR-0003 §7.3 pins --pull=false");
        assert_eq!(spec.project_config_files, ProjectConfigFiles::Inert);
        assert_eq!(
            spec.run_dir,
            PathBuf::from("/srv/ws/.evidence/runs/20261110T004203Z-a1b2c3")
        );
        assert_eq!(
            spec.payload_dir,
            PathBuf::from("/srv/ws/.evidence/incoming/20261110T004203Z-a1b2c3")
        );
        assert_eq!(
            spec.secret_file_path(),
            default_run_scratch_dir().join("20261110T004203Z-a1b2c3/secrets.env")
        );
        assert!(spec.validate().is_ok());
    }

    #[test]
    fn limits_outside_the_adr_bounds_are_refused() {
        assert!(RunnerLimits::checked(Duration::from_secs(0), 65_536, 67_108_864).is_err());
        assert!(RunnerLimits::checked(Duration::from_secs(241), 65_536, 67_108_864).is_err());
        assert!(RunnerLimits::checked(Duration::from_secs(20), 65_536, 512).is_err());
        assert!(RunnerLimits::checked(Duration::from_secs(20), 0, 67_108_864).is_err());
        assert!(RunnerLimits::checked(Duration::from_secs(20), 70_000, 67_108_864).is_err());
        // A grace so long that kill.took_ms could not stay under its schema bound.
        let mut wide = RunnerLimits::checked(Duration::from_secs(240), 65_536, 67_108_864)
            .expect("240 s is allowed");
        wide.reap_wait = Duration::from_secs(120);
        assert!(matches!(
            wide.validate(),
            Err(LimitError::TookMsBound { .. })
        ));
        assert!(Runner::new(wide).is_err());
    }

    #[test]
    fn a_spec_that_cannot_be_spawned_safely_is_refused_before_anything_happens() {
        // Relative paths: act would resolve them against its cwd, and its cwd is our
        // scratch dir, so "relative" means "wrong directory" (ADR-0003 F1).
        let mut s = spec();
        s.tree = PathBuf::from("srv/ws");
        s.workflow_file = PathBuf::from("srv/ws/.github/workflows/ci.yml");
        assert!(matches!(
            s.validate(),
            Err(RunnerError::InvalidSpec { field: "tree", .. })
        ));

        // A workflow file outside the tree (path escape).
        let mut s = spec();
        s.workflow_file = PathBuf::from("/etc/passwd");
        assert!(matches!(
            s.validate(),
            Err(RunnerError::InvalidSpec {
                field: "workflow_file",
                ..
            })
        ));

        // A secret value with a newline would turn one KEY=value line into two.
        let mut s = spec();
        s.secrets = BTreeMap::from([("TOKEN".to_owned(), "a\nB=c".to_owned())]);
        assert!(matches!(
            s.validate(),
            Err(RunnerError::InvalidSpec {
                field: "secrets",
                ..
            })
        ));

        // A secret key that is not an identifier.
        let mut s = spec();
        s.secrets = BTreeMap::from([("bad key".to_owned(), "v".to_owned())]);
        assert!(s.validate().is_err());

        // A caller cannot shadow the server's own payload contract.
        let mut s = spec();
        s.extra_env = BTreeMap::from([("ACTMCP_RUN_ID".to_owned(), "mine".to_owned())]);
        assert!(matches!(
            s.validate(),
            Err(RunnerError::InvalidSpec {
                field: "extra_env",
                ..
            })
        ));

        // A scratch dir inside the workspace would be job-readable (--bind).
        let mut s = spec();
        s.scratch_dir = PathBuf::from("/srv/ws/.scratch/20261110T004203Z-a1b2c3");
        assert!(matches!(
            s.validate(),
            Err(RunnerError::InvalidSpec {
                field: "scratch_dir",
                ..
            })
        ));

        // A zero timeout would mean "kill it immediately"; the wire forbids 0 anyway.
        let mut s = spec();
        s.timeout = Duration::ZERO;
        assert!(s.validate().is_err());

        // An event name that act would read as a flag.
        let mut s = spec();
        s.event = Some(ActEvent::Name("--json".to_owned()));
        assert!(s.validate().is_err());
    }

    #[test]
    fn runner_error_codes_are_the_adrs_ones() {
        // ADR-0004 §3.3: these are the codes the supervisor copies into `run_view.error`.
        assert_eq!(
            RunnerError::InvalidSpec {
                field: "job",
                value: String::new(),
                reason: "empty".to_owned()
            }
            .code(),
            "validation_failed"
        );
        assert_eq!(
            RunnerError::Prepare {
                run_id: "20261110T004203Z-a1b2c3".to_owned(),
                path: PathBuf::from("/run"),
                reason: "read-only".to_owned()
            }
            .code(),
            "internal_error"
        );
        assert_eq!(
            RunnerError::Spawn {
                run_id: "20261110T004203Z-a1b2c3".to_owned(),
                program: "/usr/bin/act".to_owned(),
                reason: "No such file or directory".to_owned()
            }
            .code(),
            "act_spawn_failed"
        );
    }

    #[test]
    fn status_words_are_exactly_the_five_terminal_ones() {
        let statuses = [
            RunStatus::Success,
            RunStatus::Failed,
            RunStatus::Cancelled,
            RunStatus::TimedOut,
            RunStatus::Error,
        ];
        let words = statuses
            .iter()
            .map(|status| status.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            words,
            ["success", "failed", "cancelled", "timed_out", "error"]
        );
        assert!(statuses.iter().all(|status| status.is_terminal()));
        assert!(
            !words.contains(&"succeeded"),
            "ADR-0004 §3.2: 'succeeded' is not a status"
        );
        // The two non-terminal words of ADR-0004 §3.2 are the supervisor's, not ours.
        assert!(!words.contains(&"pending") && !words.contains(&"running"));
    }

    #[test]
    fn failure_codes_are_members_of_the_closed_error_code_set() {
        let schema: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/schema/tool-contract.schema.json"
            ))
            .expect("the tool contract schema is part of the tree"),
        )
        .expect("valid JSON");
        let codes = schema["$defs"]["error_code"]["enum"]
            .as_array()
            .expect("$defs/error_code is an enum");
        for failure in [
            RunFailure::ActCrashed { signal: Some(9) },
            RunFailure::ServerShutdown,
        ] {
            assert!(
                codes.contains(&serde_json::Value::String(failure.code().to_owned())),
                "{} is not in $defs/error_code",
                failure.code()
            );
            assert!(
                !failure.message().contains('/'),
                "a message that reaches the wire must not leak a host path"
            );
        }
        assert_eq!(
            RunFailure::ServerShutdown.detail()["reason"],
            "server_shutdown"
        );
        assert_eq!(
            RunFailure::ActCrashed { signal: Some(9) }.detail()["signal"],
            "SIGKILL"
        );
    }
}
