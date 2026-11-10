//! `list_tasks` over the wire (mact-f1w.7 acceptance).
//!
//! The unit tests in `src/tools/list_tasks.rs` own the decisions; these prove the
//! things only a real MCP call can prove:
//!
//! * `tools/list` advertises `list_tasks` with a valid `inputSchema` **and**
//!   `outputSchema`, and with the annotations ADR-0001 §1 pins;
//! * a real `tools/call` returns the fixture repository's listing as
//!   `structuredContent`, and that listing agrees with the hand-written golden
//!   fixture the contract was pinned against;
//! * argument failures come back as ADR-0001 §5.1 tool execution errors
//!   (`isError: true`, `structuredContent.error.code`) and **not** as JSON-RPC
//!   `-32602`;
//! * the call answers with no `act` binary and no container runtime, and many
//!   concurrent calls all succeed — the "no workspace lock" claim, observable.

mod common;

use std::path::Path;

use act_mcp::{
    fixtures::TestRepo,
    tools::list_tasks::{CATEGORY_HINT_ADVISORY_WORD, TOOL},
};
use rmcp::{
    model::ProtocolVersion,
    transport::{
        streamable_http_client::StreamableHttpClientTransportConfig, StreamableHttpClientTransport,
    },
    ClientLifecycleMode, ClientServiceExt,
};
use serde_json::{json, Value};

use common::{config_at, meta_2026_07_28, post_jsonrpc, raw_client, TestServer};

/// The three-workflow fixture, as a repository (real git, real SHAs — none of which
/// `list_tasks` looks at, which is part of the point).
fn fixture_repo(dir: &Path) -> TestRepo {
    let repo = TestRepo::init(&dir.join("repo"));
    repo.with_list_tasks_fixture();
    repo
}

/// `tools/list` advertises `list_tasks` with both schemas and ADR-0001 §1's
/// annotations, as a stock client sees them.
#[tokio::test]
async fn tools_list_advertises_list_tasks_with_both_schemas() {
    let root = tempfile::tempdir().expect("tempdir");
    let repo = fixture_repo(root.path());
    // No act binary on purpose: discovery must be advertised regardless.
    let server = TestServer::start(config_at(repo.path(), Path::new("/nonexistent/act"))).await;

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

    let tools = client.list_tools(None).await.expect("tools/list works");
    let names: Vec<&str> = tools.tools.iter().map(|tool| tool.name.as_ref()).collect();
    assert_eq!(
        names,
        vec!["cancel_run", "list_tasks", "run_status"],
        "the tool set is this build's tools; `list_tasks` must be among them"
    );

    let tool = tools
        .tools
        .iter()
        .find(|tool| tool.name == TOOL)
        .expect("list_tasks is advertised");
    assert_eq!(
        tool.title.as_deref(),
        Some("List workflows and job ids in this workspace")
    );
    let description = tool.description.as_deref().unwrap_or_default();
    assert!(
        description.contains(CATEGORY_HINT_ADVISORY_WORD),
        "the description must say the hint is advisory: {description}"
    );
    for code in [
        "invalid_arguments",
        "unknown_argument",
        "workspace_not_found",
        "workflows_dir_missing",
    ] {
        assert!(
            description.contains(code),
            "description omits {code}: {description}"
        );
    }

    // Annotations, ADR-0001 §1's row for list_tasks.
    let annotations = tool.annotations.clone().expect("annotations are set");
    assert_eq!(annotations.read_only_hint, Some(true));
    assert_eq!(annotations.destructive_hint, Some(false));
    assert_eq!(annotations.idempotent_hint, Some(true));
    assert_eq!(annotations.open_world_hint, Some(false));

    // Both schemas, and each one is an object schema.
    let input = Value::Object(tool.input_schema.as_ref().clone());
    assert_eq!(at(&input, "/type"), json!("object"));
    assert_eq!(at(&input, "/additionalProperties"), json!(false));
    assert_eq!(at(&input, "/properties/workflow/type"), json!("string"));
    assert_eq!(
        at(&input, "/properties/include_steps/type"),
        json!("boolean")
    );
    let output = tool
        .output_schema
        .as_ref()
        .expect("list_tasks must advertise an outputSchema");
    let output = Value::Object(output.as_ref().clone());
    assert_eq!(at(&output, "/type"), json!("object"));
    for field in ["workflows_dir", "workflow_count", "workflows", "warnings"] {
        assert!(
            at(&output, "/required")
                .as_array()
                .expect("required list")
                .iter()
                .any(|required| required == &json!(field)),
            "{field} must be required: {output}"
        );
    }

    client.cancel().await.expect("client stops");
    server.shutdown().await;
}

