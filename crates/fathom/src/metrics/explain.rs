//! The drop log — which is to say, `--explain` (ADR-0008 §2's second sentence: "the drop log *is*
//! the `--explain` output").
//!
//! A rule set nobody can interrogate is a rule set nobody believes. Every record this reader read
//! is in here twice over: once as *kept* (with the fate it ended up with) or once as *dropped*
//! (with exactly one reason code from the closed 18), and the two together answer the only question
//! a wrong number actually raises — "why is this column `—`, and where did my data go".
//!
//! The per-run and per-group details below are not decoration either. They are the quantities the
//! model computes *internally* — each run's coverage total and where it came from, each benchmark
//! group's spread and which runs it spans — and they are what `docs/evidence-format.md` §12's
//! recipes print. That is what makes "the reader and the documented recipes agree" a test with
//! something to compare (see `crates/fathom/tests/evidence_recipes.rs`), and it is what the commit
//! detail view (mact-by1.14) renders: per-run totals, excluded runs, caveats by name.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use serde_json::Number;

use super::row::TotalSource;
use super::vocab::DropReason;

/// One dropped record: where it was, why, and what it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drop {
    /// The commit directory it came from (empty for a directory that is not a SHA — there is no
    /// commit to attribute it to).
    pub sha: String,
    /// `path:line`, or a bare path for a file- or directory-level refusal.
    pub location: String,
    /// The one reason, from the closed 18.
    pub reason: DropReason,
    /// The sentence half, naming the offending value.
    pub detail: String,
    /// The run the record named, when it got that far.
    pub run_id: Option<String>,
    /// The `run_status` the record named, when it said.
    pub status: Option<String>,
}

impl fmt::Display for Drop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "DROP {:22} {:58} {}",
            self.reason, self.location, self.detail
        )?;
        if let Some(run_id) = &self.run_id {
            write!(f, "  run {run_id}")?;
        }
        if let Some(status) = &self.status {
            write!(f, "  status {status}")?;
        }
        Ok(())
    }
}

/// A record that became (or could have become) a number, and what happened to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kept {
    /// The commit it belongs to.
    pub sha: String,
    /// `path:line`.
    pub location: String,
    /// `benchmark` or `coverage`.
    pub kind: String,
    /// A short handle: the file path or metric name a human would recognise.
    pub handle: String,
    /// Which run produced it.
    pub run_id: String,
    /// That run's `run_status`.
    pub status: String,
    /// Whether this record's run was the selected one for its `(kind, workflow, task)`.
    pub selected: bool,
    /// What the record fed: the pooled measure and total source, or the group it joined.
    pub note: String,
}

impl fmt::Display for Kept {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "KEEP {:9} {:58} {:24} {}",
            self.kind, self.location, self.handle, self.note
        )
    }
}

/// One run's coverage total for one measure, as the reader derived it.
#[derive(Debug, Clone, PartialEq)]
pub struct CoverageRunTotal {
    /// Which run.
    pub run_id: String,
    /// That run's `run_status` — present so a caller can see the failed run it did not use.
    pub run_status: String,
    /// The record's `source` (`lcov`, `coverage_summary`, `log_scrape`).
    pub source: String,
    /// Which measure the counts are.
    pub measure: super::vocab::Measure,
    /// Numerator.
    pub hit: u64,
    /// Denominator. `0` is vacuous, not `0 %`.
    pub found: u64,
    /// Whether the run published this total or the reader summed file records to get it.
    pub total_source: TotalSource,
    /// Whether this total fed the row's pooled number — false for a superseded run, a non-success
    /// run, a truncated run, or a non-primary measure.
    pub used: bool,
}

/// One benchmark group's spread at one commit, over every retained run.
#[derive(Debug, Clone, PartialEq)]
pub struct BenchmarkSpread {
    /// Metric name.
    pub name: String,
    /// Canonical labels (keys ascending, no whitespace) — the middle of the group key.
    pub labels: String,
    /// The unit, absent for the `unit_missing` group.
    pub unit: Option<String>,
    /// Every value the retained runs produced, in walk order: the population `min`/`max` span.
    pub values: Vec<Number>,
    /// The distinct runs those values came from, sorted.
    pub runs: Vec<String>,
    /// The distinct `source`s they came from; more than one is the `source_changed` caveat.
    pub sources: Vec<String>,
}

/// What one walk read, and what it did with every record.
///
/// This is the typed half of `--explain`; [`fmt::Display`] is the text half, and the two agree
/// because the text is rendered from the fields.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Explain {
    files: usize,
    lines: usize,
    readable: usize,
    refused: usize,
    drops: Vec<Drop>,
    kept: Vec<Kept>,
    coverage: BTreeMap<String, Vec<CoverageRunTotal>>,
    spreads: BTreeMap<String, Vec<BenchmarkSpread>>,
}

impl Explain {
    /// Record files opened.
    #[must_use]
    pub fn files(&self) -> usize {
        self.files
    }

    /// Record lines attempted.
    #[must_use]
    pub fn records(&self) -> usize {
        self.lines
    }

