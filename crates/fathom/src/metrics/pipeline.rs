//! The four stages, executed (ADR-0008 §2).
//!
//! `walk` happens in [`act_evidence::read`] and is folded in here as the walk-stage drops; the
//! three stages in this file are `retain`, `select` and `aggregate`, in that order, always, with
//! every record ending up either retained or dropped with exactly one reason. The implementation is
//! a line-for-line transcription of `scripts/metric_model_reference.py`, which is the oracle: the
//! goldens in `crates/act-mcp/tests/testdata/metrics/` and the sample tree are compared against
//! both, so "the Rust reader and the python oracle agree" is a build gate rather than an aspiration.
//!
//! Iteration order is load-bearing in two places, and both are mirrored deliberately:
//!
//! * retention walks benchmark-then-coverage per commit, because source precedence is applied per
//!   kind and *which* of two same-precedence duplicates survives depends on the order they were
//!   read in;
//! * a run's coverage total depends on whether its `scope:"total"` record was seen before its
//!   `scope:"file"` ones, which is file order — see [`run_totals`].

use std::collections::{BTreeMap, BTreeSet};

use act_evidence::read::{Reason as ReadReason, Record as ReadRecord, Scan};
use act_evidence::Kind;
use serde_json::Number;

use super::explain::{BenchmarkSpread, CoverageRunTotal, Drop, Explain, Kept};
use super::row::{
    canonical_json, median, round_half_away, ActionGroup, BenchmarkGroup, CommitMetrics, Coverage,
    ExcludedRun, Gaps, SecondaryMeasure, TotalSource,
};
use super::vocab::{ActionState, Caveat, Column, DropReason, GapCode, Measure};
use super::Reader;

/// The walk-stage refusal codes, mapped onto ADR-0008's drop codes.
///
/// One arrow each, and total: `act_evidence::read` names what it found, ADR-0008 names why that
/// matters. `torn_write` is the one collapse — the model folds a torn final line into `malformed`,
/// and the detail string keeps the distinction.
#[must_use]
pub(super) const fn walk_reason(reason: ReadReason) -> DropReason {
    match reason {
        ReadReason::NotARecordPath => DropReason::NotARecordPath,
        ReadReason::TempFile => DropReason::TempFile,
        ReadReason::DirectoryNotSha => DropReason::DirectoryNotSha,
        ReadReason::TornWrite | ReadReason::Malformed => DropReason::Malformed,
        ReadReason::SchemaUnsupported => DropReason::SchemaUnsupported,
        ReadReason::CommitMismatch => DropReason::CommitMismatch,
        ReadReason::BadRunId => DropReason::BadRunId,
    }
}

/// A commit-scoped sink for drops, so every call site states only what it knows: which record,
/// which reason, what to say.
pub(super) struct Log<'a> {
    sha: &'a str,
    explain: &'a mut Explain,
}

impl Log<'_> {
    fn record(&mut self, record: &ReadRecord, reason: DropReason, detail: impl Into<String>) {
        let envelope = record.envelope();
        self.explain.push_drop(Drop {
            sha: self.sha.to_owned(),
            location: record.location(),
            reason,
            detail: detail.into(),
            run_id: Some(envelope.run_id.to_string()),
            status: Some(envelope.run_status.to_string()),
        });
    }
}

/// A benchmark group key: `(name, canonical labels, unit)`, with an absent unit its own key value
/// (ADR-0008 §4.3). Sorted on the *display* form of the unit so the row order matches the oracle's
/// `str(unit)` sort exactly — `"None"` sorts before a lowercase unit, and so does `None`.
pub(super) type Group = (String, String, Option<String>);

/// One measure's pooled (hit, found) and the runs that fed it.
#[derive(Debug, Default)]
struct Pool {
    hit: u64,
    found: u64,
    runs: BTreeSet<String>,
}

/// One run's total for one measure, and how it was known (ADR-0008 §3.3).
#[derive(Debug, Clone, Copy)]
struct RunTotal {
    hit: u64,
    found: u64,
    source: TotalSource,
}

/// One group's spread over every retained run.
#[derive(Debug, Default, Clone)]
pub(super) struct Spread {
    values: Vec<Number>,
    runs: Vec<String>,
    sources: Vec<String>,
}

// --------------------------------------------------------------------- record accessors

/// The flag list, empty when the record carried none.
fn flags_of(record: &ReadRecord) -> Vec<String> {
    match &record.record {
        act_evidence::EvidenceRecord::Benchmark(line) => {
            line.metric.flags.clone().unwrap_or_default()
        }
        act_evidence::EvidenceRecord::Coverage(line) => {
            line.counts.flags.clone().unwrap_or_default()
        }
    }
}

/// The `source` as the wire spells it.
fn source_of(record: &ReadRecord) -> String {
    match &record.record {
        act_evidence::EvidenceRecord::Benchmark(line) => line.metric.source.as_str().to_owned(),
        act_evidence::EvidenceRecord::Coverage(line) => line.counts.source.as_str().to_owned(),
    }
}

/// The run's `run_status` as the wire spells it.
fn status_of(record: &ReadRecord) -> String {
    record.envelope().run_status.to_string()
}

