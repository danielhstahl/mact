//! One scenario's world: a throwaway git workspace, a stub `act`, and a real
//! `act-mcp serve` booted against both.
//!
//! The server is a **subprocess** ([`common::ServeProcess`]), started exactly the way an
//! operator starts it — `serve --workspace … --act-binary <stub> --bind 127.0.0.1:0` —
//! because the point of this suite is that nothing between the MCP client and the stub
//! process is faked:
//!
//! ```text
//! scenario → HTTP POST /mcp → rmcp router → validation gates (real git, stub docker)
//!          → supervisor → runner (real argv, real process group) → ./testdata/bin/act
//!          → log tee + redaction → extractors → .evidence writer → meta.json
//! ```
//!
//! The only two fakes are the ones the acceptance criteria name: `act` and the container
//! runtime. Everything else is the shipped code path, on a machine with neither
//! installed.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::{json, Value};
use tempfile::TempDir;

use crate::common::{
    git_workspace, meta_2026_07_28, post_jsonrpc, raw_client, ServeOptions, ServeProcess, TestRepo,
};

use crate::golden;
use crate::normalize::Volatile;

/// How long a scenario waits for a `wait: false` run to go terminal.
const TERMINAL_BUDGET: Duration = Duration::from_secs(30);
const TERMINAL_POLL: Duration = Duration::from_millis(50);

/// `tests/e2e/testdata`.
pub fn testdata() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("e2e")
        .join("testdata")
}

/// The fixture workflows, read from `testdata/workflows` so the files themselves are
/// the fixture (reviewable YAML, not a Rust string).
pub fn fixture_workflows() -> Vec<(String, String)> {
    let dir = testdata().join("workflows");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".yml") || name.ends_with(".yaml"))
        .collect();
    names.sort();
    assert!(
        names.len() >= 4,
        "benchmark.yml, coverage.yml, multi-job.yml and broken.yml are the fixture set; found {names:?}"
    );
    names
        .into_iter()
        .map(|name| {
            let body = std::fs::read_to_string(dir.join(&name))
                .unwrap_or_else(|e| panic!("reading {}: {e}", dir.join(&name).display()));
            (name, body)
        })
        .collect()
}

/// What the stub `act` should do, expressed as the environment the server passes it.
///
/// Every method is one documented variable (see the header of
/// `tests/e2e/testdata/bin/act`); nothing here touches argv, which is what lets the
/// stub treat argv as a contract assertion rather than a channel.
#[derive(Debug, Default, Clone)]
pub struct Stub {
    pairs: Vec<(String, String)>,
}

impl Stub {
    pub fn new() -> Self {
        Self::default()
    }

    fn set(mut self, key: &str, value: impl Into<String>) -> Self {
        self.pairs.push((key.to_owned(), value.into()));
        self
    }

    /// `none|coverage|coverage-summary|benchmark|both|all` — which payload fixtures the
    /// fake job drops into `$ACTMCP_EVIDENCE_OUT`.
    pub fn payload(self, mode: &str) -> Self {
        self.set("STUB_PAYLOAD", mode)
    }

    /// `testdata/logs/<name>`, copied to stdout verbatim: act's `--json` log.
    pub fn log(self, name: &str) -> Self {
        self.set(
            "STUB_LOG",
            testdata().join("logs").join(name).display().to_string(),
        )
    }

    /// The exit status act returns.
    pub fn exit(self, code: u8) -> Self {
        self.set("STUB_EXIT_CODE", code.to_string())
    }

    /// How long the fake job runs.
    pub fn sleep(self, secs: &str) -> Self {
        self.set("STUB_SLEEP", secs)
    }

    /// How long the fake job runs before it drops its payload — the cancel scenario's
    /// "the numbers existed, and the caller threw them away".
    pub fn payload_delay(self, secs: &str) -> Self {
        self.set("STUB_PAYLOAD_DELAY", secs)
    }

    /// Print the in-band `ACTMCP_BENCHMARK` markers (ADR-0002 §5.2).
    pub fn benchmark_marker(self) -> Self {
        self.set("STUB_BENCHMARK_MARKER", "1")
    }

    /// Print the secret file's `key=value` lines: the redaction check.
    pub fn echo_secrets(self) -> Self {
        self.set("STUB_ECHO_SECRETS", "1")
    }

