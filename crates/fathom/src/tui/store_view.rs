//! Reading the timeline out of the store: nothing else, and nothing on the UI thread.
//!
//! The store is the only data source the timeline has, and that is a deliberate narrowing rather than
//! a shortcut. Every number a row can show was derived by `fathom index` (mact-by1.10) and is
//! addressed by ADR-0006 §6.4; re-deriving any of it here would mean a second implementation of the
//! walk, the derive or the evidence reader inside the front end — the exact thing ADR-0005 P4/P5 keep
//! the crates apart. It also buys the boundary the ticket asks for ("typing and scrolling never block
//! on git or tree analysis"): this module is where all the file and `git` reads live, so the UI thread
//! can be proven not to touch them by looking at what it calls.
//!
//! What a read costs, in order:
//!
//! ```text
//! git rev-parse HEAD                    one subprocess, for the header and the staleness check
//! git symbolic-ref --short HEAD         one subprocess, for the branch name
//! list index/rows/**/*.json             one directory walk
//! one JSON parse per row file           bounded by what the last index walked, never by the repo
//! ```
//!
//! A row that is absent is not an error and is not skipped: a commit the index has not reached is a row
//! of unknowns with `indexed: false`, which is what lets a partially indexed repository render what it
//! has (§10.2's "an interrupted index leaves the store usable", used for the screen rather than for
//! `--status`).

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::boot::Boot;
use crate::classify::Classifier;
use crate::config::Config;
use crate::diff::{self, CommitDiff};
use crate::git::Git;
use crate::metrics::vocab::{ActionState, Column};
use crate::metrics::Model;
use crate::store::lock::LockBody;
use crate::store::row::Row;
use crate::store::{self, Run};

use super::bench;
use super::cells;
use super::detail;
use super::entry::{Bench, Churn, Delta, Entry, Metric};

/// How many row files one read will parse.
///
/// A guard rail, not a policy: a cache root that had been pointed at a 200 000-commit walk would
/// otherwise turn a frame's worth of work into a minute of JSON. The window a timeline can show is a
/// few thousand rows deep anyway (`history.depth` defaults far below this), so hitting the cap means
/// the header says so instead of the screen freezing.
const ROW_CAP: usize = 20_000;

/// Everything one read of one workspace answers.
///
/// Built by [`read`], consumed by [`super::state::App::apply`]. It is a value rather than a live
/// handle: the UI keeps the last snapshot it was given and the worker replaces it wholesale, which is
/// what makes "coalesce, never render a frame per record" a property of the type instead of a rule to
/// remember.
#[derive(Debug, Clone, Default)]
pub struct Reading {
    /// The repository's directory name (`act-mcp`), for the header.
    pub repo: String,
    /// The checked-out branch, or `(detached)` / `(none)`.
    pub branch: String,
    /// `HEAD`'s full SHA, absent on an unborn branch.
    pub head: Option<String>,
    /// The commits, newest first.
    pub entries: Vec<Entry>,
    /// The store's state, including "there is no store".
    pub store: StoreState,
    /// The last index run's counters, when the store has one.
    pub last_run: Option<Run>,
    /// Whether the evidence root exists (ADR-0002 §1).
    pub evidence_present: bool,
    /// The evidence root, for the empty-state line.
    pub evidence_path: PathBuf,
    /// The body of someone's `fathom.lock`, with its phase and counter (§9 L3).
    ///
    /// Read straight out of the lock file, which is never unlinked: the body of a run that finished (or
    /// died) is still there, so this is "who wrote here last and how far they got", not proof that a
    /// run is in flight. Nobody draws a present-tense conclusion from it; the engine's own progress
    /// events are what the header's live counter comes from.
    pub live_index: Option<LockBody>,
    /// A one-line refusal to show rather than a blank screen.
    pub notice: Option<String>,
    /// Commits the window had that `--limit` hid, so the header can say the picture is a cut.
    pub hidden: usize,
}

/// The store's own state, in the four states the screen needs to distinguish.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum StoreState {
    /// No `.fathom/index/` yet — nothing has ever been indexed here.
    #[default]
    Absent,
    /// A store, and how many rows it holds.
    Present {
        /// Row files found.
        rows: usize,
    },
    /// A store this build refuses to read (foreign, newer, damaged — ADR-0006 §8).
    Refused(String),
}

