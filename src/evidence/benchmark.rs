//! Benchmark extraction: a finished act run → `.evidence/benchmark` records (mact-f1w.12,
//! ADR-0002 §3.2, §3.3, §4.1, §4.4, §5.1, §5.2, §5.3).
//!
//! [`extract`] is the entry point, and like
//! [`crate::evidence::coverage::extract`] it is a function of two things that already exist
//! when a run finishes: the runner's terminal
//! [`crate::evidence::benchmark::RunOutcome`] and the
//! [`crate::workspace::ValidatedRunTarget`] the run was validated against. Nothing is
//! spawned, nothing is published, and no problem found here changes a run's status —
//! extraction problems are data (ADR-0002 §5.1).
//!
//! ## The three producers, and what each one is for
//!
//! ```text
//! $ACTMCP_EVIDENCE_OUT/benchmark.{json,jsonl} ─┐ source: job_file        (authoritative)
//!                                              │
//! act.jsonl ── ACTMCP_BENCHMARK lines ─────────┤ source: in_band_stdout  (convenience)
//!          └─ executionTime / time per step ───┘ source: act_log         (context, always)
//! ```
//!
//! Every record says which of the three it came from (`source`), and the three are merged in
//! that order of precedence:
//!
//! 1. **The payload file** is what a job wrote through the bind mount, and it wins. A
//!    second benchmark-named file is reported (`benchmark_source_shadowed`) rather than
//!    blended, for §3.2's reason: one run, one source of numbers.
//! 2. **Marker lines** (§5.2) are for a job that cannot write a file. One that also appears
//!    in the payload with the same name, labels and value is a duplicate and is dropped, with
//!    the payload's copy surviving as `job_file`.
//! 3. **act's own timings** are emitted for *every* benchmark run, so a job that prints
//!    nothing at all still leaves "did it run, and how long did it take" answerable in the
//!    same file (§5.3). `step_duration_s` comes from each step's `executionTime`;
//!    `job_duration_s` is the sum of that job's step times, falling back to the span of the
//!    run's log lines when act reported no step timings (a run killed mid-step never logged
//!    any). Sum rather than wall-clock because `executionTime` is the only sub-second timing
//!    act emits — its log timestamps are whole seconds — and a job that times its own
//!    `job_duration_s` has that value kept and act's suppressed, so a duration is never
//!    counted twice (§5.3).
//!
//! ## The rules this module exists to keep
//!
//! 1. **A number is only a number if it parses as one.** Non-finite, non-numeric and
//!    out-of-range values are refused and counted in `malformed`; so is a negative value on
//!    a `*_duration_s` metric, while a negative delta stays legal (§4.1).
//! 2. **Units are never invented or converted.** A unit is copied verbatim; no unit is
//!    recorded as absence plus `unit_missing`, because a scanner diffs units across commits
//!    and an invented `count` or a silent `ms→s` would silently move a history (§4.1).
//! 3. **The work is bounded.** 8 MiB of payload, 5 000 metrics per run, 5 000 marker lines,
//!    32 MiB of log. Hitting the metric cap is not silent: every record of the run carries
//!    `truncated` and the summary says so (§3.3, §5.1). There is no separate "truncation
//!    record", because ADR-0002 rejected a third `kind` and `benchmark/` must stay a
//!    directory of benchmark records.
//! 4. **Duplicates within a run are deduped; genuine repeats are kept.** Same name, labels,
//!    value and source is one record; the same name and labels with different values are
//!    samples and are numbered `sample: 1..n` (§4.4).
//! 5. **A failed run's metrics are still written** — with the run's real status, so a
//!    regression caused by a broken run is filterable rather than missing (§5.1). Identity
//!    (`run_id`, `commit`, `workflow`, `task`) comes from the tool call and never from a
//!    file's contents (§3.3).
//! 6. **Malformed input is counted, never fatal.** A garbage marker line, an unreadable
//!    payload, an unparseable file: all are warnings and counts on the summary, and the run
//!    keeps the status act gave it.

mod act_log;
mod entry;
mod marker;
mod payload;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

