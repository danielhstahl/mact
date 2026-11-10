//! The `.evidence` record types: the envelope plus the two `kind` variants
//! (ADR-0002 §2 and §4).
//!
//! These types are the *only* shape the writer can express, which is the point: a
//! record that carries a coverage field on a benchmark line, a `null`, an unknown
//! `kind`, a non-finite `value` or a foreign `schema_version` is not something the
//! type system lets a caller build, so the writer cannot write it. ADR-0002 §7 asks
//! exactly that of a writer ("does the server refuse to write a version it does not
//! know how to read? — it cannot even try"), and the newtypes below are how that
//! answer is made structural rather than defensive.
//!
//! ## Field order is part of the contract
//!
//! ADR-0002 §6.1 requires "one compact JSON object per line … key order stable per
//! record type (stable enough that a golden-file test is meaningful)". So the field
//! order of each struct *is* the order on disk, and it is chosen to reproduce
//! `tests/e2e/testdata/expected/{benchmark,coverage}.jsonl` byte for byte
//! (`tests/evidence_golden.rs` asserts that). Optional fields are
//! `skip_serializing_if = "Option::is_none"`, because ADR-0002 §2.1 forbids `null`
//! anywhere: absence is the only spelling of "not reported".
//!
//! ## Two checks, one direction
//!
//! * [`Envelope::check`], [`BenchmarkRecord::check`] and [`CoverageRecord::check`] are
//!   the *fast struct check*: pattern and co-occurrence rules, no schema engine, run
//!   on every record the writer publishes.
//! * The full JSON Schema check ([`super::schema::validate_record`]) is run on top in
//!   debug builds, and is what `act-mcp validate-evidence` uses.
//!
//! The struct check is allowed to refuse a value the schema would tolerate (a
//! 200-character label key, say); it must never accept one the schema would refuse.
//! `tests/evidence_contract.rs` asserts that direction over the whole fixture corpus.

use std::{borrow::Cow, collections::BTreeMap, fmt};

use chrono::{DateTime, SecondsFormat, Utc};
use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{ser::SerializeMap, Deserialize, Serialize};
use serde_json::{Map, Number, Value};
use thiserror::Error;

use crate::evidence::shapes;

/// The one `schema_version` this build writes (ADR-0002 §7).
pub const SCHEMA_VERSION: u32 = 1;

/// `tool.name`, and the only value the schema allows.
pub const TOOL_NAME: &str = "act-mcp";

/// ADR-0002 §2.2: `flags` is unique and ≤ 8 entries.
pub const MAX_FLAGS: usize = 8;
/// ADR-0002 §4.1: `labels` is at most 16 entries.
pub const MAX_LABELS: usize = 16;
/// ADR-0002 §2.2: `duration_ms` is bounded at 72 h (a run that long is a bug, and a
/// consumer must be able to trust the number without a second bound of its own).
pub const MAX_DURATION_MS: u64 = 259_200_000;

/// Why a value could not become part of a record.
#[allow(missing_docs, reason = "the #[error] string documents each variant")]
#[derive(Debug, Error)]
pub enum RecordError {
    #[error("{field} = {value:?}: {reason}")]
    Shape {
        field: &'static str,
        value: String,
        reason: String,
    },
    #[error("unsupported_schema_version: {found}, and this build only knows {known}")]
    UnsupportedSchemaVersion { found: i64, known: u32 },
    #[error("record is not a JSON object")]
    NotAnObject,
}

impl RecordError {
    /// The machine-readable code a caller stamps on `meta.json` / a tool error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Shape { .. } | Self::NotAnObject => "record_invalid",
            Self::UnsupportedSchemaVersion { .. } => "unsupported_schema_version",
        }
    }
}

/// Fail `field` with `reason` unless `ok`.
fn ensure(ok: bool, field: &'static str, value: &str, reason: &str) -> Result<(), RecordError> {
    if ok {
        Ok(())
    } else {
        Err(RecordError::Shape {
            field,
            value: value.to_owned(),
            reason: reason.to_owned(),
        })
    }
}

// ---------------------------------------------------------------------------
// `kind`
// ---------------------------------------------------------------------------

/// The closed `kind` enum: the record variant, and therefore the directory it is
/// filed under (ADR-0002 §2.2). Closed on purpose — a third kind is a version bump.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// A benchmark metric line, filed under `benchmark/`.
    Benchmark,
    /// A coverage line, filed under `coverage/`.
    Coverage,
}

