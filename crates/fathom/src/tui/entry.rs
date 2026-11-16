//! One commit, reduced to what the timeline can say about it.
//!
//! [`Entry`] is the boundary between the store (JSON sections, ADR-0006 §6.4) and the screen. Two
//! properties matter and both exist to stop a screen from inventing a number:
//!
//! * **It is glyph-free.** Every field is a value, a count or an explicit `None`; the spelling of
//!   `—` / `▲` / `✓` happens in [`crate::tui::cells`] against the model's glyph table. A test can
//!   therefore assert what a commit *knows* without also asserting which terminal it is drawn on, and
//!   `--plain` and the TUI cannot disagree about a value.
//! * **Unknown is `None`, never zero** (ADR-0008 §6.1). A row with no `evidence` section yields
//!   `coverage: Metric::UNKNOWN`, not `0.0`; a root commit yields `delta: None`, not `Flat`. The
//!   renderer has no way to draw a placeholder as a measurement because there is no placeholder to
//!   draw — an absent section becomes an absent field here, in exactly one place.
//!
//! The deltas are computed here rather than at draw time because they are not a formatting decision:
//! ADR-0008 §6.3 pins *which* parent a delta is against (the first parent, no inheritance in either
//! direction), and an arrow is only meaningful when both sides exist. [`Entry`] holds the raw parent
//! sha so [`crate::tui::store_view`] can resolve it against the rows it loaded and set the arrow only
//! then — a commit whose parent was not indexed has no arrow, which is the honest answer and the one
//! the ADR demands.

use crate::metrics::vocab::ActionState;

/// A change against the first parent, as an arrow and nothing else (ADR-0008 §6.1).
///
/// `Flat` means "exactly equal at the stored precision", which is a finding; the absence of a `Delta`
/// means "no arrow, because one side is missing" — a root commit, or a parent outside the window. The
/// two are distinct types of sentence and the type says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delta {
    /// The value rose against the parent.
    Up,
    /// The value fell against the parent.
    Down,
    /// The two are equal at the stored precision.
    Flat,
}

/// A number column: the value, the arrow, and whether a caveat sits behind it.
///
/// `caveat` is the `*`. ADR-0008 §6.1 says a marker with nothing behind it is a derivation bug, so
/// this field is set from the row's own `caveats` list (per-column membership comes from
/// `schema/metric-model.json`, not from a guess) and from ADR-0007 §4.4's `unanalysed_paths` for the
/// MI column.
#[derive(Debug, Clone, PartialEq)]
pub struct Metric {
    /// The stored number, absent when there is none. A `Some(0.0)` is a measured zero.
    pub value: Option<f64>,
    /// The arrow versus the first parent, absent when there is no both-sided comparison.
    pub delta: Option<Delta>,
    /// Whether this column carries a `*`.
    pub caveat: bool,
    /// The gap code naming *why* there is no number, when there is none (`—` is four different
    /// answers: nothing ran, it was unreadable, it was filtered out, it was vacuous).
    pub gap: Option<String>,
}

impl Metric {
    /// The unknown metric: no value, no arrow, no caveat, no gap yet.
    pub const UNKNOWN: Self = Self {
        value: None,
        delta: None,
        caveat: false,
        gap: None,
    };

    /// A measured value.
    #[must_use]
    pub const fn measured(value: f64) -> Self {
        Self {
            value: Some(value),
            delta: None,
            caveat: false,
            gap: None,
        }
    }

    /// The gap-only metric: nothing was measured, and this is the code that says why.
    #[must_use]
    pub fn missing(gap: &str) -> Self {
        Self {
            value: None,
            delta: None,
            caveat: false,
            gap: Some(gap.to_owned()),
        }
    }

    /// Whether the column has a number to draw.
    #[must_use]
    pub const fn is_known(&self) -> bool {
        self.value.is_some()
    }
}

/// The `+`/`−` column: churn, and the noise half of it labelled rather than hidden (ADR-0007 §7.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Churn {
    /// Lines added across the whole diff, `None` when the row has no diffstat at all.
    pub add: Option<u64>,
    /// Lines deleted.
    pub del: Option<u64>,
    /// How many files the diff touched, when known.
    pub files: Option<u64>,
    /// The noise half of `add` — counted in it, and shown separately because ADR-0006 §7.2's rule is
    /// that noise is labelled, never hidden.
    pub noise_add: u64,
    /// The noise half of `del`.
    pub noise_del: u64,
    /// Whether the per-file list was truncated by the reader's cap, which makes `files` a floor
    /// rather than a count.
    pub truncated: bool,
}

impl Churn {
    /// The unknown churn: a row with no `history` section.
    pub const UNKNOWN: Self = Self {
        add: None,
        del: None,
        files: None,
        noise_add: 0,
        noise_del: 0,
        truncated: false,
    };

    /// Whether nothing at all is known.
    #[must_use]
    pub const fn is_unknown(&self) -> bool {
        self.add.is_none() && self.del.is_none()
    }
}

