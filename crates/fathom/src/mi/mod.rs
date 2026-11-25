//! The Maintainability Index analyzer: ADR-0007 executed.
//!
//! This is the module that turns "complexity (Maintainability Index)" from a column header into a
//! number with a definition behind it: per-file MI from the tree at a commit, and a per-commit
//! aggregate a timeline can plot. Everything about its shape follows from the ADR being the decision
//! and this being the implementation, so the modules split the way the ADR argues:
//!
//! * [`definition`] — `schema/mi-definition.json` loaded, not restated: the formula's constants, the
//!   rounding rules, the band edges, the ladder thresholds, and the closed per-language enumerations
//!   the lexers read their rules from.
//! * `lang` — the front end: one hand-written pure-Rust tokeniser per language over one shared
//!   token model, which is ADR-0007 §6's answer to tree-sitter (it is C; this repository has refused
//!   a C build dependency three times for five cross-compiled release targets).
//! * [`score`] — the arithmetic, operation for operation with `scripts/mi_reference.py`, the oracle
//!   this port is graded against: `V`, `G`, `MI_raw`, the clamp, the stored rounding, the band.
//! * [`mod@aggregate`] — the commit's number: the LOC-weighted mean, the delta against parent #1, the
//!   watchlist transitions, the worst-N hotspots, and the count of paths that were not analysed.
//!
//! ## The two rules that decide most of the rest
//!
//! **A file with no MI is not a file with MI 0.** [`Status`] carries seven rungs — `excluded`,
//! `skipped_noise`, `unsupported_language`, `oversized`, `parse_error`, `vacuous`, `ok` — evaluated in
//! that order, and only the last one has a number. A comment-only file is `vacuous` because `ln(0)`
//! is undefined, and a broken file is `parse_error` because a repaired parse silently drops whichever
//! statements the repair swallowed; either way the field is *absent* in the serialisation (ADR-0002
//! §2.1: unknown is absent, never zero) and the reason is present, so an empty-looking column can be
//! explained rather than guessed at.
//!
//! **Analysis is of content, never of the working tree.** [`Analyzer::analyse`] takes
//! `(path, bytes)`: the caller hands over the blob, the path picks the language and nothing else.
//! That is what lets a cache key a result on the blob's SHA (the next ticket) and what keeps a row
//! describing a commit instead of describing whatever happens to be checked out — the classic way
//! this feature lies.
//!
//! ## Determinism
//!
//! Every ordering this module produces is pinned: token tables and decision counts are `BTreeMap`s,
//! the aggregate accumulates in path-bytes order, hotspots break ties on LOC then path, and the
//! reason counts are keyed by a closed vocabulary. Two runs over the same tree produce byte-identical
//! JSON, which is what makes a golden vector possible and a re-index checkable.
//!
//! ## How this is graded
//!
//! `crates/fathom/tests/mi.rs` re-derives every golden vector under
//! `crates/act-mcp/tests/testdata/mi/` at the *token-table* level — occurrence tables, decision
//! points, LOC — and then the arithmetic, so a tokeniser that reaches the right MI by the wrong route
//! is caught; the same fixtures are checked by `scripts/mi_reference.py` and by the 40-digit second
//! opinion in `scripts/validate_mi_definition.py`. That grading found three hand-count errors in the
//! vectors and one missing case, all documented in the vectors themselves and in ADR-0007 §9: the
//! front end agreed with the ADR's rules in each case, so the fixtures moved and the rules did not.

pub mod aggregate;
pub mod definition;
mod lang;
pub mod score;

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::classify::Classifier;
use crate::config::Config;

pub use aggregate::{aggregate, band_tally, commit_row, CommitAggregate, Hotspot};
use definition::Language;
pub use definition::{AnalyzerId, Definition, DefinitionError, ScoringUnit};
pub use lang::{tokenise, Block, Counts, LexError};
pub use score::{band_of, measure, round_storage, Band, Measurement, Status};

