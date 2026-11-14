//! The run supervisor's contract (mact-f1w.11, ADR-0004 §13).
//!
//! Everything here runs against the checked-in stub `act` — no docker, no containers, no
//! network — and against a temp workspace per test. The assertions are the ticket's
//! acceptance criteria, each in one named test:
//!
//! * a run outlives the request future that started it (ADR-0004 §4, the headline rule);
//! * the workspace lock is single-flight, busy is immediate, and a lock left by a dead pid is
//!   broken with a warning (ADR-0003 §6.5);
//! * `cancel_run` stops a running process group within the grace, is idempotent on a terminal
//!   run, dequeues a queued one, and does not error on any of them;
//! * a terminal run releases the workspace, so the next run starts ("restart after terminal");
//! * in-memory state is bounded by count and age while `.evidence/runs/` is kept;
//! * shutdown takes every in-flight run down and leaves no stub-act process behind.

use std::{
    io::{self},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use act_mcp::{
    registry::{RunIdentity, RunPlan, RunPolicy, RunSupervisor, RunView, WireStatus},
    runner::{process, RunId, RunSpec},
};
use serde_json::Value;

/// The stub `act`, made executable in case a checkout dropped the bit.
fn stub_act() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
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

/// One throwaway workspace + scratch root, and the runs started in it.
///
/// `RunSpec` requires the scratch dir to live *outside* the workspace (ADR-0003 F8: `--bind`
/// makes everything inside the workspace readable by the job, and the job reads secrets that
/// way), so the layout is `tmp/workspace` + `tmp/scratch`.
struct Harness {
    root: PathBuf,
    workspace: PathBuf,
    scratch_root: PathBuf,
    _tmp: tempfile::TempDir,
}

const COMMIT: &str = "2f7c1a4b9e0d3c6a5b8f1e2d4c6a8b0f3e5d7c91";

impl Harness {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(tmp.path()).expect("canonical tempdir");
        let workspace = root.join("workspace");
        let scratch_root = root.join("scratch");
        std::fs::create_dir_all(workspace.join(".github/workflows")).expect("workflows dir");
        std::fs::write(
            workspace.join(".github/workflows/ci.yml"),
            "name: ci\non:\n  push:\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n",
        )
        .expect("workflow file");
        std::fs::create_dir_all(&scratch_root).expect("scratch root");
        Self {
            root,
            workspace,
            scratch_root,
            _tmp: tmp,
        }
    }

    /// A second workspace, for the tests that need two workspaces to be independent.
    fn second_workspace(&self) -> PathBuf {
        let other = self.root.join("other");
        std::fs::create_dir_all(other.join(".github/workflows")).expect("workflows dir");
        std::fs::write(
            other.join(".github/workflows/ci.yml"),
            "name: ci\non:\n  push:\njobs:\n  build:\n    runs-on: ubuntu-latest\n",
        )
        .expect("workflow file");
        other
    }

    fn write(&self, path: &Path, contents: &str) -> PathBuf {
        std::fs::write(path, contents).expect("write fixture file");
        path.to_path_buf()
    }

    /// A plan for `run_id`, pointed at this workspace, with the stub as act.
    ///
    /// `env` is the stub's seam (`STUB_SLEEP`, `STUB_EXIT_CODE`, `STUB_LINES_FILE`, …), which
    /// is exactly how `tests/runner_execution.rs` drives the same script.
    fn plan(&self, run_id: &str, env: &[(&str, String)]) -> RunPlan {
        let id = RunId::parse(run_id).expect("test run id");
        let mut spec = RunSpec::new(
            id.clone(),
            stub_act(),
            self.workspace.clone(),
            self.workspace.join(".github/workflows/ci.yml"),
            "build",
        );
        spec.workspace = self.workspace.clone();
        spec.evidence_dir = self.workspace.join(".evidence");
        spec.run_dir = spec.evidence_dir.join("runs").join(id.as_str());
        spec.scratch_dir = self.scratch_root.join(id.as_str());
        spec.payload_dir = spec.evidence_dir.join("incoming").join(id.as_str());
        // Two seconds keeps a test that wants "still running" short and a test that wants a
        // timeout shorter, and every timeout in the policy is smaller than the act budget.
        spec.timeout = Duration::from_secs(30);
        for (key, value) in env {
            spec.process_env.insert((*key).to_owned(), value.clone());
        }
        RunPlan::new(
            RunIdentity::new("ci", ".github/workflows/ci.yml", "build", "other", COMMIT),
            spec,
        )
    }

    fn run_dir(&self, run_id: &str) -> PathBuf {
        self.workspace.join(".evidence").join("runs").join(run_id)
    }
}

/// A policy with the ADR defaults except the four knobs a test cannot wait seconds for.
fn policy() -> RunPolicy {
    RunPolicy {
        prestart_timeout: Duration::from_secs(20),
        harvest_timeout: Duration::from_secs(20),
        term_grace: Duration::from_secs(1),
        cancel_wait: Duration::from_secs(5),
        shutdown_grace: Duration::from_secs(10),
        ..RunPolicy::default()
    }
}

