//! The validator itself: [`ValidationPolicy`] (what the operator decided),
//! [`Validator`] (the seams plus a probe cache) and [`ValidatedRunTarget`] (the
//! answer).

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use crate::{
    config::Config,
    workflows::{self, IndexError, ResolveError, WorkflowFile},
    workspace::{
        display_path, parse_act_version, ActMissingReason, ArgumentError, Clock, CommandError,
        CommandOutput, CommitUnknownReason, ContainerRuntime, Environment, IsolationMode,
        IsolationPolicy, ProgramError, RunRequest, SemVer, SystemClock, ValidationError,
        COMMIT_MAX_LEN, COMMIT_MIN_LEN, DEFAULT_CONTAINER_PROBE_TIMEOUT, WARNING_DIRTY_WORKTREE,
        WARNING_ISOLATION_WORKTREE, WORKFLOWS_DIR,
    },
};

/// `--git-timeout-secs` default (ADR-0003 §10).
pub const DEFAULT_GIT_TIMEOUT: Duration = Duration::from_secs(30);

/// Upper bound for `--git-timeout-secs` (ADR-0003 §10: `1..=300`).
pub const MAX_GIT_TIMEOUT: Duration = Duration::from_secs(300);

/// How long a successful or failed environment probe is remembered (ADR-0001
/// §5.3: "probe cached 30 s").
pub const DEFAULT_PROBE_TTL: Duration = Duration::from_secs(30);

/// The server-side settings validation reads — every one of them operator-owned,
/// never caller-owned (ADR-0001 §2 deliberately has no `workspace` argument).
///
/// Build it from [`Config`] with [`ValidationPolicy::from_config`]; the fields are
/// public so a test (or a future wiring ticket) can construct a policy directly
/// without going through the CLI.
#[derive(Debug, Clone)]
pub struct ValidationPolicy {
    /// Canonical workspace root, i.e. [`Config::workspace`]. It must already be
    /// symlink-free: [`Validator::validate`] re-canonicalizes it and refuses a root
    /// that resolves to somewhere else, which is what stops a workspace path that
    /// is a symlink pointing outside the accepted root.
    pub workspace_root: PathBuf,
    /// Evidence root to hand to the runner ([`Config::evidence_dir`]).
    pub evidence_dir: PathBuf,
    /// The act program to look for ([`Config::act_binary`]) — a bare `PATH` name or
    /// a path.
    pub act_binary: PathBuf,
    /// Act versions below this are `act_missing` (`--minimum-act-version`).
    pub minimum_act_version: SemVer,
    /// `--isolation`.
    pub isolation: IsolationPolicy,
    /// `--allow-dirty-worktree`: downgrades gate G5 from an error to the
    /// `dirty_worktree` warning.
    pub allow_dirty_worktree: bool,
    /// `--git-timeout-secs`.
    pub git_timeout: Duration,
    /// `--container-daemon-socket`, when the operator pinned one: the socket the
    /// reachability probe dials instead of the runtime's default.
    pub container_daemon_socket: Option<PathBuf>,
    /// How long the container-runtime probe may take.
    pub container_probe_timeout: Duration,
    /// Probe cache lifetime ([`DEFAULT_PROBE_TTL`]).
    pub probe_ttl: Duration,
}

impl ValidationPolicy {
    /// Derive the policy from the boot configuration, so the CLI surface is the
    /// only place defaults are spelled out.
    pub fn from_config(config: &Config) -> Self {
        Self {
            workspace_root: config.workspace.clone(),
            evidence_dir: config.evidence_dir.clone(),
            act_binary: config.act_binary.clone(),
            minimum_act_version: config.minimum_act_version,
            isolation: config.isolation,
            allow_dirty_worktree: config.allow_dirty_worktree,
            git_timeout: config.git_timeout,
            container_daemon_socket: config.container_daemon_socket.clone(),
            container_probe_timeout: DEFAULT_CONTAINER_PROBE_TIMEOUT,
            probe_ttl: DEFAULT_PROBE_TTL,
        }
    }
}

/// What a call was honoured against — the only thing the runner is allowed to
/// trust, because every field is either canonicalized or resolved by git.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedRunTarget {
    /// The workflow file that was resolved and indexed.
    ///
    /// In [`IsolationMode::HeadOnly`] this is the file on disk in the workspace,
    /// canonical. In [`IsolationMode::Worktree`] the tree does not exist yet (it is
    /// created after registration by mact-f1w.8), so this is the same relpath
    /// anchored at the workspace root; a runner with a synthesized tree must use
    /// [`ValidatedRunTarget::workflow_in`] against that tree's root instead.
    pub workflow_file: PathBuf,
    /// Display name: the file basename without extension — the value ADR-0002
    /// stamps in the evidence envelope and ADR-0001 §2.2 puts in `run_view.workflow`.
    pub workflow_display: String,
    /// Workspace-relative path with forward slashes, e.g. `.github/workflows/ci.yml`.
    pub workflow_relpath: String,
    /// The job id `act -j` will receive.
    pub job_id: String,
    /// Full lowercase 40-hex SHA — never the caller's abbreviation.
    pub commit_full_sha: String,
    /// Echoed back unchanged (ADR-0001 §3.1).
    pub category: String,
    /// Canonical workspace root.
    pub workspace_root: PathBuf,
    /// Where evidence for this run lands.
    pub evidence_dir: PathBuf,
    /// Which tree the run measures (ADR-0003 §4.1).
    pub mode: IsolationMode,
    /// Warning codes to carry into the result (`isolation_worktree`,
    /// `dirty_worktree`); `[]` normally.
    pub warnings: Vec<String>,
}