    /// Ignore SIGTERM, so the runner has to escalate to SIGKILL.
    pub fn ignore_sigterm(self) -> Self {
        self.set("STUB_IGNORE_SIGTERM", "1")
    }

    /// A background `sleep` in the run's process group: the orphan check.
    pub fn child_sleep(self, secs: &str) -> Self {
        self.set("STUB_CHILD_SLEEP", secs)
    }
}

/// The stub `act` the server is told to run — `argv[0]` of every recorded command line.
pub fn harness_act_binary() -> PathBuf {
    testdata().join("bin").join("act")
}

/// A client that will sit through a blocking `cancel_run`: ADR-0004 §5.2 lets that call
/// block for the whole `--cancel-wait-secs` (30 s by default), which is longer than
/// [`common::raw_client`]'s 10 s patience.
#[must_use]
pub fn patient_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .expect("reqwest client")
}

/// One scenario's fixture: workspace, scratch, stub wiring, and the server once booted.
pub struct Scenario {
    /// Held, never read: dropping it deletes the workspace under the server.
    _tmp: TempDir,
    pub repo: TestRepo,
    scratch: PathBuf,
    stub_argv: PathBuf,
    stub_cwd: PathBuf,
    stub_pid: PathBuf,
    stub_child_pid: PathBuf,
    server: Option<ServeProcess>,
}

