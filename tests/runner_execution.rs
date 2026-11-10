//! Behavioural tests for the act runner (mact-f1w.8), run against a stub `act`
//! (`tests/testdata/stub-act.sh`) — no docker, no act install, no network.
//!
//! The ticket's own acceptance list, one test per line:
//!
//! * argv exactness — the stub writes the argv it received and the test compares it with
//!   [`build_argv`] (the golden argv itself is asserted in `src/runner/argv.rs`);
//! * the log tee — `act.jsonl` on disk holds the stub's JSON lines verbatim;
//! * timeout kill (the stub sleeps), including "the stub's process group is gone";
//! * cancellation kill, with the same orphan check;
//! * secret redaction — a fake secret value appears in no file the run produces;
//! * a non-zero exit is `failed` with the real `exit_code`, not an internal error;
//! * the on-disk cap truncates with a marker, and the in-memory tail fits its budget.
//!
//! Timings stay at the contract's floor: `--term-grace-secs` is `1..=240` seconds, so 1 s
//! is the fastest grace a legal server may have, and the tests use it rather than
//! inventing a sub-second escape hatch the wire contract does not allow.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use act_mcp::runner::{
    build_argv, ActRunOutcome, KillMethod, RunId, RunSpec, RunStatus, Runner, RunnerLimits,
    REDACTION_MARKER, WARNING_ORPHANED_CONTAINERS, WARNING_PROCESS_GROUP_KILLED, WARNING_TRUNCATED,
};

const SECRET: &str = "s3cr3t-value-do-not-leak";
const TOKEN: &str = "ghp_also_do_not_leak";
const RUN_ID: &str = "20261110T004203Z-a1b2c3";
const STUB: &str = env!("CARGO_MANIFEST_DIR");

/// One throwaway workspace: a tree, a scratch root, an evidence root.
struct Harness {
    root: PathBuf,
    tree: PathBuf,
    scratch_root: PathBuf,
    evidence: PathBuf,
    _tmp: tempfile::TempDir,
}

impl Harness {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(tmp.path()).expect("canonical tempdir");
        let tree = root.join("tree");
        let scratch_root = root.join("scratch");
        let evidence = tree.join(".evidence");
        std::fs::create_dir_all(tree.join(".github/workflows")).expect("workflows dir");
        std::fs::create_dir_all(&scratch_root).expect("scratch root");
        std::fs::write(
            tree.join(".github/workflows/ci.yml"),
            "name: ci\non:\n  push:\njobs:\n  build:\n    runs-on: ubuntu-latest\n",
        )
        .expect("workflow file");
        Self {
            root,
            tree,
            scratch_root,
            evidence,
            _tmp: tmp,
        }
    }

    /// A spec pointed at this harness, with the stub as `act` and a short timeout.
    fn spec(&self) -> RunSpec {
        let run_id = RunId::parse(RUN_ID).expect("test run id");
        let mut spec = RunSpec::new(
            run_id.clone(),
            stub_act(),
            self.tree.clone(),
            self.tree.join(".github/workflows/ci.yml"),
            "build",
        );
        spec.workspace = self.tree.clone();
        spec.evidence_dir = self.evidence.clone();
        spec.run_dir = self.evidence.join("runs").join(run_id.as_str());
        spec.scratch_dir = self.scratch_root.join(run_id.as_str());
        spec.timeout = Duration::from_secs(5);
        spec
    }

    fn file(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.file(name);
        std::fs::write(&path, contents).expect("write fixture");
        path
    }
}

/// The checked-in stub `act`, made executable in case a checkout lost the bit.
fn stub_act() -> PathBuf {
    let path = PathBuf::from(STUB)
        .join("tests")
        .join("testdata")
        .join("stub-act.sh");
    assert!(path.is_file(), "the stub act is checked in at {path:?}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod stub");
    }
    path
}

