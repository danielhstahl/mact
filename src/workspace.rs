//! Can a `run_workflow` call like this one be honoured here, right now?
//!
//! [`Validator::validate`] turns untrusted tool arguments plus the state of the
//! machine into either a [`ValidatedRunTarget`] — the canonical workflow file, the
//! job id, the full commit SHA — or one [`ValidationError`] whose `code` is a
//! member of ADR-0001 §5.3's closed set. That is the whole module: it never
//! allocates a `run_id`, never spawns act, never opens a socket and never fetches
//! (ADR-0003 §3.2). Everything it touches goes through the [`Environment`] seam, so
//! the unit tests run without a container daemon, an act install, or a network.
//!
//! The order of checks is pinned by ADR-0001 §2.9 and is part of the contract,
//! because "which error do I get when two things are wrong" is a test assertion:
//!
//! | step | what runs | codes it can produce |
//! |---|---|---|
//! | shape (no filesystem, no subprocess) | `commit`/`workflow`/`task`/`category` patterns and bounds | `invalid_arguments` |
//! | workspace | exists, is a directory, is a git work tree root, has a non-empty `.github/workflows` | `workspace_not_found`, `not_a_git_repository`, `workflows_dir_missing` |
//! | commit resolution | `git rev-parse --verify --quiet --end-of-options '<sha>^{commit}'`, then the R4/R5 classification | `commit_unknown`, `commit_ambiguous` |
//! | workflow + task resolution | one file under `.github/workflows`, that file declares the job id | `workflow_not_found`, `ambiguous_workflow`, `task_not_found` |
//! | environment probes (cached 30 s) | act resolves and is new enough, container runtime answers | `act_missing`, `docker_unreachable` |
//! | isolation gate (ADR-0003 §4) | the tree the run would use is a tree we may measure | `isolation_failed` |
//!
//! Which *tree* gets inspected is a decision of its own (ADR-0003 §4): the default
//! [`TreeSelector`] implements the pinned mode table — `HEAD` in place, any other
//! commit read out of its own tree — and the wiring ticket replaces it once it owns a
//! materialized scratch worktree. That seam is the only part of this module a later
//! ticket is expected to override.
//!
//! Two rules that look like details and are not:
//!
//! * **Shape before subprocess.** A commit that is not 7..=40 hex, a workflow name
//!   containing `..`, a leading `-`: all rejected without running anything. That is
//!   argument-injection and path-injection defence, and a test asserts the call
//!   count, not just the verdict.
//! * **Never fetch.** A commit this clone does not have is `commit_unknown`. A
//!   local, unauthenticated server must not dial a remote on a typo.
//!
//! [`Environment`]: environment::Environment
//! [`Validator::validate`]: validator::Validator::validate

mod environment;
mod validator;

#[cfg(test)]
mod tests;

pub use environment::{
    parse_act_version, Clock, CommandError, CommandOutput, ContainerProbeError, ContainerRuntime,
    Environment, ProgramError, SemVer, SystemClock, SystemEnvironment,
    DEFAULT_CONTAINER_PROBE_TIMEOUT,
};
pub use validator::{
    DefaultTreeSelector, TreePlan, TreeRoot, TreeSelector, ValidatedRunTarget, ValidationPolicy,
    Validator, DEFAULT_GIT_TIMEOUT, DEFAULT_PROBE_TTL, MAX_GIT_TIMEOUT,
};

use std::{fmt, path::Path, str::FromStr};

use serde_json::{json, Map, Value};
use thiserror::Error;

/// Where workflow files live, relative to the workspace root (ADR-0001 §2.2).
pub const WORKFLOWS_DIR: &str = ".github/workflows";

/// Oldest act release this server will run (ADR-0003 §7.7's floor).
///
/// `--minimum-act-version` may raise it; a value below this is a boot error,
/// because flags emitted by mact-f1w.8 are only known-present across
/// 0.2.62..=0.2.97.
pub const MINIMUM_ACT_VERSION: &str = "0.2.62";

