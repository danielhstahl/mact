//! act's own `--json` log: the marker lines and the timings this ticket needs.
//!
//! ## The field names, pinned
//!
//! The log is act's, not ours (ADR-0002 §1.3), so the field names below are a fact about a
//! third-party tool and are pinned by a fixture —
//! `tests/fixtures/benchmark/act.jsonl` — rather than by this code. act prints its log with
//! `logrus`' JSON formatter, so every line is one flat object whose interesting members are:
//!
//! | field | what it carries |
//! |---|---|
//! | `msg` | the message; a step's stdout arrives here, one line per entry, with `raw_output: true` |
//! | `job`, `jobID` | the job the line belongs to (`jobID` is the `jobs:` key, `job` the `name:`) |
//! | `step` | the step's display name, set on step-scoped lines |
//! | `stage` | `pre` \| `main` \| `post` — act's own phase of the step |
//! | `executionTime` | **a Go `time.Duration`, marshalled as integer nanoseconds** |
//! | `time` | the entry's timestamp, RFC 3339 (whole seconds at logrus' default format) |
//! | `jobResult`, `stepResult` | the verdict on the `🏁 Job succeeded` / `✅ Success` lines |
//!
//! `executionTime` is the only sub-second timing act emits, and it is nanoseconds because Go
//! marshals `time.Duration` as its integer value. A string spelling (`"1m28.234s"`, what Go's
//! `Duration.String()` prints and what act writes into `msg`) is accepted too, because a
//! formatter change is a plausible upgrade and the two spell the same duration.
//!
//! `Output` and `Error` are read as message fields as well: they are the spellings the log
//! shape this repo's earlier coverage fixtures use carry, and a marker may arrive in either.
//! A line that is not JSON at all is searched as plain text, so a run captured without
//! `--json` still yields its markers (just none of its timings).
//!
//! ## What is attributed to this run
//!
//! act can run several jobs' worth of log in one stream (a job with `needs:` drags its
//! dependencies through the same file). A line belongs to this run when its `jobID` or `job`
//! equals this run's job id; a line with neither is act's own progress and counts as this
//! run's, because it is the run's own container starting; a line naming a *different* job is
//! dropped, so another job's 40-minute setup never becomes this job's `job_duration_s`.

use std::{
    io::{BufRead, BufReader, Read},
    path::Path,
};

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::marker;

/// One step act reported finishing, in whole milliseconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StepTiming {
    /// The step's display name; `None` when the line carried a timing but no name, in which
    /// case it still counts toward the job total and gets no record of its own.
    pub(crate) step: Option<String>,
    /// The duration, rounded to milliseconds — the precision ADR-0002 §5.3 asks for.
    pub(crate) ms: u64,
}

/// What one pass over `act.jsonl` found.
#[derive(Debug, Default)]
pub(crate) struct Scan {
    /// Marker payloads, in log order, already parsed as JSON.
    pub(crate) markers: Vec<Value>,
    /// Marker lines seen and rejected: `malformed_metric` (§5.2).
    pub(crate) marker_malformed: usize,
    /// More marker lines existed than the cap allowed (§5.2's `marker_cap_reached`).
    pub(crate) marker_capped: bool,
    /// The steps this run's job reported finishing, in log order.
    pub(crate) steps: Vec<StepTiming>,
    /// The span of this run's log lines, when act's timestamps said anything.
    pub(crate) span_ms: Option<u64>,
    /// The log ends in our own truncation marker, so timings after that point are missing.
    pub(crate) log_truncated: bool,
    /// The byte budget ran out before the file did.
    pub(crate) budget_exceeded: bool,
}

/// Which run a log line speaks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Attribution {
    /// This run's job, by `jobID` or `job`.
    This,
    /// act's own progress: no job named, so nothing to attribute it to but this run.
    Ambient,
    /// Another job's line, in the same stream.
    Other,
}