fn supervisor() -> RunSupervisor {
    RunSupervisor::with_policy(policy()).expect("test policy is legal")
}

/// A fresh id, from the same generator production uses (they sort chronologically, which the
/// retention and listing assertions lean on).
fn new_id() -> String {
    RunId::generate().as_str().to_owned()
}

/// Install a subscriber honouring `RUST_LOG`, for the tests whose failure is only legible
/// with the run task's own log lines. `try_init` means the first test to ask wins, which is
/// fine: they all ask for the same thing.
fn init_logging() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
}

/// How much log tail the polling helpers ask for (the wire maximum, §7.1).
const LOG_TAIL: usize = 4096;

/// Poll `run_status` until `check` is satisfied, and return the view that satisfied it.
async fn wait_for_view(
    supervisor: &RunSupervisor,
    run_id: &str,
    what: &str,
    check: impl Fn(&RunView) -> bool,
) -> RunView {
    for _ in 0..500 {
        let view = supervisor
            .status(run_id, LOG_TAIL)
            .await
            .unwrap_or_else(|error| panic!("run {run_id} should be known: {}", error.run_id));
        if check(&view) {
            return view;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("run {run_id} never satisfied {what}");
}

async fn wait_until_running(supervisor: &RunSupervisor, run_id: &str) -> RunView {
    wait_for_view(supervisor, run_id, "status running", |view| {
        view.status == WireStatus::Running
    })
    .await
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// The rule the whole ticket exists for: the run belongs to the supervisor, not the request.
// ─────────────────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_run_keeps_going_after_the_request_that_started_it_is_dropped() {
    let harness = Harness::new();
    let supervisor = supervisor();
    let run_id = new_id();
    // `STUB_LINES_FILE` survives the act process, so the log proves the run finished even
    // though nobody was left to wait for it.
    let log_marker = harness.write(&harness.root.join("finished.marker"), "act finished\n");
    let plan = harness.plan(
        &run_id,
        &[
            ("STUB_SLEEP", "1".to_owned()),
            ("STUB_LINES_FILE", log_marker.display().to_string()),
        ],
    );

    // A `wait: true` request, killed mid-flight: the closest test stand-in for an HTTP client
    // that vanished (ADR-0004 §5.3: transport death removes a waiter and nothing else).
    let ticket = supervisor.start(plan.clone()).await.expect("accepted");
    let waiter = tokio::spawn(async move { ticket.wait().await });
    // Give it a moment to spawn act, then kill the request that was waiting on it.
    let view = wait_until_running(&supervisor, &run_id).await;
    assert_eq!(view.status, WireStatus::Running);
    waiter.abort();
    drop(waiter);

    // Nothing about the run changed: it reaches its terminal commit on its own…
    let done = wait_for_view(&supervisor, &run_id, "terminal", |view| {
        view.status == WireStatus::Success
    })
    .await;
    assert_eq!(done.phase.as_str(), "done");
    // …and the §3.4 guarantee holds: terminal means the manifest is already on disk.
    assert!(
        harness.run_dir(&run_id).join("meta.json").is_file(),
        "a terminal run must have published its meta.json before anyone could observe it"
    );
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(harness.run_dir(&run_id).join("meta.json")).unwrap())
            .expect("meta.json is JSON");
    assert_eq!(manifest["status"], "success");
    assert!(
        std::fs::read_to_string(&log_marker).is_ok(),
        "the stub's own log file proves act ran to completion after the caller vanished"
    );
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// Single-flight: one run per workspace, answered immediately, no queue (ADR-0004 §8).
// ─────────────────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_second_run_in_a_locked_workspace_is_busy_immediately() {
    let harness = Harness::new();
    let supervisor = supervisor();
    let first = new_id();
    let ticket = supervisor
        .start(harness.plan(&first, &[("STUB_SLEEP", "5".to_owned())]))
        .await
        .expect("first run registers");

    let second = new_id();
    let error = supervisor
        .start(harness.plan(&second, &[]))
        .await
        .expect_err("the workspace is held");
    let (code, detail) = (error.code(), error.detail());
    assert_eq!(code.0, "workspace_busy");
    assert!(code.1, "workspace_busy is retryable");
    assert_eq!(detail["run_id"], first, "the busy answer names the holder");
    assert!(
        detail.get("phase").is_some(),
        "and its phase, so the caller knows whether waiting is plausible: {detail:?}"
    );
    assert!(
        !detail.contains_key("position"),
        "there is no queue, so there is no queue position either: {detail:?}"
    );
    // "immediately" is the contract (ADR-0003 §6.5): no wait, no queue.
    drop(ticket);
    supervisor.shutdown().await;
}

#[tokio::test]
async fn a_run_in_another_workspace_is_not_blocked_by_the_first() {
    let harness = Harness::new();
    let supervisor = supervisor();
    let first = new_id();
    supervisor
        .start(harness.plan(&first, &[("STUB_SLEEP", "5".to_owned())]))
        .await
        .expect("first workspace");

    let other = harness.second_workspace();
    let second = new_id();
    let mut plan = harness.plan(&second, &[("STUB_SLEEP", "5".to_owned())]);
    plan.spec.workspace = other.clone();
    plan.spec.tree = other.clone();
    plan.spec.workflow_file = other.join(".github/workflows/ci.yml");
    plan.spec.evidence_dir = other.join(".evidence");
    plan.spec.run_dir = plan.spec.evidence_dir.join("runs").join(&second);
    supervisor
        .start(plan)
        .await
        .expect("a different workspace has its own single-flight slot");
    assert_eq!(supervisor.active_count().await, 2);
    supervisor.shutdown().await;
}

#[tokio::test]
async fn the_server_wide_cap_is_too_many_runs_rather_than_a_wait() {
    let harness = Harness::new();
    let mut limited = policy();
    limited.max_concurrent_runs = 1;
    let supervisor = RunSupervisor::with_policy(limited).expect("policy");
    let first = new_id();
    supervisor
        .start(harness.plan(&first, &[("STUB_SLEEP", "5".to_owned())]))
        .await
        .expect("first run");

    let other = harness.second_workspace();
    let second = new_id();
    let mut plan = harness.plan(&second, &[]);
    plan.spec.workspace = other.clone();
    plan.spec.tree = other.clone();
    plan.spec.workflow_file = other.join(".github/workflows/ci.yml");
    plan.spec.evidence_dir = other.join(".evidence");
    plan.spec.run_dir = plan.spec.evidence_dir.join("runs").join(&second);
    let error = supervisor.start(plan).await.expect_err("over the cap");
    assert_eq!(error.code().0, "too_many_runs");
    let detail = error.detail();
    assert_eq!(detail["limit"], 1);
    assert_eq!(detail["active"], 1);
    supervisor.shutdown().await;
}

#[tokio::test]
async fn a_terminal_run_releases_the_workspace_for_the_next_one() {
    let harness = Harness::new();
    let supervisor = supervisor();
    let first = new_id();
    let ticket = supervisor
        .start(harness.plan(&first, &[("STUB_EXIT_CODE", "0".to_owned())]))
        .await
        .expect("first run");
    let _ = ticket.wait().await;
    // §3.4 step 6: the lock is released as part of the terminal commit, so the very next call
    // in the same workspace is accepted.
    let second = new_id();
    let again = supervisor
        .start(harness.plan(&second, &[("STUB_EXIT_CODE", "0".to_owned())]))
        .await
        .expect("the workspace is free again after a terminal run");
    let _ = again.wait().await;
    assert_eq!(supervisor.active_count().await, 0);
}

#[tokio::test(flavor = "current_thread")]
async fn a_lock_left_by_a_dead_process_is_broken_with_a_warning() {
    // ADR-0003 §6.5: `flock` dies with its holder, so a crash cannot wedge a workspace. What
    // survives is the lock *body*, and the acquirer says so out loud rather than silently.
    // Single-threaded, so the scoped subscriber below sees the log line.
    let harness = Harness::new();
    let supervisor = supervisor();
    let dead = spawn_and_reap();
    let body = format!(
        "{{\"run_id\":\"20261110T000000Z-deadbe\",\"phase\":\"running\",\"pid\":{dead},\
         \"hostname\":\"{}\"}}\n",
        act_mcp::evidence::writer::hostname()
    );
    std::fs::write(harness.workspace.join(".act-mcp.lock"), body).expect("plant a stale lock body");

    let captured = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(captured.clone())
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .finish();
    // A thread-scoped subscriber (the runtime is single-threaded here, so the run's
    // registration — and its warning — happen on this thread).
    let _guard = tracing::subscriber::set_default(subscriber);

    let run_id = new_id();
    let ticket = supervisor
        .start(harness.plan(&run_id, &[("STUB_EXIT_CODE", "0".to_owned())]))
        .await
        .expect("a dead holder does not block a new run");
    let _ = ticket.wait().await;
    let log = captured.text();
    assert!(
        log.contains("stale workspace lock"),
        "the warning that a dead holder's lock was broken is the whole point: {log}"
    );
}

/// Spawn a child and reap it, returning a pid that is now definitely dead.
fn spawn_and_reap() -> u32 {
    let mut child = std::process::Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .spawn()
        .expect("spawn a child");
    let pid = child.id();
    child.wait().expect("reap it");
    std::thread::sleep(Duration::from_millis(20));
    assert!(
        !act_mcp::registry::process_is_alive(pid),
        "the pid must be dead"
    );
    pid
}

/// A `MakeWriter` that appends to a shared buffer, for asserting on log lines.
#[derive(Debug, Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("lock")).to_string()
    }
}

