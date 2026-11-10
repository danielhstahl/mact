//! The istanbul `coverage-summary.json` parser (mact-f1w.10, ADR-0002 §4.2).
//!
//! The shape istanbul's `json-summary` reporter writes:
//!
//! ```text
//! { "total": { "lines": {"total":4,"covered":3,"skipped":0,"pct":75},
//!              "statements": {...}, "branches": {...}, "functions": {...} },
//!   "/abs/path/lib/index.js": { ...same four metrics... }, ... }
//! ```
//!
//! Two things this parser is careful about, both of them ADR-0002 §4.2:
//!
//! * **statements are not lines.** They are separate `statements_*` / `lines_*` pairs on
//!   the record and are never averaged, swapped or used for one another. A JS project's
//!   "lines" and "statements" differ by every multi-expression line, and a run that
//!   reported only one of them must not be able to fill in the other.
//! * **only raw counts are stored.** The reporter's `pct` is read *only* to notice that a
//!   metric was reported without `covered`/`total` — a percentage cannot be stored under
//!   `source: "coverage_summary"`, because `reported_percent` belongs to `log_scrape`
//!   alone (ADR-0002 §4.3). Such a metric is dropped and counted in `malformed`.
//!
//! Entries whose path cannot be made workspace-relative (a file outside the tree —
//! `node_modules` is the everyday case, since istanbul instruments what was `require`d)
//! are reported in `dropped_paths` and contribute no file record. The `total` entry is
//! istanbul's own total over *all* instrumented files, and it is kept as the run's total:
//! it is the number the tool printed and the one a `--check-coverage` threshold was
//! evaluated against, so replacing it with a sum of the subset we can name would quietly
//! change what the run reported.

use serde_json::Value;

use super::{Pairs, ParsedSource};

/// The `total` roll-up's key. Every other top-level key is a file path.
const TOTAL_KEY: &str = "total";
/// istanbul's roll-up of the files that were never loaded, present with
/// `--all`/`--include`. Redundant with `total`, and it is not a file, so it is skipped
/// rather than emitted as a record for a file called `all`.
const ALL_KEY: &str = "all";
/// The four metric names istanbul reports, in the order the record writes them.
const METRICS: [(&str, Metric); 4] = [
    ("lines", Metric::Lines),
    ("statements", Metric::Statements),
    ("branches", Metric::Branches),
    ("functions", Metric::Functions),
];

#[derive(Clone, Copy)]
enum Metric {
    Lines,
    Statements,
    Branches,
    Functions,
}

/// What one parsed `coverage-summary.json` holds.
#[derive(Debug, Clone, Default)]
pub(crate) struct IstanbulReport {
    /// The `total` entry, verbatim from the file; [`Pairs::NONE`] when it was absent or
    /// carried nothing usable.
    pub(crate) total: Pairs,
    /// One entry per file entry whose path is inside the tree, sorted by the normalised
    /// path (a JSON object has no order, so the extractor imposes one — by path is the
    /// only order that means the same thing in two runs' files).
    pub(crate) sources: Vec<ParsedSource>,
    /// Metrics or entries that were present but unusable.
    pub(crate) malformed: usize,
    /// Set when the document was JSON but not a coverage summary at all.
    pub(crate) not_a_summary: bool,
}

/// Parse an istanbul `coverage-summary.json` body.
pub(crate) fn parse(text: &str) -> IstanbulReport {
    let mut report = IstanbulReport::default();
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        report.not_a_summary = true;
        return report;
    };
    let Value::Object(entries) = value else {
        report.not_a_summary = true;
        return report;
    };
    // `serde_json::Value` stores an object as a sorted map, so iteration is already
    // ascending by key; collecting the file entries keeps that order.
    let mut files: Vec<(&String, &Value)> = Vec::new();
    for (key, entry) in &entries {
        match key.as_str() {
            TOTAL_KEY => report.total = pairs_of(entry),
            ALL_KEY => {}
            _ => files.push((key, entry)),
        }
    }
    for (key, entry) in files {
        let pairs = pairs_of(entry);
        if pairs.is_empty() {
            report.malformed += 1;
            continue;
        }
        report.sources.push(ParsedSource {
            raw_path: key.clone(),
            pairs,
        });
    }
    // A summary whose `total` is missing or unusable still has a defensible total: the
    // sum of the entries that came with it.
    if report.total.is_empty() {
        let mut summed = Pairs::NONE;
        for source in &report.sources {
            summed.add(&source.pairs);
        }
        if !summed.is_empty() {
            report.total = summed;
        }
    }
    report
}

