//! Protocol conformance for MCP **2026-07-28** over Streamable HTTP (mact-f1w.16).
//!
//! Everything else in `tests/` proves this server is *internally* consistent. This file asks a
//! different question — the one an auditor asks — and ignores our own code while doing it: *is
//! `POST /mcp` a correct MCP 2026-07-28 Streamable HTTP endpoint, judged from the wire?*
//!
//! Two clients do the asking:
//!
//! * **a stock `rmcp` client** (`client` + `transport-streamable-http-client-reqwest`), pinned
//!   to the revision this server claims, dialling an ephemeral-port server the test booted. If
//!   the advertised surface stops making sense, this is what breaks first — and it is the same
//!   client an integrator will write, so its failures are the failures a downstream project
//!   would have hit instead of us.
//! * **raw HTTP** (a deliberately permissive `reqwest`, and once a raw socket), because the
//!   interesting cases in a conformance suite are the requests a well-behaved client never
//!   sends: an omitted version header, an `Accept` that cannot be satisfied, a legacy
//!   `initialize`, a body that is not JSON, a cancellation for someone else's request.
//!
//! ## What the pin costs, and where that is asserted
//!
//! The server exposes exactly one revision (`server::SUPPORTED_PROTOCOL_VERSION`), runs with
//! `legacy_session_mode = false` and `stateless_protocol_metadata_required = true`, and
//! hand-rolls none of the negotiation (all of it is `rmcp`'s). Each of those choices is
//! asserted as an *observation*, not as a source read, so that removing the pin — or letting
//! one of those three config lines drift back to its library default — fails a test here:
//!
//! | decision | asserted by |
//! |---|---|
//! | exactly one revision exposed | `server_discover_advertises_only_the_pinned_revision`, `a_client_naming_an_older_revision_is_not_served_it` |
//! | per-request version signalling required | `a_request_that_omits_the_protocol_version_is_refused_not_assumed` |
//! | no session, no handshake | `a_legacy_initialize_gets_the_documented_rejection_not_a_hang`, `no_response_ever_carries_a_session_id` |
//! | JSON answers, not SSE | `a_normal_call_answers_application_json`, `a_post_must_accept_both_json_and_sse` |
//!
//! Where `rmcp`'s own answer is debatable (a body that is not JSON is refused 415 rather than
//! 400; a `notifications/cancelled` naming an in-flight request is answered 202 but never
//! dispatched in stateless mode) the test pins what the endpoint *actually does* and the comment
//! says which half of that is `rmcp`'s rather than ours. A conformance suite that asserts what
//! the author wished had happened is worth nothing to the next reviewer.
//!
//! The four tests that need a real run in flight
//! ([`a_cancellation_notification_for_an_in_flight_call_is_answered_and_harmless`] and its
//! neighbours) boot the **shipped binary** on an ephemeral port against the e2e stub `act`, so
//! the cancellation and shutdown paths they exercise include the runner and its process group,
//! not a stand-in for them.

mod common;

use std::{path::PathBuf, time::Duration};

use act_mcp::server::SERVER_NAME;
use common::{
    git_workspace, meta_2026_07_28, raw_client, Fixture, ServeOptions, ServeProcess, TestServer,
    DEFAULT_WORKFLOW_BODY,
};
use rmcp::{
    model::ProtocolVersion,
    transport::{
        streamable_http_client::StreamableHttpClientTransportConfig, StreamableHttpClientTransport,
    },
    ClientLifecycleMode, ClientServiceExt,
};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// The revision everything here is pinned to, spelled once.
const PIN: &str = "2026-07-28";
/// `rmcp`'s JSON-RPC code for "the version you named is not one I speak" (`-32022`).
const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;
/// `rmcp`'s code for a header/`_meta` protocol mismatch or omission (`-32020`).
const HEADER_MISMATCH: i64 = -32020;
/// JSON-RPC `invalid params` (`-32602`), which is also what a bad tool name gets.
const INVALID_PARAMS: i64 = -32602;
/// JSON-RPC `method not found` (`-32601`).
const METHOD_NOT_FOUND: i64 = -32601;

/// The tools this build registers, in `tools/list` order.
const TOOLS: [&str; 4] = ["cancel_run", "list_tasks", "run_status", "run_workflow"];

// ==================================================================== the stock client
//
// One connection from a real client, then assertions about what it was handed. `rmcp` does not
// expose `server/discover` as a typed call, so that one is raw; everything else is the
// same object graph a downstream client would hold.

/// A client pinned to 2026-07-28, so it goes straight to the stateless lifecycle: no
/// `initialize`, one self-contained request at a time.
async fn stock_client(url: &str) -> rmcp::service::RunningService<rmcp::service::RoleClient, ()> {
    ().serve_with_lifecycle(
        StreamableHttpClientTransport::from_config(StreamableHttpClientTransportConfig::with_uri(
            url,
        )),
        ClientLifecycleMode::Discover {
            preferred_versions: vec![ProtocolVersion::V_2026_07_28],
        },
    )
    .await
    .expect("a 2026-07-28 client can connect to this server without a handshake")
}

/// A stock client lists four tools, and every one of them is well-formed enough to hand to an
/// agent: legal name (SEP-986), non-empty description, `inputSchema`, and an `outputSchema` —
/// structured results are the reason this server's results are machine-readable at all.
#[tokio::test]
async fn a_stock_client_gets_every_tool_with_both_schemas() {
    let fixture = Fixture::new();
    let server = TestServer::start(fixture.config()).await;
    let client = stock_client(&server.url()).await;

    let tools = client
        .list_tools(None)
        .await
        .expect("tools/list answers")
        .tools;

    assert_eq!(
        tools
            .iter()
            .map(|tool| tool.name.as_ref())
            .collect::<Vec<_>>(),
        TOOLS,
        "the advertised set is exactly the registered set"
    );
    for tool in &tools {
        let name = tool.name.as_ref();
        assert!(
            tool.input_schema.get("type").is_some(),
            "`{name}` advertises an inputSchema that is a schema object: {:?}",
            tool.input_schema
        );
        assert!(
            tool.output_schema.is_some(),
            "`{name}` advertises an outputSchema, so its structured results are checkable"
        );
        assert!(
            tool.annotations.is_some(),
            "`{name}` advertises tool annotations (ADR-0001 §1 pins them)"
        );
    }

    client.cancel().await.expect("client shuts down");
    server.shutdown().await;
}

/// SEP-986 (tool-name format), applied to what we actually advertise: 1..=128 bytes of
/// `A-Za-z0-9._-`, no spaces or commas, and nothing pathological at either end.
#[tokio::test]
async fn advertised_tool_names_are_legal_per_sep_986() {
    let fixture = Fixture::new();
    let server = TestServer::start(fixture.config()).await;
    let client = stock_client(&server.url()).await;
    let tools = client.list_tools(None).await.expect("tools/list").tools;

    for tool in &tools {
        let name = tool.name.as_ref();
        assert!(
            !name.is_empty() && name.len() <= 128,
            "`{name}` length: {name:?}",
        );
        assert!(
            name.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')),
            "`{name}` uses a character SEP-986 does not allow"
        );
        assert!(
            !name.starts_with(['-', '.']) && !name.ends_with(['-', '.']),
            "`{name}` starts or ends with a character the SEP warns about"
        );
        assert!(
            !name.contains([' ', ',']),
            "`{name}` contains a space or a comma"
        );
    }

    client.cancel().await.expect("client shuts down");
    server.shutdown().await;
}

