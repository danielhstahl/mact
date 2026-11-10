//! `run_workflow` end to end (mact-f1w.14's acceptance).
//!
//! These are the tests that prove the ticket's title, in order: validate → isolate commit →
//! run act → classify category → write `.evidence` → structured result. Everything upstream of
//! this file already has its own tests (the validator's in `tests/validation.rs`, the
//! supervisor's in `tests/run_supervisor.rs`, the runner's in `tests/runner_execution.rs`, the
//! extractors' in `tests/coverage_extraction.rs` / `benchmark_extraction.rs`); what nothing but
//! these tests can prove is the *seam*:
//!
//! * a real `tools/call` through the real transport starts a real run of a real `act` stub, and
//!   the result is the ADR-0001 §3 `run_view` — resolved identity, not the caller's abbreviation;
//! * the evidence file is on disk, at the ADR-0002 path, and parseable by the time the tool
//!   result is returned (ADR-0004 §3.4's ordering, made observable);
//! * the category decides what is written, and an unknown category says so instead of faking it;
//! * commit isolation really moves `act`'s `--directory` into a scratch worktree and cleans it
//!   up, without touching the operator's checkout;
//! * failed, timed-out and cancelled runs come back as `isError: false` results, and only
//!   precondition failures are `isError: true` — with the ADR-0001 §5.3 code that says which;
//! * `idempotency_key` replays one run rather than starting a second one, and refuses to let a
//!   key mean two different requests.
//!
//! ## Why the container runtime is faked and `act` is not
//!
//! The [`Environment`] double answers `docker info` and reports an `act --version`, because
//! validation runs those probes and CI here has no daemon; every other step is the production
//! one. `act` itself is a `/bin/sh` script written per test (the same trick as
//! `tests/testdata/stub-act.sh`, inlined so each test's stub states only what that test cares
//! about) and it is spawned by the real runner, in its own process group, with the real ADR-0003
//! §7.2 argv. So `--directory`, `--job`, `--secret-file`, `--env ACTMCP_EVIDENCE_OUT=…` and the
//! exit status are all real observations rather than mocks of themselves.

mod common;

use std::{
    io::Write as _,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use act_mcp::{
    registry::{RunPolicy, RunSupervisor},
    tools::run_workflow::{pipeline::EvidencePipeline, Wiring},
    workspace::{
        CommandError, CommandOutput, ContainerProbeError, ContainerRuntime, Environment,
        IsolationPolicy, ProgramError, SystemEnvironment, ValidationPolicy, Validator,
    },
    Config,
};
use serde_json::{json, Value};
use tempfile::TempDir;

use common::{canonical, meta_2026_07_28, post_jsonrpc, raw_client, TestRepo, TestServer};

/// The checked-in lcov fixture the stub "produces" from inside the job.
const LCOV_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/coverage/lcov.info"
);
/// The checked-in benchmark payload the stub also "produces", so the category and not the
/// accident of which file exists decides what gets published.
const BENCHMARK_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/benchmark/benchmark.json"
);

/// The tool's name, so no test spells it slightly differently.
const TOOL: &str = "run_workflow";

/// How long a test waits for a `wait: false` run to reach its terminal commit.
const TERMINAL_POLL: Duration = Duration::from_millis(50);
const TERMINAL_BUDGET: Duration = Duration::from_secs(30);

// ------------------------------------------------------------------ fixture

/// A git workspace, an `act` stub, an evidence root and a scratch root, wired into the
/// process-wide [`Wiring`] the server actually uses.
///
/// The wiring is built once per fixture and shared, because that is what production does and
/// what several of these tests depend on: an `idempotency_key` and `workspace_busy` are only
/// meaningful if the two calls in the test are looking at one supervisor and one table.
struct Fixture {
    /// Held, never read: dropping it deletes the fixture out from under the server.
    _tmp: TempDir,
    repo: TestRepo,
    evidence: PathBuf,
    act: PathBuf,
    argv: PathBuf,
    cwd: PathBuf,
    config: Arc<Config>,
    wiring: Arc<Wiring>,
}

impl Fixture {
    /// The default: `hybrid` isolation, so a run of `HEAD` executes in place (ADR-0003 §4.1)
    /// and a test that wants the worktree path says so with [`Fixture::worktree`].
    fn new() -> Self {
        Self::with_isolation(IsolationPolicy::Hybrid)
    }

    fn head_only() -> Self {
        Self::with_isolation(IsolationPolicy::HeadOnly)
    }

    fn worktree() -> Self {
        Self::with_isolation(IsolationPolicy::Worktree)
    }

    fn with_isolation(isolation: IsolationPolicy) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir for the run_workflow fixture");
        let root = tmp.path().to_path_buf();
        let repo = TestRepo::init(&root.join("workspace"));
        let evidence = root.join("evidence");
        let scratch = root.join("scratch");
        std::fs::create_dir_all(&evidence).expect("create the evidence root");
        let act = root.join("bin").join("act");
        std::fs::create_dir_all(act.parent().expect("act has a parent")).expect("create bin");

