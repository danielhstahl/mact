//! The arithmetic: `V`, `G`, `MI`, the rounding, the clamp and the bands — ADR-0007 §2, in Rust.
//!
//! This module is the port of [`scripts/mi_reference.py`](../../../../scripts/mi_reference.py), and
//! the port is the point of the ticket: the python file is the oracle the ADR names, so the rules
//! here are the rules there, operation for operation.
//!
//! Three things make two implementations of a logarithm agree to a stored digit:
//!
//! * every operation is an IEEE-754 binary64 operation, `ln` is `f64::ln` and `log2` is the platform
//!   `f64::log2` — *not* `ln(n)/ln(2)`, which differs by ULPs (P2);
//! * the stored and compared value is the rounded one, `MI_1(x) = round_half_away(x*10)/10`, applied
//!   to the product: `62.449999999999996` and `62.450000000000003` both become `62.5`, because both
//!   multiply to exactly `624.5`. `f64::round` rounds half away from zero, which is the whole
//!   definition;
//! * nothing that is not rounded is ever compared, so a 1 ULP difference in a different `libm` cannot
//!   reach a band, a watchlist membership, a hotspot order or a cache key.
//!
//! `MI_raw` is kept as a diagnostic and nothing more: the vectors pin it to a relative `1e-9`, which
//! is the room a different math library gets and no more.

use serde::Serialize;

use super::definition::Definition;
use super::lang::Counts;

/// The seven rungs of the measurability ladder, in the order the ladder is evaluated (P7).
///
/// The names are ADR-0006's row `counts` keys, adopted verbatim so a cached block, a stored row and
/// this analyzer cannot call the same failure three things. Four of them mean "not analysed" and are
/// the paths the view must announce; which four is the model's `in_caveat_count` flag, not a guess
/// made here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Not a source file at all, or under `paths.exclude`.
    Excluded,
    /// Matched `paths.noise`, or is a test file while `paths.include_tests` is false.
    SkippedNoise,
    /// An extension on the source list whose language has no front end.
    UnsupportedLanguage,
    /// Larger than `index.file_size_cap_mib`.
    Oversized,
    /// Not lexable: bad UTF-8, a NUL byte, an unterminated literal, comment or bracket.
    ParseError,
    /// Lexed cleanly, but `LOC == 0` or the vocabulary `n < 2`: there is no logarithm to take.
    Vacuous,
    /// There is a number.
    Ok,
}

impl Status {
    /// The spelling in a stored row, a `doctor` line and a vector.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Excluded => "excluded",
            Self::SkippedNoise => "skipped_noise",
            Self::UnsupportedLanguage => "unsupported_language",
            Self::Oversized => "oversized",
            Self::ParseError => "parse_error",
            Self::Vacuous => "vacuous",
            Self::Ok => "ok",
        }
    }

    /// Parse a reason back. The vocabulary is closed, so an unknown spelling is an error rather than
    /// a default that would silently become `ok`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "excluded" => Some(Self::Excluded),
            "skipped_noise" => Some(Self::SkippedNoise),
            "unsupported_language" => Some(Self::UnsupportedLanguage),
            "oversized" => Some(Self::Oversized),
            "parse_error" => Some(Self::ParseError),
            "vacuous" => Some(Self::Vacuous),
            "ok" => Some(Self::Ok),
            _ => None,
        }
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Status {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s).ok_or_else(|| format!("not an MI reason: {s:?}"))
    }
}

/// The declared traffic lights: green ≥ 50, amber ≥ 25, red < 25, and an exact edge belongs to the
/// better band (P10). The edges come from the model; the names are here because a view needs an enum
/// and not a float.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Band {
    /// `mi >= amber_below`.
    Green,
    /// `red_below <= mi < amber_below`.
    Amber,
    /// `mi < red_below`.
    Red,
}

impl Band {
    /// The spelling a row and a screen use.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Green => "green",
            Self::Amber => "amber",
            Self::Red => "red",
        }
    }
}