/// Legal-but-fast limits: the ADR floor for the grace, the ADR default ring, and a
/// caller-supplied log cap where the test needs one.
fn limits() -> RunnerLimits {
    RunnerLimits::checked(
        Duration::from_secs(1),
        act_mcp::runner::MAX_LOG_TAIL_BYTES,
        act_mcp::runner::DEFAULT_MAX_RUN_LOG_BYTES,
    )
    .expect("legal limits")
}

async fn run(spec: RunSpec) -> ActRunOutcome {
    Runner::new(limits())
        .expect("limits")
        .start(spec)
        .await
        .expect("start")
        .wait()
        .await
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

fn read_bytes(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

/// `haystack` contains `needle` as a byte substring.
fn contains_bytes(haystack: &[u8], needle: &str) -> bool {
    let needle = needle.as_bytes();
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

/// Every regular file under `root`, as (path, bytes).
fn files_under(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    let mut queue = vec![root.to_path_buf()];
    while let Some(dir) = queue.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                queue.push(path);
            } else if meta.is_file() {
                let bytes = std::fs::read(&path).unwrap_or_default();
                out.push((path, bytes));
            }
        }
    }
    out
}

/// Pids in `pgid` that are still running (a zombie is dead-but-unreaped, so it does not
/// count as an orphan: nothing is executing in it).
fn live_pids_in_group(pgid: i32) -> Vec<i32> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_string_lossy().parse::<i32>().ok() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        // `/proc/<pid>/stat` is `pid (comm) state ppid pgrp …`, and comm may contain
        // spaces and parentheses, so split after the *last* ')'.
        let Some((_, rest)) = stat.rsplit_once(')') else {
            continue;
        };
        let fields: Vec<&str> = rest.split_whitespace().collect();
        if fields.len() < 3 {
            continue;
        }
        let state = fields[0];
        let Ok(pgrp) = fields[2].parse::<i32>() else {
            continue;
        };
        if pgrp == pgid && state != "Z" {
            out.push(pid);
        }
    }
    out
}

/// The process group id of `pid`, read from `/proc`.
fn process_group_of(pid: u32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, rest) = stat.rsplit_once(')')?;
    rest.split_whitespace().nth(2)?.parse().ok()
}

