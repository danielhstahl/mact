# Security model — what "local-only, no auth" actually requires

Status: **normative**. This is the mact-f1w.13 audit: the list of everything that reaches
this server from outside, the control that closes each one, and the test that proves the
control is live. When you change a boundary below, change this file and the test in the same
commit — a security property with no test is a property that regresses.

Related: [ADR-0001](adr/0001-tool-contract.md) (error contract, secrets),
[ADR-0002](adr/0002-evidence-jsonl-contract.md) (paths, `meta.json`),
[ADR-0003](adr/0003-commit-isolation-and-serialization.md) (workspace isolation, act argv,
the `.actrc` facts F2/F3), [ADR-0004](adr/0004-run-lifecycle.md) (budgets, caps).

---

## 0. What this thing is

`act-mcp` is an HTTP MCP server with **no authentication** that runs arbitrary GitHub Actions
workflows — inside containers, with the repository bind-mounted — on the machine it runs on.
The whole design leans on one property: **the socket is reachable only from this machine**.
The second property, needed because "only this machine" includes browsers and other users'
processes, is that **every string that arrives is hostile**: it may name a workflow, a job, a
commit, a category, a path, or a secret, and none of it is trusted to be any of those things
it claims to be.

Attack surface, in the order this document takes it:

| # | Surface | Where it lives |
|---|---|---|
| 1 | the socket itself (what can reach us) | `config.rs`, `server::serve` |
| 2 | the HTTP request headers (`Host`, `Origin`) | `server::http_config` (rmcp enforces) |
| 3 | the request shape (size, content type, protocol) | rmcp, configured by `http_config` |
| 4 | tool arguments (`workflow`, `task`, `commit`, `category`, `var`/`input`/`env`) | `workspace::validator`, `workflows`, `tools::category` |
| 5 | the repository's own files (workflow YAML, `.env`/`.vars`/`.input`, `.actrc`) | `workflow_yaml`, `runner::argv`, ADR-0003 §7 |
| 6 | what a run writes back (logs, evidence, `meta.json`) | `runner::log`, `evidence::writer` |
| 7 | volume (how much of anything one run or one client can make) | `registry::RunPolicy`, `runner`, `evidence::*` |

**Out of scope, deliberately:** multi-user tenancy, network exposure, and anything a
container escape gives an attacker. If you can run a workflow here you already own the
machine; nothing below pretends otherwise.

---

## 1. The socket

* Default bind is `127.0.0.1:8080` (`config::DEFAULT_BIND`).
* A `--bind` that resolves to a non-loopback address is **refused at boot** unless the
  operator passes

  ```
  --i-understand-this-is-unauthenticated      (env: ACT_MCP_I_UNDERSTAND_THIS_IS_UNAUTHENTICATED)
  ```

  That spelling is `config::UNAUTH_ACK_FLAG`; the refusal message, the flag help, the env
  var, and the startup banner all contain it, so `grep -rn i-understand-this-is-unauthenticated`
  finds every place the decision is made. The point of a flag with that name is that using
  it is an act of typing, not a default.
* When it is used, `server::serve` prints a boxed `WARN` banner naming the address, the
  flag, and the consequence. Loopback boots get one `INFO` line instead — the quiet path is
  the safe path, and the loud path has to be un-missable.
* **The check is made twice.** `Config::build` rejects the argument;
  `server::guard_resolved_bind` re-checks `listener.local_addr()` — the address the socket
  actually got — and refuses with `ConfigError::ResolvedBindNotLoopback` if it is routable
  and unacknowledged. The second check is what catches a config assembled by an embedder
  that never went through the CLI.

`is_loopback` (`config.rs`) covers `127.0.0.0/8`, `::1`, and IPv4-mapped `::ffff:127.0.0.1`.

### Deliberate exposure: a port-forward

The supported way to reach a local server from elsewhere is an SSH tunnel, and it needs no
flags at all, because the client's `Host` is still `localhost`:

```sh
ssh -N -L 8080:127.0.0.1:8080 devbox
# client dials http://localhost:8080/mcp → Host: localhost:8080 → allowed
```

