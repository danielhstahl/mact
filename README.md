# act-mcp

An HTTP [MCP](https://modelcontextprotocol.io) server — protocol revisions **2026-07-28** and
**2025-11-25**, Streamable HTTP, bound to loopback, **no authentication** — that runs GitHub
Actions workflows with [nektos/act](https://github.com/nektos/act) and records what they
produced as `.evidence` JSONL.

Four tools: `list_tasks`, `run_workflow`, `run_status`, `cancel_run`. A run outlives the
request that started it, one run at a time per workspace, and every terminal run answers
with the `.evidence` files it wrote — benchmark and coverage records stamped with the commit
they describe.

There is no auth, by design: the server binds `127.0.0.1` and refuses any other bind unless
you say `--i-understand-this-is-unauthenticated`, because act runs arbitrary container
images. [docs/security.md](docs/security.md) is the threat model.

This page is the runbook: what you need, how to start it, every flag, how to point a client
at it, what it leaves on disk, and what to do when it does not work. The reasoning is in the
ADRs; the links are at the bottom.

| thing | the version this runbook was written and tested against |
|---|---|
| MCP revisions | **`2026-07-28`** (stateless, no handshake) and **`2025-11-25`** (`initialize` + session) — nothing else is negotiated |
| `rmcp` | `3.9.0` (the SDK that implements the negotiation) |
| `rustc` | floor **1.88** (`Cargo.toml` `rust-version`); the pinned toolchain is **1.99.1** (`rust-toolchain.toml`, honoured by rustup in this directory) |
| act | **≥ 0.2.62**, which is what `--minimum-act-version` refuses to go below; the checked-in fixtures were captured against **0.2.68** |
| container runtime | any that answers `docker info` |
| `jq` (for the doc recipes only) | 1.7.1 |

## Contents

[Prerequisites](#prerequisites) · [Build](#build) · [Run](#run) ·
[Every flag and env var](#every-flag-and-env-var) · [Pointing a client at it](#pointing-a-client-at-it) ·
[The five-minute tour](#the-five-minute-tour) · [What `.evidence` looks like](#what-evidence-looks-like) ·
[Fathom](#fathom) · [Troubleshooting](#troubleshooting) · [Checks](#checks) · [Layout](#layout) ·
[Further reading](#further-reading)

## Prerequisites

**To run it:**

* **Rust 1.88 or newer.** `rust-toolchain.toml` pins 1.99.1 with `clippy` and `rustfmt`, and
  rustup installs it on first `cargo` command in this directory. 1.88 is the floor rmcp needs,
  not a recommendation.
* **`git`** on `PATH`. The commit gates in [ADR-0003](docs/adr/0003-commit-isolation-and-serialization.md)
  *are* `git` calls; the workspace you point at must be a git repository with at least one commit.
* **`act`** on `PATH` (or wherever `--act-binary` points). Version ≥ 0.2.62.
  <https://github.com/nektos/act#installation>.
* **A working container runtime** — the boot probe runs `docker info` (through the runtime's
  client, `docker`, found on `PATH`), and a `run_workflow` call refuses with
  `docker_unreachable` if nothing answers. Rootless podman with the docker socket present works;
  `--container-daemon-socket` pins a non-default one.
* **A workspace that is a git repo containing `.github/workflows/`.** Boot only checks the
  `.github` directory exists; workflow resolution happens per call.

**Not** needed to build it or to run the whole test suite: docker, act, or a registry pull. The
`act` the tests run is a shell script (`crates/act-mcp/tests/e2e/testdata/bin/act`) and the container daemon is
probed through an injectable seam — see [docs/testing.md](docs/testing.md).

## Build

```sh
cargo build                # everything: target/debug/act-mcp and target/debug/fathom
cargo build --release      # the same, released: target/release/act-mcp, target/release/fathom
cargo build -p act-mcp     # just the server crate — what CI's release artefact is built from
cargo build -p fathom      # just the TUI crate (no tokio, no rmcp, no axum in its graph)
```

A bare `cargo run -- …` no longer knows which binary you mean, because there are two of them now:
spell the package (`cargo run -p act-mcp -- serve …`, `cargo run -p fathom -- --help`).

> On a bind-mounted checkout (the case on this repo's dev box) the linker can take SIGBUS or
> fail with `Bad address (os error 14)` while writing into `./target`. The workaround is one
> environment variable, for build and test alike: `export CARGO_TARGET_DIR=/tmp/tt`.

## Run

```sh
cargo run --release -p act-mcp -- serve --workspace /path/to/a/git/repo/with/github/workflows
```

That is the whole command. A real startup, from the tour below, looks like this:

```console
$ cargo run --release -p act-mcp -- serve --workspace /tmp/actmcp-runbook/repo \
      --act-binary ./crates/act-mcp/tests/e2e/testdata/bin/act
2026-11-10T15:09:12.606305Z  INFO act_mcp::server: bind is loopback-only: only this machine
  can reach the endpoint (there is no authentication) bind=127.0.0.1:8080
2026-11-10T15:09:12.606542Z  INFO act_mcp::server: act-mcp listening url=http://127.0.0.1:8080/mcp
  bind=127.0.0.1:8080 workspace=/tmp/actmcp-runbook/repo
  evidence_dir=/tmp/actmcp-runbook/repo/.evidence
  act_binary=/workspace/crates/act-mcp/tests/e2e/testdata/bin/act protocol_versions=2025-11-25,2026-07-28
  allowed_hosts=["localhost", "127.0.0.1", "::1"] allowed_origins=[]
  max_request_body_bytes=1048576 shared_secret_required=false
```

The listening line is the whole configuration in one place: what it resolved `act` to, which
protocol revisions it will accept, which `Host` values it takes, and whether a session token is
required. A boot *after a crash* adds lines saying so, because the leftover per-run scratch of a
previous process is cleaned up on the way up: `removed a run's leftover scratch directory from a
previous process scratch=…`, then `cleared the leftover scratch directories of a previous
process leftover_run_scratch_dirs=3`.

Every boot-time refusal is the same shape — one line, the offending value, and the `fix:` that
changes it (`--bind 0.0.0.0:8080` is refused before the listener ever opens). The log is
`tracing` with the default filter `info,rmcp=warn`; `RUST_LOG=debug` (or `RUST_LOG=rmcp=debug`
for the transport's own chatter) turns it up. Ctrl-C stops it: a run in flight is asked to
finish, gets act's SIGTERM grace window, and is then taken down with its process group.

The binary has one other subcommand, and it is a CLI tool as much as a debug aid:

```sh
cargo run --release -p act-mcp -- validate-evidence <path-or-workspace> [--stats]
# exit 0 clean · 1 findings (file:line: reason) · 2 could not run
```

### Every flag and env var

A flag always wins over its env var (clap's `env` only fills in what you did not pass), and
every value is validated at boot rather than at call time. Nothing here is required except
`--workspace` in practice.

| flag | env var | default | what it does |
|---|---|---|---|
| `--workspace <PATH>` | `ACT_MCP_WORKSPACE` | current directory | the repository to act on; must exist, must contain `.github`, canonicalised once at boot |
| `--bind <ADDR>` | `ACT_MCP_BIND` | `127.0.0.1:8080` | listen address; **loopback only**. `:0` asks the kernel for a free port and logs the real one |
| `--i-understand-this-is-unauthenticated` | `ACT_MCP_I_UNDERSTAND_THIS_IS_UNAUTHENTICATED` | `false` | allow a non-loopback `--bind`. There is no auth; everyone who can route to the socket can run workflows here |
| `--evidence-dir <PATH>` | `EVIDENCE_DIR` | `<workspace>/.evidence` | where durable evidence lands (see [docs/evidence-format.md](docs/evidence-format.md)) |
| `--act-binary <PATH>` | `ACT_MCP_ACT_BINARY` | `act` | the act executable, resolved on `PATH` at boot (an absolute path is accepted and preferred) |
| `--minimum-act-version <X.Y.Z>` | `ACT_MCP_MINIMUM_ACT_VERSION` | `0.2.62` | refuse to run an act older than this; may be raised, never lowered below `0.2.62` |
| `--log-dir <PATH>` | `ACT_MCP_LOG_DIR` | `<evidence-dir>/runs` | where act's log capture (`act.jsonl`) goes |
| `--run-scratch <DIR>` | `ACT_MCP_RUN_SCRATCH` | `$XDG_RUNTIME_DIR/act-mcp-<uid>`, else `/tmp/act-mcp-<uid>` | per-run scratch: act's cwd, `secrets.env`, and an ADR-0003 worktree. **Must be outside `--workspace`** |
| `--isolation <POLICY>` | `ACT_MCP_ISOLATION` | `hybrid` | which trees may be run: `head-only`, `hybrid` (HEAD in place, anything else in a worktree), `worktree` (always a worktree) — [ADR-0003](docs/adr/0003-commit-isolation-and-serialization.md) |
| `--allow-dirty-worktree` | `ACT_MCP_ALLOW_DIRTY_WORKTREE` | `false` | run a dirty HEAD anyway, stamping the `dirty_worktree` warning; `commit` then means "HEAD plus whatever is in it" |
| `--git-timeout-secs <SECS>` | `ACT_MCP_GIT_TIMEOUT_SECS` | `30` | deadline for each read-only `git` call made while validating a run (1…300) |
| `--container-daemon-socket <PATH>` | `ACT_MCP_CONTAINER_DAEMON_SOCKET` | *(runtime default)* | socket the reachability probe dials instead of the default. act's own `-` ("do not mount it") is treated as unset here |
| `--list-tasks-act-cross-check` | `ACT_MCP_LIST_TASKS_ACT_CROSS_CHECK` | `false` | have `list_tasks` also run `act list` and report files act refuses to load. Costs a subprocess per call |
| `--allow-host <HOST>` | `ACT_MCP_ALLOWED_HOSTS` | `localhost,127.0.0.1,::1` | extra accepted `Host:` values, repeatable or comma-separated. Name your front-end here for a deliberate port-forward |
| `--allow-origin <ORIGIN>` | `ACT_MCP_ALLOWED_ORIGINS` | *(empty: every `Origin` is refused)* | extra accepted browser `Origin`s, repeatable; must carry a scheme, e.g. `http://localhost:8080`. You need this for a browser-based client ([docs/testing.md §4](docs/testing.md#4-reproducing-by-hand-the-mcp-inspector)) |
| `--max-request-body-bytes <BYTES>` | `ACT_MCP_MAX_REQUEST_BODY_BYTES` | `1048576` | largest POST body at `/mcp`; a bigger one is answered 413 (1024…67108864) |
| `--require-session-token` | `ACT_MCP_REQUIRE_SESSION_TOKEN` | `false` | require the `x-act-mcp-token` header on every request, generating a token and logging it once at boot |
| `--session-token <TOKEN>` | `ACT_MCP_SESSION_TOKEN` | *(unset)* | require exactly this token value (implies `--require-session-token`) |
| `--max-run-dirs <COUNT>` | `ACT_MCP_MAX_RUN_DIRS` | `1000` | refuse a new run once this many `.evidence/runs/<run_id>/` directories exist. Run retention prunes memory, never disk, so this is the disk guard |
| `--max-payload-keep-bytes <BYTES>` | `ACT_MCP_MAX_PAYLOAD_KEEP_BYTES` | `4194304` | how much of the payload a job produced (`lcov.info`, `coverage-summary.json`, `benchmark.json`) is kept per run in `.evidence/runs/<run_id>/payload/` after it has been read, so a coverage record can be checked against its source. Past the budget the overflow is deleted and the run warns `payload_cap_exceeded`; `0` keeps none |
| *(no flag)* | `RUST_LOG` | `info,rmcp=warn` | the `tracing` env filter |

Flags are also `-h`/`--help` (per-subcommand) and `-V`/`--version`. `validate-evidence` takes
`--evidence-dir`, `--schema`, `--meta-schema` and `--stats`.

## Pointing a client at it

The endpoint is **`http://127.0.0.1:8080/mcp`**, Streamable HTTP. On the revision this server
prefers — **2026-07-28** — it is `POST` only, with no `initialize` handshake and no session id:
each request is self-contained and must carry

* `MCP-Protocol-Version: 2026-07-28` **and** the matching `params._meta` block,
* the SEP-2243 routing headers `Mcp-Method` and `Mcp-Name`,
* `Accept` naming **both** `application/json` and `text/event-stream`,
* `Content-Type: application/json`.

Anything else is a refusal that names what was missing — never a hang. The full request-shape
table is [docs/tool-contract.md §1](docs/tool-contract.md#1-endpoint-and-transport).

A client that cannot do that can still connect: the server also speaks **2025-11-25**, the last
revision with the classic `initialize` handshake. Send `initialize` naming `2025-11-25`, keep
the `Mcp-Session-Id` the answer gives you, put `MCP-Protocol-Version: 2025-11-25` on every
request, and the same four tools answer the same way — over an SSE stream instead of a bare
JSON body, with `GET` for the server-initiated stream and `DELETE` to close the session. Every
example below is the stateless shape.

So the config for any client is one URL, in that client's own wrapper:

```json
{ "type": "http", "url": "http://127.0.0.1:8080/mcp" }
```

**Claude Code / Claude Desktop** (`.mcp.json` in a project, or `claude mcp add`):

```json
{
  "mcpServers": {
    "act-mcp": { "type": "http", "url": "http://127.0.0.1:8080/mcp" }
  }
}
```

```console
$ claude mcp add --transport http act-mcp http://127.0.0.1:8080/mcp
```

**VS Code / Copilot** (`.vscode/mcp.json`):

```json
{
  "servers": {
    "act-mcp": { "type": "http", "url": "http://127.0.0.1:8080/mcp" }
  }
}
```

**pi** (`~/.pi/agent/mcp.json`, or `.pi/mcp.json` for one project):

```json
{
  "mcpServers": {
    "act-mcp": { "url": "http://127.0.0.1:8080/mcp", "timeout": 1900 }
  }
}
```

```console
$ pi mcp add act-mcp --url http://127.0.0.1:8080/mcp
```

`timeout` is per-request seconds: a `wait: true` `run_workflow` blocks for the run plus the
supervisor's overhead, so the default 60 s is too short for a real benchmark.

> **What a client has to speak.** These config shapes are the portable part; the part that
> decides how the connection is opened is which revision your client implements. One that
> implements **2026-07-28** talks to this server statelessly, as above. One that insists on an
> `initialize` handshake gets **2025-11-25**: a handshake naming a revision this server does not
> run one at — including a client whose newest revision is `2026-07-28` but that handshakes
> anyway — is answered with `2025-11-25` and a session, which is the MCP handshake's own rule
> and not a downgrade into an unadvertised dialect. A *request* that names an unadvertised
> revision (`2025-06-18`, `2025-03-26`) is refused with JSON-RPC `-32022` *as an answer* naming
> `["2025-11-25","2026-07-28"]` as the supported set
> ([docs/testing.md §3](docs/testing.md)). `timeout`/`headers` keys differ per client; consult
> the client's own docs for anything beyond the URL, and expect to add `--allow-origin` for
> anything that runs in a browser ([docs/security.md §2](docs/security.md)).

When a client cannot be made to work, use the checked-in hand client — it speaks all four
signals and takes ten seconds to debug with:

```sh
scripts/mcp_call.sh --tools-list
scripts/mcp_call.sh list_tasks
scripts/mcp_call.sh list_tasks '{"workflow":"coverage","include_steps":true}'
scripts/mcp_call.sh run_status '{"limit":5}'
scripts/mcp_call.sh --url http://127.0.0.1:8080/mcp run_status '{}' | jq .result.structuredContent
```

`run_workflow` needs a real commit, so it appears in
[the tour](#the-five-minute-tour) with one. The exit status is 1 when the tool answered
`isError: true`, which makes it usable in a shell loop.

Raw, for the shape itself:

```sh
curl -sS http://127.0.0.1:8080/mcp \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -H 'MCP-Protocol-Version: 2026-07-28' \
  -H 'Mcp-Method: tools/call' -H 'Mcp-Name: list_tasks' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_tasks","arguments":{},
       "_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28",
                "io.modelcontextprotocol/clientInfo":{"name":"curl","version":"0"},
                "io.modelcontextprotocol/clientCapabilities":{}}}}'
```

And the same call from a client that only does the classic handshake — `initialize`, keep the
`Mcp-Session-Id`, say `notifications/initialized`, then call the tool over the session. The
answer is an SSE stream rather than a bare JSON body, and `DELETE` closes the session:

```sh
URL=http://127.0.0.1:8080/mcp
HDR=(-H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream'
     -H 'MCP-Protocol-Version: 2025-11-25')
SID=$(curl -sS -D - -o /dev/null "$URL" "${HDR[@]}" -H 'Mcp-Method: initialize' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25",
        "capabilities":{},"clientInfo":{"name":"curl","version":"0"}}}' \
  | tr -d '\r' | awk -F': ' '/^mcp-session-id/ {print $2}')
curl -sS "$URL" "${HDR[@]}" -H "Mcp-Session-Id: $SID" -H 'Mcp-Method: notifications/initialized' \
  -d '{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}'
curl -sS "$URL" "${HDR[@]}" -H "Mcp-Session-Id: $SID" \
  -H 'Mcp-Method: tools/call' -H 'Mcp-Name: list_tasks' \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_tasks","arguments":{}}}'
curl -sS -X DELETE "$URL" "${HDR[@]}" -H "Mcp-Session-Id: $SID"   # 202; the session is gone
```

## The five-minute tour

Run this once to see the whole contract with no docker and no act installed — the server is
started against the e2e stub `act` (`crates/act-mcp/tests/e2e/testdata/bin/act`) and its stub `docker`. If
you have the real thing, drop the `--act-binary` flag and the two `PATH=`/`STUB_PAYLOAD_DIR=`
assignments; **every other command in this section is identical either way.**

Set up a throwaway workspace (one terminal):

```sh
DEMO=/tmp/actmcp-runbook
rm -rf "$DEMO" && mkdir -p "$DEMO/repo/.github/workflows"
cp crates/act-mcp/tests/e2e/testdata/workflows/coverage.yml crates/act-mcp/tests/e2e/testdata/workflows/benchmark.yml \
   "$DEMO/repo/.github/workflows/"
git -C "$DEMO/repo" init -q -b main
git -C "$DEMO/repo" config user.name act-mcp && git -C "$DEMO/repo" config user.email act-mcp@example.invalid
git -C "$DEMO/repo" add -A && git -C "$DEMO/repo" commit -qm "two workflows to run"
```

Start the server (terminal 1):

```sh
PATH="$PWD/crates/act-mcp/tests/e2e/testdata/bin:$PATH" STUB_PAYLOAD_DIR="$PWD/crates/act-mcp/tests/e2e/testdata/payload" \
  cargo run --release -p act-mcp -- serve --workspace "$DEMO/repo" \
    --act-binary "$PWD/crates/act-mcp/tests/e2e/testdata/bin/act"
```

Drive it (terminal 2, from the repo). The order is the intended one: `list_tasks` to find a
`task` and its `category` hint, `run_workflow` to run it, then read the evidence paths it hands
back.

```sh
COMMIT=$(git -C /tmp/actmcp-runbook/repo rev-parse HEAD)

scripts/mcp_call.sh list_tasks \
  | jq -c '.result.structuredContent.workflows | map({name, jobs: [.jobs[].id], hint: [.jobs[].category_hint]})'
```

```
[{"name":"benchmark","jobs":["hyperfine-bench"],"hint":["benchmark"]},{"name":"coverage","jobs":["coverage"],"hint":["coverage"]}]
```

```sh
scripts/mcp_call.sh run_workflow "{\"workflow\":\"coverage\",\"task\":\"coverage\",\"category\":\"coverage\",\"commit\":\"$COMMIT\"}" \
  | jq '.result.structuredContent'
```

```json
{
  "category": "coverage",
  "commit": "cf0ecd0eee6a9c1b014b61867fb8646a624af95b",
  "deduplicated": false,
  "duration_ms": 6,
  "evidence_files": [
    ".evidence/coverage/cf0ecd0eee6a9c1b014b61867fb8646a624af95b/20261110T150920Z-d323b9.jsonl"
  ],
  "evidence_record_count": 4,
  "exit_code": 0,
  "finished_at": "2026-11-10T15:09:20.575Z",
  "log_tail": "{\"level\":\"info\",\"job\":\"coverage\",\"jobID\":\"coverage\",\"workflow\":\"<WORKSPACE>/.github/workflows/coverage.yml\",\"msg\":\"stub act started\"}\n",
  "phase": "done",
  "run_id": "20261110T150920Z-d323b9",
  "started_at": "2026-11-10T15:09:20.569Z",
  "status": "success",
  "task": "coverage",
  "terminal": true,
  "warnings": [
    "coverage_source_shadowed"
  ],
  "workflow": "coverage",
  "workflow_file": ".github/workflows/coverage.yml"
}
```

`evidence_files` is the answer you came for, and the paths are relative to the evidence root
(`log_tail` has the absolute workspace path substituted out). A run you do not want to block on
is `"wait": false`, which answers the moment the run is queued:

```sh
scripts/mcp_call.sh run_workflow "{\"workflow\":\"benchmark\",\"task\":\"hyperfine-bench\",\"category\":\"benchmark\",\"commit\":\"$COMMIT\",\"wait\":false}" \
  | jq -c '.result.structuredContent | {run_id, status, phase, evidence_files, evidence_record_count, warnings}'
```

```json
{"run_id":"20261110T150920Z-5f0ad2","status":"pending","phase":"queued","evidence_files":[],"evidence_record_count":0,"warnings":[]}
```

and `run_status` finds it once it is terminal (newest first; a `run_summary` carries no evidence
fields, so read the files, not the history):

```sh
scripts/mcp_call.sh run_status '{"limit":4}' \
  | jq -c '.result.structuredContent | {active_count, runs: [.runs[] | {run_id, workflow, status, phase, terminal, duration_ms}]}'
```

```json
{"active_count":0,"runs":[{"run_id":"20261110T150920Z-5f0ad2","workflow":"benchmark","status":"success","phase":"done","terminal":true,"duration_ms":7},{"run_id":"20261110T150920Z-d323b9","workflow":"coverage","status":"success","phase":"done","terminal":true,"duration_ms":6}]}
```

Then look at what it wrote:

```sh
find /tmp/actmcp-runbook/repo/.evidence -type f | sed 's|/tmp/actmcp-runbook/repo/||' | sort
cat /tmp/actmcp-runbook/repo/.gitignore        # act-mcp created it: evidence is local build output
cargo run --release -p act-mcp -- validate-evidence /tmp/actmcp-runbook/repo/.evidence --stats
```

```
.evidence/README.md
.evidence/benchmark/cf0ecd0eee6a9c1b014b61867fb8646a624af95b/20261110T150920Z-5f0ad2.jsonl
.evidence/coverage/cf0ecd0eee6a9c1b014b61867fb8646a624af95b/20261110T150920Z-d323b9.jsonl
.evidence/incoming/20261110T150920Z-5f0ad2/coverage-summary.json
.evidence/incoming/20261110T150920Z-5f0ad2/lcov.info
.evidence/incoming/20261110T150920Z-d323b9/benchmark.json
.evidence/incoming/20261110T150920Z-d323b9/coverage-summary.json
.evidence/runs/20261110T150920Z-5f0ad2/act.jsonl
.evidence/runs/20261110T150920Z-5f0ad2/meta.json
.evidence/runs/20261110T150920Z-5f0ad2/payload/benchmark.json
.evidence/runs/20261110T150920Z-d323b9/act.jsonl
.evidence/runs/20261110T150920Z-d323b9/meta.json
.evidence/runs/20261110T150920Z-d323b9/payload/lcov.info
# act-mcp evidence (local only)
.evidence/
.act-mcp.lock
benchmark	cf0ecd0eee6a9c1b014b61867fb8646a624af95b	3
coverage	cf0ecd0eee6a9c1b014b61867fb8646a624af95b	4
OK 4 file(s), 7 record(s) valid
```

Note where each payload file ended up: the file a run actually **read** is kept under
`runs/<run_id>/payload/` (`--max-payload-keep-bytes`, default 4 MiB per run), so a record can be
checked against its source; a file the extractor never used stays in `incoming/` as evidence about
the job, and `meta.json` lists it with its size.

To stop the server: Ctrl-C. Anything still running gets the SIGTERM window, then its process
group is taken down; `run_status` after a restart answers `run_not_found`, but the durable
record is still under `.evidence/runs/<run_id>/`.

## What `.evidence` looks like

One tree per workspace, one file per run per kind, published atomically and never rewritten.
**The full contract, with jq recipes, is
[docs/evidence-format.md](docs/evidence-format.md)**; this is the shape.

```
<workspace>/.evidence/
├── README.md                        # "local build output, safe to delete"
├── benchmark/<full-sha>/<run_id>.jsonl   ┐ the cross-commit contract: one file per run,
├── coverage/   <full-sha>/<run_id>.jsonl ┘ one kind, immutable once published
├── runs/<run_id>/act.jsonl               # act's own log, redacted (audit, not contract)
├── runs/<run_id>/meta.json               # terminal manifest, written LAST
├── runs/<run_id>/payload/                # the payload this run read, kept to check it against
└── incoming/<run_id>/                    # transient payload scratch
```

`run_id` is `YYYYMMDDTHHMMSSZ-<6 hex>` (`20261110T150920Z-5f0ad2`), so `ls` order is
chronological order, and the directory name is the full 40-hex commit a run is attributed to.
Two example lines, one per kind, straight out of the tour above (wrapped here, one line each on
disk):

```jsonc
// benchmark/<sha>/20261110T150920Z-5f0ad2.jsonl — one metric per line
{"schema_version":1,"kind":"benchmark","run_id":"20261110T150920Z-5f0ad2",
 "commit":"cf0ecd0eee6a9c1b014b61867fb8646a624af95b","workflow":"benchmark",
 "task":"hyperfine-bench","category":"benchmark","run_status":"success",
 "tool":{"name":"act-mcp","version":"0.1.0"},
 "started_at":"2026-11-10T15:09:20.826Z","duration_ms":4,"exit_code":0,"host":"1edcddb1ead5",
 "name":"tokenizer_throughput","value":184320.4,"unit":"ops/s",
 "labels":{"job":"hyperfine-bench","os":"ubuntu-24.04","runner":"self-hosted"},"sample":1,
 "source":"job_file","source_path":".evidence/incoming/20261110T150920Z-5f0ad2/benchmark.json"}

// coverage/<sha>/20261110T150920Z-d323b9.jsonl — one file (or the total) per line
{"schema_version":1,"kind":"coverage","run_id":"20261110T150920Z-d323b9",
 "commit":"cf0ecd0eee6a9c1b014b61867fb8646a624af95b","workflow":"coverage","task":"coverage",
 "category":"coverage","run_status":"success","tool":{"name":"act-mcp","version":"0.1.0"},
 "started_at":"2026-11-10T15:09:20.569Z","duration_ms":3,"exit_code":0,"host":"1edcddb1ead5",
 "name":"total","scope":"total","lines_found":9,"lines_hit":4,"branches_found":2,"branches_hit":1,
 "source":"lcov","source_path":".evidence/incoming/20261110T150920Z-d323b9/lcov.info"}
```

The three things a consumer most needs to know: there is **no `null` anywhere** (an unknown
value is absent, never `0` or `""`); **percentages are yours to compute** from the raw
numerators, never stored next to them; and **act-mcp appends and never aggregates** — no
mean, no percentile, no dedup across runs, because the scanner owns that.

Want to look at a tree without running anything?

```sh
scripts/sample_evidence.py                      # lays the e2e goldens out as a real tree
export EV=$(scripts/sample_evidence.py --quiet)  # the same, printing only the path
scripts/evidence_recipes.sh                      # every jq recipe, run against that tree in order
```

## Fathom

`fathom` is the other binary in this workspace, and one sentence about it stops the first
misunderstanding:

> **Fathom reads `.evidence`; it does not run anything.** No MCP client, no HTTP, no act, nothing that
> can start a run ([ADR-0005 §7](docs/adr/0005-fathom-crate-layout-and-dependencies.md)). It opens the
> evidence act-mcp already wrote, plus `git log`, and shows what it finds — offline, on a plane, with
> the server stopped.

What it is: a per-commit timeline of the four things this repository measures — **coverage**, the
**maintainability index** of the code in that commit, **benchmark** groups, and whether the **actions**
(workflows) that ran there passed — one row per commit, plus a diff view, a per-commit detail view, a
benchmark panel, a machine-readable `report` for CI, and a `doctor` that explains why there is nothing to
show. Every column comes from files on disk, which is what makes all of them reproducible in CI and in
front of a reviewer.

Four commands, and every flag below is accepted on the bare command *and* on each subcommand:

```sh
fathom [WORKSPACE]              # the timeline TUI (the default)
fathom index [WORKSPACE]        # build or refresh the metric cache  (--status, --rebuild)
fathom report [WORKSPACE]       # the timeline on stdout, no TTY  (--format, --only-missing, --fail-on)
fathom doctor [WORKSPACE]       # why is there nothing to show  (--json)
```

The definitions are not on this page:
**[docs/fathom-metrics.md](docs/fathom-metrics.md)** is the user-facing statement of what each number
means (the MI formula with its constants and a worked example, how a coverage percentage is derived, when
a benchmark delta is withheld, the action-state table), each one cross-linked to the normative
[ADR-0007](docs/adr/0007-maintainability-index.md) and [ADR-0008](docs/adr/0008-per-commit-metric-model.md).
The cache directory's own contract is [`.fathom/LAYOUT.md`](.fathom/LAYOUT.md).

### Build and first run

Fathom needs no server, no docker and no act — not to build and not to run. It needs a git repository, and
for the interesting columns it needs evidence, which is what the tour above produces.

```sh
cargo build -p fathom                       # target/debug/fathom; no tokio, no rmcp, no axum in its graph
cargo run -p fathom -- --help
```

Then point it at the workspace the tour left behind (any repository works; without evidence the coverage,
benchmark and action columns stay unknown, which is a state and not an error):

```sh
cargo run -p fathom -- index /tmp/actmcp-runbook/repo     # build .fathom/index once
cargo run -p fathom -- /tmp/actmcp-runbook/repo          # the TUI
cargo run -p fathom -- /tmp/actmcp-runbook/repo --plain  # the same table on stdout, for a pipe
```

The tour leaves one commit, which is a thin timeline. Two more commits, and a coverage run whose `commit`
is each one, give the three-row timeline every screenshot in this section came from. Any source files will
do; these are the ones used here, and they are the numbers quoted below:

```sh
cd /tmp/actmcp-runbook/repo
mkdir -p src
cat > src/classify.py <<'EOF'
"""A module with two functions and a branch."""


def classify(value):
    if value > 10:
        return "high"
    if value > 0:
        return "mid"
    return "low"


def total(values):
    out = 0
    for value in values:
        out += value
    return out
EOF
git add -A && git commit -qm "add the classifier module"
cat > src/report.py <<'EOF'
"""A module that renders a row."""


def row(name, value):
    if value is None:
        return f"{name}: —"
    return f"{name}: {value}"
EOF
git add -A && git commit -qm "add the row renderer"
cd -

# evidence for each commit in the window, oldest first (ADR-0008 §3 discards the older run if you
# go the other way, which is the right behaviour and the wrong demo)
for c in $(git -C /tmp/actmcp-runbook/repo log --format=%H | tac); do
    scripts/mcp_call.sh run_workflow \
        "{\"workflow\":\"coverage\",\"task\":\"coverage\",\"category\":\"coverage\",\"commit\":\"$c\"}" \
        >/dev/null
done
```

`index` is the only command that writes anything, and what it writes is under `.fathom/index/` (see
[`.fathom/LAYOUT.md`](.fathom/LAYOUT.md) — it is a cache, and `rm -rf` on it costs time and nothing
else). A cold run, then the same command again:

```console
$ cargo run -p fathom -- index /tmp/actmcp-runbook/repo -v
fathom [INFO ] fathom::cli workspace /tmp/actmcp-runbook/repo (git root /tmp/actmcp-runbook/repo)
fathom [INFO ] fathom::store::index indexing 0/3 commits (0 rows written, cache 0/0)
fathom [INFO ] fathom::cli cold manual: 3 commits seen, 3 rows written (3 new), cache 0/8 hits, \
    5 blobs read, 5 analyses, 13418 bytes published in 34 ms

$ cargo run -p fathom -- index /tmp/actmcp-runbook/repo -v
fathom [INFO ] fathom::cli warm manual: 3 commits seen, 0 rows written (0 new), cache 3/3 hits, \
    0 blobs read, 0 analyses, 0 bytes published in 8 ms
```

The cache is keyed per blob, so the second run re-derived nothing: `cache 3/3 hits`, 0 bytes published.
ADR-0006 §4 is why that is possible and `--rebuild` is the flag that ignores it.

Then the timeline headless (`--plain` draws the same table without glyphs or colour, for a pipe):

```console
$ cargo run -p fathom -- /tmp/actmcp-runbook/repo --plain
FATHOM  repo  main@fd92c44  indexed 3 commits | 8 ms | evidence yes
SHA       age message                              cov          +/-       MI            bench action
fd92c44    3m add the row renderer             44.4%=*        +7 -0    65.1^       412.5ms +1   pass
af4920b    3m add the classifier module        44.4%=*       +19 -0     61.3                -   pass
273ae09    4m two workflows to run             44.4%*       +26 -0        -                -   pass
cov* secondary_measures
3 of 3 commits shown | d diff | enter detail | b bench | ? help | q quits
```

Nothing on those five lines is a mystery. `44.4%` is lines only, and the record it comes from says so —
the pooled `total` record for that commit, which is what the `cov` column reads
([docs/evidence-format.md §12](docs/evidence-format.md#12-recipes)):

```console
$ jq -c 'select(.scope == "total") | {lines_found, lines_hit, branches_found, source}' \
      /tmp/actmcp-runbook/repo/.evidence/coverage/*/*.jsonl
{"lines_found":9,"lines_hit":4,"branches_found":2,"source":"lcov"}
```

4 of 9 is `44.4%`, and `branches_found: 2` with no branch measure being read is exactly what earns the
`*`: `secondary_measures` is the caveat that says *the evidence carries more than the first version
reports*. The `-` in the oldest commit's MI and bench cells means *no number could be derived* — never
zero, never a failure: that commit has no parent to diff against, and there is no benchmark evidence in
the tree at all. In the TUI these cells are coloured instead of ASCII-fied, unknown is `—` instead of `-`,
and a flat delta is a dot instead of `=`; the `=`/`^` forms above are what a pipe gets.
[docs/fathom-metrics.md](docs/fathom-metrics.md) is the page for all of it.

### The timeline

One row per commit, newest first. `cov` is the pooled coverage percentage, `+/-` the churn with noise
files counted separately, `MI` the commit's LOC-weighted maintainability index, `bench` one benchmark
group and `+N` for the rest, `action` whether the workflows that ran here passed:

```text
FATHOM  repo  main@fd92c44                                     indexed 3 commits · 8 ms · evidence ✓
┌ commit ──────────────────────────────────────────────────────────────────────────────────────────┐
│  SHA       age message                          cov          +/-       MI            bench action│
│▸ fd92c44    3m add the row renderer         44.4%·*        +7 -0    65.1▲       412.5ms +1      ✓│
│  af4920b    3m add the classifier module    44.4%·*       +19 -0     61.3                —      ✓│
│  273ae09    4m two workflows to run          44.4%*       +26 -0        —                —      ✓│
└──────────────────────────────────────────────────────────────────────────────────────────────────┘
cov* secondary_measures
1/3  d diff   enter detail   b bench   / filter   R re-index   ? help   q quit
```

Three things that glyph line is doing: a `—` is *unknown*, never zero and never a failure; a `*` means
that column has caveats and the line under the table names them; and an arrow (`▲ ▼ ·`) is a comparison
against the commit's **first parent**, which needs both sides — a root commit has no arrow. The glyphs and
the twelve caveat codes behind the `*` are tabulated in
[docs/fathom-metrics.md §6](docs/fathom-metrics.md#6-glyphs-caveats-and-gaps).

### The diff view, the detail view and the bench panel

`d` opens the selected commit's patch — file list on the left, patch on the right, first parent
compared:

```text
DIFF  fd92c44  add the row renderer                            vs parent af4920b · +7 -0 · 1/1 files
┌─ files ────────────────────────┬─ diff ──────────────────────────────────────────────────────────┐
│▸ src/report.py            +7 -0│diff --git a/src/report.py b/src/report.py                       │
│                                │new file mode 100644                                             │
│                                │index 0000000..6532896                                           │
│                                │--- /dev/null                                                    │
│                                │+++ b/src/report.py                                              │
│                                │@@ -0,0 +1,7 @@                                                  │
│                                │+"""A module that renders a row."""                              │
│                                │+                                                                │
│                                │+                                                                │
│                                │+def row(name, value):                                           │
│                                │+    if value is None:                                           │
│                                │+        return f"{name}: —"                                     │
└────────────────────────────────┴─────────────────────────────────────────────────────────────────┘
1  j/k scroll   n/p file   t all files   g/G ends   d/Esc back   ? help   q quit
```

Patches are capped — `--max-diff-bytes` for the commit, `--max-diff-bytes-per-file` for one file — and
when a cap bites the view says so and names the flag, because a silently cut diff reads as a smaller
commit. `t` shows the files the path filters hide along with the totals that go with them.

`enter` opens the commit detail view (per-file coverage, MI hotspots with their `V`/`G`/`LOC` inputs under
`i`, and the diffstat); `b` opens the benchmark panel, one row per metric group with its spread, its
run count and a sparkline that refuses to compare incomparable units; `f` inside it shows the runs
retention excluded and why. `--diff <REV>`, `--detail <REV>` and `--bench <REV>` open the same screens
straight from the command line, and each prints as text when stdout is not a terminal or you pass
`--plain`.

### Keys

`?` shows these on whichever screen is up, with the glyph legend.

| key | screens | what it does |
|---|---|---|
| `j` / `k`, `↑` / `↓` | all | one row down / up (clamped at both ends, never wrapped) |
| `PgDn` / `PgUp` | all | one screenful |
| `Home` / `End`, `g` / `G` | all | first / last row of the list |
| `/` | timeline | filter by sha, author or message; `enter` applies, `Esc` clears |
| `Backspace` | timeline filter | delete one filter character (any printable character appends) |
| `R` | timeline | re-index now; `Esc` cancels a run in flight |
| `enter` | timeline, detail | open the commit detail view; from detail, back to the timeline |
| `d` | all | open this commit's diff; from the diff, back to the timeline |
| `b` | timeline, detail, bench | open the benchmark panel; from the panel, back |
| `n` / `p`, `Tab` / `Shift-Tab` | diff, detail | next / previous file (diff) or pane (detail) |
| `t` | diff, detail | diff: show the files the path filters hide · detail: the MI pane's `V`/`G`/`LOC` inputs |
| `i` | detail | the MI pane's inputs (`V`, `G`, `LOC`) |
| `s` | detail | next sort order for the focused pane |
| `f` | bench | show the runs retention excluded, and why each was |
| `Esc` | all | back out: closes help, cancels a running index, clears the filter, leaves a sub-screen |
| `?` | all | the help overlay for this screen |
| `q`, `Ctrl-C` | all | quit. `Ctrl-C` is a key here, not a signal: the screen is restored and the cache lock released |

### Flags and exit codes

Every flag is `global`, so it may go before or after the subcommand. A flag always wins over its env var,
and a refusal to parse one of them is clap's, never re-implemented.

| flag | env var | default | what it does |
|---|---|---|---|
| `--evidence-dir <PATH>` | `EVIDENCE_DIR` | `<workspace>/.evidence` | read the evidence from here; the same flag and env name act-mcp writes with |
| `--cache-dir <PATH>` | `FATHOM_DIR` | `<workspace>/.fathom` | the metric cache root ([ADR-0006 §3](docs/adr/0006-fathom-workspace-layout.md)): disposable, and never read from `fathom.toml` |
| `--config <PATH>` | `FATHOM_CONFIG` | `fathom.toml`, found root-ward | read this config instead of searching; an unreadable file is a refusal, never a fallback |
| `--from <REV>` | | `history.depth` back | start the window here (a rev, a branch, anything `git rev-parse` takes) |
| `--to <REV>` | | `HEAD` | end the window here |
| `--limit <N>` | | all commits shown | show at most N commits, newest first; a view choice, so nothing is re-indexed |
| `--first-parent` | | `history.first_parent` | walk only first parents; this changes per-commit deltas, so it costs a re-index |
| `--diff <REV>` | | timeline | open this commit's diff instead of the timeline |
| `--detail <REV>` | | timeline | open this commit's detail view instead of the timeline |
| `--bench <REV>` | | timeline | open this commit's benchmark panel instead of the timeline |
| `--max-diff-bytes <BYTES>` | | `1048576` | stop reading a commit's patch after this many bytes |
| `--max-diff-bytes-per-file <BYTES>` | | `131072` | stop reading one file's patch after this many bytes, so one generated file cannot hide the others |
| `--plain` | | a TTY is assumed | print the table instead of drawing a TUI; implied by a non-tty stdout or `TERM=dumb` |
| `--no-color` | `NO_COLOR` | colour on | no colour, unicode glyphs kept — every state is in the glyph, so nothing is lost |
| `-v`, `--verbose` | `RUST_LOG` | warnings and errors | raise the log level on stderr: `-v` info, `-vv` trace |
| `fathom index --status` | | index | print store and configuration state as `key<TAB>value` and write nothing |
| `fathom index --rebuild` | | incremental | delete `<cache root>/index` and build it again from nothing |
| `fathom report --format <FMT>` | | `json` | `json` (the versioned envelope, the stable one), `tsv` (the timeline's own columns, ascii glyphs) or `csv` |
| `fathom report --only-missing` | | every commit | only commits with no evidence at all — the queue an act-mcp sweep works through |
| `fathom report --fail-on <WHAT>` | | gate off | `coverage-drop`, `mi-drop`, `benchmark-regression`, `action-failure`; repeat or comma-separate |
| `fathom doctor --json` | | text | the same checks as one JSON document |

Logs and warnings go to stderr; stdout is data. `fathom report > timeline.tsv`, `fathom index --status` \
and `fathom doctor --json` stay parseable however broken the workspace is, because a refusal is a stderr
thing in this binary.

| code | `fathom tui` / `index` | `fathom report` | `fathom doctor` |
|---|---|---|---|
| `0` | it ran | produced, gate clean or off | every check `ok` |
| `1` | a refusal (typed, on stderr, with a `fix:` line) | `--fail-on` found something | the worst check was `warn` |
| `2` | clap's: bad argv | the report could not be produced | some check was `fail` |
| `3` | recognised argv, a feature a later ticket owns | — | — |

`report` and `doctor` are the two commands written for a CI step, so they follow **0 clean · 1 findings ·
2 could not run** — the same three meanings `act-mcp validate-evidence` has, so one `-run` line in a
pipeline treats both tools alike. A finding is the product there, not a fault: a `--fail-on` gate that
prints findings and exits `0` anyway is worse than no gate, because CI goes green.

### When the timeline is empty

The first thing everyone meets is a timeline with no numbers, and it is nearly always one of three things.
`fathom doctor` is the command that tells you which — it checks the workspace, the evidence root, the
workflows, the cache, the languages and the config, and it exits non-zero when any of them needs
attention:

```sh
cargo run -p fathom -- doctor /tmp/actmcp-runbook/repo
```

A repository that has never been served, indexed or given a workflow is what that looks like in practice
(the `fathom [WARN ]` line is the same first check, printed early for whoever ran the wrong command):

```console
$ cargo run -p fathom -- doctor /tmp/noev
fathom [WARN ] fathom::cli no evidence directory in this workspace: /tmp/noev/.evidence
  fix: run the server once (`act-mcp serve --workspace /tmp/noev`) to write it, or pass --evidence-dir; \
    coverage, benchmarks and action success will read as unknown, not as zero
fathom 0.0.1 doctor · /tmp/noev
ok   workspace  git work tree at /tmp/noev
     · HEAD 3a52e75
     · 1 commit(s) in HEAD's history
     · on `main`
warn evidence   no evidence root at /tmp/noev/.evidence — nothing has run for this workspace
  fix: run a workflow with act-mcp (it owns that directory), or point --evidence-dir / $EVIDENCE_DIR at \
    the tree that has the records
ok   runs       no runs/ directory (advisory: nothing has been run here)
warn workflows  no /tmp/noev/.github/workflows — there is nothing here for act-mcp to run
  fix: add a workflow that publishes coverage or benchmark evidence, e.g. .github/workflows/coverage.yml \
    running `cargo llvm-cov` or `hyperfine`, then run it with act-mcp
warn cache      cache root /tmp/noev/.fathom (writable)
     · no store yet: nothing has been indexed in this cache root
  fix: `fathom index` builds it; every column stays unknown until it does
ok   languages  0 of 1 path(s) in HEAD are in scope for the MI; 2 language(s) enabled (python, rust)
ok   config     no fathom.toml found; every value is this build's default (ADR-0006 §7.1)
4 ok · 3 warn · exit 1
```

Three `warn`s, no `fail`: nothing is broken, nothing has happened. Every line is a check with a state and a
`fix:`, and the same document comes out of `--json` for a machine.

**"everything is empty" — no evidence yet.** `—` in every metric column, and the header says
`not indexed`. Coverage, benchmarks and action success all come from act-mcp: they cannot exist until a
workflow has run for that commit. Run the [tour](#the-five-minute-tour) against your own workspace —
`list_tasks` to name the workflow and the task, then `run_workflow` with that commit — then:

```sh
scripts/mcp_call.sh list_tasks
scripts/mcp_call.sh run_workflow "{\"workflow\":\"coverage\",\"task\":\"coverage\",\
\"category\":\"coverage\",\"commit\":\"$(git rev-parse HEAD)\"}"
cargo run -p fathom -- index /tmp/actmcp-runbook/repo && cargo run -p fathom -- /tmp/actmcp-runbook/repo
```

A row can also be empty because the window never reached it, or because it is older than
`history.depth` (500 commits): `fathom index --from <REV>` widens the walk, and
`fathom report --only-missing` lists the commits in the window that still have no evidence — that is the
queue, and it is meant to be machine-readable.

**Wrong `--evidence-dir`.** Fathom reads `<workspace>/.evidence` unless told otherwise, and act-mcp writes
there unless *it* was told otherwise. Two machines, one run, two evidence roots is the usual story: check
`$EVIDENCE_DIR`, then pass the root explicitly and let `doctor` print what it resolved —
`--evidence-dir /path/to/.evidence`. The same flag name on both binaries is on purpose; the root it
resolves to is printed by `fathom doctor` and by `fathom index --status` (`evidence_root`,
`evidence_state`), and a root that is missing is a `warn`, while a root that is inside the cache root is a
refusal.

**A cache from an older build.** Fathom refuses a store it does not understand rather than guess at it
([ADR-0006 §8](docs/adr/0006-fathom-workspace-layout.md)): a schema or layout version this build does not
know is a refusal with a `fix:` line, not a silent migration. `fathom doctor` says which case you are in
(absent · stale · refused · yours); then `fathom index --rebuild` deletes `<cache root>/index` and builds
the whole thing again, touching nothing else under the cache root. `rm -rf <cache root>/index` is the same
thing with more typing — the cache is regenerable by definition, which is
[what makes it safe to delete](.fathom/LAYOUT.md).

## Troubleshooting

The five failures people actually hit, with the message you will see. Tool errors arrive as an
`isError: true` result whose `structuredContent.error` is `{code, message, retryable, detail}`
— [docs/tool-contract.md §5](docs/tool-contract.md) is the whole vocabulary.

### `docker_unreachable` — "no container runtime answered"

```json
{"error":{"code":"docker_unreachable","retryable":true,
 "message":"no container runtime answered (not_found): no container runtime client: `docker` was not found",
 "detail":{"reason":"not_found"}}}
```

The boot probe runs `docker info`. Start the daemon, or put its client on the server's `PATH`,
or point at a non-default socket:

```sh
docker info                                  # must succeed as the user running act-mcp
act-mcp serve --workspace … --container-daemon-socket /run/user/1000/docker.sock
```

`detail.reason` tells you which half failed: `not_found` (no `docker` client), `timeout` (it
never answered in the probe window), `error` (it answered non-zero — read `detail.detail`).

### act missing, or too old

`serve` refuses to boot rather than failing later:

```console
$ act-mcp serve --workspace /tmp/actmcp-runbook/repo --act-binary /nope/act
act-mcp: act binary not found: `/nope/act`
  fix: install act (https://github.com/nektos/act) or pass --act-binary /absolute/path/to/act
```

A too-old act is the same code with `detail.reason: "too_old"` and both versions named
(`found_version`, `minimum_version`). Note the one gap: an act that does not answer
`--version` is *not* treated as too old — "not knowing is never treated as being too old"
([ADR-0003 §7.7](docs/adr/0003-commit-isolation-and-serialization.md)) — which also means the
e2e stub in this repo cannot reproduce it. Raise the floor with `--minimum-act-version` only
after installing a newer act.

### act has no image for the runner label — `invalid reference format`

The run reaches `status: "error"` (or fails) inside act's own `Set up job` step, and act's log
says something that has nothing to do with runners:

```json
{"msg":"🚀  Start image=","step":"Set up job","level":"info"}
{"msg":"invalid reference format","step":"Set up job","level":"error"}
```

`Start image=` with nothing after it is the whole diagnosis: act resolves a job's container
image from the job's `container:` or from act's runner-label → image map, and a `runs-on`
label that is in neither leaves the image empty, so act asks docker to pull `""`. act's built
map is four entries — `ubuntu-latest`, `ubuntu-22.04`, `ubuntu-20.04`, `ubuntu-18.04` — and has
never contained `ubuntu-24.04`, so a current workflow needs a mapping from somewhere.

When there is no actrc and no `-P` at all, act may stop even earlier and say nothing about
images: it asks its first-run "Please choose the default image you want to use with act"
survey, and act-mcp gives act no terminal to answer on (its stdin is `/dev/null`), so the
question comes back as a fatal error. Same fix.

act-mcp never passes `-P` itself ([ADR-0003 §7.5](docs/adr/0003-commit-isolation-and-serialization.md)
keeps image choice with the operator, not the caller), so the mapping has to be in act's own
config files, read with **act-mcp's** environment, because act inherits it: `$XDG_CONFIG_HOME/act/actrc`,
`$HOME/.config/act/actrc`, `$HOME/.actrc`, `/etc/xdg/act/actrc`. That operator actrc is the
sanctioned extension point ([docs/security.md §5](docs/security.md)), and its `-P` lines *add* to
act's map rather than replacing it — act's `--platform` is an accumulating array flag. One line,
`-P` prefix included:

```text
# ~/.config/act/actrc
-P ubuntu-24.04=catthehacker/ubuntu:act-24.04
```

A benchmark that must be comparable across machines pins a digest there, not a moving tag
([ADR-0003 §8](docs/adr/0003-commit-isolation-and-serialization.md)).

Three ways that file fails to be read, all silent: it is another user's home (the service
account's `$HOME` is not yours), it is not readable by the user running act-mcp, or the line is
malformed — act's config is an *argument list*, so `-P␣ubuntu-24.04=image` must have exactly one
space and a stray second space kills the line. A `.actrc` committed to the repository is never
read either: act's third location is its working directory, and act-mcp runs act in the per-run
scratch directory, which is deliberate (ADR-0003 §7.6) — the repository does not configure the run.

The server now says this before act finds out the hard way. A run whose `runs-on` resolves to no
image carries `warnings: ["act_runs_on_unmapped"]`, and the log names the labels and every path
that was and was not read:

```console
WARN ... no act image maps to these `runs-on` labels: act will name an empty image ...
  job="check" labels="ubuntu-24.04" act_config_read="none" act_config_absent="/home/build/.config/act/actrc, /etc/xdg/act/actrc, /home/build/.actrc"
```

The warning is a warning, never a refusal: labels are inferred from the workflow file, and an
expression, an `include:`, or a reusable workflow can produce a label the file never spells out.
`act --bug-report`, run as the user running act-mcp, is act's own answer to the same question:
it prints each config file it found under `Config files:`, line by line.

### `workspace_busy` — "there is no queue"
```json
{"error":{"code":"workspace_busy","retryable":true,
 "message":"this workspace already has a run in flight (20261110T151121Z-b60b21), and there is no queue: poll it with run_status or cancel it with cancel_run",
 "detail":{"run_id":"20261110T151121Z-b60b21","phase":"running","started_at":"2026-11-10T15:11:21.618Z","holder_pid":709091,"holder_host":"1edcddb1ead5"}}}
```

One run per workspace is the contract ([ADR-0003 §6](docs/adr/0003-commit-isolation-and-serialization.md)),
so this is correct behaviour, not a fault: poll the `detail.run_id`, or
`scripts/mcp_call.sh cancel_run '{"run_id":"…"}'`. The lock is an `flock`, so a server that was
`kill -9`-ed released it when it died; the `.act-mcp.lock` file it leaves behind is not a
stuck state and should not be deleted by hand.

### `commit_unknown` / `commit_ambiguous`

```json
{"error":{"code":"commit_unknown","retryable":false,
 "message":"commit `deadbeef` does not resolve in this repository (not_found)",
 "detail":{"commit":"deadbeef","reason":"not_found"}}}
```

Commits are resolved locally and never fetched. Pass a 7…40 hex prefix (it is normalised to
the full SHA in every answer), or the full SHA, and check `git -C <workspace> cat-file -t <sha>`.
An ambiguous prefix — one that matches more than one object — is `commit_ambiguous` with the
candidates in `detail`; lengthen it.

### "protocol version refused" — `-32022` / `-32020`

```json
{"jsonrpc":"2.0","id":1,"error":{"code":-32022,"message":"Unsupported protocol version",
 "data":{"requested":"2025-06-18","supported":["2025-11-25","2026-07-28"]}}}
```

That is your client offering a revision this server does not speak — anything older than
`2025-11-25`. It is an **answer**, not a hang: switch the client to `2026-07-28` (self-contained
requests) or `2025-11-25` (the `initialize` handshake). A client that *handshakes* with a
revision it cannot name is not refused; it is served `2025-11-25`, which is the handshake
era's own fallback. `-32020` ("Missing `MCP-Protocol-Version` header", or "missing required
`Mcp-Method` header") means the request did not name its revision or its routing method at
all. Fix the client, or use `scripts/mcp_call.sh` / the `curl` above. See
[docs/testing.md §4](docs/testing.md#4-reproducing-by-hand-the-mcp-inspector) for reproducing
any of it by hand, including the MCP Inspector.

### The rest of the usual suspects

| symptom | what it is | what to do |
|---|---|---|
| `403 Forbidden: Host header is not allowed` on every request | DNS-rebinding defence: only `localhost`/`127.0.0.1`/`::1` are accepted | for a deliberate port-forward, `--allow-host gateway.internal:8080`; [docs/security.md §2](docs/security.md) |
| `403 Forbidden: Origin header is not allowed` from a browser-based client | every `Origin` is refused unless named | `--allow-origin 'http://localhost:*'` |
| `Address already in use (os error 98)` at boot | something already has 8080 | `--bind 127.0.0.1:0` takes any free port and logs which |
| `--bind 0.0.0.0:8080 is not a loopback address` | the loopback rule, on purpose | read [docs/security.md §1](docs/security.md) (and the port-forward recipe), then pass `--i-understand-this-is-unauthenticated` |
| `run_status` says `run_not_found` for a run you remember | run state is in-memory and does not survive a restart | the durable trace is `.evidence/runs/<run_id>/meta.json` |
| a run succeeded and `evidence_files` is `[]` | the `category` was neither `benchmark` nor `coverage` (open vocabulary, but only those two extract evidence), or extraction found nothing | read `warnings` (`unknown_category`, `no_coverage_found`, `parse_failed`…) and `meta.json`'s `warnings[]` |
| a benchmark run has no `step_duration_s` records, and `job_duration_s` looks like whole seconds | act only emits step timings (`executionTime`) from **v0.2.76**, and this server accepts act from 0.2.62; the run carries the `act_step_timings_unavailable` warning rather than a silent gap | upgrade act to ≥ 0.2.76, or have the job time its own steps; [docs/benchmark-inputs.md §3](docs/benchmark-inputs.md#3-acts-own-timings-need-act-0276-or-newer) |
| a run fails in `Set up job` with `invalid reference format`, and act's log has `🚀  Start image=` with nothing after it | act has no container image for the job's `runs-on` label — act's own table stops at `ubuntu-22.04` | put `-P <label>=<image>` in an actrc the **server** can read; the run carries the `act_runs_on_unmapped` warning — [act has no image for the runner label](#act-has-no-image-for-the-runner-label--invalid-reference-format) |
| `linker … Bad address (os error 14)` while building | bind-mounted `./target` | `export CARGO_TARGET_DIR=/tmp/tt` |
| `pip` and `python3` disagree and `scripts/validate_evidence.py` cannot import `jsonschema` | dev-box packaging, not this repo | use the built-in checker: `cargo run -p act-mcp -- validate-evidence <path>` |

## Checks

One script runs everything CI runs — locally, in the same order, with the same verdict:

```sh
scripts/ci.sh --help                # steps, flags, exit codes
scripts/ci.sh fmt clippy            # a pre-commit hook's worth of checking
scripts/ci.sh --keep-going          # what CI's check job runs
scripts/install_ci_tools.sh         # cargo-deny + python jsonschema, pinned
```

`fmt`, `clippy -D warnings`, `rustdoc` (broken intra-doc links are errors), `cargo test
--all-features`, schema validation of `schema/*.json` and the evidence tree, `cargo deny`,
and a release build that is smoke-tested by running it.
[docs/ci.md](docs/ci.md) documents each step, the pipeline, the supply-chain policy, and
`scripts/ci_drill.sh`, which proves every one of those checks can actually fail.

The GitHub workflow is `.github/workflows/ci.yml`: a `check` job that calls
`scripts/ci.sh --keep-going …` and a `release` job that builds and smoke-tests the binary
for `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu`. It contains no check logic —
logic lives in `scripts/`, which is also what makes it runnable before you push.

Everything above runs with no docker and no `act`. The one check that needs the real thing is
a manual/nightly step, because a stub can never contradict an assumption about the tool it
stands in for:

```sh
scripts/smoke-real-act.sh --help     # what it checks, and how to run it
scripts/smoke-real-act.sh            # real act + real container runtime
scripts/smoke-real-act.sh --stub     # the same steps against the stubs, in seconds
```

It proves the argv act is given is act's argv, captures `act --json`'s real field names against
the table the extractors assume, checks that a job-written file really crosses the bind mount,
measures a trivial run against ADR-0004's timeout defaults, and reports PASS/FAIL per step.
[docs/testing.md §7](docs/testing.md#7-the-real-act-smoke-manual--nightly) is the when-and-why,
and `.github/workflows/nightly-real-act.yml` runs it nightly where docker exists.

## Layout

Three crates, one lockfile, one CI run ([ADR-0005](docs/adr/0005-fathom-crate-layout-and-dependencies.md)):

| path | what it is |
|---|---|
| `Cargo.toml` | the virtual workspace: members, `default-members`, the shared `[workspace.package]` (version, MSRV, `publish = false`), the one dependency table, the shared `[workspace.lints]` |
| `crates/act-mcp/` | the MCP server binary. Its integration tests live beside it, because Cargo looks for `tests/` next to the package manifest |
| `crates/act-evidence/` | the `.evidence` contract as types: record structs, strict deserializers, the shape predicates, the embedded schemas, the tree validator. No tokio, no transport, no writer — the half a reader is allowed to link |
| `crates/fathom/` | the reader: `fathom` (the timeline, diff, detail and bench screens, `index`, `report`, `doctor`) and the lib that holds everything testable — the git walker, the MI front end, the evidence reader, the store. It never depends on `act_mcp`, and its tree names no tokio, rmcp, axum or HTTP client ([ADR-0005](docs/adr/0005-fathom-crate-layout-and-dependencies.md)); the definitions it renders are [docs/fathom-metrics.md](docs/fathom-metrics.md) |
| `schema/` | the machine-readable contract, still one copy at the root: `evidence`, `run-meta`, `tool-contract`, `mi-definition`, `metric-model` |
| `.fathom/` | this workspace's own Fathom cache — generated, gitignored, and safe to delete. `LAYOUT.md` is the one committed file in it and the on-disk statement of what the directory is ([ADR-0006](docs/adr/0006-fathom-workspace-layout.md)) |
| `scripts/` | the CI scripts, the ADR/schema consistency checkers, and the runbook helpers (`mcp_call.sh`, `sample_evidence.py`, `evidence_recipes.sh`) |

Inside `act-mcp`, which is the part that answers MCP:

| path | what it is |
|---|---|
| `crates/act-mcp/src/server.rs`, `src/tools/` | the rmcp service, the mounted endpoint, one module per tool |
| `crates/act-mcp/src/registry/` | the run supervisor: state machine, run registry, workspace lock (ADR-0004) |
| `crates/act-mcp/src/runner/` | the act process: argv, log tee with redaction, process-group teardown |
| `crates/act-mcp/src/workspace/` | the pre-start validation gate: commit/HEAD gates, isolation policy, probes |
| `crates/act-mcp/src/evidence/` | the writer's half of the contract: the atomic append-only writer, the `validate-evidence` checker, the coverage and benchmark extractors (the types come from `act-evidence` and are re-exported here) |
| `crates/act-mcp/tests/` | one suite per tool, the protocol conformance matrix, `tests/e2e` (the whole pipeline) |
| `crates/act-mcp/tests/realdata/` | the trivial two-workflow fixture, the `act --json` shape table and its captured goldens (when there are any), and the record goldens — see `crates/act-mcp/tests/realdata/fixture/README.md` |

The dependency arrows are one-way and checked by a script, not by convention:
`act-mcp → act-evidence`, `fathom → act-evidence`, and nothing from `fathom` to `act-mcp` in either
direction (`scripts/validate_crate_layout.py`, drilled by `scripts/crate_layout_drill.sh`).

## Further reading

* [docs/evidence-format.md](docs/evidence-format.md) — the `.evidence` contract for a reader, with jq recipes
* [docs/benchmark-inputs.md](docs/benchmark-inputs.md) — where a benchmark number came from, and what act version it needs
* [docs/tool-contract.md](docs/tool-contract.md) — the tool shapes and the error vocabulary
* [docs/adr/0001-tool-contract.md](docs/adr/0001-tool-contract.md) — why `run_workflow` looks like it does
* [docs/adr/0002-evidence-jsonl-contract.md](docs/adr/0002-evidence-jsonl-contract.md) — the `.evidence` contract, normatively
* [docs/adr/0003-commit-isolation-and-serialization.md](docs/adr/0003-commit-isolation-and-serialization.md) — what may be run, and one run at a time
* [docs/adr/0004-run-lifecycle.md](docs/adr/0004-run-lifecycle.md) — runs that outlive the request
* [docs/adr/0005-fathom-crate-layout-and-dependencies.md](docs/adr/0005-fathom-crate-layout-and-dependencies.md) — the Cargo workspace: where fathom lives and what it may depend on
* [docs/adr/0006-fathom-workspace-layout.md](docs/adr/0006-fathom-workspace-layout.md) — the `.fathom` cache root, `fathom.toml` and the metric cache store (also: `.fathom/LAYOUT.md`, the same contract from inside the cache directory)
* [docs/adr/0007-maintainability-index.md](docs/adr/0007-maintainability-index.md) — the Maintainability Index: the formula, what counts as an operator/decision point/logical line, the per-commit aggregate and bands, and the pure-Rust front end (also: `schema/mi-definition.json` for the machine-readable constants, `scripts/mi_reference.py` as the oracle, `crates/act-mcp/tests/testdata/mi/` for the hand-counted golden vectors)
* [docs/adr/0008-per-commit-metric-model.md](docs/adr/0008-per-commit-metric-model.md) — the per-commit metric model: how `.evidence` becomes one row per commit (which records are selected, how a coverage % is pooled, how benchmark groups are deduped, and what "action success" means when the server has forgotten the run) (also: `schema/metric-model.json` for the machine-readable rule set, `scripts/metric_model_reference.py` as the oracle, `crates/act-mcp/tests/testdata/metrics/` for the golden vectors)
* [docs/fathom-metrics.md](docs/fathom-metrics.md) — what every number on the Fathom screen means: the MI formula and its worked example, how coverage % is derived, when a benchmark delta is withheld, the action-success table — the user-facing restatement of ADR-0007 and ADR-0008, cross-linked to both
* [docs/testing.md](docs/testing.md) — the suites, and how to read a red one
* [docs/ci.md](docs/ci.md) — CI, in full
* [docs/security.md](docs/security.md) — loopback-only, and what that does and does not protect
