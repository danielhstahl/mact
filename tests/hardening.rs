//! mact-f1w.13 — the local-only hardening pass.
//!
//! "HTTP but no auth, single local machine" is only a safe claim if the surface really is
//! loopback-only and every caller-controlled string is treated as hostile. Every test here
//! attacks the running server from outside — raw HTTP, or the real binary as a subprocess —
//! and asserts the *refusal*, because a control asserted only at the unit level can be
//! bypassed by the transport it sits above.
//!
//! The acceptance criteria, one group of tests each:
//!
//! * a non-loopback bind needs `--i-understand-this-is-unauthenticated`, and using it prints
//!   a big warning (subprocess tests: the thing under test is the boot decision);
//! * a foreign `Host:` is refused, a loopback `Host:` is accepted, and a browser `Origin:`
//!   is refused unless deliberately allowed;
//! * an oversized `POST /mcp` is 413;
//! * path traversal via `workflow` and via `category` is refused;
//! * the opt-in shared secret gates every request when enabled, and gates nothing by default;
//! * the resource caps answer with typed errors instead of degrading the machine;
//! * a run's secret value appears in no file the run produces, whichever route it arrived by.
//!
//! The `Host`/`Origin` tests hand-write their HTTP rather than using reqwest because the
//! whole point is to send a `Host:` that does not match the address we dialled, and an HTTP
//! client that helpfully rewrites it would silently turn the test into a no-op.

mod common;

use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use act_mcp::{
    config::{
        Config, SessionToken, DEFAULT_ALLOWED_HOSTS, DEFAULT_MAX_REQUEST_BODY_BYTES,
        SESSION_TOKEN_HEADER, UNAUTH_ACK_FLAG,
    },
    evidence::records::Kind,
    registry::{PolicyError, RunIdentity, RunPlan, RunPolicy, RunSupervisor, StartError},
    runner::{
        RunId, RunSpec, Runner, RunnerLimits, DEFAULT_MAX_RUN_LOG_BYTES, MAX_LOG_TAIL_BYTES,
        MIN_RUN_LOG_BYTES, REDACTION_MARKER, WARNING_TRUNCATED,
    },
    tools::category,
};
use serde_json::json;

use common::{fixture_workspace, meta_2026_07_28, post_jsonrpc, raw_client, Fixture, TestServer};

/// The cheapest request that reaches a handler: a `tools/list`.
fn tools_list_body() -> String {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list",
        "params": { "_meta": meta_2026_07_28() },
    })
    .to_string()
}

/// One hand-written HTTP/1.1 POST, answered and then closed.
///
/// `Connection: close` makes "read to EOF" a complete answer, so no test has to parse
/// framing to learn the status. Async on purpose: these tests run on a single-threaded
/// runtime that is also hosting the server task, and a blocking read here would starve the
/// very task whose answer we are waiting for.
async fn raw_post(
    addr: SocketAddr,
    host: &str,
    extra_headers: &[(&str, &str)],
    body: &str,
) -> (u16, String) {
    let mut request = format!(
        "POST /mcp HTTP/1.1\r\n\
         Host: {host}\r\n\
         User-Agent: act-mcp-hardening-test\r\n\
         Accept: application/json, text/event-stream\r\n\
         Content-Type: application/json\r\n\
         MCP-Protocol-Version: 2026-07-28\r\n\
         Mcp-Method: tools/list\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n",
        body.len()
    );
    for (name, value) in extra_headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(body);

    raw_exchange(addr, request.as_bytes()).await
}

/// Send bytes, read until the peer closes, report the status.
async fn raw_exchange(addr: SocketAddr, request: &[u8]) -> (u16, String) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let mut stream = tokio::net::TcpStream::connect(addr)
        .await
        .expect("connect to the test server");
    stream.write_all(request).await.expect("write request");
    stream.flush().await.expect("flush");

    let mut bytes = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(20), stream.read_to_end(&mut bytes))
        .await
        .expect("the server answers or closes within 20s")
        .expect("read the response");
    let _ = read;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    (status_of(&text), text)
}

/// One chunked POST: no `Content-Length` anywhere, so the body size is only discoverable
/// by counting bytes as they arrive.
async fn raw_chunked_post(addr: SocketAddr, host: &str, payload: &str) -> (u16, String) {
    let mut request = format!(
        "POST /mcp HTTP/1.1\r\nHost: {host}\r\nAccept: application/json, text/event-stream\r\n\
         Content-Type: application/json\r\nMCP-Protocol-Version: 2026-07-28\r\n\
         Mcp-Method: tools/list\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
    );
    for chunk in payload.as_bytes().chunks(512) {
        request.push_str(&format!(
            "{:x}\r\n{}\r\n",
            chunk.len(),
            String::from_utf8_lossy(chunk)
        ));
    }
    request.push_str("0\r\n\r\n");
    raw_exchange(addr, request.as_bytes()).await
}

