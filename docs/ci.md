# CI for act-mcp

Everything CI checks lives in a script. `.github/workflows/ci.yml` is setup plus one call
into `scripts/`, because a check you cannot run locally is a check you cannot debug, and
this repository's convention is that the logic of a check sits in a testable script rather
than in YAML nobody can run.

| file | what it is |
|---|---|
| `scripts/ci.sh` | the whole check: `fmt`, `clippy`, `doc`, `test`, `schema`, `supply-chain`, `release`. Run it bare for everything, or name steps. `--help` is the reference |
| `scripts/smoke_binary.sh` | proves a built binary is a runnable binary (used by `ci.sh release` and by the release job) |
| `scripts/install_ci_tools.sh` | installs what `ci.sh` needs beyond rustup: `cargo-deny` (pinned, sha256-pinned) and python's `jsonschema` |
| `scripts/ci_drill.sh` | injects one violation per step into a throwaway copy and asserts the step fails — proof that each check can fail |
| `.github/workflows/ci.yml` | the pipeline: `check` (all of the above, one matrix-free job) and `release` (per-platform build + smoke test + artifact) |

## 1. Running it locally

```sh
scripts/ci.sh                       # everything, stops at the first failure
scripts/ci.sh fmt clippy            # a pre-commit hook's worth of checking
scripts/ci.sh --keep-going          # what CI's check job runs, plus the release step
scripts/ci.sh --help                # steps, flags, environment, exit codes
```

`scripts/ci.sh --offline` skips exactly one thing (`cargo deny check advisories`, which
fetches the RUSTSEC database) and says so in the log. Nothing else needs the network.

Two environment notes, both from docs/testing.md: on a bind-mounted checkout rustc can take
SIGBUS while linking into `./target`, so `CARGO_TARGET_DIR=/tmp/tt` is the fix when that is
what you are fighting; and `PYTHON` names the interpreter the schema step uses, which
matters on boxes where `pip` and `python3` disagree about where packages live.

## 2. The steps, and what each one is for

| step | command | what it catches |
|---|---|---|
| `fmt` | `cargo fmt --all -- --check` | hand-formatting drift; fix is `cargo fmt --all` |
| `clippy` | `cargo clippy --all-targets --all-features -- -D warnings` | lints, and — because warnings are fatal — a toolchain bump's new opinions. A suppress carries `reason =` |
| `doc` | `cargo doc --no-deps --all-features` with `RUSTDOCFLAGS=-D warnings -D rustdoc::broken_intra_doc_links` | a doc link that stopped pointing at the thing it meant. This crate's module docs carry the reasoning for its design, so a dangling link is a lost argument, not cosmetics |
| `test` | `cargo test --all-features` | the contract: unit tests, one suite per tool, the MCP 2026-07-28 conformance matrix, and `tests/e2e` (real server, stub `act`, `.evidence` on disk) |
| `schema` | see below | the JSON Schema of both accepted contracts — ADR-0001's tool contract and ADR-0002's evidence format — and ADR↔schema drift |
| `supply-chain` | `cargo deny check licenses bans sources` + `... advisories`, then a `cargo tree -d` note | licences, duplicate/wildcard policy, dependency provenance, RUSTSEC |
| `release` | `cargo build --release` + `scripts/smoke_binary.sh` | that the artefact exists, reports its version, answers `--help`, refuses a bad flag, and validates evidence with its embedded schemas |

`test` does not pass `--ignored`. The `#[ignore]`d tests are the ones that wait on
wall-clock time (the two `PR_SET_PDEATHSIG` ones) or that need a 20 s term grace; nothing in
the default suite needs docker or `act`, and nothing in it may start needing them — a test
that needs a container daemon belongs behind `#[ignore]` with the reason in its
`reason =`, on a docker-capable runner, not in this pipeline.

### What `schema` runs, in order

1. `act-mcp validate-evidence --schema schema/evidence.schema.json --meta-schema
   schema/run-meta.schema.json tests/e2e/testdata/expected` — the engine the binary embeds
   (`jsonschema-rs`, `include_str!` of the same bytes) compiles the schema *files* and
   validates the committed evidence tree. This is the hard one: it fails if the schema stops
   compiling or a record stops satisfying it, with no python involved.