/// The label set, canonicalised once per record: the middle column of a group key.
fn labels_of(record: &ReadRecord) -> String {
    let map = match &record.record {
        act_evidence::EvidenceRecord::Benchmark(line) => {
            line.metric.labels.clone().unwrap_or_default()
        }
        act_evidence::EvidenceRecord::Coverage(_) => BTreeMap::new(),
    };
    canonical_json(&map)
}

/// The `sample` index, when the record numbered itself.
fn sample_of(record: &ReadRecord) -> Option<u32> {
    match &record.record {
        act_evidence::EvidenceRecord::Benchmark(line) => line.metric.sample,
        act_evidence::EvidenceRecord::Coverage(_) => None,
    }
}

/// The metric value, as the JSON number it was.
fn value_of(record: &ReadRecord) -> Number {
    match &record.record {
        act_evidence::EvidenceRecord::Benchmark(line) => line.metric.value.as_number().clone(),
        act_evidence::EvidenceRecord::Coverage(_) => Number::from(0),
    }
}

/// The group this record belongs to (benchmark records only).
fn group_of(record: &ReadRecord) -> Group {
    let name = match &record.record {
        act_evidence::EvidenceRecord::Benchmark(line) => line.metric.name.clone(),
        act_evidence::EvidenceRecord::Coverage(line) => line
            .counts
            .name
            .clone()
            .unwrap_or_else(|| line.counts.scope.as_str().to_owned()),
    };
    let unit = match &record.record {
        act_evidence::EvidenceRecord::Benchmark(line) => line.metric.unit.clone(),
        act_evidence::EvidenceRecord::Coverage(_) => None,
    };
    (name, labels_of(record), unit)
}

/// The human handle `--explain` prints for a record.
fn handle_of(record: &ReadRecord) -> String {
    match &record.record {
        act_evidence::EvidenceRecord::Benchmark(line) => line.metric.name.clone(),
        act_evidence::EvidenceRecord::Coverage(line) => line
            .counts
            .path
            .as_ref()
            .map(|path| path.to_string())
            .or_else(|| line.counts.name.clone())
            .unwrap_or_else(|| line.counts.scope.as_str().to_owned()),
    }
}

/// Is this a `scope:"total"` coverage record?
fn is_total(record: &ReadRecord) -> bool {
    matches!(
        &record.record,
        act_evidence::EvidenceRecord::Coverage(line) if line.counts.scope == act_evidence::Scope::Total
    )
}

/// A scraped percentage, if this record is one (ADR-0002 §4.3: percent-only ⇔ `log_scrape`).
fn reported_percent(record: &ReadRecord) -> Option<String> {
    match &record.record {
        act_evidence::EvidenceRecord::Coverage(line) => line
            .counts
            .reported_percent
            .map(|percent| percent.to_string()),
        act_evidence::EvidenceRecord::Benchmark(_) => None,
    }
}

/// The complete `(measure, hit, found)` pairs a coverage record carries (ADR-0008 §3.5).
///
/// Partial pairs are not counts: `hit ≤ found` is deliberately *not* checked here, because that is
/// an extractor rule the schema cannot express and a reader that renumbers a tool's numbers is
/// worse than one that reports them.
fn count_pairs(record: &ReadRecord) -> Vec<(Measure, u64, u64)> {
    let act_evidence::EvidenceRecord::Coverage(line) = &record.record else {
        return Vec::new();
    };
    let counts = &line.counts;
    let mut pairs = Vec::new();
    let mut push = |measure: Measure, found: Option<u64>, hit: Option<u64>| {
        if let (Some(hit), Some(found)) = (hit, found) {
            pairs.push((measure, hit, found));
        }
    };
    push(Measure::Lines, counts.lines_found, counts.lines_hit);
    push(
        Measure::Statements,
        counts.statements_found,
        counts.statements_hit,
    );
    push(
        Measure::Branches,
        counts.branches_found,
        counts.branches_hit,
    );
    push(
        Measure::Functions,
        counts.functions_found,
        counts.functions_hit,
    );
    pairs
}

/// The `(kind, workflow, task)` identity a run is selected by, and the action verdict is grouped by.
fn selection_key(record: &ReadRecord) -> (String, String, String) {
    let envelope = record.envelope();
    (
        envelope.kind().as_str().to_owned(),
        envelope.workflow.clone(),
        envelope.task.clone(),
    )
}

// --------------------------------------------------------------------- stage 2: retain

/// Everything a commit's records split into: readable (any status), and retained (usable for a
/// number).
#[derive(Default)]
pub(super) struct Kept2 {
    /// Readable records per commit, in walk order — the population the action state is read from.
    pub readable: BTreeMap<String, Vec<ReadRecord>>,
    /// Retained records per commit, benchmark-then-coverage — the population numbers are built from.
    pub retained: BTreeMap<String, Vec<ReadRecord>>,
}

