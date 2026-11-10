# `tests/e2e/testdata` — contract fixtures

These files are the executable half of [ADR-0002](../../../docs/adr/0002-evidence-jsonl-contract.md) (under
`expected/` and `invalid/`) and of [ADR-0001](../../../docs/adr/0001-tool-contract.md) (under
`tool-io/`). They were **written by hand from the ADRs and the schemas**, not emitted by the
implementation, so that the implementation has something independent to be measured against.

```
tests/e2e/testdata/
├── README.md                        # this file
├── expected/                        # must validate: python3 scripts/validate_evidence.py tests/e2e/testdata/expected
│   ├── benchmark.jsonl              # golden for mact-f1w.12 (job metrics + in-band marker + act-log timings)
│   ├── benchmark-failed.jsonl       # run_status:"failed" — evidence is written for failed runs
│   ├── coverage.jsonl               # golden for mact-f1w.10 (lcov: 1 total + 4 files, one zero-coverage, one branchless)
│   ├── coverage-istanbul.jsonl      # coverage_summary source: statements/functions pairs
│   ├── coverage-log-scrape.jsonl    # last resort: percent_only + low_confidence, no raw counts
│   └── meta.json                    # run manifest for the coverage.jsonl run
├── invalid/                         # each file must be REJECTED, for exactly one reason
├── tool-io/                         # ADR-0001 tool arguments and structured results (see the last section)
│   ├── valid/                       # each file must validate against the $defs entry in its filename
│   └── invalid/                     # each file must be REJECTED
tests/fixtures/coverage/             # the payloads the coverage goldens are re-derived from
├── lcov.info                        # the tracefile behind coverage.jsonl
├── lcov-{absolute-sf,repeated-sf}.info # the same run in the other shapes the contract accepts
├── coverage-summary.json            # the istanbul summary behind coverage-istanbul.jsonl
├── lcov-malformed.info, lcov-empty.info, coverage-summary-malformed.json # parse_failed / no_coverage_found
├── coverage.xml                     # Cobertura: recognised, and refused by decision
└── act-log-{scrape,quiet}.jsonl     # act's captured log, with and without a coverage line
```

## Shape of the goldens

Every record is one compact JSON object per line, LF-terminated, keys in envelope-then-payload order
so a byte-for-byte golden diff is stable. That order is normative in
[ADR-0002 §6.1](../../../docs/adr/0002-evidence-jsonl-contract.md): top-level keys follow the ADR's
table order, and keys *inside* an object (`labels`, `extra`) are written in ascending code-point
order, so a fixture that sorts its top-level keys but not its nested ones cannot be reproduced by any
writer — `coverage-log-scrape.jsonl` was amended for exactly that reason (its `extra` members were
reordered; no value changed). Volatile fields are pinned to fixed values so the e2e harness
(mact-f1w.15) can normalise them before diffing:

| field | pinned value |
|---|---|
| `commit` | `2f7c1a4b9e0d3c6a5b8f1e2d4c6a8b0f3e5d7c91` |
| `run_id` | `20261110T004203Z-a1b2c3` (benchmark), `20261110T011507Z-d4e5f6` (coverage), `20261110T021244Z-0f9e8d` (istanbul), `20261110T030900Z-77aa55` (log scrape), `20261110T041530Z-bbc0dd` (failed benchmark run) |
| `started_at` | `2026-11-10T…Z`, millisecond precision |
| `host` | `build-box-07` |
| `tool` | `{"name":"act-mcp","version":"0.1.0","act_version":"0.2.68"}` |

The harness `MUST` normalise `run_id`, timestamps, `pid` and hostname as an explicit, documented step
— not by weakening the assertions. Note that these fixture files sit flat in `expected/`; the harness
writes the real `.evidence/<kind>/<full-sha>/<run_id>.jsonl` tree, so `expected/*.jsonl` names are
scenario labels, not layout, and `<run_id>` inside each file must match the file's own records.

## Which goldens the implementation reproduces

The coverage goldens are not merely hand-written any more: `tests/fixtures/coverage/` holds the
payloads they came from, and `tests/coverage_extraction.rs` re-derives each file byte for byte
from them (mact-f1w.10).

