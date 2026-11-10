//! Shared fixtures for the integration tests.
//!
//! Included with `mod common;` from each test binary, so anything unused in one
//! binary is simply dead code there.
//!
//! Two generations of helper live here. The first ([`Fixture`] + [`TestServer`]) mounts the
//! router *in-process*, which is what the per-module tests want: fast, and the seam for a
//! scripted [`act_mcp::workspace::Environment`]. The second ([`ServeProcess`] +
//! [`git_workspace`], added by mact-f1w.15) spawns the **real binary** on an ephemeral port
//! with `--act-binary` at a stub, which is what the e2e suite wants: the CLI, the boot gates
//! and the process boundaries are under test too.
#![allow(dead_code)]

use std::{
    io::{BufRead as _, BufReader as StdBufReader},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        mpsc::{self, Receiver},
        Arc, Mutex,
    },
};

use act_mcp::{
    config::{ServeArgs, DEFAULT_BIND},
    server,
    tools::run_workflow::Wiring,
    Config,
};

#[allow(unused_imports, reason = "only some test binaries need the supervisor")]
use act_mcp::registry::RunSupervisor;
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

// The throwaway git repository lives in the library (`act_mcp::fixtures`) so the
// unit tests and the integration tests share one definition; it is re-exported here
// because `tests/common` is where a test looks for a helper. mact-f1w.15 (the e2e
// harness) gets the same fixture this ticket's tests use.
#[allow(
    unused_imports,
    reason = "re-exported for whichever test binary needs them"
)]
pub use act_mcp::fixtures::{
    canonical, workflow_body, TestRepo, DEFAULT_WORKFLOW, DEFAULT_WORKFLOW_BODY,
};

/// A workspace that passes the boot gate: a directory containing
/// `.github/workflows/`.
pub fn fixture_workspace() -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir for fixture workspace");
    std::fs::create_dir_all(dir.path().join(".github/workflows"))
        .expect("create .github/workflows in fixture workspace");
    std::fs::write(
        dir.path().join(".github/workflows/ci.yml"),
        b"name: ci\non:\n  push:\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n",
    )
    .expect("write fixture workflow");
    dir
}

/// An executable stub standing in for `act`. Nothing runs it in this ticket; it
/// only has to resolve through `which` at boot (mact-f1w.8 gives it a job).
pub fn stub_act(dir: &Path, name: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, b"#!/bin/sh\nexit 0\n").expect("write stub act");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("make stub act executable");
    }
    path
}

/// A validated [`Config`] pointing at a fresh fixture workspace, plus the
/// `TempDir` guard the caller must keep alive for as long as the config is used.
pub struct Fixture {
    pub workspace: TempDir,
    pub act: std::path::PathBuf,
    pub dir: TempDir,
}

impl Fixture {
    pub fn new() -> Self {
        let workspace = fixture_workspace();
        let dir = tempfile::tempdir().expect("tempdir for stub act");
        let act = stub_act(dir.path(), "act");
        Self {
            workspace,
            act,
            dir,
        }
    }

    pub fn args(&self) -> ServeArgs {
        ServeArgs {
            workspace: Some(self.workspace.path().to_path_buf()),
            bind: DEFAULT_BIND.to_owned(),
            evidence_dir: None,
            act_binary: self.act.clone(),
            log_dir: None,
            ..ServeArgs::default()
        }
    }

    pub fn config(&self) -> Arc<Config> {
        Arc::new(
            Config::build(&self.args())
                .unwrap_or_else(|err| panic!("fixture config must validate: {err}")),
        )
    }
}

impl Default for Fixture {
    fn default() -> Self {
        Self::new()
    }
}

/// A [`Config`] for an arbitrary workspace and act binary, bypassing
/// [`Config::build`]'s boot gates.
///
/// Two things this is for, both of them acceptance criteria for `list_tasks`:
/// pointing `act_binary` at a path that does not exist (the call must work with no
/// act and no docker), and a workspace the boot gate would refuse (no `.github`, so
/// only the tool's own `workflows_dir_missing` can be reached).
pub fn config_at(workspace: &Path, act_binary: &Path) -> Arc<Config> {
    let workspace = canonical(workspace);
    Arc::new(Config::with_hardened_defaults(
        workspace.clone(),
        workspace.join(".evidence"),
        act_binary.to_path_buf(),
    ))
}

