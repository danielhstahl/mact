# `.fathom/` — the on-disk contract

This directory is Fathom's machine-local **metric cache**: the per-commit rows and the
content-addressed analysis results that `fathom tui` and `fathom report` read. It is safe to delete:
`fathom index` rebuilds every byte of it from `git log`, from `.evidence/` and from `fathom.toml`,
and nothing else on this machine depends on it — which is why nothing worth keeping (pane sizes,
filters, preferences) lives here. Everything in it is generated, nothing in it is hand-edited, and
everything in it except this file is gitignored. The full contract — how the root is resolved, what
`fathom.toml` may contain, how a cache key is derived, what happens on a version mismatch, who holds
`fathom.lock`, and what to delete when anything is wrong — is
[ADR-0006](../docs/adr/0006-fathom-workspace-layout.md); `scripts/validate_fathom_layout.py` fails
the build when this file, that ADR and `.gitignore` stop describing the same tree.

```
<cache root>/
├── LAYOUT.md                     # the one committed file: this contract, written if absent
├── fathom.lock                   # flock(2) for this cache root; advisory JSON body; never unlinked
└── index/                        # the store: delete this directory, `fathom index` rebuilds it
    ├── meta.json                 # layout_version, schema_version, key_schema, writer, digests
    ├── run.json                  # counters of the last and previous index run (`index --status`)
    ├── sources.json              # evidence-file identity ledger (path → size, mtime, sha256)
    ├── rows/<xx>/<commit>.json   # one row per commit: history+diffstat, evidence metrics, MI
    └── blocks/<xx>/<key>.json    # content-addressed analyzer results — the metric cache
```

Two rules hold whatever else changes. **Every file here is regenerable** — when in doubt, delete
`index/` and re-index. And **Fathom writes only the paths above**: anything else in this directory
belongs to another program, and Fathom neither reads it nor deletes it.

## Reading it, rebuilding it, and what to ask when it is wrong

Three commands, and one of them is the one to reach for first.

| command | what it does to this directory |
|---|---|
| `fathom doctor` | reads it and explains it: absent, yours, from an older build, damaged, held by another run. Writes nothing — start here |
| `fathom index` | the only command that writes here at all; builds or updates `index/` incrementally |
| `fathom index --rebuild` | deletes `index/` and builds it again. Nothing else under this root is touched, and `rm -rf index/` is the same act with more typing |

`fathom tui`, `fathom report` and `fathom doctor` are readers: they never write anything except
`LAYOUT.md` (written if absent, never overwritten) and `fathom.lock`. One writer at a time is the rule
([ADR-0006 §9](../docs/adr/0006-fathom-workspace-layout.md)), and `fathom.lock` is how a second Fathom
finds out: it fails fast, says who holds the root, and does not wait.

* **A store from an older build is refused, not migrated.** A layout or schema version this build does
  not know exits with a `fix:` line rather than guessing
  ([ADR-0006 §8](../docs/adr/0006-fathom-workspace-layout.md)); `fathom index --rebuild` is the answer,
  and it costs re-analysis and nothing else.
* **The row and block *keys* are content-addressed** (`fk1` over the blob, the path, the analyzer and
  its version, and the config digest — [ADR-0006 §6.3](../docs/adr/0006-fathom-workspace-layout.md)),
  so a commit that did not change is not re-analysed, and an edit to `fathom.toml` invalidates exactly
  the rows and blocks that read the key it changed — no more.
* **What a number means is not in here.** The store holds derived values, not their definitions:
  [docs/fathom-metrics.md](../docs/fathom-metrics.md) is the user-facing statement of the MI formula,
  the coverage derivation, the benchmark grouping and the action states, and
  [ADR-0007](../docs/adr/0007-maintainability-index.md) /
  [ADR-0008](../docs/adr/0008-per-commit-metric-model.md) are the normative text. A row is an input to
  those rules, and every one of them is re-derivable from the files that produced it:
  `.evidence/` ([ADR-0002](../docs/adr/0002-evidence-jsonl-contract.md), written by act-mcp), `git log`,
  and `fathom.toml`.