        let mut config =
            Config::with_hardened_defaults(canonical(repo.path()), evidence.clone(), act.clone());
        config.isolation = isolation;
        config.run_scratch = scratch.clone();
        // The shortest grace ADR-0004 §11 allows: a cancelled or stubborn run escalates to
        // SIGKILL after a second rather than the shipped thirty, which is the difference between
        // a test and a wait. The supervisor is built with the *real* pipeline on purpose —
        // `Wiring::with_supervisor` around a default supervisor would carry no extractors at all
        // and every evidence assertion below would be testing nothing.
        let supervisor = Arc::new(
            RunSupervisor::build(
                RunPolicy {
                    term_grace: Duration::from_secs(1),
                    ..RunPolicy::default()
                },
                Arc::new(EvidencePipeline::new(config.git_timeout)),
            )
            .expect("the shortened policy is still valid"),
        );
        let validator = Arc::new(Validator::new(
            ValidationPolicy::from_config(&config),
            Arc::new(FakeRuntime),
        ));
        let wiring = Arc::new(Wiring {
            validator,
            ..Wiring::with_supervisor(Arc::new(config.clone()), supervisor)
        });
        let fixture = Self {
            _tmp: tmp,
            repo,
            evidence,
            act,
            argv: root.join("argv.txt"),
            cwd: root.join("cwd.txt"),
            config: Arc::new(config),
            wiring,
        };
        fixture.write_stub("", "exit 0");
        fixture
    }

    /// Replace the `act` stub. The config points at this path, so a test can decide what "act"
    /// does at the moment it decides that.
    fn write_act(&self, script: &str) {
        let mut file = std::fs::File::create(&self.act).expect("create the act stub");
        file.write_all(script.as_bytes())
            .expect("write the act stub");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&self.act, std::fs::Permissions::from_mode(0o755))
                .expect("the act stub must be executable");
        }
    }

    /// The stub every test starts with: record argv and cwd, publish the coverage payload into
    /// whatever directory `--env ACTMCP_EVIDENCE_OUT=<dir>` named (ADR-0002 §3.1's transport —
    /// the stub reads it out of its own argv exactly as a job would find it in its environment),
    /// then run `tail`.
    fn write_stub(&self, middle: &str, tail: &str) {
        let script = format!(
            "#!/bin/sh\nset -u\nprintf '%s\\n' \"$@\" > '{}'\npwd > '{}'\nout=''\nprev=''\nsecret=''\n\
             for a in \"$@\"; do case \"$a\" in ACTMCP_EVIDENCE_OUT=*) out=\"${{a#*=}}\";; esac; \
             if [ \"$prev\" = --secret-file ]; then secret=\"$a\"; fi; prev=\"$a\"; done\n\
             if [ -n \"$out\" ]; then mkdir -p \"$out\" \\
             && cp '{LCOV_FIXTURE}' \"$out/lcov.info\" \\
             && cp '{BENCHMARK_FIXTURE}' \"$out/benchmark.json\" || exit 70; fi\n{middle}\n{tail}\n",
            self.argv.display(),
            self.cwd.display()
        );
        self.write_act(&script);
    }

    /// act exits with `code` (the failing-workflow case).
    fn stub_exits(&self, code: u8) {
        self.write_stub("", &format!("exit {code}"));
    }

    /// act stays alive for `secs` — the in-flight case, for busy/timeout/cancel tests.
    fn stub_sleeps(&self, secs: u64) {
        self.write_stub("", &format!("sleep {secs}\nexit 0"));
    }

    /// act copies the *keys* of its `--secret-file` into the log (never the values), the way
    /// `tests/testdata/stub-act.sh` does, so a test can prove the secret arrived by file.
    fn stub_echoes_secret_file(&self) {
        self.write_stub(
            "if [ -f \"$secret\" ]; then printf \
             '{\"level\":\"info\",\"msg\":\"secret keys %s\"}\\n' \"$(cut -d= -f1 \"$secret\" | \
             tr '\\n' ' ')\"; else printf \
             '{\"level\":\"warning\",\"msg\":\"secret file missing\"}\\n'; fi",
            "exit 0",
        );
    }

    /// Lines of the argv the last stub invocation recorded, one per token.
    fn argv(&self) -> Vec<String> {
        read_lines(&self.argv)
    }

    /// The working directory act ran in.
    fn cwd(&self) -> String {
        std::fs::read_to_string(&self.cwd)
            .expect("the stub records its cwd")
            .trim()
            .to_owned()
    }

    /// `git worktree list --porcelain` in the workspace, one line per worktree path.
    fn worktrees(&self) -> Vec<String> {
        self.repo
            .git(&["worktree", "list", "--porcelain"])
            .lines()
            .filter_map(|line| line.strip_prefix("worktree "))
            .map(str::to_owned)
            .collect()
    }

    async fn server(&self) -> TestServer {
        TestServer::with_wiring(Arc::clone(&self.wiring)).await
    }

    fn evidence_path(&self, relative: &str) -> PathBuf {
        self.evidence.join(relative)
    }
}

/// A container runtime that is always alive, and an `act` whose version is always new enough.
///
/// Written from outside the crate on purpose (the same double `tests/validation.rs` uses): the
/// `Environment` trait is the seam the wiring ticket is expected to inject through, and if that
/// seam were insufficient this test could not be written.
struct FakeRuntime;

impl Environment for FakeRuntime {
    fn git(
        &self,
        workspace: &Path,
        args: &[&str],
        timeout: Duration,
    ) -> Result<CommandOutput, CommandError> {
        // Real git: the gates, the commit resolution and the worktree commands under test are
        // the real thing.
        SystemEnvironment.git(workspace, args, timeout)
    }

    fn resolve_program(&self, requested: &Path) -> Result<PathBuf, ProgramError> {
        SystemEnvironment.resolve_program(requested)
    }

    fn container_runtime(
        &self,
        _socket: Option<&Path>,
        _timeout: Duration,
    ) -> Result<ContainerRuntime, ContainerProbeError> {
        Ok(ContainerRuntime {
            program: "docker".to_owned(),
            version: Some("27.1.1".to_owned()),
        })
    }

    fn program_version(&self, _program: &Path) -> Option<String> {
        Some("act version 0.2.68".to_owned())
    }
}

// ------------------------------------------------------------------ helpers

