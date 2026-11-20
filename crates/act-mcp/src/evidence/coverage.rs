//! Coverage extraction: a finished act run's coverage payload → `.evidence` coverage
//! records (mact-f1w.10, ADR-0002 §3.2, §4.2, §4.3, §5.1).
//!
//! [`extract`] is the entry point. It reads whatever a finished run's coverage job left
//! behind, turns it into [`CoverageRecord`]s carrying this run's identity, and reports what
//! it found (or did not) so the supervisor can put the same words in `meta.json` and in the
//! tool result. It never publishes a file and never changes a run's status: extraction
//! problems are data, not failures (ADR-0002 §5.1).
//!
//! ```text
//! discover ──► first found file wins ──► parse ──► one scope:"total" + one record per file
//!     │                                                        │
//!     └── nothing found ──► scrape act.jsonl (last resort) ────┘  source:"log_scrape"
//! ```
//!
//! ## The rules this module exists to keep
//!
//! 1. **First found wins, and everything found is named.** ADR-0002 §3.2's filename
//!    precedence, one source per run — never a blend of two tools' file lists, because lcov
//!    and istanbul disagree about which files exist. Every candidate that existed is listed
//!    in [`CoverageSummary::found`] with the winner marked, so a shadowed file is visible in
//!    `meta.json` rather than silently preferred, and every record carries the
//!    `source_path` it was parsed from.
//! 2. **Raw numerators only.** A record stores `*_found`/`*_hit`, never a rate beside them
//!    (§4.3). The one exception is the scraped percentage: `source: "log_scrape"` only,
//!    always flagged `percent_only`, always naming its `reported_measure`, and reached only
//!    when no payload file existed at all — so scraping can never overwrite a parsed file's
//!    numbers.
//! 3. **No host paths in evidence.** `SF:` values and istanbul keys are absolute container
//!    or host paths; `normalize_source_path` makes them workspace-relative or drops them
//!    (§4.2's "container `/srv/...` normalised away"). Nothing emitted can name a directory
//!    above the tree.
//! 4. **Never fail a run.** Every payload problem is an [`ExtractionOutcome`] plus a warning
//!    code. Only two things come back as [`Err`]: a payload in a format this extractor
//!    refuses to guess at ([`ExtractError::UnsupportedFormat`]) and a run whose identity
//!    could not become an envelope ([`ExtractError::Envelope`], a bug rather than data).
//!
//! ## Totals
//!
//! The `scope: "total"` record is the source's own total: for lcov, the sum over every `SF`
//! block (the format writes no total record of its own, and §4.2 pins "repeated `SF` blocks
//! summed per path"); for istanbul, its `total` entry. Either can exceed the sum of the
//! emitted file records when the tool instrumented files that live outside the workspace —
//! those counts belong to the run's total and to no file record, because no `relpath` can
//! name them. That is reported in [`CoverageSummary::dropped_paths`] rather than hidden.

mod istanbul;
mod lcov;
mod scrape;

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::{
    evidence::records::{
        Commit, CoverageCounts, CoverageRecord, CoverageSource, Envelope, Kind, RecordError,
        RelPath, RunId, RunRecords, RunStatus as WireStatus, Scope, Tool, UtcInstant,
        FLAG_LOW_CONFIDENCE, FLAG_TRUNCATED,
    },
    runner::{ActRunOutcome, RunStatus as RunnerStatus, WARNING_PAYLOAD_TRANSPORT_UNAVAILABLE},
    workspace::ValidatedRunTarget,
};

/// The ticket's `RunOutcome`, by the name the ticket uses: the runner's terminal outcome.
pub type RunOutcome = ActRunOutcome;

/// Warning code: the run produced no coverage payload anywhere the contract looks, or the
/// only one it produced was empty (ADR-0002 §5.1).
pub const WARNING_NO_COVERAGE_FOUND: &str = "no_coverage_found";
/// Warning code: a coverage payload existed but could not be read or parsed (§5.1).
pub const WARNING_PARSE_FAILED: &str = "parse_failed";
/// Warning code: the only coverage payload found is a format this extractor does not
/// parse (§5.1).
pub const WARNING_UNSUPPORTED_FORMAT: &str = "unsupported_format";
/// Warning code: more than one coverage payload existed, so all but one were ignored
/// (§3.2, §5.1).
pub const WARNING_COVERAGE_SOURCE_SHADOWED: &str = "coverage_source_shadowed";
/// Warning code: a payload exceeded the size cap (§3.3).
pub const WARNING_PAYLOAD_TOO_LARGE: &str = "payload_too_large";
/// Warning code: a payload existed but was not readable (§3.1).
pub const WARNING_PAYLOAD_UNREADABLE: &str = "payload_unreadable";
/// Warning code: some counts named more hits than there were things to hit, and were
/// clamped (§4.2's extractor rule).
pub const WARNING_COUNTS_REPAIRED: &str = "coverage_counts_repaired";
/// Warning code: source files were counted in the total but got no record of their own,
/// because no workspace-relative path can name them (§4.2).
pub const WARNING_PATHS_DROPPED: &str = "coverage_paths_outside_workspace";

