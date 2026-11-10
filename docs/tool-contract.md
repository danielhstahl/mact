# act-mcp tool contract (normative reference)

This is the implementer- and test-facing half of
[ADR-0001](adr/0001-tool-contract.md). The ADR holds the reasoning; this file holds the shapes you
code against. When the two disagree, the ADR wins and the checker is wrong — but they cannot disagree:
`python3 scripts/validate_tool_contract.py` fails if the error-code table, the status vocabulary or the
field sets drift apart between the ADR, this file and
[`schema/tool-contract.schema.json`](../schema/tool-contract.schema.json).

- Protocol: MCP **2026-07-28**, Streamable HTTP, one endpoint, loopback, no auth.
- Schemas: [`schema/tool-contract.schema.json`](../schema/tool-contract.schema.json) — every `*_args`,
  `*_result`, `run_view`, `tool_error` and the `x-error-codes` registry live there.
- Fixtures (the e2e oracle): [`tests/e2e/testdata/tool-io`](../tests/e2e/testdata/tool-io).
- Related decisions: [ADR-0002](adr/0002-evidence-jsonl-contract.md) (the on-disk contract, already
  accepted), ADR-0003 (commit isolation), ADR-0004 (run lifecycle).

## 1. Endpoint and transport

| item | value |
|---|---|
| URL | `http://127.0.0.1:7788/mcp` (`--listen`, default port 7788) |
| methods | `POST` only in v1 (`tools/list`, `tools/call`) |
| `MCP-Protocol-Version` | required on every request, must be `2026-07-28`; a request without it is rejected |
| other protocol versions | rejected at negotiation by rmcp; there is no downgrade path |
| `Accept` | `application/json` (server runs with `json_response = true`; no SSE streams) |
| `Host` / `Origin` | `Host` must be `127.0.0.1:<port>` or `localhost:<port>`; `Origin` denied unless added via `--allow-origin` |
| body limit | 1 MiB |
| auth | none; the listener refuses to bind anywhere but loopback |

```console
$ curl -sS http://127.0.0.1:7788/mcp \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json' \
    -H 'MCP-Protocol-Version: 2026-07-28' \
    -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
          "name":"run_workflow",
          "arguments":{"workflow":"coverage","task":"coverage","category":"coverage","commit":"2f7c1a4"}}}'
```

Every result carries `structuredContent` (the JSON objects below) *and* a one-line human-readable `content`
text block. The text block of an error result starts with `<tool>: <code>: ` so the code survives a client
that only reads text.

## 2. Tools

| tool | what it does | args | result |
|---|---|---|---|
| `run_workflow` | validate, then run one job of one workflow against one commit; by default waits for the terminal result | `run_workflow_args` | `run_workflow_result` |
| `list_tasks` | enumerate workflows and job ids in the workspace, with advisory `category_hint`s | `list_tasks_args` | `list_tasks_result` |
| `run_status` | one run by id, or the recent/active listing | `run_status_args` | `run_status_result` |
| `cancel_run` | cancel a run; idempotent | `cancel_run_args` | `cancel_run_result` |

All four use `additionalProperties: false`: an unexpected argument is the error `unknown_argument`, and a
caller-supplied `run_id` on `run_workflow` is that error, not a request.

## 3. `run_workflow`

### 3.1 Arguments

```json
{
  "workflow": "coverage",
  "task": "coverage",
  "category": "coverage",
  "commit": "2f7c1a4b9e0d3c6a5b8f1e2d4c6a8b0f3e5d7c91",
  "wait": true,
  "timeout_secs": 900,
  "log_tail_bytes": 4096,
  "inputs": {"PROFILE": "release"},
  "secrets": {"NPM_TOKEN": "…"},
  "idempotency_key": "sweep-2026-11-10:2f7c1a4:coverage"
}
```

