//! Benchmark extraction, measured against the pinned contract fixtures (mact-f1w.12).
//!
//! The entry point under test is [`act_mcp::evidence::benchmark::extract`]: a finished
//! [`RunOutcome`] plus the [`ValidatedRunTarget`] it was validated against go in, benchmark
//! records come out. The two goldens are reproduced byte for byte from a fixture pair —
//! `tests/fixtures/benchmark/benchmark.json` (what the job wrote through the bind mount)
//! and `tests/fixtures/benchmark/act.jsonl` (act's own log: its stdout, its marker lines,
//! its step timings) — because those two files together are the whole input surface this
//! ticket has: a payload file and a log.
//!
//! ## Why the act log is a fixture and not a string in a test
//!
//! The log is act's format, not ours (ADR-0002 §1.3): the field names that carry a step's
//! timing (`executionTime`, `step`, `jobID`, `time`) belong to a third-party tool, so they
//! are pinned in a file a reviewer can read as one log and diff, and the parser is written
//! against that pin rather than against a guess. The values in `act.jsonl` are the ones the
//! golden records report: the three steps sum to the run's `job_duration_s` of 91.8 s, and
//! the `Run benchmark` step alone is the golden's 88.234 s.
//!
//! Everything the contract says must *count* rather than publish — `NaN`-shaped values,
//! negative durations, the metric cap, malformed marker lines, a failed run's records — is
//! asserted below on inputs built inside the test, because those scenarios are about the
//! counts and the flags, not about a golden's bytes.
//!
//! The envelope's `host` and `tool.act_version` are pinned through [`ExtractContext`] to the
//! values the fixtures use (`build-box-07`, `0.2.68`), exactly as `coverage_extraction.rs`
//! does: the testdata README's "volatile fields are normalised explicitly" rule.

use std::{
    fs::Permissions,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use act_mcp::{
    evidence::{
        benchmark::{
            extract_with, BenchmarkSummary, Discovered, ExtractionOutcome, SourceCounts,
            WARNING_BENCHMARK_SOURCE_SHADOWED, WARNING_MALFORMED_METRIC,
            WARNING_MARKER_CAP_REACHED, WARNING_NO_BENCHMARK_METRICS, WARNING_PARSE_FAILED,
            WARNING_PAYLOAD_TOO_LARGE, WARNING_PAYLOAD_UNREADABLE,
            WARNING_STEP_TIMINGS_UNAVAILABLE, WARNING_TRUNCATED,
        },
        schema::validate_record,
        BenchmarkSource, ExtractContext,
    },
    runner::ActRunOutcome,
    workspace::{IsolationMode, ValidatedRunTarget},
};
use chrono::{DateTime, Utc};

/// The commit every contract fixture is pinned to (testdata README).
const SHA: &str = "2f7c1a4b9e0d3c6a5b8f1e2d4c6a8b0f3e5d7c91";
/// The hostname the fixtures pin.
const HOST: &str = "build-box-07";
/// The `tool.act_version` the fixtures pin.
const ACT_VERSION: &str = "0.2.68";
/// The run id the benchmark fixtures pin.
const RUN_ID: &str = "20261110T004203Z-a1b2c3";
/// The run id the failed benchmark fixture pins.
const FAILED_RUN_ID: &str = "20261110T041530Z-bbc0dd";
/// The job id — `act -j`'s argument, and every `labels.job` in the goldens.
const JOB: &str = "hyperfine-bench";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/benchmark")
}

fn expected(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/e2e/testdata/expected")
        .join(format!("{name}.jsonl"))
}

fn read(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_else(|reason| panic!("read {}: {reason}", path.display()))
}

/// A prepared run: a tree, an incoming directory, a captured log, and the two arguments
/// `extract` takes.
struct Harness {
    /// Kept alive so the tree exists for the whole test; nothing reads it, which is the
    /// point — its `Drop` is the test's cleanup.
    #[allow(dead_code)]
    dir: tempfile::TempDir,
    tree: PathBuf,
    incoming: PathBuf,
    outcome: ActRunOutcome,
    target: ValidatedRunTarget,
}

impl Harness {
    /// A tree whose run ended the way the pinned success fixture says it did.
    fn new(run_id: &str) -> Self {
        Self::with_status(
            run_id,
            act_mcp::runner::RunStatus::Success,
            Some(0),
            "2026-11-10T00:42:03.417Z",
            94_122,
        )
    }

    fn with_status(
        run_id: &str,
        status: act_mcp::runner::RunStatus,
        exit_code: Option<u32>,
        started_at: &str,
        duration_ms: u64,
    ) -> Self {
        let dir = tempfile::tempdir().expect("tempdir for the run tree");
        let tree = dir.path().canonicalize().expect("canonical tempdir");
        let incoming = tree.join(".evidence/incoming").join(run_id);
        let runs = tree.join(".evidence/runs").join(run_id);
        std::fs::create_dir_all(&incoming).expect("create the incoming dir");
        std::fs::create_dir_all(&runs).expect("create the runs dir");
        std::fs::create_dir_all(tree.join(".github/workflows")).expect("create workflows");
        // A log exists even when a test never fills it: a run always has one.
        std::fs::write(runs.join("act.jsonl"), b"").expect("write the act log");

        let started: DateTime<Utc> = DateTime::parse_from_rfc3339(started_at)
            .unwrap_or_else(|reason| panic!("{started_at}: {reason}"))
            .with_timezone(&Utc);
        let outcome = ActRunOutcome {
            run_id: run_id.to_owned(),
            status,
            exit_code,
            started_at: started,
            finished_at: started + chrono::Duration::milliseconds(duration_ms as i64),
            duration_ms,
            pid: 418_212,
            pgid: 418_212,
            log_path: runs.join("act.jsonl"),
            log_bytes: 0,
            log_truncated: false,
            log_dropped_lines: 0,
            log_stdout_bytes: 0,
            log_stderr_bytes: 0,
            log_tail: String::new(),
            kill: None,
            initiator: None,
            warnings: vec![],
            failure: None,
            dry_run: false,
        };
        let target = ValidatedRunTarget {
            workflow_file: tree.join(".github/workflows/benchmark.yml"),
            workflow_display: "benchmark".to_owned(),
            workflow_relpath: ".github/workflows/benchmark.yml".to_owned(),
            job_id: JOB.to_owned(),
            commit_full_sha: SHA.to_owned(),
            category: "benchmark".to_owned(),
            workspace_root: tree.clone(),
            evidence_dir: tree.join(".evidence"),
            mode: IsolationMode::HeadOnly,
            warnings: vec![],
        };
        Self {
            dir,
            tree,
            incoming,
            outcome,
            target,
        }
    }

