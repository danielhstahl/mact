# `tests/realdata/expected/` — what the real-act smoke's records should say, byte for byte

Empty on purpose. This is the second-tier check of `scripts/smoke-real-act.sh`, and it can
only be written by a run that happened.

The smoke always makes **value** assertions about the records its fixture earns: the coverage
totals are the arithmetic hard-coded in `tests/realdata/fixture/.github/workflows/coverage.yml`
(`4/6` lines, `1/2` branches), the benchmark file must carry the four metric names the fixture
emits, from the three producers it exercises. Those checks need no golden, and they are the
ones that catch a producer silently stopping work.

What a golden adds is everything *else* about the record: key order, the envelope fields, the
`extra` block, which labels ended up attached to which metric, the exact rounding of a
duration. Those are contract details too, and a property test does not pin them. So:

```sh
scripts/smoke-real-act.sh --prereq-only        # first: can this machine run real act at all?
scripts/smoke-real-act.sh --write-expected     # real mode only; the stub is refused
$EDITOR tests/realdata/expected/*.jsonl        # read every line: you are signing for it
```

The first line is not ceremony. `--write-expected` needs a run that happened, a run needs a
container runtime, and a machine without one is described in docs/testing.md §7 "The
pre-flight" — which is also where the nightly runner is named as the place this does work.

`--write-expected` writes one normalised file per kind (`coverage.jsonl`, `benchmark.jsonl`).
**Normalised** means the volatile values became named placeholders — `{RUN_ID}`, `{COMMIT}`,
`{TS}`, `{HOST}`, `{NUMBER}` — by the same rule and with the same vocabulary as
`tests/e2e/normalize.rs`: nothing is dropped and nothing is compared loosely, the fields that
describe *this* run are replaced by a name that says why. `scripts/realdata_report.py
normalise <file>` prints the transformation for one file if you want to see it in isolation.

From the next real run onwards, `records vs tests/realdata/expected` is a byte-for-byte diff
after that normalisation, and it will say which line moved.

Two things this directory is not:

* **not** a place for stub-derived records. The stub's payload is
  `tests/e2e/testdata/payload`, not this fixture's, so goldens written in `--stub` mode would
  record the stub's numbers and pass forever. The script refuses `--write-expected` there.
* **not** a replacement for the `validate-evidence` step. A record can be schema-valid, match
  its golden, and still be wrong about the world; the golden pins shape, the value assertions
  pin meaning.