| argument | type | required | notes |
|---|---|---|---|
| `workflow` | string | **yes** | `.github/workflows/coverage.yml`, `coverage.yml` or `coverage`; `..` not expressible; case-sensitive |
| `task` | string | **yes** | job id → `act -j <task>` |
| `category` | string | **yes** | open vocabulary, `^[a-z][a-z0-9_]*$`; known: `benchmark`, `coverage`; unknown runs and produces no evidence |
| `commit` | string | **yes** | 7..=40 hex, any case, lowercased; must resolve locally (no fetch) |
| `wait` | bool | no (true) | false ⇒ return immediately with `status: "running"` |
| `timeout_secs` | int 1..=21600 | no | run budget **and** max wait; defaults coverage 900 / benchmark 1800 / other 1800 |
| `log_tail_bytes` | int 0..=65536 | no (4096) | cap on `log_tail` |
| `inputs` | map | no | ≤ 32, key `^[A-Za-z_][A-Za-z0-9_]{0,127}$`, value ≤ 4 KiB → `act --input` (sorted keys) |
| `secrets` | map | no | ≤ 32, value ≤ 8 KiB → `act --secret-file`, a `0600` per-run file; never argv, never logged |
| `idempotency_key` | string 1..=64 | no | `^[A-Za-z0-9._:-]+$`; replay returns the remembered run |

No other argument exists. There is no `run_id`, no path, no `argv`, no `ref`/`pr`, no `dry_run`.

Validation order (short-circuits, and the error you get depends on it):
argument shape → `idempotency_key` replay → workspace gates → commit resolution → workflow/task resolution →
environment probes → isolation → capacity/lock → register the run and start act.

### 3.2 Result (`run_workflow_result` = `run_view` + `deduplicated`)

```json
{
  "run_id": "20261110T011507Z-d4e5f6",
  "status": "success",
  "terminal": true,
  "deduplicated": false,
  "phase": "done",
  "workflow": "coverage",
  "workflow_file": ".github/workflows/coverage.yml",
  "task": "coverage",
  "category": "coverage",
  "commit": "2f7c1a4b9e0d3c6a5b8f1e2d4c6a8b0f3e5d7c91",
  "started_at": "2026-11-10T01:15:07.004Z",
  "finished_at": "2026-11-10T01:16:08.237Z",
  "duration_ms": 61233,
  "exit_code": 0,
  "evidence_files": [".evidence/coverage/2f7c1a4b9e0d3c6a5b8f1e2d4c6a8b0f3e5d7c91/20261110T011507Z-d4e5f6.jsonl"],
  "evidence_record_count": 5,
  "log_tail": "exit status 0\n",
  "warnings": []
}
```

| field | type | present | notes |
|---|---|---|---|
| `run_id` | `^[0-9]{8}T[0-9]{6}Z-[0-9a-f]{6}$` | always | server-generated, sortable |
| `status` | `pending \| running \| success \| failed \| cancelled \| timed_out \| error` | always | the five terminal values are the same words `.evidence` uses |
| `terminal` | bool | always | must agree with `status` (schema-enforced) |
| `deduplicated` | bool | `run_workflow` only | this run was remembered via `idempotency_key` |
| `phase` | open enum | usually | `queued`, `preparing_tree`, `starting_act`, `running`, `extracting_evidence`, `publishing_evidence`, `cancelling`, `done`; display-only |
| `workflow` / `workflow_file` / `task` / `category` | string / relpath / string / string | always | resolved identity; `workflow` is the basename without extension |
| `commit` | `^[0-9a-f]{40}$` | always | the commit actually executed, never the abbreviation |
| `started_at` / `finished_at` | UTC `Z` instant | started / terminal | `finished_at` present exactly when terminal |
| `duration_ms` | int ≥ 0 | when known | total when terminal, elapsed while running |
| `exit_code` | int 0..=255 | when act had one | absent for `pending`/`running` |
| `evidence_files` | relpath[] | always | JSONL published this run, relative to the evidence root; `[]` is normal |
| `evidence_record_count` | int ≥ 0 | always | records across those files |
| `log_tail` | string | always | redacted; `<WORKSPACE>` / `<EVIDENCE>` substituted for absolute roots; ≤ `log_tail_bytes` |
| `warnings` | string[] | always | open vocabulary, §7 |
| `error` | object | only `status: "error"` | `{code, message, retryable, detail}`; forbidden on every other status |

Invariants enforced by the schema, not by convention: terminal ⇒ `finished_at` + `terminal: true`;
non-terminal ⇒ no `finished_at`, no `exit_code`, no `error`, `terminal: false`; `error` present iff
`status == "error"`; no `null` anywhere; no unknown fields.

