//! The end-to-end harness (mact-f1w.15): a fake `act`, a real server, the whole
//! call → run → `.evidence` pipeline, and no docker anywhere.
//!
//! Each scenario boots the shipped binary against a throwaway git workspace with
//! `--act-binary` pointed at `tests/e2e/testdata/bin/act`, makes real MCP calls over a
//! real socket, and then reads the disk: the `.evidence` tree it expects is a checked-in
//! golden (`tests/e2e/testdata/scenarios/<name>/`), compared file-set first and
//! byte-for-byte second, after the single documented normalisation pass in
//! [`crate::normalize`].
//!
//! ```text
//! 01  coverage run succeeds              coverage JSONL present, schema-valid, golden
//! 02  benchmark run succeeds             job metrics + in-band markers + act-log durations
//! 03  benchmark run exits non-zero       evidence still written, run_status failed
//! 04  run cancelled mid-flight           cancelled, nothing published, staged payload kept
//! 04b …and act ignores SIGTERM          the runner escalates to SIGKILL            (#[ignore])
//! 05  two concurrent run_workflow calls  exactly one run, one `workspace_busy`
//! 06  category `other`                   nothing written to `.evidence`
//! 07  coverage payload missing           run succeeds, `no_coverage_found`, no file
//! 08  unknown workflow / task / commit   the ADR-0001 §5.3 codes, and no run registered
//! 09  istanbul summary only              the second coverage source, through the same path
//! 10  a job that prints its secrets      the value is in no byte the run wrote
//! 11  list_tasks over the fixture set    the workflows, their jobs, and the one it cannot parse
//! 12  the stub's own argv contract       unknown/forbidden/missing flags are refused
//! ```
//!
//! Timings: every scenario but `04b` finishes in a second or two. `04b` waits out the
//! shipped 20 s term grace because `serve` has no `--term-grace-secs` flag yet
//! (ADR-0004 §11 lists it as an operator flag; `Config::run_policy` still hardcodes the
//! default), so it is `#[ignore]`d rather than costing the suite 20 s. The fast SIGKILL
//! escalation path is covered in `tests/runner_execution.rs`, which can pass the floor
//! grace directly to the runner.

#![allow(clippy::too_many_lines)]

#[path = "../common/mod.rs"]
pub mod common;

mod golden;
mod harness;
mod normalize;

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

use harness::{wait_for_process_gone, Scenario, Stub};

/// The golden directory name of each scenario, so the name is in one place.
mod names {
    pub const COVERAGE: &str = "01-coverage-success";
    pub const BENCHMARK: &str = "02-benchmark-success";
    pub const BENCHMARK_FAILED: &str = "03-benchmark-failed";
    pub const CANCELLED: &str = "04-run-cancelled-mid-flight";
    pub const CONCURRENT: &str = "05-two-runs-one-workspace";
    pub const OTHER: &str = "06-category-other";
    pub const NO_PAYLOAD: &str = "07-coverage-payload-missing";
    pub const ISTANBUL: &str = "09-coverage-summary-source";
    pub const SECRETS: &str = "10-secret-never-on-disk";
}

/// The four required `run_workflow` arguments.
fn args(workflow: &str, task: &str, category: &str, commit: &str) -> Value {
    json!({ "workflow": workflow, "task": task, "category": category, "commit": commit })
}

/// An evidence path as a result reports it: workspace-relative, so it carries the
/// `.evidence/` prefix the default root sits under.
fn evidence_path(kind: &str, commit: &str, run_id: &str) -> String {
    format!(".evidence/{kind}/{commit}/{run_id}.jsonl")
}

/// The `source` values present in a benchmark JSONL.
fn sources(lines: &[Value]) -> Vec<String> {
    let mut found: Vec<String> = lines
        .iter()
        .filter_map(|line| line["source"].as_str().map(str::to_owned))
        .collect();
    found.sort();
    found.dedup();
    found
}

/// Parse a JSONL file, one `Value` per line — and fail loudly on a line that is not JSON.
fn jsonl(path: &Path) -> Vec<Value> {
    let text = harness::read_to_string(path);
    text.lines()
        .enumerate()
        .map(|(index, line)| {
            serde_json::from_str(line)
                .unwrap_or_else(|err| panic!("{} line {}: {err}", path.display(), index + 1))
        })
        .collect()
}

// --------------------------------------------------------------- 01 coverage

