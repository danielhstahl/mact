//! The row: one commit's numbers, and the arithmetic allowed to produce them (ADR-0008 §3–§5).
//!
//! [`CommitMetrics`] is *the* thing this reader exists to make: one typed row per SHA, whose JSON
//! is byte-comparable with what `scripts/metric_model_reference.py` derives and whose field list is
//! the model's `row_shape`. Two of its rules are worth stating at the type level:
//!
//! * **No `null` anywhere.** An unknown value is an *absent key* (`skip_serializing_if`), never a
//!   null and never a zero — ADR-0002 §2.1 for records, ADR-0006 §6.3 for stored rows, ADR-0008
//!   §6.1 for these. A benchmark group with no unit has no `unit` key at all rather than a null
//!   one, and a commit with no coverage has no `coverage` key.
//! * **A `0` is always a measurement.** `percent: 0.0` means "measured, and nothing was hit";
//!   vacuous (`found == 0`) has no percent and the gap `vacuous`. Those two render `0.0` and `—`,
//!   and a renderer that conflates them has already lied about an empty generated file.
//!
//! The arithmetic lives here rather than in the pipeline so there is exactly one implementation of
//! each: [`round_half_away`] (the only rounding coverage ever gets), [`median`] (the only
//! aggregation a benchmark group gets), and [`canonical_json`] (the label identity a group key is
//! built from).

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Number;

use super::vocab::{ActionState, Caveat, GapCode, Measure};

/// One commit's derived numbers.
///
/// The field list is `row_shape.row` in `schema/metric-model.json`, and it is closed in both
/// directions: a consumer may assume nothing else is present, and this reader may not add a field
/// without adding it to the model, the ADR and the golden vectors in the same commit.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CommitMetrics {
    /// The full 40-hex lowercase SHA — the join key, never truncated (ADR-0008 §6.2).
    pub commit: String,
    /// Exactly one of the four states, always present: a commit with no evidence is
    /// `no_evidence`, which is unknown, not a failure.
    pub action_success: ActionState,
    /// The pooled coverage, absent when there is no number (see `gaps.coverage`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<Coverage>,
    /// Every benchmark group that survived retention, in `(name, labels, unit)` order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub benchmarks: Vec<BenchmarkGroup>,
    /// One entry per `(kind, workflow, task)` group: what each one's newest run said.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub action_detail: Vec<ActionGroup>,
    /// Runs that happened at this commit and were not used, with the reason.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub excluded_runs: Vec<ExcludedRun>,
    /// Older runs of a group whose newest won — kept for the spread, never the headline.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub superseded_runs: Vec<String>,
    /// Records present for this commit that this build could not read. Non-zero is the difference
    /// between "nothing ran" and "I could not read it" — both unknown, both not a failure, and
    /// different things for a user to do something about.
    #[serde(skip_serializing_if = "is_zero")]
    pub unreadable_records: u64,
    /// Why each column that has no number has no number. Always present, and `{}` when both
    /// columns produced numbers.
    pub gaps: Gaps,
    /// The caveats behind this row's `*` markers, in the order the derivation found them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub caveats: Vec<Caveat>,
}

impl CommitMetrics {
    /// The gap behind one column, `None` when that column has a number.
    #[must_use]
    pub fn gap(&self, column: super::vocab::Column) -> Option<GapCode> {
        match column {
            super::vocab::Column::Coverage => self.gaps.coverage,
            super::vocab::Column::Benchmarks => self.gaps.benchmarks,
        }
    }

    /// Whether `caveat` is on this row.
    #[must_use]
    pub fn has_caveat(&self, caveat: Caveat) -> bool {
        self.caveats.contains(&caveat)
    }

    /// Whether nothing at all was readable for this commit.
    ///
    /// This is deliberately *not* `gaps` being full: a commit can have readable records, an action
    /// state of `success`, and still have no coverage number.
    #[must_use]
    pub fn is_no_evidence(&self) -> bool {
        self.action_success == ActionState::NoEvidence
    }

    /// Whether this commit is "unknown because it could not be read" rather than "unknown because
    /// nothing ran" — the one distinction ADR-0008 §5.4 says the contract can still make.
    #[must_use]
    pub fn is_unreadable(&self) -> bool {
        self.unreadable_records > 0
    }
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// `coverage` in a row: one pooled number, the counts it came from, and the runs that fed it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Coverage {
    /// `round_half_away(100 · Σhit / Σfound, 1)` — from the raw counts, once, never from a rounded
    /// intermediate and never an average of percentages (ADR-0008 §3.1).
    pub percent: f64,
    /// The pooled denominator the rate was computed against.
    pub found: u64,
    /// The pooled numerator.
    pub hit: u64,
    /// Which measure this number is: `lines` is not `statements` (ADR-0008 §3.5).
    pub measure: Measure,
    /// The selected runs whose totals were pooled. A list of ids, distinct from a benchmark group's
    /// `run_count`, which is a count of something else.
    pub runs: Vec<String>,
    /// Other measures with a non-zero pooled denominator, pooled the same way. Never pooled *with*
    /// the primary number, and the reason the row carries `secondary_measures`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub secondary: Vec<SecondaryMeasure>,
}