    /// The extraction context with the fixtures' pinned host and act version.
    fn context(&self) -> ExtractContext {
        ExtractContext {
            tree_root: self.tree.clone(),
            workspace_root: self.tree.clone(),
            payload_dir: self.incoming.clone(),
            host: HOST.to_owned(),
            act_version: Some(ACT_VERSION.to_owned()),
            server_version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }

    /// Put a committed fixture into the run's `$ACTMCP_EVIDENCE_OUT`, under the contract's
    /// filename.
    fn payload(&self, fixture: &str, as_name: &str) {
        std::fs::copy(fixtures().join(fixture), self.incoming.join(as_name))
            .expect("copy the payload fixture into the incoming dir");
    }

    /// Write a payload built by the test itself.
    fn payload_text(&self, name: &str, body: &str) {
        std::fs::write(self.incoming.join(name), body).expect("write the payload");
    }

    /// Copy a fixture at the tree's root, the fallback places §3.2 allows.
    fn at_tree_root(&self, name: &str, body: &str) {
        std::fs::write(self.tree.join(name), body).expect("write the tree-root payload");
    }

    /// Point the run's captured log at a committed fixture.
    fn use_log(&self, fixture: &str) {
        std::fs::write(&self.outcome.log_path, read(&fixtures().join(fixture)))
            .expect("write the log fixture");
    }

    /// Point the run's captured log at text the test built.
    fn log_text(&self, body: &str) {
        std::fs::write(&self.outcome.log_path, body).expect("write the log");
    }

    /// `extract`, with the machine-specific envelope fields pinned.
    fn extract(&self) -> BenchmarkSummary {
        extract_with(&self.outcome, &self.target, &self.context())
            .unwrap_or_else(|reason| panic!("extraction itself must not fail: {reason}"))
    }
}

/// The records as the file that would be published, then compared to a golden byte for byte.
fn assert_golden(summary: &BenchmarkSummary, golden: &str) {
    let run = summary
        .run_records()
        .unwrap_or_else(|| panic!("expected records to match {golden}"));
    run.check().expect("one run's records, each one checked");
    let written = run.to_jsonl().expect("serialize");
    let wanted = read(&expected(golden));
    assert_eq!(
        String::from_utf8_lossy(&written),
        String::from_utf8_lossy(&wanted),
        "{golden} must be reproduced byte for byte"
    );
    // Every line also validates against the pinned schema, not just against the golden.
    for line in written.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_slice(line).expect("one JSON object");
        validate_record(&value).unwrap_or_else(|problem| {
            panic!("{golden}: a record failed the pinned schema: {problem}")
        });
    }
}

/// The metric half of one record, for the structural assertions.
fn fields(record: &act_mcp::evidence::BenchmarkRecord) -> serde_json::Value {
    serde_json::to_value(record).expect("a record serializes")
}

/// One of the run's own steps, reporting `millis` in act's JSON log form.
fn own_step(step: &str, millis: u64, at: &str) -> String {
    format!(
        r#"{{"dryrun":false,"executionTime":{millis},"job":"hyperfine-bench","jobID":"hyperfine-bench","level":"info","msg":"  Success - Main {step}","stage":"main","step":"{step}","stepResult":"success","time":"{at}"}}"#
    )
}

/// A step belonging to some other job of the same act run.
fn other_job_step(job: &str, step: &str, millis: u64, at: &str) -> String {
    format!(
        r#"{{"dryrun":false,"executionTime":{millis},"job":"{job}","jobID":"{job}","level":"info","msg":"  Success - Main {step}","stage":"main","step":"{step}","stepResult":"success","time":"{at}"}}"#
    )
}

/// A marker as it sits inside a JSON log line's `msg`, escaped the way act escapes it.
fn raw_marker(payload: &str) -> String {
    format!(
        r#"{{"level":"info","job":"hyperfine-bench","jobID":"hyperfine-bench","raw_output":true,"msg":"ACTMCP_BENCHMARK {}"}}"#,
        payload.replace('"', "\\\"")
    )
}

fn names(summary: &BenchmarkSummary) -> Vec<String> {
    summary
        .records
        .iter()
        .map(|record| record.metric.name.clone())
        .collect()
}

fn find(summary: &BenchmarkSummary, name: &str, step: Option<&str>) -> Vec<serde_json::Value> {
    summary
        .records
        .iter()
        .map(fields)
        .filter(|record| record["name"] == name)
        .filter(|record| match step {
            Some(step) => record["labels"]["step"] == step,
            None => true,
        })
        .collect()
}

fn marker_line(metric: &str) -> String {
    format!(
        r#"{{"dryrun":false,"job":"{JOB}","jobID":"{JOB}","level":"info","msg":"ACTMCP_BENCHMARK {}","raw_output":true,"step":"Run benchmark","time":"2026-11-10T00:43:00Z"}}"#,
        escaped(metric)
    )
}