/// One pass over a run's captured log.
///
/// `marker_cap` bounds how many marker lines are collected (§5.2); the scan keeps going
/// afterwards, because the timings further down the log are still wanted.
pub(crate) fn scan(
    path: &Path,
    job_id: &str,
    budget: u64,
    marker_cap: usize,
) -> std::io::Result<Scan> {
    let file = std::fs::File::open(path)?;
    let size = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    let reader = BufReader::new(file.take(budget));
    let mut scan = Scan::default();
    let mut first: Option<DateTime<Utc>> = None;
    let mut last: Option<DateTime<Utc>> = None;

    // `lines()` honours the `Take` budget: the read stops at `budget` bytes, and the log is
    // never held in memory whole.
    for line in reader.lines() {
        let line = line?;
        let line = line.trim_end_matches(['\n', '\r']);
        if line.trim().is_empty() {
            continue;
        }
        let parsed: Option<Value> = serde_json::from_str(line).ok();
        match parsed.as_ref() {
            Some(entry @ Value::Object(_)) => {
                if entry.get("act_mcp_truncated").is_some() {
                    // Our own marker line (`runner::log::truncation_marker`), not act's.
                    scan.log_truncated = true;
                    continue;
                }
                let attribution = attribution(entry, job_id);
                if attribution != Attribution::Other {
                    if let Some(ms) = entry.get("executionTime").and_then(duration_ms) {
                        scan.steps.push(StepTiming {
                            step: entry
                                .get("step")
                                .and_then(Value::as_str)
                                .map(str::trim)
                                .filter(|text| !text.is_empty())
                                .map(str::to_owned),
                            ms,
                        });
                    }
                    if let Some(at) = entry.get("time").and_then(timestamp) {
                        first = Some(first.map_or(at, |held| held.min(at)));
                        last = Some(last.map_or(at, |held| held.max(at)));
                    }
                }
                for text in messages(entry, line) {
                    collect(&mut scan, &text, marker_cap);
                }
            }
            // Not JSON (act run without `--json`, a torn line, or a JSON scalar): timings
            // need JSON, the marker does not.
            _ => collect(&mut scan, line, marker_cap),
        }
    }

    scan.budget_exceeded = size > budget;
    scan.span_ms = match (first, last) {
        (Some(first), Some(last)) if last > first => {
            u64::try_from((last - first).num_milliseconds()).ok()
        }
        _ => None,
    };
    Ok(scan)
}

/// Which run this line speaks for (§"What is attributed to this run").
fn attribution(entry: &Value, job_id: &str) -> Attribution {
    for key in ["jobID", "job"] {
        match entry.get(key) {
            None | Some(Value::Null) => continue,
            Some(value) => {
                return if value.as_str() == Some(job_id) {
                    Attribution::This
                } else {
                    Attribution::Other
                }
            }
        }
    }
    Attribution::Ambient
}

/// The texts on one entry that can carry a marker.
///
/// `msg` is where act puts a step's stdout; `Output`/`Error` are the other spellings this
/// log has used. When an entry has none of them the raw line is searched instead — same rule
/// [`crate::evidence::coverage`] applies when it scrapes a log for a percentage.
fn messages(entry: &Value, raw_line: &str) -> Vec<String> {
    let mut texts: Vec<String> = Vec::new();
    for key in ["msg", "Output", "Error"] {
        if let Some(text) = entry.get(key).and_then(Value::as_str) {
            texts.extend(text.split('\n').map(str::to_owned));
        }
    }
    if texts.is_empty() {
        texts.push(raw_line.to_owned());
    }
    texts
}

/// One line of log text, as a possible marker.
fn collect(scan: &mut Scan, text: &str, marker_cap: usize) {
    if !marker::mentions_marker(text) {
        return;
    }
    let Some(json) = marker::marker_json(text) else {
        return;
    };
    if scan.markers.len() >= marker_cap {
        scan.marker_capped = true;
        return;
    }
    match serde_json::from_str::<Value>(json) {
        Ok(value) => scan.markers.push(value),
        Err(_) => scan.marker_malformed += 1,
    }
}

/// A duration field as milliseconds: integer nanoseconds (what Go's `time.Duration`
/// marshals to) or a Go duration string. Never negative — a negative duration is refused
/// here and counted as malformed by the caller, exactly as a job's negative
/// `job_duration_s` is (§4.1).
fn duration_ms(value: &Value) -> Option<u64> {
    if let Some(nanos) = value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|whole| i64::try_from(whole).ok()))
    {
        return nanos.try_into().ok().map(|nanos: u64| ns_to_ms(nanos));
    }
    if let Some(bigger) = value.as_f64() {
        // A float that is still a whole number of nanoseconds is the same thing; anything
        // fractional, infinite, or negative is not a timing act-mcp will publish.
        if bigger.is_finite() && bigger >= 0.0 && bigger == bigger.trunc() {
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "the value is checked finite, non-negative and whole above"
            )]
            let nanos = bigger as u64;
            return Some(ns_to_ms(nanos));
        }
        return None;
    }
    value.as_str().and_then(go_duration_ns).map(ns_to_ms)
}

/// Round nanoseconds to milliseconds, half away from zero (§5.3's "millisecond precision").
fn ns_to_ms(nanos: u64) -> u64 {
    nanos.saturating_add(500_000) / 1_000_000
}