use thiserror::Error;

use crate::{
    evidence::{
        coverage::status_word,
        records::{
            BenchmarkMetric, BenchmarkRecord, BenchmarkSource, Commit, Envelope, Kind, MetricValue,
            RecordError, RelPath, RunId, RunRecords, Tool, UtcInstant, FLAG_TRUNCATED,
        },
        ExtractContext,
    },
    runner::{ActRunOutcome, WARNING_PAYLOAD_TRANSPORT_UNAVAILABLE},
    workspace::ValidatedRunTarget,
};

/// The ticket's `RunOutcome`, by the name the ticket uses.
pub type RunOutcome = ActRunOutcome;

/// Warning: the payload exceeded [`MAX_BENCHMARK_BYTES`] (§3.3).
pub const WARNING_PAYLOAD_TOO_LARGE: &str = "payload_too_large";
/// Warning: the payload existed and could not be opened or read (§3.1).
pub const WARNING_PAYLOAD_UNREADABLE: &str = "payload_unreadable";
/// Warning: more than one benchmark-named payload existed (§3.2).
pub const WARNING_BENCHMARK_SOURCE_SHADOWED: &str = "benchmark_source_shadowed";
/// Warning: entries or marker lines were present and unusable (§3.3, §5.2).
pub const WARNING_MALFORMED_METRIC: &str = "malformed_metric";
/// Warning: more marker lines existed than [`MAX_MARKER_LINES`] (§5.2).
pub const WARNING_MARKER_CAP_REACHED: &str = "marker_cap_reached";
/// Warning: more metrics existed than [`MAX_BENCHMARK_METRICS`] (§3.3, §5.1).
pub const WARNING_TRUNCATED: &str = "truncated";
/// Warning: the payload was not parseable at all (§5.1's word for exactly that).
pub const WARNING_PARSE_FAILED: &str = "parse_failed";
/// Warning: nothing at all — not the payload, not the markers, not act's log — produced a
/// metric, so no file is written for this run (§1.3).
pub const WARNING_NO_BENCHMARK_METRICS: &str = "no_benchmark_metrics";
/// Warning: act's captured log could not be read, so no timings could be derived.
pub const WARNING_LOG_UNREADABLE: &str = "act_log_unreadable";
/// Warning: act's log ended in our own truncation marker, so any timing after that point
/// is missing rather than absent.
pub const WARNING_LOG_TRUNCATED: &str = "act_log_truncated";

/// ADR-0002 §3.3's payload cap, applied to a benchmark file exactly as coverage applies it.
pub const MAX_BENCHMARK_BYTES: u64 = 8 * 1024 * 1024;
/// The metrics-per-run cap (§3.3): beyond it the run's records are truncated and flagged.
pub const MAX_BENCHMARK_METRICS: usize = 5_000;
/// The marker-lines cap (§5.2), which bounds the *input*, unlike the metric cap, which
/// bounds the output.
pub const MAX_MARKER_LINES: usize = 5_000;
/// How much of `act.jsonl` one scan reads.
pub const MAX_BENCHMARK_LOG_BYTES: u64 = 32 * 1024 * 1024;

/// The built-in metric naming this run's job time (ADR-0002 §5.3).
pub const JOB_DURATION: &str = "job_duration_s";
/// The built-in metric naming one step's time (§5.3).
pub const STEP_DURATION: &str = "step_duration_s";
/// The unit both built-ins carry: the name says seconds, so the value says seconds (§5.3).
pub const DURATION_UNIT: &str = "s";

/// Where this run's records came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractionOutcome {
    /// A payload file produced at least one metric.
    Extracted,
    /// Only markers and/or act's timings produced records.
    Derived,
    /// A payload existed and could not be used (unreadable, oversized, unparseable, or of
    /// the wrong shape). Whatever the log produced is still there.
    ParseFailed,
    /// Nothing produced anything, so nothing is published (§1.3).
    NoMetrics,
}

