//! The per-commit metric model: `.evidence` in, one typed row per SHA out.
//!
//! This is ADR-0008 executed once, so that four screens never have to decide for themselves which
//! of six coverage records is *the* number. It is a pure function of `(evidence tree, commit list,
//! the two `fathom.toml` benchmark filters)`: no server, no cache, no clock, no git. The four
//! stages are the model's, in order:
//!
//! ```text
//! walk      benchmark/<40-hex>/<run_id>.jsonl, coverage/<40-hex>/<run_id>.jsonl   (nothing else)
//! retain    run_status == success · not truncated · source precedence · the benchmark filter
//! select    newest run per (kind, workflow, task)      newest == max(run_id), which is chronological
//! aggregate coverage = Σ hit / Σ found over the selected runs' totals
//!           benchmark = median over the samples of the newest selected run, per (name, labels, unit)
//!           action    = newest run_status per (kind, workflow, task) → success|failed|mixed|no_evidence
//! ```
//!
//! ## What this module is allowed to know
//!
//! The constants come from [`schema/metric-model.json`](../../../../schema/metric-model.json) via
//! [`Model`] — the rounding decimal, the measure order, the source precedence, which statuses count
//! as a pass, and the three closed vocabularies. Nothing is hard-coded twice, because a constant
//! stated twice is a constant that drifts. The vocabularies are the enums in [`vocab`], and
//! [`Model::check_self_consistent`] fails the build when the code and the model disagree.
//!
//! Reading the *records* is not done here: [`act_evidence::read`] does it, with the contract
//! crate's own strict types (ADR-0005 P2 forbids a second implementation of `.evidence`, and this
//! crate has no local record structs for the same reason). What lives here is the reader's policy —
//! which is exactly the half ADR-0005 §10 refuses to let into the contract crate.
//!
//! ## The two things a row can be wrong about, and what it says in each case
//!
//! * **No data.** `gaps.coverage` / `gaps.benchmarks` name which of the seven reasons a `—` is a
//!   `—`, and `action_success: "no_evidence"` means "nothing readable ran here", never "this commit
//!   failed" ([`CommitMetrics::is_no_evidence`]).
//! * **Unreadable data.** `unreadable_records` counts what was there and could not be read, and the
//!   gap is `unreadable` rather than `no_evidence` ([`CommitMetrics::is_unreadable`]). The two are
//!   different answers to different user questions, and both are unknown; conflating them is the
//!   failure mode ADR-0008 §5.4 refuses.
//!
//! [`Explain`] is the third answer: every record read is in it as kept (with its fate) or dropped
//! (with exactly one of the 18 reasons), which is what makes a wrong number answerable and what a
//! `--explain` flag renders when mact-by1.13 grows one.
//!
//! ## Worked example, from `scripts/sample_evidence.py`
//!
//! ```text
//! commit              = 2f7c1a4b9e0d3c6a5b8f1e2d4c6a8b0f3e5d7c91
//! coverage.percent    = 86.7          (3581+1904) / (4120+2210), pooled, never averaged
//! coverage.measure    = lines         (secondary: statements 86.1, branches 80.1, functions 89.9)
//! benchmarks          = 7 groups      (tokenizer_throughput 184666.55 ops/s, median of 2 samples)
//! action_success      = mixed         (coverage jobs succeeded, the benchmark job did not)
//! excluded_runs       = 1             (the failed benchmark run, named with its status)
//! caveats             = percent_only, low_confidence, secondary_measures,
//!                       context_only, unit_missing, run_excluded
//! gaps                = none
//! ```
//! ADR-0008 §3.8 pins that block and `scripts/validate_metric_model.py` re-derives it on every CI
//! run; `crates/fathom/tests/metrics.rs` asserts this reader produces the same row.

mod explain;
pub mod model;
mod pipeline;
pub mod row;
pub mod vocab;

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::{Path, PathBuf},
};

use act_evidence::read::{self, Scan};
use thiserror::Error;

pub use explain::{BenchmarkSpread, CoverageFile, CoverageRunTotal, Drop, Explain, Kept};
pub use model::Model;
pub use row::{
    canonical_json, median, number_text, round_half_away, ActionGroup, BenchmarkGroup,
    CommitMetrics, Coverage, ExcludedRun, Gaps, SecondaryMeasure, TotalSource,
};
pub use vocab::{ActionState, Caveat, Column, DropReason, GapCode, Measure, Stage};