/// Scenario 1: a coverage run succeeds, and the coverage JSONL that lands is the
/// contract — one file at ADR-0002 §1.1's path, the run's identity on every record, and
/// `validate-evidence` clean over the whole tree.
///
/// This is also where the suite pins the command line the real `act` is promised
/// (ADR-0003 §7.2/§7.4): the stub wrote the argv it received, and the golden argv below
/// is spelled out token by token rather than derived from the code that built it.
#[tokio::test]
async fn scenario_01_coverage_run_publishes_coverage_evidence() {
    let mut scenario = Scenario::new();
    scenario.boot(
        Stub::new()
            .log("coverage-success.jsonl")
            .payload("coverage"),
    );
    let head = scenario.repo.head();

    let view = scenario
        .run_ok(args("coverage", "coverage", "coverage", &head))
        .await;
    let run_id = view["run_id"].as_str().expect("a run_id").to_owned();

    assert_eq!(view["status"], json!("success"), "{view}");
    assert_eq!(
        view["commit"],
        json!(head),
        "the full SHA, not the abbreviation"
    );
    assert_eq!(view["exit_code"], json!(0));
    assert_eq!(
        view["evidence_files"],
        json!([evidence_path("coverage", &head, &run_id)]),
        "one coverage file, at ADR-0002 §1.1's path"
    );

    let relative = evidence_path("coverage", &head, &run_id);
    let records = jsonl(&scenario.evidence_file(&relative));
    // The fixture tracefile: three SF blocks, so one total plus three file records.
    assert_eq!(records.len(), 4, "one total + one record per source file");
    assert_eq!(view["evidence_record_count"], json!(4));
    for record in &records {
        assert_eq!(record["kind"], json!("coverage"));
        assert_eq!(record["run_status"], json!("success"));
        assert_eq!(record["commit"], json!(head));
        assert_eq!(record["run_id"], json!(run_id));
        assert_eq!(record["workflow"], json!("coverage"));
        assert_eq!(record["task"], json!("coverage"));
        assert_eq!(record["source"], json!("lcov"));
        assert_eq!(record["schema_version"], json!(1));
    }
    let total = &records[0];
    assert_eq!(total["scope"], json!("total"));
    // 9 instrumented lines, 4 hit, 2 branches, 1 hit — the arithmetic of
    // `testdata/payload/lcov.info`, and the reason the payload fixture is small.
    assert_eq!(total["lines_found"], json!(9));
    assert_eq!(total["lines_hit"], json!(4));
    assert_eq!(total["branches_found"], json!(2));
    assert_eq!(total["branches_hit"], json!(1));

    // ADR-0002 §7: `validate-evidence` is the strict reader, and it must be clean over
    // everything this run wrote — the JSONL, `meta.json`, and the tree layout itself.
    let (code, output) = scenario.validate_evidence().await;
    assert_eq!(code, 0, "validate-evidence over the run's tree: {output}");

    // The whole tree, file set and bytes.
    scenario.assert_tree(names::COVERAGE, &view);

    // And the command line act was actually given, token for token (ADR-0003 §7.4).
    let scratch = scenario.run_scratch(&run_id);
    let expected: Vec<String> = vec![
        harness::testdata()
            .join("bin")
            .join("act")
            .display()
            .to_string(),
        "--directory".into(),
        scenario.repo.path().display().to_string(),
        "--workflows".into(),
        scenario
            .repo
            .path()
            .join(".github/workflows/coverage.yml")
            .display()
            .to_string(),
        "--job".into(),
        "coverage".into(),
        "--bind".into(),
        "--reuse=false".into(),
        "--rm".into(),
        "--pull=false".into(),
        "--rebuild".into(),
        "--use-gitignore".into(),
        "--json".into(),
        "--quiet=false".into(),
        "--secret-file".into(),
        scratch.join("secrets.env").display().to_string(),
        "--var-file".into(),
        scratch.join("inert.conf").display().to_string(),
        "--env-file".into(),
        scratch.join("inert.conf").display().to_string(),
        "--input-file".into(),
        scratch.join("inert.conf").display().to_string(),
        "--env".into(),
        format!(
            "ACTMCP_EVIDENCE_OUT={}",
            scenario.incoming(&run_id).display()
        ),
        "--env".into(),
        format!("ACTMCP_RUN_ID={run_id}"),
    ];
    assert_eq!(
        scenario.stub_argv(),
        expected,
        "the ADR-0003 §7.4 command line"
    );
    assert_eq!(
        scenario.stub_cwd(),
        scratch.display().to_string(),
        "act's cwd is the run's scratch directory (ADR-0003 §5.2)"
    );
}

// -------------------------------------------------------------- 02 benchmark