/// Stage 2: which records a number may be built from, and the walk-stage drops on the way past.
///
/// The readable population is filled *first*, before any retention test, because "did a run happen
/// here" is a different question from "is this number usable" (ADR-0008 §5): a failed run is
/// readable, is not retained, and still gets a vote in the action column.
pub(super) fn retain(reader: &Reader, scan: &Scan, explain: &mut Explain) -> Kept2 {
    let mut out = Kept2::default();
    for sha in scan.commits() {
        let readable: Vec<ReadRecord> = scan.records_for(sha).to_vec();
        out.readable.insert(sha.to_owned(), readable.clone());

        let mut retained: Vec<ReadRecord> = Vec::new();
        for kind in [Kind::Benchmark, Kind::Coverage] {
            let mut keep: Vec<ReadRecord> = Vec::new();
            {
                let mut log = Log {
                    sha,
                    explain: &mut *explain,
                };
                for record in readable.iter().filter(|record| record.kind() == kind) {
                    let status = status_of(record);
                    if !reader.model.is_eligible(&status) {
                        log.record(
                            record,
                            DropReason::RunStatusExcluded,
                            format!("run_status {status:?}"),
                        );
                        continue;
                    }
                    let flags = flags_of(record);
                    if flags.iter().any(|flag| flag == "truncated") {
                        log.record(
                            record,
                            DropReason::RunTruncated,
                            "the run hit the metric cap; what survived is not the run",
                        );
                        continue;
                    }
                    if kind == Kind::Benchmark && !reader.keeps_benchmark(&benchmark_name(record)) {
                        log.record(
                            record,
                            DropReason::ConfigFiltered,
                            format!(
                                "{:?} excluded by benchmarks.allow/deny",
                                benchmark_name(record)
                            ),
                        );
                        continue;
                    }
                    keep.push(record.clone());
                }
            }
            if kind == Kind::Benchmark {
                apply_source_precedence(reader, sha, &mut keep, explain);
            }
            retained.extend(keep);
        }
        out.retained.insert(sha.to_owned(), retained);
    }
    out
}

fn benchmark_name(record: &ReadRecord) -> String {
    match &record.record {
        act_evidence::EvidenceRecord::Benchmark(line) => line.metric.name.clone(),
        act_evidence::EvidenceRecord::Coverage(_) => String::new(),
    }
}

/// Within one run and one measurement the highest-precedence `source` wins (ADR-0002 §5.1, applied
/// by ADR-0008 §4.1).
///
/// The key carries `sample`: two samples of one `(name, labels)` in one run are two measurements,
/// not one measurement seen twice, and a precedence rule that collapsed them would throw away the
/// noise estimate the samples exist to provide. Two records of the *same* key and the same
/// precedence are the writer's own duplicate — the later one is dropped, which is what the oracle
/// does and what keeps a re-run of the same file stable.
fn apply_source_precedence(
    reader: &Reader,
    sha: &str,
    keep: &mut Vec<ReadRecord>,
    explain: &mut Explain,
) {
    let mut slots: BTreeMap<(String, Group, Option<u32>), usize> = BTreeMap::new();
    let mut index = 0;
    while index < keep.len() {
        let record = &keep[index];
        let key = (
            record.envelope().run_id.to_string(),
            group_of(record),
            sample_of(record),
        );
        let rank = reader.model.precedence(&source_of(record));
        match slots.get(&key).copied() {
            None => {
                slots.insert(key, index);
                index += 1;
            }
            Some(slot) => {
                let incumbent_rank = reader.model.precedence(&source_of(&keep[slot]));
                if rank >= incumbent_rank {
                    let incumbent = source_of(&keep[slot]);
                    let mut log = Log { sha, explain };
                    log.record(
                        record,
                        DropReason::SourceShadowed,
                        format!(
                            "{} shadowed by {incumbent} for the same measurement",
                            source_of(record)
                        ),
                    );
                    keep.remove(index);
                } else {
                    let winner = source_of(record);
                    let incumbent = source_of(&keep[slot]);
                    let mut log = Log { sha, explain };
                    log.record(
                        &keep[slot],
                        DropReason::SourceShadowed,
                        format!("{incumbent} superseded within the run by {winner}"),
                    );
                    keep[slot] = record.clone();
                    index += 1;
                }
            }
        }
    }
}

// --------------------------------------------------------------------- stage 3: select

/// Stage 3: the newest run per `(kind, workflow, task)`; everything older is superseded, which is
/// *named in the row*, not quietly dropped.
///
/// "Newest" is the lexical maximum of `run_id`, which is the chronologically newest run because the
/// writer minted the id with a UTC timestamp inside it (ADR-0002 §1.2) — so no clock is parsed and
/// no clock skew can reorder two runs. `kind` is in the key because one run publishes one file
/// *per kind*: without it a benchmark run would supersede a coverage run and delete its coverage.
pub(super) fn select(records: &[ReadRecord]) -> (Vec<ReadRecord>, Vec<ReadRecord>) {
    let mut groups: BTreeMap<(String, String, String), Vec<&ReadRecord>> = BTreeMap::new();
    for record in records {
        groups
            .entry(selection_key(record))
            .or_default()
            .push(record);
    }
    let mut selected = Vec::new();
    let mut superseded = Vec::new();
    for (_, group) in groups {
        let newest = group
            .iter()
            .map(|record| record.envelope().run_id.to_string())
            .max()
            .unwrap_or_default();
        for record in group {
            if record.envelope().run_id.to_string() == newest {
                selected.push((*record).clone());
            } else {
                superseded.push((*record).clone());
            }
        }
    }
    (selected, superseded)
}

