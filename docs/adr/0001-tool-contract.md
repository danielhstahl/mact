---
id: ADR-0001
title: Pin the tool contract for run_workflow and its error codes
status: accepted
ticket: mact-f1w.2
date: 2026-11-10
---

# ADR-0001: the tool contract

- **Status:** Accepted
- **Date:** 2026-11-10
- **Ticket:** mact-f1w.2
- **Machine-readable form:** [`schema/tool-contract.schema.json`](../../schema/tool-contract.schema.json)
  (JSON Schema 2020-12: `run_workflow_args` / `run_workflow_result`, `list_tasks_args` / `list_tasks_result`,
  `run_status_args` / `run_status_result`, `cancel_run_args` / `cancel_run_result`, plus `tool_error` and the
  shared `run_view`), with the error-code registry in the same file's `x-error-codes` member
- **Checked by:** [`scripts/validate_tool_contract.py`](../../scripts/validate_tool_contract.py) against the
  fixtures under [`tests/e2e/testdata/tool-io`](../../tests/e2e/testdata/tool-io)
- **Reference for implementers and the e2e test:** [`docs/tool-contract.md`](../tool-contract.md)
- **Related:** ADR-0002 (evidence JSONL contract — accepted), ADR-0003 (commit isolation / serialization),
  ADR-0004 (run lifecycle)
- **Consumers inside the epic:** mact-f1w.3 (ADR-0004), mact-f1w.4 (ADR-0003), mact-f1w.5 (scaffold + protocol
  pin), mact-f1w.6 (validator), mact-f1w.7 (`list_tasks`), mact-f1w.11 (supervisor + `run_status` /
  `cancel_run`), mact-f1w.14 (`run_workflow` wiring), mact-f1w.15 (e2e harness), mact-f1w.16 (protocol
  conformance), mact-f1w.18 (README runbook)

## Context

`act-mcp` is a single local binary: an MCP server speaking protocol **2026-07-28** over Streamable HTTP,
wrapping `nektos/act`. One call runs a GitHub Actions job against one commit and, if the category says so,
leaves a JSONL evidence file behind. ADR-0002 already froze what lands on disk; this ADR freezes what crosses
the wire.

Both are public APIs and both outlive this repo's convenience. ADR-0002 was needed first because the on-disk
files are the load-bearing artifact; the wire contract is needed now because four tickets (`run_workflow`
wiring, the validator, the supervisor, `list_tasks`) otherwise invent four slightly different shapes for the
same call, and the e2e test would have to be written against whichever one landed first.

Ground truth already established (do not re-derive):

- The `rmcp` crate is at 3.9.0 and `ProtocolVersion::V_2026_07_28` is its `LATEST`. The 2026-07-28 era is
  stateless: protocol version is signalled per request rather than by an `initialize` handshake, and
  `ServerHandler::supported_protocol_versions()` is how the server restricts itself.
- Tool results are `Json<T>` — rmcp derives an `inputSchema` from the argument struct and an `outputSchema`
  from the result struct, and the value arrives in the result's `structuredContent`.
- The workspace is the server's CWD (or `--workspace`), and it is a GitHub repository: it contains
  `.github/workflows/`. There is no auth and no remote access (loopback only, ADR-0003 / mact-f1w.13).
