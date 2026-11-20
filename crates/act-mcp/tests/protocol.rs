//! Protocol-surface tests: what a client can and cannot negotiate, and what
//! `tools/list` answers. The tools are `list_tasks` (mact-f1w.7), `run_status` and
//! `cancel_run` (mact-f1w.11) — `run_workflow` joins with mact-f1w.14 — and the tool-specific
//! assertions live in `tests/list_tasks.rs`, `tests/run_status_tool.rs` and
//! `tests/cancel_run_tool.rs`; these stay focused on the protocol.
//!
//! This is the small smoke set — the things that have to work for any client at all, on either
//! of the two lifecycles this server speaks: the self-contained `2026-07-28` one and the
//! `2025-11-25` handshake. The full MCP conformance matrix (transport headers, error mapping,
//! cancellation, shutdown, the session's own rules) is `tests/protocol_conformance.rs`; when
//! you are about to add a case here, check whether it belongs there instead.
//!
//! Half of these use rmcp's own client (the "stock MCP client" in the ticket's
//! definition of done); the other half speak raw JSON-RPC over HTTP, because they
//! deliberately do the wrong thing on purpose.

mod common;

use common::{
    meta_2026_07_28, post_jsonrpc, post_jsonrpc_raw, raw_client, sse_data_lines, Fixture,
    TestServer,
};

use rmcp::{
    model::{ClientCapabilities, ClientConfig, Implementation, ProtocolVersion},
    transport::{
        streamable_http_client::StreamableHttpClientTransportConfig, StreamableHttpClientTransport,
    },
    ClientLifecycleMode, ClientServiceExt,
};
use serde_json::json;

/// Definition of done, literally: a stock client lists tools and gets a valid
/// envelope back — a single page with no cursor, and the tool set we advertise.
#[tokio::test]
async fn a_stock_client_can_list_tools() {
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
        .expect("a 2026-07-28 client can connect without initialize");

    let tools = client
        .list_tools(None)
        .await
        .expect("tools/list returns a result envelope");
    assert_eq!(
        tools
            .tools
            .iter()
            .map(|tool| tool.name.as_ref())
            .collect::<Vec<_>>(),
        vec!["cancel_run", "list_tasks", "run_status", "run_workflow"],
        "the tool set is exactly the tools this build registers, in the order rmcp sorts them"
    );
    assert!(
        tools.next_cursor.is_none(),
        "a one-page listing must not advertise a cursor"
    );

    client.cancel().await.expect("client shuts down cleanly");
    server.shutdown().await;
}

/// The same definition of done on the other lifecycle: a stock client that handshakes at
/// `2025-11-25` gets a session, is told that `2025-11-25` is the revision it is talking, and
/// lists the same four tools over the session. Nothing about the tool surface differs between
/// the two lifecycles — which is the point of supporting the second one.
#[tokio::test]
async fn a_stock_client_on_the_session_lifecycle_can_list_tools() {
    let fixture = Fixture::new();
    let server = TestServer::start(fixture.config()).await;

    let client = ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("act-mcp-test", "0.0.0"),
    )
    .with_protocol_version(ProtocolVersion::V_2025_11_25)
    .serve_with_lifecycle(
        StreamableHttpClientTransport::from_config(StreamableHttpClientTransportConfig::with_uri(
            server.url(),
        )),
        ClientLifecycleMode::Initialize,
    )
    .await
    .expect("a 2025-11-25 client can complete the initialize handshake");

    assert_eq!(
        client.peer_info().map(|info| info.protocol_version.clone()),
        Some(ProtocolVersion::V_2025_11_25),
        "the handshake must name the revision the client asked for, not this server's newest"
    );

    let tools = client
        .list_tools(None)
        .await
        .expect("tools/list over a session returns a result envelope");
    assert_eq!(
        tools
            .tools
            .iter()
            .map(|tool| tool.name.as_ref())
            .collect::<Vec<_>>(),
        vec!["cancel_run", "list_tasks", "run_status", "run_workflow"],
        "the session-era listing is the same tool set the stateless one advertises"
    );

    client.cancel().await.expect("client shuts down cleanly");
    server.shutdown().await;
}

