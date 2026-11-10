# `tests/realdata/golden/` — what a real `act --json` log looks like

Two kinds of file live here, and they are not the same kind of claim.

| file | what it is | who writes it |
|---|---|---|
| `act-shape.json` | what this crate's extractors **assume** about act's `--json` log, field by field, each with its provenance | a human, from act's source; a machine checks it |
| `act.jsonl` + `act.jsonl.provenance.json` | a **capture**: the log of one real run, plus what act/version/host produced it | `scripts/smoke-real-act.sh --capture-golden` |

## Why there is no `act.jsonl` here yet

Because there was nothing honest to put in it. This file exists to stop the extractor fixtures
in `tests/e2e/testdata/logs/` and `tests/fixtures/benchmark/` from quietly becoming a
description of somebody's imagination, and a golden typed up from the same assumption it is
supposed to police would make that worse, not better. Writing it needs a machine with both
`act` and a working container runtime; no machine in this repository's CI had both when this
was written (`docs/testing.md` §1 is deliberate about that).

So the golden is produced by a run and reviewed by a person, in that order:

```sh
scripts/smoke-real-act.sh --capture-golden        # writes act.jsonl + its provenance sidecar
git diff tests/realdata/golden                    # READ it: these are act's bytes now
git add tests/realdata/golden && git commit       # committing it is the assertion
```

`.github/workflows/nightly-real-act.yml` runs exactly that on a runner that has act and
docker, and uploads the pair as an artefact whether the run is green or red. Committing the
artefact is still a human act: the nightly's job is to make the capture exist and to make a
divergence from `act-shape.json` loud, not to merge its own conclusions.

Once `act.jsonl` is committed:

* it is the third-party half of the `.evidence` contract, referenced by ADR-0002 §1.3 and by
  `src/evidence/benchmark/act_log.rs`'s field table — a fixture those tests can be pointed at
  instead of a hand-written log;
* a later capture that differs is a change in act, and `scripts/smoke-real-act.sh`'s shape
  step will say so before anything downstream has to discover it in production;
* the provenance sidecar is what makes "which act wrote this?" answerable in a year.

## `act-shape.json`, and what to do when the checker complains

The shape step in the smoke script diff this manifest against a live capture and prints the
difference. `act-shape.json` says, per field, its type, who reads it, how the claim was
established (`source` = read out of act at a named tag, `capture` = seen in a committed log),
and — for the fields act has *changed* across its version range — where that happened. Two
gaps are already recorded from reading act's source across the supported range:

* `executionTime` exists only from act **v0.2.76**; the extractors' step timings come from
  that field and nothing else, while this server's floor is 0.2.62.
* `Output`/`Error` are read by `act_log.rs` as message fields and are not act's fields at all.

When the smoke prints `DIVERGENCE`, open the follow-up it offers (that is the ticket the log
shape owed, not a nuisance), read the named extractor, and *then* decide whether act changed or
the assumption was wrong. Editing `act-shape.json` to match a capture without having read the
extractor is how a wrong `job_duration_s` gets published for a year.