/// Scenario 2: a benchmark run publishes the job's own metrics, the in-band markers,
/// and act's own step timings — three producers, one file (ADR-0002 §5.1).
///
/// The log fixture deliberately contains a *different* job's 10-minute step: attribution
/// is asserted here, not assumed, because a `job_duration_s` that summed another job's
/// steps would be a plausible-looking number and a lie.
#[tokio::test]
async fn scenario_02_benchmark_run_publishes_metrics_markers_and_timings() {
    let mut scenario = Scenario::new();
    scenario.boot(
        Stub::new()
            .log("benchmark-success.jsonl")
            .payload("benchmark")
            .benchmark_marker(),
    );
    let head = scenario.repo.head();

    let view = scenario
        .run_ok(args("benchmark", "hyperfine-bench", "benchmark", &head))
        .await;
    let run_id = view["run_id"].as_str().expect("a run_id").to_owned();
    assert_eq!(view["status"], json!("success"), "{view}");

    let relative = evidence_path("benchmark", &head, &run_id);
    let records = jsonl(&scenario.evidence_file(&relative));
    assert!(
        sources(&records).contains(&"act_log".to_owned()),
        "act's own timings are always emitted (§5.3): {:?}",
        sources(&records)
    );
    let kinds: Vec<&str> = records
        .iter()
        .filter_map(|record| record["name"].as_str())
        .collect();
    assert!(
        kinds.contains(&"tokenizer_throughput"),
        "the payload file's metric (job_file) is here: {kinds:?}"
    );
    assert!(
        kinds.contains(&"cold_start_ms"),
        "the in-band marker (in_band_stdout) is here: {kinds:?}"
    );
    assert!(
        kinds.contains(&"step_duration_s") && kinds.contains(&"job_duration_s"),
        "act-log timings are here: {kinds:?}"
    );

    // The other job's ten minutes must not become this run's duration.
    let durations: Vec<f64> = records
        .iter()
        .filter(|record| record["name"].as_str() == Some("job_duration_s"))
        .filter_map(|record| record["value"].as_f64())
        .collect();
    assert!(
        !durations.iter().any(|seconds| *seconds >= 600.0),
        "another job's step leaked into job_duration_s: {durations:?}"
    );
    assert!(
        durations
            .iter()
            .any(|seconds| (*seconds - 91.8).abs() < 0.001),
        "0.5 + 88.234 + 3.066 = 91.8 s of this job's own steps: {durations:?}"
    );

    let (code, output) = scenario.validate_evidence().await;
    assert_eq!(code, 0, "validate-evidence over the run's tree: {output}");
    scenario.assert_tree(names::BENCHMARK, &view);
}

// ------------------------------------------------------------ 03 failed run

/// Scenario 3: a red run is data. act exits non-zero, the run is `failed`, and the
/// metrics it earned are still written — stamped `failed`, which is how a scanner tells
/// them apart (ADR-0002 §5.1).
#[tokio::test]
async fn scenario_03_a_failing_benchmark_still_writes_evidence() {
    let mut scenario = Scenario::new();
    scenario.boot(
        Stub::new()
            .log("benchmark-failed.jsonl")
            .payload("benchmark")
            .exit(3),
    );
    let head = scenario.repo.head();

    let view = scenario
        .run_ok(args("benchmark", "hyperfine-bench", "benchmark", &head))
        .await;
    let run_id = view["run_id"].as_str().expect("a run_id").to_owned();
    assert_eq!(
        view["status"],
        json!("failed"),
        "a red run is a normal result: {view}"
    );
    assert_eq!(view["exit_code"], json!(3));
    assert_eq!(view["terminal"], json!(true));
    assert!(
        view["warnings"]
            .as_array()
            .expect("warnings")
            .contains(&json!("exit_nonzero")),
        "and says so in the warning vocabulary: {view}"
    );
    assert!(
        view.get("error").is_none(),
        "`error` is for runs that could not be run: {view}"
    );

    let relative = evidence_path("benchmark", &head, &run_id);
    let records = jsonl(&scenario.evidence_file(&relative));
    assert!(!records.is_empty(), "a failed run's numbers are still data");
    for record in &records {
        assert_eq!(record["run_status"], json!("failed"));
    }

    let (code, output) = scenario.validate_evidence().await;
    assert_eq!(code, 0, "a failed run's evidence is still valid: {output}");
    scenario.assert_tree(names::BENCHMARK_FAILED, &view);
}

// ------------------------------------------------------------- 04 cancellation