    /// Lines that came back as typed records.
    #[must_use]
    pub fn readable(&self) -> usize {
        self.readable
    }

    /// Lines and paths refused, with a reason each.
    #[must_use]
    pub fn refused(&self) -> usize {
        self.refused
    }

    /// Every drop, in commit then path then line order.
    #[must_use]
    pub fn drops(&self) -> &[Drop] {
        &self.drops
    }

    /// Every kept record.
    #[must_use]
    pub fn kept(&self) -> &[Kept] {
        &self.kept
    }

    /// Drops under one commit.
    #[must_use]
    pub fn drops_for(&self, sha: &str) -> Vec<&Drop> {
        self.drops.iter().filter(|drop| drop.sha == sha).collect()
    }

    /// How many drops each reason accounts for — the summary line of `--explain`.
    #[must_use]
    pub fn by_reason(&self) -> BTreeMap<DropReason, usize> {
        let mut counts = BTreeMap::new();
        for drop in &self.drops {
            *counts.entry(drop.reason).or_insert(0) += 1;
        }
        counts
    }

    /// Every run's coverage total at one commit, including the runs that were not used.
    #[must_use]
    pub fn coverage_totals(&self, sha: &str) -> &[CoverageRunTotal] {
        self.coverage.get(sha).map_or(&[], Vec::as_slice)
    }

    /// Every benchmark group's spread at one commit.
    #[must_use]
    pub fn spreads(&self, sha: &str) -> &[BenchmarkSpread] {
        self.spreads.get(sha).map_or(&[], Vec::as_slice)
    }

    pub(crate) fn set_counts(
        &mut self,
        files: usize,
        lines: usize,
        readable: usize,
        refused: usize,
    ) {
        self.files = files;
        self.lines = lines;
        self.readable = readable;
        self.refused = refused;
    }

    pub(crate) fn push_drop(&mut self, drop: Drop) {
        self.drops.push(drop);
    }

    pub(crate) fn push_kept(&mut self, kept: Kept) {
        self.kept.push(kept);
    }

    pub(crate) fn set_coverage_totals(&mut self, sha: &str, totals: Vec<CoverageRunTotal>) {
        if !totals.is_empty() {
            self.coverage.insert(sha.to_owned(), totals);
        }
    }

    pub(crate) fn set_spreads(&mut self, sha: &str, spreads: Vec<BenchmarkSpread>) {
        if !spreads.is_empty() {
            self.spreads.insert(sha.to_owned(), spreads);
        }
    }

    pub(crate) fn sort_drops(&mut self) {
        self.drops.sort_by(|left, right| {
            (&left.sha, &left.location, left.reason).cmp(&(
                &right.sha,
                &right.location,
                right.reason,
            ))
        });
    }

    /// The set of reasons dropped anywhere under this commit — what a gap code is chosen from.
    pub(crate) fn reasons_for(&self, sha: &str) -> BTreeSet<DropReason> {
        self.drops
            .iter()
            .filter(|drop| drop.sha == sha)
            .map(|drop| drop.reason)
            .collect()
    }
}

impl fmt::Display for Explain {
    /// The `--explain` text: a header, a per-reason tally, then one line per record — kept or
    /// dropped — grouped by commit.
    ///
    /// `#`-prefixed so the whole block is pasteable into an issue or a log without a pager thinking
    /// it is data, and ordered by commit then location so a diff of two runs is readable.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "# walk: {} file(s), {} record line(s) → {} readable, {} refused; kept {} record(s), dropped {} decision(s)",
            self.files,
            self.lines,
            self.readable,
            self.refused,
            self.kept.len(),
            self.drops.len()
        )?;
        for (reason, count) in self.by_reason() {
            writeln!(f, "# {reason:22} {count}")?;
        }
        let mut shas: Vec<&String> = self
            .kept
            .iter()
            .map(|kept| &kept.sha)
            .chain(self.drops.iter().map(|drop| &drop.sha))
            .collect();
        shas.sort();
        shas.dedup();
        for sha in shas {
            writeln!(f, "# {sha}")?;
            for kept in self.kept.iter().filter(|kept| &kept.sha == sha) {
                writeln!(f, "# {kept}")?;
            }
            for drop in self.drops.iter().filter(|drop| &drop.sha == sha) {
                writeln!(f, "# {drop}")?;
            }
        }
        for (sha, totals) in &self.coverage {
            for total in totals {
                writeln!(
                    f,
                    "# {sha} coverage-total {} {:14} {:11} {}/{} {}",
                    total.run_id,
                    total.source,
                    total.measure,
                    total.hit,
                    total.found,
                    if total.used { "used" } else { "unused" },
                )?;
            }
        }
        for (sha, spreads) in &self.spreads {
            for spread in spreads {
                let values: Vec<String> = spread.values.iter().map(ToString::to_string).collect();
                writeln!(
                    f,
                    "# {sha} spread {} [{}] over {} run(s) from {}",
                    spread.name,
                    values.join(","),
                    spread.runs.len(),
                    spread.sources.join("+"),
                )?;
            }
        }
        Ok(())
    }
}
