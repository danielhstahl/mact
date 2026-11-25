//! `schema/mi-definition.json`, loaded rather than restated.
//!
//! ADR-0007 makes that file the one normative spelling of every MI constant: the formula's four
//! coefficients, the rounding and log rules, the measurability thresholds, the seven reasons and
//! their order, the band edges, the aggregation knobs, the extension map, and — the part that makes
//! a second language a table rather than a rewrite — the closed per-language enumerations of
//! operator keywords, operand keywords, punctuation, decision points and statement rules.
//! [`scripts/mi_reference.py`](../../../../scripts/mi_reference.py) loads the same file and is the
//! oracle this port is graded against, so the two implementations agree about the numbers for the
//! same reason the two evidence readers do: neither of them re-types them.
//!
//! Only the fields this analyzer executes are deserialized; the rest of the document is prose for
//! humans and is ignored, which keeps an additive model edit from breaking a build. The one
//! exception is `Definition::raw`, kept as a parsed document so a dotted reference inside the
//! model (`aggregation.sub_threshold_set.below` → `bands.names_by_threshold.red_below`) resolves
//! through the same path the oracle walks instead of being a second copy of the edge.

use std::{collections::BTreeMap, path::Path};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The embedded definition: `schema/mi-definition.json`, the same bytes the python oracle loads
/// and the ADR checker diffs the ADR's fenced blocks against. One copy on disk, four readers of it
/// (ADR-0005 §9 — `schema/` stays at the root for exactly this reason).
pub const MI_DEFINITION_JSON: &str = include_str!("../../../../schema/mi-definition.json");

/// What could go wrong loading the definition.
#[allow(
    missing_docs,
    reason = "the #[error] format string documents each variant and its fields"
)]
#[derive(Debug, Error)]
pub enum DefinitionError {
    #[error("cannot read {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },

    #[error("{path} is not a readable MI definition: {source}")]
    Parse {
        path: String,
        source: serde_json::Error,
    },

    #[error("{path} names a rule this build cannot execute: {reason}")]
    Unsupported { path: String, reason: String },
}

/// One analyzer's identity: `id`, `version`, and the input kind it is keyed by (ADR-0006 §6.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalyzerId {
    /// e.g. `mi.tokenize`.
    pub id: String,
    /// The version that invalidates when the analyzer's rules change (ADR-0007 §8).
    pub version: u32,
    /// `blob_path` for both MI analyzers: the path picks the language, so `blob` alone would be a
    /// lie (ADR-0007 P13).
    pub input_kind: String,
}

/// `analyzer.*` — the two MI analyzers and the front end they share.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Analyzers {
    /// `lexer1`: the hand-written pure-Rust front end this crate implements (ADR-0007 §6).
    pub frontend_id: String,
    /// The tokeniser analyzer: per blob, per path.
    pub tokenize: AnalyzerId,
    /// The scoring analyzer: the arithmetic and the aggregation.
    pub score: AnalyzerId,
}

/// `formula.*` — the four constants of the 1996 MI and its scaling.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Formula {
    /// 171.0
    pub a: f64,
    /// 5.2, the weight of `ln(V)`.
    pub b_volume: f64,
    /// 0.23, the weight of `G`.
    pub c_cyclomatic: f64,
    /// 16.2, the weight of `ln(LOC)`.
    pub d_loc: f64,
    /// 100.0
    pub scale_numerator: f64,
    /// 171.0
    pub scale_denominator: f64,
    /// 0.0
    pub clamp_min: f64,
    /// 100.0, defensive: the reachable range is `(0, 99.9]` (ADR-0007 §2.1).
    pub clamp_max: f64,
}

/// `precision.*` — what "the same number in two languages" means.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Precision {
    /// 1: the stored and compared value carries one decimal.
    pub storage_decimals: u32,
    /// `boundary_slack`: how close a golden vector may sit to a rounding or band edge before the
    /// fixture is refused for depending on the host's math library.
    pub boundary_slack: f64,
    /// `operators`: Halstead volume is measured over operator tokens only.
    pub volume_basis: String,
}