/// JSON text as it sits inside a JSON string value — which is where act puts a step's stdout.
fn escaped(payload: &str) -> String {
    payload.replace('"', "\\\"")
}

/// One step-completion line in the shape act writes it.
fn step_line(step: &str, nanos: u64, at: &str) -> String {
    format!(
        r#"{{"dryrun":false,"executionTime":{nanos},"job":"{JOB}","jobID":"{JOB}","level":"info","msg":"  ✅  Success - Main {step}","stage":"main","step":"{step}","time":"{at}"}}"#
    )
}

// ── the goldens ──────────────────────────────────────────────────────────────

/// One payload file and one act log, and the whole pinned golden: two samples from the
/// file, two marker metrics out of the log's step output, and act's own timings.
#[test]
fn the_pinned_benchmark_golden_is_reproduced_from_its_fixture_pair() {
    let harness = Harness::new(RUN_ID);
    harness.payload("benchmark.json", "benchmark.json");
    harness.use_log("act.jsonl");
    let summary = harness.extract();

    assert_eq!(summary.outcome, ExtractionOutcome::Extracted);
    assert_eq!(summary.len(), 8);
    assert_eq!(
        summary.from,
        SourceCounts {
            job_file: 2,
            in_band_stdout: 2,
            act_log: 4,
        },
        "two metrics from the file, two markers, and act's job total plus its three steps"
    );
    assert_eq!(summary.malformed, 0, "nothing in the golden pair is junk");
    assert_eq!(
        summary.duplicates, 1,
        "the log's duplicate of sample 1 is dropped"
    );
    assert_eq!(
        summary.warnings,
        Vec::<String>::new(),
        "a clean run warns about nothing"
    );
    assert_eq!(
        summary
            .source_path
            .as_ref()
            .expect("file-derived records cite their file")
            .as_str(),
        ".evidence/incoming/20261110T004203Z-a1b2c3/benchmark.json"
    );
    assert_eq!(
        summary.found,
        vec![Discovered {
            path: ".evidence/incoming/20261110T004203Z-a1b2c3/benchmark.json".to_owned(),
            format: "json",
            used: true,
            bytes: summary.found[0].bytes,
        }],
        "every payload file that existed is named, with the winner marked"
    );
    assert_golden(&summary, "benchmark");
}

/// The failed run's golden: no payload, no markers, and act's timings alone — with the
/// envelope carrying the run's real status so a consumer can filter them (§5.1).
#[test]
fn the_failed_run_golden_is_reproduced_and_tagged_failed() {
    let harness = Harness::with_status(
        FAILED_RUN_ID,
        act_mcp::runner::RunStatus::Failed,
        Some(1),
        "2026-11-10T04:15:30.612Z",
        305_117,
    );
    harness.use_log("act-failed.jsonl");
    let summary = harness.extract();

    assert_eq!(summary.outcome, ExtractionOutcome::Derived);
    assert_eq!(summary.from.job_file, 0, "the failed job wrote no payload");
    assert_eq!(summary.from.in_band_stdout, 0);
    assert_eq!(summary.from.act_log, 3);
    assert_eq!(
        summary.found.first().map(|file| file.path.as_str()),
        None,
        "nothing existed to find"
    );
    assert_golden(&summary, "benchmark-failed");

    // The same fact the golden encodes, stated as an assertion: every record of a red run
    // says it is from a red run, and says what act's exit code was.
    for record in &summary.records {
        assert_eq!(
            record.envelope.run_status,
            act_mcp::evidence::RunStatus::Failed
        );
        assert_eq!(record.envelope.exit_code, Some(1));
        assert_eq!(record.envelope.duration_ms, Some(305_117));
    }
}

/// A run that was cancelled is not a success either: the status word travels, and nothing
/// is dropped because of it.
#[test]
fn a_cancelled_runs_status_travels_to_every_record() {
    let harness = Harness::with_status(
        RUN_ID,
        act_mcp::runner::RunStatus::Cancelled,
        None,
        "2026-11-10T00:42:03.417Z",
        12_000,
    );
    harness.log_text(&step_line(
        "Run benchmark",
        10_000_000_000,
        "2026-11-10T00:42:15Z",
    ));
    let summary = harness.extract();
    assert_eq!(
        summary.len(),
        2,
        "job and step duration, from the log alone"
    );
    for record in &summary.records {
        assert_eq!(
            record.envelope.run_status,
            act_mcp::evidence::RunStatus::Cancelled
        );
        assert_eq!(
            record.envelope.exit_code, None,
            "act was signalled, so it reported no code, and absence beats a made-up 0"
        );
    }
}

// ── what counts as a number ──────────────────────────────────────────────────

/// §3.3: a value that is not a finite JSON number is rejected and counted, and the run is
/// none the worse for it.
#[test]
fn non_numeric_and_out_of_range_values_are_rejected_and_counted() {
    let harness = Harness::new(RUN_ID);
    harness.payload_text(
        "benchmark.jsonl",
        concat!(
            "{\"name\":\"good\",\"value\":1.5,\"unit\":\"ms\"}\n",
            "{\"name\":\"a_string\",\"value\":\"12ms\"}\n",
            "{\"name\":\"a_null\",\"value\":null}\n",
            "{\"name\":\"a_bool\",\"value\":true}\n",
            "{\"name\":\"out_of_range\",\"value\":1e999}\n",
            "{\"name\":\"no_value\"}\n",
            "{\"value\":7}\n",
            "not json at all\n",
            "{\"name\":\"also_good\",\"value\":0}\n",
        ),
    );
    let summary = harness.extract();

    assert_eq!(
        names(&summary),
        vec!["good", "also_good"],
        "zero is a measurement; a string, a null, a bool, an out-of-range literal, a \
         missing or unnamed value are not"
    );
    assert_eq!(
        summary.malformed, 7,
        "six bad entries and one unparseable line"
    );
    assert!(summary
        .warnings
        .contains(&WARNING_MALFORMED_METRIC.to_owned()));
    assert_eq!(
        summary.outcome,
        ExtractionOutcome::Extracted,
        "a payload with one good entry is still a payload that was read"
    );
    for record in &summary.records {
        // The run's status is exactly what act said, however junky the payload was.
        assert_eq!(
            record.envelope.run_status,
            act_mcp::evidence::RunStatus::Success
        );
    }
}