impl ValidatedRunTarget {
    /// The workflow file re-anchored at the root of the tree the run actually uses
    /// — the workspace itself, or a synthesized worktree.
    pub fn workflow_in(&self, tree_root: &Path) -> PathBuf {
        tree_root.join(&self.workflow_relpath)
    }

    /// Whether `code` is already in [`Self::warnings`].
    pub fn has_warning(&self, code: &str) -> bool {
        self.warnings.iter().any(|warning| warning == code)
    }
}

/// Where validation reads workflow files from, as chosen by a [`TreeSelector`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeRoot {
    /// The workspace working directory (mode `head_only`).
    Workspace,
    /// The resolved commit's tree, read read-only through git (`ls-tree`, `show`).
    /// This is what `hybrid`/`worktree` mode inspects before the worktree exists.
    CommitTree,
    /// An already-materialized tree — e.g. the scratch worktree mact-f1w.8 creates.
    /// Validation reads its `.github/workflows` from disk.
    Materialized {
        /// Root of that tree, containing `.github/workflows`.
        root: PathBuf,
    },
}

/// What a [`TreeSelector`] decided for one call: the ADR-0003 mode, and the tree to
/// inspect while confirming the workflow and job id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreePlan {
    /// `head_only` or `worktree` — the `detail.mode` the isolation gate reports.
    pub mode: IsolationMode,
    /// Where to read the workflow files from.
    pub root: TreeRoot,
}

/// The seam ADR-0003 §4 leaves open, and the one thing the wiring ticket
/// (mact-f1w.14) must replace once it owns scratch trees.
///
/// [`DefaultTreeSelector`] is the ADR's own mode selection: `HEAD` in place, and any
/// other commit read out of its own tree, because the scratch worktree does not exist
/// at validation time (it is created after registration, phase `preparing_tree`).
/// A wiring ticket that *has* a tree — a pre-created worktree, or a cache of them —
/// returns [`TreeRoot::Materialized`] and validation then reads that directory.
///
/// A selector returning [`IsolationMode::Worktree`] should keep the tree's relpaths
/// repository-shaped (`.github/workflows/<file>.yml`), because that relpath is what
/// the run reports and what [`ValidatedRunTarget::workflow_in`] re-anchors.
pub trait TreeSelector: Send + Sync {
    /// Decide the mode and the tree for `commit`, given the operator policy and the
    /// workspace's `HEAD` (`None` when `HEAD` does not resolve).
    ///
    /// Deciding is free of judgement about *refusing*: a `head-only` policy that
    /// cannot run a non-`HEAD` commit still returns a plan, and the isolation gate
    /// produces the `isolation_failed reason: "policy_head_only"` at the step ADR-0001
    /// §2.9 pins it at — after resolution, so a typo'd workflow is reported first.
    fn select(&self, policy: IsolationPolicy, head: Option<&str>, commit: &str) -> TreePlan;
}

/// ADR-0003 §4.1 verbatim: `head-only` runs in place, `hybrid` runs `HEAD` in place
/// and everything else out of its own tree, `worktree` always uses a tree.
#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultTreeSelector;

impl TreeSelector for DefaultTreeSelector {
    fn select(&self, policy: IsolationPolicy, head: Option<&str>, commit: &str) -> TreePlan {
        match policy {
            // `head-only` refuses a non-HEAD commit, but that verdict is gate W1 and
            // runs at step 8: ADR-0001 §2.9 puts resolution (step 6) before the
            // isolation gate, so "no such workflow" must beat "policy refuses".
            IsolationPolicy::HeadOnly => TreePlan {
                mode: IsolationMode::HeadOnly,
                root: TreeRoot::Workspace,
            },
            IsolationPolicy::Hybrid => {
                if head == Some(commit) {
                    TreePlan {
                        mode: IsolationMode::HeadOnly,
                        root: TreeRoot::Workspace,
                    }
                } else {
                    TreePlan {
                        mode: IsolationMode::Worktree,
                        root: TreeRoot::CommitTree,
                    }
                }
            }
            IsolationPolicy::Worktree => TreePlan {
                mode: IsolationMode::Worktree,
                root: TreeRoot::CommitTree,
            },
        }
    }
}