### 3.3 A run that failed is not an error result

`isError: false` with `status: "failed"`:

```json
{"content": [{"type": "text", "text": "run_workflow failed 20261110T004203Z-a1b2c3 benchmark/hyperfine-bench at 2f7c1a4b: exit 1"}],
 "structuredContent": {"run_id": "20261110T004203Z-a1b2c3", "status": "failed", "terminal": true,
   "deduplicated": false, "workflow": "benchmark",
   "workflow_file": ".github/workflows/benchmark.yml", "task": "hyperfine-bench",
   "category": "benchmark", "commit": "2f7c1a4b9e0d3c6a5b8f1e2d4c6a8b0f3e5d7c91",
   "started_at": "2026-11-10T00:42:03.417Z", "finished_at": "2026-11-10T00:43:37.539Z",
   "duration_ms": 94122, "exit_code": 1,
   "evidence_files": [".evidence/benchmark/2f7c1a4b9e0d3c6a5b8f1e2d4c6a8b0f3e5d7c91/20261110T004203Z-a1b2c3.jsonl"],
   "evidence_record_count": 4, "log_tail": "##[error]Process completed with exit code 1.\n",
   "warnings": ["exit_nonzero"]},
 "isError": false}
```

`isError: true` ⇔ no run was created or controlled. A terminal run of any status is a normal result.

## 4. `list_tasks`

```json
{"workflow": "coverage", "include_steps": true}          // both optional; no workflow ⇒ list all
```

```json
{
  "workflows_dir": ".github/workflows",
  "workflow_count": 2,
  "workflows": [
    {"name": "benchmark", "file": ".github/workflows/benchmark.yml", "display": "Benchmark",
     "job_count": 2,
     "jobs": [{"id": "build", "category_hint": "other", "runs_on": ["ubuntu-24.04"]},
              {"id": "hyperfine-bench", "name": "Run hyperfine benchmarks", "needs": ["build"],
               "runs_on": ["ubuntu-24.04"], "matrix_axes": ["profile"],
               "uses_composite": true, "category_hint": "benchmark"}]}
  ],
  "warnings": []
}
```

- `category_hint` ∈ `benchmark | coverage | other` is **advisory**; you still pass `category` explicitly.
- A file that will not parse is returned with
  `"parse_error": {"code": "invalid_yaml", "message": "…line 7 column 3"}` and the call still succeeds. That
  code namespace (`invalid_yaml | not_a_map | jobs_missing | unsupported_yaml`) is separate from the tool
  error codes on purpose.
- Takes no workspace lock: safe while a benchmark is mid-flight. Errors: `invalid_arguments`,
  `unknown_argument`, `workspace_not_found`, `workflows_dir_missing`.

## 5. `run_status`

`{"run_id": "20261110T011507Z-d4e5f6", "log_tail_bytes": 2048}` → `{"run": <run_view>, "active_count": 1,
"truncated": false}`; `{}` or `{"limit": 20}` → `{"runs": [run_summary…], "active_count": 1,
"truncated": true}`. Exactly one of `run` / `runs`. `run_summary` is
`{run_id, status, terminal, phase?, workflow, task, category, commit, started_at?, duration_ms?}` — newest
first, capped at 50 (`limit` ≤ 50), and it carries no log or evidence detail on purpose.

Unknown or evicted id ⇒ error `run_not_found` (`isError: true`); the durable trace is
`.evidence/runs/<run_id>/`, which `run_status` does not read. Never blocks, never starts anything.

## 6. `cancel_run`

`{"run_id": "20261110T011507Z-d4e5f6"}` →

```json
{"run_id": "20261110T011507Z-d4e5f6", "status": "cancelled", "terminal": true,
 "already_terminal": false, "cancel_requested_at": "2026-11-10T01:15:31.900Z",
 "kill": {"method": "term", "took_ms": 812}, "warnings": ["partial_evidence_dropped"]}
```

