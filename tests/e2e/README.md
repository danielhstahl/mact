# `tests/e2e` — the whole pipeline, with a fake `act`

`cargo test --test e2e` boots the **real `act-mcp` server binary** against a throwaway
git workspace and drives `call → run → .evidence` end to end. The only fake in the loop
is `act` itself: `tests/e2e/testdata/bin/act` is a shell script that is a stand-in for
`nektos/act`, and `tests/e2e/testdata/bin/docker` is a stand-in for the container daemon
CLI. Everything else is production code — the MCP layer, argv construction, spawn and
process-group ownership, log tail and cap, evidence writer, validators, the whole
pipeline.

CI has no docker and no act, so *this* is the suite that proves the pipeline in CI. The
real-`act` integration tests (`tests/heavy/`) are `#[ignore]`d here and run on a
docker-capable runner; this suite runs everywhere and must never need docker, network, or
a registry pull.

| piece | what it is |
|---|---|
| `main.rs` | the scenarios, one `#[tokio::test]` each, numbered in pipeline order |
| `harness.rs` | boot the server on a random port against a fixture workspace; drive it; talk to the stub |
| `golden.rs` | snapshot the evidence tree to `testdata/scenarios/<name>/`, compare or regenerate |
| `normalize.rs` | replaces run id / commit / timestamps / pids / paths with `{PLACEHOLDERS}` so a golden can be diffed at all |
| `testdata/` | the stub, the fixtures (see `testdata/README.md`) and the goldens |

## What a scenario looks like

```rust
let scenario = Scenario::new("01-coverage-success", &["coverage.yml"]).await;
let run_id = scenario.run("coverage", "coverage", "coverage").await;
let result = scenario.wait_result(&run_id).await;
// ...assertions about the result...
scenario.assert_evidence().await;   // golden the tree, validate it
```

Three rules keep this suite trustworthy, and each is a scenario of its own:

1. **The stub is a client contract test, not a mock.** It parses the argv the real `act`
   would parse, refuses a flag it does not know (`--secret`, a container daemon socket
   flag, a positional event name — see `scenario_12`), takes its input through
   `--secret-file` and `ACTMCP_EVIDENCE_OUT` exactly as the real one must, and exits with
   a real exit code. If `argv.rs` regresses, the stub fails the run instead of quietly
   accepting the flags it would be given.
2. **Nothing asserts on a wall-clock duration or a pid value.** Those are volatile:
   `normalize.rs` turns them into `{NUMBER}`. A scenario that waits on sleep arithmetic
   would be flaky on a loaded CI box, so the harness waits on files and on run status
   only.
3. **The golden is the evidence tree as written** — every file, with the volatile parts
   normalised — plus `cargo run -- validate-evidence` over it. That catches the failure
   mode "the run looked fine and the file is wrong": an array-wrapped JSONL, a record
   under the wrong commit, a payload that leaked into the published file, an
   `incoming/` that was never cleaned.

## Adding a scenario, or changing one

Write the test in `main.rs`, choose a golden directory name (`NN-short-kebab-name`), run:

```sh
E2E_UPDATE_GOLDEN=1 cargo test --test e2e -- --nocapture scenario_04
```

then **read** the goldens it wrote under `testdata/scenarios/<name>/` before committing —
a golden recorded from a run that was itself wrong freezes the bug in. If a new volatile
field appears in evidence (a new timestamp, a new path), add it to `normalize.rs` and its
table there; do not let a raw timestamp into a golden, because the next day's CI run will
fail on it.

Steer the stub with the environment variables documented at the top of
`testdata/bin/act` (payload kind and location, exit code, when it dies, whether it
ignores SIGTERM, which files to report its pid through). Never add a stub flag that
`argv.rs` does not pass: the point is that the stub only ever sees argv real `act` sees.

## The one long scenario

Every scenario finishes in about a second except `scenario_04b` (`#[ignore]`d), which
proves the SIGTERM→SIGKILL escalation and so costs the shipped 20 s term grace. It cannot
be made shorter without a `--term-grace-secs` flag on `serve` that does not exist yet; run
it with `cargo test --test e2e -- --ignored` when you touch the runner's teardown.