/// The config keys `mi.tokenize` declares and reads (ADR-0007 §8's table). A value in a CI job's
/// environment that never reaches the digest of the run that used it is a cache that lies, which is
/// why the key lists are code and not prose.
pub const TOKENIZE_CONFIG_KEYS: [&str; 2] = ["languages.enabled", "index.file_size_cap_mib"];

/// The config keys `mi.score` declares and reads: scope, because which files are in the mean is a
/// fact about the number, not about the blob.
pub const SCORE_CONFIG_KEYS: [&str; 4] = [
    "languages.enabled",
    "paths.noise",
    "paths.exclude",
    "paths.include_tests",
];

/// Why the analyzer refused to run at all — as opposed to a file's own reason, which is a [`Status`]
/// on a [`FileMetrics`] and never an error.
#[allow(
    missing_docs,
    reason = "the #[error] format string documents each variant and its fields"
)]
#[derive(Debug, thiserror::Error)]
pub enum MiError {
    #[error(transparent)]
    Definition(#[from] definition::DefinitionError),

    #[error(transparent)]
    Glob(#[from] crate::classify::GlobError),
}

/// The analyzer identity a result carries: which front end, which versions, which input kind
/// (ADR-0007 P13).
///
/// The two analyzers are named separately because they fail and change separately — tokenising is
/// the expensive per-blob half, scoring is arithmetic — and a cache key has to be able to say which
/// one moved. The `fk1:` digest of the config pre-image
/// ([`Analyzer::config_preimage`]) is computed by the store, which is where `sha2` is allowed to
/// live (ADR-0006 §6.3, and the ticket that owns it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AnalyzerStamp {
    /// `lexer1`.
    pub frontend: String,
    /// `mi.tokenize`.
    pub tokenize: String,
    /// `mi.score`.
    pub score: String,
    /// The version both are at, spelled as the row spells it: `1`.
    pub version: u32,
    /// `blob_path`: the path picks the language, so `blob` alone would be a lie.
    pub input_kind: String,
}

impl Definition {
    /// The analyzer identity a result from this definition carries.
    #[must_use]
    pub fn stamp(&self) -> AnalyzerStamp {
        AnalyzerStamp {
            frontend: self.analyzer.frontend_id.clone(),
            tokenize: self.analyzer.tokenize.id.clone(),
            score: self.analyzer.score.id.clone(),
            version: self.analyzer.score.version,
            input_kind: self.analyzer.tokenize.input_kind.clone(),
        }
    }
}

/// One scoring unit of a file, measured: ADR-0007 §4.1 made visible.
///
/// A block is what a MI is a number *about*, and this is the list a reader would write down if asked
/// "which functions, and what does each score?" — which is why `Analyzer::blocks_of` is public and why a
/// file's `mi` is required to be the mean of exactly these numbers. A block that did not measure carries
/// `status` and no `mi`, and is absent from its file's mean rather than present in it as a zero.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BlockMeasurement {
    /// The defined name — `run`, `impl Block for X::len`, the `def` of a Python method.
    pub name: String,
    /// The rung of the measurability ladder this block landed on.
    pub status: Status,
    /// Logical lines of the block, which is the weight it carries in its file's mean.
    pub loc: Option<u64>,
    /// Halstead volume of the block alone.
    pub halstead_volume: Option<f64>,
    /// Cyclomatic complexity of the block alone.
    pub cyclomatic: Option<u64>,
    /// The block's own MI, which is what the file's mean is made of.
    pub mi: Option<f64>,
    /// The band of `mi`.
    pub band: Option<Band>,
}

/// The analyzer's answer for one blob, before it is dressed as a [`FileMetrics`].
///
/// `file` is always the whole-file measurement — the counts, `V`, `G` and `LOC` a reader inspects with
/// `cat`, and the number the fallback scores with — while `mi` is what the unit rule produced.
struct Scored {
    /// `ok`, or the rung of the ladder the file landed on.
    status: Status,
    /// The file's MI: the mean of its blocks, or the file's own when there are none.
    mi: Option<f64>,
    /// The pre-rounding double, only when one exists (a mean of blocks has none).
    mi_raw: Option<f64>,
    /// Which rule produced `mi`.
    unit: Unit,
    /// How many blocks fed the mean; absent when the unit is the file.
    blocks: Option<u64>,
    /// The whole-file measurement.
    file: Measurement,
}