/// One non-primary measure's pooled number (ADR-0008 §3.5).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SecondaryMeasure {
    /// Which measure.
    pub measure: Measure,
    /// Its own pooled rate.
    pub percent: f64,
    /// Its own pooled denominator.
    pub found: u64,
    /// Its own pooled numerator.
    pub hit: u64,
}

/// How a run's coverage total was known: its own `scope:"total"` record, or this reader's sum of
/// its file records (ADR-0008 §3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TotalSource {
    /// The run published a `scope:"total"` record, and that record *is* the run's total.
    ScopeTotal,
    /// The run published no total, so its file records were summed and the row says so.
    SumOfFileRecords,
}

impl TotalSource {
    /// The stored spelling — the same two strings `serde` writes, so a store section and a row cannot
    /// drift into two spellings of one fact (ADR-0006 §6.4's `coverage_files` keeps this one).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ScopeTotal => "scope_total",
            Self::SumOfFileRecords => "sum_of_file_records",
        }
    }
}

/// One benchmark group at one commit (ADR-0008 §4.2).
///
/// Group identity is `(name, labels, unit)` — an absent unit is its own key — which is what makes
/// "values from two groups are never compared" a structural fact rather than a rule a renderer has
/// to remember.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BenchmarkGroup {
    /// The metric name, verbatim.
    pub name: String,
    /// The label set, always present (empty when the record carried none) so a `group by` never
    /// type-switches.
    pub labels: BTreeMap<String, String>,
    /// Whether this group's values may be diffed or plotted: false exactly when it has no unit,
    /// which makes it displayable and incomparable (ADR-0008 §4.3).
    pub comparable: bool,
    /// `true` for `act_log`-derived rows (`job_duration_s`, `step_duration_s`): they describe the
    /// run, not the code, and are never a regression signal by default.
    pub context: bool,
    /// The median over the samples of the newest selected run. Kept as the JSON number it was, so
    /// an integral metric prints as `3` and a mean of two samples prints as `184666.55`.
    pub value: Number,
    /// How many records of this group the newest selected run published — the noise estimate, not
    /// a duplicate count.
    pub samples: usize,
    /// Spread low, over *every* retained run of this group at this commit.
    pub min: Number,
    /// Spread high, same population.
    pub max: Number,
    /// How many distinct retained runs that spread came from. `runs: 1` with `min == max` is an
    /// unmeasured noise floor, not a stable metric.
    pub run_count: usize,
    /// The run `value` came from — the newest selected one.
    pub run_id: String,
    /// That run's `source` (one value: a group's newest records are one run).
    pub source: String,
    /// That run's flags on those records, sorted and deduped.
    pub flags: Vec<String>,
    /// Absent ⇔ `comparable: false` ⇔ the `unit_missing` caveat. Never `null` (ADR-0002 §2.1).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

/// One `(kind, workflow, task)` group's verdict, so a `◐` is always explainable (§5.3).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ActionGroup {
    /// `benchmark` or `coverage`.
    pub kind: String,
    /// The workflow identity the record named.
    pub workflow: String,
    /// The task (job id) the record named.
    pub task: String,
    /// The classification that caused the evidence — which column's workflow this was.
    pub category: String,
    /// The newest run's `run_status`, verbatim.
    pub status: String,
    /// The newest run's id.
    pub run_id: String,
    /// Whether that verdict was a pass.
    pub ok: bool,
}

/// A run that happened at this commit and was not used (§3.7).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExcludedRun {
    /// Which run.
    pub run_id: String,
    /// What it ended as — `failed`, `cancelled`, `timed_out`, `error`.
    pub status: String,
    /// Why it was excluded (`run_status_excluded` or `run_truncated`).
    pub reason: super::vocab::DropReason,
}

/// `gaps` in a row: one code per column with no number. Always present, often `{}`.
///
/// Only the two columns this ADR owns are here; ADR-0006 §6.4's `no_evidence`/`no_mi`/
/// `unanalysed_paths` belong to the store row that embeds this one, and are added there.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Gaps {
    /// Why `coverage` has no number, if it has none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<GapCode>,
    /// Why `benchmarks` has no rows, if it has none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub benchmarks: Option<GapCode>,
}

