---
id: ADR-0007
title: the Maintainability Index — formula, input rules, per-commit aggregation, and a pure-Rust front end
status: accepted
ticket: mact-by1.3
date: 2026-11-16
---

# ADR-0007: the Maintainability Index — what is measured, in what units, and by which front end

- **Status:** Accepted
- **Date:** 2026-11-16
- **Ticket:** mact-by1.3
- **Decision:** **The original (1996) MI, with every input defined to the level where two
  implementations must agree.** `MI_raw = 171 - 5.2*ln(HalsteadVolume) - 0.23*CyclomaticComplexity -
  16.2*ln(LogicalLinesOfCode)`, and `MI = clamp(MI_raw * 100 / 171, 0, 100)`, stored at one decimal.
  Halstead volume is `V = N * log2(n)` over the **operator** tokens only, with the platform `log2`
  (never `ln(n)/ln(2)`); `G = 1 + decision points` counted over the whole file from a closed,
  per-language enumeration; LOC is **logical lines** — statement counting in Rust
  (`LOC = D + E + A + B`), logical-line counting in Python (`LOC = L + H`) — so a `cargo fmt` commit
  moves the metric by exactly zero. A file that cannot be measured has **no MI**: `ln(0)` is
  undefined, and an unmeasurable file renders unknown with a named reason (§3.6); it is never 0, because
  0 means "worst file in the repository" and would be a lie. The unit of analysis is a **function-like
  definition** (amended by mact-by1.10 from the file, which §4.1 explains); a file's MI is the
  **LOC-weighted mean of its blocks**, and a per-commit number is an aggregate, not a measurement — the
  **LOC-weighted mean** of the file MIs, plus `delta_vs_parent` against the first parent, plus the
  watchlist transitions (`sub_threshold_added` / `sub_threshold_removed`) and worst-N hotspots. Bands
  are declared here and nowhere else: **green ≥ 50, amber ≥ 38, red < 38**, an exact edge belonging to the
  better band,
  neither stored nor configurable. The front end is a **hand-written, pure-Rust per-language
  tokeniser** (`lexer1`) over one shared model — tree-sitter is rejected because it is C, which is
  the third time this repository has refused a C build dependency for the same five cross-compilation
  targets. v1 speaks **Rust and Python**; every other path renders unknown and is counted in
  `unanalysed_paths`, so the view can say "these paths were not analysed: N".
- **Reconciled, not inherited:** the ticket suggested bands of "< 85 amber, < 65 red". They cannot be
  used with the formula the same ticket pins — those edges belong to the *updated* MI, which
  estimates Halstead volume from LOC instead of measuring it — and §5.2 shows in arithmetic what this
  formula actually produces: this ADR's own hand-counted 16-statement fixture scores **55.9**, and a
  1000-line file scores **0**. Bands at 85/65 would paint an entire real repository red, and a band
  everything fails carries no signal. §5.2 is the argument, §5.1 the numbers, and `revisit_when`
  makes mact-by1.7's first corpus histogram confirm or amend them in one commit.