/// One `tools/call` for `run_workflow`, with the per-request `_meta` the pinned revision needs.
async fn call(url: &str, arguments: Value) -> Value {
    let mut params = json!({ "name": TOOL, "arguments": arguments });
    params["_meta"] = meta_2026_07_28();
    let (status, body) =
        post_jsonrpc(&raw_client(), url, "tools/call", params, Some("2026-07-28")).await;
    assert_eq!(status, 200, "a tools/call is answered, not refused: {body}");
    assert!(
        body.get("error").is_none(),
        "a tool failure must never be a JSON-RPC error (ADR-0001 §5.1): {body}"
    );
    body
}

/// The `structuredContent` of a successful call, with the whole body in the failure message.
fn ok(body: &Value) -> &Value {
    assert_eq!(
        body["result"]["isError"],
        json!(false),
        "expected a successful tool result, got: {body}"
    );
    &body["result"]["structuredContent"]
}

/// The `structuredContent.error.code` of a failed call.
fn error_code(body: &Value) -> String {
    assert_eq!(
        body["result"]["isError"],
        json!(true),
        "expected a failed tool result, got: {body}"
    );
    body["result"]["structuredContent"]["error"]["code"]
        .as_str()
        .unwrap_or_else(|| panic!("a failed result carries an error.code: {body}"))
        .to_owned()
}

/// The error `detail` of a failed call.
fn error_detail(body: &Value) -> Value {
    body["result"]["structuredContent"]["error"]["detail"].clone()
}

fn read_lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("reading {}: {error}", path.display()))
        .lines()
        .map(str::to_owned)
        .collect()
}

/// `--flag value` from a recorded argv, if the flag is there.
fn flag(argv: &[String], flag: &str) -> Option<String> {
    argv.iter()
        .position(|token| token == flag)
        .and_then(|index| argv.get(index + 1))
        .cloned()
}

/// Poll `run_status` until the run is terminal, failing the test if it never gets there.
async fn wait_for_terminal(url: &str, run_id: &str) -> Value {
    let deadline = std::time::Instant::now() + TERMINAL_BUDGET;
    loop {
        // `run_status` is a different tool, so it is asked directly rather than through `call`.
        let mut params = json!({ "name": "run_status", "arguments": { "run_id": run_id } });
        params["_meta"] = meta_2026_07_28();
        let (_status, body) =
            post_jsonrpc(&raw_client(), url, "tools/call", params, Some("2026-07-28")).await;
        let view = &body["result"]["structuredContent"]["run"];
        if view["terminal"].as_bool() == Some(true) {
            return view.clone();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "run {run_id} never reached its terminal commit: {view}"
        );
        tokio::time::sleep(TERMINAL_POLL).await;
    }
}

/// Assert a result's identity block is the *resolved* one (ADR-0001 §3.1).
fn assert_identity(view: &Value, head: &str, workflow: &str, file: &str, task: &str) {
    assert_eq!(view["commit"], json!(head), "commit must be the full SHA");
    assert_eq!(view["workflow"], json!(workflow));
    assert_eq!(view["workflow_file"], json!(file));
    assert_eq!(view["task"], json!(task));
    assert_eq!(view["terminal"], json!(true));
}

// ------------------------------------------------------------------ the pipeline, end to end

/// The headline acceptance: validate → run → coverage extraction → `.evidence` on disk → the
/// structured result, in one call, with the evidence already written when the call returns.
#[tokio::test]
async fn a_coverage_run_writes_evidence_and_returns_the_terminal_view() {
    let fixture = Fixture::new();
    let server = fixture.server().await;
    let head = fixture.repo.head();

    let body = call(
        &server.url(),
        json!({
            "workflow": "ci",                       // a name, not a path
            "task": "build",
            "category": "coverage",
            "commit": &head[..10],                 // a 10-char abbreviation
        }),
    )
    .await;
    let view = ok(&body);

    assert_identity(view, &head, "ci", ".github/workflows/ci.yml", "build");
    assert_eq!(view["status"], json!("success"));
    assert_eq!(view["category"], json!("coverage"));
    assert_eq!(
        view["exit_code"],
        json!(0),
        "act exited 0, and that is the exit code"
    );
    assert_eq!(view["deduplicated"], json!(false));
    assert!(
        view["started_at"].is_string() && view["finished_at"].is_string(),
        "a terminal run reports both instants: {view}"
    );
    assert!(
        view["duration_ms"].is_number(),
        "and how long it took: {view}"
    );

    // The evidence file: one file, at ADR-0002 §1.1's path, named by the resolved commit.
    let run_id = view["run_id"].as_str().expect("a run_id").to_owned();
    let files = view["evidence_files"]
        .as_array()
        .expect("evidence_files is an array");
    assert_eq!(
        files.len(),
        1,
        "a coverage run publishes exactly one coverage file: {view}"
    );
    let relative = files[0].as_str().expect("a relative path");
    assert_eq!(
        relative,
        format!("coverage/{head}/{run_id}.jsonl"),
        "the evidence path is coverage/<full-sha>/<run_id>.jsonl"
    );
    assert!(
        view["evidence_record_count"]
            .as_u64()
            .is_some_and(|count| count > 0),
        "the lcov fixture yields records: {view}"
    );

    // ADR-0004 §3.4: publishing happens before the terminal commit is observable, so a result
    // that lists a file means the file is already there — and readable as ADR-0002 JSONL.
    let written = fixture.evidence_path(relative);
    let text = std::fs::read_to_string(&written)
        .unwrap_or_else(|error| panic!("{} must exist by now: {error}", written.display()));
    let lines: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("every evidence line is JSON"))
        .collect();
    assert!(!lines.is_empty());
    for record in &lines {
        assert_eq!(record["run_status"], json!("success"));
        assert_eq!(record["workflow"], json!("ci"));
        assert_eq!(record["commit"], json!(head));
        assert_eq!(record["run_id"], json!(run_id));
        assert_eq!(record["category"], json!("coverage"));
    }

    // The audit paths exist too, and are deliberately NOT in evidence_files.
    let run_dir = fixture.evidence_path("runs").join(&run_id);
    assert!(
        run_dir.join("meta.json").is_file(),
        "meta.json is written as part of publishing, not after it: {}",
        run_dir.display()
    );
    assert!(run_dir.join("act.jsonl").is_file());
    assert!(
        !relative.contains("meta.json") && !relative.contains("act.jsonl"),
        "audit paths are not evidence: {files:?}"
    );

    // And what act was actually told: ADR-0003 §7.2's base argv, against the workspace itself
    // (mode `head_only`, because the commit is HEAD and the policy is `hybrid`).
    let argv = fixture.argv();
    assert_eq!(
        flag(&argv, "--directory").as_deref(),
        Some(fixture.repo.path().to_str().expect("utf-8 fixture path")),
        "a head-only run executes against the operator's checkout"
    );
    assert_eq!(flag(&argv, "--job").as_deref(), Some("build"));
    assert_eq!(
        flag(&argv, "--workflows").as_deref(),
        Some(
            fixture
                .repo
                .path()
                .join(".github/workflows/ci.yml")
                .to_str()
                .expect("utf-8")
        )
    );
    assert!(
        flag(&argv, "--defaultbranch").is_none(),
        "--defaultbranch is a worktree-mode flag (ADR-0003 §5.4): {argv:?}"
    );
    assert!(
        argv.iter()
            .any(|token| token.starts_with("ACTMCP_EVIDENCE_OUT=")),
        "the payload transport is announced through act's own --env: {argv:?}"
    );
    assert_eq!(
        fixture.cwd(),
        fixture
            .config
            .run_scratch
            .join(&run_id)
            .display()
            .to_string(),
        "act's cwd is the per-run scratch directory (ADR-0003 §5.2)"
    );

    server.shutdown().await;
}

