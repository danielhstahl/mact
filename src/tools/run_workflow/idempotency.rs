//! `idempotency_key` → the run that key already means (ADR-0001 §8).
//!
//! The rule is small but load-bearing: a retry after a dropped connection must not start a
//! second 30-minute benchmark. So a key remembers one run, and a repeat call either *replays*
//! that run (`deduplicated: true`) or says the key already means something else
//! ([`Replay::Conflict`], which the tool renders as `idempotency_conflict`).
//!
//! ## Why two spellings of one tuple
//!
//! ADR-0001 §2.9 puts the replay check at step 3, **before** the workspace gates, so the
//! first comparison can only use what the caller typed — and a caller who retried with
//! `coverage.yml` where the first call said `coverage` typed a different *string* while
//! asking for the identical run. The table therefore holds both spellings of one run: the
//! [`RawTuple`] available at step 3 and the [`ResolvedTuple`] the validator produced after it.
//! Step 3 matches on the raw pair (cheap, and it still works when the workspace has since
//! gone away); a key whose raw pair differs is *not* decided there — the tool resolves this
//! call first and asks [`Table::recall_identity`], which also matches on the resolved
//! identity. Only when neither agrees is the answer a conflict.
//!
//! ## Lifetime
//!
//! In memory only, like run state itself (ADR-0004 §7.2), and evicted along with the run it
//! points at: [`Table::forget_runs`] is called with the ids the supervisor just evicted, so a
//! key can never outlive its run and replay into `run_not_found`. After a restart the table
//! is empty and a replay starts a fresh run — documented behaviour rather than a surprise,
//! because the durable record of the old run is `.evidence/runs/<run_id>/`.

use std::{
    collections::HashMap,
    sync::{Mutex, MutexGuard},
};

use serde_json::{json, Value};

/// What the caller typed, normalised only for case (`commit` is hex, and `0A` and `0a` are one
/// commit). This is the pair the step-3 check compares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawTuple {
    /// `workflow` verbatim.
    pub workflow: String,
    /// `task` verbatim.
    pub task: String,
    /// `category` verbatim (shape-validated already).
    pub category: String,
    /// `commit`, lowercased.
    pub commit: String,
}

impl RawTuple {
    /// Build the pair from validated arguments.
    #[must_use]
    pub fn new(workflow: &str, task: &str, category: &str, commit: &str) -> Self {
        Self {
            workflow: workflow.to_owned(),
            task: task.to_owned(),
            category: category.to_owned(),
            commit: commit.to_ascii_lowercase(),
        }
    }
}

/// What the validator established the caller meant: display names and full ids, so two
/// spellings of one request agree. Shaped like `crate::registry::RunIdentity` on purpose, and
/// built from a validated target so the two cannot drift.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTuple {
    /// Workflow display name (basename without extension).
    pub workflow: String,
    /// Workspace-relative workflow path.
    pub workflow_file: String,
    /// Job id.
    pub task: String,
    /// Category, echoed.
    pub category: String,
    /// Full lowercase SHA.
    pub commit: String,
}

impl ResolvedTuple {
    /// Build the pair from a validated target.
    #[must_use]
    pub fn from_target(target: &crate::workspace::ValidatedRunTarget) -> Self {
        Self {
            workflow: target.workflow_display.clone(),
            workflow_file: target.workflow_relpath.clone(),
            task: target.job_id.clone(),
            category: target.category.clone(),
            commit: target.commit_full_sha.clone(),
        }
    }

    /// The `detail.previous` / `detail.requested` spelling (ADR-0001 §5.3).
    #[must_use]
    pub fn to_value(&self) -> Value {
        json!({
            "workflow": self.workflow,
            "workflow_file": self.workflow_file,
            "task": self.task,
            "category": self.category,
            "commit": self.commit,
        })
    }
}

/// One remembered key.
#[derive(Debug, Clone)]
pub struct Remembered {
    /// The run the key points at.
    pub run_id: String,
    /// The typed pair it was registered with.
    pub requested: RawTuple,
    /// The resolved pair, filled when the run was registered (validation always precedes
    /// registration, so an entry always has one).
    pub resolved: ResolvedTuple,
}

/// What a repeat call with a known key should do with the remembered run.
#[derive(Debug, Clone)]
pub enum Replay {
    /// Same request: return this run instead of starting another.
    Match(Remembered),
    /// The key means a different request; the tool answers `idempotency_conflict`.
    Conflict {
        /// What the key already points at.
        remembered: Remembered,
    },
}

/// The shared table: one per process, next to the supervisor whose evictions it follows.
#[derive(Debug, Default)]
pub struct Table {
    entries: Mutex<HashMap<String, Remembered>>,
}

impl Table {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The step-3 check: does this key already point at exactly this typed request?
    ///
    /// A key whose typed pair differs is `None` rather than a conflict, because nothing has
    /// been resolved yet and `coverage` vs `coverage.yml` is one request seen two ways. The
    /// caller resolves and then asks [`Self::recall_identity`].
    #[must_use]
    pub fn recall_raw(&self, key: &str, requested: &RawTuple) -> Option<Remembered> {
        self.lock()
            .get(key)
            .filter(|remembered| &remembered.requested == requested)
            .cloned()
    }

    /// The deferred check, asked after this call's identity is resolved.
    ///
    /// Matches when either spelling agrees, and conflicts only when neither does — so a
    /// semantically identical retry never becomes `idempotency_conflict`.
    #[must_use]
    pub fn recall_identity(
        &self,
        key: &str,
        requested: &RawTuple,
        resolved: &ResolvedTuple,
    ) -> Option<Replay> {
        let remembered = self.lock().get(key).cloned()?;
        Some(
            if &remembered.requested == requested || &remembered.resolved == resolved {
                Replay::Match(remembered)
            } else {
                Replay::Conflict { remembered }
            },
        )
    }

