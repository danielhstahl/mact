//! `cancel_run` over the wire (mact-f1w.11, ADR-0001 §6.3, ADR-0004 §5.2).
//!
//! The state-machine detail lives in `tests/run_supervisor.rs`; what these prove is the
//! tool's own promise, seen through a real `tools/call`:
//!
//! * a run started by *another* request is cancellable from here — the run belongs to the
//!   supervisor, not to the connection that started it;
//! * cancelling a terminal run is a report, not an error (`already_terminal: true`, no
//!   `kill`, `warnings: ["already_terminal"]`) — the idempotence §6.3 promises and
//!   `cancel_run_result` encodes as a `not` constraint;
//! * an unknown id is the tool error `run_not_found`, the only error this tool has;
//! * annotations and argument validation are what ADR-0001 §1/§5 pin.

use std::{path::Path, time::Duration};

use act_mcp::{
    registry::{RunIdentity, RunPlan},
    runner::{RunId, RunSpec},
    tools::cancel_run::TOOL,
};
use rmcp::{
    model::ProtocolVersion,
    transport::{
        streamable_http_client::StreamableHttpClientTransportConfig, StreamableHttpClientTransport,
    },
    ClientLifecycleMode, ClientServiceExt,
};
use serde_json::{json, Value};

use common::{meta_2026_07_28, post_jsonrpc, raw_client, Fixture, TestServer};

mod common;

const COMMIT: &str = "2f7c1a4b9e0d3c6a5b8f1e2d4c6a8b0f3e5d7c91";

fn stub_act() -> std::path::PathBuf {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("testdata")
        .join("stub-act.sh");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    path
}

/// A run of the fixture workspace, optionally ignoring `SIGTERM` (to exercise the kill
/// escalation) and optionally backgrounding a child (to prove the group is taken down).
fn plan(workspace: &Path, run_id: &str, secs: &str, ignore_term: bool) -> RunPlan {
    let id = RunId::parse(run_id).expect("test run id");
    let workflow = ".github/workflows/ci.yml";
    let mut spec = RunSpec::new(
        id,
        stub_act(),
        workspace.to_path_buf(),
        workspace.join(workflow),
        "test",
    );
    spec.timeout = Duration::from_secs(120);
    spec.process_env
        .insert("STUB_SLEEP".to_owned(), secs.to_owned());
    spec.process_env
        .insert("STUB_CHILD_SECS".to_owned(), secs.to_owned());
    if ignore_term {
        spec.process_env
            .insert("STUB_IGNORE_TERM".to_owned(), "1".to_owned());
    }
    RunPlan::new(
        RunIdentity::new("ci", workflow, "test", "other", COMMIT),
        spec,
    )
}

/// One `tools/call` for this tool.
async fn call(url: &str, arguments: Value) -> (u16, Value) {
    let mut params = json!({ "name": TOOL, "arguments": arguments });
    params["_meta"] = meta_2026_07_28();
    post_jsonrpc(&raw_client(), url, "tools/call", params, Some("2026-07-28")).await
}

/// Unwrap a call's `structuredContent`, with `isError` reported as the payload it is.
async fn receipt(url: &str, arguments: Value) -> Value {
    let (status, body) = call(url, arguments).await;
    assert_eq!(status, 200, "a tool result is HTTP 200: {body}");
    assert!(body.get("error").is_none(), "must not be JSON-RPC: {body}");
    let result = body.get("result").expect("result").clone();
    let payload = result
        .get("structuredContent")
        .cloned()
        .unwrap_or(Value::Null);
    assert_eq!(
        result.get("isError"),
        Some(&json!(false)),
        "cancel_run only errors on `run_not_found`: {payload}"
    );
    payload
}

/// The same, for a call expected to be an error.
async fn error_receipt(url: &str, arguments: Value) -> Value {
    let (status, body) = call(url, arguments).await;
    assert_eq!(status, 200, "an execution error is still HTTP 200: {body}");
    assert!(body.get("error").is_none(), "must not be JSON-RPC: {body}");
    let result = body.get("result").expect("result").clone();
    assert_eq!(
        result.get("isError"),
        Some(&json!(true)),
        "expected isError"
    );
    result
        .get("structuredContent")
        .cloned()
        .unwrap_or(Value::Null)
}