Binding the server to a LAN address is possible but is a two-step decision, because the two
guards are independent and both need naming:

```sh
act-mcp serve --bind 192.168.1.20:8080 \
  --i-understand-this-is-unauthenticated \
  --allow-host 192.168.1.20:8080        # implied by the bind address, shown for clarity
```

An acknowledged non-loopback bind automatically adds its own bound IP to the `Host`
allowlist (`Config::resolve_allowed_hosts`); a **DNS name** never does, and must be named
with `--allow-host`. That asymmetry is on purpose: the address was acknowledged, the name
was not.

Tests: `tests/boot.rs::boot_fails_when_the_bind_is_not_loopback`,
`tests/hardening.rs::{a_non_loopback_bind_without_the_acknowledgement_refuses_to_boot,
a_loopback_bind_boots_quietly, the_acknowledgement_flag_boots_and_shouts_about_it,
the_acknowledgement_can_come_from_the_environment}`.

---

## 2. `Host` and `Origin`: DNS rebinding and cross-site requests

"Loopback-only" does not mean "only this machine's *programs*". A page loaded in a browser on
this machine can make requests to `127.0.0.1` (and, with a DNS name that resolves to
`127.0.0.1` — DNS rebinding — can do it with `Host:` and `Origin:` set by the attacker's
site). Two allowlists, both enforced by rmcp inside the endpoint service, close that:

**`Host`** — `Config::allowed_hosts`, default `["localhost", "127.0.0.1", "::1"]`
(`config::DEFAULT_ALLOWED_HOSTS`), extended by `--allow-host` (repeatable, or
`ACT_MCP_ALLOWED_HOSTS=a,b`). A request whose `Host` is not on the list gets **403** before
any handler runs. The list is **never allowed to be empty**: in rmcp an empty allowlist
means "accept any `Host`", which is the hole, so `Config` always assembles at least the
loopback three.

**`Origin`** — enforcement is **always on** (`Config::origin_validation_enforced()` is a
`const true`; there is no flag to turn it off) and the default allowlist is **empty**, which
— the part that is easy to get backwards — means *every request carrying an `Origin` is
refused with 403*, not "any origin is fine". A request with **no** `Origin` header (which is
what a real MCP client sends) is unaffected. Reason: an MCP client is not a browser, so an
`Origin` on this endpoint means either a browser (the cross-site-request case) or someone
probing. A local browser-based client that genuinely needs to reach us names itself:

```sh
act-mcp serve --allow-origin 'http://localhost:3000'      # or ://…:* for any port
```

`--allow-origin` values must carry a scheme; a schemeless entry is a boot error
(`ConfigError::OriginInvalid`) rather than an allowlist entry that silently never matches.

Tests: `tests/hardening.rs::{a_foreign_host_header_is_refused,
a_loopback_host_header_is_accepted, a_browser_origin_is_refused_by_default,
a_named_origin_is_accepted_when_the_operator_allows_it, an_extra_allowed_host_is_honoured}`.

---

## 3. Request shape

| control | value | why |
|---|---|---|
| `max_request_body_bytes` | default **1 MiB** (`--max-request-body-bytes`, bounded `1 KiB..=64 MiB`) | a `run_workflow` call is a workflow name, a job id, and a handful of `--var`/`--input` pairs. Oversized POST → **413**, enforced while the body streams, so a chunked body or a lying `Content-Length` does not size our buffer |
| `json_response` | `true` | stateless request/response tools answer `application/json`; a plain client works, and SSE is used only when a handler actually notifies mid-call |
| `legacy_session_mode` | `false` | the pinned revision (`2026-07-28`) has no sessions; a server that could also speak the old handshake is a second behaviour to test for no benefit |
| `sse_keep_alive` | **15 s**, explicit (`server::SSE_KEEP_ALIVE`) | not inherited by accident. On loopback the point of a keep-alive is not beating a proxy timeout — it is noticing a half-closed client at all, so the stream, its session, and its buffers go away instead of lingering until the run's own deadline |
| protocol versions | exactly one (`SUPPORTED_PROTOCOL_VERSION`) | negotiation, discovery, and session rules are rmcp's; this server narrows the set and hand-rolls nothing |