impl Precision {
    /// Whether Halstead volume is taken over operands instead of operators. The alternative is
    /// documented in ADR-0007 §3.1 and refused there; the key exists so the choice is visible in the
    /// model rather than folklore, and this analyzer honours it if anyone ever flips it.
    #[must_use]
    pub fn volume_is_operand_basis(&self) -> bool {
        self.volume_basis == "operands"
    }
}

/// `measurability.*` — the bottom of the ladder, and the ladder's order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Measurability {
    /// 1: `LOC == 0` has no MI, because `ln(0)` is not a number.
    pub min_loc: u64,
    /// 2: `log2(1)` is 0 and `log2(0)` is undefined, so a vocabulary below 2 has no MI either.
    pub min_operator_unique: u64,
    /// The seven reasons, in evaluation order, first one wins.
    pub ordered_reasons: Vec<String>,
}

/// One row of `status_reasons[]`: a reason, and whether the view must announce it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusReason {
    /// The reason id, which is also its spelling in a stored row.
    pub id: String,
    /// Whether this reason counts toward `unanalysed_paths` — the four that do are exactly the
    /// paths the view must announce (ADR-0007 §3.6).
    #[serde(default)]
    pub in_caveat_count: bool,
}

/// `bands.names_by_threshold` — the two edges, with the better band owning an exact edge.
///
/// Flattened out of `names_by_threshold` because a checker that wanted the edges under a different
/// name would have to be told twice: the JSON spells them as a map keyed by role, and this is the
/// only place that mapping is written.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(from = "BandsDoc")]
pub struct Bands {
    /// 50.0, the green edge: below this, amber. Coleman's "easy to maintain" line (`MI_raw > 85`) on
    /// this document's scale, rounded to the round number ADR-0007 had already used.
    pub amber_below: f64,
    /// 38.0, the red edge: below this, red. Coleman's "difficult to maintain" line (`MI_raw < 65`)
    /// converted by this definition's own 100/171 factor (ADR-0007 §5.1).
    pub red_below: f64,
}

/// The JSON shape `Bands` is flattened from.
#[derive(Debug, Deserialize)]
struct BandsDoc {
    names_by_threshold: BandsThresholds,
}

/// `bands.names_by_threshold` as written.
#[derive(Debug, Deserialize)]
struct BandsThresholds {
    amber_below: f64,
    red_below: f64,
}

impl From<BandsDoc> for Bands {
    fn from(doc: BandsDoc) -> Self {
        Self {
            amber_below: doc.names_by_threshold.amber_below,
            red_below: doc.names_by_threshold.red_below,
        }
    }
}

/// `aggregation.*` — what a commit's number is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Aggregation {
    /// `loc`: the weight every file's MI is multiplied by in the mean.
    pub weight: String,
    /// Worst-N hotspots the row carries.
    pub hotspots_default: usize,
    /// How a sub-threshold set is defined: a dotted reference into this document, resolved by
    /// [`Definition::resolve`] so the edge is spelled once.
    pub sub_threshold_set: SubThresholdSet,
}

/// `aggregation.sub_threshold_set` — the watchlist, named by reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubThresholdSet {
    /// `watchlist`.
    pub name: String,
    /// `bands.names_by_threshold.red_below`, resolved at use.
    pub below: String,
}

/// `scope.*` — which paths are source at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeModel {
    /// `.rs → rust`, `.py → python`. An extension not on this list is not source and is *not*
    /// counted as unanalysed (ADR-0007 §6.5).
    pub source_extensions: BTreeMap<String, String>,
}

