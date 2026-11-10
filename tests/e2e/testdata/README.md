# Test fixtures

Test data for the `run_workflow` pipeline. Everything here is input to a test and is
checked in so a failure is reproducible without docker, network, or a registry pull.

| dir | contents |
|---|---|
| `bin/` | `act` — the stand-in for the real binary (see its header comment) and `docker` — a container-CLI that only answers the startup probe |
| `workflows/` | fixture `.github/workflows` files for the e2e scenarios, including one deliberately broken |
| `payload/` | what a job writes through the bind mount: `lcov.info`, `coverage-summary.json`, `benchmark.json` |
| `logs/` | captured `act --json` output (success + failure) used for timing and redaction checks |
| `tool-io/` | captured tool request/response pairs the tool-io contract tests replay |
| `scenarios/` | the golden evidence trees, one directory per e2e scenario — written by `E2E_UPDATE_GOLDEN=1 cargo test --test e2e`, never by hand |

The payloads are small on purpose — a few files, a few samples — but carry the real shape
(lcov `SF:`/`DA:`/`BRDA:`, Istanbul's `total` + per-file keys, a benchmark file with
samples and labels). Anything a parser accepts must be here in the shape the tool actually
emits, and the e2e goldens are the record of what the pipeline did with each.