/// What produced a file's MI (ADR-0007 §4.1).
///
/// `Block` is the normal answer: the file's MI is the LOC-weighted mean of the MI of each of its
/// function definitions. `File` is the documented fallback for a source file that declares no
/// function at all — a module of types and constants, a Python script that is all top-level
/// statements — where the only unit available is the whole file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Unit {
    /// One MI per function-like definition, averaged into the file's MI.
    Block,
    /// The file as a single scoring unit.
    File,
}

impl Unit {
    /// The spelling a cached block and a `cat` of it use.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Block => "block",
            Self::File => "file",
        }
    }
}

impl std::fmt::Display for Unit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One file's metrics: the ticket's `FileMetrics`, the reason when there is no MI, and — since
/// mact-by1.10 — which rule produced the number.
///
/// Every measurement field is an `Option` and every absent one is *skipped* on serialisation, which
/// is the whole "a file with no MI is not a file with MI 0" rule expressed in the type: there is no
/// default to forget to set, and a consumer that prints `mi` cannot print a zero for a file that was
/// never measured.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FileMetrics {
    /// The repo-relative path the blob was read from.
    pub path: String,
    /// The language the path selected, absent when it selected none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Which rung of the ladder this file landed on. `ok` is the only one with numbers.
    pub status: Status,
    /// The front end's own words for a refusal, when there was one: `parse_error` is a decision with
    /// a reason in it, and the reason belongs next to the gap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Logical lines (`D + E + A + B`, or `L + H`). Present whenever the file lexed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loc: Option<u64>,
    /// Halstead length `N`: operator occurrences.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operators: Option<u64>,
    /// Halstead vocabulary `n`: distinct operator texts. `n < 2` is a `vacuous` file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vocabulary: Option<u64>,
    /// Halstead volume `V = N * log2(n)`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub halstead_volume: Option<f64>,
    /// `G = 1 + decision points`, over the whole file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cyclomatic: Option<u64>,
    /// The decision points behind `G`, keyed by the model's rule ids.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub decision_points: BTreeMap<String, u64>,
    /// The unrounded formula output: a diagnostic, never a comparison (P2). Absent when the file's MI
    /// is a mean of blocks — a mean has no single pre-rounding double to show, and inventing one would
    /// be a number that cannot be re-derived from anything.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mi_raw: Option<f64>,
    /// What produced `mi`: the file's function blocks, or the file itself. Absent on a gap, and on a
    /// number rebuilt from a stored row, which records a file's MI and not how it was arrived at.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<Unit>,
    /// How many function blocks produced the mean. Absent for a `file` unit and for a gap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocks: Option<u64>,
    /// The stored MI, one decimal, half away from zero. Absent means *unknown*.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mi: Option<f64>,
    /// The band of `mi`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub band: Option<Band>,
    /// Which analyzer said this, so a cached block can be explained by `cat`.
    pub analyzer: AnalyzerStamp,
}

impl FileMetrics {
    /// Rebuild the file set a stored row describes, without re-lexing a byte.
    ///
    /// The aggregate and the delta are pure functions of `(path, status, loc, mi)` — which is all a
    /// cached row keeps — so a checker can re-derive a commit's number from the cache alone, and a
    /// test can grade a fixture the same way the python oracle does. The band is derived from the
    /// stored MI here rather than trusted, because a stored band is a second copy of an edge the ADR
    /// refuses to store (`bands.stored_in_cache: false`).
    #[must_use]
    pub fn from_stored(
        path: &str,
        status: Status,
        loc: Option<u64>,
        mi: Option<f64>,
        definition: &Definition,
    ) -> Self {
        Self {
            path: path.to_owned(),
            language: None,
            status,
            detail: None,
            loc,
            operators: None,
            vocabulary: None,
            halstead_volume: None,
            cyclomatic: None,
            decision_points: BTreeMap::new(),
            mi_raw: None,
            unit: None,
            blocks: None,
            mi,
            band: mi.map(|value| band_of(definition, value)),
            analyzer: definition.stamp(),
        }
    }
}