impl Gaps {
    /// Whether nothing is missing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.coverage.is_none() && self.benchmarks.is_none()
    }
}

// ------------------------------------------------------------------ the arithmetic, once

/// Round `value` to `decimals`, half away from zero, going through the shortest round-trip decimal
/// rather than scaling in binary.
///
/// Going through the decimal repr is what keeps `86.85` from deciding its own rounding by accident
/// of its binary representation, and it is what the python reference does with `Decimal(repr(v))`.
/// This is the only rounding coverage ever gets (ADR-0008 §3.1): a second rounding of a displayed
/// number is how `86.949999` becomes `87.0`.
#[must_use]
pub fn round_half_away(value: f64, decimals: u32) -> f64 {
    if !value.is_finite() || decimals > 15 {
        return value;
    }
    let repr = format!("{value}");
    let (sign, rest) = match repr.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", repr.as_str()),
    };
    let (int_part, frac_part) = match rest.split_once('.') {
        Some((int, frac)) => (int, frac),
        None => (rest, ""),
    };
    if frac_part.len() as u32 <= decimals || int_part.is_empty() {
        return value;
    }
    let keep = &frac_part[..decimals as usize];
    let rounder = frac_part.as_bytes()[decimals as usize];

    let mut digits: Vec<u8> = Vec::with_capacity(int_part.len() + keep.len());
    for byte in int_part.bytes().chain(keep.bytes()) {
        if byte.is_ascii_digit() {
            digits.push(byte - b'0');
        }
    }
    if digits.is_empty() {
        digits.push(0);
    }
    if rounder >= b'5' {
        let mut index = digits.len();
        let mut carry = true;
        while carry && index > 0 {
            index -= 1;
            let bumped = digits[index] + 1;
            if bumped > 9 {
                digits[index] = 0;
            } else {
                digits[index] = bumped;
                carry = false;
            }
        }
        if carry {
            digits.insert(0, 1);
        }
    }
    let int_len = digits.len() - keep.len();
    let text: String = digits
        .iter()
        .map(|digit| char::from(b'0' + digit))
        .collect();
    let (int_digits, frac_digits) = text.split_at(int_len);
    let rounded = format!("{sign}{int_digits}.{frac_digits}");
    rounded.parse::<f64>().unwrap_or(value)
}