impl ExtractionOutcome {
    /// The wire spelling, which doubles as the `meta.json` warning code for the two failure
    /// cases (§5.1).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Extracted => "extracted",
            Self::Derived => "derived",
            Self::ParseFailed => "parse_failed",
            Self::NoMetrics => "no_benchmark_metrics",
        }
    }
}

impl std::fmt::Display for ExtractionOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One benchmark-named file that existed, whether or not it was used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovered {
    /// Workspace-relative spelling of the path.
    pub path: String,
    /// `json` or `jsonl` — §3.3's two shapes, chosen by filename.
    pub format: &'static str,
    /// Whether these records came from it.
    pub used: bool,
    /// Its size, when it could be stat'ed.
    pub bytes: Option<u64>,
}

/// What extraction produced — the `meta.json` and tool-result view of one run's benchmark.
#[derive(Debug, Clone)]
pub struct BenchmarkSummary {
    /// The one-word answer.
    pub outcome: ExtractionOutcome,
    /// The records, in file order: job-file metrics, then marker metrics, then act's
    /// timings (§6.1's order is about keys, not lines; this order is precedence, so the
    /// authoritative numbers lead).
    pub records: Vec<BenchmarkRecord>,
    /// The payload file the `job_file` records came from, workspace-relative.
    pub source_path: Option<RelPath>,
    /// Every benchmark-named file that existed.
    pub found: Vec<Discovered>,
    /// Machine-readable caveats — `meta.json`'s `warnings[]` vocabulary (§5.1).
    pub warnings: Vec<String>,
    /// Entries and marker lines that were present and unusable: `evidence[].malformed`.
    pub malformed: usize,
    /// Metrics dropped as repeats of one already kept (§4.4). Advisory, for `meta.json`.
    pub duplicates: usize,
    /// The metric cap was reached, so the run's records are incomplete and every one of
    /// them carries `truncated` (§3.3, §5.1).
    pub truncated: bool,
    /// How many records came from each producer, so `meta.json` can say which of the three
    /// a run actually used without counting sources in the reader.
    pub from: SourceCounts,
    /// A human-readable note about a failure, for `meta.json`.
    pub detail: Option<String>,
}

impl BenchmarkSummary {
    /// The batch to publish, or `None` when there is nothing to publish (§1.3: a run that
    /// produced no records is recorded by `meta.json`, not by an empty file).
    #[must_use]
    pub fn run_records(&self) -> Option<RunRecords> {
        if self.records.is_empty() {
            return None;
        }
        Some(RunRecords::Benchmark {
            records: self.records.clone(),
        })
    }

    /// Whether there is nothing to publish.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// How many records there are.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// The warning codes for `meta.json` and the tool result: the outcome's own code first,
    /// then the counting ones, deduplicated in first-seen order so the two places cannot
    /// disagree about the same run.
    #[must_use]
    pub fn warning_codes(&self) -> Vec<String> {
        let mut codes: Vec<String> = Vec::new();
        for code in self.warnings.iter().map(String::as_str) {
            if !codes.iter().any(|held| held == code) {
                codes.push(code.to_owned());
            }
        }
        codes
    }
}

/// How many records each producer contributed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SourceCounts {
    /// Metrics read from the payload file.
    pub job_file: usize,
    /// Metrics scraped out of `ACTMCP_BENCHMARK` lines.
    pub in_band_stdout: usize,
    /// Durations derived from act's own `--json` timings.
    pub act_log: usize,
}

/// Why extraction could not produce anything at all.
///
/// There is only one such reason. Everything a job can get wrong — a payload that is not
/// JSON, a marker line that is not JSON, a value that is not a number — is reported inside
/// [`BenchmarkSummary`], because an evidence problem is not the job's failure (§5.1).
#[derive(Debug, Error)]
pub enum ExtractError {
    /// The run's identity could not become an envelope: a bug in the caller or the runner,
    /// since `run_id` and `commit` are server-generated.
    #[error("this run's identity could not be written as evidence: {0}")]
    Envelope(#[from] RecordError),
}

impl ExtractError {
    /// The warning code a caller stamps when this is what happened.
    #[must_use]
    pub const fn warning_code(&self) -> &'static str {
        match self {
            Self::Envelope(_) => WARNING_PARSE_FAILED,
        }
    }
}