impl io::Write for Capture {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(buffer);
        Ok(buffer.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Capture;

    fn make_writer(&self) -> Self::Writer {
        self.clone()
    }
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// Cancellation (ADR-0004 §5).
// ─────────────────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn cancelling_a_running_run_kills_the_process_group_and_reports_it() {
    init_logging();
    let harness = Harness::new();
    let supervisor = supervisor();
    let run_id = new_id();
    // A grandchild in the same group, like act's docker client: the kill must take the group,
    // not just the direct child (ADR-0003 §6.4).
    let ticket = supervisor
        .start(harness.plan(
            &run_id,
            &[
                ("STUB_SLEEP", "60".to_owned()),
                ("STUB_CHILD_SECS", "60".to_owned()),
            ],
        ))
        .await
        .expect("accepted");
    wait_until_running(&supervisor, &run_id).await;
    let pgid = supervisor
        .record(&run_id)
        .await
        .expect("record")
        .pgid
        .expect("act spawned, so there is a pgid");
    assert!(
        process::group_is_live(pgid),
        "the stub and its child are running before the cancel"
    );

    let receipt = supervisor.cancel(&run_id).await.expect("cancel");
    assert_eq!(receipt.status.as_str(), "cancelled");
    assert!(
        receipt.terminal,
        "the teardown finished inside --cancel-wait-secs"
    );
    assert!(!receipt.already_terminal);
    assert!(receipt.cancel_requested_at.is_some());
    let kill = receipt
        .kill
        .expect("a cancelled run always carries a kill receipt");
    assert_eq!(kill.method.as_str(), "term", "SIGTERM was enough: {kill:?}");
    assert!(
        kill.took_ms.is_some(),
        "the receipt says how long the group took: {kill:?}"
    );

    // No orphaned stub-act: the group is gone, and the record says so too.
    assert!(!process::group_is_live(pgid), "the whole group is gone");
    let record = supervisor.record(&run_id).await.expect("record");
    assert_eq!(record.state.status().as_str(), "cancelled");
    assert_eq!(record.initiator.map(|i| i.as_str()), Some("caller"));
    let _ = ticket;

    // A terminal cancel of the same run is a report, not an error, and not a second kill.
    let again = supervisor.cancel(&run_id).await.expect("idempotent");
    assert!(again.already_terminal);
    assert_eq!(again.status.as_str(), "cancelled");
    assert!(again.kill.is_none(), "nothing was killed the second time");
    assert!(
        again.warnings.iter().any(|w| w == "already_terminal"),
        "{again:?}"
    );
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(harness.run_dir(&run_id).join("meta.json")).unwrap())
            .expect("meta.json parses");
    assert_eq!(manifest["status"], "cancelled");
}