| golden | fixture that reproduces it | notes |
|---|---|---|
| `coverage.jsonl` | `tests/fixtures/coverage/lcov.info` | and four variants of it — CRLF, no final newline, absolute `SF:` paths, and repeated `SF` blocks summed per path — all of which must produce the same bytes |
| `coverage-istanbul.jsonl` | `tests/fixtures/coverage/coverage-summary.json` | the `total` entry is istanbul's own; two `node_modules` entries in the fixture are counted in that total and get no file record, which is why the total exceeds the sum of the file records |
| `coverage-log-scrape.jsonl` | `tests/fixtures/coverage/act-log-scrape.jsonl` | only scraped when no payload file exists at all |

`coverage.jsonl`'s total is therefore exactly the sum of its four file records (ADR-0002 §4.3),
which the original hand-written version — copied from the ADR's illustrative snippet — was not;
`src/parser/tokenizer.rs` was raised to `2617/2199` lines and `896/708` branches to make the two
agree. The ADR's snippet itself is untouched: it illustrates one record, not a run.

## `invalid/` — one violation per file

| file | rule being enforced (ADR-0002 §) |
|---|---|
| `missing-required-field.jsonl` | envelope `task` is required (§2.2) |
| `explicit-null.jsonl` | `null` is never valid; absence expresses unknown (§2.1) |
| `unknown-schema-version.jsonl` | a writer never emits a version it does not know (§7) |
| `short-commit.jsonl` | full 40-hex SHA only — the path key depends on it (§1.2) |
| `uppercase-commit.jsonl` | SHA is lowercase (§2.2) |
| `unsorted-run-id.jsonl` | `run_id` must be timestamp-prefixed and sortable (§1.2) |
| `bad-timestamp-offset.jsonl` | instants are UTC `Z`, never `+01:00` (§2.2) |
| `unknown-field.jsonl` | `additionalProperties: false` (§2.1) |
| `absolute-source-path.jsonl` | no absolute host paths in evidence (§2.2) |
| `traversal-source-path.jsonl` | no `..` segments (§2.2, §6.3) |
| `unit-empty-string.jsonl` | `unit` is either a real unit or absent (§4.1) |
| `unit-missing-unflagged.jsonl` | a metric without a unit must carry `flags:["unit_missing"]` (§4.1) |
| `label-value-not-string.jsonl` | label values are always strings (§4.1) |
| `benchmark-unknown-source.jsonl` | `source` is refined per kind; `log_scrape` is coverage-only (§5.1) |
| `kind-unknown.jsonl` | `kind` is a closed enum (§2.2) |
| `coverage-forbidden-metric-fields.jsonl` | coverage records carry no `value`/`unit` (§4.1) |
| `coverage-file-scope-without-path.jsonl` | `scope:"file"` requires `path` (§4.2) |
| `coverage-total-with-path.jsonl` | `scope:"total"` forbids `path` (§4.2) |
| `coverage-traversal-path.jsonl` | coverage `path` is workspace-relative, no `..` (§4.2) |
| `coverage-absolute-path.jsonl` | container `/srv/...` must be normalised away (§4.2) |
| `coverage-unpaired-counts.jsonl` | `*_found` and `*_hit` are co-required pairs (§4.2) |
| `coverage-no-raw-counts-at-all.jsonl` | a real coverage record needs one complete pair (§4.2) |
| `coverage-percent-only-unflagged.jsonl` | `reported_percent` requires `flags:["percent_only"]` and `reported_measure` (§4.3) |
| `coverage-percent-with-raw-counts.jsonl` | a percent may not sit next to raw counts it could contradict (§4.3) |
| `coverage-unknown-source.jsonl` | `source` is refined per kind (§5.1) |
| `coverage-source-path-missing.jsonl` | file-derived records must say which file (§4.2) |
| `torn-line.jsonl` | a file not ending in LF is a torn write (§6.1) — checker-level, not schema-level |
| `trailing-comma.jsonl` | strict JSON per line (§6.1) |
| `blank-line.jsonl` | no blank lines (§6.1) |
| `not-an-object.jsonl` | one object per line, not an array (§6.1) |
| `crlf-line-endings.jsonl` | LF only (§6.1) |