/// Everything that can make a read fail outright.
///
/// Note what is *not* here: a bad record, a bad directory, a foreign `schema_version`, a missing
/// `coverage/`, an empty tree, a tree with only one kind of evidence, or a commit with no evidence
/// at all. Those are findings, and they are in [`Explain`] and in the rows, not in the `Result`.
#[derive(Debug, Error)]
pub enum Error {
    /// The evidence root could not be walked at all (it is a file, or a directory refuses to be
    /// listed).
    #[error(transparent)]
    Read(#[from] read::ReadError),

    /// A `--model` copy of `schema/metric-model.json` could not be read or parsed.
    #[error("cannot read the metric model {path}: {reason}\n  fix: the shipped model is schema/metric-model.json; pass --model only at a copy of it")]
    Model {
        /// Which file.
        path: PathBuf,
        /// Why, verbatim.
        reason: String,
    },

    /// This build and the model disagree about a closed vocabulary — a build-time bug in whichever
    /// moved, reported rather than papered over.
    #[error("this build disagrees with schema/metric-model.json: {0}\n  fix: a code and a model edit belong in the same commit (ADR-0008 §Machine-readable form)")]
    Vocabulary(String),
}

/// Whether a column carries a number, no data, or unreadable data.
///
/// The three-way split is the ticket's "distinguish no data from unreadable data in the return
/// type", and the reason it is an enum rather than a boolean is that all three render `—` on screen
/// and none of the three means the same thing to the person looking at it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnState {
    /// The column has a number.
    Number,
    /// Nothing usable was found — one of the gap codes that is not `unreadable`.
    NoData,
    /// Records were present and this build could not read them.
    Unreadable,
}

impl fmt::Display for ColumnState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Number => "number",
            Self::NoData => "no data",
            Self::Unreadable => "unreadable",
        })
    }
}

impl CommitMetrics {
    /// The coverage column's state: a number, no data, or unreadable data.
    #[must_use]
    pub fn coverage_state(&self) -> ColumnState {
        state(
            self.coverage.is_some(),
            self.gaps.coverage,
            self.unreadable_records,
        )
    }

    /// The benchmark column's state.
    #[must_use]
    pub fn benchmark_state(&self) -> ColumnState {
        state(
            !self.benchmarks.is_empty(),
            self.gaps.benchmarks,
            self.unreadable_records,
        )
    }
}

fn state(has_number: bool, gap: Option<GapCode>, unreadable: u64) -> ColumnState {
    if has_number {
        ColumnState::Number
    } else if gap == Some(GapCode::Unreadable) || (gap.is_none() && unreadable > 0) {
        ColumnState::Unreadable
    } else {
        ColumnState::NoData
    }
}

/// The reader: a metric model, the benchmark filter from `fathom.toml`, and no other state.
///
/// Cheap to build and safe to share; one per process is normal, one per `fathom.toml` seen is
/// correct. It holds no `Scan` and no rows, so nothing can be cached across a configuration change
/// by accident.
#[derive(Debug, Clone)]
pub struct Reader {
    model: Model,
    allow: BTreeSet<String>,
    deny: BTreeSet<String>,
}

impl Default for Reader {
    fn default() -> Self {
        Self::new()
    }
}

impl Reader {
    /// A reader with the shipped model and no benchmark filter (allow-all: fathom does not hide
    /// evidence by default, ADR-0006 §7.2).
    #[must_use]
    pub fn new() -> Self {
        Self {
            model: Model::embedded(),
            allow: BTreeSet::new(),
            deny: BTreeSet::new(),
        }
    }

    /// A reader pointed at another copy of the model — a checker's `--model`, or a test that wants
    /// to prove the constants are actually read.
    ///
    /// # Errors
    ///
    /// [`Error::Model`] if the file cannot be read or does not parse.
    pub fn with_model_path(path: &Path) -> Result<Self, Error> {
        Ok(Self {
            model: Model::load(path)?,
            allow: BTreeSet::new(),
            deny: BTreeSet::new(),
        })
    }

    /// A reader configured from the resolved `fathom.toml` (`benchmarks.allow` / `benchmarks.deny`,
    /// deny winning — ADR-0006 §7.2's "filters what is kept, not what is plotted").
    #[must_use]
    pub fn from_config(config: &crate::config::Config) -> Self {
        Self {
            model: Model::embedded(),
            allow: config.benchmarks.allow.iter().cloned().collect(),
            deny: config.benchmarks.deny.iter().cloned().collect(),
        }
    }

    /// The model in force.
    #[must_use]
    pub const fn model(&self) -> &Model {
        &self.model
    }

    /// Whether a benchmark metric name survives `benchmarks.allow`/`deny`. Deny wins; an empty
    /// allow list allows everything.
    #[must_use]
    pub fn keeps_benchmark(&self, name: &str) -> bool {
        if self.deny.contains(name) {
            return false;
        }
        self.allow.is_empty() || self.allow.contains(name)
    }

    /// Walk `root` and derive a row for every commit the tree has records for.
    ///
    /// Convenient and slightly wrong-looking: ADR-0008 §2.4 says rows come from the *git* walk, not
    /// from a directory listing, so this is the right call for `doctor`, a test, or a tree dumped
    /// for inspection, and the wrong one for the timeline (use [`Reader::read_commits`], which is
    /// what the index will hand it).
    ///
    /// # Errors
    ///
    /// [`enum@Error`] only for a root that cannot be walked at all, or a model this build cannot use.
    pub fn read(&self, root: &Path) -> Result<Readset, Error> {
        self.derive(root, None)
    }

    /// Walk `root` and derive a row for **exactly** `commits` — including the commits with no
    /// evidence, which get `no_evidence` rows rather than no rows (P1).
    ///
    /// # Errors
    ///
    /// As [`Reader::read`].
    pub fn read_commits(&self, root: &Path, commits: &[String]) -> Result<Readset, Error> {
        self.derive(root, Some(commits))
    }

