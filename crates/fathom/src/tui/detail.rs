//! The commit detail view: `enter` on a commit, and the three answers a timeline row cannot give.
//!
//! A timeline row says `86.9% +412 -98 61.4` and stops. That is the right amount for a timeline and
//! exactly the wrong amount for the question a reader has when they stop scrolling: **what exactly got
//! worse?** This screen answers it in the three currencies a commit is measured in — the files a
//! coverage run measured, the files the maintainability index scored, the files the patch changed —
//! on one screen, with the same path highlighted in all three.
//!
//! ```text
//! ┌ coverage by file · worst first ────────────────────────────── 86.9% lines ─┐
//! │ file                        rate    lines hit/found   vs parent            │
//! │ src/gen/protobuf_out.rs      0.0%            0/512     ▼ 88.4              │
//! ┌ MI hotspots · worst first ────────────────────────────────── mi.score v1 ──┐
//! │ file                          MI    LOC     vs parent                      │
//! │ src/runner/run.rs           31.2   1024     ▼ 1.4                          │
//! ┌ diffstat · path order ────────────────────────────────── 2 noise files ────┐
//! │ file                                +a        -d                           │
//! │ crates/fathom/src/tui.rs          118         3                            │
//! └────────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! ## The rules this screen is made of
//!
//! * **Everything is read, nothing is recomputed.** Every number here came out of `.fathom` in one row
//!   read, one parent row, and one small cached block per file the commit touched
//!   ([`super::store_view::detail`], run on the engine's thread). Opening this screen never walks
//!   `.evidence`, never runs an analyzer and never calls `git`: the index did that once, and that is
//!   what makes it a key press rather than a pause.
//! * **The pooled number and its parts are shown together, and their disagreement is explained.** The
//!   run's total and the run's file records are *allowed* to differ — files instrumented outside the
//!   workspace are in the total and in no file record (ADR-0002 §4.2, ADR-0008 §3.3) — so the pane says
//!   which is which and by how much instead of letting a reader sum the rows and "find" a different
//!   number.
//! * **A file's rate is computed from its own counts, once** — the pooled rule one level down. No
//!   per-file percentage is ever averaged with another to make a number (ADR-0008 §3.1).
//! * **Unknown is not worst.** A file with no record, no MI, or no lines to cover (`vacuous`) sorts
//!   *below* the measured ones and prints `—`. Filing "not measured" under "worst" would be inventing
//!   a finding out of an absence.
//! * **Every pane says where its numbers came from**: run ids and `run_status` for coverage, analyzer
//!   ids and versions for MI, the filter for the diffstat. Those footers are the difference between a
//!   dashboard and a debugging tool — which is why `tests/tui_detail.rs` pins them.
//! * **A path that matches nothing is a row, not a hole.** The other panes say "no coverage record for
//!   this path here" rather than dropping the highlight, and a coverage path this commit did not touch
//!   carries a mark instead of disappearing.
//! * **A sort never reshuffles.** Every order is total, every tie is broken by path bytes, so two
//!   renders of one snapshot are one order forever.
//!
//! ## What this screen cannot say
//!
//! A coverage record names a *file*, not a *hunk*: nothing in `.evidence` attributes a missed line to
//! the lines this commit changed, so "the coverage of this commit's own changes" is not a number that
//! can honestly be printed here. What this screen does is put the measured files and the touched files
//! side by side and let the reader make the join — which is the whole point of one screen rather than
//! three commands.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use serde_json::Value;

use crate::store::row::FileCoverage;

use super::cells::Align;
use super::palette::Palette;
use super::plain::fit;

/// How many touched files get their MI inputs read.
///
/// One block read per file, and a 4 000-file commit would pay twice that (this commit and its parent)
/// to fill a column most readers never turn on. When the cap bites the pane says so — a partial table
/// that looks complete is the one failure this screen is not allowed.
pub const MI_INPUTS: usize = 256;

/// The detail screen's body height for a frame of `height` rows.
///
/// The frame spends one row on the heading, three on the status line (the footers are sentences, and the
/// caveats are the acceptance criterion — a two-row status cut the coverage bill mid-clause, which is the
/// one line a reader must not lose), one on the key line; what is left is split between the three panes by
/// [`Detail::layout`]. The drawing and the scroll clamps both need this number, which is why it is a
/// function and not a subtraction in two places (the diff view's [`super::diff_view::pane_height`] is the
/// same idea).
#[must_use]
pub const fn pane_height(height: u16) -> u16 {
    let body = height.saturating_sub(5);
    if body == 0 {
        1
    } else {
        body
    }
}

/// The three panes, in draw order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Pane {
    /// Coverage by file: the `scope:"file"` records of the selected runs.
    #[default]
    Coverage,
    /// The maintainability index, for the files this commit touched.
    Hotspots,
    /// The diffstat: per-file `+`/`−`, renames, binary marks, noise marks.
    Diffstat,
}

impl Pane {
    /// The pane's title, as its border prints it.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Coverage => "coverage by file",
            Self::Hotspots => "MI hotspots",
            Self::Diffstat => "diffstat",
        }
    }

    /// The next pane (`Tab`).
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Coverage => Self::Hotspots,
            Self::Hotspots => Self::Diffstat,
            Self::Diffstat => Self::Coverage,
        }
    }

    /// The previous pane (`Shift-Tab`).
    #[must_use]
    pub const fn previous(self) -> Self {
        match self {
            Self::Coverage => Self::Diffstat,
            Self::Hotspots => Self::Coverage,
            Self::Diffstat => Self::Hotspots,
        }
    }

    /// This pane's slot in the per-pane arrays.
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::Coverage => 0,
            Self::Hotspots => 1,
            Self::Diffstat => 2,
        }
    }

    /// All three, in draw order.
    pub const ALL: [Self; 3] = [Self::Coverage, Self::Hotspots, Self::Diffstat];
}

/// How a pane orders its rows. See [`Detail::order`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Sort {
    /// Worst first: the lowest rate, the lowest MI. Unknown sorts last, never first.
    #[default]
    Worst,
    /// Biggest first: most measured lines, most LOC, most churn.
    Size,
    /// The biggest fall against the parent first.
    Drop,
    /// Path order — the diffstat's default.
    Path,
}

impl Sort {
    /// The order this pane cycles to next (`s`).
    #[must_use]
    pub fn next(self, pane: Pane) -> Self {
        let order: &[Self] = match pane {
            Pane::Coverage | Pane::Hotspots => &[Self::Worst, Self::Size, Self::Drop],
            Pane::Diffstat => &[Self::Path, Self::Size, Self::Worst, Self::Drop],
        };
        let position = order.iter().position(|kind| *kind == self).unwrap_or(0);
        order[(position + 1) % order.len()]
    }

    /// The order that answers "what got worse" without being asked twice.
    #[must_use]
    pub const fn default_for(pane: Pane) -> Self {
        match pane {
            Pane::Coverage | Pane::Hotspots => Self::Worst,
            Pane::Diffstat => Self::Path,
        }
    }

    /// The pane's own words for this order, for its title.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Worst => "worst first",
            Self::Size => "biggest first",
            Self::Drop => "biggest drop first",
            Self::Path => "path order",
        }
    }
}