`stateless_protocol_metadata_required` is left **off**: the pinned revision already demands
its per-request metadata in the handler, and the stricter header rule would only change
which error a malformed request sees — see `tests/protocol.rs` for the refusals that are
already exercised.

Tests: `tests/hardening.rs::an_oversized_request_body_is_413` (both framed and chunked),
`tests/protocol.rs` for the protocol-shape refusals.

---

## 4. The opt-in shared secret — the decision

**Decision: implemented, off by default.** `--require-session-token` generates 128 bits from
`/dev/urandom`, logs it once at boot on stderr, and requires it in the `x-act-mcp-token`
header (`config::SESSION_TOKEN_HEADER`) on every request; `--session-token <value>` supplies
your own instead. A request without it, or with the wrong value, gets **401** from
`server::require_session_token` (constant-time comparison; the presented value is never
logged, so the log is not a probe oracle). `Config`'s debug output prints
`SessionToken(<redacted len=32>)`, because `Config` *is* debug-logged at boot.

Why opt-in and not on-by-default:

1. It would break the contract the epic already ships. Every existing client — `tests/protocol.rs`,
   the e2e harness, the stock rmcp client — dials loopback with no credentials, and the
   ticket's own framing is "cheap defence against another local process hammering the
   endpoint", not "access control".
2. It adds no *boundary*. The secret is delivered over the same local channel as everything
   else; a process that can read your config or your boot log has a shell on the machine and
   can read the secret file too. What it does buy is exactly what the ticket asks: a
   *stranger process* on the box can no longer `curl 127.0.0.1:8080` and get act runs.
3. Cost when on is ~zero; cost when unneeded is a confusing 401 nobody asked for.

So the flag exists, is tested, and is documented — and the default remains "loopback is the
control", which is what the rest of this file assumes.

Tests: `tests/hardening.rs::{the_opt_in_shared_secret_gates_every_request,
no_shared_secret_is_required_by_default, the_session_token_does_not_print_itself}`.

---

## 5. Every untrusted input, and what closes it

The rule for all of them: **shape first, then resolve, then never interpolate.** A shape check
runs on bytes; a resolve runs on the filesystem; interpolation is what turns a string into a
command, a path, or a shell.

