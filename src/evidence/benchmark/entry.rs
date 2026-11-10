//! One metric entry, in the shape ADR-0002 §3.3 gives it, turned into a record's metric half.
//!
//! The same entry shape arrives from two places — an element of `metrics[]` in the payload
//! file, and the JSON object on an in-band `ACTMCP_BENCHMARK` line — and both are untrusted,
//! so both come through here and get the same verdicts. That matters: §3.3's list of what to
//! reject (`NaN`, `Infinity`, `"12ms"`, a missing name) and §4.1's rules about units, labels
//! and metric names are one list, not two, and a marker line that skipped a check would be a
//! second, sloppier contract.
//!
//! A rejected entry is a *reason*, never an error: extraction counts it in `malformed_metric`
//! and moves on, because a chatty job must not fail a run (ADR-0002 §5.1).

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::evidence::records::{
    BenchmarkMetric, BenchmarkSource, MetricValue, RelPath, FLAG_UNIT_MISSING, MAX_LABELS,
};
use crate::evidence::shapes;

/// Why an entry could not become a metric. Logged, never matched on, so a `&'static str`
/// is enough and a caller cannot accidentally branch on it.
pub(crate) type Reject = &'static str;

/// One accepted metric entry, before it knows which run it belongs to.
#[derive(Debug, Clone)]
pub(crate) struct Metric {
    /// Verbatim from the entry.
    pub(crate) name: String,
    /// Finite by construction — [`MetricValue`] cannot hold anything else.
    pub(crate) value: MetricValue,
    /// Verbatim, never converted (§4.1). `None` means the entry said nothing.
    pub(crate) unit: Option<String>,
    /// Flat string→string map, empty when the entry gave none.
    pub(crate) labels: BTreeMap<String, String>,
    /// Advisory passthrough (§4.1); a non-object `extra` is dropped rather than fatal.
    pub(crate) extra: Option<Map<String, Value>>,
}

impl Metric {
    /// Attribute this metric to `job` when the entry did not name one.
    ///
    /// The record's *identity* — commit, run, workflow, task — comes from the tool call and
    /// can never be re-attributed by a payload (§3.3); `labels.job` is a grouping key, so a
    /// job that labelled its own metric keeps that label and a job that did not gets the job
    /// id this run ran under. Without the stamp, act-log durations (which no job wrote) and
    /// job metrics would not group together, which is the whole point of `labels.job`.
    pub(crate) fn attributed_to(&mut self, job: &str) {
        self.labels
            .entry("job".to_owned())
            .or_insert_with(|| job.to_owned());
    }

    /// The record's metric half, stamped with where the number came from.
    ///
    /// A metric with no unit carries `unit_missing` (§4.1); that pairing is not left to the
    /// caller, because a record that forgot it is schema-invalid (`unit-missing-unflagged`
    /// in `tests/e2e/testdata/invalid/` is exactly that record).
    pub(crate) fn into_metric(
        self,
        source: BenchmarkSource,
        source_path: Option<RelPath>,
    ) -> BenchmarkMetric {
        let mut metric = BenchmarkMetric::new(self.name, self.value, source);
        metric.unit = self.unit;
        metric.source_path = source_path;
        if metric.unit.is_none() {
            metric = metric.with_flag(FLAG_UNIT_MISSING);
        }
        if !self.labels.is_empty() {
            metric = metric.with_labels(self.labels);
        }
        if let Some(extra) = self.extra {
            // `extra` is advisory, so it is passed through by assignment rather than by a
            // builder: nothing may depend on what a job put there.
            metric.extra = Some(extra);
        }
        metric
    }
}

/// Read one entry: `{name, value, unit?, labels?, extra?}`, unknown keys ignored (§3.3).
///
/// # Errors
///
/// A [`Reject`] naming the first rule the entry broke.
pub(crate) fn parse(entry: &Value) -> Result<Metric, Reject> {
    let fields = match entry {
        Value::Object(fields) => fields,
        _ => return Err("a metric entry is a JSON object"),
    };
    let name = fields
        .get("name")
        .and_then(Value::as_str)
        .ok_or("name is a required string")?;
    if !shapes::is_metric_name(name) {
        return Err("name must start with a letter and stay inside [A-Za-z0-9_.:/-] (§4.1)");
    }
    let value = number(fields.get("value")).ok_or(
        "value is a required finite JSON number: a string, a bool, null, NaN or an \
         out-of-range number is rejected (§3.3)",
    )?;
    if is_duration(name) && value.as_f64() < 0.0 {
        // §4.1: a negative value is legal for a delta and never for a duration.
        return Err("a *_duration_s metric is never negative (§4.1)");
    }
    let unit =
        match fields.get("unit") {
            None | Some(Value::Null) => None,
            Some(Value::String(unit)) if shapes::is_unit(unit) => Some(unit.clone()),
            Some(_) => return Err(
                "unit is verbatim from the job and must be a non-empty unit-shaped string (§4.1)",
            ),
        };
    Ok(Metric {
        name: name.to_owned(),
        value,
        unit,
        labels: labels(fields.get("labels"))?,
        extra: match fields.get("extra") {
            Some(Value::Object(extra)) => Some(extra.clone()),
            // `extra` is advisory (§2.2), so a wrong-typed one is dropped rather than
            // costing the run a metric.
            _ => None,
        },
    })
}