    /// Register `run_id` under `key` (called after registration, never before: a run that was
    /// never created must not be replayable).
    pub fn remember(&self, key: &str, requested: RawTuple, resolved: ResolvedTuple, run_id: &str) {
        self.lock().insert(
            key.to_owned(),
            Remembered {
                run_id: run_id.to_owned(),
                requested,
                resolved,
            },
        );
    }

    /// Drop a key — used when the run it pointed at has stopped existing.
    pub fn forget(&self, key: &str) {
        self.lock().remove(key);
    }

    /// Drop every key pointing at one of `run_ids` (the supervisor just evicted them).
    ///
    /// Returns how many keys went, which is what a test asserts: the point of the call is that
    /// a key can never outlive its run.
    pub fn forget_runs(&self, run_ids: &[String]) -> usize {
        let mut entries = self.lock();
        let before = entries.len();
        entries.retain(|_, remembered| !run_ids.contains(&remembered.run_id));
        before - entries.len()
    }

    /// How many keys are remembered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether nothing is remembered (an assertion, and cheap for a test to state).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// Poison-tolerant lock: a panic while holding a map of strings must not turn every later
    /// `run_workflow` into an error for the life of the process.
    fn lock(&self) -> MutexGuard<'_, HashMap<String, Remembered>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(workflow: &str) -> RawTuple {
        RawTuple::new(workflow, "coverage", "coverage", "2f7c1a4")
    }

    fn resolved(workflow_file: &str) -> ResolvedTuple {
        ResolvedTuple {
            workflow: "coverage".to_owned(),
            workflow_file: workflow_file.to_owned(),
            task: "coverage".to_owned(),
            category: "coverage".to_owned(),
            commit: "2f7c1a4b9e0d3c6a5b8f1e2d4c6a8b0f3e5d7c91".to_owned(),
        }
    }

    #[test]
    fn a_replay_of_the_same_typed_request_matches() {
        let table = Table::new();
        table.remember(
            "sweep-1",
            raw("coverage"),
            resolved(".github/workflows/coverage.yml"),
            "20261110T004203Z-a1b2c3",
        );
        let remembered = table
            .recall_raw("sweep-1", &raw("coverage"))
            .expect("the same tuple replays");
        assert_eq!(remembered.run_id, "20261110T004203Z-a1b2c3");
        // An unknown key is not a conflict, it is a fresh request.
        assert!(table.recall_raw("sweep-2", &raw("coverage")).is_none());
        assert!(table
            .recall_identity("sweep-2", &raw("coverage"), &resolved("x"))
            .is_none());
    }

    #[test]
    fn a_different_tuple_under_one_key_is_a_conflict() {
        let table = Table::new();
        let remembered_resolved = resolved(".github/workflows/coverage.yml");
        table.remember(
            "sweep-1",
            raw("coverage"),
            remembered_resolved.clone(),
            "run-a",
        );
        let other = RawTuple::new("coverage", "lint", "coverage", "2f7c1a4");
        // Step 3 cannot tell yet …
        assert!(table.recall_raw("sweep-1", &other).is_none());
        // … and once it can, the answer is a conflict naming the remembered run.
        let different = ResolvedTuple {
            task: "lint".to_owned(),
            ..remembered_resolved
        };
        let replay = table
            .recall_identity("sweep-1", &other, &different)
            .expect("the key is known");
        let Replay::Conflict { remembered } = replay else {
            panic!("a different job id under one key must conflict");
        };
        assert_eq!(remembered.run_id, "run-a");
    }

    /// The case the deferred check exists for: one request, two spellings.
    #[test]
    fn two_spellings_of_one_request_replay_instead_of_conflicting() {
        let table = Table::new();
        let resolved = resolved(".github/workflows/coverage.yml");
        table.remember("sweep-1", raw("coverage"), resolved.clone(), "run-a");
        let retry = RawTuple::new("coverage.yml", "coverage", "coverage", "2f7C1A4");
        assert!(table.recall_raw("sweep-1", &retry).is_none());
        assert!(matches!(
            table.recall_identity("sweep-1", &retry, &resolved),
            Some(Replay::Match(_))
        ));
    }

    #[test]
    fn evicting_a_run_evicts_its_keys() {
        let table = Table::new();
        for (key, run) in [("a", "run-a"), ("b", "run-b"), ("c", "run-c")] {
            table.remember(
                key,
                raw("coverage"),
                resolved(".github/workflows/coverage.yml"),
                run,
            );
        }
        assert_eq!(table.forget_runs(&["run-b".to_owned()]), 1);
        assert_eq!(table.len(), 2);
        table.forget("a");
        assert!(table.recall_raw("a", &raw("coverage")).is_none());
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn commit_case_is_not_a_different_request() {
        let table = Table::new();
        table.remember(
            "k",
            RawTuple::new("w", "j", "c", "AbCdEf1"),
            resolved(".github/workflows/w.yml"),
            "run-a",
        );
        assert!(table
            .recall_raw("k", &RawTuple::new("w", "j", "c", "abcdef1"))
            .is_some());
    }

    #[test]
    fn the_conflict_detail_carries_both_identities() {
        let remembered = Remembered {
            run_id: "run-a".to_owned(),
            requested: raw("coverage"),
            resolved: resolved(".github/workflows/coverage.yml"),
        };
        let value = remembered.resolved.to_value();
        assert_eq!(value["workflow_file"], ".github/workflows/coverage.yml");
        assert_eq!(value["commit"], "2f7c1a4b9e0d3c6a5b8f1e2d4c6a8b0f3e5d7c91");
    }
}