/// Remembers the last environment probe so a burst of calls does not re-run
/// `which` and `docker info` (ADR-0001 §5.3).
#[derive(Debug)]
struct CachedProbe {
    at_millis: u64,
    outcome: Result<EnvironmentFacts, ValidationError>,
}

/// The facts a successful probe established.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EnvironmentFacts {
    act: PathBuf,
    act_version: Option<SemVer>,
    runtime: ContainerRuntime,
}

/// The pre-start gate: ADR-0001 §2.9 steps 2 and 4–8, in that order.
///
/// Cheap checks run before expensive ones, and nothing after the last check has
/// started anything, which is what makes every error here `isError: true` in
/// ADR-0001's sense ("no run was created").
pub struct Validator {
    policy: ValidationPolicy,
    environment: Arc<dyn Environment>,
    clock: Arc<dyn Clock>,
    tree_selector: Arc<dyn TreeSelector>,
    probe: Mutex<Option<CachedProbe>>,
}

impl std::fmt::Debug for Validator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Validator")
            .field("policy", &self.policy)
            .field(
                "probe_cached",
                &self.probe.lock().is_ok_and(|guard| guard.is_some()),
            )
            .finish_non_exhaustive()
    }
}

impl Validator {
    /// A validator with the system environment and the system clock.
    pub fn new(policy: ValidationPolicy, environment: Arc<dyn Environment>) -> Self {
        Self {
            policy,
            environment,
            clock: Arc::new(SystemClock),
            tree_selector: Arc::new(DefaultTreeSelector),
            probe: Mutex::new(None),
        }
    }

    /// Override the clock, so the probe cache's TTL is testable.
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Override how the tree is chosen — the seam ADR-0003 leaves for the wiring
    /// ticket, which is the one that owns scratch worktrees.
    pub fn with_tree_selector(mut self, selector: Arc<dyn TreeSelector>) -> Self {
        self.tree_selector = selector;
        self
    }

    /// The policy this validator applies.
    pub fn policy(&self) -> &ValidationPolicy {
        &self.policy
    }

    /// Forget the cached probe, e.g. after a failed `act` spawn proved the probe
    /// stale.
    pub fn invalidate_probes(&self) {
        if let Ok(mut cache) = self.probe.lock() {
            *cache = None;
        }
    }

    /// Validate one call, synchronously.
    ///
    /// This runs subprocesses (`git`, `docker`) and blocks for up to their
    /// timeouts, so an async caller wants [`Validator::validate_async`].
    pub fn validate(&self, request: &RunRequest) -> Result<ValidatedRunTarget, ValidationError> {
        // Step 1 — argument shape. No filesystem, no subprocess, no allocation of
        // anything: the cheapest possible rejection of an injected argument.
        check_argument_shape(request)?;

        // Step 2 — the workspace gates (ADR-0001 §2.9 step 4).
        let root = self.resolve_workspace()?;
        self.check_workflows_dir(&root)?;

        // Step 3 — commit resolution (§2.9 step 5, ADR-0003 §3). Never a fetch.
        let commit = self.resolve_commit(&root, &request.commit)?;

        // Step 4 — which tree would this run measure (ADR-0003 §4.1)? `HEAD` is
        // resolved here because the *mode* decides where workflow files are read
        // from; the rest of the gate stays at step 7, where ADR-0003 §4.3 pins it.
        let head = self.head_commit(&root);
        let plan = self
            .tree_selector
            .select(self.policy.isolation, head.as_deref(), &commit);
        let mode = plan.mode;

        // Step 5 — workflow and task resolution (§2.9 step 6).
        let entries = self.list_workflows(&root, &commit, &plan)?;
        if entries.is_empty() {
            return Err(ValidationError::WorkflowsDirMissing {
                path: WORKFLOWS_DIR.to_owned(),
                reason: "no `.yml` or `.yaml` file was found in the tree this run would use"
                    .to_owned(),
            });
        }
        let workflow = resolve_workflow(&entries, &request.workflow)?;
        let job_id = confirm_task(workflow, &request.task)?;

        // Step 6 — environment probes (§2.9 step 7), cached.
        self.probe_environment()?;

        // Step 7 — the isolation gate (§2.9 step 8, ADR-0003 §4). Nothing has been
        // created yet, so every failure here still means "nothing was started".
        let warnings = self.isolation_gate(&root, mode, head.as_deref(), &commit)?;

        Ok(ValidatedRunTarget {
            workflow_file: workflow_path(workflow, &root, &plan),
            workflow_display: workflow.stem.clone(),
            workflow_relpath: workflow.relpath.clone(),
            job_id,
            commit_full_sha: commit,
            category: request.category.clone(),
            workspace_root: root,
            evidence_dir: self.policy.evidence_dir.clone(),
            mode,
            warnings,
        })
    }