/// Whether this metric name states a duration, which is the one case a negative value is
/// refused for. `*_duration_s` is the built-in spelling (ADR-0002 §5.3); a job's own
/// `cold_start_ms` may well be a delta and go negative.
fn is_duration(name: &str) -> bool {
    name == "duration_s" || name.ends_with("_duration_s")
}

/// The value as a [`MetricValue`]: integral stays integral (so `3` is written `3`, not
/// `3.0`), and anything non-finite or non-numeric is refused.
///
/// JSON itself cannot spell `NaN` or `Infinity`, so the non-finite cases that reach this
/// code are an out-of-range literal (`1e999`, which `serde_json` refuses outright) and a
/// value that is not a number at all (`"12ms"`, `true`, `null`) — all of them §3.3's
/// `malformed_metric`.
fn number(value: Option<&Value>) -> Option<MetricValue> {
    match value? {
        Value::Number(number) => {
            if let Some(integral) = number.as_i64() {
                return Some(MetricValue::from_i64(integral));
            }
            // A u64 above i64::MAX, or a float: both go through the f64 door, which is
            // where non-finite is caught.
            number.as_f64().and_then(MetricValue::coerce_f64)
        }
        _ => None,
    }
}

/// `labels`, stringified on the way in: ADR-0002 §4.1 makes every label value a string "so
/// a `group by` never has to type-switch", and a job that writes `{"matrix": 3}` still means
/// the group `matrix=3`.
///
/// A label whose value is an object, an array or `null` is refused rather than stringified:
/// there is no string that means the same thing, and a flattened `{"a":1}` would be a new
/// format of its own.
///
/// # Errors
///
/// A [`Reject`] naming the first rule broken.
fn labels(value: Option<&Value>) -> Result<BTreeMap<String, String>, Reject> {
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    if matches!(value, Value::Null) {
        return Ok(BTreeMap::new());
    }
    let fields = value
        .as_object()
        .ok_or("labels is a flat string to string map")?;
    if fields.len() > MAX_LABELS {
        return Err("labels holds at most 16 entries (§4.1)");
    }
    let mut out = BTreeMap::new();
    for (key, value) in fields {
        if !shapes::is_word(key) {
            return Err("a label key is ^[a-z][a-z0-9_]*$ (§4.1)");
        }
        let text = match value {
            Value::String(text) => text.clone(),
            Value::Number(number) => number.to_string(),
            Value::Bool(flag) => flag.to_string(),
            _ => return Err("a label value is a string, a number or a bool (§4.1)"),
        };
        if !shapes::is_label_value(&text) {
            return Err("a label value is at most 128 bytes (§4.1)");
        }
        out.insert(key.clone(), text);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(text: &str) -> Value {
        serde_json::from_str(text).expect("fixture JSON parses")
    }

    fn accepted(text: &str) -> Metric {
        parse(&entry(text)).unwrap_or_else(|reason| panic!("{text} rejected: {reason}"))
    }

    #[test]
    fn a_plain_entry_survives_verbatim() {
        let metric = accepted(
            r#"{"name":"tokenizer_throughput","value":184320.4,"unit":"ops/s",
               "labels":{"os":"ubuntu-24.04"},"extra":{"tool":"hyperfine"}}"#,
        );
        assert_eq!(metric.name, "tokenizer_throughput");
        assert_eq!(metric.value.to_string(), "184320.4");
        assert_eq!(metric.unit.as_deref(), Some("ops/s"));
        assert_eq!(
            metric.labels.get("os").map(String::as_str),
            Some("ubuntu-24.04")
        );
        assert_eq!(
            metric
                .extra
                .as_ref()
                .and_then(|e| e.get("tool"))
                .and_then(Value::as_str),
            Some("hyperfine")
        );
    }

    #[test]
    fn integral_values_keep_their_integral_spelling() {
        assert_eq!(
            accepted(r#"{"name":"queue_depth","value":3}"#)
                .value
                .to_string(),
            "3"
        );
        assert_eq!(
            accepted(r#"{"name":"queue_depth","value":3.0}"#)
                .value
                .to_string(),
            "3"
        );
        assert_eq!(
            accepted(r#"{"name":"queue_depth","value":3.5}"#)
                .value
                .to_string(),
            "3.5"
        );
    }

    #[test]
    fn values_that_are_not_finite_numbers_are_refused() {
        for text in [
            r#"{"name":"x","value":"12ms"}"#,
            r#"{"name":"x","value":null}"#,
            r#"{"name":"x","value":true}"#,
            r#"{"name":"x","value":{"n":1}}"#,
            r#"{"name":"x"}"#,
        ] {
            assert!(parse(&entry(text)).is_err(), "{text} must be refused");
        }
    }

    #[test]
    fn json_that_cannot_be_parsed_is_refused_before_it_is_counted() {
        // `NaN` is not JSON at all, so the caller never reaches `parse` with it; the test
        // belongs here because it is the case §3.3 names first.
        assert!(serde_json::from_str::<Value>(r#"{"name":"x","value":NaN}"#).is_err());
        assert!(serde_json::from_str::<Value>(r#"{"name":"x","value":Infinity}"#).is_err());
        // An out-of-range literal is refused the same way: `1e999` never reaches `parse`,
        // so a `.jsonl` line carrying one is counted malformed and a `.json` file
        // containing one fails as a file (§3.3's `malformed_metric`, counted where it is
        // detectable).
        assert!(serde_json::from_str::<Value>(r#"{"name":"x","value":1e999}"#).is_err());
    }

    #[test]
    fn negative_values_are_legal_except_on_a_duration() {
        assert!(parse(&entry(r#"{"name":"regression_delta","value":-12.5}"#)).is_ok());
        assert!(parse(&entry(r#"{"name":"job_duration_s","value":-1}"#)).is_err());
        assert!(parse(&entry(r#"{"name":"step_duration_s","value":0}"#)).is_ok());
    }

    #[test]
    fn a_missing_unit_is_none_and_a_silly_unit_is_refused() {
        assert_eq!(accepted(r#"{"name":"x","value":1}"#).unit, None);
        assert_eq!(
            accepted(r#"{"name":"x","value":1,"unit":null}"#).unit,
            None,
            "absent and explicit null mean the same thing to a payload"
        );
        assert!(parse(&entry(r#"{"name":"x","value":1,"unit":""}"#)).is_err());
        assert!(parse(&entry(r#"{"name":"x","value":1,"unit":7}"#)).is_err());
    }

    #[test]
    fn label_values_are_stringified_and_bad_ones_refused() {
        let metric = accepted(r#"{"name":"x","value":1,"labels":{"matrix":3,"cuda":true}}"#);
        assert_eq!(metric.labels.get("matrix").map(String::as_str), Some("3"));
        assert_eq!(metric.labels.get("cuda").map(String::as_str), Some("true"));
        assert!(parse(&entry(r#"{"name":"x","value":1,"labels":{"a":{"b":1}}}"#)).is_err());
        assert!(parse(&entry(r#"{"name":"x","value":1,"labels":{"Bad":1}}"#)).is_err());
        assert!(parse(&entry(r#"{"name":"x","value":1,"labels":[1,2]}"#)).is_err());
    }

    #[test]
    fn unknown_keys_are_ignored_because_the_format_is_forward_compatible() {
        let metric = accepted(r#"{"name":"x","value":1,"commit":"deadbeef","run_id":"later"}"#);
        assert_eq!(metric.name, "x", "unknown entry keys cost nothing (§3.3)");
    }

    #[test]
    fn a_metric_without_a_unit_says_so() {
        let metric = accepted(r#"{"name":"queue_depth","value":3}"#)
            .into_metric(BenchmarkSource::InBandStdout, None);
        assert_eq!(metric.unit, None);
        assert_eq!(metric.flags.as_ref().map(|flags| flags.len()), Some(1));
        assert_eq!(
            metric.flags.as_ref().map(|flags| flags[0].as_str()),
            Some(FLAG_UNIT_MISSING)
        );
        metric.check().expect("the record-level check agrees");
    }

    #[test]
    fn the_job_label_is_stamped_only_when_the_entry_gave_none() {
        let mut metric = accepted(r#"{"name":"x","value":1}"#);
        metric.attributed_to("hyperfine-bench");
        assert_eq!(
            metric.labels.get("job").map(String::as_str),
            Some("hyperfine-bench")
        );

        let mut mine = accepted(r#"{"name":"x","value":1,"labels":{"job":"theirs"}}"#);
        mine.attributed_to("hyperfine-bench");
        assert_eq!(
            mine.labels.get("job").map(String::as_str),
            Some("theirs"),
            "a label the job wrote is the job's own; identity lives in the envelope"
        );
    }
}