/// A running server on an ephemeral loopback port.
pub struct TestServer {
    pub addr: SocketAddr,
    pub token: CancellationToken,
    /// The process-wide run supervisor, shared by every session this server opens. The
    /// run-control tests drive it directly and read it back through the wire.
    pub supervisor: Arc<RunSupervisor>,
    /// Everything the request handlers share: the config, the supervisor above, the validation
    /// gate and the idempotency table. A test that wants to pre-warm the probe cache, or assert
    /// what an `idempotency_key` remembered, reaches it here — it is the same `Arc` every request
    /// in this process sees, which is the property under test.
    pub wiring: Arc<Wiring>,
    task: JoinHandle<()>,
}

impl TestServer {
    /// Mount the real router (the same code path `serve` uses) on port 0 so the
    /// test never has to guess a free port, with a default-policy supervisor.
    pub async fn start(config: Arc<Config>) -> Self {
        Self::with_supervisor(
            config,
            Arc::new(RunSupervisor::new().expect("the shipped run-policy defaults are valid")),
        )
        .await
    }

    /// The same server, with a supervisor the test configured — the seam for a short
    /// `--term-grace-secs`, which no test wants to sit through at its shipped value.
    pub async fn with_supervisor(config: Arc<Config>, supervisor: Arc<RunSupervisor>) -> Self {
        let wiring = Arc::new(Wiring::with_supervisor(
            Arc::clone(&config),
            Arc::clone(&supervisor),
        ));
        Self::with_wiring(wiring).await
    }

    /// The same server over an already-assembled [`Wiring`] — the seam for a test that built its
    /// own validator (a scripted environment, a pinned clock) or its own run policy and wants
    /// the request handlers to use exactly that.
    pub async fn with_wiring(wiring: Arc<Wiring>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral loopback port");
        let addr = listener.local_addr().expect("local addr");
        let token = CancellationToken::new();
        let task = {
            let token = token.clone();
            let app = server::router(Arc::clone(&wiring), token.clone());
            tokio::spawn(async move {
                let _ = axum::serve(listener, app)
                    .with_graceful_shutdown(async move { token.cancelled().await })
                    .await;
            })
        };
        // Wait until the socket actually answers, so no test races the spawn.
        wait_until_listening(addr).await;
        Self {
            addr,
            token,
            supervisor: Arc::clone(&wiring.supervisor),
            wiring,
            task,
        }
    }

    pub fn url(&self) -> String {
        server::endpoint_url(self.addr)
    }

    pub async fn shutdown(self) {
        self.token.cancel();
        let task = self.task;
        tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .expect("server task stops within 5s")
            .expect("server task must not panic");
    }
}

async fn wait_until_listening(addr: SocketAddr) {
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("server did not start accepting on {addr}");
}

/// Client for hand-written JSON-RPC calls (deliberately permissive: no forced
/// protocol headers, because the tests ask about bad headers too).
pub fn raw_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("reqwest client")
}

/// POST one JSON-RPC message to the endpoint.
///
/// `protocol_version` becomes the `MCP-Protocol-Version` header when given;
/// `method` becomes `Mcp-Method` (the header a stateless server may route on).
pub async fn post_jsonrpc(
    client: &reqwest::Client,
    url: &str,
    method: &str,
    params: Value,
    protocol_version: Option<&str>,
) -> (u16, Value) {
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
    let mut request = client
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Mcp-Method", method)
        .json(&body);
    if let Some(version) = protocol_version {
        request = request.header("MCP-Protocol-Version", version);
    }
    // The stateless 2026-07-28 revision routes a `tools/call` from headers, and rmcp
    // requires `Mcp-Name` (the tool being called) alongside `Mcp-Method`.
    if let Some(name) = params.get("name").and_then(Value::as_str) {
        request = request.header("Mcp-Name", name);
    }
    let response = request.send().await.expect("request reaches the server");
    let status = response.status().as_u16();
    let text = response
        .text()
        .await
        .expect("response body is readable as text");
    // Non-JSON bodies fail the test loudly instead of quietly becoming Null.
    let parsed = serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("body is not JSON ({err}); got: {text}"));
    (status, parsed)
}