- ADR-0002 pinned `run_id`'s shape, the terminal `run_status` vocabulary (`success | failed | cancelled |
  timed_out | error`), the nullability policy (omit, never `null`), and the vocabulary of `warnings[]` codes.
  This ADR **reuses all of them**; a second vocabulary for the same concept is the failure mode this document
  exists to avoid.

Constraints that drive the design:

- **The caller is usually an agent, not a person.** Error results must be machine-readable and must name the
  valid options, so "one round trip to self-correct" is the normal case rather than a guess-again loop.
- **Runs are slow.** A coverage job under act is minutes; a benchmark job can be an hour. Whatever the call
  model is, it cannot assume the HTTP request and the run live and die together.
- **The server is local and unauthenticated**, so it must not become an incidental information leak: no
  absolute host paths, no secret values, and no unbounded log text in results.
- **Everything here is testable before act is installed.** The contract is exercised by stub `act`
  (mact-f1w.15) and by schema validation, so the fixtures in this repo — not a live container run — are the
  acceptance oracle.

## Decision

`MUST`, `MUST NOT` and `SHOULD` are normative. Numbering follows the ticket's "must pin" list where it can;
sections 6–8 finish what sections 1–5 depend on. Every field named here is named and typed; every error code
is listed; there is nothing in this document left to decide.

### 1. Tool set, names, and metadata

Four tools, no resources, no prompts, no `list_changed` traffic (the tool list is compile-time static).
All names are `snake_case`, ≤ 64 bytes, unique case-insensitively, and carry no `get_` / `fetch_` verb prefix
(SEP-986 naming guidance for the 2026-07-28 era):

| tool | title (human-facing) | `readOnlyHint` | `destructiveHint` | `idempotentHint` | `openWorldHint` | args / result |
|---|---|---|---|---|---|---|
| `run_workflow` | Run one workflow job against one commit | false | false | false | true | `run_workflow_args` / `run_workflow_result` |
| `list_tasks` | List workflows and job ids in this workspace | true | false | true | false | `list_tasks_args` / `list_tasks_result` |
| `run_status` | Look up one run, or list recent runs | true | false | true | false | `run_status_args` / `run_status_result` |
| `cancel_run` | Cancel a run in flight | false | false | true | true | `cancel_run_args` / `cancel_run_result` |

- `run_workflow` is `openWorldHint: true` and `destructiveHint: false`: it starts containers that may pull
  images and reach the network (the target repo's own CI job is the thing being run), but it deletes nothing
  of the caller's. `cancel_run` is `openWorldHint: true` for the same reason (it kills processes and
  containers that exist in the world).
- Each tool `MUST` ship a `description` that states, in one sentence, the error codes it can return and that
  a non-`success` run status is returned as a normal result (section 5.1). That sentence is what lets a
  non-coding client self-correct without a documentation lookup; it is part of the contract, not decoration.
- Adding a fifth tool is an additive change. Renaming one, splitting one, or changing the meaning of a field
  in an existing result is not (section 10).
- `run_status` and `cancel_run` exist because act runs outlive the request (section 4, ADR-0004). They are
  specified here, not by the supervisor ticket: the supervisor is free to add fields, not to rename them.

### 2. `run_workflow` arguments

`params.arguments` is one object; `additionalProperties: false` — an unexpected key is
`unknown_argument`, never silently ignored, because a caller who misspells `commit` must not get a run of a
different commit.

| argument | type | required | bounds | meaning |
|---|---|---|---|---|
| `workflow` | string | yes | 1..=255, `$defs/workflow_name` | which workflow file to run (2.2) |
| `task` | string | yes | 1..=255, `$defs/task_id` | the job id inside it, `act -j <task>` (2.3) |
| `category` | string | yes | 1..=64, `^[a-z][a-z0-9_]*$` | what the run is *for* and what evidence to expect (2.4) |
| `commit` | string | yes | 7..=40, `^[0-9a-fA-F]{7,40}$` | the commit the run is attributed to (2.5) |
| `wait` | bool | no (default `true`) | — | block for the terminal result, or return immediately (2.6) |
| `timeout_secs` | integer | no (default per category) | 1..=21600 | wall-clock budget for act **and** for the wait (2.6) |
| `log_tail_bytes` | integer | no (default 4096) | 0..=65536 | how much of the log to return (2.8) |
| `inputs` | string→string map | no | ≤ 32 keys, key `^[A-Za-z_][A-Za-z0-9_]{0,127}$`, value ≤ 4096 B | `workflow_dispatch`-style inputs (2.7) |
| `secrets` | string→string map | no | ≤ 32 keys, key as above, value ≤ 8192 B | secret passthrough; values never echoed anywhere (2.7) |
| `idempotency_key` | string | no | 1..=64, `^[A-Za-z0-9._:-]+$` | caller-side dedup token (section 8) |

There is deliberately **no** `run_id`, `workspace`, `path`, `argv`, `ref`, `pr`, `event` or `dry_run`
argument. A caller cannot name a path (the workspace is the server's own, fixed at startup) and cannot
supply a `run_id` (section 8); anything else would be a second, unvalidated way to say "commit" or
"workflow".

#### 2.2 `workflow`

Accepted forms, resolved in this order, case-sensitively:

1. the workspace-relative path `.github/workflows/<file>` (e.g. `.github/workflows/coverage.yml`);
2. the file name with extension (`coverage.yml`, `coverage.yaml`);
3. the basename without extension (`coverage`);
4. the `name:` key declared inside a workflow file, if exactly one file declares it.

The value `MUST` match `$defs/workflow_name`, whose character class plus a "no two consecutive dots" rule
makes `..`, absolute paths and a leading `-` inexpressible — argument-injection and path-traversal defense
before any subprocess is spawned, not after. Resolution `MUST` end at a file that canonicalizes **under**
`.github/workflows` (mact-f1w.6). The result always carries the resolved `workflow_file` and the canonical
`workflow` display name: the file basename without extension, which is exactly the value ADR-0002 stamps in
the evidence envelope. Two files matching one bare name (`bench.yml` + `bench.yaml`) is
`ambiguous_workflow`, never a precedence coin flip.

#### 2.3 `task`

The job id as declared in the workflow's `jobs:` map — exactly the string `act -j` receives. It is
`required`: the evidence's identity is `(workflow, task, commit)`, and "run the whole workflow" would make
a run whose numbers cannot be attributed to a job. `task_not_found` names the job ids that do exist.
`list_tasks` exists so nobody has to guess one.

#### 2.4 `category` — open string, server-validated shape only (**decision: open**)

`category` is an **open** vocabulary: shape-checked (`^[a-z][a-z0-9_]*$`, ≤ 64) and **not** a closed enum in
the schema. `benchmark` and `coverage` are the two known values in v1; any other shape-legal value is
accepted, the job runs, no extractor runs, `evidence_files` is empty, and the result carries
`warnings: ["unknown_category"]`.

Why open, and stated once so nobody relitigates it:

1. **Additivity.** With a closed enum, adding `fuzzing` is a breaking change to `inputSchema`: every client
   that validated its arguments against the advertised schema rejects the new value before the server can
   say yes. With an open string, adding a category is a server-side addition and old clients keep working.
   This is the reason the ticket asked for "open-ended so new categories are additive"; a closed enum
   forfeits it.
2. **Consistency with ADR-0002.** The on-disk side already treats `category` as open (`^[a-z][a-z0-9_]*$`,
   "future values") while `kind` stays a closed two-value enum. If the tool closed the enum, the wire would
   be *narrower* than the load-bearing artifact — exactly backwards.
3. **The failure mode is loud, not silent.** A misspelled category runs the job (which is what the caller
   wanted anyway), echoes the category back, writes no evidence and says `unknown_category`. A closed enum
   would turn a new-but-correct category into a hard error.

The pinned asymmetry: **`category` is open, evidence `kind` is closed** (`benchmark | coverage`, ADR-0002
§2.2). A new category only produces evidence when a matching extractor ships; until then it is a run with a
label. Known categories and what they do:

| `category` | extractor | evidence dir | default `timeout_secs` (ADR-0004) |
|---|---|---|---|
| `benchmark` | mact-f1w.12 (job metrics + act log timings) | `.evidence/benchmark/<sha>/` | 1800 |
| `coverage` | mact-f1w.10 (lcov / coverage-summary) | `.evidence/coverage/<sha>/` | 900 |
| anything else | none | — | 1800 |

#### 2.5 `commit`

7 to 40 hex digits, either case, validated **shape-first** (no subprocess) and normalized to lowercase.
Resolved with `git rev-parse --verify '<sha>^{commit}'` inside the workspace repository; no fetch, ever — a
commit the local clone does not have is `commit_unknown`, and fetching would make an unattended local server
dial out on a typo. An ambiguous abbreviation is `commit_ambiguous`. The result's `commit` is always the full
lowercase 40-hex SHA, which is what ADR-0002's directory key needs.

#### 2.6 `wait` and `timeout_secs`

`timeout_secs` is the run's wall-clock budget and, when `wait: true`, the maximum blocking time; at expiry
the act process group is killed as a group and the run ends `timed_out` (ADR-0004 owns the kill mechanics).
Defaults are per category (v1: coverage 900, benchmark 1800, other 1800) and are owned by ADR-0004, which
may add overrides; the hard bound of 21600 s is this contract's and may only be raised by an operator flag.

`wait: true` (default) blocks until the run is terminal or `timeout_secs` expires — the same number bounds
both, so a `wait: true` call returns a terminal status. `wait: false` returns as soon as the supervisor has
registered the run: `status: "running"`, no `finished_at`. Section 4 states the model and its consequences.

#### 2.7 `inputs` and `secrets`

- `inputs` → act `--input key=value`, keys in sorted order so argv is deterministic and golden-testable
  (mact-f1w.8). These are workflow-dispatch-style `with` inputs.
- `secrets` → **never argv**. They are written to a per-run `0600` file and passed with act's
  `--secret-file`, and the file is unlinked when the run finishes; `ps`, `.evidence/runs/<run_id>/meta.json`
  and every log therefore contain no secret. A `GITHUB_TOKEN` key is additionally exported in the act
  process **environment** (not argv) for image/action pulls, which is as far as it goes on a loopback
  single-user box.
- Secret values `MUST NOT` appear in a tool result, in `log_tail`, in `meta.json`, or in an evidence file.
  Redaction happens at the source (mact-f1w.8 / .13), not by filtering on the way out. A key that is not a
  legal identifier is `invalid_arguments` — that is a typo, not a secret.
- Both maps are capped (32 entries, 4 KiB / 8 KiB per value) and the request body cap (section 9) is sized
  to the worst case, so no argument map can exhaust the server.

#### 2.8 `log_tail_bytes`

Caps `log_tail` in bytes (default 4096, max 65536, `0` means "no log"). Truncation keeps whole lines and
cuts at a UTF-8 boundary; `log_tail` always holds the **end** of the log, because the end is where the reason
a run failed lives. See 3.4 for the redaction and path-normalization rules that apply to it.

#### 2.9 Validation order

Pinned, because "which error do I get when two things are wrong" is a test assertion and because the
cheapest and most-specific check should win. A call short-circuits at the first failing step:

1. JSON-RPC envelope / tool name (rmcp; `-326xx`, section 5.2).
2. Argument shape, type, bound, pattern → `invalid_arguments`, `unknown_argument`. No filesystem access yet.
3. `idempotency_key` replay: known key + same tuple → return the remembered run (section 8); known key +
   different tuple → `idempotency_conflict`.
4. Workspace gates → `workspace_not_found`, `not_a_git_repository`, `workflows_dir_missing`.
5. `commit` resolution → `commit_unknown`, `commit_ambiguous`.
6. `workflow` / `task` resolution → `workflow_not_found`, `ambiguous_workflow`, `task_not_found`.
7. Environment probes → `act_missing`, `docker_unreachable` (probes cached ≤ 30 s so a burst of calls does
   not re-`docker info`).
8. Isolation of the target tree (ADR-0003) → `isolation_failed`.
9. Capacity and the workspace single-flight lock → `too_many_runs`, `workspace_busy`.
10. Register the run, allocate `run_id`, start act. Everything after this point is a run outcome, not a call
    error (`act_spawn_failed` and friends, section 5.3).

Step 2 before step 4 is deliberate: shape validation is free, subprocesses are not, and mact-f1w.6's
validator is built in this order.

### 3. `run_workflow` result

A successful (`isError: false`) `run_workflow` call returns this object in `structuredContent`. It is
`run_view` (3.1) plus `deduplicated`; the JSON Schema spells both out so a generated Rust struct can be
diffed against either, and `scripts/validate_tool_contract.py` asserts the two field sets stay equal.

#### 3.1 `run_view` — the fields every run-returning tool shares

| field | type | present | meaning |
|---|---|---|---|
| `run_id` | string | always | `^[0-9]{8}T[0-9]{6}Z-[0-9a-f]{6}$`, server-generated (ADR-0002 §1.2) |
| `status` | string | always | `$defs/status`: `pending \| running \| success \| failed \| cancelled \| timed_out \| error` |
| `terminal` | bool | always | `true` iff `status` is one of the five terminal values; redundant by design so a poller needs no status table |
| `phase` | string | usually | coarse progress, open enum `queued \| preparing_tree \| starting_act \| running \| extracting_evidence \| publishing_evidence \| cancelling \| done`; display-only, nothing may branch on it (ADR-0004) |
| `workflow` | string | always | resolved display name: file basename without extension; same value as the evidence envelope's `workflow` |
| `workflow_file` | relpath | always | e.g. `.github/workflows/coverage.yml` |
| `task` | string | always | the job id that ran (`act -j`) |
| `category` | string | always | echoed back exactly as requested (that is the point of echoing it) |
| `commit` | string | always | full lowercase 40-hex SHA — never the caller's abbreviation |
| `started_at` | timestamp | once act was started | UTC `Z` instant, same format rule as ADR-0002 §2.2 |
| `finished_at` | timestamp | terminal only | required exactly when `status` is terminal (schema-enforced) |
| `duration_ms` | int ≥ 0 | when the clock knows | total wall duration when terminal; elapsed-so-far while `pending`/`running` |
| `exit_code` | int 0..=255 | when act had one | act's `wait(2)` code; never present for `pending`/`running` |
| `evidence_files` | relpath[] | always | the JSONL evidence this run published, relative to the evidence root (`.evidence/coverage/<sha>/<run_id>.jsonl`). `[]` is legitimate |
| `evidence_record_count` | int ≥ 0 | always | records across `evidence_files`; `0` means "ran and found nothing", not "failed" |
| `log_tail` | string | always | bounded, redacted tail of act's log (3.4); `""` when nothing was logged |
| `warnings` | string[] | always | open vocabulary of caveats (5.4); `[]` normally |
| `error` | object | only when `status: "error"` | typed `{code, message, retryable, detail}`; schema-forbidden on every other status |

Schema-enforced invariants, so an implementation cannot "helpfully" produce an inconsistent result:

- terminal `status` ⇒ `finished_at` present and `terminal: true`; non-terminal ⇒ `finished_at`,
  `exit_code` and `error` all absent and `terminal: false`;
- `status: "error"` ⇒ `error` present; any other terminal status ⇒ `error` absent;
- no `null` anywhere: a value is either present with the right type or the key is absent (ADR-0002 §2.1);
- `additionalProperties: false`.

#### 3.2 Status vocabulary (`success`, not `succeeded`)

The five terminal values are **byte-identical** to `.evidence`'s `run_status` (ADR-0002) and to
`schema/run-meta.schema.json`: `success | failed | cancelled | timed_out | error`. Meaning:

| status | meaning | `isError` |
|---|---|---|
| `success` | act exited 0 | false |
| `failed` | act ran and the job exited non-zero (`exit_code` present) | false |
| `cancelled` | `cancel_run`, or an MCP cancellation of a `wait: true` call, took it down | false |
| `timed_out` | the `timeout_secs` budget expired and the process group was killed | false |
| `error` | the server could not run the job to completion; `error.code` says which (§5.3 runner group) | false |

Three deliberate departures from the ticket's shorthand:

- The ticket wrote `succeeded`; the accepted ADR-0002, `schema/evidence.schema.json` and
  `schema/run-meta.schema.json` all say `success`, and those files are already machine-checked against
  fixtures. One word, and it is the one that already ships. `succeeded` is not a status value anywhere in
  this project, and there is a fixture
  (`run_workflow.result.status-succeeded.json`) whose only job is to keep it that way.
- The ticket's list omitted `timed_out`. A timeout is the single most common non-`success` outcome for act
  runs and is operationally different from a user cancel (one is "give it more time", the other is "someone
  stopped it"), so it keeps its own value.
- `pending` and `running` are added for the non-terminal states the run/poll model (section 4) requires.
  They can only be observed through `wait: false` or `run_status`.

#### 3.3 `evidence_files` and `evidence_record_count`

Paths are relative to the evidence root (ADR-0002 §1.1), forward slashes, no absolute host path — the same
relpath rule and for the same reasons (portability, privacy, golden tests that survive a different machine).
`meta.json` and `act.jsonl` are **not** listed: they are audit paths, and the array means "the JSONL a
scanner would read". `evidence_record_count` is the sum over those files. An unknown category, a run that
died before extraction, and a coverage job that found no `lcov.info` all yield `[]` / `0` with a warning
code — never an error.

#### 3.4 `log_tail`

Derived from act's captured log (`.evidence/runs/<run_id>/act.jsonl`), after secret redaction, capped by
`log_tail_bytes`, and with the workspace root and evidence root replaced by the literals `<WORKSPACE>` and
`<EVIDENCE>`. That last rule keeps host absolute paths out of the wire (the same rule ADR-0002 has for
evidence) and makes results diffable across machines — which is what lets the e2e harness assert on log
content at all.

### 4. Call model: `run_workflow` waits by default, and the run outlives the request

**Decision: option B of ADR-0004 with wait-by-default** — i.e. a supervisor-owned run plus an in-call wait
mode — and this section is binding on ADR-0004 (mact-f1w.3), which owns the state machine and the defaults
but not this choice:

- Validation (section 2.9 steps 1–9) is synchronous. A precondition failure never allocates a `run_id`.
- A run is owned by the supervisor, not by the request. If the HTTP request dies mid-wait, **the run keeps
  running** and the durable trace is `.evidence/runs/<run_id>/`. Nothing about the transport cancels work.
- `wait: true` (default): the request waits for the terminal result, bounded by `timeout_secs`. Because the
  wait budget and the run budget are the same number, a `wait: true` result is always terminal. This is the
  friendliest default: the common agent call is one round trip, and a client with a short timeout can lower
  `timeout_secs` or switch to `wait: false`.
- `wait: false`: return the moment the run is registered, `status: "running"`, and poll `run_status`. This
  is how a caller makes a one-hour benchmark survivable.
- An MCP cancellation (`$/cancel-request`) arriving for a `wait: true` call means "stop the work": the wait
  ends and cancellation of the run is requested (`status: cancelled`). A caller that wants the run to
  survive must use `wait: false`. ADR-0004 owns the kill mechanics (process group, grace, container cleanup)
  and the retention window, not this rule.
- `run_status` (with no `run_id`) is the recovery path after a lost `run_id`, and `idempotency_key` the
  recovery path after a lost *connection* (section 8).

Rejected in brief (full trade-offs in ADR-0004, which must record the same): a pure blocking call loses the
run when the request dies and offers no way to watch or stop a 40-minute run; the MCP Tasks extension
(SEP-1686 / SEP-2663) is the right eventual façade but only Tasks-aware clients can use it, so it wraps this
supervisor later rather than being the baseline.

### 5. Error semantics

#### 5.1 The split: tool execution errors vs JSON-RPC protocol errors

Per MCP 2026-07-28 (SEP-1303): **bad arguments and precondition failures are tool execution errors**,
returned as a tool result with `isError: true` and a machine-readable `code`, *not* as JSON-RPC protocol
errors.

The pinned rule, which the whole table in 5.3 follows:

- `isError: true` ⇔ **no run was created and no run was controlled.** Everything the caller could not have
  known from the argument shapes — workflow missing, commit unknown, workspace busy, act missing, run not
  found — is a tool execution error with a `code`.
- A run that reached a terminal state is a **successful tool call** (`isError: false`) whose `status` says
  `failed`, `timed_out`, `cancelled` or `error`. A red CI job is data: the caller asked "run this job and
  tell me", the server told it, and ADR-0002 writes evidence for failed runs on purpose. Making
  `status: "failed"` an `isError: true` would put the most common interesting result on the error path and
  invite clients to discard it.
- JSON-RPC `error` objects are produced only by rmcp, only for protocol-level faults, and never to describe
  run semantics:

  | JSON-RPC code | when |
  |---|---|
  | `-32700` parse error | body is not JSON or not one JSON object |
  | `-32600` invalid request | not a JSON-RPC 2.0 request |
  | `-32601` method not found | unknown method |
  | `-32602` invalid params | bad `params` envelope or unknown tool name — **not** bad tool arguments |
  | `-32603` internal error | rmcp's own failure (transport, serialization); `act-mcp` never reports a tool failure with it |

#### 5.2 The error payload

An `isError: true` result carries:

```json
{
  "content": [{"type": "text",
               "text": "run_workflow: workflow_not_found: no workflow named 'bench' in .github/workflows"}],
  "structuredContent": {"error": {"code": "workflow_not_found",
                                  "message": "no workflow named 'bench' in .github/workflows",
                                  "retryable": false,
                                  "detail": {"workflow": "bench",
                                             "workflows_dir": ".github/workflows",
                                             "available": ["benchmark", "coverage"]}}},
  "isError": true
}
```

- `structuredContent` is exactly `{"error": {...}}` (`$defs/tool_error`) — the tool's `outputSchema` shape is
  **not** used for errors, and clients validate against `outputSchema` only when `isError` is false.
- The text block's first two tokens are `<tool>: <code>:` in that fixed order, so a client that ignores
  `structuredContent` still gets the code machine-readably. This belt-and-braces duplication is intentional:
  the code must survive the lowest-common-denominator client.
- `error` object: `code` from the closed set below, one-sentence `message` (≤ 1024 bytes, no stack trace, no
  absolute host path, secrets redacted), `retryable` from the table below, and an optional `detail` object
  (≤ 4096 bytes compact) whose per-code keys are documented in `x-error-codes`. Resolution errors
  (`workflow_not_found`, `task_not_found`, `ambiguous_workflow`, `commit_ambiguous`) `MUST` name the valid
  options — that is the one-round-trip self-correction guarantee.
- `retryable: true` means a later identical call may succeed; it is never a licence for a tight retry loop
  (a client `SHOULD` back off, and a client that has burned its `idempotency_key` budget on a retry should
  expect `workspace_busy` anyway).

#### 5.3 The complete error-code list

Closed set. `isError` is the result flag; `retryable` is the `error.retryable` value. Both docs carry this
table verbatim and `scripts/validate_tool_contract.py` fails the build if the two, the schema's
`$defs/error_code` and `x-error-codes` ever disagree — including on the `isError` / `retryable` columns.

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
| `isolation_failed` | true | true | precondition | ADR-0003 could not produce a tree for that commit (the head-only policy, or the tree gate refused the tree: dirty, sparse, bare, submodules); nothing was started. A worktree failure *after* registration is `runner_internal_error` with `detail.stage: "preparing_tree"` (ADR-0003 §4.4, §15.3). `detail.mode`, `detail.requested`, `detail.head`, `detail.reason` |
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

The six `isError: false` rows are the runner group: they surface as `status: "error"` **inside** a normal
result, with the `error` object filled in. Every code that can name a run as its subject (`act_spawn_failed`,
`act_crashed`, `runner_internal_error`, `evidence_*`) therefore appears in `run_view.error`, and every other
row appears in `tool_error.error`.

A client that receives a `code` it does not know `MUST` treat the call as a failed tool call, never as a
success, and `MUST NOT` retry blindly: codes are added additively (section 10) and an unknown code is by
definition unclassified.

#### 5.4 Warnings are a different namespace

`warnings` (and `meta.json.warnings`) is an **open** vocabulary of caveats that never change `status`:
`unknown_category`, `deduplicated`, `exit_nonzero`, `process_group_killed`, `partial_evidence_dropped`,
`already_terminal`, `orphaned_containers` (added by ADR-0004 §5.6), `isolation_worktree`, `dirty_worktree`,
`worktree_cleanup_failed` (added by ADR-0003 §9.3), `parse_error`, plus every
ADR-0002 §5.1 code (`no_coverage_found`, `parse_failed`,
`unsupported_format`, `payload_too_large`, `payload_unreadable`, `payload_transport_unavailable`,
`coverage_source_shadowed`, `malformed_metric`, `marker_cap_reached`, `truncated`). Unknown warning codes are
ignored, never fatal. Extraction problems are warnings, never run failures (ADR-0002 §5.1); write failures
are the one evidence-adjacent exception, and they are errors (`evidence_write_failed`).

### 6. `list_tasks`, `run_status`, `cancel_run`

The shapes are pinned here so the tools that consume them (`run_workflow` wiring, the e2e harness) can be
written before the supervisor lands. mact-f1w.7 owns `list_tasks` heuristics; mact-f1w.11 owns the
supervisor that answers these.

#### 6.1 `list_tasks`

| argument | type | required | meaning |
|---|---|---|---|
| `workflow` | `$defs/workflow_name` | no | return just this workflow's jobs |
| `include_steps` | bool | no (default false) | also return each job's step summary (`name`, `uses`) |

Result (`list_tasks_result`): `workflows_dir` (`.github/workflows`), `workflow_count`, `workflows[]` of
`{name, file, display?, job_count, jobs[], parse_error?}`, and `warnings[]`. Each job is
`{id, name?, needs[], runs_on[], matrix_axes[], uses_composite?, category_hint, steps?}` with
`category_hint` ∈ `benchmark | coverage | other` — **advisory only**, from a documented heuristic, and the
caller still passes `category` explicitly to `run_workflow`.

- `category_hint` is a hint and the tool says so in its description; a caller that wants `category` decided
  for it is not what this tool is for.
- A workflow file that fails to parse is returned with `parse_error: {code, message}`, `code` ∈
  `invalid_yaml | not_a_map | jobs_missing | unsupported_yaml` — its own tiny namespace, deliberately
  distinct from the tool error codes, because one broken file is *data inside a successful call*, not a
  failed call. Never does one bad file break the call; `warnings` gets `parse_error`.
- `list_tasks` takes no path arguments and takes no workspace lock, so it is safe to call while a benchmark
  is mid-flight (which also makes it the cheapest end-to-end smoke call). Its only error codes are
  `invalid_arguments`, `unknown_argument`, `workspace_not_found` and `workflows_dir_missing`.

#### 6.2 `run_status`

| argument | type | required | meaning |
|---|---|---|---|
| `run_id` | `$defs/run_id` | no | with it: that run, as `run`. without it: the recent/active listing as `runs` |
| `log_tail_bytes` | integer | no (default 4096) | `run` mode only, same cap as 2.8 |
| `limit` | integer 1..=50 | no (default 50) | listing mode only, newest first |

Result: `{run, active_count, truncated}` or `{runs, active_count, truncated}` — exactly one of `run` /
`runs` (schema-enforced), `runs[]` being `run_summary` records (`run_id`, `status`, `terminal`, `phase`,
`workflow`, `task`, `category`, `commit`, `started_at`, `duration_ms` — deliberately no log or evidence
detail: the listing is a recovery aid, not a log viewer). `active_count` counts every non-terminal run in
the process, paged or not; `truncated` says older finished runs were dropped.

- Unknown or evicted `run_id` → `run_not_found` (error). Run state is in memory (ADR-0004); `.evidence/runs/<run_id>/`
  is the durable record and `run_status` does not read it. After a restart the listing is simply empty.
- `run_status` never blocks and never starts anything.

#### 6.3 `cancel_run`

| argument | type | required |
|---|---|---|
| `run_id` | `$defs/run_id` | yes |

Result: `{run_id, status, terminal, already_terminal, cancel_requested_at?, kill?, warnings[]}`. `kill`
records `{method: term | kill | none, took_ms?}` — how the process group went away (ADR-0004 mechanics).

- **Cancelling is idempotent and never an error for an already-terminal run**: `already_terminal: true`, the
  original terminal `status` (not `cancelled`), no `cancel_requested_at`, no `kill`. Cancelling a queued run
  dequeues it (`kill.method: "none"`). The one error is `run_not_found`.
- A `cancel_run` for a run that is mid-kill returns the same shape; the second caller sees
  `status: "cancelled"` with `terminal: false` while the teardown finishes, then `terminal: true` from
  `run_status`. In that same window `run_status`/`run_view` reports `status: "running"` with
  `phase: "cancelling"`, because `run_view`'s schema forces `terminal: true` for any terminal status and
  cannot express a non-terminal `cancelled`; ADR-0004 §16.1 pins this and the mechanics.

### 7. Protocol and version policy

- The server implements **exactly one** protocol version: `2026-07-28`.
  `ServerHandler::supported_protocol_versions()` returns `[V_2026_07_28]` and nothing else.
- No `initialize`-era compatibility, no SSE-era compatibility
  (`legacy_session_mode = false`), no stdio transport, no protocol downgrade path. A client asking for an
  older version gets rmcp's negotiation rejection (an HTTP 4xx whose body says the version is unsupported),
  no tool is executed, and `act-mcp` adds nothing of its own on top. mact-f1w.16 asserts rmcp's actual
  status and body rather than re-implementing the check, so the conformance test pins our behavior by
  pinning rmcp's.
- The era is stateless: protocol version travels per request in rmcp's stateless protocol metadata (the
  `MCP-Protocol-Version` header). `stateless_protocol_metadata_required = true` — a request that omits it is
  rejected rather than assumed to be some older dialect.
- Transport surface (the knobs mact-f1w.5 sets, pinned so hardening in mact-f1w.13 has one thing to review):

  | knob | value | why |
  |---|---|---|
  | listener | `--listen 127.0.0.1:7788` default, non-loopback bind refused at startup | loopback-only, no auth (epic non-goal) |
  | endpoint | `POST /mcp` (rmcp `StreamableHttpService`, `nest_service("/mcp", …)`) | one path, one method in v1 |
  | `allowed_hosts` | `{127.0.0.1:7788, localhost:7788}` (whatever `--listen` says) | DNS-rebinding guard |
  | `allowed_origins` | empty by default | no browser client is expected; operators add explicitly |
  | `max_request_body_bytes` | 1048576 | worst-case args are 32×8 KiB secrets ≈ 300 KiB |
  | `json_response` | `true` | responses are `application/json`; v1 sends no server-initiated streams — `run_status` polling is the progress channel |
  | `cancellation_token` | wired to the supervisor | required by the section 4 cancel rule |

### 8. Run identity, idempotency and dedup

- **`run_id` is always server-generated** and never accepted from a caller: the shape is a sortable UTC
  timestamp (ADR-0002 §1.2) and letting a client supply it would put client-chosen strings into filesystem
  paths. Passing one is `unknown_argument`. Generation happens at step 10 of the validation order, by the
  supervisor, monotonic within a process; the (astronomically unlikely) collision case is
  `evidence_path_exists`, not a silent merge.
- **A repeat of the same `(workflow, task, category, commit)` starts a new run.** No implicit tuple dedup:
  two runs of one commit are two measurements, and ADR-0002 §4.4 makes duplicates-across-runs data owned by
  the scanner, not a bug to hide. Each run gets a fresh `run_id` and publishes its own file, so a rerun can
  never overwrite history.
- The single-flight workspace lock (ADR-0003 / ADR-0004) means two identical tuples cannot run in one
  workspace concurrently anyway: the second call gets `workspace_busy` with the in-flight `run_id`, `phase`
  and `started_at` in `detail`, which is the useful answer ("your run is already going, here it is").
- **Explicit dedup is opt-in** through `idempotency_key`:
  - same key + same tuple → the remembered run is returned with `deduplicated: true`; if it is still
    running, the call waits per `wait`;
  - same key + different tuple → `idempotency_conflict` (never silently start a second run under a name that
    already means something else);
  - keys and run state live in memory with the same retention (ADR-0004). After a restart, a replay starts a
    fresh run — the durable trace is `.evidence/runs/<run_id>/`, and this is documented rather than
    papered over.
  This is what makes "retry after a dropped connection" safe, which is the realistic case for a 30-minute
  `wait: true` call whose client gave up and reconnected.

### 9. Caps

Everything caller-influenced has a number next to it; each is a schema bound, and exceeding one is
`invalid_arguments`, never a truncation surprise.

| thing | cap |
|---|---|
| HTTP request body | 1 MiB |
| `workflow` / `task` | 255 bytes |
| `category` | 64 bytes |
| `commit` | 7..=40 hex |
| `idempotency_key` | 64 bytes |
| `timeout_secs` | 21600 (6 h) |
| `timeout_secs` defaults | coverage 900, benchmark 1800, other 1800 (ADR-0004) |
| concurrent act runs per server | 2 (`--max-concurrent-runs`) |
| concurrent act runs per workspace | 1 (single-flight, ADR-0003/0004) |
| `inputs` / `secrets` entries | 32 each; values 4 KiB / 8 KiB |
| `log_tail_bytes` | 65536 (default 4096) |
| `evidence_files` | 8 |
| `warnings` | 64, unique |
| `run_status` listing | 50, newest first |
| `error.message` | 1024 bytes |
| `error.detail` | 4096 bytes when compact |
| `list_tasks` workflows / jobs / steps | 1024 / 1024 / 256 |
| evidence payload caps | ADR-0002 §3.3 (8 MiB per file, 5000 metrics, 32 incoming files) |

### 10. Compatibility policy

The contract is this ADR + `schema/tool-contract.schema.json` + `docs/tool-contract.md`, kept in lockstep by
the checker; an implementation that finds a hole fixes all three in one change.

- **Additive (no coordination needed):** a new tool; a new optional argument with a default; a new optional
  field in a result; a new `category` value; a new `warnings` member; a new `error_code` member (old clients
  treat it as unclassified failure — see 5.3); widening a cap; a new `parse_error` code.
- **Breaking (new major of the server; say so in the changelog and in the tool description):** renaming or
  removing a field, tool or code; changing a field's type or meaning; making an optional field required;
  closing an open vocabulary (`category`, `warnings`); redefining an existing `warning`; changing the
  `isError` rule (5.1) or the `run_view` invariants (3.1); moving from wait-by-default to a
  return-immediately default, which is a behavior change every caller feels.
- The version stamp is the server's own `serverInfo.version` plus `schema_version` inside the `.evidence`
  files (ADR-0002). There is no `contract_version` field on the wire: the wire contract is
  `outputSchema`/`inputSchema`, and a client that caches those detects the change itself.

## Verification

| check | owner | command / test |
|---|---|---|
| schema is a legal 2020-12 schema; all 21 valid fixtures accepted; all 34 invalid fixtures rejected | mact-f1w.2 (this ADR), then mact-f1w.17 CI | `python3 scripts/validate_tool_contract.py --stats` |
| error-code table identical in the ADR, in `docs/tool-contract.md`, in `$defs/error_code` and in `x-error-codes` (including the `isError` / `retryable` columns) | mact-f1w.2 | same command, check 5; the same command fails on a one-word edit |
| status vocabulary agrees with ADR-0002's schema files; `succeeded` nowhere | mact-f1w.2 | same command, check 4; fixture `run_workflow.result.status-succeeded.json` |
| `run_workflow` result field set is `run_view` + `deduplicated` | mact-f1w.2 | same command, check 3 |
| the Rust structs agree with the checked-in schema | mact-f1w.5 / .14 | unit test `tool_schema_parity`: serialize the schemars-derived `inputSchema` / `outputSchema` of all four tools and compare (after key-order normalization) against `schema/tool-contract.schema.json`; the fixtures are the golden payloads |
| args rejected before any subprocess, in the pinned order | mact-f1w.6 | validator unit tests, one per code in the resolution/precondition groups |
| `structuredContent` of a real call matches a fixture | mact-f1w.15 | e2e harness normalizes `run_id`, timestamps, `pid` and hostname, then diffs against `tests/e2e/testdata/tool-io/valid` |
| protocol negotiation (only 2026-07-28; missing version metadata rejected) | mact-f1w.16 | conformance suite against the running server |
| `isError` discipline: a failed act run is `isError: false`, a missing workflow is `isError: true` | mact-f1w.14 / .15 | stub-act e2e: one fixture repo, one green job, one red job, one typo'd workflow name |

## Consequences

**Good**

- Four tickets can be written in parallel against one shape, and the e2e test exists before the code does:
  the fixtures are the spec, and they were hand-written from this document, not emitted by an implementation.
- Every failure an agent can hit has a name and, for the interesting ones, a list of valid options: a typo
  costs one round trip instead of a search loop.
- One vocabulary across the wire and the disk (`run_id`, `run_status`, `warnings`, relpaths) means a scanner,
  a log line and a tool result cannot disagree about what a run was.
- Because a run's terminal outcome is a normal result, clients that only look at `isError` still get correct
  behavior for the interesting case (a red CI run) — they just see a payload.
- Secrets and host paths are structurally absent rather than filtered: `--secret-file`, redaction at the
  source, `<WORKSPACE>`/`<EVIDENCE>` substitution. There is no code path where a secret is in `ps` output.
- Caps everywhere, so a confused or hostile local caller cannot exhaust the box through the tool surface.

**Costs**

- Two knobs per failure kind (`isError` + `status`) and a richer result than "a string and a bool". A client
  must read `structuredContent` to do anything useful; a text-only client sees a one-line error and no
  payload.
- `wait: true` by default means an agent can hold an HTTP request open for up to `timeout_secs`. Documented,
  bounded, and the escape hatches (`wait: false`, `cancel_run`, lower `timeout_secs`) are cheap — but it is a
  real cost, and ADR-0004 restates it.
- `run_view` is spelled out twice in the schema (once bare, once inside `run_workflow_result`) to keep the
  Rust struct diff simple; the checker is what keeps the two copies honest rather than a `$ref`.
- The closed error-code set is a maintenance obligation: every new failure mode has to pick a name before it
  can report one. That is the point, but it is paperwork.
- Shape-only `category` validation means a misspelling is not caught at validation time; it is caught by
  `warnings: ["unknown_category"]` plus an empty `evidence_files` instead of by an error message.
- Warnings-vs-errors and status-vs-isError are three separate axes (status, `isError`, `warnings`) that a
  client must understand. The rules are in 5.1 and 5.4 and in the tool descriptions, and they are the part
  of this ADR most likely to be misread by a new consumer.

## Considered alternatives

- **`category` as a closed schema enum.** Rejected in 2.4: it makes additive categories breaking, and it
  makes the wire narrower than the already-accepted on-disk vocabulary.
- **`status: "succeeded"`** (the ticket's shorthand). Rejected: ADR-0002, `schema/evidence.schema.json` and
  `schema/run-meta.schema.json` already ship `success`, and a per-call rename of an accepted vocabulary is
  exactly the drift this ADR exists to prevent. A fixture now guards the word.
- **`timed_out` folded into `error`.** Rejected: timeouts need their own answer ("more time"), a cancel needs
  a different one ("someone stopped it"), and ADR-0002 already publishes `timed_out` in the evidence
  envelope; a run whose tool status says `error` while its evidence says `timed_out` is a bug factory.
- **`isError: true` for a failed run.** Rejected in 5.1: it puts the most common interesting result on the
  error path, and clients that discard error payloads would then throw away the numbers of a red benchmark
  run — the exact data a benchmark history most needs.
- **A run's own error codes in the same table as call errors, one flat list with no rule about which
  appears where.** Rejected: the runner group's codes live in `run_view.error` and the call group's in
  `tool_error.error`, and the checker enforces the shapes, so a runner code can never leak into a
  `tool_error` and vice versa.
- **Blocking-only `run_workflow` (option A).** Rejected with ADR-0004: a dropped request would take the run
  (and its containers) down, and a 40-minute call cannot be watched or stopped.
- **MCP Tasks as the baseline (option C).** Rejected with ADR-0004 for now: correct end state, but only
  Tasks-aware clients could use the server at all. The supervisor this ADR assumes is what Tasks would
  later wrap.
- **Client-supplied `run_id`.** Rejected: caller-controlled strings in filesystem paths (ADR-0002 §6.3) for
  no benefit once `idempotency_key` exists — dedup is what the client actually wants, and an opaque token
  gives it without path exposure.
- **Implicit dedup on `(workflow, task, category, commit)`.** Rejected in section 8: it silently throws away
  a measurement the caller asked for, and repeated samples are the point of a benchmark harness.
- **Absolute paths in results.** Rejected: ADR-0002 already forbids them in evidence for portability and
  privacy, results are diffed in tests, and `<WORKSPACE>` is stable across machines.
- **`null` for unknown fields.** Rejected in favour of omission, as ADR-0002 did: one rule, no
  absent-or-null two-valued type, and `additionalProperties: false` keeps both honest.
- **A `dry_run` argument.** Rejected: validation is already a complete answer to "would this run be
  accepted" — it happens before anything starts — and `list_tasks` is the discovery half. A `dry_run` flag
  would be a second, softer path through the same code.

## Binding on the other decisions

These are decisions *here*, stated so ADR-0003 and ADR-0004 cannot quietly contradict them. Each must be
restated verbatim in its own ADR (ticket acceptance says exactly that):

- **ADR-0004** owns the state machine, per-category timeout defaults and overrides, cancellation mechanics
  (process group, grace, container cleanup), retention of in-memory run state, and the Tasks-extension
  deferral. It must keep: the terminal status names in 3.2; "a run outlives the request" and
  "`wait: true` returns a terminal status" in section 4; cancellation-of-a-waiting-call meaning "stop the
  run"; `timeout_secs` bounds of 1..=21600; the `phase` enum's shape (open for display, nothing branches on
  it); and the `run_not_found` behavior after eviction.
- **ADR-0003** owns HEAD-gate versus worktree isolation and the workspace lock. It must keep:
  `isolation_failed` as the only code for "we cannot run this commit here" and `workspace_busy` as the busy
  answer with the in-flight run's identity in `detail`; one run per workspace, no queue; and the invariant
  that the `commit` in a result is the commit that was actually executed. (Kept: ADR-0003 §2, §4, §6.)
- **mact-f1w.6** implements the validation order of 2.9 and the resolution rules of 2.2 / 2.5 with exactly
  these code names (`commit_unknown`, `workflow_not_found`, `ambiguous_workflow`, `task_not_found`,
  `act_missing`, `docker_unreachable` are the names mact-f1w.6 already used; `isolation_failed`,
  `workspace_busy` and `too_many_runs` come later in the pipeline).