/// A description an agent can act on, and a title a UI can show: non-empty for every tool.
/// This is the cheapest possible check and the one most likely to rot, because nothing else
/// fails when a `description` attribute is edited into an empty string.
#[tokio::test]
async fn advertised_descriptions_and_titles_are_not_empty() {
    let fixture = Fixture::new();
    let server = TestServer::start(fixture.config()).await;
    let client = stock_client(&server.url()).await;
    let tools = client.list_tools(None).await.expect("tools/list").tools;

    for tool in &tools {
        let name = tool.name.as_ref();
        let description = tool
            .description
            .as_ref()
            .unwrap_or_else(|| panic!("`{name}` advertises no description at all"));
        assert!(
            description.trim().len() >= 40,
            "`{name}`'s description is too short to be a description: {description:?}"
        );
        let title = tool
            .title
            .as_ref()
            .unwrap_or_else(|| panic!("`{name}` advertises no title"));
        assert!(!title.trim().is_empty(), "`{name}`'s title is blank");
    }

    client.cancel().await.expect("client shuts down");
    server.shutdown().await;
}

/// The `outputSchema`s are not decoration.
///
/// `tools/list` is a promise a client can hold us to, so this compiles each advertised
/// `outputSchema` as the JSON Schema it claims to be and validates a real `structuredContent`
/// against it. A result that only *looks* like the schema — a renamed member, a status the
/// enum does not list — fails here even though the tool itself is happy.
#[tokio::test]
async fn an_advertised_output_schema_validates_the_result_it_advertises() {
    let fixture = Fixture::new();
    let server = TestServer::start(fixture.config()).await;
    let url = server.url();
    let client = stock_client(&url).await;
    let tools = client.list_tools(None).await.expect("tools/list").tools;
    client.cancel().await.expect("client shuts down");

    for (tool_name, arguments) in [
        ("list_tasks", json!({})),
        ("list_tasks", json!({"include_steps": true})),
        ("run_status", json!({})),
    ] {
        let advertised = tools
            .iter()
            .find(|tool| tool.name == tool_name)
            .unwrap_or_else(|| panic!("`{tool_name}` is advertised"))
            .output_schema
            .clone()
            .expect("every tool advertises an outputSchema");
        let schema = Value::Object(advertised.as_ref().clone());
        let validator = jsonschema::validator_for(&schema).unwrap_or_else(|error| {
            panic!("`{tool_name}`'s outputSchema will not compile: {error}")
        });

        let body = call_tool(&url, tool_name, arguments).await;
        assert_eq!(
            body["result"]["isError"],
            json!(false),
            "the call under test must be a success: {body}"
        );
        let instance = body["result"]["structuredContent"].clone();
        assert!(
            instance.is_object(),
            "`{tool_name}` returned no structuredContent to validate: {body}"
        );
        let errors = validator.iter_errors(&instance).collect::<Vec<_>>();
        assert!(
            errors.is_empty(),
            "`{tool_name}`'s structuredContent violates its own advertised outputSchema: {errors:?}\n\
             instance: {instance}"
        );
    }

    server.shutdown().await;
}

// ==================================================================== negotiation
//
// The pin, from the outside. Every request here names a version somewhere — in the
// `MCP-Protocol-Version` header, in `_meta.io.modelcontextprotocol/protocolVersion`, in
// `params.protocolVersion`, or nowhere.

/// `server/discover` is the stateless replacement for `initialize`, so it is where the exposed
/// revision set is visible without any cooperation from us. One revision, the pinned one, and
/// the server identity that goes with it.
#[tokio::test]
async fn server_discover_advertises_only_the_pinned_revision() {
    let fixture = Fixture::new();
    let server = TestServer::start(fixture.config()).await;

    let (status, body, _) = Request::discover().send(&server.url()).await;

    assert_eq!(status, 200, "body: {body}");
    assert_eq!(
        body["result"]["supportedVersions"],
        json!([PIN]),
        "exactly one revision is exposed: {body}"
    );
    let meta = &body["result"]["_meta"];
    let info = meta
        .get("io.modelcontextprotocol/serverInfo")
        .or_else(|| meta.get("serverInfo"))
        .unwrap_or(&Value::Null);
    assert_eq!(info["name"], json!(SERVER_NAME), "serverInfo: {info}");
    assert_eq!(
        info["version"],
        json!(env!("CARGO_PKG_VERSION")),
        "serverInfo: {info}"
    );

    server.shutdown().await;
}

/// A client that names an older revision — in the header *and* in `_meta`, which is the honest
/// way to ask — is not served it. `rmcp` refuses with the JSON-RPC code for the purpose
/// (`-32022`) and says what it does speak, and no tool ran: the assertion is about the answer,
/// so it keeps working whatever the refusal looks like internally.
#[tokio::test]
async fn a_client_naming_an_older_revision_is_not_served_it() {
    let fixture = Fixture::new();
    let server = TestServer::start(fixture.config()).await;

    for revision in ["2025-11-25", "2025-06-18", "2025-03-26"] {
        let (status, body, _) = Request::post("tools/list")
            .version(revision)
            .meta_with_version(revision)
            .send(&server.url())
            .await;

        let negotiated = body
            .pointer("/result/protocolVersion")
            .and_then(Value::as_str)
            .or_else(|| body["result"]["tools"].as_array().map(|_| PIN));
        assert_ne!(
            negotiated,
            Some(revision),
            "a client must never be served {revision}: status={status} body={}",
            brief(&body)
        );
        assert_eq!(
            error_message(&body)
                .unwrap_or_default()
                .to_ascii_lowercase(),
            "unsupported protocol version",
            "the refusal names the reason: status={status} body={body}"
        );
        assert_eq!(
            body["error"]["data"]["supported"],
            json!([PIN]),
            "and says which revision it will speak: {body}"
        );
        assert_eq!(
            body["error"]["code"],
            json!(UNSUPPORTED_PROTOCOL_VERSION),
            "with the code for exactly this: {body}"
        );
        assert_eq!(
            status, 400,
            "and an HTTP status that says bad request: {body}"
        );
    }

    server.shutdown().await;
}

/// A request that names **no** version at all is refused, not silently read as some older
/// dialect.
///
/// This is the `stateless_protocol_metadata_required = true` decision (ADR-0001 §7,
/// `docs/tool-contract.md` §1) made observable. At `rmcp`'s default an unnamed version means
/// `2025-03-26`, i.e. this server would answer a revision it advertises no knowledge of — a
/// silent downgrade through the front door. `validate_required_protocol_header` in `rmcp` is what
/// refuses it, so the assertion is `rmcp`'s status and `rmcp`'s code, exactly as ADR-0001 §7
/// asks the conformance test to pin them.
#[tokio::test]
async fn a_request_that_omits_the_protocol_version_is_refused_not_assumed() {
    let fixture = Fixture::new();
    let server = TestServer::start(fixture.config()).await;

    // No header, no `_meta`: the pre-2026 dialect's shape, and the shape a careless client
    // sends when it has not been told what this endpoint speaks.
    let (status, body, _) = Request::post("tools/list").send(&server.url()).await;
    assert_eq!(
        status,
        400,
        "an unversioned request is refused: {}",
        brief(&body)
    );
    assert_eq!(
        body["error"]["code"],
        json!(HEADER_MISMATCH),
        "refused as a protocol-signalling problem, not answered: {body}"
    );
    assert!(body["result"].is_null(), "and no listing came back: {body}");

    // The header alone is the other half-way case: version known, per-request metadata absent.
    let (status, body, _) = Request::post("tools/list")
        .version(PIN)
        .send(&server.url())
        .await;
    assert_eq!(
        status, 400,
        "a header with no `_meta` is not a self-contained request: {body}"
    );
    assert!(
        body["error"].is_object(),
        "and it is answered with an error: {body}"
    );

    // …and `_meta` alone, which contradicts itself the same way in the other direction.
    let (status, body, _) = Request::post("tools/list")
        .meta_with_version(PIN)
        .send(&server.url())
        .await;
    assert_eq!(
        status, 400,
        "a `_meta` version with no header is refused too: {body}"
    );

    // The fully-signalled request is the one that works, which is what makes the three
    // refusals above a decision rather than a broken endpoint.
    let (status, body, _) = Request::post("tools/list")
        .version(PIN)
        .meta_with_version(PIN)
        .send(&server.url())
        .await;
    assert_eq!(status, 200, "the signalled request is served: {body}");
    assert!(
        body["result"]["tools"].is_array(),
        "with the tool listing: {body}"
    );

    server.shutdown().await;
}