/// One language's closed enumerations: the data half of a front end (ADR-0007 §6.1).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Language {
    /// Display name.
    pub display: String,
    /// Extensions this front end reads, e.g. `[".rs"]`.
    pub extensions: Vec<String>,
    /// Whether v1 ships this front end; `languages.enabled` decides at run time.
    #[serde(default)]
    pub enabled_in_v1: bool,
    /// Reserved words that operate. `try` is one in both languages, whatever a lexer that never
    /// needed the word might guess.
    pub operator_keywords: Vec<String>,
    /// Reserved words that name a value, so they are operands.
    pub operand_keywords: Vec<String>,
    /// Punctuation that is an operator, matched by maximal munch.
    pub operator_punctuation: Vec<String>,
    /// Punctuation that emits nothing: brackets, braces, `,`, `;`, `:`, `::`, `->`, `=>`.
    pub ignored_punctuation: Vec<String>,
    /// Words that are keywords only in a position Python can see and a token stream can see:
    /// `match` and `case`. Everything else that looks contextual is an operand by rule.
    #[serde(default)]
    pub soft_keywords: Vec<String>,
    /// `decision_points`: `G = 1 + Σ weights`.
    pub decision_points: DecisionPoints,
    /// `statements`: the per-language LOC rule, kept as text because the lexer implements it.
    pub statements: StatementRules,
}

/// `languages.<lang>.decision_points`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct DecisionPoints {
    /// The base of `G`: 1, over the whole file (ADR-0007 §3.3).
    pub base: u64,
    /// The closed enumeration of decision-point rules.
    pub rules: Vec<DecisionRule>,
}

/// One decision-point rule.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct DecisionRule {
    /// The id a count is keyed by, and the id a golden vector names.
    pub id: String,
    /// The token text it fires on, absent for a structural rule.
    #[serde(default)]
    pub token: Option<String>,
    /// How a structural rule is decided, in prose: `match_arm` and `if_comprehension` are the two.
    #[serde(default)]
    pub structural: Option<String>,
    /// What one occurrence adds to `G`.
    pub weight: i64,
    /// `value_preceded` for `&&`/`||`/`and`/`or`; a sentence for Rust's `let … else`.
    #[serde(default)]
    pub condition: Option<String>,
}

/// `languages.<lang>.statements` — the LOC rule.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct StatementRules {
    /// `LOC = D + E + A + B` or `LOC = L + H`.
    pub formula: String,
}

/// `scoring_unit.*` — what the formula is actually applied to (ADR-0007 §4.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScoringUnit {
    /// `function`: the unit is one function-like definition. The only unit this build implements;
    /// anything else is refused at load rather than silently ignored.
    pub unit: String,
    /// `loc`: how each block's MI is weighted into its file's mean. Also the only weight implemented.
    pub weight: String,
    /// `file`: what a blob with no measurable block is scored as — itself, as one unit.
    pub fallback: String,
    /// Which header token introduces a block in each language, for the ADR's `kinds` table and for a
    /// test that wants to notice a language whose splitter does not exist.
    pub kinds: BTreeMap<String, Vec<String>>,
}

/// The parts of `schema/mi-definition.json` this analyzer executes.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Definition {
    /// Which revision of the definition this is.
    pub definition_version: u32,
    /// The two analyzer identities and the front end they share.
    pub analyzer: Analyzers,
    /// The formula's constants.
    pub formula: Formula,
    /// What a MI is computed over: a function, and how the blocks fold back into a file (§4.1).
    pub scoring_unit: ScoringUnit,
    /// Rounding, clamping and the log rules.
    pub precision: Precision,
    /// The bottom of the ladder and its order.
    pub measurability: Measurability,
    /// The closed reason vocabulary, with the caveat-count flags.
    pub status_reasons: Vec<StatusReason>,
    /// The two band edges.
    pub bands: Bands,
    /// The aggregation rules.
    pub aggregation: Aggregation,
    /// Which extensions are source at all.
    pub scope: ScopeModel,
    /// The per-language tables, keyed by language name.
    pub languages: BTreeMap<String, Language>,
    /// The parsed document, kept so a dotted reference inside the model resolves the way the
    /// oracle resolves it (see [`Definition::resolve`]).
    #[serde(skip)]
    raw: serde_json::Value,
}

impl Definition {
    /// The shipped definition, `schema/mi-definition.json` as embedded at build time.
    ///
    /// # Panics
    ///
    /// If the shipped definition does not parse, which is a build-time fact about this repository
    /// and is caught by `scripts/validate_mi_definition.py` long before a run.
    #[must_use]
    pub fn embedded() -> Self {
        Self::from_json(MI_DEFINITION_JSON)
            .expect("schema/mi-definition.json is a valid MI definition")
    }