/// The status code from a raw response's status line (`HTTP/1.1 403 Forbidden`).
fn status_of(response: &str) -> u16 {
    let first_line = response
        .lines()
        .next()
        .unwrap_or_else(|| panic!("no response at all: {response:?}"));
    let mut words = first_line.split_whitespace();
    let version = words.next().unwrap_or_default();
    assert!(
        version.starts_with("HTTP/"),
        "not a response: {first_line:?}"
    );
    words
        .next()
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("no status code in {first_line:?}"))
}

/// A [`Config`] for the fixture workspace with one field changed per test.
fn config_from(fixture: &Fixture, adjust: impl FnOnce(&mut Config)) -> Arc<Config> {
    let mut config = (*fixture.config()).clone();
    adjust(&mut config);
    Arc::new(config)
}

// --------------------------------------------------------------------------- bind

/// AC: "Server refuses to bind a non-loopback address without the explicit acknowledgement
/// flag". Run against the real binary, because the flag is a CLI fact.
#[tokio::test]
#[cfg(unix)]
async fn a_non_loopback_bind_without_the_acknowledgement_refuses_to_boot() {
    let fixture = Fixture::new();
    for bind in ["0.0.0.0:0", "10.0.0.1:0"] {
        let (status, stderr) = boot_and_capture(&fixture, &["--bind", bind], &[]).await;
        assert!(
            !status.success(),
            "`--bind {bind}` without the flag must fail; stderr: {stderr}"
        );
        assert!(stderr.contains("loopback"), "must say why: {stderr}");
        assert!(
            stderr.contains(UNAUTH_ACK_FLAG),
            "must name the only way past it (--{UNAUTH_ACK_FLAG}): {stderr}"
        );
        assert!(
            stderr.contains("--bind"),
            "must name the flag it refuses: {stderr}"
        );
    }
}

/// The loopback default needs no acknowledgement, and gets no warning banner.
#[tokio::test]
#[cfg(unix)]
async fn a_loopback_bind_boots_quietly() {
    let fixture = Fixture::new();
    let stderr = boot_until_listening(&fixture, &["--bind", "127.0.0.1:0"], &[])
        .await
        .expect("a loopback bind must boot");
    assert!(
        !stderr.to_ascii_uppercase().contains("NON-LOOPBACK"),
        "no banner on a loopback boot: {stderr}"
    );
}

/// AC: "logs a warning when that flag is used". The banner has to be findable in stderr, and
/// the server has to actually come up — a warning that refused to start would be a different
/// (also defensible, but different) design.
#[tokio::test]
#[cfg(unix)]
async fn the_acknowledgement_flag_boots_and_shouts_about_it() {
    let fixture = Fixture::new();
    let stderr = boot_until_listening(
        &fixture,
        &["--bind", "0.0.0.0:0", &format!("--{UNAUTH_ACK_FLAG}")],
        &[],
    )
    .await
    .expect("the acknowledged bind boots");

    let upper = stderr.to_ascii_uppercase();
    assert!(
        upper.contains("NON-LOOPBACK"),
        "the startup warning must say what is exposed: {stderr}"
    );
    assert!(
        upper.contains(&UNAUTH_ACK_FLAG.to_ascii_uppercase()),
        "and must name the flag that allowed it: {stderr}"
    );
}

/// The env spelling is an acknowledgement too (the CLI's flag/env precedence).
#[tokio::test]
#[cfg(unix)]
async fn the_acknowledgement_can_come_from_the_environment() {
    let fixture = Fixture::new();
    let refused = boot_and_capture(&fixture, &["--bind", "0.0.0.0:0"], &[]).await;
    assert!(!refused.0.success(), "control: without it, boot fails");

    let stderr = boot_until_listening(
        &fixture,
        &["--bind", "0.0.0.0:0"],
        &[("ACT_MCP_I_UNDERSTAND_THIS_IS_UNAUTHENTICATED", "true")],
    )
    .await
    .expect("env acknowledgement is an acknowledgement");
    assert!(stderr.to_ascii_uppercase().contains("NON-LOOPBACK"));
}

// --------------------------------------------------------------------- Host / Origin

/// A foreign `Host:` is refused before anything is dispatched. That is the DNS-rebinding
/// case: a browser tricked into resolving `evil.example` to `127.0.0.1` still sends
/// `Host: evil.example`, and so does anything else dialling our loopback socket by name.
#[tokio::test]
async fn a_foreign_host_header_is_refused() {
    let fixture = Fixture::new();
    let server = TestServer::start(fixture.config()).await;
    let body = tools_list_body();

    for host in [
        "evil.example",
        "evil.example:8080",
        "localhost.evil.example",
        "192.168.1.7:8080",
        // The bound address with the wrong port is still the wrong authority…
        "127.0.0.2:9",
    ] {
        let (status, response) = raw_post(server.addr, host, &[], &body).await;
        assert_eq!(
            status, 403,
            "`Host: {host}` must be refused; got {response}"
        );
        assert!(
            response.contains("Host"),
            "the refusal should say which header: {response}"
        );
    }

    server.shutdown().await;
}

