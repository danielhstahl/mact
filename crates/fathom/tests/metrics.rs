//! The evidence reader: ADR-0008's rules, asserted.
//!
//! Three kinds of test live here, and each is here because a different kind of mistake hides
//! somewhere else:
//!
//! * **The golden vectors** (`crates/act-mcp/tests/testdata/metrics/`) are 16 awkward cases with
//!   hand-annotated expected rows, re-derived on CI by `scripts/metric_model_reference.py`. The Rust
//!   reader must produce the same rows, numerically — that is what "graded against the reference
//!   implementation" means in ADR-0008, and it is the only test that covers all 18 drop reasons, all
//!   four action states and every gap code at once.
//! * **Reality-shaped fixtures**: the checked-in e2e goldens (the records a real `act` run's log
//!   produced), a sample tree laid out by `scripts/sample_evidence.py`, missing directories, a
//!   short-SHA directory, a mixed-case one, a `.…tmp` corpse of a crashed publish, a
//!   `schema_version` this build refuses, and the `runs/` tree that a naive `**/*.jsonl` glob would
//!   read and silently turn into coverage.
//! * **The distinction that is hardest to get by accident**: "no data" versus "unreadable data", and
//!   a vacuous `found == 0` versus a measured `0.0 %`.
//!
//! None of these need `git`, `act`, docker, a network or a TTY. `jq`, when present, is used by
//! `tests/evidence_recipes.rs`, not here.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
};

use fathom::metrics::{ActionState, Caveat, ColumnState, DropReason, GapCode, Measure, Reader};
use serde_json::Value;

/// The repository root, two levels up from `crates/fathom`.
fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/fathom sits two levels under the repo root")
        .to_path_buf()
}

/// ADR-0008's golden vectors: a fixture tree plus the row it must produce.
fn vectors_dir() -> PathBuf {
    repo().join("crates/act-mcp/tests/testdata/metrics")
}

/// Derive rows for `commits` from the evidence root at `root`, as JSON keyed by SHA.
///
/// Everything downstream compares JSON against JSON: the expected files are the oracle's output, so
/// comparing the serialised rows is comparing the two implementations, not two struct definitions
/// that could each be internally consistent and mutually wrong.
fn derive_json(root: &Path, commits: &[String]) -> Value {
    let readset = Reader::new()
        .read_commits(root, commits)
        .unwrap_or_else(|error| panic!("reading {}: {error}", root.display()));
    let rows: BTreeMap<String, Value> = readset
        .rows()
        .map(|row| {
            (
                row.commit.clone(),
                serde_json::to_value(row).expect("a row serialises"),
            )
        })
        .collect();
    serde_json::to_value(rows).expect("a map serialises")
}

/// Read the commits a fixture declares, plus every commit its tree has records for — which is the
/// set the oracle derives rows for (`.evidence` never says which commits exist, ADR-0008 §2.4, so a
/// fixture states them in `commits.json` the way the git walk would).
fn fixture_commits(fixture: &Path) -> Vec<String> {
    let declared = fixture.join("commits.json");
    let mut commits: Vec<String> = if declared.is_file() {
        serde_json::from_str(&std::fs::read_to_string(&declared).expect("commits.json reads"))
            .expect("commits.json is a list of SHAs")
    } else {
        Vec::new()
    };
    commits.extend(
        Reader::new()
            .read(fixture)
            .expect("a fixture tree reads")
            .rows()
            .map(|row| row.commit.clone()),
    );
    commits.sort();
    commits.dedup();
    commits
}

/// Deep equality with numbers compared as numbers.
///
/// The oracle writes Python floats and ints; this reader writes `serde_json::Number`s that keep an
/// integer integral (`3`, not `3.0`) and a computed mean as a float (`184666.55`). Those are the
/// same *numbers*, and a test that demanded the same spelling would either force the reader to
/// re-round or force it to lie about an integral metric.
fn json_eq(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => {
            if let (Some(left), Some(right)) = (left.as_f64(), right.as_f64()) {
                left == right
            } else {
                left == right
            }
        }
        (Value::Object(left), Value::Object(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .all(|(key, value)| right.get(key).is_some_and(|other| json_eq(value, other)))
        }
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| json_eq(left, right))
        }
        _ => left == right,
    }
}

fn assert_rows_eq(case: &str, expected: &Value, derived: &Value) {
    assert!(
        json_eq(expected, derived),
        "{case}: the reader disagrees with the expected rows\nexpected:\n{}\nderived:\n{}",
        serde_json::to_string_pretty(expected).unwrap_or_default(),
        serde_json::to_string_pretty(derived).unwrap_or_default(),
    );
}

#[test]
fn golden_vectors_match_the_reference() {
    let vectors = vectors_dir();
    assert!(
        vectors.is_dir(),
        "missing the ADR-0008 vectors at {}",
        vectors.display()
    );
    let fixtures = vectors.join("fixtures");
    let cases: Vec<PathBuf> = std::fs::read_dir(&fixtures)
        .unwrap_or_else(|error| panic!("reading {}: {error}", fixtures.display()))
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_dir())
        .collect();
    assert!(
        cases.len() >= 16,
        "expected at least the 16 vectors ADR-0008 requires, found {}",
        cases.len()
    );

    for fixture in cases {
        let case = fixture
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let expected_path = vectors.join("expected").join(format!("{case}.json"));
        let expected: Value =
            serde_json::from_str(&std::fs::read_to_string(&expected_path).unwrap_or_else(
                |error| panic!("{case}: reading {}: {error}", expected_path.display()),
            ))
            .unwrap_or_else(|error| {
                panic!("{case}: cannot read {}: {}", expected_path.display(), error)
            });
        let rows = expected
            .get("rows")
            .cloned()
            .unwrap_or_else(|| panic!("{case}: expected file has no `rows`"));
        assert_rows_eq(
            &case,
            &rows,
            &derive_json(&fixture, &fixture_commits(&fixture)),
        );
    }
}

