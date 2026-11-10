//! Byte-exact goldens, built from the record API rather than parsed from a file.
//!
//! `tests/evidence_contract.rs` proves the writer round-trips the fixtures. That test
//! starts by *reading* a golden, so on its own it would still pass if the record builders
//! produced something the fixtures do not contain — it never checks that a caller who
//! builds a record field by field can express the pinned shape at all, nor in which
//! order the keys come out.
//!
//! This file closes that hole: every expected fixture's first line is rebuilt here from
//! `Envelope` + payload structs, then compared byte for byte with the fixture. ADR-0002
//! §6.1 makes field order part of the contract ("stable enough that a golden-file test is
//! meaningful"), so the comparison is on bytes, not on parsed JSON equality.

use std::path::{Path, PathBuf};

use act_mcp::evidence::{
    BenchmarkMetric, BenchmarkRecord, BenchmarkSource, Commit, CoverageCounts, CoverageRecord,
    CoverageSource, Envelope, Kind, MetricValue, RelPath, ReportedMeasure, RunId, RunRecords,
    RunStatus, Tool, UtcInstant,
};

const SHA: &str = "2f7c1a4b9e0d3c6a5b8f1e2d4c6a8b0f3e5d7c91";

fn golden(name: &str) -> String {
    let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/e2e/testdata/expected")
        .join(format!("{name}.jsonl"));
    let body = std::fs::read_to_string(&path).expect("golden fixture");
    body.lines()
        .next()
        .expect("a golden holds at least one record")
        .to_owned()
}

/// The envelope the fixtures share, apart from the fields each call names.
///
/// `tool.act_version` is set because the fixtures record the act build; a record built
/// without it is legal, and `coverage_log_scrape_matches` shows what it looks like.
fn envelope(
    kind: Kind,
    run_id: &str,
    workflow: &str,
    task: &str,
    run_status: RunStatus,
    started_at: &str,
) -> Envelope {
    Envelope::new(
        kind,
        RunId::parse(run_id).expect("fixture run_id"),
        Commit::parse(SHA).expect("fixture commit"),
        workflow.to_owned(),
        task.to_owned(),
        run_status,
        Tool {
            name: "act-mcp".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            act_version: Some("0.2.68".to_owned()),
        },
        UtcInstant::parse(started_at).expect("fixture instant"),
        "build-box-07".to_owned(),
    )
}

fn relpath(text: &str) -> RelPath {
    RelPath::parse(text).expect("fixture relpath")
}

fn line(record: &impl serde::Serialize) -> String {
    let bytes = serde_json::to_vec(record).expect("serialize");
    assert!(
        bytes.starts_with(b"{") && bytes.ends_with(b"}"),
        "one compact object"
    );
    assert!(
        !bytes.contains(&b'\n'),
        "no insignificant whitespace inside a record"
    );
    String::from_utf8(bytes).expect("UTF-8")
}

#[test]
fn a_benchmark_built_by_hand_matches_the_golden() {
    let envelope = envelope(
        Kind::Benchmark,
        "20261110T004203Z-a1b2c3",
        "benchmark",
        "hyperfine-bench",
        RunStatus::Success,
        "2026-11-10T00:42:03.417Z",
    )
    .with_duration_ms(94_122)
    .with_exit_code(0);
    let mut labels = std::collections::BTreeMap::new();
    labels.insert("job".to_owned(), "hyperfine-bench".to_owned());
    labels.insert("os".to_owned(), "ubuntu-24.04".to_owned());
    labels.insert("runner".to_owned(), "self-hosted".to_owned());
    let metric = BenchmarkMetric::new(
        "tokenizer_throughput".to_owned(),
        MetricValue::from_f64(184_320.4).expect("finite"),
        BenchmarkSource::JobFile,
    )
    .with_unit("ops/s")
    .with_labels(labels)
    .with_sample(1)
    .with_source_path(relpath(
        ".evidence/incoming/20261110T004203Z-a1b2c3/benchmark.json",
    ));

    assert_eq!(
        line(&BenchmarkRecord::new(&envelope, metric)),
        golden("benchmark"),
        "a hand-built benchmark record must be the pinned bytes"
    );
}