impl std::fmt::Display for Band {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The stored rounding: `MI_1(x) = round_half_away(x * 10^d) / 10^d`, with `d` from the model.
///
/// Rounding the *product* is the definition, not an implementation detail: it is why
/// `62.449999999999996` and `62.450000000000003` agree, and a port that reached for a decimal
/// library or for `format!("{x:.1}")` would disagree with this one on real fixtures.
#[must_use]
pub fn round_storage(definition: &Definition, value: f64) -> f64 {
    let decimals = definition.precision.storage_decimals;
    if decimals == 0 {
        return value.round();
    }
    let factor = 10f64.powi(decimals as i32);
    (value * factor).round() / factor
}

/// `V = N * log2(n)` over the operator tokens (P3). `log2` is the platform one, and `n >= 2` is
/// guaranteed by the caller's `vacuous` check — `log2(0)` is undefined and `log2(1)` is 0, which is
/// exactly why a vocabulary below 2 gets no MI.
#[must_use]
pub fn halstead_volume(operators_total: u64, vocabulary: u64) -> f64 {
    (operators_total as f64) * (vocabulary as f64).log2()
}

/// `G = 1 + Σ decision points`, over the whole file (P4): one entry point, not one graph per
/// function, which is what the base of 1 means.
#[must_use]
pub fn cyclomatic(counts: &Counts, base: u64) -> u64 {
    base + counts.decision_sum()
}

/// The band a *stored* MI falls in. Compare the rounded value, never the raw one: that is what makes
/// a hotspot at the edge the same colour in two views (P2, P10).
#[must_use]
pub fn band_of(definition: &Definition, mi: f64) -> Band {
    if mi < definition.bands.red_below {
        Band::Red
    } else if mi < definition.bands.amber_below {
        Band::Amber
    } else {
        Band::Green
    }
}

/// What one file's counts came to: the ladder's answer, and every number a per-file row carries.
#[derive(Debug, Clone, PartialEq)]
pub struct Measurement {
    /// The rung of the ladder this file landed on.
    pub status: Status,
    /// Logical lines, when the file lexed. Absent when it did not, because there is nothing counted.
    pub loc: Option<u64>,
    /// Halstead volume `V`.
    pub volume: Option<f64>,
    /// Cyclomatic complexity `G`.
    pub cyclomatic: Option<u64>,
    /// The unrounded formula output, kept as a diagnostic only (P2).
    pub mi_raw: Option<f64>,
    /// The stored, rounded MI — the only MI any consumer compares.
    pub mi: Option<f64>,
    /// The band of `mi`.
    pub band: Option<Band>,
}

impl Measurement {
    /// A rung that never reached the arithmetic: no numbers, only the reason (ADR-0007 §3.5: an
    /// unmeasurable file has no MI field anywhere, and it is never 0).
    #[must_use]
    pub const fn gap(status: Status) -> Self {
        Self {
            status,
            loc: None,
            volume: None,
            cyclomatic: None,
            mi_raw: None,
            mi: None,
            band: None,
        }
    }
}

/// Score one file's counts: the bottom of the ladder (`vacuous`), then the formula, then the
/// rounding, then the band.
///
/// `incoming` is the verdict the caller's ladder reached with these counts in hand: `Ok` when the
/// blob lexed, or `Vacuous`/`ParseError` to keep a verdict already made. The ladder never upgrades a
/// gap into a number, and it never turns a missing number into a zero.
#[must_use]
pub fn measure(
    definition: &Definition,
    counts: &Counts,
    incoming: Status,
    base: u64,
) -> Measurement {
    if incoming != Status::Ok {
        return Measurement {
            status: incoming,
            loc: Some(counts.statements),
            ..Measurement::gap(incoming)
        };
    }
    let loc = counts.statements;
    let (total, unique) = if definition.precision.volume_is_operand_basis() {
        (counts.operand_total(), counts.operand_unique())
    } else {
        (counts.operator_total(), counts.operator_unique())
    };
    if loc < definition.measurability.min_loc
        || unique < definition.measurability.min_operator_unique
    {
        // `ln(0)` is not a number and there is no other sensible one: the value is absent and the
        // reason is present. Clamping LOC to 1 would hand a comment-only file the best score in the
        // repository, which is a different lie (ADR-0007 §3.5).
        return Measurement {
            status: Status::Vacuous,
            loc: Some(loc),
            ..Measurement::gap(Status::Vacuous)
        };
    }
    let volume = halstead_volume(total, unique);
    if volume < 1.0 {
        // Unreachable with `n >= 2` and `N >= 1`, and kept so that a future change to the vocabulary
        // rule cannot hand `ln()` a non-positive argument unnoticed.
        return Measurement {
            status: Status::Vacuous,
            loc: Some(loc),
            ..Measurement::gap(Status::Vacuous)
        };
    }
    let g = cyclomatic(counts, base);
    let f = definition.formula;
    let mi_raw =
        f.a - f.b_volume * volume.ln() - f.c_cyclomatic * (g as f64) - f.d_loc * (loc as f64).ln();
    let scaled = mi_raw * f.scale_numerator / f.scale_denominator;
    let clamped = scaled.clamp(f.clamp_min, f.clamp_max);
    let mi = round_storage(definition, clamped);
    Measurement {
        status: Status::Ok,
        loc: Some(loc),
        volume: Some(volume),
        cyclomatic: Some(g),
        mi_raw: Some(mi_raw),
        mi: Some(mi),
        band: Some(band_of(definition, mi)),
    }
}

/// How far a measurement sits from every boundary that could change what gets stored or displayed.
///
/// `1.0` means "away from all of them"; anything smaller means the fixture is sitting on a rounding
/// boundary or a band edge, where a 1 ULP difference in a host's math library could move the stored
/// digit or the traffic light — which is why P14 refuses such a vector, and why the vector test
/// calls this before it trusts a number.
#[must_use]
pub fn boundary_distance(definition: &Definition, measurement: &Measurement) -> f64 {
    let (Some(mi), Some(mi_raw)) = (measurement.mi, measurement.mi_raw) else {
        return 1.0;
    };
    let f = definition.formula;
    let clamped =
        (mi_raw * f.scale_numerator / f.scale_denominator).clamp(f.clamp_min, f.clamp_max);
    let factor = 10f64.powi(definition.precision.storage_decimals as i32);
    let distances = [
        (clamped * factor - (clamped * factor).round()).abs(),
        (mi - definition.bands.amber_below).abs(),
        (mi - definition.bands.red_below).abs(),
    ];
    let smallest = distances.iter().copied().fold(f64::INFINITY, f64::min);
    if smallest > definition.precision.boundary_slack {
        1.0
    } else {
        smallest
    }
}

/// The `1e-9` relative room a different `libm` gets on `MI_raw`, and nothing more (P14, and
/// `mi_reference.check_vector`'s own tolerance).
pub const MI_RAW_TOLERANCE: f64 = 1e-9;

/// Whether a diagnostic `MI_raw` agrees with a recomputed one, within the room the ADR allows.
#[must_use]
pub fn mi_raw_agrees(want: f64, got: f64) -> bool {
    (got - want).abs() <= MI_RAW_TOLERANCE * want.abs().max(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mi::Definition;
    use std::collections::BTreeMap;

    fn definition() -> Definition {
        Definition::embedded()
    }

    fn counts(
        operators: u64,
        vocabulary: u64,
        decisions: &[(&str, u64)],
        statements: u64,
    ) -> Counts {
        let mut c = Counts::default();
        let vocabulary = vocabulary.max(1);
        for i in 0..operators {
            *c.operators
                .entry(format!("op{}", i % vocabulary))
                .or_insert(0) += 1;
        }
        for (id, n) in decisions {
            c.decision_points.insert((*id).to_owned(), *n);
        }
        c.statements = statements;
        c
    }

    #[test]
    fn rounding_is_half_away_from_zero_on_the_product() {
        let d = definition();
        // The two floats either side of 62.45 both store as a value whose product by 10 rounds to
        // 624.5-ish, so both land on 62.5. A decimal-string intuition ("62.44999 rounds down") would
        // be wrong here, and the ADR says so in as many words: the rule is on the product, not on the
        // digits you would write.
        assert_eq!(round_storage(&d, 62.45_f64.next_down()), 62.5);
        assert_eq!(round_storage(&d, 62.45_f64.next_up()), 62.5);
        assert_eq!(round_storage(&d, 62.449999), 62.4);
        assert_eq!(round_storage(&d, 99.99999), 100.0);
        assert_eq!(round_storage(&d, 0.04), 0.0);
    }

    #[test]
    fn band_edges_belong_to_the_better_band() {
        let d = definition();
        assert_eq!(band_of(&d, 50.0), Band::Green);
        assert_eq!(band_of(&d, 49.9), Band::Amber);
        assert_eq!(band_of(&d, 38.0), Band::Amber);
        assert_eq!(band_of(&d, 37.9), Band::Red);
    }

    #[test]
    fn ln_of_zero_is_undefined_so_there_is_no_mi() {
        let d = definition();
        let zero_loc = counts(12, 6, &[("if", 1)], 0);
        let m = measure(&d, &zero_loc, Status::Ok, 1);
        assert_eq!(m.status, Status::Vacuous);
        assert_eq!(m.mi, None, "a file with no MI has no MI field, not a zero");
        let one_vocabulary = counts(1, 1, &[("if", 1)], 10);
        assert_eq!(
            measure(&d, &one_vocabulary, Status::Ok, 1).status,
            Status::Vacuous,
            "log2(1) is 0 and log2(0) is undefined, so n < 2 has no MI either"
        );
    }

    #[test]
    fn the_ladder_never_upgrades_a_gap() {
        let d = definition();
        let m = measure(&d, &Counts::default(), Status::ParseError, 1);
        assert_eq!(m.status, Status::ParseError);
        assert_eq!(m.mi, None);
        assert_eq!(m.loc, Some(0));
    }

    #[test]
    fn the_ceiling_is_declared_and_out_of_reach_in_practice() {
        let d = definition();
        // The reachable ceiling is `V == 1, G == 1, LOC == 1`, and no lexable file has it: `n <= N`,
        // so `N == 1` forces `n == 1`, which the `vacuous` rule refuses. The ADR keeps the top clamp
        // as the thing a leak through `vacuous` would land on, so it is checked at the arithmetic
        // rather than with a fixture that cannot exist.
        let ceiling_raw = d.formula.a
            - d.formula.b_volume * halstead_volume(1, 2).ln()
            - d.formula.c_cyclomatic
            - d.formula.d_loc * 1f64.ln();
        let ceiling = (ceiling_raw * 100.0 / 171.0).clamp(0.0, 100.0);
        assert_eq!(round_storage(&d, ceiling), 99.9);
        let smallest = measure(&d, &counts(2, 2, &[], 1), Status::Ok, 1);
        assert!(smallest.mi.is_some_and(|mi| mi < 100.0));
        let enormous = measure(
            &d,
            &counts(9_000_000, 900_000, &[("if", 40_000)], 400_000),
            Status::Ok,
            1,
        );
        assert_eq!(
            enormous.mi,
            Some(0.0),
            "a huge file clamps, and raw goes negative"
        );
    }

    #[test]
    fn the_worked_example_from_the_adr() {
        // ADR-0007 §2.3: N 46, n 31, 9 decision points, 16 statements.
        let d = definition();
        let mut c = Counts::default();
        for i in 0..46u64 {
            // 31 distinct operator texts, 46 occurrences.
            *c.operators.entry(format!("op{}", i % 31)).or_insert(0) += 1;
        }
        let mut decisions: BTreeMap<String, u64> = BTreeMap::new();
        for (id, n) in [
            ("if", 2u64),
            ("match_arm", 3),
            ("or_or", 1),
            ("and_and", 1),
            ("for", 1),
            ("while", 1),
        ] {
            decisions.insert(id.to_owned(), n);
        }
        c.decision_points = decisions;
        c.statements = 16;
        let m = measure(&d, &c, Status::Ok, 1);
        assert_eq!(m.volume, Some(227.89303027779624));
        assert_eq!(m.cyclomatic, Some(10));
        assert!(mi_raw_agrees(95.55390566196463, m.mi_raw.expect("raw")));
        assert_eq!(
            m.mi,
            Some(55.9),
            "the ADR's hand-check, to the stored digit"
        );
        assert_eq!(m.band, Some(Band::Green));
        assert_eq!(boundary_distance(&d, &m), 1.0);
    }
}
