//! The category pipeline: the category-specific half of a run, plugged into the supervisor.
//!
//! [`crate::registry::Pipeline`] is the seam mact-f1w.11 left for this ticket, and this is the
//! implementation it was asking for — three things, each one a step of ADR-0004 §3.4 that only
//! the wiring can decide:
//!
//! * **`prepare`** — ADR-0003 §5.1's `git worktree add --detach`, for a run whose tree is not
//!   the workspace. Head-only runs have no tree to make and say so in one comparison.
//! * **`harvest`** — the category→extractor table: `coverage` runs the lcov/istanbul extractor,
//!   `benchmark` runs the metrics/timings extractor, anything else publishes nothing and stamps
//!   `unknown_category` (ADR-0001 §2.4: an unknown category is a run with a label, not an
//!   error). The extractors themselves belong to mact-f1w.10/.12 and are used as they ship;
//!   nothing is re-parsed here.
//! * **`release`** — ADR-0003 §5.1's destroy sequence and the per-run scratch directory, after
//!   the terminal commit (§3.4 step 6), never before it.
//!
//! ## What this module never does
//!
//! It never touches run state (the run's own task owns that, ADR-0004 §3.5), never deletes a
//! published evidence file (ADR-0002 §6.2 — cleanup deletes scratch only), and never lets an
//! extraction problem fail a run: an unusable payload is a warning, so a run that published
//! nothing still reports the act exit code it earned (ADR-0002 §5.1). The only thing that makes
//! a run `error` from here is a step that could not run at all.
//!
//! ## Why extraction runs on the blocking pool
//!
//! `lcov.info` is megabytes and a `--bind` payload lives on whatever filesystem the job chose;
//! the harvest budget (default 120 s) is what bounds that, and a worker parsing 8 MiB of
//! tracefile is a worker not serving `run_status` polls.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use crate::{
    evidence::{benchmark, coverage, records::Kind, writer::hostname, ExtractContext, RunRecords},
    registry::{Harvest, Pipeline, PipelineError, RunContext, WireStatus},
    runner::{process, ActRunOutcome, RunStatus},
    workspace::{IsolationMode, ValidatedRunTarget, DEFAULT_GIT_TIMEOUT},
};

/// Directory name of the leaf ADR-0003 §5.1 creates inside `<run-scratch>/<run>/`.
///
/// The name is part of the layout the wiring owns: the tool builds the plan's `tree` from it,
/// `prepare` creates it and `release` destroys it, so exactly one place may spell it.
pub const WORKTREE_DIR: &str = "worktree";

/// Warning for a category that is legal but has no extractor (ADR-0001 §2.4).
pub const WARNING_UNKNOWN_CATEGORY: &str = "unknown_category";

/// Warning stamped when a tree or a scratch directory could not be removed (ADR-0003 §9.3).
///
/// It reaches the *log* rather than the run: cleanup happens after the terminal commit, so the
/// run's own `warnings[]` is already on disk, and inventing a post-hoc state change would break
/// "a terminal run never transitions" (ADR-0004 §17.1 T20).
pub const WARNING_WORKTREE_CLEANUP_FAILED: &str = "worktree_cleanup_failed";

/// The default [`Pipeline`] the server runs: real extractors, real worktrees.
///
/// Cheap and `Send + Sync`, because the supervisor holds one `Arc` for the process; the only
/// state is the git deadline (`--git-timeout-secs`), which applies to `worktree add` exactly as
/// it does to the validator's `git` calls.
#[derive(Debug, Clone, Copy)]
pub struct EvidencePipeline {
    /// `--git-timeout-secs`, applied to the tree's create and destroy.
    pub git_timeout: Duration,
}

impl EvidencePipeline {
    /// The pipeline a boot configuration implies.
    #[must_use]
    pub fn new(git_timeout: Duration) -> Self {
        Self { git_timeout }
    }
}