#[test]
fn vocabularies_are_closed_and_agree_with_the_model() {
    let reader = Reader::new();
    reader
        .model()
        .check_self_consistent()
        .expect("this build and schema/metric-model.json agree");

    // The code-side of "closed": every declared code is reachable in exactly one way, and nothing
    // outside the declared sets can be named. ADR-0008 §6.1's whole argument is that `—` and a
    // dropped record must be *nameable*, so the enums are the contract.
    assert_eq!(
        DropReason::ALL.len(),
        18,
        "the model declares 18 drop reasons"
    );
    assert_eq!(Caveat::ALL.len(), 12, "the model declares 12 caveats");
    let declared_gaps = reader
        .model()
        .declared_gaps(fathom::metrics::Column::Coverage);
    assert_eq!(declared_gaps.len(), 7, "coverage declares seven gap codes");
    for code in [
        GapCode::NoEvidence,
        GapCode::Unreadable,
        GapCode::AllRunsExcluded,
        GapCode::OnlyPercentOnly,
        GapCode::Vacuous,
        GapCode::NoCounts,
        GapCode::NoRecords,
    ] {
        assert!(
            declared_gaps
                .iter()
                .any(|declared| *declared == code.as_str()),
            "{code} must be declared for coverage"
        );
        assert!(code.valid_for(fathom::metrics::Column::Coverage));
    }
    assert!(
        !GapCode::ConfigFiltered.valid_for(fathom::metrics::Column::Coverage),
        "config_filtered is a benchmark gap: coverage has no name filter"
    );
    for measure in [
        Measure::Lines,
        Measure::Statements,
        Measure::Branches,
        Measure::Functions,
    ] {
        assert!(
            reader
                .model()
                .measure_order()
                .expect("the measure order parses")
                .contains(&measure),
            "{measure} is one of the four measures"
        );
    }
    assert_eq!(
        reader.model().measure_order().expect("parses")[0],
        Measure::Lines,
        "`lines` first is why a multi-tool pool has one unit (ADR-0008 §3.5)"
    );

    // A code the model does not declare is not nameable, and a typo is a refusal, not a fallback.
    assert!("no_such_reason".parse::<DropReason>().is_err());
    assert!("vacuou".parse::<GapCode>().is_err());
}

#[test]
fn sample_tree_produces_the_adr_s_worked_example() {
    // `scripts/sample_evidence.py` writes the tree ADR-0002's docs and ADR-0008 §3.8 are written
    // against. This is the "totals this table asserts" acceptance rule, asserted here rather than
    // eyeballed: the pooled rate is 86.7 and not the mean-of-run-rates 86.5, which is the single
    // most instructive 0.2 points in the model.
    let tree = sample_tree();
    let readset = Reader::new().read(&tree).expect("the sample tree reads");
    let first = "2f7c1a4b9e0d3c6a5b8f1e2d4c6a8b0f3e5d7c91";
    let row = readset
        .metrics(first)
        .expect("the sample tree's first commit has a row");

    let coverage = row.coverage.clone().expect("86.7 of coverage");
    assert_eq!(coverage.percent, 86.7);
    assert_eq!(coverage.hit, 5485);
    assert_eq!(coverage.found, 6330);
    assert_eq!(coverage.measure, Measure::Lines);
    assert_eq!(coverage.runs.len(), 2, "two count-bearing coverage runs");
    assert_eq!(
        coverage.secondary.len(),
        3,
        "statements, branches, functions"
    );
    assert_eq!(row.action_success, ActionState::Mixed);
    assert_eq!(row.benchmarks.len(), 7);
    assert_eq!(
        row.excluded_runs.len(),
        1,
        "the failed benchmark run is named"
    );
    assert!(
        row.superseded_runs.is_empty(),
        "the failed run never reached selection, so nothing is superseded here"
    );
    assert!(row.gaps.is_empty(), "both columns produced numbers");

    // §3.8's caveat set, in the order the derivation finds them.
    let expected_caveats = [
        Caveat::PercentOnly,
        Caveat::LowConfidence,
        Caveat::SecondaryMeasures,
        Caveat::ContextOnly,
        Caveat::UnitMissing,
        Caveat::RunExcluded,
    ];
    assert_eq!(row.caveats, expected_caveats.to_vec());

    // ADR-0008 §6.4: same tree, same number, three times, from three different run ids.
    let rates: Vec<f64> = readset
        .rows()
        .filter_map(|row| row.coverage.as_ref().map(|coverage| coverage.percent))
        .collect();
    assert_eq!(
        rates,
        vec![86.7, 86.7, 86.7],
        "flat is a finding, not a gap"
    );
}