/// The loopback spellings all work, including the one with the ephemeral port appended — the
/// case a careless allowlist (exact string match, port included) breaks.
#[tokio::test]
async fn a_loopback_host_header_is_accepted() {
    let fixture = Fixture::new();
    let server = TestServer::start(fixture.config()).await;
    let body = tools_list_body();
    let port = server.addr.port();

    for host in [
        format!("127.0.0.1:{port}"),
        "127.0.0.1".to_owned(),
        "localhost".to_owned(),
        format!("localhost:{port}"),
        format!("[::1]:{port}"),
    ] {
        let (status, response) = raw_post(server.addr, &host, &[], &body).await;
        assert_eq!(
            status, 200,
            "`Host: {host}` should be accepted; got {response}"
        );
    }

    assert_eq!(
        DEFAULT_ALLOWED_HOSTS.as_slice(),
        &["localhost", "127.0.0.1", "::1"],
        "those are exactly the defaults the shipped config carries"
    );
    assert_eq!(
        fixture.config().allowed_hosts,
        DEFAULT_ALLOWED_HOSTS
            .iter()
            .map(|host| (*host).to_owned())
            .collect::<Vec<_>>(),
        "and the config's allowlist is that list, never empty (empty would mean 'any Host')"
    );

    server.shutdown().await;
}

/// A browser `Origin:` means either a browser (cross-site request) or someone probing.
/// Neither is a legitimate caller of a local MCP endpoint, so the default allowlist is empty
/// and — this is the part that is easy to get backwards — empty means *refuse*, not
/// *allow anything*.
#[tokio::test]
async fn a_browser_origin_is_refused_by_default() {
    let fixture = Fixture::new();
    let server = TestServer::start(fixture.config()).await;
    let body = tools_list_body();
    let host = format!("127.0.0.1:{}", server.addr.port());

    for origin in [
        "http://evil.example",
        "https://evil.example",
        "http://localhost:3000",
        "http://127.0.0.1:3000",
        "null",
        "not-an-origin",
    ] {
        let (status, response) = raw_post(server.addr, &host, &[("Origin", origin)], &body).await;
        assert_eq!(
            status, 403,
            "`Origin: {origin}` must be refused; got {response}"
        );
    }

    // No `Origin` at all — the normal case for a non-browser MCP client — still works.
    let (status, response) = raw_post(server.addr, &host, &[], &body).await;
    assert_eq!(status, 200, "an absent Origin must be fine; got {response}");

    server.shutdown().await;
}

/// …and an operator who genuinely wants a browser-based client can name it, per origin.
#[tokio::test]
async fn a_named_origin_is_accepted_when_the_operator_allows_it() {
    let fixture = Fixture::new();
    // `localhost:*` is the documented shape for "a local web client, any port".
    let config = config_from(&fixture, |config| {
        config.allowed_origins = vec!["http://localhost:*".to_owned()];
    });
    let server = TestServer::start(config).await;
    let body = tools_list_body();
    let host = format!("localhost:{}", server.addr.port());

    let (status, response) = raw_post(
        server.addr,
        &host,
        &[("Origin", "http://localhost:3000")],
        &body,
    )
    .await;
    assert_eq!(status, 200, "the allowed origin must pass; got {response}");

    let (status, _) = raw_post(
        server.addr,
        &host,
        &[("Origin", "http://evil.example")],
        &body,
    )
    .await;
    assert_eq!(
        status, 403,
        "allowing one origin must not allow all of them"
    );

    server.shutdown().await;
}

/// An `--allow-host` entry is how a deliberate port-forward names its front-end.
#[tokio::test]
async fn an_extra_allowed_host_is_honoured() {
    let fixture = Fixture::new();
    let config = config_from(&fixture, |config| {
        config.allowed_hosts = vec![
            "localhost".to_owned(),
            "127.0.0.1".to_owned(),
            "::1".to_owned(),
            "tunnel.gateway.internal".to_owned(),
        ];
    });
    let server = TestServer::start(config).await;
    let body = tools_list_body();

    let (status, response) = raw_post(server.addr, "tunnel.gateway.internal", &[], &body).await;
    assert_eq!(
        status, 200,
        "the named front-end must be accepted; got {response}"
    );

    let (status, _) = raw_post(server.addr, "someone-elses-laptop", &[], &body).await;
    assert_eq!(status, 403, "and everything else stays refused");

    server.shutdown().await;
}

// ------------------------------------------------------------------- request shape