/// A stock client sees the annotations §1 pins for `cancel_run`: it changes state, is not
/// destructive, and is idempotent — which is exactly why a client may retry it.
#[tokio::test]
async fn tools_list_advertises_cancel_run_with_the_pinned_annotations() {
    let fixture = Fixture::new();
    let server = TestServer::start(fixture.config()).await;

    let client = ()
        .serve_with_lifecycle(
            StreamableHttpClientTransport::from_config(
                StreamableHttpClientTransportConfig::with_uri(server.url()),
            ),
            ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        )
        .await
        .expect("a 2026-07-28 client can connect");

    let tools = client.list_tools(None).await.expect("tools/list");
    let tool = tools
        .tools
        .iter()
        .find(|tool| tool.name == TOOL)
        .expect("cancel_run is advertised");
    let annotations = tool.annotations.as_ref().expect("annotations");
    assert_eq!(annotations.read_only_hint, Some(false));
    assert_eq!(annotations.destructive_hint, Some(false));
    assert_eq!(annotations.idempotent_hint, Some(true));
    assert_eq!(annotations.open_world_hint, Some(false));

    client.cancel().await.expect("client shuts down");
    server.shutdown().await;
}

/// The acceptance case in one test: one call starts a run, a *different* call cancels it,
/// the receipt carries the kill report, and the process group is gone.
#[tokio::test]
async fn a_run_started_by_one_call_is_cancelled_by_another() {
    let fixture = Fixture::new();
    let server = TestServer::start(fixture.config()).await;
    let run_id = RunId::generate().as_str().to_owned();
    let ticket = server
        .supervisor
        .start(plan(fixture.workspace.path(), &run_id, "60", false))
        .await
        .expect("accepted");

    // Wait until act is actually up, so this is a cancel of a running run and not of a
    // queued one (which is a different row of §5.2's table).
    for _ in 0..100 {
        if server
            .supervisor
            .status(&run_id, 0)
            .await
            .is_ok_and(|view| view.status == act_mcp::registry::WireStatus::Running)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let started = std::time::Instant::now();
    let payload = receipt(&server.url(), json!({ "run_id": run_id })).await;
    assert_eq!(payload["run_id"], run_id);
    assert_eq!(payload["status"], "cancelled");
    assert_eq!(payload["terminal"], true);
    assert_eq!(payload["already_terminal"], false);
    assert!(
        payload["cancel_requested_at"].is_string(),
        "a real cancel names when it was requested: {payload}"
    );
    assert_eq!(payload["kill"]["method"], "term");
    assert!(payload["kill"]["took_ms"].is_number());
    // The cancel is bounded by `--cancel-wait-secs`, and act died on SIGTERM at once.
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "cancel blocked for {:?}",
        started.elapsed()
    );

    // The run's own waiter sees the same terminal commit, and `run_status` agrees.
    let record = ticket.wait().await;
    assert_eq!(record.state.status().as_str(), "cancelled");
    let view = server.supervisor.status(&run_id, 0).await.expect("known");
    assert_eq!(view.status.as_str(), "cancelled");

    server.shutdown().await;
}

/// Cancelling a run that already finished is not an error and not a second teardown:
/// `already_terminal`, the *old* status, no `cancel_requested_at`, no `kill` — which is
/// also what `cancel_run_result`'s `not` constraint demands.
#[tokio::test]
async fn cancelling_a_finished_run_reports_instead_of_failing() {
    let fixture = Fixture::new();
    let server = TestServer::start(fixture.config()).await;
    let run_id = RunId::generate().as_str().to_owned();
    let ticket = server
        .supervisor
        .start(plan(fixture.workspace.path(), &run_id, "0", false))
        .await
        .expect("accepted");
    let _ = ticket.wait().await;

    let first = receipt(&server.url(), json!({ "run_id": &run_id })).await;
    assert_eq!(first["already_terminal"], true);
    assert_eq!(
        first["status"], "success",
        "the old terminal status, not `cancelled`"
    );
    assert_eq!(first["terminal"], true);
    assert!(
        first.get("cancel_requested_at").is_none(),
        "nothing was requested: {first}"
    );
    assert!(first.get("kill").is_none(), "nothing was killed: {first}");
    assert!(
        first["warnings"]
            .as_array()
            .expect("warnings")
            .contains(&json!("already_terminal")),
        "{first}"
    );

    // A third call answers the same way: idempotence is not a one-shot.
    let again = receipt(&server.url(), json!({ "run_id": &run_id })).await;
    assert_eq!(again, first);

    server.shutdown().await;
}