2. `scripts/validate_tool_contract.py`, `scripts/validate_run_lifecycle.py` and
   `scripts/validate_isolation.py` — the three ADR↔schema checkers docs/testing.md calls CI's:
   ADR-0001's tool contract, ADR-0004's run lifecycle and ADR-0003's isolation protocol, each
   checked against the schema that implements it. (ADR-0002's checker is
   `validate_evidence.py`, which is check (1) above and check (3) below.) Of the three,
   `validate_tool_contract.py` is the one that keeps ADR-0001's error-code table, the run
   status vocabulary and the fixture index in
   [`tests/e2e/testdata/README.md`](../tests/e2e/testdata/README.md) in step with
   `schema/tool-contract.schema.json`; its fixture-validating half needs `jsonschema` (see 3)
   and says so when the package is missing, while its docs-parity half — the half that catches
   a renamed error code or an undocumented fixture — always runs.
3. Only when `$PYTHON` can `import jsonschema`: every `schema/*.json` against the meta-schema
   it declares, and `scripts/validate_evidence.py` over the same tree as a second,
   independent implementation. When the package is missing these two are **reported as
   skipped** — they are second opinions; the verdict is (1) and (2). `scripts/install_ci_tools.sh`
   installs it, or set `PYTHON=` to an interpreter that has it.

The fixture index rule is worth naming because it is the one a newcomer trips: every file in
`tests/e2e/testdata/tool-io/{valid,invalid}/` has to be named in
`tests/e2e/testdata/README.md`, which is the human-readable statement of what the schema
forbids. Add a fixture without a row and `schema` goes red; the row is the documentation, and
an undocumented fixture is a contract rule nobody can look up. Fixtures themselves come from
`scripts/gen_tool_contract_fixtures.py`, which is idempotent — edit the generator, re-run it,
never hand-edit a fixture.

## 3. The pipeline

```
check ──► release (x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu)
```

