//! The closed vocabularies ADR-0008 declares, as Rust enums.
//!
//! Each of these is a *closed* set in [`schema/metric-model.json`](../../../../schema/metric-model.json):
//! 18 drop reasons, 8 gap codes (7 for coverage, 4 for benchmarks, 3 shared), 12 caveat codes, 4
//! action states, 4 measures. The point of closing them is that `—` is one glyph with several
//! meanings and a dropped record is one sentence with one verb: a user standing in front of an
//! unknown column is entitled to the difference, and a checker is entitled to compare what the
//! code can emit against what the model declares — which is what
//! `crates/fathom/tests/metrics.rs::vocabularies_are_closed` does.
//!
//! The `as_str` spellings are a public interface: they are what `--explain` prints, what
//! `doctor` prints, what a bug report quotes, and what the golden vectors store. Renaming one is
//! a model change, not a refactor.

use std::{fmt, str::FromStr};

use serde::Serialize;

/// Which count pair a coverage record measures (ADR-0008 §3.5).
///
/// Order matters: `measure_order` in the model is `lines → statements → branches → functions`,
/// and the primary measure is the first with a non-zero pooled denominator. `lines` is first
/// because it is the only measure every supported format reports, so it is the only one a
/// multi-tool pool can use without mixing units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Measure {
    /// lcov `LF`/`LH`, istanbul `total.lines`.
    Lines,
    /// istanbul statements — a different unit from lines, never pooled with them.
    Statements,
    /// Branches, when the format reports any.
    Branches,
    /// istanbul functions.
    Functions,
}

impl Measure {
    /// The model's spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lines => "lines",
            Self::Statements => "statements",
            Self::Branches => "branches",
            Self::Functions => "functions",
        }
    }
}

impl fmt::Display for Measure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Measure {
    type Err = UnknownCode;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "lines" => Ok(Self::Lines),
            "statements" => Ok(Self::Statements),
            "branches" => Ok(Self::Branches),
            "functions" => Ok(Self::Functions),
            other => Err(UnknownCode {
                kind: "measure",
                value: other.to_owned(),
            }),
        }
    }
}

/// The four stages of ADR-0008 §2. Each drop reason belongs to exactly one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stage {
    /// Which bytes are read at all.
    Walk,
    /// Which records a number may be built from.
    Retain,
    /// Which run is the newest for its identity.
    Select,
    /// The arithmetic that produces the row's numbers.
    Aggregate,
}

impl Stage {
    /// The model's spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Walk => "walk",
            Self::Retain => "retain",
            Self::Select => "select",
            Self::Aggregate => "aggregate",
        }
    }
}

impl fmt::Display for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The 18 reasons a record did not become a number (ADR-0008 §2.2's table, verbatim).
///
/// Exactly one per dropped record, so `--explain` is a table and not an apology. `vacuous` is
/// declared here and never emitted as a *drop* by this reader: a `found == 0` entry is a no-op in
/// a pool (0/0 adds nothing to either sum), so it surfaces as the `vacuous` caveat and gap code
/// instead — which is what the reference implementation does too, and the vector
/// `vacuous_only` pins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DropReason {
    /// The path is not `<kind>/<40-hex>/<run_id>.jsonl`.
    NotARecordPath,
    /// A dot-file is a crashed publish, never a record.
    TempFile,
    /// A directory whose name is not 40 lowercase hex.
    DirectoryNotSha,
    /// A `schema_version` this build cannot read.
    SchemaUnsupported,
    /// Not strict JSON, not one object, a missing envelope field, a kind that disagrees with its
    /// directory, or a torn final line.
    Malformed,
    /// The record's `commit` is not the directory it was read from.
    CommitMismatch,
    /// `run_id` that cannot be sorted, so it cannot be selected.
    BadRunId,
    /// `run_status` is not `success`.
    RunStatusExcluded,
    /// The run hit the per-run metric cap.
    RunTruncated,
    /// `benchmarks.allow` / `benchmarks.deny` dropped it.
    ConfigFiltered,
    /// Same run, same measurement, a higher-precedence source won.
    SourceShadowed,
    /// An older run of the same `(kind, workflow, task)`.
    Superseded,
    /// A scraped percentage with no raw counts.
    PercentOnly,
    /// Text-scraped evidence.
    LowConfidence,
    /// `found == 0`: the ratio is undefined.
    Vacuous,
    /// This record's measure is not the commit's primary measure.
    MeasureNotPrimary,
    /// A coverage record with no complete count pair.
    NoCounts,
    /// A file record under a run that published its own total.
    FileUnderTotal,
}