    /// [`Validator::validate`] on the blocking pool, for the async tool handler.
    ///
    /// `git status` on a cold index of a large repository is seconds of
    /// uninterruptible work (ADR-0003 §4.3), and that must not occupy an I/O worker.
    pub async fn validate_async(
        self: Arc<Self>,
        request: RunRequest,
    ) -> Result<ValidatedRunTarget, ValidationError> {
        tokio::task::spawn_blocking(move || self.validate(&request))
            .await
            .unwrap_or_else(|join_error| {
                Err(ValidationError::Internal {
                    reason: format!("validation task did not complete: {join_error}"),
                })
            })
    }

    // -------------------------------------------------------------- step 2: workspace

    fn resolve_workspace(&self) -> Result<PathBuf, ValidationError> {
        let requested = self.policy.workspace_root.as_path();
        let display = display_path(requested, requested);

        if !requested.exists() {
            return Err(ValidationError::WorkspaceNotFound {
                path: display,
                reason: "the directory does not exist".to_owned(),
            });
        }
        if !requested.is_dir() {
            return Err(ValidationError::WorkspaceNotFound {
                path: display.clone(),
                reason: "the path is not a directory".to_owned(),
            });
        }
        let root =
            requested
                .canonicalize()
                .map_err(|error| ValidationError::WorkspaceNotFound {
                    path: display.clone(),
                    reason: format!("it could not be resolved: {error}"),
                })?;
        // The accepted root is the canonical path the server was booted with. If
        // resolving it again lands somewhere else, the path we were handed is a
        // symlink out of that root (or the tree moved under us): not our repository.
        if root != requested {
            return Err(ValidationError::WorkspaceNotFound {
                path: display,
                reason: "it resolves through a symlink outside the accepted workspace root"
                    .to_owned(),
            });
        }

        // Gate G1 (ADR-0003 §4.2): inside a git work tree, and the work tree's root
        // is the workspace — act's `-C` and the evidence keying both assume it.
        let toplevel = match self.git(&root, &["rev-parse", "--show-toplevel"]) {
            Err(error) => {
                return Err(ValidationError::NotAGitRepository {
                    path: display,
                    reason: format!("git could not run: {error}"),
                });
            }
            Ok(output) if !output.success() => {
                return Err(ValidationError::NotAGitRepository {
                    path: display,
                    reason:
                        "`git rev-parse --show-toplevel` failed; nothing to attribute a commit to"
                            .to_owned(),
                });
            }
            Ok(output) => PathBuf::from(output.first_line()),
        };
        let toplevel = std::fs::canonicalize(&toplevel).unwrap_or(toplevel);
        if toplevel != root {
            return Err(ValidationError::NotAGitRepository {
                path: display,
                reason: "the git work tree root is above this directory; point --workspace at the repository root".to_owned(),
            });
        }
        Ok(root)
    }

    /// `.github/workflows` must exist, be a directory, stay inside the workspace,
    /// and hold at least one `*.yml`/`*.yaml` (ADR-0001 §5.3's `workflows_dir_missing`).
    fn check_workflows_dir(&self, root: &Path) -> Result<(), ValidationError> {
        let dir = root.join(WORKFLOWS_DIR);
        if !dir.exists() {
            return Err(ValidationError::WorkflowsDirMissing {
                path: WORKFLOWS_DIR.to_owned(),
                reason: "the directory does not exist; this is not a GitHub Actions repository"
                    .to_owned(),
            });
        }
        let canonical = match dir.canonicalize() {
            Ok(canonical) if canonical.is_dir() => canonical,
            Ok(_) => {
                return Err(ValidationError::WorkflowsDirMissing {
                    path: WORKFLOWS_DIR.to_owned(),
                    reason: "`.github/workflows` is not a directory".to_owned(),
                });
            }
            Err(error) => {
                return Err(ValidationError::WorkflowsDirMissing {
                    path: WORKFLOWS_DIR.to_owned(),
                    reason: format!("it could not be resolved: {error}"),
                });
            }
        };
        if !canonical.starts_with(root) {
            return Err(ValidationError::WorkflowsDirMissing {
                path: WORKFLOWS_DIR.to_owned(),
                reason: "`.github/workflows` resolves outside the workspace".to_owned(),
            });
        }
        if !workflows::has_workflow_files(&canonical) {
            return Err(ValidationError::WorkflowsDirMissing {
                path: WORKFLOWS_DIR.to_owned(),
                reason:
                    "no `.yml` or `.yaml` file is present; this is not a GitHub Actions repository"
                        .to_owned(),
            });
        }
        Ok(())
    }

    // --------------------------------------------------------------- step 3: commit