/// The median of `values`, ascending: the middle for an odd count, and the mean of the two middle
/// values for an even one — which is what `docs/evidence-format.md` recipe 6 prints and what
/// ADR-0008 §4.2 pins.
///
/// A median is allowed to be a number no sample ever had (`(184320.4 + 185012.7) / 2 =
/// 184666.55`); that is what a median is. An odd count returns the sample's own JSON number,
/// so an integral metric stays integral in the row.
///
/// [`None`] for an empty population: a median of nothing is not zero.
#[must_use]
pub fn median(values: &[Number]) -> Option<Number> {
    if values.is_empty() {
        return None;
    }
    let mut order: Vec<usize> = (0..values.len()).collect();
    order.sort_by(|left, right| {
        number_f64(&values[*left])
            .partial_cmp(&number_f64(&values[*right]))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let middle = values.len() / 2;
    if values.len() % 2 == 1 {
        return Some(values[order[middle]].clone());
    }
    let low = number_f64(&values[order[middle - 1]]);
    let high = number_f64(&values[order[middle]]);
    Number::from_f64((low + high) / 2.0)
}

/// A JSON number as an `f64`, for ordering and for the even-*n* mean. Every number this type can
/// hold is finite, so the fallback is unreachable; returning `0.0` keeps the helper infallible.
fn number_f64(value: &Number) -> f64 {
    value.as_f64().unwrap_or(0.0)
}

/// Canonical JSON for a labels object: keys ascending, no insignificant whitespace (ADR-0002
/// §6.1). A group key compares this string, so `{job:"bench",os:"x"}` and `{job:"bench"}` are two
/// groups and a `group by` cannot be order-sensitive.
#[must_use]
pub fn canonical_json(labels: &BTreeMap<String, String>) -> String {
    serde_json::to_string(labels).unwrap_or_else(|_| "{}".to_owned())
}

/// The shortest round-trip spelling of a JSON number, for display and for a spread's `[min,max]`.
///
/// Storage keeps the number and prints it this way — never a re-rounded one. A renderer that wants
/// four significant digits does it at draw time (ADR-0008 §4.2), because a stored value that has
/// already been rounded cannot be diffed or averaged later without a second rounding error.
#[must_use]
pub fn number_text(value: &Number) -> String {
    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::{canonical_json, median, number_text, round_half_away, Number};

    /// Every expected value below was read off `scripts/metric_model_reference.py` — the oracle this
    /// crate is graded against — so these tests pin agreement with it, not with my taste.
    #[test]
    fn rounding_is_half_away_from_zero_through_the_shortest_repr() {
        // 86.85 is the trap: its binary double is 86.8499999999999943, so scaling in binary and
        // rounding would answer 86.8. The decimal repr answers 86.9, which is what a reader expects
        // and what the model's `percent_decimals: 1` means (ADR-0008 §3.1).
        assert_eq!(round_half_away(86.85, 1), 86.9);
        assert_eq!(round_half_away(2.675, 2), 2.68);
        assert_eq!(round_half_away(0.25, 1), 0.3);
        assert_eq!(
            round_half_away(-1.25, 1),
            -1.3,
            "away from zero, not toward +∞"
        );

        // The rates the sample tree produces.
        assert_eq!(round_half_away(100.0 * 5485.0 / 6330.0, 1), 86.7);
        assert_eq!(round_half_away(100.0 * 3581.0 / 4120.0, 1), 86.9);
        assert_eq!(round_half_away(100.0 * 1904.0 / 2210.0, 1), 86.2);
        assert_eq!(round_half_away(33.333_333_333_333_33, 1), 33.3);
        assert_eq!(
            round_half_away(99.949_999_9, 1),
            99.9,
            "a 4 rounds down, whatever follows it"
        );

        // Already-short values, and the zero that a measured-none-covered run prints.
        assert_eq!(round_half_away(86.0, 1), 86.0);
        assert_eq!(round_half_away(12.5, 1), 12.5);
        assert_eq!(round_half_away(0.0, 1), 0.0);
        assert_eq!(round_half_away(1.0000000000000002, 1), 1.0);
    }

    #[test]
    fn rounding_never_invents_a_digit_that_was_not_there() {
        // More requested decimals than the value has: the value comes back unchanged, and a caller
        // cannot round its way to a false precision.
        assert_eq!(round_half_away(0.5, 6), 0.5);
        assert_eq!(round_half_away(7.0, 4), 7.0);
        assert!(round_half_away(f64::NAN, 1).is_nan());
        assert_eq!(round_half_away(f64::INFINITY, 1), f64::INFINITY);
    }

    #[test]
    fn median_is_the_middle_or_the_mean_of_the_two_middles() {
        let numbers = |values: &[f64]| -> Vec<Number> {
            values
                .iter()
                .map(|value| Number::from_f64(*value).expect("a finite f64 is a JSON number"))
                .collect()
        };
        assert_eq!(
            median(&numbers(&[185_012.7, 184_320.4])).and_then(|n| n.as_f64()),
            Some(184_666.55),
            "an even count is the mean of the two middle values, per recipe 6"
        );
        assert_eq!(
            median(&numbers(&[3.0, 1.0, 2.0])).and_then(|n| n.as_f64()),
            Some(2.0),
            "input order is irrelevant"
        );
        assert_eq!(
            median(&numbers(&[3.066, 1.5, 299.4])).and_then(|n| n.as_f64()),
            Some(3.066)
        );
        assert_eq!(
            median(&numbers(&[3.0])).and_then(|n| n.as_f64()),
            Some(3.0),
            "one sample is its own median, and it is the commonest case in a real tree"
        );
        assert_eq!(
            median(&[]),
            None,
            "an empty population has no median, not 0"
        );
    }

    #[test]
    fn an_integral_value_keeps_its_integer_spelling() {
        // A spread's `[min,max]` is printed from these numbers, and recipe 5 prints `3`, not `3.0`.
        assert_eq!(number_text(&Number::from(3)), "3");
        assert_eq!(number_text(&Number::from_f64(3.0).unwrap()), "3.0");
        assert_eq!(
            number_text(&Number::from_f64(184_666.55).unwrap()),
            "184666.55"
        );
    }

    #[test]
    fn labels_keys_are_canonicalised_for_the_group_key() {
        let mut labels = std::collections::BTreeMap::new();
        labels.insert("os".to_owned(), "ubuntu-24.04".to_owned());
        labels.insert("job".to_owned(), "hyperfine-bench".to_owned());
        assert_eq!(
            canonical_json(&labels),
            r#"{"job":"hyperfine-bench","os":"ubuntu-24.04"}"#,
            "insertion order must not reach the key (ADR-0002 §6.1)"
        );
        assert_eq!(canonical_json(&std::collections::BTreeMap::new()), "{}");
    }
}