impl Default for EvidencePipeline {
    fn default() -> Self {
        Self::new(DEFAULT_GIT_TIMEOUT)
    }
}

impl Pipeline for EvidencePipeline {
    fn prepare(
        &self,
        context: &RunContext,
    ) -> crate::registry::BoxFuture<'_, Result<(), PipelineError>> {
        // Cloned out so the future is `'static` and holds no borrow of the caller's context
        // (the same reason `harvest` and `release` do it).
        let context = context.clone();
        Box::pin(async move {
            let Some(tree) = synthesized_tree(&context) else {
                return Ok(());
            };
            // ADR-0003 §5.1: only the *parent* is created — `git worktree add` refuses an
            // existing leaf, so a collision at the leaf is a run_id collision, reported rather
            // than deleted.
            if let Err(reason) = create_scratch_dir(&context.scratch_dir) {
                return Err(PipelineError::Tree(format!(
                    "could not create the run's scratch directory {}: {reason}",
                    context.scratch_dir.display()
                )));
            }
            let tree_text = tree.display().to_string();
            let output = git(
                &context.workspace,
                &[
                    "worktree",
                    "add",
                    "--detach",
                    &tree_text,
                    &context.identity.commit,
                ],
                self.git_timeout,
            )
            .await;
            match output {
                Some(output) if output.success() => {
                    tracing::debug!(
                        run_id = %context.run_id,
                        tree = %tree.display(),
                        commit = %context.identity.commit,
                        "materialised the run's ADR-0003 worktree"
                    );
                    Ok(())
                }
                Some(output) => Err(PipelineError::Tree(format!(
                    "`git worktree add --detach {tree_text}` at {} failed: {}",
                    context.identity.commit,
                    output.summary()
                ))),
                None => Err(PipelineError::Tree(format!(
                    "`git worktree add --detach {tree_text}` did not complete within {:?}",
                    self.git_timeout
                ))),
            }
        })
    }

    fn harvest(
        &self,
        context: &RunContext,
        outcome: &ActRunOutcome,
        label: WireStatus,
    ) -> crate::registry::BoxFuture<'_, Result<Harvest, PipelineError>> {
        // Everything the step needs, cloned so the blocking closure is `'static`.
        let context = context.clone();
        let outcome = outcome.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || harvest(&context, &outcome, label))
                .await
                .map_err(|join_error| {
                    PipelineError::Harvest(format!("the extractor did not complete: {join_error}"))
                })?
        })
    }

    fn release(&self, context: &RunContext) -> crate::registry::BoxFuture<'_, ()> {
        let context = context.clone();
        Box::pin(async move {
            if let Some(tree) = synthesized_tree(&context) {
                destroy_tree(&context.workspace, &tree, self.git_timeout).await;
            }
            // `<run-scratch>/<run>/` held act's cwd and, until the runner unlinked it, the
            // secret file; ADR-0003 §9.1 has it gone on every terminal path.
            match tokio::fs::remove_dir_all(&context.scratch_dir).await {
                Ok(()) => {}
                Err(reason) if reason.kind() == std::io::ErrorKind::NotFound => {}
                Err(reason) => tracing::warn!(
                    run_id = %context.run_id,
                    scratch = %context.scratch_dir.display(),
                    "{WARNING_WORKTREE_CLEANUP_FAILED}: could not remove the run's scratch directory: {reason}"
                ),
            }
        })
    }
}

/// The tree that has to be created, or `None` when the run executes in the workspace itself.
///
/// `tree != workspace` is the mode test: ADR-0003 §4.1 makes `head_only` the only mode whose
/// tree *is* the workspace, and the tool builds the plan that way, so the pipeline needs no
/// separate mode field to know which §5.1 commands apply.
fn synthesized_tree(context: &RunContext) -> Option<PathBuf> {
    (context.tree != context.workspace).then(|| context.tree.clone())
}