/// Scenario 4: a run cancelled mid-flight. The payload had already been written — so
/// this is the case ADR-0004 §6.2 is about: the caller asked for the measurement to
/// stop, so nothing is published, the run says `cancelled` with
/// `partial_evidence_dropped`, the run's scratch tree is gone, and no process of the
/// run survives it.
#[tokio::test]
async fn scenario_04_a_cancelled_run_publishes_nothing_and_leaves_no_process() {
    let mut scenario = Scenario::new();
    scenario.boot(
        Stub::new()
            .log("coverage-success.jsonl")
            .payload("coverage")
            .payload_delay("0.3")
            .sleep("60")
            .child_sleep("60"),
    );
    let head = scenario.repo.head();

    let started = scenario
        .run_ok({
            let mut arguments = args("coverage", "coverage", "coverage", &head);
            arguments["wait"] = json!(false);
            arguments
        })
        .await;
    let run_id = started["run_id"].as_str().expect("a run_id").to_owned();
    assert_eq!(started["terminal"], json!(false), "{started}");

    let act_pid = scenario.wait_for_stub_pid(false);
    let child_pid = scenario.wait_for_stub_pid(true);

    // Wait until the fake job has actually dropped its payload: the interesting cancel is
    // the one with something to throw away, not the trivial one with nothing on disk.
    let payload = scenario.incoming(&run_id).join("lcov.info");
    for _ in 0..200 {
        if payload.is_file() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(payload.is_file(), "the stub never wrote its payload");

    let receipt = scenario.cancel_run(&run_id).await;
    assert_eq!(receipt["status"], json!("cancelled"), "{receipt}");
    assert_eq!(
        receipt["kill"]["method"],
        json!("term"),
        "a cooperative act dies on SIGTERM, so the receipt says so: {receipt}"
    );

    let settled = scenario.wait_terminal(&run_id).await;
    assert_eq!(settled["status"], json!("cancelled"), "{settled}");
    assert_eq!(
        settled["evidence_files"].as_array().expect("files").len(),
        0,
        "extraction is skipped for a caller cancel: {settled}"
    );
    assert!(
        settled["warnings"]
            .as_array()
            .expect("warnings")
            .contains(&json!("partial_evidence_dropped")),
        "the discarded measurement is announced, not hidden (§6.2): {settled}"
    );
    assert!(
        !scenario
            .evidence_root()
            .join("coverage")
            .join(&head)
            .exists(),
        "no partial evidence file behind the result's back"
    );
    assert_eq!(
        collect_jsonl(&scenario.evidence_root()).len(),
        0,
        "and no published record file anywhere under the root"
    );

    // What happens to the payload the job had already staged is the one place where
    // ADR-0004's two sentences disagree. §6.2 says a cancelled run's staged payload "is
    // removed with the rest of the run's scratch"; §6.3 says an interrupted run publishes
    // nothing and keeps "the usable ones", which is what `meta.json` does — it lists what
    // was staged. The shipped behaviour follows §6.3 (see the golden: `incoming/` survives,
    // with its file named in the manifest), so the payload is neither published nor deleted.
    // Both sentences cannot hold; one of them needs amending.

    // ADR-0003 §9.1: the run's scratch directory (act's cwd, secrets.env, worktree) is
    // removed on every terminal path, and ADR-0004 §3.4 orders that after the commit.
    assert!(
        !scenario.run_scratch(&run_id).exists(),
        "the run's scratch directory is gone: {}",
        scenario.run_scratch(&run_id).display()
    );

    // The orphan check: act, and the grandchild in its process group, are both reaped.
    assert!(
        wait_for_process_gone(act_pid, Duration::from_secs(5)).await,
        "act ({act_pid}) is still alive after the cancel"
    );
    assert!(
        wait_for_process_gone(child_pid, Duration::from_secs(5)).await,
        "act's child ({child_pid}) outlived its process group"
    );

    // The audit trail survives the cancel, and says `cancelled` with the kill receipt.
    let meta: Value = read_json(&scenario.run_dir(&run_id).join("meta.json"));
    assert_eq!(meta["status"], json!("cancelled"));
    assert!(
        meta["act"]["pid"].is_number(),
        "meta.json names the process that was killed: {meta}"
    );
    assert!(
        ["term", "kill"].contains(&receipt["kill"]["method"].as_str().unwrap_or("")),
        "the cancel receipt says how the group died: {receipt}"
    );

    scenario.assert_tree(
        names::CANCELLED,
        &json!({ "run_id": run_id, "commit": head }),
    );
}

/// Scenario 4b: the same cancel, against an act that ignores SIGTERM — the case the
/// runner's escalation exists for, ending in `kill.method: "kill"`.
///
/// `#[ignore]`d on purpose: `serve` has no `--term-grace-secs` yet, so this scenario can
/// only wait out the shipped 20 s grace, which is more than the suite's budget. The
/// escalation itself is covered at the runner seam (`tests/runner_execution.rs`, which
/// may use the 1 s floor). Run it with:
/// `cargo test --test e2e -- --ignored --nocapture`.
#[tokio::test]
#[ignore = "waits out the shipped 20 s term grace: serve has no --term-grace-secs yet"]
async fn scenario_04b_a_stub_that_ignores_sigterm_is_sigkilled() {
    let mut scenario = Scenario::new();
    scenario.boot(
        Stub::new()
            .log("coverage-success.jsonl")
            .payload("none")
            .sleep("120")
            .ignore_sigterm()
            .child_sleep("120"),
    );
    let head = scenario.repo.head();
    let started = scenario
        .run_ok({
            let mut arguments = args("coverage", "coverage", "coverage", &head);
            arguments["wait"] = json!(false);
            arguments
        })
        .await;
    let run_id = started["run_id"].as_str().expect("a run_id").to_owned();
    let act_pid = scenario.wait_for_stub_pid(false);
    let child_pid = scenario.wait_for_stub_pid(true);

    let receipt = scenario.cancel_run(&run_id).await;
    assert_eq!(receipt["status"], json!("cancelled"), "{receipt}");
    assert_eq!(
        receipt["kill"]["method"],
        json!("kill"),
        "SIGTERM was ignored, so the cancel receipt must say SIGKILL: {receipt}"
    );
    let settled = scenario.wait_terminal(&run_id).await;
    assert_eq!(settled["status"], json!("cancelled"), "{settled}");
    assert!(
        settled["warnings"]
            .as_array()
            .expect("warnings")
            .contains(&json!("process_group_killed")),
        "the run says the group had to be forced: {settled}"
    );
    assert!(
        wait_for_process_gone(act_pid, Duration::from_secs(5)).await,
        "act ({act_pid}) survived the escalation"
    );
    assert!(
        wait_for_process_gone(child_pid, Duration::from_secs(5)).await,
        "the grandchild ({child_pid}) survived the escalation"
    );
    assert!(!scenario.run_scratch(&run_id).exists());
}

/// The record files the *server* published: every `.jsonl` under the root except the
/// audit trail — `runs/<run>/act.jsonl` is act's log and `incoming/` is the job's own
/// payload scratch, and neither is a contract record.
fn collect_jsonl(root: &std::path::Path) -> Vec<PathBuf> {
    published_under(&root.join("coverage"))
        .into_iter()
        .chain(published_under(&root.join("benchmark")))
        .collect()
}

/// Every `.jsonl` below one directory, recursively.
fn published_under(root: &std::path::Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(published_under(&path));
        } else if path.extension().is_some_and(|ext| ext == "jsonl") {
            found.push(path);
        }
    }
    found
}