#[tokio::test]
async fn a_run_that_ignores_sigterm_is_escalated_to_sigkill() {
    init_logging();
    let harness = Harness::new();
    let supervisor = supervisor();
    let run_id = new_id();
    supervisor
        .start(harness.plan(
            &run_id,
            &[
                ("STUB_SLEEP", "60".to_owned()),
                ("STUB_CHILD_SECS", "120".to_owned()),
                ("STUB_IGNORE_TERM", "1".to_owned()),
            ],
        ))
        .await
        .expect("accepted");
    wait_until_running(&supervisor, &run_id).await;
    let pgid = supervisor
        .record(&run_id)
        .await
        .expect("record")
        .pgid
        .expect("pgid");

    let receipt = supervisor.cancel(&run_id).await.expect("cancel");
    let kill = receipt.kill.expect("kill receipt");
    assert_eq!(
        kill.method.as_str(),
        "kill",
        "a group that ignored SIGTERM within --term-grace-secs (1s here) gets SIGKILL"
    );
    assert!(!process::group_is_live(pgid));
    assert_eq!(receipt.status.as_str(), "cancelled");
}

#[tokio::test]
async fn cancelling_an_unknown_run_is_run_not_found_and_names_the_known_ones() {
    let harness = Harness::new();
    let supervisor = supervisor();
    let run_id = new_id();
    let _ticket = supervisor
        .start(harness.plan(&run_id, &[("STUB_SLEEP", "5".to_owned())]))
        .await
        .expect("accepted");

    let error = supervisor
        .cancel("20200101T000000Z-zzzzzz")
        .await
        .expect_err("never heard of it");
    assert_eq!(error.run_id, "20200101T000000Z-zzzzzz");
    let detail = error.detail();
    assert_eq!(
        detail["known_runs"].as_array().expect("known_runs").len(),
        1,
        "the payload names the runs that do exist, so a typo is recoverable"
    );
    assert_eq!(detail["known_runs"][0], run_id);
    supervisor.shutdown().await;
}