// --------------------------------------------------------------------- stage 4: coverage

/// A run's per-measure totals, in walk order, plus the drops the walk produced along the way.
///
/// A run's own `scope:"total"` record **is** that run's total and is used directly (P6). Its file
/// records are summed only when it published no total — and then the caveat `total_from_file_sum`
/// rides on the row, because that total is a reader's construction and not the tool's claim.
/// `sum(file records) == total` is *not* checked: ADR-0002 §4.2 makes the mismatch everyday
/// (files instrumented outside the workspace are in the total and in no file record), so a reader
/// that treated the difference as corruption would fire on healthy trees.
fn run_totals(
    records: &[ReadRecord],
    mut log: impl FnMut(&ReadRecord, DropReason, String),
) -> BTreeMap<String, BTreeMap<Measure, RunTotal>> {
    let mut totals: BTreeMap<String, BTreeMap<Measure, RunTotal>> = BTreeMap::new();
    for record in records {
        let pairs = count_pairs(record);
        if pairs.is_empty() {
            log(
                record,
                DropReason::NoCounts,
                "no complete count pair and no reported_percent".to_owned(),
            );
            continue;
        }
        let run = record.envelope().run_id.to_string();
        let entry = totals.entry(run).or_default();
        for (measure, hit, found) in pairs {
            if is_total(record) {
                entry.insert(
                    measure,
                    RunTotal {
                        hit,
                        found,
                        source: TotalSource::ScopeTotal,
                    },
                );
                continue;
            }
            match entry.get(&measure).copied() {
                Some(existing) if existing.source == TotalSource::ScopeTotal => log(
                    record,
                    DropReason::FileUnderTotal,
                    format!(
                        "the run published its own {measure} total, which is that run's number"
                    ),
                ),
                existing => {
                    let (base_hit, base_found) =
                        existing.map_or((0, 0), |total| (total.hit, total.found));
                    entry.insert(
                        measure,
                        RunTotal {
                            hit: base_hit + hit,
                            found: base_found + found,
                            source: TotalSource::SumOfFileRecords,
                        },
                    );
                }
            }
        }
    }
    totals
}

/// The pooled coverage for one commit: the row's `coverage` block, or `None` plus the reason why.
pub(super) fn coverage_of(
    reader: &Reader,
    selected: &[ReadRecord],
    log: &mut Log<'_>,
) -> (Option<Coverage>, Vec<Caveat>) {
    let mut caveats: Vec<Caveat> = Vec::new();
    let mut eligible: Vec<ReadRecord> = Vec::new();

    // §3.6 first: a scraped percentage is a caveat, never the plotted number. It is dropped here,
    // at the top of aggregation, because it has no denominator to pool and inventing one (874/1000)
    // is forbidden by ADR-0002 §4.3.
    for record in selected
        .iter()
        .filter(|record| record.kind() == Kind::Coverage)
    {
        let flags = flags_of(record);
        let mut scraped: Option<DropReason> = None;
        if flags.iter().any(|flag| flag == "percent_only") {
            scraped = Some(DropReason::PercentOnly);
            push_caveat(reader, &mut caveats, Caveat::PercentOnly);
        }
        if flags.iter().any(|flag| flag == "low_confidence") {
            scraped = scraped.or(Some(DropReason::LowConfidence));
            push_caveat(reader, &mut caveats, Caveat::LowConfidence);
        }
        if let Some(reason) = scraped {
            log.record(
                record,
                reason,
                format!(
                    "scraped evidence (reported_percent {} of {}): a caveat on the row, never a plotted number",
                    reported_percent(record).unwrap_or_else(|| "-".to_owned()),
                    reported_measure_of(record).unwrap_or_else(|| "-".to_owned()),
                ),
            );
            continue;
        }
        eligible.push(record.clone());
    }
    if eligible.is_empty() {
        return (None, caveats);
    }

    let totals = run_totals(&eligible, |record, reason, detail| {
        log.record(record, reason, detail);
    });

    let mut sum_runs: BTreeSet<String> = BTreeSet::new();
    let mut vacuous_runs: BTreeSet<String> = BTreeSet::new();
    for (run, measures) in &totals {
        for total in measures.values() {
            if total.source == TotalSource::SumOfFileRecords {
                sum_runs.insert(run.clone());
            }
            if total.found == 0 {
                vacuous_runs.insert(run.clone());
            }
        }
    }
    if !sum_runs.is_empty() {
        push_caveat(reader, &mut caveats, Caveat::TotalFromFileSum);
    }
    if !vacuous_runs.is_empty() {
        push_caveat(reader, &mut caveats, Caveat::Vacuous);
    }

    // One pool per measure, numerators and denominators summed separately (P5, P7). Pooling across
    // runs is a size-weighted mean of their rates, which is the property that makes it safe to pool
    // two jobs without knowing whether they measured the same files or disjoint ones.
    let mut pools: BTreeMap<Measure, Pool> = BTreeMap::new();
    for (run, measures) in &totals {
        for (measure, total) in measures {
            let pool = pools.entry(*measure).or_default();
            pool.hit += total.hit;
            pool.found += total.found;
            pool.runs.insert(run.clone());
        }
    }

    let order = reader.model.measure_order().unwrap_or_default();
    let present: Vec<Measure> = order
        .into_iter()
        .filter(|measure| pools.contains_key(measure))
        .collect();
    let Some(primary) = present
        .iter()
        .copied()
        .find(|measure| pools[measure].found > 0)
    else {
        // Every pooled denominator was 0. That is *vacuous*, not 0 %, and it is not a number:
        // printing 100 % for "the only thing instrumented was an empty generated file" would be the
        // single most misleading thing this column could do (ADR-0008 §3.4).
        return (None, caveats);
    };

    let decimals = reader.model.coverage.percent_decimals;
    let pool = &pools[&primary];
    let mut block = Coverage {
        percent: rate(pool.hit, pool.found, decimals),
        found: pool.found,
        hit: pool.hit,
        measure: primary,
        runs: pool.runs.iter().cloned().collect(),
        secondary: Vec::new(),
    };
    for measure in present
        .iter()
        .copied()
        .filter(|measure| *measure != primary)
    {
        let other = &pools[&measure];
        if other.found == 0 {
            continue;
        }
        block.secondary.push(SecondaryMeasure {
            measure,
            percent: rate(other.hit, other.found, decimals),
            found: other.found,
            hit: other.hit,
        });
    }
    if !block.secondary.is_empty() {
        push_caveat(reader, &mut caveats, Caveat::SecondaryMeasures);
    }

    // Records of a non-primary measure were selected and retained and simply do not belong in *this*
    // number — which is why `measure_not_primary` is an aggregate-stage drop and not a retention one.
    for record in &eligible {
        let pairs = count_pairs(record);
        if !pairs.is_empty() && !pairs.iter().any(|(measure, ..)| *measure == primary) {
            let measures: Vec<String> = pairs
                .iter()
                .map(|(measure, ..)| measure.to_string())
                .collect();
            log.record(
                record,
                DropReason::MeasureNotPrimary,
                format!(
                    "measures {}; this commit's primary measure is {primary}",
                    measures.join("/")
                ),
            );
            push_caveat(reader, &mut caveats, Caveat::MeasureNotPrimary);
        }
    }
    (Some(block), caveats)
}