| Input | Who supplies it | The control | Where | Test |
|---|---|---|---|---|
| `workflow` | `run_workflow`, `list_tasks` | **Shape**: 1..=255 bytes, `".."` anywhere is rejected, so traversal is inexpressible **before any path is joined**. **Resolution**: the value is matched against the *index* of `.github/workflows` — never opened directly. The index canonicalises the directory and keeps only files that canonicalise **under** it, so a symlink out of `.github/workflows` is not a workflow of this repository. Reads are size-bounded (`workflow_yaml::MAX_WORKFLOW_BYTES`, 448 KiB) | `workspace::validator` (`workflow_shape_ok`), `workflows::index_directory`, `workflows::resolve` | `tests/hardening.rs::a_traversal_in_the_workflow_argument_is_refused_before_any_path_is_joined`, `a_workflow_symlink_pointing_out_of_the_workflows_dir_is_not_indexed`, `src/workspace/tests.rs::workflow_names_that_cannot_be_a_workflow_are_rejected_by_shape` |
| `task` (job id) | caller | **Whitelisted, not validated**: it must be a key of the parsed workflow file's `jobs:` map (`task_not_found` otherwise), so the value the server uses is one it read out of the file, not one it was told. It reaches act only as the argv token after `-j` — there is no shell, no `sh -c`, anywhere in the runner | `workspace::validator`, `runner::argv` | `src/workspace/tests.rs::an_unknown_task_lists_the_job_ids_that_exist`, `src/runner/argv.rs` golden argv |
| `commit` | caller (abbrev or full) | **Hex-shape check before any `git` argv**: 7..=40 chars, all lowercase hex, no whitespace, `..`, `~`, or leading `-`. Only then is it passed to `git rev-parse`/`git rev-parse <x>^{commit}`, again as an argv element, never a shell string. Resolution happens against the *accepted* workspace root (canonicalised, symlink-free), and a root that resolves through a symlink out of the accepted root is refused | `workspace::validator` (`is_commit_shaped`, `TreeRoot` gate) | `src/workspace/tests.rs::{an_untrusted_commit_shape_is_refused…}`, `the_head_only_policy_refuses_a_non_head_commit_after_resolution` |
| `category` | caller | **Two independent rules.** (1) Shape: `^[a-z][a-z0-9_]*$`, ≤64 bytes, enforced **in Rust** by `tools::category::is_valid_category`, not only advertised in `tools/list` — the schema pattern and the Rust check name the same `SCHEMA_PATTERN` constant and a test fails if they drift. That pattern contains no `.` and no `/`, so a legal category cannot be a path component. (2) **Even a legal category does not build a path**: the output file is named from `evidence::records::Kind`, the *closed* `benchmark \| coverage` enum, reached via `category::kind_of`; an unknown-but-legal category runs the job and publishes nothing. The writer then re-checks every component (`EvidenceRoot::checked_child`) and refuses `..`, empty, and any existing component that is a symlink | `tools::category`, `evidence::writer` | `tests/hardening.rs::a_category_cannot_become_a_path`, `src/tools/category.rs::{a_category_cannot_be_a_path, the_rust_check_mirrors_the_advertised_schema}`, `src/workspace/tests.rs::job_ids_and_categories_are_shape_checked_too` |
| evidence output dir | derived (`<kind>/<sha>/<run_id>.jsonl`) | The path is built from validated components only (a `Kind`, a 40-hex `Commit`, a run id that matches the pinned shape). `checked_child` additionally canonicalises the deepest existing component and refuses when it differs from the literal path — that is what catches a `coverage/<sha>` turned into a symlink to `/home/dev`, which no input validation could catch because the input was a legitimate SHA. Publish is temp-file + rename inside the root, and the root itself was canonicalised at boot | `evidence::writer` | `src/evidence/writer.rs::a_symlinked_commit_directory_is_refused`, `tests/evidence_contract.rs` |
| `--var` / `--input` / `--env` values | caller | Keys are shape-checked (single line, no `=`/`:`, no `ACTMCP_*` shadowing) and every value is passed as an **argv element** to act, never interpolated into a shell. `ACTMCP_*` — the server's own evidence-transport contract — is refused from any caller map. Values are **excluded from the process log** (argv is not logged) and are redacted in the recorded argv that becomes `meta.json.act.argv` when the *name* says secret (§6). Keys and values both run through the redactor's name rule, so a `--var NPM_TOKEN=…` does not survive into any file | `runner::RunSpec::validate`, `runner::argv`, `RunSpec::redaction_needles` | `src/runner.rs::{a_spec_that_cannot_be_spawned_safely_is_refused_before_anything_happens, the_redaction_needles_are_the_declared_secrets_plus_everything_named_like_one}`, `tests/hardening.rs::a_secret_reaches_no_file_a_run_produces_whichever_route_it_arrived_by` |
| the repository's `.actrc` | the repository | **Not honoured, and stated: act's cwd is the per-run scratch directory we create (`ADR-0003 §7.6`), never the workspace, and act looks up `.actrc` from the *process cwd* — so a `.actrc` committed to the repository is never read.** The operator's own `$XDG_CONFIG_HOME/act/actrc` and `$HOME/.actrc` **are** honoured, because they are operator config and are the sanctioned extension point. One caveat is load-bearing and belongs to every operator who uses it (ADR-0003 F2): act's `StringArray` flags — `--env`, `--var`, `--input`, `--platform`, `--matrix` — **accumulate across sources**, so an operator actrc adds to what we pass rather than being overridden by it. Flags we merely assume are flags an operator actrc can quietly flip, which is why §7.5 of ADR-0003 lists the flags we always pass | `runner::run` (`current_dir(&spec.scratch_dir)`) | `tests/runner_execution.rs::the_stub_act_receives_exactly_the_argv_build_argv_produced` (asserts act's cwd **is** the scratch dir), `src/runner/argv.rs::adr_0003_7_4_golden_command_line_is_the_default_argv` |
| the repository's `.env` / `.vars` / `.input` | the repository | **Redirected, not ignored-by-luck**: act's `--var-file`, `--env-file` and `--input-file` all point at a 0-byte file we own (`ProjectConfigFiles::Inert`, the default), so a `.env` shipped by the target repository cannot inject host state into a measurement. `--act-workspace-config-files=discover` opts back into act's own lookup, for a repository that legitimately depends on it | `runner::argv`, `ProjectConfigFiles` | `src/runner/argv.rs::discover_mode_passes_no_inert_project_files_but_keeps_the_secret_file` |
| workflow YAML itself | the repository | Parsed by `yaml-rust2` (not the unmaintained `serde_yaml`, RUSTSEC-2024-0320) with a size cap, a flow-depth cap and an indent cap; a file that will not parse is reported as `parse_error` inside a successful `list_tasks` rather than failing the call, and never reaches a shell | `workflow_yaml` | `src/workflow_yaml.rs`, `tests/list_tasks.rs` |
| the workspace path at boot | operator | Must exist, be a directory, contain `.github`, and is canonicalised once so every later prefix comparison is against one symlink-free root | `config::validate_workspace` | `src/config.rs`, `tests/boot.rs` |

Two things on this list are worth dwelling on because they are the ones a reviewer skims:

* **The `category` row is two rows wearing a coat.** A shape check on an open vocabulary is
  not enough, because the vocabulary is open: `fuzz` is legal, so the thing that actually
  protects the filesystem is that legality has nowhere to write. `Kind` is closed; that is
  the control. The shape check's job is to make `../evil` an argument error instead of an
  unknown category that publishes nothing (silent) or, worse, a directory.
* **The `.actrc` row is a cwd trick, not a filter.** There is no "project `.actrc`
  allowlist" here (the upstream one, `--trust-project-actrc`, is unreleased as of ADR-0003
  F3); the file is simply never looked up, because act's cwd is not the repository. If you
  ever change `current_dir` to the tree, you have re-opened a hole this row closes.