    /// Parse a definition from JSON text.
    ///
    /// # Errors
    ///
    /// [`DefinitionError::Parse`] if the text is not JSON or is missing a section this analyzer
    /// needs, or [`DefinitionError::Unsupported`] if it names a scoring unit or weight this build
    /// does not implement.
    pub fn from_json(json: &str) -> Result<Self, DefinitionError> {
        let raw: serde_json::Value =
            serde_json::from_str(json).map_err(|source| DefinitionError::Parse {
                path: "<json>".to_owned(),
                source,
            })?;
        Self::from_value(raw)
    }
    /// Read and parse a definition from disk, for a test or a checker that names a file.
    ///
    /// # Errors
    ///
    /// [`DefinitionError::Io`] or [`DefinitionError::Parse`].
    pub fn load(path: &Path) -> Result<Self, DefinitionError> {
        let text = std::fs::read_to_string(path).map_err(|source| DefinitionError::Io {
            path: path.display().to_string(),
            source,
        })?;
        let raw: serde_json::Value =
            serde_json::from_str(&text).map_err(|source| DefinitionError::Parse {
                path: path.display().to_string(),
                source,
            })?;
        Self::from_value(raw)
    }

    fn from_value(raw: serde_json::Value) -> Result<Self, DefinitionError> {
        let mut definition: Self =
            serde_json::from_value(raw.clone()).map_err(|source| DefinitionError::Parse {
                path: "<json>".to_owned(),
                source,
            })?;
        // A definition naming a scoring unit or a weight this build does not implement is refused
        // here rather than measured some other way in silence: the JSON is the normative text, and a
        // build that quietly fell back to "whole file" would be reporting a different metric under
        // the same analyzer id.
        if definition.scoring_unit.unit != "function" {
            return Err(DefinitionError::Unsupported {
                path: "scoring_unit.unit".to_owned(),
                reason: format!(
                    "the scoring unit {:?} is not implemented; this build scores `function` blocks",
                    definition.scoring_unit.unit
                ),
            });
        }
        if definition.scoring_unit.weight != "loc" {
            return Err(DefinitionError::Unsupported {
                path: "scoring_unit.weight".to_owned(),
                reason: format!(
                    "the weight {:?} is not implemented; blocks are weighted by `loc`",
                    definition.scoring_unit.weight
                ),
            });
        }
        definition.raw = raw;
        Ok(definition)
    }

    /// Resolve a dotted reference inside the definition, e.g.
    /// `bands.names_by_threshold.red_below`. This is the oracle's `_resolve`, in Rust: the watchlist
    /// edge is a *reference* to the band edge so that the two can never be spelled twice.
    #[must_use]
    pub fn resolve(&self, dotted: &str) -> Option<f64> {
        let mut node = &self.raw;
        for part in dotted.split('.') {
            node = node.get(part)?;
        }
        node.as_f64()
    }

    /// The threshold the watchlist is defined by — `aggregation.sub_threshold_set.below`, resolved.
    ///
    /// # Panics
    ///
    /// If the shipped definition's watchlist reference does not resolve, which is a fact about the
    /// shipped JSON and not a runtime condition.
    #[must_use]
    pub fn sub_threshold_below(&self) -> f64 {
        self.resolve(&self.aggregation.sub_threshold_set.below)
            .expect("aggregation.sub_threshold_set.below names a band edge that exists")
    }

    /// The weight one scoring unit carries in its file's mean (ADR-0007 §4.1).
    ///
    /// Read from `scoring_unit.weight` rather than assumed by the caller, so that a definition cannot
    /// change what a file's MI is weighted by without saying so in the normative file — and cannot do
    /// it silently, because the loader (`Definition::from_value`) refuses any weight it cannot compute.
    #[must_use]
    pub fn unit_weight(&self, loc: u64) -> f64 {
        match self.scoring_unit.weight.as_str() {
            "loc" => loc as f64,
            // Unreachable for a definition that loaded; the arm is what keeps the load-time check
            // load-bearing.
            _ => 0.0,
        }
    }