#[tokio::test]
async fn a_cancel_races_a_run_that_finished_on_its_own_without_double_killing() {
    let harness = Harness::new();
    let supervisor = supervisor();
    let run_id = new_id();
    supervisor
        .start(harness.plan(
            &run_id,
            &[
                ("STUB_SLEEP", "0.3".to_owned()),
                ("STUB_CHILD_SECS", "0.3".to_owned()),
            ],
        ))
        .await
        .expect("accepted");
    // Ask for the cancel while it runs, then look at what the run ended as: either answer is
    // legal, "error" is not, and neither may leave the group alive.
    wait_until_running(&supervisor, &run_id).await;
    let receipt = supervisor.cancel(&run_id).await.expect("cancel");
    assert!(
        matches!(receipt.status.as_str(), "cancelled" | "success"),
        "{receipt:?}"
    );
    let done = wait_for_view(&supervisor, &run_id, "terminal", |view| {
        view.status != WireStatus::Running
    })
    .await;
    assert!(done.status != WireStatus::Running);
    supervisor.shutdown().await;
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// Polling (ADR-0004 §7).
// ─────────────────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn run_status_reads_phase_and_log_tail_out_of_memory() {
    let harness = Harness::new();
    let supervisor = supervisor();
    let run_id = new_id();
    let lines = harness.write(
        &harness.root.join("lines.txt"),
        "{\"level\":\"info\",\"msg\":\"phase-one-marker\"}\n",
    );
    let ticket = supervisor
        .start(harness.plan(
            &run_id,
            &[
                ("STUB_SLEEP", "5".to_owned()),
                ("STUB_LINES_FILE", lines.display().to_string()),
            ],
        ))
        .await
        .expect("accepted");
    let running = wait_until_running(&supervisor, &run_id).await;
    assert_eq!(running.phase.as_str(), "running");
    assert_eq!(running.identity.workflow, "ci");
    assert_eq!(running.identity.task, "build");
    assert!(
        running.log_tail.contains("phase-one-marker"),
        "the tail comes from the runner's ring, so a poll never reads act.jsonl: {:?}",
        running.log_tail
    );
    supervisor.shutdown().await;
    let _ = ticket;
}

#[tokio::test]
async fn an_unknown_run_is_run_not_found_but_an_empty_listing_is_a_success() {
    let supervisor = supervisor();
    let error = supervisor
        .status("20200101T000000Z-zzzzzz", 0)
        .await
        .expect_err("nothing is registered");
    assert_eq!(error.run_id, "20200101T000000Z-zzzzzz");
    assert!(error.detail()["known_runs"].as_array().unwrap().is_empty());

    // §17.3: "no runs yet" and "every run was lost with the last restart" are the same,
    // successful, empty answer.
    let listing = supervisor.listing(10).await;
    assert!(listing.runs.is_empty());
    assert_eq!(listing.active_count, 0);
    assert!(!listing.truncated);
}