`torn-line`, `trailing-comma`, `blank-line`, `not-an-object` and `crlf-line-endings` are byte-level rules
the JSON Schema cannot see; `scripts/validate_evidence.py` enforces them, and `act-mcp validate-evidence`
(mact-f1w.9) `MUST` reject them too.

## Checking

```sh
pip install jsonschema                                       # once
python3 scripts/validate_evidence.py tests/e2e/testdata/expected        # must exit 0
python3 scripts/validate_evidence.py tests/e2e/testdata/invalid         # must exit 1
python3 scripts/validate_tool_contract.py --stats                       # must exit 0
```

Any change to `schema/evidence.schema.json` must keep both commands above true, and must be reflected
in the ADR in the same commit.

## `tool-io/` — the ADR-0001 wire contract

One JSON document per file: a tool's `params.arguments`, or the object the server puts in
`result.structuredContent`, or the `isError: true` payload. Filename grammar — `validate_tool_contract.py`
reads the def to validate against out of the name:

```
<tool>.<args|result>.<scenario>.json        tool ∈ run_workflow | list_tasks | run_status | cancel_run
error.result.<scenario>.json                the shared $defs/tool_error payload
```

Regenerate the whole tree (it is generated once, from a reviewable script, so the files are byte-stable):
`python3 scripts/gen_tool_contract_fixtures.py`.

### `tool-io/valid/` — 21 documents that must validate

| file | pins |
|---|---|
| `run_workflow.args.minimal.json` | the four required identity arguments and nothing else; 7-hex commit abbreviation |
| `run_workflow.args.full.json` | every optional argument, uppercase commit, `.github/workflows/…` path form, `inputs`/`secrets` maps, `idempotency_key` |
| `run_workflow.result.success.json` | the reference `run_view`: green run, one evidence file, 5 records |
| `run_workflow.result.failed.json` | `status: "failed"` + `exit_code` — a red run is a normal result, and it still wrote evidence |
| `run_workflow.result.running.json` | `wait: false` shape: non-terminal, `duration_ms` is elapsed, no `finished_at` / `exit_code` / `error` |
| `run_workflow.result.error.json` | `status: "error"` requires `error`, and `error.code` comes from the runner group |
| `run_workflow.result.timed_out.json` | timeout is its own terminal status, not `error` and not `cancelled` |
| `run_workflow.result.cancelled.json` | `cancelled` + `partial_evidence_dropped` warning |
| `run_workflow.result.deduplicated.json` | `deduplicated: true` replay via `idempotency_key` |
| `list_tasks.args.empty.json` | no arguments is legal |
| `list_tasks.args.filtered.json` | `workflow` filter + `include_steps` |
| `list_tasks.result.full.json` | jobs with needs / matrix / runs_on / steps, advisory `category_hint`, and a `parse_error` file inside a successful call |
| `list_tasks.result.filtered.json` | the answer to `list_tasks.args.filtered.json` on the same three files: one entry, step summary, no `parse_error` warning because the broken file was not selected |
| `run_status.args.list.json` | listing mode: `{}` |
| `run_status.args.single.json` | single-run mode with `log_tail_bytes` |
| `run_status.result.running.json` | exactly one of `run` / `runs`: here `run` |
| `run_status.result.list.json` | `runs[]` of `run_summary`, newest first, `truncated: true` |
| `cancel_run.args.json` | `run_id` is required here |
| `cancel_run.result.cancelled.json` | real cancel: `cancel_requested_at` + `kill` |
| `cancel_run.result.already-terminal.json` | idempotent cancel: `already_terminal: true`, old status kept, no `cancel_requested_at` / `kill` |
| `error.result.workflow-not-found.json` | resolution error carrying `available`, so the caller self-corrects in one round trip |
| `error.result.workspace-busy.json` | busy error carrying the in-flight `run_id` and `phase` |

### `tool-io/invalid/` — 34 documents that must be rejected