fn reported_measure_of(record: &ReadRecord) -> Option<String> {
    match &record.record {
        act_evidence::EvidenceRecord::Coverage(line) => line
            .counts
            .reported_measure
            .map(|measure| measure.as_str().to_owned()),
        act_evidence::EvidenceRecord::Benchmark(_) => None,
    }
}

/// The one legal percentage: `round_half_away(100 · Σhit / Σfound, decimals)` from raw counts
/// only, never an average of per-file or per-run percentages and never from a rounded intermediate.
fn rate(hit: u64, found: u64, decimals: u32) -> f64 {
    round_half_away(100.0 * hit as f64 / found as f64, decimals)
}

// --------------------------------------------------------------------- stage 4: benchmarks

/// Every group's spread at one commit, over **every retained run** — superseded runs included.
///
/// The headline of a group comes from one run (the newest) and its error bar from all of them. That
/// asymmetry is deliberate: "best current estimate, and here is how much the runs disagree".
pub(super) fn spreads_of(retained: &[ReadRecord]) -> BTreeMap<Group, Spread> {
    let mut spreads: BTreeMap<Group, Spread> = BTreeMap::new();
    for record in retained
        .iter()
        .filter(|record| record.kind() == Kind::Benchmark)
    {
        let spread = spreads.entry(group_of(record)).or_default();
        spread.values.push(value_of(record));
        let run = record.envelope().run_id.to_string();
        if !spread.runs.contains(&run) {
            spread.runs.push(run);
        }
        let source = source_of(record);
        if !spread.sources.contains(&source) {
            spread.sources.push(source);
        }
    }
    spreads
}

