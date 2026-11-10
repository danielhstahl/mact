---
id: ADR-0004
title: run lifecycle — supervisor-owned runs with poll/cancel, wait-by-default; MCP Tasks deferred
status: accepted
ticket: mact-f1w.3
date: 2026-11-10
---

# ADR-0004: the run lifecycle

- **Status:** Accepted
- **Date:** 2026-11-10
- **Ticket:** mact-f1w.3
- **Decision:** **Option B** — a run is owned by a supervisor task, not by the request. `run_workflow`
  returns a terminal result when `wait: true` (default) and a `run_id` immediately when `wait: false`;
  `run_status` / `cancel_run` are the poll and stop channels. **Option C** (the MCP Tasks extension) is
  deferred as an additive façade over the same supervisor (§12). **Option A** (blocking-only) is rejected.
- **Machine-readable form:** none new. Every number this ADR pins sits *inside* a bound that
  [`schema/tool-contract.schema.json`](../../schema/tool-contract.schema.json) already states
  (`timeout_secs` 1..=21600 default-per-category, `kill.took_ms` ≤ 300000, `duration_ms` ≤ 259200000,
  `log_tail_bytes` ≤ 65536, listing ≤ 50). The numbers become clap defaults in mact-f1w.5's CLI, and
  §11 is that table.
- **Checked by:** [`scripts/validate_run_lifecycle.py`](../../scripts/validate_run_lifecycle.py) — it
  proves the schema claims in §16 (that `run_view` really cannot express a non-terminal `cancelled`, and
  that `cancel_run_result` can express every row of §5.2), and that §11's defaults equal one table in the
  script and sit inside the bounds in `schema/tool-contract.schema.json`. Runtime behaviour is covered by
  the supervisor and runner tests listed in §13 (stub `act`, no docker). `scripts/validate_tool_contract.py`
  still guards the tool contract and the two documentation edits §16 asks for.
- **Related:** ADR-0001 (tool contract — accepted, and it binds this one; §16 lists the reconciliations),
  ADR-0002 (evidence JSONL contract — accepted), ADR-0003 (commit isolation / workspace lock — accepted,
  and it answers §8 and the §18 handoff)
- **Consumers inside the epic:** mact-f1w.8 (runner: the only code that signals a process), mact-f1w.11
  (supervisor + `run_status` / `cancel_run`), mact-f1w.14 (`run_workflow` wiring), mact-f1w.13 (caps),
  mact-f1w.15 (e2e harness), mact-f1w.16 (protocol conformance: cancellation notification), mact-f1w.18
  (runbook: orphaned containers, tuning), mact-f1w.19 (real-act: does act honour SIGTERM)

## 1. Context

A benchmark or coverage workflow under `act` is a multi-minute to multi-hour container run. The
server's own measurement is that a coverage job is minutes and a benchmark job can be an hour, and the
`timeout_secs` bound ADR-0001 §9 already pinned (21600 s) says the contract expects to be asked for
both. So the first public API decision after "what crosses the wire" is **how a call relates to the
time a run takes**.

Constraints that drive the design, most of them already pinned elsewhere:

- **A blocking tool call is a bad home for a 40-minute run.** Clients time out, retry, and end up with
  two runs when they wanted one. A request that is still open cannot be watched or stopped from
  anywhere else.
- **The transport must not decide whether work happens.** ADR-0001 §4 already states the rule this ADR
  implements: if the HTTP request dies, *the run keeps running*, and the durable trace is
  `.evidence/runs/<run_id>/`. That is only possible if the run is owned by something other than the
  request task.
- **There is no server-pushed progress in v1.** ADR-0001 §7 pins `json_response = true` and "v1 sends no
  server-initiated streams — `run_status` polling is the progress channel". So the progress story has
  to work with a client that polls, and `run_status` must be cheap enough to poll every second.
- **Vocabulary is not this ADR's to invent.** The status names, the `phase` members, the `warnings`
  codes, the error codes, the `run_id` shape and the terminal/non-terminal split are pinned by ADR-0001
  §3.1–3.2, ADR-0002 §1.2/§5.1 and the three schema files. This ADR adds no status, no error code and
  no tool, and exactly two members to the *open* `warnings` vocabulary (§5.6, §16).