/// The warning code of a category that is legal but has no extractor, or `None` when one runs.
#[must_use]
pub fn unknown_category_warning(category: &str) -> Option<&'static str> {
    crate::tools::category::kind_of(category)
        .is_none()
        .then_some(WARNING_UNKNOWN_CATEGORY)
}

/// The category table, run off the I/O worker (see the module docs).
fn harvest(
    context: &RunContext,
    outcome: &ActRunOutcome,
    label: WireStatus,
) -> Result<Harvest, PipelineError> {
    let Some(kind) = crate::tools::category::kind_of(&context.identity.category) else {
        // ADR-0001 §2.4: no extractor ships for this category, so the run is normal, the
        // evidence list is empty, and the result says why instead of looking broken.
        return Ok(Harvest {
            batches: Vec::new(),
            warnings: vec![WARNING_UNKNOWN_CATEGORY.to_owned()],
            malformed: 0,
            consumed: Vec::new(),
        });
    };
    // The label and the outcome's own word agree by construction — `WireStatus::from_run_status`
    // is total over the same five words, and the runner owns the deadline and the kill. Asserted
    // rather than trusted because the *records* carry this word, and ADR-0004 §6.2 makes "a
    // timed_out run's records say `timed_out`" an observable rule.
    debug_assert_eq!(
        label.as_str(),
        WireStatus::from_run_status(outcome.status).as_str(),
        "harvest was asked to label a run differently from what act reported"
    );
    let target = target_for(context);
    let extract_context = extract_context(context);
    let extraction: Result<Extraction, String> = match kind {
        Kind::Coverage => coverage::extract_with(outcome, &target, &extract_context)
            .map(|summary| Extraction {
                warnings: summary.warning_codes(),
                malformed: summary.malformed,
                batches: summary
                    .run_records()
                    .into_iter()
                    .collect::<Vec<RunRecords>>(),
                found: summary
                    .found
                    .iter()
                    .map(|found| (found.path.clone(), found.used))
                    .collect(),
                payload_dir: context.payload_dir.clone(),
            })
            // The two extractors have their own error enums; the only thing this layer needs
            // from either is the ADR-0002 warning code it carries, so the types are erased here
            // rather than wrapped in a third enum.
            .map_err(|error| error.warning_code().to_owned()),
        Kind::Benchmark => benchmark::extract_with(outcome, &target, &extract_context)
            .map(|summary| {
                let mut warnings = summary.warning_codes();
                // `BenchmarkSummary::warning_codes` carries the counting codes; the outcome's
                // own word is the one a scanner filters on, and `ExtractionOutcome::as_str` is
                // exactly the `meta.json` code for those two cases (ADR-0002 §5.1).
                if matches!(
                    summary.outcome,
                    benchmark::ExtractionOutcome::ParseFailed
                        | benchmark::ExtractionOutcome::NoMetrics
                ) {
                    warnings.push(summary.outcome.as_str().to_owned());
                }
                Extraction {
                    warnings,
                    malformed: summary.malformed,
                    batches: summary
                        .run_records()
                        .into_iter()
                        .collect::<Vec<RunRecords>>(),
                    found: summary
                        .found
                        .iter()
                        .map(|found| (found.path.clone(), found.used))
                        .collect(),
                    payload_dir: context.payload_dir.clone(),
                }
            })
            .map_err(|error| error.warning_code().to_owned()),
    };
    Ok(match extraction {
        // Even a refused extraction leaves the run's status untouched: an evidence problem is a
        // warning (ADR-0002 §5.1), so an extractor's `Err` never becomes `PipelineError`.
        Err(code) => {
            tracing::warn!(
                run_id = %context.run_id,
                category = %context.identity.category,
                "extraction refused the run's payload ({code})"
            );
            Harvest {
                batches: Vec::new(),
                warnings: vec![code],
                malformed: 0,
                consumed: Vec::new(),
            }
        }
        Ok(extraction) => Harvest {
            consumed: payload_to_delete(&extraction.payload_dir, &extraction.found),
            batches: extraction.batches,
            warnings: extraction.warnings,
            malformed: extraction.malformed,
        },
    })
}