/// One file's coverage row — the coverage pane's unit.
#[derive(Debug, Clone, PartialEq)]
pub struct CoverageRow {
    /// Workspace-relative path, as the coverage record named it.
    pub path: String,
    /// This file's numerator.
    pub hit: u64,
    /// This file's denominator. `0` is vacuous: no `percent`, and not a `0 %` either.
    pub found: u64,
    /// The rate at the model's one decimal, absent when `found == 0`.
    pub percent: Option<f64>,
    /// The same path's rate at the first parent, absent when the parent had no record for it.
    pub parent: Option<f64>,
    /// `percent − parent`, in percent *points*. Absent when either side is.
    pub delta: Option<f64>,
    /// Whether this path is one of the files this commit changed. `false` is a file the run measured
    /// that this commit did not touch — everyday (a run measures the tree), marked rather than hidden.
    pub touched: bool,
    /// Which selected runs reported this file.
    pub runs: Vec<String>,
}

impl CoverageRow {
    /// The `rate` cell; `—` for a file with nothing to cover, which is not `0.0%`.
    #[must_use]
    pub fn rate(&self, palette: &Palette) -> String {
        self.percent.map_or_else(
            || palette.glyphs.unknown.clone(),
            |rate| format!("{rate:.1}%"),
        )
    }

    /// The `lines` cell: `3581/4120`, the counts the rate was computed from.
    #[must_use]
    pub fn counts(&self) -> String {
        format!("{}/{}", self.hit, self.found)
    }
}

/// One file's maintainability row, for a file this commit touched.
#[derive(Debug, Clone, PartialEq)]
pub struct HotspotRow {
    /// The path, as the diff named it.
    pub path: String,
    /// Which rung of ADR-0007's ladder this file landed on (`ok`, `unsupported_language`, …).
    pub status: String,
    /// The stored MI. Absent is not a zero.
    pub mi: Option<f64>,
    /// MI against the same path at the first parent, absent when the parent had no number for it.
    pub delta: Option<f64>,
    /// Logical lines — the weight the aggregate gave this file.
    pub loc: Option<u64>,
    /// Halstead volume `V`.
    pub volume: Option<f64>,
    /// Cyclomatic complexity `G`.
    pub cyclomatic: Option<u64>,
    /// Halstead vocabulary `n`.
    pub vocabulary: Option<u64>,
    /// The language the path selected, when one was selected at all.
    pub language: Option<String>,
}

impl HotspotRow {
    /// Whether this file has an MI at all.
    #[must_use]
    pub const fn measured(&self) -> bool {
        self.mi.is_some()
    }

    /// Why this file has no number, in words. Never empty for an unmeasured file: a `—` with no reason
    /// beside it is the silence the ticket calls "silently incomplete".
    #[must_use]
    pub fn gap(&self) -> String {
        match self.status.as_str() {
            "unsupported_language" => format!(
                "not analysed \u{b7} {} has no analyzer",
                self.language
                    .clone()
                    .unwrap_or_else(|| "this language".to_owned())
            ),
            "skipped_noise" => "not analysed \u{b7} `paths.noise`".to_owned(),
            "excluded" => "not analysed \u{b7} `paths.exclude`".to_owned(),
            "oversized" => "not analysed \u{b7} over `index.file_size_cap_mib`".to_owned(),
            "parse_error" => "not analysed \u{b7} would not lex".to_owned(),
            "vacuous" => "not analysed \u{b7} nothing to measure (0 LOC, or n < 2)".to_owned(),
            "no_block" => "no cached analysis for this file \u{b7} press R".to_owned(),
            other => format!("not analysed \u{b7} {other}"),
        }
    }

    /// The `V/G/LOC` cell, shown when `i` turns the inputs on: the answer to "why does this score 40".
    #[must_use]
    pub fn inputs(&self) -> String {
        let part = |label: &str, value: Option<f64>| match value {
            Some(value) if value.fract() == 0.0 => format!("{label}{}", value as i64),
            Some(value) => format!("{label}{value:.1}"),
            None => format!("{label}?"),
        };
        format!(
            "{} {} {}",
            part("V=", self.volume),
            part("G=", self.cyclomatic.map(|g| g as f64)),
            part("LOC=", self.loc.map(|loc| loc as f64)),
        )
    }
}

/// One file's diffstat row.
#[derive(Debug, Clone, PartialEq)]
pub struct DiffRow {
    /// The path git named (the new path, on a rename).
    pub path: String,
    /// The old path, for a rename or a copy.
    pub old_path: Option<String>,
    /// `add`, `modify`, `delete`, `rename`, `chdir`, …
    pub kind: String,
    /// Lines added. `0` for a binary file, whose count git does not give — which is what
    /// [`DiffRow::binary`] is for.
    pub add: u64,
    /// Lines deleted.
    pub del: u64,
    /// git's own mark, so `+0 -0` can be read as "no line counts" rather than "changed nothing".
    pub binary: bool,
    /// `source`, `noise` or `excluded` — the one classifier's answer (ADR-0007 §7.1).
    pub scope: String,
    /// Whether this path is in the noise-filtered subset the headline `+`/`−` leaves out.
    pub noise: bool,
}

impl DiffRow {
    /// The churn this row carried, for the size sort.
    #[must_use]
    pub const fn churn(&self) -> u64 {
        self.add + self.del
    }
}

/// The pooled coverage number — the coverage pane's header, shown once, not per row.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Pooled {
    /// The run's pooled percent, absent when the column carries a gap instead.
    pub percent: Option<f64>,
    /// Pooled numerator and denominator.
    pub hit: u64,
    /// Pooled denominator.
    pub found: u64,
    /// Which measure that is (`lines`, `statements`, …).
    pub measure: String,
    /// The gap code when there is no percent.
    pub gap: Option<String>,
    /// The selected runs whose totals were pooled.
    pub runs: Vec<String>,
    /// Non-primary measures the row carries, `measure percent` each.
    pub secondary: Vec<String>,
}

/// What the diffstat's filter did — the same question the diff view's `t` asks, answered in a footer.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Filter {
    /// Files in the headline set.
    pub source_files: u64,
    /// Files the noise/test filters left out of the headline, and their counts.
    pub noise_files: u64,
    /// Lines the noise subset added and deleted, which are in `add_all` and not in `add`.
    pub noise_add: u64,
    /// The deletion half of that pair.
    pub noise_del: u64,
    /// Files `paths.exclude` removed from the walk entirely.
    pub excluded_files: u64,
    /// Whether the per-file list itself was capped by the reader.
    pub truncated: bool,
    /// Whether git was asked to find renames.
    pub renames: bool,
    /// Headline totals, with and without the noise subset.
    pub add: u64,
    /// Headline deletions.
    pub del: u64,
    /// Totals over every file the commit touched, noise and excluded included.
    pub add_all: u64,
    /// The deletion half of the same pair.
    pub del_all: u64,
}

