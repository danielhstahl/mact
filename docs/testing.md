# Testing act-mcp

Everything here is runnable on a laptop with **no docker and no `act` installed** — every probe
of the outside world goes through `workspace::Environment`, and the `act` the tests run is a
`/bin/sh` script. Real `git` is required (the isolation gates *are* git); real containers are
never required by any test in the tree.

## 1. Running the suites

```sh
cargo test                    # everything: unit tests + every integration suite
cargo test --test <name>      # one suite, e.g. cargo test --test protocol_conformance
cargo test -- --ignored       # the two PR_SET_PDEATHSIG tests, which wait on wall-clock time
```

`scripts/ci.sh` is the one command CI runs, and it is this section as a script: fmt, clippy
`-D warnings`, rustdoc, `cargo test --all-features`, the schema checkers, `cargo deny`, and
a release build that is then run to prove it works. `scripts/ci.sh --help` lists the steps;
[docs/ci.md](ci.md) documents them, including the `rust-toolchain.toml` pin that makes a
local run and a CI run the same claim.

Two environment quirks worth knowing before the first red build:

* on this repo's dev box `rustc` takes SIGBUS while linking inside a bind-mounted `/workspace`,
  so build and test with `CARGO_TARGET_DIR=/tmp/tt` if you see it;
* `scripts/validate_evidence.py` needs the `jsonschema` Python package. If the interpreter and
  `pip` on your box disagree (they do here), use the built-in checker instead:
  `cargo run -- validate-evidence <workspace>`.

The shell checkers under `scripts/` (`validate_tool_contract.py`, `validate_run_lifecycle.py`,
`validate_isolation.py`) are CI's, and each one's Rust counterpart is named in the file it
guards.

| suite | what it pins |
|---|---|
| `src/**` unit tests | argument shapes, argv construction, error vocabularies, extraction, the evidence writer |
| `tests/boot.rs` | CLI flags, the boot gates, refusing a routable bind, what gets logged |
| `tests/hardening.rs` | `Host`/`Origin` allowlists, body cap, redaction, run-dir cap |
| `tests/protocol.rs` | the smallest useful protocol surface: a stock client can list tools, `/mcp` is the only route |
| **`tests/protocol_conformance.rs`** | **the full MCP 2026-07-28 Streamable HTTP conformance matrix — see §3** |
| `tests/tool_schema_parity.rs` | advertised `inputSchema`/`outputSchema` == `schema/tool-contract.schema.json` |
| `tests/list_tasks.rs`, `tests/run_status_tool.rs`, `tests/cancel_run_tool.rs`, `tests/run_workflow.rs` | one tool each, end to end through a real `tools/call` |
| `tests/run_supervisor.rs`, `tests/runner_execution.rs`, `tests/runner_pdeathsig.rs` | run lifecycle, the act process group, process teardown |
| `tests/e2e/main.rs` | the shipped binary + stub `act` + stub `docker`: call → `.evidence` files on disk, goldens included |
| `tests/evidence_*.rs`, `tests/*_extraction.rs` | the `.evidence` JSONL contract and the extractors |

## 2. Before trusting a green run

`cargo test` here is the whole contract, so a red test is a contract break and gets read as one.
Two conventions make the suite stay honest:

* **Assertions are observations.** Nothing reads a private constant to prove a behaviour; the
  protocol tests ask the endpoint over HTTP and check what came back. Removing a pin in
  `src/server.rs` must therefore fail a test — if it does not, the test is the bug.
* **Where the answer is `rmcp`'s, say so.** Several refusals in the conformance suite are the
  SDK's status code and message, not ours. They are still pinned (that is how a silent SDK
  change shows up as a diff instead of a client bug report), and the comment over each one names
  which half is whose.

## 3. Protocol conformance (`tests/protocol_conformance.rs`)

The question this suite answers is the auditor's: *is `POST /mcp` a correct MCP **2026-07-28**
Streamable HTTP endpoint, judged from outside the process?* Two clients ask it — a stock `rmcp`
client (`client` + `transport-streamable-http-client-reqwest`, pinned to `2026-07-28`) and raw
HTTP for the requests a well-behaved client never sends. The run-based cases boot the **shipped
binary** on an ephemeral port against the e2e stub `act`, so cancellation and shutdown include
the runner and its process group.

Covered, in the order the file is written:

* **Advertised surface.** `tools/list` returns every registered tool with a legal SEP-986 name, a
  non-empty title and description, an `inputSchema`, and an `outputSchema` — and an actual
  `structuredContent` validates against the `outputSchema` it came with (compiled as JSON Schema,
  not eyeballed).
* **Negotiation.** `server/discover` exposes exactly `["2026-07-28"]`; a client naming
  `2025-11-25`, `2025-06-18` or `2025-03-26` is refused with `-32022` and the supported set;
  a request that names **no** version is refused with `-32020` rather than silently read as the
  pre-2026 dialect (that is the `stateless_protocol_metadata_required = true` decision, and this
  is where it is asserted); a header and a `_meta` that disagree are refused.
* **Statelessness.** Three self-contained requests on three fresh connections with no handshake;
  a legacy `initialize` (any version) gets `-32022` **as an answer** — not a hang, not a session.
* **Transport headers.** `application/json` answers, no `Mcp-Session-Id` ever (in either
  direction), `Accept` must cover both `application/json` and `text/event-stream` (406
  otherwise), `Content-Type` must be JSON with an optional charset (415 otherwise), `GET`/`PUT`/
  `HEAD`/`OPTIONS`/`DELETE` on `/mcp` are 4xx refusals with `Allow: POST` and never a 500, and
  nothing outside the `/mcp` prefix is mounted. The SEP-2243 routing headers
  (`Mcp-Method`/`Mcp-Name`) are required and must agree with the body.