/// What an extractor produced, in the four shapes [`Harvest`] wants.
struct Extraction {
    batches: Vec<RunRecords>,
    warnings: Vec<String>,
    malformed: usize,
    /// Every payload file the extractor saw, as `(display path, used)`. Owned rather than
    /// borrowed: the two extractors have separate `Discovered` types that happen to carry this
    /// pair, and cloning a path and a flag beats a dyn-any cast whose lifetime outlives the
    /// summary it came from.
    found: Vec<(String, bool)>,
    payload_dir: PathBuf,
}

/// Which payload files may be deleted after publishing (ADR-0002 §3.4).
///
/// `Discovered::path` is a *display* spelling (workspace-relative), so it is not a path to
/// delete; only the file name is reused, and only against the per-run payload scratch. Anything
/// the extractor found inside the tree is left alone — in `head_only` mode the tree *is* the
/// operator's checkout, and act-mcp does not delete files it did not create.
fn payload_to_delete(payload_dir: &Path, found: &[(String, bool)]) -> Vec<PathBuf> {
    found
        .iter()
        .filter(|(_path, used)| *used)
        .filter_map(|(path, _used)| {
            let name = Path::new(path).file_name()?.to_owned();
            let candidate = payload_dir.join(name);
            candidate.is_file().then_some(candidate)
        })
        .collect()
}

/// Rebuild the identity the extractors insist on from the supervisor's [`RunContext`].
///
/// The extractors take a [`ValidatedRunTarget`] because that is the only type whose fields are
/// all *resolved* (full SHA, canonical relpath, real job id), and by the time the supervisor
/// harvests, the target itself is long gone — every field, though, is in the run's identity,
/// which was built from that target. Nothing here is re-derived from logs or from disk, which is
/// the rule ADR-0002 §3.3 cares about.
fn target_for(context: &RunContext) -> ValidatedRunTarget {
    let identity = &context.identity;
    ValidatedRunTarget {
        workflow_file: context.tree.join(&identity.workflow_file),
        workflow_display: identity.workflow.clone(),
        workflow_relpath: identity.workflow_file.clone(),
        job_id: identity.task.clone(),
        commit_full_sha: identity.commit.clone(),
        category: identity.category.clone(),
        workspace_root: context.workspace.clone(),
        evidence_dir: context.evidence_dir.clone(),
        mode: if synthesized_tree(context).is_some() {
            IsolationMode::Worktree
        } else {
            IsolationMode::HeadOnly
        },
        // The gate's warnings were stamped on the *run* by the supervisor at registration;
        // the extractors read this field for nothing more than completeness of the type.
        warnings: Vec::new(),
    }
}

/// Where the extractors look, and what they stamp in the envelope.
fn extract_context(context: &RunContext) -> ExtractContext {
    ExtractContext {
        tree_root: context.tree.clone(),
        workspace_root: context.workspace.clone(),
        payload_dir: context.payload_dir.clone(),
        host: hostname(),
        // act's own version is not captured by the runner (mact-f1w.19 will read it from act's
        // `--json` output), so the envelope omits it rather than guessing.
        act_version: None,
        server_version: env!("CARGO_PKG_VERSION").to_owned(),
    }
}

/// `mkdir` the per-run scratch dir with mode 0700, umask-proof (ADR-0003 §5.1).
fn create_scratch_dir(path: &Path) -> Result<(), String> {
    std::fs::create_dir_all(path).map_err(|reason| reason.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|reason| reason.to_string())?;
    }
    Ok(())
}