#[test]
fn a_failed_run_built_by_hand_matches_the_golden() {
    // The shape a failed run takes: `run_status: failed`, act's real exit code, and a
    // metric with no unit that says so via `unit_missing` — here it has a unit, so no
    // flag, which is the more common case (§4.1).
    let envelope = envelope(
        Kind::Benchmark,
        "20261110T041530Z-bbc0dd",
        "benchmark",
        "hyperfine-bench",
        RunStatus::Failed,
        "2026-11-10T04:15:30.612Z",
    )
    .with_duration_ms(305_117)
    .with_exit_code(1);
    let mut labels = std::collections::BTreeMap::new();
    labels.insert("job".to_owned(), "hyperfine-bench".to_owned());
    let metric = BenchmarkMetric::new(
        "job_duration_s".to_owned(),
        MetricValue::from_f64(300.9).expect("finite"),
        BenchmarkSource::ActLog,
    )
    .with_unit("s")
    .with_labels(labels);

    assert_eq!(
        line(&BenchmarkRecord::new(&envelope, metric)),
        golden("benchmark-failed")
    );
}

#[test]
fn a_pytest_run_built_by_hand_matches_the_golden() {
    let envelope = envelope(
        Kind::Coverage,
        "20261110T011507Z-d4e5f6",
        "coverage",
        "coverage",
        RunStatus::Success,
        "2026-11-10T01:15:07.004Z",
    )
    .with_duration_ms(61_233)
    .with_exit_code(0);
    let counts = CoverageCounts::total(4120, 3581, CoverageSource::Lcov)
        .with_branches(1180, 946)
        .with_source_path(relpath(
            ".evidence/incoming/20261110T011507Z-d4e5f6/lcov.info",
        ));

    assert_eq!(
        line(&CoverageRecord::new(&envelope, counts)),
        golden("coverage"),
        "the count pairs must be written in the order the fixture lists them"
    );
}

#[test]
fn an_istanbul_run_built_by_hand_matches_the_golden() {
    // All four count pairs at once: this is the golden that pins their relative order
    // (lines, statements, branches, functions — §4.3).
    let envelope = envelope(
        Kind::Coverage,
        "20261110T021244Z-0f9e8d",
        "coverage",
        "coverage-istanbul",
        RunStatus::Success,
        "2026-11-10T02:12:44.551Z",
    )
    .with_duration_ms(40_017)
    .with_exit_code(0);
    let counts = CoverageCounts::total(2210, 1904, CoverageSource::CoverageSummary)
        .with_statements(2415, 2079)
        .with_branches(702, 561)
        .with_functions(318, 286)
        .with_source_path(relpath(
            ".evidence/incoming/20261110T021244Z-0f9e8d/coverage-summary.json",
        ));

    assert_eq!(
        line(&CoverageRecord::new(&envelope, counts)),
        golden("coverage-istanbul")
    );
}

#[test]
fn a_log_scraped_run_built_by_hand_matches_the_golden() {
    // The last-resort shape: a percentage with no raw counts behind it, which must arrive
    // with `percent_only` in `flags` and `reported_measure` alongside (§4.3). Built here
    // with `with_percent_only`, which makes half-applying those rules impossible.
    let envelope = envelope(
        Kind::Coverage,
        "20261110T030900Z-77aa55",
        "coverage",
        "coverage-ad-hoc",
        RunStatus::Success,
        "2026-11-10T03:09:00.000Z",
    )
    .with_duration_ms(22_741)
    .with_exit_code(0);
    let mut extra = serde_json::Map::new();
    extra.insert(
        "scraped_line".to_owned(),
        serde_json::Value::String("Coverage: 87.4%".to_owned()),
    );
    extra.insert(
        "note".to_owned(),
        serde_json::Value::String("no lcov.info or coverage-summary.json produced".to_owned()),
    );
    // `low_confidence` is added first because the fixture lists `flags` in that order and
    // the writer keeps insertion order; `with_percent_only` then adds its own flag only if
    // it is not already there.
    let counts = CoverageCounts::total(0, 0, CoverageSource::LogScrape)
        .with_flag("low_confidence")
        .with_percent_only(87.4, ReportedMeasure::Lines)
        .with_extra(extra);

    assert_eq!(
        line(&CoverageRecord::new(&envelope, counts)),
        golden("coverage-log-scrape"),
        "`flags` must keep insertion order and `extra` must come last"
    );
}

