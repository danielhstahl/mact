---
id: ADR-0008
title: the per-commit metric model — coverage % derivation, benchmark grouping and dedup, and what 'action success' means durably
status: accepted
ticket: mact-by1.4
date: 2026-11-16
---

# ADR-0008: the per-commit metric model — how a commit's coverage %, benchmark values and action state are derived, and what each one is allowed to mean

- **Status:** Accepted
- **Date:** 2026-11-16
- **Ticket:** mact-by1.4
- **Decision:** **One row per commit, and every number in it comes out of one named four-stage
  pipeline**: *walk* (`benchmark/` and `coverage/` only, dot-files skipped, directory names are the
  40-hex SHA) → *retain* (only `run_status: "success"`, never a `truncated` run, source precedence
  inside a run, the `fathom.toml` benchmark filter) → *select* (**the newest run per
  `(kind, workflow, task)`**, newest meaning the lexical maximum of `run_id`, which is the
  chronologically newest because ADR-0002 §1.2 puts a UTC timestamp inside the id) → *aggregate*.
  **Coverage** is `rate = Σ hit / Σ found` over the selected runs' **`scope:"total"` records** —
  numerators and denominators summed, never an average of per-file or per-run percentages, never from
  a rounded intermediate — stored to **one decimal**, half away from zero, from the raw counts only. A
  run's own total **is** that run's total and its file records are never re-summed to check it
  (§3.3: the sum invariant is false by design). `found == 0` is **vacuous**: no number, reason
  `vacuous`, excluded from any average — while `hit == 0` with `found > 0` is a real **0.0 %**. A
  `percent_only` or `low_confidence` record is **never a plotted number**: it is a caveat on the row
  and a named entry in the detail view. **Benchmarks**: identity stays
  `(commit, kind, run_id, name, labels, sample)` — `sample` is the noise estimate, not a duplicate —
  the per-commit **group** key is `(name, labels, unit)`, so a metric that changed unit is a
  *different metric*, and a metric with no unit is displayable and **never comparable**: no delta, no
  sparkline, no shared axis. A group's value is the **median over the samples of the newest selected
  run** (even *n* ⇒ mean of the two middle values, which is what `docs/evidence-format.md` recipe 6
  prints), with `min`/`max` over **every retained run** as the spread and `run_count` telling the
  reader how many runs that spread came from; `act_log` metrics are **context**, shown and marked, never a regression signal.
  **Action success is read off the evidence, not asked of the server**: the durable signal is the
  `run_status` field on every record, grouped per `(kind, workflow, task)` with the newest run as that
  group's verdict, and the row state is exactly one of **`success` · `failed` · `mixed` ·
  `no_evidence`**. A commit with no records is **unknown, never "passed" and never "failed"**;
  `runs/<run_id>/meta.json` is never read by the index at all, may be read only by `doctor` and
  `--explain`, always marked advisory, and can never break a render.
- **Reconciled, not inherited:** the ticket's phrasing was "newest run per **(workflow, task)**". That
  is one key short: a run publishes one file *per kind*, so a benchmark run and a coverage run can
  share a `(workflow, task)` — and with a key that omits `kind`, selecting "newest" would let a
  benchmark run supersede a coverage run and delete its coverage. The selection key here is
  `(kind, workflow, task)`, which is the same rule, one column wider (§2.3). The ticket also asked
  what happens when "a total record whose file records do not sum to it" turns up; there is nothing to
  decide there, only a trap to disarm: ADR-0002 §4.2 *documents* that mismatch (instrumented files
  outside the workspace are in the total and in no file record), so a reader must not treat the
  difference as corruption, must not renumber the total, and must not use the sum as a check (§3.3).
  And the ticket's "success (≥1 success run)" is kept as the row state but not as the *column* rule:
  a commit can be `success` overall while its benchmark column is unknown, because a failed run never
  becomes a number (§5.3, §6.1).