/// The benchmark rows: value from the newest selected run, spread from all of them.
pub(super) fn benchmarks_of(
    reader: &Reader,
    selected: &[ReadRecord],
    spreads: &BTreeMap<Group, Spread>,
) -> (Vec<BenchmarkGroup>, Vec<Caveat>) {
    let mut caveats: Vec<Caveat> = Vec::new();
    let mut groups: BTreeMap<Group, BTreeMap<String, Vec<&ReadRecord>>> = BTreeMap::new();
    for record in selected
        .iter()
        .filter(|record| record.kind() == Kind::Benchmark)
    {
        groups
            .entry(group_of(record))
            .or_default()
            .entry(record.envelope().run_id.to_string())
            .or_default()
            .push(record);
    }

    let mut order: Vec<&Group> = groups.keys().collect();
    order.sort_by_key(|left| group_sort_key(left));

    let mut rows: Vec<BenchmarkGroup> = Vec::new();
    for key in order {
        let (name, labels_json, unit) = (key.0.clone(), key.1.clone(), key.2.clone());
        let by_run = &groups[key];
        let newest = by_run.keys().last().cloned().unwrap_or_default();
        let newest_records = by_run.get(&newest).map_or(&[][..], Vec::as_slice);
        let values: Vec<Number> = newest_records
            .iter()
            .map(|record| value_of(record))
            .collect();
        let spread = spreads.get(key).cloned().unwrap_or_else(|| Spread {
            values: values.clone(),
            runs: vec![newest.clone()],
            sources: newest_records.iter().map(source_of_ref).collect(),
        });
        let mut new_sources: Vec<String> = newest_records.iter().map(|r| source_of(r)).collect();
        new_sources.sort();
        new_sources.dedup();
        let mut flags: Vec<String> = newest_records.iter().flat_map(|r| flags_of(r)).collect();
        flags.sort();
        flags.dedup();
        let context = !newest_records.is_empty()
            && newest_records
                .iter()
                .all(|record| reader.model.is_context_source(&source_of(record)));

        let row = BenchmarkGroup {
            name,
            labels: serde_json::from_str::<BTreeMap<String, String>>(&labels_json)
                .unwrap_or_default(),
            comparable: unit.is_some(),
            context,
            value: median(&values).unwrap_or_else(|| Number::from(0)),
            samples: newest_records.len(),
            min: extremum(&spread.values, true).unwrap_or_else(|| Number::from(0)),
            max: extremum(&spread.values, false).unwrap_or_else(|| Number::from(0)),
            run_count: spread.runs.len(),
            run_id: newest,
            source: new_sources.first().cloned().unwrap_or_default(),
            flags,
            unit,
        };
        if row.unit.is_none() {
            // An absent unit is a group-key value, not a missing value: the row still has no `unit`
            // key (never a null), and the caveat is what tells a renderer not to draw a delta.
            push_caveat(reader, &mut caveats, Caveat::UnitMissing);
        }
        if spread.sources.len() > 1 {
            push_caveat(reader, &mut caveats, Caveat::SourceChanged);
        }
        if row.context {
            push_caveat(reader, &mut caveats, Caveat::ContextOnly);
        }
        rows.push(row);
    }
    (rows, caveats)
}

fn source_of_ref(record: &&ReadRecord) -> String {
    source_of(record)
}

/// The row order: `(name, canonical labels, str(unit))`, matching the oracle's sort exactly,
/// including `None` sorting as the text `"None"` (so an absent unit lands where the oracle puts it).
fn group_sort_key(group: &Group) -> (String, String, String) {
    (
        group.0.clone(),
        group.1.clone(),
        group.2.clone().unwrap_or_else(|| "None".to_owned()),
    )
}

/// `min` or `max` over the spread, returning the sample's own number so an integral metric stays
/// integral instead of becoming `3.0`.
fn extremum(values: &[Number], lowest: bool) -> Option<Number> {
    values
        .iter()
        .reduce(|best, candidate| {
            let better = if lowest {
                num(candidate) < num(best)
            } else {
                num(candidate) > num(best)
            };
            if better {
                candidate
            } else {
                best
            }
        })
        .cloned()
}

/// A JSON number as an `f64`, for ordering only — never for a stored value, which keeps the number
/// it came in as (an integer metric stays `3`, not `3.0`).
fn num(value: &Number) -> f64 {
    value.as_f64().unwrap_or(0.0)
}

// --------------------------------------------------------------------- stage 4: action

/// The four states, and the per-group verdicts that explain them (ADR-0008 §5.3).
///
/// Decision order, and it is what makes the states exclusive: no records at all ⇒ `no_evidence`;
/// every group's newest run succeeded ⇒ `success`; none did ⇒ `failed`; otherwise `mixed`. A
/// re-run is a pass rather than a mixed, because the verdict is the *newest* run's — which is what
/// people expect from a CI badge and the opposite of what a naive "any failure ⇒ failure" gives.
pub(super) fn action_of(
    reader: &Reader,
    readable: &[ReadRecord],
) -> (ActionState, Vec<ActionGroup>) {
    if readable.is_empty() {
        return (ActionState::NoEvidence, Vec::new());
    }
    let mut groups: BTreeMap<(String, String, String), Vec<&ReadRecord>> = BTreeMap::new();
    for record in readable {
        groups
            .entry(selection_key(record))
            .or_default()
            .push(record);
    }
    let mut detail = Vec::new();
    let mut verdicts: Vec<bool> = Vec::new();
    for group in groups.values() {
        let newest = group
            .iter()
            .map(|record| record.envelope().run_id.to_string())
            .max()
            .unwrap_or_default();
        let winner = *group
            .iter()
            .find(|record| record.envelope().run_id.to_string() == newest)
            .unwrap_or(&group[0]);
        let ok = reader.model.is_success(&status_of(winner));
        verdicts.push(ok);
        let envelope = winner.envelope();
        detail.push(ActionGroup {
            kind: envelope.kind().as_str().to_owned(),
            workflow: envelope.workflow.clone(),
            task: envelope.task.clone(),
            category: envelope.category.clone(),
            status: status_of(winner),
            run_id: newest,
            ok,
        });
    }
    let state = if verdicts.iter().all(|ok| *ok) {
        ActionState::Success
    } else if verdicts.iter().any(|ok| *ok) {
        ActionState::Mixed
    } else {
        ActionState::Failed
    };
    (state, detail)
}