/// The two halves of the per-request signal must agree: a header naming one revision and
/// `_meta` naming another is a request that cannot be honoured either way.
#[tokio::test]
async fn a_header_and_meta_that_disagree_are_refused() {
    let fixture = Fixture::new();
    let server = TestServer::start(fixture.config()).await;

    let (status, body, _) = Request::post("tools/list")
        .version(PIN)
        .meta_with_version("2025-06-18")
        .send(&server.url())
        .await;
    assert_eq!(status, 400, "contradictory signalling: {body}");
    assert!(body["error"].is_object(), "answered with an error: {body}");
    assert!(
        body["result"].is_null(),
        "and not served either revision: {body}"
    );

    server.shutdown().await;
}
// ==================================================================== the stateless era
//
// 2026-07-28 has no handshake and no session. That is the whole reason a benchmark run can be a
// plain request here, so it is worth pinning from both directions: what works without a
// handshake, and what a pre-2026 client is told when it tries one.

/// Self-contained requests, one after another, on fresh connections, with no handshake in
/// sight — the era's central claim, tested as a client would use it.
#[tokio::test]
async fn per_request_signalled_requests_need_no_handshake() {
    let fixture = Fixture::new();
    let server = TestServer::start(fixture.config()).await;
    let url = server.url();

    // Every request opens its own connection: reqwest pools, so the pool is disabled to make
    // "no connection state either" part of the assertion rather than an accident of transport.
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .timeout(Duration::from_secs(10))
        .build()
        .expect("reqwest client");

    for round in 1..=3 {
        let body = call_tool_with(&client, &url, "list_tasks", json!({})).await;
        assert_eq!(
            body["result"]["isError"],
            json!(false),
            "round {round} is served without any prior request: {body}"
        );
        assert!(
            body["result"]["structuredContent"]["workflows"].is_array(),
            "round {round} answers with its structured result: {body}"
        );
    }

    // And a stock client, which likewise never sent `initialize`, still sees a healthy endpoint.
    let client = stock_client(&url).await;
    assert!(!client
        .list_tools(None)
        .await
        .expect("tools/list after three stateless calls")
        .tools
        .is_empty());
    client.cancel().await.expect("client shuts down");

    server.shutdown().await;
}

/// A pre-2026 client that performs the handshake is refused with the code for the purpose, and
/// the refusal *arrives*.
///
/// `legacy_session_mode = false` means there is no session to create, so `initialize` cannot
/// succeed here by construction; what a client needs is not a session but an answer. A hang
/// would be the failure mode (the request would sit there until the client gave up), so the
/// `raw_client`'s 10 s timeout makes a hang a test failure rather than a slow test — and the
/// endpoint is checked again afterwards, because a rejection that costs the endpoint is not a
/// rejection.
#[tokio::test]
async fn a_legacy_initialize_gets_the_documented_rejection_not_a_hang() {
    let fixture = Fixture::new();
    let server = TestServer::start(fixture.config()).await;
    let url = server.url();

    for revision in ["2025-06-18", "2025-11-25"] {
        let (status, body, headers) = Request::post("initialize")
            .version(revision)
            .params(json!({
                "protocolVersion": revision,
                "capabilities": {},
                "clientInfo": { "name": "legacy-handshake-client", "version": "0" }
            }))
            .send(&url)
            .await;

        assert!(
            body["error"].is_object(),
            "an `initialize` this server cannot honour is answered, status={status}: {}",
            brief(&body)
        );
        assert_eq!(
            body["error"]["code"],
            json!(UNSUPPORTED_PROTOCOL_VERSION),
            "with the version-refusal code, and the supported set attached: {body}"
        );
        assert_eq!(
            body["error"]["data"]["supported"],
            json!([PIN]),
            "so the client can retry correctly: {body}"
        );
        assert_eq!(
            body["id"],
            json!(1),
            "the refusal belongs to this request: {body}"
        );
        assert!(
            headers.get("mcp-session-id").is_none(),
            "and no session was created to hang on to: {headers:?}"
        );

        // The same request naming the pinned revision is refused too: the handshake itself is
        // what does not exist, whatever version it carries.
        let (_, body, _) = Request::post("initialize")
            .version(PIN)
            .params(json!({
                "protocolVersion": PIN,
                "capabilities": {},
                "clientInfo": { "name": "pinned-handshake-client", "version": "0" }
            }))
            .send(&url)
            .await;
        assert_eq!(
            body["error"]["code"],
            json!(UNSUPPORTED_PROTOCOL_VERSION),
            "a handshake is not part of this revision either: {body}"
        );
    }

    // Still alive, still serving.
    let (status, body, _) = Request::post("tools/list")
        .version(PIN)
        .meta_with_version(PIN)
        .send(&url)
        .await;
    assert_eq!(
        status, 200,
        "the endpoint outlived the handshake attempts: {body}"
    );

    server.shutdown().await;
}

// ==================================================================== transport headers
//
// The transport half of conformance: what a request has to look like, and what comes back.

/// A normal call is answered with `application/json` — `json_response = true`, so a plain
/// client works and no SSE parser is required for a request that produces one message.
#[tokio::test]
async fn a_normal_call_answers_application_json() {
    let fixture = Fixture::new();
    let server = TestServer::start(fixture.config()).await;

    let (status, body, headers) = Request::post("tools/list")
        .version(PIN)
        .meta_with_version(PIN)
        .send(&server.url())
        .await;

    assert_eq!(status, 200, "body: {body}");
    let content_type = headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    assert!(
        content_type.starts_with("application/json"),
        "a single-message answer is JSON, not an SSE stream: {content_type:?}"
    );
    assert!(
        !content_type.contains("text/event-stream"),
        "and not both at once: {content_type:?}"
    );
    assert!(
        body["result"].is_object(),
        "and the body is the JSON-RPC envelope: {body}"
    );

    server.shutdown().await;
}

/// No response carries `MCP-Session-Id`, and a request that supplies one is not treated as
/// bound to a session: with `legacy_session_mode = false` there is nothing to bind to.
#[tokio::test]
async fn no_response_ever_carries_a_session_id() {
    let fixture = Fixture::new();
    let server = TestServer::start(fixture.config()).await;
    let url = server.url();

    // A success, a refusal, and a notification: none of them may answer with a session id,
    // because a client that saw one would be entitled to start a session lifecycle we do not
    // implement.
    let attempts = [
        Request::post("tools/list")
            .version(PIN)
            .meta_with_version(PIN),
        Request::post("tools/list")
            .version(PIN)
            .meta_with_version(PIN)
            .session_id("a-session-id-that-cannot-exist"),
        Request::post("tools/list").version(PIN),
        Request::notification("notifications/cancelled", json!({"requestId": 4242})).version(PIN),
    ];
    for attempt in attempts {
        let (status, body, headers) = attempt.send(&url).await;
        assert!(
            headers.get("mcp-session-id").is_none(),
            "status={status} body must not carry a session id: {headers:?}"
        );
        assert_ne!(
            headers
                .get("mcp-session-id")
                .and_then(|value| value.to_str().ok()),
            Some("a-session-id-that-cannot-exist"),
            "and must not echo a supplied one either: status={status} body={body}"
        );
    }

    server.shutdown().await;
}