/// A job that fails is a *successful call* about a failed run: ADR-0001 §5.1's central rule, and
/// the one an agent is most likely to get wrong if the server got it right but said it loudly.
#[tokio::test]
async fn a_failing_job_is_a_successful_call_reporting_failure() {
    let fixture = Fixture::new();
    fixture.stub_exits(3);
    let server = fixture.server().await;
    let head = fixture.repo.head();

    let body = call(
        &server.url(),
        json!({ "workflow": "ci", "task": "build", "category": "other", "commit": head }),
    )
    .await;
    let view = ok(&body);
    assert_eq!(view["status"], json!("failed"));
    assert_eq!(view["exit_code"], json!(3));
    assert_eq!(view["terminal"], json!(true));
    let warnings = view["warnings"]
        .as_array()
        .expect("warnings is an array")
        .clone();
    assert!(
        warnings.contains(&json!("exit_nonzero")),
        "the reason is greppable, not only implied: {warnings:?}"
    );
    assert!(
        view.get("error").is_none(),
        "`error` is reserved for a run that could not be controlled: {view}"
    );
    server.shutdown().await;
}

/// An unknown category is the contract's honest-empty case (ADR-0001 §2.4): it runs, it is
/// reported as a success, and the result states that no evidence was written and why.
#[tokio::test]
async fn an_unknown_category_runs_and_says_what_it_wrote_nothing_of() {
    let fixture = Fixture::new();
    let server = fixture.server().await;
    let head = fixture.repo.head();

    let body = call(
        &server.url(),
        json!({ "workflow": "ci", "task": "build", "category": "fuzzing", "commit": head }),
    )
    .await;
    let view = ok(&body);
    assert_eq!(view["status"], json!("success"));
    assert_eq!(
        view["category"],
        json!("fuzzing"),
        "the caller's word is echoed back"
    );
    assert_eq!(
        view["evidence_files"],
        json!([]),
        "no extractor ran, so nothing is claimed: {view}"
    );
    assert_eq!(view["evidence_record_count"], json!(0));
    assert!(
        view["warnings"]
            .as_array()
            .expect("warnings")
            .contains(&json!("unknown_category")),
        "the empty answer explains itself: {view}"
    );
    assert!(
        !fixture.evidence_path("fuzzing").exists(),
        "an unknown category must not invent a directory"
    );
    // The run directory is still there: every run is auditable whatever its category.
    let run_id = view["run_id"].as_str().expect("run_id");
    assert!(fixture
        .evidence_path("runs")
        .join(run_id)
        .join("meta.json")
        .is_file());
    server.shutdown().await;
}

/// A benchmark run publishes to the benchmark tree (ADR-0002 §1.1), which is the same code path
/// as coverage with a different extractor — so this test is really about the dispatch.
#[tokio::test]
async fn a_benchmark_run_publishes_under_benchmark() {
    let fixture = Fixture::new();
    let server = fixture.server().await;
    let head = fixture.repo.head();

    let body = call(
        &server.url(),
        json!({ "workflow": "ci", "task": "build", "category": "benchmark", "commit": head }),
    )
    .await;
    let view = ok(&body);
    assert_eq!(view["status"], json!("success"));
    let run_id = view["run_id"].as_str().expect("run_id").to_owned();
    let files = view["evidence_files"].as_array().expect("evidence_files");
    assert_eq!(
        files[0].as_str().expect("a path"),
        format!("benchmark/{head}/{run_id}.jsonl"),
        "a benchmark run's evidence lives under benchmark/, not coverage/"
    );
    assert!(fixture
        .evidence_path(&format!("benchmark/{head}/{run_id}.jsonl"))
        .is_file());
    // The evidence root pre-creates both category directories, so the check has to be about
    // the file: a benchmark run must not publish coverage records just because the stub also
    // wrote an lcov file into the same payload directory.
    assert!(
        !fixture
            .evidence_path(&format!("coverage/{head}/{run_id}.jsonl"))
            .exists(),
        "the category selects the extractor, not the payload that happens to be lying around"
    );
    assert!(
        std::fs::read_dir(fixture.evidence_path("coverage"))
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(true),
        "and nothing was left in coverage/ either"
    );
    server.shutdown().await;
}

