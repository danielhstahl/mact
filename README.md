# act-mcp

An HTTP [MCP](https://modelcontextprotocol.io) server — protocol revision **2026-07-28**,
Streamable HTTP, loopback, no authentication — that runs GitHub Actions workflows with
[nektos/act](https://github.com/nektos/act) and records what they produced as
`.evidence` JSONL.

Four tools: `list_tasks`, `run_workflow`, `run_status`, `cancel_run`. A run outlives the
request that started it, one run at a time per workspace, and every terminal run answers
with the `.evidence` files it wrote — benchmark and coverage records stamped with the commit
they describe.

There is no auth, by design: the server binds `127.0.0.1` and refuses any other bind unless
you say `--i-understand-this-is-unauthenticated`, because act runs arbitrary container
images. [docs/security.md](docs/security.md) is the threat model.

## Quick start

```sh
# a repository with .github/workflows, and act on PATH
cargo run -- serve --workspace ~/repo/with/github-workflows

# then point any 2026-07-28 Streamable HTTP client at it
curl -sS http://127.0.0.1:8080/mcp -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -H 'MCP-Protocol-Version: 2026-07-28' -H 'Mcp-Method: tools/call' -H 'Mcp-Name: list_tasks' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_tasks","arguments":{},
       "_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28",
                "io.modelcontextprotocol/clientInfo":{"name":"curl","version":"0"},
                "io.modelcontextprotocol/clientCapabilities":{}}}}'
```

`scripts/` is also a CLI of its own: `act-mcp validate-evidence <workspace-or-path>` checks
an evidence tree against the pinned schema (exit 0 clean, 1 findings, 2 could not run).

## Building it

You need a Rust toolchain and `git`. That is all: **no docker and no act are required to
build or to run the test suite** — the `act` the tests run is a shell script
(`tests/e2e/testdata/bin/act`) and the container daemon is probed through an injectable
seam.

`rust-toolchain.toml` pins the exact toolchain (`channel = "1.99.1"`, plus `clippy` and
`rustfmt`), and rustup honours it in this directory, so CI and your checkout compile with
the same compiler. `Cargo.toml`'s `rust-version = "1.88"` is the separate **floor** — the
oldest toolchain that may build this crate (rmcp 3.9's floor). Why the pin sits above the
floor, and what a bump involves: [docs/ci.md](docs/ci.md) §4.

```sh
cargo build                # debug binary: target/debug/act-mcp
cargo build --release      # release binary: target/release/act-mcp
```

## Checks

One script runs everything CI runs — locally, in the same order, with the same verdict:

```sh
scripts/ci.sh --help                # steps, flags, exit codes
scripts/ci.sh fmt clippy            # a pre-commit hook's worth of checking
scripts/ci.sh --keep-going          # what CI's check job runs
scripts/install_ci_tools.sh         # cargo-deny + python jsonschema, pinned
```

`fmt`, `clippy -D warnings`, `rustdoc` (broken intra-doc links are errors), `cargo test
--all-features`, schema validation of `schema/*.json` and the evidence tree, `cargo deny`,
and a release build that is smoke-tested by running it.
[docs/ci.md](docs/ci.md) documents each step, the pipeline, the supply-chain policy, and
`scripts/ci_drill.sh`, which proves every one of those checks can actually fail.

The GitHub workflow is `.github/workflows/ci.yml`: a `check` job that calls
`scripts/ci.sh --keep-going …` and a `release` job that builds and smoke-tests the binary
for `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu`. It contains no check logic —
logic lives in `scripts/`, which is also what makes it runnable before you push.

## Layout

| path | what it is |
|---|---|
| `src/server.rs`, `src/tools/` | the rmcp service, the mounted endpoint, one module per tool |
| `src/registry/` | the run supervisor: state machine, run registry, workspace lock (ADR-0004) |
| `src/runner/` | the act process: argv, log tee with redaction, process-group teardown |
| `src/workspace/` | the pre-start validation gate: commit/HEAD gates, isolation policy, probes |
| `src/evidence/` | the `.evidence` JSONL contract as code: records, writer, extractors, the checker |
| `schema/` | the machine-readable contract: `evidence`, `run-meta`, `tool-contract` |
| `tests/` | one suite per tool, the protocol conformance matrix, `tests/e2e` (the whole pipeline) |
| `scripts/` | the CI scripts and the ADR/schema consistency checkers |

## Further reading

* [docs/tool-contract.md](docs/tool-contract.md) — the tool shapes and the error vocabulary
* [docs/adr/0001-tool-contract.md](docs/adr/0001-tool-contract.md) — why `run_workflow` looks like it does
* [docs/adr/0002-evidence-jsonl-contract.md](docs/adr/0002-evidence-jsonl-contract.md) — the `.evidence` contract
* [docs/adr/0003-commit-isolation-and-serialization.md](docs/adr/0003-commit-isolation-and-serialization.md) — what may be run, and one run at a time
* [docs/adr/0004-run-lifecycle.md](docs/adr/0004-run-lifecycle.md) — runs that outlive the request
* [docs/testing.md](docs/testing.md) — the suites, and how to read a red one
* [docs/ci.md](docs/ci.md) — CI, in full
* [docs/security.md](docs/security.md) — loopback-only, and what that does and does not protect