/// A `POST` must be able to receive either media type, because whether an answer is one message
/// or several is the server's to decide — so a client that accepts only one of them is refused
/// before anything is parsed.
#[tokio::test]
async fn a_post_must_accept_both_json_and_sse() {
    let fixture = Fixture::new();
    let server = TestServer::start(fixture.config()).await;
    let url = server.url();
    let request = || {
        Request::post("tools/list")
            .version(PIN)
            .meta_with_version(PIN)
    };

    let (status, body, _) = request()
        .clone()
        .accept("application/json")
        .send(&url)
        .await;
    assert_eq!(status, 406, "JSON-only is not enough: {body}");
    assert!(
        body.is_null() || body["result"].is_null(),
        "and nothing was served: {body}"
    );

    let (status, _, _) = request()
        .clone()
        .accept("text/event-stream")
        .send(&url)
        .await;
    assert_eq!(status, 406, "SSE-only is not enough either");

    // The header a browser or an older SDK sends, with parameters and quality values: the
    // media types are there, so it satisfies the rule.
    let (status, _, _) = request()
        .accept("application/json;q=0.9, text/event-stream;q=0.8")
        .send(&url)
        .await;
    assert_eq!(status, 200, "a qualified Accept still satisfies it");

    server.shutdown().await;
}

/// `Content-Type` must name `application/json` (with or without a charset), and anything else
/// is refused before the body is touched.
#[tokio::test]
async fn the_request_content_type_must_be_json() {
    let fixture = Fixture::new();
    let server = TestServer::start(fixture.config()).await;
    let url = server.url();

    let (status, _, _) = Request::post("tools/list")
        .version(PIN)
        .meta_with_version(PIN)
        .content_type("text/plain")
        .send(&url)
        .await;
    assert_eq!(status, 415, "a text body is not this protocol");

    let (status, body, _) = Request::post("tools/list")
        .version(PIN)
        .meta_with_version(PIN)
        .content_type("application/json; charset=utf-8")
        .send(&url)
        .await;
    assert_eq!(status, 200, "a charset parameter is still JSON: {body}");

    let (status, _, _) = Request::post("tools/list")
        .version(PIN)
        .meta_with_version(PIN)
        .content_type("application/x-www-form-urlencoded")
        .send(&url)
        .await;
    assert_eq!(status, 415, "and a form post is not either");

    server.shutdown().await;
}

/// `GET` and `DELETE` on `/mcp` are not served — and they are *answered*. Both are the methods
/// the session-era protocol used for the server-initiated stream and for closing a session, and
/// neither exists here, so `rmcp` answers 405 with `Allow: POST`. A panic or a 500 would mean
/// the transport was configured inconsistently with its own method table.
#[tokio::test]
async fn get_and_delete_on_the_endpoint_are_answered_not_served() {
    let fixture = Fixture::new();
    let server = TestServer::start(fixture.config()).await;
    let url = server.url();

    // A GET with the Accept an SSE client would send: still no stream, because there is no
    // session to stream, and no event store to replay from.
    let response = raw_client()
        .get(&url)
        .header("Accept", "text/event-stream")
        .header("MCP-Protocol-Version", PIN)
        .send()
        .await
        .expect("GET reaches the endpoint");
    assert_eq!(response.status().as_u16(), 405);
    assert_eq!(
        response
            .headers()
            .get("allow")
            .and_then(|value| value.to_str().ok()),
        Some("POST"),
        "the refusal says what is served"
    );
    assert!(
        !response.headers().contains_key("content-type"),
        "a bodiless 405 must not claim an SSE content type"
    );

    for (method, builder) in [
        ("GET", raw_client().get(&url)),
        ("DELETE", raw_client().delete(&url)),
        ("PUT", raw_client().put(&url)),
        ("HEAD", raw_client().head(&url)),
        (
            "OPTIONS",
            raw_client().request(reqwest::Method::OPTIONS, &url),
        ),
    ] {
        let response = builder
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", PIN)
            .header("Mcp-Session-Id", "irrelevant")
            .send()
            .await
            .unwrap_or_else(|error| panic!("{method} must be answered, not dropped: {error}"));
        let status = response.status().as_u16();
        assert!(
            (400..500).contains(&status),
            "{method} /mcp must be a 4xx refusal, not {status}"
        );
        assert_ne!(status, 500, "{method} must not panic the endpoint");
    }

    server.shutdown().await;
}

/// Nothing else is mounted: the endpoint is one path, and a request anywhere else is a plain
/// 404 from the router rather than an MCP answer.
#[tokio::test]
async fn only_the_mcp_path_is_served() {
    let fixture = Fixture::new();
    let server = TestServer::start(fixture.config()).await;
    let root = format!("http://{}/", server.addr);

    for path in ["", "wrong", "health", "sse", "api/tools", "MCP"] {
        let response = raw_client()
            .post(format!("{root}{path}"))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", PIN)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}))
            .send()
            .await
            .expect("the request reaches the server");
        assert_eq!(
            response.status().as_u16(),
            404,
            "`/{path}` must not answer MCP"
        );
        assert_eq!(
            raw_client()
                .get(format!("{root}{path}"))
                .send()
                .await
                .expect("GET reaches the server")
                .status()
                .as_u16(),
            404,
            "`/{path}` must not answer anything else either"
        );
    }

    // The one route that does answer. Note what "one route" means exactly: the MCP service is
    // *nested*, so anything below `/mcp` is the same endpoint with the same answer — it does not
    // discriminate on a trailing segment, and a client that appends one gets served rather than
    // 404'd. Anything outside that prefix is the router's 404 and nothing else is mounted.
    let (status, body, _) = Request::post("tools/list")
        .version(PIN)
        .meta_with_version(PIN)
        .send(&format!("{root}mcp/with-a-trailing-segment"))
        .await;
    assert_eq!(
        status, 200,
        "a suffix under /mcp is the same endpoint, not a second one: {body}"
    );

    // The one path that does answer, for contrast.
    assert_eq!(
        raw_client()
            .post(server.url())
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", PIN)
            .header("Mcp-Method", "tools/list")
            .json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list",
                          "params":{"_meta":meta_2026_07_28()}}))
            .send()
            .await
            .expect("POST /mcp reaches the server")
            .status()
            .as_u16(),
        200
    );

    server.shutdown().await;
}

/// SEP-2243: from the pinned revision onward the routing headers *are* part of the request —
/// an intermediary is allowed to route on them without parsing the body — so they must be
/// present and must agree with the body. All four refusals happen before a tool is chosen, which
/// is the property that makes them safe.
#[tokio::test]
async fn the_sep_2243_routing_headers_must_agree_with_the_body() {
    let fixture = Fixture::new();
    let server = TestServer::start(fixture.config()).await;
    let url = server.url();

    // No routing headers at all.
    let (status, body, _) = Request::post("tools/list")
        .version(PIN)
        .meta_with_version(PIN)
        .without_sep_headers()
        .send(&url)
        .await;
    assert_eq!(
        status, 400,
        "a request at {PIN} names its method in a header: {body}"
    );
    assert_eq!(
        error_code(&body),
        Some(HEADER_MISMATCH),
        "and the refusal is the header-mismatch code: {body}"
    );

    // A header that contradicts the body: routing on it would dispatch the wrong thing.
    let (status, body, _) = Request::post("tools/list")
        .version(PIN)
        .meta_with_version(PIN)
        .method_header("tools/call")
        .send(&url)
        .await;
    assert_eq!(
        status, 400,
        "a header that lies about the body is refused: {body}"
    );
    assert_eq!(error_code(&body), Some(HEADER_MISMATCH), "{body}");

    // A `tools/call` that does not say which tool it calls, and one that says the wrong one.
    let call = json!({ "name": "list_tasks", "arguments": {} });
    let (status, body, _) = Request::post("tools/call")
        .version(PIN)
        .meta_with_version(PIN)
        .params(call.clone())
        .without_name_header()
        .send(&url)
        .await;
    assert_eq!(status, 400, "`tools/call` must name the tool: {body}");

    let (status, body, _) = Request::post("tools/call")
        .version(PIN)
        .meta_with_version(PIN)
        .params(call)
        .name("run_status")
        .send(&url)
        .await;
    assert_eq!(status, 400, "and it must name the right one: {body}");

    // The agreeable form, which is what the rest of the suite sends.
    let (status, body, _) = Request::post("tools/call")
        .version(PIN)
        .meta_with_version(PIN)
        .params(json!({ "name": "list_tasks", "arguments": {} }))
        .send(&url)
        .await;
    assert_eq!(status, 200, "headers and body in agreement: {body}");

    server.shutdown().await;
}