// ------------------------------------------------------------------ isolation (ADR-0003)

/// The worktree path: a non-`HEAD`-in-place mode moves act's `--directory` into a scratch
/// worktree of the resolved commit, tells act the operator's branch for `--defaultbranch`, and
/// leaves nothing behind.
#[tokio::test]
async fn worktree_mode_runs_in_a_scratch_tree_and_leaves_nothing_behind() {
    let fixture = Fixture::worktree();
    let server = fixture.server().await;
    let head = fixture.repo.head();
    let before = fixture.worktrees();
    assert_eq!(
        before.len(),
        1,
        "the fixture starts with one worktree (the checkout): {before:?}"
    );

    let body = call(
        &server.url(),
        json!({ "workflow": "ci", "task": "build", "category": "coverage", "commit": head }),
    )
    .await;
    let view = ok(&body);
    assert_eq!(view["status"], json!("success"), "{view}");
    let run_id = view["run_id"].as_str().expect("run_id").to_owned();

    let argv = fixture.argv();
    let tree = flag(&argv, "--directory").expect("--directory");
    assert_eq!(
        tree,
        fixture
            .config
            .run_scratch
            .join(&run_id)
            .join("worktree")
            .display()
            .to_string(),
        "act ran against the run's own scratch worktree"
    );
    assert_ne!(
        tree,
        fixture.repo.path().display().to_string(),
        "and not against the operator's checkout"
    );
    assert_eq!(
        flag(&argv, "--defaultbranch").as_deref(),
        Some("main"),
        "--defaultbranch is probed from the checkout's remote (ADR-0003 §5.4)"
    );
    // The tree really was a tree of the right commit: the workflow file act was pointed at.
    assert!(
        flag(&argv, "--workflows")
            .expect("--workflows")
            .starts_with(&tree),
        "the workflow path is inside the tree act was given"
    );

    // `release` runs before the terminal commit is observable (ADR-0004 §3.4 step 5–6), so by
    // the time this result is in hand the scratch is gone.
    let run_scratch = fixture.config.run_scratch.join(&run_id);
    assert!(
        !run_scratch.exists(),
        "the run's scratch directory must be removed on every terminal path: {}",
        run_scratch.display()
    );
    assert_eq!(
        fixture.worktrees(),
        before,
        "`git worktree add` must be matched by `git worktree remove`"
    );
    assert!(
        fixture
            .evidence_path(&format!("coverage/{head}/{run_id}.jsonl"))
            .is_file(),
        "the evidence root is never rewritten to the tree (ADR-0003 §5.3)"
    );
    server.shutdown().await;
}

/// A dirty checkout in a mode that would have to read the working tree is refused rather than
/// measured (ADR-0003 §4.3) — and refused with `isolation_failed`, not a run of wrong numbers.
#[tokio::test]
async fn a_head_only_policy_refuses_a_non_head_commit_instead_of_running_it() {
    let fixture = Fixture::head_only();
    // Two commits, so "an older commit" is a thing that exists.
    let newest = fixture
        .repo
        .add_and_commit("notes.md", "second\n", "test: a second commit");
    let oldest = fixture.repo.git(&["rev-parse", "HEAD~1"]);
    assert_ne!(newest, oldest);

    let server = fixture.server().await;
    let body = call(
        &server.url(),
        json!({ "workflow": "ci", "task": "build", "category": "coverage", "commit": &oldest[..12] }),
    )
    .await;
    assert_eq!(error_code(&body), "isolation_failed");
    let detail = error_detail(&body);
    assert_eq!(detail["mode"], json!("head_only"));
    assert_eq!(detail["reason"], json!("policy_head_only"));
    assert_eq!(
        fixture.worktrees().len(),
        1,
        "a refusal must not leave a tree behind"
    );
    assert_eq!(
        fixture
            .repo
            .git(&["status", "--porcelain", "--untracked-files=no"]),
        "",
        "and must not touch tracked files in the checkout"
    );
    server.shutdown().await;
}

/// The scratch root has to be writable *before* a run is registered, because that is the only
/// moment the answer can still be `isolation_failed` (ADR-0003 §4.4 W3) rather than a run that
/// died in `preparing_tree`.
#[tokio::test]
async fn an_unwritable_scratch_root_refuses_without_starting_a_run() {
    let fixture = Fixture::worktree();
    // Replace the scratch root with a *file*: `mkdir -p` cannot create it and nothing can be
    // written inside it, on any filesystem.
    let scratch = fixture.config.run_scratch.clone();
    std::fs::remove_dir_all(&scratch).ok();
    std::fs::write(&scratch, b"not a directory").expect("occupy the scratch path");

    let server = fixture.server().await;
    let head = fixture.repo.head();
    let body = call(
        &server.url(),
        json!({ "workflow": "ci", "task": "build", "category": "coverage", "commit": head }),
    )
    .await;
    assert_eq!(error_code(&body), "isolation_failed", "{body}");
    assert_eq!(error_detail(&body)["reason"], json!("scratch_unwritable"));
    assert_eq!(
        fixture.worktrees().len(),
        1,
        "no run was registered, so no tree was made"
    );
    server.shutdown().await;
}

// ------------------------------------------------------------------ lifecycle