impl DropReason {
    /// The model's spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotARecordPath => "not_a_record_path",
            Self::TempFile => "temp_file",
            Self::DirectoryNotSha => "directory_not_sha",
            Self::SchemaUnsupported => "schema_unsupported",
            Self::Malformed => "malformed",
            Self::CommitMismatch => "commit_mismatch",
            Self::BadRunId => "bad_run_id",
            Self::RunStatusExcluded => "run_status_excluded",
            Self::RunTruncated => "run_truncated",
            Self::ConfigFiltered => "config_filtered",
            Self::SourceShadowed => "source_shadowed",
            Self::Superseded => "superseded",
            Self::PercentOnly => "percent_only",
            Self::LowConfidence => "low_confidence",
            Self::Vacuous => "vacuous",
            Self::MeasureNotPrimary => "measure_not_primary",
            Self::NoCounts => "no_counts",
            Self::FileUnderTotal => "file_under_total",
        }
    }

    /// Which pipeline stage owns it (model: `drop_reasons[].stage`).
    #[must_use]
    pub const fn stage(self) -> Stage {
        match self {
            Self::NotARecordPath | Self::TempFile | Self::DirectoryNotSha => Stage::Walk,
            Self::SchemaUnsupported
            | Self::Malformed
            | Self::CommitMismatch
            | Self::BadRunId
            | Self::RunStatusExcluded
            | Self::RunTruncated
            | Self::ConfigFiltered
            | Self::SourceShadowed => Stage::Retain,
            Self::Superseded => Stage::Select,
            Self::PercentOnly
            | Self::LowConfidence
            | Self::Vacuous
            | Self::MeasureNotPrimary
            | Self::NoCounts
            | Self::FileUnderTotal => Stage::Aggregate,
        }
    }

    /// Every code the model declares, in the model's own order.
    pub const ALL: [Self; 18] = [
        Self::NotARecordPath,
        Self::TempFile,
        Self::DirectoryNotSha,
        Self::SchemaUnsupported,
        Self::Malformed,
        Self::CommitMismatch,
        Self::BadRunId,
        Self::RunStatusExcluded,
        Self::RunTruncated,
        Self::ConfigFiltered,
        Self::SourceShadowed,
        Self::Superseded,
        Self::PercentOnly,
        Self::LowConfidence,
        Self::Vacuous,
        Self::MeasureNotPrimary,
        Self::NoCounts,
        Self::FileUnderTotal,
    ];
}

impl fmt::Display for DropReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl serde::Serialize for DropReason {
    /// A drop reason serializes as its code, so `excluded_runs[].reason` in a row is the same token
    /// `--explain` prints and `doctor` greps for.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl FromStr for DropReason {
    type Err = UnknownCode;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|reason| reason.as_str() == text)
            .ok_or(UnknownCode {
                kind: "drop_reason",
                value: text.to_owned(),
            })
    }
}

/// Why a column has no number (ADR-0008 §3.7). One per empty column, first match wins.
///
/// The list is per-column: `only_percent_only` and `vacuous`/`no_counts` are coverage-only,
/// `config_filtered` is benchmarks-only. The model declares both lists and
/// `gap_code_is_declared_for_column` asserts what this reader emits against them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GapCode {
    /// Nothing readable at this SHA.
    NoEvidence,
    /// Records present, none readable.
    Unreadable,
    /// Runs happened, none eligible.
    AllRunsExcluded,
    /// Only scraped evidence (coverage).
    OnlyPercentOnly,
    /// Every pooled denominator was 0 (coverage).
    Vacuous,
    /// A record with no count pair at all (coverage).
    NoCounts,
    /// Nothing of this kind survived (both columns).
    NoRecords,
    /// A name was filtered by `fathom.toml` (benchmarks).
    ConfigFiltered,
}

impl GapCode {
    /// The model's spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoEvidence => "no_evidence",
            Self::Unreadable => "unreadable",
            Self::AllRunsExcluded => "all_runs_excluded",
            Self::OnlyPercentOnly => "only_percent_only",
            Self::Vacuous => "vacuous",
            Self::NoCounts => "no_counts",
            Self::NoRecords => "no_records",
            Self::ConfigFiltered => "config_filtered",
        }
    }

    /// Whether the model allows this code for this column.
    #[must_use]
    pub const fn valid_for(self, column: Column) -> bool {
        match column {
            Column::Coverage => !matches!(self, Self::ConfigFiltered),
            Column::Benchmarks => {
                matches!(
                    self,
                    Self::NoEvidence
                        | Self::Unreadable
                        | Self::AllRunsExcluded
                        | Self::ConfigFiltered
                        | Self::NoRecords
                )
            }
        }
    }
}

