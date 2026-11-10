# Test fixtures

Test data for the `run_workflow` pipeline. Everything here is input to a test and is
checked in so a failure is reproducible without docker, network, or a registry pull.

| dir | contents |
|---|---|
| `bin/` | `act` — the stand-in for the real binary (see its header comment) and `docker` — a container-CLI that only answers the startup probe |
| `workflows/` | fixture `.github/workflows` files for the e2e scenarios, including one deliberately broken |
| `payload/` | what a job writes through the bind mount: `lcov.info`, `coverage-summary.json`, `benchmark.json` |
| `logs/` | captured `act --json` output (success + failure) used for timing and redaction checks |
| `tool-io/` | the ADR-0001 contract fixtures — hand-written tool arguments and `structuredContent` payloads, all of them indexed in [§ `tool-io/`](#tool-io--the-adr-0001-contract-fixtures) below |
| `scenarios/` | the golden evidence trees, one directory per e2e scenario — written by `E2E_UPDATE_GOLDEN=1 cargo test --test e2e`, never by hand |

The payloads are small on purpose — a few files, a few samples — but carry the real shape
(lcov `SF:`/`DA:`/`BRDA:`, Istanbul's `total` + per-file keys, a benchmark file with
samples and labels). Anything a parser accepts must be here in the shape the tool actually
emits, and the e2e goldens are the record of what the pipeline did with each.

## `tool-io/` — the ADR-0001 contract fixtures

These are the readable statement of [ADR-0001](../../../docs/adr/0001-tool-contract.md):
one file per interesting tool call, written *from* the ADR and never emitted by the
implementation, so that the implementation is measured against them and not the other way
round. [`scripts/gen_tool_contract_fixtures.py`](../../../scripts/gen_tool_contract_fixtures.py)
is their single source — it is idempotent, so a hand edit is a diff away from being
overwritten — and
[`scripts/validate_tool_contract.py`](../../../scripts/validate_tool_contract.py)
(CI's `schema` step) validates every file against the `$defs` entry its name selects in
[`schema/tool-contract.schema.json`](../../../schema/tool-contract.schema.json).

Filename grammar, which the checker parses: `<tool-or-error>.<args|result>.<scenario>.json`
where the first token is `run_workflow`, `list_tasks`, `run_status`, `cancel_run` or `error`
(`error` selects the shared `tool_error` payload shape — the one every `isError: true`
result carries, whichever tool produced it). `valid/` must be accepted by that `$defs`
entry; `invalid/` must be rejected by it, and each invalid fixture is meant to violate
exactly one rule so the rejection names the rule you meant to test. The checker also fails
on **any fixture that this file does not name**, which is what the two tables below are for:
an undocumented fixture is a contract rule nobody can look up.

Two shared shapes worth knowing before reading the tables. A result never uses JSON
`null` — an unknown value is expressed by the key being absent — and no path in any result
is absolute: paths are workspace-relative or evidence-root-relative, because a host path in
a tool result is an information leak (see [docs/security.md](../../../docs/security.md)).

### `valid/` — 22 payloads the schema accepts

| fixture | what it is | what it pins |
|---|---|---|
| `run_workflow.args.minimal.json` | the four required arguments and nothing else | `workflow`, `task`, `category`, `commit` are the whole required set; a commit may be a 7-character abbreviation (`$defs/commit_input`: 7–40 hex, either case) because the server resolves and echoes the full SHA |
| `run_workflow.args.full.json` | every optional argument at once | the workflow may be named as `.github/workflows/<file>` as well as by basename; an uppercase commit is legal input; `secrets` values and `inputs` values are both string→string maps; `idempotency_key` is `^[A-Za-z0-9._:-]+$` up to 64 chars (printable, no whitespace); `timeout_secs` ≤ 21600; `log_tail_bytes` ≤ 65536; `wait: false` is the poll-instead-of-block switch (ADR-0004 §2) |
| `run_workflow.result.success.json` | the canonical terminal answer: a green coverage run | every required member of `run_workflow_result`; a terminal result carries `finished_at` and, when act reported one, `exit_code`; `evidence_files` are evidence-root-relative (`.evidence/coverage/<full-sha>/<run_id>.jsonl`) |
| `run_workflow.result.failed.json` | a red act job — `exit_code: 1`, `status: failed` | [docs/tool-contract.md §3.3](../../../docs/tool-contract.md): a workflow that ran and failed is an ordinary `isError: false` result, so it carries no `error` member but does carry `exit_nonzero` in `warnings`, and it still wrote evidence |
| `run_workflow.result.cancelled.json` | a caller-cancelled run | `cancelled` forbids `exit_code` and `error` (they belong to runs act finished); `partial_evidence_dropped` says extraction was skipped, so `evidence_files` is empty by contract, not by omission (ADR-0004 §6.2) |
| `run_workflow.result.timed_out.json` | a run killed at the end of its wall-clock budget | `timed_out` is terminal and carries `finished_at`; `process_group_killed` in `warnings` is the proof the whole group went down, not just `act`; `duration_ms` may exceed the request's `timeout_secs` by the teardown grace |
| `run_workflow.result.error.json` | the one terminal status that carries the typed error payload | `status: error` **requires** `error` (here `runner_internal_error`, `retryable: true`, with a `detail` object naming the stage); `detail` values are redacted — `<WORKSPACE>`, never a host path |
| `run_workflow.result.running.json` | the answer to a `wait: false` call, and what `run_status` mirrors | a non-terminal result must have `terminal: false` and must NOT carry `finished_at`, `exit_code` or `error`; `phase` is advisory progress inside the supervisor |
| `run_workflow.result.deduplicated.json` | a repeat `idempotency_key` returning the remembered run | `deduplicated: true` is the one field `run_workflow_result` has that `run_view` does not (checker 3 keeps the two field sets exactly one field apart); the replay is a success, not an error, and says so in `warnings` |
| `list_tasks.args.empty.json` | `{}` | every `list_tasks` argument is optional; `{}` is the "enumerate everything" call |
| `list_tasks.args.filtered.json` | one workflow plus step detail | `workflow` accepts a basename; `include_steps: true` is what asks for the per-step list |
| `list_tasks.result.full.json` | every workflow in a fixture repo, one of them unparseable | the job object's optional members — `name`, `needs`, `matrix_axes`, `runs_on`, `uses_composite`, `steps`; `category_hint` ∈ `benchmark`\|`coverage`\|`other` and is advisory only; a workflow that will not parse is reported as `parse_error: {code, message}` with `job_count: 0` and the whole call still succeeds, with `parse_error` in the top-level `warnings` |
| `list_tasks.result.filtered.json` | the answer to the filtered call above | one workflow, `workflow_count: 1`, and `steps` present only because it was asked for |
| `run_status.args.single.json` | one run plus a log-tail request | `run_id` is the server's own shape (`$defs/run_id`), and `log_tail_bytes` is honoured per call |
| `run_status.args.list.json` | `{}` | `run_id` is optional; without it the call is a page of recent runs rather than one run |
| `run_status.result.running.json` | the single-run answer, mid-run | `run_status_result` carries `run` (not `runs`) exactly when a `run_id` was given; `active_count`, `truncated` are always present; `evidence_files` is empty while extraction has not finished |
| `run_status.result.list.json` | the page answer: one running, one finished | the `runs` form of `run_status_result`, `run_summary` items — a `run_view` minus the payload, so no log tail, no evidence paths and no exit code — `active_count: 1` for the one non-terminal entry, and `truncated: true` meaning older finished runs were dropped from the page (retention is ADR-0004's) |
| `cancel_run.args.json` | the whole argument list of `cancel_run` | `run_id` only: a caller names a run it was told about and may never supply or choose a `run_id` for a new run |
| `cancel_run.result.cancelled.json` | a cancel that actually cancelled something | `already_terminal: false` **requires** `cancel_requested_at`; `kill.method` ∈ `term`\|`kill`\|`none` with an optional `took_ms` is how the process group went down (ADR-0004 §16.2) |
| `cancel_run.result.already-terminal.json` | a cancel that raced a finished run | cancelling a terminal run is not an error: `already_terminal: true` means `status` is the OLD terminal status (`success`, not `cancelled`) and both `cancel_requested_at` and `kill` are absent |
| `error.result.workflow-not-found.json` | the shared error payload for a typo'd workflow | `$defs/error` is `{code, message, retryable, detail?}` and nothing else; `workflow_not_found` is `retryable: false`; its `detail` carries the `workflows_dir`, the name asked for and the available names, which is what makes the mistake fixable in one round-trip |
| `error.result.workspace-busy.json` | the shared error payload for the single-flight lock | `workspace_busy` is `retryable: true` and its `detail` names the run in flight (`run_id`, `phase`, `started_at`) so the caller can poll instead of hammering |

### `invalid/` — 34 payloads the schema must reject

Each row states the single rule the fixture exists to test. A tool call shaped like any of
these must be refused before any subprocess starts, and — SEP-1303 — as a tool execution
error (`isError: true`), never as a JSON-RPC error.

| fixture | the rule it violates |
|---|---|
| `run_workflow.args.missing-task.json` | `task` is required; there is no "run the whole workflow" default (`act -j` always gets a job id) |
| `run_workflow.args.category-uppercase.json` | `category` is `^[a-z][a-z0-9_]*$`: the vocabulary is open but lowercase-snake, so `Coverage` is not a category — it is a typo, and a typo must not silently become "a category with no evidence" |
| `run_workflow.args.short-commit.json` | `commit` is 7–40 hex; a 6-character abbreviation is not resolvable with the confidence ADR-0001 §2.5 requires |
| `run_workflow.args.nonhex-commit.json` | `commit` is hex only: `zzzzzzz` is long enough but is not a commit |
| `run_workflow.args.bad-idempotency-key.json` | `idempotency_key` is `^[A-Za-z0-9._:-]+$`: a key is a lookup token, and `"has spaces"` is a message |
| `run_workflow.args.secret-value-not-string.json` | `secrets` is a string→string map; `42` is not a secret value, and the redaction pass is written for strings |
| `run_workflow.args.timeout-too-large.json` | `timeout_secs` ≤ 21600 (6 h): a run budget longer than that is a caller bug or a lie |
| `run_workflow.args.unknown-field.json` | `additionalProperties: false` on every object in the contract: `run_id` is a result field, and a caller that can set it can impersonate a run |
| `run_workflow.args.workflow-traversal.json` | `workflow` may not contain `..` anywhere, so `../etc/passwd` is inexpressible before any path is joined (the `workflow` row of docs/security.md's input table) |
| `run_status.args.bad-run-id.json` | `run_id` is the server-generated `%Y%m%dT%H%M%SZ-<6 hex>` shape, not an ISO timestamp: a client that pastes a timestamp into `run_id` is guessing at identity |
| `run_status.args.unknown-field.json` | `run_status` takes `run_id` and `log_tail_bytes`; a `workflow` filter is not part of this tool's contract, so the call is refused rather than quietly ignored |
| `run_status.result.both-run-and-runs.json` | `oneOf` exactly one of `run` / `runs`: a result carrying both makes the client guess which one the caller meant |
| `run_status.result.neither-run-nor-runs.json` | the same `oneOf` from the other side: `active_count` and `truncated` are not a run |
| `run_workflow.result.missing-run-id.json` | `run_id` is required on every result: without it the answer cannot be polled or cancelled |
| `run_workflow.result.missing-deduplicated.json` | `deduplicated` is required on `run_workflow_result` — the one member that distinguishes it from `run_view` (checker 3) |
| `run_workflow.result.short-commit.json` | a result's `commit` is `$defs/commit_full`: full, lowercase, 40 hex. The caller's abbreviation is resolved before the run is attributed, and `.evidence` keys are the full SHA |
| `run_workflow.result.status-succeeded.json` | `succeeded` is not a status value anywhere in this project: `success` is, and the ADR-0002 vocabulary is shared with `.evidence` `run_status` (checker 4) |
| `run_workflow.result.absolute-evidence-path.json` | no absolute host path in any result: `evidence_files` entries are relative to the evidence root |
| `run_workflow.result.explicit-null.json` | `null` is never valid anywhere in this contract; `"duration_ms": null` is a missing value spelled as a value, and unknown is spelled by omitting the key |
| `run_workflow.result.log-tail-too-long.json` | `log_tail` ≤ 65536 bytes — the largest `log_tail_bytes` a caller may ask for; a server that returns more has ignored a limit it advertised |
| `run_workflow.result.running-with-finished-at.json` | a non-terminal result may carry neither `finished_at`, `exit_code` nor `error` |
| `run_workflow.result.terminal-without-finished-at.json` | a terminal result must carry `finished_at` |
| `run_workflow.result.terminal-flag-disagrees-with-status.json` | `terminal: true` ⟺ `status` is one of the five terminal names; the flag is a convenience and may not contradict the status it summarizes |
| `run_workflow.result.error-status-without-error.json` | `status: error` requires the `error` member: an error with no reason is not actionable |
| `run_workflow.result.failed-with-error.json` | `success`, `failed`, `cancelled` and `timed_out` may not carry `error`: the run finished and its own fields explain it. Putting an error on a `failed` run is the mistake that makes a client retry a red build |
| `run_workflow.result.unknown-error-code.json` | `error_code` is a closed enum; `workflow_missing` is not a code (`workflow_not_found` is) and a code the client has never seen cannot be dispatched on |
| `error.result.missing-retryable.json` | `retryable` is required: "may I call this again?" is the one question every error answer must answer |
| `error.result.detail-not-object.json` | `detail` is an object of machine-readable keys, not a bare string: per-code keys are documented in `x-error-codes` and a client reads them by name |
| `error.result.double-nested.json` | the payload is `{"error": {…}}`, one level; a second nesting is what happens when a wrapper wraps a wrapper and the client then reads `code` as absent |
| `list_tasks.result.absolute-file.json` | a workflow `file` is workspace-relative like every other path in the contract — `/home/dev/scratch/…` leaks the server's layout |
| `list_tasks.result.bad-category-hint.json` | `category_hint` ∈ `benchmark`\|`coverage`\|`other`: the hint is advisory but it is not free text, and `cov` is neither a hint nor a category |
| `list_tasks.result.unknown-field.json` | `additionalProperties: false`, and specifically no `workspace` key: the host path is not this tool's to return |
| `cancel_run.result.missing-cancel-requested-at.json` | `already_terminal: false` requires `cancel_requested_at` — a cancel that did not record when it was asked for cannot be told apart from one that never happened |
| `cancel_run.result.already-terminal-with-kill.json` | `already_terminal: true` forbids both `cancel_requested_at` and `kill`: nothing was killed, and pretending otherwise invents a process teardown |

Three of these are rejected for more than one rule at a time —
`error.result.double-nested.json`, `run_workflow.args.short-commit.json` and
`run_workflow.args.workflow-traversal.json` — because a value that breaks a pattern usually
breaks its length bound as well. `validate_tool_contract.py --stats` counts them as
`multi_reason`; that number is a note about fixture minimalism, not a defect.

## Checking the fixtures

```sh
PYTHONPATH=... python3 scripts/validate_tool_contract.py --stats   # schema, fixtures, docs parity
python3 scripts/gen_tool_contract_fixtures.py                     # rewrite the tree from its source
```

The first runs in CI (`scripts/ci.sh schema`) and needs the `jsonschema` Python package for
its fixture and meta-schema halves — it says loudly when the package is missing, and the
docs-parity half (error-code tables, status vocabulary, this file's coverage of every
fixture) runs either way. The second is what to run after editing the generator, and re-run
on an untouched tree it must produce no diff.