impl Kind {
    /// The wire spelling, which is also the directory name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Benchmark => "benchmark",
            Self::Coverage => "coverage",
        }
    }

    /// Parse the closed enum; anything else is a hard error, never a tolerated unknown
    /// value (ADR-0002 §2.2).
    pub fn parse(text: &str) -> Result<Self, RecordError> {
        match text {
            "benchmark" => Ok(Self::Benchmark),
            "coverage" => Ok(Self::Coverage),
            other => Err(RecordError::Shape {
                field: "kind",
                value: other.to_owned(),
                reason: "kind is the closed enum benchmark | coverage (ADR-0002 §2.2)".to_owned(),
            }),
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// newtypes over strings and numbers
// ---------------------------------------------------------------------------

/// `schema_version`, which a caller cannot construct at a value this build does not
/// know.
///
/// Deserializing a foreign version fails with [`RecordError::UnsupportedSchemaVersion`],
/// so the one place a reader could pick up a record is also the place it refuses it.
/// A writer has no such path: [`SchemaVersion::CURRENT`] is the only value obtainable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct SchemaVersion(u32);

impl SchemaVersion {
    /// The version this build writes.
    pub const CURRENT: Self = Self(SCHEMA_VERSION);

    /// The numeric value.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// Accept only a version this build knows.
    pub fn parse(value: i64) -> Result<Self, RecordError> {
        u32::try_from(value)
            .ok()
            .filter(|known| *known == SCHEMA_VERSION)
            .map(Self)
            .ok_or(RecordError::UnsupportedSchemaVersion {
                found: value,
                known: SCHEMA_VERSION,
            })
    }
}

impl<'de> Deserialize<'de> for SchemaVersion {
    /// Reading a record refuses a version this build does not know, which is ADR-0002 §7's
    /// "readers must refuse what they cannot understand" — and the reason `schema_version`
    /// is a type rather than a `u32` someone can set to anything.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let found = i64::deserialize(deserializer).map_err(serde::de::Error::custom)?;
        Self::parse(found).map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for SchemaVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A `run_id` (`^[0-9]{8}T[0-9]{6}Z-[0-9a-f]{6}$`, ADR-0002 §1.2).
///
/// A newtype rather than a `String` because it is spliced into a filename. The runner
/// owns generating one (`crate::runner::RunId`); this is the same shape re-validated
/// on the way into a record, and [`From`] is implemented so the runner's type converts
/// rather than round-tripping through `String`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunId(String);

impl RunId {
    /// Accept `text` only in the pinned shape.
    pub fn parse(text: &str) -> Result<Self, RecordError> {
        ensure(
            shapes::is_run_id(text),
            "run_id",
            text,
            "expected YYYYMMDDTHHMMSSZ-<6 lowercase hex> (ADR-0002 §1.2)",
        )?;
        Ok(Self(text.to_owned()))
    }

    /// The id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The full lowercase commit a run is attributed to (`^[0-9a-f]{40}$`).
///
/// Spliced into a directory name, so it gets a type: the join key a cross-commit
/// scanner depends on is unrepresentably wrong by construction (ADR-0002 §1.2).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Commit(String);

impl Commit {
    /// Accept `text` only as a full, lowercase hex object id.
    pub fn parse(text: &str) -> Result<Self, RecordError> {
        ensure(
            shapes::is_commit(text),
            "commit",
            text,
            "expected 40 lowercase hex characters; short SHAs and uppercase are refused (ADR-0002 §2.2)",
        )?;
        Ok(Self(text.to_owned()))
    }

    /// The SHA as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Commit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A workspace-relative path used as a *value* (`path`, `source_path`), per
/// `$defs/relpath`.
///
/// The traversal ADR-0002 §6.3 refuses in filesystem paths is refused here too, so a
/// record can never name `../../etc/passwd` or `/srv/src/app.py` (the container's
/// view of the workspace, which is a privacy leak and wrong on every other host).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RelPath(String);

impl RelPath {
    /// Accept `text` only as a workspace-relative path.
    pub fn parse(text: &str) -> Result<Self, RecordError> {
        ensure(
            shapes::is_relpath(text),
            "relpath",
            text,
            "expected a workspace-relative path: '/' separators, no leading '/', no '~', no '.' or '..' segment, <= 512 characters (ADR-0002 $defs/relpath)",
        )?;
        Ok(Self(text.to_owned()))
    }

    /// The path as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A UTC instant in the pinned wire form (`…Z`, ≤ 6 fractional digits).
///
/// [`UtcInstant::from_datetime`] is the only way a clock reaches a record, and it
/// canonicalises to millisecond precision as ADR-0002 §2.2 requires; an offset like
/// `+01:00` is not expressible.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UtcInstant(String);

impl UtcInstant {
    /// The pinned form of `when`, to the millisecond.
    #[must_use]
    pub fn from_datetime(when: DateTime<Utc>) -> Self {
        Self(when.to_rfc3339_opts(SecondsFormat::Millis, true))
    }

    /// Now, canonicalised.
    #[must_use]
    pub fn now() -> Self {
        Self::from_datetime(Utc::now())
    }

    /// Accept an already-formatted instant (what a fixture or a replayed run carries).
    pub fn parse(text: &str) -> Result<Self, RecordError> {
        ensure(
            shapes::is_instant(text),
            "started_at",
            text,
            "expected a UTC instant ending in Z, fractional seconds optional (ADR-0002 §2.2)",
        )?;
        Ok(Self(text.to_owned()))
    }

    /// The wire form.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for UtcInstant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A `value` that can only be a finite JSON number.
///
/// ADR-0002 §3.3 and §6.1: non-finite values are rejected *before* serialisation,
/// because JSON cannot express them and a consumer that parses `NaN` out of a file
/// would be parsing something the standard does not allow. `serde_json::Number` keeps
/// an integer integral, which is also what makes the goldens byte-stable: `3` prints
/// as `3`, not `3.0`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MetricValue(Number);

impl MetricValue {
    /// An integral value (`3` stays `3` on disk).
    #[must_use]
    pub fn from_i64(value: i64) -> Self {
        Self(Number::from(value))
    }

    /// A fractional value, or `None` when it is not finite.
    #[must_use]
    pub fn from_f64(value: f64) -> Option<Self> {
        Number::from_f64(value).map(Self)
    }

    /// Integral when the value happens to be integral, fractional otherwise — what an
    /// extractor wants when a job's JSON may carry `3` or `3.5` for the same metric.
    #[must_use]
    pub fn coerce_f64(value: f64) -> Option<Self> {
        if !value.is_finite() {
            return None;
        }
        if value == value.trunc() && value.abs() <= 9.0e15 {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let integral = value as i64;
            return Some(Self::from_i64(integral));
        }
        Self::from_f64(value)
    }

    /// The value as an `f64` (lossless for the values this type can hold in practice).
    #[must_use]
    pub fn as_f64(&self) -> f64 {
        self.0.as_f64().unwrap_or(0.0)
    }

    /// Borrow the underlying JSON number.
    #[must_use]
    pub const fn as_number(&self) -> &Number {
        &self.0
    }
}

impl fmt::Display for MetricValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl JsonSchema for MetricValue {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        Cow::Borrowed("MetricValue")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        schema_fragment(serde_json::json!({ "type": "number" }))
    }
}

/// `tool`: `{name: "act-mcp", version, act_version?}` (ADR-0002 §2.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Tool {
    /// Always `act-mcp`; the schema allows no other producer.
    pub name: String,
    /// This server's version (`CARGO_PKG_VERSION`).
    pub version: String,
    /// The first version token of `act --version`. Absent when act reported none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub act_version: Option<String>,
}

impl Tool {
    /// The producer identity of this build.
    #[must_use]
    pub fn this_server() -> Self {
        Self {
            name: TOOL_NAME.to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            act_version: None,
        }
    }

    /// Stamp act's reported version (an act upgrade moves timings and log shapes, so
    /// the number that explains a benchmark shift is recorded, not inferred).
    #[must_use]
    pub fn with_act_version(mut self, act_version: Option<String>) -> Self {
        self.act_version = act_version.filter(|text| !text.is_empty());
        self
    }
}

impl Default for Tool {
    fn default() -> Self {
        Self::this_server()
    }
}

/// The five terminal run statuses, spelled as ADR-0001 §3.2 / ADR-0002 §2.2 spell them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// act exited 0.
    Success,
    /// act ran and the workflow failed — a normal result, not an error.
    Failed,
    /// A caller took the run down.
    Cancelled,
    /// The run's timeout expired.
    TimedOut,
    /// The server could not run the job to completion.
    Error,
}

impl RunStatus {
    /// The wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed_out",
            Self::Error => "error",
        }
    }
}

impl fmt::Display for RunStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The `source` of a benchmark metric (ADR-0002 §5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BenchmarkSource {
    /// The payload file the job wrote through the bind mount — authoritative.
    JobFile,
    /// An `ACTMCP_BENCHMARK` marker line scraped from act's log.
    InBandStdout,
    /// A timing derived from act's own `--json` output.
    ActLog,
}

impl BenchmarkSource {
    /// The wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::JobFile => "job_file",
            Self::InBandStdout => "in_band_stdout",
            Self::ActLog => "act_log",
        }
    }

    /// Whether the record must name the file it came from (ADR-0002 §4.1).
    #[must_use]
    pub const fn needs_source_path(self) -> bool {
        matches!(self, Self::JobFile)
    }
}

/// The `source` of a coverage record (ADR-0002 §5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CoverageSource {
    /// A parsed `lcov.info`.
    Lcov,
    /// A parsed istanbul `coverage-summary.json`.
    CoverageSummary,
    /// A percentage scraped out of act's log — last resort, must be flagged.
    LogScrape,
}

impl CoverageSource {
    /// The wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lcov => "lcov",
            Self::CoverageSummary => "coverage_summary",
            Self::LogScrape => "log_scrape",
        }
    }

    /// Whether the record must name the file it came from (ADR-0002 §4.2).
    #[must_use]
    pub const fn needs_source_path(self) -> bool {
        matches!(self, Self::Lcov | Self::CoverageSummary)
    }
}

/// `scope`: is this record the run total or one source file (ADR-0002 §4.2)?
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// The workspace/run total.
    Total,
    /// One source file, named by `path`.
    File,
}

impl Scope {
    /// The wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Total => "total",
            Self::File => "file",
        }
    }
}

/// Turn a literal into a `Schema` (same helper shape `tools::list_tasks` uses).
fn schema_fragment(raw: Value) -> Schema {
    Schema::try_from(raw).expect("the literal schema fragments written here are schemas")
}

/// Which measure a scraped `reported_percent` refers to (ADR-0002 §4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ReportedMeasure {
    /// A line-coverage percentage.
    Lines,
    /// A statement-coverage percentage (istanbul's unit).
    Statements,
    /// A branch-coverage percentage.
    Branches,
    /// A function-coverage percentage.
    Functions,
    /// The scraped line did not say. Chosen over guessing.
    Unknown,
}