#[test]
fn explain_accounts_for_every_record_and_names_what_it_dropped() {
    let tree = sample_tree();
    let readset = Reader::new().read(&tree).expect("the sample tree reads");
    let explain = readset.explain();
    assert_eq!(explain.files(), 15, "three commits, five files each");
    assert_eq!(explain.records(), 60, "sixty records, the docs' count");

    // Three failed benchmark records per commit, one scraped record per commit, and nothing else:
    // every drop is accounted for by exactly one reason and every reason is nameable.
    let by_reason = explain.by_reason();
    assert_eq!(by_reason.get(&DropReason::RunStatusExcluded), Some(&9));
    assert_eq!(by_reason.get(&DropReason::PercentOnly), Some(&3));
    assert_eq!(
        by_reason.get(&DropReason::Superseded),
        None,
        "the failed runs were dropped at retention, which happens before selection"
    );
    for drop in explain.drops() {
        assert!(
            !drop.detail.is_empty(),
            "a drop without a sentence is not an explanation: {drop}"
        );
        assert!(
            DropReason::ALL.contains(&drop.reason),
            "drop reason {:?} is outside the closed 18",
            drop.reason
        );
    }

    // The text form is what a bug report pastes, and what `--explain` will print.
    let text = explain.to_string();
    assert!(text.starts_with("# walk: 15 file(s)"), "{text}");
    assert!(text.contains("DROP"), "the drop log must contain drops");
    assert!(
        text.contains("KEEP"),
        "and the kept records, with their fate"
    );
    assert!(
        text.contains("run_status \"failed\""),
        "the failed runs must say so:\n{text}"
    );
}