impl StoreState {
    /// Whether rows may be expected.
    #[must_use]
    pub const fn is_present(&self) -> bool {
        matches!(self, Self::Present { .. })
    }
}

/// How many commits the screen shows, `None` for "all the window has".
///
/// `--limit` is a *view* choice in ADR-0006 §7.4's sense: it is applied here, on the way out of the
/// reader, and never reaches a digest, a row file or the walk. The rows it hides are counted in
/// [`Reading::hidden`] rather than dropped silently, because a timeline that shows 5 of 412 commits and
/// says nothing is a timeline that is being lied about.
pub type Limit = Option<std::num::NonZeroUsize>;

/// Read the workspace once. Never called on the UI thread; called by [`super::engine`].
#[must_use]
pub fn read(boot: &Boot, limit: Limit) -> Reading {
    let mut reading = rows_only(boot, limit);

    // --- git: two subprocesses, both advisory ----------------------------------------------------
    // Neither can fail the screen. An unborn HEAD is the interesting case: the timeline is legitimately
    // empty and must say so, not refuse to draw.
    match Git::open(&boot.workspace) {
        Ok(git) => {
            if let Ok(head) = git.resolve("HEAD") {
                reading.head = Some(head);
            }
            reading.branch = branch_of(&git);
        }
        Err(reason) => reading.notice = Some(reason.to_string().replace('\n', " ")),
    }
    reading
}

/// The same read without the two `git` subprocesses, for the several-times-a-second poll an index run
/// drives (§engine::Work::poll).
///
/// The identity fields are filled in by the caller from the last full read — `HEAD` cannot move while
/// this process holds the index lock, and even if it did, a stale branch name in a header is a cosmetic
/// fault and spawning `git rev-parse` five times a second next to an index that is spawning `git log`
/// would be the second-guessing kind.
#[must_use]
pub fn rows_only(boot: &Boot, limit: Limit) -> Reading {
    let git_root = boot
        .git_root
        .clone()
        .unwrap_or_else(|| boot.workspace.clone());
    let mut reading = Reading {
        repo: git_root.file_name().map_or_else(
            || "workspace".to_owned(),
            |name| name.to_string_lossy().into_owned(),
        ),
        branch: String::from("(none)"),
        head: None,
        entries: Vec::new(),
        store: StoreState::Absent,
        last_run: None,
        evidence_present: boot.evidence.present,
        evidence_path: boot.evidence.path.clone(),
        live_index: store::lock::read_body(&boot.cache.path),
        notice: None,
        hidden: 0,
    };

    // --- the store -------------------------------------------------------------------------------
    let store = match store::Store::open(&boot.cache.path) {
        Ok(None) => {
            reading.store = StoreState::Absent;
            return reading;
        }
        Ok(Some(store)) => store,
        Err(reason) => {
            // A refusal, printed rather than raised: `tui` is a reader and "there is a store there and
            // it is not mine" is a fact about the workspace the user is looking at (§8's fix lines
            // already name the path to delete).
            reading.store = StoreState::Refused(reason.to_string().replace('\n', " "));
            return finish(reading, limit);
        }
    };
    reading.last_run = store::Runbook::read(&store.index).map(|book| book.last);

    let paths = row_files(&store.rows_dir());
    let rows = if paths.len() > ROW_CAP {
        reading.notice = Some(format!(
            "showing the first {ROW_CAP} of {} rows \u{b7} narrow the window with --from/--to",
            paths.len()
        ));
        &paths[..ROW_CAP]
    } else {
        &paths[..]
    };
    // One model per read: the glyph table and the per-column caveat lists come from it, and parsing
    // `schema/metric-model.json` once per row would be the frame-per-record mistake on the JSON side.
    let model = Model::embedded();
    let mut entries: Vec<Entry> = Vec::with_capacity(rows.len());
    for path in rows {
        if let Some(row) = store.read_json::<Row>(path) {
            entries.push(entry_of(&row, &model));
        }
    }
    reading.store = StoreState::Present {
        rows: entries.len(),
    };
    reading.entries = order(entries);
    finish(reading, limit)
}

/// Put the limit's hidden count on a reading whose rows are already ordered and delta-ed.
fn hide(mut reading: Reading, limit: Limit) -> Reading {
    let Some(limit) = limit else {
        return reading;
    };
    let shown = limit.get();
    if reading.entries.len() > shown {
        reading.hidden = reading.entries.len() - shown;
        reading.entries.truncate(shown);
    }
    reading
}