/// A real call's `structuredContent` is the checked-in golden payload for the same
/// three files — the contract's own example, not a recording of this implementation.
///
/// Three relaxations, each because the payload is a hand-written *example* of
/// ADR-0001 §6.1 rather than an exhaustive answer, and each documented in
/// `tests/e2e/testdata/README.md`:
///
/// * arrays are compared sorted (by `file`, then job `id`): the contract never pinned
///   an order, and this server sorts by path;
/// * `parse_error.message` is compared as absent/present with the same `code`: the
///   message is the parser's own sentence including its position, and a hand-written
///   example cannot predict serde_yaml's wording;
/// * the optional `uses_composite` and `steps` members are dropped on both sides: the
///   example spells `uses_composite` only where it is true and lists `steps` for one
///   job only, while this call answers both wherever they apply.
///
/// Everything else — names, paths, `display`, job ids and order-independence, `needs`,
/// `runs_on`, `matrix_axes`, every `category_hint`, `job_count`, `workflow_count`,
/// `workflows_dir` and `warnings` — is compared exactly.
/// [`the_filtered_call_answers_the_filtered_golden_fixture`] then does the same job
/// with no relaxation at all, against the payload written for that call.

#[tokio::test]
async fn a_real_call_returns_the_pinned_golden_answer() {
    let root = tempfile::tempdir().expect("tempdir");
    let repo = fixture_repo(root.path());
    let server = TestServer::start(config_at(repo.path(), Path::new("/nonexistent/act"))).await;

    let (status, body) = call(&server.url(), json!({})).await;
    assert_eq!(status, 200, "body: {body}");
    assert_eq!(at(&body, "/result/isError"), json!(false), "body: {body}");
    let result = &body["result"]["structuredContent"];
    assert_eq!(result["workflows_dir"], json!(".github/workflows"));
    assert_eq!(result["workflow_count"], json!(3));

    let golden = golden_fixture("list_tasks.result.full.json");

    let got = normalize(result);
    let want = normalize(&golden);
    assert_eq!(
        got, want,
        "the call must answer what the contract fixture promises"
    );

    server.shutdown().await;
}

/// The `workflow` filter plus `include_steps`, compared to the golden fixture written
/// for exactly that pair (`list_tasks.args.filtered.json` → this payload). No
/// relaxations: one workflow, one job, nothing whose order could vary.
#[tokio::test]
async fn the_filtered_call_answers_the_filtered_golden_fixture() {
    let root = tempfile::tempdir().expect("tempdir");
    let repo = fixture_repo(root.path());
    let server = TestServer::start(config_at(repo.path(), Path::new("/nonexistent/act"))).await;

    let (status, body) = call(
        &server.url(),
        json!({"workflow": "coverage", "include_steps": true}),
    )
    .await;
    assert_eq!(status, 200, "body: {body}");
    assert_eq!(at(&body, "/result/isError"), json!(false), "{body}");
    let golden = golden_fixture("list_tasks.result.filtered.json");
    assert_eq!(
        at(&body, "/result/structuredContent"),
        golden,
        "the filtered call must answer what its fixture promises"
    );

    // And the filter accepts every form ADR-0001 §2.2 defines, not just the basename.
    for form in ["coverage", "coverage.yml", ".github/workflows/coverage.yml"] {
        let (_, body) = call(&server.url(), json!({"workflow": form})).await;
        assert_eq!(
            at(&body, "/result/structuredContent/workflow_count"),
            json!(1),
            "`{form}` must resolve to exactly one workflow: {body}"
        );
    }

    server.shutdown().await;
}

/// Bad arguments are a tool execution error, not a protocol error: `isError: true`,
/// `structuredContent.error.code`, and a text block whose first two tokens are
/// `list_tasks: <code>:` (ADR-0001 §5.1, §5.2).
#[tokio::test]
async fn bad_arguments_are_tool_errors_never_json_rpc_errors() {
    let root = tempfile::tempdir().expect("tempdir");
    let repo = fixture_repo(root.path());
    let server = TestServer::start(config_at(repo.path(), Path::new("/nonexistent/act"))).await;

    let (status, body) = call(&server.url(), json!({"run_id": "20261110T011507Z-d4e5f6"})).await;
    assert_eq!(
        status, 200,
        "an unknown argument is still a tool result: {body}"
    );
    assert_eq!(body["result"]["isError"], json!(true));
    assert_eq!(
        body["result"]["structuredContent"]["error"]["code"],
        json!("unknown_argument")
    );
    assert!(
        body.get("error").is_none(),
        "no JSON-RPC error object may appear: {body}"
    );
    let text = body["result"]["content"][0]["text"]
        .as_str()
        .expect("a text block");
    assert!(
        text.starts_with("list_tasks: unknown_argument: "),
        "text must lead with `<tool>: <code>:`, got {text}"
    );

    let (_, body) = call(&server.url(), json!({"include_steps": "yes"})).await;
    assert_eq!(body["result"]["isError"], json!(true));
    assert_eq!(
        body["result"]["structuredContent"]["error"]["code"],
        json!("invalid_arguments")
    );
    assert_eq!(
        body["result"]["structuredContent"]["error"]["detail"]["errors"][0]["path"],
        json!("include_steps")
    );

    server.shutdown().await;
}