/// ADR-0003 §5.1's destroy sequence, in the pinned order, each step best-effort.
///
/// `git worktree remove --force` is the first and the only interesting call: `--force` is
/// mandatory because a `--bind` job leaves the tree dirty, and it is idempotent in the way
/// cleanup needs (rc 0 on a vanished tree, dropping the admin entry). `prune` is deliberately
/// never run — it would edit worktree metadata this process did not create (§5.1's note).
async fn destroy_tree(workspace: &Path, tree: &Path, timeout: Duration) {
    let tree_text = tree.display().to_string();
    let remove = ["worktree", "remove", "--force", &tree_text];
    let first = git(workspace, &remove, timeout).await;
    if first.as_ref().is_some_and(|output| output.success()) {
        return;
    }
    tracing::debug!(
        tree = %tree.display(),
        reason = first.as_ref().map(|output| output.summary()).unwrap_or_else(|| "git could not run".to_owned()),
        "`git worktree remove --force` did not succeed; falling back to removing the directory"
    );
    if tokio::fs::remove_dir_all(tree).await.is_err() {
        // Second `remove --force`: on a directory that is now gone it still drops the admin
        // entry, which is what keeps `.git/worktrees/` from accumulating ghosts.
        let _ = git(workspace, &remove, timeout).await;
    }
    if tree.exists() {
        tracing::warn!(
            workspace = %workspace.display(),
            tree = %tree.display(),
            "{WARNING_WORKTREE_CLEANUP_FAILED}: neither `git worktree remove --force` nor removal \
             succeeded; the tree is still on disk and must be pruned by hand (`git worktree \
             prune` is the operator's call, not ours)"
        );
    }
}

/// One `git` call with ADR-0003 §7.8's environment, and **without** `--no-optional-locks`.
///
/// The validator's wrapper (mact-f1w.6) always passes `--no-optional-locks`, because a
/// read-only probe must not create a lockfile; a worktree mutation is the opposite case — those
/// locks are what makes concurrent `git` safe — so this call builds its own argv. Same scrubbed
/// environment, same deadline, same rule that argv elements are never interpolated into a shell.
///
/// The child is its own process group so a hung `git` (an `index.lock` held by another process,
/// a stalled network mount) is taken down the way ADR-0003 §5.5 asks: `SIGTERM` to the group,
/// then `SIGKILL`, with the pre-start budget bounding `prepare` as a whole.
async fn git(workspace: &Path, args: &[&str], timeout: Duration) -> Option<GitOutput> {
    let mut command = tokio::process::Command::new("git");
    command
        .arg("-C")
        .arg(workspace)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // `git` inherits almost nothing: no inherited `GIT_*` may decide what a mutation does,
        // and `GIT_TERMINAL_PROMPT=0` stops a credential prompt from blocking the deadline.
        .env_clear()
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C");
    for key in ["PATH", "HOME"] {
        if let Ok(value) = std::env::var(key) {
            command.env(key, value);
        }
    }
    command.process_group(0);
    let child = match command.spawn() {
        Ok(child) => child,
        Err(reason) => {
            tracing::warn!("`git {args:?}` could not be spawned: {reason}");
            return None;
        }
    };
    // `process_group(0)` makes the child the group leader, so pgid == pid.
    let pgid = child.id().unwrap_or(0) as i32;
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => Some(GitOutput {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }),
        Ok(Err(reason)) => {
            tracing::warn!("`git {args:?}` could not be waited for: {reason}");
            None
        }
        Err(_elapsed) => {
            tracing::warn!(
                timeout = ?timeout,
                "`git {args:?}` exceeded --git-timeout-secs; signalling the process group"
            );
            // `kill_on_drop` reaps the child when this future returns, but not the *group*:
            // `git` may have children of its own (a credential helper).
            let _ = process::signal_group(pgid, libc::SIGTERM);
            tokio::time::sleep(Duration::from_millis(200)).await;
            let _ = process::signal_group(pgid, libc::SIGKILL);
            None
        }
    }
}

