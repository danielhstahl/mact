# The `.evidence` format — the contract a reader gets

Who this is for: whoever writes the **downstream cross-commit scanner** — the program that
walks `<workspace>/.evidence` after a run and answers "did this commit regress?". It is the
human-facing restatement of
[ADR-0002](adr/0002-evidence-jsonl-contract.md), which stays the normative text: where this
page and the ADR disagree, the ADR wins. The machine-readable form is
[`schema/evidence.schema.json`](../schema/evidence.schema.json), and
`act-mcp validate-evidence` enforces both.

Assumed versions, restated so a recipe's output can be reproduced: `schema_version` **1**,
`act-mcp` **0.1.0**, act **≥ 0.2.62** (the fixture records were captured against
`0.2.68`, which is what `tool.act_version` says in them), `jq` **1.7.1**.

Contents: [1. a tree to read](#1-get-a-tree-to-read) ·
[2. layout](#2-layout-what-a-scanner-walks-and-what-it-must-ignore) ·
[3. filenames](#3-filename-convention-and-run_id) ·
[4. the envelope](#4-the-record-envelope) ·
[5. benchmark records](#5-the-benchmark-record) ·
[6. coverage records](#6-the-coverage-record) ·
[7. trust](#7-what-is-guaranteed-and-what-is-advisory) ·
[8. append-only](#8-append-only-atomicity-and-what-a-crashed-run-looks-like) ·
[9. who owns dedup](#9-dedup-samples-and-percentiles-who-owns-what) ·
[10. percentages](#10-percentages-are-yours-not-ours) ·
[11. version policy](#11-schema_version-bump-policy) ·
[12. jq recipes](#12-recipes) ·
[13. validating](#13-validating-a-tree)

---

## 1. Get a tree to read

Every example below is from a real tree. You do not need docker or act to look at one:

```sh
scripts/sample_evidence.py                       # writes the tree, prints what it wrote
export EV=$(scripts/sample_evidence.py --quiet)  # just the path, for scripting
```

That lays the checked-in e2e goldens (`tests/e2e/testdata/expected/`) out under the real
layout, across three commits, and `scripts/evidence_recipes.sh` runs every recipe in
[§12](#12-recipes) against it end to end. If you have run the server for real
([README runbook](../README.md#the-five-minute-tour)), use its evidence root instead:

```sh
export EV=/tmp/actmcp-runbook/repo/.evidence
```

One asymmetry to know about: `scripts/sample_evidence.py` writes only `benchmark/` and
`coverage/`. That is deliberate — §2 says those are the only two directories that are part
of the contract — but it means the sample tree has no `runs/`, so recipes about
`meta.json` need a real root.

The recipes then read **the scanner-visible files and nothing else**. Do not reach for
`"$EV"/*/*.jsonl` style globs: on a real root they walk into `runs/<run_id>/act.jsonl`, which
is act's own log and not a record (this bit me while writing them — the output was plausible).
List the files once, in every shell you run them from:

```sh
mapfile -t RECORDS < <(find "$EV"/benchmark "$EV"/coverage -name '*.jsonl' 2>/dev/null | sort)
```

## 2. Layout: what a scanner walks, and what it must ignore

```
<evidence-root>/                 # <workspace>/.evidence by default; --evidence-dir / $EVIDENCE_DIR override
├── README.md                    # one-paragraph stub: local build output, safe to delete
├── benchmark/                   ┐ the cross-commit contract. Nothing else is.
│   └── <full-sha>/              │ one directory per commit, 40 lowercase hex
│       └── <run_id>.jsonl       ┘ one file per run, one kind, immutable once published
├── coverage/
│   └── <full-sha>/
│       └── <run_id>.jsonl
├── runs/                        # NOT commit-keyed, NOT part of the contract
│   └── <run_id>/
│       ├── act.jsonl            # act's own log, redacted, not schema-versioned
│       ├── meta.json            # terminal manifest; written last (§8)
│       └── payload/             # the lcov / coverage-summary / benchmark.json this run read
└── incoming/                    # transient payload scratch; may be empty or gone
    └── <run_id>/
```

Rules a reader can rely on, all of them ADR-0002 §1.1:

* **Walk `benchmark/` and `coverage/` and nothing else.** `runs/` and `incoming/` are
  per-run audit material: `runs/` tells you *why* a run produced what it did, `incoming/`
  is transient and may be cleaned at any time.
* Every file the writer creates is `<run_id>.jsonl`, `meta.json`, `act.jsonl`, a kept payload
  file under `runs/<run_id>/payload/`, a temp file
  named `.<run_id>.jsonl.<random>.tmp`, or that `README.md`. Everything else is yours.
  **Skip dot-files** — a `.…tmp` file is a crashed publish, never a record.
* A directory name is always a **full lowercase** commit SHA. Short SHAs never appear in a
  path, which is what lets a scanner key a lookup on `git log --format=%H` with no
  resolution step (§3).
* The evidence root is per-workspace. One scanner per workspace, or a collector that
  copies trees into one place; nothing in the format joins two workspaces.

## 3. Filename convention and `run_id`

```
<kind>/<full-sha>/<run_id>.jsonl

run_id  = YYYYMMDDTHHMMSSZ "-" 6 lowercase hex      example 20261110T004203Z-a1b2c3
regex   ^[0-9]{8}T[0-9]{6}Z-[0-9a-f]{6}$
```

* One file is **exactly one run**: every line shares `run_id`, `commit`, `workflow`,
  `task`, and one `kind`. A file whose name says `benchmark/` never contains a coverage
  record.
* The timestamp is *inside* the id, so `ls` order is chronological order. `run_id`
  therefore doubles as "when", and no separate field can disagree with it. Be careful
  writing a scanner the other way round: `git log` yields newest-first, `ls` yields
  oldest-first.
* Keying the path by the full SHA is what makes a scan cheap: `git log` N commits, `stat`
  `<kind>/<sha>/` per commit, and open no file for a commit you do not care about.

## 4. The record envelope

Every line of every file is one compact JSON object: the envelope, then the payload fields
of that line's `kind`. No `null` anywhere — a value that is unknown is **absent**
(`null` is a validator error, and `0`/`""` are never stand-ins).

```json
{"schema_version":1,"kind":"benchmark","run_id":"20261110T004203Z-a1b2c3",
 "commit":"2f7c1a4b9e0d3c6a5b8f1e2d4c6a8b0f3e5d7c91","workflow":"benchmark",
 "task":"hyperfine-bench","category":"benchmark","run_status":"success",
 "tool":{"name":"act-mcp","version":"0.1.0","act_version":"0.2.68"},
 "started_at":"2026-11-10T00:42:03.417Z","duration_ms":94122,"exit_code":0,
 "host":"build-box-07", …per-kind fields… }
```

| field | type | always there | notes |
|---|---|---|---|
| `schema_version` | int | ✅ | `1` today. §11 is the bump policy; **refuse what you cannot read.** |
| `kind` | string | ✅ | closed enum `benchmark` \| `coverage`. Also the directory the file lives in. |
| `run_id` | string | ✅ | §3. Unique per run; sortable. |
| `commit` | string | ✅ | 40 lowercase hex; **equals the directory name** (trust the directory, then assert it). |
| `workflow` | string | ✅ | workflow identity as resolved — file basename without `.yml`/`.yaml`. Never a path. |
| `task` | string | ✅ | the job id that ran (`act -j <job>`). Never re-derived from logs. |
| `category` | string | ✅ | the *classification that caused evidence to be written*. In v1 always equal to `kind`; it exists so the two can diverge later without a breaking change. Open vocabulary. |
| `run_status` | string | ✅ | `success` \| `failed` \| `cancelled` \| `timed_out` \| `error`. Evidence from a non-`success` run is **written, not suppressed** — filtering is the consumer's job (§9). |
| `tool` | object | ✅ | `{name:"act-mcp", version, act_version?}`. `act_version` is there because an act upgrade moves timings and log shapes. |
| `started_at` | string | ✅ | UTC, always `Z`, millisecond precision: `…T00:42:03.417Z`. Never `+00:00`. |
| `duration_ms` | int | ⚪ optional | whole-run wall time in ms. Absent when unknown — never `0` as a guess. |
| `exit_code` | int | ⚪ optional | act's wait(2) code. Absent when act produced none (signalled, failed spawn). |
| `host` | string | ✅ | lowercased hostname. Provenance; **never a join key**. |

Shape guarantees that are load-bearing for a parser (ADR-0002 §2.1, §6.1):

* one compact JSON object per line, **strict RFC 8259** — no `NaN`, no `Infinity`, no
  comments, no trailing commas;
* **LF only**, no CR, no NULs, no embedded raw newlines, and a **trailing LF is required**:
  a file whose last byte is not `LF` is a torn write, and a checker must fail it;
* key order is stable per record type (envelope first, in the table order above, then the
  kind's payload with `source`, `source_path`, `flags`, `extra` last), and keys inside
  `labels`/`extra`/`tool` are in ascending code-point order — so a golden diff is
  meaningful and re-serialising a file is a no-op;
* `additionalProperties: false`: **an unknown field is a hard error for a strict reader.**
  That is what makes "add an optional field" an additive change (§11). A permissive reader
  should still ignore unknown *values* of open vocabularies;
* the two payload field sets are mutually exclusive: a benchmark line never carries
  `scope`/`path`/`lines_*`/`branches_*`/`statements_*`/`functions_*`/`reported_*`, and a
  coverage line never carries `value`/`unit`/`labels`/`sample`. The words that appear in
  both tables (`name`, `source`, `flags`, `extra`) take the per-kind meaning below;
* any relative-path-shaped value (`path`, `source_path`) is workspace-relative: `/`
  separators, no leading `/`, no `~`, no `.`/`..` segment, ≤ 512 chars. **An absolute host
  path never appears in evidence** — act mounts the workspace at `/srv`, and leaking that
  would be both a portability and a privacy bug.

## 5. The benchmark record

One metric per line. Three sources, one file:

```json
{"…envelope…","name":"tokenizer_throughput","value":184320.4,"unit":"ops/s",
 "labels":{"job":"hyperfine-bench","os":"ubuntu-24.04","runner":"self-hosted"},"sample":1,
 "source":"job_file","source_path":".evidence/incoming/20261110T004203Z-a1b2c3/benchmark.json"}
```

| field | type | always there | notes |
|---|---|---|---|
| `name` | string | ✅ | verbatim from the job, or one of the built-ins `job_duration_s` / `step_duration_s`. |
| `value` | number | ✅ | any finite JSON number. Non-finite values are rejected **before** serialisation, because JSON cannot express them. |
| `unit` | string | ⚪ | verbatim, always — act-mcp **never** converts, scales or invents a unit. Absent ⇔ `flags` contains `unit_missing`. Never combine values whose units differ or are missing. |
| `labels` | object | ⚪ | flat string→string, ≤ 16 entries, keys `^[a-z][a-z0-9_]*$`, values ≤ 128 chars. Values are **always strings**, so a `group by` never has to type-switch. Known keys: `job`, `step`, `os`, `runner`, `matrix`, `attempt`, `workflow`, `task`. Keys are open. |
| `sample` | int ≥ 1 | ⚪ | present only when the job really emitted more than one sample for the same `(name, labels)`; numbered `1..n` (§9). |
| `source` | enum | ✅ | `job_file` \| `in_band_stdout` \| `act_log` — §7's trust table. |
| `source_path` | relpath | when `job_file` | the payload file it was parsed from — scratch; the kept copy is `runs/<run_id>/payload/<name>` (§10). Absent for the two log-derived sources. |
| `flags` | string[] | ⚪ | machine-readable caveats; unique, ≤ 8. |
| `extra` | object | ⚪ | advisory passthrough. **Nothing may depend on a key here.** |

Every benchmark run also gets `job_duration_s` and `step_duration_s` (labels `job`, and
`job`+`step`) with `source: "act_log"`, `unit: "s"`, derived from act's own `--json`
timings. So *every* benchmark run leaves at least one row even if its job reports nothing —
which is also why `act_log` rows are context rather than a measurement: if a job measures
its own `job_duration_s`, the job's value wins and act's is suppressed for that label set.
Note the deliberate asymmetry: the envelope's `duration_ms` is integer milliseconds, the
metric names are human-facing seconds (`*_s`).

One version dependency to know about when diffing history: `step_duration_s` needs act's
`executionTime`, which act gained in **v0.2.76**, and this server accepts act from **0.2.62**. A
run on `0.2.62 … 0.2.75` therefore publishes no `step_duration_s` at all, and its
`job_duration_s` is the whole-second span of the log lines instead of a sum of steps — which the
run says with the `act_step_timings_unavailable` warning instead of going quiet about. Absent
`step_duration_s` rows are *unknown*, never `0 s`. See
[docs/benchmark-inputs.md §3](benchmark-inputs.md#3-acts-own-timings-need-act-0276-or-newer).

## 6. The coverage record

One source file — or the run total — per line:

```json
{"…envelope…","name":"total","scope":"total","lines_found":4120,"lines_hit":3581,
 "branches_found":1180,"branches_hit":946,"source":"lcov",
 "source_path":".evidence/incoming/20261110T011507Z-d4e5f6/lcov.info"}
```

| field | type | always there | notes |
|---|---|---|---|
| `scope` | enum | ✅ | `total` \| `file`. |
| `path` | relpath | when `scope == "file"`; **forbidden** for `total` | the source file, workspace-relative (container `/srv/...` normalised away). A file record's identity is `path`, not `name`. |
| `name` | string | ⚪ | a label, conventionally `"total"` on a total record; omitted on file records. |
| `lines_found` / `lines_hit` | int ≥ 0 | one complete pair, or `statements_*`, or `reported_percent` | raw numerators. `hit ≤ found` is an extractor rule the schema cannot express, so a scanner should sanity-check rather than assume. |
| `branches_found` / `branches_hit` | int ≥ 0 | ⚪ co-required pair | absent when the format reports no branches (generated files are the everyday case). |
| `statements_found` / `statements_hit` | int ≥ 0 | ⚪ co-required pair | istanbul **statements are a different unit from lines**. Never average the two. |
| `functions_found` / `functions_hit` | int ≥ 0 | ⚪ co-required pair | istanbul `functions`; reserved in v1 so adding it later stays additive. |
| `reported_percent` | number 0..=100 | ⚪ | only ever for `source: "log_scrape"`, only with `reported_measure`, always under `flags: ["percent_only"]`. Never with raw counts. |
| `reported_measure` | enum | ⚪ | `lines` \| `statements` \| `branches` \| `functions` \| `unknown`. A percentage nobody labelled is `unknown`, not a guess. |
| `source` | enum | ✅ | `lcov` \| `coverage_summary` \| `log_scrape`. |
| `source_path` | relpath | for `lcov` / `coverage_summary` | the payload file it was parsed from — scratch; the kept copy is `runs/<run_id>/payload/<name>` (§10). Absent for `log_scrape`, whose input is act's log. |
| `flags`, `extra` | | ⚪ | as in §5. |

Format notes a scanner needs (ADR-0002 §4.2):

* supported: **lcov** (`SF`/`DA`/`LF`/`LH`/`BRDA`/`BRF`/`BRH`, repeated `SF` blocks summed
  per path) and **istanbul `coverage-summary.json`**. **Cobertura `coverage.xml` is not
  supported** — a job that emits only that produces no coverage records and a
  `no_coverage_found` warning in `meta.json`, rather than a guess.
* a `scope:"total"` record is *the source's own total*, which can exceed the sum of that
  run's `file` records when the tool instrumented files outside the workspace
  (`node_modules` is the everyday case). No relpath can name those, so they get no file
  record while still belonging to what the run measured. The extractor reports the count it
  dropped (`coverage_paths_outside_workspace` in `meta.json`'s `warnings[]`) instead of
  quietly renumbering the total. **Do not use "sum of file records == total" as an
  invariant.**
* a percentage-only record (`log_scrape` + `percent_only`) is a last resort: it says what a
  tool printed, not what it measured. Treat it as a signal, never as a number to plot
  a percentage against lcov runs — recipe 7 shows the filter.

## 7. What is guaranteed, and what is advisory

The short version of this section: **the envelope and the raw counts are a promise;
everything that describes how a number was arrived at is a hint about how much to trust it.**

| bucket | what it means |
|---|---|
| **guaranteed** — shape-stable, always present | `schema_version`, `kind`, `run_id`, `commit`, `workflow`, `task`, `category`, `run_status`, `tool.name`, `tool.version`, `started_at`, `host`; per kind: `name`, `value`, `source` (benchmark) and `scope`, `source` (coverage) |
| **optional but meaningful** | absent ⇔ "not reported", never guessed: `duration_ms`, `exit_code`, `tool.act_version`, `unit`, `labels`, `sample`, `source_path`, `name` (coverage), `path` |
| **advisory** — read it, never depend on it | `extra` (no key is load-bearing), `host` (provenance, not a key), `category` (in v1 equals `kind`), `meta.json` in general (audit, not contract) |
| **authority, by `source`** | `job_file`, `lcov`, `coverage_summary` = parsed from a machine-readable file the job produced → authoritative. `act_log` = derived from act's own timings → context, always emitted. `in_band_stdout` = a marker scraped from stdout → convenience, lower precedence (a `job_file` metric with the same `name`+`labels`+`value` wins and the marker line is dropped). `log_scrape` = a percentage scraped out of log text → last resort, always `percent_only` + `low_confidence`. |
| **caveats, by `flags`** (open vocabulary; unknown flags must be ignored, not fatal) | `unit_missing` no unit was reported · `percent_only` a percentage with no raw counts · `low_confidence` derived from text scraping · `truncated` the run hit the per-run metric cap, so **every** record of that run carries it and a consumer must never sum what survived and call it the run |

Two consequences worth stating plainly, because they are the difference between a scanner
and a chart:

* a record is only comparable to another record with the same `unit` (or both
  `unit_missing`), and coverage `lines` is not `statements`;
* extraction problems never fail a run. A run whose lcov file was unparseable still ends
  `success` with `run_status: "success"` and no coverage file; the explanation is in
  `meta.json`'s `warnings[]`, which is a separate file for a separate question.

## 8. Append-only, atomicity, and what a crashed run looks like

* **One file per run, published atomically, never rewritten.** The writer builds the whole
  record list in memory, writes `.<run_id>.jsonl.<random>.tmp` in the destination
  directory, `fsync`s it, `rename()`s it into place and `fsync`s the directory. A reader
  therefore never sees a partial record: the file appears with its final name and full
  content. "Append-only" happens **at the directory level** — a published file is never
  opened for writing again, and a crashed run's damage is exactly one file.
* Rollback is `rm`. There is no read-modify-write path anywhere, which is also why the
  version policy in §11 is safe.
* `meta.json` is written **last**, after act exits and after the JSONL is published:
  * a `runs/<run_id>/meta.json` that exists ⇒ the run reached a terminal state;
  * a `runs/<run_id>/` with **no** `meta.json` ⇒ act-mcp died mid-run; anything in that
    directory, and any evidence file with that `run_id`, is suspect.
* A **caller-cancelled** run publishes *nothing*: `cancel_run` stops extraction, so there
  is a `runs/<run_id>/meta.json` with `status: "cancelled"` and no `<run_id>.jsonl` under
  `benchmark/`/`coverage/`. A `timed_out` run *does* publish what it collected. A scanner
  that reports "commit X has no benchmark data" should be able to tell those three cases
  apart (no run at all · run cancelled · run succeeded, produced nothing).
* `evidence[]` in `meta.json` distinguishes "ran, found nothing" (empty `evidence[]`) from
  "never ran" (no `meta.json`), and per file records `records`, `truncated`, `malformed`.

## 9. Dedup, samples and percentiles: who owns what

**act-mcp appends. It never edits, aggregates, dedups across runs, or deletes history.** No
mean, no percentile, no "best of" ever lives in these files. Concretely:

* within **one run** the writer never emits two records with the same
  `(name, labels, value, source)`; genuine repeats of `(name, labels)` with *different*
  values are **samples**, all kept, numbered `sample: 1..n`.
* across **runs**, duplicates are data, not a bug: two runs of one commit are two
  measurements, possibly of different noise levels. The scanner chooses.
* the scanner owns: dedup policy, percentiles, outlier rejection, regression detection, and
  unit reconciliation. (Non-goals of this epic, deliberately.)
* identity, for diffing:

  | kind | identity |
  |---|---|
  | benchmark | `(commit, kind, run_id, name, labels, sample)` |
  | coverage | `(commit, kind, run_id, scope, path)` |

  `sample` belongs in the benchmark key: two samples of one `(name, labels)` in one run are
  distinct records, and a dedup that drops them silently throws away the noise estimate.
  Recipe 8 prints any repeats under those keys — an empty answer is the writer
  keeping its promise.
* filter by `run_status` before you aggregate. `failed` runs are published on purpose, so a
  benchmark dip caused by a broken run is visible and filterable rather than missing.

## 10. Percentages are yours, not ours

Evidence stores **raw numerators and denominators only**. No record stores a rate next to
the counts it came from, because two tools disagree about rounding and we refuse to
arbitrate.

* compute `rate = hit / found` yourself.
* `found == 0` is **vacuous**, not 0 %: a consumer that must print a number prints `100 %`
  (the lcov/istanbul/cobertura convention); a consumer that ranks or averages should
  **exclude** zero-denominator entries rather than let an empty generated file move the
  number.
* cross-run or cross-commit totals: **sum numerators and denominators**, never average
  per-file percentages. A run's `scope: "total"` record may be used directly as that run's
  total, with the one exception in §6.
* a percentage with no raw counts exists only as `reported_percent` (§6). Inventing a
  denominator (`874/1000`) to make a scraped percentage look raw is forbidden; a scanner
  must be able to filter them out via `flags: ["percent_only"]`.

### Where the raw file went

A record's `source_path` is where the extractor *read* — always
`.evidence/incoming/<run_id>/<name>`, scratch that may be gone by the time you ask. The file
itself is kept at `runs/<run_id>/payload/<name>` (budget: `--max-payload-keep-bytes`, default
4 MiB per run; `0` keeps nothing), and `meta.json`'s `payload` member spells the mapping:

```sh
# the lcov a commit's coverage total was computed from, if it is still on disk
RUN=$(jq -r 'select(.scope=="total") | .run_id' "$EV"/coverage/"$COMMIT"/*.jsonl | head -1)
jq -r '.payload.dir, (.payload.files[]?.path)' "$EV"/runs/"$RUN"/meta.json
```

`runs/` is audit material, not the contract: a scanner that needs the raw payload is debugging,
and `payload.dropped` plus the `payload_cap_exceeded` warning tell you when the budget deleted it.

## 11. `schema_version` bump policy

A flat, monotonically increasing integer, stamped on every record and on `meta.json`.
Currently **1**.

* **Additive ⇒ no bump**: a new optional field; a new member of an *open* vocabulary
  (`category`, `flags`, `source`, `warnings`); a newly supported coverage format; a new
  accepted payload filename. A v1 reader of v1 data with extra fields must still work.
* **Breaking ⇒ bump**: changing the type or meaning of an existing field; making an
  optional field required; renaming or removing a field; closing an open vocabulary;
  changing the closed `kind` enum; changing the layout, filename convention, path keying,
  or the *meaning* of an existing flag. Adding a flag is additive; redefining one is not.
* **Writers** emit only the version they were built with. There is no code path that can
  re-emit a foreign version, so "does the server refuse to write a version it cannot read?"
  is answered structurally: it cannot even try.
* **Readers must refuse what they cannot understand.** `validate-evidence` exits non-zero
  on an unknown `schema_version` (`unsupported_schema_version`) and never "upgrades" a
  file in place.
* **You** are a reader with your own policy, and the recommended one is *permissive on
  values, strict on shape*: ignore unknown `category`/`flag`/`source` members and unknown
  fields, **skip** (with a count) records whose `schema_version` is newer than yours, and
  treat a structurally unknown record as an error to report. Because files are immutable,
  a version bump leaves **mixed versions in one directory forever** — never assume one
  version per directory or per commit.

## 12. Recipes

Every recipe below is copied verbatim from
[`scripts/evidence_recipes.sh`](../scripts/evidence_recipes.sh), which runs them all in
order and exits non-zero if one stops working (recipe 11 runs only when you point the script
at a repository with `--repo`, because it is the one that needs commits that exist). They
need `jq` (1.7.1), `$EV` and `RECORDS` as set in §1. Each recipe's heading says what its
columns mean; commit SHAs are shortened in output only for width — the join key is always the
full 40 hex.

### 1 · what a scanner walks

```sh
find "$EV"/benchmark "$EV"/coverage -name '*.jsonl' 2>/dev/null | sed "s|^$EV/||" | sort
```

```
benchmark/2f7c1a4b9e0d3c6a5b8f1e2d4c6a8b0f3e5d7c91/20261110T004203Z-a1b2c3.jsonl
benchmark/2f7c1a4b9e0d3c6a5b8f1e2d4c6a8b0f3e5d7c91/20261110T041530Z-bbc0dd.jsonl
benchmark/3a91c7d2e4f608b1c5d7e9f0a2b4c6d8e0f13579/20261111T004203Z-a1b2c3.jsonl
…
coverage/4b02d8e3f5a719c2d6e8f0a3b5c7d9e1f2a4b6c8/20261112T030900Z-77aa55.jsonl
```

`ls` order here *is* chronological order within a commit (§3). Across commits, note that
`git log` is newest-first and this is oldest-first.

### 2 · every coverage total for one commit

Rate recomputed from the raw counts (§10); percent-only rows excluded because there are no
counts to recompute from — recipe 7 lists them instead.

```sh
COMMIT=$(ls "$EV"/coverage | head -1)
jq -r 'def pct($hit; $found): if ($found // 0) == 0 then 100 else ((10000 * $hit / $found) | round) / 100 end;
       select(.scope == "total" and .reported_percent == null)
       | [.run_id, .run_status, .source, ((.lines_hit // 0) | tostring),
          ((.lines_found // 0) | tostring), (pct(.lines_hit // 0; .lines_found // 0) | tostring)] | @tsv' \
    "$EV"/coverage/"$COMMIT"/*.jsonl
```

```
20261110T011507Z-d4e5f6	success	lcov	3581	4120	86.92
20261110T021244Z-0f9e8d	success	coverage_summary	1904	2210	86.15
```

Two runs of one commit, two formats, two near-identical totals: this is the whole point of
"the scanner owns dedup" (§9) — nothing here picks a winner.

### 3 · per-file line coverage, ranked, empty files excluded

```sh
jq -r 'select(.scope == "file" and (.lines_found // 0) > 0)
       | [(((10000 * .lines_hit / .lines_found) | round) / 100 | tostring), .path] | @tsv' \
    "$EV"/coverage/"$COMMIT"/*.jsonl | sort -rn
```

```
100	src/gen/protobuf_out.rs
96.35	src/parser/mod.rs
93.91	lib/index.js
84.03	src/parser/tokenizer.rs
0	src/util/debug_helpers.rs
0	lib/legacy/no-tests-here.js
```

### 4 · newest benchmark value per metric, across every commit

Group by the ADR's identity rather than by `name` alone: `step_duration_s` is one metric
with many label sets, and `max_by(.run_id)` is only meaningful per group.

```sh
jq -rs 'group_by([.name, (.labels // {})]) | map(max_by(.run_id))
        | sort_by([.name, ((.labels // {}) | tojson)]) | .[]
        | [.name, ((.labels // {}) | tojson), (.value | tostring),
           (.unit // "unit_missing"), .run_id] | @tsv' "$EV"/benchmark/*/*.jsonl
```

```
cold_start_ms	{"job":"hyperfine-bench","step":"Run benchmark"}	412.5	ms	20261112T004203Z-a1b2c3
job_duration_s	{"job":"hyperfine-bench"}	300.9	s	20261112T041530Z-bbc0dd
queue_depth	{"job":"hyperfine-bench"}	3	unit_missing	20261112T004203Z-a1b2c3
step_duration_s	{"job":"hyperfine-bench","step":"Complete job"}	3.066	s	20261112T004203Z-a1b2c3
step_duration_s	{"job":"hyperfine-bench","step":"Run benchmark"}	299.4	s	20261112T041530Z-bbc0dd
step_duration_s	{"job":"hyperfine-bench","step":"Set up job"}	1.5	s	20261112T041530Z-bbc0dd
tokenizer_throughput	{"job":"hyperfine-bench","os":"ubuntu-24.04","runner":"self-hosted"}	185012.7	ops/s	20261112T004203Z-a1b2c3
```

`queue_depth` arrives with `unit_missing`: the honest answer to "what is 3?" is "unknown",
and a scanner that plots it against a unit-bearing metric is making up a y-axis.

### 5 · the spread per commit, successful runs only

```sh
jq -rs '[ group_by([.name, (.labels // {})])[]
         | { metric: .[0].name,
             unit: (.[0].unit // "unit_missing"),
             commits: ([.[] | select(.run_status == "success")]
                       | [ group_by(.commit)[]
                          | { commit: (.[0].commit[0:7]), n: length,
                              min: (map(.value) | min), max: (map(.value) | max) } ] ) } ]
        | .[]
        | [ .metric, .unit,
            (.commits | map("\(.commit): [\(.min),\(.max)] x\(.n)") | join("  ")) ] | @tsv' \
    "$EV"/benchmark/*/*.jsonl
```

```
cold_start_ms	ms	2f7c1a4: [412.5,412.5] x1  3a91c7d: [412.5,412.5] x1  4b02d8e: [412.5,412.5] x1
job_duration_s	s	2f7c1a4: [91.8,91.8] x1  3a91c7d: [91.8,91.8] x1  4b02d8e: [91.8,91.8] x1
queue_depth	unit_missing	2f7c1a4: [3,3] x1  3a91c7d: [3,3] x1  4b02d8e: [3,3] x1
step_duration_s	s	2f7c1a4: [3.066,3.066] x1  3a91c7d: [3.066,3.066] x1  4b02d8e: [3.066,3.066] x1
step_duration_s	s	2f7c1a4: [88.234,88.234] x1  3a91c7d: [88.234,88.234] x1  4b02d8e: [88.234,88.234] x1
step_duration_s	s	2f7c1a4: [0.5,0.5] x1  3a91c7d: [0.5,0.5] x1  4b02d8e: [0.5,0.5] x1
tokenizer_throughput	ops/s	2f7c1a4: [184320.4,185012.7] x2  3a91c7d: [184320.4,185012.7] x2  4b02d8e: [184320.4,185012.7] x2
```

Without the `select(.run_status == "success")`, this tree's `failed` run would add
`job_duration_s: 300.9` next to the success run's `91.8` — the same commit, a 3× "regression"
caused by a workflow that died. §9.

### 6 · percentiles within one commit

act-mcp never aggregates, so this is the scanner's job. Median of the samples of one
`(name, labels)` in one commit:

```sh
jq -rs 'def median: sort | length as $n
        | if $n == 0 then null elif ($n % 2) == 1 then .[($n - 1) / 2]
          else (.[($n / 2) - 1] + .[$n / 2]) / 2 end;
        [.[] | select(.run_status == "success")] | group_by([.name, (.labels // {})])[]
        | [.[0].name, ((.[0].labels // {}) | tojson), (length | tostring),
           ((map(.value) | median) | tostring), (.[0].unit // "unit_missing")] | @tsv' \
    "$EV"/benchmark/"$COMMIT"/*.jsonl
```

```
cold_start_ms	{"job":"hyperfine-bench","step":"Run benchmark"}	1	412.5	ms
job_duration_s	{"job":"hyperfine-bench"}	1	91.8	s
queue_depth	{"job":"hyperfine-bench"}	1	3	unit_missing
step_duration_s	{"job":"hyperfine-bench","step":"Complete job"}	1	3.066	s
step_duration_s	{"job":"hyperfine-bench","step":"Run benchmark"}	1	88.234	s
step_duration_s	{"job":"hyperfine-bench","step":"Set up job"}	1	0.5	s
tokenizer_throughput	{"job":"hyperfine-bench","os":"ubuntu-24.04","runner":"self-hosted"}	2	184666.55	ops/s
```

### 7 · the advisory records, and the ones a plot must exclude

```sh
jq -r 'select((.flags // []) | any(. == "percent_only" or . == "low_confidence"
                                   or . == "unit_missing" or . == "truncated"))
       | [.run_id, .kind, .source, ((.flags // []) | join("+")), (.name // .path // "-")] | @tsv' \
    "${RECORDS[@]}"
```

```
20261110T004203Z-a1b2c3	benchmark	in_band_stdout	unit_missing	queue_depth
20261111T004203Z-a1b2c3	benchmark	in_band_stdout	unit_missing	queue_depth
20261112T004203Z-a1b2c3	benchmark	in_band_stdout	unit_missing	queue_depth
20261110T030900Z-77aa55	coverage	log_scrape	low_confidence+percent_only	total
20261111T030900Z-77aa55	coverage	log_scrape	low_confidence+percent_only	total
20261112T030900Z-77aa55	coverage	log_scrape	low_confidence+percent_only	total
```

### 8 · does the writer keep its dedup promise?

Identity keys per §9, printed for every record; duplicates mean the writer emitted the same
measurement twice:

```sh
jq -r '[.commit, .run_id, .kind, (.name // .scope // "-"), ((.labels // {}) | tojson),
        ((.sample // 0) | tostring), (.path // "-")] | @tsv' "${RECORDS[@]}" \
    | sort | uniq -d
```

```
(empty: 60 records, 60 distinct identities)
```

`sample` belongs in that key — drop it and the two `tokenizer_throughput` samples of one run
look like a duplicate the writer should not have produced.

### 9 · which evidence came from a non-success run

```sh
jq -r '.run_status' "${RECORDS[@]}" | sort | uniq -c
jq -r 'select(.run_status != "success")
       | [.run_status, .commit[0:7], .run_id, input_filename] | @tsv' "${RECORDS[@]}" | sort -u
```

```
      9 failed
     51 success
failed	2f7c1a4	20261110T041530Z-bbc0dd	/tmp/act-mcp-evidence-demo/benchmark/2f7c1a4b9e0d3c6a5b8f1e2d4c6a8b0f3e5d7c91/20261110T041530Z-bbc0dd.jsonl
failed	3a91c7d	20261111T041530Z-bbc0dd	/tmp/act-mcp-evidence-demo/benchmark/3a91c7d2e4f608b1c5d7e9f0a2b4c6d8e0f13579/20261111T041530Z-bbc0dd.jsonl
failed	4b02d8e	20261112T041530Z-bbc0dd	/tmp/act-mcp-evidence-demo/benchmark/4b02d8e3f5a719c2d6e8f0a3b5c7d9e1f2a4b6c8/20261112T041530Z-bbc0dd.jsonl
```

### 10 · the version guard

```sh
jq -r '.schema_version' "${RECORDS[@]}" | sort | uniq -c
jq -c 'select(.schema_version != 1)' "${RECORDS[@]}"
```

```
     60 1
```

The second command printed nothing: every record in the tree is a version this reader
understands. Anything it *does* print is a record to **skip and count**, not to guess about
(§11).

### 11 · the same join from `git log` (a real repository)

This one needs commits that exist — the sample tree's SHAs are fixtures. Against the README's
demo repo ([the five-minute tour](../README.md#the-five-minute-tour)), whose evidence root is
the tree being scanned; `scripts/evidence_recipes.sh <root> --repo <repo>` runs exactly this:

```sh
REPO=/tmp/actmcp-runbook/repo
EV=$REPO/.evidence
git -C "$REPO" log --format=%H | while read -r sha; do
    for kind in benchmark coverage; do
        d=$EV/$kind/$sha
        [[ -d $d ]] || continue
        printf '%s %s %s file(s), %s record(s)\n' \
            "${sha:0:7}" "$kind" "$(find "$d" -name '*.jsonl' | wc -l)" \
            "$(find "$d" -name '*.jsonl' -exec cat {} + | wc -l)"
    done
done
```

```
cf0ecd0 benchmark 1 file(s), 3 record(s)
cf0ecd0 coverage 1 file(s), 4 record(s)
```

Newest first, one line per commit per kind, and no file opened for a commit with no
evidence — that is the whole reason the path is keyed by the full SHA.

## 13. Validating a tree

```sh
cargo run -- validate-evidence "$EV" --stats     # or: target/release/act-mcp validate-evidence …
```

```
benchmark	2f7c1a4b9e0d3c6a5b8f1e2d4c6a8b0f3e5d7c91	11
benchmark	3a91c7d2e4f608b1c5d7e9f0a2b4c6d8e0f13579	11
benchmark	4b02d8e3f5a719c2d6e8f0a3b5c7d9e1f2a4b6c8	11
coverage	2f7c1a4b9e0d3c6a5b8f1e2d4c6a8b0f3e5d7c91	9
coverage	3a91c7d2e4f608b1c5d7e9f0a2b4c6d8e0f13579	9
coverage	4b02d8e3f5a719c2d6e8f0a3b5c7d9e1f2a4b6c8	9
OK 15 file(s), 60 record(s) valid
```

Exit codes: `0` everything valid, `1` findings (each printed as `file:line: reason`), `2`
could not run. It is a **strict** checker by design — unknown field, unknown `kind`,
unknown `schema_version` are all errors — which is the §11 policy applied to the writer's
own output. `python3 scripts/validate_evidence.py` is the same contract in a second
implementation (it needs the `jsonschema` package), and is what CI runs for
second-opinion checking.

---

**Where the rules actually come from** (this document is a restatement, not the source):
[ADR-0002](adr/0002-evidence-jsonl-contract.md) (the contract, in full, with the reasoning),
[schema/evidence.schema.json](../schema/evidence.schema.json) (records),
[schema/run-meta.schema.json](../schema/run-meta.schema.json) (`meta.json`),
[ADR-0004](adr/0004-run-lifecycle.md) (why a cancelled run publishes nothing),
[docs/tool-contract.md](tool-contract.md) (`evidence_files` on the tool result),
[`scripts/evidence_recipes.sh`](../scripts/evidence_recipes.sh) (the recipes, runnable).
