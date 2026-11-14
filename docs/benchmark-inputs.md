# Benchmark inputs — where a number came from, and what act has to do with it

Who this is for: whoever reads a `benchmark/` record and needs to know **what produced it** —
including the case where nothing did. It restates
[ADR-0002 §3, §5](adr/0002-evidence-jsonl-contract.md) for the input side and adds the facts
about third-party act versions that the ADR cannot know. Where this page and the ADR disagree,
the ADR wins; where it disagrees with a released act, act wins and
[tests/realdata/golden/act-shape.json](../tests/realdata/golden/act-shape.json) is where the
contradiction gets recorded.

Contents: [1. the three producers](#1-the-three-producers) ·
[2. precedence](#2-precedence-and-suppression) ·
[3. act's own timings need act 0.2.76 or newer](#3-acts-own-timings-need-act-0276-or-newer) ·
[4. what a run says when an input is missing](#4-what-a-run-says-when-an-input-is-missing) ·
[5. making sure a number exists](#5-making-sure-a-number-exists)

Assumed versions, restated so a claim can be reproduced: act-mcp **0.1.0**, act
**≥ 0.2.62** (`src/workspace.rs::MINIMUM_ACT_VERSION`, raisable with `--minimum-act-version`),
`schema_version` **1**.

---

## 1. The three producers

```text
$ACTMCP_EVIDENCE_OUT/benchmark.{json,jsonl} ─┐ source: job_file        (authoritative)
                                             │
act.jsonl ── ACTMCP_BENCHMARK lines ─────────┤ source: in_band_stdout  (convenience)
         └─ executionTime / time per step ───┘ source: act_log         (context, always)
```

| producer | who writes it | depends on act at all? | record `source` |
|---|---|---|---|
| **the payload file** | the job, into `$ACTMCP_EVIDENCE_OUT` (a bind-mounted `incoming/<run_id>/`) | no | `job_file` |
| **the in-band marker** | the job, as `ACTMCP_BENCHMARK {…}` lines on stdout, which arrive in act's `msg` on `raw_output: true` lines | only that act passes step stdout through | `in_band_stdout` |
| **act's own timings** | act, on each step's Success/Failure line | **yes — see §3** | `act_log` |

The first two are the contract; the third is context that makes a run answerable ("did it run,
and how long did it take") even when its job reports nothing
([ADR-0002 §5.3](adr/0002-evidence-jsonl-contract.md)). `job_duration_s` and `step_duration_s`
come from the third producer, and from nowhere else.

Once a run has read the payload file it is moved out of `incoming/` and kept at
`runs/<run_id>/payload/<name>` (budget: `--max-payload-keep-bytes`, `0` keeps none), so the file a
`job_file` record was parsed from survives the scratch directory that `source_path` names
([ADR-0002 §3.4](adr/0002-evidence-jsonl-contract.md)).

The parsing lives in [`act_mcp::evidence::benchmark`](../src/evidence/benchmark.rs); the
field names it reads are pinned by
[`tests/fixtures/benchmark/act.jsonl`](../tests/fixtures/benchmark/act.jsonl) rather than by
code comments, because the log is act's format and not ours.

## 2. Precedence and suppression

Order of authority, highest first — and the order the lines come out of a run's file:

1. `job_file` beats everything; a second benchmark-named file is reported
   (`benchmark_source_shadowed`) rather than blended, because one run wants one source of
   numbers ([ADR-0002 §3.2](adr/0002-evidence-jsonl-contract.md)).
2. `in_band_stdout` fills what the file did not cover; a marker repeating a payload metric is
   a duplicate and is dropped.
3. `act_log` fills what neither produced, and is **suppressed** for a measurement the job
   made itself: a job that writes its own `job_duration_s` (or a `step_duration_s` for the
   same `job`/`step` labels) wins, so a duration is never counted twice (§5.3).

Units are copied verbatim and never converted; a value that is not a finite number is refused
and counted in `malformed` rather than published
([ADR-0002 §4.1](adr/0002-evidence-jsonl-contract.md)).

## 3. act's own timings need act 0.2.76 or newer

**The fact, pinned.** act's `--json` log carries a step's duration in exactly one field,
`executionTime` — a Go `time.Duration`, so logrus marshals it as **integer nanoseconds** — and
`pkg/runner/step.go` gained that field in **v0.2.76**. The same line in v0.2.62 and v0.2.75,
read out of act at those tags:

```go
// v0.2.62 … v0.2.75 — no duration in the fields, and none in the text
logger.WithField("stepResult", stepResult.Outcome)
      .Infof("  ✅  Success - %s %s", stage, stepString)

// v0.2.76 — the same line, with the duration in both places
logger.WithFields(logrus.Fields{"executionTime": executionTime, "stepResult": stepResult.Outcome})
      .Infof("  ✅  Success - %s %s [%s]", stage, stepString, executionTime)
```

So on `0.2.62 … 0.2.75` there is nothing to fall back to: the duration is not in the fields and
not in the message either. That range is *inside* this server's supported range, whose floor is
`MINIMUM_ACT_VERSION` **0.2.62**.

**The decision: keep the floor at 0.2.62 and name the gap.** Raising the floor to 0.2.76 was
the other option and was rejected, because:

* **A missing evidence nicety must not stop a run.** [ADR-0002
  §5.1](adr/0002-evidence-jsonl-contract.md) is explicit that extraction problems are warnings
  and never change `run_status`; `run_workflow`'s job is to run the workflow. Refusing to run
  act 0.2.63 because it cannot time its steps would fail a run over `step_duration_s`.
* **The floor is a supported-range claim, not a bug fix.** 0.2.62 is written into
  [ADR-0003 §1.3](adr/0003-commit-isolation-and-serialization.md#13-ground-truth-about-act-established-before-this-ticket)
  and into [README](../README.md#every-flag-and-env-var) as the range this project supports and
  tests against; changing it is an ADR-scale decision that would strand operators whose act comes
  from a distro package.
* **Silence was the actual bug.** A scanner that finds no `act_log` rows cannot tell "act is too
  old" from "the extractor silently stopped working" — and the fixtures all had
  `executionTime`, so nothing in the suite could see it happening.

**What happens instead.** The scan counts a run's *step-completion lines that carried no usable
timing*. A completion line is act's Success/Failure line — the one line that ever owes a timing
— recognised by `stepResult: "success" | "failure"`, or, failing that, by act's own
`Success - ` / `Failure - ` wording on a step-scoped line. Two exclusions keep that count
honest about what act actually owes:

* `stepResult: "skipped"` is **not** a completion: act logs a skipped step that way in every
  version, v0.2.76 included, and never times a step it did not run. Counting it would make
  every workflow with one `if:`-ed-out step warn forever.
* A line naming no step is not one either — act's job-level `🏁 Job succeeded` carries neither
  a step nor a timing in any version.

A run with completion lines and no timings is then no longer quiet. It carries:

| where | what |
|---|---|
| the tool result's `warnings[]`, and `meta.json`'s `warnings[]` | `act_step_timings_unavailable` |
| the server's log, at `warn` | the count of untimed completions, the count of timed ones, the version that added `executionTime`, this server's floor, and what `job_duration_s` had to fall back to |

What deliberately does **not** change: `job_duration_s` is still published when no step was
timed, from the span of the run's own log lines — act's only other clock, and whole seconds —
and any step that *was* timed is still published, so a partially-timed log loses only the steps
it lost. A run with no completion lines at all (killed before a step finished) is a different
case and gets no such warning; its status already says it was cancelled.

**For a reader.** `step_duration_s` rows (`source: "act_log"`) are the only benchmark rows an
old act cannot produce. A run with `act_log` rows and no `step_duration_s` is that case: read
its `warnings[]`. Do not infer `0 s` steps from their absence, and do not compare a
whole-second `job_duration_s` against a millisecond one as if they were the same measurement —
the one came from the log span, which is why the warning exists.

**For an operator.** `--minimum-act-version 0.2.76` (or
`ACT_MCP_MINIMUM_ACT_VERSION=0.2.76`) is the way to say "I would rather refuse to run than run
without step timings": the flag may be raised freely, and never lowered below 0.2.62
([README](../README.md#every-flag-and-env-var)). That is a per-deployment choice, which is
exactly why it was not made for everybody by raising the floor.

**Where this is pinned.**
[`tests/fixtures/benchmark/act-pre-0.2.76.jsonl`](../tests/fixtures/benchmark/act-pre-0.2.76.jsonl)
is the pre-v0.2.76 log shape (built from act's source at those tags, not typed from hope),
[`tests/benchmark_extraction.rs`](../tests/benchmark_extraction.rs) turns it into the warning
assertion, `src/evidence/benchmark/act_log.rs` carries the source quotation, and
[`tests/realdata/golden/act-shape.json`](../tests/realdata/golden/act-shape.json) keeps the
`executionTime-version-floor` gap on the record for the real-act smoke to re-check against a
live log.

## 4. What a run says when an input is missing

Every one of these is a warning code in `warnings[]` (the tool result and
`meta.json`), never a run failure ([ADR-0002 §5.1](adr/0002-evidence-jsonl-contract.md)):

| warning | what was missing |
|---|---|
| `no_benchmark_metrics` | nothing at all — no payload, no marker, no usable timing — so no `benchmark/` file was written for the run (§1.3) |
| `payload_too_large`, `payload_unreadable` | the payload file existed and could not be used (over the 8 MiB cap, unreadable) |
| `parse_failed` | the payload existed and was not usable JSON, or held no usable entry |
| `benchmark_source_shadowed` | more than one benchmark-named file existed; the highest-precedence one was used |
| `malformed_metric` | entries or marker lines were present and unusable |
| `marker_cap_reached` | more than 5 000 marker lines existed |
| `truncated` | more than 5 000 metrics existed; every record of the run carries the `truncated` flag |
| `payload_transport_unavailable` | the operator runs with `--act-no-bind`, so the job could not write a payload at all |
| `act_log_unreadable` | the run's captured log could not be read |
| `act_log_truncated` | the log hit our own byte cap, so anything after the cut is missing rather than absent |
| `act_step_timings_unavailable` | act timed nothing it ran — §3 |

## 5. Making sure a number exists

In order of preference:

1. **Write the payload file.** It is the contract, it is authoritative, and it depends on no act
   behaviour at all — one metric per line in `$ACTMCP_EVIDENCE_OUT` (the directory act-mcp made
   for this run), e.g.
   ```sh
   printf '%s\n' '{"name":"cold_start_ms","value":412.5,"unit":"ms"}' \
     >"$ACTMCP_EVIDENCE_OUT/benchmark.jsonl"
   ```
   (shape details: [ADR-0002 §3.3](adr/0002-evidence-jsonl-contract.md),
   [docs/evidence-format.md §5](evidence-format.md#5-the-benchmark-record)).
2. **Or print markers** — `ACTMCP_BENCHMARK {"name":…,"value":…,"unit":…}` — for a job that
   cannot write files.
3. **Or run act ≥ 0.2.76** and let act's own timings be the floor of what is always available.

A benchmark that needs sub-second step timings, on an act that cannot emit them, has exactly one
honest fix: the job measures itself. That is producer 1 or 2, and by §2 its numbers win over
act's anyway.