/// Everything one read of the store answered, before the view decides what to show.
///
/// Built by [`super::store_view::detail`] and nothing else: the row, its parent's row, and one cached
/// block per touched file. A value, so the screen can hold it and re-render from it forever without
/// touching a file again.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// The commit, full SHA.
    pub sha: String,
    /// Its message head.
    pub subject: String,
    /// The first parent, when the row names one.
    pub parent: Option<String>,
    /// The tree the MI was computed from, for the footer.
    pub tree: String,
    /// The pooled coverage number.
    pub pooled: Pooled,
    /// This commit's per-file coverage rows.
    pub files: Vec<CoverageRow>,
    /// The runs whose totals fed the pooled number, used and unused alike.
    pub runs: Vec<RunLine>,
    /// The files this commit changed: the diffstat pane's rows *and* the MI pane's file set, so the
    /// two panes' path columns mean one thing.
    pub diff: Vec<DiffRow>,
    /// This commit's analysed numbers for those files, in path order.
    pub mi: Vec<HotspotRow>,
    /// Analyzer ids and versions that produced the MI (ADR-0006 §8's staleness fields).
    pub analyzers: BTreeMap<String, u32>,
    /// The `mi` aggregate and what it was a LOC-weighted mean over.
    pub aggregate: Option<f64>,
    /// How many files that mean was taken over.
    pub aggregate_files: u64,
    /// How many LOC it divided by.
    pub aggregate_loc: u64,
    /// Paths seen and not measured, whatever the rung.
    pub unanalysed_paths: u64,
    /// The row's coverage caveat codes, in the reader's order.
    pub caveats: Vec<String>,
    /// Runs that happened here and were excluded, with the reason.
    pub excluded: Vec<(String, String)>,
    /// The diffstat filter's bill.
    pub filter: Filter,
    /// How many touched files were past [`MI_INPUTS`] — so the MI pane can say it is a partial table.
    pub mi_uncached: usize,
    /// The row's `gaps.no_mi`: "no MI column" and "an MI column of gaps" are different answers.
    pub no_mi: bool,
    /// The coverage total and the file records disagree, in the run's own numbers.
    pub total_only: bool,
}

/// One run's contribution to the coverage number, as the footer prints it.
#[derive(Debug, Clone, PartialEq)]
pub struct RunLine {
    /// Which run.
    pub run_id: String,
    /// Its `run_status`, verbatim — the audit trail's other half.
    pub run_status: String,
    /// `lcov`, `coverage_summary`, `log_scrape`.
    pub source: String,
    /// Which measure.
    pub measure: String,
    /// Its totals: numerator and denominator.
    pub hit: u64,
    /// The run's denominator. `0` is vacuous, and the pane says so.
    pub found: u64,
    /// Whether its total was the run's own `scope:"total"` record or the reader's sum of file records.
    pub total_source: String,
    /// Whether this run fed the pooled number at all.
    pub used: bool,
}

impl Snapshot {
    /// The name one file has in this snapshot, whichever name a pane's source used for it.
    ///
    /// A rename is what this exists for: the diff names the new path, a coverage record that predates
    /// the move may name the old one, and a highlight that falls off the screen because two sources
    /// disagreed about a filename is a highlight the reader stops trusting. The diff's rename rows are
    /// the only rename evidence a snapshot carries, so they are the map — and the rule is one sentence:
    /// two paths name the same file when they name the same file *now*.
    #[must_use]
    pub fn alias<'a>(&'a self, path: &'a str) -> &'a str {
        self.diff
            .iter()
            .find(|row| row.old_path.as_deref() == Some(path))
            .map_or(path, |row| row.path.as_str())
    }

    /// The pooled number as the pane's title prints it, gap included.
    #[must_use]
    pub fn pooled_text(&self, palette: &Palette) -> String {
        match self.pooled.percent {
            Some(percent) => format!("{percent:.1}% {}", self.pooled.measure),
            None => format!(
                "{} {}",
                self.pooled.gap.as_deref().unwrap_or("no coverage"),
                palette.glyphs.unknown
            ),
        }
    }

    /// The lines the coverage pane's empty state says, which is also where a gap code gets explained.
    #[must_use]
    pub fn coverage_empty(&self) -> Vec<String> {
        if !self.files.is_empty() {
            return Vec::new();
        }
        let mut out: Vec<String> = Vec::new();
        if let Some(gap) = &self.pooled.gap {
            out.push(format!("no per-file coverage records here \u{b7} {gap}"));
            out.extend(gap_note(gap));
            return out;
        }
        if self.pooled.percent.is_some() && self.total_only {
            return vec![
                "the run published a total and no per-file records \u{b7} the total is in the title \
                 above"
                    .to_owned(),
                "  a run that publishes only its summary has no per-file rows to list".to_owned(),
            ];
        }
        if self.pooled.percent.is_some() {
            return vec!["no per-file records, and no gap code either".to_owned()];
        }
        out
    }

    /// The lines the MI pane's empty state says.
    #[must_use]
    pub fn hotspots_empty(&self) -> Vec<String> {
        if !self.mi.is_empty() {
            return Vec::new();
        }
        if self.no_mi {
            return vec![
                "no maintainability number for this tree".to_owned(),
                "  a window root has no parent to diff against, and a tree with nothing in scope has \
                 nothing to score"
                    .to_owned(),
            ];
        }
        if self.diff.is_empty() {
            return vec!["this commit touched no files".to_owned()];
        }
        vec!["no cached analysis for any file this commit touched \u{b7} press R".to_owned()]
    }

    /// The lines the diffstat pane's empty state says.
    #[must_use]
    pub fn diffstat_empty(&self) -> Vec<String> {
        if !self.diff.is_empty() {
            return Vec::new();
        }
        vec![
            "this commit has no files against its first parent".to_owned(),
            "  a merge that brings nothing its first parent did not have, or a commit made with \
             --allow-empty"
                .to_owned(),
        ]
    }

    /// The pane's empty-state lines.
    #[must_use]
    pub fn empty(&self, pane: Pane) -> Vec<String> {
        match pane {
            Pane::Coverage => self.coverage_empty(),
            Pane::Hotspots => self.hotspots_empty(),
            Pane::Diffstat => self.diffstat_empty(),
        }
    }
}

/// One rendered row: its cells, and whether it carries the highlighted path.
#[derive(Debug, Clone)]
pub struct Row {
    /// The four cells, already fitted to their columns.
    pub cells: [String; 4],
    /// Whether this row is the one the cross-pane selection points at.
    pub marked: bool,
    /// Whether this is the focused pane's cursor row — never true in a pane the focus is not in.
    pub cursor: bool,
}

/// One pane, ready to draw: title, column row, rows, footer.
#[derive(Debug, Clone)]
pub struct Frame {
    /// Which pane this is.
    pub pane: Pane,
    /// The border title, sort and provenance token already in it.
    pub title: String,
    /// The right half of the border title (the pooled number, the analyzer stamp, the filter).
    pub aside: String,
    /// The column titles.
    pub columns: [String; 4],
    /// The rows, in the pane's order. Empty panes carry their empty-state text in cell 0.
    pub rows: Vec<Row>,
    /// The pane's footer: the provenance line, always drawn.
    pub footer: String,
    /// Whether the pane has no rows of its own (so the drawing may dim the empty state).
    pub empty: bool,
}

/// The detail screen's state: three panes, one shared path, three sorts, no I/O.
///
/// A pure state machine, in the style of [`super::state::App`] and [`super::bench::Panel`]: keys in,
/// field writes out, snapshot handed in whole by the engine like a diff is.
#[derive(Debug, Clone)]
pub struct Detail {
    /// The commit this screen is about, full SHA.
    pub sha: String,
    /// A read is in flight (or was never started).
    pub loading: bool,
    /// The read's refusal, when it failed.
    pub error: Option<String>,
    /// What the store answered, once it has.
    pub snapshot: Option<Snapshot>,
    /// Which pane the keys act on (`Tab` cycles it).
    pub focus: Pane,
    /// Whether the MI pane shows `V`/`G`/`LOC` (`i`).
    pub show_inputs: bool,
    /// Row cursor per pane, in that pane's ordered row list.
    pub cursor: [usize; 3],
    /// First drawn row per pane.
    pub offset: [usize; 3],
    /// Rows each pane may draw, from the last [`Detail::set_height`].
    pub heights: [u16; 3],
    /// The whole body height those heights were derived from, kept so a snapshot that arrives later can
    /// be laid out the same way without the frame having to tell anyone the terminal's size again.
    pub body: u16,
    /// Sort per pane (`s` cycles the focused one).
    pub sorts: [Sort; 3],
    /// The `App` cache's recency counter, so detail screens evict the way diff views do.
    pub touched: u64,
}