/// Go's `time.Duration.String()` spelling, back to nanoseconds: `500ms`, `3.066s`,
/// `1m28.234s`, `2h3m4s`. A leading `-` is refused (negative durations are not data here).
fn go_duration_ns(text: &str) -> Option<u64> {
    let text = text.trim();
    if text.is_empty() || text.starts_with('-') {
        return None;
    }
    let mut rest = text;
    let mut total: u128 = 0;
    let mut parts = 0usize;
    while !rest.is_empty() {
        let digits = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        let number: f64 = rest[..digits].parse().ok()?;
        rest = &rest[digits..];
        let unit_end = rest
            .find(|c: char| !c.is_alphabetic() && c != '\u{b5}')
            .unwrap_or(rest.len());
        let per_second: u64 = match &rest[..unit_end] {
            "ns" => 1,
            "us" | "\u{b5}s" => 1_000,
            "ms" => 1_000_000,
            "s" => 1_000_000_000,
            "m" => 60_000_000_000,
            "h" => 3_600_000_000_000,
            _ => return None,
        };
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "number came from digits only so it is non-negative, and the scaled value is checked finite before it becomes an integer again"
        )]
        {
            let scaled = number * per_second as f64;
            if !scaled.is_finite() {
                return None;
            }
            total += scaled.round() as u128;
        }
        rest = &rest[unit_end..];
        parts += 1;
    }
    (parts > 0).then(|| u64::try_from(total).unwrap_or(u64::MAX))
}