/// AC: `max_request_body_bytes` is configured, and an oversized `POST /mcp` is 413 —
/// whether the size is announced in `Content-Length` or discoverable only by counting a
/// chunked stream.
#[tokio::test]
async fn an_oversized_request_body_is_413() {
    let fixture = Fixture::new();
    let config = config_from(&fixture, |config| {
        config.max_request_body_bytes = 4096;
    });
    assert_eq!(
        DEFAULT_MAX_REQUEST_BODY_BYTES, 1_048_576,
        "the shipped default is a megabyte; docs/security.md says why"
    );

    let server = TestServer::start(config).await;
    let host = format!("127.0.0.1:{}", server.addr.port());

    // Small and well-formed: through.
    let (status, response) = raw_post(server.addr, &host, &[], &tools_list_body()).await;
    assert_eq!(status, 200, "sanity check failed: {response}");

    // Over the cap by a wide margin: 413.
    let filler = "x".repeat(8192);
    let head = tools_list_body().trim_end_matches('}').to_owned();
    let oversized = format!("{head},\"pad\":\"{filler}\"}}");
    let (status, response) = raw_post(server.addr, &host, &[], &oversized).await;
    assert_eq!(
        status, 413,
        "oversized body must be rejected; got {response}"
    );

    // No `Content-Length` to read: enforced on bytes actually streamed.
    let (status, response) = raw_chunked_post(server.addr, &host, &oversized).await;
    assert_eq!(
        status, 413,
        "a chunked oversized body must also be rejected; got {response}"
    );

    server.shutdown().await;
}

// ------------------------------------------------------------------ shared secret

/// The opt-in shared secret: no header → 401, wrong value → 401, right value → through.
#[tokio::test]
async fn the_opt_in_shared_secret_gates_every_request() {
    let fixture = Fixture::new();
    let config = config_from(&fixture, |config| {
        config.session_token = SessionToken::new("a-test-token-value");
    });
    let server = TestServer::start(config).await;
    let host = format!("127.0.0.1:{}", server.addr.port());
    let body = tools_list_body();

    let (status, response) = raw_post(server.addr, &host, &[], &body).await;
    assert_eq!(status, 401, "no header must be refused; got {response}");
    assert!(
        !response.contains("a-test-token-value"),
        "the refusal must not name the value it expected: {response}"
    );

    let (status, _) = raw_post(
        server.addr,
        &host,
        &[(SESSION_TOKEN_HEADER, "wrong-value")],
        &body,
    )
    .await;
    assert_eq!(status, 401, "a wrong token must be refused");

    let (status, response) = raw_post(
        server.addr,
        &host,
        &[(SESSION_TOKEN_HEADER, "a-test-token-value")],
        &body,
    )
    .await;
    assert_eq!(status, 200, "the right token must pass; got {response}");

    // A foreign `Host` is still refused *after* the secret check passes: the layers stack,
    // they do not replace each other.
    let (status, _) = raw_post(
        server.addr,
        "evil.example",
        &[(SESSION_TOKEN_HEADER, "a-test-token-value")],
        &body,
    )
    .await;
    assert_eq!(
        status, 403,
        "the Host guard still applies behind the secret"
    );

    server.shutdown().await;
}

/// With the feature off (the default) nothing is required — the decision recorded in
/// `docs/security.md` is that the secret is *opt-in*, so "loopback is the control" must stay
/// the default behaviour rather than something a test has to work around.
#[tokio::test]
async fn no_shared_secret_is_required_by_default() {
    let fixture = Fixture::new();
    assert!(
        fixture.config().session_token.is_none(),
        "the default config carries no secret"
    );
    let server = TestServer::start(fixture.config()).await;
    let (status, response) = raw_post(
        server.addr,
        &format!("127.0.0.1:{}", server.addr.port()),
        &[],
        &tools_list_body(),
    )
    .await;
    assert_eq!(
        status, 200,
        "a default server accepts a plain request: {response}"
    );
    server.shutdown().await;
}

/// A token must not be printable by accident: `Config` is debug-logged at boot.
#[test]
fn the_session_token_does_not_print_itself() {
    let token = SessionToken::new("do-not-print-me").expect("non-empty");
    let debug = format!("{token:?}");
    assert!(
        !debug.contains("do-not-print-me"),
        "Debug leaked it: {debug}"
    );
    assert_eq!(token.expose(), "do-not-print-me");
    assert!(SessionToken::new("").is_none(), "empty is not a secret");
}

// --------------------------------------------------------------------- injection

/// AC: "a test exists for path traversal via workflow".
///
/// Over the wire, with the real tool: the traversal is refused by *shape*, before any path is
/// joined, so the answer is an argument error rather than a filesystem surprise.
#[tokio::test]
async fn a_traversal_in_the_workflow_argument_is_refused_before_any_path_is_joined() {
    let fixture = Fixture::new();
    let server = TestServer::start(fixture.config()).await;

    for workflow in [
        "../../../etc/passwd",
        ".github/workflows/../../../etc/passwd",
        "/etc/passwd",
        "..%2f..%2fetc",
        "..",
    ] {
        let (status, body) = post_jsonrpc(
            &raw_client(),
            &server.url(),
            "tools/call",
            json!({
                "name": "list_tasks",
                "arguments": { "workflow": workflow },
                "_meta": meta_2026_07_28(),
            }),
            Some("2026-07-28"),
        )
        .await;
        assert_eq!(status, 200, "a tool error is still an HTTP 200: {body}");
        assert_eq!(
            body["result"]["isError"].as_bool(),
            Some(true),
            "`workflow: {workflow}` must be refused: {body}"
        );
        let code = body["result"]["structuredContent"]["error"]["code"]
            .as_str()
            .unwrap_or_default();
        assert!(
            matches!(code, "invalid_arguments" | "workflow_not_found"),
            "`workflow: {workflow}` got {code}: {body}"
        );
        // Nothing about /etc/passwd came back: no content, no absolute path.
        let text = body.to_string();
        assert!(
            !text.contains("root:"),
            "the response must not contain the file the traversal asked for: {text}"
        );
    }

    server.shutdown().await;
}

