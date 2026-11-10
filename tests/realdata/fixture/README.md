# `tests/realdata/fixture/` — the workflow pair the real-act smoke runs

Copied verbatim into a throwaway git repo by `scripts/smoke-real-act.sh` (docs/testing.md §7),
committed, and then run for real by `act`. Nothing else reads these files. They are not used by
`tests/e2e`, which has its own `testdata/workflows/` for its stub.

Every line here exists to make a claim checkable, so this file records what each one claims and
which number to look at when it breaks.

## `coverage.yml` — job `coverage`

| step | the claim it proves | what the smoke checks |
|---|---|---|
| print the smoke marker | step stdout reaches act's `--json` log (`--quiet=false` does not suppress it — ADR-0003 **V2**) | the captured `runs/<run_id>/act.jsonl` contains `ACTMCP_SMOKE_MARKER` |
| write a trivial lcov tracefile | `$ACTMCP_EVIDENCE_OUT` reaches the job **and** names a path that is the same on both sides of the bind mount (ADR-0002 §3.1, ADR-0003 **F8/V1**) | coverage records exist, and the totals below are exactly right |

The tracefile is two `SF` blocks with arithmetic a human can do in their head:

```text
src/smoke/mod.rs   LF:4 LH:3   BRF:2 BRH:1
src/smoke/util.rs  LF:2 LH:1   BRF:0 BRH:0
total              LF:6 LH:4   BRF:2 BRH:1     → 1 total-scope + 2 file-scope records
```

Those are the numbers `scripts/smoke-real-act.sh` asserts, so change the tracefile and the
script fails until you change the assertion too. That is deliberate: a fixture nobody has to
re-read when editing is a fixture that drifts.

## `benchmark.yml` — jobs `warmup` and `benchmark` (`needs: [warmup]`)

The second job needs the first, which is the whole point of having two. The runner selects a job
with `--job <id>` (ADR-0003 §7.2) and nobody in this repository had confirmed that act then also
runs the job's `needs:` chain. `benchmark`'s first step fails the run unless the file
`warmup` wrote into the workspace is there — so the answer is `success` or a red run naming the
dependency, never silence. It also produces a log with two jobs in it, which is the live test of
the benchmark extractor's attribution rule (only `benchmark`'s steps may become
`job_duration_s`).

| step | the claim | checked as |
|---|---|---|
| prove the needs chain ran | act's `--job` honours `needs:` | the run is `success` |
| emit the in-band markers | `ACTMCP_BENCHMARK …` on stdout becomes `in_band_stdout` records — one metric with labels and a unit, one with neither | both names appear; `smoke_latency_ms == 12.5` |
| write the benchmark payload | `benchmark.json` crosses the bind mount and becomes `job_file` records | both names appear, source list contains `job_file` |

On act ≥ v0.2.76 the run must *also* carry `act_log` records (step timings); on older act it
must not, and the script says which case it is in. See `tests/realdata/golden/act-shape.json`.

## `.gitignore`

Read it before deleting a line. It commits the two patterns the evidence writer would otherwise
*append* to a tracked `.gitignore`, and an append to a tracked file is a tracked modification —
which ADR-0003 G5 reads as `dirty_worktree`, breaking the second run in the workspace. The
fixture commits them so the append is a no-op; a repository that has not committed them hits
that trap, and the smoke script recognises the failure and names it.

## Two things this fixture deliberately does not do

* **no toolchain requirements.** Every step is `sh` plus coreutils, so it runs on whatever image
  the local act maps `ubuntu-latest` to — act's own default mapping is the small
  `node:16-buster-slim`, not a multi-gigabyte runner image. A smoke that needs a compiler is a
  smoke nobody runs.
* **no actions.** No `uses:` steps at all: they'd make act resolve and cache third-party action
  repos over the network, and the smoke's claims are about act, not about the marketplace.
