//! The benchmark panel: `b` on a commit, every metric group it has, and no comparison that cannot
//! be made.
//!
//! A timeline column can show *one* group (ADR-0008 §4.5), which is why the cell says `412.5ms +6*`
//! and a reader who wants the six has to ask. This is that asking: one row per `(name, labels, unit)`
//! group at the selected commit, with the value the reader's derivation produced, the spread over the
//! runs behind it, and a history line across the indexed window.
//!
//! ## The rules this screen is made of
//!
//! * **`unit` is part of the identity** (ADR-0008 §4.2, §4.3). Rows are the reader's groups, so
//!   `queue_depth` in `ms` and `queue_depth` in `s` are two rows, and a group with no unit is a third
//!   state: displayable, never comparable. An incomparable row gets no delta, no sparkline and a
//!   visible `not cmp`, because a y-axis drawn under a bare `3` is a y-axis nobody measured.
//! * **The value is the newest selected run's median; the spread is every retained run's** (§4.2).
//!   That asymmetry is said in the column headers — `commit 3b8e9f1` under one value, `spread(2 runs)`
//!   under both — because "best current estimate, and here is how much the runs disagree" reads like a
//!   contradiction only when it is left unstated.
//! * **A sparkline states its own axis.** Every one is rescaled to its own min/max, which is exactly
//!   how a rescaled sparkline has lied more than once, so the labels are printed beside it rather than
//!   assumed. A history with holes does not connect them: absent means unknown, and the count of
//!   unknowns is on the line.
//! * **A delta needs a baseline and it needs samples.** The baseline is the newest *older* commit in
//!   the window carrying the same group identity; the percentage is judged against a declared band
//!   (`benchmarks.regression_band_percent`, a percent of value) and nothing on this screen invents a
//!   significance test. One sample, or a spread from one run, is `insufficient`, never a percentage:
//!   ADR-0008 §4.2 calls that an unmeasured noise floor, and a screen that rounds it to `·0.0%` would
//!   be a screen claiming a stability it did not measure.
//! * **`act_log` rows are context.** `~job_duration_s` describes the run, not the code (ADR-0008
//!   §4.4): it is shown, marked, and never marked *outside its band*.
//! * **Absence is not a hole.** A metric the window has seen and this commit has not gets its own row
//!   saying when it was last seen, and `step_duration_s` says the documented reason when the evidence
//!   carries it rather than leaving a gap for a reader to fill with a regression story.
//! * **Nothing here reads `.evidence`, `git` or the server.** The panel is a function of rows the index
//!   already published ([`super::store_view`]), and the empty state's "what would produce this" line
//!   quotes the runbook's `list_tasks` example instead of calling `list_tasks`.
//!
//! ## What this screen cannot say, and says so instead
//!
//! A failed run's *numbers* are not in a row: retention kept its id, its status and the reason it was
//! dropped, which is what the `f` overlay prints (ADR-0008 §3.7). And a percentage has no direction of
//! good — nothing in this data model declares that a bigger `tokenizer_throughput` is a better commit
//! or that a bigger `job_duration_s` is a worse one — so the band's answer is "outside the declared
//! band" with an arrow, and the judgement stays with the reader.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::{Number, Value};

use crate::metrics::row::canonical_json;
use crate::store::row::Row;

use super::cells::Align;
use super::entry::{delta_of, four_significant, Delta};
use super::palette::Palette;
use super::plain::fit;

/// How many commits a history line walks back, selected commit included (the ticket's `last 20`).
///
/// Twenty is not a statistical claim: it is about two screens of commits at an ordinary pace, and it
/// bounds what opening a panel costs on a 40 000-commit repository.
pub const HISTORY_WINDOW: usize = 20;

/// The default regression band, in percent of the value, when `fathom.toml` says nothing.
///
/// The canonical value lives in [`crate::config::DEFAULT_REGRESSION_BAND_PERCENT`] because it is a
/// `fathom.toml` default first and a panel constant second; this alias is here so the screen can name
/// what it falls back to without reaching past its own layer for a default.
pub const DEFAULT_BAND_PERCENT: f64 = crate::config::DEFAULT_REGRESSION_BAND_PERCENT;

/// The panel's row budget for a frame of `height` rows.
///
/// The frame spends one row on the heading, *two* on the status line, one on the key line, and three on
/// the table — two borders and the column titles. Both the drawing and the scroll clamp need the same
/// number, which is why it is a function and not a subtraction in two places (the diff view's
/// [`super::diff_view::pane_height`] is the same idea with a different table shape).
#[must_use]
pub const fn pane_height(height: u16) -> u16 {
    let body = height.saturating_sub(7);
    if body == 0 {
        1
    } else {
        body
    }
}

/// The flag a run carries when its act was too old to time its steps
/// (docs/evidence-format.md §5, docs/benchmark-inputs.md §3).
///
/// Declared here because this panel is what renders it. `flags` is an open vocabulary (ADR-0002 §7),
/// so a record carrying it is legal today; a record not carrying it is not a statement that the
/// metric is fine.
pub const FLAG_STEP_TIMINGS_UNAVAILABLE: &str = "act_step_timings_unavailable";

/// The note for an absent `step_duration_s`, worded to be readable without the ADR open.
const STEP_TIMINGS_NOTE: &str =
    "step timings need act 0.2.76 or newer; an older act publishes none, so this is unknown, not 0 s";

/// The metric name act's own step timings arrive under.
const STEP_DURATION_S: &str = "step_duration_s";

/// The sparkline's eight steps, ascending ink (unicode column of ADR-0008 §6.1's table).
const STEPS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
/// The same eight for the terminal that gets `^` instead of `▲`.
const STEPS_ASCII: [char; 8] = ['.', ',', ':', '-', '=', '+', '*', '#'];

/// "runs happened here and none was eligible" (ADR-0008 §3.7).
const GAP_ALL_EXCLUDED: &str = "all_runs_excluded";
/// "`fathom.toml` dropped every metric" (ADR-0006 §7.2).
const GAP_CONFIG_FILTERED: &str = "config_filtered";
/// "records were there and this build could not read them".
const GAP_UNREADABLE: &str = "unreadable";

/// A stored JSON number, kept two ways on purpose: the float a comparison needs and the shortest
/// round-trip text the row was written with (ADR-0008 §4.2's stored form).
///
/// [`Amount::shown`] is the four-significant-digit rendering every surface in this crate draws numbers
/// at (mact-by1.9's timeline cell uses the same helper); `text` is the stored spelling the recipe tests
/// compare against. Keeping both here means no cell ever re-rounds a rounded number to get at the other.
#[derive(Debug, Clone, PartialEq)]
pub struct Amount {
    /// The value as a binary64, for ordering and for a percentage.
    pub value: f64,
    /// The row's own spelling of it (`3`, `3.0`, `184666.55`), never re-rounded.
    pub text: String,
}

impl Amount {
    /// From a JSON number in a stored row.
    #[must_use]
    pub fn of(number: &Number) -> Self {
        Self {
            value: number.as_f64().unwrap_or(0.0),
            text: number.to_string(),
        }
    }

    /// The four-significant-digit rendering.
    #[must_use]
    pub fn shown(&self) -> String {
        four_significant(self.value)
    }
}

/// A benchmark group's identity: `(name, labels, unit)`, with an absent unit as its own key value.
///
/// This is ADR-0008 §4.2's group key as a type, because two commits may only be compared inside one of
/// these and "same group" has to be nameable at both ends.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Identity {
    /// The metric name, verbatim.
    pub name: String,
    /// The label set as canonical JSON (keys ascending), `{}` when the record carried none.
    pub labels: String,
    /// The unit, verbatim; `None` *is* `unit_missing` and is a key value of its own.
    pub unit: Option<String>,
}