impl fmt::Display for GapCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for GapCode {
    type Err = UnknownCode;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "no_evidence" => Ok(Self::NoEvidence),
            "unreadable" => Ok(Self::Unreadable),
            "all_runs_excluded" => Ok(Self::AllRunsExcluded),
            "only_percent_only" => Ok(Self::OnlyPercentOnly),
            "vacuous" => Ok(Self::Vacuous),
            "no_counts" => Ok(Self::NoCounts),
            "no_records" => Ok(Self::NoRecords),
            "config_filtered" => Ok(Self::ConfigFiltered),
            other => Err(UnknownCode {
                kind: "gap_reason",
                value: other.to_owned(),
            }),
        }
    }
}

/// Which of the two evidence-backed columns a gap or a caveat belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Column {
    /// `coverage`.
    Coverage,
    /// `benchmarks`.
    Benchmarks,
}

impl Column {
    /// The model's spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Coverage => "coverage",
            Self::Benchmarks => "benchmarks",
        }
    }
}

impl fmt::Display for Column {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The 12 caveat codes a row's `caveats` list may hold (ADR-0008 §6.1).
///
/// A caveat is not a drop: the number exists, and this is what a reader should know about it. The
/// timeline shows one `*`; the detail view, `--explain` and `report --format json` name the code,
/// which is the only reason the codes exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Caveat {
    /// A scraped percentage was present and was not used.
    PercentOnly,
    /// Text-scraped evidence was present and was not used.
    LowConfidence,
    /// At least one pooled run had `found == 0`.
    Vacuous,
    /// A run published no total, so its total is the reader's sum of its file records.
    TotalFromFileSum,
    /// `coverage.secondary` holds numbers of another measure.
    SecondaryMeasures,
    /// A record measured something other than the primary measure and was left out of the pool.
    MeasureNotPrimary,
    /// `excluded_runs` names runs that happened and were not used.
    RunExcluded,
    /// `superseded_runs` names older runs whose newest won.
    SupersededRuns,
    /// `unreadable_records` counts records this build could not read.
    Unreadable,
    /// A benchmark group has no unit: displayable, never comparable.
    UnitMissing,
    /// A group's runs at this commit came from different `source`s.
    SourceChanged,
    /// A group is `act_log`-derived context, not a measurement of the code.
    ContextOnly,
}

impl Caveat {
    /// The model's spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PercentOnly => "percent_only",
            Self::LowConfidence => "low_confidence",
            Self::Vacuous => "vacuous",
            Self::TotalFromFileSum => "total_from_file_sum",
            Self::SecondaryMeasures => "secondary_measures",
            Self::MeasureNotPrimary => "measure_not_primary",
            Self::RunExcluded => "run_excluded",
            Self::SupersededRuns => "superseded_runs",
            Self::Unreadable => "unreadable",
            Self::UnitMissing => "unit_missing",
            Self::SourceChanged => "source_changed",
            Self::ContextOnly => "context_only",
        }
    }

    /// Every code the model declares, in the model's own order.
    pub const ALL: [Self; 12] = [
        Self::PercentOnly,
        Self::LowConfidence,
        Self::Vacuous,
        Self::TotalFromFileSum,
        Self::SecondaryMeasures,
        Self::MeasureNotPrimary,
        Self::RunExcluded,
        Self::SupersededRuns,
        Self::Unreadable,
        Self::UnitMissing,
        Self::SourceChanged,
        Self::ContextOnly,
    ];
}

impl fmt::Display for Caveat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Caveat {
    type Err = UnknownCode;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|caveat| caveat.as_str() == text)
            .ok_or(UnknownCode {
                kind: "caveat",
                value: text.to_owned(),
            })
    }
}

/// The four action states (ADR-0008 §5.3). Exactly one per commit, and `no_evidence` *is*
/// unknown: it renders as the unknown glyph and never as a pass and never as a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionState {
    /// Every group's newest run succeeded.
    Success,
    /// No group's newest run succeeded.
    Failed,
    /// Some did and some did not — the common case, not an error state.
    Mixed,
    /// No records at all for this SHA.
    NoEvidence,
}

impl ActionState {
    /// The model's spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failed => "failed",
            Self::Mixed => "mixed",
            Self::NoEvidence => "no_evidence",
        }
    }
}

impl fmt::Display for ActionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A code that is not in the closed set it was looked up in.
///
/// Reaching one means the code and `schema/metric-model.json` disagree, which is a build-time bug
/// in whichever moved, not a runtime condition to tolerate — hence a plain error type rather than
/// an `Option`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownCode {
    /// Which vocabulary (`drop_reason`, `gap_reason`, `caveat`, `measure`).
    pub kind: &'static str,
    /// The text that was not in it.
    pub value: String,
}

impl fmt::Display for UnknownCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} is not a member of the closed {} vocabulary in schema/metric-model.json",
            self.value, self.kind
        )
    }
}

impl std::error::Error for UnknownCode {}