impl ReportedMeasure {
    /// The wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lines => "lines",
            Self::Statements => "statements",
            Self::Branches => "branches",
            Self::Functions => "functions",
            Self::Unknown => "unknown",
        }
    }
}

/// The flag stamped on a metric that arrived without a unit (ADR-0002 §4.1, §5.1).
pub const FLAG_UNIT_MISSING: &str = "unit_missing";
/// The flag stamped on a coverage record that carries a percentage and no raw counts.
pub const FLAG_PERCENT_ONLY: &str = "percent_only";
/// The flag stamped on a record derived from text scraping rather than from a
/// machine-readable file (ADR-0002 §5.1).
pub const FLAG_LOW_CONFIDENCE: &str = "low_confidence";
/// The flag stamped on every record of a run whose metric cap was hit.
pub const FLAG_TRUNCATED: &str = "truncated";

// The pinned patterns, restated as `schema_with` fragments so a derived
// `JsonSchema` for a record says what `schema/evidence.schema.json` says. The pinned
// file stays the contract; these are its mirrors.

/// `$defs/relpath`.
fn relpath_schema(_: &mut SchemaGenerator) -> Schema {
    schema_fragment(serde_json::json!({
        "type": "string",
        "minLength": 1,
        "maxLength": 512,
        "pattern": shapes::RELPATH_PATTERN
    }))
}

/// `flags[]`.
fn flags_schema(_: &mut SchemaGenerator) -> Schema {
    schema_fragment(serde_json::json!({
        "type": "array",
        "maxItems": MAX_FLAGS,
        "uniqueItems": true,
        "items": { "type": "string", "minLength": 1, "maxLength": 32, "pattern": shapes::WORD_PATTERN }
    }))
}

/// `labels`.
fn labels_schema(_: &mut SchemaGenerator) -> Schema {
    schema_fragment(serde_json::json!({
        "type": "object",
        "maxProperties": MAX_LABELS,
        "propertyNames": { "type": "string", "pattern": shapes::WORD_PATTERN },
        "additionalProperties": { "type": "string", "maxLength": 128 }
    }))
}

/// `extra` — advisory, nothing may depend on a key here.
fn extra_schema(_: &mut SchemaGenerator) -> Schema {
    schema_fragment(serde_json::json!({ "type": "object", "maxProperties": 32 }))
}

/// A pattern-constrained `String` field, built from the pinned pattern.
fn pattern_schema(
    _generator: &mut SchemaGenerator,
    min: usize,
    max: usize,
    pattern: &'static str,
) -> Schema {
    schema_fragment(serde_json::json!({
        "type": "string",
        "minLength": min,
        "maxLength": max,
        "pattern": pattern
    }))
}

/// `workflow` / `task`: `^[A-Za-z0-9][A-Za-z0-9 ._/-]*$`.
fn identifier_schema(generator: &mut SchemaGenerator) -> Schema {
    pattern_schema(generator, 1, 255, shapes::IDENTIFIER_PATTERN)
}

/// `category`: `^[a-z][a-z0-9_]*$`, ≤ 64.
fn category_schema(generator: &mut SchemaGenerator) -> Schema {
    pattern_schema(generator, 1, 64, shapes::WORD_PATTERN)
}

/// `host`: `^[A-Za-z0-9][A-Za-z0-9._-]*$`.
fn hostname_schema(generator: &mut SchemaGenerator) -> Schema {
    pattern_schema(generator, 1, 255, shapes::HOSTNAME_PATTERN)
}

/// A metric / coverage `name`: `^[A-Za-z][A-Za-z0-9_.:/-]*$`.
fn metric_name_schema(generator: &mut SchemaGenerator) -> Schema {
    pattern_schema(generator, 1, 128, shapes::METRIC_NAME_PATTERN)
}

/// `unit`: `^[A-Za-z0-9_%/.^+-]+$`.
fn unit_schema(generator: &mut SchemaGenerator) -> Schema {
    pattern_schema(generator, 1, 32, shapes::UNIT_PATTERN)
}

/// `started_at`: an instant ending in `Z`.
fn instant_schema(generator: &mut SchemaGenerator) -> Schema {
    pattern_schema(generator, 20, 32, shapes::INSTANT_PATTERN)
}

/// The newtypes advertise their pinned shape to `schemars` as a constrained string,
/// so a derived schema for a record is a faithful (if partial) restatement.
impl JsonSchema for SchemaVersion {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        Cow::Borrowed("SchemaVersion")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        schema_fragment(serde_json::json!({ "type": "integer", "const": SCHEMA_VERSION }))
    }
}

impl JsonSchema for RunId {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        Cow::Borrowed("RunId")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        pattern_schema(generator, 23, 23, shapes::RUN_ID_PATTERN)
    }
}

impl JsonSchema for Commit {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        Cow::Borrowed("Commit")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        pattern_schema(generator, 40, 40, shapes::COMMIT_PATTERN)
    }
}

impl JsonSchema for RelPath {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        Cow::Borrowed("RelPath")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        relpath_schema(generator)
    }
}

impl JsonSchema for UtcInstant {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        Cow::Borrowed("UtcInstant")
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        instant_schema(generator)
    }
}

// ---------------------------------------------------------------------------
// the envelope (ADR-0002 §2.2)
// ---------------------------------------------------------------------------

/// The envelope every line of every evidence file carries.
///
/// `category` defaults to the `kind` word, which is what ADR-0002 §2.2 pins for v1
/// (`category == kind` always); the setter exists for the day a benchmark run turns out
/// to have produced a coverage file and the two must be allowed to diverge.
#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
pub struct Envelope {
    /// Stamped, never caller-chosen: see [`SchemaVersion`].
    schema_version: SchemaVersion,
    /// Stamped by the record variant that owns this envelope; read with [`Envelope::kind`].
    kind: Kind,
    /// Which run produced this line.
    pub run_id: RunId,
    /// Which commit the run is attributed to.
    pub commit: Commit,
    /// Workflow identity as the validator resolved it — never a path.
    #[schemars(schema_with = "identifier_schema")]
    pub workflow: String,
    /// The job id that ran (`act -j <job>`).
    #[schemars(schema_with = "identifier_schema")]
    pub task: String,
    /// The classification decision that caused evidence to be written at all.
    #[schemars(schema_with = "category_schema")]
    pub category: String,
    /// How the run ended. Non-`success` runs still write evidence.
    pub run_status: RunStatus,
    /// Producer identity.
    pub tool: Tool,
    /// When the run started, UTC, millisecond precision.
    #[schemars(schema_with = "instant_schema")]
    pub started_at: UtcInstant,
    /// Whole-run wall duration. Absent when unknown — never `0` as a stand-in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// act's wait(2) code. Absent when act produced none (signalled, spawn failure).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<u8>,
    /// Lowercased hostname of the machine that ran act. Advisory, never a join key.
    #[schemars(schema_with = "hostname_schema")]
    pub host: String,
}