    fn derive(&self, root: &Path, commits: Option<&[String]>) -> Result<Readset, Error> {
        self.model.check_self_consistent()?;
        let scan: Scan = read::read_root(root)?;

        let mut explain = Explain::default();
        explain.set_counts(
            scan.files(),
            scan.lines(),
            scan.records(),
            scan.refusals().len(),
        );

        // Stage 1's leftovers: the walk-stage refusals become drops with their own reason codes, so
        // `--explain` accounts for every path the walk touched, not only the ones that parsed.
        for refusal in scan.refusals() {
            explain.push_drop(Drop {
                sha: refusal.sha.clone().unwrap_or_default(),
                location: refusal.location.clone(),
                reason: pipeline::walk_reason(refusal.reason),
                detail: refusal.detail.clone(),
                run_id: refusal.run_id.clone(),
                status: refusal.run_status.clone(),
            });
        }

        let populations = pipeline::retain(self, &scan, &mut explain);

        // The commit set: git's when the caller has one, the tree's when it does not. A SHA with no
        // evidence still gets a row — an absent commit in the timeline is a missing row, and a
        // missing row is worse than an unknown one.
        let mut shas: Vec<String> = match commits {
            Some(commits) => commits.to_vec(),
            None => {
                let mut shas: Vec<String> = populations.readable.keys().cloned().collect();
                for (sha, _) in scan.unreadable_commits() {
                    shas.push(sha.to_owned());
                }
                shas
            }
        };
        shas.sort();
        shas.dedup();

        let mut rows = Vec::with_capacity(shas.len());
        for sha in &shas {
            let readable: &[read::Record] = populations
                .readable
                .get(sha)
                .map_or(&[], |records| records.as_slice());
            let retained: &[read::Record] = populations
                .retained
                .get(sha)
                .map_or(&[], |records| records.as_slice());
            let unreadable = u64::try_from(scan.unreadable(sha)).unwrap_or(u64::MAX);
            rows.push(pipeline::derive_commit(
                self,
                sha,
                readable,
                retained,
                unreadable,
                &mut explain,
            ));
        }
        explain.sort_drops();

        Ok(Readset {
            root: root.to_path_buf(),
            root_present: scan.root_present(),
            rows: rows
                .into_iter()
                .map(|row| (row.commit.clone(), row))
                .collect(),
            explain,
        })
    }
}

/// One read of one evidence root: a row per commit, and the drop log that explains them.
#[derive(Debug, Clone, Default)]
pub struct Readset {
    root: PathBuf,
    root_present: bool,
    rows: BTreeMap<String, CommitMetrics>,
    explain: Explain,
}

impl Readset {
    /// The root that was read.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Whether the root existed. Absent is legal and means every commit is unknown, not that the
    /// read failed.
    #[must_use]
    pub fn root_present(&self) -> bool {
        self.root_present
    }

    /// The rows, oldest SHA first (`BTreeMap` order over the 40-hex keys).
    pub fn rows(&self) -> impl Iterator<Item = &CommitMetrics> {
        self.rows.values()
    }

    /// How many rows were produced (which, per P1, is how many commits the caller named).
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether no rows were asked for.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// One commit's row.
    #[must_use]
    pub fn metrics(&self, sha: &str) -> Option<&CommitMetrics> {
        self.rows.get(sha)
    }

    /// The drop log.
    #[must_use]
    pub fn explain(&self) -> &Explain {
        &self.explain
    }

    /// One commit's per-file coverage rows (ADR-0008 §3's `scope:"file"` half, pooled per path over
    /// the selected runs), in path order.
    ///
    /// Not part of the metric-model row shape — `row_shape` is closed and belongs to ADR-0008 — but the
    /// same derivation's output all the same, which is what lets the store cache it beside the row it
    /// belongs to instead of a second reader re-walking the tree to get it (mact-by1.14).
    #[must_use]
    pub fn coverage_files(&self, sha: &str) -> &[CoverageFile] {
        self.explain.coverage_files(sha)
    }

    /// One commit's per-run coverage totals, used and unused alike.
    #[must_use]
    pub fn coverage_totals(&self, sha: &str) -> &[CoverageRunTotal] {
        self.explain.coverage_totals(sha)
    }

    /// Rows whose action state is one of `failed` / `mixed` — the set
    /// `fathom report --fail-on action-failure` gates on, which deliberately excludes
    /// `no_evidence` (a commit that never ran is not a commit that failed).
    #[must_use]
    pub fn failures(&self) -> Vec<&CommitMetrics> {
        self.rows
            .values()
            .filter(|row| matches!(row.action_success, ActionState::Failed | ActionState::Mixed))
            .collect()
    }

    /// Every commit whose rows carry at least one caveat.
    #[must_use]
    pub fn caveated(&self) -> Vec<&CommitMetrics> {
        self.rows
            .values()
            .filter(|row| !row.caveats.is_empty())
            .collect()
    }
}