/// A workspace with no workflows answers `workflows_dir_missing` — the code ADR-0001
/// §6.1 lists — with the reason in `detail`, and never a host path.
#[tokio::test]
async fn a_workspace_without_workflows_says_workflows_dir_missing() {
    let root = tempfile::tempdir().expect("tempdir");
    let empty = root.path().join("empty");
    std::fs::create_dir_all(empty.join(".github")).expect("create .github");
    let server = TestServer::start(config_at(&empty, Path::new("/nonexistent/act"))).await;

    let (status, body) = call(&server.url(), json!({})).await;
    assert_eq!(status, 200);
    assert_eq!(body["result"]["isError"], json!(true));
    let error = &body["result"]["structuredContent"]["error"];
    assert_eq!(error["code"], json!("workflows_dir_missing"));
    assert_eq!(error["detail"]["path"], json!(".github/workflows"));
    assert!(
        !error["message"]
            .as_str()
            .unwrap_or_default()
            .contains(&empty.display().to_string()),
        "no absolute host path in the message: {error}"
    );

    server.shutdown().await;
}

/// The tool's headline promise: it answers while nothing else is idle and nothing is
/// required — no act, no docker, and nothing serialized. Eight concurrent calls all
/// return, and a workspace with a huge-looking tree is not touched by any of them.
#[tokio::test]
async fn many_calls_at_once_all_answer_and_nothing_is_required() {
    let root = tempfile::tempdir().expect("tempdir");
    let repo = fixture_repo(root.path());
    let config = config_at(repo.path(), Path::new("/nonexistent/act"));
    let server = TestServer::start(config).await;
    let url = server.url();
    let before = repo.porcelain();

    let calls: Vec<_> = (0..8)
        .map(|_| {
            let url = url.clone();
            tokio::spawn(async move {
                let (status, body) = call(&url, json!({})).await;
                (status, body)
            })
        })
        .collect();
    for handle in calls {
        let (status, body) = handle.await.expect("call task");
        assert_eq!(status, 200);
        assert_eq!(body["result"]["isError"], json!(false), "{body}");
        assert_eq!(
            body["result"]["structuredContent"]["workflow_count"],
            json!(3)
        );
    }
    assert_eq!(
        repo.porcelain(),
        before,
        "a discovery call must not touch the workspace"
    );

    server.shutdown().await;
}

/// One `tools/call` over raw JSON-RPC, with the per-request metadata the pinned
/// revision requires.
async fn call(url: &str, arguments: Value) -> (u16, Value) {
    let mut params = json!({ "name": TOOL, "arguments": arguments });
    params["_meta"] = meta_2026_07_28();
    post_jsonrpc(&raw_client(), url, "tools/call", params, Some("2026-07-28")).await
}

/// JSON Pointer lookup that answers `null` instead of panicking.
fn at(value: &Value, pointer: &str) -> Value {
    value
        .pointer(pointer)
        .cloned()
        .unwrap_or_else(|| panic!("`{pointer}` missing from {value}"))
}

/// A checked-in golden payload.
fn golden_fixture(name: &str) -> Value {
    let path = format!(
        "{}/tests/e2e/testdata/tool-io/valid/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(path).expect("the golden fixture is in the tree"))
        .expect("the golden fixture is JSON")
}

/// Sort workflow/job arrays and drop the things the golden payload does not pin:
/// `parse_error.message` (the parser's own wording), `uses_composite` (optional, and
/// the hand-written example left it out), `steps` (opt-in), and order.
fn normalize(value: &Value) -> Value {
    let mut value = value.clone();
    if let Some(workflows) = value.get_mut("workflows").and_then(Value::as_array_mut) {
        for workflow in workflows.iter_mut() {
            let Some(object) = workflow.as_object_mut() else {
                continue;
            };
            if let Some(error) = object.remove("parse_error") {
                object.insert(
                    "parse_error".to_owned(),
                    json!({ "code": error["code"].clone() }),
                );
            }
            if let Some(jobs) = object.get_mut("jobs").and_then(Value::as_array_mut) {
                for job in jobs.iter_mut() {
                    if let Some(job) = job.as_object_mut() {
                        job.remove("uses_composite");
                        job.remove("steps");
                    }
                }
                jobs.sort_by_key(|job| job["id"].as_str().unwrap_or_default().to_owned());
            }
        }
        workflows.sort_by(|left, right| {
            (
                left["file"].as_str().unwrap_or_default(),
                left["name"].as_str().unwrap_or_default(),
            )
                .cmp(&(
                    right["file"].as_str().unwrap_or_default(),
                    right["name"].as_str().unwrap_or_default(),
                ))
        });
    }
    if let Some(warnings) = value.get("warnings").and_then(Value::as_array) {
        let mut warnings: Vec<String> = warnings
            .iter()
            .map(|warning| warning.as_str().unwrap_or_default().to_owned())
            .collect();
        warnings.sort();
        value["warnings"] = json!(warnings);
    }
    value
}