/// The `bench` column: **one** group, chosen deterministically (ADR-0008 §4.5).
///
/// The row keeps every group that survived the `fathom.toml` filter and a timeline cell cannot show
/// fourteen, so the rule is: sort by `(name, canon(labels))`, take the first that is comparable and
/// not context, else the first of any. The sort is by name and not by "biggest delta" precisely so the
/// column means the same thing on every row — a column that changed which metric it displayed with the
/// data would be a column that silently changed what it means.
#[derive(Debug, Clone, PartialEq)]
pub struct Bench {
    /// The chosen group's identity: `name` plus its labels, for the detail line and the delta test.
    pub name: String,
    /// The label set, canonicalised (`{}` when empty), so a delta only compares a group to itself.
    pub labels: String,
    /// The value at four significant digits, already joined to its unit: `412.5ms`, `3 ops/s`.
    pub value: String,
    /// Whether the group's unit is unknown: displayable, never comparable (ADR-0008 §4.3).
    pub comparable: bool,
    /// Whether it is `act_log`-derived context, which earns the `~` prefix (ADR-0008 §4.4).
    pub context: bool,
    /// How many *other* groups the row carries — the `+N`.
    pub rest: usize,
    /// Whether this column carries a `*`.
    pub caveat: bool,
    /// The arrow versus the same group at the first parent, absent unless both sides exist and the
    /// group identity matches.
    pub delta: Option<Delta>,
    /// The gap code when there is no group at all.
    pub gap: Option<String>,
    /// The stored number the text was made from, kept so a delta can compare two commits' numbers
    /// instead of two strings. `None` for a unitless group: displayable, never comparable (§4.3).
    pub raw: Option<f64>,
}

impl Bench {
    /// The unknown benchmark column.
    #[must_use]
    pub fn missing(gap: Option<&str>) -> Self {
        Self {
            name: String::new(),
            labels: String::new(),
            value: String::new(),
            comparable: false,
            context: false,
            rest: 0,
            caveat: false,
            delta: None,
            gap: gap.map(str::to_owned),
            raw: None,
        }
    }

    /// Whether there is a group to show.
    #[must_use]
    pub const fn is_known(&self) -> bool {
        !self.value.is_empty()
    }

    /// The group's identity as a delta key: two commits may only be compared inside one group.
    #[must_use]
    pub fn identity(&self) -> String {
        format!("{}\0{}\0", self.name, self.labels)
    }
}

/// One row of the timeline.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// The full 40-hex SHA (ADR-0008 §6.2). Display truncates; identity never does.
    pub sha: String,
    /// The first parent, when the row names one. `None` is a root commit: no arrow anywhere.
    pub parent: Option<String>,
    /// Author instant, Unix seconds — the clock the `age` column counts back from.
    pub author_utc: i64,
    /// Committer instant when the row carries one; the clock the *ordering* uses, because that is the
    /// order `git log` prints. Equal to `author_utc` on a row with no history section.
    pub committer_utc: i64,
    /// The message head.
    pub subject: String,
    /// The author name, as written (unicode arrives here untouched).
    pub author: String,
    /// The coverage column (ADR-0008 §3).
    pub coverage: Metric,
    /// The churn column (ADR-0007 §7.1's file set, totals with noise labelled).
    pub churn: Churn,
    /// The MI column (ADR-0007 §4).
    pub mi: Metric,
    /// How many paths the MI could not analyse, ADR-0007 §4.4's caveat counter.
    pub unanalysed_paths: u64,
    /// The benchmark column (ADR-0008 §4.5).
    pub bench: Bench,
    /// The action column (ADR-0008 §5.3's four states).
    pub action: ActionState,
    /// Whether this commit has a row at all. `false` is a commit the index has not reached: every
    /// column is unknown and the header says why, rather than the row pretending to be a measurement.
    pub indexed: bool,
    /// The row's caveat codes, in the model's spelling. The table shows one `*` per column and this is
    /// what the line under it names (ADR-0008 §6.1: a marker with nothing behind it is a bug).
    /// Which of them qualify *which* column is the model's answer rather than this row's — see
    /// [`crate::tui::cells::caveat_line`], which resolves the membership from the model.
    pub caveats: Vec<String>,
}

impl Entry {
    /// Seven hex — the display form ADR-0008 §6.2 pins.
    #[must_use]
    pub fn short(&self) -> String {
        self.sha.chars().take(7).collect()
    }

    /// The instant the timeline orders by (committer when known, author otherwise).
    #[must_use]
    pub fn sort_key(&self) -> i64 {
        self.committer_utc.max(self.author_utc)
    }

    /// The text the `/` filter matches against: short SHA, full SHA, author and message (the ticket's
    /// four fields, case-insensitively; the caller lowercases once).
    #[must_use]
    pub fn haystack(&self) -> String {
        let mut text = self.sha.clone();
        text.push('\n');
        text.push_str(&self.subject);
        text.push('\n');
        text.push_str(&self.author);
        text.make_ascii_lowercase();
        text
    }

    /// Whether every metric column is unknown — the "all-unknown row" the ticket's snapshot list asks
    /// for, and the state of a commit the index walked but found nothing about.
    #[must_use]
    pub fn all_unknown(&self) -> bool {
        !self.coverage.is_known()
            && self.churn.is_unknown()
            && !self.mi.is_known()
            && !self.bench.is_known()
            && self.action == ActionState::NoEvidence
    }

