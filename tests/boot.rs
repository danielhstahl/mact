//! Boot behaviour of the real binary: fail loudly on a bad workspace / missing
//! act, log the bind address when happy, keep stdout empty, and stop promptly on
//! SIGTERM.
//!
//! These run `act-mcp` as a subprocess (`CARGO_BIN_EXE_act-mcp`) because the
//! things under test are process-level: exit codes, which stream a message went
//! to, and signal handling.

mod common;

use std::{
    net::SocketAddr,
    process::{ExitStatus, Stdio},
    time::Duration,
};

use tokio::{
    io::{AsyncBufReadExt as _, AsyncReadExt as _, BufReader},
    process::Command,
};

use common::{post_jsonrpc, raw_client, Fixture};

use serde_json::json;

/// Ambient env would silently change what the CLI does; every test starts clean.
fn serve_command(args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_act-mcp"));
    cmd.args(args)
        .env_remove("ACT_MCP_WORKSPACE")
        .env_remove("ACT_MCP_BIND")
        .env_remove("ACT_MCP_ACT_BINARY")
        .env_remove("ACT_MCP_LOG_DIR")
        .env_remove("EVIDENCE_DIR")
        .env_remove("RUST_LOG")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

async fn run(args: &[&str]) -> (ExitStatus, String, String) {
    let output = serve_command(args)
        .output()
        .await
        .expect("act-mcp binary runs");
    (
        output.status,
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[tokio::test]
async fn boot_fails_when_the_workspace_does_not_exist() {
    let fixture = Fixture::new();
    let missing = fixture.workspace.path().join("no-such-repo");
    let (status, stdout, stderr) = run(&[
        "serve",
        "--workspace",
        missing.to_str().unwrap(),
        "--act-binary",
        fixture.act.to_str().unwrap(),
    ])
    .await;

    assert!(!status.success(), "must exit non-zero; stderr: {stderr}");
    assert!(
        stderr.contains("workspace does not exist"),
        "stderr should name the problem: {stderr}"
    );
    assert!(
        stderr.contains("fix:"),
        "stderr should say what to do about it: {stderr}"
    );
    assert_eq!(stdout, "", "stdout must stay empty");
}

#[tokio::test]
async fn boot_fails_when_the_workspace_has_no_dot_github() {
    let bare = tempfile::tempdir().expect("tempdir");
    let fixture = Fixture::new();
    let (status, stdout, stderr) = run(&[
        "serve",
        "--workspace",
        bare.path().to_str().unwrap(),
        "--act-binary",
        fixture.act.to_str().unwrap(),
    ])
    .await;

    assert!(!status.success(), "must exit non-zero; stderr: {stderr}");
    assert!(
        stderr.contains(".github"),
        "stderr should name .github: {stderr}"
    );
    assert_eq!(stdout, "", "stdout must stay empty");
}

#[tokio::test]
async fn boot_fails_when_act_is_not_installed() {
    let fixture = Fixture::new();
    // `--act-binary act` plus a stripped PATH is "act is not installed", with no
    // dependence on what happens to be on the developer's PATH.
    let status = serve_command(&[
        "serve",
        "--workspace",
        fixture.workspace.path().to_str().unwrap(),
    ])
    .env_remove("PATH")
    .spawn()
    .expect("spawn")
    .wait_with_output()
    .await
    .expect("wait");

    assert!(!status.status.success());
    let stderr = String::from_utf8_lossy(&status.stderr);
    assert!(
        stderr.contains("act binary not found"),
        "stderr should name act: {stderr}"
    );
    assert!(
        stderr.contains("nektos/act") || stderr.contains("--act-binary"),
        "stderr should say how to fix it: {stderr}"
    );
    assert_eq!(String::from_utf8_lossy(&status.stdout), "");
}

#[tokio::test]
async fn boot_fails_when_the_bind_is_not_loopback() {
    let fixture = Fixture::new();
    let (status, stdout, stderr) = run(&[
        "serve",
        "--workspace",
        fixture.workspace.path().to_str().unwrap(),
        "--act-binary",
        fixture.act.to_str().unwrap(),
        "--bind",
        "0.0.0.0:8080",
    ])
    .await;

    assert!(!status.success(), "must exit non-zero; stderr: {stderr}");
    assert!(
        stderr.contains("loopback"),
        "stderr should explain the loopback rule: {stderr}"
    );
    assert_eq!(stdout, "", "stdout must stay empty");
}

/// The end-to-end acceptance path, run for both stop signals: valid workspace →
/// server logs its address, answers `tools/list`, writes nothing to stdout, and
/// stops promptly without panicking.
#[tokio::test]
#[cfg(unix)]
async fn sigterm_stops_a_serving_server_promptly_and_cleanly() {
    stops_promptly(libc::SIGTERM, "SIGTERM").await;
}

#[tokio::test]
#[cfg(unix)]
async fn ctrl_c_stops_a_serving_server_promptly_and_cleanly() {
    stops_promptly(libc::SIGINT, "SIGINT").await;
}

async fn stops_promptly(signal: libc::c_int, signal_name: &str) {
    let fixture = Fixture::new();
    let mut child = serve_command(&[
        "serve",
        "--workspace",
        fixture.workspace.path().to_str().unwrap(),
        "--act-binary",
        fixture.act.to_str().unwrap(),
        // Port 0 = the kernel picks a free port; the server logs the real one,
        // which is also what this asserts.
        "--bind",
        "127.0.0.1:0",
    ])
    .spawn()
    .expect("spawn act-mcp serve");

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    let mut stderr_lines = BufReader::new(stderr).lines();

    let addr = wait_for_listen_line(&mut stderr_lines).await;

    // It answers the pinned protocol while running.
    let (status, body) = post_jsonrpc(
        &raw_client(),
        &format!("http://{addr}/mcp"),
        "tools/list",
        json!({ "_meta": common::meta_2026_07_28() }),
        Some("2026-07-28"),
    )
    .await;
    assert_eq!(status, 200, "tools/list over the real binary: {body}");
    assert_eq!(body["result"]["tools"], json!([]), "body: {body}");

    let pid = child.id().expect("child pid");
    // The way a supervisor (or a terminal) stops us.
    let rc = unsafe { libc::kill(pid as libc::pid_t, signal) };
    assert_eq!(rc, 0, "sending {signal_name} to {pid} failed");

    let started = std::time::Instant::now();
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("server exits within 5s of the signal")
        .expect("wait() succeeds");
    let stopped_in = started.elapsed();
    assert!(
        stopped_in < Duration::from_secs(3),
        "shutdown took {stopped_in:?}, the budget is a couple of seconds"
    );
    assert!(
        status.success(),
        "graceful shutdown should exit 0: {status}"
    );

    // Drain what is left of both streams after exit.
    let mut stdout_rest = String::new();
    let mut stdout = stdout;
    stdout
        .read_to_string(&mut stdout_rest)
        .await
        .expect("read stdout to EOF");
    assert_eq!(stdout_rest, "", "nothing may be written to stdout");

    let mut tail = String::new();
    while let Some(line) = stderr_lines.next_line().await.expect("read stderr to EOF") {
        tail.push_str(&line);
        tail.push('\n');
    }
    assert!(
        !tail.contains("panicked"),
        "shutdown must not panic: {tail}"
    );
    assert!(
        tail.contains(signal_name) && tail.contains("act-mcp stopped"),
        "expected an orderly shutdown log, got: {tail}"
    );
}

/// The remaining stderr of the happy-path test is read lazily above; anything
/// still buffered when the process dies is dropped with the child, which is fine
/// for a test that only asserts on two lines.
/// Boot the server, return the address it logged, then stop it again.
///
/// `Err((status, stderr))` if it refused to start.
#[cfg(unix)]
async fn boot_then_stop(
    args: &[&str],
    envs: &[(&str, &str)],
) -> Result<SocketAddr, (ExitStatus, String)> {
    let mut cmd = serve_command(args);
    for (key, value) in envs {
        cmd.env(key, value);
    }
    let mut child = cmd.spawn().expect("spawn act-mcp serve");
    let mut stderr_lines = BufReader::new(child.stderr.take().expect("stderr piped")).lines();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let line = tokio::time::timeout_at(deadline, stderr_lines.next_line())
            .await
            .expect("boot or fail within 15s")
            .expect("read stderr line");
        let line = match line {
            Some(line) => line,
            // stderr closed: the process is gone; collect the rest for the error.
            None => {
                let mut rest = String::new();
                while let Some(line) = stderr_lines.next_line().await.expect("read stderr") {
                    rest.push_str(&line);
                    rest.push('\n');
                }
                let status = child.wait().await.expect("wait for exit");
                return Err((status, rest));
            }
        };
        if let Some(addr) = listen_addr_from_log_line(&line) {
            let pid = child.id().expect("child pid");
            assert_eq!(
                unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) },
                0,
                "stopping the test server"
            );
            tokio::time::timeout(Duration::from_secs(5), child.wait())
                .await
                .expect("test server stops")
                .expect("wait succeeds");
            return Ok(addr);
        }
    }
}