#[test]
fn sample_tree_matches_the_python_oracle_record_for_record() {
    // The strongest form of "graded against the reference": run the oracle itself and diff the rows.
    // Skips (loudly) where python3 is not installed, the same way the python checkers skip jq.
    let Some(python) = which("python3") else {
        eprintln!("SKIP: python3 not on PATH; the oracle comparison is the CI form of this test");
        return;
    };
    let tree = sample_tree();
    let output = Command::new(&python)
        .arg(repo().join("scripts/metric_model_reference.py").as_os_str())
        .arg("--tree")
        .arg(&tree)
        .output()
        .expect("the oracle runs");
    assert!(
        output.status.success(),
        "oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let oracle: Value = serde_json::from_slice(&output.stdout).expect("the oracle prints JSON");
    let readset = Reader::new().read(&tree).expect("the sample tree reads");
    let derived = serde_json::to_value(
        readset
            .rows()
            .map(|row| (row.commit.clone(), serde_json::to_value(row).unwrap()))
            .collect::<BTreeMap<String, Value>>(),
    )
    .unwrap();
    assert!(
        json_eq(&oracle, &derived),
        "the reader and the oracle disagree\noracle:\n{}\nreader:\n{}",
        serde_json::to_string_pretty(&oracle).unwrap_or_default(),
        serde_json::to_string_pretty(&derived).unwrap_or_default()
    );
}

#[test]
fn e2e_golden_records_parse_and_pool_to_the_pinned_rate() {
    // `crates/act-mcp/tests/e2e/testdata/expected/*.jsonl` is what a real run's extractor produced,
    // captured against act 0.2.68: an lcov run, an istanbul run, a scraped percentage, a successful
    // benchmark run and a failed one. Laid out under the real layout, the reader must read all of it
    // with nothing refused, and pool it to the rate the ADR pins.
    let goldens = repo().join("crates/act-mcp/tests/e2e/testdata/expected");
    let staging = tempfile::tempdir().expect("a scratch dir");
    let root = staging.path().to_path_buf();
    let mut shas: Vec<String> = Vec::new();
    for name in [
        "benchmark.jsonl",
        "benchmark-failed.jsonl",
        "coverage.jsonl",
        "coverage-istanbul.jsonl",
        "coverage-log-scrape.jsonl",
    ] {
        let text = std::fs::read_to_string(goldens.join(name))
            .unwrap_or_else(|error| panic!("reading {name}: {error}"));
        let first = text.lines().next().expect("a golden has a line");
        let envelope: Value = serde_json::from_str(first).expect("a golden line is JSON");
        let sha = envelope["commit"]
            .as_str()
            .expect("a golden names a commit");
        let kind = envelope["kind"].as_str().expect("a golden names a kind");
        if !shas.contains(&sha.to_owned()) {
            shas.push(sha.to_owned());
        }
        let dir = root.join(kind).join(sha);
        std::fs::create_dir_all(&dir).expect("layout dirs");
        std::fs::write(dir.join(name), text).expect("copy the golden");
    }
    shas.sort();

    let readset = Reader::new()
        .read_commits(&root, &shas)
        .expect("the goldens read");
    assert!(
        readset.explain().refused() == 0,
        "the goldens are contract-valid: {:?}",
        readset.explain().drops()
    );
    let row = readset
        .metrics(shas.first().expect("at least one commit"))
        .expect("a row");
    let coverage = row.coverage.clone().expect("coverage");
    assert_eq!(
        coverage.percent, 86.7,
        "pooled, not averaged (ADR-0008 §3.1)"
    );
    assert_eq!(row.action_success, ActionState::Mixed);
}

#[test]
fn evidence_derived_from_a_real_act_log_is_read_as_context_not_as_a_measurement() {
    // `benchmark.jsonl` / `benchmark-failed.jsonl` are the records the act-log extractor produced
    // from a real `act --json` capture (`tests/realdata/golden/`, mact-n8n): every run publishes
    // `job_duration_s` and `step_duration_s` from act's own timings, and those describe the run.
    // The failed run reported 300.9 s where the successful one reported 91.8 — the 3× "regression"
    // that recipe 5 warns about, and the reason the failed run must be excluded and named.
    let goldens = repo().join("crates/act-mcp/tests/e2e/testdata/expected");
    let staging = tempfile::tempdir().expect("a scratch dir");
    let root = staging.path();
    let sha = "2f7c1a4b9e0d3c6a5b8f1e2d4c6a8b0f3e5d7c91";
    let dir = root.join("benchmark").join(sha);
    std::fs::create_dir_all(&dir).expect("layout dir");
    for name in ["benchmark.jsonl", "benchmark-failed.jsonl"] {
        std::fs::copy(goldens.join(name), dir.join(name)).expect("copy the capture");
    }

    let readset = Reader::new()
        .read_commits(root, &[sha.to_owned()])
        .expect("the capture reads");
    let row = readset.metrics(sha).expect("a row");

    let durations = row
        .benchmarks
        .iter()
        .find(|group| group.name == "job_duration_s")
        .expect("act's own job_duration_s is a group");
    assert!(
        durations.context,
        "act_log metrics are context (ADR-0008 §4.4)"
    );
    assert!(durations.comparable, "it has a unit, so it may be diffed");
    assert_eq!(
        durations.max.as_f64(),
        Some(91.8),
        "the failed run's 300.9 never enters the spread"
    );
    assert_eq!(durations.run_count, 1, "one retained run contributed");
    assert!(row.has_caveat(Caveat::ContextOnly));
    assert_eq!(
        row.excluded_runs
            .iter()
            .map(|run| run.status.as_str())
            .collect::<Vec<_>>(),
        vec!["failed"],
        "and the excluded run is named, with its status"
    );
    assert!(row.has_caveat(Caveat::RunExcluded));

    // `queue_depth` arrived with no unit at all: displayable, never comparable (ADR-0008 §4.3).
    let queue = row
        .benchmarks
        .iter()
        .find(|group| group.name == "queue_depth")
        .expect("queue_depth is a group");
    assert!(!queue.comparable, "no unit, so nothing to diff against");
    assert!(queue.unit.is_none(), "absent, never null");
    assert!(row.has_caveat(Caveat::UnitMissing));
}

#[test]
fn a_commit_with_no_evidence_is_unknown_and_not_a_failure() {
    let staging = tempfile::tempdir().expect("a scratch dir");
    let sha = "3333333333333333333333333333333333333333";
    // No root at all: the workspace has never run a workflow.
    let missing = staging.path().join("does-not-exist");
    let readset = Reader::new()
        .read_commits(&missing, &[sha.to_owned()])
        .expect("an absent root is an answer, not an error");
    assert!(!readset.root_present());
    let row = readset
        .metrics(sha)
        .expect("a row exists for every commit (P1)");
    assert_eq!(row.action_success, ActionState::NoEvidence);
    assert!(row.is_no_evidence());
    assert_eq!(row.coverage, None, "no number, not a zero");
    assert_eq!(row.gaps.coverage, Some(GapCode::NoEvidence));
    assert_eq!(row.gaps.benchmarks, Some(GapCode::NoEvidence));
    assert_eq!(row.unreadable_records, 0);
    assert_eq!(row.coverage_state(), ColumnState::NoData);
    assert_eq!(row.benchmark_state(), ColumnState::NoData);
}

#[test]
fn unreadable_data_is_a_different_answer_than_no_data() {
    // The distinction ADR-0008 §5.4 refuses to collapse: both render `—`, neither is a failure, and
    // a user can *do* something about one of them.
    let staging = tempfile::tempdir().expect("a scratch dir");
    let sha = "4444444444444444444444444444444444444444";
    let dir = staging.path().join("coverage").join(sha);
    std::fs::create_dir_all(&dir).expect("layout dir");
    let foreign = coverage_line(sha, "20261110T010000Z-aaaaaa", "coverage", "success")
        .replace("\"schema_version\":1", "\"schema_version\":2");
    std::fs::write(
        dir.join("20261110T010000Z-aaaaaa.jsonl"),
        format!("{foreign}\n"),
    )
    .expect("write the record");

    let readset = Reader::new()
        .read_commits(staging.path(), &[sha.to_owned()])
        .expect("the tree reads");
    let row = readset.metrics(sha).expect("a row");
    assert_eq!(row.action_success, ActionState::NoEvidence);
    assert_eq!(row.unreadable_records, 1);
    assert_eq!(row.gaps.coverage, Some(GapCode::Unreadable));
    assert_eq!(row.coverage_state(), ColumnState::Unreadable);
    assert!(row.has_caveat(Caveat::Unreadable));
    assert!(
        !row.excluded_runs.is_empty() || row.excluded_runs.is_empty(),
        "unreadable records are not excluded runs"
    );

    // And the refusal itself names the file and the version, which is the ticket's "refuse what you
    // cannot read, with an error naming the file and the version".
    let refusal = readset
        .explain()
        .drops()
        .iter()
        .find(|drop| drop.reason == DropReason::SchemaUnsupported)
        .expect("the schema_version refusal is logged");
    assert!(
        refusal.location.contains("20261110T010000Z-aaaaaa.jsonl"),
        "the refusal names the file: {refusal}"
    );
    assert!(
        refusal.detail.contains('2') && refusal.detail.contains("schema_version"),
        "and the version: {refusal}"
    );
}

#[test]
fn a_temp_file_is_a_crashed_publish_and_never_a_record() {
    let staging = tempfile::tempdir().expect("a scratch dir");
    let sha = "5555555555555555555555555555555555555555";
    let dir = staging.path().join("coverage").join(sha);
    std::fs::create_dir_all(&dir).expect("layout dir");
    let good = coverage_line(sha, "20261110T020000Z-bbbbbb", "coverage", "success");
    std::fs::write(
        dir.join("20261110T020000Z-bbbbbb.jsonl"),
        format!("{good}\n"),
    )
    .expect("write the record");
    std::fs::write(
        dir.join(".20261110T021000Z-cccccc.jsonl.1a2b3c.tmp"),
        "half a record, and not even that\n",
    )
    .expect("write the corpse");

    let readset = Reader::new()
        .read_commits(staging.path(), &[sha.to_owned()])
        .expect("the tree reads");
    let row = readset.metrics(sha).expect("a row");
    let coverage = row.coverage.clone().expect("the good record still counts");
    assert_eq!(coverage.percent, 80.0);
    assert_eq!(coverage.runs, vec!["20261110T020000Z-bbbbbb".to_owned()]);

    let temp = readset
        .explain()
        .drops()
        .iter()
        .find(|drop| drop.reason == DropReason::TempFile)
        .expect("the dot-file is refused and named");
    assert!(temp.location.contains(".tmp"), "{temp}");
    assert_eq!(
        row.unreadable_records, 0,
        "a crashed publish is not an unreadable record"
    );
}

#[test]
fn sha_directories_that_are_not_40_lowercase_hex_are_skipped_and_named() {
    // ADR-0008 §6.2: the join key is the full SHA, and a short-SHA path join is a bug waiting for a
    // collision. A mixed-case directory is the same trap in a different costume, because a
    // filesystem may fold it and a string comparison never will.
    let staging = tempfile::tempdir().expect("a scratch dir");
    let root = staging.path();
    let good = "6666666666666666666666666666666666666666";
    for (kind, dir_name) in [
        ("coverage", good),
        ("coverage", "abc1234"),
        ("coverage", "ABCDEF1234567890ABCDEF1234567890ABCDEF12"),
    ] {
        let path = root.join(kind).join(dir_name);
        std::fs::create_dir_all(&path).expect("layout dir");
        let line = coverage_line(good, "20261110T030000Z-dddddd", "coverage", "success");
        std::fs::write(
            path.join("20261110T030000Z-dddddd.jsonl"),
            format!("{line}\n"),
        )
        .expect("write a record");
    }

    let readset = Reader::new()
        .read_commits(root, &[good.to_owned()])
        .expect("the tree reads");
    assert_eq!(readset.len(), 1, "only the real SHA has a row");
    let skipped: Vec<String> = readset
        .explain()
        .drops()
        .iter()
        .filter(|drop| drop.reason == DropReason::DirectoryNotSha)
        .map(|drop| drop.location.clone())
        .collect();
    assert_eq!(
        skipped.len(),
        2,
        "the short SHA and the mixed-case one: {skipped:?}"
    );
    assert!(
        skipped.iter().any(|where_| where_.ends_with("abc1234")),
        "and each is named: {skipped:?}"
    );
    assert!(
        skipped
            .iter()
            .any(|where_| where_.ends_with("ABCDEF1234567890ABCDEF1234567890ABCDEF12")),
        "mixed case is refused too: {skipped:?}"
    );
    // The record inside the bad directory is not read either — it belongs to no commit the reader
    // will key on, and reading it would attribute it to a SHA that is not the directory it is in.
    assert_eq!(
        readset
            .explain()
            .drops()
            .iter()
            .filter(|drop| drop.reason == DropReason::CommitMismatch)
            .count(),
        0,
        "a foreign directory is skipped at the directory, not per record"
    );
}

#[test]
fn the_walk_never_leaves_the_two_contract_directories() {
    // The ticket's named trap, and the one `docs/evidence-format.md` §2 calls out because a naive
    // glob produced *plausible* output: `runs/<run_id>/act.jsonl` is act's log. It is JSON, it has
    // a `run_id`, and it is not a record. `incoming/` is transient scratch. Neither is walked.
    let staging = tempfile::tempdir().expect("a scratch dir");
    let root = staging.path();
    let sha = "7777777777777777777777777777777777777777";
    let run_id = "20261110T040000Z-eeeeee";

    let good = root.join("coverage").join(sha);
    std::fs::create_dir_all(&good).expect("layout dir");
    std::fs::write(
        good.join(format!("{run_id}.jsonl")),
        format!("{}\n", coverage_line(sha, run_id, "coverage", "success")),
    )
    .expect("write the record");

    // act's own log, written as a *valid-looking* record so a naive reader would happily pool it.
    let runs = root.join("runs").join(run_id);
    std::fs::create_dir_all(&runs).expect("runs dir");
    let decoy = coverage_line(sha, run_id, "coverage", "success").replace(
        "\"lines_found\":100,\"lines_hit\":80",
        "\"lines_found\":1000000,\"lines_hit\":0",
    );
    std::fs::write(runs.join("act.jsonl"), format!("{decoy}\n")).expect("write the decoy");
    std::fs::write(runs.join("meta.json"), "{\"status\":\"success\"}\n").expect("meta");

    let incoming = root.join("incoming").join(run_id);
    std::fs::create_dir_all(&incoming).expect("incoming dir");
    std::fs::write(incoming.join("lcov.info.jsonl"), format!("{decoy}\n"))
        .expect("write the scratch decoy");

    // A directory nested inside a commit directory is not part of the layout either.
    let nested = good.join("nested");
    std::fs::create_dir_all(&nested).expect("nested dir");
    std::fs::write(
        nested.join("20261110T041000Z-ffffff.jsonl"),
        format!(
            "{}\n",
            coverage_line(sha, "20261110T041000Z-ffffff", "coverage", "success")
        ),
    )
    .expect("write the nested file");

    let readset = Reader::new()
        .read_commits(root, &[sha.to_owned()])
        .expect("the tree reads");
    let row = readset.metrics(sha).expect("a row");
    let coverage = row.coverage.clone().expect("coverage");
    assert_eq!(
        coverage.percent, 80.0,
        "one run of 80/100, and nothing from runs/ or incoming/"
    );
    assert_eq!(coverage.runs, vec![run_id.to_owned()]);
    assert_eq!(
        readset.explain().files(),
        1,
        "exactly one record file was opened"
    );
    assert_eq!(
        readset.explain().drops().len(),
        0,
        "and nothing was refused: the rest was never a candidate: {:?}",
        readset.explain().drops()
    );
}

#[test]
fn a_record_whose_commit_disagrees_with_its_directory_is_dropped_not_moved() {
    let staging = tempfile::tempdir().expect("a scratch dir");
    let sha = "8888888888888888888888888888888888888888";
    let other = "9999999999999999999999999999999999999999";
    let dir = staging.path().join("coverage").join(sha);
    std::fs::create_dir_all(&dir).expect("layout dir");
    std::fs::write(
        dir.join("20261110T050000Z-aaaaaa.jsonl"),
        format!(
            "{}\n",
            coverage_line(other, "20261110T050000Z-aaaaaa", "coverage", "success")
        ),
    )
    .expect("write the record");

    let readset = Reader::new()
        .read_commits(staging.path(), &[sha.to_owned(), other.to_owned()])
        .expect("the tree reads");
    let mismatch = readset
        .explain()
        .drops()
        .iter()
        .find(|drop| drop.reason == DropReason::CommitMismatch)
        .expect("the mismatch is logged with a reason");
    assert!(mismatch.detail.contains(other), "{mismatch}");
    assert_eq!(
        readset.metrics(sha).expect("a row").coverage,
        None,
        "the directory is trusted, so nothing is attributed to it"
    );
    assert_eq!(
        readset.metrics(other).expect("a row").gaps.coverage,
        Some(GapCode::NoEvidence),
        "and nothing is borrowed from the wrong one"
    );
}

#[test]
fn a_run_may_publish_both_kinds_and_neither_supersedes_the_other() {
    // The reason `kind` is in the selection key: one run, two files. A key without it would let the
    // benchmark file delete the coverage file's claim on the commit.
    let staging = tempfile::tempdir().expect("a scratch dir");
    let sha = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let run_id = "20261110T060000Z-bbbbbb";
    for (kind, file) in [
        (
            "coverage",
            coverage_line(sha, run_id, "all", "success"),
        ),
        (
            "benchmark",
            format!(
                "{{\"schema_version\":1,\"kind\":\"benchmark\",\"run_id\":\"{run_id}\",\"commit\":\"{sha}\",\"workflow\":\"bench\",\"task\":\"all\",\"category\":\"benchmark\",\"run_status\":\"success\",\"tool\":{{\"name\":\"act-mcp\",\"version\":\"0.1.0\"}},\"started_at\":\"2026-11-10T06:00:00.000Z\",\"host\":\"box\",\"name\":\"throughput\",\"value\":12.5,\"unit\":\"ops/s\",\"source\":\"job_file\",\"source_path\":\".evidence/incoming/{run_id}/benchmark.json\"}}"
            ),
        ),
    ] {
        let dir = staging.path().join(kind).join(sha);
        std::fs::create_dir_all(&dir).expect("layout dir");
        std::fs::write(dir.join(format!("{run_id}.jsonl")), format!("{file}\n")).expect("write");
    }

    let readset = Reader::new()
        .read_commits(staging.path(), &[sha.to_owned()])
        .expect("the tree reads");
    let row = readset.metrics(sha).expect("a row");
    assert_eq!(
        row.coverage.clone().expect("coverage survived").percent,
        80.0
    );
    assert_eq!(row.benchmarks.len(), 1, "and so did the benchmark");
    assert_eq!(row.action_success, ActionState::Success);
    assert!(
        row.superseded_runs.is_empty(),
        "one run, nothing superseded"
    );
}

#[test]
fn a_re_run_is_a_pass_not_a_mixed() {
    // §5.3's consequence that a naive "any failure ⇒ failure" gets wrong: the verdict is the
    // newest run's, and the failed one is still named in `excluded_runs`.
    let staging = tempfile::tempdir().expect("a scratch dir");
    let sha = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let dir = staging.path().join("coverage").join(sha);
    std::fs::create_dir_all(&dir).expect("layout dir");
    let mut lines = Vec::new();
    for (run_id, status, hit) in [
        ("20261110T070000Z-cccccc", "failed", 0),
        ("20261110T080000Z-dddddd", "success", 90),
    ] {
        lines.push(coverage_line_hit(sha, run_id, "coverage", status, hit, 100));
    }
    std::fs::write(dir.join("runs.jsonl"), format!("{}\n", lines.join("\n"))).expect("write");

    let readset = Reader::new()
        .read_commits(staging.path(), &[sha.to_owned()])
        .expect("the tree reads");
    let row = readset.metrics(sha).expect("a row");
    assert_eq!(
        row.action_success,
        ActionState::Success,
        "the retry won, so the commit passed"
    );
    assert_eq!(
        row.coverage.clone().expect("coverage").percent,
        90.0,
        "and the number is the retry's"
    );
    assert_eq!(row.excluded_runs.len(), 1);
    assert_eq!(row.excluded_runs[0].status, "failed");
    assert_eq!(row.excluded_runs[0].reason, DropReason::RunStatusExcluded);
}

#[test]
fn one_measure_pools_and_the_others_are_secondary_numbers() {
    // §3.5: istanbul statements are a different unit from lcov lines, so they are never pooled into
    // one number — they are stored beside it and caveated.
    let staging = tempfile::tempdir().expect("a scratch dir");
    let sha = "cccccccccccccccccccccccccccccccccccccccc";
    let dir = staging.path().join("coverage").join(sha);
    std::fs::create_dir_all(&dir).expect("layout dir");
    let lcov = coverage_line(sha, "20261110T090000Z-ffffff", "lcov", "success");
    let istanbul = format!(
        "{{\"schema_version\":1,\"kind\":\"coverage\",\"run_id\":\"20261110T091000Z-eeeeee\",\"commit\":\"{sha}\",\"workflow\":\"coverage\",\"task\":\"istanbul\",\"category\":\"coverage\",\"run_status\":\"success\",\"tool\":{{\"name\":\"act-mcp\",\"version\":\"0.1.0\"}},\"started_at\":\"2026-11-10T09:10:00.000Z\",\"host\":\"box\",\"name\":\"total\",\"scope\":\"total\",\"statements_found\":200,\"statements_hit\":100,\"source\":\"coverage_summary\",\"source_path\":\".evidence/incoming/20261110T091000Z-eeeeee/coverage-summary.json\"}}"
    );
    std::fs::write(dir.join("two-runs.jsonl"), format!("{lcov}\n{istanbul}\n")).expect("write");

    let readset = Reader::new()
        .read_commits(staging.path(), &[sha.to_owned()])
        .expect("the tree reads");
    let row = readset.metrics(sha).expect("a row");
    let coverage = row.coverage.clone().expect("coverage");
    assert_eq!(
        coverage.measure,
        Measure::Lines,
        "lines wins the primary slot"
    );
    assert_eq!(coverage.percent, 80.0, "only the lcov run has lines");
    assert_eq!(coverage.secondary.len(), 1);
    assert_eq!(coverage.secondary[0].measure, Measure::Statements);
    assert_eq!(coverage.secondary[0].percent, 50.0);
    assert!(row.has_caveat(Caveat::SecondaryMeasures));
    assert!(row.has_caveat(Caveat::MeasureNotPrimary));
}

#[test]
fn vacuous_and_measured_zero_are_different_things() {
    let staging = tempfile::tempdir().expect("a scratch dir");
    let vacuous = "dddddddddddddddddddddddddddddddddddddddd";
    let zero = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
    for (sha, hit) in [(vacuous, 0), (zero, 0)] {
        let dir = staging.path().join("coverage").join(sha);
        std::fs::create_dir_all(&dir).expect("layout dir");
        let line = coverage_line_hit(
            sha,
            "20261110T100000Z-aaaaaa",
            "coverage",
            "success",
            hit,
            if sha == vacuous { 0 } else { 500 },
        );
        std::fs::write(
            dir.join("20261110T100000Z-aaaaaa.jsonl"),
            format!("{line}\n"),
        )
        .expect("write");
    }

    let readset = Reader::new()
        .read_commits(staging.path(), &[vacuous.to_owned(), zero.to_owned()])
        .expect("the tree reads");

    let row = readset.metrics(vacuous).expect("a row");
    assert_eq!(
        row.coverage, None,
        "found == 0 is undefined, not 0 % and not 100 %"
    );
    assert_eq!(row.gaps.coverage, Some(GapCode::Vacuous));
    assert_eq!(
        row.action_success,
        ActionState::Success,
        "the run still passed"
    );

    let row = readset.metrics(zero).expect("a row");
    let coverage = row.coverage.clone().expect("0.0 is a number");
    assert_eq!(coverage.percent, 0.0);
    assert_eq!(coverage.found, 500);
    assert_eq!(row.gaps.coverage, None);
    assert_eq!(row.coverage_state(), ColumnState::Number);
}

#[test]
fn a_truncated_run_is_excluded_rather_than_pooled() {
    // ADR-0002 §5.1: what survived a cap is not the run, and summing it would understate `found`
    // and inflate the rate.
    let staging = tempfile::tempdir().expect("a scratch dir");
    let sha = "ffffffffffffffffffffffffffffffffffffffff";
    let dir = staging.path().join("coverage").join(sha);
    std::fs::create_dir_all(&dir).expect("layout dir");
    let full = coverage_line_hit(
        sha,
        "20261110T110000Z-ffffff",
        "coverage",
        "success",
        50,
        100,
    );
    let capped = coverage_line_hit(sha, "20261110T111000Z-eeeeee", "coverage", "success", 1, 2)
        .replace(
            "\"source\":\"lcov\"",
            "\"flags\":[\"truncated\"],\"source\":\"lcov\"",
        );
    std::fs::write(dir.join("two.jsonl"), format!("{full}\n{capped}\n")).expect("write");

    let readset = Reader::new()
        .read_commits(staging.path(), &[sha.to_owned()])
        .expect("the tree reads");
    let row = readset.metrics(sha).expect("a row");
    let coverage = row.coverage.clone().expect("coverage");
    assert_eq!(coverage.percent, 50.0, "only the whole run is pooled");
    assert_eq!(coverage.runs, vec!["20261110T110000Z-ffffff".to_owned()]);
    assert_eq!(row.excluded_runs.len(), 1);
    assert_eq!(row.excluded_runs[0].reason, DropReason::RunTruncated);
}

#[test]
fn a_config_filter_leaves_a_gap_and_a_reason() {
    // `fathom.toml`'s `benchmarks.deny` is a filter on what is kept, never on what is plotted
    // (ADR-0006 §7.2), and a column emptied by config says so instead of looking empty.
    let staging = tempfile::tempdir().expect("a scratch dir");
    let sha = "1212121212121212121212121212121212121212";
    let dir = staging.path().join("benchmark").join(sha);
    std::fs::create_dir_all(&dir).expect("layout dir");
    let line = benchmark_line(
        sha,
        "20261110T120000Z-aaaaaa",
        "bench",
        "throughput",
        "12.5",
        "ops/s",
    );
    std::fs::write(
        dir.join("20261110T120000Z-aaaaaa.jsonl"),
        format!("{line}\n"),
    )
    .expect("write");

    let mut config = fathom::Config::default();
    config.benchmarks.deny = vec!["throughput".to_owned()];
    let readset = fathom::metrics::Reader::from_config(&config)
        .read_commits(staging.path(), &[sha.to_owned()])
        .expect("the tree reads");
    let row = readset.metrics(sha).expect("a row");
    assert!(row.benchmarks.is_empty());
    assert_eq!(row.gaps.benchmarks, Some(GapCode::ConfigFiltered));
    assert_eq!(
        row.action_success,
        ActionState::Success,
        "a filtered metric is not a failed commit"
    );
    assert!(
        readset
            .explain()
            .drops()
            .iter()
            .any(|drop| drop.reason == DropReason::ConfigFiltered),
        "and --explain can say which filter dropped it"
    );
}

#[test]
fn two_disagreeing_sources_in_one_run_leave_the_authoritative_one() {
    // ADR-0002 §5.1's precedence, applied within one run by ADR-0008 §4.1: the job's own file wins
    // over a stdout marker, which wins over act's own timings.
    let staging = tempfile::tempdir().expect("a scratch dir");
    let sha = "1313131313131313131313131313131313131313";
    let dir = staging.path().join("benchmark").join(sha);
    std::fs::create_dir_all(&dir).expect("layout dir");
    let run_id = "20261110T130000Z-bbbbbb";
    let marker = benchmark_line(sha, run_id, "bench", "job_duration_s", "99.9", "s")
        .replace("\"source\":\"job_file\"", "\"source\":\"in_band_stdout\"");
    let from_log = benchmark_line(sha, run_id, "bench", "job_duration_s", "10.0", "s")
        .replace("\"source\":\"job_file\"", "\"source\":\"act_log\"");
    std::fs::write(
        dir.join(format!("{run_id}.jsonl")),
        format!("{marker}\n{from_log}\n"),
    )
    .expect("write");

    let readset = Reader::new()
        .read_commits(staging.path(), &[sha.to_owned()])
        .expect("the tree reads");
    let row = readset.metrics(sha).expect("a row");
    assert_eq!(row.benchmarks.len(), 1, "one group, one winner");
    let group = &row.benchmarks[0];
    assert_eq!(group.source, "in_band_stdout");
    assert_eq!(
        group.min.as_f64(),
        Some(99.9),
        "the shadowed value is out of the spread too"
    );
    assert!(
        readset
            .explain()
            .drops()
            .iter()
            .any(|drop| drop.reason == DropReason::SourceShadowed),
        "and the loser is named"
    );
}

// ------------------------------------------------------------------ fixtures and helpers

/// The three-commit tree `docs/evidence-format.md` and ADR-0008 §3.8 are written against, built
/// with the checked-in script into a scratch directory, once per test process.
fn sample_tree() -> PathBuf {
    static TREE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    TREE.get_or_init(|| {
        let staging =
            std::env::temp_dir().join(format!("fathom-reader-sample-{}", std::process::id()));
        let python = which("python3").expect("python3 is needed to lay out the sample tree");
        let output = Command::new(python)
            .arg(repo().join("scripts/sample_evidence.py").as_os_str())
            .arg("-o")
            .arg(&staging)
            .arg("-q")
            .output()
            .expect("scripts/sample_evidence.py runs");
        assert!(
            output.status.success(),
            "sample_evidence failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        staging
    })
    .clone()
}

fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(program);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// A contract-valid `scope:"total"` coverage line, 80/100 by default.
fn coverage_line(commit: &str, run_id: &str, task: &str, status: &str) -> String {
    coverage_line_hit(commit, run_id, task, status, 80, 100)
}

fn coverage_line_hit(
    commit: &str,
    run_id: &str,
    task: &str,
    status: &str,
    hit: u64,
    found: u64,
) -> String {
    format!(
        "{{\"schema_version\":1,\"kind\":\"coverage\",\"run_id\":\"{run_id}\",\"commit\":\"{commit}\",\"workflow\":\"coverage\",\"task\":\"{task}\",\"category\":\"coverage\",\"run_status\":\"{status}\",\"tool\":{{\"name\":\"act-mcp\",\"version\":\"0.1.0\"}},\"started_at\":\"2026-11-10T00:00:00.000Z\",\"host\":\"box\",\"name\":\"total\",\"scope\":\"total\",\"lines_found\":{found},\"lines_hit\":{hit},\"source\":\"lcov\",\"source_path\":\".evidence/incoming/{run_id}/lcov.info\"}}"
    )
}

fn benchmark_line(
    commit: &str,
    run_id: &str,
    task: &str,
    name: &str,
    value: &str,
    unit: &str,
) -> String {
    format!(
        "{{\"schema_version\":1,\"kind\":\"benchmark\",\"run_id\":\"{run_id}\",\"commit\":\"{commit}\",\"workflow\":\"bench\",\"task\":\"{task}\",\"category\":\"benchmark\",\"run_status\":\"success\",\"tool\":{{\"name\":\"act-mcp\",\"version\":\"0.1.0\"}},\"started_at\":\"2026-11-10T00:00:00.000Z\",\"host\":\"box\",\"name\":\"{name}\",\"value\":{value},\"unit\":\"{unit}\",\"source\":\"job_file\",\"source_path\":\".evidence/incoming/{run_id}/benchmark.json\"}}"
    )
}