/// Poll `predicate` until it holds or the deadline passes.
async fn wait_until(label: &str, deadline: Duration, predicate: impl Fn() -> bool) {
    let started = Instant::now();
    while !predicate() {
        if started.elapsed() > deadline {
            panic!("{label} did not happen within {deadline:?}");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn the_stub_act_receives_exactly_the_argv_build_argv_produced() {
    let harness = Harness::new();
    let mut spec = harness.spec();
    let argv_file = harness.file("argv.txt");
    let cwd_file = harness.file("cwd.txt");
    spec.process_env = BTreeMap::from([
        ("STUB_ARGV_FILE".to_owned(), argv_file.display().to_string()),
        ("STUB_CWD_FILE".to_owned(), cwd_file.display().to_string()),
        (
            "STUB_SECRET_FILE_PROBE".to_owned(),
            spec.secret_file_path().display().to_string(),
        ),
    ]);
    spec.inputs = BTreeMap::from([("fuzz_rounds".to_owned(), "4".to_owned())]);
    spec.secrets = BTreeMap::from([
        ("TOP_SECRET".to_owned(), SECRET.to_owned()),
        ("GITHUB_TOKEN".to_owned(), TOKEN.to_owned()),
    ]);
    let expected = build_argv(&spec);

    let outcome = run(spec).await;
    assert_eq!(outcome.status, RunStatus::Success, "{outcome:?}");

    let received = read(&argv_file);
    let received: Vec<String> = received.lines().map(str::to_owned).collect();
    // The file records "$@", i.e. argv[1..].
    assert_eq!(
        received,
        expected[1..],
        "act got a different argv than we built"
    );

    // The secret file reached act (by path), and carried the caller's keys plus the
    // always-present GITHUB_TOKEN (ADR-0003 §7.7) — key names only, never values.
    let log = read(&outcome.log_path);
    assert!(
        log.contains("secret file keys: GITHUB_TOKEN,TOP_SECRET,"),
        "secret file contents were not what ADR-0003 §7.7 pins: {log}"
    );

    // ADR-0003 §7.6: act's cwd is the per-run scratch dir, never the workspace.
    assert_eq!(
        read(&cwd_file).trim(),
        harness.scratch_root.join(RUN_ID).display().to_string()
    );
}

#[tokio::test]
async fn act_jsonl_on_disk_is_the_stubs_log_verbatim() {
    let harness = Harness::new();
    let stdout = concat!(
        "{\"level\":\"info\",\"msg\":\"Starting job build\"}\n",
        "{\"level\":\"info\",\"msg\":\"step 1/2 hello\"}\n",
        "{\"level\":\"info\",\"msg\":\"Job build succeeded\"}\n",
    );
    let stderr = "{\"level\":\"warning\",\"msg\":\"a warning on stderr\"}\n";
    let lines_file = harness.write("lines.jsonl", stdout);
    let stderr_file = harness.write("stderr.jsonl", stderr);

    let mut spec = harness.spec();
    spec.process_env = BTreeMap::from([
        (
            "STUB_LINES_FILE".to_owned(),
            lines_file.display().to_string(),
        ),
        (
            "STUB_STDERR_FILE".to_owned(),
            stderr_file.display().to_string(),
        ),
    ]);

    let outcome = run(spec).await;
    assert_eq!(outcome.status, RunStatus::Success);
    assert!(!outcome.log_truncated);
    assert_eq!(outcome.log_dropped_lines, 0);

    let on_disk = read(&outcome.log_path);
    for line in stdout.lines() {
        assert!(on_disk.contains(line), "{line} missing from act.jsonl");
    }
    // Both pipes are teed: act's JSON stream has been observed on either fd across
    // versions, so the runner captures both into the one file ADR-0002 §1.1 names.
    for line in stderr.lines() {
        assert!(on_disk.contains(line), "{line} missing from act.jsonl");
    }
    assert_eq!(outcome.log_bytes, (stdout.len() + stderr.len()) as u64);
    assert_eq!(outcome.log_stdout_bytes, stdout.len() as u64);
    assert_eq!(outcome.log_stderr_bytes, stderr.len() as u64);
    assert_eq!(
        outcome.log_path,
        harness.evidence.join("runs").join(RUN_ID).join("act.jsonl")
    );

    // The tail holds the whole log here (it fits the ring). Which pipe arrives last is not
    // defined — they are two independent pipes — so the tail must end with one of the two
    // final lines, and must contain all five.
    assert!(outcome.log_tail.len() <= act_mcp::runner::MAX_LOG_TAIL_BYTES);
    assert!(
        outcome.log_tail.ends_with("Job build succeeded\"}\n")
            || outcome.log_tail.ends_with("a warning on stderr\"}\n"),
        "unexpected tail: {:?}",
        outcome.log_tail
    );
    assert_eq!(
        outcome.log_tail.lines().count(),
        stdout.lines().count() + stderr.lines().count()
    );
}

#[tokio::test]
async fn a_partial_last_line_is_still_captured() {
    let harness = Harness::new();
    let mut spec = harness.spec();
    spec.process_env = BTreeMap::from([(
        "STUB_NO_NEWLINE".to_owned(),
        "no trailing newline".to_owned(),
    )]);
    let outcome = run(spec).await;
    let on_disk = read(&outcome.log_path);
    assert!(on_disk.ends_with("no trailing newline"));
    assert!(outcome.log_tail.contains("no trailing newline"));
}

#[tokio::test]
async fn a_nonzero_exit_is_a_failed_run_with_the_real_code() {
    let harness = Harness::new();
    let mut spec = harness.spec();
    spec.process_env = BTreeMap::from([
        ("STUB_EXIT_CODE".to_owned(), "7".to_owned()),
        (
            "STUB_LINES_FILE".to_owned(),
            harness
                .write(
                    "lines.jsonl",
                    "{\"level\":\"info\",\"msg\":\"job failed\"}\n",
                )
                .display()
                .to_string(),
        ),
    ]);

    let outcome = run(spec).await;
    assert_eq!(outcome.status, RunStatus::Failed);
    assert_eq!(outcome.exit_code, Some(7));
    assert!(
        outcome.failure.is_none(),
        "a failed workflow is not a runner error"
    );
    assert!(outcome.kill.is_none());
    assert!(outcome.initiator.is_none());
    assert!(outcome.log_tail.contains("job failed"));
    // duration_ms is measured from act's spawn (ADR-0004 §4.4).
    assert!(outcome.duration_ms < 30_000, "{outcome:?}");
    assert!(outcome.finished_at >= outcome.started_at);
}

#[tokio::test]
async fn an_act_killed_by_a_signal_we_did_not_send_is_error_act_crashed() {
    let harness = Harness::new();
    let mut spec = harness.spec();
    spec.process_env = BTreeMap::from([("STUB_KILL_SELF".to_owned(), "TERM".to_owned())]);

    let outcome = run(spec).await;
    assert_eq!(outcome.status, RunStatus::Error);
    let failure = outcome
        .failure
        .clone()
        .expect("an error outcome carries a failure");
    assert_eq!(failure.code(), "act_crashed");
    assert_eq!(
        outcome.exit_code, None,
        "a signalled process has no exit code"
    );
    assert!(outcome.initiator.is_none(), "nobody terminated this run");
    assert!(failure.message().contains("SIGTERM"), "{failure:?}");
}

#[tokio::test]
async fn a_secret_value_appears_in_no_file_the_run_produces() {
    let harness = Harness::new();
    let mut spec = harness.spec();
    spec.secrets = BTreeMap::from([
        ("TOP_SECRET".to_owned(), SECRET.to_owned()),
        ("GITHUB_TOKEN".to_owned(), TOKEN.to_owned()),
    ]);
    spec.process_env = BTreeMap::from([
        // The stub prints the secret the way a job echo would; the runner must redact it
        // before it reaches either copy of the log.
        ("STUB_ECHO_SECRET".to_owned(), "LEAK_ME".to_owned()),
        ("LEAK_ME".to_owned(), SECRET.to_owned()),
        (
            "STUB_ARGV_FILE".to_owned(),
            harness.file("argv.txt").display().to_string(),
        ),
    ]);
    spec.dry_run = true;

    let runner = Runner::new(limits()).expect("limits");
    let spec_scratch = spec.scratch_dir.clone();
    let run = runner.start(spec).await.expect("start");
    let outcome = run.wait().await;
    assert_eq!(outcome.status, RunStatus::Success);

    // 1. every file the run produced, scanned for the value.
    let mut scanned = 0;
    for root in [&harness.evidence, &harness.scratch_root, &harness.root] {
        for (path, bytes) in files_under(root) {
            scanned += 1;
            assert!(
                !contains_bytes(&bytes, SECRET),
                "secret leaked into {}",
                path.display()
            );
            assert!(
                !contains_bytes(&bytes, TOKEN),
                "GITHUB_TOKEN leaked into {}",
                path.display()
            );
        }
    }
    assert!(scanned > 3, "the scan found nothing to scan ({scanned})");

    // 2. the outcome's tail, which is what `log_tail` becomes on the wire.
    assert!(!outcome.log_tail.contains(SECRET));
    assert!(!outcome.log_tail.contains(TOKEN));
    assert!(
        outcome.log_tail.contains(REDACTION_MARKER),
        "the log line that echoed the secret should show the marker: {:?}",
        outcome.log_tail
    );
    assert!(
        !outcome
            .log_tail
            .contains(&harness.tree.display().to_string()),
        "log_tail must carry <WORKSPACE>, not the host path (ADR-0001 §3.4)"
    );

    // 3. argv never had a secret either (the file the stub wrote is one of the files
    //    scanned above, but say it out loud).
    assert!(!read(&harness.file("argv.txt")).contains(SECRET));

    // 4. the secret file existed during the run and is gone after it (ADR-0003 §9.1).
    assert!(
        !spec_scratch.join("secrets.env").exists(),
        "the secret file outlived the run"
    );
    // The inert project-config file stays for the supervisor's cleanup, and is empty.
    assert_eq!(
        read_bytes(&spec_scratch.join("inert.conf")),
        Vec::<u8>::new(),
        "inert.conf must exist and be 0 bytes (ADR-0003 §7.7)"
    );
}

#[tokio::test]
async fn a_timeout_kills_the_process_group_and_labels_the_run_timed_out() {
    let harness = Harness::new();
    let mut spec = harness.spec();
    spec.timeout = Duration::from_millis(400);
    spec.process_env = BTreeMap::from([
        ("STUB_SLEEP".to_owned(), "30".to_owned()),
        ("STUB_CHILD_SECS".to_owned(), "30".to_owned()),
    ]);

    let started = Instant::now();
    let runner = Runner::new(limits()).expect("limits");
    let run = runner.start(spec).await.expect("start");
    let handle = run.handle();
    let pid = run.pid();
    let pgid = run.pgid();

    // Inside the run: act leads its own process group (that is what makes `killpg` reach
    // act *and* the grandchild `sleep`), and it is still alive.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        process_group_of(pid),
        Some(pgid),
        "act must be its own process-group leader (ADR-0004 §5.1)"
    );
    assert!(handle.is_running());
    assert!(
        !live_pids_in_group(pgid).is_empty(),
        "the grandchild is missing"
    );

    let outcome = run.wait().await;
    let elapsed = started.elapsed();
    assert_eq!(outcome.status, RunStatus::TimedOut);
    assert_eq!(
        outcome.initiator.map(|initiator| initiator.as_str()),
        Some("deadline")
    );
    let kill = outcome.kill.expect("a timed-out run has a kill report");
    assert_eq!(kill.method, KillMethod::Term, "act honoured SIGTERM");
    assert!(kill.took_ms.unwrap() < 2_000, "took_ms {kill:?}");
    assert!(
        elapsed < Duration::from_secs(5),
        "the run overshot its budget by {elapsed:?} (grace is 1 s)"
    );
    assert!(!outcome.has_warning(WARNING_PROCESS_GROUP_KILLED));

    // The whole group is gone: no orphaned grandchild survived the killpg.
    wait_until("the process group is gone", Duration::from_secs(5), || {
        live_pids_in_group(pgid).is_empty()
    })
    .await;
}

#[tokio::test]
async fn cancellation_ends_the_run_as_cancelled_and_leaves_no_orphan() {
    let harness = Harness::new();
    let mut spec = harness.spec();
    spec.process_env = BTreeMap::from([
        ("STUB_SLEEP".to_owned(), "30".to_owned()),
        ("STUB_CHILD_SECS".to_owned(), "30".to_owned()),
    ]);
    let token = spec.cancellation.clone();

    let runner = Runner::new(limits()).expect("limits");
    let run = runner.start(spec).await.expect("start");
    let pgid = run.pgid();

    tokio::time::sleep(Duration::from_millis(200)).await;
    // This is what `cancel_run` does: signal through the handle, then read the receipt.
    let kill = run
        .handle()
        .terminate(act_mcp::runner::TerminateInitiator::Caller)
        .await;
    assert_eq!(kill.method, KillMethod::Term);

    let outcome = run.wait().await;
    assert_eq!(outcome.status, RunStatus::Cancelled);
    assert_eq!(
        outcome.initiator.map(|initiator| initiator.as_str()),
        Some("caller")
    );
    assert_eq!(
        outcome.kill.map(|report| report.method),
        Some(KillMethod::Term)
    );
    assert!(outcome.exit_code.is_none(), "SIGTERM leaves no exit code");
    assert!(outcome.failure.is_none(), "a cancel is not an error");
    assert!(!outcome.has_warning(WARNING_ORPHANED_CONTAINERS));

    wait_until("the process group is gone", Duration::from_secs(5), || {
        live_pids_in_group(pgid).is_empty()
    })
    .await;

    // And the cancellation token path labels the run the same way.
    let mut spec = harness.spec();
    spec.process_env = BTreeMap::from([("STUB_SLEEP".to_owned(), "30".to_owned())]);
    let token2 = spec.cancellation.clone();
    let run = Runner::new(limits())
        .expect("limits")
        .start(spec)
        .await
        .expect("start");
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        token2.cancel();
    });
    let outcome = run.wait().await;
    assert_eq!(outcome.status, RunStatus::Cancelled);
    assert_eq!(
        outcome.kill.map(|report| report.method),
        Some(KillMethod::Term)
    );

    // The token we grabbed above is unused now; dropping it must be harmless.
    drop(token);
}