- **Rejected:** averaging percentages (per-file, per-run, or per-commit: it is a mean of ratios with
  different denominators, and it moves when an empty generated file appears); treating `found == 0` as
  `0 %` or as `100 %` in a trend line; using `sum(file records) == total` as a validity check;
  picking one "primary" coverage tool per commit and hiding the others (the pooled rate is a
  size-weighted mean, so it cannot lie about the total — §3.2 shows the arithmetic); keeping `unit` out
  of the benchmark group key and filtering incomparable values at render time instead (a comparison
  that has to be remembered is a comparison that will happen); reading `run_status` from the MCP tool
  or from `runs/<run_id>/meta.json` as the durable record; a fifth `unknown` action state (four states
  with `no_evidence` *is* unknown; a fifth would let a render confuse "no records" with "records we
  could not read", which is why the row carries `unreadable_records` separately); carrying a parent's
  number into a child with no evidence; and storing a percentage, a mean or a percentile in
  `.evidence` — that is ADR-0002 §4.3's line and this ADR does not move it. Each with its reason in
  [Considered alternatives](#considered-alternatives).
- **Machine-readable form:** three artifacts, the same shape ADR-0006 and ADR-0007 set.
  [`schema/metric-model.json`](../../schema/metric-model.json) is the one normative spelling of the
  pipeline, the closed vocabularies (18 drop reasons, 7 + 5 gap codes, 12 caveat codes), the
  aggregation arithmetic, the four-state action machine, the glyph table, the forbidden list and the
  row field list; [`scripts/metric_model_reference.py`](../../scripts/metric_model_reference.py) is
  the reference implementation — *the oracle* mact-by1.6's Rust reader is graded against, and it
  **loads** that JSON rather than restating it (`--tree`, `--explain`, `--table`, `--vectors`); and
  [`tests/testdata/metrics/`](../../tests/testdata/metrics) holds 16 golden vectors — a fixture tree
  plus the hand-annotated row it must produce, one per awkward case.
- **Checked by:** [`scripts/validate_metric_model.py`](../../scripts/validate_metric_model.py)
  (C1–C11, run in `scripts/ci.sh`'s `schema` step beside the other ADR checkers; `--self-test` breaks
  one rule at a time in a scratch copy and asserts the check it names is the one that fires). C7 is
  the interesting one: it compares this reference implementation against the **jq recipes' recorded
  output** in `docs/evidence-format.md` §12 — recipes 2 and 6, authored in a different language
  against the same tree — so "the reader agrees with the documented recipes" is a build gate and not a
  hope. C8 is the second one: it re-checks the arithmetic properties (a pooled rate always lies
  between the rates it pooled; a vacuous commit has no percent; a unitless metric is never comparable;
  no row anywhere contains a `null`) over every golden vector *and* the real sample tree.
- **Related:** [ADR-0002](0002-evidence-jsonl-contract.md) (the record contract this derives from:
  §1.1 layout, §1.2 `run_id` sortability, §2.1 unknown-is-absent, §4.2 the outside-workspace total,
  §4.3 percentages are the consumer's, §4.4 dedup and aggregation ownership, §5.1 the `source` and
  `flags` vocabularies), [ADR-0006](0006-fathom-workspace-layout.md) (§6.4's row — `coverage`,
  `benchmarks`, `action_success` and `gaps` are its fields and this ADR owns their meaning; §6.3's
  no-null canonical JSON, which is why a row never carries a null; §7.2's `benchmarks.allow/deny` and
  `history.first_parent`, the two config keys this ADR is allowed to consult),
  [ADR-0007](0007-maintainability-index.md) (the sibling: same "no null, no invented zero", same
  round-half-away-at-one-decimal rule, same reference-plus-oracle testing pattern),
  [ADR-0005](0005-fathom-crate-layout-and-dependencies.md) §7 (fathom is a reader and never a client:
  this ADR's "never ask the server" rule is that pin applied to a question),
  [ADR-0004](0004-run-lifecycle.md) §5–§6 (why a cancelled run leaves nothing to read),
  [docs/evidence-format.md](../evidence-format.md) (the recipes C7 diffs against),
  [docs/tool-contract.md](../tool-contract.md) §5 (the 50-entry in-memory `run_status` listing this
  ADR refuses as a source of truth).
- **Consumers inside the epic:** mact-by1.6 (the reader: implements §2–§5 once, and its `--explain`
  *is* this pipeline's drop log), mact-by1.9 (the timeline: §6's glyph table and gap codes are what it
  renders, and it may not invent either), mact-by1.10 (the store: this ADR's row field list is what
  goes into a row's `coverage`/`benchmarks`/`action_success` sections under §6.4's digests),
  mact-by1.12 (the bench panel: §4's groups, spreads and non-comparability rules), mact-by1.13
  (`fathom report` and `doctor`: same rows, `--fail-on action-failure` reads §5, and §5.5 is what
  `doctor` may say about `runs/`), mact-by1.14 (the detail view: per-run coverage totals, excluded
  runs, caveats by name), mact-by1.15 (the runbook: links this ADR, does not restate it),
  mact-by1.18 (the deferred contract v2 that would let `no_evidence` be split into its three real
  cases — §5.4).
- **Explicitly not this ADR's:** what a record *is* (ADR-0002), where rows live and how they are
  invalidated (ADR-0006), what MI means (ADR-0007), regression *detection* (the epic's non-goal: this
  ADR fixes which numbers exist and which comparisons are legal, not what counts as a regression),
  outlier rejection and percentile statistics beyond the median and the min/max spread (also the
  epic's non-goal — this ADR hands the spread to the UI and stops), and the evidence-contract change
  that would make a run with no evidence visible at all (mact-by1.18).

## 0. The decision, in one screen

```
walk      benchmark/<40-hex>/<run_id>.jsonl, coverage/<40-hex>/<run_id>.jsonl   (nothing else)
retain    run_status == success · not truncated · source precedence · fathom.toml filter
select    newest run per (kind, workflow, task)      newest == max(run_id), which is chronological
aggregate coverage = Σ hit / Σ found over the selected runs' totals
          benchmark = median over the samples of the newest selected run, per (name, labels, unit)
          action    = newest run_status per (kind, workflow, task) → success | failed | mixed | no_evidence
```

The two blocks below are the pinned constants, duplicated from
[`schema/metric-model.json`](../../schema/metric-model.json) on purpose: the checker diffs the copies,
so a change has to be made in both places and noticed.

```metric-model
percent_decimals      = 1
percent_rounding      = half away from zero
percent_input         = raw counts only, never a rounded intermediate
run_status_eligible   = success
selection_key         = kind, workflow, task
newest_by             = run_id
coverage_run_total    = scope_total, else_sum_of_file_records
coverage_pool         = sum of numerators over sum of denominators, per measure
measure_order         = lines, statements, branches, functions
vacuous_found_zero    = no number, reason vacuous, excluded from every average
zero_means_zero       = hit == 0 with found > 0 is 0.0 %
benchmark_group_key   = name, labels, unit
benchmark_value       = median over the samples of the newest selected run
median_even_n         = mean of the two middle values
benchmark_spread      = min and max over every retained run of that group
benchmark_context     = act_log
action_states         = success, failed, mixed, no_evidence
action_source         = the run_status field on every evidence record
action_group_key      = kind, workflow, task
meta_json_in_index    = never
```

```metric-glyphs
unknown        = — | -
caveat_marker  = * | *
context        = ~ | ~
delta_up       = ▲ | ^
delta_down     = ▼ | v
delta_flat     = · | =
success        = ✓ | pass
failed         = ✗ | fail
mixed          = ◐ | mix
no_evidence    = — | -
```

Numbered pins. `MUST` / `MUST NOT` / `SHOULD` are normative; each is either enforced by
`scripts/validate_metric_model.py` (the check is named) or is named as a judgement call.

| # | pin | check |
|---|---|---|
| P1 | A row exists for every commit the git walk yields, and only for those. `.evidence` never decides which commits exist; a commit with no evidence directory is a row with unknown columns, not a missing row. | C4, C8 |
| P2 | Only `benchmark/` and `coverage/` are walked. `runs/` and `incoming/` are never opened by the index, `.<run_id>.jsonl.<rand>.tmp` is skipped, and a directory whose name is not 40 lowercase hex is skipped and named. | C3, C9 |
| P3 | Only `run_status: "success"` records feed a number, and a record from a run carrying `flags: ["truncated"]` never does: what survived a cap is not the run. Every excluded run is still listed in the row (`excluded_runs`, with its status and reason). | C3, C8 |
| P4 | Selection is the newest run per `(kind, workflow, task)`, newest being the lexical maximum of `run_id`. Superseded runs are named in the row, never silently dropped. | C3, C5 |
| P5 | Coverage `percent = round_half_away(100 · Σhit / Σfound, 1)`, computed from the raw counts of the selected runs, and from nothing else. Averaging percentages, or rounding twice, is forbidden. | C7, C8 |
| P6 | A selected run's `scope:"total"` record is that run's total. Its file records are not summed to check it, and file records are summed only when a run published no total — which is then caveated as `total_from_file_sum`. | C3, C5 |
| P7 | One measure per pooled number, chosen by `lines → statements → branches → functions`, first with `found > 0`. Records of any other measure are secondary numbers, stored and displayed, never pooled. | C3, C5 |
| P8 | `found == 0` is vacuous: no coverage number, gap `vacuous`, excluded from every average. `hit == 0` with `found > 0` is `0.0 %` and is a number. The unknown glyph and a zero are never interchangeable. | C8 |
| P9 | `percent_only` and `low_confidence` records never enter the plotted number; they are caveats on the row and named records in the detail view. A commit whose only coverage evidence is scraped shows no percentage at all. | C3, C5, C8 |
| P10 | A benchmark group is `(name, labels, unit)`, an absent unit being its own key. Values from two different groups are never compared, diffed or plotted together, and a group with no unit is displayable only. | C3, C8 |
| P11 | A group's value is the median over its samples in the newest selected run, even *n* being the mean of the two middle values; `min`/`max` span every retained run and `runs` counts them. A stored number is the computed double in shortest round-trip form, never a re-rounded one. | C5, C7 |
| P12 | `act_log` metrics are context: kept, displayed, marked `~`, and never a regression signal by default. An absent `step_duration_s` is absent — not `0 s`, not a hole in a line, not a regression. | C3, C8 |
| P13 | Action success is derived from the `run_status` field on records on disk. Fathom MUST NOT call the server or the `run_status` tool, MUST NOT read `runs/` in the index path, and MUST NOT let a missing `meta.json` fail a render. | C9 |
| P14 | Exactly one of `success`, `failed`, `mixed`, `no_evidence` per commit, by the decision order of §5.3 — and `no_evidence` renders as unknown, never as a pass and never as a failure. | C3, C8 |
| P15 | Rows are keyed by the full 40-hex lowercase SHA and displayed at 7; a delta is against the **first parent** and needs both sides to draw an arrow; a root commit has no delta; a commit never borrows a parent's number. | C4, C10 |
| P16 | Every column with no number carries exactly one gap code, every caveat is one of the 12 declared codes, every drop is one of the 18 declared reasons, and no value anywhere in a row is `null`. | C3, C4, C8 |

## 1. Context

ADR-0002 fixes what a *record* means. This ADR fixes what a **commit** means, because the product is one
row per commit and four programs are going to render it: the timeline (mact-by1.9), the bench panel
(mact-by1.12), `fathom report` (mact-by1.13) and the commit detail view (mact-by1.14). If they each
decide "which of these six coverage records is *the* number", they will decide differently, and a
number that changes depending on which screen you looked at is worse than no number. ADR-0006 §6.4
already reserved the row's `coverage`, `benchmarks` and `action_success` fields for this ADR to fill;
ADR-0007 §4 did the same job one level down, for a metric computed from files.

The facts below were re-derived for this ticket, from `scripts/sample_evidence.py`'s tree (three
commits, fifteen files, sixty records — the same tree `docs/evidence-format.md` is written against) and
from the contract documents, not remembered:

- one commit of that tree — `2f7c1a4…` — has **three coverage runs**, from three different jobs
  (`coverage`, `coverage-istanbul`, `coverage-ad-hoc`), in three different sources (`lcov`,
  `coverage_summary`, `log_scrape`), one of which is a scraped percentage. Its per-run line rates are
  **86.92 %** and **86.15 %** (the third has no counts at all). Any rule that picks one of them is a
  policy; the rule here is arithmetic (§3.2).
- the same commit has a `benchmark` run that **failed** and one that succeeded. Filtering by
  `run_status` is what stops a 3× "regression" from a run that died (`docs/evidence-format.md` recipe
  5 says exactly this), and it is also what makes "did this commit pass?" a *separate* question from
  "what is its coverage?" (§5).
- `run_status` as a *tool* cannot answer the question at all: the listing is in-memory, newest-first,
  **capped at 50**, and an evicted id is `run_not_found` (`docs/tool-contract.md` §5). Three weeks and
  fifty runs later, the tool has forgotten the commit you are looking at.
- `runs/<run_id>/meta.json` *could* answer it, and ADR-0002 §1.3 says in as many words that it is
  debug and audit context and **not** the load-bearing contract — and `runs/` is not commit-keyed, so
  using it means scanning every run directory to place one commit.
- the contract already carries the answer, on every single line: the envelope's
  `run_status ∈ success|failed|cancelled|timed_out|error`, guaranteed, schema-validated, on disk forever
  (ADR-0002 §2.2). Evidence from a non-success run is **written, not suppressed**, precisely so that a
  consumer can filter with it (§9 of the format doc). That is the durable signal, and this ADR uses
  nothing else.
- `fathom.toml` already declares the only two knobs this derivation may consult — `benchmarks.allow`
  and `benchmarks.deny` (ADR-0006 §7.2, "filters what is kept, not what is plotted") — and
  `history.first_parent`, which narrows the walk but does not choose a parent for a delta.
- every "unknown" in this model has to be *absence*, not a sentinel: ADR-0002 §2.1 forbids `null` in a
  record, ADR-0006 §6.3 forbids it in a stored row's canonical JSON, and ADR-0007 §3.5 refused to let
  an unmeasurable file be `0`. A coverage percentage is the third place this bites (§3.4).

## 2. The pipeline: walk, retain, select, aggregate

Four stages, in this order, always. Each stage's job is one sentence, every record read is either
retained or dropped with exactly one reason code, and the drop log *is* the `--explain` output mact-by1.6
is asked to provide. That is deliberate: a rule set nobody can interrogate is a rule set nobody
believes.

```
walk      which bytes are read at all
retain    which records a number may be built from        (per record)
select    which run is the newest for its identity        (per group)
aggregate the arithmetic that produces the row's numbers
```

### 2.1 Walk

`<root>/<kind>/<full-sha>/<run_id>.jsonl` for `kind ∈ {benchmark, coverage}`, and nothing else —
ADR-0002 §1.1. In particular: not `runs/` (act's own log is not a record and the format doc calls out
this exact mistake, because it produced plausible output), not `incoming/`, not a dot-file
(`.<run_id>.jsonl.<rand>.tmp` is a crashed publish), and not a directory whose name is not 40
lowercase hex. A file whose last byte is not `LF` is a torn write and its records are not read.

That "40 lowercase hex or skip" rule is also the answer to the short-SHA question in §6.2: the join key
is the directory name, the record's `commit` field is asserted against it, and a mismatch is a drop
with a reason, never a coercion.

### 2.2 Retain

A record is retained when its run was **successful** (`run_status ∈ retention.run_status_eligible`,
which is `["success"]` and nothing else) and did not hit the metric cap (`flags` has no `truncated`,
because ADR-0002 §5.1 says a consumer must never sum what survived a cap and call it the run). For
benchmark records, the `fathom.toml` `benchmarks.deny`/`allow` filter applies here (deny wins), and so
does ADR-0002 §5.1's source precedence *within* a run and measurement: `job_file` >
`in_band_stdout` > `act_log`, with `sample` part of the measurement identity so two samples of one
`(name, labels)` are not one measurement seen twice.

Every non-retained record is dropped with exactly one reason from this closed list — 18 codes, each
belonging to one stage. They are the vocabulary of `--explain`, of `doctor`'s evidence checks, and of
any bug report that says "the number is wrong".

| code | stage | why a record does not become a number |
|---|---|---|
| `not_a_record_path` | walk | the path is not `<kind>/<40-hex>/<run_id>.jsonl` |
| `temp_file` | walk | a dot-file is a crashed publish, never a record (ADR-0002 §1.1) |
| `directory_not_sha` | walk | directory name is not 40 lowercase hex: short SHA, mixed case, foreign name |
| `schema_unsupported` | retain | a newer `schema_version` than this build reads: skip and count, never guess (ADR-0002 §7) |
| `malformed` | retain | not strict JSON, not one object, a missing envelope field, `kind` disagreeing with its directory, or a torn final line |
| `commit_mismatch` | retain | the record's `commit` is not the directory it was read from |
| `bad_run_id` | retain | `run_id` does not match ADR-0002 §1.2, so it cannot be sorted and cannot be selected |
| `run_status_excluded` | retain | `run_status` is not `success` |
| `run_truncated` | retain | the run hit the per-run metric cap |
| `config_filtered` | retain | `benchmarks.allow` / `benchmarks.deny` (ADR-0006 §7.2) |
| `source_shadowed` | retain | same run, same measurement, lower-precedence `source` (ADR-0002 §5.1) |
| `superseded` | select | an older run of the same `(kind, workflow, task)`: kept for the spread, never the headline |
| `percent_only` | aggregate | a scraped percentage with no raw counts: a caveat, not a number (ADR-0002 §4.3) |
| `low_confidence` | aggregate | text-scraped: a caveat, not a number |
| `vacuous` | aggregate | `found == 0`: the ratio is undefined, and 0/0 adds nothing to a pool |
| `measure_not_primary` | aggregate | this record measures `statements` when the commit's primary measure is `lines` |
| `no_counts` | aggregate | a coverage record with no complete count pair at all |
| `file_under_total` | aggregate | a file record of a run that also published a total, which is that run's number (ADR-0002 §4.3) |

A record that is *readable but unusable* is counted as `unreadable_records` on the row. That matters
only for one question — "is this commit unknown because nothing ran or because I cannot read what ran"
— and it is worth answering separately (§5.4).

### 2.3 Select

Per `(kind, workflow, task)`, the run with the **greatest `run_id`** is the selected one; the others are
`superseded`. Because `run_id` is `YYYYMMDDTHHMMSSZ-<6 hex>` (ADR-0002 §1.2), a lexical maximum *is* the
chronologically newest run, so no timestamp parsing is involved and no clock skew can reorder two runs —
the writer minted the id. Ties are impossible: a `run_id` is unique per run, and one file is exactly one
run (§1.2 again).

The three questions the ticket asked about "newest" all resolve here:

- **a commit with three coverage runs of the same job** keeps one of them for the number and names the
  two losers as `superseded_runs`. A re-run of a coverage job replaces the number, because it is a
  better measurement of the same thing; the losers are not deleted, because a re-run that *disagrees*
  is information.
- **a commit with two coverage workflows** (or one workflow with two jobs — `coverage` and
  `coverage-istanbul`, which is what the sample tree has) selects **both**, one per group, and the
  pooled rate §3.2 is what combines them. They are not duplicates of one measurement; they are two
  measurements of two file sets.
- **a run that publishes both kinds** is why `kind` is in the key at all: one run may publish
  `benchmark/<sha>/<run_id>.jsonl` *and* `coverage/<sha>/<run_id>.jsonl`, and a key without `kind`
  would let one of those two files supersede the other.

### 2.4 Rows come from git, not from `.evidence`

The set of rows is the set of commits the git walk yields (ADR-0006 §7.2's `history.*`), and the
evidence tree only fills them in. This is stated because the opposite is tempting — `ls coverage/` is
one syscall — and wrong: it would make a commit with no evidence *disappear* from the timeline instead
of showing `—`, which is exactly the "silently missing is worse than unknown" failure this ADR refuses
everywhere else. mact-by1.6 therefore takes a list of SHAs, not a directory listing.

## 3. Coverage %

### 3.1 The rate is the consumer's to compute, and only one formula is allowed

ADR-0002 §4.3 is already explicit: evidence stores numerators and denominators, never a rate, because
two tools round differently and we refuse to arbitrate. So Fathom computes the rate, and there is
exactly one legal formula:

```
percent = round_half_away( 100 · Σ hit / Σ found , 1 )
```

summing **numerators and denominators separately** over the records the selection kept, per measure.
Three things follow, and each of them is a thing a plausible implementation does wrong:

- **never the mean of per-file percentages.** A 512-line generated file at 100 % and an 88-line file at
  0 % would otherwise outvote a 2 617-line parser module at 84 %. In the sample tree that is the
  difference between 86.92 % and the *mean of that run's four file rates*, 70.09 % — a 16.8-point error
  made by arithmetic that looks reasonable.
- **never the mean of per-run percentages either.** `2f7c1a4…`'s two count-bearing runs are 86.9175 %
  (3581/4120, lcov) and 86.1539 % (1904/2210, istanbul). Their mean is 86.5357 → **86.5**; the pooled
  rate is 5485/6330 = 86.6509 → **86.7**. The decision is worth a fifth of a point in a column whose
  arrows are drawn at a tenth of a point, which is the sort of difference a CI gate would trip on.
- **never from a rounded intermediate.** `86.6509…` rounds to 86.7; the recorded recipe output
  `86.92`/`86.15` (two decimals) is a *display*, and re-rounding a displayed number is how
  86.949999 turns into 87.0. The stored `percent` is derived from the counts and only once.

Rounding is **half away from zero** at one decimal — the same rule ADR-0007 §2.2 pinned for MI, so the
two columns round alike — and `percent_decimals` is **1**, which is what ADR-0006 §6.4's row already
shows (`"percent": 82.4`) and what the timeline's column width assumes.

### 3.2 Which records, and why pooling is safe across tools

The inputs are the **selected** runs' `scope:"total"` coverage records (§2.3), each contributing one
`(hit, found)` pair for a given measure. Pooling across those runs is a **size-weighted mean of their
rates**, which is the property that makes it safe to use without knowing whether two jobs measured the
same files or disjoint ones:

> For any set of runs with `found > 0`, the pooled rate lies between the smallest and the largest of
> their individual rates. Here: 86.1539 ≤ **86.6509** ≤ 86.9175.

So a pool can neither invent coverage nor hide a drop, and it degrades sensibly in both directions:
two jobs measuring disjoint halves of the repository produce the repository's coverage; two jobs
measuring the same thing produce the more heavily weighted estimate of it, which is the better one to
have. What pooling does *not* do is pick a fight between `lcov` and `coverage_summary` — nothing here
claims one tool is right, and both remain individually visible in the detail view with their run ids.
Silently preferring one source is on the forbidden list.

Two corollaries worth pinning because they are everyday cases:

- a **vacuous run is a no-op in a pool**: 0/0 adds nothing to either sum, so the "exclude
  zero-denominator entries" instruction of ADR-0002 §4.3 is satisfied by construction instead of by a
  special case. The special case is only needed when *every* denominator is zero (§3.4).
- **a `truncated` run is excluded, not pooled**: the cap means the run's own counts are incomplete, so
  summing what survived would understate `found` and inflate the rate — and ADR-0002 §5.1 forbids
  exactly that reading.

### 3.3 A run's total is that run's total

A selected run's `scope:"total"` record **is** its total. Its file records are not summed to check it.
ADR-0002 §4.2 documents why the two can differ — a tool that instrumented files outside the workspace
(`node_modules` is the everyday case) counts them in the total and can give them no workspace-relative
path, so they get no file record — and the extractor reports the count it dropped
(`coverage_paths_outside_workspace`) rather than renumbering the total. The golden vector
`outside_workspace_total` fixes the arithmetic: total 4000/5000, file records summing 3000/4000 ⇒
**80.0 %**, not the file sum's 75.0 %, and no warning.

When a run publishes **no** total record, its file records are summed and the row carries the caveat
`total_from_file_sum`: that total is a reader's construction, not the tool's claim, and a viewer
entitled to know the difference. `vector: total_absent` pins it (150+50 over 300+100 = **50.0 %**).

### 3.4 `found == 0` is vacuous, and `0.0 %` is a number

Two different states, two different renderings, and never the same glyph:

| state | counts | coverage in the row | glyph | in a window average |
|---|---|---|---|---|
| measured | `hit 3581 / found 4120` | `percent: 86.9` | `86.9` | yes |
| real zero | `hit 0 / found 500` | `percent: 0.0` | `0.0` | yes |
| vacuous | every pooled `found == 0` | **absent**, `gaps.coverage: "vacuous"` | `—` | **no** |
| nothing to pool | no eligible record | **absent**, one of the other gap codes | `—` | no |

ADR-0002 §4.3 allows a consumer that *must* print a number to print `100 %` for a vacuous entry; a
trend line is not that consumer, and printing 100 % for "an empty generated file was the only thing
instrumented" would be the single most misleading thing this column could do. ADR-0007 §3.5 refused
the same trap for MI with `ln(0)`; this is `0/0` wearing different clothes. `vacuous` and
`zero_is_zero` are golden vectors precisely because a one-line change would blur them.

### 3.5 Measures: one per number, secondary numbers stored

A coverage record can carry several count pairs — an istanbul total has lines, statements, branches and
functions. Percentages of different measures are different measurements (ADR-0002 §4.2: istanbul
*statements* are not lcov *lines*), so one pooled number never mixes them:

- the **primary measure** is the first of `lines → statements → branches → functions` whose pooled
  `found > 0`. It is what `coverage.percent` is, and `coverage.measure` names it.
- every other measure with pooled `found > 0` is pooled the same way and stored in
  `coverage.secondary` — for `two_coverage_tasks` that is `statements 86.1`, `branches 80.1`,
  `functions 89.9` beside the primary `lines 86.7` — with the caveat `secondary_measures`.
- records of a non-primary measure are dropped with `measure_not_primary`, which is a *stage-4* drop:
  they were selected and retained; they simply do not belong in this particular number.

`lines` first is not a preference for Rust. It is the only measure every supported format reports
(lcov's `LF`/`LH` and istanbul's `total.lines`), so it is the only one a multi-tool pool can use
without being a weighted mean of two different units.

### 3.6 Scraped percentages are caveats, never the plotted number

A `log_scrape` record with `percent_only` (and therefore `low_confidence`) says what a tool *printed*,
not what it measured, and it has no denominator to pool. So: excluded from `percent`, recorded as the
row caveats `percent_only` and `low_confidence`, and listed in the detail view with its
`reported_percent`, `reported_measure` and the run that produced it. A commit whose *only* coverage
evidence is scraped shows **no percentage** — `—*`, gap `only_percent_only` — and its action state can
still be `success`, because "the job ran" and "the number exists" are different columns (§5). Inventing
a denominator to make 87.4 look poolable is forbidden by ADR-0002 §4.3 and repeated here because the
temptation is right there in the data.

### 3.7 `run_status`, and where the excluded runs still show

Only `success` feeds the number (§2.2). The others — `failed`, `cancelled`, `timed_out`, `error` — are
written on purpose (ADR-0002 §4.4: a dip caused by a broken run should be *visible and filterable*, not
missing), so they must be visible and filtered, which is what `excluded_runs` is for: one entry per
excluded run with its `status` and the reason it was excluded, and the row caveat `run_excluded`. The
detail view (mact-by1.14) and the bench panel's failed-run overlay (mact-by1.12) render them;
`fathom report` can print them; the timeline does not let them become a number.

The gap codes for this column, first match wins, are closed:
`no_evidence` (nothing readable at this SHA) · `unreadable` (records present, none readable) ·
`all_runs_excluded` (runs happened, none eligible) · `only_percent_only` (only scraped evidence) ·
`vacuous` (every denominator was 0) · `no_counts` (a record with no count pair) · `no_records`
(nothing of this kind). The point of naming all seven is that `—` is one glyph with seven meanings, and
a user standing in front of it is entitled to the difference.

### 3.8 Worked example, from the real sample tree

```sample-row
commit              = 2f7c1a4b9e0d3c6a5b8f1e2d4c6a8b0f3e5d7c91
coverage.percent    = 86.7
coverage.hit        = 5485
coverage.found      = 6330
coverage.measure    = lines
coverage.run_count  = 2
coverage.secondary  = statements 86.1; branches 80.1; functions 89.9
benchmarks.groups   = 7
benchmarks.median   = tokenizer_throughput 184666.55
benchmarks.min/max  = tokenizer_throughput [184320.4, 185012.7]
benchmarks.samples  = tokenizer_throughput 2
action_success      = mixed
excluded_runs       = 1
caveats             = percent_only; low_confidence; secondary_measures; context_only; unit_missing; run_excluded
gaps                = none
```

That is 3581+1904 over 4120+2210; seven benchmark groups of which `queue_depth` has no unit and four
are `act_log` context; `mixed` because the coverage jobs succeeded and the benchmark job did not; one
failed run excluded and named; six caveats; and no gaps, because both columns produced numbers. The
checker re-derives every line of that block from `scripts/sample_evidence.py` (C6), so the example
cannot rot.

Two of those labels are counts or summaries rather than row fields — `coverage.run_count` counts
`coverage.runs` (which is ADR-0006 §6.4's list of selected run ids), `benchmarks.groups` counts the
groups, and `benchmarks.median` / `benchmarks.min/max` / `benchmarks.samples` name one group's
`value`, `min`/`max` and `samples`. The distinction is written down because a benchmark group's
`run_count` *is* a row field and is a different kind of thing from `coverage.runs`, and a reader who
conflates them will write a row that cannot be parsed.

## 4. Benchmark results

### 4.1 Identity, and why `sample` stays in the key

ADR-0002 §4.4 gives the record identity — `(commit, kind, run_id, name, labels, sample)` — and this ADR
keeps it, including `sample`. A benchmark job that runs hyperfine three times emits three records with
the same `(name, labels)` and different values; they are the **noise estimate**, and a dedup that
collapses them leaves a single number where a spread used to be, which is the one case where "cleaning
the data" destroys the only information that made it interpretable. `even_median_and_spread` exists so
this cannot be "fixed" later: two samples in, `samples: 2` and a median between them out.

Within one run, the *measurement* identity for source precedence is `(name, labels, unit, sample)`;
duplicates of exactly that within one run are the writer's `malformed` case and never arise from a
well-behaved writer.

### 4.2 The group, the value, and the spread

The per-commit group key is **`(name, labels, unit)`**, where an absent unit is its own key value
(`unit_missing`). Two rules make up the whole of the number:

| field | how it is computed |
|---|---|
| `value` | the **median** of the samples of that group in the **newest selected run** for the group's `(kind, workflow, task)` |
| `samples` | how many records of that group that newest selected run published |
| `min`, `max` | the spread over **every retained run** of that group at this commit — superseded runs included |
| `run_count` | how many distinct retained runs contributed to that spread (`coverage.runs` is a list of run ids; this is a count, and the two names are different on purpose) |
| `run_id`, `source`, `flags` | the newest selected run's, since that is the run `value` came from |

Deliberately asymmetric: the headline is *one run* (the newest), while the error bar spans *all of them*
(mact-by1.12's "spread(3 runs)"). A value from one run with a spread from three is not a
contradiction — it is "best current estimate, and here is how much the runs disagree".

Tie and missing-value rules, spelled out because two implementations will otherwise differ:

- **even sample count:** the mean of the two middle values of the ascending sort, which is what
  `docs/evidence-format.md` recipe 6 does and prints — `(184320.4 + 185012.7) / 2 = 184666.55`. A
  median is then allowed to be a number no sample ever had; that is what a median is.
- **stored form:** the computed binary64 value in **shortest round-trip** decimal (what Rust `{}`,
  Python `repr` and jq 1.7.1 all print), never re-rounded. 184666.55 above is not a coincidence: it is
  the same double printed three ways. Renderers use 4 significant digits at draw time (ADR-0006 §6.4
  stores, the view formats); storage never rounds, because a rounded stored value cannot be averaged or
  diffed later without a second rounding error.
- **a group absent at this commit is absent.** No `0`, no carry-forward from the parent commit, no
  interpolation across commits to "fill" a sparkline. A metric a job stopped reporting stops being
  plotted, and the panel says when it last appeared.
- **`runs: 1` with `min == max` is an unmeasured noise floor,** not a stable metric. The panel labels
  it; a delta off one sample is "insufficient samples", never a percentage.
- **labels are compared verbatim and canonically** (keys ascending, no whitespace — ADR-0002 §6.1's
  rule). `{job:"bench",os:"x"}` and `{job:"bench"}` are two groups, because they are two measurements
  of differently-described things, and a `group by` that type-switches or order-sensitive on labels is a
  bug. Labels stay flat string→string, so equality is enough.

### 4.3 Units: part of the identity, and the only honest non-comparability rule

ADR-0002 §5 is blunt: act-mcp never converts, scales or invents a unit, and an absent `unit` always
comes with `flags: ["unit_missing"]`. Putting `unit` **in the group key** is the mechanism that makes
mis-comparison impossible rather than forbidden:

- a value can only ever be diffed or plotted against values in its own group, and same group ⇒ same
  unit by construction;
- a metric whose unit *changed* between commits is **two metrics** — `queue_depth` in `ms` and
  `queue_depth` in `s` are not a trend, they are two questions — and `unit_missing_and_changed` pins
  that the two rows are independent and that the second is `comparable: true` while the first is not;
- a group with no unit is `comparable: false`: it is displayable (3 is a fine number to show a human),
  and it gets **no delta arrow, no sparkline, and no place on a shared axis**. The UI must not draw a
  delta for it — mact-by1.12's "display only, not comparable" line is this rule.
- different metric names never share an axis either. Comparing `cold_start_ms` to `tokenizer_throughput`
  because they happen to be numbers on the same screen is the same category of error.
- `coverage`'s equivalent is the measure rule of §3.5: `lines` is not `statements`.

### 4.4 `act_log` metrics are context, and absence is absence

Every benchmark run also publishes `job_duration_s` and `step_duration_s` from act's own timings
(ADR-0002 §5.3), so a job that measures nothing still leaves a row. Those rows describe **the run**,
not the code: they are kept, displayed, marked `context: true` (glyph `~`), and never a regression
signal by default. If a job reports its own `job_duration_s`, the job's value wins and act's is dropped
as `source_shadowed` — the writer's precedence rule applied to whatever survived its dedup
(`act_log_context` is that vector).

`step_duration_s` needs act's `executionTime`, which act gained in **0.2.76**, while this server accepts
act from 0.2.62 (docs/evidence-format.md §5). So runs on 0.2.62…0.2.75 publish **no `step_duration_s` at
all**, and the row must say *absent*, not `0 s`, not a hole in a line, and above all not a regression —
an act upgrade would otherwise look like a 90-second speedup on every commit at once. The rule is the
general one: absence renders the unknown glyph with its gap code, and the view may *note*
`tool.act_version` as the likely reason when it is present, but must not infer a cause from an absence
(`act_step_timings_unavailable` is a run *warning* — docs/tool-contract.md §6 — recorded on the run's
`meta.json`, which the index path does not read (§5.5)).

### 4.5 What a row keeps, and what the timeline column shows

The row keeps **every group** that survives the `fathom.toml` `benchmarks.allow`/`deny` filter — default
allow-all, because Fathom does not hide evidence by default (ADR-0006 §7.2). A commit that reports
fourteen metrics stores fourteen; narrowing that is configuration, and configuration invalidates the
`evidence` section of the affected rows and nothing else.

A timeline column cannot show fourteen, so the single `bench` cell shows **one** group, chosen
deterministically: sort the row's groups by `(name, canon(labels))` and take the first that is
comparable and not `context`, else the first of any; render its value at 4 significant digits with its
unit, prefix `~` if it is context, append `+N` for the rest, and append `*` if that column carries any
caveat — `412.5ms +6*` for the sample tree, whose other six groups include a unitless metric.
Full detail is one keypress away (mact-by1.12). Which group that is must be *stable across commits*,
which is why the sort is by name and not by "biggest delta": a column whose displayed metric changes
with the data is a column that silently changes what it means.

## 5. Action success — the durable question

### 5.1 The trap

"Did commit `9f31a2c` pass?" is the question with the shortest shelf life in this product, and three
available answers are all wrong:

- **the `run_status` tool**: in-memory, newest-first, **capped at 50**, and `run_not_found` after
  eviction (docs/tool-contract.md §5). It is a *live run* API, and it is being asked about history.
- **`runs/<run_id>/meta.json`**: it does have `status`, and it is written last so it is a decent
  terminal manifest — and ADR-0002 §1.3 says it is debug and audit context, **not** the contract. It is
  also not commit-keyed, so answering "for this commit" means walking every run directory.
- **asking the server at render time**: forbidden outright — ADR-0005 §7 makes Fathom a reader of files
  and never a client, and a TUI whose columns depend on a live server is a TUI that cannot be used on a
  plane or diffed in CI.

### 5.2 The durable signal

The envelope's `run_status` on every evidence record. It is required, schema-validated, guaranteed
present (ADR-0002 §2.2), on disk under the commit's own directory, and it survives the server, the
machine and the memory cap — because it *is* the artifact. The enum is closed:
`success | failed | cancelled | timed_out | error`, and only `success` counts as a pass.

This is also why mact-by1.18 (a durable run-outcome record for runs that emit **no** evidence) is a
contract change and not a feature: today, "ran and produced nothing" leaves no trace in the contract at
all. Until it lands, the honest answer for such a commit is `no_evidence`, and the honest UI text is
"no evidence", not "passed" and not "failed".

### 5.3 The four states, and the order that makes them exclusive

Group the readable records of the commit by `(kind, workflow, task)` — the same key as §2.3 — and take
each group's **newest** run's `run_status` as that group's verdict. Then, **in this order**:

| # | condition | state | glyph | ascii |
|---|---|---|---|---|
| 1 | no records at all for this SHA, under either kind | `no_evidence` | `—` | `-` |
| 2 | every group's verdict is `success` | `success` | `✓` | `pass` |
| 3 | no group's verdict is `success` | `failed` | `✗` | `fail` |
| 4 | some are and some are not | `mixed` | `◐` | `mix` |

Mutually exclusive and exhaustive by construction, and the row also carries `action_detail` — one entry
per group with `kind`, `workflow`, `task`, `category`, `status`, `run_id` and whether that group is what
drove the state — so the glyph is always explainable by opening the detail view.

Three consequences worth stating:

- **a re-run is a pass, not a mixed.** A failed run followed by a successful re-run of the same
  `(kind, workflow, task)` verdicts on the *newest*, so the row says `success` — and the failed run is
  still in `excluded_runs` (that is `retry_passes`, and it is the behaviour people expect from a CI
  badge and the opposite of what a naive "any failure ⇒ failure" rule gives).
- **`mixed` is the common case, not an error state.** The sample tree's three commits are all `mixed`:
  coverage succeeded, the benchmark job failed. `mixed` means "some workflows passed and some did not",
  which is exactly the truth, and the columns stay independent: mact-by1.9's `action` glyph says
  nothing about why the `bench` cell is `—`, and vice versa.
- **`category` is the grouping that says *which* column failed.** In v1 it equals `kind`
  (ADR-0002 §2.2), so the grouping is coverage / benchmark / other; the row keeps it verbatim so that
  when the two diverge the UI does not have to be rethought.

### 5.4 No evidence is unknown, and there are three kinds of it

`no_evidence` covers three real situations that the contract cannot currently distinguish: no workflow
ever ran for this SHA; a run was **cancelled before publishing** (`cancel_run` stops extraction, so
`meta.json` says `cancelled` and no JSONL is published — ADR-0002 §1.1, ADR-0004 §5–§6); and a run
**succeeded and extracted nothing** (its `meta.json`'s `evidence[]` would say so, and the index may not
read it). What Fathom shows for all three is the unknown glyph with `gaps: {coverage: "no_evidence",
benchmarks: "no_evidence"}`, and what it must **never** show is a pass, a failure, a `0 %`, or a `—`
that reads like the commit broke. Where the distinction *is* knowable — a run directory that published
nothing — `doctor` may say so, as advisory (§5.5). What is *not* unknown is a commit whose records exist
but this build cannot read: that row is still `no_evidence` and additionally carries
`unreadable_records: N` with the `unreadable` caveat, because "I could not read it" and "nothing ran"
lead to different user actions (`unreadable_only` is that vector).

### 5.5 `runs/<run_id>/meta.json`: advisory, opt-in, and never load-bearing

- **the index never reads `runs/`.** Not for action success, not for coverage, not for benchmarks. P2,
  and it is why a scan of `.evidence` stays `stat`-per-commit-cheap.
- **`fathom doctor` and `fathom index --explain` may** read it, to answer the one question the contract
  cannot: "there *was* a run for this commit; it published nothing, and here is its `warnings[]`".
  Anything so derived is labelled **advisory** in the output — same word ADR-0002 uses for `extra` and
  `host` — and is never written into a row's contract fields, so no cached number ever depends on a file
  outside the contract.
- **a missing, unreadable or renamed `meta.json` can never break a render.** Nor can a `meta.json` that
  disagrees with the evidence: where they disagree, the evidence wins and `doctor` reports the
  disagreement. A directory with no `meta.json` means act-mcp died mid-run (ADR-0002 §1.3) — worth
  printing in `doctor`, worth nothing in a column.

## 6. Cross-cutting display rules

### 6.1 Unknown, zero, empty — three states, and 0 always means zero

Every number in a row is in exactly one of three states, and the rendering says which:

| state | meaning | in the row | on screen | in JSON |
|---|---|---|---|---|
| **unknown** | no measurement exists | key absent, one gap code | `—` | `null` (mact-by1.13's report), `-` in TSV/CSV |
| **empty / vacuous** | measured, but the measurement is undefined (`found == 0`) | key absent, gap `vacuous` | `—` | `null`, never `0` |
| **zero** | measured, and the value is zero | `0`, `0.0` | `0.0`, `0 s` | `0` |

The rule is inherited verbatim from ADR-0002 §2.1 and ADR-0007 §3.5 and it is not negotiable: **a `0`
in this product is a measurement, never a placeholder.** It is why `percent: 0.0` and "no `percent`
key" are different rows of the table above, why an absent `step_duration_s` is not `0 s`, and why a
report's TSV must never emit an empty field a spreadsheet reads as zero.

Glyphs, pinned (the `schema/metric-model.json` table is normative; this is the same table):

| glyph | ascii | means |
|---|---|---|
| `—` | `-` | unknown, in any column, including `action` for `no_evidence` |
| `*` | `*` | caveat marker: this column's value has caveats, named in the detail view; on an unknown column it says the gap behind the `—` has caveats of its own |
| `~` | `~` | context value (`act_log`-derived), shown, not a measurement of the code |
| `▲` `▼` `·` | `^` `v` `=` | delta vs the first parent: up, down, exactly equal at the stored precision |
| `✓` `✗` `◐` | `pass` `fail` `mix` | action state; `—`/`-` is the fourth (`no_evidence`) |

Rules that go with them: no signal is carried by colour alone (`NO_COLOR` and `TERM=dumb` are supported
terminals, mact-by1.9); an arrow requires both sides, so no parent value means no arrow and `·` is
reserved for a real tie at the stored precision; `--plain`, `fathom report --format tsv` and log files
use the ascii column, because a CI log that needs a unicode terminal is a CI log nobody reads; and a
`*` must always have caveats behind it — a marker with nothing to show is a bug in the derivation, not
decoration.

The `*` stands for one of twelve codes, closed, on the row's `caveats` list. The timeline shows one
glyph; the detail view, `--explain` and `fathom report --format json` name them, which is the whole
reason the codes exist. One more field pairing is pinned here because two ADRs touch the same object:
ADR-0006 §6.4 already puts `no_evidence`, `no_mi` and `unanalysed_paths` in the row's `gaps`, and those
three keys are theirs, not this ADR's — the per-column codes above sit beside them, additively, and
`gaps.no_evidence` is true exactly when `action_success == "no_evidence"`, which is the one equality
between the two spellings that a checker can test (C4).

| caveat | column | what it says about the number |
|---|---|---|
| `percent_only` | coverage | a scraped percentage was present and was not used (§3.6) |
| `low_confidence` | coverage | text-scraped evidence was present and was not used (§3.6) |
| `vacuous` | coverage | at least one pooled run had `found == 0`; the ratio is undefined for it |
| `total_from_file_sum` | coverage | a run published no total, so its total is the reader's sum of its file records (§3.3) |
| `secondary_measures` | coverage | `coverage.secondary` holds numbers of another measure (§3.5) |
| `measure_not_primary` | coverage | a record measured something other than the primary measure and was left out of the pool |
| `run_excluded` | either | `excluded_runs` names runs that happened and were not used (§3.7) |
| `superseded_runs` | either | `superseded_runs` names older runs of a group whose newest won (§2.3) |
| `unreadable` | either | `unreadable_records` counts records present that this build could not read (§5.4) |
| `unit_missing` | benchmarks | a group has no unit: displayable, never comparable (§4.3) |
| `source_changed` | benchmarks | a group's runs across this commit came from different `source`s, so the spread crosses precedence levels (§4.1) |
| `context_only` | benchmarks | a group is `act_log`-derived context, not a measurement of the code (§4.4) |

### 6.2 The row key is the full 40-hex SHA

Directory names under `.evidence` are full lowercase SHAs (ADR-0002 §1.1), `git log --format=%H` yields
them, and ADR-0006 §6.4 keys a row file by the same 40 hex. So the join key is the full SHA, the row
filename is the full SHA, and **display truncates to 7** — with the short form accepted as *input* to a
filter (mact-by1.9) and resolved to a full SHA before anything is looked up. A short-SHA path join is
not a shortcut: it is correct until the day the repository grows a collision inside the first 7 hex, and
then it silently attributes one commit's evidence to another. Matching a record's `commit` field against
the directory name is the check that catches a hand-moved tree, and it is a drop with a reason
(`commit_mismatch`), not a coercion.

### 6.3 Parents: a root, a merge, and the number never borrowed

A row's absolute values are its own; some of its interesting cells are differences against a parent, so
"parent" is defined once, here, for all of them (coverage delta, benchmark delta, ADR-0007 §4.4's
`delta_vs_parent`, mact-by1.11's "vs parent" header):

- **the parent is the first parent** — `git log --format=%P`'s first field. It is the line history was
  merged *onto*, which is the diff a reviewer of a merge commit actually wants and the one that matches
  `git show`'s default. `history.first_parent = true` (ADR-0006 §7.2) narrows the *walk* to the first
  parent chain; it does not change which parent a delta is against, and it must not be allowed to.
- **a root commit has no parent**, so it has no delta: the delta fields are **absent**, not `0`, not the
  value itself, not "vs an empty tree". Its absolute values still render normally — `86.7` with no arrow
  — and a root commit is never a regression against nothing.
- **a merge commit is judged on its own evidence.** Its evidence is whatever ran on that SHA; its diff is
  against its first parent; its additional parents are displayable in the detail view and are never a
  delta base. A merge commit whose only runs happened on the merged branch has *no evidence for its own
  SHA*, and therefore shows `no_evidence` — it does **not** inherit the second parent's numbers.
- **no inheritance, ever, in either direction.** A commit with no evidence never borrows a parent's, a
  child's or a sibling's coverage or benchmark value, and no metric is carried forward over history
  (the same rule ADR-0007 §4.3 pins for the MI aggregate). A number attributed to a commit that never
  ran is a lie with a SHA on it — and a stale number is exactly what makes a timeline look reassuring
  while a repository rots.

### 6.4 Same tree, same number

Identical evidence on two commits prints identical numbers, and this model never smooths, interpolates
or deduplicates *across* commits to make a line look more interesting: the sample tree's three commits
have identical coverage and therefore show it — 86.7 three times, `runs` naming three different run ids.
"Flat" is a finding, not a gap in the data, and the only thing allowed to make a line prettier is the
renderer's own axis choice.

## Considered alternatives

**Coverage: average the per-file percentages, or the per-run ones.** Rejected because a mean of ratios
weights a 512-line generated file the same as a 2 617-line module (§3.1: 70.09 % against the pooled
86.92 %), and because ADR-0002 §4.3 already told the consumer to sum numerators and denominators. The
version of this that keeps coming back is "average the run percentages, it's simpler" — it is simpler
and it is 0.2 points wrong on the tree in the repo, which is more than a tenth-of-a-point arrow is worth.

**Coverage: pick one primary coverage tool per commit.** Rejected: it needs a tool-preference table
someone has to maintain, it silently discards a real measurement of a real part of the repository, and
the pooled rate has the between-the-endpoints property (§3.2) that makes picking unnecessary. A
`coverage.source_preference` config key would also be a config key that changes what a commit's coverage
*means*, which is exactly what ADR-0006 §7 tries not to create.

**Coverage: use the newest run only, per whole kind.** Rejected: on the sample tree that means
`coverage-ad-hoc`'s scraped percentage or nothing at all, and it throws away the istanbul run's
measurement of the JavaScript half of the repository. "Newest wins" is right *within* a job because
those two runs measure the same thing; across jobs it is a category error.

**Coverage: validate `sum(file records) == total` and flag a mismatch.** Rejected on the facts:
ADR-0002 §4.2 makes the mismatch a documented, everyday property (`node_modules`, generated files,
anything instrumented outside the workspace), so the check fires on healthy trees and trains the user to
ignore warnings. `outside_workspace_total` pins the good behaviour.

**Coverage: treat `found == 0` as `100 %`, as `0 %`, or as `null`-and-average-anyway.** All three are the
same mistake in a different costume: an undefined ratio is not a number, and any of the three lets an
empty generated file move a trend line. ADR-0007 §3.5's `ln(0)` case, `vacuous` vector included.

**Benchmarks: keep `unit` out of the group key and refuse to compare at render time.** Rejected: every
renderer would have to remember the rule, and one of them would forget. The key *is* the enforcement
(§4.3), and it is what makes "the same metric changed unit" render as two metrics — which is what
mact-by1.12 already asks for in its own words.

**Benchmarks: mean instead of median, or `best of` across runs.** Rejected: `sample` exists to expose
noise, a mean is dragged by the one outlier sample the job produced, and "best of" is a leaderboard,
which the epic explicitly put out of scope. Median plus min/max spread is the smallest pair of
statistics that tells the truth about two samples.

**Benchmarks: store the mean/percentile in `.evidence` so readers agree.** Rejected — ADR-0002 §4.4
gives dedup, percentiles and outlier rejection to the scanner on purpose, and the writer has said it
never aggregates. Putting an aggregate in the contract would also freeze a policy this ADR is allowed to
change by bumping a version.

**Action success: ask the `run_status` tool, or read `meta.json`.** Rejected for the shelf life and the
contract reason in §5.1, and a third time by ADR-0005 §7 (Fathom is a reader).

**Action success: a fifth `unknown` state distinct from `no_evidence`.** Rejected: `no_evidence` *is*
unknown, and adding both would let a renderer treat "nothing ran" and "I could not read it" as one thing
while pretending to separate them. The row separates them honestly instead: state plus
`unreadable_records`.

**Action success: any-failure-wins, or latest-run-wins across the whole commit.** Rejected: any-failure
turns every re-run into a permanent `failed` badge, and whole-commit-latest throws away the fact that a
commit can have a good coverage run and a broken benchmark job — which is `mixed`, and is what the
sample tree actually looks like.

**A separate `run_outcome` record type, now.** Deferred, correctly, to mact-by1.18: it is an evidence
contract change (new kind or new required field ⇒ a breaking change under ADR-0002 §7), not a
reader policy, and this ADR must
not pretend the contract says something it does not.

## What checks what

| artifact | what it pins | who runs it |
|---|---|---|
| [`schema/metric-model.json`](../../schema/metric-model.json) | every constant, closed vocabulary, the row field list, the four states, the glyph table, the forbidden list | read by the reference and by the checker; nothing else parses it |
| [`scripts/metric_model_reference.py`](../../scripts/metric_model_reference.py) | the semantics, executable: `--tree` derives rows, `--explain` prints every keep/drop decision, `--vectors --diff` re-derives the goldens | mact-by1.6's test oracle; CI via the checker |
| [`tests/testdata/metrics/`](../../tests/testdata/metrics) | 16 vectors, each a fixture tree plus the row it must produce and a hand-written note showing the arithmetic (`expected/*.json` → `notes`); a fixture that needs an *empty* directory to make its point — a foreign SHA-shaped name, or a commit with no evidence at all — carries a `.gitkeep`, because git stores content and not intent and a directory a clone cannot recreate is a rule nobody can test | re-derived on every CI run |
| [`scripts/validate_metric_model.py`](../../scripts/validate_metric_model.py) | ADR ↔ JSON ↔ reference ↔ vectors ↔ docs, plus the arithmetic properties of C8 and a `--self-test` that proves the checks fire | `scripts/ci.sh schema` |
| `docs/evidence-format.md` §12 recipes | the *other* implementation: jq, written from the reader's side | C7 diffs this reference against the recorded output of recipes 2 and 6 on the same tree |

C8's properties are worth listing, because they are the ones that would otherwise be caught by a user
comparing two screens: no derived row anywhere contains a `null`; a row never has a `percent` without a
non-zero `found`; a vacuous row has no `percent`; a pooled percent always lies between the smallest and
largest rate it pooled; every `comparable: false` benchmark has no unit; every group's records agree on
their unit; every gap and caveat code that appears is one the model declares; and every action state is
one of the four.

## Consequences

- **mact-by1.6** implements §2–§5 once and is graded against `scripts/metric_model_reference.py`; its
  `--explain` is this pipeline's drop log, so the reason codes are a public interface, not debug noise.
- **mact-by1.9 / .12 / .13 / .14** render. They get glyphs, gap codes, caveat names and one
  parent rule from here, and they may not invent any: an unknown column is `—` in the TUI, `null` in
  `fathom report --format json` and `-` in its TSV, and `fathom report --fail-on action-failure` is
  `action_success ∈ {failed, mixed}` — never `no_evidence`, because a commit with no evidence is not a
  failing commit and a CI gate must not punish an un-run workflow as if it had run and lost.
- **mact-by1.10** stores what §6.4 of ADR-0006 already provides room for. The fields this ADR adds —
  `coverage.measure`, `coverage.secondary`, `benchmarks[].labels/comparable/context/run_count/source`,
  `action_detail`, `excluded_runs`, `superseded_runs`, `unreadable_records`, `gaps` — are additive, so
  `row_version` stays 1; a future change to what an existing field *means* is not additive and bumps it
  (ADR-0006 §8 then refuses rather than migrates).
- **Numbers already in caches are the ones this model produced.** Every constant here is versioned
  through the evidence-derivation analyzer's version (ADR-0006 §6.3), so changing the pooling rule, the
  measure order, the median tie or the rounding is a version bump in the same commit — otherwise the
  cache serves old numbers under new keys, which is the silent-wrong-number failure the whole ADR-0006
  keying scheme exists to avoid.
- **Users get explanations, not shrugs.** `—` is one glyph, but every `—` has a gap code, every dropped
  record has a reason, and every caveat is named. That is what makes the difference between a tool that
  says "no coverage" and one that says "coverage ran twice and both runs failed; the third job printed a
  percentage but wrote no lcov" — the second is a fixable sentence and the first is a support ticket.
- **What this makes possible later:** regression detection and outlier rejection (both epic non-goals
  today) now have a stable substrate: a fixed group key, a stored spread, and a documented list of which
  comparisons are legal. Anyone implementing them starts from §4's fields instead of inventing a
  parallel notion of "the value for this commit".

## Open questions (owned elsewhere)

- **Runs that emit no evidence at all** — mact-by1.18, and it is an ADR-0002 §7 breaking change: a
  durable run-outcome record would let `no_evidence` split into `not-run`, `cancelled` and
  `ran-but-produced-nothing`. §5.4 is written so that splitting it later is additive to this ADR, not a
  rewrite of it.
- **Regression bands and outlier rejection** — mact-by1.12 owns the percent-of-value band and how many
  samples make a claim defensible; §4.2 hands it `min`, `max`, `samples` and `run_count` and stops.
- **Cross-workspace or cross-fork comparison** — the evidence root is per-workspace (ADR-0002 §1.1) and
  nothing here joins two of them.
- **A coverage number for a *subset* of paths** (per-directory coverage over time) — a different
  aggregation of the same records, needing its own measure and vacuous rules; nothing in §3 forbids it,
  nothing in §3 does it for free.