---

## 6. Secrets

The rule from ADR-0001 §2.7, unchanged: **redaction happens at the source**, before a line
reaches either the file or the ring — not by filtering on the way out, because the way out
has more than one door.

What gets redacted (`RunSpec::redaction_needles`, `runner::log::Redactor`):

1. **Declared secrets** (`secrets` map → `secrets.env`): every value, no length floor. They
   are never argv; they travel in a `0600` file outside the workspace, passed as
   `--secret-file`, and unlinked as soon as act's exit status is known.
2. **Caller-supplied values whose *name* says secret** — `--var`, `--input`, `--matrix`, the
   job's `--env`. The name rule is `runner::log::name_looks_secret`: uppercase substring
   match against `SECRET_NAME_MARKERS` (`SECRET`, `TOKEN`, `PASSWORD`, `PASSWD`, `PASS`,
   `CREDENTIAL(S)`, `AUTH`, `APIKEY`, `PRIVATE(_KEY)`, `ACCESS_KEY`). A name rule rather than
   a denylist of variable names, because the pattern that leaks a credential is the one named
   `SOMETHING_TOKEN_WEIRD`.
3. **The server's own environment, name-matched.** act is spawned with act-mcp's environment
   (ADR-0003 §7.6), so an operator-exported `GITHUB_TOKEN` would otherwise be pasted into
   `act.jsonl` by any job that runs `env`. This is also why `ACT_MCP_SESSION_TOKEN` cannot
   leak: the name matches.