impl Detail {
    /// A screen for a commit whose snapshot has not been read yet.
    #[must_use]
    pub fn open(sha: &str) -> Self {
        Self {
            sha: sha.to_owned(),
            loading: true,
            error: None,
            snapshot: None,
            focus: Pane::default(),
            show_inputs: false,
            cursor: [0; 3],
            offset: [0; 3],
            heights: [3; 3],
            body: 9,
            sorts: [
                Sort::default_for(Pane::Coverage),
                Sort::default_for(Pane::Hotspots),
                Sort::default_for(Pane::Diffstat),
            ],
            touched: 0,
        }
    }

    /// A screen that already has its snapshot — `--detail REV`, and what the engine hands back.
    #[must_use]
    pub fn read(snapshot: Snapshot) -> Self {
        let mut detail = Self::open(&snapshot.sha);
        detail.loading = false;
        detail.snapshot = Some(snapshot);
        detail
    }

    /// A screen that is a refusal, so the frame still draws something with a `fix:` in it.
    #[must_use]
    pub fn refusal(sha: &str, reason: String) -> Self {
        let mut detail = Self::open(sha);
        detail.loading = false;
        detail.error = Some(reason);
        detail
    }

    /// Focus another pane, wrapping (`Tab`, `Shift-Tab`, `n`/`p`).
    ///
    /// A ring, not a clamp: three panes is a cycle, and the reader at the first pane who presses
    /// `Shift-Tab` means "the one before this one", not "nothing happens". The sign of `step` is the
    /// direction, which is why it is not taken apart into a magnitude here.
    pub fn cycle_pane(&mut self, step: isize) {
        let panes = i64::try_from(Pane::ALL.len()).unwrap_or(3);
        let index = i64::try_from(self.focus.index()).unwrap_or(0);
        let step = i64::try_from(step).unwrap_or(0);
        let wrapped = (index + step).rem_euclid(panes);
        let pane = usize::try_from(wrapped)
            .ok()
            .and_then(|wrapped| Pane::ALL.get(wrapped).copied())
            .unwrap_or(Pane::Coverage);
        self.focus_pane(pane);
    }

    /// Take a snapshot the engine handed back.
    pub fn set_snapshot(&mut self, snapshot: Snapshot) {
        self.loading = false;
        self.error = None;
        let same = self
            .snapshot
            .as_ref()
            .is_some_and(|current| current.sha == snapshot.sha);
        self.snapshot = Some(snapshot);
        // A fresh snapshot for the same commit keeps the reader's place; a different commit is a
        // different screen and starts at the top of each pane.
        if !same {
            self.cursor = [0; 3];
            self.offset = [0; 3];
        }
        let body = self.body;
        self.set_height(body);
    }

    /// Record a refusal, so the panes say what failed and what to press instead of drawing nothing.
    pub fn set_error(&mut self, reason: String) {
        self.loading = false;
        self.error = Some(reason);
    }

    /// Show or hide the MI pane's `V`/`G`/`LOC` inputs (`i`).
    pub fn toggle_inputs(&mut self) {
        self.set_show_inputs(!self.show_inputs);
    }

    /// The rows to draw when there is no snapshot: the refusal, or what a read is waiting for.
    ///
    /// A loading screen and a refused one both have to say something, because a blank three-pane frame is
    /// how "this commit has no data" and "the read is slow" look identical.
    #[must_use]
    pub fn notice(&self) -> Vec<String> {
        if let Some(reason) = &self.error {
            let mut lines = vec![reason.clone()];
            lines.push(String::new());
            lines.push(
                "esc goes back to the timeline \u{b7} r re-reads the store \u{b7} R indexes it"
                    .to_owned(),
            );
            return lines;
        }
        if self.loading {
            return vec![
                "reading this commit \u{2026}".to_owned(),
                String::new(),
                "one row, its parent, and the cached analysis of each file it touched".to_owned(),
            ];
        }
        vec!["nothing to show".to_owned()]
    }

    /// The frame's rows each pane may draw, given the whole body height.
    ///
    /// Three panes share one column of rows. Each gets at least three (a border and one line of
    /// content); what is left goes, round-robin, to the panes that have more to show, and any slack is
    /// spread evenly. A function of the data and nothing else, so a three-pane layout snapshot test is
    /// a test of a layout and not of a race.
    #[must_use]
    pub fn layout(height: u16, need: [usize; 3]) -> [u16; 3] {
        let total = usize::from(height);
        let floor = 3_usize;
        if total < floor * 3 {
            // Too short to be readable, but never unreadable-on-purpose: every pane gets a row (a pane
            // with none is a pane that is not on screen), and what little is left goes to the panes with
            // the most to show rather than to whichever pane is drawn first.
            let mut rows = [0_usize; 3];
            let mut left = total;
            for row in &mut rows {
                if left == 0 {
                    break;
                }
                *row = 1;
                left -= 1;
            }
            let mut order: Vec<usize> = (0..3).collect();
            order.sort_by(|first, second| need[*second].cmp(&need[*first]));
            while left > 0 {
                for index in &order {
                    if left == 0 {
                        break;
                    }
                    rows[*index] += 1;
                    left -= 1;
                }
            }
            return rows.map(|rows| u16::try_from(rows).unwrap_or(u16::MAX));
        }
        let mut rows = [floor; 3];
        // What a pane wants: its rows, its column-title row, its footer, two border rows.
        let want: Vec<usize> = need.iter().map(|rows| rows + 4).collect();
        let mut left = total - floor * 3;
        while left > 0 {
            let mut moved = false;
            for index in 0..3 {
                if left == 0 {
                    break;
                }
                if rows[index] < want[index] {
                    rows[index] += 1;
                    left -= 1;
                    moved = true;
                }
            }
            if !moved {
                // Everything with rows has all of them; the slack is spread so the panes stay even.
                for row in &mut rows {
                    if left == 0 {
                        break;
                    }
                    *row += 1;
                    left -= 1;
                }
            }
        }
        rows.map(|rows| u16::try_from(rows).unwrap_or(u16::MAX))
    }

    /// Remember the frame's layout, clamping every pane's scroll to what it can now draw.
    pub fn set_height(&mut self, height: u16) {
        self.body = height;
        self.heights = Self::layout(height, self.needs());
        self.clamp_view();
    }

    /// Rows each pane would draw, plus one for its empty-state line so no pane is ever allocated zero.
    ///
    /// An empty pane is not a pane that needs nothing: it has an explanation to print and, when a file
    /// is selected in one of the other two, a line saying this pane does not know it. Those lines are
    /// the reason the pane is drawn at all, so they are what it asks the layout for.
    #[must_use]
    pub fn needs(&self) -> [usize; 3] {
        Pane::ALL.map(|pane| match self.count(pane) {
            0 => self
                .snapshot
                .as_ref()
                .map_or(2, |snapshot| snapshot.empty(pane).len() + 1),
            rows => rows,
        })
    }