- **Rejected:** the updated MI (volume estimated from LOC); per-function MI with a per-file
  aggregation (a function boundary in Rust is closures, `async` blocks and `impl` items — a second
  enumeration problem to own, and §5.2 shows it would not fix the band scale either); tree-sitter plus
  grammar crates; `syn` for Rust and a hand lexer for Python (two mechanisms, so "proven general"
  would be a claim about one language); a language-agnostic comment+token approximation for decision
  points; storing the unrounded double; carrying the aggregate as a running sum over history; making
  the bands configuration; the operand-based Halstead variant. Each with its reason in
  [Considered alternatives](#11-considered-alternatives).
- **Machine-readable form:** three artifacts. [`schema/mi-definition.json`](../../schema/mi-definition.json)
  is the one normative spelling of every constant, every closed enumeration (per-language operator
  keywords, ignored punctuation, decision points, statement rules, ambiguities) and every reason code;
  [`scripts/mi_reference.py`](../../scripts/mi_reference.py) is the reference implementation of the
  arithmetic — *the oracle* mact-by1.7's Rust port is checked against, and it loads the JSON rather
  than duplicating it; and [`tests/testdata/mi/`](../../crates/act-mcp/tests/testdata/mi) holds the golden vectors:
  fixture sources whose token tables, V, G, LOC, MI and band were counted by hand and are re-derived
  on every CI run. The ADR carries its own copy of the numbers twice more, in the two blocks below,
  and the checker diffs all four copies.
- **Checked by:** [`scripts/validate_mi_definition.py`](../../scripts/validate_mi_definition.py)
  (C1–C13, run in `scripts/ci.sh`'s `schema` step beside the other ADR checkers; `--self-test` builds
  a scratch fixture out of the real artifacts, breaks one rule at a time, and asserts the check it
  names is the one that fires). C7 is the interesting one: it recomputes every golden vector a second
  time in 40-digit `decimal` arithmetic and requires the stored digit to come out the same, so the
  binary64 pipeline and an independent implementation of the formula agree — ADR-0002 §7's
  "two engines, one contract" rule applied to arithmetic instead of to a schema. The front-end
  decision is additionally a supply-chain fact: `deny.toml` bans `tree-sitter` and its grammars, the
  same mechanism ADR-0006 §5 used for sqlite.
- **Related:** [ADR-0005](0005-fathom-crate-layout-and-dependencies.md) (the dependency budget §6 has
  to live inside, and the "not decided here" line that handed the tree-sitter question to this ADR),
  [ADR-0006](0006-fathom-workspace-layout.md) (§6.3's `fk1` block keys and `input_kind` vocabulary,
  which is where this ADR's `(analyzer id, version, config digest, input kind, input hash)` lives;
  §7.2's key table, which this ADR extends by exactly one key; the row `mi` block whose reason codes
  and field names these are; §5's refusal of a C toolchain, which §6 re-uses verbatim),
  [ADR-0002](0002-evidence-jsonl-contract.md) (§2.1 unknown-is-absent-never-zero, and §7's
  two-implementation rule C7 applies to arithmetic),
  [ADR-0003](0003-commit-isolation-and-serialization.md) (the no-libgit2 precedent, first of the two
  §6 stands on).
- **Consumers inside the epic:** mact-by1.7 (the analyzer: §2–§9 in full, and
  `scripts/mi_reference.py` is its test oracle), mact-by1.8 (the diffstat: it must reuse the §7
  classifier and `paths.include_tests` so the `+`/`-` column and the MI column describe the same set
  of files), mact-by1.6 (which supplies blobs at a commit), mact-by1.9/.11/.12/.14 (the view: §5's
  bands and §4.4's caveat counts are what it renders, and it may not invent either), mact-by1.10 (the
  store: §8's digests are block keys), mact-by1.13 (`fathom doctor`: the vectors are its fixture
  corpus, and §5's `revisit_when` is its follow-up), mact-by1.15 (the runbook: links this ADR, does
  not restate it), mact-by1.16 (cargo-deny: the bans in §6.4 land there).
- **Explicitly not this ADR's:** what a coverage percentage or a benchmark value *means* (ADR-0008),
  which commits are walked and how rows are stored (ADR-0006), whether MI ever drives a gate or a
  ratchet (nothing in this epic gates on it; MI is a trend line, and §5.2 explains why it would be a
  bad gate), and any claim that MI measures *quality* — it measures size, coupling to vocabulary,
  and branch density, in one number, and §5.2 says so plainly.

## 0. The decision, in one screen

```
MI_raw = 171 - 5.2*ln(V) - 0.23*G - 16.2*ln(LOC)
MI     = clamp(MI_raw * 100 / 171, 0, 100)      then MI_1 = round_half_away(MI*10)/10

V   = N * log2(n)      operators only; N occurrences, n distinct
G   = 1 + decision points, over the whole file
LOC = logical lines (statements), never physical lines, never screen lines
```

Numbered pins. `MUST` / `MUST NOT` / `SHOULD` are normative; each is either enforced by
`scripts/validate_mi_definition.py` (the check is named) or is named as a judgement call.

| # | pin | check |
|---|---|---|
| P1 | The formula and its constants are those in §2.1, and they are constants: they live in `schema/mi-definition.json`, are copied into this ADR, and a change to one is a `mi.score` version bump in the same commit. | C2 |
| P2 | Every operation is an IEEE-754 binary64 operation; `ln` is the platform natural log and `log2` the platform `log2`. The **stored and compared** value is the rounded one (`MI_1`, one decimal, half away from zero); the unrounded double is never persisted and never compared. | C2, C7 |
| P3 | Halstead volume is measured over operator tokens, keyed by source text; a string/char/number literal is one operand and its contents are never tokenised. | C5, C6 |
| P4 | `G = 1 + decision points`, one per occurrence of an enumerated decision point, from the closed per-language lists in the JSON; `&&`/`||`/`and`/`or` are decision points only by the preceding-token rule. | C5, C6 |
| P5 | LOC is logical: Rust `D + E + A + B`, Python `L + H`. Comments, blanks, `{-`only lines, attributes and doc comments (Rust) cost zero; a multi-line call costs one; a macro's token tree costs nothing. | C5, C6 |
| P6 | A file with `LOC == 0` or Halstead vocabulary `n < 2` has **no** MI. The reason is recorded (`vacuous`), the value is absent, and it is never 0. | C4, C6 |
| P7 | Reasons are the seven of §3.6, evaluated in that order; the four that mean "not analysed" are exactly the ones in `unanalysed_paths`, and the names are the ones ADR-0006's row `counts` already uses. | C4 |
| P8 | The unit of analysis is a **function-like definition** (§4.1): `V`, `G` and LOC are counted over one block and a file's MI is the LOC-weighted mean of its blocks, the kinds that make a block being closed per language in `scoring_unit.kinds`. A blob with no measurable block is its own unit and stores which rule made its number. A per-commit number is an aggregate: the LOC-weighted mean over the files that produced a measurement, accumulated in path order, and it is a **pure function of that commit's own file set** — no carry-forward over history. | C8, C9, C2 |
| P9 | The commit row also carries `loc`, `files`, `counts`, `delta_vs_parent` (against the first parent, absent for a root commit), worst-N `hotspots` (worst-first, `loc` then path as tie-breaks), the watchlist transitions, and `unanalysed_paths`. | C8, C9 |
| P10 | Bands are §5.1's, declared here, not stored in the cache, not configuration; an exact edge belongs to the better band. | C3 |
| P11 | The MI file set is the diffstat's file set: ADR-0006 §7.2's `paths.source` / `paths.noise` / `paths.exclude`, plus the extension map of §7.1 and one new key, `paths.include_tests` (default `false`, opt-in). | C11 |
| P12 | The front end is `lexer1`: pure Rust, hand-written per language, no C and no parser library. `tree-sitter` and its grammars are banned in `deny.toml`. | C10 |
| P13 | Every analyzer result carries `(id, version, config_digest)` and is keyed by ADR-0006 §6.3's `fk1` derivation with `input_kind = blob_path`; a formula tweak invalidates exactly the MI blocks and nothing else. | C2, C11 |
| P14 | The golden vectors of §9 exist, are hand-counted, are re-derived by two independent implementations of the arithmetic, and none of them may sit within `1e-6` of a rounding or band boundary. | C6, C7 |

## 1. Context

"Code complexity (measured via the Maintainability Index)" is the one column this project has never
measured, and it is the one column that will be read as an opinion about somebody's code. ADR-0006
line 98 hands the definition here; ADR-0006 §6.3 says a value that varies with a file's path must be
keyed by path, and leaves MI's exact line to this ADR. Everything below was decided because a
different tool would decide it differently, and two implementations that decide it differently produce
two different numbers for the same commit — one of which is in the cache.

Facts, all measured for this ticket rather than remembered:

- The formula is from Coleman, DeMers, Ramani & Sweet (IEEE Software 13(5), 1996), scaled by
  Microsoft's `100/171`. It is three subtractions from 171; there is no fitted constant in it and
  nothing to tune, which is why pinning it is a matter of definitions rather than of calibration.
- **The unit decides the scale, and the ticket pins the unit to the file.** `16.2*ln(LOC)` is the
  dominant term: at LOC 16 it is 45 points, at LOC 100 it is 74.6, at LOC 1000 it is 111.8. Adding
  `5.2*ln(V)` on a *measured* volume (Halstead volume grows superlinearly with size) means a
  1000-line file has no MI_raw left: §5.2 computes one at −39, clamped to 0. Tools that quote "MI is
  0–100, keep it above 20" are quoting either the updated MI or a per-*method* unit. This ADR keeps
  the ticket's per-file unit, and §5.2 states out loud what that means for reading the number.
- Real examples, hand-counted for this ticket (they are the golden vectors of §9, and the
  hand-arithmetic was re-checked in 40-digit decimal): a 16-statement Rust file with a `match`, a
  guard, a `for`, a `while` and two short-circuit operators measures **V 227.9, G 10, LOC 16 → MI
  55.9**; a one-line function measures **95.1**; a 36-line Python module with a `match`, a
  comprehension and a `try` measures **45.3**.
- The repo has **zero C build dependencies** and has said so twice: ADR-0003 refused libgit2 and
  ADR-0006 refused `rusqlite`/`libsqlite3-sys`, both for the same reason `cd.yml` gives — five release
  targets built from one runner, two of them musl and one aarch64-apple-darwin, so `cc` is not just a
  compiler, it is a cross toolchain per target. ADR-0005's dependency budget is where the
  tree-sitter question was parked; §6 answers it the same way.
- ADR-0006 §7.2 already names the two MI analyzers (`mi.tokenize`, `mi.score`) and the row `mi`
  block already names six counts (`ok`, `skipped_noise`, `unsupported_language`, `parse_error`,
  `vacuous`, `oversized`). This ADR adopts those names verbatim rather than inventing a second
  vocabulary — C4 fails if the two drift.

## 2. The formula

### 2.1 The pinned formula

```
MI_raw = 171 - 5.2*ln(HalsteadVolume) - 0.23*CyclomaticComplexity - 16.2*ln(LogicalLinesOfCode)
MI     = clamp(MI_raw * 100 / 171, 0, 100)
MI_stored = round_half_away_from_zero(MI * 10.0) / 10.0
```

with, from [`schema/mi-definition.json`](../../schema/mi-definition.json) — the normative copy, which
this ADR's copy below is diffed against field by field:

<!-- mi-constants:begin -->
```json
{
  "definition_version": 2,
  "analyzer": {
    "frontend_id": "lexer1",
    "tokenize": {
      "id": "mi.tokenize",
      "version": 1,
      "input_kind": "blob_path"
    },
    "score": {
      "id": "mi.score",
      "version": 2,
      "input_kind": "blob_path"
    },
    "bump_rules": {
      "tokenize": "any change to a token stream, a keyword/punctuation enumeration, a decision-point rule, a statement rule, or a lexer bug fix that moves a count",
      "score": "any change to formula, rounding, clamping, band edges, measurability thresholds, the scoring unit or aggregation arithmetic",
      "score_v2": "v2 is the scoring unit: MI is computed per function-definition block and a file's MI is the LOC-weighted mean of its blocks (section 4.1), where v1 scored the whole file as one unit. The token stream itself did not change, so `mi.tokenize` stays 1 in this document; note that the store stamps both MI analyzers with one version (ADR-0007 section 8), so a warm store re-analyses every blob in scope on this commit, which is what a change of unit needs anyway."
    }
  },
  "formula": {
    "reference": "Coleman, DeMers, Ramani & Sweet, \"Integrating software metrics into a commercial software development environment\", IEEE Software 13(5), 1996 — the original MI, scaled by the 100/171 Microsoft published with it so the score is 0-100. This is the formula ADR-0006 line 98 points here for.",
    "halstead_volume": "V = N * log2(n)",
    "cyclomatic": "G = 1 + decision points, counted over the scoring unit — one function (section 4.1); a file's own whole-file G is reported alongside as a diagnostic and is not what its MI is made of",
    "raw": "MI_raw = 171 - 5.2*ln(V) - 0.23*G - 16.2*ln(LOC)",
    "scaled": "MI = clamp(MI_raw * 100 / 171, 0, 100)",
    "a": 171.0,
    "b_volume": 5.2,
    "c_cyclomatic": 0.23,
    "d_loc": 16.2,
    "scale_numerator": 100.0,
    "scale_denominator": 171.0,
    "clamp_min": 0.0,
    "clamp_max": 100.0
  },
  "scoring_unit": {
    "unit": "function",
    "weight": "loc",
    "formula": "MI_file = MI_1( sum(MI_b * LOC_b) / sum(LOC_b) ), b over the blocks of one file that produced a measurement, accumulated in source order",
    "fallback": "file",
    "kinds": {
      "rust": [
        "fn"
      ],
      "python": [
        "def",
        "async def"
      ]
    }
  },
  "precision": {
    "storage_decimals": 1,
    "rounding": "round_half_away_from_zero",
    "ln": "platform natural log: Rust f64::ln, Python math.log",
    "log2": "platform log2: Rust f64::log2, Python math.log2. NOT ln(n)/ln(2).",
    "arithmetic": "ieee754-binary64-every-operation",
    "boundary_slack": 1e-06,
    "volume_basis": "operators"
  },
  "measurability": {
    "min_loc": 1,
    "min_operator_unique": 2,
    "ordered_reasons": [
      "excluded",
      "skipped_noise",
      "unsupported_language",
      "oversized",
      "parse_error",
      "vacuous",
      "ok"
    ]
  },
  "aggregation": {
    "weight": "loc",
    "formula": "MI_agg = MI_1( sum(MI_i * LOC_i) / sum(LOC_i) ), i over status == ok",
    "hotspots_default": 5,
    "hotspot_sort": [
      "mi ascending",
      "loc descending",
      "path ascending"
    ],
    "accumulation_order": "path bytes ascending",
    "sub_threshold_set": {
      "name": "watchlist",
      "below": "bands.names_by_threshold.red_below",
      "note": "the set whose membership the row reports is exactly the red band, named by reference so the band edge cannot be spelled twice. The ticket asked for 'files added/removed from the sub-threshold set'; membership changes of a set the size of the amber band (below 65 here) is a list that churns on every commit and says nothing, so the reported set is the red one and the amber population is reported as a count."
    },
    "parent": "git parent #1 (first parent), whatever the walk mode",
    "delta_rule": "delta = MI_1(aggregate(C) - aggregate(parent)); absent when the commit has no parent",
    "recompute_rule": "the aggregate is a pure function of the commit's own file set; no carry-forward recurrence over history is allowed, because float addition order would then decide the stored digit and `doctor` could not re-derive it",
    "empty_population": "aggregate is unknown (gaps.no_mi = true), never 0"
  },
  "scope": {
    "source_extensions": {
      ".rs": "rust",
      ".py": "python"
    },
    "precedence": "exclude > noise > (source AND an enabled language)",
    "binary_rule": "a blob containing a NUL byte before the first newline, or that is not valid UTF-8, is `parse_error` if its extension is in scope, `excluded` otherwise"
  }
}
```
<!-- mi-constants:end -->

Consequences of the constants worth knowing before reading the input definitions:

- The **reachable range is (0, 99.9]**, not [0, 100]. `V ≥ 1` and `LOC ≥ 1` make every subtracted
  term non-negative and `0.23*G` is at least 0.23, so `MI_raw < 171`. `clamp_max` is defensive: it is
  what a file that leaked past the `vacuous` rule would land on instead of an impossible 100+.
- `ln(1) = 0`, so a one-line file loses the LOC term entirely (that is `rust_tiny`, 95.1).
- `ln(0)` is undefined, so `LOC == 0` has no MI at all (P6), and `log2(1) = 0` / `log2(0)`
  undefined is why a vocabulary below 2 has no MI either.

### 2.2 Arithmetic, rounding, and where two implementations could disagree

Two implementations must agree *bit for bit* where a number is stored, and the honest version of that
claim is: floating point has one place where implementations legitimately differ (`ln`), so the
contract is written so that the difference cannot reach a stored digit.

1. **Every operation is a binary64 operation.** No `f32`, no fixed point, no long double, no
   decimal library, in any implementation, including this ADR's Python oracle.
2. **`log2` is the platform `log2`** (`f64::log2`, `math.log2`) — *not* `ln(n)/ln(2)`. The two differ
   by ULPs, and pinning which one is used is what makes `V` reproducible. `V` is then *stored* at full
   round-trip precision, so `ln(V)` downstream is computed from the same double in every implementation.
3. **The stored value is the rounded one.** `MI_1(x) = round_half_away(x * 10.0) / 10.0` — in Rust
   `(x * 10.0).round() / 10.0` (`f64::round` rounds half away from zero), in Python
   `copysign(floor(abs(x*10.0) + 0.5), x*10.0) / 10.0`. Rounding is applied to the product, which is
   the whole definition: `62.449999999999996` and `62.450000000000003` both become `62.5`, because
   both multiply to exactly `624.5`. A decimal-string intuition ("that rounds down") would be wrong
   here, and a port that used a decimal library would disagree with this one — so neither is allowed.
4. **Comparisons use the stored value.** Bands, the watchlist, the hotspot sort and the delta all
   read the rounded number. The aggregate is therefore reproducible from the cache alone, and a
   hotspot at 49.95 cannot be amber in one view and red in another.
5. **`MI_raw` is a diagnostic**, kept in the vectors and compared to a relative tolerance of `1e-9`
   (that is the room a different `libm` gets). The stored `MI` is compared with zero tolerance.
6. **Hygiene:** no golden vector may sit within `1e-6` of a rounding boundary or a band edge (P14,
   enforced). A fixture on a boundary is a fixture whose value depends on the host's math library.
7. **JSON encoding is pinned:** `V` at shortest round-trip `repr` (no trailing-zero padding), `MI` as
   the rounded double rendered with at most one decimal, so a re-read parses to the same double.

A second implementation of the whole arithmetic — `decimal`, 40 digits, `ROUND_HALF_UP` — runs in CI
over every vector (C7) and must produce the same stored digit. That is what "reconciled, not
inherited" looks like for arithmetic: not a claim of agreement, but a check that would notice.

### 2.3 Worked example (the hand-check)

`rust_basic.rs` (§9): operators `N = 46`, distinct operators `n = 31`, decision points 9, LOC 16.

```
V      = 46 * log2(31)          = 227.89303027779624
G      = 1 + 2 + 3 + 1 + 1 + 1 + 1 = 10        (2 if, 3 match arms, ||, &&, for, while)
MI_raw = 171 - 5.2*ln(227.893) - 0.23*10 - 16.2*ln(16)
       = 171 - 27.5940 - 2.3 - 45.1521 = 95.55390566196463
MI     = 95.55390566196463 * 100 / 171 = 55.87947699530096  ->  MI_1 = 55.9  (green)
```

The same arithmetic in 40-digit decimal gives `55.8794769953009630300600345582`, which rounds to the
same stored digit; the binary64 chain gives `55.9`. Every golden vector carries its own version of
this block, and `scripts/mi_reference.py --vector tests/testdata/mi/vectors/*.json` re-derives them
in milliseconds.

## 3. The three inputs

### 3.1 Halstead volume: `V = N * log2(n)` over operators

A file is tokenised into a stream of **operator**, **operand** and **ignored** tokens (§3.2's lexical
rules produce it); `N` is the count of operator occurrences and `n` the number of *distinct* operator
texts; `V = N * log2(n)`.

- **Operators only.** The operand-based variant (N and n over both classes) is the alternative that
  moves the number least — the two definitions are correlated almost perfectly on real code — and it
  is refused for comparability with the 1996 paper and every MI tool a reader may have used before.
  The choice is a key in the JSON (`precision.volume_basis`) so the decision is visible, not folklore.
- **Distinctness is byte-for-byte source text.** No case folding, no Unicode normalisation, no
  stripping of Rust's `r#` raw-identifier prefix. A number keeps the form it was written in, so `1`,
  `1.0`, `1_000` and `0x10` are four operands. A string, bytes or char literal is keyed by its source
  text *including* the quotes and prefix, and its escapes are never interpreted: `'x'` is not the
  operand `x`, and `r#"a"#` is not `"a"`. Keeping the quotes is what stops the identifier `a` and the
  string `"a"` collapsing into one operand — the trap in every hand-rolled Halstead counter, and the
  reason the `rust_generics` fixture hides a `(` inside a raw string.
- **A literal's contents are never tokenised.** A doc comment that contains `if a && b` cannot move a
  metric; an f-string that contains `{[x for x in y if x]}` cannot move one either.

### 3.2 What is an operator, what is an operand, per language

The classification is **lexical and ordered**: no implementation may need types, a symbol table, or a
parse. Each language's closed lists live in the JSON (`operator_keywords`, `operand_keywords`,
`operator_punctuation`, `ignored_punctuation`, `name_rules_ordered`, `ambiguities`), and the rules
that were judgement calls are recorded here, because these are the places tools quietly differ:

1. **Reserved keywords are operators; keywords that denote a value are operands.** In Rust that means
   `self`, `Self`, `super`, `crate`, `true`, `false`, `_` are operands (they name things, they do not
   operate), and primitive type names (`u8`, `str`, `bool`) are operands because they are ordinary
   identifiers to the grammar. Contextual keywords (`dyn`-style is reserved, but `union`, `macro`,
   `catch`, `default`, `raw`, `safe`, `async`-as-block) are **operands**: classifying them as
   operators would require knowing the position, which is exactly the parse this rule exists to avoid.
   In Python, `None`/`True`/`False`/`self`/`cls` are operands and the soft keywords `match`/`case`
   are keywords only in statement position (`case = 1` inside a function is an assignment).
2. **Definition names are operands; call-shaped names are operators.** A name immediately after `fn`,
   `struct`, `enum`, `trait`, `impl`, `mod`, `type`, `const`, `static`, `macro_rules!`, `def` or
   `class` names the thing being defined and is an operand. Otherwise a name immediately followed by
   `(` — or by `!` in Rust, the macro head — is an operator. Everything else is an operand. Two
   consequences are deliberate and pinned: a **call-shaped match pattern** (`Some(rest)`) counts
   `Some` as an operator exactly as the expression does, because the test is lexical and the
   pattern/expression distinction needs a parse; and a **turbofished call** `foo::<T>(x)` counts
   `foo` as an *operand*, because the token after it is `::`. Both look arbitrary and both are the
   price of a rule every implementation can apply identically; `rust_question.rs` pins them.
3. **Punctuation is split into operators and separators.** Operators: arithmetic, comparison, logical,
   assignment (including Python's `:=`, and `=` in a keyword argument), `?` in Rust, `..`/`..=` in
   Rust, and Python's `**`/`//`. Ignored: brackets and braces, `,` `;` `:` `::` `.` `->` `=>` `@` `#`
   — structural noise that adds a constant to every file and says nothing. `=>` is ignored as a
   *token* precisely because the match **arm** is the decision point (§3.3), so an arm is not counted
   twice. `...` (Python's Ellipsis) is an *operand* — it denotes a value — while Rust's `..` is an
   operator: the value test and the notation test disagree here and the value test wins, in both
   directions, and both are written down.
4. **Operators are lexed by maximal munch**, exactly as a compiler lexes them: `Vec<Vec<u8>>` yields
   one `>>`, not two `>`. Both readings are operator tokens, so only `n` could differ — which is
   exactly the kind of ULP-level difference this ADR is pinning, so it is pinned, with a fixture.
5. **Ignored token classes**: whitespace, line/block/doc comments, Rust attributes (a `#` or `#!`
   followed by `[` swallows the whole bracketed form and emits nothing), Rust lifetimes and loop
   labels (`'a`, `'a:`), Python's line-leading decorator marker `@` (its expression is still
   tokenised), shebangs and encoding lines. A Rust char literal `'x'` is an operand and a lifetime
   `'a` is ignored; the rule is a quote closed by a matching quote on the same line.
6. **A macro invocation** is its name (operator), its `!` (operator), and its token tree tokenised by
   the same rules recursively — `vec!["a", "b"]` costs `vec`, `!`, `"a"`, `"b"`. A `format!`/
   `println!` format string is one operand token whose `{}` slots are never tokenised; a Rust
   attribute's contents and a Python f-string's `{}` slots are the same kind of nothing.

### 3.3 Cyclomatic complexity: `G = 1 + decision points`, over the scoring unit

The base is 1 and the ticket states it, so the scoring unit is treated as one entry point rather than
as a set of disconnected graphs (§4.1 fixes what the unit is; before mact-by1.10 it was the whole
file, and `G` was therefore the file's). The alternative (§11.4) — sum one graph per function — would
add `(#functions − 1)` to every `G`, which at weight 0.23 moves MI by ~1.2 points for a ten-function
file; it is refused because it needs a definition of "function" (closures? `async` blocks? `impl`
items? module-level statements in Python?) and that is a second closed enumeration nobody asked for.
Counting `G` per function would need the same enumeration, and §4.1 explains why it is worth paying
for once — with the answer being the front end's own block kinds, which are closed per language and
already enumerated for Halstead.

A decision point is one occurrence of an enumerated rule. The full lists live in the JSON
(per-language `decision_points.rules`); the rules
that are actually arguable are these:

| construct | Rust | Python |
|---|---|---|
| `if`, `else if`, `if let`, ternary `a if b else c` | 1 per `if` | 1 per `if` (including a comprehension's `if` and a ternary's) |
| `elif` | — | 1 (it is a separate keyword and a separate fork) |
| `else` | **0** — the nested `if` already counted | **0**, including `for/else` and `try/finally` |
| `match` / `case` | **1 per arm** (an arm with a guard costs 2: the arm plus the guard's `if`) | 1 per `case` clause (a guarded `case` costs 2) |
| `switch`-like `while`/`loop`/`for` | 1 each; `while let` counts once | 1 per `for`/`while`; a comprehension's `for` counts **0** |
| `&&`, `\|\|` | 1 each **when it follows a value** | `and`/`or`: same rule, same predicate |
| `?` | 1 per occurrence | — (Python's equivalent is `raise`, which is straight-line) |
| `let ... = .. else { .. }` | 1 (two exits) | — |
| `try`/`except` | — | 1 per `except` clause; the `try` itself is 0 |
| `return`, `break`, `continue`, `panic!`, `assert!`, `with`, `lambda`, `yield`, `not` | 0 | 0 |

Three of those rows are the ones the ticket called out:

- **Early-return guards cost exactly one.** A guard clause is an `if` and a `?` is a `?`; there is no
  per-function control-flow graph, so `return` itself is never a decision point and a function written
  as five guards costs 5, the same as five nested blocks. That is deliberate: the two spellings have
  the same number of forks, and a metric that punishes the flat, readable spelling would be a metric
  with a style opinion in it.
- **The `||`/`&&` problem is solved positionally.** `unwrap_or_else(\|\| x)` is not a fork,
  `fn f(x: &&str)` is not a fork, `if a \|\| b` is. The rule: look at the nearest preceding token of
  any class except whitespace and comments — **ignored punctuation counts** — and it is a fork only if
  that token is an operand or one of `)` `]` `}`. So `key: &&str` is not a fork (the preceding token is
  `:`), `foo(1) \|\| bar` is (the preceding token is `)`). A rule about positions in the token stream,
  chosen over a rule about positions in the grammar, because the first is decidable without a parse.
- **The decision is the arm, not the keyword.** A two-arm `match` and an `if`/`else` both cost 2, which
  is why `=>` is an ignored token and `match_arm` is the rule: neither language can have it both ways.

### 3.4 Logical lines: statements, not screen lines

LOC is the number of **logical lines**, and that choice is the single most consequential one in this
ADR, so the reason is stated first: **a reformat must not move MI.** Under any physical-line counting
(including "physical lines minus comments and blanks", which is what several tools call logical),
`cargo fmt` changes LOC, so a pure-formatting commit shows a delta, so the trend line records a
maintainability change that did not happen. Statement counting is formatting-invariant by
construction, and it is what the 1996 "logical lines of code" meant.

Two rules, one per language, both expressible without an AST:

- **Rust: `LOC = D + E + A + B`.** `D` — one per introduction keyword wherever it appears (`let`,
  `use`, `const`, `static`, `type`, `struct`, `enum`, `union`, `trait`, `impl`, `mod`, `fn`, a
  `macro_rules!` definition). `E` — one per `;`-terminated statement that is not the `;` closing a
  `D`-counted introduction and not a `;` inside a macro token tree. `A` — one per match arm. `B` —
  one per control-flow block header that is followed by a block (`if`, `for`, `while`, `loop`); an
  `else` block adds nothing and `match` adds nothing, because its arms already carry the count.
- **Python: `LOC = L + H`.** `L` — one per logical line (a `\` continuation or a bracketed run is one
  line), including a bare expression statement such as a module docstring. `H` — one extra per logical
  line that opens a block, which is exactly the logical line whose last token before the newline is
  `:`, so `class`, `def`, `if`, `elif`, `else`, `for`, `while`, `try`, `except`, `finally`, `with`,
  `match` and every `case` clause cost 2 and a decorator line costs 0 (it is folded into its
  definition's header).

What this buys, and what it deliberately costs, all pinned:

- a **`{`-only line**, a closing `}` line, a `)` line and a blank line cost **0** — they are not
  statements in either language;
- a **multi-line call** costs 1 in Rust (one `;`-terminated statement) and 1 in Python (one logical
  line), so rustfmt exploding a call over eight lines changes nothing;
- **comments and doc comments cost 0** in both languages, and a Rust **attribute** costs 0 because it
  emits no tokens at all: LOC measures executable logic, not prose about it or declarations of data.
  A file that is 90% doc comments therefore measures a small LOC — that is the intended reading of
  "logical lines of code", and it is stated here because a reader will otherwise assume the opposite;
- a **struct field, an enum variant, a `where` clause and a parameter list cost 0**: declarations of
  data are not statements;
- the inside of a **macro's token tree** costs 0 while its tokens count toward Halstead: `vec![…]`
  spanning six lines is one statement, and `println!("{a} {b}")` is one statement with one string
  operand. This is the `token tree`/format-string rule the ticket asked for, and both halves of it are
  in the vectors;
- a **docstring in Python counts** (it is a statement), while Rust doc comments do not, because in
  Python it *is* an expression statement and in Rust it is lexically a comment. The asymmetry is real
  and follows from the rule rather than being added to it.

`tests/testdata/mi/src/rust_reformat_a.rs` and `rust_reformat_b.rs` are the test: different bytes,
identical token streams, identical `N`, `n`, `G`, `LOC`, `V`, `MI`. C6 fails if they ever become the
same file (which would make the pair vacuous).

### 3.5 `ln(0)` is undefined: no MI, rendered unknown, never 0

`ln(LOC)` with `LOC == 0` and `log2(n)` with `n < 1` are not numbers, and there is no conventional
substitute: clamping LOC to 1 would hand a comment-only file an MI of ~100 — the *best* score in the
repository — which is a different lie. So an unmeasurable file has no MI field anywhere: not in the
block, not in the row, not in the aggregate's numerator. The value is absent and the reason is
present, which is ADR-0002 §2.1's rule (unknown is absent, never zero) applied to a metric instead of
to a field.

### 3.6 The measurability ladder: seven reasons, in order

The ladder in one line:

```
excluded → skipped_noise → unsupported_language → oversized → parse_error → vacuous → ok
```

Reasons are evaluated top to bottom and the first one wins, so a broken file in a vendor directory is
`skipped_noise` and not `parse_error`, and a 40 MB Python file is `oversized` before it is
`parse_error`:

| order | reason | meaning | counted in `unanalysed_paths` |
|---|---|---|---|
| 1 | `excluded` | not a source file at all, or under `paths.exclude` | no |
| 2 | `skipped_noise` | matched `paths.noise`, or is a test file while `paths.include_tests` is false | no |
| 3 | `unsupported_language` | an extension on the source list whose language has no front end | **yes** |
| 4 | `oversized` | larger than `index.file_size_cap_mib` (ADR-0006 §7.2) | **yes** |
| 5 | `parse_error` | not lexable: invalid UTF-8, a NUL byte, an unterminated literal or block comment, an unterminated bracket at EOF | **yes** |
| 6 | `vacuous` | lexed cleanly, but `LOC == 0` or the vocabulary `n < 2`, so there is no logarithm to take | **yes** |
| 7 | `ok` | there is a number | no |

The four bold rows are what the view announces as "these paths were not analysed: N", and they are
exactly the keys ADR-0006's row `counts` already had room for — C4 checks the two vocabularies agree.
A **refused parse is a decision, not a shrug**: a front end that repaired the parse would silently drop
whichever statements the repair swallowed, and a plausible wrong LOC on a trend line is worse than a
gap that says why. A cached failure is cached as a failure (ADR-0006 §6.3's last bullet), so one broken
file is not re-parsed on every index forever.

## 4. The unit of analysis and the per-commit aggregate

### 4.1 The unit is a function, and a file's MI is the mean of its functions

**Amended by mact-by1.10.** The unit of a MI measurement was the file, and publishing the first real
corpus (§5.3) showed that was wrong in a way no fixture would have caught: the bands graded the tree
by size. The formula is `171 − 5.2·ln(V) − 0.23·G − 16.2·ln(LOC)`; scale a plain function up by
replication and `V`, `N` and `LOC` all multiply, so the two volume terms cancel and `−16.2·ln(LOC)`
remains alone. The whole file was a replicated plain function, so a whole file was graded by its
length, and a 1 400-line module scored 19.5 next to a 12-line one at 66.9.

The unit is now a **function-like definition**, and a file's MI is the LOC-weighted mean of its
blocks. Three things follow, and each is a test rather than an intention:

- `V`, `G` and LOC are counted over one block, so the `ln(LOC)` a block pays for is the length of the
  code whose density it is paying for. §5.2's size-dominance arithmetic still holds — a 250-logical-line
  function is capped at 28 no matter how regular — but it now describes a function, which is the
  granularity Coleman's `V` was derived at, instead of a file, which it never was.
- A file cannot be rescued by the plain functions it happens to sit next to, and cannot be condemned
  by the length of a module whose individual functions are all reasonable. Its number is the average
  of its parts, weighted by how much of the file each part is.
- A blob with no function-like definition in it is its own unit: a Rust type module, a Python script
  of top-level statements, a `build.rs` that is all configuration. It is measured as one block, and
  it *says* which rule made it — `unit` is `block` or `file`, and `blocks` carries how many fed the
  mean. That pair is stored, so a number from 2026 can still be read correctly in 2031.

The kinds that make a block are closed per language, in `scoring_unit.kinds` in the JSON, and they are
lexical rather than syntactic because the front end is a tokeniser and has no parse to consult (§6): in
Rust, a `fn` word token followed by a name, which catches `fn`, `pub fn`, `async fn`, `unsafe fn`,
`const fn` and a method in an `impl` or `trait` alike, and refuses a `fn` *type* (in
`struct S { f: fn(u8) -> u8 }` the token after `fn` is `(`, and there is no definition there); in Python,
a `def` or `async def` at any indentation. A block runs to the `}` that returns the brace depth to the
`fn`'s own — so a nested `fn` is measured inside the function that declares it rather than twice — and it
is widened back to the nearest `;`, `}` or `{` at that depth, which is what keeps `pub`, a `#[cfg]` and a
doc comment inside the block they decorate and keeps an `impl Gate for Plain {` header out of the first
method's volume. A definition with no body, a trait method signature, contributes no block: there is
nothing to weigh. Python has no braces to count, so a block is its `def` line plus every following line
indented deeper than that `def`, and it is dedented to column zero before being measured because
indentation is syntax there and a method arrives indented by its class — which is also why a Python
*class* is not a block of its own but each of its methods is.

Three consequences are worth naming, and the third is a weakness rather than a feature. A block whose own
numbers are not measurable (`status != ok`: fewer than two operators, no logical lines) is excluded from
its file's mean rather than scored 0, exactly as §3.5 refuses to borrow a zero for a whole file. The number
of blocks a blob can yield is bounded by the blob being in scope at all: a file over
`index.file_size_cap_mib` is `oversized` before anything is tokenised, so there is no separate per-blob
block ceiling to declare and no unbounded loop to find. And **code that is not inside a function belongs to
no block and is not weighed** — a Rust `use` and `struct` preamble, a Python script of top-level
statements, a class header. A Python file of sixty assignments and one two-line `def` reads green on the
strength of that `def`, and the rule does not notice; what it does instead is put the tell on the screen,
`blocks: 1` beside a `loc` sixty times the code that was measured, and report the file's own `V`, `G` and
`LOC` whole so the gap between a file and the weighed part of it is visible. `module_level_code` in the
JSON says the same thing to a machine, and `module_level_code_is_not_weighed_and_the_row_says_so` is the
test that keeps this paragraph honest rather than aspirational.

What has *not* changed is that a measurement is a property of one blob at one commit, never of a
working tree: the slice is a slice of a blob, the path still picks the language, and the walk is still
`git ls-tree` rather than `std::fs` (ADR-0006 §6.3 keys on exactly that). The amendment is about which
part of the blob the arithmetic is about, not about what is stored or where it came from.

The reason the change is honest rather than cosmetic is that the *tree aggregate* is untouched by it: it
still weights files by their LOC (§4.3), so `fathom report` still answers "how much of this tree is hard
to keep" and does not answer it a different way. What moved is the number that weight is averaging. A
store holding the same blobs measured two ways would be a drift artifact, so `mi.tokenize`, `mi.score`
and `definition_version` all moved 1 → 2 in this commit and the cache re-analysed everything rather than
serving two definitions out of one store (§8).

### 4.2 The commit aggregate is an aggregate, not a measurement

Population: **every file in scope in that commit's tree** (`git ls-tree -r`), not only the files the
commit touched. The ticket's own examples — a sub-threshold set that files join and leave, a 900-line
module that must outweigh a 12-line `mod.rs` — only exist in a whole-tree population, and a
touched-files-only number would make a one-file commit incomparable with a fifty-file one. Cost is
bounded by content-addressing: unchanged files are a cached block lookup (the key is
`blob_path`-keyed, so "unchanged blob and unchanged path" is a hit), and the walk is `O(tree)`
enumeration per commit with `O(changed)` analysis.

### 4.3 The arithmetic

```
MI_agg = MI_1( Σ (MI_i * LOC_i) / Σ LOC_i )      i over status == ok, accumulated path-bytes ascending
delta  = MI_1( MI_agg(commit) - MI_agg(parent #1) )          absent for a root commit
watchlist S = { path : MI_i < 25.0 }
sub_threshold_added   = S(commit) \ S(parent)
sub_threshold_removed = S(parent) \ S(commit)
hotspots = worst 5 by (MI ascending, LOC descending, path ascending)
```

- **The weight is LOC**, for the reason the ticket gives: a per-file mean lets a 12-line file speak for
  a repository. `tests/testdata/mi/aggregates/weighted_versus_per_file_mean.json` pins the difference:
  a 12-line file at 95.0 beside a 900-line file at 61.0 aggregate to **61.4**, not to the 78.0 a
  per-file mean would report: `(95*12 + 61*900)/912 = 61.4473`.
- **Accumulation order is pinned** (path bytes ascending) and the aggregate must be a **pure function
  of the commit's own file set**. A carry-forward recurrence — parent's sums, minus removed, plus
  added — is cheaper and is *refused*: floating-point addition is only deterministic given the order,
  so a recurrence would make the stored digit depend on the order the commits were indexed in, and
  `fathom doctor` could never re-derive a row from scratch to check it. Indexes are allowed to be slow
  and reproducible; they are not allowed to be either.
- **The delta is computed on the rounded aggregates**, so `delta_vs_parent` is reproducible from two
  stored rows without any unrounded state.
- **`parent #1` (first parent) is the parent**, whatever the walk mode, and the choice is stated rather
  than left to the caller: a merge with no rule silently compares against the wrong commit. A merge
  whose second parent was the interesting one is an ADR amendment, not an implementation detail.
- **An empty population is unknown**, with `gaps.no_mi = true` — not 0. A commit that only touches
  `Cargo.lock` and a PNG has no MI, and rendering 100 ("perfect") or 0 ("catastrophe") would both be
  an invention.
- **`loc`, `files`, `counts` and `unanalysed_paths` are reported alongside**, because a number that is
  a mean over six files and a number that is a mean over six thousand should not look the same.

### 4.4 What the view gets, and what it may not invent

The row's `mi` section (ADR-0006 §6.4) carries `aggregate`, `loc`, `files`, `delta_vs_parent`,
`hotspots`, `counts`, and — added by this ADR, additively, which §6.4 permits — `sub_threshold_added`,
`sub_threshold_removed` and `amber_paths`. The view renders those; it MUST NOT compute a band, a
weight, a threshold or a filter of its own, and it MUST render the caveat line whenever
`unanalysed_paths > 0`, because a number computed over 120 of 300 files with no note to that effect is
the exact failure mode this ticket exists to prevent.

## 5. Bands

### 5.1 The declared edges

<!-- mi-bands:begin -->
```json
{
  "green_min": 50.0,
  "amber_min": 38.0,
  "rule": "band(mi) = red if mi < 38.0, amber if mi < 50.0, green otherwise; an exact edge belongs to the better band",
  "names": [
    "green",
    "amber",
    "red"
  ],
  "names_by_threshold": {
    "amber_below": 50.0,
    "red_below": 38.0
  },
  "edge_ownership": "better band",
  "stored_in_cache": false,
  "is_config": false,
  "note": "declared here, not invented in the view layer (ticket rule). Not stored: a band change must not invalidate a cache full of numbers that did not move. Not config: two readers of one repository must not see different traffic lights for one commit, and a per-user band would make `fathom report --json` disagree between CI and a laptop for a non-measurement reason.",
  "calibration": "the edges are the two thresholds the 1996 paper published with the formula, converted to this definition's own 100/171 scale: `MI_raw < 65` was 'difficult to maintain' and `MI_raw > 85` was 'easy', and 65*100/171 = 38.0 while 85*100/171 = 49.7, which this definition rounds to the round number it already used. The ticket's `< 85 amber, < 65 red` cannot be used as *stored* numbers: they belong to the UPDATED MI, which estimates Halstead volume from LOC instead of measuring it, lands 60-90 for ordinary code, and pays ~5 points where a measured volume pays 25-55; applied to stored values they would call an entire real repository red, and a band that everything fails is not a signal. Placed against the scoring unit of section 4.1 — a function, which is the granularity the paper's own numbers were meant for — the two literature edges separate this repository's corpus into 77% green, 16% amber and 1% red blocks (section 5.3), which is a traffic light rather than a verdict. Section 5.2 carries the arithmetic and the line counts each edge falls at.",
  "provenance": "Coleman et al. (1996) section 'Metric Interpretation', converted by this document's own scale factor, plus the hand-counted golden vectors in tests/testdata/mi and the extrapolation table in ADR-0007 section 5.2",
  "revisit_when": "consumed twice. mact-by1.7 published the first corpus histogram (then per file) and left 50/25. The scoring-unit correction of section 4.1 put the numbers under the paper's own thresholds: the green edge was already one of them, and the red edge moves from 25.0 to 38.0 to become the other. The next trigger is a second corpus of a different shape whose block histogram places its fiftieth percentile more than a band width from this one."
}
```
<!-- mi-bands:end -->

**green ≥ 50, amber ≥ 38, red < 38**, and an exact edge belongs to the better band, so a file that
measures exactly 50.0 is green and one that measures exactly 38.0 is amber. Declared here and not
invented in the view layer: the TUI, `fathom report --json` and CI all read the same constants, and
the numbers are *not stored* in the cache (a band change must not invalidate a store full of numbers
that did not move) and *not configuration* (two clones of one repository must not disagree about a
measurement for a non-measurement reason).

**Where the edges come from, mact-by1.10.** They are Coleman's own two thresholds, converted to this
scale. The 1996 paper publishes `MI_raw < 65` as "difficult to maintain" and `MI_raw > 85` as "easy
to maintain"; this definition's scale is the raw scale divided by 1.71 (§2.1), so 65 × 100/171 = 38.0
and 85 × 100/171 = 49.7, which rounds to the 50 this ADR already used. The green edge did not move —
it had been sitting on the paper's "easy" line by accident — and the red edge moved from 25 to the
paper's "difficult" line. Nothing was fitted to a histogram to get here, which is the point: the old
red edge was placed by eye against one corpus (§5.2), and this one has a citation.

That is also the answer to "why not the ticket's 85/65 directly", whose arithmetic §5.2 keeps. Those
numbers belong to the UPDATED variant, which *estimates* `V` from LOC and so pays ~5 points where this
definition *measures* it and pays 25–55; applied to stored values they call an entire repository red,
and a band nothing passes is not a signal. What §5.2 disclosed in favour of the old edges — that at
*file* granularity a 170-line file could never be green — was not an argument for 25 either. It was an
argument that the unit was wrong, and §4.1 took it. At block granularity the same arithmetic says a
function is out of green above 190 logical lines and out of amber above 658, which are sizes at which
"hard to maintain" is a fair thing to say about a function.

### 5.2 The edges the ticket suggested do not work here, and here is the arithmetic

The ticket offered `< 85 amber, < 65 red` as an example. Those edges are Microsoft's, published with
the *updated* MI, which replaces the measured Halstead volume with an estimate derived from LOC. That
estimate is small and its logarithm is small, so updated-MI files land in the 60–90 band and the 85/65
edges cut somewhere inside real code. With a **measured** volume — which is what this ticket pins —
`5.2*ln(V)` costs 25–55 points on ordinary files before the LOC term starts, and the edges have to
move down by roughly that much or everything is red. The 40-digit hand-check:

| file | V | G | LOC | MI |
|---|---|---|---|---|
| a one-line function (`rust_tiny`, hand-counted) | 4.75 | 1 | 1 | **95.1** |
| 16 statements (`rust_basic`, hand-counted) | 227.9 | 10 | 16 | **55.9** |
| 45 logical lines (`python_basic`, hand-counted) | 257.6 | 13 | 45 | **45.3** |
| ~60 logical lines *(assumed inputs)* | 932 | 25 | 60 | **37.1** |
| ~150 logical lines *(assumed inputs)* | 2455 | 60 | 150 | **20.7** |
| ~400 logical lines *(assumed inputs)* | 6852 | 150 | 400 | **0** (raw −6.5, clamped) |
| ~1000 logical lines *(assumed inputs)* | 17715 | 300 | 1000 | **0** (raw −60.8, clamped) |

The three *hand-counted* rows are golden vectors with independent decimal arithmetic behind them. The
four *assumed inputs* rows hold `N = 2.9·LOC` (the measured 46 operators / 16 statements),
`G ≈ 0.4·LOC` (the measured 10 / 16) and a distinct-operator count that crawls from 41 to 69 across
two orders of magnitude of file size; the MI in those rows is the pinned formula computed exactly on
those assumptions, so the row is an extrapolation of the inputs and not of the arithmetic. Nobody
hand-tokenised a 400-line file, and that is itself the finding: **reaching red requires a file big
enough that no one will count it by hand**, which is why `revisit_when` exists — mact-by1.7's first
full-corpus run publishes the real histogram, and §5 is amended in the same commit if the edges are
wrong by more than a few points. Two honest disclosures fall out of this table and belong in the
runbook (mact-by1.15):

1. **At file granularity this metric is substantially a size metric.** `16.2*ln(LOC)` is the largest
   term for any file over about 20 logical lines. Reading a red file as "this code is bad" is a
   misreading; reading it as "this file is big and dense enough that the formula wants it split" is
   right, and it is why the hotspots list carries `loc` beside `mi` always.
2. **It is a poor gate.** Nothing in this epic gates on MI; if anyone ever wants to, the argument to
   re-litigate is this table, not a threshold.

**mact-by1.10 added a fifth reading.** Disclosures 1 and 2 were written as facts about the metric, and
the edges were chosen to live with them. Disclosure 1 was not a fact about the metric: it was a fact
about the *unit*, and the answer is §4.1 rather than an edge move. So this table is still load-bearing
and still read the same way — as the arithmetic of the **whole-blob** unit, which after mact-by1.10 is
the path a blob takes when it contains no function (§4.1). That is precisely why it stays: a `build.rs`,
a 900-line validator and a Python script of top-level statements all take that path, and whoever reads
their number needs to know it was size before it was anything else. The same table computed per block
says a *function* is out of green past 190 logical lines and out of amber past 658, which is a
disclosure a reader can act on, because splitting a 700-line function is a thing one can do.

### 5.3 The full-corpus run, republished: what the numbers look like when the unit is a function

§5.1's `revisit_when` and §5.2's closing paragraph made one promise: the analyzer's first corpus run
publishes its histogram here, in the same commit as the analyzer, or the edges are amended. mact-by1.7
spent that trigger with a histogram measured per file; mact-by1.10 amends the edges (§5.1) and
republishes the run on the same terms — same test,
`cargo test -p fathom --test mi the_repositorys_own_source_measures_plausibly -- --nocapture`, same
measurement path as production (each file handed to the analyzer as `(path, bytes)`, the shared
classifier deciding scope, the edges of §5.1 applied afterwards) — with the unit of §4.1 in force. The
numbers below are that test's stdout, copied.

Corpus: this repository at the commit that added this section. 166 files in scope, **112 measurable**,
54 `skipped_noise` (the `tests/` and `testdata/` trees the classifier sets aside), 26 931 logical LOC,
and zero gaps: every file in scope lexed, which is the front end's own pass criterion. The unit rule
cut 3 984 blocks out of those files, and 109 of the 112 measurable files are scored as a mean of
blocks; the three that are not are blobs whose whole body is configuration, and they say so.

| MI | files | |
|---|---|---|
| 40–49 | 9 | `#########` |
| 50–59 | 36 | `####################################` |
| 60–69 | 50 | `##################################################` |
| 70–79 | 15 | `###############` |
| 80–89 | 2 | `##` |

| band | files | logical LOC of the files in that band |
|---|---|---|
| green (`>= 50`) | 103 | 8 – 1284 |
| amber (38 – 49.9) | 9 | 131 – 903 |
| red (`< 38`) | 0 | – |

Aggregate **56.7** (green) over the corpus; median per-file MI **61.6**; **nothing clamped to 0.0**,
which is the sentence this section used to fail to be able to write. The scoring units themselves spread
p05 48.9 / p25 61.5 / p50 71.9 / p75 85.8 / p95 93.5, splitting **green 3 744 / amber 204 / red 36** at
the same edges, and the ten densest of them are:

| block | file | logical LOC | MI |
|---|---|---|---|
| `render_blocks` | `scripts/build_docs_site.py` | 126 | 26.6 |
| `self_test` | `scripts/validate_metric_model.py` | 129 | 29.3 |
| `classify` | `crates/fathom/src/mi/lang/rust.rs` | 82 | 30.3 |
| `cmd_shape` | `scripts/realdata_report.py` | 113 | 30.3 |
| `build_site` | `scripts/build_docs_site.py` | 101 | 30.5 |
| `main` | `scripts/metric_model_reference.py` | 110 | 30.9 |
| `check_live_gate` | `scripts/validate_isolation.py` | 104 | 31.0 |
| `check_vector` | `scripts/mi_reference.py` | 102 | 31.2 |
| `run` | `crates/fathom/src/tui.rs` | 73 | 31.8 |
| `parse` | `crates/act-mcp/src/evidence/coverage/lcov.rs` | 73 | 32.7 |

Four things follow, and the first two are the difference between this table and the one it replaced.

1. **The bands stopped being a file-size sort.** Under the file unit the largest green file was 17
   logical lines and the smallest red one 124: the traffic light was reading `wc -l`. Now green runs
   from 8 to 1 284 logical lines, and the ten worst scoring units are a docs renderer, a self-test, the
   Rust front end's own classifier and a handful of large validators — findings a reader will agree are
   the dense things in this tree, named at the function they belong to rather than at the file that
   happens to be long. Nothing sits at the floor of the scale, because nothing in this repository is a
   250-line function; §5.2's arithmetic, applied per block, says the scale runs out above roughly 650
   logical lines per block and no block here is close.
2. **The edges did not have to be fitted to get here.** They are Coleman's (§5.1), and this corpus is
   what applying them produces: 92% of files green, 94% of blocks green, 0.9% of blocks red. That is a
   traffic light with room in it, on a repository that is not unusual; the same tree read 3% green
   before §4.1, when the unit inflated the size penalty. The temptation now will be to raise the green
   edge toward 60 to make the light look busier. §5.1 exists to say no to that: an edge fitted to a
   histogram is a percentile with a colour on it, and the next corpus invalidates it.
3. **The tree aggregate still reads green because aggregate-LOC weighting is what it is.** 56.7 is a mean
   over 26 931 LOC with big files voting big, and §4.2's rule — an aggregate is an aggregate, not a
   measurement — is what keeps that honest. It moved from 9.2 because the unit changed, not because the
   tree did.
4. **The trigger is spent twice, so name the next one honestly.** The corpus of one is now a corpus of
   one, measured twice; `revisit_when` asks for a second corpus of a different shape — a workspace of
   30-line modules, or a Python tree of 500-line scripts — whose *block* median sits more than a band
   width from 61.6. That is an amendment to §5.1 and §5.3 together, in one commit, and it belongs to
   whichever ticket runs a second repository.

The five files with the lowest MI are `scripts/validate_metric_model.py` (903 logical LOC, 41.5),
`scripts/sample_evidence.py` (131, 43.1), `scripts/realdata_report.py` (419, 45.2),
`scripts/validate_run_lifecycle.py` (343, 46.6) and `scripts/validate_mi_definition.py` (873, 47.1) —
this repository's own largest and densest validators, which is what a hotspot list exists to say out
loud. Under the file unit the same list was five files scoring 0.0, which told a reader nothing.

## 6. The front end

### 6.1 The decision

**`lexer1`: a hand-written, pure-Rust token front end per language, one shared model, one scoring
function.** One crate module per language (`crates/fathom/src/mi/lang/rust.rs`, `…/python.rs` in
mact-by1.7), each producing the same intermediate: operator occurrence counts, operand occurrence
counts, decision-point counts by rule id, and a statement count. Everything downstream — the
measurability ladder, `V`, `G`, `MI`, the bands, the aggregate — is one implementation shared by every
language, so a second language can never disagree with the first about what the formula means.

Uniformity, which is the real want behind "one tokenizer, uniform decision points", comes from the
shared model and not from a shared grammar:

- one `Token` / `DecisionPoint` / `Statement` model,
- one measurability ladder (§3.6), one `score()`, one aggregate (§4.3), one band function (§5.1),
- per-language **data** only: the closed enumerations in the JSON (keyword lists, punctuation lists,
  decision rules, statement rules, ambiguities) plus the lexer,
- per-language **golden vectors** (one normal file, one vacuous, one hard case) before a language
  counts as supported.

Per-language work is therefore a table and a lexer — the Rust one is the harder of the two (maximal
munch, raw strings, `char` versus lifetime, nested block comments, attributes, macro token trees), the
Python one has indentation and soft keywords — and the cost is stated rather than hidden: roughly a
few hundred lines of lexer each plus the enumeration table, all of it owned here. Adding Go,
TypeScript, C or Java is an extension of the table plus a lexer; §6.5 rates each.

### 6.2 Why not tree-sitter

The ticket recommended tree-sitter and one grammar per language. It is refused, on four grounds, the
first of which is decisive on its own.

1. **It is C, and this repository has refused a C build dependency twice already for exactly this
   reason.** The `tree-sitter` crate builds the C runtime with `cc`, and each grammar crate is tens of
   thousands of lines of generated C. `cd.yml` builds five release targets from one Linux runner —
   `x86_64-unknown-linux-gnu`, `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-gnu`,
   `aarch64-unknown-linux-musl`, `aarch64-apple-darwin`. Two of those need `musl-gcc` and one needs an
   OS X cross toolchain, so "`cc` is available" is not a fact, it is a cross-compilation matrix, and a
   missing one fails as a Rust build error that looks like Rust's fault. That is verbatim ADR-0006 §5's
   argument against sqlite and ADR-0003's against libgit2 ("a C build dependency for no gain"). Being
   the third exception, in the reader crate, would make the policy a preference.
2. **The feature tree-sitter is bought for is the feature this ADR refuses.** Its value is error
   recovery for editors; §3.6 pins that a file that does not lex is `parse_error` and gets no number,
   because a repaired tree silently drops statements and a silently wrong LOC on a trend line is worse
   than an honest gap. Buying a parser to then refuse its central capability is the wrong trade.
3. **The definition would be written in somebody else's node-kind names.** Every rule in §3 is
   currently a sentence about tokens; under tree-sitter it becomes "count `match_arm` nodes", and those
   names belong to a third-party grammar release that changes them. The version that would then have to
   enter every cache key is one more moving part per block (ADR-0006 §6.3's key already carries the
   analyzer version and the config digest; a grammar version would be a third input that changes on an
   ordinary dependency bump).
4. **MI needs tokens, statement boundaries and decision points. It does not need a syntax tree.** §3.2
   went out of its way to make every classification lexically decidable — that is what the
   call-shaped-name rule, the preceding-token rule and the value-denotes-an-operand test are for —
   precisely so that two implementations could not diverge on a question of types or scoping. A real
   parser would be strictly more power than the definition needs.

### 6.3 Why not `syn` for Rust and a hand lexer for Python

`syn` is pure Rust, mature, and would give exact Rust structure (and would settle the
pattern-versus-call and turbofish questions "properly"). It is refused for one reason: it works for one
of the two languages, so the design would have two mechanisms and the claim that the front end is
general — proven on a scripting language, with indentation, soft keywords and f-strings — would be a
claim about Rust with a Python exception bolted on. A per-language hand lexer with a shared model is
the version of the design where every language is the same problem. (`rustpython-parser` was the other
half of the pairing and was not seriously considered: a second parser's entire dependency graph to
count tokens is the same mistake as sqlite for point lookups.)

### 6.4 The decision as a supply-chain fact

`deny.toml` bans `tree-sitter`, `tree-sitter-language`, `tree-sitter-rust` and `tree-sitter-python`,
each with the reason above — the same mechanism ADR-0006 §5 used to make "no sqlite" a fact the build
enforces rather than a preference someone forgets. C10 fails if one appears in `Cargo.lock`, if a
`Cargo.toml` asks for one, or if a ban is removed.

### 6.5 What is out of the set, what a file in it shows, and what v2 would cost

v1 speaks **Rust** — this repository is the first test corpus — and **Python**, one scripting language,
chosen because indentation-sensitive, soft-keyword-bearing, whitespace-significant code is the hardest
case that is still a token problem: a model that survives Python has proven something about brace
languages for free, and Python is also the language most of this project's tooling adapters are written
in.

A file whose extension is on the source list but whose language has no front end is
`unsupported_language`, renders unknown, and is counted in `unanalysed_paths`, so a screen can say
"these paths were not analysed: N (3 `.go`, 1 `.ts`)". A file whose extension is not on the source
list is not source at all (`excluded`) and is **not** counted as unanalysed — otherwise every Markdown
file in a repository would be a permanent caveat. Adding `.pyi` to the map was declined for v1: a stub
file is declarations with no statements, so it would measure `vacuous` and only add noise; the table
entry costs nothing later.

Cost per added language, from the two this ticket cost: the enumeration table (~40 keyword entries and
a punctuation split), a lexer, and three vectors. Known-hard parts: **Go** (easy — `gofmt` makes the
token stream unnervingly regular), **TypeScript/JS** (automatic semicolon insertion is Python's
logical-line problem with more exceptions), **C** (the preprocessor: `#ifdef` means a file has several
token streams and someone must pick which one is measured), **Java** (annotations are `@Name(args)`
blocks, unlike Python's decorator marker).

## 7. Which files count, and the one new config key

### 7.1 The classifier, shared with the diffstat

One classifier answers for MI and for the `+`/`/−` column, because a `+` column that counts files the
MI column ignores (or the other way round) makes the two columns of one row describe two different
commits. It is ADR-0006 §7.2's, reused verbatim — `paths.source`, `paths.noise`, `paths.exclude`, with
the same precedence (`exclude > noise > (source ∩ an enabled language)`), the same glob semantics and
the same rule that noise is *labelled*, never hidden (`noise_add`/`noise_del` still count it) — plus:

| extension | language | note |
|---|---|---|
| `.rs` | Rust | |
| `.py` | Python | `.pyi` deliberately out of v1 (§6.5) |

Longest-suffix, case-sensitive, on the repo-relative path's final segment. A blob with a NUL byte
before the first newline, or that is not valid UTF-8, is `parse_error` if its extension is in scope
and `excluded` if it is not.

### 7.2 The declared config keys, and what each one invalidates

Following ADR-0006 §7.3's rule that an analyzer declares the keys it reads and reads no others:

| key | default | read by | changes |
|---|---|---|---|
| `languages.enabled` | the v1 set (§6.5) | `mi.tokenize`, `taxonomy.path` | every `mi.tokenize` block for a file whose language moved |
| `paths.noise` | ADR-0006 §7.2's list | `mi.score`, `diff.per_file` | scope only: rows' `mi`/`diffstat` sections; blocks whose key names a noise glob |
| `paths.exclude` | ADR-0006 §7.2's list | every path walk | everything those paths feed |
| `paths.include_tests` (new, this ADR) | `false` | `mi.score`, `diff.per_file` | scope of both columns at once, which is the point |
| `index.file_size_cap_mib` | `2` | `mi.tokenize` (via ingest) | `oversized` verdicts |
| `mi.*` | — | nobody | **there is no `mi` table**, deliberately: see below |

`paths.include_tests` is the ticket's "tests/ opt-in". It is a *shared* key, so flipping it moves the
`+`/`−` column and the MI column together — the requirement of §7.1 — and an excluded test file is
labelled with the `skipped_noise` reason and still counted in `noise_add`/`noise_del`. It defaults to
`false` (tests are opt-in for MI, per the ticket) and `fathom doctor` says so out loud, because a
default that hides half the repository must at least announce itself.

**The formula, the constants, the bands and the enumerations are not configuration.** A per-user knob
for "what counts as an operator" would make two clones of one repository disagree about a measurement
for a reason that has nothing to do with the code, and would have to enter every cache key to be
honest (at which point it is a second analyzer, and §8's versioning is the right tool for that instead).
`doctor` refuses a `languages.enabled` entry outside §6.5's set (ADR-0006 §7.2 already says that); it
refuses `[mi]` as an unknown table.

## 8. Identity, versioning and the cache key

ADR-0006 §6.3 owns the derivation; this ADR fills in the two MI analyzers it already named:

```
analyzer id / version   mi.tokenize/1   mi.score/2        frontend id: lexer1 (in code, not config)
input_kind              blob_path       (path picks the language, so `blob` alone would be a lie)
config_digest(mi.tokenize) = fk1 of the resolved values of { languages.enabled, index.file_size_cap_mib }
config_digest(mi.score)    = fk1 of the resolved values of { languages.enabled, paths.noise,
                                                              paths.exclude, paths.include_tests }
```

Two analyzers rather than one, because they fail and change differently: tokenising a blob is the
expensive part and depends on the language, the size cap and the lexer's rules; scoring is arithmetic
and depends on the scope rules and the formula. So:

- **a formula tweak** bumps `mi.score` and invalidates the `mi` blocks and the rows' `mi` sections
  (ADR-0006 §7.3's table row for exactly this change); a **band tweak** bumps nothing, because a band is
  not stored (§5.1). Whether the tokeniser's blocks survive a `mi.score` bump depends on the store stamping
  the two analyzers with separate versions, and it does not yet \u2014 see the paragraph below, and §13;
- **a tokeniser fix** ("`'a` inside a macro was counted as an operand") bumps `mi.tokenize` and
  invalidates the token blocks; `mi.score` blocks built from them are rebuilt by the dependency digest;
- **editing `paths.noise`** changes only which files are in scope; nothing is re-tokenised;
- **the front-end identity** (`lexer1`) is part of the analyzer's version, not of its config: if a
  tree-sitter front end were ever added after a policy change, it would be `mi.tokenize_ts` with its own
  ids, because two front ends that disagree about `Vec<Vec<u8>>` must never be able to serve each
  other's cached rows.

**mact-by1.10 moved `definition_version`, `mi.score` and `mi.aggregate` to 2, and left `mi.tokenize` at 1.**
The scoring unit of §4.1 changes what a *number* is, and it does not change one token: the front end emits
the stream it emitted in v1, and a block is now cut out of that stream and scored per definition. So the
tokeniser's version stands. Two things a reader should know rather than guess: the store stamps both MI
analyzers with **one** version (`AnalyzerStamp` carries a single `version`, and `mi.aggregate` takes the
scorer's rather than a literal a bump would leave behind), so a warm store re-analyses every blob in scope
on this commit instead of serving two definitions out of one store \u2014 which is what a change of unit needs
anyway; and §5.1's band edges are still not a version at all, because they are not stored, so re-banding a
repository stays a re-render rather than a re-analysis.

Every block body repeats its `analyzer`, `version`, `input_kind`, `input_hash`, `config_digest` and
`result`, per ADR-0006 §6.3, so a stray block can be explained by `cat`, and a cached **failure** (the
`parse_error` and `vacuous` verdicts) is cached as a failure so one broken file is not re-parsed
forever. The version-mismatch rule is inherited unchanged: refuse, name the path, say `--rebuild`, never
migrate blind.

## 9. Golden vectors, and the tests that make them real

Everything a reader needs to re-derive a number lives under
[`tests/testdata/mi/`](../../crates/act-mcp/tests/testdata/mi): fixture sources, one vector per fixture
(`vectors/*.json`, each naming its `source`), and two commit fixtures
(`aggregates/*.json`). The token tables, the decision-point counts and the statement counts were
counted **by hand** from the files — that is the ticket's "hand-computed V, G, LOC, MI in the test
data", and the vector's `hand_computed` field says so — and every derived value is re-derived on
demand by `scripts/mi_reference.py` and in CI by `scripts/validate_mi_definition.py`, twice, in two
different numeric representations (C6 and C7).

| vector | what it pins |
|---|---|
| `rust_tiny` | `LOC == 1`, so `ln(LOC) == 0`: legal, and the reachable ceiling. A tail expression with no `;` is not a statement. |
| `rust_basic` | the normal case: `match` arms, a guarded arm, `for`, `while`, `&&`, `\|\|`, a macro, `use`/`impl`/`struct` headers, `Ok(` as a call, `Some(` as a call-shaped pattern. |
| `rust_question` | `?` and `let … else` as decision points, a `;` that closes an introduction not counting twice, `Ok(number)` in a pattern as an operator, and the turbofish consequence (`parse::<u32>()` costs `parse` as an operand). |
| `rust_generics` | maximal munch (`>>` in nested generics), `&&str` as an operator token that is *not* a fork, lifetime versus char literal, attributes and doc comments emitting nothing, a raw string swallowing a `(`, `cmp` an operator at a call site and an operand in a path, a `where` clause costing no line. |
| `rust_reformat_a` / `_b` | the pair: identical token streams, different bytes, therefore identical `V`, `G`, `LOC`, `MI`. `cargo fmt` costs zero. |
| `rust_loop` | rule coverage for `loop` (a decision point with no condition token) and for `break`, which is an operator keyword and never a decision point. |
| `rust_comment_only` | `LOC == 0` ⇒ `vacuous`, and **no `mi` field anywhere**. |
| `rust_unparseable` | an unterminated string and an unbalanced brace ⇒ `parse_error`, no number, and the reason is cached. |
| `python_basic` | logical-line counting (`31 + 14 = 45`), a docstring that counts, a comprehension `if` and a ternary, `and`/`or`, `except`, a `match` with three `case` arms and a guard, a walrus, an f-string as one operand, `=` as a keyword-argument marker. |
| `python_comment_only` | `LOC == 0` again, including a `def` hidden inside a comment — the case the "comments are not tokens" rule exists for. |
| `python_while` | rule coverage for Python's `while`: four logical lines plus two block headers (`def`, `while`) = 6, one decision point. Small on purpose: a file this size is one a human can count, and that is the point of a golden vector. |
| `python_docstring` | `LOC == 1` but Halstead vocabulary `n == 0`: the **second** way a file is vacuous, since `ln(1)` was fine and `log2(0)` is not. |
| `rust_deep_nesting` | the deep-nesting case the ticket named: seven block headers inside one another, two `else if` rungs, a two-arm `match`, both short-circuit operators as forks, and `fn grade(` as an *operand* because §3.2's name rules are ordered and rule 3 outranks rule 4. Added by mact-by1.7. |

**What pins the unit rule of §4.1, and what cannot.** A vector pins `Analyzer::measure_blob`: the
arithmetic over a *whole blob*, with its `V`, `G` and `LOC` counted over the whole fixture. That is
deliberate and it is what makes the vectors unit-independent — §4.1 moved none of the numbers in them and
C6 passes on the same digits it always did. It also means a vector's `mi` is no longer the number
`fathom` will print for the same bytes, because production scores the blob's functions and averages
them. Four of the fixtures, measured both ways at this commit:

| fixture | the vector's `mi`, over the whole blob | the same file scored per §4.1 | blocks |
|---|---|---|---|
| `rust_basic` | 55.9 | 58.2 | 1 |
| `rust_generics` | 61.8 | 65.0 | 1 |
| `python_basic` | 45.3 | 50.4 | 2 |
| `python_while` | 73.0 | 73.0 | 1 |

`python_while` is the case that says what is going on: it is one function and a couple of headers, so
the blob and the block are the same code and the two numbers are identical. Where a fixture has more than
one function, or a preamble of `use` and `struct` lines around a single one, the mean is higher, because
the plain code is no longer charged with the dense code's decisions. Both numbers are in the tree, the
oracle derives the first and the Rust tests pin the second, and neither is a substitute for the other.

What §4.1 added is a rule about *slicing*, and a slice is only wrong if you already know where the cuts
belong — so it is pinned in Rust, where the front end that does the slicing lives:

- `mi::lang::rust` and `mi::lang::python` pin the block kinds, the nesting rule (an inner block belongs
  to its enclosing definition), the `impl`/body pairing that stops one `impl` being counted twice, and
  the statement-level boundaries in Python (a `def` header and its body are one block; a top-level `for`
  is not a block at all).
- `crates/fathom/tests/mi.rs` pins the consequences: a file's `mi` is exactly the LOC-weighted mean of
  the blocks `blocks_of` reports — asserted against an independent fold of the same block list, not
  against the production fold; an unmeasurable block is *out* of the mean rather than in it as 0.0, which
  is §3.5 one level down; a blob with no function is `unit: file` with no `blocks` count; and
  reformatting a file moves neither its blocks nor its number.
- `the_repositorys_own_source_measures_plausibly` re-derives the mean for every block-scored file in
  this repository, and fails if any scoring unit in the corpus is unreachable by the red band, because
  an edge nothing in the corpus reaches is a decoration (§5.3).

What is *not* claimed: `scripts/mi_reference.py` has no block front end, so it cannot re-derive a block
list and does not pretend to. The mean is the same weighted-mean arithmetic C9 already pins at file
level, over `scoring_unit.weight`, with the same rounding and the same clamp, so the second engine
checks that arithmetic wherever a weighted mean of measurements appears; what only the Rust engine can
check is where the cuts go. Saying so here is cheaper than a fixture that checks a slice against
itself.

`definition.vectors.required_cases` is the list the ticket demanded and C6 refuses a set that stops
covering it: one normal file per language, the comment-only/`LOC == 0` case, the `LOC == 1` case, the
reachable ceiling, the reformat pair, and the hard lexical cases per language. Beyond the required
list, `aggregates/` carries the two commit fixtures: `weighted_versus_per_file_mean.json` (the 12-line
versus 900-line case, with its parent, so the delta and the watchlist transitions are exercised) and
`commit_small.json` (six measurable files and five gaps of each kind, over a parent whose two worst
files got better). C8 additionally asserts the properties as properties, not as fixtures: the watchlist
is exactly the set below the red edge, hotspots are worst-first and no longer than the declared worst-N,
`loc` is the sum of the counted files, an empty population is unknown, and **shuffling the input rows
does not change the aggregate**.

The **determinism** tests the ticket asked for, in the order each one can fail:

1. *same tree, two runs, identical MI* — the oracle runs twice in `--selftest` and compares the
   serialised results byte for byte; mact-by1.7 re-runs the same assertion in Rust over every vector,
   and over this repository's own source, in [`crates/fathom/tests/mi.rs`](../../crates/fathom/tests/mi.rs).
2. *two implementations, one number* — C7 recomputes every vector in 40-digit decimal and requires the
   same stored digit; `scripts/mi_reference.py` is the oracle a Rust port is graded against, and the
   vectors' `mi_raw` fields pin the intermediate to a relative `1e-9`, which is the room a different
   `libm` gets and no more.
3. *no value on a knife edge* — no vector may land within `1e-6` of a rounding boundary or a band edge
   (P14), so the stored digit cannot depend on the host's math library.
4. *formatting is not a change* — the reformat pair (C6 refuses it degenerating into one file).
5. *row order is not a change* — the aggregate is accumulated in path order and C8 shuffles the fixture
   rows to prove it.
6. *an `analyzer_version` bump is loud* — ADR-0006 §6.3's rule that every analyzer has a golden block
   fixture means a count change without a version bump shows up as a fixture diff in the PR that caused
   it. The vectors above are those fixtures.

**What the Rust port found in these vectors (mact-by1.7).** Running the Rust front end against the hand
counts disagreed with three of them, and the hand count was wrong every time; the rules were not.
`python_basic` listed the operand `1` six times, and the file has five — the sixth was the `1` of `.1f`
inside an f-string, which §3.1 forbids tokenising and this vector's own notes celebrate. `rust_generics`
keyed the raw string by its contents, which §3.1's "the delimiter text is part of the operand key, so
`r#"a"#` is not `"a"`"` forbids. `rust_reformat` omitted the `for` from its decision points although
§3.3's table lists `for` and the vector's own statement note names the `for` as a block header, so `G`
is 3 and its MI is 70.9. None of the three moved a rule, so none of them is an amendment under ADR-0006
§7 — the ADR text was authoritative and the fixtures moved, which is what §6.4 of that ADR prescribes.
The two operand corrections are inert (operands do not enter `V`); the `for` moved one file's MI by 0.2,
and `commit_small.json`'s aggregate is 27.5 before and after, which is the LOC weighting doing its job
rather than the change being too small to matter. One vector was added: `rust_deep_nesting`, the case
the ticket asked for that nothing here carried.

## 10. Failure modes and what is shown

| situation | what happens | what is rendered |
|---|---|---|
| language outside the v1 set | `unsupported_language`, cached | `—` plus "these paths were not analysed: N" |
| file larger than the size cap | `oversized`, cached | `—`, same caveat line |
| blob not lexable (bad UTF-8, NUL, unterminated literal/comment/bracket) | `parse_error`, cached | `—`, same caveat line, and the reason is one `cat` away in the block |
| comment-only, blank-only, or vocabulary < 2 | `vacuous`, cached | `—`, never `0` |
| no measurable file in a commit's tree | `aggregate` absent, `gaps.no_mi = true` | `—` in the MI column |
| commit has no parent (root) | `delta_vs_parent` absent | `—` in the delta column, not `0.0` |
| merge commit | delta against parent #1, stated | no special rendering; §4.3's rule |
| file in a vendor or target directory | `skipped_noise`, still counted in `noise_add`/`noise_del` | excluded from MI, visible in the diffstat's noise count |
| tests while `paths.include_tests = false` | `skipped_noise`, and `doctor` announces the default | as above |
| `fathom.toml` invalid, or an unknown key such as `[mi]` | refuse at boot, name the file and the line (ADR-0006 §7.1) | a refusal with a `fix:`, never a silent default |
| two front ends ever disagree | different analyzer ids, so they cannot share a block (§8) | — |

Two of these deserve the emphasis they cost. A `parse_error` is cached as a failure because otherwise
one broken file is re-parsed on every index forever, and `vacuous` exists as a distinct reason because
"a file with no code in it" and "a file we could not read" are different conversations to have with a
codebase, and collapsing them would make the caveat line a lie in one direction or the other.

## 11. Considered alternatives

1. **The updated MI (volume estimated from LOC).** Rejected: it makes the Halstead front end
   decorative — a metric whose input is derived from its own other input cannot explain a change — and
   it is the formula Microsoft's 85/65-style bands were calibrated for, which §5.2 shows does not fit
   the measured-volume formula this ticket pins.
2. **Bands at the ticket's suggested 85/65.** Rejected on arithmetic, not taste (§5.2). The ticket wrote
   "e.g."; using the example verbatim would have shipped a column that is red everywhere.
3. **Per-function MI, aggregated to the file.** Rejected: it needs a definition of a function
   (closures, `async` blocks, `impl` items, module-level statements), which is a second closed
   enumeration; it contradicts the ticket's `G = 1 + decision points` base of 1; and §5.2's arithmetic
   says it would not rescue the band scale either, because per-method values on the measured formula
   land in the 40–90 region, not the 60–90 the familiar thresholds assume. Kept on the table as the
   honest remedy if the corpus histogram says so; adopting it is an amendment to this ADR, not an
   implementation choice inside mact-by1.7.
4. **`G` as the sum of per-function graphs (`E − N + 2P` over `P` components).** Graph-theoretically the
   more defensible answer, and rejected for the same reason as 11.3 plus one: it makes `G` depend on
   where a language's implementation draws a function boundary, which is not visible from a token
   stream. Difference on a ten-function file: ~1.2 MI points.
5. **Operand-based Halstead (`N`, `n` over both classes).** Documented in §3.1, refused for
   comparability; the JSON key makes the choice visible.
6. **Physical-line or "physical minus comments" LOC.** Rejected in §3.4: it puts formatting changes on a
   maintainability trend line, which is a measurable harm with a one-word fix.
7. **`&&`/`\|\|` counted as decision points unconditionally.** Rejected: it counts
   `unwrap_or_else(\|\| …)` — one of the most common shapes in idiomatic Rust — as a branch, and a metric
   that punishes idiomatic style gets ignored inside a week.
8. **tree-sitter plus grammar crates.** §6.2, four grounds.
9. **`syn` for Rust plus a hand lexer for Python.** §6.3.
10. **A language-agnostic comment-plus-token approximation** (no per-language keyword tables). Rejected
    for decision points exactly as the ticket expected: "count the `if`s" requires knowing which words
    are keywords and which brackets open blocks, and an approximation of that is a metric that moves
    when a project renames a function to `iff`. It is acceptable as a *fallback path* only in the narrow
    sense that §6.5's `unsupported_language` already is one: an honest gap beats an approximation, and if
    a fallback is ever wanted it must be a separate analyzer id so its numbers never share a cache key
    with `lexer1`'s.
11. **Storing the unrounded double, or aggregating on it.** §2.2: the rounded value is the contract, so
    that a delta and an aggregate are recomputable from stored rows by a reader, and so a 1 ULP `libm`
    difference cannot reach a digit.
12. **Carry-forward aggregates over history.** §4.3: cheaper, and refused because the stored digit would
    then depend on indexing order.
13. **Bands, thresholds or enumerations as `fathom.toml` keys.** §7.2: they would have to enter every
    cache key to be honest, at which point they are a second analyzer.

## 12. What checks what

| what the ADR decides | check | how |
|---|---|---|
| the formula, the precision rules, the ladder thresholds, the analyzer ids, the aggregation and scope defaults are the same numbers in the ADR and in the JSON | C2 | the ADR's `mi-constants` block is diffed field by field against `schema/mi-definition.json` |
| bands: values, edge ownership, not stored, not config, and argued rather than asserted | C3 | `mi-bands` block + prose tokens + `band_of()` agreement with the declared edges |
| the seven reasons, their order, the caveat set, and the names ADR-0006's row already uses | C4 | vocabulary and ladder-order comparison, including ADR-0006's `counts` keys |
| the per-language enumerations are complete, non-contradictory, and agree with the extension map | C5 | table presence, keyword/punctuation collisions, decision-rule token membership |
| the vectors re-derive, cover what they promise, and none sits on a boundary | C6 | `mi_reference.check_vector` per vector + the coverage list + the reformat pair |
| a second implementation of the arithmetic agrees on the stored digit | C7 | 40-digit `decimal` recomputation of every vector |
| the commit fixtures re-derive, and the aggregation properties hold | C8 | fixture diff + properties (watchlist, hotspot order, LOC sum, empty population, order independence) |
| the aggregation rules in prose are the ones in the JSON | C9 | weight, parent rule, recompute rule, required reported fields |
| the scoring unit of §4.1 is the one in the JSON, and a file's number is the mean of its blocks | C2 + `mi.rs` | `scoring_unit` diffed against the ADR block; then `a_file_is_the_mean_of_its_function_blocks` (folded by hand in the test, not by the production fold), `an_unmeasurable_block_is_out_of_the_mean_and_not_in_it_as_zero`, `a_blob_with_no_function_is_its_own_unit`, `a_reformatted_file_scores_identically_under_the_unit_rule`, and the per-file re-fold inside `the_repositorys_own_source_measures_plausibly` |
| the front-end decision holds in the graph | C10 | no `tree-sitter*` in `Cargo.lock` or any `Cargo.toml`, and each is banned in `deny.toml`, and §6's argument names `cc`, musl, the grammars and error recovery |
| MI and the diffstat share one classifier and one noise list | C11 | ADR-0006 §7.2's `paths.noise` default cell versus this ADR's, verbatim; `paths.include_tests` declared here |
| the checker is wired where CI runs it | C12 | `scripts/ci.sh`, `docs/ci.md`, `docs/testing.md`, README's ADR list |
| none of the above is a lie | C13 | `--self-test`: seventeen mutations, each of which must fire exactly the checks that claim it |

## 13. What this ADR does not decide, and what would reopen it

- **Whether MI is worth a column.** That is the epic's premise, not this ADR's question; §5.2 is the
  honest statement of what the number is sensitive to, and it is the paragraph to read before anyone
  gates on it.
- **Anything about storage layout, digests as bytes, or the row's JSON shape** — ADR-0006 owns those and
  this ADR fills in two analyzers and three additive `mi` fields inside its rules.
- **Coverage and benchmark semantics** — ADR-0008.
- **The first real histogram.** Spent twice: mact-by1.7 published it per file, and mact-by1.10
  republished it per function (§5.3) while amending the edges to the paper's own thresholds. What would
  reopen the edges now is in `revisit_when`: a **second** corpus, of a different shape than this
  repository, whose block median sits more than a band width from 61.6.
- **Whether a block is ever worth its own row.** §4.1 scores blocks and stores one number per file; the
  blocks themselves are not stored, though each file's `mi` record says how many of them there were and
  which rule made the number (`unit`, `blocks`) and the `mi.score` block body repeats both. That is enough
  to re-derive a stored number from its blob. It is not enough for a per-function history, and growing one
  would be a new analyzer id with its own blocks rather than a wider `mi` section.
- **Whether the block kinds are right for a language nobody has run yet.** `scoring_unit.kinds` is closed
  per language, and a kind this repository's own corpus never reaches is a kind nobody has checked. The
  corpus test fails if the red band describes no scoring unit in the tree (§5.3's finding 1), which is the
  closest thing to a tripwire for that.
- **A third language.** It is a table, a lexer and three vectors (§6.5), and nothing in §2–§5 changes.
- **Separating the two MI analyzer versions in the store.** This ADR names two analyzers precisely so a
  scoring change does not re-tokenise a tree (ADR-0006 §7.3's table row), and `schema/mi-definition.json`
  keeps two versions to that end \u2014 but `AnalyzerStamp` carries one `version` and `store::analyzer::registry`
  gives both MI analyzers the scorer's, so today a `mi.score` bump invalidates the tokeniser's blocks too.
  Nothing in §2\u2013§5 depends on that being fixed; the cost is that a warm store re-analyses a tree it had
  already tokenised, which for a repository of this size is a second and is not worth a schema change. The
  first time it is worth one, the fix is in `store/analyzer.rs`'s registry and in the stamp's one version
  field, and §8's bullet list is the statement of what it must then deliver.