/// A branch name for the header, without assuming `HEAD` points at a branch.
fn branch_of(git: &Git) -> String {
    git.run(&["symbolic-ref", "--short", "HEAD"])
        .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_owned())
        .unwrap_or_else(|_| "(detached)".to_owned())
}

/// The row files of `index/rows/`, in a stable order.
fn row_files(directory: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = store::walk(directory)
        .into_iter()
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with(".json"))
        })
        .collect();
    paths.sort();
    paths
}

/// Newest first, and a parent always below its child.
///
/// The key is the one the store orders by too — committer instant, then SHA, so two commits in one second
/// still have exactly one order (§11.3's "every ordering in this store is pinned"). But an instant tie is
/// not rare (a rebased branch, a `fast-import`, two commits inside the same second), and sorting *alone*
/// puts a parent under its child whenever the child's SHA happens to sort first: a timeline where the
/// cause comes after the effect is a timeline nobody trusts.
///
/// So the sorted list is a priority, not the answer. The answer is a walk from the tips — the rows whose
/// first parent is not in this window — newest tip first, each followed down its first-parent chain. For
/// the ordinary linear case that reproduces the sort exactly; for a tie it produces git's own order, and
/// it costs one pass and a hash map. Anything the walk cannot reach (a parent that is absent from the
/// window *and* the store, which is the same thing a delta already reports as "no arrow") keeps its place
/// in the priority order at the end, because a row that cannot be placed must still be shown.
fn order(mut entries: Vec<Entry>) -> Vec<Entry> {
    entries.sort_by(|left, right| {
        right
            .sort_key()
            .cmp(&left.sort_key())
            .then_with(|| right.sha.cmp(&left.sha))
    });
    // Owned keys, so the map outlives the borrow of the vector it indexes: the walk needs both at once.
    let by_sha: std::collections::HashMap<String, usize> = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| (entry.sha.clone(), index))
        .collect();
    let parent_of = |entry: &Entry| {
        entry
            .parent
            .as_deref()
            .and_then(|parent| by_sha.get(parent).copied())
    };
    let mut slots: Vec<Option<Entry>> = entries.into_iter().map(Some).collect();
    let mut placed: Vec<Entry> = Vec::with_capacity(slots.len());
    // A tip is a commit that *nothing in this window* has as its first parent — the newest end of a
    // branch, which is exactly what a timeline is built from. The root commit is the least of the tips,
    // not the first: it is at the bottom of the screen.
    let parents: std::collections::HashSet<String> = slots
        .iter()
        .flatten()
        .filter_map(|entry| entry.parent.clone())
        .collect();
    for start in 0..slots.len() {
        let tip = slots[start]
            .as_ref()
            .is_some_and(|entry| !parents.contains(&entry.sha));
        if !tip {
            continue;
        }
        let mut cursor = Some(start);
        while let Some(index) = cursor {
            if slots[index].is_none() {
                break;
            }
            // `take` is the visited mark: a row is placed once, and a chain that runs into a row already
            // placed (the usual case where two branches share a history) stops there.
            let entry = slots[index].take().expect("just checked");
            cursor = parent_of(&entry);
            placed.push(entry);
        }
    }
    // Whatever the walks did not reach: a cycle (impossible in git, unworthy of a special case) or a row
    // whose parent is present but which is not on any first-parent chain from a tip.
    for slot in slots.iter_mut() {
        if let Some(entry) = slot.take() {
            placed.push(entry);
        }
    }
    placed
}