    /// The pane's rows, as indices into its own list, in the order the current sort says.
    ///
    /// Total, with path bytes as the final tie-break, and with `None` (unknown) placed last in every
    /// ascending order: "worst first" is a claim about measurements, and an absence is not one.
    #[must_use]
    pub fn order(&self, pane: Pane) -> Vec<usize> {
        let Some(snapshot) = self.snapshot.as_ref() else {
            return Vec::new();
        };
        let sort = self.sorts[pane.index()];
        match pane {
            Pane::Coverage => {
                let mut order: Vec<usize> = (0..snapshot.files.len()).collect();
                order.sort_by(|left, right| {
                    let (left, right) = (&snapshot.files[*left], &snapshot.files[*right]);
                    match sort {
                        Sort::Worst => none_last(left.percent, right.percent)
                            .then_with(|| right.found.cmp(&left.found))
                            .then_with(|| left.path.cmp(&right.path)),
                        Sort::Drop => none_last(
                            left.delta.map(|delta| -delta),
                            right.delta.map(|delta| -delta),
                        )
                        .then_with(|| left.path.cmp(&right.path)),
                        Sort::Size => right
                            .found
                            .cmp(&left.found)
                            .then_with(|| left.path.cmp(&right.path)),
                        Sort::Path => left.path.cmp(&right.path),
                    }
                });
                order
            }
            Pane::Hotspots => {
                let mut order: Vec<usize> = (0..snapshot.mi.len()).collect();
                order.sort_by(|left, right| {
                    let (left, right) = (&snapshot.mi[*left], &snapshot.mi[*right]);
                    match sort {
                        Sort::Worst => none_last(left.mi, right.mi)
                            .then_with(|| loc(right).cmp(&loc(left)))
                            .then_with(|| left.path.cmp(&right.path)),
                        Sort::Drop => none_last(
                            left.delta.map(|delta| -delta),
                            right.delta.map(|delta| -delta),
                        )
                        .then_with(|| left.path.cmp(&right.path)),
                        Sort::Size => loc(right)
                            .cmp(&loc(left))
                            .then_with(|| left.path.cmp(&right.path)),
                        Sort::Path => left.path.cmp(&right.path),
                    }
                });
                order
            }
            Pane::Diffstat => {
                let mut order: Vec<usize> = (0..snapshot.diff.len()).collect();
                order.sort_by(|left, right| {
                    let (left, right) = (&snapshot.diff[*left], &snapshot.diff[*right]);
                    match sort {
                        // The diffstat has no "worst": biggest churn is the closest thing to it, and
                        // `Path` is the default, which is git's own order already.
                        Sort::Path => left.path.cmp(&right.path),
                        Sort::Worst => right
                            .churn()
                            .cmp(&left.churn())
                            .then_with(|| left.path.cmp(&right.path)),
                        Sort::Size => right
                            .churn()
                            .cmp(&left.churn())
                            .then_with(|| left.path.cmp(&right.path)),
                        Sort::Drop => right
                            .del
                            .cmp(&left.del)
                            .then_with(|| left.path.cmp(&right.path)),
                    }
                });
                order
            }
        }
    }

    /// How many rows a pane has.
    #[must_use]
    pub fn count(&self, pane: Pane) -> usize {
        let Some(snapshot) = self.snapshot.as_ref() else {
            return 0;
        };
        match pane {
            Pane::Coverage => snapshot.files.len(),
            Pane::Hotspots => snapshot.mi.len(),
            Pane::Diffstat => snapshot.diff.len(),
        }
    }

    /// Move the focused pane's cursor by `step`, clamped at both ends (never wrapping: a cursor that
    /// wraps makes you count presses to know where you are).
    pub fn move_cursor(&mut self, step: isize) {
        let index = self.focus.index();
        let last = self.order(self.focus).len().saturating_sub(1);
        self.cursor[index] = if step < 0 {
            self.cursor[index].saturating_sub(usize::try_from(-step).unwrap_or(usize::MAX))
        } else {
            self.cursor[index]
                .saturating_add(usize::try_from(step).unwrap_or(usize::MAX))
                .min(last)
        };
        self.clamp_view();
    }

    /// First or last row of the focused pane.
    pub fn jump(&mut self, to_end: bool) {
        let index = self.focus.index();
        let last = self.order(self.focus).len().saturating_sub(1);
        self.cursor[index] = if to_end { last } else { 0 };
        self.clamp_view();
    }

    /// Cycle the focus (`Tab` / `Shift-Tab`), parking the new pane's cursor on the shared path when it
    /// has one. That is what makes cross-pane selection *useful*: the cursor lands on the file you were
    /// looking at, not at the top of the pane you just reached.
    pub fn focus_pane(&mut self, pane: Pane) {
        self.focus = pane;
        if let Some(path) = self.path() {
            if let Some(row) = self.row_for(pane, &path) {
                self.cursor[pane.index()] = row;
            }
        }
        self.clamp_view();
    }

    /// Cycle the focused pane's sort (`s`).
    pub fn cycle_sort(&mut self) {
        let pane = self.focus;
        self.sorts[pane.index()] = self.sorts[pane.index()].next(pane);
        self.cursor[pane.index()] = 0;
        self.clamp_view();
    }

    /// Turn the MI inputs on or off (`i`).
    pub fn set_show_inputs(&mut self, show: bool) {
        self.show_inputs = show;
    }

    /// The focused pane's cursor index, clamped to what is there.
    #[must_use]
    pub fn cursor_index(&self, pane: Pane) -> usize {
        let last = self.order(pane).len().saturating_sub(1);
        self.cursor[pane.index()].min(last)
    }

    /// The path the cursor is on, in whichever pane is focused — the shared cross-pane selection.
    ///
    /// Resolved through [`Detail::order`], because the cursor is a position in the *drawn* list and the
    /// snapshot's vector is in whatever order the reader's store handed it in. Indexing the vector
    /// directly would make the highlight depend on the sort, which is the one thing a shared selection
    /// must never do: the file you pointed at is the file the other two panes light up, `s` or no `s`.
    #[must_use]
    pub fn path(&self) -> Option<String> {
        let snapshot = self.snapshot.as_ref()?;
        let row = self
            .order(self.focus)
            .into_iter()
            .nth(self.cursor_index(self.focus))?;
        Some(match self.focus {
            Pane::Coverage => snapshot.files.get(row)?.path.clone(),
            Pane::Hotspots => snapshot.mi.get(row)?.path.clone(),
            Pane::Diffstat => snapshot.diff.get(row)?.path.clone(),
        })
    }

    /// Where `path` sits in one pane's ordered rows, if that pane has it.
    #[must_use]
    pub fn row_for(&self, pane: Pane, path: &str) -> Option<usize> {
        let snapshot = self.snapshot.as_ref()?;
        self.order(pane).into_iter().position(|index| {
            let candidate = match pane {
                Pane::Coverage => snapshot.files.get(index).map(|row| &row.path),
                Pane::Hotspots => snapshot.mi.get(index).map(|row| &row.path),
                Pane::Diffstat => snapshot.diff.get(index).map(|row| &row.path),
            };
            candidate.is_some_and(|candidate| self.same(candidate, path))
        })
    }