/// Turn a finished run's benchmark payload and act's timings into records, with the default
/// [`ExtractContext`] for `target`.
///
/// # Errors
///
/// [`ExtractError::Envelope`] when the run's identity is not writable as evidence. Everything
/// else is reported inside the returned [`BenchmarkSummary`].
pub fn extract(
    outcome: &RunOutcome,
    target: &ValidatedRunTarget,
) -> Result<BenchmarkSummary, ExtractError> {
    extract_with(
        outcome,
        target,
        &ExtractContext::for_run(target, &outcome.run_id),
    )
}

/// [`extract`] with an explicit [`ExtractContext`].
///
/// # Errors
///
/// As [`extract`].
pub fn extract_with(
    outcome: &RunOutcome,
    target: &ValidatedRunTarget,
    context: &ExtractContext,
) -> Result<BenchmarkSummary, ExtractError> {
    let envelope = envelope_for(outcome, target, context)?;
    let mut extraction = Extraction::new(envelope, target.job_id.clone(), context);
    if outcome.has_warning(WARNING_PAYLOAD_TRANSPORT_UNAVAILABLE) {
        // `--no-bind`: the job could not write a payload at all, which is the one
        // explanation worth repeating next to an empty `incoming/`.
        extraction.push_warning(WARNING_PAYLOAD_TRANSPORT_UNAVAILABLE);
    }
    extraction.run(outcome);
    extraction.settle();
    Ok(extraction.summary)
}

/// What the payload step concluded, which is what decides the outcome word.
#[derive(Debug)]
enum PayloadState {
    /// No benchmark-named file existed.
    Absent,
    /// One existed, had the right shape, and held nothing.
    Empty,
    /// One existed and could not be used at all.
    Failed,
    /// At least one entry became a metric.
    Read,
}

/// A benchmark-named file that existed.
#[derive(Clone)]
struct PayloadFile {
    path: PathBuf,
    /// The workspace-relative spelling a record cites.
    display: String,
    format: payload::Format,
    bytes: Option<u64>,
}

/// A metric on its way to becoming a record: parsed, attributed, not yet deduped or numbered.
struct Candidate {
    /// `0` payload file, `1` marker, `2` act log. This is both the precedence of §5.2 and
    /// the order the lines come out in, which is why one number does both.
    rank: u8,
    metric: BenchmarkMetric,
}

/// What identifies a metric for dedup and sampling: `(name, labels)` plus the value as text
/// (ADR-0002 §4.4). Text rather than the number type because `MetricValue` keeps `3` and
/// `3.0` as different JSON numbers, and a repeat written a different way is still the same
/// measurement.
fn identity(metric: &BenchmarkMetric) -> (String, String, String) {
    (
        metric.name.clone(),
        metric.labels.as_ref().map_or_else(String::new, |labels| {
            serde_json::to_string(labels).unwrap_or_default()
        }),
        metric.value.to_string(),
    )
}

/// The same identity without the value: "the same measurement", which is what a job's own
/// duration suppresses act's derived one by (§5.3).
fn measurement(metric: &BenchmarkMetric) -> (String, Option<String>, Option<String>) {
    let label = |key: &str| {
        metric
            .labels
            .as_ref()
            .and_then(|labels| labels.get(key))
            .cloned()
    };
    (metric.name.clone(), label("job"), label("step"))
}

/// One run's extraction, in progress.
struct Extraction<'context> {
    envelope: Envelope,
    /// This run's job id: stamped as `labels.job` on anything a job left unlabelled, and
    /// the filter that keeps another job's log lines out of this run's timings.
    job_id: String,
    context: &'context ExtractContext,
    summary: BenchmarkSummary,
    candidates: Vec<Candidate>,
    payload_files: Vec<PayloadFile>,
    payload: PayloadState,
    /// The span act's own timestamps covered, used only when no step reported a time.
    span_ms: Option<u64>,
}