// ==================================================================== error mapping
//
// ADR-0001 §5 splits failures in two: a *tool* failure is an `isError: true` result the caller
// can read and correct, a *protocol* failure is a JSON-RPC error. SEP-1303 is the same rule from
// the client's side. Getting this backwards is the single most common MCP server bug, because it
// is invisible until an agent's retry loop hits it.

/// An unknown tool is a JSON-RPC error (`-32602`), not a tool result: the call never named
/// anything that could fail.
#[tokio::test]
async fn an_unknown_tool_is_a_json_rpc_error() {
    let fixture = Fixture::new();
    let server = TestServer::start(fixture.config()).await;

    let (status, body, _) = Request::post("tools/call")
        .version(PIN)
        .name("no_such_tool")
        .meta_with_version(PIN)
        .params(json!({"name": "no_such_tool", "arguments": {}}))
        .send(&server.url())
        .await;

    assert_eq!(status, 200, "the envelope still arrives: {body}");
    assert_eq!(
        body["error"]["code"],
        json!(INVALID_PARAMS),
        "an unknown tool is rmcp's invalid params, as ADR-0001 §5.1 pins: {body}"
    );
    assert!(
        body["result"].is_null(),
        "and not reported as a tool that ran: {body}"
    );

    server.shutdown().await;
}

