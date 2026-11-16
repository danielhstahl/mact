//! The analyzer registry — the one code spelling of "which analyzers exist, at which version, on
//! which input, reading which config keys" (ADR-0006 §6.2, §7.3).
//!
//! The ADR calls `meta.json`'s `analyzers` array "a *recording* of the in-code registry", which
//! makes this module the thing it records. Two rules from §7.3 are load-bearing here and are asserted
//! by `crates/fathom/tests/store_keys.rs`:
//!
//! * **A key list is what the code reads.** The MI entries take their versions, their input kind and
//!   their declared keys from `schema/mi-definition.json` and [`crate::mi`], so a formula bump that
//!   forgets to bump the version shows up as a golden-fixture diff (ADR-0007 §9) *and* as a digest
//!   move here, rather than as a silently-stale block.
//! * **A value found under a key that omits one of its inputs is a bug of the worst kind** — a wrong
//!   number that never changes. So `mi.score`'s key list is the union of the tokenizer's and the
//!   scorer's: the cached value is one file's `FileMetrics`, which is produced by both and therefore
//!   depends on both. `dep_digest` says which analyzers those were.

use serde::{Deserialize, Serialize};

use crate::classify;
use crate::config::Config;
use crate::mi::{self, Definition};

use super::keys::{self, InputKind};

/// One row of the registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Analyzer {
    /// The analyzer's id, as `meta.json` and every block spell it.
    pub id: String,
    /// The integer bumped in code (§6.3's rule), never derived from the product version.
    pub version: u32,
    /// Which of §6.3's six input kinds keys this analyzer's blocks.
    ///
    /// Owned rather than `&'static str` because for the `mi.*` rows it comes from
    /// `schema/mi-definition.json`, which is data at runtime and not a literal (§6.3).
    pub input_kind: String,
    /// The declared config keys (§7.2's `consumed by` column), sorted.
    pub config_keys: Vec<String>,
    /// Transitive analyzer dependencies, as `id@version` (§6.3's `dep_digest` input).
    pub depends: Vec<String>,
}

impl Analyzer {
    /// This analyzer's `dep_digest` (§6.3).
    #[must_use]
    pub fn dep_digest(&self) -> String {
        let deps: Vec<(&str, u32)> = self
            .depends
            .iter()
            .map(|spelled| {
                let (id, version) = spelled.split_once('@').unwrap_or((spelled.as_str(), "0"));
                (id, version.parse().unwrap_or(0))
            })
            .collect();
        keys::dep_digest(&deps)
    }
}

/// The whole registry, in `meta.json`'s order.
///
/// `taxonomy.path` produces no block of its own — the classifier's answer is a handful of glob
/// matches, and caching it would cost a file read to save an `fnmatch` — but it is *recorded*,
/// because it is a declared consumer of four config keys and §7.3's test is about the declaration
/// being honest, not about whether the value is worth caching.
#[must_use]
pub fn registry(definition: &Definition) -> Vec<Analyzer> {
    let stamp = definition.stamp();
    vec![
        Analyzer {
            id: "taxonomy.path".to_owned(),
            version: 1,
            input_kind: InputKind::PATH.as_str().to_owned(),
            config_keys: sorted(&classify::DECLARED_CONFIG_KEYS),
            depends: vec![],
        },
        Analyzer {
            id: stamp.tokenize.clone(),
            version: stamp.version,
            input_kind: stamp.input_kind.clone(),
            config_keys: sorted(&mi::TOKENIZE_CONFIG_KEYS),
            depends: vec![],
        },
        Analyzer {
            // The per-blob block: one file's `FileMetrics`, which the tokenizer produced and the
            // scorer scored, so its key covers both lists and its dependency digest names the
            // tokenizer. `input_kind` is `blob_path` because the path picks the language (ADR-0007
            // P13) — keying it on content alone would be the rule-R1 bug in one filename.
            id: stamp.score.clone(),
            version: stamp.version,
            input_kind: stamp.input_kind.clone(),
            config_keys: sorted(&mi::SCORE_CONFIG_KEYS),
            depends: vec![format!("{}@{}", stamp.tokenize, stamp.version)],
        },
        Analyzer {
            // Not an analyzer that produces a block: the ADR-0008 reader. It is in the registry for
            // the same reason `taxonomy.path` is — it declares two config keys (`benchmarks.allow`,
            // `benchmarks.deny`) whose change invalidates every row's `evidence` section and no block,
            // which is §7.3's second table row. Recording it is what makes §7.2's `consumed by` column
            // the whole answer rather than a list with a hole where the evidence reader should be.
            id: "evidence.read".to_owned(),
            version: 1,
            input_kind: InputKind::EVIDENCE.as_str().to_owned(),
            config_keys: sorted(&Section::Evidence.config_keys()),
            depends: vec![],
        },
        Analyzer {
            // The per-tree aggregate of ADR-0007 §4.2, keyed on the tree oid (§6.3's `tree` input:
            // two commits that share a tree have one MI). Its version follows `mi.score`, whose
            // `bump_rules` in `schema/mi-definition.json` name "aggregation arithmetic" as one of
            // the things that move it — so there is no second version to forget to bump.
            id: "mi.aggregate".to_owned(),
            version: stamp.version,
            input_kind: InputKind::TREE.as_str().to_owned(),
            config_keys: file_analyzer_keys()
                .iter()
                .map(|key| (*key).to_owned())
                .collect(),
            depends: vec![format!("{}@{}", stamp.score, stamp.version)],
        },
    ]
}