* **Triggers.** Pushes to `master` that touch the crate (`src/`, `tests/`, `schema/`,
  `scripts/`, `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, `deny.toml`, the workflow
  itself) and every pull request — PRs are unfiltered because a required status check that
  sometimes does not run is a required status check that blocks merges. A docs-only push
  does not pay for a build.
* **Toolchain.** Nothing installs a toolchain: rustup reads `rust-toolchain.toml`, which is
  the pin (see §4), and installs the `clippy`/`rustfmt` components listed in it.
* **Cache.** `actions/cache` over `~/.cargo` (registry, index, git checkouts, installed
  binaries) and `target/`, keyed on `Cargo.lock` + `rust-toolchain.toml` with partial
  restore-keys, so a lockfile bump keeps the toolchain's build and a toolchain bump keeps
  the downloads.
* **Limits.** `timeout-minutes` on both jobs (25 and 30), `permissions: contents: read` at
  the top — nothing here writes to the repository, and a CI token that could is a token
  nobody reviewed. Concurrency cancels a superseded PR push.
* **`--keep-going`.** The check job runs every step even after one fails, so one red step
  does not hide the others.
* **`release`** builds each platform this project ships, smoke-tests the artefact
  (`scripts/smoke_binary.sh`, which appends its version and sha256 to the step summary),
  and uploads `act-mcp-<target>` containing the binary, `dist/version.txt` and
  `dist/sha256.txt`. `download-artifact` drops the exec bit — `chmod +x` after download.

Cold, the check job is dominated by compiling dependencies once (cached afterwards) and the
`test` step; the release job is a second, optimised build per platform. Total wall time on
a cold cache is under ten minutes for `check`, and `release` runs after it.

## 4. Toolchain policy

* `Cargo.toml`'s `rust-version = "1.88"` is the **floor**: rmcp 3.9's own floor, and the
  oldest compiler allowed to build this crate. Do not lower it.
* `rust-toolchain.toml` is the **pin**: the one toolchain CI and every local `cargo`
  invocation use, so "green on my box" and "green on GitHub" are the same claim. It is
  1.99.1 rather than the 1.88 floor because the committed `Cargo.lock` cannot be built by
  1.88.0 — `uuid 1.29.2` in the graph requires rustc 1.89 — so pinning the floor would pin
  a compiler that fails on this lockfile.
* Bumping the pin is a code change: a newer clippy is a new set of `-D warnings` opinions,
  so run `scripts/ci.sh` and read what it says before and after.

## 5. Supply-chain policy

`deny.toml` is the policy, `scripts/ci.sh supply-chain` is the enforcement, and
`scripts/install_ci_tools.sh` is how the checker is obtained (pinned version, sha256 pinned
in the script — a version bump is a reviewable two-line diff).

* **licences**: permissive set, exactly the crates the graph needs today; a new licence is a
  one-line, reviewed decision rather than a shrug.
* **bans**: duplicate versions warn (and `ci.sh` prints the `cargo tree -d` note next to
  them — two majors of `getrandom` is normal, two majors of a big crate for no reason is
  worth a commit message); wildcard requirements are denied; `serde_yaml` is banned outright
  because Cargo.toml already decided against it (RUSTSEC-2024-0320).
* **sources**: crates.io only. A `git` dependency needs a named entry in `deny.toml`.
* **advisories**: the RUSTSEC database, dev-dependencies included, yanked = deny. This is
  the one check that needs the network.

## 6. Proving the checks can fail

`scripts/ci_drill.sh` copies the tree to a temporary directory, injects one violation per
check, and asserts that the matching `scripts/ci.sh` step exits non-zero:

| drill | violation injected | step that must fail |
|---|---|---|
| `fmt-misformatted-code` | valid Rust that rustfmt rejects | `fmt` |
| `clippy-lint` | `return x;` at the end of a function (`clippy::needless_return`) | `clippy` |
| `doc-broken-intra-doc-link` | a public doc comment linking to a non-existent item | `doc` |
| `test-failing-assertion` | a unit test that asserts false | `test` |
| `schema-broken-schema-file` | `schema/evidence.schema.json` edited into a document the engine refuses to compile | `schema` |
| `schema-bad-evidence-record` | a record missing the required envelope fields appended to a golden | `schema` |
| `schema-undocumented-tool-io-fixture` | the `tests/e2e/testdata/README.md` row naming one tool-io fixture, deleted | `schema` |
| `supply-chain-disallowed-licence` | `MIT` removed from `deny.toml`'s allow list | `supply-chain` |

Run it after changing `ci.sh` itself, and before trusting a "green" that arrived after
someone touched a check:

```sh
scripts/ci_drill.sh                 # all of them
scripts/ci_drill.sh --step schema   # just the schema drills
scripts/ci_drill.sh -v              # print the captured step logs
```

It uses its own `CARGO_TARGET_DIR` (kept between runs, `$TMPDIR/act-mcp-drill-target` by
default) so it cannot churn yours — and do not make it share one: two source trees with the
same package name and version under one target directory can convince cargo that the other
tree's artefact is fresh. The first run pays a cold dependency build, afterwards it is quick.

It is not a step in the pipeline, and that is a cost decision rather than an oversight: eight
drills means eight compiles of the crate, which would double the check job to guard a file
(`ci.sh`) that changes rarely. Running it is part of *changing* `ci.sh`, `ci_drill.sh`, or a
checker they call — the same rule as re-running a test when you edit the test, not a rule the
pipeline can enforce for you.

Two plumbing notes, because both cost an hour once:

* a step runs inside `( set -e; step_fn )` rather than `if step_fn` — in an `if` *condition*
  `set -e` is switched off for the whole function body, so a failing command in the middle
  of a step would fall on the floor and the step would report the exit status of the last
  command it ran;
* step names use hyphens (`supply-chain`) and bash function names use underscores
  (`step_supply_chain`); `ci.sh` maps one to the other and refuses a step with no
  implementation instead of reporting 127.

## 7. Deliberately not checked here

* **Real `act`, real docker.** The runner has neither and must never need either;
  `tests/e2e` is what proves the pipeline without them (docs/testing.md §1). Anything that
  needs a container daemon is `#[ignore]`d and lives on a docker-capable runner.
* **Non-linux platforms.** `deny.toml`'s `[graph] targets` and the release matrix are the
  platforms this project ships; `runner` and `registry` are POSIX-only by decision, so a
  windows build is not a goal this pipeline guards.
* **The workflow YAML itself.** `ci.yml` gets lint-checked with
  [`actionlint`](https://github.com/rhysd/actionlint) by hand before a change is committed;
  it is not a step in the pipeline, because the file it would check is the file that runs the
  check. It is cheap to add to `install_ci_tools.sh` if the workflow starts changing often.
* **Publishing.** `publish = false`; the release job builds and smoke-tests artefacts, it
  does not ship them. A release workflow that attaches binaries to a tag is a later ticket,
  and `scripts/ci.sh release` is the piece it would reuse.