/// A compiled front end and the functions that run it.
///
/// This table *is* the set of languages this build can lex, which is the shorter list than the
/// model's on purpose: a language in `schema/mi-definition.json` without an entry here is
/// `unsupported_language` and a counted gap, never a zero (ADR-0007 §6.5).
struct Frontend {
    front: lang::Front,
    run: fn(&str, &lang::Front) -> Result<Counts, LexError>,
    /// The scoring units of a blob: its function definitions (ADR-0007 §4.1).
    blocks: fn(&str, &lang::Front) -> Vec<lang::Block>,
}

/// The one place that knows which front ends exist.
fn frontend_for(language: &str, table: &Language) -> Option<Frontend> {
    let run: fn(&str, &lang::Front) -> Result<Counts, LexError> = match language {
        "rust" => lang::rust::tokenise,
        "python" => lang::python::tokenise,
        _ => return None,
    };
    let blocks: fn(&str, &lang::Front) -> Vec<lang::Block> = match language {
        "rust" => lang::rust::blocks,
        "python" => lang::python::blocks,
        _ => return None,
    };
    Some(Frontend {
        front: lang::Front::new(table),
        run,
        blocks,
    })
}

/// The analyzer: the definition, the shared path classifier, the enabled languages and the size cap.
///
/// Built once per run and reused per blob; the per-language lexers' tables are compiled at
/// construction, because building them is fifty string clones and a repository has thousands of
/// files.
pub struct Analyzer {
    definition: Definition,
    classifier: Classifier,
    enabled: BTreeSet<String>,
    file_size_cap: u64,
    frontends: BTreeMap<String, Frontend>,
}

impl Default for Analyzer {
    fn default() -> Self {
        Self::from_config(&Config::default()).expect("the default config compiles a classifier")
    }
}

impl Analyzer {
    /// An analyzer from the resolved `fathom.toml` and the shipped definition.
    ///
    /// # Errors
    ///
    /// [`MiError::Glob`] if a configured glob is uncompilable — the same refusal the shared
    /// classifier makes, because a noise list that silently stopped matching is a MI column that
    /// quietly started measuring a lockfile.
    pub fn from_config(config: &Config) -> Result<Self, MiError> {
        Ok(Self::new(
            Definition::embedded(),
            Classifier::try_from_config(config)?,
            config.languages.enabled.clone(),
            u64::from(config.index.file_size_cap_mib) * 1024 * 1024,
        ))
    }

    /// The pieces, spelled out, for a test or a checker that has no config file.
    #[must_use]
    pub fn new(
        definition: Definition,
        classifier: Classifier,
        enabled: impl IntoIterator<Item = impl Into<String>>,
        file_size_cap: u64,
    ) -> Self {
        let enabled: BTreeSet<String> = enabled.into_iter().map(Into::into).collect();
        let frontends = definition
            .languages
            .iter()
            .filter_map(|(name, table)| {
                frontend_for(name, table).map(|frontend| (name.clone(), frontend))
            })
            .collect();
        Self {
            definition,
            classifier,
            enabled,
            file_size_cap,
            frontends,
        }
    }

    /// The analyzer identity this run's results carry.
    #[must_use]
    pub fn stamp(&self) -> AnalyzerStamp {
        self.definition.stamp()
    }

    /// The definition this analyzer runs by.
    #[must_use]
    pub const fn definition(&self) -> &Definition {
        &self.definition
    }

    /// The shared classifier, exposed so `doctor` can announce the tests default (ADR-0007 §7.2).
    #[must_use]
    pub const fn classifier(&self) -> &Classifier {
        &self.classifier
    }

    /// The languages this analyzer will actually measure: `languages.enabled`, intersected with the
    /// languages this build has a front end for. A language in the model without a lexer here is
    /// `unsupported_language` rather than a silent zero, which is the honest failure (§6.5).
    #[must_use]
    pub fn enabled(&self) -> &BTreeSet<String> {
        &self.enabled
    }