impl Identity {
    /// The comparison key: two identities are the same metric exactly when this string is equal, which
    /// is what makes "values from two groups are never compared" structural rather than a rule a
    /// renderer has to remember.
    #[must_use]
    pub fn key(&self) -> String {
        format!(
            "{}\u{0}{}\u{0}{}",
            self.name,
            self.labels,
            self.unit.as_deref().unwrap_or("")
        )
    }

    /// `unit` as the recipes spell it: `unit_missing` when there is none.
    #[must_use]
    pub fn unit_or_missing(&self) -> String {
        self.unit
            .clone()
            .unwrap_or_else(|| "unit_missing".to_owned())
    }
}

/// One group at one commit, as the row the index published states it.
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    /// `(name, labels, unit)`.
    pub id: Identity,
    /// The median over the samples of the newest selected run (ADR-0008 §4.2).
    pub value: Amount,
    /// How many records of this group that run published — the noise estimate.
    pub samples: usize,
    /// Spread low, over every retained run of this group at this commit.
    pub min: Amount,
    /// Spread high, same population.
    pub max: Amount,
    /// How many distinct retained runs the spread came from. `1` with `min == max` is an unmeasured
    /// noise floor, which is a thing to say and not a thing to plot as stability.
    pub run_count: usize,
    /// The run `value` came from.
    pub run_id: String,
    /// That run's `source`: `job_file`, `in_band_stdout` or `act_log` (ADR-0002 §7).
    pub source: String,
    /// That run's flags on those records, as the row stores them.
    pub flags: Vec<String>,
    /// Whether these values may be diffed or plotted at all: false exactly when there is no unit.
    pub comparable: bool,
    /// Whether this is `act_log` context rather than a measurement of the code.
    pub context: bool,
}

impl Group {
    /// Whether the row carries one of the writer's flags.
    #[must_use]
    pub fn has_flag(&self, flag: &str) -> bool {
        self.flags.iter().any(|present| present == flag)
    }

    /// Whether the spread was ever measured: more than one run, or one run whose samples disagreed.
    ///
    /// ADR-0008 §4.2's "runs: 1 with min == max is an unmeasured noise floor, not a stable metric",
    /// said once so the panel and the verdict cannot disagree about it.
    #[must_use]
    pub fn spread_measured(&self) -> bool {
        self.run_count > 1 || self.min.text != self.max.text
    }
}

/// A run that happened at this commit and was not used (ADR-0008 §3.7).
///
/// The row keeps an identity and a reason, not numbers: retention dropped the records, so the panel can
/// name the run and say why it is not a number. That is what the `f` overlay is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Excluded {
    /// Which run.
    pub run_id: String,
    /// What it ended as.
    pub status: String,
    /// The model's reason code (`run_status_excluded`, `run_truncated`).
    pub reason: String,
}

/// One `(kind, workflow, task)` group's presence, for the header's "which workflow ran here".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workflow {
    /// `benchmark` or `coverage`.
    pub kind: String,
    /// The workflow identity the record named.
    pub workflow: String,
    /// The task (job id) the record named.
    pub task: String,
}

/// One commit's benchmark evidence, as far as the panel is concerned.
///
/// Built from a stored row ([`Commit::from_row`]) and nothing else — no evidence walk, no git, no
/// clock. The empty value is legal: a commit with no benchmark evidence is a timeline row that opens
/// onto an empty panel with a next step, not a key that does nothing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Commit {
    /// The full SHA.
    pub sha: String,
    /// Its message head, for the heading row.
    pub subject: String,
    /// Every group that survived retention, in `(name, labels, unit)` order.
    pub groups: Vec<Group>,
    /// Runs that happened here and were excluded, with the reason.
    pub excluded: Vec<Excluded>,
    /// How many runs were superseded by a newer run of the same group.
    pub superseded: usize,
    /// Why there are no groups, when there are none (`gaps.benchmarks`).
    pub gap: Option<String>,
    /// The row's caveat codes, for the status line.
    pub caveats: Vec<String>,
    /// Which workflow/task ran here, both kinds.
    pub workflows: Vec<Workflow>,
    /// Records present and unreadable — "nothing ran" and "I could not read it" are different answers
    /// (ADR-0008 §5.4).
    pub unreadable: u64,
    /// Whether the row had an `evidence` section at all.
    pub has_evidence: bool,
}