/// `error.message` bound, ADR-0001 §9.
pub const MAX_ERROR_MESSAGE_BYTES: usize = 1024;

/// `error.detail` bound when compactly serialized, ADR-0001 §9.
pub const MAX_ERROR_DETAIL_BYTES: usize = 4096;

/// Longest `available`/`jobs` list a resolution error may carry.
///
/// ADR-0001 §5.2 requires resolution errors to *name the valid options*; the cap
/// keeps `detail` under its byte budget on a repository with hundreds of
/// workflows, and lists are truncated whole-element, never mid-element.
pub const MAX_LISTED_OPTIONS: usize = 64;

/// `commit` bounds, ADR-0001 §2.5 / `$defs/commit_input`.
const COMMIT_MIN_LEN: usize = 7;
const COMMIT_MAX_LEN: usize = 40;

/// Warning stamped on a run whose tree was synthesized (ADR-0003 D-3).
pub const WARNING_ISOLATION_WORKTREE: &str = "isolation_worktree";

/// Warning stamped when `--allow-dirty-worktree` downgraded gate G5 (ADR-0003 §9.3).
pub const WARNING_DIRTY_WORKTREE: &str = "dirty_worktree";

/// The four caller arguments validation consumes, as `run_workflow` defines them
/// (ADR-0001 §2). There are no path arguments: the workspace comes from
/// [`ValidationPolicy`], which is built from the server's own boot configuration,
/// so a caller cannot name a file at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRequest {
    /// Which workflow file, in any of ADR-0001 §2.2's four forms.
    pub workflow: String,
    /// The job id inside it — exactly what `act -j` receives.
    pub task: String,
    /// What the run is for (open vocabulary, shape-checked only).
    pub category: String,
    /// The commit to attribute the run to, 7..=40 hex.
    pub commit: String,
}

impl RunRequest {
    /// Bundle the four arguments.
    pub fn new(
        workflow: impl Into<String>,
        task: impl Into<String>,
        category: impl Into<String>,
        commit: impl Into<String>,
    ) -> Self {
        Self {
            workflow: workflow.into(),
            task: task.into(),
            category: category.into(),
            commit: commit.into(),
        }
    }
}

/// The isolation policy the operator chose (`--isolation`, ADR-0003 D-1).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum IsolationPolicy {
    /// Only `HEAD` may be run, in place.
    HeadOnly,
    /// `HEAD` in place, anything else in a synthesized worktree.
    #[default]
    Hybrid,
    /// Always synthesize a worktree, `HEAD` included.
    Worktree,
}

impl IsolationPolicy {
    /// The `--isolation` spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HeadOnly => "head-only",
            Self::Hybrid => "hybrid",
            Self::Worktree => "worktree",
        }
    }

    /// Whether this policy is willing to run a commit other than `HEAD`.
    pub fn allows_non_head(self) -> bool {
        !matches!(self, Self::HeadOnly)
    }
}

impl fmt::Display for IsolationPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for IsolationPolicy {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "head-only" => Ok(Self::HeadOnly),
            "hybrid" => Ok(Self::Hybrid),
            "worktree" => Ok(Self::Worktree),
            other => Err(format!(
                "`{other}` is not one of head-only, hybrid, worktree"
            )),
        }
    }
}

/// Which tree the run will measure, as decided by ADR-0003 §4.1. These are the two
/// `detail.mode` values `x-error-codes` documents for `isolation_failed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationMode {
    /// The workspace working directory itself, proved to be `commit`.
    HeadOnly,
    /// A detached worktree at `commit`, created after registration by mact-f1w.8.
    Worktree,
}

impl IsolationMode {
    /// The `detail.mode` spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HeadOnly => "head_only",
            Self::Worktree => "worktree",
        }
    }
}

impl fmt::Display for IsolationMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why an argument's shape is wrong — one `detail.errors[]` entry (ADR-0001 §5.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgumentError {
    /// Argument name, e.g. `commit`.
    pub path: String,
    /// What is wrong with it.
    pub reason: String,
    /// What it should have been.
    pub expected: String,
}