impl<'context> Extraction<'context> {
    fn new(envelope: Envelope, job_id: String, context: &'context ExtractContext) -> Self {
        Self {
            envelope,
            job_id,
            context,
            summary: BenchmarkSummary {
                outcome: ExtractionOutcome::NoMetrics,
                records: Vec::new(),
                source_path: None,
                found: Vec::new(),
                warnings: Vec::new(),
                malformed: 0,
                duplicates: 0,
                truncated: false,
                from: SourceCounts::default(),
                detail: None,
            },
            candidates: Vec::new(),
            payload_files: Vec::new(),
            payload: PayloadState::Absent,
            span_ms: None,
        }
    }

    fn push_warning(&mut self, code: &str) {
        if !self.summary.warnings.iter().any(|held| held == code) {
            self.summary.warnings.push(code.to_owned());
        }
    }

    /// The first human-readable note wins: it is the one that explains a missing file, and a
    /// later technicality should not crowd it out.
    fn note(&mut self, detail: impl Into<String>) {
        let detail = detail.into();
        if self.summary.detail.is_none() {
            self.summary.detail = Some(detail);
        }
    }

    fn run(&mut self, outcome: &RunOutcome) {
        self.discover();
        self.read_payload();
        self.scan_log(outcome);
    }

    /// The files §3.2's benchmark row names, in precedence order: `$ACTMCP_EVIDENCE_OUT`
    /// first, because the place we told the job to write is the place that must win.
    ///
    /// The directory outranks the filename: a run that wrote `benchmark.jsonl` into the bind
    /// mount beats a stale `benchmark.json` sitting at the tree root, even though `.json` is
    /// the higher-precedence *name*. The two names are alternative spellings of one kind of
    /// file (§3.3), not two sources to rank against each other.
    fn discover(&mut self) {
        for root in [&self.context.payload_dir, &self.context.tree_root] {
            for name in PAYLOAD_NAMES {
                let Some(format) = payload::Format::for_name(name) else {
                    continue;
                };
                let path = root.join(name);
                let Ok(meta) = std::fs::metadata(&path) else {
                    continue;
                };
                if !meta.is_file() {
                    continue;
                }
                self.payload_files.push(PayloadFile {
                    display: crate::evidence::coverage::display_path(&path, self.context),
                    path,
                    format,
                    bytes: Some(meta.len()),
                });
            }
        }
        if self.payload_files.len() > 1 {
            // One source wins, and the fact that another existed is data (§3.2).
            self.push_warning(WARNING_BENCHMARK_SOURCE_SHADOWED);
        }
        self.summary.found = self
            .payload_files
            .iter()
            .map(|file| Discovered {
                path: file.display.clone(),
                format: file.format.as_str(),
                used: false,
                bytes: file.bytes,
            })
            .collect();
    }

    /// Read the winner, if any, and turn its entries into candidates.
    fn read_payload(&mut self) {
        let Some(winner) = self.payload_files.first().cloned() else {
            return;
        };
        let source_path = match RelPath::parse(&winner.display) {
            Ok(path) => path,
            Err(reason) => {
                // A payload outside both roots cannot be cited, and a `job_file` metric must
                // cite the file it came from (§4.1), so the file is reported and skipped.
                self.payload = PayloadState::Failed;
                self.push_warning(WARNING_PARSE_FAILED);
                self.note(format!(
                    "{} could not be named as a source_path: {reason}",
                    winner.display
                ));
                return;
            }
        };
        match payload::read(&winner.path, winner.format, MAX_BENCHMARK_BYTES) {
            Ok(parsed) => {
                self.summary.malformed += parsed.malformed;
                if let Some(detail) = parsed.detail {
                    // A `detail` is either "unusable file" or "usable file, nothing in it";
                    // the malformed count the reader added says which.
                    self.payload = if parsed.malformed > 0 {
                        PayloadState::Failed
                    } else {
                        PayloadState::Empty
                    };
                    self.note(format!("{}: {detail}", winner.display));
                    return;
                }
                if self.push_entries(&parsed.entries, &source_path, &winner.display) == 0 {
                    self.payload = PayloadState::Failed;
                    self.note(format!("{} held no usable metric entries", winner.display));
                    return;
                }
                self.payload = PayloadState::Read;
                self.summary.source_path = Some(source_path);
                if let Some(file) = self
                    .summary
                    .found
                    .iter_mut()
                    .find(|file| file.path == winner.display)
                {
                    file.used = true;
                }
            }
            Err(payload::ReadError::TooLarge(len)) => {
                self.payload = PayloadState::Failed;
                self.push_warning(WARNING_PAYLOAD_TOO_LARGE);
                self.note(format!(
                    "{} is {len} bytes, over the {} MiB payload cap",
                    winner.display,
                    MAX_BENCHMARK_BYTES / (1024 * 1024)
                ));
            }
            Err(payload::ReadError::Unreadable(reason)) => {
                self.payload = PayloadState::Failed;
                self.push_warning(WARNING_PAYLOAD_UNREADABLE);
                self.note(format!("{}: {reason}", winner.display));
            }
        }
    }

