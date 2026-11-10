# `tests/e2e/testdata` — contract fixtures

These files are the executable half of [ADR-0002](../../../docs/adr/0002-evidence-jsonl-contract.md).
They were **written by hand from the ADR and the schema**, not emitted by the implementation, so that
the implementation has something independent to be measured against.

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
└── invalid/                         # each file must be REJECTED, for exactly one reason
```

## Shape of the goldens

Every record is one compact JSON object per line, LF-terminated, keys in envelope-then-payload order
so a byte-for-byte golden diff is stable. Volatile fields are pinned to fixed values so the e2e harness
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
```

Any change to `schema/evidence.schema.json` must keep both commands above true, and must be reflected
in the ADR in the same commit.
