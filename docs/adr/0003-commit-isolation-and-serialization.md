---
id: ADR-0003
title: commit isolation (HEAD gate + worktree, chosen by server policy) and workspace run serialization
status: accepted
ticket: mact-f1w.4
date: 2026-11-10
---

# ADR-0003: commit isolation and workspace serialization

- **Status:** Accepted
- **Date:** 2026-11-10
- **Ticket:** mact-f1w.4
- **Decision:** **Option (c), with the choice of mechanism moved from the caller to the server.** act runs
  the working tree it is pointed at, so act-mcp decides *where* to point it: if the requested commit is
  `HEAD` of the workspace, act runs the workspace in place behind a **gate** that proves the tree really is
  that commit (`head_only`); otherwise act runs a **temporary linked worktree** created for exactly this run
  (`worktree`). The operator picks the policy with `--isolation head-only|hybrid|worktree` (**default
  `hybrid`**); a caller cannot, because there is no such argument and adding one would let a caller buy
  honesty with somebody else's disk and clock. Serialization is a **two-layer lock**: an in-process registry
  slot plus an OS `flock(2)` on `<workspace>/.act-mcp.lock`, so it survives a second server instance and dies
  with its holder; scope is **one run per workspace**, fail-fast with `workspace_busy`, no queue.
- **Machine-readable form:** none new. Every code this ADR emits already exists in
  [`schema/tool-contract.schema.json`](../../schema/tool-contract.schema.json) (`isolation_failed`,
  `workspace_busy`, `commit_unknown`, `commit_ambiguous`, `not_a_git_repository`, `internal_error`,
  `runner_internal_error`), the two `detail.mode` values it uses (`head_only`, `worktree`) are already the
  ones `x-error-codes` documents, and the three warning codes it adds belong to the **open** `warnings`
  vocabulary (ADR-0001 §5.4). No argument, no status, no new error code, no schema change.
- **Checked by:** [`scripts/validate_isolation.py`](../../scripts/validate_isolation.py) — it rebuilds the
  §7.2 argv from the §7.3 table and diffs it against the literal command line in §7.4, rejects any flag
  outside the transcribed act flag set (§1.3), asserts the §4 gate table emits only real error codes and only
  the `detail.mode` values the schema allows, then **executes** the pinned git and lock protocol against
  throwaway repositories and two real processes (clean/dirty/sparse/bare/detached/abbreviated/unknown/
  ambiguous/non-commit/submodule cases; contention, death-release and never-unlinked for the lock;
  `worktree add`/`remove --force` for §5). Only act itself is stubbed; docker is not needed.
- **Related:** ADR-0001 (tool contract — accepted, and it binds this one: its "Binding on the other
  decisions" list is §1.1 below), ADR-0002 (evidence JSONL contract — accepted; §15.1 corrects one of its
  paths), ADR-0004 (run lifecycle — accepted; it hands this ADR the `preparing_tree` phase and the lock
  lifetime, §8 of it)
- **Consumers inside the epic:** mact-f1w.6 (the §3 resolution and the §4 gate are its step 5 and step 8),
  mact-f1w.8 (the argv builder of §7 is *the* argv it builds; §9 is its teardown), mact-f1w.11 (the lock of
  §6 and `workspace_busy`), mact-f1w.13 (the exposure notes in §8.4), mact-f1w.5 (the §11 flags),
  mact-f1w.15 (stub-act harness: the §7.4 command line is its golden argv), mact-f1w.18 (runbook: sweeps,
  operator actrc, orphaned containers), mact-f1w.19 (the three act-behaviour claims marked **V** in §1.3)

## 1. Context

### 1.1 What is already decided, and binds this document

Not re-litigated here; every one of these is restated where it is used:

- **The `commit` in a result is the commit that was actually executed** (ADR-0001 "Binding on the other
  decisions"). This is the whole reason this ADR exists: act has no commit argument, so an unverified
  `commit` field would be a claim about the caller's checkout, not about the run.
- **`isolation_failed` is the only code for "we cannot run this commit here", `workspace_busy` is the busy
  answer with the in-flight run's identity in `detail`; one run per workspace, no queue** (same list).
- **Validation order** (ADR-0001 §2.9): commit *resolution* is step 5, isolation of the target tree is step
  8, capacity and the single-flight lock are step 9, registration and `run_id` allocation are step 10. ADR-0004
  §18 sharpens it: the cheap "can we produce a tree for this commit" check stays **before** `run_id`
  allocation, and everything after registration is `runner_internal_error` with `detail.stage`.
- **The workspace is the server's own, fixed at startup**; a caller cannot name a path (ADR-0001 §2). So
  every decision here is about one workspace per server process.
- **act always runs with `--bind`** and the payload scratch directory is `<worktree>/.evidence/incoming/<run_id>`
  (ADR-0002 §3.1); the durable evidence root stays in the *primary* workspace even when act runs in a tree we
  synthesized (ADR-0002 §1.1).
- **The lock is held from registration to ADR-0004 §3.4 step 6** (after evidence publish and `meta.json`,
  before waiters are notified), a request task never holds or releases it, and the evidence layer adds no
  lock of its own (ADR-0004 §8, ADR-0002 §6.3).
- **act is always invoked with `--rm`** (ADR-0004 §5.5), act-mcp never removes containers behind act's back,
  and a `SIGKILL` teardown carries the `orphaned_containers` warning.
- `phase: preparing_tree` exists and means "ADR-0003 tree materialisation"; whether it is a visible step or
  a zero-duration hop is this ADR's call (§3.6 of ADR-0004). It is visible here in `worktree` mode and a hop
  in `head_only` mode.

### 1.2 Why the two obvious answers are each half right

A **gate** (refuse unless `git rev-parse HEAD == commit`) is three lines, costs nothing, leaves the operator's
checkout alone, and inside the container `git` works normally because `.git` is a real directory inside the
bind mount. Its cost: the caller must manage checkouts, two callers can never use two commits, and a *dirty*
tree passes a HEAD-only comparison — the commit field would then describe HEAD while the run measured
HEAD-plus-local-edits.

A **worktree** (`git worktree add --detach`) is honest for any commit that exists locally and never touches the
operator's checkout. Its costs are all the ways a synthesized tree is not the tree a runner would have:
a linked worktree's `.git` is a *file* pointing at `<common>/worktrees/<name>` **outside** the bind mount, so
`git` inside the job container cannot resolve the repository; submodules are not populated; LFS content is
not fetched and this server never fetches (ADR-0001 §2.5); and the checkout itself costs real time and disk
next to every run.

So: gate where a gate is provable, worktree where a gate is impossible, and never let the caller choose — the
choice is a statement about the *server's* disk, clock and trust posture.

### 1.3 Ground truth about act, established before this ticket

Verified by reading `nektos/act` at `main` (2026-11-10) and at the released tags named in each row; the flag
spellings and defaults this ADR emits are identical at `v0.2.62`, `v0.2.72`, `v0.2.85`, `v0.2.94` and
`v0.2.97` (the newest release). **V** marks the three claims that need confirming against a live docker+act run
(mact-f1w.19) because they cannot be settled from source alone.

| # | fact | where |
|---|---|---|
| F1 | act has **no** `--workdir`. The working directory is `-C/--directory <path>` (default `.`), and `-W/--workflows`, `--env-file`, `--var-file`, `--input-file`, `--secret-file` and `--eventpath` are all resolved *relative to it* when relative. The ticket's "`act --workdir <tmp>`" is `--directory <tmp>` plus absolute `-C`/`-W` values. | `cmd/input.go` `resolve()`/`Workdir()`, `cmd/root.go:102` |
| F2 | act reads config from `$XDG_CONFIG_HOME/act/actrc`, then `$HOME/.actrc`, then **`./.actrc` — the process *cwd*, not `-C`** — and finally the command line. File arguments are prepended, so a CLI flag wins over a config file for scalar flags, **but `StringArray` flags accumulate across sources**: `--env`, `--var`, `--input`, `--platform`, `--matrix`. | `cmd/root.go` `configLocations()`/`args()`, `cmd/actrc.go` `actrcArgs()` |
| F3 | On every **released** act, `./.actrc` is applied with full trust and `$VAR` expansion. The allowlist that restricts an untrusted project `.actrc` to non-host-reaching flags (`--trust-project-actrc`) exists only on `main`, unreleased. | `cmd/actrc.go`, absent from tags through `v0.2.97` |
| F4 | `--rm` **defaults to false** and means "remove containers/volumes even after a *failure*"; a container is stopped-and-removed when `AutoRemove || jobError == nil`. Without `--rm`, every failed job leaves a container behind — and failed jobs are the normal output of this tool. | `cmd/root.go:95`, `pkg/runner/job_executor.go:107` |
| F5 | `--reuse` (`-r`) defaults false and keeps containers and their workdirs between runs. | `cmd/root.go:83` |
| F6 | `--pull` is a **boolean** (default true = re-pull images that already exist). The correct "offline" spelling is `--pull=false`, not `--pull never`. | `cmd/root.go:85` |
| F7 | `--rebuild` defaults true (rebuild local action images), `--use-gitignore` defaults true (gitignored paths are *not* copied into the container in the non-`--bind` path). | `cmd/root.go:86,92` |
| F8 | With `--bind`, the bind string is `<workdir>:<ToContainerPath(workdir)>`, and on Linux `ToContainerPath` returns the **same absolute path**. So inside the job container the workspace path is the host path, `GITHUB_WORKSPACE` is that path, and any file under the workspace — including `.evidence/` — is readable by the job. | `pkg/runner/run_context.go:173-183`, `pkg/container/linux_container_environment_extensions.go:20-46` |
| F9 | act resolves `GITHUB_SHA` / `GITHUB_REF` **on the host**, in the workdir, with go-git `PlainOpenWithOptions{DetectDotGit: true, EnableDotGitCommonDir: true}` — i.e. linked worktrees are understood, and a detached HEAD makes `FindGitRef` fail, after which `github.ref` falls back to `refs/heads/<--defaultbranch>`, or `refs/heads/master` when that flag is unset. | `pkg/model/github_context.go` `SetRef`/`SetSha`, `pkg/common/git/git.go:58-165` |
| F10 | If no `GITHUB_TOKEN` secret is present, act runs **`gh auth token` on the host** to mint one. | `cmd/root.go` `newRunCommand`, `pkg/gh/gh.go:10-40` |
| F11 | A `--secret NAME` with no `=` makes act **prompt on stdin** (`term.ReadPassword`). `--secret-file` never prompts, and its parser *ignores* a `.env`-style file that lives inside the workdir but symlinks out of it; a file named *outside* the workdir is read as given. | `cmd/secrets.go:14-33`, `cmd/project_files.go` `escapesWorkdir` |
| F12 | act's own servers: the artifact server is off unless `--artifact-server-path` is set; the Actions **cache** server is on unless `--no-cache-server`, stores under `--cache-server-path` (default `$HOME/.cache/actcache`) and binds `--cache-server-addr`, whose default is the host's **outbound IP**, not loopback. | `cmd/root.go:118-127`, `newRunCommand` |
| F13 | `--network` defaults to `host`, so job containers share the host network namespace. | `cmd/root.go:129` |
| F14 | Unless `--container-daemon-socket -`, act bind-mounts the docker socket into the job container at `/var/run/docker.sock`; `--container-options-policy auto` restricts host-path volumes only in that case. | `pkg/runner/run_context.go:133-140,798-818` |
| F15 | `--container-options` is used **only when the job declares no `container:`**; a job that does sets its own options and ours are discarded, so it cannot be relied on to carry a mount we require. | `pkg/runner/run_context.go:852-860` |
| F16 | An unrecognised flag is a fatal cobra error before any container starts. | cobra `Execute()`, `cmd/root.go` |
| V1 | `--bind` mount of a temp worktree is readable/writable by the daemon at the same path (F8 holds for a worktree under `$XDG_RUNTIME_DIR`). | F8 + V |
| V2 | act's JSON stream is unaffected by `--quiet=false` at the versions in range (the run log must keep step output). | F7 + V |
| V3 | act exits non-zero (and `--rm` really removes the containers) when the group gets `SIGTERM`; ADR-0004 §5.5's `orphaned_containers` path is the fallback. | F4 + V |

Two of these are load-bearing for decisions that would otherwise look arbitrary:

- **F2 + F3** mean a target repository named `.actrc` in its own root is, on released act, *config for our
  run*. Redirecting act's cwd (§7.6) is the only complete defence, because F2's accumulation rule means
  `--env`/`--var`/`--input`/`--platform` lines in that file would be **added** to ours no matter that we pass
  our flags last.
- **F8** means anything we put inside the workspace is readable by the job. The per-run secret file therefore
  must not live in the workspace, nor in the evidence root, which is inside it (§7.7).

## 2. Decision

`MUST`, `MUST NOT` and `SHOULD` are normative. §16 is the block the runner and supervisor tickets copy.

**D-1 — isolation is a server policy with three values, chosen by the operator: `--isolation
head-only|hybrid|worktree`, default `hybrid`.** `hybrid` runs `HEAD` in place and any other commit in a
synthesized worktree. There is no per-call argument for it (§14.1).

**D-2 — a `head_only` run is a *proved* HEAD, not an assumed one.** The gate (§4.2) requires: `HEAD`
resolves, the resolved target equals it, no tracked file is modified, sparse checkout is off, and the
repository is not bare. Anything else is `isolation_failed`, `isError: true`, nothing started.

**D-3 — a `worktree` run is a detached `git worktree add` at the resolved full SHA, in a per-run scratch
directory outside the workspace**, created after registration (phase `preparing_tree`) and destroyed in the
terminal commit path (§5). It carries the warning `isolation_worktree` so evidence consumers can tell the two
provenances apart, because a synthesized tree is genuinely different in three documented ways (§5.4).

**D-4 — commit resolution never fetches and never guesses** (§3): shape-validate before any subprocess, then
`git rev-parse --verify --quiet --end-of-options '<sha>^{commit}'`, `commit_unknown` / `commit_ambiguous`
otherwise, with candidate enumeration bounded by refs (§3.4).

**D-5 — serialization is two layers, checked in this order at registration: (L1) an in-process registry slot
keyed by the canonical workspace path, and (L2) `flock(2, LOCK_EX|LOCK_NB)` on
`<workspace>/.act-mcp.lock`.** L2 is the authority and catches both a second process and a second run inside
this one; L1 exists because re-locking the *same* open file description always succeeds — a cloned handle
would double-book the workspace silently — and because only the registry can answer `workspace_busy` with the
live `phase` and `started_at` ADR-0001 §5.3 demands, at zero filesystem cost. Busy is fail-fast
(`workspace_busy`, retryable, no queue, no `--lock-wait-secs`) (§6).

**D-6 — the lock file is never unlinked**, ever. Its content is advisory JSON that the holder rewrites as the
run's phase changes, which is what lets a *foreign* holder still answer `detail.{run_id,phase,started_at}` as
ADR-0001 §5.3 demands. Liveness is the kernel's: `flock` dies with the process, so there is no stale-lock
recovery protocol to get wrong (§6.5).

**D-7 — the argv is fully determined by the server's configuration and the call, never by anything in the
workspace** (§7): every flag whose value changes what runs is passed explicitly with its value (act's own
defaults can be overridden by an operator actrc, F2), act is spawned with `cwd = <run scratch>`, `stdin =
/dev/null`, absolute `-C`/`-W`, the four `--*-file` defaults redirected to our own inert file, `GITHUB_TOKEN`
always present in the secret file so act never runs `gh` (F10), and `--bind --rm --reuse=false --pull=false
--rebuild --use-gitignore --json --quiet=false` as the invariant core.

**D-8 — act's state stays where act puts it, except the two things this server must be able to sweep** (§8):
per-run scratch (secret file, inert file, worktree) under `--run-scratch`, default
`$XDG_RUNTIME_DIR/act-mcp-<uid>`; action cache, cache-server store and socket handling left at act's defaults
and documented as operator surface.

**D-9 — cleanup is per-terminal-path, and crash recovery is a startup sweep** (§9): containers are act's job
(`--rm`, plus the `orphaned_containers` warning after a `SIGKILL`), everything under `--run-scratch` is ours,
and `act-mcp` on startup removes leftover scratch — including worktree admin entries, which
`git worktree remove --force` cleans even for a directory that is already gone, so act-mcp never runs
`git worktree prune`.

**D-10 — nothing in this design may mutate the caller's repository in `head_only` mode** beyond what act's
`--bind` inevitably does: git commands are read-only (`--no-optional-locks`), the lock file is a new file
beside `.evidence/`, and `--isolation=worktree` is the answer for jobs that write.

## 3. Commit resolution (step 5 of ADR-0001 §2.9)

Resolution turns the caller's `commit` string into a full lowercase 40-hex object id **without network,
without guessing, and without letting the string reach a shell or a git revision parser unfiltered**.
mact-f1w.6 implements this; the runner only ever sees the resolved 40-hex form.

### 3.1 The pipeline, pinned

```
R1  shape          ^[0-9a-fA-F]{7,40}$                      else invalid_arguments   (no subprocess)
R2  normalize      lowercase; keep the original for detail.commit
R3  resolve        git -C <ws> rev-parse --verify --quiet --end-of-options '<sha>^{commit}'
                   rc 0  -> stdout is the full id; use it (this peels tags to commits)
                   rc !=0 -> R4
R4  classify       git -C <ws> cat-file -t '<sha>'          rc 0 -> commit_unknown reason=not_a_commit
                   else -> R5
R5  ambiguity      git -C <ws> for-each-ref --format='%(objectname) %(*objectname) %(refname)'
                   count refs whose tip or peeled object starts with <sha>
                   count >= 2 -> commit_ambiguous (candidates = those ids, sorted, first 10)
                   count <= 1 -> commit_unknown reason=not_found
```

`<ws>` is the canonical workspace path. `'<sha>^{commit}'` is passed as **one argv element**, quoted by
`Command::arg`, never interpolated into a shell string; `--end-of-options` is placed *after* `--verify
--quiet` (git rejects a flag after it, which is the point: a shape-legal string can never be read as an
option) and R1 already makes the payload hex-only, so the path is belt-and-braces rather than clever.

### 3.2 Never fetch

`fetch`, `pull`, `clone`, `remote` and `ls-remote` `MUST NOT` be invoked, ever, by any code path in
act-mcp (ADR-0001 §2.5). A commit the local object database lacks is `commit_unknown`; the remedy is the
caller's (`git fetch` in the workspace, then call again). The reason is not purity: an unattended local
server that fetches on a typo dials whichever remote the workspace happens to have configured, on a
caller-controlled revision, with the operator's credentials, during what the caller thinks is a validation
step.

### 3.3 Error mapping

| outcome | code | `isError` | `detail` |
|---|---|---|---|
| not 7..40 hex | `invalid_arguments` | true | `errors: [{path: "commit", reason, expected}]` |
| `^{commit}` fails, object exists with another type | `commit_unknown` | true | `{commit, reason: "not_a_commit"}` |
| `^{commit}` fails, no ref shares the prefix | `commit_unknown` | true | `{commit, reason: "not_found"}` |
| ≥ 2 refs share the abbreviation | `commit_ambiguous` | true | `{commit, candidates: [full ids]}` |
| resolved | — | — | the full id flows on; the result's `commit` is this value |

### 3.4 Ambiguity is bounded by refs, and that is a decision

`git rev-parse --disambiguate=<p>` is the textbook primitive and does not do what its man page suggests in
current git (it returns nothing for a prefix with two matching commits); the only complete enumeration is
`git cat-file --batch-all-objects --batch-check`, which is O(all objects) — seconds on a large clone. Putting
an O(object database) scan on the validation path of every mistyped commit is the wrong trade, so
act-mcp enumerates **refs only** (R5), which is where a caller can actually get a short id from. Consequence,
stated rather than hidden: an abbreviation that is ambiguous only against *unreachable* objects is reported as
`commit_unknown` rather than `commit_ambiguous`. The caller's remedy is identical in both cases — pass more
hex — and 40 hex is never ambiguous.

### 3.5 Things the resolved id already handles

- **abbreviated** input: R3 prints the full id, so everything downstream (evidence directory key,
  `commit` field) sees 40 lowercase hex (ADR-0002 §1.2).
- **tag** input: `^{commit}` peels annotated and lightweight tags; a tag is a *name for a commit* here and
  nothing else. (`github.ref` is act's business, §5.4.)
- **`HEAD` itself** is not accepted: R1 requires hex. A caller who means "whatever is checked out" passes
  `git rev-parse HEAD`'s output. This is deliberate — a non-hex revision expression (`main`, `HEAD~3`,
  `v1.2`) is a second, moving way to say commit, and the evidence is keyed by id.

### 3.6 Which mode this feeds

The resolved id is compared to `HEAD` in §4.1. Nothing in §3 mutates anything; steps R1–R5 run **before**
`run_id` allocation (ADR-0001 §2.9 step 5) and are inside the pre-start budget of ADR-0004 §4.4 but cheap
enough to ignore.

## 4. Choosing the tree: mode selection and the gate

### 4.1 Mode selection

| policy | requested commit == workspace `HEAD` | requested commit != `HEAD` |
|---|---|---|
| `head-only` | run in place (`mode: head_only`) | `isolation_failed` `detail.reason: "policy_head_only"` |
| `hybrid` (**default**) | run in place (`mode: head_only`) | worktree (`mode: worktree`) |
| `worktree` | worktree (`mode: worktree`) | worktree (`mode: worktree`) |

`detail.mode` in `isolation_failed` is therefore always one of the two values
`x-error-codes` already documents for that code. The comparison is **string equality of two full lowercase
ids**, each obtained from git, never a `git rev-list A..B` or `merge-base` question: "is this the tree act is
about to run" is not a genealogy question.

### 4.2 The gate (`head_only`), in pinned order

Each row is one subprocess. `git` is invoked as
`git -C <ws> --no-optional-locks <args>` with a scrubbed environment (§7.8) and a `--git-timeout-secs`
deadline (§10). `--no-optional-locks` is not decoration: without it `git status` refreshes and rewrites the
caller's index, which would make act-mcp a writer in somebody else's repository.

| # | check | command (after `-C <ws> --no-optional-locks`) | pass | on fail |
|---|---|---|---|---|
| G1 | inside a work tree (step 4, done by .6) | `rev-parse --show-toplevel` | toplevel path | `not_a_git_repository` |
| G2 | `HEAD` resolves | `rev-parse --verify --quiet --end-of-options HEAD` | full id = `head` | `isolation_failed` `reason: "head_unresolvable"` |
| G3 | not bare | `rev-parse --is-bare-repository` | `false` | `isolation_failed` `reason: "bare_repository"` |
| G4 | head is the commit | compare G2 to the resolved id | continue | (mode `worktree`, §4.4 or D-1) |
| G5 | no tracked modification | `status --porcelain --untracked-files=no` | empty output | `isolation_failed` `reason: "dirty_worktree"`, or warning `dirty_worktree` when `--allow-dirty-worktree` |
| G6 | full checkout | `config --bool core.sparseCheckout` | absent or `false` | `isolation_failed` `reason: "sparse_checkout"` |
| G7 | no submodules | `cat-file -e '<commit>:.gitmodules'` | rc != 0 | `isolation_failed` `reason: "submodules_unsupported"` |

Why each of G3–G7 is a hard gate rather than a warning:

- **G3** — a bare repository has no tree to point act at. `--isolation=worktree` works from a bare repository
  (git allows `worktree add` there); `head_only` cannot, so the answer is a typed error, not `git workdir`.
- **G5** — a dirty tree is the gate's own blind spot: `HEAD == commit` proves the *commits* agree, and the run
  then measures something else. Tracked-and-modified is the whole check; **untracked and ignored files are
  deliberately not dirt**, because build output, `node_modules` and `.evidence/` itself are untracked and a
  check that rejected them would reject every real workspace. Submodule pointer changes *are* reported by
  this invocation and count as dirt. The escape hatch `--allow-dirty-worktree` (operator, default off) turns
  G5 into the warning `dirty_worktree` — an operator who keeps a permanently-dirty tree and accepts that the
  `commit` field then means "HEAD plus whatever is in it" can say so; a caller cannot.
- **G6** — a sparse checkout is a *partial* materialisation of the commit. A coverage run against a cone that
  excluded half the source would publish a `commit` that the numbers do not describe. Detected through
  config, not through `git sparse-checkout list`, because `rev-parse --sparse-checkout` prints a literal
  option name rather than a state in current git.
- **G7** — checked in the **target commit's tree** (`cat-file -e`), not the working tree, so the same rule
  works in both modes. A workflow whose repo has `.gitmodules` will meet empty directories in `worktree` mode
  and whatever the operator happens to have initialised in `head_only` mode; neither is a function of the
  commit, so act-mcp refuses instead of measuring a third thing. The message names the remedy (a workspace
  without submodules, or a job that does not need them); act-mcp never runs `submodule update`, because that
  needs the network (§3.2).

`isolation_failed` `detail` is `{mode, requested, head, reason}` — all four always present, `head` the empty
string only in G2's own failure — plus, where useful, the extra keys `path` and `os_error` that
`$defs/error.detail` explicitly allows. `message` never contains an absolute host path (ADR-0001 §5.2).

### 4.3 Where the gate runs, and what its failures look like

G1–G7 run at ADR-0001 §2.9 step 8: after resolution (step 5) and workflow/task resolution (step 6), before
capacity and the lock (step 9) and before `run_id` exists (step 10). That ordering is what makes
`isolation_failed` mean "nothing was started" and keeps it `isError: true`. It also means the gate is
**re-checked** by nothing later: a workspace whose tree changes between step 8 and act's `exec` is a race the
operator lost, not something act-mcp can prevent (ADR-0004 §18 asks only that the check stay cheap and early).

Cost note for G5: `git status` on a cold index of a very large repository can take many seconds. It is bounded
by `--git-timeout-secs` (§10) and lives inside ADR-0004's `--prestart-timeout-secs` (120 s default), so the
worst case is a `isolation_failed reason: "git_timeout"` that the caller may retry, never a hung call.

### 4.4 The worktree path's own pre-checks

`mode: worktree` replaces G3/G5/G6 with three different checks, in this order, all also at step 8 and all
`isolation_failed` `detail.mode: "worktree"`:

| # | check | how | on fail |
|---|---|---|---|
| W1 | policy allows it | `--isolation != head-only` | `reason: "policy_head_only"` (mode `head_only`) |
| W2 | no submodules | as G7 | `reason: "submodules_unsupported"` |
| W3 | scratch root usable | `mkdir -p <run-scratch>` then `stat` for writability | `reason: "scratch_unwritable"`, with `path` and `os_error` |

`--allow-dirty-worktree` has no effect in this mode: a synthesized worktree is clean by construction, which is
the main thing it buys.

Anything that fails **after** registration — `git worktree add` itself, the checkout filling up the disk, a
`git` call that hangs past `--git-timeout-secs` — is `runner_internal_error` with
`detail.stage: "preparing_tree"` and a `reason`, surfaced as `status: "error"` inside a normal result
(ADR-0004 §18, and §15.3 for the one line of prose this narrows in ADR-0001's table).

## 5. `worktree` mode

### 5.1 The commands, pinned

`<scratch>` is `--run-scratch` (§11), `<run>` is the `run_id` allocated at step 10, and `<tree>` is
`<scratch>/<run>/worktree`. `git` here is the same wrapper as §4.2 (scrubbed env, `--git-timeout-secs`
deadline, argv elements never interpolated), but **without** `--no-optional-locks`: this is a real mutation
and git's locks are wanted.

```
create   mkdir -p <scratch>/<run>                        # mode 0700, fchmod-applied
         git -C <ws> worktree add --detach <tree> <full-sha>
         # optional, only when the workspace HEAD is symbolic (§5.4):
         git -C <ws> symbolic-ref --quiet --short HEAD   # -> <branch>, rc 1 when detached
destroy  git -C <ws> worktree remove --force <tree>      # rc 0: directory and admin entry gone
         # fallback, in order, each step best-effort:
         rm -rf <tree>
         git -C <ws> worktree remove --force <tree>      # rc 0 again: cleans the admin entry of a vanished tree
         rm -rf <scratch>/<run>
```

Notes on the details that an implementer would otherwise guess:

- The leaf `<tree>` must **not** exist: `git worktree add` fails on an existing path, so act-mcp creates only
  the parent `<scratch>/<run>`. A collision there means a `run_id` collision, which is ADR-0002 §6.2's
  "bug worth surfacing": `runner_internal_error` `detail.stage: "preparing_tree"`, nothing deleted.
- `--detach` is not optional. Without it git refuses a commit already checked out elsewhere, and every
  synthesized tree is detached anyway. Detachedness has a visible consequence (§5.4).
- `worktree remove` refuses a tree containing modified or untracked files — which is the normal outcome of a
  job under `--bind` — so `--force` is mandatory, not a fallback.
- `git worktree remove --force` is idempotent in the way cleanup needs: on a path whose directory is already
  gone it still exits 0 and drops the admin entry (verified by C10), so the fallback above needs no
  `git worktree prune`. Not running `prune` at all is deliberate: `prune` would also drop admin entries for
  worktrees *the operator* deleted by hand, and act-mcp has no business editing that part of somebody else's
  repository metadata.
- `git worktree add` writes a fresh checkout (no hardlinks, no copy of the object database). Cost is the
  working-tree size in inodes and seconds — typically a second or two next to a minutes-long act run, and
  unbounded in principle on a huge monorepo, bounded in practice only by disk.

### 5.2 What the job sees

`-C <tree>` and `-W <tree>/.github/workflows/<file>` (F1), the payload scratch at
`<tree>/.evidence/incoming/<run>` (identical path inside the container, F8), and nothing of the operator's
checkout — no `.git` directory, no untracked build output, no local-only files. That last part is the point:
`worktree` mode is the mode that says "the commit, the whole commit", which is also why it is the mode to
choose when the job *writes* (§13).

### 5.3 Durable evidence never lives in the tree

ADR-0002 §1.1 already fixes the evidence root to the primary workspace. Restated because the runner is the
code that could get it wrong: `--evidence-dir` is never rewritten to `<tree>`, `act.jsonl`/`meta.json` are
written under `<evidence-root>/runs/<run>/`, and only `incoming/` follows the tree — so `rm -rf` of a worktree
can never delete a published number.

### 5.4 The three honest differences, and what act-mcp does about each

| difference | cause | action |
|---|---|---|
| `git` inside the job container cannot resolve the repository: `<tree>/.git` is a *file* whose `gitdir:` points at `<common>/worktrees/<name>`, which is outside the single bind mount act creates | a linked worktree shares the object store *by reference*; there is no way to make the tree self-contained without copying the object database or mounting a second path | act-mcp does **not** try to mount the common dir with `--container-options`: F15 shows a job that declares `container:` would discard our options, so the mount could not be guaranteed. Instead the run carries the warning `isolation_worktree`, and the runbook (mact-f1w.18) says the fix for a job that needs `git` is `--isolation=head-only` with a caller-managed checkout |
| `github.ref` / `ref_name` / `ref_type` are not what a real runner would report: a detached HEAD makes act's `FindGitRef` fail (F9) and `github.ref` falls back to `refs/heads/<--defaultbranch>` or `refs/heads/master` | the synthesized tree has no ref; inventing one would be a lie of a different shape | act-mcp passes `--defaultbranch <branch>` when `git symbolic-ref --quiet --short HEAD` in the **primary** workspace succeeded (§5.1), which makes the fallback name the operator's real branch instead of git's `master` guess, and is identical in both modes. It does not fake `GITHUB_REF` itself. `github.sha` is correct in both modes, because act reads it from the worktree on the host (F9) and the worktree's HEAD *is* the commit |
| the tree is not the operator's tree: untracked-but-committed-adjacent state (generated files, tool caches outside `target/`, a `gh` config) is missing, and LFS content is whatever the local object cache already holds (no fetch, §3.2) | same root cause as row 1 | warning `isolation_worktree`, so a scanner can exclude these runs from a trend the way ADR-0002 §5.1 lets it exclude anything else it does not trust |

A comparison-integrity consequence follows and is *not* papered over: under `hybrid`, a run at `HEAD` and a
run at its parent use **different** mechanisms and can therefore differ in the rows above. A sweep that
publishes a trend line across commits should run one mechanism throughout, i.e. start the server with
`--isolation=worktree` (uniform, and immune to the operator's own dirty tree) or `--isolation=head-only` plus a
caller-managed checkout loop. `hybrid` is the right default for what a caller does most often — one spot
check of the commit it is standing on.

### 5.5 Cancellation and failure inside `preparing_tree`

`preparing_tree` is a real phase, not a zero-duration hop (ADR-0004 §3.6 answered by this ADR), so a cancel
may arrive during it. Rules:

- the `git worktree add` process group is killed with the same mechanism ADR-0004 §5 uses for act (its own
  process group, `SIGTERM` then `SIGKILL`), and a partially created `<tree>` is removed by §5.1's destroy
  sequence;
- a cancel observed before act spawns still produces `kill.method: "none"` (ADR-0004 §16.2) — creating a
  worktree is not starting the process;
- §9's cleanup matrix applies to every terminal state, `cancelled` included: the worktree is destroyed whether
  act ever ran or not.

## 6. Workspace run serialization

### 6.1 Two layers, and why one is not enough

| layer | mechanism | catches | `workspace_busy` `detail` source |
|---|---|---|---|
| L1 | in-process registry: `HashMap<canonical workspace path, run_id>` written at registration, cleared at ADR-0004 §3.4 step 6 | a second run in this process when the lock handle was *shared* (re-`flock`ing the same open file description always succeeds), and it is the only layer that knows the live phase | the registry's own `run_id`, `phase`, `started_at` (exact, live, no filesystem read) |
| L2 | an `O_CREAT` open plus `flock(fd, LOCK_EX \| LOCK_NB)` on `<workspace>/.act-mcp.lock` | a second act-mcp instance on this host pointed at the same workspace, and a second run in this one through an independent handle | the lock file body (§6.3), which names another process's run |

One workspace per server process (ADR-0001 §2) means L1 is normally a single slot; it is a map so that a
future multi-workspace server needs no protocol change. `list_tasks` takes no lock (ADR-0001 §6.1) and stays
the cheap way to look at a busy workspace; `run_status` and `cancel_run` must work while the lock is held —
they are how a caller inspects and releases the thing that holds it.

Both claims in that table are executed by the checker rather than asserted (C9): Linux denies an independent
second descriptor even inside the holding process, *and* trivially grants a second `flock` on the descriptor
that already holds it. That asymmetry is the whole justification for L1 — it is what turns a
"the lock handle accidentally ended up in two RunHandles" bug from a silent double-run into a
`workspace_busy`.

### 6.2 Scope: the workspace, never the workflow

One run per **workspace**, always, in every isolation mode. Per-workflow or per-tree locking is finer than the
hazard: what must not overlap is two container-heavy jobs inside one machine measuring each other. A benchmark
that runs beside another benchmark measures both of them badly (CPU, page cache, docker daemon, the shared
`$HOME/.cache/actcache` of F12), and ADR-0004 §8 already rejected a queue for the same reason. With
`--max-concurrent-runs` at 2, the second slot is for a **different workspace**; a second run in *this*
workspace is `workspace_busy` even when the server has capacity, and even in `worktree` mode where two trees
would technically fit.

The key is the canonicalized workspace path (`realpath`), so `/ws`, `/ws/./` and a symlink to it contend.

### 6.3 The lock protocol

```
acquire   fd = open(<lock>, O_CREAT|O_RDWR|O_CLOEXEC, 0644)
          flock(fd, LOCK_EX | LOCK_NB)          # EWOULDBLOCK -> workspace_busy (fail-fast)
          ftruncate(fd, 0); write(body); fsync(fd)
hold      body rewritten (no fsync) whenever the run's phase changes
release   flock(fd, LOCK_UN); close(fd)         # at ADR-0004 §3.4 step 6
```

- `O_CLOEXEC` is not optional. If the lock fd leaked into act, act would hold the workspace lock for its own
  lifetime and every subsequent run in that workspace would be `workspace_busy` until the container exited.
  Rust sets `FD_CLOEXEC` by default for `File::open`; the invariant is asserted by a test (§12.2 T7) rather
  than trusted.
- The lock is taken at **registration** (step 9/10 boundary) and released at **step 6 of the terminal commit**
  — i.e. it covers `preparing_tree`, act's whole life, evidence extraction and publication. A dropped HTTP
  request cannot release it early (ADR-0004 §8).
- `open` failing (EACCES, EROFS, ENOENT on a vanished workspace) is a server misconfiguration, not a caller
  error: `internal_error`, `isError: true`, `retryable: true`, `detail: {reason:
  "workspace_lock_unavailable", path, os_error}`. `$defs/error.detail` explicitly allows keys beyond the ones
  `x-error-codes` lists, so no contract change is needed (§15.5).

### 6.4 The body, which is what makes `detail` honest across processes

Single-line JSON, rewritten in place by the holder:

```json
{"run_id":"20261110T004203Z-a1b2c3","phase":"running","started_at":"20261110T00:42:03Z",
 "pid":41688,"hostname":"buildbox","actmcp":"0.1.0","isolation":"head_only"}
```

A caller that loses to L2 gets `detail: {run_id, phase, started_at, holder_pid, holder_host}` with the three
keys ADR-0001 §5.3 requires filled from the body — so "poll it or cancel it" works across server processes,
not just inside one. The body is **advisory**: it is never used to decide whether the lock is free, it may be
a few hundred milliseconds stale, and a reader that cannot parse it reports `run_id: "unreadable"` and
`phase: "unreadable"` and **omits** `started_at` — the one documented case where a `detail` key is absent,
which is what ADR-0001's nullability rule ("unknown is expressed by omission; JSON null is never valid")
demands instead of a `null` timestamp.

### 6.5 Stale locks, restarts, and what "survives" means

The file survives a restart (it is never unlinked, by any process, ever); **the lock does not**. `flock` is
owned by the open file description, so a crash, `SIGKILL`, OOM or panic releases it with the process, which is
exactly what ADR-0004 §8 assumed and what makes a dead holder unable to wedge a workspace. There is therefore
no pid-liveness protocol, no lock TTL, and no "break a stale lock" path: a `flock` that cannot be taken is
held by something alive. Deleting the file would be the bug — a lockfile unlinked while held lets the next
two callers each create a fresh inode and both believe they own the workspace.

Cross-host: `flock` on NFS is version- and mount-dependent and `docker` cannot be shared across hosts anyway;
the supported configuration is one host, local filesystem, and act-mcp does not pretend otherwise.

Timeouts: none. Busy is answered in microseconds with `workspace_busy` (`retryable: true`, no queue —
ADR-0004 §8), and `detail.run_id` is the caller's hint about when a retry is worth it.

### 6.6 What the lock is *not*

It is not a git lock: two `git worktree add` calls from *different* tools on one repository are serialized by
git's own `REFS`/worktree locks and may legitimately fail with git's own message (→
`runner_internal_error`, `detail.stage: "preparing_tree"`). It is not an evidence lock — the evidence layer
relies on it rather than adding its own (ADR-0002 §6.3), and per-run filenames make a collision impossible
anyway. It does not cover `list_tasks`, `run_status`, `cancel_run`, or act's own daemon-facing work.

## 7. The act invocation

This section is the deliverable mact-f1w.8 asked for: the exact argv, in order, with the reason each flag is
present. `meta.json.act.argv` (ADR-0002 §1.3) records the result with secrets already redacted, so a run is
replayable from its manifest.

### 7.1 The rule that generates the argv

**Every flag whose value changes what runs, or what the evidence means, is passed explicitly with its value;
flags left out are operator surface, deliberately.** The reason is F2: act's own defaults can be overridden
by config files act-mcp does not control, and for `StringArray` flags config files *add* to our argv rather
than being overridden by it. A flag we merely assume is a flag an operator actrc can quietly flip.

Derived rules, all checked by `scripts/validate_isolation.py`:

1. absolute paths only (`-C`, `-W`, `--secret-file`, `--*-file`), so act's `resolve()` never has an opinion;
2. never emit a flag that does not exist across the whole supported act version range (§7.7) — F16 makes an
   unknown flag a fatal error before any container starts, which would look like a failing workflow;
3. never pass a value-less `--secret`/`-s` (F11: act prompts on stdin);
4. `--input key=value` occurrences sorted by key, so argv is byte-stable for golden tests (ADR-0001 §2.7);
5. secret values are never argv (ADR-0001 §2.7) — the file is the only channel;
6. no `--act-arg`-style passthrough (§14.6): the argv must be a function of the call and the server config
   only.

### 7.2 The base argv

Order is normative (it makes `act.argv` diffable); `<>` marks one substitution.

```
act
  --directory   <tree>
  --workflows   <tree>/.github/workflows/<file>
  --job         <task>
  --bind
  --reuse=false
  --rm
  --pull=false
  --rebuild
  --use-gitignore
  --json
  --quiet=false
  --secret-file <scratch>/<run>/secrets.env
  --var-file    <scratch>/<run>/inert.conf
  --env-file    <scratch>/<run>/inert.conf
  --input-file  <scratch>/<run>/inert.conf
  --env         ACTMCP_EVIDENCE_OUT=<tree>/.evidence/incoming/<run>
  --env         ACTMCP_RUN_ID=<run>
  --input       <k>=<v>            # repeated, sorted by k; omitted when the call had no inputs
  --defaultbranch <branch>         # worktree mode only, and only when §5.1's symbolic-ref succeeded
```

### 7.3 Why each one is there

| fragment | reason |
|---|---|
| `--directory <tree>` | F1. `<tree>` is the workspace (`head_only`) or the synthesized worktree (`worktree`). This one flag *is* the isolation decision as far as act is concerned. |
| `--workflows <tree>/.github/workflows/<file>` | the file mact-f1w.6 resolved (ADR-0001 §2.2), absolute; act's default is `./.github/workflows/` (a directory, which would run other workflows' plans) |
| `--job <task>` | ADR-0001 §2.3 — one job, so the numbers are attributable |
| `--bind` | ADR-0002 §3.1: the payload transport is the bind mount. Also what makes the tree the *live* tree rather than a snapshot act copies |
| `--reuse=false` | F5: reusing containers carries state (files, `/tmp`, running services) from one measurement to the next |
| `--rm` | F4: act's default leaves a container behind after a **failed** job, and failed jobs are ordinary output here. ADR-0004 §5.5 already assumed this flag; here it is |
| `--pull=false` | F6. Two reasons, one empirical: a benchmark whose first step is a 400 MB layer pull measures the registry; and act's default re-pulls *mutable tags*, so `ubuntu-latest` differs run to run anyway. Staying on the local cache makes consecutive runs comparable and lets a build box run with no network at all. What it costs is stated in §8.2 |
| `--rebuild` | F7: a local action's `Dockerfile` is part of the commit and must be rebuilt, not reused from an older commit's build |
| `--use-gitignore` | F7, only load-bearing in the `--act-no-bind` degraded mode: a real runner gets a fresh checkout without ignored artifacts, and so should the copied context |
| `--json` | ADR-0002 §1.3: `act.jsonl` is act's JSON stream, and ADR-0002 §5.2's in-band marker rides on it |
| `--quiet=false` | explicit, because act's log is both the operator's debugging surface and the ring buffer behind `log_tail` (ADR-0001 §2.8) |
| `--secret-file <scratch>/<run>/secrets.env` | ADR-0001 §2.7. `mode 0600`, created 0700-dir, contains `KEY=value` lines for the call's secrets **plus `GITHUB_TOKEN=` when the caller supplied none** (§7.7). It lives outside the workspace because F8 makes everything inside the workspace readable by the job, and outside the evidence root for the same reason |
| `--var-file` / `--env-file` / `--input-file` `<scratch>/<run>/inert.conf` | F1: act's defaults (`.vars`, `.env`, `.input`) are resolved against `-C`, i.e. **against the target tree**, so a repository shipping a `.env` would silently inject host environment into the job and change what is measured without appearing anywhere in the evidence. `inert.conf` is a 0-byte file we created in a 0700 directory we own; a file named outside the workdir is read as given (F11). `--act-workspace-config-files discover` restores act's default for a repository that legitimately depends on `.env` |
| `--env ACTMCP_EVIDENCE_OUT=...` / `ACTMCP_RUN_ID=...` | ADR-0002 §3.1, with one correction: under `--bind` the container path **equals the host path** (F8), so the value is the host path of the incoming dir, not a `/srv/...` path. §15.1 |
| `--input <k>=<v>` | ADR-0001 §2.7 |
| `--defaultbranch <branch>` | §5.4 row 2 |

### 7.4 A complete example (the golden argv; mact-f1w.15 asserts against it)

Workspace `/srv/ws`, run `20261110T004203Z-a1b2c3`, `head_only`, one input:

```console
$ act --directory /srv/ws \
      --workflows /srv/ws/.github/workflows/coverage.yml \
      --job coverage --bind --reuse=false --rm --pull=false --rebuild --use-gitignore \
      --json --quiet=false \
      --secret-file /run/user/1000/act-mcp-1000/20261110T004203Z-a1b2c3/secrets.env \
      --var-file /run/user/1000/act-mcp-1000/20261110T004203Z-a1b2c3/inert.conf \
      --env-file /run/user/1000/act-mcp-1000/20261110T004203Z-a1b2c3/inert.conf \
      --input-file /run/user/1000/act-mcp-1000/20261110T004203Z-a1b2c3/inert.conf \
      --env ACTMCP_EVIDENCE_OUT=/srv/ws/.evidence/incoming/20261110T004203Z-a1b2c3 \
      --env ACTMCP_RUN_ID=20261110T004203Z-a1b2c3 \
      --input fuzz_rounds=4
```

`worktree` mode differs in exactly four places: `<tree>` is
`/run/user/1000/act-mcp-1000/<run>/worktree` in `--directory`, `--workflows` and `ACTMCP_EVIDENCE_OUT`, and
`--defaultbranch <branch>` is appended when available.

### 7.5 Flags deliberately never passed

| flag | why not |
|---|---|
| `-s/--secret NAME` | F11 — act prompts on stdin and hangs the run until the pre-start budget expires |
| `--artifact-server-path` and friends | rejected in ADR-0002 §3.1; leaving them off also means act binds no extra port |
| `--container-working-directory` | F8's path identity (host path == container path) is what `ACTMCP_EVIDENCE_OUT` relies on; changing it breaks the payload contract, and act's job cwd is already the workdir path |
| `--container-options` | F15 — a job declaring `container:` discards it, so nothing correctness-critical may ride on it |
| `-P/--platform`, `--default-image`, `--network`, `--container-daemon-socket`, `--privileged`, `--cache-server-*`, `--no-cache-server`, `--action-offline-mode`, `--env`/`--var`/`--matrix` beyond the two `ACTMCP_*` | operator surface (F2's accumulation makes config the *additive* place for them); several are also the security levers mact-f1w.13 should be able to pull without recompiling act-mcp — see §8.4 |
| a positional event name, `--eventpath`, `--detect-event` | the contract has no event concept; act's own default (`push`, or the workflow's single event) is what the tool silently gets today and pretending otherwise would need a new argument |

### 7.6 act's cwd, stdin and environment

```
cwd    = <scratch>/<run>/            # NOT the workspace
stdin  = /dev/null
stdout/stderr = the run's log pipe (ADR-0004 §5, ADR-0002 §1.3)
env    = act-mcp's own environment, unscrubbed
process_group(0)                                # ADR-0004 §9, the only signalling unit
```

The cwd choice is the fix for F2/F3 and deserves to be stated as the rule it is: **act's project-config lookup
uses the process cwd, so act-mcp gives it a cwd that contains no `.actrc`.** Pointing act at the target
repository with `-C` while keeping our own cwd means a repository named `.actrc` is simply never read —
including the `--env`/`--var`/`--input`/`--platform` lines that rule 2 of F2 says would otherwise be
*appended* to our run. Everything act needs from the tree it gets through absolute flags. Nothing the operator
configures is lost: `$XDG_CONFIG_HOME/act/actrc` and `$HOME/.actrc` are looked up by absolute path.

`stdin = /dev/null` is the same class of protection (F11, and any future act prompt): a prompt must fail
instantly rather than eat the pre-start budget.

The environment is passed through untouched on purpose: act needs `DOCKER_HOST`/`DOCKER_CONTEXT`, `HOME` and
`XDG_*` for exactly the caches of F12, and F8/F14 explain why host environment values do not reach the job —
the job's environment is what act builds, plus our two `--env` lines.

### 7.7 Server-side defaults that are not flags

| item | pinned value | why |
|---|---|---|
| `GITHUB_TOKEN` in `secrets.env` | always present; the caller's value, or an empty one | F10: otherwise act shells out to the operator's `gh auth token` — an unattended host subprocess reading the operator's credentials on behalf of a caller-selected workflow. A job that genuinely needs a token now fails with an authentication error that names the missing secret, instead of borrowing the operator's identity |
| `inert.conf` | exists, 0 bytes, 0600, inside the 0700 run dir | §7.3; a missing path would also work (act ignores unreadable/inaccessible project files) but an empty file we created is observable, testable and not a bug-shaped coincidence |
| act version floor | every flag emitted exists with identical semantics from **v0.2.62** through **v0.2.97** | mact-f1w.6 owns `--minimum-act-version` and may raise it; it `MUST NOT` go below `0.2.62`, and any flag added later to act `MUST NOT` be emitted while the configured minimum is older (F16) |
| `--act-no-bind` (operator, default off) | ADR-0002 §3.1's degraded mode: omit `--bind`, keep everything else, stamp `payload_transport_unavailable` | the payload then arrives only through the in-band marker; nothing else in this ADR changes |

### 7.8 The git wrapper used by §3–§5

```
git -C <ws> [--no-optional-locks] <args...>
env = { PATH, HOME, LC_ALL=C, GIT_TERMINAL_PROMPT=0 }     # every GIT_* from our own env is dropped
timeout = --git-timeout-secs, on the process group
```

Dropping inherited `GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`, `GIT_OBJECT_DIRECTORY` and friends is what
lets this ADR promise that the gate depends only on the workspace path and the repository's own config;
`GIT_TERMINAL_PROMPT=0` keeps a credential helper from blocking a call that is supposed to be validation.

## 8. Where act's state lives

### 8.1 The table

| state | default | act-mcp's choice | operator lever |
|---|---|---|---|
| per-run scratch (secret file, inert file, worktree) | — | `--run-scratch` = `$XDG_RUNTIME_DIR/act-mcp-<uid>` (fallback `/tmp/act-mcp-<uid>`), mode 0700; `<run-scratch>/<run>/` per run, wiped on teardown and at startup | `--run-scratch` |
| action cache (cloned action repos, host workspaces) | `$HOME/.cache/act` | left alone; shared across workspaces, which is what makes runs fast and is act's own design | `--action-cache-path` in the user actrc |
| Actions cache server store | `$HOME/.cache/actcache` | left alone; note it is **shared**, so a warm cache makes run N cheaper than run N-1 — a benchmark that restores caches measures a warm machine, deliberately | `--cache-server-path`, `--no-cache-server` |
| cache server socket | binds `--cache-server-addr`, default the host's **outbound IP** (F12) | left alone; flagged to mact-f1w.13 as a listener that is not loopback | `--cache-server-addr 127.0.0.1` (works because F13's `--network host` means containers reach loopback) |
| artifact server | off unless `--artifact-server-path` | stays off (ADR-0002 §3.1) | none needed |
| docker daemon socket | bind-mounted into the job (F14) | left at act's default: a job that builds images genuinely needs it, and taking it away changes what workflows run | `--container-daemon-socket -` — the single biggest hardening lever, and it also arms `--container-options-policy auto`; **V3**-adjacent, so measure it in mact-f1w.19 before recommending it |
| `.actrc` | XDG, `$HOME`, then `./.actrc` of the **cwd** (F2/F3) | project file never reached (§7.6); user files honoured as operator config | the user actrc is *the* sanctioned extension point; remember rule 2 of §7.1 (array flags accumulate) |
| images | local daemon store | `--pull=false` (§7.3) | `-P`/`--default-image` with a digest-pinned reference, which is how an operator makes a benchmark reproducible instead of merely repeatable |

### 8.2 What `--pull=false` gives up, honestly

An image tag is mutable, so `--pull=false` means "the run used whatever `node:20` was on this machine",
and nothing in `meta.json` records which digest that was. Accepted, for three reasons: the alternative
(`--pull=true`, act's default) does not fix the ambiguity — it moves it from "stable and local" to "whatever
upstream published today", while also spending minutes per run; recording a digest would mean act-mcp
inspecting docker behind act's back, which ADR-0004 §5.5 already declines as a category; and the honest
answer to "make benchmarks reproducible across machines" is digest pinning via `-P`, which the runbook
publishes. If mact-f1w.19 finds a cheap read-only way to record the resolved digest per run, adding it is an
**additive** `meta.json` change under ADR-0002 §7 and belongs to that ticket, not here.

### 8.3 Determinism consequences, stated as evidence semantics

Two runs of the same commit on the same machine can differ because of: a warm action cache, a warm
cache-server store, a mutated local image tag, a concurrent run in another workspace (§6.2), or a
`gh`/token-dependent network path (closed by §7.7). act-mcp does not pretend to sandbox measurements; the
contract is that it removes the sources of drift a *caller* could cause (checkout state, injected `.env`,
caller tokens, container reuse) and documents the ones left.

### 8.4 For mact-f1w.13 (hardening), three facts this ADR establishes

1. `--network host` is act's default (F13), so a job can reach `127.0.0.1:7788` — i.e. **a workflow can call
   `run_workflow` on the server that started it**. The recursion is bounded (`--max-concurrent-runs`, the
   single-flight lock, `workspace_busy`) and it crosses no trust boundary that `--bind` does not already cross
   (ADR-0002 §3.1 declares the workflow trusted), but it should be in the security notes, with the mitigation
   (`--network` to a bridge network in the operator actrc, at the cost of service-container ergonomics).
2. The docker socket is mounted into the job unless `--container-daemon-socket -` (F14), which is
   host-root-equivalent. Same trust domain, different magnitude; worth one runbook paragraph and one
   measured experiment.
3. Everything act-mcp writes inside the workspace (`.evidence/`, `.act-mcp.lock`) is world-readable to the
   job (F8); that is why secrets moved to `--run-scratch`.

## 9. Cleanup guarantees

### 9.1 Per terminal path

Order inside the terminal commit is ADR-0004 §3.4's; "step 6" is where the lock and the tree go. The secret
file is unlinked as soon as act's exit status is known — act does not need it after exit, and holding secrets
longer than necessary has no upside.

| path | act process | containers | worktree | `<run-scratch>/<run>/` | `secrets.env` | payload scratch (`incoming/<run>`) | lock |
|---|---|---|---|---|---|---|---|
| `success` / `failed` | exited | removed by act (`--rm`, F4) | removed at step 6 | removed after act exit | unlinked at exit | per ADR-0002 §3.4, then the directory is removed | released at step 6 |
| `cancelled` (caller cancel) | group `SIGTERM` → `SIGKILL` if needed → reaped | removed if act handled `SIGTERM`; `orphaned_containers` after a `kill` (ADR-0004 §5.5) | removed at step 6, even if act never spawned (§5.5) | removed | unlinked | removed; `partial_evidence_dropped` if payload had staged (ADR-0004 §6.2) | released |
| `timed_out` | same mechanism, deadline-initiated | same | same | same | same | extraction attempted (ADR-0004 §5.1) | released |
| `error` (server shutdown) | same, `--shutdown-grace-secs` | same | same | same | same | as `timed_out` | released |
| act-mcp `SIGKILL` / OOM / panic | may survive it (mact-f1w.8's `PDEATHSIG` verdict decides) | orphans possible, and nothing on this host can warn: the run has no `meta.json`, which is ADR-0002 §1.3's meaning | **leftover** → §9.2 | **leftover** → §9.2 | leftover → §9.2 (0600, but still: swept) | leftover inside the tree; harmless, removed by the next `--bind` run's own cleanup or §9.2 | died with the process (§6.5) |

Two invariants tie the rows together: **a published evidence file is never deleted** (ADR-0002 §6.2) — cleanup
deletes only scratch, never output — and **nothing outside `--run-scratch` is deleted, and no repository
metadata is edited, that this process did not create**.

### 9.2 Startup sweep

Runs after ADR-0004 §9.3's "no `meta.json`" log lines, in this order, all best-effort with one `WARN` line
each:

```
for d in <run-scratch>/*/:                       # nothing is live yet, so all of it is debris
    git -C <ws> worktree remove --force d/worktree        # rc 0 even if d/worktree is already gone
    rm -rf d
for d in <evidence-root>/incoming/*/:           # ADR-0002 §3.4: incoming is scratch, may vanish anytime
    if <evidence-root>/runs/<d>/meta.json absent: rm -rf d
```

The sweep is the whole crash-recovery story for §5: no daemon, no scheduled job, no lock needed (nothing is
live before the server accepts a call), and nothing it can destroy — the alternative would be that every
`SIGKILL` of act-mcp leaks a checked-out tree into a runtime directory until the disk fills.

### 9.3 Warnings this ADR adds

Three members of the **open** `warnings` vocabulary (ADR-0001 §5.4; additive, unknown codes are ignored, so
nothing downstream breaks). They belong in `meta.json.warnings` and in `run_view.warnings`:

| warning | meaning |
|---|---|
| `isolation_worktree` | the run executed in a synthesized worktree, not the operator's checkout: `git` inside the job may be unusable and `github.ref` is a fallback (§5.4) |
| `dirty_worktree` | `head_only` accepted a tree with tracked modifications because `--allow-dirty-worktree` is set; `commit` names HEAD, the run measured HEAD-plus-edits |
| `worktree_cleanup_failed` | the temporary worktree could not be removed (permissions, busy mount); the numbers are valid, the operator must delete `<run-scratch>/<run>/` |

## 10. Budgets

Everything in this ADR happens before act spawns, so it all lives inside ADR-0004 §4.4's
`--prestart-timeout-secs` (120 s). One new budget, because a hung `git status` on a network mount is a
different failure from a slow registration and needs its own kill:

| budget | default | bound | on expiry |
|---|---|---|---|
| `--git-timeout-secs` | 30 | 1..=300, and startup aborts if it exceeds `--prestart-timeout-secs` | the git process group is killed; step-8 failure → `isolation_failed` `reason: "git_timeout"` (retryable), post-registration → `runner_internal_error` `detail.stage: "preparing_tree"` |

`git worktree add` is the one call in §5 that can legitimately want more than 30 s (a huge monorepo on a cold
cache). It gets `--git-timeout-secs` like everything else; an operator who hits it raises `--git-timeout-secs`
and `--prestart-timeout-secs` together, and the startup check refuses the combination that cannot work.

## 11. Configuration surface

All are clap flags on `serve`, in the same style as ADR-0004 §11 so mact-f1w.5 ships one table.

| flag | default | bound / validation | owner of the mechanism |
|---|---|---|---|
| `--isolation <policy>` | `hybrid` | one of `head-only` / `hybrid` / `worktree`, else startup abort | .6 (mode selection), .8 (worktree create) |
| `--allow-dirty-worktree` | `false` | gate-only, §4.2 G5 | .6 |
| `--workspace-lock <path>` | `<workspace>/.act-mcp.lock` | parent directory must exist and be writable, else startup abort | .11 |
| `--run-scratch <dir>` | `$XDG_RUNTIME_DIR/act-mcp-<uid>`, fallback `/tmp/act-mcp-<uid>` | created mode 0700; must be on a local filesystem; must **not** be inside the workspace | .8 |
| `--git-timeout-secs` | 30 | 1..=`--prestart-timeout-secs` | .6, .8 |
| `--act-binary <path>` | `act` | probed by .6 (`act_missing`, ADR-0001 §5.3) | .8 |
| `--act-no-bind` | `false` | degraded mode, §7.7 | .8 |
| `--act-workspace-config-files <mode>` | `inert` | `inert` redirects the four `--*-file` flags to our own file (§7.3); `discover` leaves act's defaults | .8 |

No flag here changes the wire contract. `--minimum-act-version` stays mact-f1w.6's, with the floor of §7.7.

## 12. Verification

### 12.1 `scripts/validate_isolation.py` (dev-time, runs without docker)

| # | check |
|---|---|
| C1 | this ADR exists, says `status: accepted`, carries no unresolved marker, and mentions the tokens its own decisions depend on (`isolation_failed`, `workspace_busy`, `ACTMCP_EVIDENCE_OUT`, `--run-scratch`, the three warning codes) |
| C2 | every act flag emitted by §7.2/§7.3/§7.5 is in the transcribed act flag set (`ACT_FLAGS`, from act's `cmd/root.go` across the version range), and every flag in §7.5's "never passed" list is *absent* from the §7.2 argv — the two lists cannot quietly swap a member |
| C3 | the §7.4 golden command line tokenizes to exactly the argv §7.2's table implies for that scenario (same order, same values) |
| C4 | every `reason` string in the §4.2/§4.4 tables is `snake_case` and appears in the checker's pinned set, and every code named in those tables exists in `$defs/error_code` |
| C5 | the only `detail.mode` values the ADR uses are the ones `x-error-codes` documents for `isolation_failed` (`head_only`, `worktree`) — nobody invents a third mechanism name |
| C6 | the three new warnings of §9.3 appear in ADR-0001 §5.4 and in `docs/tool-contract.md` §7, and match `run-meta.schema.json`'s `warnings` pattern |
| C7 | §11's table parses, equals the checker's `EXPECTED`, and each bound is compatible with ADR-0004's (`--git-timeout-secs` ≤ `--prestart-timeout-secs`) |
| C8 | **live git**: builds throwaway repositories and runs the §3 and §4 commands as written, asserting the whole mapping — clean tree → `head_only`; modified tracked file → `dirty_worktree`; untracked-only → not dirty; sparse → `sparse_checkout`; bare → `bare_repository`; detached `HEAD` → mode `head_only` still works; `.gitmodules` in the target tree → `submodules_unsupported`; abbreviated id → 40 hex; tag → peeled; unknown id → `commit_unknown/not_found`; blob id → `commit_unknown/not_a_commit`; ambiguous prefix (built by collision) → `commit_ambiguous` with 2 candidates; and that resolution never wrote to the repository (`--no-optional-locks` claim, checked by index `mtime` + no new lock files) |
| C9 | **live lock**: two real processes contend on the §6.3 protocol (second gets `EWOULDBLOCK`), an independent second descriptor inside the holder is denied while a second `flock` on the *holding* descriptor is trivially granted (the asymmetry L1 exists for), the body round-trips as JSON and carries the keys §6.4 lists, `SIGKILL`ing the holder lets a third process in (the no-stale-lock claim), and the file still exists after release (the never-unlink claim) |
| C10 | **live worktree**: `worktree add --detach` at a non-`HEAD` commit yields `<tree>` whose `HEAD` equals it and whose `.git` is a file pointing outside `<tree>` (the §5.4 claim, proven rather than asserted); a dirty tree makes `worktree remove` fail without `--force` and succeed with it; `remove --force` also cleans the admin entry of a directory that was `rm -rf`-ed, so `worktree prune` is never needed |

Exit status: 0 all consistent, 1 a contract violation (one line each), 2 environment error (missing file, or
`git` absent). `jsonschema` remains optional, as in the sibling checkers.

### 12.2 Runtime tests owned by other tickets

| # | test | ticket |
|---|---|---|
| T1 | stub act receives byte-for-byte the §7.4 argv (and the §7.4 `worktree` variant) | mact-f1w.15 |
| T2 | a second `run_workflow` for the same workspace while act runs → `workspace_busy` with `detail.run_id` equal to the in-flight run and `phase` from the registry (L1) | mact-f1w.11 |
| T3 | a second act-mcp process pointed at the same workspace → `workspace_busy` with `detail` filled from the **lock body**, not the registry (L2) | mact-f1w.11 |
| T4 | `run_id` is never allocated when §4 fails (`isolation_failed` ⇒ no run directory, no evidence file, `run_status` says `run_not_found`) | mact-f1w.11, .14 |
| T5 | cancel during `preparing_tree` → `kill.method: "none"`, worktree gone, no act invocation in the log | mact-f1w.8 |
| T6 | `--isolation=head-only` + a dirty tree → `isolation_failed reason: "dirty_worktree"`; with `--allow-dirty-worktree` → runs and carries `dirty_worktree` | mact-f1w.6 |
| T7 | while act runs, `/proc/<act pid>/fd` contains no descriptor for the lock file (the `O_CLOEXEC` claim of §6.3) | mact-f1w.8 |
| T8 | a job that writes into the tree does not make the next run fail: in `worktree` mode by construction, in `head_only` mode only via `--allow-dirty-worktree` (this is the test that documents §13's cost) | mact-f1w.15 |
| T9 | `secrets.env` is not readable from inside the job (it is outside the bind); the fixture job tries and records the failure | mact-f1w.13, .19 |
| T10 | kill act-mcp with `SIGKILL` mid-run, restart, assert: scratch swept, `git worktree list` clean, workspace lock acquirable, `.evidence` untouched | mact-f1w.8, .11 |

### 12.3 For mact-f1w.19 (real act)

`V1`–`V3` of §1.3, plus: does a `.actrc` in act's cwd still get applied as §7.1 claims (the defence we are
relying on), does `--container-daemon-socket -` break the fixture workflows (the §8.4 lever), and what is the
wall-clock cost of `git worktree add` on the largest repository we benchmark (informs whether
`--git-timeout-secs` 30 is the right default).

## 13. Consequences

**What this buys**

- The `commit` field is a fact, not a claim: either the tree provably is that commit and provably unmodified,
  or the run happened in a tree we built from it and said so.
- Non-`HEAD` commits are runnable at all, which is the entire point of commit-keyed evidence
  (ADR-0002 §1.2), without asking the caller to mutate a repository it does not own.
- Two server processes cannot trample one checkout, and the answer they give when they meet (`workspace_busy`
  with a run id and phase) is actionable across process boundaries.
- A repository cannot configure our run: no `.actrc`, no `.env`, no `.vars`, no `.input`, no borrowed
  `gh` token.
- Crash debris is bounded and swept by code that runs in one place (§9.2).
- The argv is a pure function of config plus call, recorded verbatim in `meta.json`, so a run is replayable
  and golden-testable.

**What it costs**

- `head_only` is strict, and one consequence needs saying plainly: because act runs with `--bind` (F8), **a job
  that modifies tracked files can make the next `head_only` run fail** with `dirty_worktree`. That is the
  gate doing its job — the tree no longer matches the commit — but the operator's remedy is `--allow-dirty-worktree`
  or `--isolation=worktree`, and the runbook must say so rather than let it look like a flaky server.
- `worktree` mode buys isolation from that problem at the price of §5.4's three differences, and those
  differences are invisible to a scanner unless it looks at `warnings`.
- `--isolation=hybrid` mixes mechanisms across commits (§5.4 end). Uniform sweeps must set a uniform policy.
- Four `--*-file` redirects and a `--defaultbranch` make the argv longer than act's help text suggests, and a
  reader will want to delete them. §7.3 is the reason they must not be deleted.
- `--pull=false` trades upstream freshness for repeatability, and does not record image digests (§8.2).
- The lock file and `.evidence/` are two new gitignored entries in every workspace we touch (§15.2).

## 14. Considered alternatives

### 14.1 Per-call isolation argument (the ticket's option (c) as written) — rejected

A `worktree: true` argument would let a caller choose the mechanism, and the arguments against are the same
ones ADR-0001 used to keep `workspace` and `ref` out of the schema: it is not the caller's cost to spend (disk,
checkout time, and the trust consequences of §5.4 land on the host), it makes `run_view` mean different things
in different calls of the same arguments, and it makes one server produce two provenances for the same
commit-shaped request. Policy is the operator's; the caller's request is a commit and the server answers with
whatever mechanism honestly delivers it.

### 14.2 Gate only (option (a)) — rejected

Two callers can never use two commits, which is the use case the evidence layout is built around, and a
check-out-then-run loop is racy against anything else on the box (including the operator). Kept as the
`head-only` policy for operators who want exactly that, and as the `HEAD` branch of `hybrid`, where its cost is
zero and its honesty is provable.

### 14.3 Worktree always (option (b) as the default) — rejected as default, kept as policy

Every common run ("coverage on what I have checked out") would pay a checkout and inherit §5.4's differences,
including losing in-container `git` for the workflow's own `git log` steps. As `--isolation=worktree` it is the
right choice for jobs that write and for uniform sweeps.

### 14.4 act-mcp checks the commit out itself — rejected

Checking out into the operator's workspace would destroy uncommitted work, fight the operator's editor and any
other agent on the box, and make the tool's own `commit` claim depend on nobody else touching the tree in the
interim. This is the one option strictly worse than the gate.

### 14.5 "Fix" the worktree's git with `--container-options -v <git-common-dir>:…:ro` — rejected

It would be elegant, and it does not survive F15: a job that declares `container:` replaces act's container
options with its own, so the mount we would be depending on silently disappears for exactly the jobs whose
workflows look most like real CI. A correctness property that can be revoked by the workflow text is not a
property. The warning of §5.4 is the honest substitute.

### 14.6 `--act-arg <flag>` passthrough — rejected

It reintroduces inside our own CLI the problem §7.6 solves for act's, and it makes `act.argv` non-replayable
(the argv would depend on a config list whose order and interactions we do not model). Operator extension
points are the user actrc plus the named flags of §7.5.

### 14.7 `O_EXCL` lockfile with pid liveness instead of `flock` — rejected

It needs a stale-lock protocol (read pid, `kill -0`, race against reuse, TTL, "who deletes the file"), and its
safety depends on pid reuse. `flock` gets the same properties with none of the state: the kernel cannot
misattribute a lock to a dead process, and there is nothing to delete.

### 14.8 In-process async mutex only — rejected

It is what ADR-0002 §6.3 assumed for the *evidence* layer, and it is insufficient here: a second act-mcp
process on the same host is a first-class supported configuration (two agents, one repository family), and an
in-process table cannot see it. The reverse is equally true — `flock` grants a second `flock` on the
*descriptor* that already holds the lock, so a handle that leaks into two run records would double-book the
workspace silently — so the mutex stays as L1 *as well as* the file lock, not instead of it (§6.1).

### 14.9 Lock the tree path instead of the workspace — rejected

Permissive in exactly the case §6.2 refuses to permit: two worktrees in one workspace still share the
machine, the daemon, the cache directories and the evidence root, and two benchmarks running side by side
measure each other. Per-workflow locking was rejected for the same reason ADR-0004 §8 rejected a queue.

### 14.10 Warn instead of gate on dirty / sparse / submodules — rejected

Every one of them makes the run measure something that is not the commit, and a warning that callers ignore is
a slower way of shipping a wrong number. `--allow-dirty-worktree` exists because that particular lie is cheap
to accept knowingly; the others are not cheap.

## 15. Reconciliations with the accepted contracts

Items 1, 2, 3, 4 and 5 are prose edits that are part of this ticket's commit, so nothing here stays a rumour;
items 6–8 are confirmations that no edit is needed. `scripts/validate_isolation.py` asserts 2, 4, 5 and 6.

1. **ADR-0002 §3.1's container path is wrong, and this ADR corrects it.** It pairs
   `host: <worktree>/.evidence/incoming/<run_id>` with `container: /srv/.evidence/incoming/<run_id>`. Under
   `--bind` there is no `/srv` translation: act binds the workdir to **the same absolute path** (F8), so a job
   told to write to `/srv/...` writes to a path that is not the mount — into the container's own filesystem,
   which vanishes, and the run would publish `no_coverage_found` forever with nothing wrong in its log. The
   corrected pair is now `host: <tree>/.evidence/incoming/<run_id>` / `container: the same string`, which is
   what §7.2 passes in `ACTMCP_EVIDENCE_OUT`. (`--container-working-directory` is why this looked plausible and
   is on the never-pass list for exactly that reason.)
2. **ADR-0002 §6.4's gitignore marker block gains one line.** The block act-mcp appends to a target workspace
   becomes:

   ```
   # act-mcp evidence (local only)
   .evidence/
   .act-mcp.lock
   ```

   Same marker, same idempotence rule (checked with `git check-ignore`), same skip-if-not-writable. This
   repository's own `.gitignore` is updated in this commit, because this repository is a fixture workspace for
   act-mcp runs (§6.2 of ADR-0002).
3. **ADR-0001 §5.3's `isolation_failed` row is narrowed to match its own "nothing was started".** The row said
   "worktree create/cleanup failed", which cannot be true after registration: ADR-0004 §18 fixes the boundary
   and §4.4 here follows it. The row now reads "the head-only policy or the tree gate refused the tree"; a
   `git worktree add` that fails mid-run is `runner_internal_error` with `detail.stage: "preparing_tree"`, and
   a cleanup failure is the warning `worktree_cleanup_failed` on an otherwise valid run (§9.3). Same for the
   mirrored row in `docs/tool-contract.md` §6 and the `meaning` string of `x-error-codes` in the schema.
4. **ADR-0001 §5.4 and `docs/tool-contract.md` §7 gain the three warnings of §9.3.** Additive on an open
   vocabulary, no schema change, and `meta.json.warnings`' pattern already admits them.
5. **`internal_error`'s `detail` gains `reason`, `path`, `os_error`** for the lock-unavailable case (§6.3).
   `$defs/error.detail` states explicitly that keys beyond the ones `x-error-codes` lists are allowed, so this
   is documentation rather than a change — the `detail` string of `x-error-codes.internal_error` now spells the
   alternative shape, and no subschema was touched. The code, its `isError: true` and its `retryable: true`
   are untouched.
6. **ADR-0004 §8's four promises are kept**: the lock is a file lock whose liveness is the owning process
   (§6.5, `flock`), it is held from registration to step 6 (§6.3), `workspace_busy` is resolvable by killing
   the holder named in `detail` (the body supplies that identity across processes), and no second lock is
   added in the evidence layer. `phase: preparing_tree` is a visible phase here (§5.5), which ADR-0004 §3.6
   left to this ADR.
7. **ADR-0002 §1.1's enumeration of files under the evidence root is untouched.** This ADR creates no file
   there: the secret file and the inert file live under `--run-scratch`, and the worktree never goes near the
   evidence root (§5.3). The §9.2 sweep only *deletes* `incoming/` debris, which ADR-0002 §3.4 explicitly
   permits ("may vanish at any time").
8. **ADR-0001 §2.9's ordering and §2.5's commit rules are implemented, not amended**: §3 is step 5, §4 is
   step 8, §6 is step 9, and the never-fetch rule of §2.5 is restated in §3.2 because the runner could
   otherwise reach the same conclusion by a different path.

## 16. Copy verbatim into the runner / supervisor / validator tickets

```
POLICY  --isolation head-only|hybrid(default)|worktree   (operator only; no caller argument exists)
MODE    resolved(full40) == git rev-parse HEAD -> head_only : worktree   (head-only policy: refuse)

GATE head_only (all at ADR-0001 step 8, before run_id; git -C <ws> --no-optional-locks; env scrubbed):
  G1 rev-parse --show-toplevel                       -> not_a_git_repository      (step 4, .6)
  G2 rev-parse --verify --quiet --end-of-options HEAD-> isolation_failed head_unresolvable
  G3 rev-parse --is-bare-repository == false         -> isolation_failed bare_repository
  G4 resolved(full40) == G2 output                  -> else mode worktree (D-1)
  G5 status --porcelain --untracked-files=no empty   -> isolation_failed dirty_worktree
                                                     -> warning dirty_worktree if --allow-dirty-worktree
  G6 config --bool core.sparseCheckout != true       -> isolation_failed sparse_checkout
  G7 cat-file -e '<commit>:.gitmodules' rc!=0        -> isolation_failed submodules_unsupported
  detail = {mode:"head_only", requested, head, reason}   (+path/os_error where useful)

WORKTREE worktree (W1..W3 pre-registration; create after registration, phase preparing_tree):
  W1 policy != head-only        W2 as G7        W3 mkdir/stat <run-scratch>
  create: mkdir -p <scratch>/<run>/ ; git -C <ws> worktree add --detach <tree> <full40>
          git -C <ws> symbolic-ref --quiet --short HEAD  -> --defaultbranch <branch> if rc==0
  destroy: git -C <ws> worktree remove --force <tree> || (rm -rf <tree> ; git -C <ws> worktree remove --force <tree>)
           (the second remove cleans the admin entry of a vanished tree; prune is never used)
  failures: pre-registration -> isolation_failed{mode:"worktree",reason} ; after -> runner_internal_error
            stage:"preparing_tree" ; cleanup failure -> warning worktree_cleanup_failed
  every worktree run carries warning isolation_worktree

LOCK  L1 in-process map canonical(ws) -> run_id      (re-flocking the SAME handle always succeeds; L1 is
                                                       the guard against a cloned lock handle)
      L2 open(<ws>/.act-mcp.lock, O_CREAT O_RDWR O_CLOEXEC, 0644) + flock(LOCK_EX LOCK_NB)
         EWOULDBLOCK -> workspace_busy {run_id, phase, started_at, holder_pid, holder_host}
         body = single-line JSON, rewritten on phase change, advisory, never parsed for liveness
      held registration -> ADR-0004 3.4 step 6 ; never unlinked ; no wait, no queue, no TTL
      open failure -> internal_error{reason:"workspace_lock_unavailable", path, os_error}

ARGV  act --directory <tree> --workflows <tree>/.github/workflows/<file> --job <task>
        --bind --reuse=false --rm --pull=false --rebuild --use-gitignore --json --quiet=false
        --secret-file <scratch>/<run>/secrets.env
        --var-file <scratch>/<run>/inert.conf --env-file <same> --input-file <same>
        --env ACTMCP_EVIDENCE_OUT=<tree>/.evidence/incoming/<run>
        --env ACTMCP_RUN_ID=<run> [--input k=v sorted] [--defaultbranch <branch>]
      secrets.env always contains GITHUB_TOKEN (caller's value or empty); mode 0600; outside the workspace
      cwd=<scratch>/<run>/  stdin=/dev/null  process_group(0)  env passthrough
      never: -s/--secret, --artifact-server-path, --container-working-directory, --container-options,
             --network, -P, --container-daemon-socket, positional event name, any act-arg passthrough

COMMIT  R1 ^[0-9a-fA-F]{7,40}$ -> R2 lowercase -> R3 rev-parse --verify --quiet --end-of-options '<sha>^{commit}'
        R4 cat-file -t -> commit_unknown/not_a_commit
        R5 for-each-ref prefix count >=2 -> commit_ambiguous{candidates} else commit_unknown/not_found
        NEVER fetch/pull/clone/ls-remote. O(all-objects) enumeration is refused: refs-only ambiguity.

CLEANUP  containers: act's job (--rm); after SIGKILL warn orphaned_containers (ADR-0004 5.5)
         worktree + scratch + secrets: ours, destroyed in the terminal commit path for EVERY terminal state
         published evidence and meta.json: never deleted (ADR-0002 6.2)
         startup sweep: wipe <run-scratch>/* (worktree remove --force each), drop incoming/* with no meta.json
```

## 17. Handoffs

| owed by this ADR to | what |
|---|---|
| mact-f1w.5 | the §11 flags with these exact defaults (one table with ADR-0004 §11), and `--isolation`/`--act-workspace-config-files` as value-enums that abort startup on a typo |
| mact-f1w.6 | §3 (its step 5) and §4 (its step 8) as written, including the scrubbed git environment of §7.8 and the `--minimum-act-version` floor of §7.7 |
| mact-f1w.8 | §7 as the only argv builder, §5.1's create/destroy commands, §9's matrix, T5/T7/T10, and the `--git-timeout-secs` kill on the git process group |
| mact-f1w.11 | §6 including L1 (do not skip it), the lock body's schema, `workspace_busy` detail assembly across both layers, and the §9.2 sweep next to its §9.3 startup log |
| mact-f1w.13 | §8.4 (host network reaches the server; docker socket mounted by default; everything in the workspace is job-readable), T9, and §7.7's `GITHUB_TOKEN` rule as a security decision, not a convenience |
| mact-f1w.15 | §7.4 as the golden argv (both modes), T1, T8 |
| mact-f1w.18 | runbook: the sweeps, "why did the run after a writing job fail with `dirty_worktree`", the operator actrc surface (and its array-flag accumulation), `-P` digest pinning for reproducible benchmarks, `--allow-dirty-worktree`, and the `orphaned_containers` recipe it already owes .19 |
| mact-f1w.19 | `V1`–`V3` of §1.3, §12.3, and the measured cost of `git worktree add` on a real monorepo |
