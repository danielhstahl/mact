# act-mcp

An HTTP [MCP](https://modelcontextprotocol.io) server — protocol revision **2026-07-28**,
Streamable HTTP, bound to loopback, **no authentication** — that runs GitHub Actions
workflows with [nektos/act](https://github.com/nektos/act) and records what they produced as
`.evidence` JSONL.

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
| MCP revision | **`2026-07-28`**, and only that one — there is no `initialize` handshake and no downgrade |
| `rmcp` | `3.9.0` (the SDK that implements the negotiation) |
| `rustc` | floor **1.88** (`Cargo.toml` `rust-version`); the pinned toolchain is **1.99.1** (`rust-toolchain.toml`, honoured by rustup in this directory) |
| act | **≥ 0.2.62**, which is what `--minimum-act-version` refuses to go below; the checked-in fixtures were captured against **0.2.68** |
| container runtime | any that answers `docker info` |
| `jq` (for the doc recipes only) | 1.7.1 |

## Contents

[Prerequisites](#prerequisites) · [Build](#build) · [Run](#run) ·
[Every flag and env var](#every-flag-and-env-var) · [Pointing a client at it](#pointing-a-client-at-it) ·
[The five-minute tour](#the-five-minute-tour) · [What `.evidence` looks like](#what-evidence-looks-like) ·
[Troubleshooting](#troubleshooting) · [Checks](#checks) · [Layout](#layout) ·
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
`act` the tests run is a shell script (`tests/e2e/testdata/bin/act`) and the container daemon is
probed through an injectable seam — see [docs/testing.md](docs/testing.md).

## Build

```sh
cargo build                # debug binary: target/debug/act-mcp
cargo build --release      # release binary: target/release/act-mcp
```

> On a bind-mounted checkout (the case on this repo's dev box) the linker can take SIGBUS or
> fail with `Bad address (os error 14)` while writing into `./target`. The workaround is one
> environment variable, for build and test alike: `export CARGO_TARGET_DIR=/tmp/tt`.

## Run

```sh
cargo run --release -- serve --workspace /path/to/a/git/repo/with/github/workflows
```

That is the whole command. A real startup, from the tour below, looks like this:

```console
$ cargo run --release -- serve --workspace /tmp/actmcp-runbook/repo \
      --act-binary ./tests/e2e/testdata/bin/act
2026-11-10T15:09:12.606305Z  INFO act_mcp::server: bind is loopback-only: only this machine
  can reach the endpoint (there is no authentication) bind=127.0.0.1:8080
2026-11-10T15:09:12.606542Z  INFO act_mcp::server: act-mcp listening url=http://127.0.0.1:8080/mcp
  bind=127.0.0.1:8080 workspace=/tmp/actmcp-runbook/repo
  evidence_dir=/tmp/actmcp-runbook/repo/.evidence
  act_binary=/workspace/tests/e2e/testdata/bin/act protocol_version=2026-07-28
  allowed_hosts=["localhost", "127.0.0.1", "::1"] allowed_origins=[]
  max_request_body_bytes=1048576 shared_secret_required=false
```

The listening line is the whole configuration in one place: what it resolved `act` to, which
protocol revision it will accept, which `Host` values it takes, and whether a session token is
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
cargo run --release -- validate-evidence <path-or-workspace> [--stats]
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
| *(no flag)* | `RUST_LOG` | `info,rmcp=warn` | the `tracing` env filter |

Flags are also `-h`/`--help` (per-subcommand) and `-V`/`--version`. `validate-evidence` takes
`--evidence-dir`, `--schema`, `--meta-schema` and `--stats`.

## Pointing a client at it

The endpoint is **`http://127.0.0.1:8080/mcp`**, Streamable HTTP, `POST` only. There is no
`initialize` handshake and no session id: at the 2026-07-28 revision each request is
self-contained and must carry

* `MCP-Protocol-Version: 2026-07-28` **and** the matching `params._meta` block,
* the SEP-2243 routing headers `Mcp-Method` and `Mcp-Name`,
* `Accept` naming **both** `application/json` and `text/event-stream`,
* `Content-Type: application/json`.

Anything else is a refusal that names what was missing — never a hang. The full request-shape
table is [docs/tool-contract.md §1](docs/tool-contract.md#1-endpoint-and-transport).

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
> decides whether the connection works is whether your client implements **2026-07-28
> statelessly**. A client that insists on an `initialize` handshake, or that offers only
> `2025-11-25` and earlier, is refused with JSON-RPC `-32022` *as an answer* naming
> `["2026-07-28"]` as the supported set ([docs/testing.md §3](docs/testing.md)). That is the
> pin, not a bug. `timeout`/`headers` keys differ per client; consult the client's own docs
> for anything beyond the URL, and expect to add `--allow-origin` for anything that runs in a
> browser ([docs/security.md §2](docs/security.md)).

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

## The five-minute tour

Run this once to see the whole contract with no docker and no act installed — the server is
started against the e2e stub `act` (`tests/e2e/testdata/bin/act`) and its stub `docker`. If
you have the real thing, drop the `--act-binary` flag and the two `PATH=`/`STUB_PAYLOAD_DIR=`
assignments; **every other command in this section is identical either way.**

Set up a throwaway workspace (one terminal):

```sh
DEMO=/tmp/actmcp-runbook
rm -rf "$DEMO" && mkdir -p "$DEMO/repo/.github/workflows"
cp tests/e2e/testdata/workflows/coverage.yml tests/e2e/testdata/workflows/benchmark.yml \
   "$DEMO/repo/.github/workflows/"
git -C "$DEMO/repo" init -q -b main
git -C "$DEMO/repo" config user.name act-mcp && git -C "$DEMO/repo" config user.email act-mcp@example.invalid
git -C "$DEMO/repo" add -A && git -C "$DEMO/repo" commit -qm "two workflows to run"
```

Start the server (terminal 1):

```sh
PATH="$PWD/tests/e2e/testdata/bin:$PATH" STUB_PAYLOAD_DIR="$PWD/tests/e2e/testdata/payload" \
  cargo run --release -- serve --workspace "$DEMO/repo" \
    --act-binary "$PWD/tests/e2e/testdata/bin/act"
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
cargo run --release -- validate-evidence /tmp/actmcp-runbook/repo/.evidence --stats
```

```
.evidence/README.md
.evidence/benchmark/cf0ecd0eee6a9c1b014b61867fb8646a624af95b/20261110T150920Z-5f0ad2.jsonl
.evidence/coverage/cf0ecd0eee6a9c1b014b61867fb8646a624af95b/20261110T150920Z-d323b9.jsonl
.evidence/incoming/20261110T150920Z-5f0ad2/benchmark.json
.evidence/incoming/20261110T150920Z-5f0ad2/coverage-summary.json
.evidence/incoming/20261110T150920Z-5f0ad2/lcov.info
.evidence/incoming/20261110T150920Z-d323b9/benchmark.json
.evidence/incoming/20261110T150920Z-d323b9/coverage-summary.json
.evidence/incoming/20261110T150920Z-d323b9/lcov.info
.evidence/runs/20261110T150920Z-5f0ad2/act.jsonl
.evidence/runs/20261110T150920Z-5f0ad2/meta.json
.evidence/runs/20261110T150920Z-d323b9/act.jsonl
.evidence/runs/20261110T150920Z-d323b9/meta.json
# act-mcp evidence (local only)
.evidence/
.act-mcp.lock
benchmark	cf0ecd0eee6a9c1b014b61867fb8646a624af95b	3
coverage	cf0ecd0eee6a9c1b014b61867fb8646a624af95b	4
OK 4 file(s), 7 record(s) valid
```

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
 "data":{"requested":"2025-11-25","supported":["2026-07-28"]}}}
```

That is your client offering an older revision; it is an **answer**, not a hang, and there is
no downgrade path. `-32020` ("Missing `MCP-Protocol-Version` header", or "missing required
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
| `pip` and `python3` disagree and `scripts/validate_evidence.py` cannot import `jsonschema` | dev-box packaging, not this repo | use the built-in checker: `cargo run -- validate-evidence <path>` |

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

| path | what it is |
|---|---|
| `src/server.rs`, `src/tools/` | the rmcp service, the mounted endpoint, one module per tool |
| `src/registry/` | the run supervisor: state machine, run registry, workspace lock (ADR-0004) |
| `src/runner/` | the act process: argv, log tee with redaction, process-group teardown |
| `src/workspace/` | the pre-start validation gate: commit/HEAD gates, isolation policy, probes |
| `src/evidence/` | the `.evidence` JSONL contract as code: records, writer, extractors, the checker |
| `schema/` | the machine-readable contract: `evidence`, `run-meta`, `tool-contract` |
| `tests/` | one suite per tool, the protocol conformance matrix, `tests/e2e` (the whole pipeline) |
| `tests/realdata/` | the trivial two-workflow fixture, the `act --json` shape table and its captured goldens (when there are any), and the record goldens — see `tests/realdata/fixture/README.md` |
| `scripts/` | the CI scripts, the ADR/schema consistency checkers, and the runbook helpers (`mcp_call.sh`, `sample_evidence.py`, `evidence_recipes.sh`) |

## Further reading

* [docs/evidence-format.md](docs/evidence-format.md) — the `.evidence` contract for a reader, with jq recipes
* [docs/benchmark-inputs.md](docs/benchmark-inputs.md) — where a benchmark number came from, and what act version it needs
* [docs/tool-contract.md](docs/tool-contract.md) — the tool shapes and the error vocabulary
* [docs/adr/0001-tool-contract.md](docs/adr/0001-tool-contract.md) — why `run_workflow` looks like it does
* [docs/adr/0002-evidence-jsonl-contract.md](docs/adr/0002-evidence-jsonl-contract.md) — the `.evidence` contract, normatively
* [docs/adr/0003-commit-isolation-and-serialization.md](docs/adr/0003-commit-isolation-and-serialization.md) — what may be run, and one run at a time
* [docs/adr/0004-run-lifecycle.md](docs/adr/0004-run-lifecycle.md) — runs that outlive the request
* [docs/testing.md](docs/testing.md) — the suites, and how to read a red one
* [docs/ci.md](docs/ci.md) — CI, in full
* [docs/security.md](docs/security.md) — loopback-only, and what that does and does not protect