/// Flag on a record whose counts had to be clamped to `hit <= found`.
pub const FLAG_COUNTS_REPAIRED: &str = "counts_repaired";

/// The size cap for one coverage payload — ADR-0002 §3.3's payload cap, applied to coverage
/// files for the same reason: one run must not balloon the evidence tree.
pub const MAX_COVERAGE_BYTES: u64 = 8 * 1024 * 1024;
/// How many distinct source files one run may contribute records for.
pub const MAX_COVERAGE_SOURCES: usize = 5_000;
/// How much of `act.jsonl` the last-resort scrape reads.
pub const MAX_SCRAPE_BYTES: u64 = 32 * 1024 * 1024;

/// Which coverage format a file holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverageFormat {
    /// An lcov tracefile.
    Lcov,
    /// An istanbul `coverage-summary.json`.
    CoverageSummary,
    /// A Cobertura `coverage.xml`, which v1 does not parse (ADR-0002 §4.2).
    CoberturaXml,
}

impl CoverageFormat {
    /// The spelling used in `meta.json`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lcov => "lcov",
            Self::CoverageSummary => "coverage_summary",
            Self::CoberturaXml => "cobertura_xml",
        }
    }

    /// Whether this format can be turned into records at all.
    #[must_use]
    pub const fn supported(self) -> bool {
        !matches!(self, Self::CoberturaXml)
    }

    /// The record `source` a file of this format carries.
    const fn source(self) -> Option<CoverageSource> {
        match self {
            Self::Lcov => Some(CoverageSource::Lcov),
            Self::CoverageSummary => Some(CoverageSource::CoverageSummary),
            Self::CoberturaXml => None,
        }
    }
}

/// `$ACTMCP_EVIDENCE_OUT`'s names from ADR-0002 §3.2's table, highest precedence first.
const PAYLOAD_NAMES: [(&str, CoverageFormat); 2] = [
    ("lcov.info", CoverageFormat::Lcov),
    ("coverage-summary.json", CoverageFormat::CoverageSummary),
];
/// The tree-root fallbacks, same order. They exist because most coverage tooling writes a
/// conventional `coverage/lcov.info` and rewriting a target repo's tooling is not this
/// server's business (§3.2).
const TREE_NAMES: [(&str, CoverageFormat); 4] = [
    ("lcov.info", CoverageFormat::Lcov),
    ("coverage/lcov.info", CoverageFormat::Lcov),
    ("coverage-summary.json", CoverageFormat::CoverageSummary),
    (
        "coverage/coverage-summary.json",
        CoverageFormat::CoverageSummary,
    ),
];
/// Names recognised as coverage but not parsed. Finding one is worth saying out loud: a
/// Cobertura run is not a run that produced nothing.
const UNSUPPORTED_NAMES: [&str; 3] = ["coverage.xml", "coverage/cobertura.xml", "cobertura.xml"];

/// What extraction concluded, in one word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractionOutcome {
    /// Records came from a payload file.
    Extracted,
    /// Records came from scraping act's log, because no payload file existed.
    Scraped,
    /// Nothing that could carry coverage existed, or the only file found was empty.
    NoCoverageFound,
    /// A payload existed and could not be read or parsed.
    ParseFailed,
}

impl ExtractionOutcome {
    /// The wire spelling, which doubles as the `meta.json` warning code for the two failure
    /// cases (ADR-0002 §5.1).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Extracted => "extracted",
            Self::Scraped => "scraped",
            Self::NoCoverageFound => "no_coverage_found",
            Self::ParseFailed => "parse_failed",
        }
    }
}

