# CI for act-mcp

Everything CI checks lives in a script. `.github/workflows/ci.yml` is setup plus one call
into `scripts/`, because a check you cannot run locally is a check you cannot debug, and
this repository's convention is that the logic of a check sits in a testable script rather
than in YAML nobody can run. That is as true for the fathom crate as for the server: one
pipeline, one `scripts/ci.sh`, three crates, two binaries.

| file | what it is |
|---|---|
| `scripts/ci.sh` | the whole check: `fmt`, `clippy`, `build`, `doc`, `docs-site`, `test`, `tui-snapshot`, `fathom-report`, `fathom-doctor`, `coverage`, `benchmark`, `mcp-driver`, `schema`, `supply-chain`, `release-workspace`, `release`. Run it bare for everything, or name steps. `--help` is the reference |
| `scripts/coverage.sh` | the coverage collector: `cargo llvm-cov` over the workspace into an lcov tracefile at `$ACTMCP_EVIDENCE_OUT/lcov.info` (ADR-0002 §3.2's first path), the totals printed the way the contract reads them, and `--check` for the parsing/path/floor logic that needs no toolchain |
| `scripts/bench.sh` | the benchmark producer: cold, warm, store-open and index timings over a throwaway fixture, published as an ADR-0002 §3.3 `benchmark.json` that it validates against every bound the contract states before it claims success |
| `scripts/mcp_run.sh` | the runner driver one job calls: asks a live `act-mcp serve` to run a task and fails unless the runner advertised it, ran it to `success` on a `runs-on` label act could actually map, and published a record — `--check` proves all five outcomes against a stub server, no act required |
| `scripts/smoke_binary.sh` | proves a built `act-mcp` is a runnable binary (used by `ci.sh release` and by the release job) |
| `scripts/smoke_fathom.sh` | proves a built `fathom` is a runnable binary, and that `report`/`doctor` keep their 0/1/2 promise on a real evidence tree (`--check artefact\|report\|doctor\|all`); the binary is a positional, defaulting to `$CARGO_TARGET_DIR`/`target/debug/fathom` so the step a doc prints is a line you can paste |
| `scripts/check_dependency_firewalls.sh` | the two dependency-graph promises `deny.toml` cannot make: ADR-0005's forbidden edges are absent *transitively*, and no crate in any shipped target's graph needs a C toolchain |
| `scripts/build_docs_site.py` | the docs site: `docs/` in, static HTML out, standard library only. `--check` is its own suite (anchors, the Markdown subset, the refusal cases, byte-for-byte reproducibility); a real build is the check that every link inside `docs/` still resolves |
| `scripts/install_ci_tools.sh` | installs what `ci.sh` needs beyond rustup: `cargo-deny` (pinned, sha256-pinned) and python's `jsonschema` |
| `scripts/ci_drill.sh` | injects one violation per step into a throwaway copy and asserts the step fails — proof that each check can fail |
| `.github/workflows/ci.yml` | the pipeline: `check` (all of the above, one matrix-free job) and `release` (per-platform supply-chain gate, workspace release build, one artefact each for `act-mcp` and `fathom`) |
| `.github/workflows/cd.yml` | the release train: one tag sets one version, then `mact-<target>.tar.gz` and `fathom-<target>.tar.gz` are built, smoke-tested and uploaded as separate steps |
| `.github/workflows/coverage.yml` | the coverage *producer*, hand-triggered because the runner runs it (`act -j coverage`); writes the tracefile to `$ACTMCP_EVIDENCE_OUT` |
| `.github/workflows/benchmarks.yml` | the benchmark *producer*, same shape (`act -j benchmark`); writes `benchmark.json` to `$ACTMCP_EVIDENCE_OUT` |
| `.github/workflows/mcp-evidence.yml` | the evidence jobs: on every push and PR, starts `act-mcp serve`, asks it over MCP to run each producer, and fails unless the run was advertised, succeeded and published a record. §3a is why this exists and why the producers are elsewhere |
| `.github/workflows/docs.yml` | publishes `docs/` to GitHub Pages: `build` (self-test, build, artefact) and `deploy` (default branch only). §3b is what it publishes and why the logic is a script |

## 1. Running it locally

```sh
scripts/ci.sh                       # everything, stops at the first failure
scripts/ci.sh fmt clippy            # a pre-commit hook's worth of checking
scripts/ci.sh --keep-going          # what CI's check job runs, plus the release steps
scripts/ci.sh --help                # steps, flags, environment, exit codes
scripts/ci.sh tui-snapshot          # just the screens, drawn: ~2s once it is built
scripts/ci.sh fathom-report         # the report goldens, then the artefact over a sample tree
scripts/ci.sh coverage benchmark mcp-driver   # the evidence jobs' own logic, no act, no docker
scripts/coverage.sh --check         # the lcov rules alone; drop --check to actually collect coverage
scripts/bench.sh --check            # the §3.3 payload rules alone
scripts/mcp_run.sh --check          # the runner driver, against a stub MCP on loopback
scripts/smoke_fathom.sh --check all             # the headless contract, alone, on ./target/debug/fathom
scripts/build_docs_site.py --check            # the docs builder's own tests, no tree needed
scripts/build_docs_site.py --out /tmp/site    # build docs/ as a site and look at it
scripts/check_dependency_firewalls.sh                      # the graph's shape, alone
```

The three `fathom-*`/`tui-snapshot` steps are subsets of `test` on purpose — they cost seconds
once the tree is built, and a step whose name is the thing that broke is a step a reviewer reads.
`scripts/smoke_fathom.sh` and `scripts/sample_evidence.py` are runnable by themselves, which is the
same argument: a check you can point at a binary you just built is a check you can debug.

`scripts/ci.sh --offline` skips exactly one thing (`cargo deny check advisories`, which
fetches the RUSTSEC database) and says so in the log. Nothing else needs the network.

Two environment notes, both from docs/testing.md: on a bind-mounted checkout rustc can take
SIGBUS while linking into `./target`, so `CARGO_TARGET_DIR=/tmp/tt` is the fix when that is
what you are fighting — and `scripts/ci.sh` knows that failure's own signature, so it says which
fault it is looking at and reruns the command once before believing it, because a fault that comes
and goes is not a broken tree; and `PYTHON` names the interpreter the schema step uses, which
matters on boxes where `pip` and `python3` disagree about where packages live.

## 2. The steps, and what each one is for

| step | command | what it catches |
|---|---|---|
| `fmt` | `cargo fmt --all -- --check` | hand-formatting drift; fix is `cargo fmt --all` |
| `clippy` | `cargo clippy --all-targets --all-features -- -D warnings` | lints, and — because warnings are fatal — a toolchain bump's new opinions. A suppress carries `reason =` |
| `build` | `cargo build --workspace --all-features`, then a check that `act-mcp` **and** `fathom` exist under `target/debug/` | a binary that stopped linking. `clippy` and `test` compile the same graph as *their* targets and neither promises a shipped binary came out: this is the step that says the workspace still builds the two programs it ships (ADR-0005 §3) |
| `doc` | `cargo doc --no-deps --all-features` with `RUSTDOCFLAGS=-D warnings -D rustdoc::broken_intra_doc_links` | a doc link that stopped pointing at the thing it meant — including a link into a `#[cfg(test)]` module, which rustdoc never builds and therefore can never resolve. These crates' module docs carry the reasoning for their design, so a dangling link is a lost argument, not cosmetics; the fix is to name a linkable path or write the reference as a code span, never to soften the lint |
| `docs-site` | `scripts/build_docs_site.py --check`, then a real build of `docs/` into a scratch directory | the same promise one step earlier, about Markdown instead of Rust: a link from one document to another, or to a section of one, plus the rules that make a site out of a tree — GitHub-compatible heading anchors (so `#3-coverage-` means the section it says it means in a document and on the rendered page), a code fence that closes, front matter that closes. Takes about a second and needs no toolchain, and it is the same build `docs.yml` publishes |
| `test` | `cargo test --all-features` | the contract: unit tests, one suite per tool, the MCP 2026-07-28 conformance matrix, and `crates/act-mcp/tests/e2e` (real server, stub `act`, `.evidence` on disk) |
| `tui-snapshot` | `cargo test -p fathom --test tui_render --test tui_detail --test tui_detail_store --test tui_diff --test tui_bench --test tui_store` | the fathom screens, drawn into ratatui's `TestBackend` and compared against a golden written out in the test file. A mismatch is a *design* change wearing a test failure's clothes: read the diff, and re-record only what you meant to change (there is no `--accept` step here — the goldens are string literals in the test, so the diff is the review). Colour is asserted where it is decided, never in a snapshot |
| `fathom-report` | `cargo test -p fathom --test report`, then `scripts/smoke_fathom.sh --check report <fathom>` | first the two envelope goldens (`report.json`, and `report-sample-tree.json`, which is `scripts/sample_evidence.py`'s tree re-stamped onto the fixture's commits): a renamed, dropped or re-typed field is a failing test, not a broken dashboard downstream. Then the *artefact*: the same binary over the sample tree, checking the envelope's shape, that the tsv header is still the columns a `cut -f` is written against, that `fathom index` lands where the report reads, and that `--fail-on` exits 1 exactly when its own `gate.status` says `findings` |
| `fathom-doctor` | `scripts/smoke_fathom.sh --check doctor` (ci.sh names the binary it just built; without a path the script takes `target/debug/fathom`) | `fathom doctor --json`'s exit code is derived from its own checks (0 all ok, 1 worst is warn, 2 something failed), every documented check is present, and a directory that is not a work tree is exit 2 — "could not run", never "nothing to report". `doctor` is a command whose whole output is a status, so the status is smoke-tested rather than trusted. Note what this step does *not* do: it smoke-tests the contract on a temporary cache root, where the store is absent and the honest verdict is a `warn`, so a green step is not a claim that `fathom doctor` on your own checkout is clean |
| `coverage` | `scripts/coverage.sh --check` | the lcov arithmetic (a tracefile whose `LF`/`LH`/`BF`/`BH` totals are known by hand), the three output-path rules, and the `--fail-under` comparison — including that an empty tracefile is *absence* and not a 0% score. The instrumented run is not here: it needs a container and an evidence directory, so it lives in `coverage.yml` (§3a) |
| `benchmark` | `scripts/bench.sh --check` | the ADR-0002 §3.3 payload rules in both directions: a conforming payload accepted, and each refusal stated by the contract — missing `name`/`value`, a non-finite `value` (null means *failed*, not zero), the 16-character unit bound, eight flat string labels, 2000 bytes per record, 64 metrics, both accepted top-level shapes — shown refusing. Plus the median, including the even-count middle pair, because a mean of five samples is a benchmark one slow container can move |
| `mcp-driver` | `scripts/mcp_run.sh --check` | the five outcomes of the script `mcp-evidence.yml` trusts its exit code to: published, failed, succeeded-with-nothing, not-advertised, succeeded-with-the-leg-skipped — five real MCP calls to a stub server on loopback through `scripts/mcp_call.sh`. This is the check that the driver *fails* when evidence is missing or when act ran no steps at all, which is the only reason a green evidence job means anything |
| `schema` | see below | the JSON Schema of both accepted contracts — ADR-0001's tool contract and ADR-0002's evidence format — and ADR↔schema drift |
| `supply-chain` | `cargo deny check licenses bans sources` + `... advisories`, then a `cargo tree -d` note, then `scripts/check_dependency_firewalls.sh` | licences, duplicate/wildcard policy, dependency provenance, RUSTSEC — over **all three crates**, since one lockfile is one policy — and then the two things `deny.toml` cannot say: that ADR-0005's forbidden edges stay absent *transitively* (`cargo tree`, because `fathom` acquiring `schemars` through `act-evidence`'s `json-schema` feature is invisible in its manifest), and that no crate in any shipped target's graph invokes a C compiler (the refusal ADR-0003, ADR-0006 §5 and ADR-0007 §6 each made) |
| `release-workspace` | `cargo build --release --workspace` | the release profile over every member, before anything is packaged: an optimisation-time failure or a `cfg(not(debug_assertions))` arm is red here rather than in the middle of an artefact build |
| `release` | `cargo build --release -p <crate> --bin <crate>` + that binary's smoke, one artefact at a time (`--artefact act-mcp`, `--artefact fathom`, default both) | that each artefact exists, reports this tree's version, answers `--help`, refuses a bad flag, and does its real work: `act-mcp` validates evidence with its embedded schemas (`scripts/smoke_binary.sh`), `fathom` keeps `report`/`doctor`'s exit-code contract on a sample tree (`scripts/smoke_fathom.sh --check all`). Selective and one-at-a-time on purpose: the artefact is one renamed binary, so a broken `fathom` must never block shipping the server (ADR-0005 §3) |