/// Malformed tool arguments must be an `isError: true` **result** with the ADR-0001 §5.2
/// payload — and must carry **no** JSON-RPC `error` member (SEP-1303). That distinction is what
/// this server does by hand instead of letting `rmcp` deserialize, and it is exactly the thing a
/// later "simplify this" refactor would break.
#[tokio::test]
async fn malformed_tool_arguments_are_an_is_error_result_with_no_json_rpc_error() {
    let fixture = Fixture::new();
    let server = TestServer::start(fixture.config()).await;
    let url = server.url();

    // Wrong type for a known argument.
    let body = call_tool(&url, "list_tasks", json!({"include_steps": "yes"})).await;
    assert!(
        body.get("error").is_none(),
        "a bad argument must never become a JSON-RPC error: {body}"
    );
    assert_eq!(body["result"]["isError"], json!(true), "{body}");
    assert_eq!(
        body["result"]["structuredContent"]["error"]["code"],
        json!("invalid_arguments"),
        "{body}"
    );
    assert!(
        body["result"]["structuredContent"]["error"]["detail"]["errors"]
            .as_array()
            .is_some_and(|errors| !errors.is_empty()),
        "and it says which argument and how: {body}"
    );
    assert_eq!(
        body["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .split(':')
            .next()
            .unwrap_or_default(),
        "list_tasks",
        "the text block leads with `<tool>: <code>:` for a client that only reads text: {body}"
    );

    // An argument the tool does not define.
    let body = call_tool(
        &url,
        "list_tasks",
        json!({"run_id": "20260101T000000Z-abcdef"}),
    )
    .await;
    assert!(body.get("error").is_none(), "{body}");
    assert_eq!(body["result"]["isError"], json!(true), "{body}");
    assert_eq!(
        body["result"]["structuredContent"]["error"]["code"],
        json!("unknown_argument"),
        "{body}"
    );

    // A tool whose own gate refuses the call fails the same way — same shape, different code.
    let body = call_tool(&url, "run_status", json!({"run_id": "not-a-run-id"})).await;
    assert!(body.get("error").is_none(), "{body}");
    assert_eq!(body["result"]["isError"], json!(true), "{body}");

    server.shutdown().await;
}

/// A request that is not a well-formed JSON-RPC message is refused at the transport, always as
/// a 4xx, and never answered with a result.
///
/// `rmcp` maps "the body would not deserialize" to **415** rather than the 400 a strict reading
/// of the spec's error table suggests. This asserts the real status and says so out loud: the
/// conformance suite's job is to record what the endpoint does, and if `rmcp` ever changes this
/// the diff lands here too rather than in a client.
#[tokio::test]
async fn a_malformed_json_rpc_request_is_refused_without_running_a_tool() {
    let fixture = Fixture::new();
    let server = TestServer::start(fixture.config()).await;
    let url = server.url();

    for (label, body) in [
        ("not JSON at all", "this is not json".to_owned()),
        (
            "JSON but no JSON-RPC",
            json!({"hello": "world"}).to_string(),
        ),
        ("no method", json!({"jsonrpc":"2.0","id":1}).to_string()),
        (
            "unknown method",
            json!({"jsonrpc":"2.0","id":1,"method":"who/knows"}).to_string(),
        ),
        ("an array", json!([1, 2, 3]).to_string()),
        ("empty", String::new()),
    ] {
        let response = raw_client()
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", PIN)
            .body(body)
            .send()
            .await
            .unwrap_or_else(|error| panic!("{label} must be answered: {error}"));
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        assert!(
            (400..500).contains(&status),
            "{label} must be a 4xx refusal, got {status}: {text}"
        );
        let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        assert!(
            parsed["result"].is_null(),
            "{label} must not be served a result: {text}"
        );
    }

    // A well-formed envelope naming a method that does not exist gets the JSON-RPC answer for
    // that (`-32601`), which `rmcp` maps onto HTTP 404.
    let (status, body, _) = Request::post("who/knows")
        .version(PIN)
        .meta_with_version(PIN)
        .send(&url)
        .await;
    assert_eq!(status, 404, "unknown method: {body}");
    assert_eq!(
        body["error"]["code"],
        json!(METHOD_NOT_FOUND),
        "with the code for exactly that: {body}"
    );

    // The endpoint is unchanged afterwards: a refusal is not a crash.
    let (status, body, _) = Request::post("tools/list")
        .version(PIN)
        .meta_with_version(PIN)
        .send(&url)
        .await;
    assert_eq!(status, 200, "still serving: {body}");

    server.shutdown().await;
}

/// A notification with nothing behind it is accepted and ignored — the JSON-RPC rule for a
/// message with no answer, and the one shape of "cancellation" a stateless endpoint can always
/// honour without lying.
#[tokio::test]
async fn an_unknown_notification_is_accepted_and_ignored() {
    let fixture = Fixture::new();
    let server = TestServer::start(fixture.config()).await;

    for (method, params) in [
        (
            "notifications/cancelled",
            json!({"requestId": 999_999, "reason": "nobody is waiting"}),
        ),
        ("notifications/initialized", json!({})),
        ("$/cancel-request", json!({"requestId": 999_999})),
        ("notifications/dont_look_it_up", json!({})),
    ] {
        let (status, body, _) = Request::notification(method, params)
            .version(PIN)
            .send(&server.url())
            .await;
        assert_eq!(status, 202, "`{method}` is accepted: {body}");
        assert!(
            body.get("error").is_none(),
            "and never answered with an error: {body}"
        );
    }

    let (status, body, _) = Request::post("tools/list")
        .version(PIN)
        .meta_with_version(PIN)
        .send(&server.url())
        .await;
    assert_eq!(status, 200, "and nothing broke: {body}");

    server.shutdown().await;
}
// ==================================================================== cancellation
//
// ADR-0004 §5 gives a caller three ways to stop a run, and the ticket's question is which of
// them the *protocol* actually honours. All three are exercised against the shipped binary with
// a real `act` stub asleep, because cancellation of something that is not running proves
// nothing:
//
// 1. `cancel_run` from any other connection — the documented, application-level path, which
//    releases a `wait: true` call with the terminal `cancelled` result;
// 2. hanging up on the in-flight request — the transport-level path `rmcp` wires to the
//    per-request cancellation token, which this server maps onto the run's cancellation;
// 3. `notifications/cancelled` naming the in-flight request id — accepted, and **not**
//    dispatched in stateless mode: `rmcp`'s stateless branch answers every notification with
//    `202 Accepted` without handing it to the service, so `ActMcp::on_cancelled`'s waiter-map
//    fallback is unreachable over this transport (it is the documented fallback for transports
//    that do dispatch notifications). The test pins the real behaviour and says which half of
//    it is `rmcp`'s, so that the day the SDK starts dispatching stateless notifications the diff
//    lands here too.

/// A cancellation scenario: the shipped binary on an ephemeral port, a git workspace, and an
/// `act` stub that stays asleep until it is stopped.
struct Live {
    /// Held, never read: dropping it deletes the workspace under the running server.
    _tmp: tempfile::TempDir,
    repo: common::TestRepo,
    process: ServeProcess,
}

impl Live {
    /// Boot with an `act` that sleeps `secs` — long enough that no test here can mistake the
    /// stub finishing on its own for a cancellation.
    fn boot(stub_sleep: &str) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir for the cancellation fixture");
        let root = std::fs::canonicalize(tmp.path()).expect("canonical tempdir");
        let repo = git_workspace(
            &root.join("workspace"),
            &[
                ("ci.yml", DEFAULT_WORKFLOW_BODY),
                ("other.yml", DEFAULT_WORKFLOW_BODY),
            ],
        );
        let scratch = root.join("scratch");
        std::fs::create_dir_all(&scratch).expect("create the scratch root");
        // The e2e stubs: `act` is steered by environment (see its header comment) and the stub
        // `docker` on PATH is what answers the container probe on a daemon-less machine.
        let bin = e2e_bin();
        let options = ServeOptions::new(repo.path(), &bin.join("act"), &scratch, &bin)
            .with_env("STUB_SLEEP", stub_sleep)
            .with_env("STUB_PAYLOAD", "none");
        let process = ServeProcess::start(&options);
        Self {
            _tmp: tmp,
            repo,
            process,
        }
    }

    fn url(&self) -> String {
        self.process.url()
    }

    /// `host:port` for a raw socket to the same endpoint.
    fn host(&self) -> String {
        self.process.addr().to_string()
    }

    /// The one run this scenario has started, as `run_status`' listing describes it.
    ///
    /// Polled, because between the request arriving and the run being registered the validation
    /// gates run real `git` and the container probe, and a test that reads the listing the
    /// millisecond after writing its request would be racing them.
    async fn only_run(&self) -> Value {
        for _ in 0..100 {
            let runs = self.listed_runs().await;
            match runs.len() {
                0 => tokio::time::sleep(Duration::from_millis(100)).await,
                1 => return runs[0].clone(),
                more => panic!("these scenarios start exactly one run, saw {more}"),
            }
        }
        panic!("no run was ever registered;\n{}", self.process.transcript());
    }

    /// Every run `run_status`' listing mode currently reports.
    async fn listed_runs(&self) -> Vec<Value> {
        let body = call_tool(&self.url(), "run_status", json!({})).await;
        body["result"]["structuredContent"]["runs"]
            .as_array()
            .unwrap_or_else(|| panic!("run_status did not answer with a listing: {body}"))
            .clone()
    }

    /// `active_count` from the same listing — the number that says whether anything leaked.
    async fn active_count(&self) -> i64 {
        let body = call_tool(&self.url(), "run_status", json!({})).await;
        body["result"]["structuredContent"]["active_count"]
            .as_i64()
            .unwrap_or_else(|| panic!("run_status did not answer with a listing: {body}"))
    }

    /// Poll until the scenario's run is terminal (or the budget runs out, which is itself the
    /// failure: a leaked run is what this is looking for).
    async fn wait_for_terminal_run(&self) -> Value {
        for _ in 0..200 {
            let run = self.only_run().await;
            if run["terminal"] == json!(true) {
                return run;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!(
            "the run never reached its terminal commit;\n{}",
            self.process.transcript()
        );
    }
}

/// The `run_workflow` arguments every cancellation scenario uses: a run that will still be
/// going when the test decides to stop it.
fn running_arguments(commit: &str) -> Value {
    json!({
        "workflow": "ci",
        "task": "build",
        "category": "other",
        "commit": commit,
        "timeout_secs": 120,
    })
}

/// `cancel_run` stopped a run that a `tools/call` was still waiting on, and the waiting call
/// comes back with that result instead of hanging.
///
/// This is the cancellation a client is actually meant to use, and the assertions are the
/// ticket's: the answer arrives (`isError: false`, `status: cancelled`), the run is terminal and
/// no longer counted as active — no leaked task still driving it — and the endpoint still
/// answers afterwards, so nothing panicked on the way.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_cancelled_under_a_waiting_call_releases_that_call() {
    let live = Live::boot("120");
    let url = live.url();
    let host = live.host();
    let commit = live.repo.head();

    let body = json!({
        "jsonrpc": "2.0", "id": 5, "method": "tools/call",
        "params": { "name": "run_workflow", "arguments": running_arguments(&commit),
                    "_meta": meta_2026_07_28() }
    })
    .to_string();
    let (reader, socket) = open_request(&host, &body).await;
    let waiting = tokio::spawn(read_answer(reader));

    let run = live.only_run().await;
    let run_id = run["run_id"]
        .as_str()
        .expect("the run is listed")
        .to_owned();
    assert_eq!(run["terminal"], json!(false), "it is still running: {run}");

    // Cancel it from a different connection, while the first call is still blocked.
    let receipt = call_tool(&url, "cancel_run", json!({"run_id": &run_id})).await;
    assert_eq!(
        receipt["result"]["isError"],
        json!(false),
        "cancel_run is a normal result: {receipt}"
    );

    let (status, answer) = tokio::time::timeout(ANSWER_BUDGET, waiting)
        .await
        .expect("the waiting call is released by its run's terminal commit")
        .expect("the reader task");
    assert_eq!(status, 200, "the waiting call answers: {answer}");
    let view = &answer["result"]["structuredContent"];
    assert_eq!(
        view["status"],
        json!("cancelled"),
        "with the run's real outcome: {answer}"
    );
    assert_eq!(view["terminal"], json!(true), "{answer}");
    assert_eq!(view["run_id"], json!(&run_id), "{answer}");
    assert_eq!(
        answer["result"]["isError"],
        json!(false),
        "a cancelled run is a reported outcome, not a tool failure: {answer}"
    );
    drop(socket);

    // No leaked work: the listing is terminal and counts nothing active.
    let settled = live.only_run().await;
    assert_eq!(settled["status"], json!("cancelled"), "{settled}");
    assert_eq!(settled["terminal"], json!(true), "{settled}");
    assert_eq!(
        live.active_count().await,
        0,
        "nothing is still active: a terminal run that is still counted is a leaked run"
    );

    // And a server that cancelled a run is still a server.
    let (status, body, _) = Request::post("tools/list")
        .version(PIN)
        .meta_with_version(PIN)
        .send(&url)
        .await;
    assert_eq!(status, 200, "the endpoint is unharmed: {body}");
}

/// `notifications/cancelled` naming an in-flight `tools/call` is accepted and changes nothing.
///
/// Two things are asserted, and the second is the interesting one: the notification is a
/// well-formed message so the endpoint takes it (`202`, no error, no panic), and — because
/// `rmcp`'s stateless branch never dispatches notifications — the run it names keeps running.
/// That is not this server ignoring the client; it is the transport's, and the honest thing is
/// to pin it, name it, and keep the run under test until it is stopped by a path that does work.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancellation_notification_for_an_in_flight_call_is_answered_and_harmless() {
    let live = Live::boot("120");
    let url = live.url();
    let host = live.host();
    let commit = live.repo.head();

    // The in-flight call carries a request id the cancellation can name.
    let body = json!({
        "jsonrpc": "2.0", "id": 777, "method": "tools/call",
        "params": { "name": "run_workflow", "arguments": running_arguments(&commit),
                    "_meta": meta_2026_07_28() }
    })
    .to_string();
    let (reader, socket) = open_request(&host, &body).await;
    let waiting = tokio::spawn(read_answer(reader));

    let run = live.only_run().await;
    let run_id = run["run_id"].as_str().expect("run_id").to_owned();

    for (method, params) in [
        (
            "notifications/cancelled",
            json!({ "requestId": 777, "reason": "conformance probe" }),
        ),
        // The pre-MCP spelling some clients still emit; unknown methods are ignored, not fatal.
        ("$/cancel-request", json!({ "requestId": 777 })),
    ] {
        let (status, answer, _) = Request::notification(method, params)
            .version(PIN)
            .send(&url)
            .await;
        assert_eq!(status, 202, "`{method}` is accepted: {answer}");
        assert!(answer.get("error").is_none(), "{answer}");
    }

    // The call is still in flight, and so is the run it waits on.
    assert!(
        !waiting.is_finished(),
        "an accepted notification did not tear the waiting call down"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    let still = live.only_run().await;
    assert_eq!(
        still["terminal"],
        json!(false),
        "the stateless transport does not route the notification to the handler: {still}"
    );

    // Stop it the way that does work, and let the waiting call finish.
    call_tool(&url, "cancel_run", json!({ "run_id": &run_id })).await;
    let (_status, answer) = tokio::time::timeout(ANSWER_BUDGET, waiting)
        .await
        .expect("the waiting call is released once the run is terminal")
        .expect("the reader task");
    assert_eq!(
        answer["result"]["structuredContent"]["status"],
        json!("cancelled"),
        "{answer}"
    );
    drop(socket);

    let settled = live.only_run().await;
    assert_eq!(settled["terminal"], json!(true), "{settled}");
    assert_eq!(
        live.active_count().await,
        0,
        "the cancelled run is finished, not merely forgotten"
    );
}

/// Hanging up on an in-flight `tools/call` cancels the run it was waiting on.
///
/// This is the transport-level cancellation `rmcp` wires for a stateless request: the handler's
/// cancellation token fires when the response can no longer be delivered, and ADR-0001 §4 makes
/// "the request went away" mean "stop the work". The observable parts, in order: the run goes
/// `cancelled`, nothing is left active, and the endpoint is fine.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hanging_up_on_an_in_flight_call_cancels_the_run_behind_it() {
    let live = Live::boot("120");
    let url = live.url();
    let host = live.host();
    let commit = live.repo.head();

    let body = json!({
        "jsonrpc": "2.0", "id": 9, "method": "tools/call",
        "params": { "name": "run_workflow", "arguments": running_arguments(&commit),
                    "_meta": meta_2026_07_28() }
    })
    .to_string();
    let (reader, writer) = open_request(&host, &body).await;
    let run = live.only_run().await;
    assert_eq!(run["terminal"], json!(false), "{run}");

    // Hang up: closing both halves is the disconnect, from this side of the socket.
    drop(reader);
    drop(writer);

    let settled = live.wait_for_terminal_run().await;
    assert_eq!(
        settled["status"],
        json!("cancelled"),
        "a caller that hangs up stops the work it asked for: {settled}"
    );
    assert_eq!(
        live.active_count().await,
        0,
        "the run is finished, not merely forgotten"
    );

    let (status, body, _) = Request::post("tools/list")
        .version(PIN)
        .meta_with_version(PIN)
        .send(&url)
        .await;
    assert_eq!(status, 200, "the endpoint survived the disconnect: {body}");
}