impl Envelope {
    /// Build an envelope from the fields every run must answer for.
    #[must_use]
    #[allow(
        clippy::too_many_arguments,
        reason = "the envelope carries nine required contract fields (ADR-0002 §2.2); they are all mandatory, so a builder would only rename the arguments"
    )]
    pub fn new(
        kind: Kind,
        run_id: RunId,
        commit: Commit,
        workflow: String,
        task: String,
        run_status: RunStatus,
        tool: Tool,
        started_at: UtcInstant,
        host: String,
    ) -> Self {
        Self {
            schema_version: SchemaVersion::CURRENT,
            kind,
            run_id,
            commit,
            workflow,
            task,
            category: kind.as_str().to_owned(),
            run_status,
            tool,
            started_at,
            duration_ms: None,
            exit_code: None,
            host,
        }
    }

    /// Which variant this envelope is being used in.
    #[must_use]
    pub const fn kind(&self) -> Kind {
        self.kind
    }

    /// Re-stamp `kind`. Only the record constructors in this module should need it: the
    /// variant is the authority on `kind`, because the variant decides the directory.
    #[must_use]
    pub fn with_kind(mut self, kind: Kind) -> Self {
        self.kind = kind;
        self
    }

    /// Override the classification (ADR-0002 §2.2 keeps it distinct from `kind` for
    /// exactly this reason).
    #[must_use]
    pub fn with_category(mut self, category: impl Into<String>) -> Self {
        self.category = category.into();
        self
    }

    /// Stamp the whole-run duration.
    #[must_use]
    pub const fn with_duration_ms(mut self, duration_ms: u64) -> Self {
        self.duration_ms = Some(duration_ms);
        self
    }

    /// Stamp act's exit code.
    #[must_use]
    pub const fn with_exit_code(mut self, exit_code: u8) -> Self {
        self.exit_code = Some(exit_code);
        self
    }

    /// The fast struct check: shape of every free-text field, and the bounds the
    /// schema states as `minimum`/`maximum`. The newtyped fields were validated when
    /// they were parsed, so this cannot fail on a value that came in through a
    /// constructor — it exists for the ones that came in as text.
    ///
    /// # Errors
    ///
    /// [`RecordError::Shape`] for the first field that fails, named and quoted.
    pub fn check(&self) -> Result<(), RecordError> {
        ensure(
            self.schema_version == SchemaVersion::CURRENT,
            "schema_version",
            &self.schema_version.to_string(),
            "a writer only ever emits the version it was built with (ADR-0002 §7)",
        )?;
        ensure(
            shapes::is_identifier(&self.workflow),
            "workflow",
            &self.workflow,
            "workflow identity, never a path (ADR-0002 §2.2)",
        )?;
        ensure(
            shapes::is_identifier(&self.task),
            "task",
            &self.task,
            "the resolved job id (ADR-0002 §2.2)",
        )?;
        ensure(
            shapes::is_word(&self.category),
            "category",
            &self.category,
            "open vocabulary, but spelled `^[a-z][a-z0-9_]*$` (ADR-0002 §2.2)",
        )?;
        ensure(
            shapes::is_hostname(&self.host),
            "host",
            &self.host,
            "lowercased hostname (ADR-0002 §2.2)",
        )?;
        ensure(
            self.tool.name == TOOL_NAME,
            "tool.name",
            &self.tool.name,
            "the only producer this schema admits",
        )?;
        ensure(
            !self.tool.version.is_empty() && self.tool.version.len() <= 64,
            "tool.version",
            &self.tool.version,
            "1..=64 characters",
        )?;
        if let Some(act_version) = &self.tool.act_version {
            ensure(
                !act_version.is_empty() && act_version.len() <= 64,
                "tool.act_version",
                act_version,
                "1..=64 characters when present",
            )?;
        }
        if let Some(duration_ms) = self.duration_ms {
            ensure(
                duration_ms <= MAX_DURATION_MS,
                "duration_ms",
                &duration_ms.to_string(),
                "at most 72 h in ms (ADR-0002 §2.2)",
            )?;
        }
        Ok(())
    }

    /// Write the envelope's entries into `map`, in the pinned order.
    ///
    /// This is hand-written rather than `#[serde(flatten)]`-derived on purpose: serde's
    /// flatten path buffers into a `serde_json::Map`, which is a `BTreeMap`, so flattened
    /// records come out with their keys **alphabetically** — a valid JSON object, but not
    /// the byte sequence the goldens pin (ADR-0002 §6.1 makes key order part of the
    /// contract precisely so a golden diff is meaningful). There is no way to state that
    /// order in a derive, so it is stated here instead. `tests/evidence_golden.rs` is what
    /// fails if this list and the fixture ever drift.
    pub(crate) fn write_into<K: serde::ser::SerializeMap>(
        &self,
        map: &mut K,
    ) -> Result<(), K::Error> {
        map.serialize_entry("schema_version", &self.schema_version)?;
        map.serialize_entry("kind", &self.kind)?;
        map.serialize_entry("run_id", &self.run_id)?;
        map.serialize_entry("commit", &self.commit)?;
        map.serialize_entry("workflow", &self.workflow)?;
        map.serialize_entry("task", &self.task)?;
        map.serialize_entry("category", &self.category)?;
        map.serialize_entry("run_status", &self.run_status)?;
        map.serialize_entry("tool", &self.tool)?;
        map.serialize_entry("started_at", &self.started_at)?;
        if let Some(duration_ms) = self.duration_ms {
            map.serialize_entry("duration_ms", &duration_ms)?;
        }
        if let Some(exit_code) = self.exit_code {
            map.serialize_entry("exit_code", &exit_code)?;
        }
        map.serialize_entry("host", &self.host)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// per-record payload (ADR-0002 §4.1, §4.2)
// ---------------------------------------------------------------------------

/// Does `flags` name `flag`?
fn has_flag(flags: &Option<Vec<String>>, flag: &str) -> bool {
    flags
        .as_ref()
        .is_some_and(|flags| flags.iter().any(|held| held == flag))
}

/// `flags` is optional, unique, ≤ [`MAX_FLAGS`], and each member is a small word.
fn check_flags(flags: &Option<Vec<String>>) -> Result<(), RecordError> {
    let Some(flags) = flags else { return Ok(()) };
    let shown = flags.join(",");
    ensure(
        flags.len() <= MAX_FLAGS,
        "flags",
        &shown,
        "at most 8 flags (ADR-0002 §2.2)",
    )?;
    let unique: std::collections::BTreeSet<&str> = flags.iter().map(String::as_str).collect();
    ensure(
        unique.len() == flags.len(),
        "flags",
        &shown,
        "flags are a set; a repeated flag is a writer bug",
    )?;
    for flag in flags {
        ensure(
            shapes::is_short_word(flag),
            "flags",
            flag,
            "open vocabulary, but spelled `^[a-z][a-z0-9_]*$` and <= 32 characters (ADR-0002 §2.2)",
        )?;
    }
    Ok(())
}

/// `labels` is a flat string→string map, ≤ [`MAX_LABELS`] entries, keys spelled like
/// `category`, values always strings (ADR-0002 §4.1).
fn check_labels(labels: &Option<BTreeMap<String, String>>) -> Result<(), RecordError> {
    let Some(labels) = labels else { return Ok(()) };
    ensure(
        labels.len() <= MAX_LABELS,
        "labels",
        &format!("{} entries", labels.len()),
        "at most 16 labels (ADR-0002 §4.1)",
    )?;
    for (key, value) in labels {
        ensure(
            shapes::is_word(key),
            "labels",
            key,
            "a label key is `^[a-z][a-z0-9_]*$` so a group-by never type-switches (ADR-0002 §4.1)",
        )?;
        ensure(
            shapes::is_label_value(value),
            "labels",
            value,
            "label values are strings of at most 128 bytes (ADR-0002 §4.1)",
        )?;
    }
    Ok(())
}

/// The metric half of a benchmark line (ADR-0002 §4.1).
///
/// A benchmark record cannot carry a coverage field and a coverage record cannot carry
/// this — the two structs do not overlap, which is the type-level version of the
/// `false` subschemas the pinned schema uses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct BenchmarkMetric {
    /// The metric name, verbatim from the job, or one of the built-in
    /// `job_duration_s` / `step_duration_s`.
    #[schemars(schema_with = "metric_name_schema")]
    pub name: String,
    /// The measured value; finite by construction.
    pub value: MetricValue,
    /// Verbatim from the job. Absent means the job reported none, which must be
    /// flagged (ADR-0002 §4.1).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "unit_schema")]
    pub unit: Option<String>,
    /// Flat grouping keys. A `BTreeMap`, so the serialized order is stable.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "labels_schema")]
    pub labels: Option<BTreeMap<String, String>>,
    /// 1-based sample index; present only when a job emitted several samples for the
    /// same `(name, labels)` (ADR-0002 §4.4).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample: Option<u32>,
    /// Where the number came from.
    pub source: BenchmarkSource,
    /// The payload file it came from; required for `job_file`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_path: Option<RelPath>,
    /// Machine-readable caveats.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "flags_schema")]
    pub flags: Option<Vec<String>>,
    /// Advisory passthrough; nothing load-bearing may live here (ADR-0002 §2.2).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "extra_schema")]
    pub extra: Option<Map<String, Value>>,
}