Already-terminal ⇒ `already_terminal: true`, the old `status`, no `cancel_requested_at`, no `kill`, and it is
**not** an error. Queued run (cancelled before `act` was exec'd) ⇒ `kill.method: "none"`. Mid-kill (second
caller) ⇒ `status: "cancelled"`, `terminal: false`, no `kill` — the same window reads as `status:
"running"` + `phase: "cancelling"` through `run_status`, which cannot express a non-terminal `cancelled`
(ADR-0004 §5.2, §16.1). A cancel arriving after `act` exited is a no-op: current status, warning
`already_terminal`. Only error: `run_not_found`. The kill mechanics (process group, SIGTERM → 20 s grace →
SIGKILL, container cleanup) and the partial-evidence policy are ADR-0004 §5–§6.

## 7. Errors

`isError: true` results look like this — the code appears twice, deliberately:

```json
{"content": [{"type": "text",
              "text": "run_workflow: workflow_not_found: no workflow named 'bench' in .github/workflows"}],
 "structuredContent": {"error": {
     "code": "workflow_not_found",
     "message": "no workflow named 'bench' in .github/workflows",
     "retryable": false,
     "detail": {"workflow": "bench", "workflows_dir": ".github/workflows",
                "available": ["benchmark", "coverage"]}}},
 "isError": true}
```

JSON-RPC `error` objects are reserved for protocol faults and produced by rmcp, never for tool semantics:
`-32700` malformed JSON, `-32600` not a JSON-RPC request, `-32601` unknown method, `-32602` bad `params`
envelope or unknown tool name (not bad tool arguments), `-32603` rmcp-internal. A tool failure is always a
result with `isError: true`.

### Error codes

Closed set; both contract docs carry this table verbatim and the checker compares all four copies (here, the
ADR, `$defs/error_code`, `x-error-codes`), `isError` and `retryable` columns included.

| code | isError | retryable | group | meaning and `detail` keys |
|---|---|---|---|---|
| `invalid_arguments` | true | false | arguments | A required argument is missing, has the wrong JSON type, or violates a bound or pattern. `detail.errors: [{path, reason, expected}]` |
| `unknown_argument` | true | false | arguments | An argument the tool does not define, **including a caller-supplied `run_id`**. `detail.unexpected`, `detail.expected` |
| `workspace_not_found` | true | false | precondition | Workspace directory missing, not a directory, or unreadable. `detail.path`, `detail.reason` |
| `not_a_git_repository` | true | false | precondition | Workspace is not inside a git work tree, so no commit can resolve. `detail.path` |
| `workflows_dir_missing` | true | false | precondition | `.github/workflows` absent, not a directory, or holds no `.yml`/`.yaml` — "this is not an Actions repo". `detail.path` |
| `act_missing` | true | true | precondition | act not on PATH, not executable, or older than the minimum supported version (probe cached 30 s). `detail.probed`, `detail.reason`, `detail.found_version`, `detail.minimum_version` |
| `docker_unreachable` | true | true | precondition | No container runtime answered (`docker info` failed or timed out). `detail.reason`, `detail.detail` |
| `workflow_not_found` | true | false | resolution | No workflow file matches `workflow`. `detail.workflow`, `detail.workflows_dir`, `detail.available` |
| `ambiguous_workflow` | true | false | resolution | Two or more files match one bare name; pass the file name. `detail.workflow`, `detail.candidates` |
| `task_not_found` | true | false | resolution | That job id is not declared in the resolved workflow. `detail.workflow_file`, `detail.task`, `detail.jobs` |
| `commit_unknown` | true | false | resolution | `git rev-parse --verify <sha>^{commit}` failed; never fetched. `detail.commit`, `detail.reason` |
| `commit_ambiguous` | true | false | resolution | Abbreviation matches more than one object; pass a longer prefix. `detail.commit`, `detail.candidates` |
| `isolation_failed` | true | true | precondition | ADR-0003 could not produce a tree for that commit (head-only policy, or worktree create/cleanup failed); nothing was started. `detail.mode`, `detail.requested`, `detail.head`, `detail.reason` |
| `workspace_busy` | true | true | precondition | The workspace single-flight lock is held; there is no queue. `detail.run_id`, `detail.phase`, `detail.started_at` |
| `too_many_runs` | true | true | precondition | Server-wide `--max-concurrent-runs` (default 2) reached across workspaces. `detail.limit`, `detail.active` |
| `idempotency_conflict` | true | false | precondition | That `idempotency_key` is remembered for a different tuple. `detail.idempotency_key`, `detail.run_id`, `detail.previous`, `detail.requested` |
| `run_not_found` | true | false | control | No such run in this process's registry; durable trace is `.evidence/runs/<run_id>/`. `detail.run_id`, `detail.known_runs` |
| `internal_error` | true | true | call | Unpredicted server failure before any run was created; nothing was written. `detail.panic` |
| `act_spawn_failed` | false | true | runner | act could not be exec'd (fork/exec, cwd, argv assembly). Terminal `status: error`. `detail.os_error`, `detail.argv` |
| `act_crashed` | false | true | runner | act died by signal with no exit status, not a timeout and not a cancel. Terminal `status: error`. `detail.signal`, `detail.exit_code` |
| `runner_internal_error` | false | true | runner | Supervisor failure after act started (state inconsistency, worktree vanished, log pipe died). Terminal `status: error`. `detail.stage`, `detail.reason` |
| `evidence_write_failed` | false | false | runner | act finished but the evidence could not be written (EACCES, ENOSPC, read-only root). Fails the run on purpose. `detail.path`, `detail.os_error` |
| `evidence_path_exists` | false | false | runner | The evidence file to publish already exists — a `run_id` collision, ADR-0002 §6.2. `detail.path`, `detail.run_id` |
| `evidence_path_escape` | false | false | runner | A payload-derived path resolved outside the evidence root or through a symlink; write refused, ADR-0002 §6.3. `detail.path`, `detail.root` |