    fn resolve_commit(&self, root: &Path, raw: &str) -> Result<String, ValidationError> {
        // R1/R2: shape-checked by `check_argument_shape`, then lowercased. The
        // original is kept for `detail.commit` so the caller sees what they typed.
        let normalized = raw.to_ascii_lowercase();

        // R3. `--end-of-options` after `--verify --quiet` so a shape-legal string can
        // never be read as a flag, and the whole revision is one argv element.
        let spec = format!("{normalized}^{{commit}}");
        let resolved = self
            .git(
                root,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    "--end-of-options",
                    &spec,
                ],
            )
            .map_err(|error| self.internal("commit resolution", &error))?;
        if resolved.success() {
            let full = resolved.first_line();
            if is_full_sha(&full) {
                return Ok(full);
            }
            return Err(ValidationError::Internal {
                reason: format!(
                    "`git rev-parse --verify` printed `{full}`, which is not a 40-hex id"
                ),
            });
        }

        // R4: an object by that name may still exist and not be a commit.
        let kind = self
            .git(root, &["cat-file", "-t", &normalized])
            .map_err(|error| self.internal("commit classification", &error))?;
        if kind.success() {
            return Err(ValidationError::CommitUnknown {
                commit: raw.to_owned(),
                reason: CommitUnknownReason::NotACommit,
            });
        }