impl BenchmarkMetric {
    /// The three required fields; everything else starts absent.
    #[must_use]
    pub fn new(name: String, value: MetricValue, source: BenchmarkSource) -> Self {
        Self {
            name,
            value,
            unit: None,
            labels: None,
            sample: None,
            source,
            source_path: None,
            flags: None,
            extra: None,
        }
    }

    /// Stamp the unit verbatim (act-mcp never converts one, ADR-0002 §4.1).
    #[must_use]
    pub fn with_unit(mut self, unit: impl Into<String>) -> Self {
        self.unit = Some(unit.into());
        self
    }

    /// Attach the grouping labels.
    #[must_use]
    pub fn with_labels(mut self, labels: BTreeMap<String, String>) -> Self {
        self.labels = Some(labels);
        self
    }

    /// Number a sample `1..n` (ADR-0002 §4.4: present only when a job really emitted
    /// more than one sample for the same `(name, labels)`).
    #[must_use]
    pub const fn with_sample(mut self, sample: u32) -> Self {
        self.sample = Some(sample);
        self
    }

    /// Name the payload file the metric was parsed from.
    #[must_use]
    pub fn with_source_path(mut self, path: RelPath) -> Self {
        self.source_path = Some(path);
        self
    }

    /// Add one caveat flag, ignoring a repeat of a flag already there (§2.2 says flags
    /// are a set).
    pub fn with_flag(mut self, flag: &str) -> Self {
        let flags = self.flags.get_or_insert_with(Vec::new);
        if !flags.iter().any(|held| held == flag) {
            flags.push(flag.to_owned());
        }
        self
    }

    /// A metric with no unit must say it has none (ADR-0002 §4.1): absence plus
    /// `unit_missing`, so a consumer can tell "not reported" from a real unit.
    ///
    /// # Errors
    ///
    /// [`RecordError::Shape`] on the first field that fails the contract.
    pub fn check(&self) -> Result<(), RecordError> {
        ensure(
            shapes::is_metric_name(&self.name),
            "name",
            &self.name,
            "a metric name starts with a letter and stays inside `[A-Za-z0-9_.:/-]` (ADR-0002 §4.1)",
        )?;
        if let Some(unit) = &self.unit {
            ensure(
                shapes::is_unit(unit),
                "unit",
                unit,
                "a unit is verbatim from the job, 1..=32 characters, and never the empty string (ADR-0002 §4.1)",
            )?;
        } else {
            ensure(
                has_flag(&self.flags, FLAG_UNIT_MISSING),
                "flags",
                &self
                    .flags
                    .as_ref()
                    .map_or_else(String::new, |flags| flags.join(",")),
                "a metric reported without a unit must carry flags:[\"unit_missing\"] (ADR-0002 §4.1)",
            )?;
        }
        check_flags(&self.flags)?;
        check_labels(&self.labels)?;
        if let Some(sample) = self.sample {
            ensure(
                (1..=1_000_000).contains(&sample),
                "sample",
                &sample.to_string(),
                "1-based (ADR-0002 §4.1)",
            )?;
        }
        self.check_source_path()?;
        Ok(())
    }

    /// `job_file` metrics must name their file; a `RelPath` already refuses the
    /// absolute and the traversing form (ADR-0002 §4.1, §6.3).
    fn check_source_path(&self) -> Result<(), RecordError> {
        if self.source.needs_source_path() {
            ensure(
                self.source_path.is_some(),
                "source_path",
                "<absent>",
                &format!(
                    "source {:?} must name the payload file it was read from (ADR-0002 §4.1)",
                    self.source
                ),
            )?;
        }
        Ok(())
    }
}

/// The counts half of a coverage line (ADR-0002 §4.2).
///
/// Raw numerators and denominators only: a record never carries a rate next to the
/// counts it came from (ADR-0002 §4.3). The single exception is `reported_percent`,
/// which is possible only with `source: log_scrape`, only with `reported_measure`, and
/// only under the `percent_only` flag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CoverageCounts {
    /// A label, conventionally `"total"` on a total record.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "metric_name_schema")]
    pub name: Option<String>,
    /// Total or one file.
    pub scope: Scope,
    /// The source file, workspace-relative; required for `file`, forbidden for `total`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<RelPath>,
    /// LINE denominator.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines_found: Option<u64>,
    /// LINE numerator.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines_hit: Option<u64>,
    /// istanbul statement denominator — a different unit from lines, never averaged
    /// with them (ADR-0002 §4.2).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub statements_found: Option<u64>,
    /// istanbul statement numerator.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub statements_hit: Option<u64>,
    /// Branch denominator.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branches_found: Option<u64>,
    /// Branch numerator.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branches_hit: Option<u64>,
    /// istanbul function denominator (reserved in v1; ADR-0002 §4.2).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub functions_found: Option<u64>,
    /// istanbul function numerator.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub functions_hit: Option<u64>,
    /// Percent-only sources (last-resort scraping). Forbidden alongside any raw pair.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reported_percent: Option<f64>,
    /// Which measure `reported_percent` refers to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reported_measure: Option<ReportedMeasure>,
    /// Where the numbers came from.
    pub source: CoverageSource,
    /// The file they came from; required for `lcov` / `coverage_summary`, forbidden for
    /// `log_scrape`, whose input is the act log.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_path: Option<RelPath>,
    /// Machine-readable caveats.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "flags_schema")]
    pub flags: Option<Vec<String>>,
    /// Advisory passthrough.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "extra_schema")]
    pub extra: Option<Map<String, Value>>,
}

impl CoverageCounts {
    /// A total record with a lines pair, which is what every real coverage run emits.
    #[must_use]
    pub fn total(lines_found: u64, lines_hit: u64, source: CoverageSource) -> Self {
        Self {
            name: Some("total".to_owned()),
            scope: Scope::Total,
            path: None,
            lines_found: Some(lines_found),
            lines_hit: Some(lines_hit),
            statements_found: None,
            statements_hit: None,
            branches_found: None,
            branches_hit: None,
            functions_found: None,
            functions_hit: None,
            reported_percent: None,
            reported_measure: None,
            source,
            source_path: None,
            flags: None,
            extra: None,
        }
    }

    /// A per-file record with a lines pair.
    #[must_use]
    pub fn file(path: RelPath, lines_found: u64, lines_hit: u64, source: CoverageSource) -> Self {
        Self {
            name: None,
            scope: Scope::File,
            path: Some(path),
            lines_found: Some(lines_found),
            lines_hit: Some(lines_hit),
            statements_found: None,
            statements_hit: None,
            branches_found: None,
            branches_hit: None,
            functions_found: None,
            functions_hit: None,
            reported_percent: None,
            reported_measure: None,
            source,
            source_path: None,
            flags: None,
            extra: None,
        }
    }

    /// Attach an istanbul statements pair.
    #[must_use]
    pub const fn with_statements(mut self, found: u64, hit: u64) -> Self {
        self.statements_found = Some(found);
        self.statements_hit = Some(hit);
        self
    }