/// Fill in what only a whole snapshot can fill: the delta arrows, which need the parent's row.
///
/// ADR-0008 §6.3 is the rule and it is unforgiving: the parent is the **first** parent, a root has no
/// delta at all, and nothing is ever borrowed from a sibling or carried forward. A parent that is not
/// in this window (the walk's oldest commit, a shallow clone, a `--from` that cut it off) therefore
/// gets no arrow — which is the honest answer and the one the model's `parent_rules` demands.
/// Derive the parent-relative deltas over the *whole* window, then apply `--limit`.
///
/// The order matters and is the whole reason `hide` is a separate step: a delta needs the parent row
/// present, and a limit applied before this function would silently cost the oldest shown commit its
/// delta — the one row a reader is most likely to ask "changed compared to what?" about.
fn finish(mut reading: Reading, limit: Limit) -> Reading {
    let by_sha: std::collections::HashMap<String, usize> = reading
        .entries
        .iter()
        .enumerate()
        .map(|(index, entry)| (entry.sha.clone(), index))
        .collect();
    let parents: Vec<Option<usize>> = reading
        .entries
        .iter()
        .map(|entry| {
            entry
                .parent
                .as_ref()
                .and_then(|parent| by_sha.get(parent).copied())
        })
        .collect();
    let coverages: Vec<Option<f64>> = reading
        .entries
        .iter()
        .map(|entry| entry.coverage.value)
        .collect();
    let benches: Vec<(String, Option<f64>, bool)> = reading
        .entries
        .iter()
        .map(|entry| {
            (
                entry.bench.identity(),
                entry.bench.raw,
                entry.bench.comparable,
            )
        })
        .collect();
    for index in 0..reading.entries.len() {
        let Some(parent) = parents[index] else {
            continue;
        };
        let entry = &mut reading.entries[index];
        if let (Some(now), Some(before)) = (entry.coverage.value, coverages[parent]) {
            entry.coverage.delta = Some(delta_of(now, before));
        }
        if entry.bench.raw.is_some()
            && benches[parent].1.is_some()
            && benches[parent].2
            && benches[index].0 == benches[parent].0
        {
            entry.bench.delta = benches[index]
                .1
                .zip(benches[parent].1)
                .map(|(now, before)| delta_of(now, before));
        }
    }
    hide(reading, limit)
}

fn delta_of(now: f64, before: f64) -> Delta {
    super::entry::delta_of(now, before)
}

/// One stored row becomes one [`Entry`], field by field, and every absent section becomes an absent
/// value rather than a zero.
fn entry_of(row: &Row, model: &Model) -> Entry {
    let history = row.history.clone().unwrap_or(Value::Null);
    let evidence = row.evidence.clone().unwrap_or(Value::Null);
    let mi = row.mi.as_ref().map(|section| section.aggregate.clone());

    let subject = text(&history, "subject").unwrap_or_default();
    let author = text(&history, "author_name").unwrap_or_default();
    let committer_utc = number(&history, "committer_utc").map(|value| value as i64);

    // coverage: one pooled percent, and the gap code that says why there is none.
    let mut coverage = percent(&evidence);
    if let Some(gap) = gap_code(&evidence, "coverage") {
        coverage.gap = Some(gap);
    }
    let caveats: Vec<String> = string_list(&evidence, "caveats");
    coverage.caveat = !cells::caveats_for(model, Column::Coverage, &caveats).is_empty();

    let mi_value = mi
        .as_ref()
        .and_then(|value| value.get("aggregate"))
        .and_then(Value::as_f64);
    let mut mi_metric = match mi_value {
        Some(value) => Metric::measured(value),
        // No aggregate. `no_mi` is the store row's own gap (§6.4's `gaps`), and it is the only name
        // this build has for the absence: the metric model's gap vocabulary covers the two
        // evidence-backed columns and deliberately says nothing about the MI.
        None => Metric {
            gap: row.gaps.no_mi.then(|| "no_mi".to_owned()),
            ..Metric::UNKNOWN
        },
    };
    // ADR-0007 §4.4: the delta is the row's own `delta_vs_parent`, computed from two stored numbers by
    // the index. A root commit has none and gets no arrow.
    mi_metric.delta = mi
        .as_ref()
        .and_then(|value| value.get("delta_vs_parent"))
        .and_then(Value::as_f64)
        .map(|delta| match delta.partial_cmp(&0.0) {
            Some(std::cmp::Ordering::Greater) => Delta::Up,
            Some(std::cmp::Ordering::Less) => Delta::Down,
            _ => Delta::Flat,
        });
    let unanalysed = row.gaps.unanalysed_paths;
    mi_metric.caveat = unanalysed > 0;
    if mi_metric.value.is_none() {
        mi_metric.caveat = false;
    }

    Entry {
        sha: row.commit.clone(),
        parent: row.sources.git.parents.first().cloned(),
        author_utc: row.sources.git.author_utc,
        // Replaced by `with_committer` below; the row's history section is the only place the
        // committer clock is stored, and a row with none sorts by its author clock.
        committer_utc: row.sources.git.author_utc,
        subject,
        author,
        coverage,
        churn: churn_of(&history),
        mi: mi_metric,
        unanalysed_paths: unanalysed,
        bench: bench_of(&evidence, &caveats, model),
        // The panel's whole input, from the same section the timeline's one-cell summary is cut from.
        detail: bench::Commit::from_row(row),
        action: action_of(&evidence),
        indexed: true,
        caveats,
    }
    .with_committer(committer_utc)
}