/// A workflow *file* that is a symlink out of `.github/workflows` is not offered as a
/// workflow at all — the traversal shape that survives shape-validation, caught by
/// canonicalisation instead.
#[test]
#[cfg(unix)]
fn a_workflow_symlink_pointing_out_of_the_workflows_dir_is_not_indexed() {
    let workspace = fixture_workspace();
    let outside = tempfile::tempdir().expect("tempdir");
    let secret = outside.path().join("stolen.yml");
    std::fs::write(
        &secret,
        "name: stolen\non: push\njobs:\n  x:\n    runs-on: x\n",
    )
    .unwrap();
    std::os::unix::fs::symlink(
        &secret,
        workspace.path().join(".github/workflows/looks-local.yml"),
    )
    .expect("symlink");

    let indexed = act_mcp::workflows::index_directory(workspace.path())
        .expect("index")
        .into_iter()
        .map(|file| file.file_name)
        .collect::<Vec<_>>();
    assert!(
        !indexed.contains(&"looks-local.yml".to_owned()),
        "a symlink out of .github/workflows must not be offered: {indexed:?}"
    );
    assert!(
        indexed.contains(&"ci.yml".to_owned()),
        "the real workflow is still there: {indexed:?}"
    );
}

/// AC: "a test exists for path traversal via category".
///
/// Two halves. The argument is refused by shape, and — the half that actually protects the
/// filesystem — a category never names a path: the output file is built from the closed
/// [`Kind`] enum, so a category that passes validation but is unknown (`fuzz`) produces no
/// directory at all.
#[test]
fn a_category_cannot_become_a_path() {
    for hostile in [
        "../evil",
        "../../etc/passwd",
        "..",
        ".",
        "benchmark/../../../etc",
        "/absolute",
        "bench mark",
        "bench\nrm",
        "Benchmark",
        "",
    ] {
        assert!(
            !category::is_valid_category(hostile),
            "{hostile:?} must not be a category"
        );
        assert_eq!(category::kind_of(hostile), None, "{hostile:?}");
        let problem = category::validate(hostile).expect_err("must be refused");
        assert_eq!(problem.path, "category");
    }

    assert_eq!(category::kind_of("benchmark"), Some(Kind::Benchmark));
    assert_eq!(category::kind_of("coverage"), Some(Kind::Coverage));
    assert_eq!(category::kind_of("fuzz"), None);
    for kind in [Kind::Benchmark, Kind::Coverage] {
        assert!(
            !kind.as_str().contains(['.', '/', '\\']),
            "the only strings that ever become an evidence directory are {:?} = {:?}",
            kind,
            kind.as_str()
        );
    }
}

// ----------------------------------------------------------------------- the caps

/// AC: "Log/metric volume caps are enforced with typed errors rather than unbounded growth."
///
/// The on-disk run-directory count is the one nothing else bounds (retention prunes memory;
/// a published run dir is never deleted), so it gets its own typed refusal.
#[tokio::test]
#[cfg(unix)]
async fn a_run_is_refused_when_the_runs_directory_is_at_its_cap() {
    let root = tempfile::tempdir().expect("tempdir");
    let base = std::fs::canonicalize(root.path()).expect("canonical");
    // Two workspaces: the accepted run holds its own workspace lock (single-flight), and the
    // refusal has to be about the *directory count*, not about that lock. Each workspace
    // counts its own `<workspace>/.evidence/runs`, so this separation is also the point.
    let workspace = workspace_at(&base, "ws");
    let full = workspace_at(&base, "ws-full");
    let runs = full.join(".evidence/runs");
    std::fs::create_dir_all(&runs).expect("runs dir");

    let policy = RunPolicy {
        max_run_dirs: 3,
        ..RunPolicy::default()
    };
    policy.validate().expect("legal policy");
    let supervisor = RunSupervisor::with_policy(policy).expect("policy");

    // Under the cap: accepted.
    let ticket = supervisor
        .start(plan_at(&workspace, &base, "20261110T004203Z-aaaaaa"))
        .await
        .expect("a run under the cap is accepted");
    assert!(!ticket.run_id().is_empty());

    // Fill the second workspace to the cap: the next request is a typed error rather than a
    // slow machine.
    for index in 0..3 {
        std::fs::create_dir_all(runs.join(format!("filler-{index}"))).expect("filler dir");
    }
    let error = supervisor
        .start(plan_at(&full, &base, "20261110T004203Z-bbbbbb"))
        .await
        .expect_err("the cap must refuse the run");
    let StartError::TooManyRunDirs {
        limit,
        existing,
        path: _,
    } = error
    else {
        panic!("expected TooManyRunDirs, got {error:?}");
    };
    assert_eq!(limit, 3);
    assert!(existing >= 3, "counted {existing}");
    assert!(
        format!("{error:?}").contains("TooManyRunDirs"),
        "the typed variant is the answer, not a generic internal_error"
    );
    assert!(
        error.message().contains("--max-run-dirs"),
        "the message must carry the fix: {}",
        error.message()
    );
    assert_eq!(error.code(), ("too_many_runs", true));

    supervisor.shutdown().await;
}