/// Per-request `_meta` for the pinned 2026-07-28 revision: self-contained
/// requests must name the version, the client, and its capabilities.
pub fn meta_2026_07_28() -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": { "name": "act-mcp-test", "version": "0.0.0" },
        "io.modelcontextprotocol/clientCapabilities": {}
    })
}

// ================================================================== the real binary
//
// Everything above mounts the router in this process. Everything below spawns the
// `act-mcp` binary the way an operator does, because mact-f1w.15's question is
// "does the whole shipped thing work against a fake act on a machine with no
// docker?" — and an in-process router cannot answer the flag-parsing, boot-gate,
// process-group or exit-code halves of that.

/// A throwaway git workspace with `workflows` under `.github/workflows`, committed.
///
/// Two commits rather than one, so a scenario that wants a commit that is not `HEAD`
/// has one. The plumbing is [`TestRepo`]'s — real `git init`/`add`/`commit`, pinned
/// author and dates, `core.hooksPath` off — which is the same shape the ADR-0003
/// gates expect: a clean tree whose `HEAD` resolves.
pub fn git_workspace(dir: &Path, workflows: &[(&str, &str)]) -> TestRepo {
    let repo = TestRepo::init(dir);
    // `TestRepo::init` seeds a default workflow so a bare repo has a commit; a fixture
    // that names its own set gets exactly that set, so `list_tasks` and every
    // "which workflows exist" assertion is about the fixture and nothing else.
    repo.remove_all_workflows();
    for (name, body) in workflows {
        repo.write_workflow(name, body);
    }
    repo.commit_all("add the fixture workflows");
    assert_eq!(
        repo.git(&["rev-list", "--count", "HEAD"]),
        "2",
        "the fixture workspace is built from two commits"
    );
    assert!(repo.porcelain().is_empty(), "the fixture must be clean");
    repo
}

/// How to boot the real binary for one test.
#[derive(Debug, Clone)]
pub struct ServeOptions {
    /// The workspace to serve (canonical).
    pub workspace: PathBuf,
    /// `--act-binary`: the stub `act`.
    pub act_binary: PathBuf,
    /// `--run-scratch`; must be outside the workspace.
    pub run_scratch: PathBuf,
    /// `--evidence-dir`; `None` keeps the `<workspace>/.evidence` default, which is the
    /// layout the e2e tree goldens are written against.
    pub evidence_dir: Option<PathBuf>,
    /// Extra environment for the child. This is how a scenario steers the stub `act`:
    /// the runner hands its own environment to act, so a variable set here arrives in
    /// the stub unchanged without touching argv.
    pub env: Vec<(String, String)>,
    /// Extra `serve` flags, verbatim.
    pub extra_args: Vec<String>,
}

impl ServeOptions {
    /// The defaults every scenario starts from: serve `workspace` with `act_binary`, put
    /// per-run scratch in `scratch`, and put `bin_dir` at the front of `PATH` — that is
    /// where the stub `act` and the stub `docker` live, so the container-runtime probe
    /// (`docker info`, ADR-0001 §5.3) is answered on a machine with no daemon.
    pub fn new(workspace: &Path, act_binary: &Path, scratch: &Path, bin_dir: &Path) -> Self {
        let path = std::env::join_paths(std::iter::once(bin_dir.to_path_buf()).chain(
            std::env::split_paths(&std::env::var("PATH").unwrap_or_default()),
        ))
        .expect("re-joining PATH is lossless by construction");
        Self {
            workspace: canonical(workspace),
            act_binary: act_binary.to_path_buf(),
            run_scratch: scratch.to_path_buf(),
            evidence_dir: None,
            env: Vec::new(),
            extra_args: Vec::new(),
        }
        .with_env("PATH", path)
    }