/// Look one up by id.
#[must_use]
pub fn find(definition: &Definition, id: &str) -> Option<Analyzer> {
    registry(definition)
        .into_iter()
        .find(|analyzer| analyzer.id == id)
}

/// The keys the per-file value depends on: the tokenizer's and the scorer's, unioned and sorted.
///
/// Unioned rather than one of the two because the value is one `FileMetrics`, whose ladder runs both:
/// `index.file_size_cap_mib` decides `oversized`, `paths.noise` decides `skipped_noise`, and
/// `languages.enabled` decides `unsupported_language` — three of the seven rungs, in two different
/// analyzers' key lists.
#[must_use]
pub fn file_analyzer_keys() -> Vec<&'static str> {
    let mut keys: Vec<&'static str> = mi::TOKENIZE_CONFIG_KEYS.to_vec();
    keys.extend_from_slice(&mi::SCORE_CONFIG_KEYS);
    keys.sort_unstable();
    keys.dedup();
    keys
}

/// Every dotted config key this build knows, with its resolved value (§7.2's table, as code).
///
/// This is the single place a digest reads a config value from, which is what makes "every key is
/// read through the parsed struct" (§7.1) checkable: a key in `fathom.toml` that is not in this list
/// is an unknown key (a parse error), and a key in this list that no analyzer declares is `doctor`'s
/// warning.
#[must_use]
pub fn resolved_config(config: &Config) -> std::collections::BTreeMap<String, serde_json::Value> {
    use serde_json::Value;
    let strings = |values: &[String]| {
        Value::Array(
            values
                .iter()
                .cloned()
                .map(Value::String)
                .collect::<Vec<_>>(),
        )
    };
    let mut out = std::collections::BTreeMap::new();
    let mut insert = |key: &'static str, value: Value| {
        out.insert(key.to_owned(), value);
    };
    insert("history.depth", Value::from(config.history.depth));
    insert("history.refs", strings(&config.history.refs));
    insert(
        "history.first_parent",
        Value::from(config.history.first_parent),
    );
    insert("paths.source", strings(&config.paths.source));
    insert("paths.noise", strings(&config.paths.noise));
    insert("paths.exclude", strings(&config.paths.exclude));
    insert(
        "paths.include_tests",
        Value::from(config.paths.include_tests),
    );
    insert("languages.enabled", strings(&config.languages.enabled));
    insert(
        "index.file_size_cap_mib",
        Value::from(config.index.file_size_cap_mib),
    );
    insert("benchmarks.allow", strings(&config.benchmarks.allow));
    insert("benchmarks.deny", strings(&config.benchmarks.deny));
    insert("cache.block_cap", Value::from(config.cache.block_cap));
    insert(
        "cache.prune_on_index",
        Value::from(config.cache.prune_on_index),
    );
    out
}

/// The four derivation sections of a row's `sources.config_digest` (§6.4, §7.3).
///
/// Four, not one, so §7.3 can recompute exactly one section of a row and reuse the rest — the whole
/// reason a noise-glob edit recomputes diffstats without re-analysing a blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    /// The commit's own history line, and which parent a delta is measured against.
    History,
    /// The `+`/`−` and the per-file change list.
    Diffstat,
    /// Coverage, benchmarks and action success, i.e. everything read from `.evidence`.
    Evidence,
    /// The MI section.
    Mi,
}