/// A cap that would refuse every run is a boot-time policy error, not a first-request surprise.
#[test]
fn a_zero_run_dir_cap_is_refused_at_boot() {
    let policy = RunPolicy {
        max_run_dirs: 0,
        ..RunPolicy::default()
    };
    assert!(matches!(policy.validate(), Err(PolicyError::MaxRunDirs)));

    let fixture = Fixture::new();
    let mut args = fixture.args();
    args.max_run_dirs = 0;
    assert!(
        Config::build(&args).is_err(),
        "--max-run-dirs 0 is not a cap"
    );
    assert!(Config::build(&fixture.args()).is_ok());
}

/// The two caps the runner owns are enforced too, and both are visible in the outcome rather
/// than in unbounded growth: the on-disk log truncates with a marker, and the in-memory tail
/// stays inside its byte budget.
#[tokio::test]
#[cfg(unix)]
async fn the_log_caps_truncate_instead_of_growing() {
    let harness = RunHarness::new();
    let mut spec = harness.spec();
    spec.process_env = BTreeMap::from([("STUB_REPEAT".to_owned(), "2000".to_owned())]);

    // The smallest legal on-disk cap (ADR-0004 §11: `--max-run-log-bytes` ≥ 1 MiB) and a
    // tail ring well below it.
    let runner = Runner::new(
        RunnerLimits::checked(Duration::from_secs(1), 4096, MIN_RUN_LOG_BYTES)
            .expect("legal limits"),
    )
    .expect("runner");
    let outcome = runner.start(spec).await.expect("start").wait().await;

    assert!(
        outcome.log_truncated,
        "a 1 MiB cap on a much longer log must report truncation"
    );
    assert!(
        outcome
            .warnings
            .iter()
            .any(|warning| warning == WARNING_TRUNCATED),
        "and stamp the warning: {:?}",
        outcome.warnings
    );
    assert!(
        outcome.log_tail.len() <= 4096,
        "the in-memory tail respected its ring: {} bytes",
        outcome.log_tail.len()
    );
    assert!(outcome.log_bytes <= MIN_RUN_LOG_BYTES);
    let on_disk = std::fs::read(&outcome.log_path).expect("read log");
    assert!(
        on_disk.len() as u64 <= MIN_RUN_LOG_BYTES + 512,
        "the on-disk log respected its cap: {} bytes",
        on_disk.len()
    );
    assert!(
        String::from_utf8_lossy(&on_disk).contains("act_mcp_truncated"),
        "truncation is marked, not silent"
    );
}

/// The metric caps are the evidence side of the same rule; the constants are the ones the
/// extractors enforce, named here so a change cannot silently drop the bound.
#[test]
fn the_metric_caps_exist_and_are_finite() {
    // Named rather than compared with zero: the point is that the extractor enforces *these*
    // numbers, and a change to one has to be a deliberate edit here too.
    assert_eq!(act_mcp::evidence::benchmark::MAX_BENCHMARK_METRICS, 5_000);
    assert_eq!(act_mcp::evidence::coverage::MAX_COVERAGE_SOURCES, 5_000);
    assert_eq!(
        act_mcp::evidence::benchmark::MAX_BENCHMARK_BYTES,
        8 * 1024 * 1024
    );
    assert_eq!(
        act_mcp::evidence::coverage::MAX_COVERAGE_BYTES,
        8 * 1024 * 1024
    );
    assert_eq!(act_mcp::runner::DEFAULT_MAX_RUN_LOG_BYTES, 64 * 1024 * 1024);
    assert_eq!(act_mcp::runner::MAX_LOG_TAIL_BYTES, 65_536);
    assert_eq!(act_mcp::registry::LISTING_CAP, 50);
    assert_eq!(act_mcp::registry::KNOWN_RUNS_CAP, 20);
}

// ------------------------------------------------------------------ secret scanning