/// ADR-0003 §5.4's `--defaultbranch` probe: the primary workspace's branch name, or `None` when
/// `HEAD` is detached.
///
/// A synthesized worktree is detached, so act's `FindGitRef` fails and `github.ref` falls back
/// to `refs/heads/master` unless told otherwise. Naming the operator's real branch is the honest
/// half of that fix; faking `GITHUB_REF` would be a lie of a different shape, so this is all of
/// it.
pub async fn default_branch(workspace: &Path, timeout: Duration) -> Option<String> {
    let output = git(
        workspace,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
        timeout,
    )
    .await?;
    output
        .success()
        .then(|| output.first_line().map(str::to_owned))
        .flatten()
}

/// Whether this outcome is one the extractors may be run for.
///
/// A caller-cancelled run never reaches `harvest` (the supervisor skips extraction and stamps
/// `partial_evidence_dropped`, ADR-0004 §6.2), so the states seen here are
/// `success | failed | timed_out | error`, all four of which publish what they earned (§6.1:
/// there is no such thing as half-written evidence, and a timed-out benchmark still publishes
/// the samples it collected).
#[must_use]
pub fn harvests(status: RunStatus) -> bool {
    !matches!(status, RunStatus::Cancelled)
}

/// The slice of a `git` result this pipeline needs.
#[derive(Debug)]
struct GitOutput {
    code: i32,
    stdout: String,
    stderr: String,
}

impl GitOutput {
    fn success(&self) -> bool {
        self.code == 0
    }

    /// The one-line summary for a message or a log line: stderr is where git explains itself.
    fn summary(&self) -> String {
        self.combined()
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("git produced no output")
            .to_owned()
    }

    /// First non-empty stdout line, trimmed — the branch name of `symbolic-ref --short`.
    fn first_line(&self) -> Option<&str> {
        self.stdout
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
    }

    fn combined(&self) -> String {
        if self.stderr.trim().is_empty() {
            self.stdout.clone()
        } else {
            self.stderr.clone()
        }
    }
}