impl Section {
    /// The stored key.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::History => "history",
            Self::Diffstat => "diffstat",
            Self::Evidence => "evidence",
            Self::Mi => "mi",
        }
    }

    /// Every section, in the order `meta.json` and the status lines print them.
    pub const ALL: [Self; 4] = [Self::History, Self::Diffstat, Self::Evidence, Self::Mi];

    /// Which bit of [`super::row::Freshness`] this section owns.
    ///
    /// Spelled here rather than as a `position()` over [`Section::ALL`] so the bitmap's layout is a
    /// `const`-evaluable fact and cannot drift from the section list: adding a section means adding an
    /// arm here, in `ALL`, and in `config_keys`, and the compiler stops each of those separately.
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::History => 0,
            Self::Diffstat => 1,
            Self::Evidence => 2,
            Self::Mi => 3,
        }
    }

    /// The config keys that section's digest covers (§7.2's `consumed by` column, read as code).
    #[must_use]
    pub fn config_keys(self) -> Vec<&'static str> {
        match self {
            // The window is not a key (`history.depth`, `history.refs`); which parent a delta is
            // taken against is (§7.2's last bullet, and §7.4's reason `--first-parent` costs a
            // re-index).
            Self::History => vec!["history.first_parent"],
            Self::Diffstat => classify::DECLARED_CONFIG_KEYS.to_vec(),
            Self::Evidence => vec!["benchmarks.allow", "benchmarks.deny"],
            Self::Mi => file_analyzer_keys(),
        }
    }
}

/// The digest guarding one section, from the same rule as a block's (§6.3, §7.3).
#[must_use]
pub fn section_digest(
    section: Section,
    values: &std::collections::BTreeMap<String, serde_json::Value>,
) -> String {
    let declared = section.config_keys();
    keys::config_digest(&declared, values)
}

/// The declared keys of every analyzer in the registry, as a flat sorted set (§7.3's test input).
#[must_use]
pub fn declared_keys(definition: &Definition) -> Vec<String> {
    let mut out: Vec<String> = registry(definition)
        .iter()
        .flat_map(|analyzer| analyzer.config_keys.clone())
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

fn sorted(keys: &[&'static str]) -> Vec<String> {
    let mut out: Vec<&str> = keys.to_vec();
    out.sort_unstable();
    out.iter().map(|key| (*key).to_owned()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_registry_key_is_a_key_this_build_can_resolve() {
        let definition = Definition::embedded();
        let resolved = resolved_config(&Config::default());
        for analyzer in registry(&definition) {
            for key in &analyzer.config_keys {
                assert!(
                    resolved.contains_key(key.as_str()),
                    "{} declares {key}, which fathom's config has no resolved value for",
                    analyzer.id
                );
            }
        }
    }

    /// §7.2's two columns reconciled: every key a *section* digest covers is either one an analyzer
    /// declares (§7.2's `consumed by` column, which is the tested rule) or one of the walk's own keys,
    /// which no analyzer reads because no analyzer runs before the walk — `history.depth` and
    /// `history.first_parent` decide *which commits exist in the store*, and §7.3's table says so
    /// ("edit `history.depth` → nothing cached; the walk set and prune's input change").
    #[test]
    fn section_keys_are_declared_keys_or_walk_keys() {
        let definition = Definition::embedded();
        let registry = declared_keys(&definition);
        let walk_keys = ["history.depth", "history.refs", "history.first_parent"];
        for section in Section::ALL {
            for key in section.config_keys() {
                assert!(
                    registry.iter().any(|declared| declared == key) || walk_keys.contains(&key),
                    "section {} names {key}, which no analyzer declares and no walk flag owns",
                    section.as_str()
                );
            }
        }
    }

    #[test]
    fn an_analyzer_version_is_the_definition_not_a_wild_guess() {
        let definition = Definition::embedded();
        let stamp = definition.stamp();
        let score = find(&definition, &stamp.score).expect("mi.score is registered");
        assert_eq!(score.version, stamp.version);
        assert_eq!(score.input_kind, stamp.input_kind);
        let aggregate = find(&definition, "mi.aggregate").expect("mi.aggregate is registered");
        assert_eq!(aggregate.depends, vec![format!("{}@1", stamp.score)]);
        assert_ne!(score.dep_digest(), aggregate.dep_digest());
    }
}