impl Commit {
    /// One commit's view, from one stored row.
    #[must_use]
    pub fn from_row(row: &Row) -> Self {
        let subject = row
            .history
            .as_ref()
            .and_then(|history| history.get("subject"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        Commit {
            subject,
            ..Commit::from_evidence(&row.commit, row.evidence.as_ref())
        }
    }

    /// One commit's view, from an evidence section on its own.
    ///
    /// [`Commit::from_row`] delegates here, and this is public because the recipe tests build the same
    /// value from a freshly derived [`crate::metrics::CommitMetrics`]: one parser, so the panel and the
    /// store cannot disagree about what a row means.
    #[must_use]
    pub fn from_evidence(sha: &str, evidence: Option<&Value>) -> Self {
        let Some(evidence) = evidence else {
            return Commit {
                sha: sha.to_owned(),
                has_evidence: false,
                gap: Some("no_evidence".to_owned()),
                ..Commit::default()
            };
        };
        let list = |key: &str| -> Vec<&Value> {
            evidence
                .get(key)
                .and_then(Value::as_array)
                .map_or_else(Vec::new, |rows| rows.iter().collect())
        };
        let codes = |key: &str| -> Vec<String> {
            list(key)
                .into_iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        };
        let mut groups: Vec<Group> = list("benchmarks")
            .into_iter()
            .filter_map(group_of)
            .collect();
        groups.sort_by(|left, right| left.id.cmp(&right.id));
        Commit {
            sha: sha.to_owned(),
            subject: String::new(),
            groups,
            excluded: list("excluded_runs")
                .into_iter()
                .filter_map(|row| {
                    Some(Excluded {
                        run_id: text(row, "run_id")?.to_owned(),
                        status: text(row, "status")?.to_owned(),
                        reason: text(row, "reason")?.to_owned(),
                    })
                })
                .collect(),
            superseded: list("superseded_runs").len(),
            gap: evidence
                .get("gaps")
                .and_then(|gaps| gaps.get("benchmarks"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            caveats: codes("caveats"),
            workflows: list("action_detail")
                .into_iter()
                .filter_map(|row| {
                    Some(Workflow {
                        kind: text(row, "kind")?.to_owned(),
                        workflow: text(row, "workflow")?.to_owned(),
                        task: text(row, "task")?.to_owned(),
                    })
                })
                .collect(),
            unreadable: evidence
                .get("unreadable_records")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            has_evidence: true,
        }
    }

    /// The group with this exact identity, if this commit has one.
    #[must_use]
    pub fn group(&self, identity: &Identity) -> Option<&Group> {
        let key = identity.key();
        self.groups.iter().find(|group| group.id.key() == key)
    }

    /// Every identity this commit reports.
    #[must_use]
    pub fn identities(&self) -> Vec<Identity> {
        self.groups.iter().map(|group| group.id.clone()).collect()
    }

    /// Whether there is anything to draw.
    #[must_use]
    pub fn has_benchmarks(&self) -> bool {
        !self.groups.is_empty()
    }
}

/// One stored row's benchmark group, or `None` for a row this build cannot read.
///
/// Every field this needs is required in the row shape (ADR-0008 §4.2), so a group missing one is not
/// a group with a blank: it is a row this build does not understand, and it is dropped rather than
/// drawn as a zero.
fn group_of(value: &Value) -> Option<Group> {
    let name = text(value, "name")?.to_owned();
    let labels = value
        .get("labels")
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
                .collect::<BTreeMap<String, String>>()
        })
        .unwrap_or_default();
    let number = |key: &str| value.get(key).and_then(Value::as_number);
    let value_number = number("value")?;
    Some(Group {
        id: Identity {
            name,
            labels: canonical_json(&labels),
            unit: text(value, "unit").map(str::to_owned),
        },
        value: Amount::of(value_number),
        samples: count(value, "samples")?,
        min: Amount::of(number("min")?),
        max: Amount::of(number("max")?),
        run_count: count(value, "run_count")?,
        run_id: text(value, "run_id")?.to_owned(),
        source: text(value, "source")?.to_owned(),
        flags: value
            .get("flags")
            .and_then(Value::as_array)
            .map(|flags| {
                flags
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        comparable: value
            .get("comparable")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        context: value
            .get("context")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

/// Where the panel's evidence would have come from, for its empty state.
///
/// Both halves come from the reading (ADR-0006 §3's resolved roots), never from the server: the panel
/// can name the directory a record would appear in without asking anything that could start a run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Evidence {
    /// Whether the evidence root exists.
    pub present: bool,
    /// Where it is, for the line that names it.
    pub path: PathBuf,
}

/// One point of a history line: one commit, and whether it reported the group.
#[derive(Debug, Clone, PartialEq)]
pub struct Point {
    /// Seven hex, the display form (ADR-0008 §6.2).
    pub sha: String,
    /// The value at that commit; `None` when it reported nothing for this group.
    pub value: Option<Amount>,
}

/// Why a baseline exists but a percentage does not.
///
/// A closed set rather than a free-text reason, because the status line and the tests both have to
/// name it, and because "insufficient" is one glyph's worth of signal with exactly three honest causes
/// in this data model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Short {
    /// The value is a median over fewer than two samples, so there is no noise estimate to compare
    /// against (ADR-0008 §4.2).
    Samples(usize),
    /// One run, and its samples agreed: `min == max` is an unmeasured noise floor, not a stable
    /// metric, so a movement off it cannot be called small or large.
    NoiseFloor,
    /// The baseline is `0`, and a percent of zero is not a sentence.
    ZeroBaseline,
}

impl Short {
    /// The cell: as short as a column can carry.
    #[must_use]
    pub fn text(self) -> String {
        match self {
            Short::Samples(n) => format!("n={n}"),
            Short::NoiseFloor => "1 run".to_owned(),
            Short::ZeroBaseline => "base 0".to_owned(),
        }
    }

    /// The status line's sentence, which is the part that has to be readable.
    #[must_use]
    pub fn sentence(self) -> String {
        match self {
            Short::Samples(n) => format!(
                "insufficient samples (n={n}) \u{b7} a percentage off one sample is not a \
                 measurement of a change"
            ),
            Short::NoiseFloor => "spread from 1 run \u{b7} the noise floor is unknown, so no \
                                   percentage is honest here"
                .to_owned(),
            Short::ZeroBaseline => "the baseline is 0, so there is no percent of value".to_owned(),
        }
    }
}

/// What a delta against the baseline can honestly be.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// The group has no unit: displayable, never comparable (ADR-0008 §4.3).
    NotComparable,
    /// No older commit in the window carries this identity, so there is no baseline. Unknown, and not
    /// a tie: `—` and `·` are different sentences.
    NoBaseline,
    /// A baseline exists and the data does not support a percentage.
    Insufficient(Short),
    /// A percentage against the baseline. `outside` is the declared band's answer, and it is never set
    /// for a context row: run-level metrics move, that is what they do.
    Moved {
        /// Up, down, or equal at the stored precision.
        direction: Delta,
        /// Percent of the baseline's value, signed so the arrow and the number say one sentence.
        percent: f64,
        /// Whether `|percent|` is outside the declared band.
        outside: bool,
        /// Whether the row is `act_log` context, which the band does not judge.
        context: bool,
    },
}

impl Verdict {
    /// The cell's text, in the palette's alphabet.
    #[must_use]
    pub fn text(&self, palette: &Palette) -> String {
        let glyphs = &palette.glyphs;
        match self {
            Verdict::NotComparable => "not cmp".to_owned(),
            Verdict::NoBaseline => glyphs.unknown.clone(),
            Verdict::Insufficient(short) => short.text(),
            Verdict::Moved {
                direction,
                percent,
                outside,
                context,
            } => {
                let arrow = match direction {
                    Delta::Up => glyphs.delta_up.clone(),
                    Delta::Down => glyphs.delta_down.clone(),
                    Delta::Flat => glyphs.delta_flat.clone(),
                };
                let sign = if *percent > 0.0 { "+" } else { "" };
                let mut text = format!("{arrow}{sign}{percent:.1}%");
                if *context {
                    text.push_str(&glyphs.context);
                }
                if *outside {
                    text.push('!');
                }
                text
            }
        }
    }
}

/// One row of the panel: one identity, measured here or not.
///
/// `measured: None` is a metric the window has seen and this commit has not. It is still a row, because
/// "the job stopped reporting this" is a fact about this commit, and a screen that omits it lets the
/// reader assume the number simply did not move.
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    /// Which metric this row is about.
    pub id: Identity,
    /// The group as measured at this commit, if it was.
    pub measured: Option<Group>,
    /// The delta against the newest older commit carrying this identity.
    pub verdict: Option<Verdict>,
    /// That baseline commit's seven hex, when there is one.
    pub baseline: Option<String>,
    /// The history points, oldest first, ending at this commit.
    pub history: Vec<Point>,
    /// How many of those points reported nothing.
    pub missing: usize,
    /// For a metric absent here: where it was last seen, and how many commits ago.
    pub last_seen: Option<(String, usize)>,
    /// The reason an absent metric can name, when the evidence gives one
    /// ([`FLAG_STEP_TIMINGS_UNAVAILABLE`]).
    pub note: Option<String>,
}

impl Line {
    /// The measured group's value, when this commit has one.
    #[must_use]
    pub fn value(&self) -> Option<&Amount> {
        self.measured.as_ref().map(|group| &group.value)
    }

    /// Whether this row describes the run rather than the code.
    #[must_use]
    pub fn is_context(&self) -> bool {
        self.measured.as_ref().is_some_and(|group| group.context)
    }

    /// Whether this row's identity may be plotted at all.
    #[must_use]
    pub fn comparable(&self) -> bool {
        self.measured.as_ref().is_some_and(|group| group.comparable)
    }

    /// The `metric` cell: name, unit or the fact that it has none, and the source mark.
    #[must_use]
    pub fn metric_text(&self, palette: &Palette) -> String {
        let mut text = String::new();
        if self.is_context() {
            text.push_str(&palette.glyphs.context);
        }
        text.push_str(&self.id.name);
        match &self.id.unit {
            Some(unit) => text.push_str(&format!(" {unit}")),
            // The ticket's own words: a number with no unit is displayable, and the screen says so
            // rather than letting a bare `3` read as a measurement of something.
            None => text.push_str(" (unit missing)"),
        }
        if let Some(group) = &self.measured {
            if let Some(mark) = source_mark(&group.source, palette) {
                text.push(' ');
                text.push_str(&mark);
            }
        }
        text
    }

    /// The `labels` cell: the canonical set flattened to `job=x step=y`; empty when there is none.
    #[must_use]
    pub fn labels_text(&self) -> String {
        flatten_labels(&self.id.labels)
    }

    /// The value cell: the stored number at four significant digits, and the sample count it is the
    /// median of, because `184666.55` from one sample and from forty are different claims.
    ///
    /// `n=` is the first thing to go when the column is tight — it is the one fact the verdict cell says
    /// out loud anyway (`n=1` is a whole verdict of its own), and the number itself is the one fact the
    /// row cannot be drawn without.
    #[must_use]
    pub fn value_text(&self, palette: &Palette, width: usize) -> String {
        let Some(group) = &self.measured else {
            return palette.glyphs.unknown.clone();
        };
        let shown = group.value.shown();
        let full = format!("{shown} n={}", group.samples);
        if full.chars().count() <= width {
            return full;
        }
        shown
    }

    /// The `spread` cell: `[min..max]` and the runs it came from.
    ///
    /// One number is printed once, with its run count: `[91.8..91.8]` reads as a precision the run
    /// does not have, and the run count is the part that says why.
    #[must_use]
    pub fn spread_text(&self, palette: &Palette, width: usize) -> String {
        let Some(group) = &self.measured else {
            return palette.glyphs.unknown.clone();
        };
        let runs = if group.run_count == 1 {
            "1 run".to_owned()
        } else {
            format!("{} runs", group.run_count)
        };
        let range = if group.min.text == group.max.text {
            format!("[{}]", group.min.shown())
        } else {
            format!("[{}..{}]", group.min.shown(), group.max.shown())
        };
        let full = format!("{range} {runs}");
        if full.chars().count() <= width {
            return full;
        }
        // Too narrow for both: the range is what the column is for, and the run count is in its title
        // (`spread(3 runs)`) and on the status line, so it is the one to give up.
        if range.chars().count() <= width {
            return range;
        }
        // Still `[412.5..412.6] 1 run` too wide: at least the low end survives.
        format!("[{}]", group.min.shown())
            .chars()
            .take(width)
            .collect()
    }

    /// The `history` cell, drawn to fit `width` columns, and why it might be blank.
    ///
    /// Three rules, all of them about what a line is allowed to join:
    ///
    /// * a group with no unit, or a group this commit does not report, draws nothing (ADR-0008 §4.3) —
    ///   an unlabelled number joined to another unlabelled number is the picture that lies;
    /// * a history with a hole in it draws nothing either, because the drawn line would run straight
    ///   across the gap and a reader reads a drawn line as "this is the path". The axis survives,
    ///   because the axis is a claim about the points there are, not about the path between them;
    /// * what is drawn is printed with its own axis, always. A sparkline on its own rescaled axis,
    ///   with no numbers, is the version of this chart that is allowed to exaggerate.
    #[must_use]
    pub fn history_text(&self, palette: &Palette, width: usize) -> String {
        if self.measured.is_none() {
            return self.absent_cell(palette);
        }
        if !self.comparable() {
            return String::new();
        }
        let values = self.values();
        if values.len() < 2 {
            return format!("{} 1 point", palette.glyphs.unknown);
        }
        let axis = axis_within(&values, width.saturating_sub(3));
        if self.missing > 0 {
            // The gap is said in the status line too, in words; here it is the absence of a line.
            return format!("{} {axis}", palette.glyphs.unknown)
                .chars()
                .take(width)
                .collect();
        }
        // The axis is the part a reader cannot reconstruct, so it is paid for first: the sparkline gets
        // whatever is left, and disappears before the numbers do.
        let room = width.saturating_sub(axis.chars().count() + 1);
        debug_assert!(axis.chars().count() < width || width == 0);
        if room < 2 {
            return axis.chars().take(width).collect();
        }
        let points = sample(&values, room.min(values.len()));
        format!("{} {axis}", spark(&points, palette.unicode()))
    }

    /// What the history column says for a metric this commit does not report.
    #[must_use]
    pub fn absent_cell(&self, palette: &Palette) -> String {
        match &self.last_seen {
            Some((sha, ago)) => format!("last seen {sha} ({ago} ago)"),
            None => palette.glyphs.unknown.clone(),
        }
    }

    /// The row's five cells, in [`Column`] draw order, labels folded into the metric cell.
    ///
    /// `fit` carries the widths of the two columns whose text has to be *drawn* to size rather than cut
    /// afterwards: a sparkline that gets `…`-ed loses its last points, which are the ones a reader is
    /// looking at, and a spread that gets `…`-ed loses its run count, which is the difference between a
    /// measurement and a guess.
    #[must_use]
    pub fn cells(&self, palette: &Palette, fit: Fit) -> [String; COLUMNS] {
        [
            {
                let mut text = self.metric_text(palette);
                let labels = self.labels_text();
                if !labels.is_empty() {
                    text = format!("{text} \u{b7} {labels}");
                }
                text
            },
            self.value_text(palette, fit.value),
            self.spread_text(palette, fit.spread),
            self.verdict.as_ref().map_or_else(
                || palette.glyphs.unknown.clone(),
                |verdict| verdict.text(palette),
            ),
            self.history_text(palette, fit.history),
        ]
    }

    /// The values this row's history has, oldest first — the sparkline's whole population.
    #[must_use]
    pub fn values(&self) -> Vec<f64> {
        self.history
            .iter()
            .filter_map(|point| point.value.as_ref().map(|amount| amount.value))
            .collect()
    }

    /// The partiality of this row's history, for the status line.
    #[must_use]
    pub fn history_note(&self) -> Option<String> {
        if self.missing == 0 || self.history.is_empty() {
            return None;
        }
        Some(format!(
            "unknown at {} of {} commits",
            self.missing,
            self.history.len()
        ))
    }
}

/// The panel: one commit's rows, where the reader is standing on them, and what `f` has toggled.
#[derive(Debug, Clone)]
pub struct Panel {
    /// The commit this panel is about, full SHA.
    pub sha: String,
    /// Its message head.
    pub subject: String,
    /// The rows: measured groups first, then the ones this commit stopped reporting, each in
    /// `(name, labels, unit)` order.
    pub lines: Vec<Line>,
    /// Selected row within [`Panel::lines`].
    pub selection: usize,
    /// First drawn row.
    pub offset: usize,
    /// Rows the frame can draw.
    pub height: u16,
    /// Whether the failed-run overlay is up (`f`).
    pub show_excluded: bool,
    /// The declared regression band, percent of value (`benchmarks.regression_band_percent`).
    pub band_percent: f64,
    /// The commit's own evidence, for the header and the empty state.
    pub commit: Commit,
    /// Where the evidence would have come from.
    pub evidence: Evidence,
    /// How many commits the history walked back for these rows.
    pub window: usize,
}

impl Panel {
    /// Build the panel for one commit, given the commits older than it in the reading (newest first).
    ///
    /// Only the first [`HISTORY_WINDOW`] of `older` are walked, so opening a panel costs the same on a
    /// 40 000-commit repository as on a 40-commit one.
    #[must_use]
    pub fn open(commit: &Commit, older: &[Commit], evidence: &Evidence, band_percent: f64) -> Self {
        let window: Vec<&Commit> = std::iter::once(commit)
            .chain(older.iter().take(HISTORY_WINDOW.saturating_sub(1)))
            .collect();
        let mut lines: Vec<Line> = commit
            .groups
            .iter()
            .map(|group| measured_line(group, &window, band_percent))
            .collect();
        lines.extend(absent_lines(&commit.groups, &window));
        Panel {
            sha: commit.sha.clone(),
            subject: commit.subject.clone(),
            lines,
            selection: 0,
            offset: 0,
            height: 1,
            show_excluded: false,
            band_percent: if band_percent.is_finite() && band_percent > 0.0 {
                band_percent
            } else {
                DEFAULT_BAND_PERCENT
            },
            commit: commit.clone(),
            evidence: evidence.clone(),
            window: window.len(),
        }
    }

    /// The frame's height for this pane, remembered so the keys clamp against it.
    pub fn set_height(&mut self, height: u16) {
        self.height = height.max(1);
        self.clamp_view();
    }

    /// Move the row selection by `step`, clamped at both ends (never wrapping: a panel that wraps
    /// makes you count presses to know where you are).
    pub fn move_selection(&mut self, step: isize) {
        if self.lines.is_empty() {
            return;
        }
        let last = self.lines.len() - 1;
        self.selection = if step < 0 {
            self.selection
                .saturating_sub(usize::try_from(-step).unwrap_or(usize::MAX))
        } else {
            self.selection
                .saturating_add(usize::try_from(step).unwrap_or(usize::MAX))
                .min(last)
        };
        self.clamp_view();
    }

    /// Park the selection on one row index, clamped.
    ///
    /// The seam [`super::state::App`] uses to put a reader back where they were after a re-read rebuilt
    /// the panel under them.
    pub fn select(&mut self, index: usize) {
        self.selection = index;
        self.clamp_view();
    }

    /// Jump to the first or the last row.
    pub fn jump(&mut self, to_end: bool) {
        if self.lines.is_empty() {
            return;
        }
        self.selection = if to_end { self.lines.len() - 1 } else { 0 };
        self.clamp_view();
    }

    /// Turn the failed-run overlay on or off (`f`).
    pub fn set_show_excluded(&mut self, show: bool) {
        self.show_excluded = show;
    }

    /// The selected row.
    #[must_use]
    pub fn selected(&self) -> Option<&Line> {
        self.lines.get(self.selection)
    }

    /// Keep the selection and the offset inside the rows and the viewport.
    fn clamp_view(&mut self) {
        if self.lines.is_empty() {
            self.selection = 0;
            self.offset = 0;
            return;
        }
        if self.selection >= self.lines.len() {
            self.selection = self.lines.len() - 1;
        }
        let page = usize::from(self.height.max(1));
        if self.offset + page > self.lines.len() {
            self.offset = self.lines.len().saturating_sub(page);
        }
        if self.selection < self.offset {
            self.offset = self.selection;
        }
        if self.selection >= self.offset + page {
            self.offset = self.selection + 1 - page;
        }
    }

    /// The heading's halves: `BENCH  3b8e9f1 coverage: drop node_modules` and the facts.
    ///
    /// The facts answer "which workflow is this, and how much of it did I not get": the workflows that
    /// ran at this commit, *per kind*, which is the difference between "the coverage workflow ran and
    /// the benchmark one did not" and "the run produced nothing" — three different nothings
    /// (docs/evidence-format.md §8) and three different things for a reader to do.
    #[must_use]
    pub fn header(&self, palette: &Palette) -> (String, String) {
        let left = format!(
            "BENCH  {}  {}",
            short(&self.sha),
            if self.subject.is_empty() {
                "(no subject)"
            } else {
                &self.subject
            }
        );
        let mut parts: Vec<String> = Vec::new();
        // In the order a reader decides what to do next: which workflow ran (the benchmark one first,
        // because it is the one whose absence this screen is about), then what retention threw away,
        // then the two facts that are already in a column title. The header is one line and is cut from
        // the right, so `source:` goes last.
        for kind in ["benchmark", "coverage"] {
            let named: Vec<String> = self
                .commit
                .workflows
                .iter()
                .filter(|workflow| workflow.kind == kind)
                .map(|workflow| format!("{}/{}", workflow.workflow, workflow.task))
                .collect();
            parts.push(if named.is_empty() {
                format!("{kind}: none for this commit")
            } else {
                format!("{kind}: {}", named.join(","))
            });
        }
        let excluded = self.commit.excluded.len();
        if excluded > 0 {
            let noun = if excluded == 1 { "run" } else { "runs" };
            parts.push(if self.show_excluded {
                format!("{excluded} excluded {noun} shown")
            } else {
                format!("{excluded} excluded {noun} \u{b7} f")
            });
        }
        if self.commit.superseded > 0 {
            parts.push(format!("{} superseded runs", self.commit.superseded));
        }
        if self.commit.unreadable > 0 {
            parts.push(format!(
                "{} unreadable record(s) {}",
                self.commit.unreadable, palette.glyphs.caveat_marker
            ));
        }
        parts.push(format!("history: last {} commit(s)", self.window));
        parts.push("source: act-mcp benchmark/".to_owned());

        (left, parts.join(" \u{b7} "))
    }

    /// The table's column titles, in [`COLUMNS`] order.
    ///
    /// The counts live in the titles because they are the axis: `spread(2 runs)` is the difference
    /// between "one number, twice" and "two runs that agreed", and `vs earlier (5%)` is the only place
    /// the declared band can sit next to the arrows it judges.
    #[must_use]
    pub fn headers(&self) -> [String; COLUMNS] {
        let runs = self
            .lines
            .iter()
            .filter_map(|line| line.measured.as_ref())
            .map(|group| group.run_count)
            .max()
            .unwrap_or(0);
        [
            "metric (name \u{b7} labels)".to_owned(),
            format!("commit {}", short(&self.sha)),
            format!("spread({runs} {})", if runs == 1 { "run" } else { "runs" }),
            format!("vs earlier ({}%)", trim_percent(self.band_percent)),
            format!("history (last {})", self.window.max(1)),
        ]
    }

    /// The table's drawn rows, cut to the columns `width` allows.
    ///
    /// `width` is the content width *excluding* the two-cell marker column, the same budget
    /// [`super::cells::layout`] is given for the timeline.
    #[must_use]
    pub fn rows(&self, width: usize, palette: &Palette, marker: bool) -> Vec<String> {
        let slots = layout(width);
        let cursor = if palette.unicode() { "\u{25b8} " } else { "> " };
        self.lines
            .iter()
            .enumerate()
            .map(|(index, line)| {
                let head = if marker && index == self.selection {
                    cursor
                } else {
                    "  "
                };
                join(&slots, &line.cells(palette, Fit::of(&slots)), head, palette)
            })
            .collect()
    }

    /// The header row, cut exactly like the rows are, so the titles sit over their columns.
    #[must_use]
    pub fn header_row(&self, width: usize, palette: &Palette) -> String {
        join(&layout(width), &self.headers(), "  ", palette)
    }

    /// What the panel says about the selected row: every clause that makes a cell explainable, ordered
    /// from "what is this number" to "and here is the fine print", because the status row is one row and
    /// it gets cut from the right.
    ///
    /// The overlay's bill and the empty state come first, because both are answers to "why is this
    /// screen not showing me the run I know ran here", and that question outranks any one row.
    #[must_use]
    pub fn status(&self, palette: &Palette) -> String {
        if let Some(notice) = self.notice() {
            return notice;
        }
        let Some(line) = self.selected() else {
            return String::new();
        };
        let glyph = &palette.glyphs;
        let mut parts: Vec<String> = Vec::new();
        let labels = line.labels_text();
        parts.push(if labels.is_empty() {
            line.id.name.clone()
        } else {
            format!("{} \u{b7} {labels}", line.id.name)
        });
        // The verdict goes second, before the numbers it is about: this row is one line and it gets cut
        // from the right, so the clause that decides whether a reader should care goes earliest.
        match &line.verdict {
            Some(Verdict::Moved {
                direction,
                percent,
                outside,
                context,
            }) => {
                let word = match direction {
                    Delta::Up => "up",
                    Delta::Down => "down",
                    Delta::Flat => "equal",
                };
                let mut sentence = format!(
                    "{word} {percent:.1}% vs {} (band {}%)",
                    line.baseline.as_deref().unwrap_or("?"),
                    trim_percent(self.band_percent)
                );
                if *outside {
                    sentence.push_str(if *context {
                        " \u{b7} context, so never a regression signal"
                    } else {
                        " \u{b7} OUTSIDE THE DECLARED BAND"
                    });
                }
                parts.push(sentence);
            }
            Some(Verdict::Insufficient(short)) => parts.push(short.sentence()),
            Some(Verdict::NoBaseline) => parts.push(format!(
                "no earlier commit in this window has this metric \u{b7} {} is unknown, not flat",
                glyph.unknown
            )),
            Some(Verdict::NotComparable) => {
                parts.push("no unit, so no delta and no sparkline (ADR-0008 4.3)".to_owned());
            }
            None => {}
        }
        match &line.measured {
            Some(group) => {
                let unit = match &line.id.unit {
                    Some(unit) => format!("{unit} "),
                    None => String::new(),
                };
                parts.push(format!(
                    "{}{}= {} (median of {} sample{})",
                    unit,
                    if line.is_context() {
                        format!("{} ", glyph.context)
                    } else {
                        String::new()
                    },
                    group.value.shown(),
                    group.samples,
                    if group.samples == 1 { "" } else { "s" }
                ));
                if group.context {
                    parts.push("act_log: describes the run, not the code".to_owned());
                }
                if !group.comparable {
                    parts.push("no unit: display only, not comparable".to_owned());
                }
                parts.push(format!(
                    "spread {} over {} run{}",
                    if group.min.text == group.max.text {
                        group.min.shown()
                    } else {
                        format!("{}..{}", group.min.shown(), group.max.shown())
                    },
                    group.run_count,
                    if group.run_count == 1 { "" } else { "s" }
                ));
                if !group.flags.is_empty() {
                    parts.push(format!("flags {}", group.flags.join("+")));
                }
            }
            None => {
                parts.push("not reported at this commit".to_owned());
                if let Some((sha, ago)) = &line.last_seen {
                    parts.push(format!("last seen {sha}, {ago} commit(s) ago"));
                }
                if let Some(note) = &line.note {
                    parts.push(note.clone());
                }
            }
        }
        // Last, because they are the provenance clauses: which run the number came from, and how much of
        // the history a reader is *not* seeing. Useful, and useless before the number itself.
        if let Some(group) = &line.measured {
            parts.push(format!("run {} \u{b7} {}", group.run_id, group.source));
        }
        let values = line.values();
        if values.len() > 1 {
            parts.push(format!("axis {}", axis_text(&values)));
        }
        if let Some(note) = line.history_note() {
            parts.push(format!("history: {note}"));
        }
        parts.join("  \u{b7}  ")
    }

    /// The one thing the panel must say before anything else: the gap, or the overlay's bill.
    ///
    /// `None` when the table is the whole answer.
    #[must_use]
    pub fn notice(&self) -> Option<String> {
        if self.lines.is_empty() {
            return Some(self.empty_headline());
        }
        if self.show_excluded && !self.commit.excluded.is_empty() {
            let named: Vec<String> = self
                .commit
                .excluded
                .iter()
                .map(|run| format!("{} {}", run.run_id, run.status))
                .collect();
            return Some(format!(
                "runs excluded by retention \u{b7} {} \u{b7} their numbers are not in any row above \
                 (ADR-0008 3.7)",
                named.join(", ")
            ));
        }
        None
    }

    /// What to show when there are no rows, and what the reader can do about it.
    ///
    /// Three different nothings (docs/evidence-format.md §8): no run at all, a run that was cancelled
    /// or failed, and a run that succeeded and reported nothing. The panel names which one the row can
    /// prove, names the workflow and task that would change it, and quotes the runbook's `list_tasks`
    /// example rather than calling `list_tasks` — this crate may not (ADR-0005 P4).
    #[must_use]
    pub fn next_step(&self) -> Vec<String> {
        if !self.lines.is_empty() {
            return Vec::new();
        }
        let mut lines = vec![self.empty_headline()];
        match self.commit.gap.as_deref() {
            Some(GAP_ALL_EXCLUDED) => lines.push(
                "  runs happened here and none of them ended `success`, so there is no number to show \
                  \u{b7} f names them"
                    .to_owned(),
            ),
            Some(GAP_CONFIG_FILTERED) => lines.push(
                "  every metric this run reported was dropped by `benchmarks.allow`/`deny` in \
                 fathom.toml \u{b7} that is a filter you set, not missing evidence"
                    .to_owned(),
            ),
            Some(GAP_UNREADABLE) => lines.push(format!(
                "  {} record(s) were present and this build could not read them \u{b7} `act-mcp \
                 validate-evidence {}` names the line",
                self.commit.unreadable,
                self.evidence.path.display()
            )),
            _ => {
                if self.evidence.present {
                    lines.push(format!(
                        "  there is nothing under {}/benchmark/{}/ for this commit",
                        self.evidence.path.display(),
                        self.sha
                    ));
                } else {
                    lines.push(format!(
                        "  and there is no evidence root at {} at all",
                        self.evidence.path.display()
                    ));
                }
                lines.push(
                    "  what would produce it: a run of this workspace's benchmark workflow \u{b7} \
                     `list_tasks` names the workflow and task (the runbook's example is workflow \
                     `benchmark`, task `hyperfine-bench`, category `benchmark`)"
                        .to_owned(),
                );
                lines.push(
                    "  start `act-mcp serve`, call `run_workflow` for this commit, then press R to \
                     index what it wrote"
                        .to_owned(),
                );
            }
        }
        if !self.commit.workflows.is_empty() {
            let named: Vec<String> = self
                .commit
                .workflows
                .iter()
                .map(|workflow| {
                    format!("{}: {}/{}", workflow.kind, workflow.workflow, workflow.task)
                })
                .collect();
            lines.push(format!("  what did run here: {}", named.join(", ")));
        }
        lines
    }

    /// The empty state's headline: which commit, and which of the three nothings the row can prove.
    #[must_use]
    pub fn empty_headline(&self) -> String {
        match self.commit.gap.as_deref() {
            Some(GAP_ALL_EXCLUDED) => format!(
                "no benchmark number at {} \u{b7} every run here was excluded",
                short(&self.sha)
            ),
            Some(GAP_CONFIG_FILTERED) => format!(
                "no benchmark rows at {} \u{b7} fathom.toml filtered every metric",
                short(&self.sha)
            ),
            Some(GAP_UNREADABLE) => format!(
                "no benchmark number at {} \u{b7} the records are unreadable",
                short(&self.sha)
            ),
            _ => format!("no benchmark evidence at {}", short(&self.sha)),
        }
    }
}

// --------------------------------------------------------------------------------- the arithmetic

/// One measured row: the group, its history, its baseline, and the verdict the pair supports.
fn measured_line(group: &Group, window: &[&Commit], band: f64) -> Line {
    let history = walk(&group.id, window);
    let missing = history.iter().filter(|point| point.value.is_none()).count();
    // The baseline is the newest *older* commit carrying this exact identity: not the first parent (it
    // may not have the metric at all) and not the previous row on screen (the same thing, said twice).
    let baseline = history
        .iter()
        .rev()
        .skip(1)
        .find(|point| point.value.is_some())
        .and_then(|point| {
            point
                .value
                .as_ref()
                .map(|amount| (point.sha.clone(), amount.value))
        });
    let verdict = if !group.comparable {
        Some(Verdict::NotComparable)
    } else {
        match &baseline {
            None => Some(Verdict::NoBaseline),
            Some((_, before)) => Some(verdict_against(group, *before, band)),
        }
    };
    Line {
        id: group.id.clone(),
        measured: Some(group.clone()),
        verdict,
        baseline: baseline.map(|(sha, _)| sha),
        history,
        missing,
        last_seen: None,
        note: None,
    }
}

/// The percentage, and the three reasons there may not be one.
fn verdict_against(group: &Group, before: f64, band: f64) -> Verdict {
    if before == 0.0 {
        return Verdict::Insufficient(Short::ZeroBaseline);
    }
    if group.samples < 2 {
        return Verdict::Insufficient(Short::Samples(group.samples));
    }
    if !group.spread_measured() {
        return Verdict::Insufficient(Short::NoiseFloor);
    }
    Verdict::Moved {
        direction: delta_of(group.value.value, before),
        percent: (group.value.value - before) / before.abs() * 100.0,
        outside: (group.value.value - before) / before.abs() * 100.0 > band,
        context: group.context,
    }
}

/// The rows for metrics the window has and this commit does not.
fn absent_lines(present: &[Group], window: &[&Commit]) -> Vec<Line> {
    let here: Vec<String> = present.iter().map(|group| group.id.key()).collect();
    // `window` is newest-first, so the first sighting of an identity walking from index 1 downwards is
    // by construction the most recent one — which is what "last seen" means.
    let mut seen: Vec<(Identity, (String, usize))> = Vec::new();
    for (ago, commit) in window.iter().enumerate().skip(1) {
        for group in &commit.groups {
            let key = group.id.key();
            if here.contains(&key) || seen.iter().any(|(identity, _)| identity.key() == key) {
                continue;
            }
            seen.push((group.id.clone(), (short(&commit.sha), ago)));
        }
    }
    let mut lines: Vec<Line> = seen
        .into_iter()
        .map(|(id, last_seen)| {
            let history = walk(&id, window);
            let missing = history.iter().filter(|point| point.value.is_none()).count();
            let note = absent_note(&id, window);
            Line {
                history,
                missing,
                id,
                measured: None,
                verdict: None,
                baseline: None,
                last_seen: Some(last_seen),
                note,
            }
        })
        .collect();
    lines.sort_by(|left, right| left.id.cmp(&right.id));
    lines
}

/// The reason an absent metric can name, when the evidence gives one.
///
/// `step_duration_s` is the case the ticket names: act gained `executionTime` in 0.2.76 and this server
/// accepts act from 0.2.62, so an older act publishes **no** `step_duration_s` at all
/// (docs/evidence-format.md §5). The flag is the evidence; the metric name alone is a documented
/// suspicion, and is worded as one rather than as a cause — ADR-0008 §4.4 forbids inferring one from an
/// absence.
fn absent_note(id: &Identity, window: &[&Commit]) -> Option<String> {
    let flagged = window
        .iter()
        .filter_map(|commit| commit.group(id))
        .any(|group| group.has_flag(FLAG_STEP_TIMINGS_UNAVAILABLE));
    if flagged {
        return Some(format!("absent on act <0.2.76 \u{b7} {STEP_TIMINGS_NOTE}"));
    }
    if id.name == STEP_DURATION_S {
        return Some(STEP_TIMINGS_NOTE.to_owned());
    }
    None
}

/// The history points for one identity across `window`, oldest first, ending at the selected commit.
fn walk(identity: &Identity, window: &[&Commit]) -> Vec<Point> {
    window
        .iter()
        .rev()
        .map(|commit| Point {
            sha: short(&commit.sha),
            value: commit.group(identity).map(|group| group.value.clone()),
        })
        .collect()
}

/// The sparkline: eight steps, scaled to this row's own min and max.
///
/// A sparkline is always rescaled to its own data, which is precisely how it lies, so this function is
/// pair-readable with [`axis_text`] and the two are printed together or not at all. A flat history has
/// no range to normalise against, so it draws the middle step (a level, not a rise) and [`axis_text`]
/// prints the single value instead of a fake range.
#[must_use]
pub fn spark(values: &[f64], unicode: bool) -> String {
    let steps: &[char; 8] = if unicode { &STEPS } else { &STEPS_ASCII };
    let Some(low) = values.iter().copied().reduce(f64::min) else {
        return String::new();
    };
    let high = values.iter().copied().reduce(f64::max).unwrap_or(low);
    // Not `!(high > low)`: a NaN in the data has no ordering at all, and the flat path is the honest
    // answer for that too — a shape drawn from a value that cannot be compared is a shape that lies.
    if !matches!(high.partial_cmp(&low), Some(std::cmp::Ordering::Greater)) {
        return std::iter::repeat_n(steps[3], values.len()).collect();
    }
    values
        .iter()
        .map(|value| {
            let step = ((value - low) / (high - low) * 7.0).round() as i32;
            steps[usize::try_from(step.clamp(0, 7)).unwrap_or(0)]
        })
        .collect()
}

/// The sparkline's axis: this row's own min and max, at as many significant digits as fit.
///
/// Four is the number the rest of the screen uses ([`four_significant`]), and it is what a reader wants
/// — but `1.785e5..1.847e5` is sixteen columns, and a sparkline whose axis ate the column has no
/// sparkline. So the digit count steps down until the axis fits the space it was given, floor of two:
/// an axis of `1.8e5..1.9e5` still bounds the shape, which is all an axis has to do.
#[must_use]
pub fn axis_text(values: &[f64]) -> String {
    axis_within(values, usize::MAX)
}

/// [`axis_text`] with a width to fit: four significant digits down to two.
#[must_use]
fn axis_within(values: &[f64], width: usize) -> String {
    let Some(low) = values.iter().copied().reduce(f64::min) else {
        return String::new();
    };
    let high = values.iter().copied().reduce(f64::max).unwrap_or(low);
    if high <= low {
        return four_significant(low);
    }
    for digits in (2..=4).rev() {
        let text = format!(
            "{}..{}",
            significant(low, digits),
            significant(high, digits)
        );
        if text.chars().count() <= width || digits == 2 {
            return text;
        }
    }
    String::new()
}

/// The first `digits` significant digits.
///
/// Scientific, always, below four digits: `1.8e5` rather than `184666.5`, because the point of stepping
/// down is to *shorten*, and a decimal form with the digits chopped off the end is longer than the
/// scientific one and says less. [`four_significant`] stays in charge of four digits so the axis and the
/// value column agree whenever there is room for both.
fn significant(value: f64, digits: usize) -> String {
    if digits >= 4 {
        return four_significant(value);
    }
    let text = format!("{:.*e}", digits.saturating_sub(1), value);
    match text.split_once('e') {
        Some((mantissa, exponent)) => format!("{}e{exponent}", trim_zeros(mantissa)),
        None => text,
    }
}

/// Drop the trailing zeros a `{:.N}` format leaves behind, and a `.0` that says nothing.
fn trim_zeros(text: &str) -> String {
    if !text.contains('.') {
        return text.to_owned();
    }
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// The source marks: `job_file` plain, `in_band_stdout` marked, `act_log` marked context
/// (ADR-0002 §7's trust table, in the ticket's own words).
#[must_use]
fn source_mark(source: &str, palette: &Palette) -> Option<String> {
    match source {
        "job_file" => None,
        // `act_log` is already marked: the context glyph in front of the name is the same glyph, and
        // printing it twice read as two different things about the same row.
        "act_log" => None,
        "in_band_stdout" => Some(if palette.unicode() {
            "\u{2691}".to_owned()
        } else {
            "!".to_owned()
        }),
        other => Some(format!("[{other}]")),
    }
}

/// `{"job":"bench","step":"Run"}` → `job=bench step=Run`, and empty for `{}`.
#[must_use]
pub fn flatten_labels(canonical: &str) -> String {
    let body = canonical.trim_start_matches('{').trim_end_matches('}');
    if body.is_empty() {
        return String::new();
    }
    body.split(',')
        .filter_map(|pair| {
            let (key, value) = pair.split_once(':')?;
            Some(format!(
                "{}={}",
                key.trim_matches('"'),
                value.trim_matches('"')
            ))
        })
        .collect::<Vec<String>>()
        .join(" ")
}

/// One of the panel's columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Column {
    /// The metric's name, unit, source mark and labels.
    Metric,
    /// The value at this commit.
    Value,
    /// The spread behind it.
    Spread,
    /// The delta against the baseline.
    Delta,
    /// The history line.
    History,
}

/// How many columns the panel draws.
pub const COLUMNS: usize = 5;

/// One column that survived the width budget: which one, how it aligns, and how wide it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    /// Which column.
    pub kind: Column,
    /// How its text is aligned inside it.
    pub align: Align,
    /// How many cells it may use.
    pub width: usize,
}

/// `(column, preferred width, drop order)` in draw order; the *lowest* drop number is dropped first.
///
/// The three columns a reader is scanning for — the metric, its value, its history — hold their room;
/// the two that also live in full on the status line go first. Nothing is lost, only moved: the line
/// under the table carries the selected row's whole sentence whatever the width.
///
/// The five add up to 91, which is a 100-column terminal minus its borders, its gutter and its
/// separators: the panel is built for the narrowest window anyone is likely to open it in, and a wide
/// terminal hands the spare to the metric column, which is the only one that can use an unlimited amount.
/// Each column is at least as wide as the *title* over it, because a cut title (`vs earlier…`) is worse
/// than a cut value — the title is what makes the number mean something.
const PREFERRED: [(Column, usize, u8); COLUMNS] = [
    (Column::Metric, 25, u8::MAX),
    (Column::Value, 14, u8::MAX),
    (Column::Spread, 19, 20),
    (Column::Delta, 15, 10),
    (Column::History, 18, 30),
];

/// Fit the columns into `width` cells, giving up the least informative first and letting the metric
/// column absorb what is spare.
#[must_use]
pub fn layout(width: usize) -> Vec<Slot> {
    let mut dropped: Vec<Column> = Vec::new();
    loop {
        let kept: Vec<(Column, usize)> = PREFERRED
            .iter()
            .filter(|(kind, _, _)| !dropped.contains(kind))
            .map(|(kind, width, _)| (*kind, *width))
            .collect();
        // One space after every column, which is also the gutter the cursor marker sits in.
        let used: usize = kept.iter().map(|(_, width)| width + 1).sum();
        let droppable = PREFERRED
            .iter()
            .filter(|(kind, _, _)| !dropped.contains(kind))
            .filter_map(|(kind, _, drop_first)| {
                if *drop_first == u8::MAX {
                    None
                } else {
                    Some((*kind, *drop_first))
                }
            })
            .min_by_key(|(_, drop_first)| *drop_first)
            .map(|(kind, _)| kind);
        if used > width && droppable.is_none() {
            // Nothing left to give up: the metric column and the value column, the two the ticket says a
            // reader is scanning for, are over budget on their own. Shrink the widest of them, one column
            // at a time, down to its floor — and past the floor if the terminal is that narrow, because a
            // row that runs off the edge is worse than a row that truncates.
            let mut slots: Vec<Slot> = kept
                .iter()
                .map(|(kind, preferred)| Slot {
                    kind: *kind,
                    align: align_of(*kind),
                    width: *preferred,
                })
                .collect();
            let floor = |kind: Column| match kind {
                Column::Value => "1.847e5 n=2".chars().count(),
                Column::Metric => 12,
                _ => 0,
            };
            let mut over = used.saturating_sub(width);
            while over > 0 {
                let Some(index) = slots
                    .iter()
                    .enumerate()
                    .filter(|(_, slot)| slot.width > floor(slot.kind))
                    .max_by_key(|(_, slot)| slot.width)
                    .map(|(index, _)| index)
                else {
                    break;
                };
                slots[index].width -= 1;
                over -= 1;
            }
            // Still over: take it off the metric column whatever the floor says.
            if let Some(first) = slots.first_mut() {
                first.width = first.width.saturating_sub(over);
            }
            return slots;
        }
        if used <= width {
            let spare = width.saturating_sub(used);
            let mut slots: Vec<Slot> = kept
                .iter()
                .map(|(kind, width)| Slot {
                    kind: *kind,
                    align: align_of(*kind),
                    width: *width,
                })
                .collect();
            if let Some(first) = slots.first_mut() {
                first.width += spare;
            }
            return slots;
        }
        dropped.push(droppable.unwrap_or(Column::History));
    }
}

/// The columns a row has to be drawn *to*, rather than drawn and then cut.
///
/// Built from a laid-out row ([`Fit::of`]) so that a cell never has to guess how wide its column turned
/// out to be — the layout is what decides, and a cell that assumed 22 columns would be wrong on the
/// first narrow terminal.
#[derive(Debug, Clone, Copy)]
pub struct Fit {
    /// The value column's width, or 0 when the terminal gave it up.
    pub value: usize,
    /// The spread column's width, or 0 when the terminal gave it up.
    pub spread: usize,
    /// The history column's width, or 0 when the terminal gave it up.
    pub history: usize,
}

impl Fit {
    /// The widths a laid-out row actually has.
    #[must_use]
    pub fn of(slots: &[Slot]) -> Self {
        let width_of = |kind: Column| {
            slots
                .iter()
                .find(|slot| slot.kind == kind)
                .map_or(0, |slot| slot.width)
        };
        Fit {
            value: width_of(Column::Value),
            spread: width_of(Column::Spread),
            history: width_of(Column::History),
        }
    }
}

/// Downsample a history to at most `room` points for a column narrower than the history is long.
///
/// The endpoints are never dropped, and the interior is taken at even stride: the first and last
/// commits in a window are the two points a reader is actually comparing, and a sample that dropped
/// one of them would quietly change what the shape claims.
#[must_use]
fn sample(values: &[f64], room: usize) -> Vec<f64> {
    if values.len() <= room || room == 0 {
        return values.to_vec();
    }
    if room == 1 {
        return vec![*values.last().expect("non-empty above")];
    }
    let last = values.len() - 1;
    (0..room)
        .map(|index| {
            let source = (index * last + room / 2) / (room - 1);
            values[source.min(last)]
        })
        .collect()
}

/// A column's alignment: numbers right-aligned so the digits line up, text left.
#[must_use]
const fn align_of(kind: Column) -> Align {
    match kind {
        Column::Value | Column::Spread | Column::Delta => Align::Right,
        Column::Metric | Column::History => Align::Left,
    }
}

/// Lay `cells` out in `slots`, cutting each to its column with the palette's marker.
fn join(slots: &[Slot], cells: &[String], head: &str, palette: &Palette) -> String {
    let mut out = String::from(head);
    for (slot, text) in slots.iter().zip(cells) {
        out.push_str(&fit(text, slot.width, slot.align, palette.ellipsis()));
        out.push(' ');
    }
    out.trim_end().to_owned()
}

/// A percent with no trailing zeros, because it sits in a column title: `5%`, not `5.0%`.
#[must_use]
fn trim_percent(percent: f64) -> String {
    format!("{percent}").trim_end_matches(".0").to_owned()
}

/// One string field of a stored row.
fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// One integer field of a stored row, as a count.
fn count(value: &Value, key: &str) -> Option<usize> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|count| usize::try_from(count).ok())
}

/// Seven hex, ADR-0008 §6.2's display form.
#[must_use]
pub fn short(sha: &str) -> String {
    sha.chars().take(7).collect()
}