    /// The payload's entries, each judged on its own (§3.3).
    fn push_entries(
        &mut self,
        entries: &[serde_json::Value],
        source_path: &RelPath,
        file: &str,
    ) -> usize {
        let mut accepted = 0usize;
        for entry in entries {
            match entry::parse(entry) {
                Ok(mut metric) => {
                    metric.attributed_to(&self.job_id);
                    accepted += 1;
                    self.candidates.push(Candidate {
                        rank: 0,
                        metric: metric
                            .into_metric(BenchmarkSource::JobFile, Some(source_path.clone())),
                    });
                }
                Err(reason) => {
                    tracing::debug!(
                        run_id = %self.envelope.run_id,
                        "rejected a benchmark metric from {file}: {reason}"
                    );
                    self.summary.malformed += 1;
                }
            }
        }
        accepted
    }

    /// One pass over `act.jsonl`, for markers and for timings.
    fn scan_log(&mut self, outcome: &RunOutcome) {
        let scan = match act_log::scan(
            &outcome.log_path,
            &self.job_id,
            MAX_BENCHMARK_LOG_BYTES,
            MAX_MARKER_LINES,
        ) {
            Ok(scan) => scan,
            Err(reason) => {
                // A run always has a log; not being able to read it is worth a line and is
                // still not the job's failure.
                self.push_warning(WARNING_LOG_UNREADABLE);
                self.note(format!(
                    "{} could not be read: {reason}",
                    outcome.log_path.display()
                ));
                return;
            }
        };
        self.span_ms = scan.span_ms;
        if scan.log_truncated || scan.budget_exceeded {
            // Not the §5.1 `truncated` (that word is the metric cap's): this one says the
            // *input* ran out, so a timing may be missing rather than absent.
            self.push_warning(WARNING_LOG_TRUNCATED);
            self.note("act's log was truncated, so step timings after the cut are missing");
        }
        self.summary.malformed += scan.marker_malformed;
        if scan.marker_capped {
            self.push_warning(WARNING_MARKER_CAP_REACHED);
        }
        for marker in scan.markers {
            match entry::parse(&marker) {
                Ok(mut metric) => {
                    metric.attributed_to(&self.job_id);
                    self.candidates.push(Candidate {
                        rank: 1,
                        metric: metric.into_metric(BenchmarkSource::InBandStdout, None),
                    });
                }
                Err(reason) => {
                    tracing::debug!(run_id = %self.envelope.run_id, "rejected a marker metric: {reason}");
                    self.summary.malformed += 1;
                }
            }
        }
        self.push_durations(&scan.steps);
    }