impl ArgumentError {
    fn new(path: &str, reason: impl Into<String>, expected: &str) -> Self {
        Self {
            path: path.to_owned(),
            reason: reason.into(),
            expected: expected.to_owned(),
        }
    }

    fn to_value(&self) -> Value {
        json!({ "path": self.path, "reason": self.reason, "expected": self.expected })
    }
}

/// `act_missing`'s `detail.reason` vocabulary (ADR-0001 §5.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActMissingReason {
    /// Not on `PATH` at all.
    NotFound,
    /// Present but not executable.
    NotExecutable,
    /// Present, but older than `--minimum-act-version`.
    TooOld,
}

impl ActMissingReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::NotExecutable => "not_executable",
            Self::TooOld => "too_old",
        }
    }
}

impl fmt::Display for ActMissingReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `commit_unknown`'s `detail.reason` vocabulary (ADR-0003 §3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitUnknownReason {
    /// Nothing — ref, object or abbreviation — matches it.
    NotFound,
    /// The object exists but is a blob or tree, not a commit.
    NotACommit,
}

impl CommitUnknownReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::NotACommit => "not_a_commit",
        }
    }
}

impl fmt::Display for CommitUnknownReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A rejected call, in exactly the vocabulary ADR-0001 §5.3 pins.
///
/// Every variant carries the fields its `detail` needs and nothing else; the
/// `Display` text is the `error.message` the caller sees, so no variant may format
/// an absolute host path (there is a test for that, and paths enter only as
/// workspace-relative display strings for that reason).
#[allow(
    missing_docs,
    reason = "the #[error] format string documents each variant"
)]
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ValidationError {
    #[error("argument validation failed: {summary}")]
    InvalidArguments {
        summary: String,
        errors: Vec<ArgumentError>,
    },

    #[error("workspace `{path}` is not usable ({reason})")]
    WorkspaceNotFound { path: String, reason: String },

    #[error("`{path}` is not a git work tree root ({reason})")]
    NotAGitRepository { path: String, reason: String },

    #[error("`{path}` is not a usable workflows directory ({reason})")]
    WorkflowsDirMissing { path: String, reason: String },

    #[error("act is not usable ({}): looked for {}", reason.as_str(), probed.join(", "))]
    ActMissing {
        probed: Vec<String>,
        reason: ActMissingReason,
        found_version: Option<String>,
        minimum_version: Option<String>,
    },

    #[error("no container runtime answered ({reason}): {detail}")]
    DockerUnreachable { reason: String, detail: String },

    #[error("no workflow named `{workflow}` in {workflows_dir}")]
    WorkflowNotFound {
        workflow: String,
        workflows_dir: &'static str,
        available: Vec<String>,
        reason: Option<String>,
    },

    #[error("`{workflow}` matches more than one workflow file; pass one of the file names")]
    AmbiguousWorkflow {
        workflow: String,
        candidates: Vec<String>,
    },

    #[error("`{workflow_file}` declares no job named `{task}`")]
    TaskNotFound {
        workflow_file: String,
        task: String,
        jobs: Vec<String>,
        reason: Option<String>,
    },

    #[error("commit `{commit}` does not resolve in this repository ({})", reason.as_str())]
    CommitUnknown {
        commit: String,
        reason: CommitUnknownReason,
    },

    #[error("commit `{commit}` is ambiguous; pass a longer prefix")]
    CommitAmbiguous {
        commit: String,
        candidates: Vec<String>,
    },

    #[error("this workspace cannot run commit {requested} ({reason})")]
    IsolationFailed {
        mode: IsolationMode,
        requested: String,
        head: String,
        reason: &'static str,
    },

    #[error("validation could not complete ({reason})")]
    Internal { reason: String },
}