// ==================================================================== graceful shutdown
//
// ADR-0004 §9.2's ordering is "stop accepting, take the runs down concurrently, drain, then end
// the sessions". From the client's side the only promise that matters is the one the ticket
// names: a call that was in flight when the signal arrived gets an answer.

/// SIGTERM with a `wait: true` call in flight: the call is answered, not abandoned, and the
/// process exits on its own.
///
/// `run_workflow`'s wait is released by the shutdown sweep's terminal commit, so the client
/// gets a normal result whose status says the server went away — and the whole process is gone
/// seconds later. "Hanging forever" is the failure this exists to catch, so both waits are
/// bounded and named.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_call_in_flight_at_shutdown_is_answered_and_the_process_exits() {
    let mut live = Live::boot("120");
    let host = live.host();
    let commit = live.repo.head();

    let body = json!({
        "jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": { "name": "run_workflow", "arguments": running_arguments(&commit),
                    "_meta": meta_2026_07_28() }
    })
    .to_string();
    let (reader, socket) = open_request(&host, &body).await;
    let waiting = tokio::spawn(read_answer(reader));

    let run = live.only_run().await;
    assert_eq!(run["terminal"], json!(false), "{run}");

    live.process.signal_term();

    let started = std::time::Instant::now();
    let (status, answer) = tokio::time::timeout(ANSWER_BUDGET, waiting)
        .await
        .unwrap_or_else(|_| {
            panic!(
                "the in-flight call is answered rather than dropped on the floor;\n{}",
                live.process.transcript()
            )
        })
        .expect("the reader task");
    let answered_in = started.elapsed();
    drop(socket);

    assert_eq!(status, 200, "the call is answered: {answer}");
    let view = &answer["result"]["structuredContent"];
    assert!(
        view["terminal"] == json!(true),
        "with a terminal run, so the caller is not left polling a ghost: {answer}"
    );
    assert!(
        [json!("error"), json!("cancelled")].contains(&view["status"]),
        "the shutdown outcome is reported as the run's own status: {answer}"
    );
    assert!(
        answered_in < Duration::from_secs(20),
        "and quickly enough that no client timed out first: {answered_in:?}"
    );

    let exited = std::time::Instant::now();
    while !live.process.has_exited() {
        assert!(
            exited.elapsed() < Duration::from_secs(30),
            "the process did not exit after SIGTERM;\n{}",
            live.process.transcript()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        live.process.transcript().contains("act-mcp stopped"),
        "it stopped by itself, and said so;\n{}",
        live.process.transcript()
    );

    // The port was the server's to release; with the process gone the socket must refuse.
    assert!(
        tokio::net::TcpStream::connect(&host).await.is_err(),
        "nothing is listening any more"
    );
}

// ==================================================================== helpers
//
// Everything above talks to an endpoint; these are the two dialects it speaks — a typed-ish
// `tools/call` through a shared client, and hand-built requests where the point of the test is
// the header that is missing.

/// Where the e2e suite's stub `act` and stub `docker` live; reused by the run-based tests here
/// so there is exactly one fake `act` in the tree.
fn e2e_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("e2e")
        .join("testdata")
        .join("bin")
}

/// How long a test waits for a call that should be released: generous enough that only a real
/// hang fails it, short enough that a hang is not the last thing anyone learns today.
const ANSWER_BUDGET: Duration = Duration::from_secs(30);

/// One `tools/call`, returning the decoded envelope. Asserts nothing: several tests need to look
/// at an envelope that is deliberately an error.
async fn call_tool(url: &str, name: &str, arguments: Value) -> Value {
    call_tool_with(&raw_client(), url, name, arguments).await
}

/// [`call_tool`] over a client the caller chose.
async fn call_tool_with(
    client: &reqwest::Client,
    url: &str,
    name: &str,
    arguments: Value,
) -> Value {
    let mut params = json!({ "name": name, "arguments": arguments });
    params["_meta"] = meta_2026_07_28();
    let response = client
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", PIN)
        .header("Mcp-Method", "tools/call")
        .header("Mcp-Name", name)
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":params}))
        .send()
        .await
        .unwrap_or_else(|error| panic!("tools/call {name} must be answered: {error}"));
    let text = response.text().await.expect("body is readable");
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("not JSON ({error}): {text}"))
}