    /// act's own durations (§5.3): one `step_duration_s` per step it timed, and one
    /// `job_duration_s` for the job.
    fn push_durations(&mut self, steps: &[act_log::StepTiming]) {
        // The job's time is the sum of its steps when act timed any: its timestamps are
        // whole seconds, so the sum is the finer number. With none — a run killed before a
        // step reported completion — the span of its log lines is what there is.
        let job_ms = if steps.is_empty() {
            self.span_ms.unwrap_or(0)
        } else {
            steps.iter().map(|step| step.ms).sum()
        };
        if job_ms > 0 {
            if let Some(metric) = self.duration_metric(JOB_DURATION, job_ms, None) {
                self.candidates.push(Candidate { rank: 2, metric });
            }
        }
        for step in steps {
            let Some(name) = step.step.as_deref() else {
                // Timed, but unnamed: it counts in the job's total and gets no line of its
                // own, because a label that says nothing groups nothing.
                continue;
            };
            if let Some(metric) = self.duration_metric(STEP_DURATION, step.ms, Some(name)) {
                self.candidates.push(Candidate { rank: 2, metric });
            }
        }
    }

    /// `ms` as a `*_duration_s` metric in seconds, or `None` when the number is not one a
    /// record may carry. The unit is part of the name (`*_s`, §5.3), so the value is the
    /// plain second count at millisecond precision and nothing else is invented.
    fn duration_metric(
        &mut self,
        name: &str,
        ms: u64,
        step: Option<&str>,
    ) -> Option<BenchmarkMetric> {
        #[allow(
            clippy::cast_precision_loss,
            reason = "a duration in milliseconds fits an f64 exactly, well past any run this server waits for"
        )]
        let value = MetricValue::coerce_f64(ms as f64 / 1_000.0)?;
        let mut labels = std::collections::BTreeMap::new();
        labels.insert("job".to_owned(), self.job_id.clone());
        if let Some(step) = step {
            labels.insert("step".to_owned(), step.to_owned());
        }
        let metric = BenchmarkMetric::new(name.to_owned(), value, BenchmarkSource::ActLog)
            .with_unit(DURATION_UNIT)
            .with_labels(labels);
        // A duration is never negative and act cannot have produced a negative one; the
        // check stays so a log lying about it is refused like any other bad value.
        if let Err(reason) = metric.check() {
            tracing::debug!(run_id = %self.envelope.run_id, "rejected a derived duration: {reason}");
            self.summary.malformed += 1;
            return None;
        }
        Some(metric)
    }

    /// Dedup, suppress, cap, number, flag, build: the merge step, and the only place the
    /// rules about *sets* of metrics (§4.4, §5.2, §5.3) are applied.
    fn settle(&mut self) {
        // §5.3: a job that timed its own phase beats act timing the whole job, and keeping
        // both would count the same duration twice.
        let measured: BTreeSet<(String, Option<String>, Option<String>)> = self
            .candidates
            .iter()
            .filter(|candidate| candidate.rank < 2)
            .map(|candidate| measurement(&candidate.metric))
            .collect();

        // Candidates are already in precedence order, so "keep the first `(name, labels,
        // value)` seen" is also "keep the most authoritative source" (§5.2).
        let mut kept: Vec<BenchmarkMetric> = Vec::new();
        let mut seen: BTreeSet<(String, String, String)> = BTreeSet::new();
        for candidate in &self.candidates {
            if candidate.rank == 2 && measured.contains(&measurement(&candidate.metric)) {
                continue;
            }
            if !seen.insert(identity(&candidate.metric)) {
                // §4.4: a repeat of one already kept is a chatty job, not a sample. A
                // repeat with a *different* value falls through and is numbered below.
                self.summary.duplicates += 1;
                continue;
            }
            kept.push(candidate.metric.clone());
        }

        // §3.3: bound the run, and say so — the flag goes on every line of it (§5.1), which
        // is why truncation is a flag and not an extra record of a third kind.
        if kept.len() > MAX_BENCHMARK_METRICS {
            kept.truncate(MAX_BENCHMARK_METRICS);
            self.summary.truncated = true;
            self.push_warning(WARNING_TRUNCATED);
        }

        // §4.4: number one `(name, labels)` group 1..n, and only when it has more than one
        // member: a lone measurement needs no index to tell it apart.
        let mut groups: BTreeMap<(String, String), Vec<usize>> = BTreeMap::new();
        for (index, metric) in kept.iter().enumerate() {
            let (name, labels, _) = identity(metric);
            groups.entry((name, labels)).or_default().push(index);
        }
        for members in groups.into_values().filter(|members| members.len() > 1) {
            for (number, index) in members.iter().enumerate() {
                kept[*index].sample = Some(u32::try_from(number + 1).unwrap_or(u32::MAX));
            }
        }

        if self.summary.truncated {
            for metric in &mut kept {
                let flags = metric.flags.get_or_insert_with(Vec::new);
                if !flags.iter().any(|flag| flag == FLAG_TRUNCATED) {
                    flags.push(FLAG_TRUNCATED.to_owned());
                }
            }
        }

        let mut records = Vec::with_capacity(kept.len());
        for metric in kept {
            let record = BenchmarkRecord::new(&self.envelope, metric);
            // Belt and braces: every value was checked on the way in, so a record failing
            // here is a writer bug, and the answer is still to drop the line and count it
            // rather than to publish a line `validate-evidence` will reject.
            match record.check() {
                Ok(()) => records.push(record),
                Err(reason) => {
                    tracing::warn!(
                        run_id = %self.envelope.run_id,
                        "dropped an unusable benchmark record: {reason}"
                    );
                    self.summary.malformed += 1;
                }
            }
        }
        self.summary.records = records;

        let mut from = SourceCounts::default();
        for record in &self.summary.records {
            match record.metric.source {
                BenchmarkSource::JobFile => from.job_file += 1,
                BenchmarkSource::InBandStdout => from.in_band_stdout += 1,
                BenchmarkSource::ActLog => from.act_log += 1,
            }
        }
        self.summary.from = from;

        self.summary.outcome = match self.payload {
            PayloadState::Failed => ExtractionOutcome::ParseFailed,
            PayloadState::Read => ExtractionOutcome::Extracted,
            _ if !self.summary.records.is_empty() => ExtractionOutcome::Derived,
            _ => ExtractionOutcome::NoMetrics,
        };
        match self.summary.outcome {
            ExtractionOutcome::ParseFailed => self.push_warning(WARNING_PARSE_FAILED),
            ExtractionOutcome::NoMetrics => self.push_warning(WARNING_NO_BENCHMARK_METRICS),
            ExtractionOutcome::Extracted | ExtractionOutcome::Derived => {}
        }
        if self.summary.malformed > 0 {
            self.push_warning(WARNING_MALFORMED_METRIC);
        }
        let codes = self.summary.warning_codes();
        self.summary.warnings = codes;
    }
}