// --------------------------------------------------------------------- the row

/// Derive one commit's row, logging every drop and every kept record on the way.
pub(super) fn derive_commit(
    reader: &Reader,
    sha: &str,
    readable: &[ReadRecord],
    retained: &[ReadRecord],
    unreadable: u64,
    explain: &mut Explain,
) -> CommitMetrics {
    let (selected, superseded) = select(retained);
    let mut caveats: Vec<Caveat> = Vec::new();

    let coverage = {
        let mut log = Log {
            sha,
            explain: &mut *explain,
        };
        let (block, cov_caveats) = coverage_of(reader, &selected, &mut log);
        for caveat in cov_caveats {
            push_caveat(reader, &mut caveats, caveat);
        }
        block
    };
    let spreads = spreads_of(retained);
    let (benchmarks, bench_caveats) = benchmarks_of(reader, &selected, &spreads);
    for caveat in bench_caveats {
        push_caveat(reader, &mut caveats, caveat);
    }
    let (action_success, action_detail) = action_of(reader, readable);

    // Excluded runs: every run that happened and was not used, named. §3.7's whole point is that
    // filtering a broken run out of a number must not filter it out of the report too.
    let mut excluded: Vec<ExcludedRun> = Vec::new();
    let mut seen: BTreeSet<(String, DropReason)> = BTreeSet::new();
    for drop in explain.drops_for(sha) {
        if matches!(
            drop.reason,
            DropReason::RunStatusExcluded | DropReason::RunTruncated
        ) {
            if let Some(run_id) = drop.run_id.clone() {
                let key = (run_id.clone(), drop.reason);
                if seen.insert(key) {
                    excluded.push(ExcludedRun {
                        run_id,
                        status: drop.status.clone().unwrap_or_default(),
                        reason: drop.reason,
                    });
                    push_caveat(reader, &mut caveats, Caveat::RunExcluded);
                }
            }
        }
    }
    excluded.sort_by(|left, right| {
        left.run_id
            .cmp(&right.run_id)
            .then_with(|| left.reason.cmp(&right.reason))
    });

    if unreadable > 0 {
        push_caveat(reader, &mut caveats, Caveat::Unreadable);
    }

    let superseded_runs: Vec<String> = {
        let mut ids: BTreeSet<String> = BTreeSet::new();
        for record in &superseded {
            let mut log = Log {
                sha,
                explain: &mut *explain,
            };
            log.record(
                record,
                DropReason::Superseded,
                "an older run of the same (kind, workflow, task)",
            );
            ids.insert(record.envelope().run_id.to_string());
        }
        ids.into_iter().collect()
    };
    if !superseded_runs.is_empty() {
        push_caveat(reader, &mut caveats, Caveat::SupersededRuns);
    }

    keep_records(
        reader, sha, retained, &selected, &coverage, &spreads, explain,
    );

    let mut gaps = Gaps::default();
    if coverage.is_none() {
        gaps.coverage = Some(gap_code(
            reader,
            explain,
            sha,
            readable,
            Column::Coverage,
            &caveats,
            unreadable,
        ));
    }
    if benchmarks.is_empty() {
        gaps.benchmarks = Some(gap_code(
            reader,
            explain,
            sha,
            readable,
            Column::Benchmarks,
            &caveats,
            unreadable,
        ));
    }

    explain.set_coverage_totals(sha, coverage_detail(readable, coverage.as_ref()));
    explain.set_spreads(
        sha,
        spreads
            .iter()
            .map(|(group, spread)| BenchmarkSpread {
                name: group.0.clone(),
                labels: group.1.clone(),
                unit: group.2.clone(),
                values: spread.values.clone(),
                runs: {
                    let mut runs = spread.runs.clone();
                    runs.sort();
                    runs
                },
                sources: spread.sources.clone(),
            })
            .collect(),
    );

    CommitMetrics {
        commit: sha.to_owned(),
        action_success,
        coverage,
        benchmarks,
        action_detail,
        excluded_runs: excluded,
        superseded_runs,
        unreadable_records: unreadable,
        gaps,
        caveats,
    }
}