fn read_json(path: &std::path::Path) -> Value {
    let text = harness::read_to_string(path);
    serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("{} is not JSON: {err}", path.display()))
}

// ------------------------------------------------------------- 05 concurrency

/// Scenario 5: two `run_workflow` calls at once. ADR-0003 §6 serialises a workspace, so
/// exactly one run exists and the other call is refused with `workspace_busy` naming the
/// run that holds the lock — the caller can poll that run instead of guessing.
#[tokio::test]
async fn scenario_05_two_concurrent_calls_answer_with_one_busy_error() {
    let mut scenario = Scenario::new();
    // Long enough that the second call certainly lands while the first is still running.
    scenario.boot(Stub::new().sleep("10").payload("none"));
    let head = scenario.repo.head();
    let first = scenario
        .run_ok({
            let mut arguments = args("coverage", "coverage", "coverage", &head);
            arguments["wait"] = json!(false);
            arguments
        })
        .await;
    let run_id = first["run_id"].as_str().expect("a run_id").to_owned();

    let (code, detail) = scenario
        .run_error(args("coverage", "coverage", "coverage", &head))
        .await;
    assert_eq!(code, "workspace_busy");
    assert_eq!(
        detail["run_id"],
        json!(run_id),
        "the refusal names the run holding the workspace: {detail}"
    );
    assert!(
        detail["phase"].is_string(),
        "and which phase that run is in: {detail}"
    );

    // Nothing was registered for the refused call: exactly one run directory exists.
    let listing = scenario.call("run_status", json!({})).await;
    let runs = listing["result"]["structuredContent"]["runs"]
        .as_array()
        .expect("runs");
    assert_eq!(runs.len(), 1, "the busy call started nothing: {listing}");
    assert_eq!(
        collect_jsonl(&scenario.evidence_root()).len(),
        0,
        "a refused call writes no evidence"
    );

    // Stop the running one so the scenario does not leave a live process behind.
    scenario.cancel_run(&run_id).await;
    scenario.wait_terminal(&run_id).await;
    scenario.assert_tree(
        names::CONCURRENT,
        &json!({ "run_id": run_id, "commit": head }),
    );
}

// ---------------------------------------------------------- 06 category other

/// Scenario 6: `category: "other"` is a legal category with no extractor (ADR-0001
/// §2.4). The run is a normal success, the result says nothing was written and why, and
/// nothing under the evidence root is a record — even though the job dropped a coverage
/// payload on disk. The category decides what is published; the payload does not.
#[tokio::test]
async fn scenario_06_category_other_writes_no_evidence() {
    let mut scenario = Scenario::new();
    scenario.boot(
        Stub::new()
            .log("coverage-success.jsonl")
            .payload("coverage"),
    );
    let head = scenario.repo.head();

    let view = scenario
        .run_ok(args("coverage", "coverage", "other", &head))
        .await;
    let run_id = view["run_id"].as_str().expect("a run_id").to_owned();

    assert_eq!(view["status"], json!("success"), "{view}");
    assert_eq!(
        view["category"],
        json!("other"),
        "the caller's word is echoed"
    );
    assert_eq!(view["evidence_files"], json!([]));
    assert_eq!(view["evidence_record_count"], json!(0));
    assert!(
        view["warnings"]
            .as_array()
            .expect("warnings")
            .contains(&json!("unknown_category")),
        "an empty answer explains itself: {view}"
    );
    assert_eq!(
        collect_jsonl(&scenario.evidence_root()).len(),
        0,
        "no JSONL of any kind: the category, not the payload, picks the extractor"
    );
    assert!(!scenario
        .evidence_file(&evidence_path("coverage", &head, &run_id))
        .exists());

    let (code, output) = scenario.validate_evidence().await;
    assert_eq!(
        code, 0,
        "a tree of audit files is still a valid tree: {output}"
    );
    scenario.assert_tree(names::OTHER, &view);
}