/// The front of a body, for an assertion message. A wrong-but-successful `tools/list` answer is
/// tens of kilobytes, and a failure that scrolls a terminal past the interesting line teaches
/// nothing; the short refusal any of these expected fits comfortably.
fn brief(body: &Value) -> String {
    const LIMIT: usize = 400;
    let text = body.to_string();
    if text.len() <= LIMIT {
        return text;
    }
    format!(
        "{}… (+{} bytes)",
        &text[..char_boundary(&text, LIMIT)],
        text.len() - char_boundary(&text, LIMIT)
    )
}

/// The largest offset `<= want` that does not split a character.
fn char_boundary(text: &str, want: usize) -> usize {
    let mut end = want.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

/// The `error.code` of a JSON-RPC error envelope, if it has one.
fn error_code(body: &Value) -> Option<i64> {
    body["error"]["code"].as_i64()
}

/// The `error.message` of a JSON-RPC error envelope, if it has one.
fn error_message(body: &Value) -> Option<String> {
    body["error"]["message"].as_str().map(str::to_owned)
}

/// A hand-built JSON-RPC request, with every header independently optional — that is the whole
/// point: a conformance test needs to be able to *omit* a header a typed client will not let you
/// omit, and to put one in that contradicts the body.
///
/// Defaults: `POST`, both media types accepted, `application/json`, SEP-2243 headers derived
/// from the body, no protocol version anywhere.
#[derive(Clone)]
struct Request {
    method: String,
    params: Value,
    version: Option<String>,
    accept: String,
    content_type: String,
    session_id: Option<String>,
    meta: Option<Value>,
    method_header: Option<String>,
    name_header: Option<String>,
    sep_headers: bool,
    notification: bool,
}

impl Request {
    fn post(method: &str) -> Self {
        Self {
            method: method.to_owned(),
            params: json!({}),
            version: None,
            accept: "application/json, text/event-stream".to_owned(),
            content_type: "application/json".to_owned(),
            session_id: None,
            meta: None,
            method_header: None,
            name_header: None,
            sep_headers: true,
            notification: false,
        }
    }

    /// A notification: same shape, no `id`, and the endpoint must not answer it with a result.
    fn notification(method: &str, params: Value) -> Self {
        Self {
            notification: true,
            params,
            ..Self::post(method)
        }
    }

    /// A `server/discover`, which the pinned revision requires to be self-signalled.
    fn discover() -> Self {
        Self::post("server/discover")
            .version(PIN)
            .meta_with_version(PIN)
    }

    fn version(mut self, version: &str) -> Self {
        self.version = Some(version.to_owned());
        self
    }

    /// `params._meta` naming `version` plus the client info the revision demands.
    fn meta_with_version(mut self, version: &str) -> Self {
        self.meta = Some(json!({
            "io.modelcontextprotocol/protocolVersion": version,
            "io.modelcontextprotocol/clientInfo": { "name": "conformance-suite", "version": "0" },
            "io.modelcontextprotocol/clientCapabilities": {}
        }));
        self
    }

    fn params(mut self, params: Value) -> Self {
        self.params = params;
        self
    }

    fn name(mut self, name: &str) -> Self {
        self.name_header = Some(name.to_owned());
        self
    }

    fn accept(mut self, accept: &str) -> Self {
        self.accept = accept.to_owned();
        self
    }

    fn content_type(mut self, content_type: &str) -> Self {
        self.content_type = content_type.to_owned();
        self
    }

    fn session_id(mut self, session_id: &str) -> Self {
        self.session_id = Some(session_id.to_owned());
        self
    }

    /// Send `Mcp-Method` with a value that is not the body's method (SEP-2243 says no).
    fn method_header(mut self, value: &str) -> Self {
        self.method_header = Some(value.to_owned());
        self
    }

    /// Drop the SEP-2243 headers entirely.
    fn without_sep_headers(mut self) -> Self {
        self.sep_headers = false;
        self
    }

    /// Send `Mcp-Method` but no `Mcp-Name`, which a `tools/call` requires.
    fn without_name_header(mut self) -> Self {
        self.name_header = Some(String::new());
        self
    }

    /// Send it, and hand back the status, the decoded body (if it is JSON) and the headers.
    async fn send(self, url: &str) -> (u16, Value, reqwest::header::HeaderMap) {
        let mut params = self.params.clone();
        if let Some(meta) = self.meta.clone() {
            params
                .as_object_mut()
                .expect("params is an object")
                .insert("_meta".to_owned(), meta);
        }
        let mut message = json!({"jsonrpc":"2.0","method":self.method,"params":params});
        if !self.notification {
            message["id"] = json!(1);
        }

        let mut request = raw_client()
            .post(url)
            .header("Content-Type", &self.content_type)
            .header("Accept", &self.accept);
        if let Some(version) = &self.version {
            request = request.header("MCP-Protocol-Version", version);
        }
        if let Some(session_id) = &self.session_id {
            request = request.header("Mcp-Session-Id", session_id);
        }
        if self.sep_headers {
            request = request.header(
                "Mcp-Method",
                self.method_header
                    .as_deref()
                    .unwrap_or(self.method.as_str()),
            );
            let name = self.name_header.clone().or_else(|| {
                params
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            });
            if let Some(name) = name.filter(|name| !name.is_empty()) {
                request = request.header("Mcp-Name", name);
            }
        }

        let response = request
            .json(&message)
            .send()
            .await
            .unwrap_or_else(|error| panic!("{} must be answered: {error}", self.method));
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let text = response.text().await.unwrap_or_default();
        let body = if text.trim().is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or(Value::String(text))
        };
        (status, body, headers)
    }
}

/// Send a raw request on a socket the test owns, and return both halves of it.
///
/// Owning it is the point: two of these tests need to hang up mid-call, which no HTTP client lets
/// you do precisely enough to be a test. The write half staying alive is what keeps the
/// connection open while a task reads the response off the read half; dropping both is the
/// hang-up. `Connection: close` means a completed response also closes the socket, so
/// [`read_answer`] needs no framing of its own.
async fn open_request(
    host: &str,
    body: &str,
) -> (
    tokio::net::tcp::OwnedReadHalf,
    tokio::net::tcp::OwnedWriteHalf,
) {
    let stream = tokio::net::TcpStream::connect(host)
        .await
        .unwrap_or_else(|error| panic!("connecting to {host}: {error}"));
    let (reader, mut writer) = stream.into_split();
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\nMCP-Protocol-Version: {PIN}\r\n\
         Mcp-Method: tools/call\r\nMcp-Name: run_workflow\r\nConnection: close\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    writer
        .write_all(request.as_bytes())
        .await
        .expect("write the request");
    writer.flush().await.expect("flush the request");
    (reader, writer)
}

/// Read a whole response: the status line's code and the JSON body, or `Null` if it never got
/// one (which is what a refusal that answers with a bare status looks like).
async fn read_answer(mut reader: tokio::net::tcp::OwnedReadHalf) -> (u16, Value) {
    let mut raw = Vec::new();
    let read = tokio::time::timeout(
        ANSWER_BUDGET + Duration::from_secs(5),
        reader.read_to_end(&mut raw),
    )
    .await
    .expect("the endpoint answers or closes within the budget");
    assert!(read.is_ok(), "the response stream did not end cleanly");
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|token| token.split('.').next())
        .and_then(|token| token.parse::<u16>().ok())
        .unwrap_or(0);
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .and_then(|body| serde_json::from_str::<Value>(body).ok())
        .unwrap_or(Value::Null);
    (status, body)
}