impl std::fmt::Display for ExtractionOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What one coverage-named file was, and whether its numbers are in the records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovered {
    /// Workspace-relative spelling of the file's path.
    pub path: String,
    /// What format it held, by name.
    pub format: CoverageFormat,
    /// Whether the records came from it.
    pub used: bool,
    /// Its size, when it could be stat'ed.
    pub bytes: Option<u64>,
}

/// What extraction produced — the `meta.json` and tool-result view of one run's coverage.
#[derive(Debug, Clone)]
pub struct CoverageSummary {
    /// The one-word answer.
    pub outcome: ExtractionOutcome,
    /// The records, in file order: the `scope: "total"` line first, then one line per
    /// source file.
    pub records: Vec<CoverageRecord>,
    /// The record `source` the numbers came from.
    pub source: Option<CoverageSource>,
    /// …and the file they came from, workspace-relative.
    pub source_path: Option<RelPath>,
    /// Every coverage-named file that existed.
    pub found: Vec<Discovered>,
    /// Machine-readable caveats — `meta.json`'s `warnings[]` vocabulary (ADR-0002 §5.1).
    pub warnings: Vec<String>,
    /// Entries, blocks or lines that were present but unusable: `meta.json`'s
    /// `evidence[].malformed`.
    pub malformed: usize,
    /// Source files counted in the total but given no record of their own.
    pub dropped_paths: usize,
    /// A human-readable note about a failure, for `meta.json`.
    pub detail: Option<String>,
}