/// A flag beats its env var, which is the documented precedence.
#[tokio::test]
#[cfg(unix)]
async fn flags_win_over_env() {
    let fixture = Fixture::new();
    // Both env values would fail boot on their own (missing repo / non-loopback),
    // so a clean boot proves the flags won.
    let result = boot_then_stop(
        &[
            "serve",
            "--workspace",
            fixture.workspace.path().to_str().unwrap(),
            "--act-binary",
            fixture.act.to_str().unwrap(),
            "--bind",
            "127.0.0.1:0",
        ],
        &[
            ("ACT_MCP_WORKSPACE", "/definitely/not/a/repo"),
            ("ACT_MCP_BIND", "0.0.0.0:8080"),
        ],
    )
    .await;
    assert!(
        result.is_ok(),
        "flags should win; stderr: {:?}",
        result.err()
    );
}

/// ...and the env vars are honoured when the flag is absent.
#[tokio::test]
#[cfg(unix)]
async fn env_supplies_what_the_command_line_omits() {
    let fixture = Fixture::new();
    let result = boot_then_stop(
        &["serve"],
        &[
            (
                "ACT_MCP_WORKSPACE",
                fixture.workspace.path().to_str().unwrap(),
            ),
            ("ACT_MCP_ACT_BINARY", fixture.act.to_str().unwrap()),
            ("ACT_MCP_BIND", "127.0.0.1:0"),
        ],
    )
    .await;
    let addr = match result {
        Ok(addr) => addr,
        Err((status, stderr)) => panic!("env-only boot should work; {status}: {stderr}"),
    };
    // Port 0 means the kernel picked one, so the logged port is the real one.
    assert_ne!(
        addr.port(),
        0,
        "logged address should be the bound one: {addr}"
    );
}

async fn wait_for_listen_line<R>(lines: &mut tokio::io::Lines<BufReader<R>>) -> SocketAddr
where
    R: tokio::io::AsyncRead + Unpin,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let line = tokio::time::timeout_at(deadline, lines.next_line())
            .await
            .expect("server logs its bind address within 15s")
            .expect("read stderr line")
            .unwrap_or_else(|| panic!("stderr closed before the listen line appeared"));
        if let Some(addr) = listen_addr_from_log_line(&line) {
            return addr;
        }
        // Anything else is just logging; keep scanning.
    }
}

/// Pull the `bind=` field out of the startup log line.
///
/// `tracing_subscriber::fmt` renders a display-formatted field as
/// `bind=127.0.0.1:39281`, optionally quoted.
fn listen_addr_from_log_line(line: &str) -> Option<SocketAddr> {
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