impl ValidationError {
    /// The `error.code` this maps to — a member of `$defs/error_code`.
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidArguments { .. } => "invalid_arguments",
            Self::WorkspaceNotFound { .. } => "workspace_not_found",
            Self::NotAGitRepository { .. } => "not_a_git_repository",
            Self::WorkflowsDirMissing { .. } => "workflows_dir_missing",
            Self::ActMissing { .. } => "act_missing",
            Self::DockerUnreachable { .. } => "docker_unreachable",
            Self::WorkflowNotFound { .. } => "workflow_not_found",
            Self::AmbiguousWorkflow { .. } => "ambiguous_workflow",
            Self::TaskNotFound { .. } => "task_not_found",
            Self::CommitUnknown { .. } => "commit_unknown",
            Self::CommitAmbiguous { .. } => "commit_ambiguous",
            Self::IsolationFailed { .. } => "isolation_failed",
            Self::Internal { .. } => "internal_error",
        }
    }

    /// `error.retryable`, taken from ADR-0001 §5.3's table and checked against the
    /// schema by `error_codes_match_the_pinned_contract`.
    pub fn retryable(&self) -> bool {
        match self {
            // Preconditions the environment can fix between calls.
            Self::ActMissing { .. }
            | Self::DockerUnreachable { .. }
            | Self::IsolationFailed { .. }
            | Self::Internal { .. } => true,
            // Everything else is a caller mistake that needs a different call.
            _ => false,
        }
    }

    /// The `error.message`: this error's `Display`, truncated to the contract's
    /// byte bound at a character boundary.
    pub fn message(&self) -> String {
        truncate_bytes(self.to_string().as_str(), MAX_ERROR_MESSAGE_BYTES)
    }

    /// The `error.detail` object, with the keys ADR-0001 §5.3 documents for this
    /// code (plus, where useful, extra keys the schema's `error.detail` allows).
    pub fn detail(&self) -> Map<String, Value> {
        let mut detail: Value = match self {
            Self::InvalidArguments { errors, .. } => json!({
                "errors": errors
                    .iter()
                    .take(MAX_LISTED_OPTIONS)
                    .map(ArgumentError::to_value)
                    .collect::<Vec<_>>(),
            }),
            Self::WorkspaceNotFound { path, reason } => {
                json!({ "path": path, "reason": reason })
            }
            Self::NotAGitRepository { path, reason } => {
                json!({ "path": path, "reason": reason })
            }
            Self::WorkflowsDirMissing { path, reason } => {
                json!({ "path": path, "reason": reason })
            }
            Self::ActMissing {
                probed,
                reason,
                found_version,
                minimum_version,
            } => {
                let mut detail = json!({ "probed": probed, "reason": reason.as_str() });
                if let Some(object) = detail.as_object_mut() {
                    insert_if_some(object, "found_version", found_version.as_deref());
                    insert_if_some(object, "minimum_version", minimum_version.as_deref());
                }
                detail
            }
            Self::DockerUnreachable { reason, detail } => {
                json!({ "reason": reason, "detail": detail })
            }
            Self::WorkflowNotFound {
                workflow,
                workflows_dir,
                available,
                reason,
            } => {
                let mut detail = json!({
                    "workflow": workflow,
                    "workflows_dir": workflows_dir,
                    "available": Self::option_list(available.clone()),
                });
                if let Some(object) = detail.as_object_mut() {
                    insert_if_some(object, "reason", reason.as_deref());
                }
                detail
            }
            Self::AmbiguousWorkflow {
                workflow,
                candidates,
            } => json!({
                "workflow": workflow,
                "candidates": Self::option_list(candidates.clone()),
            }),
            Self::TaskNotFound {
                workflow_file,
                task,
                jobs,
                reason,
            } => {
                let mut detail = json!({
                    "workflow_file": workflow_file,
                    "task": task,
                    // Job ids keep file order (the caller is scanning for their typo),
                    // so this list is truncated rather than sorted.
                    "jobs": jobs.iter().take(MAX_LISTED_OPTIONS).collect::<Vec<_>>(),
                });
                if let Some(object) = detail.as_object_mut() {
                    insert_if_some(object, "reason", reason.as_deref());
                }
                detail
            }
            Self::CommitUnknown { commit, reason } => {
                json!({ "commit": commit, "reason": reason.as_str() })
            }
            Self::CommitAmbiguous { commit, candidates } => json!({
                "commit": commit,
                "candidates": Self::option_list(candidates.clone()),
            }),
            Self::IsolationFailed {
                mode,
                requested,
                head,
                reason,
            } => json!({
                "mode": mode.as_str(),
                "requested": requested,
                "head": head,
                "reason": reason,
            }),
            // ADR-0001 documents `detail.panic` for internal_error; a real panic is
            // the wiring ticket's to report. Everywhere else this code appears it is
            // an environment fault, and `reason` is the honest key.
            Self::Internal { reason } => json!({ "reason": reason }),
        };
        let detail = detail.as_object_mut().expect("every detail is an object");
        shrink_detail(detail);
        detail.clone()
    }

    /// The complete `$defs/error` payload: `{code, message, retryable, detail}`.
    ///
    /// The tool layer (mact-f1w.14) puts this in `structuredContent.error` and
    /// prefixes the text block with `<tool>: <code>:` per ADR-0001 §5.2.
    pub fn to_error_value(&self) -> Value {
        json!({
            "code": self.code(),
            "message": self.message(),
            "retryable": self.retryable(),
            "detail": Value::Object(self.detail()),
        })
    }

    /// A resolution error's option list, capped (ADR-0001 §5.2's one-round-trip
    /// guarantee, bounded).
    pub(crate) fn option_list(values: Vec<String>) -> Vec<String> {
        let mut values = values;
        values.sort();
        values.dedup();
        values.truncate(MAX_LISTED_OPTIONS);
        values
    }
}