`test` does not pass `--ignored`. The `#[ignore]`d tests are the ones that wait on
wall-clock time (the `runner_pdeathsig` net, and its helper) or that need a 20 s term grace;
nothing in
the default suite needs docker or `act`, and nothing in it may start needing them — a test
that needs a container daemon belongs behind `#[ignore]` with the reason in its
`reason =`, on a docker-capable runner, not in this pipeline.

### What `schema` runs, in order

1. `act-mcp validate-evidence --schema schema/evidence.schema.json --meta-schema
   schema/run-meta.schema.json crates/act-mcp/tests/e2e/testdata/expected` — the engine the binary embeds
   (`jsonschema-rs`, `include_str!` of the same bytes) compiles the schema *files* and
   validates the committed evidence tree. This is the hard one: it fails if the schema stops
   compiling or a record stops satisfying it, with no python involved.
2. `scripts/validate_tool_contract.py`, `scripts/validate_run_lifecycle.py`,
   `scripts/validate_isolation.py`, `scripts/validate_crate_layout.py`,
   `scripts/validate_fathom_layout.py`,
   `scripts/validate_mi_definition.py`,
   `scripts/validate_metric_model.py` and
   `scripts/validate_fathom_docs.py` — the eight
   ADR↔contract checkers docs/testing.md calls CI's (plus
   `scripts/sync_mi_adr_blocks.py --check`, which is not a checker of the same kind but a generator: it
   rewrites ADR-0007's two normative blocks from `schema/mi-definition.json` and fails the build where the
   committed ADR has drifted, so the schema and the ADR cannot land in separate commits): ADR-0001's tool contract, ADR-0004's run
   lifecycle, ADR-0003's isolation protocol, ADR-0005's crate layout, ADR-0006's `.fathom`
   workspace layout, ADR-0007's Maintainability Index, ADR-0008's per-commit metric model, each checked against the
   artifact that implements it (a `schema/*.json` for the first three, `Cargo.toml`/`Cargo.lock` for
   the fourth — ADR-0005's load-bearing form *is* the manifests, so that checker reads them instead of
   a fourth schema — and `.gitignore` plus `.fathom/LAYOUT.md` for the last, because ADR-0006's
   artifacts are a directory contract and the ignore rules that grant it visibility; it is what makes
   an orphaned or unreachable `.fathom` rule a build failure instead of a mystery; and
   `schema/mi-definition.json` plus the golden vectors in `crates/act-mcp/tests/testdata/mi` for the sixth, because
   what ADR-0007 pins is a set of numbers and a set of closed enumerations — the checker diffs the
   ADR's own copy of the constants against the JSON, re-derives every golden vector twice
   (`scripts/mi_reference.py` in binary64, the checker itself in 40-digit decimal), and fails if the
   `tree-sitter` ban ADR-0007 §6 depends on has been lifted; and
   `schema/metric-model.json` plus the fixture trees in `crates/act-mcp/tests/testdata/metrics` for the seventh,
   because what ADR-0008 pins is which records a commit's number is made of — that one additionally
   grades `scripts/metric_model_reference.py` against the recorded output of the jq recipes in
   `docs/evidence-format.md` §12, which is the only place this repo has the same question asked in two
   languages), plus the eighth, which checks documentation rather than a contract: it asks the built
   `fathom --help` (bare, and each of the four subcommands) what the argv actually is and fails when a
   flag, a help-overlay key or an exit code is undocumented, documented in two pages, or named in a page
   and absent from the binary — the cross-check the Fathom runbook claims, made a check instead of a
   rereading. It is why this step builds `fathom` as well as `act-mcp`: the checker reads the binary, not
   clap's source. (ADR-0002's checker is
   `validate_evidence.py`, which is check (1) above and check (3) below.) Of these,
   `validate_tool_contract.py` is the one that keeps ADR-0001's error-code table, the run
   status vocabulary and the fixture index in
   [`crates/act-mcp/tests/e2e/testdata/README.md`](../crates/act-mcp/tests/e2e/testdata/README.md) in step with
   `schema/tool-contract.schema.json`; its fixture-validating half needs `jsonschema` (see 3)
   and says so when the package is missing, while its docs-parity half — the half that catches
   a renamed error code or an undocumented fixture — always runs.