#[tokio::test]
async fn a_listing_is_newest_first_and_says_when_it_truncated() {
    let harness = Harness::new();
    let supervisor = supervisor();
    let mut ids = Vec::new();
    for _ in 0..3 {
        let run_id = new_id();
        ids.push(run_id.clone());
        let ticket = supervisor
            .start(harness.plan(&run_id, &[("STUB_EXIT_CODE", "0".to_owned())]))
            .await
            .expect("accepted");
        let _ = ticket.wait().await;
        // ids are time-ordered; make the order strict even inside one millisecond.
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let listing = supervisor.listing(10).await;
    assert_eq!(listing.runs.len(), 3);
    assert_eq!(
        listing
            .runs
            .iter()
            .map(|run| run.run_id.as_str())
            .collect::<Vec<_>>(),
        vec![ids[2].as_str(), ids[1].as_str(), ids[0].as_str()],
        "newest first"
    );
    assert!(!listing.truncated);
    assert_eq!(listing.active_count, 0);

    let page = supervisor.listing(2).await;
    assert_eq!(page.runs.len(), 2);
    assert!(page.truncated, "a page that dropped older runs says so");
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// Retention (ADR-0004 §7.3): memory is bounded, disk is not.
// ─────────────────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn retention_evicts_old_finished_runs_and_keeps_their_directories() {
    let mut short = policy();
    // Below the documented `--run-retention-mins` floor on purpose: the floor is a flag-level
    // bound, and the eviction rule under test is the same at any age.
    short.retention_age = Duration::from_millis(150);
    let supervisor = RunSupervisor::with_policy(short).expect("policy");
    let harness = Harness::new();
    let run_id = new_id();
    let ticket = supervisor
        .start(harness.plan(&run_id, &[("STUB_EXIT_CODE", "0".to_owned())]))
        .await
        .expect("accepted");
    let _ = ticket.wait().await;

    tokio::time::sleep(Duration::from_millis(250)).await;
    let evicted = supervisor.prune().await;
    assert_eq!(evicted, vec![run_id.clone()]);
    supervisor
        .status(&run_id, 0)
        .await
        .expect_err("the in-memory state is gone");
    assert!(
        harness.run_dir(&run_id).join("meta.json").is_file(),
        "ADR-0004 §7.3: memory goes, .evidence/runs/ stays — that is where the numbers live now"
    );

    // A run that is still going is never evicted, however old it is.
    let live = new_id();
    let _ticket = supervisor
        .start(harness.plan(&live, &[("STUB_SLEEP", "5".to_owned())]))
        .await
        .expect("accepted");
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(
        supervisor.prune().await.is_empty(),
        "a non-terminal run is never evicted"
    );
    assert_eq!(
        supervisor
            .status(&live, 0)
            .await
            .expect("still known")
            .status,
        WireStatus::Running
    );
    supervisor.shutdown().await;
}

#[tokio::test]
async fn the_count_limit_evicts_oldest_finished_first() {
    let mut capped = policy();
    capped.retention_age = Duration::from_secs(3600);
    // `LISTING_CAP` (50) is the floor the policy validator enforces.
    capped.retention_count = 50;
    let supervisor = RunSupervisor::with_policy(capped).expect("policy");
    let harness = Harness::new();
    let floor = 50_usize;
    let mut ids = Vec::new();
    for index in 0..(floor + 3) {
        let run_id = new_id();
        ids.push(run_id.clone());
        let ticket = supervisor
            .start(harness.plan(&run_id, &[("STUB_EXIT_CODE", "0".to_owned())]))
            .await
            .expect("accepted");
        let _ = ticket.wait().await;
        if index == 0 {
            // Give the first run a `finished_at` strictly older than the rest.
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    let listing = supervisor.listing(floor + 10).await;
    assert_eq!(
        listing.runs.len(),
        floor,
        "exactly the retention count is kept in memory"
    );
    for evicted in &ids[..3] {
        supervisor
            .status(evicted, 0)
            .await
            .expect_err("the three oldest finished runs are the ones that went");
    }
    for kept in &ids[3..] {
        supervisor.status(kept, 0).await.expect("newer runs stay");
    }
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// Shutdown (ADR-0004 §9.2).
// ─────────────────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn shutdown_takes_every_in_flight_run_down_and_leaves_no_process_behind() {
    let harness = Harness::new();
    let supervisor = supervisor();
    let mut started = Vec::new();
    for index in 0..2 {
        let run_id = new_id();
        let workspace = if index == 0 {
            harness.workspace.clone()
        } else {
            harness.second_workspace()
        };
        let mut plan = harness.plan(
            &run_id,
            &[
                ("STUB_SLEEP", "300".to_owned()),
                ("STUB_CHILD_SECS", "300".to_owned()),
            ],
        );
        plan.spec.workspace = workspace.clone();
        plan.spec.tree = workspace.clone();
        plan.spec.evidence_dir = workspace.join(".evidence");
        plan.spec.run_dir = plan.spec.evidence_dir.join("runs").join(&run_id);
        plan.spec.workflow_file = workspace.join(".github/workflows/ci.yml");
        let run_dir = plan.spec.run_dir.clone();
        supervisor.start(plan).await.expect("accepted");
        wait_until_running(&supervisor, &run_id).await;
        let pgid = supervisor
            .record(&run_id)
            .await
            .expect("record")
            .pgid
            .expect("pgid");
        started.push((run_id, pgid, run_dir));
    }
    assert_eq!(supervisor.active_count().await, 2);

    // §9.2: refuse new runs, terminate all in parallel, wait the grace for the commits.
    supervisor.begin_shutdown();
    assert!(supervisor.is_shutting_down());
    let refused = supervisor
        .start(harness.plan(&new_id(), &[]))
        .await
        .expect_err("a shutting-down server refuses new runs");
    // ADR-0004 §9.2: the closed error set has no `shutting_down` code, so this is
    // `internal_error` + `retryable: true` + `detail.reason: "shutting_down"`.
    assert_eq!(refused.code().0, "internal_error");
    assert!(
        refused.code().1,
        "and it is retryable, because a restart fixes it"
    );
    assert_eq!(refused.detail()["reason"], "shutting_down");

    let started_at = std::time::Instant::now();
    supervisor.drain().await;
    // Two runs, one grace period: the teardowns run concurrently, not serially.
    assert!(
        started_at.elapsed() < Duration::from_secs(6),
        "shutdown waited {}s for two runs with a 1s term grace",
        started_at.elapsed().as_secs_f32()
    );

    for (run_id, pgid, run_dir) in started {
        let record = supervisor.record(&run_id).await.expect("record");
        assert!(
            record.state.is_terminal(),
            "run {run_id} must be terminal by the end of the window"
        );
        // ADR-0004 T13: a run the shutdown cut short is `error` +
        // `runner_internal_error`/`detail.reason: "server_shutdown"`, **not** `cancelled` —
        // nobody asked for this run to stop, the server had to stop.
        assert_eq!(record.state.status().as_str(), "error");
        let error = record.error.clone().expect("T13 error payload");
        assert_eq!(error.code, "runner_internal_error");
        assert_eq!(error.detail["reason"], "server_shutdown");
        assert_eq!(
            record.initiator.map(|i| i.as_str()),
            Some("shutdown"),
            "a shutdown kill is not reported as a caller cancel"
        );
        assert!(
            !process::group_is_live(pgid),
            "no orphaned stub-act for {run_id}"
        );
        assert!(
            run_dir.join("meta.json").is_file(),
            "a terminal run wrote its manifest, shutdown included"
        );
    }
    supervisor.shutdown().await;
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// The `Pipeline` seam (ADR-0004 §4.4/§4.5, ADR-0003 §5): the pre-start budget, a tree that
// cannot be made, and the cancel that lands before act exists. mact-f1w.14 fills this seam
// in for real; the supervisor must already behave when it stalls or fails.
// ─────────────────────────────────────────────────────────────────────────────────────────

use act_mcp::registry::{Pipeline, PipelineError, RunContext};

/// The pin of a `Pin<Box<dyn Future>>` spelled out, because the alias the trait uses is
/// private to the module that declares it.
type Pinned<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// A pipeline whose `prepare` either stalls (to put a run in `preparing_tree`) or fails.
#[derive(Debug, Clone)]
struct Seam {
    stall: Option<Duration>,
    fail: Option<String>,
}

impl Seam {
    fn stalling_for(secs: f64) -> Self {
        Self {
            stall: Some(Duration::from_secs_f64(secs)),
            fail: None,
        }
    }
    fn refusing(reason: &str) -> Self {
        Self {
            stall: None,
            fail: Some(reason.to_owned()),
        }
    }
}

impl Pipeline for Seam {
    fn prepare(&self, _context: &RunContext) -> Pinned<'_, Result<(), PipelineError>> {
        let stall = self.stall;
        let fail = self.fail.clone();
        Box::pin(async move {
            if let Some(how_long) = stall {
                tokio::time::sleep(how_long).await;
            }
            match fail {
                Some(reason) => Err(PipelineError::Tree(reason)),
                None => Ok(()),
            }
        })
    }
}

#[tokio::test]
async fn a_cancel_during_the_tree_step_reports_kill_none_and_writes_a_manifest() {
    // The acceptance bullet "cancelling a queued run dequeues it": no process ever existed,
    // so the receipt is `kill: {method: "none"}` — present, not absent (§16.2) — and §3.4
    // still requires a `meta.json`.
    let harness = Harness::new();
    let supervisor = RunSupervisor::build(policy(), Arc::new(Seam::stalling_for(2.0)))
        .expect("supervisor with the test pipeline");
    let run_id = new_id();
    supervisor
        .start(harness.plan(&run_id, &[]))
        .await
        .expect("accepted");
    wait_for_view(&supervisor, &run_id, "preparing_tree", |view| {
        view.phase.as_str() == "preparing_tree"
    })
    .await;

    let receipt = supervisor.cancel(&run_id).await.expect("cancel");
    assert_eq!(receipt.status.as_str(), "cancelled");
    assert!(
        receipt.terminal,
        "the run task noticed the cancel when prepare returned"
    );
    let kill = receipt.kill.expect("kill receipt");
    assert_eq!(
        kill.method.as_str(),
        "none",
        "nothing was ever signalled: {kill:?}"
    );
    assert_eq!(
        kill.took_ms, None,
        "there is no teardown duration to report for a run with no process: {kill:?}"
    );

    let record = supervisor.record(&run_id).await.expect("record");
    assert!(record.pgid.is_none(), "no act process was ever spawned");
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(harness.run_dir(&run_id).join("meta.json")).unwrap())
            .expect("meta.json");
    assert_eq!(manifest["status"], "cancelled");
    assert!(
        manifest.get("act").is_none(),
        "a run that never spawned act must not claim an act section: {manifest}"
    );
}

#[tokio::test]
async fn a_tree_step_that_outlives_the_prestart_budget_is_an_error_not_a_wedge() {
    let harness = Harness::new();
    let mut tight = policy();
    tight.prestart_timeout = Duration::from_millis(300);
    let supervisor = RunSupervisor::build(tight, Arc::new(Seam::stalling_for(30.0)))
        .expect("supervisor with the test pipeline");
    let run_id = new_id();
    let ticket = supervisor
        .start(harness.plan(&run_id, &[]))
        .await
        .expect("accepted");

    let record = ticket.wait().await;
    assert_eq!(record.state.status().as_str(), "error");
    let error = record.error.clone().expect("error payload");
    assert_eq!(error.code, "runner_internal_error");
    assert_eq!(error.detail["stage"], "preparing_tree");
    assert!(
        error.retryable,
        "a slow tree step may well succeed on a retry"
    );
    assert!(record.pgid.is_none());

    // And the workspace came back: the budget expiring is a terminal commit like any other,
    // so step 6 released the lock.
    let second = new_id();
    let _again = supervisor
        .start(harness.plan(&second, &[("STUB_EXIT_CODE", "0".to_owned())]))
        .await
        .expect("the workspace is free after a pre-start failure");
    supervisor.shutdown().await;
}

#[tokio::test]
async fn a_tree_that_cannot_be_made_fails_the_run_before_act() {
    let harness = Harness::new();
    let supervisor = RunSupervisor::build(
        policy(),
        Arc::new(Seam::refusing("worktree add: no such ref")),
    )
    .expect("supervisor with the test pipeline");
    let run_id = new_id();
    let ticket = supervisor
        .start(harness.plan(&run_id, &[]))
        .await
        .expect("accepted");

    let record = ticket.wait().await;
    assert_eq!(record.state.status().as_str(), "error");
    let error = record.error.clone().expect("error payload");
    assert_eq!(error.code, "runner_internal_error");
    assert_eq!(error.detail["stage"], "preparing_tree");
    assert!(
        record
            .view(0)
            .warnings
            .iter()
            .all(|warning| warning != "process_group_killed"),
        "nothing was killed, so nothing may say it was: {:?}",
        record.view(0).warnings
    );
    assert!(harness.run_dir(&run_id).join("meta.json").is_file());
    assert_eq!(
        supervisor.active_count().await,
        0,
        "the run is terminal, not wedged"
    );
    assert!(
        supervisor.status(&run_id, 0).await.is_ok(),
        "and it stays pollable, so the caller can read what failed"
    );
}

/// The manifests this ticket writes are the durable half of the contract, so they are checked
/// against the pinned schema — the same validator `act-mcp validate-evidence` uses, not a
/// hand-written lookalike. This is the check that catches a manifest whose `status` is a live
/// state word (ADR-0002 §1.3 forbids one, and no in-memory assertion can see it).
#[tokio::test]
async fn every_terminal_manifest_validates_against_the_pinned_schema() {
    use act_mcp::evidence::schema::{run_meta_validator, validate_against};
    let harness = Harness::new();
    let supervisor = supervisor();

    // `success`: act exited 0.
    let success = new_id();
    let ticket = supervisor
        .start(harness.plan(&success, &[("STUB_EXIT_CODE", "0".to_owned())]))
        .await
        .expect("accepted");
    let _ = ticket.wait().await;

    // `cancelled`: act was taken down mid-flight.
    let cancelled = new_id();
    supervisor
        .start(harness.plan(&cancelled, &[("STUB_SLEEP", "60".to_owned())]))
        .await
        .expect("accepted");
    wait_until_running(&supervisor, &cancelled).await;
    let _ = supervisor.cancel(&cancelled).await.expect("cancel");

    // `error`: the pre-start budget expired, so act never existed.
    let mut tight = policy();
    tight.prestart_timeout = Duration::from_millis(300);
    let stalled =
        RunSupervisor::build(tight, Arc::new(Seam::stalling_for(30.0))).expect("pipeline");
    let errored = new_id();
    let other = harness.second_workspace();
    let mut plan = harness.plan(&errored, &[]);
    plan.spec.workspace = other.clone();
    plan.spec.tree = other.clone();
    plan.spec.workflow_file = other.join(".github/workflows/ci.yml");
    plan.spec.evidence_dir = other.join(".evidence");
    plan.spec.run_dir = plan.spec.evidence_dir.join("runs").join(&errored);
    let ticket = stalled.start(plan).await.expect("accepted");
    let _ = ticket.wait().await;

    let validator = run_meta_validator().expect("the pinned run-meta schema compiles");
    let cases = [
        (success.clone(), harness.run_dir(&success)),
        (cancelled.clone(), harness.run_dir(&cancelled)),
        (
            errored.clone(),
            other.join(".evidence").join("runs").join(&errored),
        ),
    ];
    for (run_id, run_dir) in cases {
        let path = run_dir.join("meta.json");
        let text = std::fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!(
                "{run_id} must have a meta.json at {}: {error}",
                path.display()
            )
        });
        let document: Value = serde_json::from_str(&text).expect("meta.json is JSON");
        validate_against(validator, &document).unwrap_or_else(|problem| {
            panic!("{run_id}'s meta.json is not schema-legal: {problem}\n{text}")
        });
        assert_eq!(
            document["run_id"], run_id,
            "a manifest must describe its own run"
        );
        assert_eq!(document["commit"], COMMIT);
        assert!(
            ["success", "cancelled", "error"].contains(&document["status"].as_str().unwrap()),
            "{text}"
        );
    }
    stalled.shutdown().await;
    supervisor.shutdown().await;
}