/// AC: "A run producing a known fake secret leaves that value in no file under `.evidence`
/// and in no tool result".
///
/// The declared-secret path is already covered by `tests/runner_execution.rs`. This closes
/// the two routes that were left: a credential that arrives as a caller `--var` (which
/// reaches act as argv, and would be echoed back by any job that prints its arguments), and
/// one that is already in the server's own environment (which act inherits, ADR-0003 §7.6).
#[tokio::test]
async fn a_secret_reaches_no_file_a_run_produces_whichever_route_it_arrived_by() {
    let from_var = "npm-token-from-the-caller";
    let from_server_env = "credential-in-the-server-environment";
    let ordinary = "release";
    std::env::set_var("ACT_MCP_HARDENING_TEST_TOKEN", from_server_env);

    let harness = RunHarness::new();
    let mut spec = harness.spec();
    spec.vars = BTreeMap::from([
        // A name that says secret: redacted wherever the value shows up.
        ("NPM_TOKEN".to_owned(), from_var.to_owned()),
        // An ordinary build flag: must survive, or the log stops being useful.
        ("BUILD_MODE".to_owned(), ordinary.to_owned()),
    ]);
    spec.process_env = BTreeMap::from([
        // The stub echoes its argv (so the `--var NPM_TOKEN=…` value reaches the log) and
        // the value of the named env var (so the server-env credential does).
        ("STUB_ECHO_ARGV".to_owned(), "1".to_owned()),
        (
            "STUB_ECHO_SECRET2".to_owned(),
            "ACT_MCP_HARDENING_TEST_TOKEN".to_owned(),
        ),
    ]);

    let runner = Runner::new(
        RunnerLimits::checked(
            Duration::from_secs(1),
            MAX_LOG_TAIL_BYTES,
            DEFAULT_MAX_RUN_LOG_BYTES,
        )
        .expect("legal limits"),
    )
    .expect("runner");
    let running = runner.start(spec).await.expect("start");
    // The recorded argv is what `meta.json.act.argv` will contain (ADR-0002 §1.3), so it is
    // part of "no file a run produces", not a separate question.
    let recorded_argv = running.argv().to_vec();
    let outcome = running.wait().await;

    std::env::remove_var("ACT_MCP_HARDENING_TEST_TOKEN");

    // 0. the argv record: masked where the name said secret, verbatim where it did not.
    assert!(
        !recorded_argv
            .iter()
            .any(|argument| argument.contains(from_var)),
        "a name-matched `--var` value must not survive into meta.json: {recorded_argv:?}"
    );
    assert!(
        recorded_argv
            .iter()
            .any(|argument| argument.contains(REDACTION_MARKER)),
        "and the masking should be visible rather than silent: {recorded_argv:?}"
    );
    assert!(
        recorded_argv
            .iter()
            .any(|argument| argument.contains(&format!("BUILD_MODE={ordinary}"))),
        "an ordinary --var must still be recorded for the audit trail: {recorded_argv:?}"
    );

    // 1. the wire-shaped output (what a tool result would carry).
    for secret in [from_var, from_server_env] {
        assert!(
            !outcome.log_tail.contains(secret),
            "log_tail leaked {secret}: {:?}",
            outcome.log_tail
        );
    }
    assert!(
        outcome.log_tail.contains(REDACTION_MARKER),
        "the echoed lines should show the marker: {:?}",
        outcome.log_tail
    );
    assert!(
        outcome.log_tail.contains(ordinary),
        "an ordinary var value must survive redaction: {:?}",
        outcome.log_tail
    );

    // 2. every file under the evidence root, walked byte by byte.
    let mut scanned = 0;
    for (path, bytes) in files_under(&harness.evidence) {
        scanned += 1;
        for secret in [from_var, from_server_env] {
            assert!(
                !contains(&bytes, secret.as_bytes()),
                "{secret} leaked into {}",
                path.display()
            );
        }
    }
    assert!(scanned > 0, "the run produced no files to scan");

    // 3. the scratch dir too (act's cwd, and the home of the plaintext secret file).
    for (path, bytes) in files_under(&harness.scratch_root) {
        for secret in [from_var, from_server_env] {
            assert!(
                !contains(&bytes, secret.as_bytes()),
                "{secret} leaked into {}",
                path.display()
            );
        }
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|window| window == needle)
}

// ---------------------------------------------------------------------- test scaffolding

/// Every file under `root`, as `(path, bytes)`.
fn files_under(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(bytes) = std::fs::read(&path) {
                out.push((path, bytes));
            }
        }
    }
    out
}

fn stub_act() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("testdata")
        .join("stub-act.sh");
    assert!(path.is_file(), "the stub act is checked in at {path:?}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    path
}

/// A workspace with one workflow, rooted at `base/name`.
#[cfg(unix)]
fn workspace_at(base: &Path, name: &str) -> PathBuf {
    let workspace = base.join(name);
    std::fs::create_dir_all(workspace.join(".github/workflows")).expect("workflows");
    std::fs::write(
        workspace.join(".github/workflows/ci.yml"),
        "name: ci\non:\n  push:\njobs:\n  build:\n    runs-on: ubuntu-latest\n",
    )
    .expect("workflow");
    workspace
}