    /// Whether a reason counts toward `unanalysed_paths`.
    #[must_use]
    pub fn counts_as_unanalysed(&self, reason: &str) -> bool {
        self.status_reasons
            .iter()
            .any(|row| row.id == reason && row.in_caveat_count)
    }

    /// The language a repo-relative path belongs to, by the ADR's extension rule: longest suffix,
    /// case-sensitive, applied to the final segment (ADR-0007 §7.1).
    #[must_use]
    pub fn language_for_path(&self, path: &str) -> Option<&str> {
        let segment = path.rsplit('/').next().unwrap_or(path);
        let mut best: Option<(usize, &str)> = None;
        for (extension, language) in &self.scope.source_extensions {
            if segment.len() > extension.len()
                && segment.ends_with(extension.as_str())
                && best.is_none_or(|(len, _)| extension.len() > len)
            {
                best = Some((extension.len(), language.as_str()));
            }
        }
        best.map(|(_, language)| language)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_definition_loads() {
        let definition = Definition::embedded();
        assert_eq!(definition.definition_version, 2);
        assert_eq!(definition.analyzer.tokenize.id, "mi.tokenize");
        assert_eq!(definition.analyzer.tokenize.input_kind, "blob_path");
        assert_eq!(definition.formula.a, 171.0);
        assert_eq!(definition.bands.amber_below, 50.0);
        assert_eq!(definition.bands.red_below, 38.0);
        assert_eq!(definition.scoring_unit.unit, "function");
        assert_eq!(definition.scoring_unit.weight, "loc");
        assert_eq!(definition.scoring_unit.fallback, "file");
        assert_eq!(definition.unit_weight(12), 12.0);
        assert_eq!(definition.aggregation.weight, "loc");
        assert_eq!(definition.aggregation.hotspots_default, 5);
    }

    #[test]
    fn the_watchlist_edge_is_a_reference_not_a_second_number() {
        let definition = Definition::embedded();
        assert_eq!(
            definition.aggregation.sub_threshold_set.below,
            "bands.names_by_threshold.red_below"
        );
        assert_eq!(definition.sub_threshold_below(), definition.bands.red_below);
    }

    #[test]
    fn the_extension_map_routes_the_v1_languages() {
        let definition = Definition::embedded();
        assert_eq!(definition.language_for_path("src/main.rs"), Some("rust"));
        assert_eq!(
            definition.language_for_path("scripts/ci.py"),
            Some("python")
        );
        assert_eq!(definition.language_for_path("README.md"), None);
        assert_eq!(
            definition.language_for_path("rs"),
            None,
            "an extension needs a name"
        );
        assert_eq!(definition.language_for_path("src/.rs"), None);
        assert_eq!(
            definition.language_for_path("crates/fathom/src/mi/lang/rust.rs"),
            Some("rust")
        );
    }

    #[test]
    fn both_v1_languages_carry_a_table() {
        let definition = Definition::embedded();
        for name in ["rust", "python"] {
            let table = definition
                .languages
                .get(name)
                .unwrap_or_else(|| panic!("{name} has no table"));
            assert!(table.enabled_in_v1, "{name} is not enabled in v1");
            assert!(!table.decision_points.rules.is_empty());
            assert_eq!(table.decision_points.base, 1);
        }
        // `try` is a keyword in both languages: it opens a block in Rust and a block in Python, and
        // a table that forgot it would make every `try:` in the corpus an operand.
        assert!(definition.languages["python"]
            .operator_keywords
            .contains(&"try".to_owned()));
        assert!(definition.languages["rust"]
            .operator_keywords
            .contains(&"try".to_owned()));
    }

    #[test]
    fn exactly_the_four_gap_reasons_are_announced() {
        let definition = Definition::embedded();
        let announced: Vec<&str> = definition
            .status_reasons
            .iter()
            .filter(|row| row.in_caveat_count)
            .map(|row| row.id.as_str())
            .collect();
        assert_eq!(
            announced,
            vec![
                "unsupported_language",
                "oversized",
                "parse_error",
                "vacuous"
            ]
        );
        assert!(!definition.counts_as_unanalysed("skipped_noise"));
        assert!(definition.counts_as_unanalysed("vacuous"));
    }
}