| file | rule (ADR-0001 §) |
|---|---|
| `run_workflow.args.missing-task.json` | `task` is required (§2.1) |
| `run_workflow.args.unknown-field.json` | a caller-supplied `run_id` is `unknown_argument`; run ids are server-generated (§8) |
| `run_workflow.args.short-commit.json` | commit is 7..=40 hex (§2.5) |
| `run_workflow.args.nonhex-commit.json` | hex only — no whitespace, no `~`, no leading `-` (§2.5) |
| `run_workflow.args.workflow-traversal.json` | `..` is not expressible in a workflow name (§2.2) |
| `run_workflow.args.category-uppercase.json` | `category` is lowercase snake_case, and the shape check is all the validation it gets (§2.4) |
| `run_workflow.args.timeout-too-large.json` | `timeout_secs` hard bound 21600 (§2.6, §9) |
| `run_workflow.args.secret-value-not-string.json` | secret values are strings (§2.7) |
| `run_workflow.args.bad-idempotency-key.json` | `idempotency_key` may not contain spaces (§2.1) |
| `run_workflow.result.missing-run-id.json` | every run result carries `run_id` (§3.1) |
| `run_workflow.result.status-succeeded.json` | the word is `success`; `succeeded` is not a status anywhere (§3.2) |
| `run_workflow.result.terminal-without-finished-at.json` | terminal ⇒ `finished_at` (§3.1) |
| `run_workflow.result.running-with-finished-at.json` | non-terminal ⇒ no `finished_at` (§3.1) |
| `run_workflow.result.failed-with-error.json` | `error` is forbidden unless `status: "error"` (§3.1) |
| `run_workflow.result.error-status-without-error.json` | `status: "error"` requires `error` (§3.1) |
| `run_workflow.result.explicit-null.json` | `null` is never valid; absence expresses unknown (§3.1, ADR-0002 §2.1) |
| `run_workflow.result.absolute-evidence-path.json` | no absolute host paths in a result (§3.3) |
| `run_workflow.result.short-commit.json` | a result always carries the full 40-hex commit (§3.1) |
| `run_workflow.result.unknown-error-code.json` | the error-code set is closed (§5.3) |
| `run_workflow.result.log-tail-too-long.json` | `log_tail` respects the 65536-byte cap (§2.8, §9) |
| `run_workflow.result.missing-deduplicated.json` | `deduplicated` is required on `run_workflow` results (§3) |
| `run_workflow.result.terminal-flag-disagrees-with-status.json` | `terminal` must agree with `status` (§3.1) |
| `run_status.args.bad-run-id.json` | `run_id` shape is ADR-0002 §1.2 (§3.1) |
| `run_status.args.unknown-field.json` | no extra arguments (§2) |
| `run_status.result.neither-run-nor-runs.json` | exactly one of `run` / `runs` (§6.2) |
| `run_status.result.both-run-and-runs.json` | exactly one of `run` / `runs` (§6.2) |
| `cancel_run.result.already-terminal-with-kill.json` | nothing was killed, so no `kill` (§6.3) |
| `cancel_run.result.missing-cancel-requested-at.json` | a real cancel records `cancel_requested_at` (§6.3) |
| `list_tasks.result.unknown-field.json` | no extra fields, and no absolute workspace path (§6.1) |
| `list_tasks.result.bad-category-hint.json` | `category_hint` ∈ benchmark \| coverage \| other (§6.1) |
| `list_tasks.result.absolute-file.json` | workflow paths are workspace-relative (§6.1) |
| `error.result.missing-retryable.json` | `retryable` is required and comes from the table (§5.2) |
| `error.result.detail-not-object.json` | `detail` is an object (§5.2) |
| `error.result.double-nested.json` | `structuredContent` of an error is exactly `{"error": …}` (§5.2) |

Three of them (`run_workflow.args.short-commit.json`, `run_workflow.args.workflow-traversal.json`,
`error.result.double-nested.json`) are one violation each but report several leaf messages — a 6-hex string
breaks both the pattern and `minLength`. `--stats` prints the count so it cannot quietly grow.

Volatile values are pinned the same way as the evidence goldens so the e2e harness (mact-f1w.15) can
normalise them explicitly: `commit` `2f7c1a4b…c91`, `run_id` `20261110T011507Z-d4e5f6` (coverage) and
`20261110T004203Z-a1b2c3` (benchmark), `started_at` / `finished_at` to the matching instants, and
`log_tail` free of absolute paths (`<WORKSPACE>` / `<EVIDENCE>`).
