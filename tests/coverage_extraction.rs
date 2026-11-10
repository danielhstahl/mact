//! Coverage extraction, measured against the pinned contract fixtures (mact-f1w.10).
//!
//! The entry point under test is
//! [`act_mcp::evidence::coverage::extract`]: a finished [`RunOutcome`] plus the
//! [`ValidatedRunTarget`] it was validated against go in, coverage records come out. The
//! assertions here are byte-for-byte against
//! [`tests/e2e/testdata/expected`](../e2e/testdata/README.md), because those files are the
//! contract's executable half — ADR-0002's verification table names exactly these goldens as
//! this ticket's check.
//!
//! Each test builds a real tree in a temp dir (the layout `--bind` gives the job:
//! `.evidence/incoming/<run_id>/` plus the tree's root), copies a committed fixture into it
//! under the contract's filename, and extracts. Nothing here mocks the filesystem: the
//! precedence rules and the path normalisation are only proven against real files.
//!
//! The envelope's `host` and `tool.act_version` are pinned through
//! [`ExtractContext`] to the values the fixtures use (`build-box-07`, `0.2.68`) — the
//! "volatile fields are normalised as an explicit, documented step" rule of the testdata
//! README, applied to extraction instead of to the harness.

use std::{
    fs::Permissions,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use act_mcp::{
    evidence::{
        coverage::{
            CoverageFormat, ExtractContext, ExtractError, ExtractionOutcome,
            WARNING_COVERAGE_SOURCE_SHADOWED, WARNING_PARSE_FAILED, WARNING_PATHS_DROPPED,
            WARNING_PAYLOAD_UNREADABLE,
        },
        schema::validate_record,
        CoverageRecord, CoverageSource, RunRecords, Scope,
    },
    runner::{ActRunOutcome, RunStatus},
    workspace::{IsolationMode, ValidatedRunTarget},
};
use chrono::{DateTime, Utc};

/// The commit every contract fixture is pinned to (testdata README).
const SHA: &str = "2f7c1a4b9e0d3c6a5b8f1e2d4c6a8b0f3e5d7c91";
/// The hostname the fixtures pin.
const HOST: &str = "build-box-07";
/// The `tool.act_version` the fixtures pin.
const ACT_VERSION: &str = "0.2.68";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/coverage")
}

fn expected(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/e2e/testdata/expected")
        .join(format!("{name}.jsonl"))
}

fn read(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_else(|reason| panic!("read {}: {reason}", path.display()))
}

fn text(path: &Path) -> String {
    String::from_utf8(read(path)).unwrap_or_else(|reason| panic!("{}: {reason}", path.display()))
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
    /// A tree whose run ended the way the pinned coverage fixtures say it did.
    fn new(run_id: &str, task: &str, started_at: &str, duration_ms: u64) -> Self {
        Self::with_status(
            run_id,
            task,
            started_at,
            duration_ms,
            RunStatus::Success,
            Some(0),
        )
    }

    fn with_status(
        run_id: &str,
        task: &str,
        started_at: &str,
        duration_ms: u64,
        status: RunStatus,
        exit_code: Option<u32>,
    ) -> Self {
        let dir = tempfile::tempdir().expect("tempdir for the run tree");
        let tree = dir.path().canonicalize().expect("canonical tempdir");
        let incoming = tree.join(".evidence/incoming").join(run_id);
        let runs = tree.join(".evidence/runs").join(run_id);
        std::fs::create_dir_all(&incoming).expect("create the incoming dir");
        std::fs::create_dir_all(&runs).expect("create the runs dir");
        std::fs::create_dir_all(tree.join(".github/workflows")).expect("create workflows");
        // A log exists even when a test does not scrape it: a run always has one.
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
            workflow_file: tree.join(".github/workflows/coverage.yml"),
            workflow_display: "coverage".to_owned(),
            workflow_relpath: ".github/workflows/coverage.yml".to_owned(),
            job_id: task.to_owned(),
            commit_full_sha: SHA.to_owned(),
            category: "coverage".to_owned(),
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

    /// Copy a fixture into the run's `$ACTMCP_EVIDENCE_OUT`, renaming it to the contract's
    /// filename. `@TREE@` in the fixture expands to this tree's path, which is how one
    /// committed file can hold absolute container paths.
    fn payload(&self, fixture: &str, as_name: &str) {
        self.place(fixture, &self.incoming.join(as_name));
    }

    /// Copy a fixture at the tree's root, the fallback places §3.2 allows.
    fn at_tree_root(&self, fixture: &str, rel: &str) {
        self.place(fixture, &self.tree.join(rel));
    }

    fn place(&self, fixture: &str, destination: &Path) {
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).expect("create the destination's directory");
        }
        let body = text(&fixtures().join(fixture)).replace("@TREE@", &self.tree.to_string_lossy());
        std::fs::write(destination, body)
            .unwrap_or_else(|reason| panic!("write {}: {reason}", destination.display()));
    }

    /// Point the run's captured log at a fixture.
    fn use_log(&self, fixture: &str) {
        std::fs::write(&self.outcome.log_path, read(&fixtures().join(fixture)))
            .expect("write the log fixture");
    }

    /// `extract`, with the machine-specific envelope fields pinned.
    fn extract(&self) -> Result<act_mcp::evidence::CoverageSummary, ExtractError> {
        act_mcp::evidence::coverage::extract_with(&self.outcome, &self.target, &self.context())
    }
}