    /// Whether this row carries at least one caveat.
    #[must_use]
    pub fn has_caveats(&self) -> bool {
        !self.caveats.is_empty()
    }
}

/// Format a duration as the `age` column spells it: minutes, hours, days, weeks, years.
///
/// Monotone and total: any `delta` ≥ 0 has exactly one spelling, and a future-dated commit (clock
/// skew, a badly set `--date`) reads as brand new rather than as a negative number.
#[must_use]
pub fn age(delta_seconds: i64) -> String {
    let seconds = delta_seconds.max(0);
    if seconds < 3_600 {
        return format!("{}m", seconds / 60);
    }
    if seconds < 48 * 3_600 {
        return format!("{}h", seconds / 3_600);
    }
    if seconds < 7 * 86_400 {
        return format!("{}d", seconds / 86_400);
    }
    if seconds < 52 * 7 * 86_400 {
        return format!("{}w", seconds / (7 * 86_400));
    }
    // Past 52 weeks the unit is years, floored — and never `0y`, which would be a unit that lies twice
    // over: a commit 364 days old is a year old for a reader's purposes, and `52w` is the last thing
    // the week column ever says. Monotone either way: the spelling only ever moves forward with `delta`.
    format!("{}y", (seconds / (365 * 86_400)).max(1))
}

/// A number at four significant digits (ADR-0008 §4.5's "render its value at 4 significant digits").
///
/// Stays positional where positional is readable and switches to `1.847e5` beyond it, because a
/// timeline cell that grows to eleven characters to print a nanosecond count pushes the message column
/// off the screen. Trailing zeros are trimmed, so a group that measured exactly `3` prints `3` and not
/// `3.000` — the same reason the stored number keeps its own spelling (`metrics::row::number_text`).
#[must_use]
pub fn four_significant(value: f64) -> String {
    if !value.is_finite() {
        return String::new();
    }
    if value == 0.0 {
        return "0".to_owned();
    }
    let magnitude = value.abs().log10().floor();
    // Four significant digits means the last kept decimal is 3 − magnitude.
    let decimals = 3.0 - magnitude;
    if !(-3.0..=6.0).contains(&magnitude) || decimals < 0.0 {
        let mantissa = value / 10f64.powf(magnitude);
        let text = format!("{mantissa:.3}");
        let trimmed = trim_zeros(&text);
        return format!("{trimmed}e{}", magnitude as i32);
    }
    trim_zeros(&format!("{value:.*}", decimals as usize))
}

fn trim_zeros(text: &str) -> String {
    if !text.contains('.') {
        return text.to_owned();
    }
    let trimmed = text.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() || trimmed == "-" {
        "0".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// The direction of `now − then`, at the precision the column stores.
///
/// The comparison is on the *stored* digits, which is what makes `·` mean "a tie at the stored
/// precision" rather than "a tie in some float space nobody chose" (ADR-0008 §6.1).
#[must_use]
pub const fn delta_of(now: f64, parent: f64) -> Delta {
    if now > parent {
        Delta::Up
    } else if now < parent {
        Delta::Down
    } else {
        Delta::Flat
    }
}

#[cfg(test)]
mod tests {
    use super::{age, delta_of, four_significant, Delta, Metric};

    #[test]
    fn age_uses_the_largest_unit_under_100() {
        assert_eq!(age(0), "0m");
        assert_eq!(age(59 * 60 + 59), "59m");
        assert_eq!(age(2 * 3_600), "2h");
        assert_eq!(age(47 * 3_600 + 59), "47h");
        assert_eq!(age(48 * 3_600), "2d");
        assert_eq!(age(6 * 86_400), "6d");
        assert_eq!(age(7 * 86_400), "1w");
        assert_eq!(age(51 * 7 * 86_400), "51w");
        assert_eq!(age(52 * 7 * 86_400), "1y");
        assert_eq!(age(-30), "0m", "a future-dated commit is new, not negative");
    }

    #[test]
    fn four_significant_keeps_four_digits_and_trims_the_rest() {
        assert_eq!(four_significant(412.5), "412.5");
        assert_eq!(four_significant(3.0), "3");
        assert_eq!(four_significant(0.0), "0");
        assert_eq!(four_significant(184_666.55), "1.847e5");
        assert_eq!(four_significant(1_234.0), "1234");
        assert_eq!(four_significant(99.999), "100");
        assert_eq!(four_significant(0.012_345_6), "0.01235");
        assert_eq!(four_significant(-7.5), "-7.5");
        assert_eq!(four_significant(f64::NAN), "");
    }

    #[test]
    fn a_delta_is_a_direction_and_the_absence_of_one_is_not_flat() {
        assert_eq!(delta_of(90.0, 87.4), Delta::Up);
        assert_eq!(delta_of(79.8, 82.1), Delta::Down);
        assert_eq!(delta_of(87.4, 87.4), Delta::Flat);
        assert!(Metric::UNKNOWN.delta.is_none(), "no parent is not a tie");
    }
}