        // R5: ambiguity, bounded by refs (ADR-0003 §3.4 — no O(all objects) scan).
        let refs = self
            .git(
                root,
                &[
                    "for-each-ref",
                    "--format=%(objectname) %(*objectname) %(refname)",
                ],
            )
            .map_err(|error| self.internal("commit ambiguity scan", &error))?;
        let mut candidates: BTreeSet<String> = BTreeSet::new();
        for line in refs.stdout_lossy().lines() {
            for field in line.split_whitespace().take(2) {
                let id = field.to_ascii_lowercase();
                if id.len() >= normalized.len() && id.starts_with(&normalized) {
                    candidates.insert(id);
                }
            }
        }
        if candidates.len() >= 2 {
            return Err(ValidationError::CommitAmbiguous {
                commit: raw.to_owned(),
                candidates: ValidationError::option_list(candidates.into_iter().collect()),
            });
        }
        Err(ValidationError::CommitUnknown {
            commit: raw.to_owned(),
            reason: CommitUnknownReason::NotFound,
        })
    }

    /// Gate G2: `HEAD`'s full id, or `None` when there is no `HEAD` (empty
    /// repository). Which failure that is belongs to the gate, not here.
    fn head_commit(&self, root: &Path) -> Option<String> {
        let output = self
            .git(
                root,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    "--end-of-options",
                    "HEAD",
                ],
            )
            .ok()?;
        let head = output.first_line();
        output
            .success()
            .then(|| is_full_sha(&head).then_some(head))
            .flatten()
    }

    // ------------------------------------------------------ step 5: workflow + task

    fn list_workflows(
        &self,
        root: &Path,
        commit: &str,
        plan: &TreePlan,
    ) -> Result<Vec<WorkflowFile>, ValidationError> {
        match &plan.root {
            TreeRoot::Workspace => self.list_workdir_workflows(root),
            TreeRoot::Materialized { root: tree } => self.list_workdir_workflows(tree),
            TreeRoot::CommitTree => self.list_commit_workflows(root, commit),
        }
    }

    /// Index every workflow file under `tree`'s `.github/workflows`. Relpaths are
    /// tree-relative, which for a materialized worktree is the same string the
    /// workspace would report — that relpath is what the run carries.
    ///
    /// The walk itself is [`workflows::index_directory`], shared with `list_tasks` so
    /// the two tools cannot disagree about which files exist.
    fn list_workdir_workflows(&self, tree: &Path) -> Result<Vec<WorkflowFile>, ValidationError> {
        workflows::index_directory(tree).map_err(|error| match error {
            IndexError::Unresolvable => ValidationError::WorkflowsDirMissing {
                path: WORKFLOWS_DIR.to_owned(),
                reason: "`.github/workflows` could not be resolved".to_owned(),
            },
            IndexError::ReadDir { reason } => ValidationError::Internal {
                reason: format!("could not read `{WORKFLOWS_DIR}`: {reason}"),
            },
            IndexError::ReadFile { file, reason } => ValidationError::Internal {
                reason: format!("could not read `{file}`: {reason}"),
            },
        })
    }

    /// Read the workflow files of a commit's tree, without creating anything:
    /// `git ls-tree` for the list, `git show` per file. One subprocess per workflow
    /// file, which is a handful, on a path that is already running `git`.
    fn list_commit_workflows(
        &self,
        root: &Path,
        sha: &str,
    ) -> Result<Vec<WorkflowFile>, ValidationError> {
        let listing = self
            .git(
                root,
                &["ls-tree", "-r", "--name-only", "-z", sha, WORKFLOWS_DIR],
            )
            .map_err(|error| self.internal("workflow listing", &error))?;
        if !listing.success() {
            return Err(ValidationError::Internal {
                reason: format!("`git ls-tree` failed: {}", listing.first_error_line()),
            });
        }
        let mut entries = Vec::new();
        for path in listing
            .stdout_lossy()
            .split('\0')
            .filter(|path| !path.is_empty())
        {
            if !path.ends_with(".yml") && !path.ends_with(".yaml") {
                continue;
            }
            // Belt and braces: the paths come from git, but a tree entry named `../`
            // is exactly the sort of thing not to hand to a path joiner.
            if !path.starts_with(&format!("{WORKFLOWS_DIR}/")) || path.contains("..") {
                return Err(ValidationError::Internal {
                    reason: format!("`git ls-tree` reported `{path}`, outside `.github/workflows`"),
                });
            }
            let spec = format!("{sha}:{path}");
            let content = self
                .git(root, &["show", &spec])
                .map_err(|error| self.internal("workflow file read", &error))?;
            if !content.success() {
                return Err(ValidationError::Internal {
                    reason: format!(
                        "`git show` failed for `{path}`: {}",
                        content.first_error_line()
                    ),
                });
            }
            entries.push(WorkflowFile::new(
                path.to_owned(),
                root.join(path),
                &content.stdout,
            ));
        }
        entries.sort_by(|left, right| left.relpath.cmp(&right.relpath));
        Ok(entries)
    }

    // ----------------------------------------------------- step 6: environment probes

    fn probe_environment(&self) -> Result<EnvironmentFacts, ValidationError> {
        let now = self.clock.now_millis();
        if let Ok(guard) = self.probe.lock() {
            if let Some(cached) = guard.as_ref() {
                let age = now.saturating_sub(cached.at_millis);
                if age < ttl_millis(self.policy.probe_ttl) {
                    return clone_outcome(&cached.outcome);
                }
            }
        }
        let outcome = self.probe_uncached();
        if let Ok(mut guard) = self.probe.lock() {
            *guard = Some(CachedProbe {
                at_millis: now,
                outcome: outcome.clone(),
            });
        }
        outcome
    }

    fn probe_uncached(&self) -> Result<EnvironmentFacts, ValidationError> {
        let requested = self.policy.act_binary.clone();
        let probed = vec![requested.display().to_string()];
        let act = self
            .environment
            .resolve_program(&requested)
            .map_err(|error| {
                let reason = match error {
                    ProgramError::NotExecutable { .. } => ActMissingReason::NotExecutable,
                    ProgramError::NotFound { .. } => ActMissingReason::NotFound,
                };
                ValidationError::ActMissing {
                    probed: probed.clone(),
                    reason,
                    found_version: None,
                    minimum_version: Some(self.policy.minimum_act_version.to_string()),
                }
            })?;
        let version = self
            .environment
            .program_version(&act)
            .as_deref()
            .and_then(parse_act_version);
        if let Some(found) = version {
            if found < self.policy.minimum_act_version {
                return Err(ValidationError::ActMissing {
                    probed,
                    reason: ActMissingReason::TooOld,
                    found_version: Some(found.to_string()),
                    minimum_version: Some(self.policy.minimum_act_version.to_string()),
                });
            }
        }
        let runtime = self
            .environment
            .container_runtime(
                self.policy.container_daemon_socket.as_deref(),
                self.policy.container_probe_timeout,
            )
            .map_err(|error| ValidationError::DockerUnreachable {
                reason: error.reason().to_owned(),
                detail: error.detail(),
            })?;
        Ok(EnvironmentFacts {
            act,
            act_version: version,
            runtime,
        })
    }

    // ---------------------------------------------------------- step 7: isolation gate

    /// ADR-0003 §4.2 (mode `head_only`) and §4.4 (mode `worktree`), in the pinned
    /// order, returning the warning codes the run carries.
    fn isolation_gate(
        &self,
        root: &Path,
        mode: IsolationMode,
        head: Option<&str>,
        commit: &str,
    ) -> Result<Vec<String>, ValidationError> {
        let mut warnings = Vec::new();
        let fail = |reason: &'static str, head: &str| -> ValidationError {
            ValidationError::IsolationFailed {
                mode,
                requested: commit.to_owned(),
                head: head.to_owned(),
                reason,
            }
        };
        let gate_git = |args: &[&str], head: &str| -> Result<CommandOutput, ValidationError> {
            self.git(root, args).map_err(|error| {
                let reason = if error.retryable() {
                    "git_timeout"
                } else {
                    "git_unavailable"
                };
                fail(reason, head)
            })
        };

        if mode == IsolationMode::Worktree {
            // W1 (policy) cannot fire here: a `head-only` policy always selects
            // `head_only` mode (ADR-0003 §4.1), and W3 (scratch writability) belongs
            // to the ticket that creates the scratch (mact-f1w.8).
            if !self.policy.isolation.allows_non_head() {
                return Err(fail("policy_head_only", head.unwrap_or_default()));
            }
            self.check_no_submodules(root, commit, mode, head.unwrap_or_default())?;
            warnings.push(WARNING_ISOLATION_WORKTREE.to_owned());
            return Ok(warnings);
        }

        // G2 — HEAD must resolve, or "run in place at this commit" is unprovable.
        let Some(head_sha) = head else {
            return Err(fail("head_unresolvable", ""));
        };
        // G3 — a bare repository has no tree to point act at.
        let bare = gate_git(&["rev-parse", "--is-bare-repository"], head_sha)?;
        if bare.success() && bare.first_line() == "true" {
            return Err(fail("bare_repository", head_sha));
        }
        // G4 — the commit must be the HEAD we just proved.
        if head_sha != commit {
            if !self.policy.isolation.allows_non_head() {
                return Err(fail("policy_head_only", head_sha));
            }
            return Err(fail("head_mismatch", head_sha));
        }
        // G5 — no tracked modification, or the `commit` field would describe a tree
        // that is not the one being measured.
        let status = gate_git(&["status", "--porcelain", "--untracked-files=no"], head_sha)?;
        if !status.stdout_lossy().trim().is_empty() {
            if self.policy.allow_dirty_worktree {
                warnings.push(WARNING_DIRTY_WORKTREE.to_owned());
            } else {
                return Err(fail("dirty_worktree", head_sha));
            }
        }
        // G6 — a sparse checkout is a partial materialisation of the commit.
        let sparse = gate_git(&["config", "--bool", "core.sparseCheckout"], head_sha)?;
        if sparse.success() && sparse.first_line() == "true" {
            return Err(fail("sparse_checkout", head_sha));
        }
        // G7 — submodules, checked in the target tree (ADR-0003 §4.2 G7).
        self.check_no_submodules(root, commit, mode, head_sha)?;
        Ok(warnings)
    }

    /// G7 / W2: `.gitmodules` in the *target commit's* tree.
    fn check_no_submodules(
        &self,
        root: &Path,
        commit: &str,
        mode: IsolationMode,
        head: &str,
    ) -> Result<(), ValidationError> {
        let output = self.git(root, &["cat-file", "-e", &format!("{commit}:.gitmodules")]);
        match output {
            Err(error) => {
                let reason = if error.retryable() {
                    "git_timeout"
                } else {
                    "git_unavailable"
                };
                Err(ValidationError::IsolationFailed {
                    mode,
                    requested: commit.to_owned(),
                    head: head.to_owned(),
                    reason,
                })
            }
            // rc 0 means the path exists in that tree.
            Ok(output) if output.success() => Err(ValidationError::IsolationFailed {
                mode,
                requested: commit.to_owned(),
                head: head.to_owned(),
                reason: "submodules_unsupported",
            }),
            Ok(_) => Ok(()),
        }
    }

    // -------------------------------------------------------------------- plumbing

    fn git(&self, root: &Path, args: &[&str]) -> Result<CommandOutput, CommandError> {
        self.environment.git(root, args, self.policy.git_timeout)
    }

    fn internal(&self, context: &str, error: &CommandError) -> ValidationError {
        // ADR-0003 §10 pins `git_timeout` only for step-8 failures (mapped in
        // `isolation_gate`); anywhere else a git failure is an environment fault the
        // caller may retry, and `internal_error` is the only honest code for it.
        ValidationError::Internal {
            reason: format!("{context} failed: {error}"),
        }
    }
}