/// `wait: false` answers as soon as the run is registered and the run goes on without the
/// caller (ADR-0001 §2.6): the immediate view is non-terminal, `run_status` finishes the story.
#[tokio::test]
async fn wait_false_returns_immediately_and_the_run_finishes_anyway() {
    let fixture = Fixture::new();
    fixture.stub_sleeps(2);
    let server = fixture.server().await;
    let head = fixture.repo.head();

    let body = call(
        &server.url(),
        json!({
            "workflow": "ci", "task": "build", "category": "coverage", "commit": head,
            "wait": false, "timeout_secs": 60,
        }),
    )
    .await;
    let view = ok(&body);
    assert_eq!(view["terminal"], json!(false));
    assert!(
        ["pending", "running"].contains(&view["status"].as_str().expect("a status")),
        "a registered run is pending or running: {view}"
    );
    assert!(
        view.get("finished_at").is_none(),
        "a non-terminal run has no finish time (ADR-0001 §3.2): {view}"
    );
    let run_id = view["run_id"].as_str().expect("run_id").to_owned();

    let settled = wait_for_terminal(&server.url(), &run_id).await;
    assert_eq!(settled["status"], json!("success"), "{settled}");
    assert_eq!(
        settled["evidence_files"].as_array().expect("files").len(),
        1,
        "evidence is published by the run, not by the call that waited"
    );
    server.shutdown().await;
}

/// One run at a time per workspace, and the refusal names the run that is going (ADR-0001 §5.3
/// `workspace_busy`), so the caller can poll it instead of guessing.
#[tokio::test]
async fn a_second_run_while_one_is_in_flight_is_workspace_busy() {
    let fixture = Fixture::new();
    fixture.stub_sleeps(5);
    let server = fixture.server().await;
    let head = fixture.repo.head();
    let url = server.url();

    let first = call(
        &url,
        json!({ "workflow": "ci", "task": "build", "category": "coverage", "commit": head, "wait": false }),
    )
    .await;
    let first = ok(&first);
    let run_id = first["run_id"].as_str().expect("run_id").to_owned();

    let second = call(
        &url,
        json!({ "workflow": "ci", "task": "lint", "category": "coverage", "commit": head, "wait": false }),
    )
    .await;
    assert_eq!(error_code(&second), "workspace_busy");
    let detail = error_detail(&second);
    assert_eq!(
        detail["run_id"],
        json!(run_id),
        "the refusal says who is holding the workspace: {detail}"
    );
    assert!(
        detail["phase"].is_string(),
        "and which phase it is in: {detail}"
    );
    // The refused call changed nothing: still exactly one run.
    let listing = {
        let mut params = json!({ "name": "run_status", "arguments": {} });
        params["_meta"] = meta_2026_07_28();
        let (_status, body) = post_jsonrpc(
            &raw_client(),
            &url,
            "tools/call",
            params,
            Some("2026-07-28"),
        )
        .await;
        body["result"]["structuredContent"].clone()
    };
    assert_eq!(listing["runs"].as_array().expect("runs").len(), 1);

    // Clean up: stop the sleeping run so the server can go down.
    let mut params = json!({ "name": "cancel_run", "arguments": { "run_id": run_id } });
    params["_meta"] = meta_2026_07_28();
    let (_status, cancelled) = post_jsonrpc(
        &raw_client(),
        &url,
        "tools/call",
        params,
        Some("2026-07-28"),
    )
    .await;
    assert_eq!(cancelled["result"]["isError"], json!(false), "{cancelled}");
    server.shutdown().await;
}

/// `timeout_secs` is act's budget, and expiring it is a normal result: the group is killed, the
/// run says `timed_out`, and what the run had already collected is still published
/// (ADR-0004 §4.6, §6.2 — the asymmetry with a caller cancel that this test is the other half of).
#[tokio::test]
async fn a_timed_out_run_is_a_reported_timeout_not_an_error() {
    let fixture = Fixture::new();
    fixture.stub_sleeps(60);
    let server = fixture.server().await;
    let head = fixture.repo.head();

    let started = std::time::Instant::now();
    let body = call(
        &server.url(),
        json!({
            "workflow": "ci", "task": "build", "category": "coverage", "commit": head,
            "timeout_secs": 1,
        }),
    )
    .await;
    let elapsed = started.elapsed();
    let view = ok(&body);
    assert_eq!(view["status"], json!("timed_out"), "{view}");
    assert_eq!(view["terminal"], json!(true));
    assert!(
        elapsed < Duration::from_secs(30),
        "the wait ends with the run, not with the budget: {elapsed:?}"
    );
    assert!(
        view["evidence_files"]
            .as_array()
            .expect("evidence_files")
            .len()
            == 1,
        "a timeout keeps its numbers; a cancel does not: {view}"
    );
    server.shutdown().await;
}

/// A caller cancel is the other half of that asymmetry: the run ends `cancelled` and publishes
/// nothing, because being asked to stop means the measurement is not wanted (ADR-0004 §6.2).
#[tokio::test]
async fn a_cancelled_run_reports_cancelled_and_publishes_nothing() {
    let fixture = Fixture::new();
    fixture.stub_sleeps(60);
    let server = fixture.server().await;
    let head = fixture.repo.head();
    let url = server.url();

    let started = call(
        &url,
        json!({ "workflow": "ci", "task": "build", "category": "coverage", "commit": head, "wait": false }),
    )
    .await;
    let run_id = ok(&started)["run_id"].as_str().expect("run_id").to_owned();

    let mut params = json!({ "name": "cancel_run", "arguments": { "run_id": &run_id } });
    params["_meta"] = meta_2026_07_28();
    let (_status, receipt) = post_jsonrpc(
        &raw_client(),
        &url,
        "tools/call",
        params,
        Some("2026-07-28"),
    )
    .await;
    assert_eq!(receipt["result"]["isError"], json!(false), "{receipt}");

    let settled = wait_for_terminal(&url, &run_id).await;
    assert_eq!(settled["status"], json!("cancelled"), "{settled}");
    assert_eq!(
        settled["evidence_files"].as_array().expect("files").len(),
        0,
        "extraction is skipped for a caller cancel: {settled}"
    );
    assert!(
        !fixture
            .evidence_path(&format!("coverage/{head}/{run_id}.jsonl"))
            .exists(),
        "and nothing is written behind the result's back"
    );
    server.shutdown().await;
}

