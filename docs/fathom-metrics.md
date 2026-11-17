# Fathom's metrics — what every number on the screen means

This is the **user-facing** restatement of
[ADR-0007](adr/0007-maintainability-index.md) (the Maintainability Index) and
[ADR-0008](adr/0008-per-commit-metric-model.md) (the per-commit metric model): the page you read when
you want to know what `44.4%=*` in the `cov` column is a measurement *of*. Where this page and an ADR
disagree, the ADR wins; where an ADR and the machine-readable contracts disagree, the contracts win.
Every rule below names the ADR section it comes from and, where one exists, the
[`docs/evidence-format.md`](evidence-format.md) jq recipe that gets the same answer out of the raw
records — those recipes and this page are graded against each other (§7), so a rule cannot change in one
of them without the build noticing.

Nothing here is a policy this page invented. Fathom is a reader: it opens `<workspace>/.evidence` (written
by act-mcp, [ADR-0002](adr/0002-evidence-jsonl-contract.md)) and the repository's own history, and it
never runs anything ([ADR-0005 §7](adr/0005-fathom-crate-layout-and-dependencies.md)). Every number
below is a derivation from files already on disk, which is what makes all of them reproducible — and
`fathom doctor` is the command that tells you when one of the inputs is missing
([README: Fathom](../README.md#fathom)).

Contents: [1. one row per commit](#1-one-row-per-commit) ·
[2. the Maintainability Index](#2-the-maintainability-index-mi) ·
[3. coverage](#3-coverage) ·
[4. benchmarks](#4-benchmarks) ·
[5. action success](#5-action-success) ·
[6. glyphs, caveats and gaps](#6-glyphs-caveats-and-gaps) ·
[7. what checks what](#7-what-checks-what)

---

## 1. One row per commit

A row is a commit — every commit in the window the git walk yields, and only those
([ADR-0008 §2.4](adr/0008-per-commit-metric-model.md#24-rows-come-from-git-not-from-evidence)). A commit
with no evidence is a row of unknowns, not a missing row. The four data columns are built by four
independent derivations, and one column's `—` says nothing about another's:

| column | what it holds | derived by | normative section |
|---|---|---|---|
| `cov` | one pooled line-coverage percentage, or `—` | the newest successful run per `(kind, workflow, task)`, pooled | [§3](#3-coverage) · [ADR-0008 §3](adr/0008-per-commit-metric-model.md#3-coverage-) |
| `+/-` | added and deleted lines in that commit, headline totals with noise counted separately | git's own numstat output, classified by `paths.*` | [ADR-0006 §7.2](adr/0006-fathom-workspace-layout.md#72-the-keys) |
| `MI` | the LOC-weighted mean of the per-file Maintainability Index over that commit's whole tree | Fathom's own front end, over the blobs in that commit | [§2](#2-the-maintainability-index-mi) · [ADR-0007 §4](adr/0007-maintainability-index.md#4-the-unit-of-analysis-and-the-per-commit-aggregate) |
| `bench` | one benchmark group — the first comparable, non-context group by name — with `+N` for the rest | the newest successful benchmark run, grouped by `(name, labels, unit)` | [§4](#4-benchmarks) · [ADR-0008 §4](adr/0008-per-commit-metric-model.md#4-benchmark-results) |
| `action` | `✓` / `✗` / `◐` / `—`: did the workflows that ran here pass | the `run_status` field on the records themselves | [§5](#5-action-success) · [ADR-0008 §5](adr/0008-per-commit-metric-model.md#5-action-success--the-durable-question) |

The derivation runs in four stages, always in this order
([ADR-0008 §2](adr/0008-per-commit-metric-model.md#2-the-pipeline-walk-retain-select-aggregate)):

```
walk      which bytes are read at all      — only benchmark/<40-hex>/<run_id>.jsonl and coverage/<…>
retain    which records may feed a number  — run_status == success, not truncated, config filter
select    which run is newest per group    — max(run_id) per (kind, workflow, task)
aggregate the arithmetic that fills the row
```

`runs/` and `incoming/` are **never** read while indexing
([ADR-0008 P2](adr/0008-per-commit-metric-model.md#0-the-decision-in-one-screen)); act's own log is not a
record, and a scanner that globs `**/*.jsonl` gets plausible-looking numbers for the wrong reason. Every
record that is read but does not become a number is dropped with exactly one of 18 reason codes
([ADR-0008 §2.2](adr/0008-per-commit-metric-model.md#22-retain)) — the vocabulary of `fathom doctor`, of
`fathom report --format json`, and of the `f` overlay in the bench panel.

Two rules apply to every column and are worth internalising before reading the rest:

* **A `0` is always a measurement.** Unknown renders `—` (ASCII `-`, JSON `null`), a measured zero
  renders `0.0`, and the two are never interchangeable
  ([ADR-0008 §6.1](adr/0008-per-commit-metric-model.md#61-unknown-zero-empty--three-states-and-0-always-means-zero)).
* **A commit never borrows a number.** Deltas are against the **first parent**, and need both sides; a
  root commit has no delta; a merge is judged on its own evidence, not its second parent's
  ([ADR-0008 §6.3](adr/0008-per-commit-metric-model.md#63-parents-a-root-a-merge-and-the-number-never-borrowed)).

---

## 2. The Maintainability Index (MI)

MI is the one column Fathom measures itself rather than reads from evidence: it is computed from the
source in the commit's own tree, per file, by Fathom's pure-Rust front end
([ADR-0007 §6](adr/0007-maintainability-index.md#6-the-front-end)) — no C, no parser library, no
tree-sitter.

### 2.1 The formula and its constants

```
MI_raw = 171 - 5.2·ln(V) - 0.23·G - 16.2·ln(LOC)
MI     = clamp(MI_raw · 100 / 171, 0, 100)
MI_1   = round_half_away_from_zero(MI · 10) / 10        ← the stored, compared, displayed value
```

| constant | value | term it weights |
|---|---|---|
| `a` | `171.0` | the offset (the 1996 Coleman–DeMers–Ramani–Sweet formula, scaled by Microsoft's `100/171`) |
| `b_volume` | `5.2` | `ln(V)` — Halstead volume |
| `c_cyclomatic` | `0.23` | `G` — cyclomatic complexity |
| `d_loc` | `16.2` | `ln(LOC)` — logical lines |
| scale | `100 / 171` | the 0–100 normalisation |
| clamp | `[0, 100]` | defensive; the reachable range is `(0, 99.9]` |

These are constants, not knobs: they live in
[`schema/mi-definition.json`](../schema/mi-definition.json), are copied into
[ADR-0007 §2.1](adr/0007-maintainability-index.md#21-the-pinned-formula), and a checker diffs the two
copies. Changing one is a `mi.score` version bump in the same commit, which invalidates exactly the MI
blocks in the cache and nothing else.

The three inputs, as a reader needs them
([ADR-0007 §3](adr/0007-maintainability-index.md#3-the-three-inputs)):

* **`V = N · log2(n)`** — Halstead volume over **operator tokens only**: `N` occurrences, `n` distinct
  operator texts. Literals are one operand each and their contents are never tokenised, so a doc comment
  containing `if a && b` cannot move your score.
* **`G = 1 + decision points`**, counted over the whole file (one entry point, not one graph per
  function). `if`, `elif`, each `match` arm (a guarded arm costs 2), `for`/`while`/`loop`, `&&`/`||`
  *when they follow a value*, Rust `?`, `let … else`, Python `except`. `else`, `return`, `break`,
  `continue`, `panic!`, `assert!`, `with`, `lambda`, `yield` and `not` cost **zero** — so a flat guard
  clause and a nested block cost the same two forks.
* **`LOC` = logical lines (statements)**, never physical lines: comments, blanks, Rust attributes and
  doc comments cost nothing, a multi-line call costs one. This is the whole reason a `cargo fmt` commit
  shows no MI delta.

A file with `LOC == 0` or Halstead vocabulary `n < 2` has **no MI at all** — `ln(0)` and `log2(1)` are
the reason ([ADR-0007 §3.5](adr/0007-maintainability-index.md#35-ln0-is-undefined-no-mi-rendered-unknown-never-0)).

### 2.2 A worked example, reproducible from the test data

`rust_basic` is one of the hand-counted golden vectors under
`crates/act-mcp/tests/testdata/mi/`. Its token table says operators `N = 46`, distinct operators
`n = 31`, decision points 9, logical lines 16:

```
V      = 46 · log2(31)     = 227.89303027779624
G      = 1 + 2 + 3 + 1 + 1 + 1 + 1 = 10      (2 `if`, 3 `match` arms, `||`, `&&`, `for`, `while`)
MI_raw = 171 - 5.2·ln(227.89303027779624) - 0.23·10 - 16.2·ln(16)
       = 171 - 28.2302 - 2.3 - 44.9159 = 95.55390566196463
MI     = 95.55390566196463 · 100 / 171 = 55.87947699530096   →  MI_1 = 55.9   (green)
```

Re-run it yourself — the oracle, the Rust implementation and the checker all print the same digit:

```sh
python3 scripts/mi_reference.py --vector crates/act-mcp/tests/testdata/mi/vectors/rust_basic.json --explain
cargo test -p fathom --test mi                      # the Rust port, graded against the same vectors
python3 scripts/validate_mi_definition.py           # re-derives every vector twice more (40-digit decimal)
```

### 2.3 Bands

| band | edge | reading |
|---|---|---|
| green | `MI ≥ 50.0` | nobody needs to argue about this file |
| amber | `25.0 ≤ MI < 50.0` | worth a look |
| red | `MI < 25.0` | size and density *are* the finding |

An exact edge belongs to the better band. The edges are declared in
[ADR-0007 §5.1](adr/0007-maintainability-index.md#51-the-declared-edges), are **not** stored in the cache
and are **not** configuration — two readers of one repository must not see different traffic lights for
one commit. They are not Microsoft's familiar `< 85 / < 65` edges, which belong to the *updated* MI that
estimates volume from LOC;
[ADR-0007 §5.2](adr/0007-maintainability-index.md#52-the-edges-the-ticket-suggested-do-not-work-here-and-here-is-the-arithmetic)
does that arithmetic out loud. A 16-statement file measuring 55.9 is normal, and a 1 000-line file can
legitimately measure 0: the `16.2·ln(LOC)` term dominates when volume is measured rather than estimated.

### 2.4 From files to the commit's number

The `MI` column is an aggregate over **every file in scope in that commit's tree** — not only the files
the commit touched ([ADR-0007 §4.2](adr/0007-maintainability-index.md#42-the-commit-aggregate-is-an-aggregate-not-a-measurement)):

```
MI_commit = MI_1( Σ (MI_file · LOC_file) / Σ LOC_file )      over the files whose status is `ok`
```

accumulated in path order, and a **pure function of that commit's file set** — no carry-forward from the
parent, because then a float's addition order would decide a stored digit. The same fixture as
[ADR-0007 §9](adr/0007-maintainability-index.md#9-golden-vectors-and-the-tests-that-make-them-real),
`aggregates/commit_small.json`, makes the weighting concrete:

```
files   src/huge.rs 22.0·400  src/python_basic.py 45.3·45  src/rust_basic.rs 55.9·16
        src/rust_generics.rs 61.8·10  src/rust_reformat_a.rs 70.9·6  src/rust_question.rs 73.4·6
        src/rust_tiny.rs 95.1·1
Σ(MI·LOC) = 13311.8   Σ LOC = 484   →   27.504…  →  MI_commit = 27.5   (amber)
```

One 400-line file at 22.0 outweighs a one-line file at 95.1 because the weight is lines, not files. The
mean of those seven scores is 60.6, and reporting *that* would let one tidy one-liner speak for a
repository whose bulk measures 22.0; `aggregates/weighted_versus_per_file_mean.json` is the case isolated
— a 12-line module at 95.0 beside a 900-line module at 61.0, where `(95·12 + 61·900) / 912 = 61.4` is the
answer and 78.0 is not
([ADR-0007 §4.3](adr/0007-maintainability-index.md#43-the-arithmetic)). The same arithmetic on the two
Python files from the [README runbook](../README.md#build-and-first-run), which a reader can paste into a
repository of their own and re-index:

```
commit fd92c44   src/classify.py 61.3·17   src/report.py 74.3·7
                 Σ(MI·LOC) = 1085.2   Σ LOC = 24   →   65.0583…   →   MI_commit = 65.1  (green)
parent af4920b   src/classify.py 61.3·17                                 →   61.3        (green)

MI delta vs parent = 65.1 - 61.3 = +3.8   →   ▲ up, shown next to the number
```

Both rows are `.fathom/index/rows/<xx>/<commit>.json` verbatim, and the detail view (`enter`) is where
the per-file numbers are on screen.

The row also carries:

* **hotspots** — the worst *N* files (default 5), sorted MI ascending, then LOC descending, then path;
* **the watchlist** — files that entered or left the **red** band (`MI < 25.0`) against the parent, as
  `sub_threshold_added` / `sub_threshold_removed`. Not the amber band: a set that large churns on every
  commit and says nothing
  ([ADR-0007 §4.3](adr/0007-maintainability-index.md#43-the-arithmetic));
* **`delta_vs_parent`** — `MI_1(aggregate(C) − aggregate(first parent))`, absent for a root commit;
* **`counts`** — the per-file status tally, and `unanalysed_paths`, the four statuses that mean "not
  analysed".

The detail view (`enter`) shows per-file MI for the selected commit; `i` (or `t`) there shows each file's
`V`, `G` and `LOC`, which is what a score is actually made of.

### 2.5 When there is no MI, and which files count

Seven reasons, evaluated in this order, first match wins
([ADR-0007 §3.6](adr/0007-maintainability-index.md#36-the-measurability-ladder-seven-reasons-in-order)):
`excluded` → `skipped_noise` → `unsupported_language` → `oversized` → `parse_error` → `vacuous` → `ok`.
The four in bold there — `unsupported_language`, `oversized`, `parse_error`, `vacuous` — are counted as
`unanalysed_paths` and announced by the view; the first two are your configuration, not the code's
fault. A file with no MI has **no MI field anywhere** — never `0`, which would be the best-impossible
score pretending to be the worst.

Which files are in scope is `fathom.toml`
([ADR-0006 §7.2](adr/0006-fathom-workspace-layout.md#72-the-keys), precedence `exclude` > `noise` >
(`source` ∩ an enabled language)):

| key | default | note |
|---|---|---|
| `paths.source` | `["**/*"]` | the candidate set |
| `paths.noise` | `target/**`, `node_modules/**`, `**/*.lock`, `Cargo.lock`, `**/generated/**`, `**/*.min.*`, `.venv/**`, `vendor/**` | the **same** list feeds MI and the `+/-` headline |
| `paths.exclude` | `.git/**`, `.fathom/**`, `.evidence/**` | never read at all |
| `paths.include_tests` | `false` | test files are noise by default — a default that hides half a repository must announce itself, and `doctor` does |
| `languages.enabled` | `rust`, `python` | the only two front ends in v1 |
| `index.file_size_cap_mib` | `2` | bigger files are `oversized`, not analysed |

`fathom index --status` prints every one of these as resolved by the build you are holding
([README: Fathom](../README.md#fathom)).

---

## 3. Coverage

### 3.1 The one legal formula

Evidence stores numerators and denominators and never a rate
([ADR-0002 §4.3](adr/0002-evidence-jsonl-contract.md)); Fathom computes the rate, and there is exactly
one allowed arithmetic ([ADR-0008 §3.1](adr/0008-per-commit-metric-model.md#31-the-rate-is-the-consumers-to-compute-and-only-one-formula-is-allowed)):

```
percent = round_half_away_from_zero( 100 · Σ hit / Σ found , 1 )
```

summed **separately** over the selected runs' totals, per measure, from raw counts, rounded once. The
three ways a plausible implementation gets this wrong are the three things forbidden:

* **not the mean of per-file percentages** — in the sample tree that is the difference between the run's
  pooled **86.92 %** and the mean of that run's four file rates, **70.09 %**: a 512-line generated file at
  100 % beside an 88-line file at 0 % outvotes a 2 617-line parser module at 84 %
  (recipe [3](evidence-format.md#12-recipes) is that ranking, empty files excluded);
* **not the mean of per-run percentages** — the sample tree's two count-bearing runs are 86.9175 %
  (3581/4120, lcov) and 86.1539 % (1904/2210, istanbul), whose mean is 86.5, while the pooled rate is
  5485/6330 = 86.6509 → **86.7**: a fifth of a point, in a column whose arrows are drawn at a tenth of a
  point;
* **never from a rounded intermediate** — a displayed `86.92` is a display, and re-rounding a display is
  how `86.949999` becomes `87.0`.

Pooling is safe across tools because it is a size-weighted mean: for any set of runs with `found > 0` the
pooled rate lies between their smallest and largest individual rates, so it can neither invent coverage
nor hide a drop ([ADR-0008 §3.2](adr/0008-per-commit-metric-model.md#32-which-records-and-why-pooling-is-safe-across-tools)).
It does not claim lcov and istanbul measured the same files — both stay individually visible in the
detail view with their run ids.

Raw equivalent, jq recipe
[2](evidence-format.md#12-recipes) — every coverage total for one commit — and recipe
[3](evidence-format.md#12-recipes) for the per-file ranking with empty files excluded.

### 3.2 Which records feed it

Selected by `(kind, workflow, task)`, newest run per group (`max(run_id)`, which is chronological
because the writer minted the id), then
([ADR-0008 §2.2–§2.3, §3.3](adr/0008-per-commit-metric-model.md#23-select)):

* only `run_status: "success"`; `failed`, `cancelled`, `timed_out` and `error` are **written on purpose**
  so they can be filtered, and each is named in the row's `excluded_runs` (recipe
  [9](evidence-format.md#12-recipes): *which evidence came from a non-success run*);
* a run whose records carry `flags: ["truncated"]` never feeds a number — what survived a cap is not the
  run;
* a selected run's `scope: "total"` record **is** its total. Its file records are not summed to check it
  (a tool may legitimately instrument files outside the workspace). Only when a run published **no**
  total are its file records summed, and then the row carries the caveat `total_from_file_sum` — that
  total is a reader's construction, not the tool's claim.

### 3.3 Measures: one number per unit

The primary measure is the first of `lines → statements → branches → functions` whose pooled
`found > 0`; every other measure with pooled counts is pooled the same way into
`coverage.secondary` with the caveat `secondary_measures`, and records of a non-primary measure are
dropped with `measure_not_primary`
([ADR-0008 §3.5](adr/0008-per-commit-metric-model.md#35-measures-one-per-number-secondary-numbers-stored)).
`lines` is first because it is the only measure every supported format reports — `lines` and `statements`
are different measurements and never pooled together.

### 3.4 `found == 0` is not `0.0 %`

| state | counts | `cov` cell | in a window average |
|---|---|---|---|
| measured | `hit 3581 / found 4120` | `86.9` | yes |
| real zero | `hit 0 / found 500` | `0.0` | yes |
| **vacuous** | every pooled `found == 0` | `—`, gap `vacuous` | **no** |
| nothing to pool | no eligible record | `—`, one of the other gap codes | no |

`0/0` is undefined, and printing `100 %` for "an empty generated file was the only thing instrumented"
would be the most misleading thing this column could do
([ADR-0008 §3.4](adr/0008-per-commit-metric-model.md#34-found--0-is-vacuous-and-00--is-a-number)). A
vacuous run is a no-op in a pool — 0/0 adds nothing to either sum — so the exclusion is arithmetic, not
a special case; the special case only bites when *every* denominator was zero. Recipe
[3](evidence-format.md#12-recipes) excludes empty files for the same reason.

### 3.5 A scraped percentage is a caveat, never the plotted number

A `log_scrape` record carrying `percent_only` (and therefore `low_confidence`) says what a tool
*printed* and has no denominator to pool. It is excluded from `percent`, recorded as the caveats
`percent_only` and `low_confidence`, and listed in the detail view with its reported value and the run
that produced it ([ADR-0008 §3.6](adr/0008-per-commit-metric-model.md#36-scraped-percentages-are-caveats-never-the-plotted-number)).
A commit whose **only** coverage evidence is scraped shows no percentage at all — `—*`, gap
`only_percent_only` — and its `action` column can still be `✓`, because "the job ran" and "the number
exists" are different columns. Inventing a denominator so a scraped number can be pooled is forbidden.

### 3.6 Worked example, from the sample tree

```sh
EV=$(scripts/sample_evidence.py --quiet)
python3 scripts/metric_model_reference.py --tree "$EV" --commit 2f7c1a4 --explain
```

```
coverage.percent   = 86.7          (3581 + 1904) / (4120 + 2210) = 5485 / 6330 = 86.6509…
coverage.measure   = lines
coverage.run_count = 2             two coverage workflows, both selected, both pooled
coverage.secondary = statements 86.1 · branches 80.1 · functions 89.9
excluded_runs      = 1             one failed benchmark run, named, with reason run_status_excluded
caveats            = percent_only, low_confidence, secondary_measures, context_only, unit_missing, run_excluded
action_success     = mixed         coverage jobs passed, the benchmark job did not
```

That is the tree the tour in the README produces the shape of, and `scripts/metric_model_reference.py`
is ADR-0008's independent reference implementation — the same numbers the checker compares the Rust
reader against (§7).

### 3.7 Gap codes for this column

`—` is one glyph with seven meanings, and a reader standing in front of it is entitled to the difference
([ADR-0008 §3.7](adr/0008-per-commit-metric-model.md#37-run_status-and-where-the-excluded-runs-still-show)):
`no_evidence` · `unreadable` · `all_runs_excluded` · `only_percent_only` · `vacuous` · `no_counts` ·
`no_records`. The name is in the row, and `fathom report --format json`, `--explain` and the detail view
print it.

---

## 4. Benchmarks

### 4.1 A group is `(name, labels, unit)` — and `unit` is not decoration

Two benchmark records are the same measurement only if their name, their labels and their unit all match
([ADR-0008 §4.1–§4.3](adr/0008-per-commit-metric-model.md#41-identity-and-why-sample-stays-in-the-key)).
Labels are compared verbatim and canonically (keys ascending), so `{job:"bench"}` and
`{job:"bench",os:"x"}` are two groups: two differently-described measurements, not one with a missing
field. `sample` stays inside one run's identity, because three samples of one `(name, labels)` **are**
the noise estimate and collapsing them destroys the only thing that made a single value interpretable.

| field | how it is computed |
|---|---|
| `value` | the **median** of the samples of that group in the **newest selected run** |
| `samples` | how many records that newest selected run published for the group |
| `min` / `max` | the spread over **every retained run** at this commit, superseded ones included |
| `run_count` | how many distinct retained runs that spread came from |
| `run_id`, `source`, `flags` | the newest selected run's, because that is where `value` came from |

The headline is one run; the error bar spans all of them. That asymmetry is deliberate: "best current
estimate, and here is how much the runs disagree". An even sample count takes the mean of the two middle
values, and a median is allowed to be a number no sample ever had — the sample tree's two samples
`184320.4` and `185012.7` give `184666.55` (recipe
[6](evidence-format.md#12-recipes), *percentiles within one commit*, prints the same number; recipe
[5](evidence-format.md#12-recipes) is the spread per commit over successful runs only, which is exactly
retention's rule). Values are stored as the computed double in shortest round-trip form and **never
re-rounded**; renderers format at four significant digits at draw time.

### 4.2 When a delta is withheld

A delta needs a baseline **and** data that supports a percentage. The baseline is the **newest *older*
commit in the window carrying that exact group identity** — not the first parent, because a benchmark
that only exists on a side branch has nothing to be compared against on the trunk. The panel prints the
baseline's short SHA next to the arrow. A percentage is withheld, and the row says why, in these cases
([ADR-0008 §4.2–§4.3](adr/0008-per-commit-metric-model.md#42-the-group-the-value-and-the-spread)):

| situation | the cell | the status line says |
|---|---|---|
| no older commit in the window carries this group | `—`, baseline `?` | `no earlier commit in this window has this metric · — is unknown, not flat` — a first appearance is not a regression |
| the median is over fewer than two samples | `n=1` | `insufficient samples (n=1) · a percentage off one sample is not a measurement of a change` |
| one retained run, and its samples agreed (`min == max`) | `1 run` | `spread from 1 run · the noise floor is unknown, so no percentage is honest here` |
| the baseline's value is `0` | `base 0` | `the baseline is 0, so there is no percent of value` |
| the group has no unit (`unit_missing`) | `not cmp` | `no unit, so no delta and no sparkline (ADR-0008 4.3)` — displayable, never comparable |
| a metric's unit changed between commits | two independent rows | `queue_depth` in `ms` and in `s` are two questions, not a trend — `unit` is part of the identity |
| the group is `act_log` context | an arrow, never `!` | `… · context, so never a regression signal` — run-level metrics move, that is what they do |

A moved row's sentence names its baseline: `up 12.4% vs 5f2a1c9 (band 5%)`, and `· OUTSIDE THE DECLARED
BAND` when the band's answer is yes

What is left is a percentage against the baseline, judged against a declared band —
`benchmarks.regression_band_percent`, default **5 %** of the value, printed in the panel's own column
title so no arrow is read against an invisible threshold. Nothing in this data model declares that
bigger is better: the band's answer is "outside the declared band", and the judgement stays with you.
Every sparkline is scaled to its own min and max, which is why its axis is printed beside it, and a
history with holes does not connect them.

### 4.3 `act_log` metrics are context, and absence is absence

Every benchmark run also publishes `job_duration_s` and `step_duration_s` from act's own timings. Those
describe **the run**, not the code: kept, displayed, marked `~` (`context_only`), never a regression
signal by default ([ADR-0008 §4.4](adr/0008-per-commit-metric-model.md#44-act_log-metrics-are-context-and-absence-is-absence)).
A job that reports its own `job_duration_s` wins and act's is dropped as `source_shadowed`.

`step_duration_s` needs act's `executionTime`, which act gained in **0.2.76** while this server accepts
act from 0.2.62, so runs between those versions publish no `step_duration_s` **at all**. Absent means
absent: not `0 s`, not a hole in a line, and above all not a regression — otherwise upgrading act looks
like a 90-second speedup on every commit at once. The view may *note* `tool.act_version` as the likely
reason; it may not infer a cause from an absence.

A metric this commit stopped reporting keeps a row in the panel saying when it was last seen, and is
never carried forward. The timeline's `bench` cell shows **one** group — the first by `(name, labels)`
that is comparable and not context, else the first of any, rendered at four significant digits with `+N`
for the rest and `*` if the column carries any caveat, e.g. `412.5ms +1*`
([ADR-0008 §4.5](adr/0008-per-commit-metric-model.md#45-what-a-row-keeps-and-what-the-timeline-column-shows)).
Which group that is stays stable across commits on purpose: a column whose displayed metric changed with
the data is a column that silently changed what it means. `b` opens the panel with all of them; `f`
there shows the runs retention excluded and why.

---

## 5. Action success

"Did this commit pass?" is the question with the shortest shelf life in the product, and the three
available wrong answers are all refused ([ADR-0008 §5.1–§5.2](adr/0008-per-commit-metric-model.md#51-the-trap)):
the `run_status` **tool** is in-memory, newest-first and capped at 50 runs
([docs/tool-contract.md §5](tool-contract.md)); `runs/<run_id>/meta.json` is audit context and not the
contract ([ADR-0002 §1.3](adr/0002-evidence-jsonl-contract.md)); and asking the **server** at render time
is forbidden outright, because a TUI whose columns need a live server cannot be used on a plane or diffed
in CI ([ADR-0005 §7](adr/0005-fathom-crate-layout-and-dependencies.md)).

The durable signal is the `run_status` field on **every evidence record** — required, schema-validated,
on disk under the commit's own directory, and immune to the server's memory. Group the readable records
by `(kind, workflow, task)`, take each group's newest run's `run_status` as that group's verdict, and
then evaluate **in this order** so the states are exclusive and exhaustive
([ADR-0008 §5.3](adr/0008-per-commit-metric-model.md#53-the-four-states-and-the-order-that-makes-them-exclusive)):

| # | condition | state | glyph | ascii |
|---|---|---|---|---|
| 1 | no records at all for this SHA, under either kind | `no_evidence` | `—` | `-` |
| 2 | every group's verdict is `success` | `success` | `✓` | `pass` |
| 3 | no group's verdict is `success` | `failed` | `✗` | `fail` |
| 4 | some are and some are not | `mixed` | `◐` | `mix` |

Three consequences a reader actually runs into:

* **a re-run is a pass, not a mixed.** A failed run followed by a successful re-run of the same
  `(kind, workflow, task)` verdicts on the newest, so the row says `✓` — and the failed run is still in
  `excluded_runs`. That is what a CI badge means by green, and it is the opposite of a naive
  "any failure ⇒ failure" rule (`retry_passes` is the golden vector that pins it).
* **`mixed` is the common case, not an error state.** Coverage passed and the benchmark job died: that
  is the truth, and the columns stay independent — the `action` glyph says nothing about why the `bench`
  cell is `—`, and vice versa.
* **`no_evidence` is unknown, never a verdict.** It covers three situations the contract cannot currently
  distinguish — nothing ran for this SHA, a run was cancelled before publishing, and a run succeeded and
  extracted nothing — and the honest rendering is `—` with gap `no_evidence`, not a pass and not a
  failure ([ADR-0008 §5.4](adr/0008-per-commit-metric-model.md#54-no-evidence-is-unknown-and-there-are-three-kinds-of-it)).
  Records that exist but this build cannot read still read `no_evidence` **plus** `unreadable_records: N`
  and the `unreadable` caveat, because "nothing ran" and "I could not read it" call for different fixes.
  `fathom doctor` may add, as **advisory** only, that a run directory published nothing.

The row carries `action_detail` — one entry per group with its `kind`, `workflow`, `task`, `category`,
`status`, `run_id` and whether that group drove the state — so the glyph is always explainable by
opening the detail view.

---

## 6. Glyphs, caveats and gaps

The glyph table is pinned in [`schema/metric-model.json`](../schema/metric-model.json); the `?` overlay
on any screen is the same table, next to the keys of the screen you are on. No signal is carried by
colour alone — `--no-color`, `$NO_COLOR` and `TERM=dumb` keep every state in the glyph, and `--plain` and
`fathom report --format tsv` use the ASCII column
([ADR-0008 §6.1](adr/0008-per-commit-metric-model.md#61-unknown-zero-empty--three-states-and-0-always-means-zero)).

| glyph | ascii | meaning |
|---|---|---|
| `—` | `-` | unknown, in any column — including `action` for `no_evidence` |
| `*` | `*` | this column has caveats; the line under the table names them |
| `~` | `~` | context (`act_log`): a run-level number, not a measurement of the code |
| `▲` `▼` `·` | `^` `v` `=` | versus the first parent: up, down, equal **at the stored precision**. An arrow needs both sides; `·` is a real tie, never an unknown |
| `✓` `✗` `◐` | `pass` `fail` `mix` | the action state table in §5 |
| `▸` | `>` | the selected row — the TUI's cursor, not part of the pinned table above it |

The `*` stands for one of twelve closed caveat codes
([ADR-0008 §6.1](adr/0008-per-commit-metric-model.md#61-unknown-zero-empty--three-states-and-0-always-means-zero));
a `*` with nothing behind it is a derivation bug, not decoration:

| caveat | column | what it says |
|---|---|---|
| `percent_only` | coverage | a scraped percentage was present and was not used |
| `low_confidence` | coverage | text-scraped evidence was present and was not used |
| `vacuous` | coverage | a pooled run had `found == 0`; the ratio is undefined for it |
| `total_from_file_sum` | coverage | a run published no total, so its total is the reader's sum of file records |
| `secondary_measures` | coverage | another measure is stored beside the primary one |
| `measure_not_primary` | coverage | a record measured something other than the primary measure and was left out of the pool |
| `run_excluded` | either | `excluded_runs` names runs that happened and were not used |
| `superseded_runs` | either | older runs of a group whose newest won |
| `unreadable` | either | records were present that this build could not read |
| `unit_missing` | benchmarks | no unit: displayable, never comparable |
| `source_changed` | benchmarks | this commit's runs for the group came from different `source`s, so the spread crosses precedence levels |
| `context_only` | benchmarks | the group is `act_log` context, not a measurement of the code |

Gap codes name *which* kind of unknown a `—` is: `coverage` takes the seven in §3.7, `benchmarks` takes
`no_evidence` · `unreadable` · `all_runs_excluded` · `config_filtered` · `no_records`, and the store's own
row adds `no_evidence`, `no_mi` and `unanalysed_paths`
([ADR-0006 §6.4](adr/0006-fathom-workspace-layout.md#64-a-row)).

---

## 7. What checks what

Every number above has at least two independent implementations, and CI is what keeps them honest —
not this page
([ADR-0007 §9, §12](adr/0007-maintainability-index.md#9-golden-vectors-and-the-tests-that-make-them-real),
[ADR-0008 "What checks what"](adr/0008-per-commit-metric-model.md#what-checks-what),
[docs/testing.md](testing.md)).

| claim | normative | machine-readable | oracle | Rust tests |
|---|---|---|---|---|
| the MI formula, inputs, bands, aggregation | [ADR-0007](adr/0007-maintainability-index.md) | [`schema/mi-definition.json`](../schema/mi-definition.json) | `scripts/mi_reference.py` | `crates/fathom/tests/mi.rs` |
| coverage, benchmark and action derivation | [ADR-0008](adr/0008-per-commit-metric-model.md) | [`schema/metric-model.json`](../schema/metric-model.json) | `scripts/metric_model_reference.py` | `crates/fathom/tests/metrics.rs` |
| the records those derivations read | [ADR-0002](adr/0002-evidence-jsonl-contract.md) | [`schema/evidence.schema.json`](../schema/evidence.schema.json) | the jq recipes in [evidence-format.md §12](evidence-format.md#12-recipes) | `crates/fathom/tests/evidence_recipes.rs`, `crates/act-mcp/tests/evidence_*.rs` |
| the headless envelope (`fathom report`) | [ADR-0006 §10.1](adr/0006-fathom-workspace-layout.md#10-failure-modes-each-with-the-line-the-user-sees) | the golden files under `crates/fathom/tests/golden/` | — | `crates/fathom/tests/report.rs` |

`scripts/validate_mi_definition.py` and `scripts/validate_metric_model.py` are the ADR↔contract checkers:
they diff each ADR's copied constants against the JSON, re-derive every golden vector through the
Python oracle (the MI one a second time in 40-digit `decimal`), re-check each ADR's worked example
against a freshly built sample tree, and diff the metric model's Python answer against the recorded
output of the `docs/evidence-format.md` §12 recipes. Both run in `scripts/ci.sh`.

To re-derive any number in this page from the checked-in fixtures:

```sh
python3 scripts/mi_reference.py --vector crates/act-mcp/tests/testdata/mi/vectors/rust_basic.json --explain
python3 scripts/mi_reference.py --aggregate crates/act-mcp/tests/testdata/mi/aggregates/commit_small.json
EV=$(scripts/sample_evidence.py --quiet) && python3 scripts/metric_model_reference.py --tree "$EV" --commit 2f7c1a4 --explain
```

Two things this page deliberately does not decide: **which commits exist** (the git walk's `history.*`
window, [ADR-0006 §7.2](adr/0006-fathom-workspace-layout.md#72-the-keys)) and **which files count**
(`paths.*`, §2.5). Both are configuration, both are printed by `fathom index --status`, and both are
restated in [ADR-0006](adr/0006-fathom-workspace-layout.md) rather than here.