    /// Attach a branch pair.
    #[must_use]
    pub const fn with_branches(mut self, found: u64, hit: u64) -> Self {
        self.branches_found = Some(found);
        self.branches_hit = Some(hit);
        self
    }

    /// Attach a function pair.
    #[must_use]
    pub const fn with_functions(mut self, found: u64, hit: u64) -> Self {
        self.functions_found = Some(found);
        self.functions_hit = Some(hit);
        self
    }

    /// A scraped percentage: this constructor makes the three rules of ADR-0002 §4.3
    /// (`reported_measure`, the `percent_only` flag, no raw counts) impossible to
    /// half-apply, so a last-resort source always announces itself.
    #[must_use]
    pub fn with_percent_only(mut self, percent: f64, measure: ReportedMeasure) -> Self {
        self.reported_percent = Some(percent);
        self.reported_measure = Some(measure);
        self.lines_found = None;
        self.lines_hit = None;
        self.statements_found = None;
        self.statements_hit = None;
        self.branches_found = None;
        self.branches_hit = None;
        self.functions_found = None;
        self.functions_hit = None;
        let flags = self.flags.get_or_insert_with(Vec::new);
        if !flags.iter().any(|held| held == FLAG_PERCENT_ONLY) {
            flags.push(FLAG_PERCENT_ONLY.to_owned());
        }
        self
    }

    /// Name the coverage file the numbers were parsed from.
    #[must_use]
    pub fn with_source_path(mut self, path: RelPath) -> Self {
        self.source_path = Some(path);
        self
    }

    /// Attach the advisory passthrough object (§2.2: kept for humans and for the
    /// ingest job, never read by the contract).
    #[must_use]
    pub fn with_extra(mut self, extra: Map<String, Value>) -> Self {
        self.extra = Some(extra);
        self
    }

    /// Add one caveat flag (unique, as §2.2 requires).
    pub fn with_flag(mut self, flag: &str) -> Self {
        let flags = self.flags.get_or_insert_with(Vec::new);
        if !flags.iter().any(|held| held == flag) {
            flags.push(flag.to_owned());
        }
        self
    }

    /// The fast struct check: the co-occurrence rules the pinned schema writes as
    /// `if`/`then`/`anyOf`, plus the `path` scope rule.
    ///
    /// It deliberately does **not** check `hit <= found`: the schema cannot express it
    /// either (ADR-0002 §4.2 calls it an extractor rule), and a writer that failed a
    /// run over a strange lcov would contradict §5.1's "extraction problems never fail
    /// a run".
    ///
    /// # Errors
    ///
    /// [`RecordError::Shape`] on the first rule broken.
    pub fn check(&self) -> Result<(), RecordError> {
        if let Some(name) = &self.name {
            ensure(
                shapes::is_metric_name(name),
                "name",
                name,
                "a coverage label is spelled like a metric name (ADR-0002 §4.2)",
            )?;
        }
        match (self.scope, &self.path) {
            (Scope::File, None) => {
                return Err(RecordError::Shape {
                    field: "path",
                    value: "<absent>".to_owned(),
                    reason: "scope \"file\" requires the workspace-relative path it describes"
                        .to_owned(),
                })
            }
            (Scope::Total, Some(path)) => {
                return Err(RecordError::Shape {
                    field: "path",
                    value: path.as_str().to_owned(),
                    reason:
                        "scope \"total\" describes no file, so path is forbidden (ADR-0002 §4.2)"
                            .to_owned(),
                })
            }
            _ => {}
        }

        for (found, hit, pair) in [
            (self.lines_found, self.lines_hit, "lines"),
            (self.statements_found, self.statements_hit, "statements"),
            (self.branches_found, self.branches_hit, "branches"),
            (self.functions_found, self.functions_hit, "functions"),
        ] {
            ensure(
                found.is_some() == hit.is_some(),
                pair,
                &format!("found={found:?} hit={hit:?}"),
                "a *_found / *_hit pair is co-required: a format that reports one reports both (ADR-0002 §4.2)",
            )?;
        }

        if let Some(percent) = self.reported_percent {
            ensure(
                percent.is_finite() && (0.0..=100.0).contains(&percent),
                "reported_percent",
                &percent.to_string(),
                "0..=100 (ADR-0002 §4.2)",
            )?;
            ensure(
                self.reported_measure.is_some(),
                "reported_measure",
                "<absent>",
                "a percentage must say which measure it scraped — `unknown` beats guessing (ADR-0002 §4.2)",
            )?;
            ensure(
                has_flag(&self.flags, FLAG_PERCENT_ONLY),
                "flags",
                &self
                    .flags
                    .as_ref()
                    .map_or_else(String::new, |flags| flags.join(",")),
                "a percentage with no raw counts must carry flags:[\"percent_only\"] (ADR-0002 §4.3)",
            )?;
            ensure(
                self.lines_found.is_none()
                    && self.statements_found.is_none()
                    && self.branches_found.is_none()
                    && self.functions_found.is_none(),
                "reported_percent",
                &percent.to_string(),
                "a scraped percentage may not sit next to raw counts it could contradict (ADR-0002 §4.3)",
            )?;
        } else {
            ensure(
                self.reported_measure.is_none(),
                "reported_measure",
                "<present>",
                "reported_measure only means something with reported_percent",
            )?;
            ensure(
                (self.lines_found.is_some() && self.lines_hit.is_some())
                    || (self.statements_found.is_some() && self.statements_hit.is_some()),
                "lines_found",
                "<absent>",
                "a coverage record needs one complete raw pair, or reported_percent (ADR-0002 §4.2)",
            )?;
        }

        check_flags(&self.flags)?;
        if self.source.needs_source_path() {
            ensure(
                self.source_path.is_some(),
                "source_path",
                "<absent>",
                "a file-derived coverage record must name the file it was parsed from (ADR-0002 §4.2)",
            )?;
        } else {
            ensure(
                self.source_path.is_none(),
                "source_path",
                &self
                    .source_path
                    .as_ref()
                    .map_or_else(String::new, |path| path.as_str().to_owned()),
                "log_scrape reads the act log, so it has no source_path to name (ADR-0002 §4.2)",
            )?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// whole records, and one run's worth of them
// ---------------------------------------------------------------------------

/// One benchmark line: the envelope and one metric (ADR-0002 §4.1).
///
/// `kind` is stamped from the variant, so a line written into `benchmark/` can never
/// claim `kind: "coverage"` — the directory and the record always agree, which is what
/// lets a scanner trust the path.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct BenchmarkRecord {
    /// The run identity, flattened so the line is flat JSON.
    #[serde(flatten)]
    pub envelope: Envelope,
    /// The metric this line reports.
    #[serde(flatten)]
    pub metric: BenchmarkMetric,
}

impl serde::Serialize for BenchmarkRecord {
    /// Field order is the pinned wire order (see [`Envelope::write_into`]).
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        self.envelope.write_into(&mut map)?;
        let metric = &self.metric;
        map.serialize_entry("name", &metric.name)?;
        map.serialize_entry("value", &metric.value)?;
        if let Some(unit) = &metric.unit {
            map.serialize_entry("unit", unit)?;
        }
        if let Some(labels) = &metric.labels {
            map.serialize_entry("labels", labels)?;
        }
        if let Some(sample) = metric.sample {
            map.serialize_entry("sample", &sample)?;
        }
        map.serialize_entry("source", &metric.source)?;
        if let Some(path) = &metric.source_path {
            map.serialize_entry("source_path", path)?;
        }
        if let Some(flags) = &metric.flags {
            map.serialize_entry("flags", flags)?;
        }
        if let Some(extra) = &metric.extra {
            map.serialize_entry("extra", extra)?;
        }
        map.end()
    }
}

impl BenchmarkRecord {
    /// Stamp `kind: benchmark` onto `envelope` and pair it with `metric`.
    #[must_use]
    pub fn new(envelope: &Envelope, metric: BenchmarkMetric) -> Self {
        Self {
            envelope: envelope.clone().with_kind(Kind::Benchmark),
            metric,
        }
    }