    /// The canonical pre-image of an analyzer's `config_digest` — ADR-0006 §6.3's
    /// `config_digest(keys) = fk1("fc1\0" || canon({"<dotted.key>": resolved_value, …}))`, minus the
    /// hash, because `sha2` belongs to the store and not to a tokenizer.
    ///
    /// Keys are the analyzer's declared list in sorted order, so a key added to
    /// [`TOKENIZE_CONFIG_KEYS`] or [`SCORE_CONFIG_KEYS`] changes this string and therefore, once the
    /// store hashes it, every block built by the old key. That is the point: a formula tweak bumps
    /// `mi.score` and invalidates exactly the MI blocks and nothing else.
    #[must_use]
    pub fn config_preimage(&self, keys: &[&str]) -> String {
        let mut parts: Vec<String> = Vec::with_capacity(keys.len());
        for key in keys {
            let value = match *key {
                "languages.enabled" => {
                    let mut names: Vec<&String> = self.enabled.iter().collect();
                    names.sort();
                    format!(
                        "[{}]",
                        names
                            .iter()
                            .map(|name| name.as_str())
                            .collect::<Vec<_>>()
                            .join(",")
                    )
                }
                "index.file_size_cap_mib" => (self.file_size_cap / (1024 * 1024)).to_string(),
                other => other.to_owned(),
            };
            parts.push(format!("{key}={value}"));
        }
        format!("fc1\0{}", parts.join("\0"))
    }

    /// Analyse one blob: the ladder, top to bottom, first rung that matches wins.
    ///
    /// `path` is repo-relative with forward slashes and picks the language; `bytes` is the blob's
    /// content at one commit. Nothing here touches the filesystem: the working tree is not the
    /// commit's content, and confusing them is how a metric ends up describing the wrong thing.
    #[must_use]
    pub fn analyse(&self, path: &str, bytes: &[u8]) -> FileMetrics {
        // 1–2. the scope the diffstat uses too, in its precedence order.
        let scope = self.classifier.classify(path);
        if scope == crate::classify::Scope::Excluded {
            return self.gap(path, None, Status::Excluded, None);
        }
        if scope == crate::classify::Scope::Noise {
            return self.gap(path, None, Status::SkippedNoise, None);
        }
        // An extension that is not on the source list is not source at all, and is NOT counted as
        // unanalysed — otherwise every Markdown file in a repository is a permanent caveat (§6.5).
        let Some(language) = self.definition.language_for_path(path) else {
            return self.gap(path, None, Status::Excluded, None);
        };
        // 3. a source extension whose language has no enabled front end.
        if !self.enabled.contains(language) || !self.frontends.contains_key(language) {
            return self.gap(
                path,
                Some(language.to_owned()),
                Status::UnsupportedLanguage,
                None,
            );
        }
        // 4. too big to look at.
        if (bytes.len() as u64) > self.file_size_cap {
            return self.gap(path, Some(language.to_owned()), Status::Oversized, None);
        }
        // 5. binary or unterminated: a refusal, recorded as a refusal and cacheable as one.
        let text = match lexable(bytes) {
            Ok(text) => text,
            Err(detail) => {
                return self.gap(
                    path,
                    Some(language.to_owned()),
                    Status::ParseError,
                    Some(detail),
                );
            }
        };
        let frontend = &self.frontends[language];
        let counts = match (frontend.run)(text, &frontend.front) {
            Ok(counts) => counts,
            Err(error) => {
                return self.gap(
                    path,
                    Some(language.to_owned()),
                    Status::ParseError,
                    Some(error.to_string()),
                );
            }
        };
        // 6–7. `vacuous` or `ok`, over the scoring unit the definition names (ADR-0007 §4.1).
        let base = self
            .definition
            .languages
            .get(language)
            .map_or(1, |table| table.decision_points.base);
        let scored = self.score_units(frontend, text, counts.clone(), base);
        let file = &scored.file;
        FileMetrics {
            path: path.to_owned(),
            language: Some(language.to_owned()),
            status: scored.status,
            detail: None,
            loc: file.loc,
            operators: Some(counts.operator_total()),
            vocabulary: Some(counts.operator_unique()),
            halstead_volume: file.volume,
            cyclomatic: file.cyclomatic,
            decision_points: counts.decision_points.clone(),
            mi_raw: scored.mi_raw,
            unit: Some(scored.unit),
            blocks: scored.blocks,
            mi: scored.mi,
            band: scored.mi.map(|value| band_of(&self.definition, value)),
            analyzer: self.definition.stamp(),
        }
    }