    /// Whether two cells of two panes are the same file — equal, or two names for one rename
    /// ([`Snapshot::alias`], which is the whole of "rename-in-coverage path matching").
    #[must_use]
    pub fn same(&self, left: &str, right: &str) -> bool {
        left == right
            || self
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.alias(left) == snapshot.alias(right))
    }

    /// The pane's title: its name, its order, and one provenance token, with the pooled number (or
    /// the analyzer stamp, or the filter bill) on the right of the border.
    #[must_use]
    pub fn title(&self, pane: Pane, palette: &Palette) -> (String, String) {
        let sort = self.sorts[pane.index()];
        let left = format!(" {} \u{b7} {} ", pane.title(), sort.label());
        let Some(snapshot) = self.snapshot.as_ref() else {
            let right = self
                .error
                .clone()
                .unwrap_or_else(|| format!("reading {} \u{2026}", short(&self.sha)));
            return (left, right);
        };
        let right = match pane {
            Pane::Coverage => format!(
                " {} \u{b7} {} file(s) ",
                snapshot.pooled_text(palette),
                snapshot.files.len()
            ),
            Pane::Hotspots => format!(
                " {} \u{b7} {} file(s) ",
                self.analyzer_stamp(snapshot),
                snapshot.mi.len()
            ),
            Pane::Diffstat => format!(
                " +{} -{} \u{b7} {} file(s) ",
                snapshot.filter.add,
                snapshot.filter.del,
                snapshot.diff.len()
            ),
        };
        (left, right)
    }

    fn analyzer_stamp(&self, snapshot: &Snapshot) -> String {
        let named: Vec<String> = snapshot
            .analyzers
            .iter()
            .map(|(id, version)| format!("{id} v{version}"))
            .collect();
        if named.is_empty() {
            "no analyzer stamp".to_owned()
        } else {
            named.join(",")
        }
    }

    /// The pane's column titles.
    #[must_use]
    pub fn columns(&self, pane: Pane) -> [String; 4] {
        match pane {
            Pane::Coverage => ["file", "rate", "lines hit/found", "vs parent"],
            Pane::Hotspots if self.show_inputs => ["file", "MI", "V/G/LOC", "vs parent"],
            Pane::Hotspots => ["file", "MI", "LOC", "vs parent"],
            Pane::Diffstat => ["file", "kind", "+a", "-d"],
        }
        .map(str::to_owned)
    }

    /// One pane, rendered: title, columns, rows, footer. Everything [`super::render`] and
    /// [`super::plain`] need, decided here so neither of them contains an `if` about a measurement.
    #[must_use]
    pub fn frame(&self, pane: Pane, width: usize, palette: &Palette) -> Frame {
        let (title, aside) = self.title(pane, palette);
        let columns = self.columns(pane);
        let footer = self.footer(pane, palette);
        let Some(snapshot) = self.snapshot.as_ref() else {
            return Frame {
                pane,
                title,
                aside,
                columns,
                rows: Vec::new(),
                footer,
                empty: true,
            };
        };
        let path = self.path();
        let order = self.order(pane);
        let mut cells: Vec<[String; 4]> = Vec::with_capacity(order.len());
        let mut marked: Vec<bool> = Vec::with_capacity(order.len());
        for (position, index) in order.into_iter().enumerate() {
            let (row, row_path) = match pane {
                Pane::Coverage => {
                    let row = &snapshot.files[index];
                    (
                        [
                            row_mark(row) + &row.path,
                            row.rate(palette),
                            row.counts(),
                            self.delta_cell(row.delta, palette),
                        ],
                        row.path.clone(),
                    )
                }
                Pane::Hotspots => {
                    let row = &snapshot.mi[index];
                    let number = row
                        .mi
                        .map_or_else(|| palette.glyphs.unknown.clone(), |mi| format!("{mi:.1}"));
                    let second = if self.show_inputs {
                        if row.measured() {
                            row.inputs()
                        } else {
                            row.gap()
                        }
                    } else {
                        row.loc.map_or_else(|| row.gap(), |loc| loc.to_string())
                    };
                    (
                        [
                            row.path.clone(),
                            number,
                            second,
                            self.delta_cell(row.delta, palette),
                        ],
                        row.path.clone(),
                    )
                }
                Pane::Diffstat => {
                    let row = &snapshot.diff[index];
                    let path = match &row.old_path {
                        Some(old) => format!("{old} \u{2192} {}", row.path),
                        None => row.path.clone(),
                    };
                    let (add, del) = if row.binary {
                        ("[binary]".to_owned(), String::new())
                    } else {
                        (format!("+{}", row.add), format!("-{}", row.del))
                    };
                    (
                        [
                            if row.noise {
                                format!("~{path}")
                            } else {
                                path.clone()
                            },
                            row.kind.clone(),
                            add,
                            del,
                        ],
                        row.path.clone(),
                    )
                }
            };
            marked.push(
                path.as_deref()
                    .is_some_and(|want| self.same(want, &row_path)),
            );
            cells.push(row);
            let _ = position;
        }
        // A selected path this pane does not know is said here rather than dropped: the reader's
        // highlight must never vanish without a sentence explaining where it went. It goes *after* the
        // empty state, so a pane with nothing to say and nothing to highlight says both.
        if cells.is_empty() {
            for line in snapshot.empty(pane) {
                cells.push([line, String::new(), String::new(), String::new()]);
                marked.push(false);
            }
        }
        if !marked.iter().any(|mark| *mark) {
            let note = match (pane, &path) {
                (Pane::Coverage, Some(path)) => {
                    format!("no coverage record for {} here", short(path))
                }
                (Pane::Hotspots, Some(path)) => {
                    format!("no MI entry for {} here", short(path))
                }
                (Pane::Diffstat, Some(path)) => {
                    format!("this commit did not change {}", short(path))
                }
                (pane, None) => (match pane {
                    Pane::Coverage => "no file to select here",
                    Pane::Hotspots => "no file to select here",
                    Pane::Diffstat => "no file to select here",
                })
                .to_owned(),
            };
            cells.push([note.to_owned(), String::new(), String::new(), String::new()]);
            marked.push(true);
        }
        let empty = self.count(pane) == 0;
        let focused = self.focus == pane;
        // Column widths, sized from the titles *and* every cell, so a pane's columns line up and the
        // path column — the only cell a reader recognises from its beginning — gets what is left.
        let widths = widths(&columns, &cells, width);
        // An empty pane's only text is its explanation, and an explanation truncated at 60 cells is a
        // sentence with no verb in it. With no data to make room for, the whole inner width belongs to
        // the line that says why there is no data — and the note is what tells the reader what to run.
        let row_widths = if empty {
            [width.saturating_sub(3), 1, 1, 1]
        } else {
            widths
        };
        let rows: Vec<Row> = cells
            .iter()
            .enumerate()
            .map(|(position, cells)| Row {
                cells: cells.clone(),
                marked: marked[position],
                // One cursor, drawn in one pane: the other two show the shared file mark instead, so a
                // reader never has to guess which pane the arrow keys are moving.
                cursor: focused && !empty && position == self.cursor_index(pane),
            })
            .collect();
        Frame {
            pane,
            title,
            aside,
            columns: layout_cells(&columns, &widths, palette.ellipsis()),
            rows: window(
                rows,
                self.offset[pane.index()],
                usize::from(self.heights[pane.index()].saturating_sub(4)).max(1),
                palette.ellipsis(),
                &row_widths,
            ),
            footer,
            empty,
        }
    }

    /// The `vs parent` cell: an arrow, the movement, and — when there is nothing to compare — the
    /// reason, not a bare dash.
    #[must_use]
    pub fn delta_cell(&self, delta: Option<f64>, palette: &Palette) -> String {
        let glyphs = &palette.glyphs;
        let Some(parent) = self
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.parent.clone())
        else {
            return "no parent".to_owned();
        };
        match delta {
            None => format!("{} no record at {}", glyphs.unknown, short(&parent)),
            Some(delta) if delta.abs() < 0.05 => format!("{} 0.0", glyphs.delta_flat),
            Some(delta) => {
                let (arrow, magnitude) = if delta > 0.0 {
                    (&glyphs.delta_up, delta)
                } else {
                    (&glyphs.delta_down, -delta)
                };
                format!("{arrow} {magnitude:.1}")
            }
        }
    }

    /// The pane's footer: the "why is this number what it is" line.
    #[must_use]
    pub fn footer(&self, pane: Pane, palette: &Palette) -> String {
        let _ = palette;
        let Some(snapshot) = self.snapshot.as_ref() else {
            return match (&self.error, self.loading) {
                (Some(reason), _) => reason.clone(),
                (None, true) => format!(
                    "reading {} \u{b7} from the index, not recomputed",
                    short(&self.sha)
                ),
                (None, false) => String::new(),
            };
        };
        match pane {
            Pane::Coverage => {
                let mut parts: Vec<String> = Vec::new();
                // The caveats come first, not last. A footer that buries "this number disagrees with the
                // file records it is pooled over" behind a list of run ids is a footer whose most
                // important sentence is the one a narrow terminal cuts.
                if !snapshot.caveats.is_empty() {
                    parts.push(format!("caveats {}", snapshot.caveats.join(",")));
                }
                let note = self.outside_workspace_note(snapshot);
                if !note.is_empty() {
                    parts.push(note);
                }
                if snapshot.runs.is_empty() {
                    parts.push(if snapshot.pooled.runs.is_empty() {
                        "no run fed a coverage number here".to_owned()
                    } else {
                        format!("runs {}", snapshot.pooled.runs.join(","))
                    });
                }
                for run in &snapshot.runs {
                    // Terse on purpose: this line competes with the caveats for the same three rows.
                    // `summed` marks a run whose total this build added up from its file records rather
                    // than reading from the run's own `scope:"total"` record (ADR-0008 3.3).
                    parts.push(format!(
                        "{} {} {}/{}{}{}",
                        run.run_id,
                        run.run_status,
                        run.hit,
                        run.found,
                        if run.used { "" } else { " unused" },
                        if run.total_source == "sum_of_files" {
                            " summed"
                        } else {
                            ""
                        }
                    ));
                }
                if !snapshot.excluded.is_empty() {
                    let named: Vec<String> = snapshot
                        .excluded
                        .iter()
                        .map(|(run, status)| format!("{run} {status}"))
                        .collect();
                    parts.push(format!("excluded by retention: {}", named.join(", ")));
                }
                if !snapshot.pooled.secondary.is_empty() {
                    parts.push(format!(
                        "not pooled with this measure: {}",
                        snapshot.pooled.secondary.join(", ")
                    ));
                }
                parts.join("  \u{b7}  ")
            }
            Pane::Hotspots => {
                let mut parts: Vec<String> = Vec::new();
                parts.push(if snapshot.analyzers.is_empty() {
                    "no analyzer version was recorded on this row".to_owned()
                } else {
                    format!("analyzers {}", self.analyzer_stamp(snapshot))
                });
                parts.push(match snapshot.aggregate {
                    Some(aggregate) => format!(
                        "aggregate {aggregate:.1} is the LOC-weighted mean over {} file(s) / {} LOC",
                        snapshot.aggregate_files, snapshot.aggregate_loc
                    ),
                    None => "this tree has no aggregate".to_owned(),
                });
                if snapshot.unanalysed_paths > 0 {
                    parts.push(format!(
                        "{} path(s) in the tree were not analysed",
                        snapshot.unanalysed_paths
                    ));
                }
                if snapshot.mi_uncached > 0 {
                    parts.push(format!(
                        "V/G/LOC was read for the first {MI_INPUTS} file(s); {} more were not \
                         \u{b7} this table is partial",
                        snapshot.mi_uncached
                    ));
                }
                if !snapshot.tree.is_empty() {
                    parts.push(format!("tree {}", short(&snapshot.tree)));
                }
                parts.join("  \u{b7}  ")
            }
            Pane::Diffstat => {
                let filter = &snapshot.filter;
                let mut parts: Vec<String> = vec![
                    format!(
                        "filter fathom.toml paths.* \u{b7} {} file(s) in the headline",
                        filter.source_files
                    ),
                    format!(
                        "renames {}",
                        if filter.renames { "detected" } else { "off" }
                    ),
                ];
                if filter.noise_files > 0 {
                    parts.insert(
                        1,
                        format!(
                            "{} noise/test file(s) (+{} -{}) are in this table and out of the \
                             headline: +{} -{} over every file the commit touched, +{} -{} in the \
                             headline",
                            filter.noise_files,
                            filter.noise_add,
                            filter.noise_del,
                            filter.add_all,
                            filter.del_all,
                            filter.add,
                            filter.del
                        ),
                    );
                }
                if filter.excluded_files > 0 {
                    parts.push(format!(
                        "{} file(s) removed by paths.exclude, in neither count",
                        filter.excluded_files
                    ));
                }
                if filter.truncated {
                    parts.push(
                        "the per-file list was capped by the reader \u{b7} the count is a floor, \
                         not a total"
                            .to_owned(),
                    );
                }
                parts.push(format!(
                    "vs {}",
                    snapshot.parent.as_deref().map_or(
                        "the empty tree (root commit)".to_owned(),
                        |parent| format!("parent {}", short(parent))
                    )
                ));
                parts.join("  \u{b7}  ")
            }
        }
    }

    /// The contract's own reassurance, in the pane's words: the run's total and the sum of its file
    /// records differ, and that is a fact about the tool, not a bug in the reader (ADR-0002 §4.2).
    #[must_use]
    pub fn outside_workspace_note(&self, snapshot: &Snapshot) -> String {
        let (_hit, found) = snapshot
            .files
            .iter()
            .fold((0_u64, 0_u64), |(hit, found), file| {
                (hit + file.hit, found + file.found)
            });
        if snapshot.pooled.found == 0 || found == 0 {
            return String::new();
        }
        if found == snapshot.pooled.found {
            return format!(
                "the file records account for all {} of the total's lines",
                snapshot.pooled.found
            );
        }
        let more = snapshot.pooled.found.saturating_sub(found);
        if more > 0 {
            format!(
                "{more} measured line(s) are in files outside this workspace, so the total is \
                 larger than the sum of the records (evidence-format 6)"
            )
        } else {
            format!(
                "the records name {} more lines than the total claims; the total is the run's own \
                 claim, and it is what the timeline shows (ADR-0008 3.3)",
                found - snapshot.pooled.found
            )
        }
    }

    /// The heading's two halves: `DETAIL 3b8e9f1 subject` and the facts.
    #[must_use]
    pub fn header_line(&self, palette: &Palette) -> (String, String) {
        let Some(snapshot) = self.snapshot.as_ref() else {
            let right = self
                .error
                .clone()
                .unwrap_or_else(|| format!("reading {} \u{2026}", short(&self.sha)));
            return (format!("DETAIL  {}", short(&self.sha)), right);
        };
        let subject = if snapshot.subject.is_empty() {
            "(no subject)"
        } else {
            snapshot.subject.as_str()
        };
        let mut parts: Vec<String> = Vec::new();
        parts.push(match &snapshot.parent {
            Some(parent) => format!("vs parent {}", short(parent)),
            None => "root commit \u{b7} no parent".to_owned(),
        });
        parts.push(snapshot.pooled_text(palette));
        parts.push(format!(
            "MI {}",
            snapshot.aggregate.map_or_else(
                || "\u{2014}".to_owned(),
                |aggregate| format!("{aggregate:.1}")
            )
        ));
        parts.push(format!(
            "+{} -{} in {} file(s)",
            snapshot.filter.add,
            snapshot.filter.del,
            snapshot.diff.len()
        ));
        (
            format!("DETAIL  {}  {}", short(&self.sha), subject),
            parts.join(" \u{b7} "),
        )
    }

    /// The status line: the focused pane's footer, with the shared path in front of it.
    #[must_use]
    pub fn status(&self, palette: &Palette) -> String {
        let footer = self.footer(self.focus, palette);
        match self.path() {
            Some(path) if footer.is_empty() => path,
            Some(path) => format!("{path}  \u{b7}  {footer}"),
            None => footer,
        }
    }

    /// Keep every pane's cursor and offset inside its rows and its slice of the frame.
    fn clamp_view(&mut self) {
        for pane in Pane::ALL {
            let index = pane.index();
            let count = match self.snapshot.as_ref() {
                None => 0,
                Some(snapshot) => match pane {
                    Pane::Coverage => snapshot.files.len(),
                    Pane::Hotspots => snapshot.mi.len(),
                    Pane::Diffstat => snapshot.diff.len(),
                },
            };
            if count == 0 {
                self.cursor[index] = 0;
                self.offset[index] = 0;
                continue;
            }
            if self.cursor[index] >= count {
                self.cursor[index] = count - 1;
            }
            let page = self.page(index);
            if self.offset[index] + page > count {
                self.offset[index] = count.saturating_sub(page);
            }
            if self.cursor[index] < self.offset[index] {
                self.offset[index] = self.cursor[index];
            }
            if self.cursor[index] >= self.offset[index] + page {
                self.offset[index] = self.cursor[index] + 1 - page;
            }
        }
    }

    /// Rows one pane draws: its height minus its border, its column row and its footer.
    #[must_use]
    pub fn page(&self, index: usize) -> usize {
        usize::from(self.heights[index].saturating_sub(4)).max(1)
    }
}

