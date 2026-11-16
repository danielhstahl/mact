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
