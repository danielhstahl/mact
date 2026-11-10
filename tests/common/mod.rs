//! Shared fixtures for the integration tests.
//!
//! Included with `mod common;` from each test binary, so anything unused in one
//! binary is simply dead code there.
#![allow(dead_code)]

use std::{net::SocketAddr, path::Path, sync::Arc};

use act_mcp::{
    config::{ServeArgs, DEFAULT_BIND},
    server,
    workspace::{SemVer, DEFAULT_GIT_TIMEOUT},
    Config,
};
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

/// A [`Config`] for an arbitrary workspace and act binary, bypassing
/// [`Config::build`]'s boot gates.
///
/// Two things this is for, both of them acceptance criteria for `list_tasks`:
/// pointing `act_binary` at a path that does not exist (the call must work with no
/// act and no docker), and a workspace the boot gate would refuse (no `.github`, so
/// only the tool's own `workflows_dir_missing` can be reached).
pub fn config_at(workspace: &Path, act_binary: &Path) -> Arc<Config> {
    Arc::new(Config {
        workspace: canonical(workspace),
        evidence_dir: workspace.join(".evidence"),
        bind: DEFAULT_BIND.parse().expect("loopback default parses"),
        act_binary: act_binary.to_path_buf(),
        log_dir: None,
        isolation: Default::default(),
        allow_dirty_worktree: false,
        git_timeout: DEFAULT_GIT_TIMEOUT,
        minimum_act_version: SemVer {
            major: 0,
            minor: 2,
            patch: 62,
        },
        container_daemon_socket: None,
        list_tasks_act_cross_check: false,
    })
}

/// A running server on an ephemeral loopback port.
pub struct TestServer {
    pub addr: SocketAddr,
    pub token: CancellationToken,
    task: JoinHandle<()>,
}

impl TestServer {
    /// Mount the real router (the same code path `serve` uses) on port 0 so the
    /// test never has to guess a free port.
    pub async fn start(config: Arc<Config>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral loopback port");
        let addr = listener.local_addr().expect("local addr");
        let token = CancellationToken::new();
        let task = {
            let token = token.clone();
            let app = server::router(config, token.clone());
            tokio::spawn(async move {
                let _ = axum::serve(listener, app)
                    .with_graceful_shutdown(async move { token.cancelled().await })
                    .await;
            })
        };
        // Wait until the socket actually answers, so no test races the spawn.
        wait_until_listening(addr).await;
        Self { addr, token, task }
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