- **One run per workspace, no queue** is pinned by ADR-0001 §5.3 (`workspace_busy`: "there is no
  queue") and ADR-0002 §6.3 (that lock is the evidence layer's single-writer guarantee). mact-f1w.11
  leaves the queue question to this document; the answer stays *no queue* (§8).
- **Cancellation has to reach containers, not just `act`.** `act` is a container orchestrator: it
  forks `docker`/`podman` clients and leaves containers behind. Killing one pid is not cancelling a
  run (mact-f1w.8 already assumes `process_group(0)` for exactly this reason).
- **The caller is an agent.** Every state it can observe has to be nameable and every terminal state has
  to be reachable by a documented path, or the agent polls forever. §10 is the poll contract; the
  transition table in §3.3 is the enumeration that makes "what can I still see?" answerable.

Ground truth established before this ticket (do not re-derive): ADR-0001 §4 pins option B with
wait-by-default and states it is binding on this ADR; ADR-0002 §6.2 pins atomic publish (temp file +
`fsync` + `rename`) and "a published file is never opened for writing again"; ADR-0002 §1.3 pins that
`meta.json` is written last, so *a run directory without `meta.json` means the server died mid-run*;
ADR-0002's "Open questions" hands `run_id` generation to this ADR and explicitly hands on-disk
retention of `.evidence/runs/` to the operator.

## 2. Decision

`MUST`, `MUST NOT` and `SHOULD` are normative. Every table here is copied verbatim into mact-f1w.11 and
mact-f1w.14 (the copies are in §17); nothing in this document is left open.

**D-1 — the call model is option B: supervisor-owned runs with poll and cancel, plus an in-call wait
mode.** A run is a detached tokio task owned by the `RunSupervisor`; the request that started it either
subscribes to its terminal transition (`wait: true`, default) or returns as soon as the run is
registered (`wait: false`). This is not "option A with extra tools": the ownership direction is the
decision, and `wait: true` is a *derived* convenience on top of it, which is exactly what ADR-0001 §4
guessed it would be.

**D-2 — the MCP Tasks extension is deferred, explicitly** (§12). It is the right eventual façade, it is
additive over this design, and it is not the baseline because a Tasks-only server is unusable to every
client that has not implemented the extension. §12 states the upgrade path and what must not change when
it lands.

**D-3 — no queue** (§8). One run per workspace, server-wide cap of two concurrent act processes, busy
answered immediately with the in-flight run's identity.

Everything below is the detail those three decisions force: the state machine (§3), timeouts (§4),
cancellation and teardown (§5), what happens to evidence when a run is cut short (§6), where run state
lives and how long it survives (§7), concurrency (§8), process lifecycle (§9), the client-facing poll
contract (§10), the configuration surface (§11), and the reconciliations with the accepted contracts
(§16).

## 3. The run state machine

### 3.1 Two levels on purpose

There are **seven non-terminal internal states** the supervisor moves through, plus the five terminal
ones, and **seven wire statuses**: `pending` and `running`, plus the five terminal words ADR-0002 shares
with `.evidence`. The split is deliberate: the internal states exist because teardown, extraction and
publication have different failure modes that an operator needs to see in a log; the wire statuses are
frozen because they are also `.evidence`'s vocabulary.

A run's state is written by exactly one task — the run's own supervisor task. Cancel and timeout do
**not** change state; they set a flag or fire an event that the run task samples (§3.5). This is the
rule that makes "cancel wins the race with act exiting" a testable question instead of a heisenbug.

### 3.2 States, and how each projects to the wire

| internal state | wire `status` | `phase` | meaning |
|---|---|---|---|
| `Queued` | `pending` | `queued` | registered: `run_id` allocated, workspace lock held, `.evidence/runs/<run_id>/` created. No process, and under D-3 none is queued behind anything — this is the short window between registration and the supervisor driving the run |
| `PreparingTree` | `pending` | `preparing_tree` | ADR-0003 tree materialisation for the target commit (instantaneous if ADR-0003 chooses a HEAD gate — see §3.6) |
| `ActStarting` | `pending` | `starting_act` | `exec` of `act` in flight; `started_at` is set when it returns |
| `ActRunning` | `running` | `running` | act is alive; deadline armed |
| `Cancelling` | `running` | `cancelling` | teardown initiated: SIGTERM → grace → SIGKILL → reap (§5.2). **Non-terminal**, and `phase` is the only place it shows up on the wire (§16.1) |
| `Harvesting` | `running` | `extracting_evidence` | act is reaped; the extractors for this `category` are running (§6) |
| `Publishing` | `running` | `publishing_evidence` | atomic evidence publish + `meta.json` write (§3.4) |
| `Succeeded` | `success` | `done` | act exited 0 |
| `Failed` | `failed` | `done` | act exited non-zero; `exit_code` present |
| `Cancelled` | `cancelled` | `done` | a caller took it down (§5.3) |
| `TimedOut` | `timed_out` | `done` | the `timeout_secs` deadline expired and the group was killed |
| `Error` | `error` | `done` | the server could not run the job to completion; `error.code` is one of the six `isError: false` runner codes from ADR-0001 §5.3 |

Terminal internal states **are** the five terminal wire statuses — one-to-one, no extra names.
`terminal: true` ⇔ one of those five, and the schema already enforces that agreement
(`run_view`'s `allOf`), so the projection cannot drift.

### 3.3 Transition table

`label` below is the outcome the run task carries into teardown; it is fixed by *who initiated* the
teardown, which is what keeps cancel / timeout / shutdown distinguishable even though they share the kill
machinery.

| # | from | to | trigger | label / outcome | `kill` |
|---|---|---|---|---|---|
| T1 | `Queued` | `PreparingTree` | supervisor picks the run up | — | — |
| T2 | `Queued` | `Cancelled` | cancel requested before spawn | `cancelled` | absent (`method: "none"`) |
| T3 | `Queued` | `Error` | pre-start budget expired (§4.4), or supervisor fault | `error` + `runner_internal_error` `detail.stage:"queued"` | absent |
| T4 | `PreparingTree` | `ActStarting` | tree ready | — | — |
| T5 | `PreparingTree` | `Cancelled` | cancel requested before spawn | `cancelled` | `method: "none"` |
| T6 | `PreparingTree` | `Error` | tree prep fails or pre-start budget expires | `error` + `runner_internal_error` `detail.stage:"preparing_tree"` | absent |
| T7 | `ActStarting` | `ActRunning` | `exec` succeeded; `started_at` set, deadline armed | — | — |
| T8 | `ActStarting` | `Error` | `exec` failed | `error` + `act_spawn_failed` | absent |
| T9 | `ActStarting` | `Cancelled` | cancel wins the race with `exec` | `cancelled` | `method: "none"` |
| T10 | `ActRunning` | `Harvesting` | child exit observed | `success` (exit 0) / `failed` (exit ≠ 0) / `error` + `act_crashed` (killed by a signal we did not send, not at the deadline) | — |
| T11 | `ActRunning` | `Cancelling` | `cancel_run`, or MCP cancellation of a waiting call | `cancelled` | `term` \| `kill` |
| T12 | `ActRunning` | `Cancelling` | `timeout_secs` deadline expired | `timed_out` | `term` \| `kill` |
| T13 | `ActRunning` | `Cancelling` | graceful shutdown sweep (§9.2) | `error` + `runner_internal_error` `detail.reason:"server_shutdown"` | `term` \| `kill` |
| T14 | `Cancelling` | `Cancelled` | reaped, and the initiator was a caller cancel → extraction skipped (§6.2) | `cancelled` | as above |
| T15 | `Cancelling` | `Harvesting` | reaped, and the initiator was the deadline or shutdown → extraction attempted | `timed_out` / `error` | as above |
| T16 | `Harvesting` | `Publishing` | records built (possibly zero) | label unchanged | — |
| T17 | `Harvesting` | `Error` | extraction hung past `--harvest-timeout-secs`, or the extractor panicked | `error` + `runner_internal_error` `detail.stage:"extracting_evidence"` | — |
| T18 | `Publishing` | label's terminal state | publish + `meta.json` succeeded | `success` / `failed` / `timed_out` / `error` | — |
| T19 | `Publishing` | `Error` | evidence or manifest write failed | `error` + `evidence_write_failed` \| `evidence_path_exists` \| `evidence_path_escape` | — |
| T20 | any terminal state | — | **none** | terminal is terminal | — |

Consequences of T20 worth stating: a `cancel_run` for a terminal run is answered from the stored state
(ADR-0001 §6.3: `already_terminal: true`, the *original* status, no `cancel_requested_at`, no `kill`),
never by a second transition; a late `run_status` always returns the same terminal `run_view`.

Terminal statuses are **`success`, `failed`, `cancelled`, `timed_out`, `error`** (ADR-0001 §3.2, byte-
identical to `.evidence`'s `run_status`). Non-terminal are **`pending` and `running`**. Nothing else
exists, and `succeeded` is not a status value anywhere in this project.

### 3.4 The terminal commit order

Exactly one code path reaches a terminal state, and it does these things in this order. The order is the
contract, because it buys the guarantee in the last line.

1. **Decide the outcome.** act's exit status (or the cancel/timeout/shutdown label) fixes the terminal
   status and `exit_code` / `error`.
2. **Harvest** — run the extractors for this `category`, bounded by `--harvest-timeout-secs` (T17 on
   expiry). Skipped entirely for a caller cancel (§6.2).
3. **Publish** — ADR-0002 §6.2 (temp file in the destination directory, `fsync`, `rename`, `fsync` the
   directory). Failure here is `evidence_write_failed` / `evidence_path_exists` / `evidence_path_escape`
   and the run still goes terminal (T19).
4. **Write `meta.json` last** (ADR-0002 §1.3). Every terminal run writes one, including a run cancelled
   before `act` was spawned. A `meta.json` write failure is retried once and then reported as
   `evidence_write_failed` — the already-published JSONL is **not** rolled back (§6.4).
5. **Transition in the registry** — one write under the registry lock: status, `finished_at`,
   `duration_ms`, `exit_code`, `evidence_files`, `evidence_record_count`, `warnings`, `error`,
   `phase: "done"`. `tracing::info` the transition (mact-f1w.11).
6. **Release resources** — ADR-0003 worktree cleanup and the workspace lock, the per-run secret file
   (ADR-0001 §2.7), the payload scratch dir per ADR-0002 §3.4.
7. **Notify waiters** — a `wait: true` request is released only now.

> **Guarantee:** no caller ever observes a terminal `run_view` whose evidence is not already durable. A
> result naming `.evidence/coverage/<sha>/<run_id>.jsonl` means that file exists, is complete, and has a
> sibling `meta.json`. A scanner can therefore treat "the run is terminal" and "the file is there" as
> the same question.

The cost of step 5 before step 6 is a short window in which the registry shows a terminal run while the
workspace lock is still held. That is fine and undocumented-by-design: `workspace_busy` is the answer a
second caller gets during it, and it is already the answer a second caller gets while a run is alive.

### 3.5 Races, pinned

- **Who may write state:** the run's own task only. `cancel_run`, the deadline timer and the shutdown
  sweep are *requests*: they set `cancel_requested_at` / fire an event / call `terminate(initiator)` on
  the runner. Any observable state change happens in the run task.
- **Where flags are sampled:** immediately after `exec` returns (T7/T8/T9), on the child-exit event
  (T10), and before extraction (T16). A cancel flag first observed at or after the child-exit event is
  a **no-op** — the run is past the cancel window (§5.4) — and is reported `already_terminal: true`.
- **Cancel vs. deadline in the same instant:** the first one the run task samples wins, and the label
  follows the winner. Both are non-error outcomes, so the distinction only affects which word a scanner
  filters on; the loser's request is answered from the stored terminal state (T20).
- **Cancel vs. `exec`:** T9 exists because `exec` is not instantaneous. If the cancel flag is set before
  `exec` is attempted, the spawn is skipped; if `act` is already spawned, the teardown signals it.
- **Two `cancel_run` calls for one run:** the second is served from state, never by a second teardown:
  `cancel_requested_at` is the *first* request's instant and is never overwritten, and `kill` is absent
  from the second reply because the first caller owns the teardown (ADR-0001 §6.3).
- **Duplicate transitions are impossible by construction**, not by discipline: there is one writer, and
  the state field is written only by the commit sequence in §3.4.

### 3.6 `phase` is display-only, and that is load-bearing

ADR-0001 §3.1 pins `phase` as something a client may display and nothing may branch on. This ADR
depends on that: whether `PreparingTree` is a visible state or a zero-duration hop is ADR-0003's
question (a HEAD gate makes it instantaneous; a worktree mode makes it seconds), and whether
`Cancelling` is observed at all depends on how fast act honours SIGTERM (mact-f1w.19). Both are
answerable without touching the wire contract *because* nothing branches on `phase`. The one internal
rule that is not display-only — "`Cancelling` is non-terminal" — is expressed as `status: running` +
`phase: cancelling`, and a client that polls `terminal` is unaffected either way.

## 4. Timeouts

### 4.1 What `timeout_secs` measures

`timeout_secs` is the **act process wall-clock budget**, measured from `started_at` (the instant `exec`
returned — the same value the result reports) to the moment the group is signalled. The deadline is
**absolute** (`deadline = started_at + timeout_secs`) and armed once as a `sleep_until`, so a
starved tokio worker cannot steal seconds from the run and a slow scheduler cannot extend it.

Pre-spawn work is *not* charged to it (§4.4), and neither is harvest/publish (§4.5). ADR-0001 §2.6's
"the same number bounds both" is therefore true up to a bounded, documented supervisor overhead
(§4.6) — the §16.4 reconciliation.

`timeout_secs` bounds: 1..=21600 by default, per ADR-0001 §9; a caller value outside the effective
range is `invalid_arguments` with `detail.errors[].expected` naming the effective range — never a
silent clamp (ADR-0001 §9's rule for caps).

### 4.2 Defaults per category (pinned)

| `category` | default `timeout_secs` | hard max | reasoning |
|---|---|---|---|
| `coverage` | **900** (15 min) | 21600 | a coverage job is build + tests; a coverage run still going at 15 minutes is almost always hung, and a hung job is worse than a missing number. Cheap to re-run with a bigger budget |
| `benchmark` | **1800** (30 min) | 21600 | benchmarks want samples, and killing a benchmark early truncates samples — but a `timed_out` benchmark still publishes what it collected (§6.3), so the generous default loses nothing that a timeout would have caught |
| anything else (shape-legal, unknown) | **1800** | 21600 | no extractor runs, so there is no "partial numbers" risk; the caller's own budget is the only meaningful one |

These are the values ADR-0001 §2.4 and §9 already published; they are restated here because this ADR
owns them and mact-f1w.14 must read them from here.

Coverage is deliberately shorter than benchmark: a coverage number is a property of a commit (it either
finishes or it does not), while a benchmark number is a property of a *sample count*, so the cost of a
longer budget is different in kind.

### 4.3 Per-category overrides (operator)

```
--default-timeout-secs <category>=<secs>   repeatable; '*' is the fallback
--max-timeout-secs <secs>                 default 21600, allowed 1..=86400
```

- Example: `--default-timeout-secs coverage=1200 --default-timeout-secs benchmark=3600
  --default-timeout-secs *=900`.
- `--max-timeout-secs` is the **effective ceiling** for both a caller's `timeout_secs` and any
  overridden default. ADR-0001 §2.6 allows this ceiling to be moved by an operator flag; pinned here:
  the ceiling is compiled with an outer limit of 86400 s (24 h) and the *advertised* `inputSchema`
  bound for `timeout_secs` is the effective value, so a client always learns the truth from
  `tools/list`. `schema/tool-contract.schema.json` keeps 21600 as the shipped default bound, and the
  `tool_schema_parity` test (ADR-0001 Verification) runs with default configuration. Widening the
  compiled 86400 limit is a contract change to ADR-0001 + schema + fixtures, not a flag.
- **Startup validation, fail fast:** every `--default-timeout-secs` value must be in
  1..=`--max-timeout-secs`; a default above the ceiling aborts startup with a message naming both
  values rather than being clamped. 24 h is inside `run_view.duration_ms`'s schema bound
  (259200000 ms = 72 h), so no widened timeout can overflow a duration field.
- A caller may always pass a `timeout_secs` **above** the category default, up to the ceiling — that is
  the point of the argument.

### 4.4 Pre-start budget

Registration → `exec` is bounded by `--prestart-timeout-secs` (default **120**): it covers a
`git worktree add`, the `exec` itself, and a momentarily slow container runtime. Expiry ends the run in
`Error` with `runner_internal_error` and `detail.stage` naming the phase (T3/T6). It is separate from
`timeout_secs` on purpose: a run that never started should not have to eat into the budget of the job
it never ran, and "it hung before act" and "the job was too slow" are different answers.

There is deliberately **no** tool-visible field for pre-start elapsed time (§16.5): `started_at` names
act's spawn, and `duration_ms` is omitted while `started_at` is absent. A caller that wants to know
what the server is doing before act starts reads `phase`.

### 4.5 Harvest budget

`Harvesting` + `Publishing` are bounded by `--harvest-timeout-secs` (default **120**). A hung lcov parse
or a `stat` on a hung network mount must not leave a run in `running` forever. Expiry is
`runner_internal_error` with `detail.stage: "extracting_evidence"` (T17).

### 4.6 Worst-case blocking time of a `wait: true` call

A `wait: true` call returns when the run reaches its terminal commit (step 7 of §3.4). The bound a
client may compute is:

```
timeout_secs                       (act budget, caller-chosen, ≤ 21600 by default)
  + prestart_timeout_secs   ≤ 120  (before act started)
  + term_grace_secs         ≤ 240  (SIGTERM grace; default 20 — §5.2)
  + harvest_timeout_secs    ≤ 120  (extraction + publish)
  ≈ timeout_secs + 300 s at defaults
```

So: **set your HTTP timeout to `timeout_secs + 300` at defaults**, or use `wait: false`. The default
`--term-grace-secs` of 20 keeps the real-world overshoot near 40 s; the 240 maximum is what keeps
`kill.took_ms` inside its schema bound of 300000 ms (§16.3).

`wait: false` returns as soon as step 5 of registration is done (run registered, `run_id` allocated,
supervisor tasked) — no act output, `status: "running"` per ADR-0001 §2.6, and the caller polls.

## 5. Cancellation and teardown

### 5.1 Three initiators, one mechanism

`terminate(initiator)` lives in the runner (mact-f1w.8): it is the only code in the codebase that
signals a process, and it returns a `KillReport { method: term | kill | none, took_ms }` that becomes
`cancel_run`'s `kill` field (mact-f1w.11 copies it verbatim; it does not invent it).

| initiator | entry point | label | extraction |
|---|---|---|---|
| `caller` | `cancel_run(run_id)`, or an MCP cancellation notification naming a `wait: true` request | `cancelled` | **skipped** (§6.2) |
| `deadline` | `timeout_secs` expiry | `timed_out` | attempted |
| `shutdown` | SIGINT/SIGTERM to the server (§9.2) | `error` + `runner_internal_error` | attempted, budget-bounded |

**Process-group kill, pinned.** act is spawned in its own process group (`process_group(0)` /
`setpgid`, per mact-f1w.8), and teardown signals the **group**, because act's own children — `docker`
clients, and containers it has started — are in it:

1. `killpg(pgid, SIGTERM)`. act's shutdown path is what removes the containers it started (`--rm` is
   always passed per mact-f1w.8 / ADR-0003), so a graceful act exits quickly.
2. wait up to `--term-grace-secs` (**20** s default, allowed 1..=240) for the group to exit.
   `KillReport.method = "term"`.
3. if it is still alive: `killpg(pgid, SIGKILL)`, then `waitpid` to reap.
   `method = "kill"`, and the run gets the warning `process_group_killed`.
4. `took_ms` = first signal sent → reaped. Bounded by the grace, hence the 240 maximum (§16.3).

`method` is **never absent when a process existed**. For a run cancelled before `exec` there was no
process, and `kill: {"method": "none"}` is reported (§16.2 — `cancel_run_result`'s `kill` description
used to say "absent" for that case and is corrected in this ADR's commit).

`PDEATHSIG` is adopted as defense-in-depth (`prctl(PR_SET_PDEATHSIG, SIGKILL)` in `pre_exec`), so a
`SIGKILL`ed act-mcp takes act down with it instead of orphaning it. mact-f1w.8 verifies empirically
that it fires under tokio's multithreaded runtime (it is tied to the spawning *thread*) and drops it if
it does not; the real guarantees remain §9.2 plus the runbook.

### 5.2 What `cancel_run` does, by state

| state at arrival | reply | notes |
|---|---|---|
| `Queued` / `PreparingTree` / `ActStarting` | `status: cancelled`, `terminal: true`, `already_terminal: false`, `cancel_requested_at`, `kill: {method: "none"}` | the spawn is skipped (T2/T5/T9) |
| `ActRunning` | `status: cancelled`, `terminal: true`, `kill: {method: "term" \| "kill", took_ms}` once reaped; `status: cancelled`, `terminal: false`, `kill` absent if teardown is still running when `--cancel-wait-secs` expires | the call blocks up to `--cancel-wait-secs` (**30** s default, must be ≥ `--term-grace-secs`, ≤ 300) for the terminal transition, then hands off to `run_status` |
| `Cancelling` | `status: cancelled`, `terminal: false`, `already_terminal: false`, `cancel_requested_at` = first request's instant, no `kill` | ADR-0001 §6.3; the first caller owns the teardown |
| `Harvesting` / `Publishing` | **no-op**: `status: running`, `terminal: false`, `already_terminal: false`, `cancel_requested_at`, no `kill`, warning `already_terminal` | act already exited; there is nothing to stop and the evidence is already determined (§5.4) |
| any terminal state | `already_terminal: true`, the original status, no `cancel_requested_at`, no `kill`, warning `already_terminal` | ADR-0001 §6.3: idempotent, never an error |
| unknown / evicted `run_id` | error `run_not_found` (`isError: true`, `retryable: false`) | §7.4 |

`cancel_run` is the only non-`run_status` tool that can block, and its block is bounded and shorter than
the act budget. It is `idempotentHint: true` per ADR-0001 §1.

### 5.3 MCP cancellation of a waiting call

ADR-0001 §4 pins the *meaning*: a cancellation notification arriving for a `wait: true` call means
"stop the work". The mechanism, in the stateless 2026-07-28 era:

- Each waiting request registers `request_id → run_id` in the supervisor's waiter map on entry and
  removes it when the wait ends (normally, by cancel, or by the request future being dropped).
- A cancellation notification resolves that map and calls `terminate(caller)` on the run. ADR-0001
  calls this `$/cancel-request`; **mact-f1w.5 confirms the exact method name and payload rmcp 3.9 uses,
  and mact-f1w.16 pins the observable behaviour against a running server** rather than this document
  guessing the wire spelling.
- A notification naming an unknown, already-finished, or already-`wait: false` request id is ignored.
  A notification cannot be answered, so ignoring it *is* the whole error handling.
- **Transport death is never cancellation.** A closed HTTP connection, a client that vanished, or a
  dropped request future removes a waiter and nothing else. This is the sentence that distinguishes B
  from A, and mact-f1w.11 tests it directly.

`run_status` polling stays the progress channel either way (§10): cancellation and polling are
independent, so a client that cancels by request id and polls by `run_id` never needs a live stream.

### 5.4 The cancel window closes when act exits

Once act has exited (`Harvesting`, `Publishing`) a cancel is a no-op. The reasoning, because it will be
re-litigated otherwise: act's exit already determined the outcome, the harvest is bounded at 120 s, and
discarding numbers the run already earned to satisfy a late "stop" throws away the only data the caller
paid minutes for. A late cancel is therefore *reported honestly* (`already_terminal: true` once terminal,
`status: running` with the `already_terminal` warning while harvesting) rather than honoured.

### 5.5 Container cleanup

- act is always invoked with `--rm` (mact-f1w.8 / ADR-0003), so a graceful SIGTERM teardown removes the
  run's containers, and `method: "term"` implies no orphans.
- After `method: "kill"`, containers may survive: act's own cleanup is exactly what SIGTERM was supposed
  to let it do. The run then carries the warning **`orphaned_containers`** (§16.6) and `meta.json`
  records `warnings` so the operator can find the run.
- act-mcp does **not** enumerate or force-remove containers behind act's back. It has no reliable
  per-run handle on them (there is no documented act label per run), and a wrong `docker rm -f` is a
  way to destroy someone else's container on a shared box. Instead: warn, and put the recipe in the
  runbook — **mact-f1w.19 pins the exact `docker ps` filter that identifies act-mcp's containers on
  real act**, and mact-f1w.18 publishes it with a bounded, operator-run cleanup command. A tool that
  guessed at container identity to be helpful here would be a destructive bug with better manners.

### 5.6 New warning codes

Two members are added to the **open** `warnings` vocabulary (ADR-0001 §5.4; additive, unknown codes are
never fatal, so nothing breaks):

| warning | meaning |
|---|---|
| `orphaned_containers` | SIGKILL was required and containers may have survived; see the runbook (mact-f1w.18) |
| `already_terminal` | a `cancel_run` arrived for a run that was already terminal or was past the cancel window; nothing was done |

`process_group_killed` (SIGKILL was needed) and `partial_evidence_dropped` (§6.2) already existed in the
ADR-0001 §5.4 list and are now defined, since this ADR owns the mechanics that produce them.

## 6. Evidence when a run is cut short

### 6.1 There is no such thing as half-written evidence

ADR-0002 §6.2 publishes each run's records with `rename()`, and `meta.json` is written last (§3.4 step
4). So a run in flight has **no** evidence file at all: either the file exists, complete and immutable,
or it does not exist. That collapses "what happens to already-written evidence when a run is cancelled"
into a much smaller question: **do we publish at all, and do we roll back.**

### 6.2 The policy

| terminal status | extraction attempted? | evidence published? | notes |
|---|---|---|---|
| `success`, `failed` | yes | yes | ADR-0002: a red run's numbers are data, stamped `failed` |
| `timed_out` | yes | yes if the payload survived | records carry `run_status: "timed_out"`; a partial payload is a *legitimate* measurement of "how far it got in 30 minutes", and the status is how a scanner filters it |
| `error` (`act_crashed`, `runner_internal_error`, evidence codes) | yes, when act was spawned and left a log/payload | if possible | extraction faults are warnings (ADR-0002 §5.1); *write* faults are the run failure T19 |
| `cancelled` | **no** | no | see below |
| `cancelled` before spawn | n/a | no | nothing could exist; `evidence_files: []`, `evidence_record_count: 0`, no warning needed |

**Caller cancel skips extraction, and that asymmetry is deliberate.** `timed_out` is something that
*happened to the run*; `cancelled` is something the *caller asked for*, and the plain meaning of "stop
this run" is "I do not want this measurement". Publishing half a benchmark's metrics under a status a
scanner has to remember to filter is the silent-pollution failure mode; discarding them is loud, cheap,
and reversible (re-run it). The escape hatch is documented in the tool description and §10: keep a slow
run's numbers by raising `timeout_secs`, or by using `wait: false` and simply not cancelling.

When a caller cancel tears down a run whose payload dir is non-empty, or whose publish temp file
(`.<run_id>.jsonl.*.tmp`, ADR-0002 §6.2) still exists, the staged payload and the temp file are removed
and the run carries **`partial_evidence_dropped`**. That is the whole content of that warning: *records
that could have been built were discarded because you cancelled.* It is rare, it always co-occurs with
`evidence_files: []`, and it never changes the status.

### 6.3 Payload scratch on every path

`.evidence/incoming/<run_id>/` is cleaned per ADR-0002 §3.4 on **every** terminal path, including a
cancel and a pre-spawn cancel: parsed payload files deleted, unparsed/unusable ones kept and listed in
`meta.json` with `bytes` and `sha256`, directory removed if empty. A cancelled run leaving scratch
behind forever is how a long-lived box fills its disk, and "scanners must not read `incoming/`" is only
safe if act-mcp keeps its own scratch bounded.

### 6.4 Published files are never deleted

Rollback of a *published* evidence file never happens on a cancel, a timeout, a shutdown, or a
`meta.json` failure. ADR-0002 §6.2 offers "rollback = delete the file" as a writer primitive; act-mcp
uses it only for its own `*.tmp` files. Once `rename()` has succeeded the file stands, is reported in
`evidence_files`, and is the run's permanent record. If `meta.json` is what failed, the run is terminal
`error` + `evidence_write_failed`, the JSONL stays, and ADR-0002 §1.3's rule ("no `meta.json` ⇒ suspect
that run's directory") does the honest work of flagging it. Deleting published numbers to keep a
manifest consistent would destroy the run's whole purpose to fix an audit file.

## 7. Run state: what is stored, where, and for how long

### 7.1 What a `RunHandle` holds

Registry entry per run (in memory, behind the `RwLock<HashMap<RunId, RunHandle>>` of mact-f1w.11):
the identity tuple (`workflow`, `workflow_file`, `task`, `category`, `commit`), internal state +
`phase`, `cancel_requested_at`, `deadline`, `registered_at`, `started_at`, `finished_at`, `exit_code`,
the pending label, the pgid (for the teardown), the **bounded log-tail ring buffer**, the published
`evidence_files` + record count, accumulated `warnings`, the terminal `error`, and the wait
subscription (one `tokio::sync::watch`-style notification per terminal transition).

Two bounds matter:

- **Log tail ring buffer: `--log-tail-ring-bytes` = 65536 per run**, which is exactly `log_tail_bytes`'s
  maximum (ADR-0001 §9). A requested tail is therefore always servable from memory; `run_status` never
  reads `act.jsonl` off disk to answer a tail, which is what lets it stay non-blocking (ADR-0001 §6.2).
- **On-disk log cap: `--max-run-log-bytes` = 67108864 (64 MiB) per run.** The runner
  truncates-with-marker at the cap, sets `act.log_truncated`, and adds the ADR-0002 `truncated` warning.
  act's `--json` stream on a matrix job is not small, and a run that fills the disk takes the box and
  every other run with it.

### 7.2 Run state is memory-only, by decision

Nothing about run state is persisted. There is no `runs.sqlite3`, no write-ahead log of run states, no
`.evidence/runs/<run_id>/state.json`. ADR-0002 already *is* the durable record — `meta.json` plus the
JSONL — and a second, partial copy of the same facts in a place nobody validates is a drift generator,
not a feature.

Consequences, stated plainly rather than papered over:

- after a restart, `run_status` listing mode returns `runs: []`, `active_count: 0`, `truncated: false`
  (an empty listing is **not** an error), and every previously issued `run_id` yields `run_not_found`;
- `idempotency_key` memory is gone, so a replay after a restart starts a fresh run (ADR-0001 §8 already
  documents this);
- act processes that were mid-flight when the server was `SIGKILL`ed are not adopted (§9.3).

### 7.3 Retention (in memory)

| knob | default | meaning |
|---|---|---|
| `--run-retention-count` | **200** | finished runs kept in the registry |
| `--run-retention-mins` | **120** | a finished run may be evicted once it is older than this |

- A finished run is **eligible** when it is older than the age limit *or* when the number of retained
  finished runs exceeds the count limit; eviction then removes the oldest-finished first, one at a time,
  until the entry fits. Whichever limit bites first.
- A non-terminal run is **never** evicted, no matter how long it runs (its own `timeout_secs` is what
  ends it, not the retention policy).
- `idempotency_key → run_id` entries are evicted **with** their run, never before it: a key that
  outlives its run would replay into `run_not_found`, which is worse than replaying into a fresh run.
- Memory bound at defaults: ≤ (200 + 2 active) × (~1 KiB record + 64 KiB tail) ≈ **13 MiB**, and it is
  a hard bound because both multipliers are configured, not observed.
- `run_status` listing (cap 50, newest first, ADR-0001 §6.2) is drawn from this set, so `truncated:
  true` means exactly "the page or the retention window dropped older finished runs".
- `.evidence/runs/<run_id>/` is **not** pruned by act-mcp — on disk, forever, at every retention
  setting. That is ADR-0002's open question answered where it was left: disk pruning is an operator
  cron, and deleting `runs/<run_id>/` older than N days is safe *because* the JSONL contract does not
  live there.

### 7.4 An unknown `run_id`

Both `run_status(run_id)` and `cancel_run(run_id)` answer **`run_not_found`** (`isError: true`,
`retryable: false`) for a run this process does not have — never seen, or evicted, or from before a
restart. `error.detail` is `{run_id, known_runs}` where `known_runs` is up to the **20** newest
`run_id`s known to this process, newest first (`[]` after a restart). The message says the durable
trace is `.evidence/runs/<run_id>/` and that `run_status` does not read it (ADR-0001 §5.3, §6.2).

Why not read the disk to be helpful: `run_status`'s contract is that it never blocks and never starts
anything; answering from `.evidence/runs/` would mean parsing `meta.json` per call, would mix "this run
is live and cancellable" with "this run happened", and would make a `cancel_run` on a days-old run
lookable-but-meaningless. A caller that has lost its `run_id` uses listing mode; a caller that has the
`run_id` and a restarted server reads the file it was told about. Same answer, no ambiguity.

## 8. Concurrency and the workspace lock

- **One run per workspace, no queue.** A second `run_workflow` while the lock is held is
  `workspace_busy` immediately, with the in-flight `run_id`, `phase` and `started_at` in `detail`
  (ADR-0001 §5.3). mact-f1w.11 asks whether this ADR wants a bounded FIFO instead: **no.** A queued
  benchmark is a measurement of whatever HEAD is when the queue drains (ADR-0003's problem, inherited
  silently), a queued caller has no idea how long it waits, and the caller already gets a useful answer
  (`here is the run that is going — poll it or cancel it`). `phase: "queued"` therefore denotes the
  short pre-spawn window, never a queue position, and `--max-concurrent-runs` is a *limit*, never a
  semaphore anyone waits on.
- **Server-wide cap: `--max-concurrent-runs` = 2** (ADR-0001 §9), checked at registration; exceeding it
  is `too_many_runs` (retryable), never a wait.
- **Lock lifetime:** taken by the supervisor at registration (ADR-0001 §2.9 step 9) and released at step
  6 of §3.4 — after evidence is published and `meta.json` is written, before waiters are notified. A
  request task never holds or releases the lock: a dropped HTTP connection cannot release it early, which
  would let a second run into a workspace whose worktree the first run is still using.
- **A dead holder must not wedge the workspace.** The lock is a file lock whose liveness is the owning
  process (mact-f1w.11's stale-lock-with-warning path); a crashed act-mcp releases it with the process.
  ADR-0003 owns the mechanism; this ADR only pins that `workspace_busy` must be resolvable by killing
  the holder named in `detail.run_id`, and that act-mcp does not add a second lock in the evidence
  layer (ADR-0002 §6.3 already says so).
- `list_tasks` takes no lock (ADR-0001 §6.1), so it remains the cheapest way to probe a busy workspace.

## 9. Process lifecycle

### 9.1 Startup

`serve` binds loopback, checks nothing expensive, and is ready to answer `tools/list`. No
persisted registry to reload (§7.2). The environment probes (act, docker) stay lazy and cached ≤ 30 s
per ADR-0001 §2.9 step 7, so a box without docker still starts and still answers `list_tasks`.

### 9.2 Graceful shutdown (SIGINT / SIGTERM)

1. Stop accepting new runs. A `run_workflow` after this point is `internal_error` (`isError: true`,
   `retryable: true`) with `detail.reason: "shutting_down"` — the closed error set has no
   "shutting_down" code and inventing one for a sub-second window is not worth a contract change;
   `retryable: true` is the right advice, because after the restart the call may well succeed.
2. `terminate(shutdown)` on every active run **concurrently** — separate process groups, so wall time is
   one grace period, not N of them.
3. Wait up to `--shutdown-grace-secs` (**30**) for the terminal commits, including evidence publish and
   `meta.json` for the runs that get there.
4. `SIGKILL` the stragglers' groups, then exit. A run that did not reach step 4 of §3.4 leaves no
   `meta.json`, which is precisely ADR-0002 §1.3's documented meaning ("the process died mid-run") — the
   file layout already distinguishes a clean shutdown from a hard one, and inventing a third state would
   make it less trustworthy, not more.
5. Waiting `wait: true` requests are released by their runs' terminal commits and answer normally.

mact-f1w.11's acceptance ("no orphaned stub-act processes after shutdown") is the test for steps 2–4.

### 9.3 Startup reconciliation after a hard kill

On startup act-mcp logs one `WARN` line per `.evidence/runs/<run_id>/` with no `meta.json` newer than
the last shutdown: *"run <id> has no manifest; it was interrupted, its act process may have survived"*.
Log line, not a tool surface, and not a `warnings[]` member (those describe a run this process ran).
It does not kill, adopt, or delete anything it did not start. What it *must* do is keep the workspace
lock usable, which §8 covers by construction (the lock dies with the process).

## 10. The poll contract (what a client is expected to do)

```
res = run_workflow(workflow, task, category, commit)            # wait: true, timeout_secs = T
  → if the client's own budget < T + 300: use wait: false instead

res = run_workflow(..., wait: false)                           # → run_id, status: running
loop:
    s = run_status(res.run_id, log_tail_bytes = 0)             # 0 = no log; cheapest poll
    if s.run.terminal: done
    sleep(interval); interval = min(interval * 1.5, 15 s)      # start 1 s
    if now > expected_deadline + 300 s: cancel_run(res.run_id); bail
```

- `run_status` never blocks, never starts work, and answers from memory (§7.1). A 1 s poll is
  ~nothing; a 15 s poll on a 40-minute benchmark is noise. `active_count` in the same reply is the
  cheap "is anything still going" check when a caller has lost its ids entirely.
- `terminal` is the only field a poller needs (ADR-0001 §3.1 keeps it redundant with `status` for
  exactly this reason). Nothing may branch on `phase` (§3.6).
- **`log_tail_bytes: 0`** is the cheap-poll argument, and `duration_ms` while running is the
  "is it making progress" number (absent before act starts — §4.4).
- A caller that wants a *guaranteed* single round trip uses `wait: true` with a client timeout of
  `timeout_secs + 300` (§4.6). A caller that cannot hold a connection uses `wait: false` + polling.
  There is no third option in v1, and the Tasks extension (§12) would not change this for clients that
  do not have it.
- On `run_not_found` after a restart: read `.evidence/runs/<run_id>/meta.json` if you need the answer,
  or re-run with the same `idempotency_key` and accept a fresh run (ADR-0001 §8).

## 11. Configuration surface

Every default this ADR owns, in one table, so mact-f1w.5 can paste it and mact-f1w.13 has one list to
review. All are clap flags on `serve`.

| flag | default | bound / validation | owner of the mechanism |
|---|---|---|---|
| `--max-concurrent-runs` | 2 | ≥ 1 (server-wide act processes) | supervisor (.11) |
| `--default-timeout-secs <cat>=<secs>` | coverage 900, benchmark 1800, `*` 1800 | 1..=`--max-timeout-secs`, else startup aborts | supervisor (.11), read by .14 |
| `--max-timeout-secs` | 21600 | 1..=86400 (compiled outer limit); advertised as the `timeout_secs` bound | validator (.6), advertised by .5 |
| `--prestart-timeout-secs` | 120 | ≥ 1 | supervisor (.11) |
| `--term-grace-secs` | 20 | 1..=240 (keeps `kill.took_ms` ≤ 300000) | runner (.8) |
| `--harvest-timeout-secs` | 120 | ≥ 1 | supervisor (.11) |
| `--cancel-wait-secs` | 30 | `--term-grace-secs`..=300 | .11 |
| `--shutdown-grace-secs` | 30 | ≥ 1 | supervisor (.11) |
| `--run-retention-count` | 200 | ≥ `run_status` listing cap (50) | registry (.11) |
| `--run-retention-mins` | 120 | ≥ 1 | registry (.11) |
| `--log-tail-ring-bytes` | 65536 | = `log_tail_bytes` max; raising one raises the other | runner (.8) |
| `--max-run-log-bytes` | 67108864 | ≥ 1 MiB | runner (.8) |

Interaction rules: `--max-timeout-secs` bounds both caller args and per-category defaults (§4.3);
`--cancel-wait-secs` ≥ `--term-grace-secs` so the common `cancel_run` returns a terminal receipt rather
than `terminal: false`; `--run-retention-count` ≥ 50 so a full listing never implies an eviction that
did not happen.

## 12. The MCP Tasks extension: deferred, with an upgrade path

**Deferred, explicitly.** Option C — the Tasks extension (SEP-1686 / SEP-2663, 2026-07-28 era) — is the
right eventual façade for multi-minute work and it is not this server's baseline, for one reason: a
Tasks-shaped server is unusable to every client that has not implemented the extension, and act-mcp's
primary consumer is whatever agent is pointed at it today. Options A and C both make the server's
reach a function of client sophistication; B works with a client that can POST JSON.

The deferral is cheap because **B is the thing C would have to wrap anyway**:

| Tasks concept | already exists in this design |
|---|---|
| submit returns a handle immediately | `wait: false` → `run_id` |
| poll for state / progress | `run_status(run_id)` → status, phase, elapsed, tail, evidence so far |
| cancel a task | `cancel_run(run_id)` |
| fetch the final result when ready | the terminal `run_view` — identical payload, no second shape |
| task storage / lifetime | §7's registry + `.evidence/runs/<run_id>/` as the durable half |

**Upgrade path (additive, no migration):**

1. Confirm rmcp 3.9's actual surface at scaffold time — the crate's `examples/servers/src/task_stdio.rs`
   and its task manager are the reference; mact-f1w.5 records the exact trait/type names and which
   Tasks methods (submit / get / result / list, and any notifications) the version in `Cargo.lock`
   implements, and mact-f1w.16 adds negotiation coverage for them. **No behaviour in this ADR depends on
   that answer**, which is the point of deferring rather than guessing.
2. Add the Tasks surface as a thin adapter: one task per run, `taskId` carrying or mapping to `run_id`,
   task state a projection of §3.2's internal state, task result the *same* `run_view` JSON. The
   supervisor is unchanged; `run_workflow` is unchanged; the four tools stay exactly as ADR-0001 pins
   them, because they are the contract and the fixtures prove them.
3. Tasks then buys what B cannot: a Tasks-aware client gets the outcome without holding a request open
   and without a bespoke poll loop, and a server that later grows SSE streams (`json_response = false`)
   gets a standard place to announce progress.

**What must not change when C lands:** the state machine and terminal vocabulary (§3), timeout defaults
(§4), the cancellation mechanics and the cancel-window rule (§5), the evidence policy (§6), retention
and `run_not_found` (§7), single-flight (§8). A Tasks layer that lets a caller bypass any of those is a
second lifecycle, and a second lifecycle is the thing this ADR exists to prevent.

**Trigger to pick it up:** a real consumer that needs server-push progress or result retrieval after a
dropped connection *without* `idempotency_key` replay. Until then, the poll loop in §10 is the feature.

## 13. Verification

All of it runs without docker, against the stub `act` of mact-f1w.15.

| check | owner | how |
|---|---|---|
| run survives the request future being dropped | .11 | drop the caller's `wait: true` future mid-run; `run_status` still reports `running`, the stub act still finishes, evidence still published (ADR-0001 §4's rule, made a test) |
| every transition in §3.3 reachable and terminal states final | .11 | table-driven test; a `cancel_run` on a terminal run returns `already_terminal: true` and changes nothing |
| `Cancelling` is non-terminal on the wire | .11 | stub act that ignores SIGTERM: `run_status` mid-teardown is `status: "running"`, `phase: "cancelling"`, `terminal: false`; then `cancelled` + `terminal: true` |
| process-group kill really kills children | .8 | stub act that spawns a child which outlives it; assert the child is gone and `method: "kill"` + `process_group_killed` after the grace |
| grace honoured: SIGTERM-compliant stub → `method: "term"`, no SIGKILL | .8 | assert `took_ms` ≪ grace and no `process_group_killed` |
| timeout kills and labels `timed_out` | .8 / .11 | stub sleeps past `timeout_secs=1`; terminal `timed_out`, `kill.method` present, extraction attempted |
| cancel-before-spawn yields `kill.method: "none"` | .11 | cancel the run between registration and spawn; no process ever existed |
| cancel after act exited is a no-op | .11 | cancel during a slow-harvest stub → `already_terminal: true` once terminal, `status: running` + warning `already_terminal` while harvesting, evidence still published |
| caller cancel publishes nothing | .11 | stub writes a payload then hangs; cancel → `evidence_files: []`, `partial_evidence_dropped`, payload scratch cleaned |
| `timed_out` publishes partial evidence | .10 / .11 | stub writes `lcov.info` then sleeps; JSONL exists with `run_status: "timed_out"`, `meta.json` exists |
| pre-start budget, harvest budget | .11 | stub/fault injection: hang tree prep and extraction; `error` + `runner_internal_error` with the right `detail.stage` |
| retention eviction | .11 | small `--run-retention-count`; oldest finished evicted first; a non-terminal run is never evicted; `truncated: true` in the listing; `run_not_found` for an evicted id |
| unknown `run_id` | .11 | `run_not_found`, `isError: true`, `retryable: false`, `detail.known_runs` ≤ 20; empty listing mode is not an error |
| no result before its evidence is durable (§3.4 guarantee) | .14 / .15 | after every e2e call with a non-empty `evidence_files`, assert the file and `meta.json` exist on disk at the moment the result is returned |
| shutdown kills groups, writes manifests where it can, leaves no stub-act | .11 | SIGTERM the server mid-run; assert no surviving process, `error` + `detail.reason: "server_shutdown"`, `internal_error` for a call made after step 1 |
| worst-case wait bound (§4.6) | .11 / .15 | fault-injection: prestart hang + slow teardown; `wait: true` returns within `timeout_secs + 300` at defaults |
| cancellation notification maps to the run | .16 | conformance suite against the running server (also settles ADR-0001's `$/cancel-request` spelling, §5.3) |
| §11 defaults equal the clap defaults | .5 | unit test parsing the built CLI defaults and comparing to this table (the same pattern as ADR-0001's `tool_schema_parity`); `scripts/validate_run_lifecycle.py` already checks the ADR against its own copy of the table |
| the §16 schema claims stay true (mid-kill is unrepresentable in `run_view`, §5.2's rows validate) | .3 (this ADR), then .17 CI | `python3 scripts/validate_run_lifecycle.py --stats` |
| timeouts, `kill.took_ms`, `duration_ms` stay inside their schema bounds at every allowed config | .5 / .17 | static assertion + the §4.3 / §11 startup validations |

## 14. Consequences

**Good**

- A 40-minute run is a `run_id`, a poll and a cancel button. Nothing about the HTTP layer can end it,
  and nothing about the HTTP layer is needed to watch it.
- `wait: true` keeps the common agent case one round trip, so option B does not cost a normal caller
  anything: the two-call shape is opt-in, not compulsory.
- One lifecycle, one writer, one terminal commit. "Did my run finish, and is the file there?" is one
  question with one answer (§3.4's guarantee), which is the property the downstream scanner actually
  needs.
- Cancellation is honest about what it can and cannot stop: it reaches containers via the process group,
  it refuses to pretend after act has exited, and it does not delete the numbers a run already earned.
- Every budget is a number in one table (§11), each with a failure mode next to it, and each inside a
  bound the schema already states — so no flag can produce a result the contract rejects.
- The Tasks extension stays additive. Choosing B now costs the future Tasks layer an adapter, not a
  rewrite, because B *is* the task supervisor Tasks would wrap.
- The state vocabulary is unchanged: zero new statuses, zero new error codes, two new members in an
  explicitly open vocabulary.

**Costs**

- Three moving parts instead of one request handler: a supervisor task, a waiter subscription, and a
  registry with a retention policy. A blocking-only server would have had none of these, and would have
  shipped a server that loses runs when a laptop sleeps.
- `cancel_run` may block for up to `--cancel-wait-secs`, so it is the one control tool with a non-trivial
  latency (bounded, and shorter than anything act does).
- A mid-kill run shows as `running` + `cancelling` in `run_view` while `cancel_run` itself says
  `cancelled` / `terminal: false` (§16.1). Two truthful views of one moment; the reconciliation is
  written down rather than left to be discovered.
- Memory is bounded by configuration rather than by observation (§7.3) — the bound is only as trustworthy
  as the ring-buffer size, which is why that number is pinned instead of "a few KB".
- Post-restart amnesia (§7.2) is a real hole a caller can fall into. It is documented, the durable
  artifacts answer the question the caller really has, and the alternative (a persisted run registry)
  would put a second, weaker copy of the evidence contract somewhere nobody validates.
- Server-wide concurrency of 2 with no queue means a burst of calls gets `workspace_busy` /
  `too_many_runs` rather than eventually succeeding. Explicit, immediate, and retryable-by-advice.
- Two warning codes are new, and anything that prints warnings verbatim will show words it has never
  seen. That is what "open vocabulary, unknown codes ignored" is for.

## 15. Considered alternatives

- **A — blocking-only `run_workflow` with a caller-tunable timeout and MCP cancellation.** Rejected,
  and ADR-0001 §4 already committed to rejecting it: a dropped request takes a 38-minute run and its
  containers down with it, and there is no way to watch or stop the run from elsewhere. Its one genuine
  advantage — one tool, one shape, no registry — is bought by losing work, which is the one thing a
  benchmark harness must never do. `wait: true` gives A's ergonomics with none of that failure mode.
- **C — the MCP Tasks extension as the baseline.** Rejected for now (§12): correct end state, but only
  Tasks-aware clients could use the server at all, and it would put rmcp-version-specific behaviour on
  the critical path before mact-f1w.5 has even confirmed the API surface. It is deferred *with an
  upgrade path and a trigger*, not shelved.
- **A bounded FIFO queue per workspace** (mact-f1w.11 explicitly leaves this to this ADR). Rejected
  (§8): a queued run silently re-targets whatever HEAD is when it drains, the caller cannot tell
  "queued 5 s" from "queued 40 min", and `workspace_busy` with the in-flight `run_id` already gives the
  caller a better answer than a number in a line.
- **A sixth wire status `cancelling`.** Rejected: `$defs/status` is a closed enum shared with
  `.evidence`'s `run_status` and `run-meta.schema.json`, the checker fails any disagreement between the
  three files, and "in teardown" is not a run outcome anyone downstream should key on. `phase:
  "cancelling"` on a non-terminal `running` says it, and `phase` is already declared display-only.
- **Persisting run state** (sqlite, or a `state.json` under `.evidence/runs/<run_id>/`). Rejected
  (§7.2): `meta.json` *is* the persisted terminal state, written last so it cannot lie; a live mirror
  would be a second copy that can disagree with the evidence contract and would have to be validated as
  strictly, for no capability a caller lacks after a restart.
- **Rolling back published evidence on cancel.** Rejected (§6.4): a published file is immutable by
  contract, and deleting data is a strictly worse fix for a failed manifest than flagging the run.
- **Publishing partial evidence for caller-cancelled runs** (as `timed_out` does). Rejected with the
  asymmetry written down (§6.2): cancel is caller intent ("do not want this measurement"), timeout is a
  run outcome about the commit, and a scanner silently full of half-benchmarks is the pollution we
  refused.
- **Clamping a caller's `timeout_secs` to the effective ceiling.** Rejected: ADR-0001 §9 pins that
  exceeding a cap is `invalid_arguments`, never a truncation surprise. A run that silently got less time
  than it asked for is a benchmark that lies.
- **Charging pre-spawn time to `timeout_secs`.** Rejected (§4.4): it makes a run's fate depend on
  `git worktree add`, and makes `duration_ms` mean two different intervals depending on how slow the
  machine was that second.
- **`SIGKILL` immediately, no grace.** Rejected: act's own cleanup is what removes the containers, and
  SIGTERM is the only chance it gets. The grace is bounded and the escalation is unconditional, so the
  no-grace case (a 20 s wait for a hung act) is already the worst case.
- **Using act's `--rm` as the only container cleanup story and reporting nothing on the `kill` path.**
  Rejected: that is exactly the case where `--rm` did not run. Hence `orphaned_containers` + a pinned
  runbook recipe (mact-f1w.19 / .18).
- **Having act-mcp force-remove leftover containers itself.** Rejected (§5.5): no reliable per-run
  handle exists, and a wrong filter on a shared box destroys someone else's containers. Warn and
  document instead.
- **Owning the child from the request task with `kill_on_drop(true)` as the cancel mechanism.**
  Rejected: that is option A with extra steps — the whole point of B is that the child belongs to the
  supervisor and a vanished request changes nothing. (`kill_on_drop` stays on as a last-resort safety net
  inside the run task, which is where it is harmless.)
- **Letting `run_status` fall back to reading `.evidence/runs/` for an unknown id.** Rejected (§7.4): it
  would make `run_status` block, and would blur "live and cancellable" into "happened once".
- **A `shutting_down` error code.** Rejected (§9.2): the closed set is a maintenance obligation
  (ADR-0001 §10) and one sub-second window does not justify paying it; `internal_error` +
  `retryable: true` + `detail.reason` is honest.
- **A `duration_ms` measured from registration while `pending`.** Rejected (§16.5): it needs a second
  origin clock that no field names, and ADR-0001 already says duration is omitted when act never
  started.

## 16. Reconciliations with the accepted contracts

Every item here was checked against ADR-0001, ADR-0002 and the three schema files while writing this
document. None of them changes a wire shape; three of them change prose or a `description` string, and
those edits are part of this ticket's commit so nothing is left as a rumour. Items 1, 2, 3, 5 and 8 are
asserted by `scripts/validate_run_lifecycle.py`, so if a schema is relaxed later this checker notices
instead of quietly making the prose wrong.

1. **The mid-kill window in `run_view`.** ADR-0001 §6.3 says a second `cancel_run` for a run that is
   mid-kill sees `status: "cancelled"` with `terminal: false`. `cancel_run_result` can express that and
   does. `run_view` cannot: its `allOf` forces `terminal: true` whenever `status` is one of the five
   terminal values (machine-checked today by `run_status.result.terminal-flag-disagrees-with-status`).
   Pinned: **`run_status` reports the mid-kill window as `status: "running"`, `phase: "cancelling"`,
   `terminal: false`; `cancel_run` reports `status: "cancelled"`, `terminal: false`.** `run_status` is
   the polling surface, `cancel_run`'s payload is a receipt for the request. ADR-0001 §6.3 gets a
   parenthetical saying so.
2. **`kill.method: "none"` for a pre-spawn cancel.** `cancel_run_result.kill`'s description said
   "absent for a queued run that never started a process", while ADR-0001 §6.3's prose says a queued
   cancel reports `kill.method: "none"` — and `"none"` exists in the enum for exactly that case. Pinned:
   **present, `{"method": "none"}`, for any run cancelled before `exec`**; the schema description is
   corrected to say "absent for an `already_terminal` run".
3. **`--term-grace-secs` ≤ 240** because `kill.took_ms` is schema-bounded at 300000 (grace + signal
   delivery + reap must fit inside the bound the fixture-visible field cannot exceed).
4. **`wait: true` blocking bound.** ADR-0001 §2.6's "the same number bounds both" is implemented as
   `timeout_secs` + a bounded, documented supervisor overhead (§4.6: ≤ 300 s at defaults), because the
   budget starts at act's spawn and teardown has to publish evidence before it reports (T-steps in
   §3.4). "A `wait: true` result is always terminal" holds unchanged.
5. **`duration_ms` while `pending`.** ADR-0001 §3.1 offers "elapsed-so-far while `pending`/`running`";
   `run_view` also says duration is omitted when act never started. Pinned: `duration_ms` is
   `finished_at - started_at` when act spawned and **absent otherwise**, including while `pending` —
   there is no named origin clock for pre-spawn elapsed time, and `phase` is the answer to "what is it
   doing before act".
6. **Two new `warnings` members** (`orphaned_containers`, `already_terminal`), appended to the open
   vocabulary list in ADR-0001 §5.4 and `docs/tool-contract.md` §7. `partial_evidence_dropped` and
   `process_group_killed` already existed and are defined here (§5.6, §6.2).
7. **`--max-timeout-secs`.** ADR-0001 §2.6 allows the 21600 bound to be moved by an operator flag; this
   ADR pins the flag, a compiled outer limit of 86400 s, and that the *advertised* `inputSchema` bound is
   the effective value, so client-side validation and server-side validation always agree.
   `schema/tool-contract.schema.json` keeps 21600 (the default), and `tool_schema_parity` runs with
   default configuration.
8. **`phase` values are exactly the eight in `$defs/phase`.** ADR-0001 §3.1 calls it an "open enum",
   which the schema spells as a closed list plus "unknown values are ignored". Read the schema: this ADR
   adds **no** phase member, and nobody should invent one on the strength of that prose. What "nothing may
   branch on `phase`" buys is §3.6 — phases may be instantaneous or slow without the contract changing.
9. **Nothing from ADR-0002 is amended.** §6 uses its publish protocol, its scratch cleanup and its
   `meta.json`-last rule as given; §7.2 declines the retention question it handed over rather than
   answering it.

## 17. Copy verbatim into the wiring tickets

mact-f1w.11 (supervisor + `run_status` / `cancel_run`) and mact-f1w.14 (`run_workflow` wiring) take
these blocks as written. If an implementation finds one of them unimplementable, the ADR is amended first
— the tickets do not improvise a state, a default or a warning.

### 17.1 State machine (both tickets)

```
NON-TERMINAL internal states → wire projection (status / phase)
  Queued         pending / queued            registered, run_id allocated, workspace lock held, run dir created
  PreparingTree  pending / preparing_tree    ADR-0003 tree materialisation
  ActStarting    pending / starting_act      exec in flight; started_at set when it returns
  ActRunning     running / running           act alive, deadline armed
  Cancelling     running / cancelling        SIGTERM → grace → SIGKILL → reap  (NON-TERMINAL)
  Harvesting     running / extracting_evidence
  Publishing     running / publishing_evidence

TERMINAL (wire status == internal state; phase "done")
  success | failed | cancelled | timed_out | error     ← the five .evidence run_status words

TRANSITIONS
  Queued        → PreparingTree | Cancelled | Error
  PreparingTree → ActStarting  | Cancelled | Error
  ActStarting   → ActRunning   | Cancelled | Error(act_spawn_failed)
  ActRunning    → Harvesting(exit observed: success|failed|error+act_crashed)
                | Cancelling(cancel | deadline | shutdown)
  Cancelling    → Cancelled (caller cancel: extraction skipped)
                | Harvesting (deadline/shutdown: extraction attempted) label timed_out | error
  Harvesting    → Publishing | Error(runner_internal_error stage=extracting_evidence)
  Publishing    → the label's terminal state | Error(evidence_* )
  terminal      → nothing (T20: a terminal run never transitions; cancel on it is already_terminal)

RULES
  * only the run's own task writes state; cancel/timeout/shutdown set flags or call terminate(initiator)
  * flags sampled after exec returns, on child exit, and before extraction; a cancel first seen at or
    after child exit is a no-op → already_terminal
  * terminal commit order (one code path): outcome → harvest → publish JSONL → meta.json → registry
    write → release lock/worktree → notify waiters
  * guarantee: no terminal run_view is observed before its evidence + meta.json are on disk
```

### 17.2 Defaults (mact-f1w.14 reads these, mact-f1w.5 ships them)

```
timeout_secs defaults   coverage 900 · benchmark 1800 · any other category 1800
timeout_secs bounds     1 ..= 21600 default ceiling (--max-timeout-secs, 1..=86400 compiled limit)
per-category override   --default-timeout-secs <cat>=<secs> repeatable, '*' = fallback;
                        startup aborts if a default > the ceiling
pre-start budget        --prestart-timeout-secs 120        (registration → exec)
act budget              timeout_secs, absolute from started_at (act spawn), one sleep_until
harvest budget          --harvest-timeout-secs 120         (extract + publish)
term grace              --term-grace-secs 20  (1..=240, because kill.took_ms ≤ 300000)
cancel_run block        --cancel-wait-secs 30 (≥ term grace, ≤ 300)
shutdown grace          --shutdown-grace-secs 30
concurrency             --max-concurrent-runs 2 · 1 per workspace · NO QUEUE
retention (memory)      --run-retention-count 200 · --run-retention-mins 120 · never a non-terminal run
log tail in memory      --log-tail-ring-bytes 65536 (= log_tail_bytes max)
log on disk             --max-run-log-bytes 67108864, truncate-with-marker + act.log_truncated + warning truncated
max wait for wait:true  timeout_secs + ~300 s at defaults
```

### 17.3 Cancellation (mact-f1w.8 implements, .11 exposes)

```
initiators: caller (cancel_run | MCP cancellation of a waiting request) → cancelled
            deadline (timeout_secs)                                      → timed_out
            shutdown (SIGINT/SIGTERM)                                    → error + runner_internal_error
                                                                         detail.reason "server_shutdown"
mechanism: process group (process_group(0)) — killpg SIGTERM → --term-grace-secs → killpg SIGKILL → waitpid
           KillReport{method: term|kill|none, took_ms}; "none" = cancelled before exec
           method "kill" ⇒ warning process_group_killed, and orphaned_containers is possible → runbook
window:  cancellable while act is unspawned or alive; a cancel first observed at/after child exit is a
         no-op (already_terminal), never an error
evidence: caller cancel → extraction skipped, payload scratch cleaned, warning partial_evidence_dropped
          if staged payload or a *.jsonl.*.tmp existed; timed_out/error → extraction attempted
          published JSONL is NEVER deleted
run_status: unknown or evicted run_id → run_not_found (isError true, retryable false),
            detail {run_id, known_runs: newest ≤ 20}; listing mode with nothing in it is
            runs: [] + active_count 0 + truncated false, not an error
```

## 18. Handoffs

| owed by this ADR to | what |
|---|---|
| mact-f1w.4 (ADR-0003) | the cheap "can we produce a tree for this commit" check must stay **before** `run_id` allocation, so `isolation_failed` keeps meaning "nothing was started" (ADR-0001 §2.9 step 8); anything that goes wrong after registration is `runner_internal_error` with `detail.stage`, and `phase: preparing_tree` is what makes that visible. Whether tree prep is a visible phase or an instantaneous hop is ADR-0003's call (§3.6). The workspace lock must be held from registration to §3.4 step 6 |
| mact-f1w.5 | clap defaults exactly as §11 (unit-tested against this table); `--max-timeout-secs` advertised as the `timeout_secs` bound; the rmcp Tasks surface and the exact MCP cancellation method name recorded from the locked version (§5.3, §12) |
| mact-f1w.8 | `terminate(initiator) → KillReport` as the only signalling code; `--term-grace-secs`; the on-disk log cap and its truncation marker; verify `PDEATHSIG` under tokio and keep or drop it accordingly |
| mact-f1w.11 | §17 verbatim; retention; `run_not_found`; shutdown sweep; the "request dropped ≠ run cancelled" test |
| mact-f1w.14 | §17 defaults; the §3.4 ordering (return only after evidence is durable); tool description states the cancel-skips-evidence rule so a caller is not surprised that a cancelled run has no numbers |
| mact-f1w.16 | cancellation-notification conformance; Tasks negotiation when/if §12 step 1 lands |
| mact-f1w.19 | does real act honour SIGTERM within 20 s (else raise the default grace), and the exact `docker ps` filter that identifies a run's containers |
| mact-f1w.18 | runbook: orphaned containers recipe (from .19), the §10 poll loop, timeout tuning per §4.2/§4.3, and "the server keeps runs in memory only: after a restart, read `.evidence/runs/`" |
| mact-f1w.13 | §11 as the resource-cap surface; the registry memory bound in §7.3 |