/// The records as the file that would be published, then compared to a golden byte for byte.
fn assert_golden(summary: &act_mcp::evidence::CoverageSummary, golden: &str) {
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

/// Every string in every record, for the "no host paths anywhere" sweep.
fn every_string(value: &serde_json::Value, found: &mut Vec<String>) {
    match value {
        serde_json::Value::String(text) => found.push(text.clone()),
        serde_json::Value::Array(items) => {
            for item in items {
                every_string(item, found);
            }
        }
        serde_json::Value::Object(entries) => {
            for (key, item) in entries {
                found.push(key.clone());
                every_string(item, found);
            }
        }
        _ => {}
    }
}

/// Nothing in a record may name a directory: no absolute path, and not this tree's own.
fn assert_no_host_paths(records: &[CoverageRecord], tree: &Path) {
    let mut strings = Vec::new();
    for record in records {
        let value = serde_json::to_value(record).expect("a record serializes");
        every_string(&value, &mut strings);
    }
    for string in strings {
        assert!(
            !string.starts_with('/') && !string.contains("~") && !string.contains('\\'),
            "an absolute or host-shaped path reached a record: {string}"
        );
        assert!(
            !string.contains(&tree.to_string_lossy().to_string()),
            "the workspace's own absolute path reached a record: {string}"
        );
    }
}

fn counts(record: &CoverageRecord) -> (Option<u64>, Option<u64>, Option<u64>, Option<u64>) {
    (
        record.counts.lines_found,
        record.counts.lines_hit,
        record.counts.branches_found,
        record.counts.branches_hit,
    )
}

// ── lcov ───────────────────────────────────────────────────────────────────────

/// The lcov fixture, and the four variants the acceptance criteria name, must all produce
/// `expected/coverage.jsonl` exactly: one `scope:"total"` line, then one line per file in
/// the tracefile's own order, raw numerators throughout.
#[test]
fn every_lcov_variant_reproduces_the_pinned_records() {
    // (fixture label, how to put it in place)
    let harness = Harness::new(
        "20261110T011507Z-d4e5f6",
        "coverage",
        "2026-11-10T01:15:07.004Z",
        61_233,
    );
    // Each variant is (what it does to the tracefile, how to put it in place).
    type Variant = (&'static str, Box<dyn Fn(&Harness)>);
    let variants: [Variant; 5] = [
        (
            "the tracefile as gcov wrote it",
            Box::new(|h: &Harness| h.payload("lcov.info", "lcov.info")),
        ),
        (
            "CRLF line endings",
            Box::new(|h: &Harness| {
                let body = text(&fixtures().join("lcov.info")).replace('\n', "\r\n");
                std::fs::write(h.incoming.join("lcov.info"), body).expect("write CRLF");
            }),
        ),
        (
            "no final newline",
            Box::new(|h: &Harness| {
                let body = text(&fixtures().join("lcov.info"))
                    .trim_end_matches('\n')
                    .to_owned();
                std::fs::write(h.incoming.join("lcov.info"), body).expect("write torn");
            }),
        ),
        (
            "absolute SF paths",
            Box::new(|h: &Harness| h.payload("lcov-absolute-sf.info", "lcov.info")),
        ),
        (
            "repeated SF blocks, summed per path",
            Box::new(|h: &Harness| h.payload("lcov-repeated-sf.info", "lcov.info")),
        ),
    ];

    for (label, place) in variants {
        for stale in std::fs::read_dir(&harness.incoming).expect("read incoming") {
            std::fs::remove_file(stale.expect("entry").path()).ok();
        }
        place(&harness);
        let summary = harness.extract().expect("extraction itself succeeds");
        assert_eq!(summary.outcome, ExtractionOutcome::Extracted, "{label}");
        assert_eq!(summary.len(), 5, "{label}: one total and four files");
        assert_eq!(summary.source, Some(CoverageSource::Lcov), "{label}");
        assert_eq!(
            summary
                .source_path
                .as_ref()
                .expect("a file-derived record names its file")
                .as_str(),
            ".evidence/incoming/20261110T011507Z-d4e5f6/lcov.info",
            "{label}: the payload path is cited workspace-relative"
        );
        assert_golden(&summary, "coverage");
        assert_no_host_paths(&summary.records, &harness.tree);
    }
}

#[test]
fn the_total_is_the_first_line_and_carries_the_sum() {
    let harness = Harness::new(
        "20261110T011507Z-d4e5f6",
        "coverage",
        "2026-11-10T01:15:07.004Z",
        61_233,
    );
    harness.payload("lcov.info", "lcov.info");
    let summary = harness.extract().expect("extract");
    let first = summary.records.first().expect("records");
    assert_eq!(first.counts.scope, Scope::Total);
    assert_eq!(first.counts.name.as_deref(), Some("total"));
    assert_eq!(first.counts.path, None, "a total names no file");
    assert_eq!(
        counts(first),
        (Some(4120), Some(3581), Some(1180), Some(946))
    );

    // And the sum of the file records really is that total — ADR-0002 §4.3 lets a
    // consumer use either, so they may not disagree.
    let files = &summary.records[1..];
    assert_eq!(files.len(), 4);
    assert_eq!(
        files
            .iter()
            .filter_map(|record| record.counts.lines_found)
            .sum::<u64>(),
        4120
    );
    assert_eq!(
        files
            .iter()
            .filter_map(|record| record.counts.lines_hit)
            .sum::<u64>(),
        3581
    );
    assert_eq!(
        files
            .iter()
            .filter_map(|record| record.counts.branches_found)
            .sum::<u64>(),
        1180
    );
}

#[test]
fn file_records_carry_raw_counts_never_a_percentage_and_paths_relative_to_the_tree() {
    let harness = Harness::new(
        "20261110T011507Z-d4e5f6",
        "coverage",
        "2026-11-10T01:15:07.004Z",
        61_233,
    );
    harness.payload("lcov.info", "lcov.info");
    let summary = harness.extract().expect("extract");
    for record in &summary.records {
        assert!(
            record.counts.reported_percent.is_none(),
            "a parsed file's record may not carry a percentage: {:?}",
            record.counts.path
        );
        if record.counts.scope == Scope::File {
            let path = record
                .counts
                .path
                .as_ref()
                .expect("a file record names a path");
            assert!(path.as_str().starts_with("src/"), "{}", path);
        }
    }
    // The zero-coverage file is a record with zeros, not a missing record, and the
    // generated file keeps its missing branch pair instead of inventing one.
    let zero = summary
        .records
        .iter()
        .find(|record| {
            record
                .counts
                .path
                .as_ref()
                .is_some_and(|p| p.as_str().contains("debug_helpers"))
        })
        .expect("the zero-coverage file is still a record");
    assert_eq!(counts(zero), (Some(88), Some(0), Some(24), Some(0)));
    let branchless = summary
        .records
        .iter()
        .find(|record| {
            record
                .counts
                .path
                .as_ref()
                .is_some_and(|p| p.as_str().contains("protobuf_out"))
        })
        .expect("the branchless file is still a record");
    assert_eq!(branchless.counts.branches_found, None);
    assert_eq!(branchless.counts.branches_hit, None);
}

#[test]
fn a_container_path_outside_the_tree_is_rebased_when_the_file_actually_exists_there() {
    // The ADR-0003 §15.1 shape: the tool recorded a path from a different mount. Rebasing
    // is allowed only where the rebased file exists, so the tree has to contain it.
    let harness = Harness::new(
        "20261110T011507Z-d4e5f6",
        "coverage",
        "2026-11-10T01:15:07.004Z",
        61_233,
    );
    std::fs::create_dir_all(harness.tree.join("src/parser")).expect("src/parser");
    std::fs::write(
        harness.tree.join("src/parser/tokenizer.rs"),
        "fn main() {}\n",
    )
    .expect("write");
    std::fs::write(
        harness.incoming.join("lcov.info"),
        "SF:/srv/work/repo/src/parser/tokenizer.rs\nLF:10\nLH:9\nend_of_record\n",
    )
    .expect("write payload");
    let summary = harness.extract().expect("extract");
    assert_eq!(
        summary
            .records
            .iter()
            .find(|record| record.counts.scope == Scope::File)
            .expect("a file record")
            .counts
            .path
            .as_ref()
            .expect("path")
            .as_str(),
        "src/parser/tokenizer.rs"
    );
}

#[test]
fn a_source_file_outside_the_tree_is_counted_in_the_total_and_gets_no_record() {
    // No `relpath` can name `/usr/lib/...`, so no record may claim it; its counts still
    // belong to what the run measured.
    let harness = Harness::new(
        "20261110T011507Z-d4e5f6",
        "coverage",
        "2026-11-10T01:15:07.004Z",
        61_233,
    );
    std::fs::write(
            harness.incoming.join("lcov.info"),
            "SF:/usr/lib/python3.11/json/decoder.py\nLF:400\nLH:200\nend_of_record\nSF:src/a.py\nLF:10\nLH:5\nend_of_record\n",
        )
        .expect("write payload");
    let summary = harness.extract().expect("extract");
    assert_eq!(summary.dropped_paths, 1);
    assert!(
        summary
            .warnings
            .iter()
            .any(|warning| warning == WARNING_PATHS_DROPPED),
        "{:?}",
        summary.warnings
    );
    assert_eq!(summary.len(), 2, "one total and one file");
    assert_eq!(
        counts(summary.records.first().expect("total")),
        (Some(410), Some(205), None, None),
        "the unnameable file is in the total"
    );
    assert_no_host_paths(&summary.records, &harness.tree);
}

// ── istanbul ───────────────────────────────────────────────────────────────────

#[test]
fn an_istanbul_summary_reproduces_the_pinned_records_and_keeps_statements_separate() {
    let harness = Harness::new(
        "20261110T021244Z-0f9e8d",
        "coverage-istanbul",
        "2026-11-10T02:12:44.551Z",
        40_017,
    );
    harness.payload("coverage-summary.json", "coverage-summary.json");
    let summary = harness.extract().expect("extract");
    assert_eq!(summary.outcome, ExtractionOutcome::Extracted);
    assert_eq!(summary.source, Some(CoverageSource::CoverageSummary));
    assert_golden(&summary, "coverage-istanbul");
    assert_no_host_paths(&summary.records, &harness.tree);

    // Statements and lines are different units, and this fixture makes them differ so a
    // conflation cannot pass: the total's two pairs are not the same numbers.
    let total = summary.records.first().expect("total").counts.clone();
    assert_ne!(total.lines_found, total.statements_found);
    assert_ne!(total.lines_hit, total.statements_hit);
    let index = summary
        .records
        .iter()
        .find(|record| {
            record
                .counts
                .path
                .as_ref()
                .is_some_and(|p| p.as_str() == "lib/index.js")
        })
        .expect("lib/index.js");
    assert_eq!(
        (
            index.counts.lines_found,
            index.counts.lines_hit,
            index.counts.statements_found,
            index.counts.statements_hit
        ),
        (Some(640), Some(601), Some(700), Some(651)),
        "istanbul's `lines` went to `lines_*` and its `statements` to `statements_*`"
    );
}

#[test]
fn istanbul_entries_that_live_outside_the_tree_are_dropped_and_said_so() {
    let harness = Harness::new(
        "20261110T021244Z-0f9e8d",
        "coverage-istanbul",
        "2026-11-10T02:12:44.551Z",
        40_017,
    );
    harness.payload("coverage-summary.json", "coverage-summary.json");
    let summary = harness.extract().expect("extract");
    assert_eq!(summary.dropped_paths, 2, "the two node_modules entries");
    assert!(summary
        .warnings
        .iter()
        .any(|warning| warning == WARNING_PATHS_DROPPED));
    assert_eq!(summary.len(), 3, "one total and the two workspace files");
    // istanbul's own total is over every instrumented file, so it is larger than the sum
    // of the records we could name — and that is stated, not hidden.
    let total = summary.records.first().expect("total").counts.lines_found;
    let sum: u64 = summary.records[1..]
        .iter()
        .filter_map(|record| record.counts.lines_found)
        .sum();
    assert_eq!(total, Some(2210));
    assert_eq!(sum, 677);
}

// ── the log-scrape fallback ────────────────────────────────────────────────────

#[test]
fn the_log_scrape_is_a_last_resort_and_announces_itself() {
    let harness = Harness::new(
        "20261110T030900Z-77aa55",
        "coverage-ad-hoc",
        "2026-11-10T03:09:00.000Z",
        22_741,
    );
    harness.use_log("act-log-scrape.jsonl");
    let summary = harness.extract().expect("extract");
    assert_eq!(summary.outcome, ExtractionOutcome::Scraped);
    assert_eq!(summary.source, Some(CoverageSource::LogScrape));
    assert_eq!(
        summary.source_path, None,
        "a scraped record has no payload file"
    );
    assert_golden(&summary, "coverage-log-scrape");

    let record = summary.records.first().expect("one total record");
    assert!(
        record
            .counts
            .flags
            .as_ref()
            .is_some_and(|flags| flags.contains(&"percent_only".to_owned())
                && flags.contains(&"low_confidence".to_owned())),
        "a scraped percentage must be filterable out: {:?}",
        record.counts.flags
    );
    assert_eq!(record.counts.lines_found, None, "no invented denominator");
    assert_no_host_paths(&summary.records, &harness.tree);
}

#[test]
fn a_payload_file_always_beats_the_log() {
    // The same log as the scrape test, plus a real tracefile: the parsed numbers win and
    // no percentage survives anywhere.
    let harness = Harness::new(
        "20261110T030900Z-77aa55",
        "coverage-ad-hoc",
        "2026-11-10T03:09:00.000Z",
        22_741,
    );
    harness.use_log("act-log-scrape.jsonl");
    harness.payload("lcov.info", "lcov.info");
    let summary = harness.extract().expect("extract");
    assert_eq!(summary.outcome, ExtractionOutcome::Extracted);
    assert_eq!(summary.source, Some(CoverageSource::Lcov));
    assert_eq!(summary.len(), 5);
    assert!(
        summary
            .records
            .iter()
            .all(|record| record.counts.reported_percent.is_none()),
        "log scraping may not overwrite a parsed file's numbers"
    );
}

#[test]
fn a_percentage_only_table_row_reports_an_unknown_measure() {
    let harness = Harness::new(
        "20261110T030900Z-77aa55",
        "coverage-ad-hoc",
        "2026-11-10T03:09:00.000Z",
        22_741,
    );
    std::fs::write(
        &harness.outcome.log_path,
        "{\"Action\":\"output\",\"Output\":\"All files | 87.4 | 100.0 | 92.1\\n\"}\n",
    )
    .expect("write log");
    let summary = harness.extract().expect("extract");
    let record = summary.records.first().expect("one record");
    assert_eq!(
        record.counts.reported_measure,
        Some(act_mcp::evidence::ReportedMeasure::Unknown),
        "an unlabelled table column is not a guess about lines"
    );
}

// ── what is *not* an error ─────────────────────────────────────────────────────

#[test]
fn a_missing_payload_is_no_coverage_found_and_changes_nothing_about_the_run() {
    let harness = Harness::new(
        "20261110T050000Z-aaaaaa",
        "coverage",
        "2026-11-10T05:00:00.000Z",
        1_000,
    );
    harness.use_log("act-log-quiet.jsonl");
    let summary = harness.extract().expect("extract");
    assert_eq!(summary.outcome, ExtractionOutcome::NoCoverageFound);
    assert!(summary.is_empty());
    assert!(
        summary.run_records().is_none(),
        "no records means no file to publish"
    );
    assert!(summary
        .warning_codes()
        .contains(&"no_coverage_found".to_owned()));
    // The run itself is untouched: extraction only reports.
    assert_eq!(harness.outcome.status, RunStatus::Success);
}

#[test]
fn an_empty_payload_is_no_coverage_found() {
    let harness = Harness::new(
        "20261110T050000Z-aaaaaa",
        "coverage",
        "2026-11-10T05:00:00.000Z",
        1_000,
    );
    harness.use_log("act-log-quiet.jsonl");
    harness.payload("lcov-empty.info", "lcov.info");
    let summary = harness.extract().expect("extract");
    assert_eq!(summary.outcome, ExtractionOutcome::NoCoverageFound);
    assert!(summary
        .detail
        .as_deref()
        .is_some_and(|d| d.contains("empty")));
}

#[test]
fn a_malformed_payload_is_parse_failed_and_a_failed_run_stays_failed() {
    let harness = Harness::with_status(
        "20261110T050100Z-bbbbbb",
        "coverage",
        "2026-11-10T05:00:00.000Z",
        1_000,
        RunStatus::Failed,
        Some(1),
    );
    harness.use_log("act-log-quiet.jsonl");
    harness.payload("lcov-malformed.info", "lcov.info");
    let summary = harness
        .extract()
        .expect("a broken payload is not a broken extraction");
    assert_eq!(summary.outcome, ExtractionOutcome::ParseFailed);
    assert!(summary.is_empty());
    // The outcome's own code is the warning: no second code was invented for it.
    assert_eq!(
        summary.warning_codes(),
        vec![WARNING_PARSE_FAILED.to_owned()]
    );
    assert!(
        summary.malformed > 0,
        "and it counted what it could not read"
    );
    // The run's own status is whatever act reported, unchanged: a red run still gets its
    // evidence written, with `run_status: "failed"`.
    assert_eq!(harness.outcome.status, RunStatus::Failed);
}

#[test]
fn a_malformed_summary_is_parse_failed() {
    let harness = Harness::new(
        "20261110T050100Z-bbbbbb",
        "coverage",
        "2026-11-10T05:00:00.000Z",
        1_000,
    );
    harness.payload("coverage-summary-malformed.json", "coverage-summary.json");
    let summary = harness.extract().expect("extract");
    assert_eq!(summary.outcome, ExtractionOutcome::ParseFailed);
}

#[test]
fn an_unreadable_payload_is_reported_not_raised() {
    let harness = Harness::new(
        "20261110T050200Z-cccccc",
        "coverage",
        "2026-11-10T05:00:00.000Z",
        1_000,
    );
    harness.payload("lcov.info", "lcov.info");
    let payload = harness.incoming.join("lcov.info");
    std::fs::set_permissions(&payload, Permissions::from_mode(0o000))
        .expect("chmod the payload unreadable");
    let summary = harness.extract().expect("extract");
    assert_eq!(summary.outcome, ExtractionOutcome::ParseFailed);
    assert!(
        summary
            .warnings
            .iter()
            .any(|warning| warning == WARNING_PAYLOAD_UNREADABLE),
        "{:?}",
        summary.warnings
    );
    std::fs::set_permissions(&payload, Permissions::from_mode(0o644)).ok();
}

#[test]
fn a_failed_run_writes_records_with_run_status_failed() {
    let harness = Harness::with_status(
        "20261110T011507Z-d4e5f6",
        "coverage",
        "2026-11-10T01:15:07.004Z",
        61_233,
        RunStatus::Failed,
        Some(1),
    );
    harness.payload("lcov.info", "lcov.info");
    let summary = harness.extract().expect("extract");
    assert_eq!(summary.len(), 5, "a red run is still a run with coverage");
    let line = summary.records.first().expect("total");
    assert_eq!(line.envelope.run_status.as_str(), "failed");
    assert_eq!(line.envelope.exit_code, Some(1));
}

// ── discovery ──────────────────────────────────────────────────────────────────

#[test]
fn the_payload_directory_wins_and_everything_found_is_listed() {
    let harness = Harness::new(
        "20261110T011507Z-d4e5f6",
        "coverage",
        "2026-11-10T01:15:07.004Z",
        61_233,
    );
    // An istanbul summary in `$ACTMCP_EVIDENCE_OUT` and a stray lcov at the tree root. The
    // place we told the job to write wins (§3.2), so the payload's summary beats the tree's
    // tracefile — a stray file from an earlier build cannot shadow this run's payload — and
    // both are listed, with the loser marked as shadowed.
    harness.payload("coverage-summary.json", "coverage-summary.json");
    harness.at_tree_root("lcov.info", "lcov.info");
    let summary = harness.extract().expect("extract");
    assert_eq!(summary.source, Some(CoverageSource::CoverageSummary));
    assert!(summary
        .warnings
        .iter()
        .any(|warning| warning == WARNING_COVERAGE_SOURCE_SHADOWED));
    let used: Vec<&str> = summary
        .found
        .iter()
        .filter(|file| file.used)
        .map(|file| file.path.as_str())
        .collect();
    assert_eq!(
        used,
        vec![".evidence/incoming/20261110T011507Z-d4e5f6/coverage-summary.json"]
    );
    assert_eq!(summary.found.len(), 2, "both files are listed");
    assert!(
        summary
            .found
            .iter()
            .any(|file| file.path == "lcov.info" && !file.used),
        "the shadowed file is named so a human can tell the tools apart"
    );
}

#[test]
fn the_tree_fallbacks_are_searched_in_order() {
    let harness = Harness::new(
        "20261110T011507Z-d4e5f6",
        "coverage",
        "2026-11-10T01:15:07.004Z",
        61_233,
    );
    // Only `coverage/lcov.info` exists: the conventional place a project's own tooling
    // writes, which the contract accepts (§3.2) even though it is not where we told the job.
    harness.at_tree_root("lcov.info", "coverage/lcov.info");
    let summary = harness.extract().expect("extract");
    assert_eq!(summary.len(), 5);
    assert_eq!(
        summary.source_path.as_ref().expect("source").as_str(),
        "coverage/lcov.info"
    );
}

#[test]
fn cobertura_is_refused_cleanly_instead_of_guessed() {
    let harness = Harness::new(
        "20261110T050300Z-dddddd",
        "coverage",
        "2026-11-10T05:00:00.000Z",
        1_000,
    );
    harness.payload("coverage.xml", "coverage.xml");
    let error = harness
        .extract()
        .expect_err("a Cobertura-only run is unsupported");
    match &error {
        ExtractError::UnsupportedFormat { path, format, .. } => {
            assert_eq!(
                path,
                ".evidence/incoming/20261110T050300Z-dddddd/coverage.xml"
            );
            assert_eq!(*format, "cobertura coverage.xml");
        }
        other => panic!("expected unsupported_format, got {other}"),
    }
    assert_eq!(error.warning_code(), "unsupported_format");
}

#[test]
fn a_cobertura_file_next_to_a_real_payload_is_a_warning_not_a_refusal() {
    let harness = Harness::new(
        "20261110T011507Z-d4e5f6",
        "coverage",
        "2026-11-10T01:15:07.004Z",
        61_233,
    );
    harness.payload("coverage.xml", "coverage.xml");
    harness.payload("lcov.info", "lcov.info");
    let summary = harness
        .extract()
        .expect("the parseable file still produced records");
    assert_eq!(summary.outcome, ExtractionOutcome::Extracted);
    assert!(summary
        .warnings
        .iter()
        .any(|warning| warning == "unsupported_format"));
    assert!(summary
        .found
        .iter()
        .any(|file| file.format == CoverageFormat::CoberturaXml));
    assert_eq!(summary.len(), 5);
}

#[test]
fn the_run_id_and_commit_of_the_envelope_come_from_the_run_not_from_the_payload() {
    // A payload cannot re-attribute a run (ADR-0002 §3.3): even a tracefile that mentions
    // another commit's paths lands under this run's identity.
    let harness = Harness::new(
        "20261110T011507Z-d4e5f6",
        "coverage",
        "2026-11-10T01:15:07.004Z",
        61_233,
    );
    harness.payload("lcov.info", "lcov.info");
    let summary = harness.extract().expect("extract");
    let run = RunRecords::Coverage {
        records: summary.records.clone(),
    };
    run.check().expect("one run, one identity");
    let envelope = run.envelope();
    assert_eq!(envelope.run_id.as_str(), "20261110T011507Z-d4e5f6");
    assert_eq!(envelope.commit.as_str(), SHA);
    assert_eq!(envelope.workflow, "coverage");
    assert_eq!(envelope.task, "coverage");
    assert_eq!(envelope.category, "coverage");
    assert_eq!(envelope.host, HOST);
    assert_eq!(
        envelope.tool.act_version.as_deref(),
        Some(ACT_VERSION),
        "act's version is what the caller knew, never what a file claimed"
    );
}