/// The filenames §3.2 pins for a benchmark payload, highest precedence first. The whole of
/// `$ACTMCP_EVIDENCE_OUT` is looked for before the tree root; within either, each name is
/// tried in the order written here.
const PAYLOAD_NAMES: [&str; 2] = ["benchmark.json", "benchmark.jsonl"];

/// The envelope every record of this run shares (ADR-0002 §2.2).
///
/// Identity comes from the validator and the runner and nowhere else: a payload may never
/// re-attribute a run (§3.3), so not one of these fields is read from a file's contents.
fn envelope_for(
    outcome: &RunOutcome,
    target: &ValidatedRunTarget,
    context: &ExtractContext,
) -> Result<Envelope, RecordError> {
    let envelope = Envelope::new(
        Kind::Benchmark,
        RunId::parse(&outcome.run_id)?,
        Commit::parse(&target.commit_full_sha)?,
        target.workflow_display.clone(),
        target.job_id.clone(),
        status_word(outcome.status),
        Tool {
            name: crate::evidence::records::TOOL_NAME.to_owned(),
            version: context.server_version.clone(),
            act_version: context.act_version.clone(),
        },
        UtcInstant::from_datetime(outcome.started_at),
        context.host.clone(),
    )
    .with_category(target.category.clone())
    .with_duration_ms(outcome.duration_ms);
    // act was signalled, or was never spawned: `exit_code` is absent rather than 0, because
    // absence expresses unknown (§2.1). A code too large for the contract's one byte is
    // absent for the same reason.
    Ok(
        match outcome.exit_code.and_then(|code| u8::try_from(code).ok()) {
            Some(code) => envelope.with_exit_code(code),
            None => envelope,
        },
    )
}