#[tokio::test]
async fn an_act_that_ignores_sigterm_is_sigkilled_after_the_grace() {
    let harness = Harness::new();
    let mut spec = harness.spec();
    spec.timeout = Duration::from_millis(300);
    spec.process_env = BTreeMap::from([
        ("STUB_SLEEP".to_owned(), "60".to_owned()),
        ("STUB_IGNORE_TERM".to_owned(), "1".to_owned()),
        ("STUB_CHILD_SECS".to_owned(), "60".to_owned()),
    ]);

    let started = Instant::now();
    let outcome = run(spec).await;
    let elapsed = started.elapsed();
    assert_eq!(outcome.status, RunStatus::TimedOut);
    let kill = outcome.kill.expect("kill report");
    assert_eq!(kill.method, KillMethod::Kill);
    assert!(
        kill.took_ms.unwrap_or_default() >= 1_000,
        "SIGKILL came before the grace expired: {kill:?}"
    );
    assert!(elapsed < Duration::from_secs(8), "{elapsed:?}");
    assert!(outcome.has_warning(WARNING_PROCESS_GROUP_KILLED));
    assert!(
        outcome.has_warning(WARNING_ORPHANED_CONTAINERS),
        "a SIGKILL teardown must warn about containers (ADR-0004 §5.5)"
    );
}