impl CoverageSummary {
    /// The batch to publish, or `None` when there is nothing to publish — ADR-0002 §1.3: a
    /// run that produced no records is recorded by `meta.json`, not by an empty file.
    #[must_use]
    pub fn run_records(&self) -> Option<RunRecords> {
        if self.records.is_empty() {
            return None;
        }
        Some(RunRecords::Coverage {
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
    /// then the extraction warnings, deduplicated in first-seen order so the two places
    /// cannot disagree about the same run.
    #[must_use]
    pub fn warning_codes(&self) -> Vec<String> {
        let mut codes: Vec<String> = Vec::new();
        for code in match self.outcome {
            ExtractionOutcome::NoCoverageFound => Some(WARNING_NO_COVERAGE_FOUND),
            ExtractionOutcome::ParseFailed => Some(WARNING_PARSE_FAILED),
            ExtractionOutcome::Extracted | ExtractionOutcome::Scraped => None,
        }
        .into_iter()
        .chain(self.warnings.iter().map(String::as_str))
        {
            if !codes.iter().any(|held| held == code) {
                codes.push(code.to_owned());
            }
        }
        codes
    }
}

/// Why extraction could not produce anything at all.
#[derive(Debug, Error)]
pub enum ExtractError {
    /// The only coverage payload the run produced is a format this extractor does not
    /// parse. ADR-0002 §4.2 is explicit that Cobertura is unsupported in v1 and that the
    /// right answer is a clean refusal rather than a guess about what `<line hits="1"/>`
    /// means for a language that counts lines differently.
    #[error("coverage payload {path} is {format}, which act-mcp does not parse: {hint}")]
    UnsupportedFormat {
        /// The file that was found, workspace-relative.
        path: String,
        /// Which format it was.
        format: &'static str,
        /// What would have to be settled before it could be supported.
        hint: &'static str,
    },
    /// The run's identity could not become an envelope. That is a bug in the caller or the
    /// runner, not a data problem: `run_id` and `commit` are server-generated.
    #[error("this run's identity could not be written as evidence: {0}")]
    Envelope(#[from] RecordError),
}

impl ExtractError {
    /// The warning code a caller stamps when this error is what happened. Even here the
    /// run's status is untouched: an error about evidence is not an error about the job
    /// (ADR-0002 §5.1).
    #[must_use]
    pub const fn warning_code(&self) -> &'static str {
        match self {
            Self::UnsupportedFormat { .. } => WARNING_UNSUPPORTED_FORMAT,
            Self::Envelope(_) => WARNING_PARSE_FAILED,
        }
    }
}

// `ExtractContext` lives in the parent module: coverage and benchmark extraction look in
// the same three places and stamp the same envelope, so they share one context type.
pub use crate::evidence::ExtractContext;

/// Turn a finished run's coverage payload into records, with the default
/// [`ExtractContext`] for `target`.
///
/// # Errors
///
/// [`ExtractError::UnsupportedFormat`] when the only coverage file the run produced is a
/// format this extractor refuses to guess at, and [`ExtractError::Envelope`] when the run's
/// identity is not writable as evidence. Everything else — an absent, empty, unreadable,
/// oversized or garbage payload — is reported inside the returned [`CoverageSummary`] and
/// leaves the run's status exactly as act reported it.
pub fn extract(
    outcome: &RunOutcome,
    target: &ValidatedRunTarget,
) -> Result<CoverageSummary, ExtractError> {
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
) -> Result<CoverageSummary, ExtractError> {
    let envelope = envelope_for(outcome, target, context)?;
    let mut extraction = Extraction {
        envelope,
        context,
        summary: CoverageSummary {
            outcome: ExtractionOutcome::NoCoverageFound,
            records: Vec::new(),
            source: None,
            source_path: None,
            found: Vec::new(),
            warnings: Vec::new(),
            malformed: 0,
            dropped_paths: 0,
            detail: None,
        },
    };
    if outcome.has_warning(WARNING_PAYLOAD_TRANSPORT_UNAVAILABLE) {
        // `--no-bind`: the job could not write a payload at all, which is the one
        // explanation worth repeating next to an empty `incoming/`.
        extraction.push_warning(WARNING_PAYLOAD_TRANSPORT_UNAVAILABLE);
    }
    extraction.run(outcome)?;
    extraction.summary.warnings = extraction.summary.warning_codes();
    Ok(extraction.summary)
}

/// One run's extraction, in progress.
struct Extraction<'context> {
    envelope: Envelope,
    context: &'context ExtractContext,
    summary: CoverageSummary,
}

impl<'context> Extraction<'context> {
    fn push_warning(&mut self, code: &str) {
        if !self.summary.warnings.iter().any(|held| held == code) {
            self.summary.warnings.push(code.to_owned());
        }
    }

    /// Discover, take the winner, and fall back to the log only when nothing was found.
    fn run(&mut self, outcome: &RunOutcome) -> Result<(), ExtractError> {
        let found = self.discover();
        self.summary.found = found
            .iter()
            .map(|candidate| Discovered {
                path: candidate.display.clone(),
                format: candidate.format,
                used: false,
                bytes: candidate.bytes,
            })
            .collect();

        let usable: Vec<&Candidate> = found.iter().filter(|c| c.format.supported()).collect();
        let unusable: Vec<&Candidate> = found.iter().filter(|c| !c.format.supported()).collect();
        if usable.len() > 1 {
            // §3.2: one source wins, and the fact that another existed is data.
            self.push_warning(WARNING_COVERAGE_SOURCE_SHADOWED);
        }
        if !unusable.is_empty() {
            self.push_warning(WARNING_UNSUPPORTED_FORMAT);
        }

        if let Some(winner) = usable.first() {
            self.parse_payload(winner);
            return Ok(());
        }
        if let Some(unsupported) = unusable.first() {
            // A structured payload exists; it is simply a format v1 refuses to guess at.
            // Scraping the log instead would turn "unsupported" into a made-up number.
            return Err(ExtractError::UnsupportedFormat {
                path: unsupported.display.clone(),
                format: "cobertura coverage.xml",
                hint: "v1 supports lcov and istanbul coverage-summary.json only; Cobertura's \
                       line/branch-condition model would have to be mapped to the record's \
                       pairs before it can be parsed (ADR-0002 §4.2)",
            });
        }
        self.scrape_fallback(outcome);
        Ok(())
    }

    /// The files the contract looks at, in ADR-0002 §3.2's precedence order.
    fn discover(&self) -> Vec<Candidate> {
        let mut found: Vec<Candidate> = Vec::new();
        for (name, format) in PAYLOAD_NAMES {
            found.extend(Candidate::at(
                &self.context.payload_dir.join(name),
                format,
                self.context,
            ));
        }
        for (name, format) in TREE_NAMES {
            found.extend(Candidate::at(
                &self.context.tree_root.join(name),
                format,
                self.context,
            ));
        }
        for name in UNSUPPORTED_NAMES {
            for root in [&self.context.payload_dir, &self.context.tree_root] {
                found.extend(Candidate::at(
                    &root.join(name),
                    CoverageFormat::CoberturaXml,
                    self.context,
                ));
            }
        }
        found
    }

    /// Parse the winning payload and build its records.
    fn parse_payload(&mut self, winner: &Candidate) {
        let Some(source) = winner.format.source() else {
            // Unreachable: callers only hand over supported formats. Stated rather than
            // assumed so a future format cannot slip through silently.
            self.summary.outcome = ExtractionOutcome::ParseFailed;
            self.summary.detail = Some(format!(
                "{} is a format this extractor does not parse",
                winner.display
            ));
            return;
        };
        let Some(source_path) = RelPath::parse(&winner.display).ok() else {
            // A payload path that cannot be spelled workspace-relative cannot be cited, and
            // a file-derived record must cite its file (ADR-0002 §4.2).
            self.push_warning(WARNING_PATHS_DROPPED);
            self.summary.outcome = ExtractionOutcome::ParseFailed;
            self.summary.detail = Some(format!(
                "the coverage payload at {} is outside the workspace, so no record may name it",
                winner.display
            ));
            return;
        };

        let body = match read_bounded(&winner.path, winner.bytes) {
            Ok(Body::Text(body)) => body,
            Ok(Body::TooLarge(len)) => {
                self.push_warning(WARNING_PAYLOAD_TOO_LARGE);
                self.summary.outcome = ExtractionOutcome::ParseFailed;
                self.summary.detail = Some(format!(
                    "{} is {len} bytes, over the {} MiB payload cap",
                    winner.display,
                    MAX_COVERAGE_BYTES / (1024 * 1024)
                ));
                return;
            }
            Err(reason) => {
                self.push_warning(WARNING_PAYLOAD_UNREADABLE);
                self.summary.outcome = ExtractionOutcome::ParseFailed;
                self.summary.detail = Some(format!("{}: {reason}", winner.display));
                return;
            }
        };
        if body.trim().is_empty() {
            // An empty file is not a parse failure: it is a job that produced nothing.
            self.summary.outcome = ExtractionOutcome::NoCoverageFound;
            self.summary.detail = Some(format!("{} was empty", winner.display));
            return;
        }

        let parsed = match winner.format {
            CoverageFormat::Lcov => {
                let report = lcov::parse(&body, MAX_COVERAGE_SOURCES);
                self.summary.malformed += report.malformed_blocks + report.malformed_lines;
                if report.truncated {
                    self.push_warning(FLAG_TRUNCATED);
                }
                Parsed {
                    totals: report.totals,
                    sources: report.sources,
                    truncated: report.truncated,
                }
            }
            CoverageFormat::CoverageSummary => {
                let report = istanbul::parse(&body);
                if report.not_a_summary {
                    self.summary.outcome = ExtractionOutcome::ParseFailed;
                    self.summary.detail = Some(format!(
                        "{} is not an istanbul coverage summary",
                        winner.display
                    ));
                    return;
                }
                self.summary.malformed += report.malformed;
                Parsed {
                    totals: report.total,
                    sources: report.sources,
                    truncated: false,
                }
            }
            CoverageFormat::CoberturaXml => return,
        };
        self.emit(source, source_path, &winner.display, &parsed);
    }

    /// The records themselves: the total first, then one line per source file.
    fn emit(
        &mut self,
        source: CoverageSource,
        source_path: RelPath,
        winner_display: &str,
        parsed: &Parsed,
    ) {
        let mut records: Vec<CoverageRecord> = Vec::new();
        if !parsed.totals.is_empty() {
            if let Some((mut counts, repaired)) =
                counts_for(&parsed.totals, Scope::Total, None, source, &source_path)
            {
                if parsed.truncated {
                    counts = counts.with_flag(FLAG_TRUNCATED);
                }
                if repaired {
                    self.push_warning(WARNING_COUNTS_REPAIRED);
                    counts = counts.with_flag(FLAG_COUNTS_REPAIRED);
                }
                self.push_record(&mut records, counts);
            }
        }

        for file in &parsed.sources {
            let Some(path) = normalize_source_path(&file.raw_path, self.context) else {
                // Counted in the total, nameless in the records: no `relpath` reaches above
                // the tree (§4.2), and substituting a basename would attribute one file's
                // counts to a different file.
                self.summary.dropped_paths += 1;
                continue;
            };
            if let Some((mut counts, repaired)) =
                counts_for(&file.pairs, Scope::File, Some(path), source, &source_path)
            {
                if parsed.truncated {
                    counts = counts.with_flag(FLAG_TRUNCATED);
                }
                if repaired {
                    self.push_warning(WARNING_COUNTS_REPAIRED);
                    counts = counts.with_flag(FLAG_COUNTS_REPAIRED);
                }
                self.push_record(&mut records, counts);
            } else {
                self.summary.malformed += 1;
            }
        }
        if self.summary.dropped_paths > 0 {
            self.push_warning(WARNING_PATHS_DROPPED);
        }
        if records.is_empty() {
            self.summary.outcome = ExtractionOutcome::ParseFailed;
            self.summary.detail = Some("the coverage payload held no usable counts".to_owned());
            return;
        }
        self.summary.outcome = ExtractionOutcome::Extracted;
        self.summary.source = Some(source);
        self.summary.source_path = Some(source_path);
        if let Some(winner) = self
            .summary
            .found
            .iter_mut()
            .find(|file| file.path == winner_display)
        {
            winner.used = true;
        }
        self.summary.records = records;
    }

    /// The last resort: a percentage out of act's log, reached only when no payload file
    /// existed (ADR-0002 §4.3).
    fn scrape_fallback(&mut self, outcome: &RunOutcome) {
        let scraped = match scrape::scrape(&outcome.log_path, MAX_SCRAPE_BYTES) {
            Ok(scan) => {
                if scan.truncated {
                    self.push_warning(FLAG_TRUNCATED);
                }
                scan.found
            }
            Err(reason) => {
                self.summary.detail = Some(format!(
                    "no coverage payload existed and {} could not be read: {reason}",
                    outcome.log_path.display()
                ));
                return;
            }
        };
        let Some(scraped) = scraped else {
            return;
        };
        let mut extra = serde_json::Map::new();
        extra.insert(
            "note".to_owned(),
            serde_json::Value::String("no lcov.info or coverage-summary.json produced".to_owned()),
        );
        extra.insert(
            "scraped_line".to_owned(),
            serde_json::Value::String(scraped.line),
        );
        // `with_percent_only` is the only way a percentage reaches a record, and it makes
        // §4.3's three rules (`reported_measure`, the flag, no raw counts) impossible to
        // half-apply. `low_confidence` goes on first so the flag order matches the golden.
        let counts = CoverageCounts::total(0, 0, CoverageSource::LogScrape)
            .with_flag(FLAG_LOW_CONFIDENCE)
            .with_percent_only(scraped.percent, scraped.measure)
            .with_extra(extra);
        let mut records = Vec::new();
        self.push_record(&mut records, counts);
        if records.is_empty() {
            self.summary.detail = Some("the scraped record did not pass its own check".to_owned());
            return;
        }
        self.summary.records = records;
        self.summary.source = Some(CoverageSource::LogScrape);
        self.summary.outcome = ExtractionOutcome::Scraped;
    }

    /// One record, checked before it is kept: a line `validate-evidence` would reject is
    /// dropped and counted rather than published (never fail the run, never write a line a
    /// checker will reject).
    fn push_record(&mut self, records: &mut Vec<CoverageRecord>, counts: CoverageCounts) {
        let record = CoverageRecord::new(&self.envelope, counts);
        match record.check() {
            Ok(()) => records.push(record),
            Err(reason) => {
                tracing::warn!(
                    run_id = %self.envelope.run_id,
                    "dropped an unusable coverage record: {reason}"
                );
                self.summary.malformed += 1;
            }
        }
    }
}

/// The envelope every record of this run shares (ADR-0002 §2.2).
///
/// Identity comes from the validator and the runner and nowhere else: a payload may never
/// re-attribute a run (§3.3), so not one of these fields is read from file contents.
fn envelope_for(
    outcome: &RunOutcome,
    target: &ValidatedRunTarget,
    context: &ExtractContext,
) -> Result<Envelope, RecordError> {
    let envelope = Envelope::new(
        Kind::Coverage,
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

/// The runner's terminal word is the evidence contract's terminal word; the mapping is
/// total because both enums carry exactly the five ADR-0001 §3.2 words.
pub(crate) const fn status_word(status: RunnerStatus) -> WireStatus {
    match status {
        RunnerStatus::Success => WireStatus::Success,
        RunnerStatus::Failed => WireStatus::Failed,
        RunnerStatus::Cancelled => WireStatus::Cancelled,
        RunnerStatus::TimedOut => WireStatus::TimedOut,
        RunnerStatus::Error => WireStatus::Error,
    }
}

/// What two parsers are reduced to before records are built.
struct Parsed {
    /// The run's total, from the format's own answer.
    totals: Pairs,
    /// The files, in the order their records are emitted.
    sources: Vec<ParsedSource>,
    /// Whether the parse hit the record cap.
    truncated: bool,
}

/// One source file, before its path is normalised.
#[derive(Debug, Clone)]
struct ParsedSource {
    raw_path: String,
    pairs: Pairs,
}

/// The four co-required count pairs, as the formats report them.
///
/// Every pair is `(found, hit)` — denominator first, the order the wire fields are written
/// in (`lines_found`, then `lines_hit`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Pairs {
    lines: Option<(u64, u64)>,
    statements: Option<(u64, u64)>,
    branches: Option<(u64, u64)>,
    functions: Option<(u64, u64)>,
    /// Whether any sum or source said more was hit than exists.
    needs_repair: bool,
}

impl Pairs {
    /// An empty set of counts.
    const NONE: Self = Self {
        lines: None,
        statements: None,
        branches: None,
        functions: None,
        needs_repair: false,
    };

    /// Whether these counts say nothing at all.
    fn is_empty(&self) -> bool {
        self.lines.is_none()
            && self.statements.is_none()
            && self.branches.is_none()
            && self.functions.is_none()
    }

    /// Add another file's (or another `SF` block's) counts, pair by pair.
    ///
    /// A pair present in one and absent in the other stays absent: a format that reported no
    /// branches for a generated file does not acquire a zero branch pair from a neighbouring
    /// file, and ADR-0002 §4.2 makes the pairs co-required per record.
    fn add(&mut self, other: &Pairs) {
        let sum = |held: &mut Option<(u64, u64)>, added: Option<(u64, u64)>| {
            if let Some((added_found, added_hit)) = added {
                match held {
                    Some((found, hit)) => {
                        *found += added_found;
                        *hit += added_hit;
                    }
                    None => *held = Some((added_found, added_hit)),
                }
            }
        };
        sum(&mut self.lines, other.lines);
        sum(&mut self.statements, other.statements);
        sum(&mut self.branches, other.branches);
        sum(&mut self.functions, other.functions);
        self.needs_repair |= other.needs_repair;
    }
}

/// Clamp one pair to `hit <= found` — ADR-0002 §4.2 calls that an extractor rule, and this
/// is its only home — and say whether anything moved.
fn repair(pair: &mut Option<(u64, u64)>) -> bool {
    let Some((found, hit)) = pair else {
        return false;
    };
    if *hit > *found {
        *found = *hit;
        return true;
    }
    false
}

/// Build the counts half of a record, clamped to `hit <= found`.
///
/// `None` when nothing usable remains: ADR-0002 §4.2 requires one complete raw pair on every
/// record that is not a scraped percentage, and a record with no counts is not writable.
fn counts_for(
    pairs: &Pairs,
    scope: Scope,
    path: Option<RelPath>,
    source: CoverageSource,
    source_path: &RelPath,
) -> Option<(CoverageCounts, bool)> {
    let mut pairs = *pairs;
    let mut repaired = repair(&mut pairs.lines)
        || repair(&mut pairs.statements)
        || repair(&mut pairs.branches)
        || repair(&mut pairs.functions);
    repaired |= pairs.needs_repair;
    if pairs.is_empty() {
        return None;
    }
    let counts = CoverageCounts {
        name: (scope == Scope::Total).then(|| "total".to_owned()),
        scope,
        path,
        lines_found: pairs.lines.map(|(found, _)| found),
        lines_hit: pairs.lines.map(|(_, hit)| hit),
        statements_found: pairs.statements.map(|(found, _)| found),
        statements_hit: pairs.statements.map(|(_, hit)| hit),
        branches_found: pairs.branches.map(|(found, _)| found),
        branches_hit: pairs.branches.map(|(_, hit)| hit),
        functions_found: pairs.functions.map(|(found, _)| found),
        functions_hit: pairs.functions.map(|(_, hit)| hit),
        reported_percent: None,
        reported_measure: None,
        source,
        source_path: Some(source_path.clone()),
        flags: None,
        extra: None,
    };
    Some((counts, repaired))
}

/// A coverage-named payload file that existed.
struct Candidate {
    path: PathBuf,
    display: String,
    format: CoverageFormat,
    bytes: Option<u64>,
}

impl Candidate {
    /// `path`, if it is a readable file, with its workspace-relative spelling.
    fn at(path: &Path, format: CoverageFormat, context: &ExtractContext) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        if !meta.is_file() {
            return None;
        }
        Some(Self {
            path: path.to_path_buf(),
            display: display_path(path, context),
            format,
            bytes: Some(meta.len()),
        })
    }
}

/// The workspace-relative spelling a record cites for its payload file.
pub(crate) fn display_path(path: &Path, context: &ExtractContext) -> String {
    for root in [&context.tree_root, &context.workspace_root] {
        if let Ok(relative) = path.strip_prefix(root) {
            let text = relative.to_string_lossy().replace('\\', "/");
            if !text.is_empty() {
                return text;
            }
        }
    }
    path.file_name()
        .map(|name| name.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|| path.to_string_lossy().replace('\\', "/"))
}

/// Turn a path a coverage tool wrote into the workspace-relative [`RelPath`] the contract
/// requires (ADR-0002 §4.2: "`/` separators, container `/srv/...` normalised away").
///
/// Three shapes arrive: paths already relative to the tree, paths absolute under the tree
/// (what a `--bind` container writes, since the bind makes container and host paths the same
/// file), and paths absolute somewhere else entirely (an installed dependency, a build
/// directory that has since been deleted). The first two become records; the third has no
/// honest `relpath` and is dropped, counted in the total and reported in
/// [`CoverageSummary::dropped_paths`] rather than guessed at.
#[must_use]
pub fn normalize_source_path(raw: &str, context: &ExtractContext) -> Option<RelPath> {
    let cleaned = lexical_clean(raw)?;
    let relative = if cleaned.starts_with('/') {
        relativize_absolute(&cleaned, context)?
    } else {
        cleaned
    };
    RelPath::parse(&relative).ok()
}

/// Collapse `.`/`..`, drop empty components, unify separators. `None` when a relative path
/// climbs out of the tree, which is a traversal rather than a path.
fn lexical_clean(raw: &str) -> Option<String> {
    let text = raw.trim().replace('\\', "/");
    if text.is_empty() {
        return None;
    }
    let absolute = text.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in text.split('/') {
        match part {
            "" | "." => continue,
            ".." => {
                if parts.pop().is_none() && !absolute {
                    return None;
                }
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(if absolute {
        format!("/{}", parts.join("/"))
    } else {
        parts.join("/")
    })
}

/// The inside-the-tree spelling of an absolute path.
fn relativize_absolute(absolute: &str, context: &ExtractContext) -> Option<String> {
    for root in [&context.tree_root, &context.workspace_root] {
        let root = root.to_string_lossy().replace('\\', "/");
        let root = root.trim_end_matches('/');
        if let Some(rest) = absolute.strip_prefix(&format!("{root}/")) {
            let rest = rest.trim_matches('/');
            if !rest.is_empty() {
                return Some(rest.to_owned());
            }
        }
    }
    // The container saw the tree at a different mount than the host does — the shape
    // ADR-0003 §15.1 documents, and the shape a tool that ran outside the container records.
    // Rebasing is allowed only where the rebased path really exists, so this cannot invent a
    // file or launder a path that happens to share a suffix with one.
    let parts: Vec<&str> = absolute.trim_start_matches('/').split('/').collect();
    for index in 1..parts.len() {
        let candidate = parts[index..].join("/");
        if !candidate.is_empty() && context.tree_root.join(&candidate).is_file() {
            return Some(candidate);
        }
    }
    None
}

/// A payload read from disk.
enum Body {
    Text(String),
    /// The payload's size, when it was over the cap.
    TooLarge(u64),
}

/// Read a payload, refusing to read more than [`MAX_COVERAGE_BYTES`].
fn read_bounded(path: &Path, known_len: Option<u64>) -> Result<Body, String> {
    use std::io::Read;
    if let Some(len) = known_len.filter(|len| *len > MAX_COVERAGE_BYTES) {
        return Ok(Body::TooLarge(len));
    }
    let mut file = std::fs::File::open(path).map_err(|reason| reason.to_string())?;
    let mut body = String::new();
    let read = file
        .by_ref()
        .take(MAX_COVERAGE_BYTES + 1)
        .read_to_string(&mut body);
    if let Err(reason) = read {
        // A payload that is not valid UTF-8 is not a coverage file; say so rather than
        // publishing mojibake or failing the run.
        return Err(reason.to_string());
    }
    if body.len() as u64 > MAX_COVERAGE_BYTES {
        return Ok(Body::TooLarge(body.len() as u64));
    }
    Ok(Body::Text(body))
}