Values 2 and 3 have a **length floor** (`MIN_HEURISTIC_NEEDLE_BYTES = 8`): a needle of one
byte would redact every `1` in the log, and a redactor that eats the log is worse than one
that misses a one-character value. Declared secrets have no floor — the caller said so.

Where the redaction is applied:

* `act.jsonl` (the durable log) — secret values only, real host paths kept: it is the
  operator's audit file;
* `log_tail` (the wire field) — secrets **plus** `<WORKSPACE>` / `<EVIDENCE>` substitution,
  because ADR-0001 §3.4 says a wire field does not get to contain a host absolute path;
* **the recorded argv**, i.e. `meta.json.act.argv` (mact-f1w.13). act's argv carries every
  `--var`/`--input`/`--env` value, so the copy stored for the durable manifest is the
  redacted one: the audit trail still says what was run, without storing the credential we
  were paid to keep out. An ordinary `--var BUILD_MODE=release` is still recorded verbatim.

Redaction is byte-level, not `str`-level: container output need not be valid UTF-8, and a
redactor that failed on invalid UTF-8 would leak the secret it was standing on. Needles are
applied longest-first so overlapping secrets leave no remnant.

Tests: `tests/runner_execution.rs::a_secret_value_appears_in_no_file_the_run_produces`,
`tests/hardening.rs::a_secret_reaches_no_file_a_run_produces_whichever_route_it_arrived_by`
(walks every file under the evidence root **and** the scratch dir, asserts the argv record is
masked, and asserts an ordinary var value *survives* — a redactor that over-redacts is also a
bug), `src/runner/log.rs::{redactor_survives_invalid_utf8_around_a_secret, longest_needle_wins_so_overlapping_secrets_leave_no_remnant, the_name_rule_skips_values_too_short_to_redact}`,
`src/runner.rs::the_redaction_needles_are_the_declared_secrets_plus_everything_named_like_one`.

---

## 7. Volume: what one run, or one client, can make

Two failure styles, deliberately: **truncate-with-marker** where the run is worth finishing
and the overflow is data, and **typed error** where continuing would degrade the machine or
lie about the result.

| Bound | Default | Floor/cap | On overflow | Where |
|---|---|---|---|---|
| concurrent act runs, server-wide | 2 | ≥1 | `StartError::TooManyRuns` → `too_many_runs`, retryable | `registry::start` |
| runs per workspace | **1** (single-flight) | — | `workspace_busy`, naming the holder, answered in microseconds; there is no queue | `registry::lock` (L1 in-process, L2 `flock`) |
| `.evidence/runs/` directories | **1000** (`--max-run-dirs`) | ≥1 (0 is a boot error) | `StartError::TooManyRunDirs` → `too_many_runs`, retryable, and the message says prune or raise the flag | `registry::start`, `config::max_run_dirs` |
| on-disk log per run | 64 MiB (`--max-run-log-bytes`) | ≥1 MiB | truncate **with a marker line** (`act_mcp_truncated`) + `truncated` warning — the run still finishes | `runner::run` |
| in-memory tail ring | 64 KiB (`--log-tail-ring-bytes`) | >0 | oldest bytes evicted; `log_tail` is whole lines from the end | `runner::log::LogRing` |
| one log line | 1 MiB | — | split: a job `cat`ting a 200 MiB file does not become our buffer | `MAX_LOG_LINE_BYTES` |
| benchmark metrics per run | 5 000 | — | records truncated, every record of the run flagged | `evidence::benchmark` |
| coverage sources per run | 5 000 | — | idem | `evidence::coverage` |
| one coverage / benchmark payload | 8 MiB each | — | `payload_too_large` warning, file skipped | `evidence::{coverage,benchmark}` |
| metrics scanned out of the log | 32 MiB | — | stop, warn | `MAX_*_LOG_BYTES`, `MAX_SCRAPE_BYTES` |
| evidence files in one `run_view` | 8 | — | capped list | `state::MAX_EVIDENCE_FILES` |
| warnings per run | 64 | — | dropped with a note in the log | `state::MAX_WARNINGS` |
| runs kept in memory | 200 / 120 min | ≥ listing cap (50) | eviction, then `run_not_found` | `registry` retention |
| `run_status` page / `known_runs` | 50 / 20 | — | truncation flag set | `LISTING_CAP`, `KNOWN_RUNS_CAP` |
| workflow files listed / jobs / steps | 1024 / 1024 / 256 | — | capped, `truncated` flag | `tools::list_tasks` |
| error message / detail on the wire | 1 KiB / 4 KiB | — | truncated | `workspace::MAX_ERROR_*_BYTES` |
| a `git` call | 30 s (`--git-timeout-secs`) | 1..=300 s | killed, typed error | `workspace::validator` |
| a run | `timeout_secs`, ceiling 6 h (`--max-timeout-secs`) | ≤ 24 h compiled | process-group kill, `timed_out` | ADR-0004 §4 |