#[tokio::test]
async fn terminate_is_idempotent_and_the_first_initiator_wins() {
    let harness = Harness::new();
    let mut spec = harness.spec();
    spec.timeout = Duration::from_secs(30);
    spec.process_env = BTreeMap::from([("STUB_SLEEP".to_owned(), "30".to_owned())]);

    let runner = Runner::new(limits()).expect("limits");
    let run = runner.start(spec).await.expect("start");
    let handle = run.handle();

    let (first, second) = tokio::join!(
        handle.terminate(act_mcp::runner::TerminateInitiator::Caller),
        handle.terminate(act_mcp::runner::TerminateInitiator::Deadline),
    );
    assert_eq!(first.method, KillMethod::Term);
    assert_eq!(
        second, first,
        "a second terminate gets the first caller's receipt, not a second teardown"
    );

    let outcome = run.wait().await;
    assert_eq!(
        outcome.initiator.map(|initiator| initiator.as_str()),
        Some("caller"),
        "the first initiator to signal owns the label (ADR-0004 §3.5)"
    );

    // After act is reaped, a cancel must not invent a second teardown and must not
    // relabel the run. ADR-0004 §5.1: `method` is never absent (and never `none`) when a
    // process existed, so a late caller gets the *first* teardown's receipt — and the
    // supervisor, per §5.2, does not even ask: a terminal run answers `already_terminal`
    // with no `kill` at all.
    let late = handle
        .terminate(act_mcp::runner::TerminateInitiator::Deadline)
        .await;
    assert_eq!(
        late, first,
        "a late terminate repeats the first receipt, it does not signal again"
    );
    assert_eq!(
        handle.initiator().map(|initiator| initiator.as_str()),
        Some("caller"),
        "a late cancel must not relabel the run"
    );
}