// ------------------------------------------------------------ 07 no payload

/// Scenario 7: the run succeeded but never produced a coverage payload, and the act log
/// has no coverage line to scrape either. That is a *successful* run with a
/// `no_coverage_found` warning and no coverage file (ADR-0002 §5.1: an evidence problem
/// is data, never a red run).
#[tokio::test]
async fn scenario_07_a_missing_coverage_payload_is_a_warning_not_a_failure() {
    let mut scenario = Scenario::new();
    scenario.boot(Stub::new().log("coverage-success.jsonl").payload("none"));
    let head = scenario.repo.head();

    let view = scenario
        .run_ok(args("coverage", "coverage", "coverage", &head))
        .await;
    assert_eq!(view["status"], json!("success"), "{view}");
    assert_eq!(view["evidence_files"], json!([]));
    assert_eq!(view["evidence_record_count"], json!(0));
    assert!(
        view["warnings"]
            .as_array()
            .expect("warnings")
            .contains(&json!("no_coverage_found")),
        "and the result says which of the two empty cases this is: {view}"
    );
    assert_eq!(
        collect_jsonl(&scenario.evidence_root()).len(),
        0,
        "no coverage file was written"
    );

    let (code, output) = scenario.validate_evidence().await;
    assert_eq!(
        code, 0,
        "validate-evidence over an empty-handed run: {output}"
    );
    scenario.assert_tree(names::NO_PAYLOAD, &view);
}

// -------------------------------------------------------------- 08 bad requests

/// Scenario 8: the three ways a call can name something that is not there, each answered
/// with its own ADR-0001 §5.3 code and nothing else — no run registered, no directory
/// created, no act process spawned.
#[tokio::test]
async fn scenario_08_unknown_workflow_task_and_commit_get_their_own_codes() {
    let mut scenario = Scenario::new();
    scenario.boot(Stub::new().payload("none"));
    let head = scenario.repo.head();

    // A workflow that does not exist: the answer carries the list, so the caller
    // self-corrects in one round trip instead of guessing twice.
    let (code, detail) = scenario
        .run_error(args("no-such-workflow", "coverage", "coverage", &head))
        .await;
    assert_eq!(code, "workflow_not_found");
    let available: Vec<&str> = detail["available"]
        .as_array()
        .expect("the refusal lists what is there")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        available.contains(&"coverage") && available.contains(&"multi-job"),
        "the real workflow names are in the refusal: {available:?}"
    );
    assert_eq!(
        available.len(),
        4,
        "and the list is the fixture set, not the fixture set plus TestRepo's seed: {available:?}"
    );

    // A job that workflow does not have.
    let (code, _) = scenario
        .run_error(args("coverage", "no-such-job", "coverage", &head))
        .await;
    assert_eq!(code, "task_not_found");

    // A commit that is not this repository's.
    let (code, _) = scenario
        .run_error(args("coverage", "coverage", "coverage", "deadbee"))
        .await;
    assert_eq!(code, "commit_unknown");

    // A workflow file that exists and is not parseable: the file resolves, so the refusal
    // is `task_not_found` — but its `reason` says which of the two cases it is, and "no
    // such job" would be a misleading answer for "this file is not YAML".
    let (broken_code, broken_detail) = scenario
        .run_error(args("broken", "broken", "coverage", &head))
        .await;
    assert_eq!(broken_code, "task_not_found");
    assert!(
        broken_detail["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("could not be indexed"),
        "the refusal names the parse failure, not a typo: {broken_detail}"
    );

    // And none of those five calls left a trace on disk beyond the pre-created root.
    let listing = scenario.call("run_status", json!({})).await;
    assert_eq!(
        listing["result"]["structuredContent"]["runs"]
            .as_array()
            .expect("runs")
            .len(),
        0,
        "a refused call registers no run: {listing}"
    );
    assert_eq!(collect_jsonl(&scenario.evidence_root()).len(), 0);
    assert!(!scenario.stub_pid_exists(), "and never spawned act");
}

// ---------------------------------------------------------------- 09 istanbul