// ------------------------------------------------------------------ idempotency

/// The point of `idempotency_key` (ADR-0001 §8): a retry does not start a second run.
#[tokio::test]
async fn an_idempotency_key_replays_the_run_it_started() {
    let fixture = Fixture::new();
    fixture.stub_sleeps(2);
    let server = fixture.server().await;
    let head = fixture.repo.head();

    let arguments = json!({
        "workflow": "ci", "task": "build", "category": "coverage", "commit": head,
        "idempotency_key": "ci-build-head-1", "wait": false,
    });
    let binding = call(&server.url(), arguments.clone()).await;
    let first = ok(&binding);
    let run_id = first["run_id"].as_str().expect("run_id").to_owned();
    assert_eq!(
        first["deduplicated"],
        json!(false),
        "the first call started it"
    );

    let binding = call(&server.url(), arguments).await;
    let again = ok(&binding);
    assert_eq!(
        again["run_id"],
        json!(run_id),
        "the same key and the same tuple must answer with the same run"
    );
    assert_eq!(again["deduplicated"], json!(true), "and say it is a replay");

    // A different spelling of the same request is still the same request: the key is matched
    // against the *resolved* identity, so a path and a name are one tuple.
    let binding = call(
        &server.url(),
        json!({
            "workflow": ".github/workflows/ci.yml", "task": "build", "category": "coverage",
            "commit": &head[..8], "idempotency_key": "ci-build-head-1", "wait": false,
        }),
    )
    .await;
    let respelled = ok(&binding);
    assert_eq!(
        respelled["run_id"],
        json!(run_id),
        "resolved identity is the key's meaning"
    );

    let settled = wait_for_terminal(&server.url(), &run_id).await;
    assert_eq!(settled["status"], json!("success"), "{settled}");
    // And a replay after the run is terminal still answers with that run, not a fresh one.
    let binding = call(
        &server.url(),
        json!({
            "workflow": "ci", "task": "build", "category": "coverage", "commit": head,
            "idempotency_key": "ci-build-head-1",
        }),
    )
    .await;
    let after = ok(&binding);
    assert_eq!(after["run_id"], json!(run_id));
    assert_eq!(after["terminal"], json!(true));
    server.shutdown().await;
}

/// …and one key must never mean two requests: the conflict names both tuples, because "which
/// one did I mean" is the question the caller has to answer.
#[tokio::test]
async fn one_idempotency_key_cannot_mean_two_requests() {
    let fixture = Fixture::new();
    let server = fixture.server().await;
    let head = fixture.repo.head();

    let binding = call(
        &server.url(),
        json!({
            "workflow": "ci", "task": "build", "category": "coverage", "commit": head,
            "idempotency_key": "one-key", "wait": false,
        }),
    )
    .await;
    let run_id = ok(&binding)["run_id"].as_str().expect("run_id").to_owned();
    // Let it finish so the key is not racing its own run.
    wait_for_terminal(&server.url(), &run_id).await;

    let conflict = call(
        &server.url(),
        json!({
            "workflow": "ci", "task": "lint", "category": "coverage", "commit": head,
            "idempotency_key": "one-key",
        }),
    )
    .await;
    assert_eq!(error_code(&conflict), "idempotency_conflict");
    let detail = error_detail(&conflict);
    assert_eq!(detail["idempotency_key"], json!("one-key"));
    assert_eq!(detail["run_id"], json!(run_id), "the run that owns the key");
    assert_eq!(detail["previous"]["task"], json!("build"));
    assert_eq!(detail["requested"]["task"], json!("lint"));
    assert_eq!(
        fixture
            .repo
            .git(&["status", "--porcelain", "--untracked-files=no"]),
        "",
        "a conflict is refused before anything tracked is touched"
    );
    server.shutdown().await;
}

// ------------------------------------------------------------------ failures that are not runs