impl Scenario {
    /// A fresh temp root, a two-commit git workspace carrying the fixture workflows, and
    /// a scratch root outside it (ADR-0003 §11).
    pub fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir for the e2e scenario");
        let root = std::fs::canonicalize(tmp.path()).expect("canonical tempdir");
        let workflows = fixture_workflows();
        let pairs: Vec<(&str, &str)> = workflows
            .iter()
            .map(|(name, body)| (name.as_str(), body.as_str()))
            .collect();
        let repo = git_workspace(&root.join("workspace"), &pairs);
        let scratch = root.join("scratch");
        std::fs::create_dir_all(&scratch).expect("create the scratch root");
        Self {
            _tmp: tmp,
            repo,
            scratch,
            stub_argv: root.join("stub-argv.txt"),
            stub_cwd: root.join("stub-cwd.txt"),
            stub_pid: root.join("stub-pid.txt"),
            stub_child_pid: root.join("stub-child-pid.txt"),
            server: None,
        }
    }

    /// Boot the server against this workspace with the stub steered by `stub`.
    pub fn boot(&mut self, stub: Stub) -> &ServeProcess {
        let options = ServeOptions::new(
            self.repo.path(),
            &harness_act_binary(),
            &self.scratch,
            &testdata().join("bin"),
        )
        .with_env("STUB_ARGV_FILE", &self.stub_argv)
        .with_env("STUB_CWD_FILE", &self.stub_cwd)
        .with_env("STUB_PID_FILE", &self.stub_pid)
        .with_env("STUB_CHILD_PID_FILE", &self.stub_child_pid)
        .with_env("STUB_PAYLOAD_DIR", testdata().join("payload"));
        let options = stub
            .pairs
            .into_iter()
            .fold(options, |options, (key, value)| {
                options.with_env(key, value)
            });
        let process = ServeProcess::start(&options);
        self.server = Some(process);
        self.server()
    }

    pub fn server(&self) -> &ServeProcess {
        self.server
            .as_ref()
            .expect("the scenario booted its server first")
    }

    pub fn url(&self) -> String {
        self.server().url()
    }

    /// `<workspace>/.evidence` — the default root, which is what the layout goldens
    /// are written against.
    pub fn evidence_root(&self) -> PathBuf {
        self.repo.path().join(".evidence")
    }

    /// The run's `--run-scratch/<run_id>/` directory (act's cwd, `secrets.env`, and the
    /// ADR-0003 worktree when there was one).
    pub fn run_scratch(&self, run_id: &str) -> PathBuf {
        self.scratch.join(run_id)
    }

    /// `.evidence/incoming/<run_id>/` — the per-run payload scratch.
    pub fn incoming(&self, run_id: &str) -> PathBuf {
        self.evidence_root().join("incoming").join(run_id)
    }

    /// A file the result named in `evidence_files`, resolved.
    ///
    /// Those paths are workspace-relative (ADR-0001 §3.3 forbids absolute host paths in a
    /// result), and the default evidence root is `<workspace>/.evidence`, so the strings
    /// start with `.evidence/` — joining them to the evidence root instead would double it.
    pub fn evidence_file(&self, workspace_relative: &str) -> PathBuf {
        self.repo.path().join(workspace_relative)
    }

    pub fn run_dir(&self, run_id: &str) -> PathBuf {
        self.evidence_root().join("runs").join(run_id)
    }

    /// The argv the stub recorded, one token per line.
    pub fn stub_argv(&self) -> Vec<String> {
        read_lines(&self.stub_argv)
    }

    pub fn stub_cwd(&self) -> String {
        std::fs::read_to_string(&self.stub_cwd)
            .unwrap_or_else(|e| panic!("the stub did not record its cwd: {e}"))
            .trim()
            .to_owned()
    }

    /// A pid the stub reported, once it exists — polled because the run may not have
    /// reached the point where the stub writes it.
    pub fn wait_for_stub_pid(&self, child: bool) -> u32 {
        let path = if child {
            &self.stub_child_pid
        } else {
            &self.stub_pid
        };
        for _ in 0..600 {
            if let Ok(text) = std::fs::read_to_string(path) {
                if let Ok(pid) = text.trim().parse::<u32>() {
                    return pid;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("{} never appeared", path.display());
    }

    pub fn stub_pid_exists(&self) -> bool {
        std::fs::metadata(&self.stub_pid).is_ok()
    }

    // ------------------------------------------------------------- the wire

    /// One `tools/call`, with the per-request `_meta` the pinned revision needs.
    pub async fn call(&self, tool: &str, arguments: Value) -> Value {
        self.call_with(&raw_client(), tool, arguments).await
    }

    /// The same call over a client the scenario chose (see [`patient_client`]).
    pub async fn call_with(&self, client: &reqwest::Client, tool: &str, arguments: Value) -> Value {
        let mut params = json!({ "name": tool, "arguments": arguments });
        params["_meta"] = meta_2026_07_28();
        let (status, body) = post_jsonrpc(
            client,
            &self.url(),
            "tools/call",
            params,
            Some("2026-07-28"),
        )
        .await;
        assert_eq!(status, 200, "a tools/call is answered, not refused: {body}");
        assert!(
            body.get("error").is_none(),
            "a tool failure must never be a JSON-RPC error (ADR-0001 §5.1): {body}"
        );
        body
    }

    /// `run_workflow`, and the assertion that it answered at all.
    pub async fn run_workflow(&self, arguments: Value) -> Value {
        self.call("run_workflow", arguments).await
    }

    /// The `structuredContent` of a successful call.
    pub async fn run_ok(&self, arguments: Value) -> Value {
        let body = self.run_workflow(arguments).await;
        assert_eq!(
            body["result"]["isError"],
            json!(false),
            "expected a successful tool result, got: {body}"
        );
        body["result"]["structuredContent"].clone()
    }

    /// The `error.code` (and `detail`) of a failed call.
    pub async fn run_error(&self, arguments: Value) -> (String, Value) {
        let body = self.run_workflow(arguments).await;
        assert_eq!(
            body["result"]["isError"],
            json!(true),
            "expected a failed tool result, got: {body}"
        );
        let content = &body["result"]["structuredContent"];
        let code = content["error"]["code"]
            .as_str()
            .unwrap_or_else(|| panic!("a failed result carries error.code: {body}"))
            .to_owned();
        (code, content["error"]["detail"].clone())
    }

    pub async fn run_status(&self, run_id: &str) -> Value {
        self.call("run_status", json!({ "run_id": run_id })).await["result"]["structuredContent"]
            ["run"]
            .clone()
    }

    /// `cancel_run` may block for the whole `--cancel-wait-secs` (30 s by default,
    /// ADR-0004 §5.2), which is longer than the ordinary client's patience: a stub that
    /// ignores SIGTERM is still being SIGKILLed while this call is open.
    pub async fn cancel_run(&self, run_id: &str) -> Value {
        let body = self
            .call_with(&patient_client(), "cancel_run", json!({ "run_id": run_id }))
            .await;
        assert_eq!(
            body["result"]["isError"],
            json!(false),
            "cancelling a live run is a normal result: {body}"
        );
        body["result"]["structuredContent"].clone()
    }

    pub async fn list_tasks(&self, arguments: Value) -> Value {
        self.call("list_tasks", arguments).await["result"]["structuredContent"].clone()
    }

    /// Poll `run_status` until the run is terminal.
    pub async fn wait_terminal(&self, run_id: &str) -> Value {
        let deadline = std::time::Instant::now() + TERMINAL_BUDGET;
        loop {
            let view = self.run_status(run_id).await;
            if view["terminal"].as_bool() == Some(true) {
                return view;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "run {run_id} never went terminal: {view}"
            );
            tokio::time::sleep(TERMINAL_POLL).await;
        }
    }

    // ------------------------------------------------------------- the disk

    /// `act-mcp validate-evidence <root>` — the ADR-0002 §7 strict reader, as the CI
    /// guard the acceptance criteria ask for.
    pub async fn validate_evidence(&self) -> (i32, String) {
        let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_act-mcp"))
            .arg("validate-evidence")
            .arg(self.evidence_root())
            .env_remove("EVIDENCE_DIR")
            .output()
            .await
            .expect("validate-evidence runs");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        (output.status.code().unwrap_or(-1), text)
    }

    /// Snapshot the tree (normalised) and compare it with the golden.
    pub fn assert_tree(&self, scenario: &str, view: &Value) {
        let volatile = self.volatile_from(view);
        golden::check(
            scenario,
            &golden::snapshot(&self.evidence_root(), &volatile),
        );
    }

    /// The volatile facts, taken from the run's own result wherever the result
    /// reports them: a scenario that asserts a value cannot also be shown a placeholder
    /// for it, so normalisation cannot hide a wrong `run_id` or `commit`.
    pub fn volatile_from(&self, view: &Value) -> Volatile {
        let run_id = view["run_id"].as_str().unwrap_or("").to_owned();
        let commit = view["commit"].as_str().unwrap_or("").to_owned();
        Volatile::new(
            run_id,
            commit,
            act_mcp::evidence::hostname(),
            &harness_act_binary(),
            self.repo.path(),
            &self.evidence_root(),
            &self.scratch,
        )
    }

    /// Every file under the evidence root as `path -> bytes`, for "this value is
    /// nowhere on disk" assertions (the redaction scenario).
    pub fn evidence_bytes(&self) -> BTreeMap<String, Vec<u8>> {
        let mut files = BTreeMap::new();
        collect(&self.evidence_root(), Path::new(""), &mut files);
        files
    }
}

fn collect(directory: &Path, prefix: &Path, files: &mut BTreeMap<String, Vec<u8>>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let relative = prefix.join(entry.file_name());
        if path.is_dir() {
            collect(&path, &relative, files);
        } else if let Ok(bytes) = std::fs::read(&path) {
            files.insert(relative.display().to_string(), bytes);
        }
    }
}

fn read_lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Whether `pid` is still a live process (`kill(pid, 0)`).
#[must_use]
pub fn process_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        // A zombie has exited but still has a process-table entry until its parent
        // (or PID 1, if orphaned) reaps it. In containers PID 1 often never does, so
        // `kill(pid, 0)` would report it alive forever. Treat state `Z`/`X` as gone.
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            // Fields: pid (comm) state ...; comm may contain spaces/parens, so split
            // on the LAST ')'.
            let state = stat
                .rsplit(')')
                .next()
                .and_then(|rest| rest.split_whitespace().next());
            if matches!(state, Some("Z" | "X")) {
                return false;
            }
        }
    }
    #[cfg(unix)]
    {
        // 0 probes existence and permission without signalling anything. A process the
        // server has not reaped yet still answers, which is why callers poll this.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

/// Wait for a pid to disappear; `false` if it is still there after `budget`.
pub async fn wait_for_process_gone(pid: u32, budget: Duration) -> bool {
    let deadline = std::time::Instant::now() + budget;
    loop {
        if !process_alive(pid) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Read a file's lines, for a scenario that quotes a record back.
pub fn read_to_string(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}