    /// Score one blob over its **scoring units** (ADR-0007 §4.1).
    ///
    /// The unit is a function definition, not a file, because MI's constants are calibrated on
    /// functions: `-0.23·G` is linear in the decision count and `-16.2·ln(LOC)` is logarithmic, so
    /// over a whole file the two terms stop measuring the code and start measuring how big the file
    /// is — on this repository's own source, per-file scoring put every file over ~120 logical lines
    /// in the red band and 20 of them at exactly 0.0, while the same files scored per function sat at
    /// a mean of 75.8. See ADR-0007 §4.1 and §5.3.
    ///
    /// The file's MI is `MI_1( Σ mi_i·loc_i / Σ loc_i )` over the blocks that measured, the same
    /// LOC-weighted mean §4.3 pins one level up, so a 400-line function is not outvoted by twelve
    /// one-liners beside it. Blocks that do not measure (a one-line stub with no vocabulary, a slice
    /// the lexer would not accept) are excluded from the mean rather than scored as zero — the
    /// never-borrow-a-number rule, applied inside the file.
    ///
    /// A blob with no measurable block is scored as its own single unit (`Unit::File`), which is what
    /// keeps a module of types, or a Python script of top-level statements, a measurement instead of
    /// a gap.
    fn score_units(&self, frontend: &Frontend, text: &str, counts: Counts, base: u64) -> Scored {
        let file = measure(&self.definition, &counts, Status::Ok, base);
        let blocks = self.block_measurements(frontend, text, base);
        let mut weighted = 0_f64;
        let mut weight_total = 0_f64;
        let mut counted = 0_u64;
        for (_, measurement) in &blocks {
            let (Some(mi), Some(loc)) = (measurement.mi, measurement.loc) else {
                continue;
            };
            // The weight is the definition's, not this function's (ADR-0007 §4.1).
            let weight = self.definition.unit_weight(loc);
            if weight <= 0.0 {
                continue;
            }
            weighted += mi * weight;
            weight_total += weight;
            counted += 1;
        }
        if counted == 0 || weight_total <= 0.0 {
            return Scored {
                status: file.status,
                mi: file.mi,
                mi_raw: file.mi_raw,
                unit: Unit::File,
                blocks: None,
                file,
            };
        }
        Scored {
            status: Status::Ok,
            mi: Some(round_storage(&self.definition, weighted / weight_total)),
            // A mean has no single pre-rounding double that means anything; say so rather than
            // reconstruct one from the rounded parts.
            mi_raw: None,
            unit: Unit::Block,
            blocks: Some(counted),
            file,
        }
    }

    /// Measure each scoring unit of a blob (ADR-0007 §4.1). One code path serves both the file's mean
    /// and [`Analyzer::blocks_of`], so a block a reader inspects is provably the block that was
    /// averaged.
    fn block_measurements(
        &self,
        frontend: &Frontend,
        text: &str,
        base: u64,
    ) -> Vec<(String, Measurement)> {
        (frontend.blocks)(text, &frontend.front)
            .into_iter()
            .filter_map(|block| {
                let counts = (frontend.run)(&block.source, &frontend.front).ok()?;
                Some((
                    block.name,
                    measure(&self.definition, &counts, Status::Ok, base),
                ))
            })
            .collect()
    }