// A warning the gate knew *before the run existed* has to survive registration: ADR-0003 §5.4
// promises a worktree run "carries the warning `isolation_worktree`", and nothing downstream
// can re-derive it — by harvest time the tree is just a directory again, and the label check
// belongs to a validation pass that has already finished.
#[tokio::test]
async fn a_run_carries_the_warning_codes_the_gate_handed_it() {
    let harness = Harness::new();
    let supervisor = supervisor();
    let run_id = new_id();
    supervisor
        .start(harness.plan(&run_id, &[]).carrying(&[
            "isolation_worktree".to_owned(),
            "act_runs_on_unmapped".to_owned(),
        ]))
        .await
        .expect("accepted");
    let view = wait_for_view(&supervisor, &run_id, "terminal", |view| {
        view.status != WireStatus::Running
    })
    .await;
    for code in ["isolation_worktree", "act_runs_on_unmapped"] {
        assert!(
            view.warnings.iter().any(|held| held == code),
            "{code} should be on the run: {:?}",
            view.warnings
        );
    }
    supervisor.shutdown().await;

    // And on disk, where a run that predates a restart is read from (ADR-0002 §7).
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(harness.run_dir(&run_id).join("meta.json")).unwrap())
            .expect("meta.json parses");
    let warnings = manifest["warnings"].as_array().expect("meta.json warnings");
    for code in ["isolation_worktree", "act_runs_on_unmapped"] {
        assert!(
            warnings.iter().any(|held| held == code),
            "{code} should be in the manifest: {manifest}"
        );
    }
}