#[test]
fn a_run_of_hand_built_records_writes_the_whole_file() {
    // The file-level contract in one assertion: two lines, each LF-terminated, no joiner,
    // no trailing blank line beyond the final LF.
    let envelope = envelope(
        Kind::Benchmark,
        "20261110T004203Z-a1b2c3",
        "benchmark",
        "hyperfine-bench",
        RunStatus::Success,
        "2026-11-10T00:42:03.417Z",
    )
    .with_duration_ms(94_122)
    .with_exit_code(0);
    let record = |name: &str| {
        BenchmarkRecord::new(
            &envelope,
            BenchmarkMetric::new(
                name.to_owned(),
                MetricValue::from_i64(1),
                BenchmarkSource::ActLog,
            )
            .with_unit("count"),
        )
    };
    let run = RunRecords::Benchmark {
        records: vec![record("tokenizer_throughput"), record("cold_start_ms")],
    };
    run.check().expect("the batch is one run");

    let jsonl = String::from_utf8(run.to_jsonl().expect("jsonl")).expect("UTF-8");
    let first = format!("{}\n", line(&record("tokenizer_throughput")));
    assert!(
        jsonl.starts_with(&first),
        "first line must be the record on its own, LF-terminated: {jsonl}"
    );
    assert_eq!(jsonl.lines().count(), 2);
    assert!(jsonl.ends_with('\n') && !jsonl.ends_with("\n\n"));

    // And it reads back as the same batch: parse is the inverse of write, not a
    // third interpretation.
    let again = RunRecords::from_jsonl(jsonl.as_bytes()).expect("re-parse");
    assert_eq!(
        again.to_jsonl().expect("jsonl"),
        run.to_jsonl().expect("jsonl")
    );
}

#[test]
fn nested_object_keys_are_written_in_ascending_order() {
    // The other half of the key-order rule (ADR-0002 §6.1): top-level keys follow the
    // record type's declaration order, and the objects *inside* a record — `labels`,
    // `extra` — come out sorted by key, because the writer serializes them as a map.
    // Determinism is the requirement; this pins which determinism it is, so the goldens
    // and the writer cannot disagree about it.
    let envelope = envelope(
        Kind::Coverage,
        "20261110T030900Z-77aa55",
        "coverage",
        "coverage-ad-hoc",
        RunStatus::Success,
        "2026-11-10T03:09:00.000Z",
    )
    .with_duration_ms(22_741)
    .with_exit_code(0);
    let mut extra = serde_json::Map::new();
    for (key, value) in [("zebra", "3"), ("alpha", "1"), ("Middle", "2")] {
        extra.insert(key.to_owned(), serde_json::Value::String(value.to_owned()));
    }
    let counts = CoverageCounts::total(10, 5, CoverageSource::LogScrape)
        .with_flag("low_confidence")
        .with_percent_only(50.0, ReportedMeasure::Lines)
        .with_extra(extra);
    let record = line(&CoverageRecord::new(&envelope, counts));

    let position = |needle: &str| {
        record
            .find(needle)
            .unwrap_or_else(|| panic!("{needle} missing from {record}"))
    };
    assert!(
        position("\"Middle\"") < position("\"alpha\"")
            && position("\"alpha\"") < position("\"zebra\""),
        "extra keys are ordered by code point, which is ASCII-order: {record}"
    );
}