/// `run_not_found` is the only error this tool has, and the payload names the runs that do
/// exist (ADR-0004 §7.4).
#[tokio::test]
async fn an_unknown_run_is_run_not_found() {
    let fixture = Fixture::new();
    let server = TestServer::start(fixture.config()).await;

    let payload = error_receipt(
        &server.url(),
        json!({ "run_id": "20200101T000000Z-abcdef" }),
    )
    .await;
    assert_eq!(payload["error"]["code"], "run_not_found");
    assert_eq!(payload["error"]["retryable"], false);
    assert_eq!(
        payload["error"]["detail"]["run_id"],
        "20200101T000000Z-abcdef"
    );
    assert_eq!(
        payload["error"]["detail"]["known_runs"]
            .as_array()
            .expect("known_runs"),
        &Vec::<Value>::new(),
        "this server has never run anything, so there is nothing to suggest"
    );

    // A malformed id is a different code, because it is the caller's typo, not a lost run.
    let payload = error_receipt(&server.url(), json!({ "run_id": "latest" })).await;
    assert_eq!(payload["error"]["code"], "invalid_arguments");
    let payload = error_receipt(&server.url(), json!({})).await;
    assert_eq!(payload["error"]["code"], "invalid_arguments");
    assert_eq!(
        payload["error"]["detail"]["errors"][0]["path"], "run_id",
        "the missing argument is named: {payload}"
    );
    let payload = error_receipt(
        &server.url(),
        json!({ "run_id": "20200101T000000Z-abcdef", "force": true }),
    )
    .await;
    assert_eq!(payload["error"]["code"], "unknown_argument");

    server.shutdown().await;
}

/// A run that ignores `SIGTERM` is escalated to `SIGKILL` inside `--term-grace-secs` and
/// the receipt says `method: "kill"` — the same escalation the supervisor test asserts,
/// seen through the tool, and bounded so the caller always gets an answer.
#[tokio::test]
async fn a_run_that_ignores_sigterm_comes_back_as_a_kill() {
    let fixture = Fixture::new();
    // A short grace: the escalation is the same rule at any length, and no test should
    // pay the shipped 20 s for it.
    let policy = act_mcp::registry::RunPolicy {
        term_grace: Duration::from_secs(1),
        cancel_wait: Duration::from_secs(10),
        ..act_mcp::registry::RunPolicy::default()
    };
    let supervisor =
        std::sync::Arc::new(act_mcp::registry::RunSupervisor::with_policy(policy).expect("policy"));
    let server = TestServer::with_supervisor(fixture.config(), supervisor).await;
    let run_id = RunId::generate().as_str().to_owned();
    let ticket = server
        .supervisor
        .start(plan(fixture.workspace.path(), &run_id, "60", true))
        .await
        .expect("accepted");
    for _ in 0..100 {
        if server
            .supervisor
            .status(&run_id, 0)
            .await
            .is_ok_and(|view| view.status == act_mcp::registry::WireStatus::Running)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let payload = receipt(&server.url(), json!({ "run_id": &run_id })).await;
    assert_eq!(payload["status"], "cancelled");
    assert_eq!(payload["kill"]["method"], "kill", "{payload}");
    let took = payload["kill"]["took_ms"].as_u64().expect("took_ms");
    assert!(
        took >= 1000,
        "SIGKILL came before the grace expired: {payload}"
    );
    let _ = ticket.wait().await;

    // And now it is terminal, so the next cancel is a report.
    let again = receipt(&server.url(), json!({ "run_id": &run_id })).await;
    assert_eq!(again["already_terminal"], true);
    assert_eq!(again["status"], "cancelled");

    server.shutdown().await;
}