/// §4.1: negative is legal for a delta and never for a duration.
#[test]
fn a_negative_duration_is_rejected_and_a_negative_delta_is_not() {
    let harness = Harness::new(RUN_ID);
    harness.payload_text(
        "benchmark.json",
        r#"{"metrics":[
            {"name":"regression_vs_last_commit","value":-12.5,"unit":"ms"},
            {"name":"job_duration_s","value":-1,"unit":"s"},
            {"name":"step_duration_s","value":-0.5,"unit":"s"},
            {"name":"cold_start_ms","value":-3,"unit":"ms"}
        ]}"#,
    );
    let summary = harness.extract();
    assert_eq!(
        names(&summary),
        vec!["regression_vs_last_commit", "cold_start_ms"],
        "only the `*_duration_s` names are refused a negative value"
    );
    assert_eq!(summary.malformed, 2);
}

// ── duplicates, samples, and the cap ─────────────────────────────────────────

/// §4.4: a repeat of one already kept is a chatty job; a genuine repeat of values is a
/// sample and is numbered.
#[test]
fn an_identical_metric_is_deduped_and_a_genuine_repeat_becomes_a_sample() {
    let harness = Harness::new(RUN_ID);
    harness.payload_text(
        "benchmark.json",
        r#"{"metrics":[
            {"name":"once","value":1},
            {"name":"once","value":1},
            {"name":"sampled","value":1},
            {"name":"sampled","value":2},
            {"name":"sampled","value":3},
            {"name":"labelled","value":1,"labels":{"os":"ubuntu-24.04"}},
            {"name":"labelled","value":1,"labels":{"os":"fedora-41"}}
        ]}"#,
    );
    let summary = harness.extract();

    assert_eq!(
        summary.duplicates, 1,
        "the exact repeat is dropped and counted"
    );
    assert_eq!(names(&summary).len(), 6);

    // §4.1: `sample` is present only when a group really has several samples, so a lone
    // measurement carries none — an index that can only be 1 says nothing.
    assert_eq!(find(&summary, "once", None)[0].get("sample"), None);
    let samples: Vec<u64> = find(&summary, "sampled", None)
        .iter()
        .map(|record| record["sample"].as_u64().expect("numbered"))
        .collect();
    assert_eq!(samples, vec![1, 2, 3], "three values are samples 1..3");
    // Same name, different labels: two different measurements, not samples of one thing,
    // so neither is numbered.
    for record in find(&summary, "labelled", None) {
        assert_eq!(
            record.get("sample"),
            None,
            "one measurement per label set is not a sample: {record}"
        );
    }
}

/// §3.3 and §5.1: the cap is enforced, the excess is dropped, and every surviving record of
/// the run carries `truncated` so a consumer cannot average a partial run silently.
#[test]
fn the_metric_cap_truncates_the_run_and_flags_every_record() {
    let harness = Harness::new(RUN_ID);
    let mut payload = String::new();
    for index in 0..5_004 {
        payload.push_str(&format!(
            "{{\"name\":\"metric_{index}\",\"value\":{index},\"unit\":\"count\"}}\n"
        ));
    }
    harness.payload_text("benchmark.jsonl", &payload);
    let summary = harness.extract();

    assert_eq!(summary.len(), 5_000, "the per-run cap is the cap");
    assert!(summary.truncated);
    assert!(summary.warnings.contains(&WARNING_TRUNCATED.to_owned()));
    let run = summary.run_records().expect("records");
    assert!(run.truncated(), "the published file says it is truncated");
    for record in &summary.records {
        assert!(
            record
                .metric
                .flags
                .as_ref()
                .is_some_and(|flags| flags.iter().any(|flag| flag == "truncated")),
            "every record of a truncated run says so"
        );
    }
    // The numbers that survived are the first ones: the payload's order is the file order.
    assert_eq!(summary.records.first().unwrap().metric.name, "metric_0");
    assert_eq!(summary.records.last().unwrap().metric.name, "metric_4999");
}

// ── units, labels, extra ─────────────────────────────────────────────────────

/// §4.1: a unit is copied verbatim or absent-and-flagged; act-mcp never invents or converts
/// one, because a scanner diffs units across commits.
#[test]
fn units_pass_through_verbatim_and_a_missing_unit_says_so() {
    let harness = Harness::new(RUN_ID);
    harness.payload_text(
        "benchmark.json",
        r#"{"metrics":[
            {"name":"throughput","value":1,"unit":"ops/s"},
            {"name":"latency","value":2,"unit":"us-per-key"},
            {"name":"non_ascii_unit","value":9,"unit":"µs"},
            {"name":"ratio","value":3,"unit":"%"},
            {"name":"unnamed_unit","value":4},
            {"name":"empty_unit","value":5,"unit":""}
        ]}"#,
    );
    let summary = harness.extract();
    let units: Vec<Option<String>> = summary
        .records
        .iter()
        .map(|record| record.metric.unit.clone())
        .collect();
    assert_eq!(
        units,
        vec![
            Some("ops/s".to_owned()),
            Some("us-per-key".to_owned()),
            Some("%".to_owned()),
            None
        ],
        "no unit is converted, scaled, or defaulted; an empty or non-ASCII unit is not a unit"
    );
    let flagless = summary
        .records
        .iter()
        .filter(|record| record.metric.unit.is_none())
        .collect::<Vec<_>>();
    assert_eq!(flagless.len(), 1);
    assert_eq!(
        flagless[0].metric.flags.clone(),
        Some(vec!["unit_missing".to_owned()]),
        "absence plus the flag, so a consumer can tell it from a real unit"
    );
    assert_eq!(
        summary.malformed, 2,
        "the empty-unit and the non-ASCII-unit entries are the two rejections (§4.1's shape)"
    );
    for record in &summary.records {
        let value = fields(record);
        validate_record(&value).expect("every record satisfies the pinned schema");
    }
}