// ------------------------------------------------------------------ pure helpers

/// Step 1: every shape rule of ADR-0001 §2 that can be checked without touching a
/// file, collected into one `invalid_arguments` so the caller fixes all of it in
/// one round trip.
fn check_argument_shape(request: &RunRequest) -> Result<(), ValidationError> {
    let mut errors = Vec::new();
    if !commit_shape(&request.commit) {
        errors.push(ArgumentError::new(
            "commit",
            format!(
                "`{}` is not a plain hexadecimal abbreviation",
                request.commit
            ),
            &format!("{COMMIT_MIN_LEN} to {COMMIT_MAX_LEN} hex characters: no whitespace, no `..`, no `~`, no leading `-`"),
        ));
    }
    if !workflow_shape(&request.workflow) {
        errors.push(ArgumentError::new(
            "workflow",
            format!("`{}` is not a workflow name or path", request.workflow),
            "a name (`coverage`, `coverage.yml`) or a path under `.github/workflows/`, 1..=255 bytes, never `..`",
        ));
    }
    if !task_shape(&request.task) {
        errors.push(ArgumentError::new(
            "task",
            format!("`{}` is not a job id", request.task),
            "a job id as declared in the workflow file, 1..=255 bytes, never `..`",
        ));
    }
    if !category_shape(&request.category) {
        errors.push(ArgumentError::new(
            "category",
            format!("`{}` is not a category", request.category),
            "`^[a-z][a-z0-9_]*$`, at most 64 bytes",
        ));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        let summary = errors
            .iter()
            .map(|error| format!("{}: {}", error.path, error.reason))
            .collect::<Vec<_>>()
            .join("; ");
        Err(ValidationError::InvalidArguments { summary, errors })
    }
}