/// A plan whose run dir is `<workspace>/.evidence/runs/<run_id>` and whose scratch dir is
/// outside the workspace (ADR-0003 F8).
#[cfg(unix)]
fn plan_at(workspace: &Path, base: &Path, run_id: &str) -> RunPlan {
    let id = RunId::parse(run_id).expect("test run id");
    let mut spec = RunSpec::new(
        id.clone(),
        stub_act(),
        workspace.to_path_buf(),
        workspace.join(".github/workflows/ci.yml"),
        "build",
    );
    spec.workspace = workspace.to_path_buf();
    spec.evidence_dir = workspace.join(".evidence");
    spec.run_dir = spec.evidence_dir.join("runs").join(id.as_str());
    spec.scratch_dir = base.join("scratch").join(id.as_str());
    spec.payload_dir = spec.evidence_dir.join("incoming").join(id.as_str());
    spec.timeout = Duration::from_secs(20);
    RunPlan::new(
        RunIdentity::new("ci", ".github/workflows/ci.yml", "build", "other", {
            "2f7c1a4b9e0d3c6a5b8f1e2d4c6a8b0f3e5d7c91"
        }),
        spec,
    )
}

/// A minimal `RunSpec` harness, mirroring `tests/runner_execution.rs` with only what these
/// tests need.
struct RunHarness {
    tree: PathBuf,
    evidence: PathBuf,
    scratch_root: PathBuf,
    _tmp: tempfile::TempDir,
}

impl RunHarness {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(tmp.path()).expect("canonical");
        let tree = root.join("tree");
        std::fs::create_dir_all(tree.join(".github/workflows")).expect("workflows");
        std::fs::write(
            tree.join(".github/workflows/ci.yml"),
            "name: ci\non:\n  push:\njobs:\n  build:\n    runs-on: ubuntu-latest\n",
        )
        .expect("workflow");
        let scratch_root = root.join("scratch");
        std::fs::create_dir_all(&scratch_root).expect("scratch");
        Self {
            evidence: tree.join(".evidence"),
            scratch_root,
            tree,
            _tmp: tmp,
        }
    }

    fn spec(&self) -> RunSpec {
        let run_id = RunId::parse("20261110T004203Z-a1b2c3").expect("valid id");
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
        spec.timeout = Duration::from_secs(20);
        spec
    }
}

// ---------------------------------------------------------------- subprocess boot helpers

/// The env vars that could change what a boot test means, removed from every spawn.
const AMBIENT_ENV: [&str; 13] = [
    "ACT_MCP_WORKSPACE",
    "ACT_MCP_BIND",
    "ACT_MCP_ACT_BINARY",
    "ACT_MCP_LOG_DIR",
    "EVIDENCE_DIR",
    "RUST_LOG",
    "ACT_MCP_I_UNDERSTAND_THIS_IS_UNAUTHENTICATED",
    "ACT_MCP_ALLOWED_HOSTS",
    "ACT_MCP_ALLOWED_ORIGINS",
    "ACT_MCP_MAX_REQUEST_BODY_BYTES",
    "ACT_MCP_REQUIRE_SESSION_TOKEN",
    "ACT_MCP_SESSION_TOKEN",
    "ACT_MCP_MAX_RUN_DIRS",
];

fn serve_command(
    fixture: &Fixture,
    args: &[&str],
    envs: &[(&str, &str)],
) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_act-mcp"));
    cmd.args([
        "serve",
        "--workspace",
        fixture.workspace.path().to_str().unwrap(),
        "--act-binary",
        fixture.act.to_str().unwrap(),
    ])
    .args(args)
    .stdin(std::process::Stdio::null());
    for name in AMBIENT_ENV {
        cmd.env_remove(name);
    }
    for (key, value) in envs {
        cmd.env(key, value);
    }
    cmd
}

/// Boot the real binary, wait for the listen line, stop it, and hand back its stderr.
#[cfg(unix)]
async fn boot_until_listening(
    fixture: &Fixture,
    args: &[&str],
    envs: &[(&str, &str)],
) -> Result<String, (std::process::ExitStatus, String)> {
    use tokio::io::{AsyncBufReadExt as _, BufReader};

    let mut child = serve_command(fixture, args, envs)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn act-mcp serve");
    let mut lines = BufReader::new(child.stderr.take().expect("stderr piped")).lines();

    let mut seen = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let line = tokio::time::timeout_at(deadline, lines.next_line())
            .await
            .expect("the server boots or fails within 20s")
            .expect("read a stderr line");
        let Some(line) = line else {
            let status = child.wait().await.expect("wait");
            return Err((status, seen));
        };
        seen.push_str(&line);
        seen.push('\n');
        if line.contains("act-mcp listening") {
            let pid = child.id().expect("pid");
            assert_eq!(unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) }, 0);
            let _ = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
            return Ok(seen);
        }
    }
}

/// Run the binary to completion (it refuses to boot) and hand back its status and stderr.
#[cfg(unix)]
async fn boot_and_capture(
    fixture: &Fixture,
    args: &[&str],
    envs: &[(&str, &str)],
) -> (std::process::ExitStatus, String) {
    let output = serve_command(fixture, args, envs)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .expect("run act-mcp serve");
    (
        output.status,
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}