/// `extra` is advisory and travels; labels are the job's own and travel too.
#[test]
fn extra_and_labels_are_passed_through_and_the_record_stays_valid() {
    let harness = Harness::new(RUN_ID);
    harness.payload_text(
        "benchmark.json",
        r#"{"metrics":[{"name":"tokenizer_throughput","value":1,"unit":"ops/s",
             "labels":{"os":"ubuntu-24.04","matrix":"3.12"},
             "extra":{"tool":"hyperfine","samples":100}}]}"#,
    );
    let record = fields(&harness.extract().records.remove(0));
    assert_eq!(record["labels"]["os"], "ubuntu-24.04");
    assert_eq!(record["labels"]["matrix"], "3.12");
    assert_eq!(record["extra"]["tool"], "hyperfine");
    assert_eq!(record["extra"]["samples"], 100);
    validate_record(&record).expect("and it is still a contract record");
}

/// A payload may not re-attribute a run: identity comes from the tool call, never from file
/// contents (§3.3).
#[test]
fn a_payload_cannot_re_attribute_the_run() {
    let harness = Harness::new(RUN_ID);
    harness.payload_text(
        "benchmark.json",
        r#"{"metrics":[{"name":"x","value":1,"commit":"0000000000000000000000000000000000000000",
             "run_id":"20200101T000000Z-deadbe","workflow":"someone-elses","task":"lies",
             "kind":"coverage","schema_version":2}]}"#,
    );
    let summary = harness.extract();
    assert_eq!(summary.len(), 1);
    let record = fields(&summary.records[0]);
    assert_eq!(record["commit"], SHA);
    assert_eq!(record["run_id"], RUN_ID);
    assert_eq!(record["workflow"], "benchmark");
    assert_eq!(record["task"], JOB);
    assert_eq!(record["kind"], "benchmark");
    assert_eq!(record["schema_version"], 1);
}

// ── the in-band marker ───────────────────────────────────────────────────────