/// Arguments are checked before anything is looked at, reported as much at once as possible, and
/// are `isError: true` tool results — never JSON-RPC `-32602` (which would make a typo look like
/// a broken server).
#[tokio::test]
async fn bad_arguments_are_tool_errors_naming_every_problem() {
    let fixture = Fixture::new();
    let server = fixture.server().await;
    let head = fixture.repo.head();
    let url = server.url();

    let mut cases: Vec<(&str, Value, Vec<&str>)> = vec![
        (
            "everything missing",
            json!({}),
            vec!["workflow", "task", "category", "commit"],
        ),
        (
            "wrong types",
            json!({"workflow": 3, "task": [], "category": null, "commit": true}),
            vec!["workflow", "task", "commit"],
        ),
        (
            "a category with the wrong shape",
            json!({"workflow": "ci", "task": "build", "category": "Bad Category", "commit": head}),
            vec!["category"],
        ),
        (
            "a commit that is not hex",
            json!({"workflow": "ci", "task": "build", "category": "coverage", "commit": "zzzzzzzz"}),
            vec!["commit"],
        ),
        (
            "wait that is not a boolean",
            json!({"workflow": "ci", "task": "build", "category": "coverage", "commit": head, "wait": "yes"}),
            vec!["wait"],
        ),
        (
            "a timeout over the ceiling",
            json!({"workflow": "ci", "task": "build", "category": "coverage", "commit": head, "timeout_secs": 99999}),
            vec!["timeout_secs"],
        ),
        (
            "a log tail over the cap",
            json!({"workflow": "ci", "task": "build", "category": "coverage", "commit": head, "log_tail_bytes": 65537}),
            vec!["log_tail_bytes"],
        ),
        (
            "inputs that are not a map of strings",
            json!({"workflow": "ci", "task": "build", "category": "coverage", "commit": head,
                    "inputs": {"ok": "1", "bad": 2, "also-bad": ["x"]}}),
            vec!["inputs"],
        ),
        (
            "a secret named like a shell command",
            json!({"workflow": "ci", "task": "build", "category": "coverage", "commit": head,
                    "secrets": {"9LIVES": "x"}}),
            vec!["secrets"],
        ),
        (
            "an idempotency key with a space",
            json!({"workflow": "ci", "task": "build", "category": "coverage", "commit": head,
                    "idempotency_key": "two words"}),
            vec!["idempotency_key"],
        ),
    ];
    // One case per entry, plus one that is about a key rather than a value.
    for (what, arguments, expected_paths) in cases.drain(..) {
        let body = call(&url, arguments).await;
        assert_eq!(error_code(&body), "invalid_arguments", "{what}: {body}");
        let errors = error_detail(&body)["errors"]
            .as_array()
            .unwrap_or_else(|| panic!("invalid_arguments lists its problems: {body}"))
            .clone();
        let paths: Vec<&str> = errors
            .iter()
            .filter_map(|problem| problem["path"].as_str())
            .collect();
        for expected in &expected_paths {
            assert!(
                paths.contains(expected),
                "{what} must report `{expected}`, reported {paths:?}"
            );
        }
        for problem in &errors {
            assert!(
                problem["expected"].is_string() && problem["reason"].is_string(),
                "every problem says what was wanted: {problem}"
            );
        }
    }

    // A caller that supplies `run_id` is trying to name its own run: ADR-0001 §2 says the
    // server mints it, and an unknown key must be refused rather than silently ignored.
    let body = call(
        &url,
        json!({ "workflow": "ci", "task": "build", "category": "coverage", "commit": head,
                "run_id": "20261110T000000Z-deadbe" }),
    )
    .await;
    assert_eq!(error_code(&body), "unknown_argument");
    let detail = error_detail(&body);
    assert_eq!(
        detail["unexpected"],
        json!(["run_id"]),
        "the refusal names the key: {detail}"
    );
    server.shutdown().await;
}

/// The validator's codes reach the caller unchanged (ADR-0001 §5.3), in the order §2.9 fixes —
/// so a caller with two problems learns about the one it would have hit first.
#[tokio::test]
async fn preconditions_answer_with_the_code_that_names_them() {
    let fixture = Fixture::new();
    let server = fixture.server().await;
    let head = fixture.repo.head();
    let url = server.url();

    let cases = [
        (
            json!({"workflow": "nope", "task": "build", "category": "coverage", "commit": head}),
            "workflow_not_found",
        ),
        (
            json!({"workflow": "ci", "task": "no-such-job", "category": "coverage", "commit": head}),
            "task_not_found",
        ),
        (
            json!({"workflow": "ci", "task": "build", "category": "coverage", "commit": "deadbee"}),
            "commit_unknown",
        ),
    ];
    for (arguments, expected) in cases {
        let shown = arguments.to_string();
        let body = call(&url, arguments).await;
        assert_eq!(error_code(&body), expected, "{shown} => {body}");
    }
    // Shape beats existence: a bad category with a bad commit is reported as bad arguments,
    // because arguments are checked before the filesystem is touched at all.
    let body = call(
        &url,
        json!({"workflow": "nope", "task": "build", "category": "BAD", "commit": "deadbee"}),
    )
    .await;
    assert_eq!(error_code(&body), "invalid_arguments");
    server.shutdown().await;
}

/// A run's arguments are only half the isolation story: a secret must never be argv, a log line,
/// or a file the caller can read back. This is the end-to-end half of that claim — the stub proves
/// the secret arrived by *file* and the server proves it is nowhere else.
#[tokio::test]
async fn a_secret_reaches_the_job_by_file_and_nowhere_else() {
    let fixture = Fixture::new();
    fixture.stub_echoes_secret_file();
    let server = fixture.server().await;
    let head = fixture.repo.head();

    let body = call(
        &server.url(),
        json!({
            "workflow": "ci", "task": "build", "category": "coverage", "commit": head,
            "secrets": {"GITHUB_TOKEN": "hunter2-super-secret", "NPM_TOKEN": "npm-also-secret"},
            "inputs": {"profile": "ci"},
        }),
    )
    .await;
    let view = ok(&body);
    assert_eq!(view["status"], json!("success"), "{view}");

    // The keys got to act, by file.
    let log = view["log_tail"].as_str().expect("log_tail");
    assert!(
        log.contains("secret keys") && log.contains("GITHUB_TOKEN"),
        "the secret file arrived with its keys: {log}"
    );
    // Neither value appears anywhere in the result, nor in the argv the stub recorded.
    let argv = fixture.argv().join(" ");
    for secret in ["hunter2-super-secret", "npm-also-secret"] {
        assert!(
            !argv.contains(secret),
            "a secret must never be argv: {argv}"
        );
        assert!(
            !log.contains(secret),
            "a secret must never reach the log: {log}"
        );
        assert!(
            !body.to_string().contains(secret),
            "a secret must never reach the tool result"
        );
    }
    // `--input` is argv, and is not a secret channel: it arrives as `profile=ci`.
    assert!(
        fixture
            .argv()
            .windows(2)
            .any(|pair| pair[0] == "--input" && pair[1].starts_with("profile=")),
        "inputs go through act's --input, sorted and visible: {:?}",
        fixture.argv()
    );
    server.shutdown().await;
}