/// One line per retained record: was its run the selected one, and what did it feed?
#[allow(
    clippy::too_many_lines,
    reason = "one match arm per fate a retained record can have"
)]
fn keep_records(
    reader: &Reader,
    sha: &str,
    retained: &[ReadRecord],
    selected: &[ReadRecord],
    coverage: &Option<Coverage>,
    spreads: &BTreeMap<Group, Spread>,
    explain: &mut Explain,
) {
    for record in retained {
        let selected_run = selected
            .iter()
            .any(|candidate| candidate.location() == record.location());
        let note = if record.kind() == Kind::Coverage {
            let measures: Vec<String> = count_pairs(record)
                .iter()
                .map(|(measure, ..)| measure.to_string())
                .collect();
            let scope = if is_total(record) { "total" } else { "file" };
            match coverage {
                Some(block)
                    if selected_run
                        && block.runs.contains(&record.envelope().run_id.to_string())
                        && count_pairs(record)
                            .iter()
                            .any(|(measure, ..)| *measure == block.measure) =>
                {
                    format!(
                        "pooled into {scope} {measures:?} → {} {}",
                        block.measure, block.percent
                    )
                }
                _ if selected_run => format!("selected, not pooled: {scope} {measures:?}"),
                _ => "superseded run: kept for history, not the number".to_owned(),
            }
        } else {
            let group = group_of(record);
            let spread = spreads.get(&group);
            let newest = spread.is_some_and(|_| {
                selected
                    .iter()
                    .any(|candidate| candidate.location() == record.location())
            });
            let unit = group.2.as_deref().unwrap_or("unit_missing");
            if newest {
                format!(
                    "group {} {} [{unit}]: the newest run, so the value",
                    group.0, group.1
                )
            } else {
                format!("group {} {} [{unit}]: spread only", group.0, group.1)
            }
        };
        let _ = reader;
        explain.push_kept(Kept {
            sha: sha.to_owned(),
            location: record.location(),
            kind: record.kind().as_str().to_owned(),
            handle: handle_of(record),
            run_id: record.envelope().run_id.to_string(),
            status: status_of(record),
            selected: selected_run,
            note,
        });
    }
}

/// Per-run coverage totals over **every readable** coverage record — the runs that were used and
/// the runs that were not, which is what the detail view shows and what recipe 2 of
/// `docs/evidence-format.md` prints.
fn coverage_detail(readable: &[ReadRecord], coverage: Option<&Coverage>) -> Vec<CoverageRunTotal> {
    let records: Vec<ReadRecord> = readable
        .iter()
        .filter(|record| record.kind() == Kind::Coverage)
        .cloned()
        .collect();
    let totals = run_totals(&records, |_, _, _| {});
    let mut out = Vec::new();
    for (run, measures) in totals {
        for (measure, total) in measures {
            let record_used =
                coverage.is_some_and(|block| block.runs.contains(&run) && block.measure == measure);
            let status = readable
                .iter()
                .find(|record| record.envelope().run_id.to_string() == run)
                .map_or_else(String::new, status_of);
            let source = readable
                .iter()
                .find(|record| record.envelope().run_id.to_string() == run)
                .map_or_else(String::new, source_of);
            out.push(CoverageRunTotal {
                run_id: run.clone(),
                run_status: status,
                source,
                measure,
                hit: total.hit,
                found: total.found,
                total_source: total.source,
                used: record_used,
            });
        }
    }
    out
}

/// Why a column has no number, from the closed per-column list, first match wins.
///
/// The priority is "the most specific thing that actually happened": a commit whose only coverage
/// run was a scraped percentage is `only_percent_only` and not `no_records`, because the fixable
/// sentence is "go make it write an lcov", and `—` alone says nothing either way.
fn gap_code(
    reader: &Reader,
    explain: &Explain,
    sha: &str,
    readable: &[ReadRecord],
    column: Column,
    caveats: &[Caveat],
    unreadable: u64,
) -> GapCode {
    let reasons = explain.reasons_for(sha);
    let has = |reason: DropReason| reasons.contains(&reason);
    let excluded = has(DropReason::RunStatusExcluded) || has(DropReason::RunTruncated);
    let code = if readable.is_empty() {
        if unreadable > 0 {
            GapCode::Unreadable
        } else {
            GapCode::NoEvidence
        }
    } else if column == Column::Coverage {
        if caveats.contains(&Caveat::PercentOnly) || caveats.contains(&Caveat::LowConfidence) {
            GapCode::OnlyPercentOnly
        } else if caveats.contains(&Caveat::Vacuous) {
            GapCode::Vacuous
        } else if excluded {
            GapCode::AllRunsExcluded
        } else if has(DropReason::NoCounts) || has(DropReason::MeasureNotPrimary) {
            GapCode::NoCounts
        } else {
            GapCode::NoRecords
        }
    } else if has(DropReason::ConfigFiltered) {
        GapCode::ConfigFiltered
    } else if excluded {
        GapCode::AllRunsExcluded
    } else {
        GapCode::NoRecords
    };
    debug_assert!(
        reader
            .model
            .declared_gaps(column)
            .iter()
            .any(|declared| declared == code.as_str()),
        "gap code {code} is not declared for {column}"
    );
    code
}

/// Append a caveat unless it is already there, and refuse a code the model does not declare.
fn push_caveat(reader: &Reader, into: &mut Vec<Caveat>, caveat: Caveat) {
    debug_assert!(
        reader
            .model
            .declared_caveats()
            .iter()
            .any(|declared| declared == caveat.as_str()),
        "caveat {caveat} is not declared by the model"
    );
    if !into.contains(&caveat) {
        into.push(caveat);
    }
}
