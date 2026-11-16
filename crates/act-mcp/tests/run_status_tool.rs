//! `run_status` over the wire (mact-f1w.11, ADR-0001 §6.2, ADR-0004 §7).
//!
//! The supervisor tests (`tests/run_supervisor.rs`) own the state machine; these prove the
//! things only a real `tools/call` can prove:
//!
//! * `tools/list` advertises `run_status` with both schemas and the annotations ADR-0001 §1
//!   pins (`readOnlyHint`/`idempotentHint` true — polling is what a client is *supposed* to
//!   do repeatedly);
//! * an empty listing is a successful `{runs: [], active_count: 0, truncated: false}`
//!   (§17.3), and an unknown `run_id` is the tool error `run_not_found`, not a JSON-RPC
//!   `-32602`;
//! * a run started by one call is pollable by another, sees its log tail from memory, and
//!   the answer keeps the shape the outputSchema promises.

use std::{path::Path, time::Duration};

use act_mcp::{
    registry::{RunIdentity, RunPlan, RunSupervisor},
    runner::{RunId, RunSpec},
    tools::run_status::TOOL,
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

/// The checked-in stub `act`, which is what lets these tests run a real run in milliseconds.
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

/// A plan for a run of the fixture workspace.
fn plan(workspace: &Path, run_id: &str, env: &[(&str, String)]) -> RunPlan {
    let id = RunId::parse(run_id).expect("test run id");
    let workflow = ".github/workflows/ci.yml";
    let mut spec = RunSpec::new(
        id,
        stub_act(),
        workspace.to_path_buf(),
        workspace.join(workflow),
        "test",
    );
    spec.timeout = Duration::from_secs(60);
    for (key, value) in env {
        spec.process_env.insert((*key).to_owned(), value.clone());
    }
    RunPlan::new(
        RunIdentity::new("ci", workflow, "test", "other", COMMIT),
        spec,
    )
}

/// One `tools/call` for this tool over raw JSON-RPC, with the metadata the pinned
/// revision requires.
async fn call(url: &str, arguments: Value) -> (u16, Value) {
    let mut params = json!({ "name": TOOL, "arguments": arguments });
    params["_meta"] = meta_2026_07_28();
    post_jsonrpc(&raw_client(), url, "tools/call", params, Some("2026-07-28")).await
}

/// The `structuredContent` of a successful call, with the error case spelled out.
fn content(status: u16, body: &Value) -> Value {
    assert_eq!(
        status, 200,
        "a tool result is HTTP 200 even when isError: {body}"
    );
    assert!(
        body.get("error").is_none(),
        "an argument failure must be a tool result, not JSON-RPC: {body}"
    );
    let result = body
        .get("result")
        .unwrap_or_else(|| panic!("no result in {body}"));
    if result.get("isError") == Some(&json!(true)) {
        panic!(
            "expected a successful payload, got an error: {}",
            result.get("structuredContent").unwrap_or(result)
        );
    }
    result
        .get("structuredContent")
        .cloned()
        .unwrap_or(Value::Null)
}

/// The `structuredContent` of a call that is expected to be an `isError` tool result.
fn error_content(status: u16, body: &Value) -> Value {
    assert_eq!(status, 200, "an execution error is still HTTP 200: {body}");
    assert!(
        body.get("error").is_none(),
        "must not be a JSON-RPC error object: {body}"
    );
    let result = body
        .get("result")
        .unwrap_or_else(|| panic!("no result in {body}"));
    assert_eq!(
        result.get("isError"),
        Some(&json!(true)),
        "expected isError on {result}"
    );
    result
        .get("structuredContent")
        .cloned()
        .unwrap_or(Value::Null)
}

/// A stock client sees `run_status` advertised with the annotations §1 pins: read-only,
/// idempotent, and not open-world (it does not touch `act` or the network).
#[tokio::test]
async fn tools_list_advertises_run_status_with_the_pinned_annotations() {
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
        .expect("run_status is advertised");
    let annotations = tool
        .annotations
        .as_ref()
        .expect("annotations are advertised");
    assert_eq!(annotations.read_only_hint, Some(true));
    assert_eq!(annotations.destructive_hint, Some(false));
    assert_eq!(annotations.idempotent_hint, Some(true));
    assert_eq!(annotations.open_world_hint, Some(false));
    assert!(
        tool.output_schema.is_some(),
        "run_status advertises an outputSchema"
    );

    client.cancel().await.expect("client shuts down");
    server.shutdown().await;
}

/// A server that has never run anything answers the listing mode with an empty page — a
/// success, not an error (§17.3: "no runs" and "every run died with the last restart" are
/// the same answer).
#[tokio::test]
async fn a_server_with_no_runs_answers_an_empty_listing() {
    let fixture = Fixture::new();
    let server = TestServer::start(fixture.config()).await;

    let (status, body) = call(&server.url(), json!({})).await;
    assert_eq!(
        content(status, &body),
        json!({ "runs": [], "active_count": 0, "truncated": false })
    );

    // `limit` is accepted, and out-of-range limits are `invalid_arguments`, not a clamp.
    let (status, body) = call(&server.url(), json!({ "limit": 0 })).await;
    assert_eq!(
        error_content(status, &body)["error"]["code"],
        "invalid_arguments"
    );

    server.shutdown().await;
}

/// An unknown id is `run_not_found`, and the payload names the runs that do exist so a
/// caller who garbled an id can recover without a second round trip (§7.4).
#[tokio::test]
async fn an_unknown_run_is_a_tool_error_naming_the_known_runs() {
    let fixture = Fixture::new();
    let config = fixture.config();
    let server = TestServer::start(config).await;
    let run_id = RunId::generate().as_str().to_owned();
    let ticket = server
        .supervisor
        .start(plan(fixture.workspace.path(), &run_id, &[]))
        .await
        .expect("accepted");

    // A well-formed id this server has never seen (a malformed one is `invalid_arguments`,
    // which the next test covers).
    let (status, body) = call(
        &server.url(),
        json!({ "run_id": "20200101T000000Z-abcdef" }),
    )
    .await;
    let error = error_content(status, &body);
    assert_eq!(error["error"]["code"], "run_not_found");
    assert_eq!(error["error"]["retryable"], false);
    assert_eq!(
        error["error"]["detail"]["run_id"],
        "20200101T000000Z-abcdef"
    );
    assert_eq!(
        error["error"]["detail"]["known_runs"]
            .as_array()
            .expect("known_runs")
            .len(),
        1
    );
    assert_eq!(error["error"]["detail"]["known_runs"][0], run_id);

    let _ = ticket.wait().await;
    server.shutdown().await;
}

/// The headline poll contract: a run started by one request is readable by another, from
/// memory, all the way to its terminal commit — with a log tail, a phase, and the identity
/// the run was registered with.
#[tokio::test]
async fn a_run_started_elsewhere_is_pollable_over_the_wire_until_it_finishes() {
    let fixture = Fixture::new();
    let server = TestServer::start(fixture.config()).await;
    let run_id = RunId::generate().as_str().to_owned();
    let marker = fixture.dir.path().join("marker.txt");
    std::fs::write(&marker, "{\"level\":\"info\",\"msg\":\"wire-marker\"}\n").expect("marker");
    let ticket = server
        .supervisor
        .start(plan(
            fixture.workspace.path(),
            &run_id,
            &[
                ("STUB_SLEEP", "1".to_owned()),
                ("STUB_LINES_FILE", marker.display().to_string()),
            ],
        ))
        .await
        .expect("accepted");

    // While act runs: `running`, with a tail from the ring.
    let mut view = Value::Null;
    for _ in 0..100 {
        let (status, body) = call(
            &server.url(),
            json!({ "run_id": run_id, "log_tail_bytes": 4096 }),
        )
        .await;
        view = content(status, &body);
        if view["run"]["status"] == json!("running") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(view["run"]["status"], "running", "last poll: {view}");
    assert_eq!(view["run"]["phase"], "running");
    assert_eq!(view["run"]["run_id"], run_id);
    assert_eq!(view["run"]["workflow"], "ci");
    assert_eq!(view["run"]["task"], "test");
    assert_eq!(view["run"]["category"], "other");
    assert_eq!(view["run"]["commit"], COMMIT);

    // The tail is whatever the log pump has read so far, so give act the moment it needs to
    // print — but the point stands: it arrives with no disk read and no act round trip.
    for _ in 0..50 {
        let (status, body) = call(
            &server.url(),
            json!({ "run_id": run_id, "log_tail_bytes": 4096 }),
        )
        .await;
        view = content(status, &body);
        if view["run"]["log_tail"]
            .as_str()
            .unwrap_or_default()
            .contains("wire-marker")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        view["run"]["log_tail"]
            .as_str()
            .unwrap_or_default()
            .contains("wire-marker"),
        "the tail comes from the runner's ring: {view}"
    );

    // The listing sees it too, and counts it as active.
    let (status, body) = call(&server.url(), json!({ "limit": 5 })).await;
    let listing = content(status, &body);
    assert_eq!(listing["active_count"], 1);
    assert_eq!(listing["runs"].as_array().expect("runs").len(), 1);

    let _ = ticket.wait().await;
    let (status, body) = call(&server.url(), json!({ "run_id": run_id })).await;
    let done = content(status, &body)["run"].clone();
    assert_eq!(done["status"], "success");
    assert_eq!(done["phase"], "done");
    assert!(
        done["finished_at"].is_string(),
        "a terminal run carries finished_at"
    );
    assert_eq!(done["exit_code"], 0);
    assert!(
        done["duration_ms"].as_u64().is_some_and(|ms| ms > 0),
        "a finished run reports how long act was up: {done}"
    );

    // And it stays readable after it is gone from the active set (ADR-0004 §7.3 keeps it
    // in memory for `--run-retention-mins`).
    let (status, body) = call(&server.url(), json!({ "limit": 5 })).await;
    let listing = content(status, &body);
    assert_eq!(listing["active_count"], 0);
    assert_eq!(listing["runs"].as_array().expect("runs").len(), 1);

    server.shutdown().await;
}

/// Bad arguments are `invalid_arguments` with one `detail.errors[]` entry per problem
/// (ADR-0001 §5.3) — the same shape every other tool uses, and never `-32602`.
#[tokio::test]
async fn malformed_arguments_are_invalid_arguments_naming_each_problem() {
    let fixture = Fixture::new();
    let server = TestServer::start(fixture.config()).await;

    let cases = [
        (json!({ "run_id": "yesterday" }), "run_id"),
        (json!({ "run_id": 12 }), "run_id"),
        (json!({ "log_tail_bytes": 65537 }), "log_tail_bytes"),
        (json!({ "limit": 0 }), "limit"),
        (json!({ "limit": 51 }), "limit"),
        (json!({ "log_tail_bytes": -1 }), "log_tail_bytes"),
    ];
    for (arguments, path) in cases {
        let (status, body) = call(&server.url(), arguments.clone()).await;
        let payload = error_content(status, &body);
        assert_eq!(payload["error"]["code"], "invalid_arguments", "{arguments}");
        assert_eq!(payload["error"]["retryable"], false);
        let errors = payload["error"]["detail"]["errors"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert!(
            errors.iter().any(|problem| problem["path"] == json!(path)),
            "{arguments} should name `{path}` in {payload}"
        );
    }

    // An unknown argument is refused rather than ignored, so a typo cannot silently
    // change what a poll means.
    let (status, body) = call(&server.url(), json!({ "runId": "20261110T000000Z-abcdef" })).await;
    assert_eq!(
        error_content(status, &body)["error"]["code"],
        "unknown_argument"
    );

    server.shutdown().await;
}

/// `run_status` never blocks: with a run in flight and a client that wants 0 bytes of
/// tail, the call still answers in milliseconds — the §7.1 "reads no disk, talks to no
/// act" promise, observable from the outside.
#[tokio::test]
async fn polling_a_run_in_flight_does_not_block() {
    let fixture = Fixture::new();
    let server = TestServer::start(fixture.config()).await;
    let supervisor: &RunSupervisor = &server.supervisor;
    let run_id = RunId::generate().as_str().to_owned();
    let ticket = supervisor
        .start(plan(
            fixture.workspace.path(),
            &run_id,
            &[("STUB_SLEEP", "30".to_owned())],
        ))
        .await
        .expect("accepted");

    let started = std::time::Instant::now();
    let mut polls = 0;
    for _ in 0..50 {
        let (status, body) = call(
            &server.url(),
            json!({ "run_id": run_id, "log_tail_bytes": 0 }),
        )
        .await;
        let view = content(status, &body);
        assert_eq!(
            view["run"]["log_tail"], "",
            "0 bytes asked for, none given: {view}"
        );
        polls += 1;
        if started.elapsed() > Duration::from_millis(250) {
            break;
        }
    }
    assert!(
        polls >= 10,
        "a blocking poll would not have looped {polls} times"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );

    supervisor.cancel(&run_id).await.expect("cancelled");
    let _ = ticket.wait().await;
    server.shutdown().await;
}