#[test]
fn a_cancel_for_a_run_that_never_spawned_reports_none() {
    // The shape of ADR-0003 §12.2 T5's receipt: no process, so nothing was signalled.
    // The supervisor makes the decision to skip the spawn; the runner's side is that a
    // handle with no process group says `method: none` rather than inventing a kill.
    let report = act_mcp::runner::KillReport::none();
    assert_eq!(report.method, KillMethod::None);
    assert_eq!(
        report.to_value(),
        serde_json::json!({ "method": "none" }),
        "`took_ms` is absent for method none ($defs/cancel_run_result.kill)"
    );
}

#[tokio::test]
async fn the_on_disk_cap_truncates_with_a_marker_and_the_tail_stays_in_budget() {
    let harness = Harness::new();
    let mut spec = harness.spec();
    spec.process_env = BTreeMap::from([("STUB_REPEAT".to_owned(), "2000".to_owned())]);

    // The smallest legal on-disk cap (ADR-0004 §11: --max-run-log-bytes ≥ 1 MiB) and a
    // tail ring well below it.
    let tight = RunnerLimits::checked(
        Duration::from_secs(1),
        4096,
        act_mcp::runner::MIN_RUN_LOG_BYTES,
    )
    .expect("legal limits");
    let outcome = Runner::new(tight)
        .expect("runner")
        .start(spec)
        .await
        .expect("start")
        .wait()
        .await;

    assert_eq!(outcome.status, RunStatus::Success, "{outcome:?}");
    assert!(outcome.log_truncated);
    assert!(outcome.has_warning(WARNING_TRUNCATED));
    assert!(outcome.log_dropped_lines > 0);
    assert!(outcome.log_bytes <= act_mcp::runner::MIN_RUN_LOG_BYTES);

    let on_disk = read_bytes(&outcome.log_path);
    assert!(on_disk.len() as u64 <= act_mcp::runner::MIN_RUN_LOG_BYTES + 256);
    let last = on_disk
        .split(|byte| *byte == b'\n')
        .rfind(|line| !line.is_empty())
        .expect("at least one line");
    let marker: serde_json::Value = serde_json::from_slice(last).unwrap_or_else(|error| {
        panic!(
            "the last line of act.jsonl must be the marker ({error}); it was {:?}",
            String::from_utf8_lossy(last)
        )
    });
    assert_eq!(marker["act_mcp_truncated"], true);
    assert_eq!(marker["reason"], "max_run_log_bytes");
    assert_eq!(marker["limit_bytes"], act_mcp::runner::MIN_RUN_LOG_BYTES);

    // The in-memory tail is bounded by its own budget, holds whole lines, and ends where
    // the log ended (the last line *written*, not the last line act printed).
    assert!(outcome.log_tail.len() <= 4096);
    assert!(outcome.log_tail.starts_with("{\"level\""));
    assert!(outcome.log_tail.ends_with("}\n"));
    assert_eq!(
        outcome.log_tail.matches('\n').count(),
        outcome.log_tail.matches("{\"level\"").count()
    );
}