    /// One more environment variable. Values may be paths — a `PATH`, a payload directory,
    /// a log fixture — so anything that borrows as an `OsStr` is accepted.
    pub fn with_env(
        mut self,
        key: impl AsRef<std::ffi::OsStr>,
        value: impl AsRef<std::ffi::OsStr>,
    ) -> Self {
        let text = |value: &std::ffi::OsStr| value.to_string_lossy().into_owned();
        self.env.push((text(key.as_ref()), text(value.as_ref())));
        self
    }

    pub fn with_arg(mut self, arg: impl Into<String>) -> Self {
        self.extra_args.push(arg.into());
        self
    }
}

/// The `act-mcp serve` subprocess, its address already known.
///
/// Started with `--bind 127.0.0.1:0`, so the kernel chose the port and the only way to
/// learn it is the line the server logs — which this reads, and keeps reading, on a
/// helper thread. Dropping it stops the server (SIGTERM, then SIGKILL if it refuses), so
/// no test leaks a server into the next one's workspace or port.
pub struct ServeProcess {
    child: Option<std::process::Child>,
    addr: SocketAddr,
    stderr: Arc<Mutex<Receiver<String>>>,
    transcript: Arc<Mutex<String>>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl ServeProcess {
    /// Boot the binary and wait for the line that names the bound address.
    ///
    /// Panics — with everything the server logged — if it refuses to start: to a test, a
    /// boot failure is a failure of the thing under test, not something to paper over.
    pub fn start(options: &ServeOptions) -> Self {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_act-mcp"));
        command
            .arg("serve")
            .arg("--workspace")
            .arg(&options.workspace)
            .arg("--act-binary")
            .arg(&options.act_binary)
            .arg("--run-scratch")
            .arg(&options.run_scratch)
            .arg("--bind")
            .arg("127.0.0.1:0");
        if let Some(evidence) = &options.evidence_dir {
            command.arg("--evidence-dir").arg(evidence);
        }
        for arg in &options.extra_args {
            command.arg(arg);
        }
        // Nothing ambient may decide what the server does. This is `tests/boot.rs`'s list
        // plus every flag the e2e suite depends on being at its default.
        for key in [
            "ACT_MCP_WORKSPACE",
            "ACT_MCP_BIND",
            "ACT_MCP_ACT_BINARY",
            "ACT_MCP_EVIDENCE_DIR",
            "EVIDENCE_DIR",
            "ACT_MCP_LOG_DIR",
            "ACT_MCP_RUN_SCRATCH",
            "ACT_MCP_ISOLATION",
            "ACT_MCP_ALLOW_DIRTY_WORKTREE",
            "ACT_MCP_MINIMUM_ACT_VERSION",
            "ACT_MCP_CONTAINER_DAEMON_SOCKET",
            "ACT_MCP_LIST_TASKS_ACT_CROSS_CHECK",
            "ACT_MCP_I_UNDERSTAND_THIS_IS_UNAUTHENTICATED",
            "ACT_MCP_ALLOWED_HOSTS",
            "ACT_MCP_ALLOWED_ORIGINS",
            "ACT_MCP_MAX_REQUEST_BODY_BYTES",
            "ACT_MCP_REQUIRE_SESSION_TOKEN",
            "ACT_MCP_SESSION_TOKEN",
            "ACT_MCP_MAX_RUN_DIRS",
            "RUST_LOG",
        ] {
            command.env_remove(key);
        }
        for (key, value) in &options.env {
            command.env(key, value);
        }

        let mut child = command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("the act-mcp binary spawns");
        let pipe = child.stderr.take().expect("stderr is piped");
        let (sender, receiver) = mpsc::channel::<String>();
        let transcript = Arc::new(Mutex::new(String::new()));
        let reader = {
            let transcript = Arc::clone(&transcript);
            std::thread::spawn(move || {
                for line in StdBufReader::new(pipe).lines() {
                    let Ok(line) = line else { break };
                    {
                        let mut log = transcript.lock().unwrap_or_else(|p| p.into_inner());
                        log.push_str(&line);
                        log.push('\n');
                    }
                    if sender.send(line).is_err() {
                        break;
                    }
                }
            })
        };

        let mut process = Self {
            child: Some(child),
            addr: SocketAddr::from(([127, 0, 0, 1], 0)),
            stderr: Arc::new(Mutex::new(receiver)),
            transcript,
            reader: Some(reader),
        };
        process.addr = process.wait_for_listen_addr();
        process
    }

    fn wait_for_listen_addr(&mut self) -> SocketAddr {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let line = {
                let receiver = self.stderr.lock().unwrap_or_else(|p| p.into_inner());
                match receiver.recv_timeout(std::time::Duration::from_millis(100)) {
                    Ok(line) => Some(line),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => panic!(
                        "stderr closed before the bind address was logged;\n{}",
                        self.transcript()
                    ),
                }
            };
            let Some(line) = line else {
                let exited = self
                    .child
                    .as_mut()
                    .map(|child| child.try_wait().ok().flatten().is_some())
                    .unwrap_or(false);
                assert!(
                    !exited,
                    "the server exited before it bound; {}",
                    self.transcript()
                );
                assert!(
                    std::time::Instant::now() < deadline,
                    "no bind address within 30s;\n{}",
                    self.transcript()
                );
                continue;
            };
            if let Some(addr) = listen_addr_from_log_line(&line) {
                return addr;
            }
        }
    }