/// A row's sort instant: the committer clock when the history section has one (the order git itself
/// prints), and the author clock otherwise.
impl Entry {
    fn with_committer(mut self, committer_utc: Option<i64>) -> Self {
        self.committer_utc = committer_utc.unwrap_or(self.author_utc);
        self
    }
}

fn churn_of(history: &Value) -> Churn {
    if history.is_null() {
        return Churn::UNKNOWN;
    }
    Churn {
        add: number(history, "n_add").map(|value| value as u64),
        del: number(history, "n_del").map(|value| value as u64),
        files: number(history, "n_files").map(|value| value as u64),
        noise_add: number(history, "noise_add").map_or(0, |value| value as u64),
        noise_del: number(history, "noise_del").map_or(0, |value| value as u64),
        truncated: history
            .get("files_truncated")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

fn percent(evidence: &Value) -> Metric {
    evidence
        .get("coverage")
        .and_then(|coverage| coverage.get("percent"))
        .and_then(Value::as_f64)
        .map_or_else(|| Metric::UNKNOWN, Metric::measured)
}

fn gap_code(evidence: &Value, column: &str) -> Option<String> {
    evidence
        .get("gaps")
        .and_then(|gaps| gaps.get(column))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn action_of(evidence: &Value) -> ActionState {
    match evidence
        .get("action_success")
        .and_then(Value::as_str)
        .unwrap_or(ActionState::NoEvidence.as_str())
    {
        "success" => ActionState::Success,
        "failed" => ActionState::Failed,
        "mixed" => ActionState::Mixed,
        _ => ActionState::NoEvidence,
    }
}

/// The §4.5 choice: sort by `(name, canon(labels))`, take the first that is comparable and not
/// context, else the first of any — and `+N` for every group behind it.
fn bench_of(evidence: &Value, caveats: &[String], model: &Model) -> Bench {
    let Some(groups) = evidence.get("benchmarks").and_then(Value::as_array) else {
        return Bench::missing(gap_code(evidence, "benchmarks").as_deref());
    };
    let mut ordered: Vec<&Value> = groups.iter().collect();
    ordered.sort_by_key(|group| {
        (
            text(group, "name").unwrap_or_default(),
            crate::metrics::row::canonical_json(&labels_of(group)),
        )
    });
    let chosen = ordered
        .iter()
        .copied()
        .find(|group| {
            group
                .get("unit")
                .and_then(Value::as_str)
                .is_some_and(|unit| !unit.is_empty())
                && !group
                    .get("context")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
        })
        .or_else(|| ordered.first().copied());
    let Some(group) = chosen else {
        return Bench::missing(gap_code(evidence, "benchmarks").as_deref());
    };
    let unit = text(group, "unit").unwrap_or_default();
    let value = group
        .get("value")
        .and_then(Value::as_f64)
        .map(|value| (super::entry::four_significant(value), value));
    let comparable = !unit.is_empty();
    let caveat = !cells::caveats_for(model, Column::Benchmarks, caveats).is_empty();
    let (text_value, raw) = match value {
        Some((digits, raw)) => (
            if unit.is_empty() {
                digits
            } else if unit.contains(' ') {
                format!("{digits} {unit}")
            } else {
                format!("{digits}{unit}")
            },
            Some(raw),
        ),
        None => (String::new(), None),
    };
    // A unitless group is displayable and never comparable (ADR-0008 §4.3), so it can carry a value
    // but must not carry an arrow.
    Bench {
        name: text(group, "name").unwrap_or_default(),
        labels: crate::metrics::row::canonical_json(&labels_of(group)),
        value: text_value,
        comparable,
        context: group
            .get("context")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        rest: groups.len().saturating_sub(1),
        caveat,
        delta: None,
        gap: None,
        raw: raw.filter(|_| comparable),
    }
}

fn labels_of(group: &Value) -> std::collections::BTreeMap<String, String> {
    group
        .get("labels")
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .map(|(key, value)| (key.clone(), value.as_str().unwrap_or_default().to_owned()))
                .collect()
        })
        .unwrap_or_default()
}

fn text(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn number(value: &Value, key: &str) -> Option<f64> {
    value.get(key).and_then(Value::as_f64)
}

/// A JSON array of strings as `Vec<String>`, and an empty one for anything else.
///
/// Total on purpose: a row whose `caveats` arrived as a string, a number or a missing key is a row with
/// no caveats as far as the screen is concerned, and the reader's own strictness (`metrics::row`) is
/// where a malformed row is actually refused. The front end's job is to draw what it was given.
fn string_list(value: &Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// `HEAD` has no row, so the store is behind the checkout.
///
/// Two states are *not* staleness, and conflating them is the difference between an actionable header and
/// a useless one: with no store at all the story is "nothing has been indexed" (press `R`), and with no
/// commits at all there is no `HEAD` to be behind. Staleness is the narrow, common, and genuinely
/// misleading case — there is a picture here, and it is not the current one.
#[must_use]
pub fn is_stale(reading: &Reading) -> bool {
    if !reading.store.is_present() {
        return false;
    }
    match &reading.head {
        None => false,
        Some(head) => !reading.entries.iter().any(|entry| &entry.sha == head),
    }
}

/// The `fathom index` this screen starts, as `fathom index` would have run it — the same trigger
/// spelling `run.json` already reserves for the TUI (ADR-0006 §6.2's `trigger` field).
pub const TRIGGER: &str = "tui";

/// One commit's patch, for the diff view — the second and last place this front end touches `git`.
///
/// It belongs here rather than in [`crate::diff`] because `crate::diff` is a reader that knows nothing
/// about a workspace: this is where the workspace's `git`, the resolved `fathom.toml` and the same
/// classifier that produced the timeline row's `+`/`−` column are put together and handed to it. That
/// is what makes the two panes agree: the file list the diff view shows is the history reader's answer
/// with the same configuration the index used.
///
/// # Errors
///
/// [`crate::diff::Error`] — the same refusals the reader makes, plus a broken `paths.*` glob.
pub fn diff(
    boot: &Boot,
    config: &Config,
    sha: &str,
    limits: diff::Limits,
) -> Result<CommitDiff, diff::Error> {
    let git = Git::open(&boot.workspace)?;
    let classifier = Classifier::try_from_config(config)?;
    diff::commit(&git, &classifier, sha, limits)
}

// ------------------------------------------------------------------ the commit detail view

/// One commit's detail snapshot — what `enter` opens, read from the store and from nothing else.
///
/// Three kinds of read, all small, all on the engine's thread:
///
/// ```text
/// index/rows/<xx>/<commit>.json        one row: history, evidence, coverage_files, mi
/// index/rows/<xx>/<parent>.json        its first parent, for the two delta columns
/// index/blocks/<xx>/<key>.json         one per file the commit touched, here and at the parent
/// ```
///
/// No `.evidence` walk and no `git`: the per-file coverage rows and the per-file MI are in the store
/// precisely so that opening this screen costs reads instead of derivations — mact-by1.14's "read from
/// the index, not recomputed per keypress", taken literally. A refusal is one line with a fix in it,
/// because an empty pane with no reason is indistinguishable from a commit that measured nothing.
///
/// # Errors
///
/// A sentence per refusal: no store, no row, a store this build will not open.
pub fn detail(boot: &Boot, sha: &str) -> Result<detail::Snapshot, String> {
    let store = match store::Store::open(&boot.cache.path) {
        Ok(None) => {
            return Err("nothing is indexed here — press R, or run `fathom index`".to_owned());
        }
        Ok(Some(store)) => store,
        Err(reason) => return Err(reason.to_string().replace('\n', " ")),
    };
    let row: Row = store.read_json(&store.row_path(sha)).ok_or_else(|| {
        format!(
            "{} has no row in this store — press R to index it, or widen --from/--to",
            detail::short(sha)
        )
    })?;
    let parent = row.sources.git.parents.first().cloned();
    let parent_row: Option<Row> = parent
        .as_deref()
        .and_then(|parent| store.read_json(&store.row_path(parent)));
    Ok(snapshot(&store, &row, parent_row.as_ref(), parent))
}

/// Assemble one commit's snapshot out of its row, its parent's row, and the cached MI blocks.
///
/// Pure with respect to the store: everything that reads is a `read_json` of a path the store already
/// owns, so the whole cost is bounded by the files the commit touched and is visible from here.
fn snapshot(
    store: &store::Store,
    row: &Row,
    parent: Option<&Row>,
    parent_sha: Option<String>,
) -> detail::Snapshot {
    let history = row.history.clone().unwrap_or(Value::Null);
    let evidence = row.evidence.clone().unwrap_or(Value::Null);
    let present = row.evidence.is_some();

    // --- the diffstat: the row's own per-file list, in git's order -------------------------------
    let mut diff: Vec<detail::DiffRow> = Vec::new();
    if let Some(files) = history.get("files").and_then(Value::as_array) {
        for file in files {
            let scope = text(file, "scope").unwrap_or_else(|| "source".to_owned());
            diff.push(detail::DiffRow {
                path: text(file, "path").unwrap_or_default(),
                old_path: text(file, "old_path"),
                kind: text(file, "kind").unwrap_or_default(),
                add: uint(file, "add"),
                del: uint(file, "del"),
                binary: file.get("binary").and_then(Value::as_bool).unwrap_or(false),
                noise: scope != "source",
                scope,
            });
        }
    }
    let filter = detail::Filter {
        source_files: uint(&history, "n_files"),
        noise_files: uint(&history, "noise_files"),
        noise_add: uint(&history, "noise_add"),
        noise_del: uint(&history, "noise_del"),
        excluded_files: uint(&history, "excluded_files"),
        truncated: history
            .get("files_truncated")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            > 0,
        // The reader always asks git for renames (`-M`), and the row's rename rows are the proof; the
        // footer says it anyway, because "which filter produced this list" should not need a guess.
        renames: true,
        add: uint(&history, "n_add"),
        del: uint(&history, "n_del"),
        add_all: uint(&history, "n_add_all"),
        del_all: uint(&history, "n_del_all"),
    };

    // --- coverage: the pooled number, its file records, and the runs that fed it -----------------
    let pooled = detail::pooled(if present { Some(&evidence) } else { None });
    let section = row.coverage_files.clone().unwrap_or_default();
    let touched: std::collections::BTreeSet<&str> =
        diff.iter().map(|file| file.path.as_str()).collect();
    // A rename means the coverage record and the diff may name either side of the move, so the match
    // is on the new path *or* the old one — otherwise `R` a file and its coverage row falls off the
    // screen, which is the ticket's "rename-in-coverage path matching".
    let renamed: std::collections::BTreeSet<&str> = diff
        .iter()
        .filter_map(|file| file.old_path.as_deref())
        .collect();
    let parent_files = parent
        .and_then(|parent| parent.coverage_files.clone())
        .unwrap_or_default();
    let files: Vec<detail::CoverageRow> = section
        .files
        .iter()
        .map(|file| {
            detail::coverage_row(
                file,
                parent_files
                    .files
                    .iter()
                    .find(|candidate| candidate.path == file.path)
                    .and_then(|candidate| candidate.percent),
                touched.contains(file.path.as_str()) || renamed.contains(file.path.as_str()),
            )
        })
        .collect();
    let runs = section
        .runs
        .iter()
        .map(|run| detail::RunLine {
            run_id: run.run_id.clone(),
            run_status: run.run_status.clone(),
            source: run.source.clone(),
            measure: run.measure.clone(),
            hit: run.hit,
            found: run.found,
            total_source: run.total_source.clone(),
            used: run.used,
        })
        .collect();
    let excluded = evidence_rows(&evidence, "excluded_runs")
        .iter()
        .filter_map(|run| {
            Some((
                text(run, "run_id")?.to_owned(),
                text(run, "status")?.to_owned(),
            ))
        })
        .collect();

    // --- MI: the touched files, their blocks, and the parent's numbers ---------------------------
    let blocks = row
        .mi
        .as_ref()
        .map(|mi| mi.blocks.clone())
        .unwrap_or_default();
    let parent_blocks = parent
        .and_then(|parent| parent.mi.as_ref())
        .map(|mi| mi.blocks.clone())
        .unwrap_or_default();
    let mut mi: Vec<detail::HotspotRow> = Vec::new();
    let mut mi_uncached = 0_usize;
    for (index, file) in diff.iter().enumerate() {
        // Past the cap the pane is a partial table and says so; nothing is dropped silently.
        if index >= detail::MI_INPUTS {
            mi_uncached += 1;
            continue;
        }
        let key = blocks
            .get(&file.path)
            .or_else(|| file.old_path.as_deref().and_then(|old| blocks.get(old)));
        let numbers = key
            .and_then(|key| block_numbers(store, key))
            .unwrap_or_else(|| detail::HotspotRow {
                path: file.path.clone(),
                // No block at all. The classifier's answer is the only reason there is one: a noise or
                // excluded path was never analysed on purpose, and a source path with no block is a
                // store that has not seen this file yet — two different sentences, and the second one
                // is the only one "press R" can fix.
                status: match file.scope.as_str() {
                    "noise" => "skipped_noise".to_owned(),
                    "excluded" => "excluded".to_owned(),
                    _ => "no_block".to_owned(),
                },
                mi: None,
                delta: None,
                loc: None,
                volume: None,
                cyclomatic: None,
                vocabulary: None,
                language: None,
            });
        let mut here = numbers;
        here.path = file.path.clone();
        // The parent's number for the same file: its own block, keyed on the parent's content. A
        // deleted file has no entry here at the parent and no entry there at all, and a rename is
        // looked up under the name it had then.
        if let Some(parent_key) = parent_blocks.get(&file.path).or_else(|| {
            file.old_path
                .as_deref()
                .and_then(|old| parent_blocks.get(old))
        }) {
            here.delta = block_mi(store, parent_key).and_then(|before| {
                here.mi
                    .map(|now| (now - before) * 10.0)
                    .map(|delta| delta / 10.0)
            });
        }
        mi.push(here);
    }

    let aggregate = row
        .mi
        .as_ref()
        .map(|mi| mi.aggregate.clone())
        .unwrap_or(Value::Null);
    let tree = row.sources.git.tree.clone();
    detail::Snapshot {
        sha: row.commit.clone(),
        subject: text(&history, "subject").unwrap_or_default(),
        parent: parent_sha.or_else(|| row.sources.git.parents.first().cloned()),
        tree,
        pooled,
        files,
        runs,
        diff,
        mi,
        analyzers: row
            .mi
            .as_ref()
            .map(|mi| mi.analyzers.clone())
            .unwrap_or_default(),
        aggregate: number(&aggregate, "aggregate"),
        aggregate_files: uint(&aggregate, "files"),
        aggregate_loc: uint(&aggregate, "loc"),
        unanalysed_paths: row.gaps.unanalysed_paths,
        caveats: string_list(&evidence, "caveats"),
        excluded,
        filter,
        mi_uncached,
        no_mi: row.gaps.no_mi,
        total_only: section.files.is_empty() && !section.runs.is_empty(),
    }
}

/// One cached `mi.score` block, as the MI pane needs it. Absent or unparseable is `None`: the pane
/// prints `no cached analysis` rather than a zero.
fn block_numbers(store: &store::Store, key: &str) -> Option<detail::HotspotRow> {
    let block: store::index::Block = store.read_json(&store.block_path(key))?;
    let result = &block.result;
    let number = |key: &str| result.get(key).and_then(Value::as_f64);
    Some(detail::HotspotRow {
        path: text(result, "path").unwrap_or_default(),
        status: text(result, "status").unwrap_or_else(|| "no_block".to_owned()),
        mi: number("mi"),
        delta: None,
        loc: number("loc").map(|value| value as u64),
        volume: number("halstead_volume"),
        cyclomatic: number("cyclomatic").map(|value| value as u64),
        vocabulary: number("vocabulary").map(|value| value as u64),
        language: text(result, "language"),
    })
}

/// Just one block's MI, for a delta that needs nothing else from the parent.
fn block_mi(store: &store::Store, key: &str) -> Option<f64> {
    let block: store::index::Block = store.read_json(&store.block_path(key))?;
    block.result.get("mi").and_then(Value::as_f64)
}

/// One array of objects from the stored evidence section.
fn evidence_rows(evidence: &Value, key: &str) -> Vec<Value> {
    evidence
        .get(key)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// A JSON number as a `u64`, for the counts this view only ever displays.
fn uint(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}