#[tokio::test]
async fn the_tail_budget_holds_whole_lines_from_the_end() {
    let harness = Harness::new();
    let mut lines = String::new();
    for index in 0..500 {
        lines.push_str(&format!(
            "{{\"level\":\"info\",\"msg\":\"line {index}\"}}\n"
        ));
    }
    let lines_file = harness.write("many.jsonl", &lines);
    let mut spec = harness.spec();
    spec.process_env = BTreeMap::from([(
        "STUB_LINES_FILE".to_owned(),
        lines_file.display().to_string(),
    )]);
    let outcome = run(spec).await;
    assert_eq!(outcome.status, RunStatus::Success);
    assert_eq!(
        outcome.log_tail.lines().count(),
        500,
        "the whole log fits the ring"
    );

    // Every budget that can hold at least one whole line must come back as whole lines.
    // The budget has to fit the *last* line before the whole-line rule can hold at all:
    // the tail grows from the end, and the first candidate is the final line.
    let last_line = outcome.log_tail.lines().last().expect("lines").len() + 1;
    assert_eq!(
        outcome.log_tail_within(0),
        "",
        "`log_tail_bytes: 0` means no tail"
    );
    let available = outcome.log_tail.len();
    for requested in [last_line, last_line + 1, 64, 1000, 4096, 65536] {
        let tail = outcome.log_tail_within(requested);
        if requested >= available {
            assert_eq!(
                tail, outcome.log_tail,
                "{requested}: a budget bigger than the log returns the log"
            );
            continue;
        }
        assert!(tail.len() <= requested, "{requested}: {} bytes", tail.len());
        assert!(
            tail.starts_with("{\"level\""),
            "{requested}: starts mid-line"
        );
        assert!(tail.ends_with('\n'), "{requested}: ends mid-line");
        // Not stingier than it has to be: cutting whole lines can cost at most one line,
        // and a line here is far smaller than half the budget.
        assert!(
            tail.len() > requested / 2,
            "{requested}: only {} bytes returned",
            tail.len()
        );
    }

    // Below one whole line the whole-line rule is impossible, and the runner returns a
    // byte window rather than nothing: a fragment of the last line still says the run was
    // alive, which is the difference between `log_tail: ""` and a diagnosable failure.
    let tiny = outcome.log_tail_within(1);
    assert_eq!(tiny.len(), 1);
    let small = outcome.log_tail_within(8);
    assert_eq!(
        small.len(),
        8,
        "a byte window, because no whole line fits in 8 bytes"
    );
    assert!(
        outcome.log_tail.ends_with(&small),
        "and it is the end of the log"
    );
}