3. Only when `$PYTHON` can `import jsonschema`: every `schema/*.json` against the meta-schema
   it declares, and `scripts/validate_evidence.py` over the same tree as a second,
   independent implementation. When the package is missing these two are **reported as
   skipped** — they are second opinions; the verdict is (1) and (2). `scripts/install_ci_tools.sh`
   installs it, or set `PYTHON=` to an interpreter that has it.

The fixture index rule is worth naming because it is the one a newcomer trips: every file in
`crates/act-mcp/tests/e2e/testdata/tool-io/{valid,invalid}/` has to be named in
`crates/act-mcp/tests/e2e/testdata/README.md`, which is the human-readable statement of what the schema
forbids. Add a fixture without a row and `schema` goes red; the row is the documentation, and
an undocumented fixture is a contract rule nobody can look up. Fixtures themselves come from
`scripts/gen_tool_contract_fixtures.py`, which is idempotent — edit the generator, re-run it,
never hand-edit a fixture.

## 3. The pipeline

```
check ──► release (x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu, aarch64-apple-darwin)
                └─► supply-chain + release-workspace, then act-mcp and fathom, each built,
                    smoke-tested and uploaded as `act-mcp-<target>` / `fathom-<target>`
```

* **Triggers.** Pushes to `master` that touch the workspace (`crates/**` — every member's manifest,
  sources and tests, in one pattern that cannot forget the next crate — `schema/`,
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
* **`release`** builds each platform this project ships. It installs `cargo-deny` and runs
  `scripts/ci.sh --keep-going supply-chain release-workspace` first — the same gate the check job ran,
  now beside the binaries it governs, and on the triples (`*-musl`, `aarch64-apple-darwin`) that only
  this matrix resolves — then `scripts/ci.sh release`, which builds and smoke-tests each artefact and
  writes its `--version` and sha256 into `dist/<binary>/`. Both smoke scripts append a row to the step
  summary, and the job uploads `act-mcp-<target>` and `fathom-<target>`, each containing its binary plus
  that binary's `version.txt` and `sha256.txt`. `download-artifact` drops the exec bit — `chmod +x`
  after download.
* **`cd.yml`, the release train.** One tag sets one version (`scripts/set_version.py` writes
  `[workspace.package] version` once and every member inherits it, so `fathom` can never ship a
  different version from the server), then four steps per platform: build+smoke+tar `act-mcp` as
  `mact-<target>.tar.gz`, upload it, then build+smoke+tar `fathom` as `fathom-<target>.tar.gz` and
  upload that. Separate steps on purpose — by the time the fathom build runs, the server's tarball is
  already uploaded (ADR-0005 §3), and ADR-0005's open question "own artefacts, or ride the server's
  tarball?" was answered in favour of its own: a reader that needs none of the server's 200 crates
  should not download them, and a single tarball with two binaries in it is a tarball whose sha256
  describes both at once. `fathom` is not renamed inside its tarball; `act-mcp` is still renamed `mact`
  because that is the name the server has always shipped under.

Cold, the check job is dominated by compiling dependencies once (cached afterwards) and the
`test` step; the release job is a second, optimised build per platform. Total wall time on
a cold cache is under ten minutes for `check`, and `release` runs after it. The fathom steps
add seconds, not minutes: `tui-snapshot` is the drawn-screen suite (about a second once built),
`fathom-report` is one test binary plus one run of the CLI over a three-commit tree, and
`fathom-doctor` does not compile anything the report step did not already build.

## 3a. The evidence jobs: coverage and benchmark, through the runner

ADR-0006 §10 asks for one row no other check in this repository fills:

> | Evidence production (coverage, benchmark) | one job per category through act-mcp | act installed,
> network to the endpoint, a commit to run |

`fathom doctor` can answer "does a coverage job exist?" by reading `.github/workflows`, and before
`coverage.yml` existed it answered no. That question is worth one warning and not much else. The question
a repository actually needs an answer to is whether a job ran, through the runner, and produced a record
for the commit — because fathom's columns come from `.evidence/<category>/<commit>/*.jsonl` and nothing
else, so a workflow that exists and produces nothing leaves the timeline exactly as empty as no workflow
at all.

Three files, split on purpose:

| file | who runs it | what it is for |
|---|---|---|
| `coverage.yml` | the runner (`act -j coverage`), or a human with `workflow_dispatch` | run `scripts/coverage.sh`, which writes `$ACTMCP_EVIDENCE_OUT/lcov.info` |
| `benchmarks.yml` | the runner (`act -j benchmark`), or a human | run `scripts/bench.sh`, which writes `$ACTMCP_EVIDENCE_OUT/benchmark.json` |
| `mcp-evidence.yml` | GitHub, on every push and pull request | start `act-mcp serve`, ask it over MCP to run each producer, assert what came back |

* **Why the producers are separate files.** A job that starts a runner must not be a job a runner runs.
  `run_workflow` refuses the nested case itself with `act_in_container` (ADR-0006 §10.1), and the
  `if: ${{ !env.ACT }}` guard on each driver job means that if someone does ask for one, it exits in
  seconds instead of after a release build. The producers are `workflow_dispatch`-only for the mirror
  image of the same reason: GitHub never runs them on a push, so the act run is the only run, and the
  evidence it writes is the evidence the commit gets.
* **Why the job names are not free-form.** ADR-0002 §3.1 derives a job's category from the *whole words*
  in the job id and its `name:` — `coverage`, and `benchmark` in the singular. So `name: Coverage` and
  `name: Benchmark` are load-bearing: rename the benchmark job to `Benchmarks` and the plural is not the
  marker, the runner files the job as `other`, its payload is not extracted, `list_tasks` stops
  advertising it as a benchmark, and the column goes empty without a word of complaint from anyone. The
  same rule is why the driver passes `--category` explicitly and why `mcp_run.sh` compares it against
  `category_hint` instead of trusting it.
* **What a job has to look like for act to run it at all.** act starts a container for the labels in its
  own platform table, plus whatever `-P`/`--platform` entries the operator's actrc carries — and act-mcp
  never adds a `-P` of its own, because the operator's mapping is the operator's. Two shapes therefore do
  not run: a `runs-on` label act has no image for on that host (`ubuntu-24.04` on Apple silicon, whose
  image is amd64-only), and a `runs-on` that is an expression (`${{ matrix.runner }}`), which act reads
  as a literal. Both fail *open*: act exits 0 having skipped every leg, and the only trace is the
  `act_runs_on_unmapped` warning — which is why `mcp_run.sh` refuses it, and why `dogfood-local.yml` is a
  plain `ubuntu-latest` job with no matrix. For the same reason no act-runnable job pins
  `CARGO_BUILD_TARGET`: act starts a Linux container on a Mac too, and a Darwin triple there makes every
  step that links die in `cc` rather than tell anyone the OS was wrong.
* **What "it ran against the MCP" means concretely.** `mcp_run.sh` speaks the same protocol the
  inspector does, through `scripts/mcp_call.sh`, and it names its own four failures: `unreachable` (the
  endpoint never answered `list_tasks`), `not-advertised` (`list_tasks` does not offer that job, or
  offers it with a different `category_hint`), `not-success` (the run reached a terminal state that was
  not `success`, with `log_tail` and `.evidence/runs/<run_id>/act.jsonl` printed), `no-evidence`
  (the run succeeded and published no record for the commit it executed), and `runs-on-unmapped` (the
  run succeeded but act had no image for the job's `runs-on` label and skipped the leg, so the green
  status describes a container that never ran a step — `--allow-unmapped-runs-on` accepts it anyway).
  The last two are the point. A
  green job that published nothing is the failure this whole arrangement exists to make impossible, and
  the driver is the only place it can be caught — the run itself cannot see whether anyone read its
  output.
* **Then fathom reads it.** Each driver job ends with `fathom report` and `fathom doctor`, read-only and
  non-gating, over the tree that now contains the evidence. That is the loop the ADR describes: run →
  publish → read, with no step in the middle that has to be remembered.

Reproducing one locally, with no docker and no act — the same stub `act` the e2e suite drives (see
docs/testing.md §7), pointed at a throwaway checkout:

```sh
export PATH=$PWD/tests-stub:$PATH            # a `docker` stub: `info --format …` must exit 0
STUB_PAYLOAD=all act-mcp serve --workspace /tmp/ws \
  --act-binary crates/act-mcp/tests/e2e/testdata/bin/act --bind 127.0.0.1:8080 &
scripts/mcp_run.sh --workflow coverage.yml --task coverage --category coverage \
  --commit "$(git -C /tmp/ws rev-parse HEAD)" --evidence-kind coverage
```

With a real `act` and a reachable container runtime, the same driver runs the whole CI payload:
`dogfood-local.yml` is ci.yml's check step list shaped for a container act can start (§3a above), and
GitHub never runs it.

```sh
scripts/mcp_run.sh --workflow dogfood-local.yml --task check --category other \
  --commit "$(git rev-parse HEAD)" --timeout-secs 3600
```

## 3b. Publishing `docs/` as a static site

`docs/` is normative: ADR-0002 is the evidence contract, `docs/security.md` is the threat model,
`docs/tool-contract.md` is what clients code against. A reader who is not in a terminal and has not
cloned the tree still has to be able to read the set of documents as a set, so
`.github/workflows/docs.yml` publishes it as static HTML on GitHub Pages.

The builder is `scripts/build_docs_site.py`, standard library only — no mdBook, no Sphinx, no Hugo,
no binary fetched off a release page — for the same reason the rest of the tooling is: a build nobody
can audit is a build nobody can run offline, and this repository has refused a C dependency twice
(ADR-0006 §5, ADR-0007 §6) while refusing to add an unaudited one never came up. It runs in under a
second, needs no network, and does four things worth naming:

| it | because |
|---|---|
| keeps the tree's shape — `docs/adr/0002-evidence-jsonl-contract.md` becomes `adr/0002-evidence-jsonl-contract.html` — and writes every link relative | the same tree is then correct at a project page, a custom domain, a path under either, and `file://` from an unzipped job artefact. There is no `base_url` to configure, so there is no `base_url` to get wrong |
| generates GitHub-compatible heading anchors and checks section links against them | the links already written into these documents (`#3-coverage-`, `#5-action-success--the-durable-question`) are GitHub's slugs, odd trailing hyphen and all. A link to a section that is not a heading is a broken reference in a normative document, which is why it is an error in the `docs-site` step rather than a dead click on the published page |
| rewrites links that leave `docs/` through `--github-url` | `../README.md` and `../../schema/evidence.schema.json` are real paths in the repository and dead paths on a site that publishes only `docs/`. In CI that URL is pinned to the commit being built, so a published link cannot drift to a newer file than the document that cited it. With no `--github-url` they are warnings instead, because a laptop build has nowhere to send them |
| produces the same bytes twice | sorted paths, no timestamps, no run ids. "Did this change alter the site?" is a `diff`, and `--check` asserts exactly that by building a fixture tree twice and comparing |

Run it locally before pushing:

```sh
scripts/build_docs_site.py --check                    # the builder's own tests
scripts/build_docs_site.py --out /tmp/site            # the real build; also what ci.sh docs-site does
python3 -m http.server -d /tmp/site 8000              # or open /tmp/site/index.html directly
```

The workflow is two jobs, for the same reason the evidence producers live apart from the jobs that ask
for them: `build` is a fact and `deploy` is an action. `build` runs the self-test, builds `docs/`
with `--strict` and `--github-url`, uploads a downloadable artefact (that is the preview a fork PR
gets, and the only one that does not touch anyone's Pages settings) and, on a push, uploads the Pages
artifact. `deploy` runs on the default branch only — named through the API rather than hardcoded, so
renaming the branch cannot silently stop publication — and publishes through the `github-pages`
environment.

Two things to know before the first run. Pages has to be switched on once, in Settings → Pages, with
"Source: GitHub Actions"; until then `deploy` says `Pages not enabled on this repository`, which is a
configuration answer and not a build failure. And the workflow declares `pages: write` and
`id-token: write` at the top: a fork's pull request gets a downgraded token regardless, and its job
builds and uploads an ordinary artefact, so there is nothing for the downgrade to break.

## 4. Toolchain policy

* The root manifest's `[workspace.package] rust-version = "1.88"` is the **floor**: rmcp 3.9's own
  floor, and the oldest compiler allowed to build any crate in this workspace. Members inherit it
  (`rust-version.workspace = true`); do not lower it, and do not give a member its own —
  `scripts/validate_crate_layout.py` C3 is about that, from both directions.
* `rust-toolchain.toml` is the **pin**: the one toolchain CI and every local `cargo`
  invocation use, so "green on my box" and "green on GitHub" are the same claim. It is
  1.99.1 rather than the 1.88 floor because the committed `Cargo.lock` cannot be built by
  1.88.0 — `uuid 1.29.2` in the graph requires rustc 1.89 — so pinning the floor would pin
  a compiler that fails on this lockfile.
* Bumping the pin is a code change: a newer clippy is a new set of `-D warnings` opinions,
  so run `scripts/ci.sh` and read what it says before and after.
* **No C toolchain, and the pin is why.** `profile = "minimal"` means rustup installs rustc, cargo,
  clippy and rustfmt and nothing else — no linker front for C, no `musl-gcc`. That is compatible with
  this workspace only because every ADR that was offered a C dependency refused it (libgit2 in
  ADR-0003, bundled sqlite in ADR-0006 §5, tree-sitter in ADR-0007 §6, and `fathom`'s own manifest
  refusing `git2`), and mact-by1.16 checked it three ways rather than trusting it: `cargo tree -e build`
  names no third-party build-dependency for any shipped triple, `deny.toml` bans the known carriers,
  and the `check` matrix builds `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl`, where a
  crate that invoked `cc` would fail on the runner (the images carry gcc, not `musl-gcc`) long before
  it failed on someone's laptop. `scripts/check_dependency_firewalls.sh --check no-C` is the version
  of the first check that runs in CI. A future ticket that wants a C dependency is therefore not
  choosing a library, it is choosing a build-environment change on five targets — which is what
  ADR-0006 and ADR-0007 wrote down, and it stays true until someone rewrites those ADRs.

## 5. Supply-chain policy

`deny.toml` is the policy, `scripts/ci.sh supply-chain` is the enforcement, and
`scripts/install_ci_tools.sh` is how the checker is obtained (pinned version, sha256 pinned
in the script — a version bump is a reviewable two-line diff). One file governs all three crates,
because one lockfile is one supply chain (ADR-0005 §10), which has meant re-reading it against
fathom's tree: `ratatui`, `crossterm`, `rustix`, `toml`, `sha2`, `yaml-rust2` and everything they
bring are checked by the same four rules as `axum` is.

* **licences**: permissive set, exactly the crates the graph needs today; a new licence is a
  one-line, reviewed decision rather than a shrug. What fathom needed added: nothing — the whole TUI
  stack is MIT or MIT/Apache-2.0, and `rustix`'s "Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT"
  is satisfied by the plain Apache-2.0/MIT already allowed. `deny.toml` says this where a reader would
  look for it, including which spelling is deliberately *not* in the list.
* **bans**: duplicate versions warn (and `ci.sh` prints the `cargo tree -d` note next to
  them — two majors of `getrandom` is normal, two majors of a big crate for no reason is
  worth a commit message); wildcard requirements are denied — with `allow-wildcard-paths`, which is
  what lets the two siblings' `act-evidence = { workspace = true }` path dependency (a path with no
  version, because ADR-0005 §8 keeps one version for one repo) through while a third-party `"*"` still
  fails; `serde_yaml` is banned outright because Cargo.toml already decided against it
  (RUSTSEC-2024-0320); and the crates this repo has *refused* are banned by name with the ADR that
  refused them — `rusqlite`/`libsqlite3-sys` (ADR-0006 §5), `tree-sitter` and its grammars
  (ADR-0007 §6), `git2`/`libgit2-sys` (ADR-0003, written down here as well as in
  `validate_crate_layout.py`'s forbidden list so a transitive edge cannot smuggle one in).
* **sources**: crates.io only. A `git` dependency needs a named entry in `deny.toml`.
* **advisories**: the RUSTSEC database, dev-dependencies included, yanked = deny. This is
  the one check that needs the network. It carries exactly one ignore today, `RUSTSEC-2026-0543`
  (`chrono`, `unmaintained`, no fixed version exists), and the entry in `deny.toml` says what it does
  and does not excuse: an `unmaintained` advisory is not a vulnerability, a future chrono
  *vulnerability* is a different id and still fatal, the surface used is four functions deep, and
  removing it is a migration of ADR-0002's envelope time type — a ticket of its own, not a waiver's.
  `chrono` reached `fathom`'s tree with this epic, because `act-evidence` is the crate both binaries
  share.
* **graph targets**: the triples `cd.yml` ships — both linux GNU targets *and* `aarch64-apple-darwin`,
  which was missing until mact-by1.16 and matters now that the graph is platform-specific (`rustix`
  on linux, `core-foundation-sys` on macOS, via chrono's `iana-time-zone`). A target that is not named
  is a target whose dependencies are never licensed, never ban-checked and never scanned.
* **the two things deny.toml cannot say** live in `scripts/check_dependency_firewalls.sh`: ADR-0005's
  forbidden edges checked on `cargo tree` rather than on manifests, and the absence of a C-toolchain
  driver anywhere in the resolved graph of a shipped target.

## 6. Proving the checks can fail

`scripts/ci_drill.sh` copies the tree to a temporary directory, injects one violation per
check, and asserts that the matching `scripts/ci.sh` step exits non-zero:

| drill | violation injected | step that must fail |
|---|---|---|
| `fmt-misformatted-code` | valid Rust that rustfmt rejects | `fmt` |
| `clippy-lint` | `return x;` at the end of a function (`clippy::needless_return`) | `clippy` |
| `doc-broken-intra-doc-link` | a public doc comment linking to a non-existent item | `doc` |
| `doc-link-to-test-only-module` | a public doc comment linking to a `#[cfg(test)]` module — it compiles and tests pass, only rustdoc (which never builds `cfg(test)`) can see the link is dead | `doc` |
| `docs-site-broken-cross-page-link` | a section link at the end of `docs/ci.md` pointing at a heading that was never written, in a file that does exist — the shape of rot no checker but the anchor comparison can see | `docs-site` |
| `test-failing-assertion` | a unit test that asserts false | `test` |
| `tui-snapshot-snapshot-mismatch` | one character changed inside a drawn timeline golden, so the screen still says what the code draws and the snapshot says what it does not | `tui-snapshot` |
| `fathom-report-report-golden-drift` | `counts.with_evidence` nudged by one in `crates/fathom/tests/golden/report.json` — still valid JSON, still an integer, now describing something the tool did not compute | `fathom-report` |
| `fathom-report-flat-header-drift` | the `churn` column renamed to `churned` in `report::COLUMNS`: the Rust tests pin the column *count* and the last two names, so only the smoke's whole-header assertion sees it — which is the smoke's reason to exist | `fathom-report` |
| `fathom-doctor-doctor-exit-code-lie` | `Report::exit` made to return `Success` whatever the checks say: same statuses, same fixes, same JSON, and a CI step written `fathom doctor \|\| exit 1` goes green on a broken workspace | `fathom-doctor` |
| `schema-broken-schema-file` | `schema/evidence.schema.json` edited into a document the engine refuses to compile | `schema` |
| `schema-bad-evidence-record` | a record missing the required envelope fields appended to a golden | `schema` |
| `schema-undocumented-tool-io-fixture` | the `crates/act-mcp/tests/e2e/testdata/README.md` row naming one tool-io fixture, deleted | `schema` |
| `supply-chain-disallowed-licence` | `MIT` removed from `deny.toml`'s allow list | `supply-chain` |
| `supply-chain-fathom-dependency-banned` | `ratatui` banned in `deny.toml`: nothing in the server's tree reaches it, so this fires only if cargo-deny is actually looking at fathom's graph | `supply-chain` |
| `supply-chain-fathom-firewall-crossed` | `schemars = { workspace = true }` added to `crates/fathom/Cargo.toml` — the transitive-edge shape ADR-0005 §11 says to check, invisible to a manifest checker | `supply-chain` |

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

That hazard is the drill's own problem too, and `copy_repo` handles it in two ways worth knowing
before adding a drill. A copy is not a git work tree and has no `.fathom/`, so (a) the one file of
`.fathom` that `include_str!` needs (`LAYOUT.md`) is copied, or the fathom crate cannot compile at all
and every fathom drill would "catch" a violation by failing to build; and (b) the copy's member
manifests are `touch`ed, because `cp -a` preserves mtimes and cargo would otherwise hand the copy an
artefact built from another tree — one whose `env!("CARGO_MANIFEST_DIR")` points at a directory that
has already been deleted, or (quieter and worse) at the real repository. A drill whose tests never ran
against the copy proves nothing.

It is not a step in the pipeline, and that is a cost decision rather than an oversight: fifteen
drills compile the crate fifteen times, which would double the check job to guard a file
(`ci.sh`) that changes rarely. Running it is part of *changing* `ci.sh`, `ci_drill.sh`, or a
checker they call — the same rule as re-running a test when you edit the test, not a rule the
pipeline can enforce for you. (`docs-site` is the sixteenth drill and the one that pays nothing: it
injects a broken link into `docs/ci.md`, and the step it proves has no toolchain in it.)

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
  `crates/act-mcp/tests/e2e` is what proves the pipeline without them (docs/testing.md §1). Anything that
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