/// The second coverage source, through the same path: an istanbul `coverage-summary.json`
/// and nothing else. Pinned because §3.2's precedence rule ("one source per run") is
/// only observable if both sources exist somewhere in the suite.
#[tokio::test]
async fn scenario_09_an_istanbul_summary_is_a_coverage_source() {
    let mut scenario = Scenario::new();
    scenario.boot(
        Stub::new()
            .log("coverage-success.jsonl")
            .payload("coverage-summary"),
    );
    let head = scenario.repo.head();

    let view = scenario
        .run_ok(args("coverage", "coverage", "coverage", &head))
        .await;
    let run_id = view["run_id"].as_str().expect("a run_id").to_owned();
    assert_eq!(view["status"], json!("success"), "{view}");
    let records = jsonl(&scenario.evidence_file(&evidence_path("coverage", &head, &run_id)));
    assert!(!records.is_empty());
    for record in &records {
        assert_eq!(record["source"], json!("coverage_summary"));
    }

    let (code, output) = scenario.validate_evidence().await;
    assert_eq!(code, 0, "validate-evidence over an istanbul run: {output}");
    scenario.assert_tree(names::ISTANBUL, &view);
}

// ----------------------------------------------------------------- 10 secrets

/// Scenario 10: a job that prints its own environment. The declared secret values must
/// be nowhere — not in the log on disk, not in the tool result, not in `meta.json`, not
/// in the stub's own argv. Redaction happens at the source (ADR-0001 §2.7), and this is
/// the scenario where the whole pipeline, argv through disk, is checked for it.
#[tokio::test]
async fn scenario_10_a_secret_never_reaches_the_disk_or_the_result() {
    const SECRET: &str = "e2e-super-secret-value-4f3a";
    const TOKEN: &str = "ghp_e2e_also_secret_9b2c";

    let mut scenario = Scenario::new();
    scenario.boot(
        Stub::new()
            .log("coverage-success.jsonl")
            .payload("none")
            .echo_secrets(),
    );
    let head = scenario.repo.head();

    let body = scenario
        .run_workflow({
            let mut arguments = args("coverage", "coverage", "coverage", &head);
            arguments["secrets"] = json!({ "API_TOKEN": SECRET, "GITHUB_TOKEN": TOKEN });
            arguments["log_tail_bytes"] = json!(4096);
            arguments
        })
        .await;
    assert_eq!(body["result"]["isError"], json!(false), "{body}");
    let view = &body["result"]["structuredContent"];
    assert_eq!(view["status"], json!("success"), "{view}");

    // The stub really did print them: the log shows the redaction where they were.
    let log_tail = view["log_tail"].as_str().expect("a log tail");
    assert!(
        log_tail.contains("***REDACTED***"),
        "the secret lines were captured and eaten: {log_tail}"
    );

    // The value appears nowhere: not in the response, not in any byte under the root,
    // not in the argv the stub recorded.
    for secret in [SECRET, TOKEN] {
        assert!(
            !body.to_string().contains(secret),
            "a secret reached the tool result"
        );
        assert!(
            !scenario.stub_argv().join(" ").contains(secret),
            "a secret reached act's argv (it must travel by --secret-file)"
        );
        for (path, bytes) in scenario.evidence_bytes() {
            assert!(
                !bytes
                    .windows(secret.len())
                    .any(|window| window == secret.as_bytes()),
                "a secret was found in {}",
                path
            );
        }
    }
    scenario.assert_tree(names::SECRETS, view);
}

// --------------------------------------------------------------- 11 list_tasks

/// Scenario 11: `list_tasks` over the fixture workflows — including `multi-job.yml`
/// (a needs-chain plus a matrix, which is what the fixture exists for) and `broken.yml`
/// (valid file name, invalid YAML), which is a `parse_error` *inside a successful call*
/// rather than a failed one.
#[tokio::test]
async fn scenario_11_list_tasks_reports_the_fixture_workflows() {
    let mut scenario = Scenario::new();
    scenario.boot(Stub::new().payload("none"));

    let content = scenario.list_tasks(json!({ "include_steps": true })).await;
    let files = content["workflows"].as_array().expect("workflows");
    let by_file: Vec<String> = files
        .iter()
        .filter_map(|entry| entry["file"].as_str().map(str::to_owned))
        .collect();
    for expected in [
        ".github/workflows/benchmark.yml",
        ".github/workflows/coverage.yml",
        ".github/workflows/multi-job.yml",
        ".github/workflows/broken.yml",
    ] {
        assert!(
            by_file.iter().any(|file| file == expected),
            "{expected} is listed: {by_file:?}"
        );
    }

    let broken = files
        .iter()
        .find(|entry| entry["file"].as_str() == Some(".github/workflows/broken.yml"))
        .expect("broken.yml is listed");
    assert!(
        broken.get("parse_error").is_some(),
        "the unparseable file says so, inside a successful call: {broken}"
    );

    let multi = files
        .iter()
        .find(|entry| entry["file"].as_str() == Some(".github/workflows/multi-job.yml"))
        .expect("multi-job.yml is listed");
    let jobs = multi["jobs"].as_array().expect("jobs");
    let bench = jobs
        .iter()
        .find(|job| job["id"].as_str() == Some("matrix-bench"))
        .expect("the matrix job is listed");
    assert_eq!(
        bench["needs"].as_array().expect("needs").len(),
        1,
        "the needs-chain is reported: {bench}"
    );
    let axes: Vec<&str> = bench["matrix_axes"]
        .as_array()
        .expect("the matrix axis names are reported")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        axes.contains(&"profile") && axes.contains(&"os"),
        "both axes of the fixture matrix: {bench}"
    );
    assert!(
        bench["steps"]
            .as_array()
            .is_some_and(|steps| !steps.is_empty()),
        "include_steps came through: {bench}"
    );
}