* **Error mapping.** Unknown tool → JSON-RPC `-32602`; malformed or unknown *arguments* →
  `isError: true` **result** with the ADR-0001 §5.2 payload and **no** JSON-RPC `error` member
  (SEP-1303); a body that is not JSON, or JSON that is not JSON-RPC, is a 4xx with no result; an
  unknown method is `-32601`.
* **Cancellation.** `cancel_run` under a `wait: true` call releases that call with `cancelled`;
  hanging up on the in-flight request cancels the run behind it; `notifications/cancelled` naming
  an in-flight request is accepted and harmless (see the known gap below). In every case the run
  ends terminal, `active_count` returns to 0, and the endpoint still answers.
* **Graceful shutdown.** SIGTERM with a call in flight: the call is answered with a terminal
  result, and the process exits by itself.

### Two observations that belong to `rmcp`, not to us

Both are asserted as they actually behave, and both are worth knowing before you debug a client:

1. **A body that will not deserialize is refused `415`**, not `400` ("fail to deserialize request
   body …"). Strictly the spec's error table suggests 400; `rmcp` answers 415 and this suite
   pins 415 so the change is a diff here rather than a surprise in the field.
2. **`notifications/cancelled` is not delivered in stateless mode.** `rmcp`'s stateless branch
   answers every notification with `202 Accepted` without dispatching it to the service, so
   `ActMcp::on_cancelled` (the waiter-map fallback documented in ADR-0004 §5.3) is unreachable
   over this transport. Cancellation that *does* work today: `cancel_run` from any connection,
   and the transport-level cancel that fires when the client hangs up — both tested. A future
   `rmcp` that dispatches stateless notifications will fail
   `a_cancellation_notification_for_an_in_flight_call_is_answered_and_harmless`, which is the
   intended outcome: go read it, then update it.

Also note the endpoint is **nested** at `/mcp`: the service does not look at the remaining path,
so `/mcp/anything` is the same endpoint and answers, while everything outside that prefix is a
plain 404 from the router.

## 4. Reproducing by hand: the MCP Inspector

A test suite can be wrong in a way only a human notices — a UI that cannot connect, a result that
renders as a blob, a refusal that looks like a hang. Walk it once with the Inspector before you
report a protocol bug, and again after you fix one.

```sh
# terminal 1 — the server, on the default loopback port
cargo run -- serve --workspace ~/repo/with/github-workflows --act-binary ./tests/e2e/testdata/bin/act

# terminal 2 — the Inspector
npx @modelcontextprotocol/inspector --transport http http://127.0.0.1:8080/mcp
```

If your Inspector offers a protocol-version or "negotiation" override, pin it to **`2026-07-28`**
(older Inspectors only speak the `initialize`-era revisions, and this server has no handshake for
them). Then:

| # | where to click | what you must see |
|---|---|---|
| 1 | **Connect** | connected over Streamable HTTP, no `initialize` round trip in the log. If it fails with an unsupported-version refusal, that is the pin: the Inspector offered only pre-2026 revisions. |
| 2 | **List Tools** | four tools — `cancel_run`, `list_tasks`, `run_status`, `run_workflow` — each with a real description and both schemas shown |
| 3 | **Run Tool** `list_tasks` `{}` | `isError: false`, a structured result with `workflows[]`; `workflows_dir` is `.github/workflows` |
| 4 | **Run Tool** `run_status` `{}` | `active_count: 0`, `runs: []` on a quiet server |
| 5 | **Run Tool** `run_workflow` with a real `workflow`/`task`/`commit` from step 3, `"wait": false` | a `run_view` with a `run_id`; then `run_status(run_id)` polls it to a terminal state |
| 6 | **Run Tool** `run_status` `{"run_id": "nope"}` | `isError: true` and a `structuredContent.error.code` — a tool failure, not a transport error |
| 7 | **Tools List** → any tool → **Get Tool Info** → **Load Tool** | the Inspector validates our `outputSchema` against the result it just got; no validation warnings |

Notes for the human in the loop:

* one run at a time per workspace: a second `run_workflow` while one is going answers
  `workspace_busy` naming the run that holds it — that is the contract (ADR-0001 §5.3), not a bug.
* the Inspector runs in a browser, so its requests carry an `Origin`. By default **every** `Origin`
  is refused (403) — that is `docs/security.md` §2. To let the Inspector talk to the server, name
  it: `act-mcp serve --allow-origin 'http://localhost:*'`.
* to see a refusal that only a legacy client can produce, point any pre-2026 client at the
  endpoint and expect a JSON-RPC `-32022` *answer* naming `["2026-07-28"]` as the supported set.
  It must answer, not hang.

### When the Inspector cannot speak 2026-07-28

Any HTTP client can, because the revision is self-contained per request. This is the request the
manual checks above reduce to:

```sh
curl -sS http://127.0.0.1:8080/mcp \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -H 'MCP-Protocol-Version: 2026-07-28' \
  -H 'Mcp-Method: tools/call' \
  -H 'Mcp-Name: list_tasks' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
        "name":"list_tasks","arguments":{},
        "_meta":{
          "io.modelcontextprotocol/protocolVersion":"2026-07-28",
          "io.modelcontextprotocol/clientInfo":{"name":"manual","version":"0"},
          "io.modelcontextprotocol/clientCapabilities":{}}}}'
```

Drop any one of the four `MCP-Protocol-Version` / `_meta` / `Mcp-Method` signals and you should get
a `400` refusal that says which one — never a result, never a hang. `Accept` must name both media
types (406 if you name one); `Content-Type` must be `application/json` (415 otherwise).