The `isError: false` rows live in `run_view.error` on a `status: "error"` run; the rest live in
`tool_error.error`.

### Warnings

Open vocabulary in `warnings[]` (and in `.evidence/runs/<run_id>/meta.json`); a warning never changes
`status`: `unknown_category`, `deduplicated`, `exit_nonzero`, `process_group_killed`,
`partial_evidence_dropped`, `already_terminal`, `orphaned_containers`, `parse_error`, plus the
ADR-0002 §5.1 set (`no_coverage_found`, `parse_failed`, `unsupported_format`, `payload_too_large`,
`payload_unreadable`, `payload_transport_unavailable`, `coverage_source_shadowed`, `malformed_metric`,
`marker_cap_reached`, `truncated`). Unknown warnings are ignored, never fatal.

## 8. Limits

| thing | cap |
|---|---|
| request body | 1 MiB |
| `workflow` / `task` | 255 bytes |
| `category` | 64 bytes |
| `commit` | 7..=40 hex |
| `idempotency_key` | 64 bytes |
| `timeout_secs` | 21600; defaults coverage 900 / benchmark 1800 / other 1800 |
| concurrent act runs | 2 per server, 1 per workspace, no queue |
| `inputs` / `secrets` | 32 entries; values 4 KiB / 8 KiB |
| `log_tail_bytes` | 65536 (default 4096) |
| `evidence_files` / `warnings` | 8 / 64 unique |
| `run_status` listing | 50 newest first |
| `error.message` / `error.detail` | 1024 B / 4096 B |
| `list_tasks` workflows / jobs / steps | 1024 / 1024 / 256 |

## 9. Changing this contract

Additive (no coordination): a new tool, an optional argument with a default, an optional result field, a new
`category`, `warning`, `parse_error` or `error_code` member, a wider cap. Breaking (say so, in the changelog
and the tool description): rename or removal of a tool, argument or field; a type or meaning change; optional
→ required; closing an open vocabulary; redefining a warning; changing the `isError` rule; flipping
`wait`'s default.

Any change lands in one commit that touches: this file,
[`docs/adr/0001-tool-contract.md`](adr/0001-tool-contract.md),
[`schema/tool-contract.schema.json`](../schema/tool-contract.schema.json), and the fixtures (regenerate with
`python3 scripts/gen_tool_contract_fixtures.py` after editing that script). Then:

```console
$ pip install jsonschema
$ python3 scripts/validate_tool_contract.py --stats
$ python3 scripts/validate_evidence.py tests/e2e/testdata/expected   # the sibling contract, unchanged by this
```