fn insert_if_some(detail: &mut Map<String, Value>, key: &str, value: Option<&str>) {
    if let Some(value) = value {
        detail.insert(key.to_owned(), json!(value));
    }
}

/// Truncate to at most `max` bytes without splitting a UTF-8 sequence.
fn truncate_bytes(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// Force `error.detail` under [`MAX_ERROR_DETAIL_BYTES`] by dropping whole
/// elements from its longest list, which is the truncation rule ADR-0001 §5.2
/// states ("lists are truncated rather than truncated mid-element").
fn shrink_detail(detail: &mut Map<String, Value>) {
    for _ in 0..MAX_LISTED_OPTIONS * 2 {
        let fits = serde_json::to_string(detail)
            .map(|text| text.len() <= MAX_ERROR_DETAIL_BYTES)
            .unwrap_or(false);
        if fits {
            return;
        }
        let longest = detail
            .iter()
            .filter(|(_, value)| value.is_array())
            .max_by_key(|(_, value)| value.as_array().map_or(0, Vec::len))
            .map(|(key, value)| (key.clone(), value.as_array().map_or(0, Vec::len)));
        let Some((key, _)) = longest.filter(|(_, length)| *length > 0) else {
            return;
        };
        if let Some(list) = detail.get_mut(&key).and_then(Value::as_array_mut) {
            list.pop();
        }
    }
}

/// Workspace-relative, forward-slash display form of `path` — what an error may
/// carry instead of an absolute host path (ADR-0001 §5.2, §3.4).
///
/// Anything outside the workspace is reduced to its file name, and the workspace
/// itself to *its* file name: the caller already knows which workspace the server
/// was pointed at, and a host path in a result is a leak and a cross-machine diff
/// failure in one stroke.
pub(crate) fn display_path(path: &Path, workspace_root: &Path) -> String {
    let strip = |candidate: &Path| {
        candidate
            .strip_prefix(workspace_root)
            .unwrap_or(candidate)
            .to_string_lossy()
            .replace('\\', "/")
    };
    let relative = strip(path);
    if !relative.is_empty() {
        return relative;
    }
    path.file_name()
        .map(|name| name.to_string_lossy().replace('\\', "/"))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| path.to_string_lossy().replace('\\', "/"))
}