/// The sentence a coverage gap code earns.
///
/// What the code means, then what to do about it, in one line each: a reader who only reads the first
/// line still knows the name of their problem, and one who reads both has the command. Every line is
/// short enough to survive the pane's width and its page, which is the whole reason a note is one
/// clause rather than three — an explanation truncated at 60 cells is a sentence with no verb in it,
/// and a fourth line is a line a 30-row terminal does not draw.
fn gap_note(gap: &str) -> Vec<String> {
    match gap {
        "no_evidence" => vec![
            "  nothing ran for this commit \u{b7} act-mcp's `run_workflow` starts one, then R".to_owned(),
        ],
        "no_records" => vec![
            "  nothing that ran here published coverage \u{b7} `run_workflow` the coverage task, \
             then R"
                .to_owned(),
        ],
        "all_runs_excluded" => vec![
            "  runs happened here and none ended `success` \u{b7} they are named in the footer"
                .to_owned(),
        ],
        "only_percent_only" => vec![
            "  the only coverage evidence was a scraped percentage \u{b7} a caveat, never a number (ADR-0008 3.6)".to_owned(),
        ],
        "vacuous" => vec![
            "  every instrumented file had 0 lines to cover \u{b7} that is unknown, not 0%".to_owned(),
        ],
        "unreadable" => vec![
            "  records were present and this build could not read them \u{b7} `act-mcp validate-evidence` names the line".to_owned(),
        ],
        _ => vec!["  nothing usable was found for this commit".to_owned()],
    }
}