/// The endpoint answers a raw `tools/list` too — i.e. it is not rmcp-client-only.
#[tokio::test]
async fn a_raw_tools_list_request_returns_the_tool_array() {
    let fixture = Fixture::new();
    let server = TestServer::start(fixture.config()).await;

    let (status, body) = post_jsonrpc(
        &raw_client(),
        &server.url(),
        "tools/list",
        json!({ "_meta": meta_2026_07_28() }),
        Some("2026-07-28"),
    )
    .await;

    assert_eq!(status, 200, "body: {body}");
    assert_eq!(
        body["result"]["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .map(|tool| tool["name"].as_str().unwrap_or_default())
            .collect::<Vec<_>>(),
        vec!["cancel_run", "list_tasks", "run_status", "run_workflow"],
        "body: {body}"
    );
    assert!(
        body.get("error").is_none(),
        "expected a result, got an error: {body}"
    );

    server.shutdown().await;
}

/// `server/discover` advertises exactly the two revisions this server speaks — the pin is
/// visible from outside, not just asserted in source — oldest first.
#[tokio::test]
async fn discover_advertises_the_two_supported_revisions() {
    let fixture = Fixture::new();
    let server = TestServer::start(fixture.config()).await;

    let (status, body) = post_jsonrpc(
        &raw_client(),
        &server.url(),
        "server/discover",
        json!({ "_meta": meta_2026_07_28() }),
        Some("2026-07-28"),
    )
    .await;

    assert_eq!(status, 200, "body: {body}");
    assert_eq!(
        body["result"]["supportedVersions"],
        json!(["2025-11-25", "2026-07-28"]),
        "server must expose exactly the revisions it implements: {body}"
    );

    server.shutdown().await;
}

/// Discover also carries the server identity, so this is where `server_info`
/// (name `act-mcp`, version = `CARGO_PKG_VERSION`) shows up on the wire.
#[tokio::test]
async fn discover_reports_the_server_name_and_version() {
    let fixture = Fixture::new();
    let server = TestServer::start(fixture.config()).await;

    let (status, body) = post_jsonrpc(
        &raw_client(),
        &server.url(),
        "server/discover",
        json!({ "_meta": meta_2026_07_28() }),
        Some("2026-07-28"),
    )
    .await;

    assert_eq!(status, 200, "body: {body}");
    let meta = &body["result"]["_meta"];
    let info = meta
        .get("io.modelcontextprotocol/serverInfo")
        .or_else(|| meta.get("serverInfo"))
        .unwrap_or(&serde_json::Value::Null);
    assert_eq!(info["name"], json!("act-mcp"), "serverInfo: {info}");
    assert_eq!(
        info["version"],
        json!(env!("CARGO_PKG_VERSION")),
        "serverInfo: {info}"
    );

    server.shutdown().await;
}

/// A client that tries to `initialize` a revision this server does not serve does not get it.
/// What it gets instead is the newest revision in the advertised set that a handshake can run
/// at — `2025-11-25` — which is the MCP handshake's own rule (answer with the latest version
/// you support and let the client decide whether it can speak it) and is rmcp's, not a
/// negotiation this crate re-implements. The invariant this test pins is the one that matters
/// from outside: the negotiated revision is always one of the advertised two, and never the
/// unadvertised one the caller asked for.
#[tokio::test]
async fn initialize_never_agrees_to_a_revision_outside_the_advertised_set() {
    let fixture = Fixture::new();
    let server = TestServer::start(fixture.config()).await;

    // `2025-06-18` is older than anything this server serves; `2026-07-28` is newer but has no
    // handshake at all, so a server that answers a handshake can never name it either.
    for revision in ["2025-06-18", "2025-03-26", "2026-07-28"] {
        let (status, text, headers) = post_jsonrpc_raw(
            &raw_client(),
            &server.url(),
            "initialize",
            json!({
                "protocolVersion": revision,
                "capabilities": {},
                "clientInfo": { "name": "legacy-client", "version": "0.0.0" }
            }),
            Some(revision),
            None,
        )
        .await;

        assert_eq!(status, 200, "a handshake is answered, not dropped: {text}");
        assert!(
            headers.contains_key("mcp-session-id"),
            "and a handshake-era client is given the session its revision needs: {headers:?}"
        );
        let negotiated = sse_data_lines(&text)
            .into_iter()
            .find_map(|data| {
                data.pointer("/result/protocolVersion")
                    .and_then(|value| value.as_str())
                    .map(str::to_owned)
            })
            .unwrap_or_default();
        assert_ne!(
            negotiated, revision,
            "a revision outside the set must never be agreed to: {text}"
        );
        assert_eq!(
            negotiated, "2025-11-25",
            "the handshake lands on the newest advertised revision that has one: {text}"
        );
    }

    server.shutdown().await;
}

/// Same rule for the per-request metadata form the 2026-07-28 revision uses.
#[tokio::test]
async fn per_request_metadata_naming_an_older_revision_is_refused() {
    let fixture = Fixture::new();
    let server = TestServer::start(fixture.config()).await;

    let (status, body) = post_jsonrpc(
        &raw_client(),
        &server.url(),
        "tools/list",
        json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2025-03-26",
                "io.modelcontextprotocol/clientInfo": { "name": "legacy", "version": "0.0.0" },
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        }),
        Some("2025-03-26"),
    )
    .await;

    assert!(
        body.get("error").is_some(),
        "older per-request version must be refused: status={status} body={body}"
    );
    let message = body["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_lowercase();
    assert!(
        message.contains("protocol") || message.contains("version"),
        "the error should say why: {body}"
    );

    server.shutdown().await;
}

/// The endpoint lives at `/mcp` and nowhere else.
#[tokio::test]
async fn requests_outside_the_mcp_route_are_not_served() {
    let fixture = Fixture::new();
    let server = TestServer::start(fixture.config()).await;

    let response = raw_client()
        .post(format!("http://{}/wrong", server.addr))
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
        .send()
        .await
        .expect("request reaches the server");
    assert_eq!(response.status().as_u16(), 404);

    server.shutdown().await;
}