/// The four metric pairs of one entry (`total` or a file).
fn pairs_of(entry: &Value) -> Pairs {
    let Value::Object(metrics) = entry else {
        return Pairs::NONE;
    };
    let mut pairs = Pairs::NONE;
    for (name, metric) in METRICS {
        let Some(metric_entry) = metrics.get(name) else {
            continue;
        };
        let Some((found, hit)) = covered(metric_entry) else {
            continue;
        };
        let slot = match metric {
            Metric::Lines => &mut pairs.lines,
            Metric::Statements => &mut pairs.statements,
            Metric::Branches => &mut pairs.branches,
            Metric::Functions => &mut pairs.functions,
        };
        *slot = Some((found, hit));
    }
    pairs
}

/// `{"total": N, "covered": M, ...}` → `(N, M)`.
///
/// Both are required, because ADR-0002 §4.2 makes `*_found`/`*_hit` a co-required pair; a
/// metric that carries only `pct` is not a raw count and is not stored (ADR-0002 §4.3).
fn covered(metric: &Value) -> Option<(u64, u64)> {
    let Value::Object(fields) = metric else {
        return None;
    };
    let found = count(fields.get("total")?)?;
    let hit = count(fields.get("covered")?)?;
    Some((found, hit))
}

/// A count istanbul wrote as a JSON number. It is an integer in every reporter, but the
/// field is a number in the schema, so a whole-number float is accepted and a fractional
/// one is not — there is no such thing as half a covered line.
fn count(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number.as_u64().or_else(|| {
            number
                .as_f64()
                .filter(|value| *value >= 0.0 && value.fract() == 0.0)
                .map(|value| value as u64)
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn total_and_file_entries_are_both_read() {
        let report = parse(
            r#"{"total":{"lines":{"total":10,"covered":8,"pct":80.0},
                             "statements":{"total":12,"covered":9,"pct":75.0}},
                "/w/lib/a.js":{"lines":{"total":6,"covered":5,"pct":83.33},
                                "statements":{"total":7,"covered":4,"pct":57.14}}}"#,
        );
        assert_eq!(report.total.lines, Some((10, 8)));
        assert_eq!(report.total.statements, Some((12, 9)));
        assert_eq!(report.sources.len(), 1);
        assert_eq!(report.sources[0].raw_path, "/w/lib/a.js");
        assert_eq!(report.sources[0].pairs.lines, Some((6, 5)));
        assert_eq!(report.sources[0].pairs.statements, Some((7, 4)));
    }

    #[test]
    fn percent_only_metrics_are_not_stored_as_raw_counts() {
        // `branches` carries a `pct` and no counts: it is istanbul's rounding, not a raw
        // numerator, so it cannot go on a `coverage_summary` record (ADR-0002 §4.3).
        let report = parse(
            r#"{"a.js":{"lines":{"total":4,"covered":2,"pct":50},
                        "branches":{"pct":"100"}}}"#,
        );
        assert_eq!(report.sources[0].pairs.lines, Some((4, 2)));
        assert_eq!(report.sources[0].pairs.branches, None);
    }

    #[test]
    fn a_missing_total_is_summed_from_the_entries() {
        let report = parse(
            r#"{"a.js":{"lines":{"total":4,"covered":2}},"b.js":{"lines":{"total":6,"covered":6}}}"#,
        );
        assert_eq!(report.total.lines, Some((10, 8)));
    }

    #[test]
    fn the_all_roll_up_is_not_a_file() {
        let report = parse(
            r#"{"total":{"lines":{"total":10,"covered":8}},"all":{"lines":{"total":4,"covered":0}},
                "a.js":{"lines":{"total":10,"covered":8}}}"#,
        );
        assert_eq!(report.sources.len(), 1);
        assert_eq!(report.sources[0].raw_path, "a.js");
    }

    #[test]
    fn documents_that_are_not_summaries_are_said_so() {
        for text in ["", "not json", "[]", "3", "{\"total\":7}"] {
            let report = parse(text);
            assert!(report.not_a_summary || report.total.is_empty(), "{text:?}");
        }
    }

    #[test]
    fn inverted_counts_are_passed_through_for_the_record_builder_to_repair() {
        // The parser keeps what the tool said; clamping `hit <= found` is one rule with
        // one home (ADR-0002 §4.2 calls it an extractor rule).
        let report = parse(r#"{"a.js":{"lines":{"total":2,"covered":5}}}"#);
        assert_eq!(report.sources[0].pairs.lines, Some((2, 5)));
    }
}