// ------------------------------------------------------- 12 the stub's own guard

/// Scenario 12: the stub `act` refuses an argv it was not promised.
///
/// This is the acceptance criterion "the stub validates the argv it receives and fails
/// the test on unexpected flags" made testable in isolation: a scenario whose stub died on
/// a bad flag would fail anyway, but only by the accident of a missing evidence file, and
/// a *removed* flag would go unnoticed entirely. Every flag the contract forbids and every
/// flag it requires is checked here directly.
#[test]
fn scenario_12_the_stub_act_refuses_any_argv_it_was_not_promised() {
    let act = harness::testdata().join("bin").join("act");
    let dir = tempfile::tempdir().expect("tempdir for the stub's own checks");
    let secret = dir.path().join("secrets.env");
    std::fs::write(&secret, "GITHUB_TOKEN=x\n").expect("write a secret file");
    let payload = dir.path().join("out");
    std::fs::create_dir_all(&payload).expect("create the payload dir");

    let good = vec![
        act.display().to_string(),
        "--directory".into(),
        dir.path().display().to_string(),
        "--workflows".into(),
        dir.path().join("wf.yml").display().to_string(),
        "--job".into(),
        "coverage".into(),
        "--bind".into(),
        "--reuse=false".into(),
        "--rm".into(),
        "--pull=false".into(),
        "--rebuild".into(),
        "--use-gitignore".into(),
        "--json".into(),
        "--quiet=false".into(),
        "--secret-file".into(),
        secret.display().to_string(),
        "--env".into(),
        format!("ACTMCP_EVIDENCE_OUT={}", payload.display()),
        "--env".into(),
        "ACTMCP_RUN_ID=20261110T004203Z-a1b2c3".into(),
    ];

    // The shape ADR-0003 §7.2 promises is accepted, in every spelling act allows.
    let run = run_command(&good);
    assert_eq!(run.0, 0, "the promised argv is accepted: {}", run.1);
    assert!(
        run_command(&[act.display().to_string(), "--version".into()]).0 == 0,
        "act's own --version is answered"
    );

    // Short forms are the same flags: act's `-j` is its `--job`.
    let mut short_job = good.clone();
    short_job[5] = "-j".into(); // --job
    assert_eq!(run_command(&short_job).0, 0, "act's -j is accepted");

    // A flag the runner must never emit, and one it simply does not know.
    for (what, argv) in [
        ("an unknown flag", {
            let mut argv = good.clone();
            argv.push("--frobnicate".into());
            argv
        }),
        ("a secret in argv", {
            let mut argv = good.clone();
            argv.extend(["--secret".into(), "TOKEN=hunter2".into()]);
            argv
        }),
        ("a pinned daemon socket", {
            let mut argv = good.clone();
            argv.extend(["--container-daemon-socket".into(), "unix:///x".into()]);
            argv
        }),
        ("a relative --directory", {
            let mut argv = good.clone();
            argv[2] = ".".into();
            argv
        }),
        ("no --json", {
            let mut argv = good.clone();
            argv.retain(|token| token != "--json");
            argv
        }),
        ("no --job", {
            let mut argv = good.clone();
            let at = argv
                .iter()
                .position(|token| token == "--job")
                .expect("--job");
            argv.remove(at);
            argv.remove(at);
            argv
        }),
        ("no ACTMCP_EVIDENCE_OUT", {
            let mut argv = good.clone();
            argv.retain(|token| !token.starts_with("ACTMCP_EVIDENCE_OUT="));
            argv
        }),
    ] {
        let (code, output) = run_command(&argv);
        assert_eq!(code, 64, "{what} must be refused (got: {output})");
        assert!(
            output.contains("stub-act:"),
            "{what} is refused with a reason the log will show: {output}"
        );
    }
}

/// Run the stub directly and report `(exit code, combined output)`.
fn run_command(argv: &[String]) -> (i32, String) {
    let mut command = std::process::Command::new(&argv[0]);
    command.args(&argv[1..]);
    let output = command.output().expect("the stub act runs");
    (
        output.status.code().unwrap_or(-1),
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )
}