/// ADR-0003 §9.2's startup sweep: every `<run-scratch>/<run>/` still on disk belongs to a process
/// that is gone, and this is the only thing that ever removes it.
///
/// Runs before the server accepts a call, so nothing can be live and no lock or coordination is
/// needed — the ADR's whole crash-recovery story is "nothing is live yet, so all of it is debris".
/// `git worktree remove --force` runs first, because a bare `rm -rf` would leave the checkout's
/// `.git/worktrees/` metadata pointing at a directory that no longer exists; `git worktree prune`
/// is deliberately *not* run (§5.1: editing the metadata of a tree this process did not create is
/// the operator's call).
///
/// Returns how many leftover directories were removed, for the caller's log line and for tests.
pub async fn sweep_scratch_root(scratch_root: &Path, workspace: &Path, timeout: Duration) -> usize {
    let Ok(mut entries) = tokio::fs::read_dir(scratch_root).await else {
        // No scratch root at all: nothing has ever run here.
        return 0;
    };
    let mut removed = 0;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if !tokio::fs::metadata(&path)
            .await
            .is_ok_and(|meta| meta.is_dir())
        {
            continue;
        }
        let tree = path.join(WORKTREE_DIR);
        if tokio::fs::metadata(&tree).await.is_ok() {
            let text = tree.display().to_string();
            match git(
                workspace,
                &["worktree", "remove", "--force", &text],
                timeout,
            )
            .await
            {
                Some(output) if output.success() => {}
                outcome => tracing::debug!(
                    tree = %tree.display(),
                    reason = outcome.map(|output| output.summary()).unwrap_or_else(|| "git could not run".to_owned()),
                    "the startup sweep could not ask git to remove a leftover tree; removing the \
                     directory anyway (ADR-0003 §5.1 leaves `git worktree prune` to the operator)"
                ),
            }
        }
        match tokio::fs::remove_dir_all(&path).await {
            Ok(()) => {
                removed += 1;
                tracing::info!(
                    scratch = %path.display(),
                    "removed a run's leftover scratch directory from a previous process"
                );
            }
            Err(reason) => tracing::warn!(
                scratch = %path.display(),
                "{WARNING_WORKTREE_CLEANUP_FAILED}: the startup scratch sweep could not remove \
                 this directory; it is debris from a previous process and must be deleted by hand: \
                 {reason}"
            ),
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scratch root of one dead run: `<scratch>/<run>/{inert.conf, worktree/}`, exactly what
    /// `prepare` leaves behind, with a real worktree so the `git` half of the sweep is exercised
    /// against real git metadata rather than a mocked call.
    fn leave_debris(scratch: &Path, repo: &crate::fixtures::TestRepo, run_id: &str) -> PathBuf {
        let run = scratch.join(run_id);
        std::fs::create_dir_all(&run).expect("create the leftover scratch dir");
        std::fs::write(run.join("inert.conf"), "act-mcp inert\n").expect("write the inert file");
        let tree = run.join(WORKTREE_DIR);
        let text = tree.display().to_string();
        let output = std::process::Command::new("git")
            .args(["-C"])
            .arg(repo.path())
            .args(["worktree", "add", "--detach", &text, "HEAD"])
            .output()
            .expect("git worktree add");
        assert!(
            output.status.success(),
            "the leftover worktree must be creatable: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        run
    }

    fn worktree_count(repo: &crate::fixtures::TestRepo) -> usize {
        repo.git(&["worktree", "list", "--porcelain"])
            .lines()
            .filter(|line| line.starts_with("worktree "))
            .count()
    }

    #[tokio::test]
    async fn the_startup_sweep_removes_everything_a_dead_run_left() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = crate::fixtures::TestRepo::init(&temp.path().join("ws"));
        let scratch = temp.path().join("scratch");
        std::fs::create_dir_all(&scratch).expect("create the scratch root");
        let first = leave_debris(&scratch, &repo, "20261110T000000Z-aaaaaa");
        let second = leave_debris(&scratch, &repo, "20261110T000001Z-bbbbbb");
        assert_eq!(worktree_count(&repo), 3, "the checkout plus two leftovers");

        let removed = sweep_scratch_root(&scratch, repo.path(), Duration::from_secs(30)).await;
        assert_eq!(removed, 2, "both leftovers are debris");
        assert!(!first.exists() && !second.exists(), "and both are gone");
        assert_eq!(
            worktree_count(&repo),
            1,
            "`git worktree remove --force` must have dropped the admin entries too"
        );
        assert!(
            repo.porcelain().is_empty(),
            "a sweep of scratch must not touch the checkout: {}",
            repo.porcelain()
        );

        // And it is idempotent: a second sweep finds nothing and removes nothing.
        assert_eq!(
            sweep_scratch_root(&scratch, repo.path(), Duration::from_secs(30)).await,
            0
        );
    }

    #[tokio::test]
    async fn sweeping_a_scratch_root_that_does_not_exist_is_a_no_op() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = crate::fixtures::TestRepo::init(&temp.path().join("ws"));
        let missing = temp.path().join("never-created");
        assert_eq!(
            sweep_scratch_root(&missing, repo.path(), Duration::from_secs(30)).await,
            0
        );
        assert!(worktree_count(&repo) == 1, "nothing was touched");
    }

    #[test]
    fn a_category_with_an_extractor_never_becomes_the_unknown_warning() {
        assert_eq!(unknown_category_warning("coverage"), None);
        assert_eq!(unknown_category_warning("benchmark"), None);
        assert_eq!(
            unknown_category_warning("fuzzing"),
            Some(WARNING_UNKNOWN_CATEGORY)
        );
        // The word itself is part of the documented vocabulary, so it must not drift.
        assert_eq!(WARNING_UNKNOWN_CATEGORY, "unknown_category");
        assert_eq!(WARNING_WORKTREE_CLEANUP_FAILED, "worktree_cleanup_failed");
    }
}