/// The coverage pane's path mark: a file the run measured that this commit did not touch.
fn row_mark(row: &CoverageRow) -> String {
    if row.touched {
        String::new()
    } else {
        "\u{b7} ".to_owned()
    }
}

/// The MI pane's LOC key, so the sorts never invent a zero for an unknown weight.
fn loc(row: &HotspotRow) -> u64 {
    row.loc.unwrap_or(0)
}

/// `None` last, ascending otherwise: unknown is not the worst case, it is the absence of a case.
fn none_last(left: Option<f64>, right: Option<f64>) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => left.partial_cmp(&right).unwrap_or(Ordering::Equal),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// Column widths: the three right-hand columns take their content's width (at least their title's),
/// and the path column takes the rest with a floor of four cells.
fn widths(columns: &[String; 4], cells: &[[String; 4]], total: usize) -> [usize; 4] {
    let mut widths = [0_usize; 4];
    for index in 1..4 {
        widths[index] = columns[index].chars().count();
        for row in cells {
            widths[index] = widths[index].max(row[index].chars().count());
        }
        // One cell of gutter per column. The cells are concatenated when a row is drawn, so a column
        // that is exactly as wide as its widest cell would print `+0-3100` and read as one number.
        widths[index] += 1;
    }
    let used: usize = widths[1..].iter().sum();
    widths[0] = total.saturating_sub(used).max(8);
    widths
}

/// The column-title row, laid out in the same widths as the rows.
fn layout_cells(columns: &[String; 4], widths: &[usize; 4], ellipsis: char) -> [String; 4] {
    // Titles keep their own column's alignment but are never the reason a column is wide: they are
    // already counted in `widths`, and a title longer than its content is the column that lies about
    // the data it heads.
    std::array::from_fn(|index| fit(&columns[index], widths[index], align_of(index), ellipsis))
}

/// Which cells are right-aligned: the last two, always. Column 0 is the path, column 1 the number
/// that sorts, and both read from the left.
const fn align_of(index: usize) -> Align {
    if index >= 2 {
        Align::Right
    } else {
        Align::Left
    }
}

/// The window of rows a pane draws, each cell fitted to its column.
fn window(
    rows: Vec<Row>,
    offset: usize,
    height: usize,
    ellipsis: char,
    widths: &[usize; 4],
) -> Vec<Row> {
    rows.into_iter()
        .skip(offset)
        .take(height)
        .map(|row| Row {
            cells: std::array::from_fn(|index| {
                fit(&row.cells[index], widths[index], align_of(index), ellipsis)
            }),
            marked: row.marked,
            cursor: row.cursor,
        })
        .collect()
}

/// Seven hex, ADR-0008 §6.2's display form.
#[must_use]
pub fn short(value: &str) -> String {
    value.chars().take(7).collect()
}

/// A stored `FileCoverage` → the pane's row, with the parent's rate joined on.
#[must_use]
pub fn coverage_row(file: &FileCoverage, parent: Option<f64>, touched: bool) -> CoverageRow {
    CoverageRow {
        path: file.path.clone(),
        hit: file.hit,
        found: file.found,
        percent: file.percent,
        parent,
        delta: file.percent.zip(parent).map(|(now, before)| now - before),
        touched,
        runs: file.runs.clone(),
    }
}

/// A stored row's `evidence.coverage` block, as the pane needs it.
#[must_use]
pub fn pooled(evidence: Option<&Value>) -> Pooled {
    let Some(evidence) = evidence else {
        return Pooled {
            gap: Some("no_evidence".to_owned()),
            ..Pooled::default()
        };
    };
    let text = |value: &Value, key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
    let empty = Value::Null;
    let coverage = evidence.get("coverage").unwrap_or(&empty);
    let percent = coverage.get("percent").and_then(Value::as_f64);
    let gap = percent.map_or_else(
        || {
            evidence
                .get("gaps")
                .and_then(|gaps| gaps.get("coverage"))
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| Some("no_records".to_owned()))
        },
        |_| None,
    );
    let secondary = coverage
        .get("secondary")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    let measure = text(row, "measure")?;
                    let percent = row.get("percent").and_then(Value::as_f64)?;
                    Some(format!("{measure} {percent:.1}%"))
                })
                .collect()
        })
        .unwrap_or_default();
    let runs = coverage
        .get("runs")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    Pooled {
        percent,
        hit: coverage.get("hit").and_then(Value::as_u64).unwrap_or(0),
        found: coverage.get("found").and_then(Value::as_u64).unwrap_or(0),
        measure: text(coverage, "measure").unwrap_or_else(|| "lines".to_owned()),
        gap,
        runs,
        secondary,
    }
}