    /// The **scoring units** of a blob and each one's MI (ADR-0007 §4.1).
    ///
    /// A file's stored MI is the mean of these, so this is the only way to see *why* a file sits where
    /// it does: a file averaging 48 could be one 900-line function or thirty small ones. Exposed for
    /// the corpus test that derives the band edges from the block histogram, and for any view that
    /// wants the breakdown; the store keeps the file row and not these.
    ///
    /// `None` when the language has no front end in this build, or the blob did not lex.
    #[must_use]
    pub fn blocks_of(&self, path: &str, bytes: &[u8]) -> Option<Vec<BlockMeasurement>> {
        let language = self.definition.language_for_path(path)?;
        let frontend = self.frontends.get(language)?;
        let text = lexable(bytes).ok()?;
        (frontend.run)(text, &frontend.front).ok()?;
        let base = self
            .definition
            .languages
            .get(language)
            .map_or(1, |table| table.decision_points.base);
        Some(
            self.block_measurements(frontend, text, base)
                .into_iter()
                .map(|(name, measurement)| BlockMeasurement {
                    name,
                    status: measurement.status,
                    loc: measurement.loc,
                    halstead_volume: measurement.volume,
                    cyclomatic: measurement.cyclomatic,
                    mi: measurement.mi,
                    band: measurement.band,
                })
                .collect(),
        )
    }

    /// The front-end counts and the score for one blob, exposed for the golden-vector tests, which
    /// compare the *token tables* and not only the final MI. A test that checked only the MI could
    /// pass with a lexer that got the volume right by accident.
    ///
    /// `None` when the language has no front end in this build.
    #[must_use]
    pub fn measure_blob(&self, path: &str, bytes: &[u8]) -> Option<(Counts, Measurement)> {
        let language = self.definition.language_for_path(path)?;
        let frontend = self.frontends.get(language)?;
        let text = lexable(bytes).ok()?;
        let counts = (frontend.run)(text, &frontend.front).ok()?;
        let base = self
            .definition
            .languages
            .get(language)
            .map_or(1, |table| table.decision_points.base);
        let measurement = measure(&self.definition, &counts, Status::Ok, base);
        Some((counts, measurement))
    }

    fn gap(
        &self,
        path: &str,
        language: Option<String>,
        status: Status,
        detail: Option<String>,
    ) -> FileMetrics {
        FileMetrics {
            path: path.to_owned(),
            language,
            status,
            detail,
            loc: None,
            operators: None,
            vocabulary: None,
            halstead_volume: None,
            cyclomatic: None,
            decision_points: BTreeMap::new(),
            mi_raw: None,
            unit: None,
            blocks: None,
            mi: None,
            band: None,
            analyzer: self.definition.stamp(),
        }
    }
}

