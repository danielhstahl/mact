# Testing act-mcp

Everything here is runnable on a laptop with **no docker and no `act` installed** — every probe
of the outside world goes through `workspace::Environment`, and the `act` the tests run is a
`/bin/sh` script. Real `git` is required (the isolation gates *are* git); real containers are
never required by anything that runs by default. The one place real containers are needed is
[§7](#7-the-real-act-smoke-manual--nightly), and it is `#[ignore]`d and nightly for exactly
that reason.

## 1. Running the suites

```sh
cargo test                    # everything: unit tests + every integration suite
cargo test --test <name>      # one suite, e.g. cargo test --test protocol_conformance
cargo test -- --ignored       # the runner_pdeathsig tests (they wait on wall-clock time),
                              # the one helper they re-spawn to test themselves with,
                              # …and, since mact-f1w.19, the real-act gate, which needs act and
                              # a container runtime: run it as §7 describes, not by reflex
```

`scripts/ci.sh` is the one command CI runs, and it is this section as a script: fmt, clippy
`-D warnings`, rustdoc, `cargo test --all-features`, the schema checkers, `cargo deny`, and
a release build that is then run to prove it works. `scripts/ci.sh --help` lists the steps;
[docs/ci.md](ci.md) documents them, including the `rust-toolchain.toml` pin that makes a
local run and a CI run the same claim.

A few environment quirks worth knowing before the first red build:

* on this repo's dev box `rustc` takes SIGBUS while linking inside a bind-mounted `/workspace`,
  so build and test with `CARGO_TARGET_DIR=/tmp/tt` if you see it;
* on macOS `$TMPDIR` is under `/var`, and `/var` is a symlink to `/private/var`, so every
  temp directory a test makes has two spellings of one directory. Anything in a test that
  compares a path the code resolved (`Path::canonicalize`, or act's own `pwd`) against a path
  the test assembled can therefore disagree with itself on macOS and agree on Linux, where
  `/tmp` is a real directory. Reproduce it here without a Mac: `ln -s /tmp /tmp/link-tmp`
  and run `TMPDIR=/tmp/link-tmp cargo test`;
* the process-group tests read `/proc` (`runner_execution`'s `live_pids_in_group`,
  `process_group_of`), which Linux has and macOS does not: a `/proc` assertion in a test is
  either gated `#[cfg(target_os = "linux")]` or cross-checked against a syscall that exists
  on both (`getpgrp(2)`, `getpgid(2)`, `killpg(2, 0)`), because the library's own `/proc` read
  has a fallback and the test must not be the only thing that assumes procfs.
* "act dies with act-mcp" is a different mechanism per platform, and they are not scoped the
  same way: Linux arms the child with `PR_SET_PDEATHSIG`, which is tied to the spawning
  **thread**, while macOS has no such option and `runner::process::watcher` forks a helper
  that `kevent`s for the server's exit — a net scoped to the **process**. `tests/runner_pdeathsig.rs`
  (`--ignored`) retires a thread on Linux and a process on both, because a mechanism that is
  process-scoped is not proved by a thread vanishing and vice versa. Anything that changes
  `die_with_parent` gets run there **on both platforms**: a watcher that misidentifies its own
  parent `SIGKILL`s act at `exec`, and every run on macOS then goes `pending → error`
  (`act_crashed`, because a signalled process has no exit code) — which reads like a server
  that never booted, not like a signal bug.
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
| `tests/realdata_smoke.rs` | the real-act smoke script's own plumbing (stub), and — `#[ignore]`d — the real act + runtime run of it (**§7**) |
| `tests/evidence_*.rs`, `tests/*_extraction.rs` | the `.evidence` JSONL contract and the extractors |

## 2. Before trusting a green run

`cargo test` here is the whole contract, so a red test is a contract break and gets read as one.
Three conventions make the suite stay honest:

* **Assertions are observations.** Nothing reads a private constant to prove a behaviour; the
  protocol tests ask the endpoint over HTTP and check what came back. Removing a pin in
  `src/server.rs` must therefore fail a test — if it does not, the test is the bug.
* **Where the answer is `rmcp`'s, say so.** Several refusals in the conformance suite are the
  SDK's status code and message, not ours. They are still pinned (that is how a silent SDK
  change shows up as a diff instead of a client bug report), and the comment over each one names
  which half is whose.
* **Paths are compared canonically, on both sides.** The crate resolves paths — the workspace
  at boot, a workflow file to prove it did not escape `.github/workflows` (ADR-0001 §2.2),
  act's own cwd — so a test that compares its own assembled path against a resolved one is
  comparing a spelling against a place. It passes on Linux and fails on a `$TMPDIR` that is a
  symlink (§1), which is how `--run-scratch` inside the workspace came to be waved through on
  macOS: `Config` canonicalises the workspace and the comparison only resolved one side.
  Use `crate::fixtures::canonical` (integration tests already take canonical paths from
  `TestServer`), and expect the same of the product: a relpath that comes back absolute is a
  bug, not a platform quirk — ADR-0001 §2.2 keeps absolute host paths out of the payloads.

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

The same requests, without the curl flags: `scripts/mcp_call.sh` is the checked-in hand client
(the one the [README runbook](../README.md#the-five-minute-tour) drives), and it exits non-zero
when the tool answered `isError: true`, so it composes in a shell loop.

```sh
scripts/mcp_call.sh --tools-list
scripts/mcp_call.sh list_tasks '{"workflow":"coverage","include_steps":true}'
scripts/mcp_call.sh run_status '{"limit":5}' | jq .result.structuredContent
scripts/mcp_call.sh --url http://127.0.0.1:9000/mcp cancel_run '{"run_id":"20261110T004203Z-a1b2c3"}'
```

## 5. Reproducing the on-disk contract without a run

`docs/evidence-format.md` is the reader's contract, and its recipes are meant to be run, not
admired. Two scripts keep them honest, and both work with no docker and no act:

```sh
scripts/sample_evidence.py            # lay the e2e goldens out as a real .evidence tree (3 commits, 5 runs)
scripts/evidence_recipes.sh           # every jq recipe in that doc, in order, against that tree
scripts/evidence_recipes.sh <your>/.evidence   # the same recipes against a real evidence root
```

They need `jq` (verified against 1.7.1) and are **not wired into CI**, because CI does not install
jq and adding a pinned third-party binary is a supply-chain change that does not belong in a docs
ticket. Run them by hand before touching anything under `src/evidence/`, `schema/evidence.schema.json`,
or `docs/evidence-format.md`: a recipe that no longer parses, or that prints nothing, means the
document and the writer have parted company. `scripts/sample_evidence.py` also stamps two
re-stamped "later" commits into the sample tree, because one commit cannot exercise a cross-commit
scan; `--no-extra-commits` leaves them out.

## 6. `scripts/mcp_call.sh`

The same requests, without the curl flags: `scripts/mcp_call.sh` is the checked-in hand client
(the one the [README runbook](../README.md#the-five-minute-tour) drives), and it exits non-zero
when the tool answered `isError: true`, so it composes in a shell loop.

`commit` is a **commit abbreviation**, not a ref name: pass `$(git rev-parse --short HEAD)` (or an
older abbreviation — the scan walks `git log` back to it). `"HEAD"` is not one, and the tool error
says so rather than running a job you did not ask for.

```sh
scripts/mcp_call.sh --tools-list
scripts/mcp_call.sh list_tasks '{"workflow":"coverage","include_steps":true}'
scripts/mcp_call.sh run_status '{"limit":5}' | jq .result.structuredContent
scripts/mcp_call.sh --url http://127.0.0.1:9000/mcp cancel_run '{"run_id":"20261110T004203Z-a1b2c3"}'
```

## 7. The real-act smoke (manual / nightly)

Everything in §1–§6 runs against a stub, which is what makes it fast, hermetic and reviewable —
and it is also why **the assumptions about `act` itself are the ones with no test behind them**.
Which flags `act` accepts; what `--json` log fields the extractors' field tables claim; whether
`--bind` really makes a file a *job* wrote visible on the host; whether `--job x` also runs `x`'s
`needs:` chain; whether act honours `SIGTERM`; how many seconds a trivial job is. A stub cannot
contradict an assumption about the thing it is standing in for, so those claims sat as prose in
the ADRs. `scripts/smoke-real-act.sh` is the script that goes and asks.

```sh
cargo build --release
scripts/smoke-real-act.sh --help        # the whole surface, from the script itself
scripts/smoke-real-act.sh               # the real thing: act on PATH + a running docker
scripts/smoke-real-act.sh --prereq-only # just the "can this machine do it" report
scripts/smoke-real-act.sh --stub        # the identical steps against the e2e stubs: seconds, no docker
cargo test --test realdata_smoke -- --ignored --nocapture   # the same run, as the gated test
```

It prints one `PASS`/`FAIL`/`SKIP` line per step with the evidence paths, and finishes with the
per-run table — wall time, act's own `duration_ms`, `act.jsonl` bytes, evidence bytes, each
against the ADR-0004 §4.2 default for that category. That table is the ticket's other
deliverable: ADR-0004 reasoned about 900 s for coverage and 1800 s for benchmark without ever
measuring a job, and this is the measurement. Expect run 1 to be dominated by the container image
pull and run 2 to be the warm number worth comparing.

### What it actually settles

Before it touches the fixture the script checks **argv acceptance**: every flag
`src/runner/argv.rs` emits must appear in `act --help`. It needs no daemon, so it runs even on a
box with no container runtime, and it is the fact most likely to be quietly wrong — an unknown flag
is a cobra error that arrives as a failing workflow rather than as "unknown flag", and
`--use-gitignore` is emitted whenever the workspace carries no `.actrc`. Checked against
**act v0.2.63** it passes, and that pins down something the ADR left vague: `--use-gitignore`
exists in 0.2.63 and defaults to `true`, so ADR-0002 dating it from v0.2.80 is about behaviour,
not existence — and the claim ADR-0002 actually leans on (act copies `.gitignore`d paths into the
container) holds whichever way the flag goes.

| step | the claim, and where the claim lives |
|---|---|
| prerequisites | act exists and is ≥ `MINIMUM_ACT_VERSION`; a runtime answers **the same probe the server uses**; there is disk for an image |
| prerequisites (argv) | real `act` documents every flag `src/runner/argv.rs` emits — needs no daemon, and an unknown flag here would otherwise look like a failing workflow |
| `payload crossed the bind mount` | ADR-0003 **F8/V1**: under `--bind` the container path *is* the host path, so `$ACTMCP_EVIDENCE_OUT` works. If this fails, ADR-0002 §3.1's payload transport does not work |
| `act log captured` | ADR-0003 **V2**: `--quiet=false` keeps step output in the `--json` stream, which is the only reason the in-band marker channel exists |
| `run_workflow(benchmark)` | the fixture's `benchmark` job fails unless act ran its `needs: [warmup]` dependency first (`--job` semantics, ADR-0003 §7.2) |
| `act --json shape` | the field table in `tests/realdata/golden/act-shape.json` against a live log — see below |
| `worktree isolation (V1)` | **V1** again, but for a worktree under `$XDG_RUNTIME_DIR`, which is where ADR-0003 §5 actually puts non-`HEAD` runs |
| `workspace .actrc is inert` | ADR-0003 **F2/F3**: a planted workspace `.actrc` pinning an unpullable image is *ignored*, because act reads `.actrc` from its cwd and the runner starts act in the run scratch (§12.3's first question) |
| `cancel leaves nothing` | ADR-0003 **V3** / ADR-0004 §11.2: a cancelled run reaches `cancelled`, and `docker ps -a --filter name=act` is the filter that finds act's leftovers (`--with-cancel`) |
| `validate-evidence`, `record values` | the tree a real run produced satisfies ADR-0002 and says the numbers the fixture hard-codes |

### Run it when

* **before touching anything act-facing**: `src/runner/argv.rs`, `src/evidence/benchmark/act_log.rs`,
  `src/runner/log.rs`, an ADR that cites an `F`/`V` row about act;
* **after an act release**, or after bumping the act version pinned in
  `.github/workflows/nightly-real-act.yml` (that pin is the supported range's upper edge — it is
  a deliberate, reviewable change, not a drift);
* **nightly**, by `.github/workflows/nightly-real-act.yml`. That job gates nothing — no branch
  protection, no merge queue — because a nightly that blocks a merge when a third party changed
  an image is a nightly people silence. It is nonetheless required to be *loud*: no
  `continue-on-error` anywhere, `--strict-shape` so a shape divergence turns the run red, a job
  summary, `::error::` annotations, and `if: always()` artefact uploads so a red run still
  carries its evidence. A red nightly is a report, not an outage.

`cargo test -- --ignored` therefore now includes something that needs a container runtime
(`tests/runner_pdeathsig.rs` aside, it only needs wall-clock time). That is deliberate and it is
documented at the test: the ignored test is the gate, and `the_smoke_script_passes_against_the_stub`
— which runs in ordinary CI and needs no docker — is what keeps the smoke script itself from
rotting. If you are on a machine without `jq`, the stub self-check says `SKIP` out loud instead
of pretending.

### The pre-flight, because most machines cannot do this

Real mode is not a flag you can wish into existence: it needs `act` **and** a live container
runtime. This repository's own development sandboxes have neither — no `docker` binary, no
socket, no `DOCKER_HOST` — and they are unprivileged (`uid 100`, empty capability set,
`unshare -Urm` → `EPERM`), so rootless docker and podman are not startable either. That is a
fact about the *machine*, not about the code, and finding it out an hour into a session is pure
waste. Ask the machine first; it costs two seconds:

```sh
scripts/smoke-real-act.sh --prereq-only   # the argv check, plus the probe the server itself uses
```

If it says "nothing answered `docker info …`", stop: no flag, stub or amount of patience
makes a real capture on this host. Either do it where a runtime exists — for this repository,
that means the nightly's `ubuntu-24.04` runner: dispatch
`.github/workflows/nightly-real-act.yml`, download the `smoke-artefacts-*` upload, read it, and
commit it from a machine that can push — or leave the two directories below alone. They are
empty because they are *waiting for a run*. An `act.jsonl` typed up by hand, or emitted by the
stub, or coaxed out of a Docker-API shim, is worse than an empty directory: it is the
third-party half of the `.evidence` contract, and a forged third party is exactly how a wrong
`job_duration_s` gets published for a year. `mact-n8n` — the ticket to commit these two
artifacts — was once claimed in precisely such a sandbox and closed for precisely this reason,
with no bytes invented.

### Two artifacts that are not goldens yet

`tests/realdata/golden/README.md` and `tests/realdata/expected/README.md` are worth reading
before you "fix" either directory. Both are empty of captured data on purpose:

* **`golden/act.jsonl`** is `act`'s own log from one real run. Nothing has been captured yet,
  because no machine in this repository's CI had act *and* a runtime when this landed, and a
  golden typed up from the same assumption it exists to police would be worse than none. The
  nightly captures one and uploads it; a human reviews and commits it.
* **`expected/*.jsonl`** is the byte-for-byte form of the records a real run publishes, with
  volatile fields replaced by named placeholders (`{RUN_ID}`, `{COMMIT}`, `{TS}`, `{HOST}`,
  `{NUMBER}` — the same rule as `tests/e2e/normalize.rs`). `--write-expected` writes them, in
  real mode only, and the script shouts that committing them is a human act.

Until then the smoke makes **value** assertions instead (the coverage arithmetic in the fixture,
the four benchmark metric names and their producers), which need no golden and still catch a
producer quietly stopping work. Committing the two real artifacts is **mact-n8n**, and that
stays open work for a machine that passes the pre-flight above — not a file to write.

### What a `DIVERGENCE` means

`tests/realdata/golden/act-shape.json` is the field table the extractors assume, and every row
carries its provenance: `source` (read out of act at a named tag) or `capture` (seen in a real
log). The shape step diffs that table against a live log and prints the difference. When it says

```text
DIVERGENCE — a field something in this crate reads did not hold up
```

open the follow-up ticket it offers, read the named extractor, and *then* decide whether act
changed or the assumption was wrong. Do not edit the manifest to match the log: the file exists
so a silent third-party change surfaces as a named gap instead of as a plausible number in
`.evidence` for a year. One gap is recorded there from reading act's source across the
supported range, and the smoke prints it whenever a capture triggers it:

* `executionTime` — the only source of step timings — exists from act **v0.2.76**; this server's
  floor is `MINIMUM_ACT_VERSION` **0.2.62**, so on `0.2.62 … 0.2.75` a benchmark run publishes
  no `act_log` timings at all. *Decided:* the floor stays 0.2.62 and the missing case is the named
  warning `act_step_timings_unavailable`, pinned by
  `tests/fixtures/benchmark/act-pre-0.2.76.jsonl` and asserted in `tests/benchmark_extraction.rs`
  — the reasoning is [docs/benchmark-inputs.md §3](benchmark-inputs.md#3-acts-own-timings-need-act-0276-or-newer),
  and what the smoke adds is confirming it against a live log.

A **retired** claim in that file lives under `resolved_gaps`, not in `fields` and not in
`known_gaps`. `Output`/`Error` are that entry: `act_log::messages()` used to read them as
alternative message fields although no released act emits either — dead code wearing a
compatibility costume — and mact-dfu deleted the read instead of explaining it. They are no longer
assumptions this crate makes, so they are no longer rows a live capture is checked against; what
the file keeps is the reasoning and the two tests that assert the phantom field is *not* read.
Nothing prints them on a run — a gap that fires on every capture trains nobody to read the ones
that matter.

The checker distinguishes three verdicts, and `scripts/realdata_report.py selftest` pins all
three (the Rust test runs it, so the checker is not itself the untested thing):

| what happened | verdict | red in the nightly? |
|---|---|---|
| a field something in this crate reads **went missing** or **changed type** | `DIVERGENCE` | yes — act broke a claim we depend on |
| a manifest row marked `phantom` (a field we once mistook for act's) is absent, as ever | a note about our dead code | no — cleanup, not a regression |
| act emitted a field no manifest row claims | `NEW` | no — documentation debt, add a row when a human agrees it is real |

In `--stub` mode the shape step runs against the stub's log fixture
(`tests/e2e/testdata/logs/`), so anything it reports there describes the stub. The script says
so; the claim about act needs the real run.

### Found by this script on its first runs (before it ever met real act)

`scripts/smoke-real-act.sh --stub`, which needs no docker, turned up a live interaction between
two accepted ADRs: ADR-0002 §6.4 makes the evidence writer append `.evidence/` and
`.act-mcp.lock` to the workspace repository's `.gitignore`, and if that `.gitignore` is a
**tracked** file that did not already list them, the append is a tracked modification — which
ADR-0003 G5 reads as `dirty_worktree`, so the *second* run in that workspace is refused until
the change is committed. `tests/realdata/fixture/.gitignore` commits those patterns, which makes
the append a no-op; the script recognises the failure shape and says what happened instead of
looking like a flaky server. The reconciliation (§6.4 vs G5) is a follow-up ticket, not a
doc fix.