/// logrus' `time` field — RFC 3339, usually whole seconds, sometimes not.
fn timestamp(value: &Value) -> Option<DateTime<Utc>> {
    let text = value.as_str()?;
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|instant| instant.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    const JOB: &str = "hyperfine-bench";

    fn logged(lines: &[&str]) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().expect("temp log");
        std::fs::write(file.path(), format!("{}\n", lines.join("\n"))).expect("write");
        file
    }

    fn scan_of(lines: &[&str]) -> Scan {
        let file = logged(lines);
        scan(file.path(), JOB, 1 << 20, 5_000).expect("scan")
    }

    #[test]
    fn a_step_timing_line_becomes_a_step_in_milliseconds() {
        let scan = scan_of(&[
            r#"{"job":"hyperfine-bench","jobID":"hyperfine-bench","level":"info","msg":"  ✅  Success - Main Run benchmark [1m28.234s]","step":"Run benchmark","executionTime":88234000000,"time":"2026-11-10T00:43:33Z"}"#,
        ]);
        assert_eq!(scan.steps.len(), 1);
        assert_eq!(scan.steps[0].step.as_deref(), Some("Run benchmark"));
        assert_eq!(scan.steps[0].ms, 88_234);
    }

    #[test]
    fn a_duration_act_could_only_print_as_a_string_is_still_read() {
        for (text, ms) in [
            (r#""500ms""#, 500),
            (r#""3.066s""#, 3_066),
            (r#""1m28.234s""#, 88_234),
            (r#""2h""#, 7_200_000),
            (r#""1h1m1.5s""#, 3_661_500),
        ] {
            let line = format!(
                r#"{{"jobID":"{JOB}","executionTime":{text},"step":"s","time":"2026-11-10T00:00:00Z"}}"#
            );
            let scan = scan_of(&[&line]);
            assert_eq!(scan.steps.first().map(|step| step.ms), Some(ms), "{text}");
        }
    }

    #[test]
    fn durations_that_are_not_timings_are_ignored() {
        let scan = scan_of(&[
            r#"{"jobID":"hyperfine-bench","executionTime":-5}"#,
            r#"{"jobID":"hyperfine-bench","executionTime":"-1s"}"#,
            r#"{"jobID":"hyperfine-bench","executionTime":"soon"}"#,
            r#"{"jobID":"hyperfine-bench","executionTime":null}"#,
            r#"{"jobID":"hyperfine-bench","executionTime":1.5}"#,
        ]);
        assert!(
            scan.steps.is_empty(),
            "no negative, no garbage: {:?}",
            scan.steps
        );
    }

    #[test]
    fn another_jobs_step_is_not_this_runs() {
        let scan = scan_of(&[
            r#"{"jobID":"setup","step":"Install","executionTime":60000000000,"time":"2026-11-10T00:00:00Z"}"#,
            r#"{"jobID":"hyperfine-bench","step":"Run benchmark","executionTime":1000000000,"time":"2026-11-10T00:01:00Z"}"#,
        ]);
        assert_eq!(scan.steps.len(), 1);
        assert_eq!(scan.steps[0].step.as_deref(), Some("Run benchmark"));
    }

    #[test]
    fn the_span_of_the_runs_lines_is_the_fallback_job_duration() {
        let scan = scan_of(&[
            r#"{"jobID":"hyperfine-bench","msg":"start","time":"2026-11-10T00:42:04Z"}"#,
            r#"{"level":"info","msg":"act is ambient to this run","time":"2026-11-10T00:42:05Z"}"#,
            r#"{"jobID":"hyperfine-bench","jobResult":"success","msg":"done","time":"2026-11-10T00:43:35Z"}"#,
        ]);
        assert!(scan.steps.is_empty());
        assert_eq!(scan.span_ms, Some(91_000));
    }

    #[test]
    fn markers_are_found_in_step_output_and_counted_when_broken() {
        let scan = scan_of(&[
            r#"{"jobID":"hyperfine-bench","step":"Run benchmark","raw_output":true,"msg":"ACTMCP_BENCHMARK {\"name\":\"cold_start_ms\",\"value\":412.5,\"unit\":\"ms\"}"}"#,
            r#"{"jobID":"hyperfine-bench","raw_output":true,"msg":"ACTMCP_BENCHMARK {\"name\":\"x\",}"}"#,
            r#"{"jobID":"hyperfine-bench","raw_output":true,"msg":"ACTMCP_BENCHMARK {\"name\":\"queue_depth\",\"value\":3}"}"#,
        ]);
        assert_eq!(scan.markers.len(), 2);
        assert_eq!(scan.marker_malformed, 1);
        assert_eq!(scan.markers[1]["name"], "queue_depth");
    }

    #[test]
    fn the_marker_cap_is_reported_without_stopping_the_timings() {
        let mut lines = vec![
            r#"{"jobID":"hyperfine-bench","raw_output":true,"msg":"ACTMCP_BENCHMARK {\"name\":\"a\",\"value\":1}"}"#,
            r#"{"jobID":"hyperfine-bench","raw_output":true,"msg":"ACTMCP_BENCHMARK {\"name\":\"b\",\"value\":2}"}"#,
        ];
        lines.push(r#"{"jobID":"hyperfine-bench","step":"s","executionTime":1000000}"#);
        let file = logged(&lines);
        let scan = scan(file.path(), JOB, 1 << 20, 1).expect("scan");
        assert_eq!(scan.markers.len(), 1);
        assert!(scan.marker_capped);
        assert_eq!(scan.steps.len(), 1, "the timings further down still matter");
    }

    #[test]
    fn a_plain_text_log_yields_markers_but_no_timings() {
        let scan = scan_of(&[
            "ACTMCP_BENCHMARK {\"name\":\"x\",\"value\":1}",
            "nothing to see here",
        ]);
        assert_eq!(scan.markers.len(), 1);
        assert!(scan.steps.is_empty());
        assert_eq!(scan.span_ms, None);
    }

    #[test]
    fn our_own_truncation_marker_is_recognised_and_skipped() {
        let scan = scan_of(&[
            r#"{"jobID":"hyperfine-bench","step":"s","executionTime":1000000}"#,
            r#"{"act_mcp_truncated":true,"reason":"max_run_log_bytes","bytes_written":64,"dropped_lines":12,"limit_bytes":64}"#,
        ]);
        assert!(scan.log_truncated);
        assert_eq!(scan.steps.len(), 1);
    }

    #[test]
    fn a_log_bigger_than_the_budget_says_so() {
        let padding = "x".repeat(256);
        let file = logged(&[
            r#"{"jobID":"hyperfine-bench","step":"s","executionTime":1000000}"#,
            &padding,
        ]);
        let scan = scan(file.path(), JOB, 16, 5_000).expect("scan");
        assert!(scan.budget_exceeded);
    }

    #[test]
    fn go_durations_parse_and_nonsense_does_not() {
        for (text, nanos) in [
            ("1ns", 1),
            ("1.5us", 1_500),
            ("1\u{b5}s", 1_000),
            ("90s", 90_000_000_000),
            ("0s", 0),
        ] {
            assert_eq!(go_duration_ns(text), Some(nanos), "{text}");
        }
        for text in ["", "-", "-1s", "1", "s", "1w", "1.2.3s", "500 ms"] {
            assert_eq!(go_duration_ns(text), None, "{text} must not parse");
        }
    }

    #[test]
    fn nanoseconds_round_to_milliseconds_half_away_from_zero() {
        assert_eq!(ns_to_ms(88_234_000_000), 88_234);
        assert_eq!(ns_to_ms(500_000), 1);
        assert_eq!(ns_to_ms(499_999), 0);
        assert_eq!(ns_to_ms(0), 0);
    }

    #[test]
    fn the_marker_constant_is_the_grammars_literal() {
        assert_eq!(marker::MARKER, "ACTMCP_BENCHMARK");
        assert!(marker::mentions_marker(marker::MARKER));
    }
}