/// The binary rule and the UTF-8 rule, which are one function because they are one question: can
/// these bytes be read as text at all? A `NUL` before the first newline is git's own binary test
/// (ADR-0007 §7.1), and invalid UTF-8 has no tokenisation to speak of.
fn lexable(bytes: &[u8]) -> Result<&str, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "the blob is not valid UTF-8".to_owned())?;
    let head = text.split('\n').next().unwrap_or(text);
    if head.contains('\0') {
        return Err("the blob holds a NUL byte before the first newline".to_owned());
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn analyzer() -> Analyzer {
        Analyzer::default()
    }

    #[test]
    fn the_ladder_runs_in_the_adrs_order() {
        let a = analyzer();
        // excluded: not a source extension at all.
        assert_eq!(a.analyse("README.md", b"# hi").status, Status::Excluded);
        // excluded: under paths.exclude.
        assert_eq!(
            a.analyse(".evidence/runs/x.json", b"{}").status,
            Status::Excluded
        );
        // skipped_noise: a vendored file, and a test file while the default holds.
        assert_eq!(
            a.analyse("vendor/lib.rs", b"fn f() {}").status,
            Status::SkippedNoise
        );
        assert_eq!(
            a.analyse("tests/legacy.rs", b"fn f() {}").status,
            Status::SkippedNoise
        );
        // unsupported_language: a source extension with no front end would need the model to list
        // one; today every listed extension is lexed, so the rung is exercised in tests/mi.rs with a
        // definition that names a language this build cannot read.
        // oversized: the default cap is 2 MiB.
        let big = vec![b'a'; 2 * 1024 * 1024 + 1];
        assert_eq!(a.analyse("src/big.rs", &big).status, Status::Oversized);
        // parse_error: an unterminated literal.
        assert_eq!(
            a.analyse("src/broken.rs", b"fn f() {\n    \"open\n}\n")
                .status,
            Status::ParseError
        );
        // parse_error: a NUL byte before the first newline.
        assert_eq!(
            a.analyse("src/binary.rs", b"a\0b\nc").status,
            Status::ParseError
        );
        // vacuous: lexes cleanly, has no logarithm to take.
        assert_eq!(
            a.analyse("src/notes.rs", b"// nothing but prose\n").status,
            Status::Vacuous
        );
        // ok.
        assert_eq!(
            a.analyse(
                "src/t.rs",
                b"pub fn add(a: u8, b: u8) -> u8 {\n    a + b\n}\n"
            )
            .status,
            Status::Ok
        );
    }

    #[test]
    fn a_file_without_a_number_carries_no_number_field_at_all() {
        let a = analyzer();
        let metrics = a.analyse("src/notes.rs", b"// nothing but prose\n");
        let json = serde_json::to_value(&metrics).expect("serialises");
        assert_eq!(json["status"], "vacuous");
        assert!(
            json.get("mi").is_none(),
            "unknown must be absent, never 0: {json}"
        );
        assert!(json.get("halstead_volume").is_none());
        assert_eq!(
            json["loc"],
            serde_json::json!(0),
            "a vacuous file still has a LOC of 0"
        );
        let ok = a.analyse(
            "src/t.rs",
            b"pub fn add(a: u8, b: u8) -> u8 {\n    a + b\n}\n",
        );
        let ok_json = serde_json::to_value(&ok).expect("serialises");
        assert_eq!(ok_json["mi"], 95.1);
        assert_eq!(ok_json["band"], "green");
        assert_eq!(ok_json["analyzer"]["frontend"], "lexer1");
        assert_eq!(ok_json["analyzer"]["tokenize"], "mi.tokenize");
    }

    #[test]
    fn the_vacuous_rung_has_two_ways_in() {
        let a = analyzer();
        assert_eq!(
            a.analyse("src/empty.rs", b"// prose\n").status,
            Status::Vacuous,
            "LOC == 0"
        );
        assert_eq!(
            a.analyse("src/doc.py", b"\"\"\"only a docstring\"\"\"\n")
                .status,
            Status::Vacuous,
            "vocabulary n == 0, where ln(LOC) would have been fine"
        );
    }

    #[test]
    fn a_disabled_language_is_unsupported_and_announced() {
        let config = Config::default();
        let mut analyzer = Analyzer::from_config(&config).expect("build");
        analyzer.frontends = BTreeMap::new();
        let metrics = analyzer.analyse("src/t.rs", b"fn f() {}\n");
        assert_eq!(metrics.status, Status::UnsupportedLanguage);
        assert_eq!(metrics.language.as_deref(), Some("rust"));
    }

    #[test]
    fn the_config_preimage_names_the_keys_it_resolves() {
        let a = analyzer();
        let tokenize = a.config_preimage(&TOKENIZE_CONFIG_KEYS);
        assert!(tokenize.starts_with("fc1\0"), "{tokenize}");
        assert!(
            tokenize.contains("languages.enabled=[python,rust]"),
            "{tokenize}"
        );
        assert!(tokenize.contains("index.file_size_cap_mib=2"), "{tokenize}");
    }

    #[test]
    fn two_runs_of_one_blob_are_byte_identical() {
        let a = analyzer();
        let source = b"pub fn f(v: &[u8]) -> u32 {\n    let mut t = 0;\n    for b in v { t += *b as u32; }\n    t\n}\n";
        let first = serde_json::to_vec(&a.analyse("src/a.rs", source)).expect("serialises");
        let second = serde_json::to_vec(&a.analyse("src/a.rs", source)).expect("serialises");
        assert_eq!(
            first, second,
            "same bytes, same tree, byte-identical output"
        );
    }
}