    /// The MCP endpoint: `/mcp` on the ephemeral port the kernel chose.
    pub fn url(&self) -> String {
        format!("http://{}/mcp", self.addr)
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The child's pid, for a test that signals the server itself instead of blocking in
    /// [`ServeProcess::stop`] — the shutdown-under-load test has to SIGTERM and then keep
    /// watching the wire.
    pub fn pid(&self) -> u32 {
        self.child
            .as_ref()
            .expect("the child is alive until terminate() runs")
            .id()
    }

    /// Send SIGTERM and return immediately, leaving the exit to the caller to observe.
    ///
    /// `stop` is the polite full teardown; this is the one a test uses when the *timing* of
    /// the exit is what is under test.
    pub fn signal_term(&self) {
        if let Some(child) = self.child.as_ref() {
            #[cfg(unix)]
            unsafe {
                libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
            }
        }
    }

    /// Has the process exited yet (without waiting for it).
    pub fn has_exited(&mut self) -> bool {
        match self.child.as_mut() {
            Some(child) => child.try_wait().ok().flatten().is_some(),
            None => true,
        }
    }

    /// Everything the server has logged so far.
    pub fn transcript(&self) -> String {
        self.transcript
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Every already-queued stderr line, leaving later ones for later.
    pub fn drain_stderr(&self) -> Vec<String> {
        let receiver = self.stderr.lock().unwrap_or_else(|p| p.into_inner());
        let mut lines = Vec::new();
        while let Ok(line) = receiver.try_recv() {
            lines.push(line);
        }
        lines
    }

    /// Stop it: SIGTERM, the way a supervisor would, then SIGKILL if it will not.
    pub fn stop(mut self) {
        self.terminate();
    }

    fn terminate(&mut self) {
        if let Some(child) = self.child.as_mut() {
            #[cfg(unix)]
            unsafe {
                libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
            }
            #[cfg(not(unix))]
            let _ = child.kill();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    _ => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                }
            }
        }
        self.child = None;
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

impl Drop for ServeProcess {
    fn drop(&mut self) {
        self.terminate();
    }
}

/// Pull the `bind=` field out of the startup log line.
///
/// `tracing_subscriber::fmt` renders a display-formatted field as
/// `bind=127.0.0.1:39281`, optionally quoted. Moved here from `tests/boot.rs` when the
/// e2e suite started booting the same binary.
pub fn listen_addr_from_log_line(line: &str) -> Option<SocketAddr> {
    if !line.contains("act-mcp listening") {
        return None;
    }
    let token = line
        .split("bind=")
        .nth(1)?
        .split([' ', '\t'])
        .next()?
        .trim_matches('"');
    token.parse::<SocketAddr>().ok()
}