#[tokio::test]
async fn a_dryrun_run_passes_dryrun_and_is_reported_back() {
    let harness = Harness::new();
    let mut spec = harness.spec();
    spec.dry_run = true;
    let argv_file = harness.file("dryrun-argv.txt");
    spec.process_env =
        BTreeMap::from([("STUB_ARGV_FILE".to_owned(), argv_file.display().to_string())]);

    let argv = build_argv(&spec);
    let outcome = run(spec).await;
    assert_eq!(outcome.status, RunStatus::Success);
    assert!(outcome.dry_run);
    assert!(argv.contains(&"--dryrun".to_owned()));
    assert!(read(&argv_file).lines().any(|token| token == "--dryrun"));
}

#[tokio::test]
async fn an_act_binary_that_cannot_be_spawned_leaves_no_secret_file_behind() {
    // ADR-0004 §3.3 T8 is `act_spawn_failed`: the supervisor decides the status, but the
    // runner must fail cleanly and must not leak the secret file it wrote a moment
    // earlier — this is the case where nobody is left holding the cleanup path.
    let harness = Harness::new();
    let mut spec = harness.spec();
    spec.secrets = BTreeMap::from([("TOP_SECRET".to_owned(), SECRET.to_owned())]);
    spec.act_binary = PathBuf::from("/nonexistent/act-do-not-exist");
    let scratch = spec.scratch_dir.clone();
    let error = Runner::new(limits())
        .expect("limits")
        .start(spec)
        .await
        .err()
        .expect("the binary does not exist");
    let text = format!("{error:#}");
    assert_eq!(error.code(), "act_spawn_failed", "{text}");
    assert!(text.contains("could not be spawned"), "{text}");
    assert!(
        !text.contains(SECRET),
        "the spawn error must not carry the secret"
    );
    assert!(
        !scratch.join("secrets.env").exists(),
        "a failed spawn left the secret file on disk"
    );
}

#[tokio::test]
async fn a_workspace_that_declines_to_bind_warns_instead_of_failing() {
    // `--act-no-bind` (ADR-0003 §7.7): the run is still a run, with a warning.
    let harness = Harness::new();
    let mut spec = harness.spec();
    spec.bind = false;
    let argv = build_argv(&spec);
    let outcome = run(spec).await;
    assert_eq!(outcome.status, RunStatus::Success);
    assert!(outcome
        .warnings
        .contains(&act_mcp::runner::WARNING_PAYLOAD_TRANSPORT_UNAVAILABLE.to_owned()));
    assert!(!argv.contains(&"--bind".to_owned()));
}