/// §5.2's precedence: a marker that repeats the payload wins nothing, and the payload's
/// record keeps `source: "job_file"`.
#[test]
fn a_marker_that_repeats_a_payload_metric_loses_to_the_payload() {
    let harness = Harness::new(RUN_ID);
    harness.payload_text(
        "benchmark.json",
        r#"{"metrics":[{"name":"cold_start_ms","value":412.5,"unit":"ms"}]}"#,
    );
    harness.log_text(&format!(
        "{}\n{}\n{}\n{}\n",
        r#"{"job":"hyperfine-bench","jobID":"hyperfine-bench","level":"info","msg":"a line before the step"}"#,
        r#"{"job":"hyperfine-bench","jobID":"hyperfine-bench","level":"info","msg":"the README documents ACTMCP_BENCHMARK as a marker","step":"Run benchmark"}"#,
        raw_marker(r#"{"name":"cold_start_ms","value":412.5,"unit":"ms"}"#),
        raw_marker(r#"{"name":"peak_rss_bytes","value":1048576}"#),
    ));
    let summary = harness.extract();

    let cold = find(&summary, "cold_start_ms", None);
    assert_eq!(
        cold.len(),
        1,
        "the repeat is dropped, not numbered as a sample"
    );
    assert_eq!(
        cold[0]["source"], "job_file",
        "and the surviving record is the authoritative one"
    );
    assert_eq!(summary.duplicates, 1);
    assert_eq!(
        names(&summary),
        vec!["cold_start_ms", "peak_rss_bytes"],
        "prose that merely names the marker is not a metric: only an anchored line is"
    );
}

/// §5.2: a malformed marker line is counted and ignored, never a run's failure.
#[test]
fn malformed_marker_lines_are_counted_and_never_fail_the_run() {
    let harness = Harness::new(RUN_ID);
    harness.log_text(concat!(
        "ACTMCP_BENCHMARK {\"name\":\"good\",\"value\":1,\"unit\":\"ms\"}\n",
        "ACTMCP_BENCHMARK {\"name\":\"broken\",}\n",
        "ACTMCP_BENCHMARK {\"name\":\"not_a_number\",\"value\":\"12ms\"}\n",
        "ACTMCP_BENCHMARK {}\n",
        "ACTMCP_BENCHMARK not json\n",
    ));
    let summary = harness.extract();

    assert_eq!(names(&summary), vec!["good"]);
    assert_eq!(
        summary.malformed, 3,
        "the three lines that are marker-shaped and unusable count; `ACTMCP_BENCHMARK not \
         json` does not match the grammar at all, so it is a line that mentions the marker, \
         not a metric the job tried to report"
    );
    assert!(summary
        .warnings
        .contains(&WARNING_MALFORMED_METRIC.to_owned()));
    assert_eq!(summary.outcome, ExtractionOutcome::Derived);
    assert_eq!(
        summary.records[0].envelope.run_status,
        act_mcp::evidence::RunStatus::Success
    );
}

/// §5.2: the marker lives in act's log, so it is found in the JSON `msg` field and in a
/// plain-text log alike.
#[test]
fn a_marker_is_found_wherever_the_log_put_the_step_output() {
    let harness = Harness::new(RUN_ID);
    let in_output_field = format!(
        r#"{{"job":"hyperfine-bench","level":"info","Output":"ACTMCP_BENCHMARK {}"}}"#,
        escaped(r#"{"name":"from_output","value":2}"#)
    );
    harness.log_text(&format!(
        "{}\n{}\n{}\n{}\n",
        raw_marker(r#"{"name":"from_msg","value":1}"#),
        in_output_field,
        r#"ACTMCP_BENCHMARK {"name":"from_plain_text","value":3}"#,
        r#"{"job":"hyperfine-bench","level":"info","msg":"a line with no marker at all"}"#,
    ));
    let summary = harness.extract();
    assert_eq!(
        names(&summary),
        vec!["from_msg", "from_output", "from_plain_text"]
    );
    assert_eq!(summary.malformed, 0);
}

/// §5.2's marker cap: the excess is not collected, and the run says so — while the timings
/// further down the log are still read.
#[test]
fn the_marker_cap_stops_the_markers_and_says_so() {
    let harness = Harness::new(RUN_ID);
    let mut log = String::new();
    for index in 0..5_002 {
        log.push_str(&marker_line(&format!(
            "{{\"name\":\"m{index}\",\"value\":{index}}}"
        )));
        log.push('\n');
    }
    log.push_str(&step_line(
        "Run benchmark",
        5_000_000_000,
        "2026-11-10T00:44:00Z",
    ));
    harness.log_text(&log);
    let summary = harness.extract();

    assert_eq!(summary.from.in_band_stdout, 5_000, "the cap is the cap");
    assert!(summary
        .warnings
        .contains(&WARNING_MARKER_CAP_REACHED.to_owned()));
    assert!(summary.warnings.contains(&WARNING_TRUNCATED.to_owned()));
    assert_eq!(
        summary.from.act_log, 0,
        "the per-run budget is shared across the three sources, so 5 000 markers used it up \
         and act's timings had no room left — which is what every record's `truncated` flag \
         is for: a consumer must not sum these and call it the run"
    );
    assert!(summary.records.iter().all(|record| {
        record
            .metric
            .flags
            .as_ref()
            .is_some_and(|flags| flags.iter().any(|flag| flag == "truncated"))
    }));
}

// ── which file wins ──────────────────────────────────────────────────────────

/// §3.2: the directory we told the job to write into beats a stray file at the tree root,
/// and the fact that both existed is reported rather than silently resolved.
#[test]
fn the_incoming_payload_wins_and_a_shadowed_file_is_named() {
    let harness = Harness::new(RUN_ID);
    harness.payload_text(
        "benchmark.json",
        r#"{"metrics":[{"name":"from_the_bind_mount","value":1,"unit":"count"}]}"#,
    );
    harness.at_tree_root(
        "benchmark.json",
        r#"{"metrics":[{"name":"from_an_earlier_build","value":2,"unit":"count"}]}"#,
    );
    let summary = harness.extract();

    assert_eq!(names(&summary), vec!["from_the_bind_mount"]);
    assert!(summary
        .warnings
        .contains(&WARNING_BENCHMARK_SOURCE_SHADOWED.to_owned()));
    assert_eq!(summary.found.len(), 2, "both files that existed are listed");
    assert_eq!(
        summary
            .found
            .iter()
            .filter(|file| file.used)
            .map(|file| file.path.clone())
            .collect::<Vec<_>>(),
        vec![format!(".evidence/incoming/{RUN_ID}/benchmark.json")],
        "exactly one of them is marked as the source"
    );
}

/// The directory outranks the filename: a run that wrote the `.jsonl` form into the bind
/// mount beats a stale `.json` sitting at the tree root, even though `.json` is the
/// higher-precedence *name* — the two names are alternative spellings of one kind of file.
#[test]
fn the_bind_mounted_directory_outranks_the_tree_roots_more_preferred_name() {
    let harness = Harness::new(RUN_ID);
    harness.payload_text(
        "benchmark.jsonl",
        "{\"name\":\"from_the_bind_mount\",\"value\":1,\"unit\":\"count\"}\n",
    );
    harness.at_tree_root(
        "benchmark.json",
        r#"{"metrics":[{"name":"from_the_tree_root","value":2,"unit":"count"}]}"#,
    );
    let summary = harness.extract();
    assert_eq!(names(&summary), vec!["from_the_bind_mount"]);
    assert_eq!(
        summary.source_path.as_ref().expect("cited").as_str(),
        format!(".evidence/incoming/{RUN_ID}/benchmark.jsonl")
    );
    assert_eq!(
        summary
            .found
            .iter()
            .map(|file| (file.path.as_str(), file.used))
            .collect::<Vec<_>>(),
        vec![
            (
                format!(".evidence/incoming/{RUN_ID}/benchmark.jsonl").as_str(),
                true
            ),
            ("benchmark.json", false),
        ],
        "the file that won is the first one listed"
    );
}

/// With nothing in `$ACTMCP_EVIDENCE_OUT`, the tree-root fallback is what a job that wrote
/// to a conventional path produced, and it is still cited by name.
#[test]
fn a_tree_root_payload_is_used_when_the_incoming_dir_is_empty() {
    let harness = Harness::new(RUN_ID);
    harness.at_tree_root(
        "benchmark.jsonl",
        "{\"name\":\"from_the_tree_root\",\"value\":7,\"unit\":\"count\"}\n",
    );
    let summary = harness.extract();
    assert_eq!(names(&summary), vec!["from_the_tree_root"]);
    assert_eq!(
        summary.source_path.as_ref().expect("cited").as_str(),
        "benchmark.jsonl",
        "the `.jsonl` form is chosen by filename, as §3.3 pins"
    );
    assert_eq!(summary.found[0].format, "jsonl");
}

/// A payload over the size cap is refused, counted, and changes nothing about the run.
#[test]
fn an_oversized_payload_is_reported_and_the_run_keeps_its_status() {
    let harness = Harness::new(RUN_ID);
    // Sparse: the cap is a stat check before the read, and this proves it is.
    let path = harness.incoming.join("benchmark.json");
    std::fs::File::create(&path)
        .expect("create")
        .set_len(8 * 1024 * 1024 + 1)
        .expect("sparse extend");
    harness.log_text(&step_line(
        "Run benchmark",
        1_000_000_000,
        "2026-11-10T00:42:20Z",
    ));
    let summary = harness.extract();

    assert!(summary
        .warnings
        .contains(&WARNING_PAYLOAD_TOO_LARGE.to_owned()));
    assert_eq!(summary.outcome, ExtractionOutcome::ParseFailed);
    assert!(summary.warnings.contains(&WARNING_PARSE_FAILED.to_owned()));
    assert_eq!(
        names(&summary),
        vec!["job_duration_s", "step_duration_s"],
        "the refusal costs the run its payload, not act's timings"
    );
    assert!(summary
        .detail
        .as_deref()
        .is_some_and(|detail| detail.contains("over the 8 MiB payload cap")));
}

/// A payload that cannot be opened at all is the same shape of answer.
#[test]
fn an_unreadable_payload_is_reported_not_fatal() {
    let harness = Harness::new(RUN_ID);
    let path = harness.incoming.join("benchmark.json");
    std::fs::write(&path, r#"{"metrics":[{"name":"x","value":1}]}"#).expect("write");
    std::fs::set_permissions(&path, Permissions::from_mode(0o000)).expect("chmod 000");
    let summary = harness.extract();
    assert!(summary
        .warnings
        .contains(&WARNING_PAYLOAD_UNREADABLE.to_owned()));
    assert_eq!(summary.outcome, ExtractionOutcome::ParseFailed);
    assert!(
        summary.is_empty(),
        "with no timings in the log either, nothing to write"
    );
    assert!(summary.warnings.contains(&WARNING_PARSE_FAILED.to_owned()));
}

// ── act's own timings ────────────────────────────────────────────────────────

/// §5.3: a benchmark job that emits nothing at all still leaves at least one row, so "did
/// it run, and how long did it take" is answerable from the same file.
#[test]
fn a_job_that_emits_nothing_still_leaves_its_durations() {
    let harness = Harness::new(RUN_ID);
    harness.log_text(&format!(
        "{}\n{}\n",
        r#"{"dryrun":false,"job":"hyperfine-bench","jobID":"hyperfine-bench","level":"info","msg":"Start image=ubuntu","time":"2026-11-10T00:42:04Z"}"#,
        own_step("Run benchmark", 88_234_000_000, "2026-11-10T00:43:33Z"),
    ));
    let summary = harness.extract();

    assert_eq!(names(&summary), vec!["job_duration_s", "step_duration_s"]);
    for record in &summary.records {
        assert_eq!(record.metric.source, BenchmarkSource::ActLog);
        assert_eq!(record.metric.unit.as_deref(), Some("s"));
        assert_eq!(
            record.metric.source_path, None,
            "the log is not a payload file"
        );
    }
    assert_eq!(find(&summary, "job_duration_s", None)[0]["value"], 88.234);
    assert_eq!(summary.outcome, ExtractionOutcome::Derived);
    assert_eq!(summary.warnings, Vec::<String>::new());
}

/// The fallback the derivation needs: act killed a run mid-step, so no step ever logged a
/// completion, and the only timing in the log is the span of its own lines.
#[test]
fn a_log_without_step_timings_falls_back_to_the_span_of_its_lines() {
    let harness = Harness::new(RUN_ID);
    harness.log_text(&format!(
        "{}\n{}\n{}\n",
        r#"{"job":"hyperfine-bench","jobID":"hyperfine-bench","level":"info","msg":"Start","time":"2026-11-10T00:42:04Z"}"#,
        r#"{"job":"hyperfine-bench","jobID":"hyperfine-bench","level":"info","msg":"Run benchmark","step":"Run benchmark","time":"2026-11-10T00:42:05Z"}"#,
        r#"{"job":"hyperfine-bench","jobID":"hyperfine-bench","level":"info","msg":"killed","time":"2026-11-10T00:43:35Z"}"#,
    ));
    let summary = harness.extract();
    assert_eq!(
        names(&summary),
        vec!["job_duration_s"],
        "no step reported a time, so only the job row exists"
    );
    assert_eq!(
        find(&summary, "job_duration_s", None)[0]["value"],
        91.0,
        "act's timestamps are whole seconds, and the fallback says exactly that"
    );
}

/// The `0.2.62 … 0.2.75` case, on act's real log shape for those tags: every step reports that
/// it finished and none of them is timed, because `executionTime` arrived in v0.2.76 and those
/// versions print no duration anywhere on the line. The timings are then **unavailable** rather
/// than absent, and the run has to say so — a successful benchmark run publishing no act timings
/// and no reason is the bug this test exists for (docs/benchmark-inputs.md §3).
#[test]
fn an_act_too_old_to_time_its_steps_says_so_instead_of_going_quiet() {
    let harness = Harness::new(RUN_ID);
    harness.use_log("act-pre-0.2.76.jsonl");
    let summary = harness.extract();

    assert!(
        find(&summary, "step_duration_s", None).is_empty(),
        "there is nothing in this log to build a step duration from: {:?}",
        names(&summary)
    );
    let job = find(&summary, "job_duration_s", None);
    assert_eq!(job.len(), 1, "the job still gets one duration row");
    assert_eq!(
        job[0]["value"], 92.0,
        "and it is the whole-second span of the log's own lines, which is coarser than a sum of \
         steps and is what the warning says"
    );
    assert!(
        names(&summary).contains(&"cold_start_ms".to_owned()),
        "the in-band marker channel is alive in these versions too, so the run still publishes: \
         {:?}",
        names(&summary)
    );
    assert_eq!(summary.outcome, ExtractionOutcome::Derived);
    assert!(
        summary
            .warnings
            .iter()
            .any(|warning| warning == WARNING_STEP_TIMINGS_UNAVAILABLE),
        "the missing timings are named, not quiet: {:?}",
        summary.warnings
    );
    let detail = summary.detail.expect("a note about the timings");
    assert!(
        detail.contains("v0.2.76"),
        "and the note names the version that added the field: {detail}"
    );
}

/// act runs a job's dependencies through the same log stream. None of their time belongs to
/// this run's `job_duration_s`.
#[test]
fn another_jobs_step_is_not_this_runs_duration() {
    let harness = Harness::new(RUN_ID);
    harness.log_text(&format!(
        "{}\n{}\n",
        other_job_step("setup", "Install", 600_000_000_000, "2026-11-10T00:52:00Z"),
        own_step("Run benchmark", 2_500_000_000, "2026-11-10T00:52:03Z"),
    ));
    let summary = harness.extract();
    assert_eq!(names(&summary), vec!["job_duration_s", "step_duration_s"]);
    assert_eq!(find(&summary, "job_duration_s", None)[0]["value"], 2.5);
    assert_eq!(find(&summary, "step_duration_s", None)[0]["value"], 2.5);
    assert_eq!(
        find(&summary, "step_duration_s", None)[0]["labels"]["step"],
        "Run benchmark"
    );
}

/// §5.3: a job that times its own phase beats act timing the whole job — keeping both would
/// count the same duration twice.
#[test]
fn a_job_reporting_its_own_duration_suppresses_acts_derived_one() {
    let harness = Harness::new(RUN_ID);
    harness.payload_text(
        "benchmark.jsonl",
        concat!(
            "{\"name\":\"job_duration_s\",\"value\":80.0,\"unit\":\"s\"}\n",
            "{\"name\":\"step_duration_s\",\"value\":79.5,\"unit\":\"s\",\"labels\":{\"step\":\"Run benchmark\"}}\n",
        ),
    );
    harness.log_text(&step_line(
        "Run benchmark",
        88_000_000_000,
        "2026-11-10T00:43:33Z",
    ));
    let summary = harness.extract();

    let job = find(&summary, "job_duration_s", None);
    assert_eq!(job.len(), 1, "one job duration, not two");
    assert_eq!(job[0]["value"], 80.0, "the job's own measurement wins");
    assert_eq!(job[0]["source"], "job_file");

    let steps = find(&summary, "step_duration_s", Some("Run benchmark"));
    assert_eq!(steps.len(), 1, "and the same for a step it timed itself");
    assert_eq!(steps[0]["value"], 79.5);
    assert_eq!(
        summary.from,
        SourceCounts {
            job_file: 2,
            in_band_stdout: 0,
            act_log: 0,
        }
    );
}

/// A duration act reported for a step it never named still counts in the job's total, and
/// gets no line of its own: a label that says nothing groups nothing.
#[test]
fn an_unnamed_step_counts_in_the_total_and_gets_no_record() {
    let harness = Harness::new(RUN_ID);
    harness.log_text(&format!(
        "{}\n{}\n",
        own_step("Run benchmark", 80_000_000_000, "2026-11-10T00:43:30Z"),
        r#"{"dryrun":false,"executionTime":11000000000,"job":"hyperfine-bench","jobID":"hyperfine-bench","level":"info","msg":"  Success","time":"2026-11-10T00:43:31Z"}"#,
    ));
    let summary = harness.extract();
    assert_eq!(names(&summary), vec!["job_duration_s", "step_duration_s"]);
    assert_eq!(find(&summary, "job_duration_s", None)[0]["value"], 91.0);
}

// ── nothing at all ───────────────────────────────────────────────────────────

/// A run whose job produced no payload, no markers, and whose log carries no timings has
/// nothing to publish — which `meta.json` records, rather than an empty file (§1.3).
#[test]
fn nothing_at_all_produces_no_records_and_says_so() {
    let harness = Harness::new(RUN_ID);
    let summary = harness.extract();
    assert!(summary.is_empty());
    assert_eq!(summary.outcome, ExtractionOutcome::NoMetrics);
    assert_eq!(
        summary.warnings,
        vec![WARNING_NO_BENCHMARK_METRICS.to_owned()],
        "the warning vocabulary is the whole report"
    );
    assert_eq!(
        summary.warning_codes(),
        summary.warnings,
        "and it is deduplicated"
    );
    assert!(
        summary.run_records().is_none(),
        "no batch, so the writer publishes no file"
    );
}

/// A log that cannot be read is a warning too; extraction never fails a run.
#[test]
fn an_unreadable_act_log_is_a_warning_and_not_a_failure() {
    let harness = Harness::new(RUN_ID);
    harness.payload_text(
        "benchmark.json",
        r#"{"metrics":[{"name":"x","value":1,"unit":"count"}]}"#,
    );
    std::fs::remove_file(&harness.outcome.log_path).expect("remove the log");
    let summary = harness.extract();
    assert_eq!(names(&summary), vec!["x"], "the payload still counts");
    assert_eq!(
        summary.outcome,
        ExtractionOutcome::Extracted,
        "and the run is still a run whose payload was read"
    );
    assert!(summary
        .warnings
        .iter()
        .any(|warning| warning == "act_log_unreadable"));
    assert!(
        summary.detail.is_some(),
        "and it says which file it could not read"
    );
}