    /// The fast struct check for this line.
    ///
    /// # Errors
    ///
    /// [`RecordError::Shape`] on the first rule broken.
    pub fn check(&self) -> Result<(), RecordError> {
        ensure(
            self.envelope.kind() == Kind::Benchmark,
            "kind",
            self.envelope.kind().as_str(),
            "a benchmark record must say it is one (ADR-0002 §2.2)",
        )?;
        self.envelope.check()?;
        self.metric.check()
    }
}

/// One coverage line: the envelope and one file's or the total's counts (ADR-0002 §4.2).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CoverageRecord {
    /// The run identity, flattened so the line is flat JSON.
    #[serde(flatten)]
    pub envelope: Envelope,
    /// The counts this line reports.
    #[serde(flatten)]
    pub counts: CoverageCounts,
}

impl serde::Serialize for CoverageRecord {
    /// Field order is the pinned wire order (see [`Envelope::write_into`]): envelope,
    /// then label/scope/path, then the four count pairs in the order the istanbul golden
    /// lists them, then the scraped-percentage pair, then provenance.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        self.envelope.write_into(&mut map)?;
        let counts = &self.counts;
        if let Some(name) = &counts.name {
            map.serialize_entry("name", name)?;
        }
        map.serialize_entry("scope", &counts.scope)?;
        if let Some(path) = &counts.path {
            map.serialize_entry("path", path)?;
        }
        for (key, value) in [
            ("lines_found", counts.lines_found),
            ("lines_hit", counts.lines_hit),
            ("statements_found", counts.statements_found),
            ("statements_hit", counts.statements_hit),
            ("branches_found", counts.branches_found),
            ("branches_hit", counts.branches_hit),
            ("functions_found", counts.functions_found),
            ("functions_hit", counts.functions_hit),
        ] {
            if let Some(value) = value {
                map.serialize_entry(key, &value)?;
            }
        }
        if let Some(percent) = counts.reported_percent {
            map.serialize_entry("reported_percent", &percent)?;
        }
        if let Some(measure) = counts.reported_measure {
            map.serialize_entry("reported_measure", &measure)?;
        }
        map.serialize_entry("source", &counts.source)?;
        if let Some(path) = &counts.source_path {
            map.serialize_entry("source_path", path)?;
        }
        if let Some(flags) = &counts.flags {
            map.serialize_entry("flags", flags)?;
        }
        if let Some(extra) = &counts.extra {
            map.serialize_entry("extra", extra)?;
        }
        map.end()
    }
}

impl CoverageRecord {
    /// Stamp `kind: coverage` onto `envelope` and pair it with `counts`.
    #[must_use]
    pub fn new(envelope: &Envelope, counts: CoverageCounts) -> Self {
        Self {
            envelope: envelope.clone().with_kind(Kind::Coverage),
            counts,
        }
    }

    /// The fast struct check for this line.
    ///
    /// # Errors
    ///
    /// [`RecordError::Shape`] on the first rule broken.
    pub fn check(&self) -> Result<(), RecordError> {
        ensure(
            self.envelope.kind() == Kind::Coverage,
            "kind",
            self.envelope.kind().as_str(),
            "a coverage record must say it is one (ADR-0002 §2.2)",
        )?;
        self.envelope.check()?;
        self.counts.check()
    }
}

/// Either variant. The writer's unit of work is [`RunRecords`]; this is what a reader
/// parses a line into.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum EvidenceRecord {
    /// A benchmark line.
    Benchmark(BenchmarkRecord),
    /// A coverage line.
    Coverage(CoverageRecord),
}

impl EvidenceRecord {
    /// Dispatch on the closed `kind` field rather than letting serde guess with an
    /// untagged enum: a hybrid line must be a named error, not silently matched by the
    /// first variant whose required fields happen to fit.
    ///
    /// # Errors
    ///
    /// [`RecordError::NotAnObject`] when `value` is not an object,
    /// [`RecordError::Shape`] when `kind` is missing or unknown or the variant's fields
    /// do not fit, and [`RecordError::UnsupportedSchemaVersion`] when the line names a
    /// version this build cannot read.
    pub fn from_value(value: &Value) -> Result<Self, RecordError> {
        let object = value.as_object().ok_or(RecordError::NotAnObject)?;
        let kind = match object.get("kind").and_then(Value::as_str) {
            Some(kind) => Kind::parse(kind)?,
            None => {
                return Err(RecordError::Shape {
                    field: "kind",
                    value: "<absent>".to_owned(),
                    reason: "every evidence line carries the closed enum kind (ADR-0002 §2.2)"
                        .to_owned(),
                })
            }
        };
        if let Some(found) = object.get("schema_version").and_then(Value::as_i64) {
            SchemaVersion::parse(found)?;
        }
        match kind {
            Kind::Benchmark => serde_json::from_value::<BenchmarkRecord>(value.clone())
                .map(Self::Benchmark)
                .map_err(|err| RecordError::Shape {
                    field: "benchmark",
                    value: err.to_string(),
                    reason: "line does not fit the benchmark variant".to_owned(),
                }),
            Kind::Coverage => serde_json::from_value::<CoverageRecord>(value.clone())
                .map(Self::Coverage)
                .map_err(|err| RecordError::Shape {
                    field: "coverage",
                    value: err.to_string(),
                    reason: "line does not fit the coverage variant".to_owned(),
                }),
        }
    }

    /// The variant tag.
    #[must_use]
    pub fn kind(&self) -> Kind {
        match self {
            Self::Benchmark(_) | Self::Coverage(_) => self.envelope().kind(),
        }
    }

    /// The shared envelope.
    #[must_use]
    pub fn envelope(&self) -> &Envelope {
        match self {
            Self::Benchmark(record) => &record.envelope,
            Self::Coverage(record) => &record.envelope,
        }
    }
}

/// One run's records of one kind: exactly the contents of
/// `.evidence/<kind>/<full-sha>/<run_id>.jsonl` (ADR-0002 §1.2, §6.2).
///
/// A file is one run, and this type cannot express anything else: the variant fixes the
/// `kind`, so a batch cannot mix kinds, and [`RunRecords::check`] proves every line
/// agrees with the first about `run_id`, `commit`, `workflow` and `task` — the
/// one-run-per-file rule of ADR-0002 §6.1.
#[derive(Debug, Clone)]
pub enum RunRecords {
    /// Benchmark lines for one run.
    Benchmark {
        /// The lines, in the order they will be written.
        records: Vec<BenchmarkRecord>,
    },
    /// Coverage lines for one run.
    Coverage {
        /// The lines, in the order they will be written.
        records: Vec<CoverageRecord>,
    },
}

impl RunRecords {
    /// The `kind` this batch is filed under.
    #[must_use]
    pub fn kind(&self) -> Kind {
        match self {
            Self::Benchmark { .. } => Kind::Benchmark,
            Self::Coverage { .. } => Kind::Coverage,
        }
    }