/// `$defs/commit_input`: `^[0-9a-fA-F]{7,40}$`.
fn commit_shape(text: &str) -> bool {
    (COMMIT_MIN_LEN..=COMMIT_MAX_LEN).contains(&text.len())
        && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// `$defs/workflow_name`: the two alternations, plus the "no two consecutive dots"
/// rule that makes traversal inexpressible.
fn workflow_shape(text: &str) -> bool {
    name_shape(text, true)
}

/// `$defs/task_id`.
fn task_shape(text: &str) -> bool {
    name_shape(text, false)
}

fn name_shape(text: &str, allow_workflows_prefix: bool) -> bool {
    if text.is_empty() || text.len() > 255 || text.contains("..") {
        return false;
    }
    if allow_workflows_prefix {
        if let Some(rest) = text.strip_prefix(&format!("{WORKFLOWS_DIR}/")) {
            return plain_name(rest);
        }
    }
    plain_name(text)
}

/// `[A-Za-z0-9][A-Za-z0-9 ._/-]*` — letters, digits, space, dot, underscore, slash
/// and dash, starting with alphanumeric. No leading `-` (argv injection), no
/// leading `/` (absolute paths), and combined with the dot rule, no traversal.
fn plain_name(text: &str) -> bool {
    let mut characters = text.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric())
        && characters.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, ' ' | '.' | '_' | '/' | '-')
        })
}

/// `$defs/category`.
fn category_shape(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 64
        && text.starts_with(|first: char| first.is_ascii_lowercase())
        && text.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '_'
        })
}

/// Resolve one workflow name through ADR-0001 §2.2's four forms.
///
/// The precedence itself lives in [`workflows::resolve`], shared with `list_tasks`;
/// this only translates the outcome into ADR-0001 §5.3's codes.
fn resolve_workflow<'a>(
    entries: &'a [WorkflowFile],
    requested: &str,
) -> Result<&'a WorkflowFile, ValidationError> {
    workflows::resolve(entries, requested).map_err(|error| match error {
        ResolveError::NotFound { available, reason } => ValidationError::WorkflowNotFound {
            workflow: requested.to_owned(),
            workflows_dir: WORKFLOWS_DIR,
            available: ValidationError::option_list(available),
            reason,
        },
        ResolveError::Ambiguous { candidates } => ValidationError::AmbiguousWorkflow {
            workflow: requested.to_owned(),
            candidates: ValidationError::option_list(candidates),
        },
    })
}

/// ADR-0001 §2.3: the job id must be one the resolved file declares.
fn confirm_task(workflow: &WorkflowFile, task: &str) -> Result<String, ValidationError> {
    if let Some(error) = &workflow.parse_error {
        return Err(ValidationError::TaskNotFound {
            workflow_file: workflow.relpath.clone(),
            task: task.to_owned(),
            jobs: Vec::new(),
            reason: Some(format!(
                "the file could not be indexed: {} ({})",
                error.code(),
                error.message()
            )),
        });
    }
    if workflow.index.has_job(task) {
        Ok(task.to_owned())
    } else {
        Err(ValidationError::TaskNotFound {
            workflow_file: workflow.relpath.clone(),
            task: task.to_owned(),
            // Job ids keep file order (the caller is scanning for their typo),
            // so this list is truncated rather than sorted.
            jobs: workflow
                .index
                .job_ids()
                .into_iter()
                .map(str::to_owned)
                .collect(),
            reason: None,
        })
    }
}

/// The file in the tree being inspected; for a commit tree that does not exist on
/// disk yet, the workspace-anchored path (see [`ValidatedRunTarget::workflow_file`]).
fn workflow_path(workflow: &WorkflowFile, root: &Path, plan: &TreePlan) -> PathBuf {
    match plan.root {
        TreeRoot::Workspace | TreeRoot::Materialized { .. } => workflow.on_disk.clone(),
        // The tree does not exist on disk yet, so anchor the relpath at the
        // workspace; whoever materializes the tree re-anchors it.
        TreeRoot::CommitTree => root.join(&workflow.relpath),
    }
}

/// Probe TTL in the clock's unit, saturating on absurd values.
fn ttl_millis(ttl: Duration) -> u64 {
    u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX)
}

fn clone_outcome(
    outcome: &Result<EnvironmentFacts, ValidationError>,
) -> Result<EnvironmentFacts, ValidationError> {
    outcome.clone()
}

/// A full lowercase 40-hex object id.
fn is_full_sha(text: &str) -> bool {
    text.len() == 40 && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}