The single-flight lock is the most important row: it is what makes "one unauthenticated
process can spawn N containers" false. The server-wide cap is above 1 only so two
*different* workspaces can be measured at once; within a workspace the answer is always one,
and a second request is refused rather than queued (`phase: "queued"` means pre-spawn, never
"waiting its turn").

Tests: `tests/hardening.rs::{a_run_is_refused_when_the_runs_directory_is_at_its_cap,
a_zero_run_dir_cap_is_refused_at_boot, the_log_caps_truncate_instead_of_growing,
the_metric_caps_exist_and_are_finite}`, `tests/run_supervisor.rs::{a_second_run_in_a_locked_workspace_is_busy_immediately, a_run_in_another_workspace_is_not_blocked_by_the_first, the_server_wide_cap_is_too_many_runs_rather_than_a_wait}`, `tests/runner_execution.rs::{the_on_disk_cap_truncates_with_a_marker_and_the_tail_stays_in_budget, the_tail_budget_holds_whole_lines_from_the_end}`.

---

## 8. Residual risks, stated plainly

* **No authentication.** Any process on this machine that can open the socket can run
  workflows, and act runs containers with the repository bind-mounted. `--require-session-token`
  (§4) raises the bar for a *stranger* process; it does not create access control, and there
  is none.
* **A run reads its own workspace.** `.evidence` lives inside the workspace (ADR-0002 §1.1)
  and `--bind` mounts the tree, so a job can read earlier runs' logs and evidence. The
  secret file is deliberately *outside* the workspace for exactly this reason (ADR-0003 F8)
  and is unlinked at exit; nothing else here is secret from the job.
* **A container escape is game over.** act's `--bind`, `--network` defaults, and image
  pulls are act's threat model, not ours. `--pull=false` by default (ADR-0003 §7.3) and the
  act version floor exist to keep that model recognizable, not to strengthen it.
* **The operator actrc is powerful and accumulates** (§5). It is the supported extension
  point for the flags we do not pass, and its array flags add to ours.
* **`--allow-host` widens the rebinding guard.** Name the front-end, not a wildcard: an entry
  without a port matches any port.
* **The generated session token is printed once, on stderr.** That is the only way a client
  can learn it; if you ship stderr to a log aggregator, treat that line as a secret and use
  `--session-token` from your own secret store instead.
* **A run that is killed by `SIGKILL` can leave containers** — reported as the
  `orphaned_containers` warning (ADR-0004 §5.5), because the cleanup that would have removed
  them was act's.

---

## 9. Reviewing a change to this file's territory

1. New caller-supplied string? Shape-check it, add the refusal to the table in §5, and add
   the test that sends the traversal/hex-breaking/newline version of it.
2. New file a run writes? Add it to the redaction scan in
   `tests/hardening.rs::a_secret_reaches_no_file_a_run_produces_whichever_route_it_arrived_by`.
3. New cap? Decide and state which failure style (§7) it uses; "silently smaller" is not one.
4. New bind/host/origin flag? It must not widen anything by default, and the widened case must
   be as loud as `--i-understand-this-is-unauthenticated`.
5. `cargo clippy --all-targets -- -D warnings` and the whole `tests/` suite, because every
   claim above is a test someone can run.