    /// How many lines this batch is.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Benchmark { records } => records.len(),
            Self::Coverage { records } => records.len(),
        }
    }

    /// Whether there is nothing to publish (ADR-0002 §1.3: a run that produced nothing
    /// is recorded by `meta.json`, not by an empty file).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The envelope of the first record, which [`RunRecords::check`] proves all
    /// records share.
    ///
    /// # Panics
    ///
    /// If the batch is empty; call [`RunRecords::is_empty`] first.
    #[must_use]
    pub fn envelope(&self) -> &Envelope {
        match self {
            Self::Benchmark { records } => &records.first().expect("non-empty").envelope,
            Self::Coverage { records } => &records.first().expect("non-empty").envelope,
        }
    }

    /// Whether any line carries the `truncated` flag — `meta.json`'s
    /// `evidence[].truncated` is then true (ADR-0002 §5.1).
    #[must_use]
    pub fn truncated(&self) -> bool {
        let flagged = |flags: &Option<Vec<String>>| has_flag(flags, FLAG_TRUNCATED);
        match self {
            Self::Benchmark { records } => records.iter().any(|r| flagged(&r.metric.flags)),
            Self::Coverage { records } => records.iter().any(|r| flagged(&r.counts.flags)),
        }
    }

    /// Every line as a `serde_json::Value`, in file order.
    ///
    /// # Errors
    ///
    /// Serializing one of these records cannot fail — non-finite numbers are
    /// unrepresentable — so this only fails on a serializer bug.
    pub fn to_values(&self) -> Result<Vec<Value>, RecordError> {
        match self {
            Self::Benchmark { records } => records.iter().map(value_of).collect(),
            Self::Coverage { records } => records.iter().map(value_of).collect(),
        }
    }

    /// Serialize one record as one compact JSON line, LF included (ADR-0002 §6.1).
    fn write_line(record: &impl Serialize) -> Result<Vec<u8>, RecordError> {
        let mut line = Vec::new();
        serde_json::to_writer(&mut line, record).map_err(|reason| RecordError::Shape {
            field: "record",
            value: reason.to_string(),
            reason: "a record failed to serialize as compact JSON".to_owned(),
        })?;
        line.push(b'\n');
        Ok(line)
    }

    /// The file's bytes: one compact object per line, each ending in LF (ADR-0002
    /// §6.1 — the trailing LF is what makes "the last byte is not LF" a torn write).
    ///
    /// The records are serialized **directly**, not through [`RunRecords::to_values`]:
    /// `serde_json::Value` stores an object as a `BTreeMap`, so a round trip through
    /// `Value` would sort the keys and destroy the pinned field order.
    ///
    /// # Errors
    ///
    /// As [`RunRecords::to_values`].
    pub fn to_jsonl(&self) -> Result<Vec<u8>, RecordError> {
        let mut bytes = Vec::new();
        match self {
            Self::Benchmark { records } => {
                for record in records {
                    bytes.extend_from_slice(&Self::write_line(record)?);
                }
            }
            Self::Coverage { records } => {
                for record in records {
                    bytes.extend_from_slice(&Self::write_line(record)?);
                }
            }
        }
        Ok(bytes)
    }

    /// Parse a file body back into one run's records: the reader side of
    /// [`RunRecords::to_jsonl`].
    ///
    /// This is what lets a caller prove "I re-derived exactly what I already published"
    /// (ADR-0002 §5's ingest job, mact-f1w.12) and it is what the contract test round-trips
    /// the fixtures through. It is deliberately strict about the byte rules of §6.1 — a
    /// torn last line, a blank line, or a missing trailing LF is rejected rather than
    /// tolerated — because a file that breaks them is a file whose records cannot be
    /// counted.
    ///
    /// # Errors
    ///
    /// [`RecordError::Shape`] for a file that is empty, has a blank or torn line, does not
    /// end in LF, mixes kinds, or holds a record that fails [`RunRecords::check`].
    pub fn from_jsonl(bytes: &[u8]) -> Result<Self, RecordError> {
        let shape = |line: usize, reason: String| RecordError::Shape {
            field: "line",
            value: format!("line {line}"),
            reason,
        };
        if bytes.is_empty() {
            return Err(shape(
                1,
                "an evidence file holds at least one record".to_owned(),
            ));
        }
        if bytes.last() != Some(&b'\n') {
            return Err(RecordError::Shape {
                field: "file",
                value: "final byte".to_owned(),
                reason:
                    "the file does not end in LF, so its last record may be torn (ADR-0002 §6.1)"
                        .to_owned(),
            });
        }
        let lines: Vec<&[u8]> = bytes.split(|byte| *byte == b'\n').collect();
        let last = lines.len() - 1;
        let mut benchmark = Vec::new();
        let mut coverage = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            let number = index + 1;
            if index == last {
                // The piece after the final LF is empty, and only there is empty legal.
                if !line.is_empty() {
                    return Err(shape(number, "content after the final LF".to_owned()));
                }
                continue;
            }
            if line.is_empty() {
                return Err(shape(number, "blank line".to_owned()));
            }
            let value: Value = serde_json::from_slice(line)
                .map_err(|reason| shape(number, format!("{reason}")))?;
            match EvidenceRecord::from_value(&value)
                .map_err(|err| shape(number, err.to_string()))?
            {
                EvidenceRecord::Benchmark(record) => benchmark.push(record),
                EvidenceRecord::Coverage(record) => coverage.push(record),
            }
        }
        let run = match (benchmark.is_empty(), coverage.is_empty()) {
            (false, true) => Self::Benchmark { records: benchmark },
            (true, false) => Self::Coverage { records: coverage },
            (false, false) => {
                return Err(shape(
                    1,
                    "one file holds one kind of record (ADR-0002 §6.1)".to_owned(),
                ));
            }
            (true, true) => {
                return Err(shape(
                    1,
                    "an evidence file holds at least one record".to_owned(),
                ));
            }
        };
        run.check()?;
        Ok(run)
    }

    /// Check every line, and that the batch is one run's lines.
    ///
    /// # Errors
    ///
    /// [`RecordError::Shape`] naming the first line that breaks a rule or disagrees
    /// with the first record about identity.
    pub fn check(&self) -> Result<(), RecordError> {
        let problem = |index: usize, reason: String| RecordError::Shape {
            field: "record",
            value: format!("index {index}"),
            reason,
        };
        match self {
            Self::Benchmark { records } => {
                let first = records.first().map(|record| &record.envelope);
                for (index, record) in records.iter().enumerate() {
                    record
                        .check()
                        .map_err(|err| problem(index, err.to_string()))?;
                    same_run(first, &record.envelope, index)?;
                }
            }
            Self::Coverage { records } => {
                let first = records.first().map(|record| &record.envelope);
                for (index, record) in records.iter().enumerate() {
                    record
                        .check()
                        .map_err(|err| problem(index, err.to_string()))?;
                    same_run(first, &record.envelope, index)?;
                }
            }
        }
        Ok(())
    }
}

/// One record as a `Value`. Serializing these cannot fail: the only fallible part of
/// writing a record is a non-finite number, and [`MetricValue`] cannot hold one.
fn value_of<T: serde::Serialize>(record: &T) -> Result<Value, RecordError> {
    serde_json::to_value(record).map_err(|err| RecordError::Shape {
        field: "record",
        value: err.to_string(),
        reason: "a record failed to serialize".to_owned(),
    })
}

/// ADR-0002 §6.1: one file carries exactly one run, so `run_id`, `commit`, `workflow`
/// and `task` are identical on every line.
fn same_run(
    first: Option<&Envelope>,
    envelope: &Envelope,
    index: usize,
) -> Result<(), RecordError> {
    let Some(first) = first else { return Ok(()) };
    for (field, a, b) in [
        ("run_id", first.run_id.as_str(), envelope.run_id.as_str()),
        ("commit", first.commit.as_str(), envelope.commit.as_str()),
        (
            "workflow",
            first.workflow.as_str(),
            envelope.workflow.as_str(),
        ),
        ("task", first.task.as_str(), envelope.task.as_str()),
    ] {
        if a != b {
            return Err(RecordError::Shape {
                field,
                value: b.to_owned(),
                reason: format!(
                    "one file carries exactly one run, but line {index} says {field} {b:?} while earlier lines say {a:?} (ADR-0002 §6.1)"
                ),
            });
        }
    }
    Ok(())
}

// PART-F
