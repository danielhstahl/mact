//! The row (§6.4) — one commit's metrics, and the freshness rule that makes an index incremental.
//!
//! Row identity is `commit SHA → { history+diffstat, evidence metrics, MI aggregate, indexed_utc,
//! source digests }`. The two halves of that sentence have different lifetimes, which is the whole
//! design:
//!
//! * **git history is immutable.** A commit's tree, parents and author instant never change, so a
//!   `history`/`diffstat`/`mi` section derived from one is reusable until the *config* or an analyzer
//!   version moves.
//! * **evidence is not.** `.evidence` is append-only and older SHAs gain records after the fact, so
//!   the `evidence` section is guarded by a digest over the identity of the files that fed it
//!   ([`crate::store::ledger`]). A re-run of act-mcp on an old commit changes that digest and
//!   re-derives exactly that one row.
//!
//! So freshness is per **section**, not per row (§7.3): `sources.config_digest` carries one entry per
//! derivation, a stored section whose digest still matches is copied forward untouched, and a row is
//! re-published only if some section actually moved. That is why a warm re-index with no changes
//! writes zero rows and why editing a noise glob re-derives diffstats without touching the MI.
//!
//! The shape rules are ADR-0006 §6.4's: unknown is absent and never zero (ADR-0002 §2.1), paths are
//! repo-relative, no file contents, no absolute host paths, no secrets.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::history::Commit;
use crate::metrics::vocab::ActionState;
use crate::metrics::CommitMetrics;
use crate::mi::Status;

use super::analyzer::Section;
use super::keys;
use super::keys::is_commit_id;

/// The row shape version, carried per row so §8's "one stale row, not a whole store" rule works.
pub const ROW_VERSION: u32 = 1;

/// What a row was derived from (§6.4's `sources`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sources {
    /// The git identity: three facts that, together with the commit id, pin everything git can say.
    pub git: GitIdentity,
    /// `fk1` over the sorted `path\0sha256` list of the evidence files that fed this row (§6.2).
    pub evidence_digest: String,
    /// The digest of the analyzer dependency graph this row's blocks were keyed with.
    pub dep_digest: String,
    /// One digest per derivation, so one section can be re-derived and the rest reused (§7.3).
    pub config_digest: BTreeMap<String, String>,
}

/// The git half of [`Sources`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GitIdentity {
    /// The commit's tree id (`git log %T`).
    pub tree: String,
    /// The parents, first-parent first.
    pub parents: Vec<String>,
    /// The author instant, Unix seconds.
    pub author_utc: i64,
}

impl GitIdentity {
    /// The identity of one walked commit.
    #[must_use]
    pub fn of(commit: &Commit) -> Self {
        Self {
            tree: commit.tree.clone(),
            parents: commit.parents.clone(),
            author_utc: commit.author_utc,
        }
    }
}

/// The row's `mi` section: ADR-0007 §4's aggregate, plus the two things that make its numbers
/// re-derivable without re-analysing a byte.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MiSection {
    /// The per-file blocks this row was built from, path → block key (§6.4's `mi.blocks`).
    ///
    /// Prune's referenced-block set is read out of here, and a checker can re-derive the aggregate
    /// from these keys alone — which is what makes a stored aggregate auditable instead of folklore.
    pub blocks: BTreeMap<String, String>,
    /// The tree whose file set this is (§6.3's `tree` input kind: two commits, one tree, one MI).
    pub tree_block: String,
    /// The analyzer versions that produced it (§8's row-level staleness).
    pub analyzers: BTreeMap<String, u32>,
    /// The tree-level aggregate's own numbers (ADR-0007 §4.2).
    #[serde(flatten)]
    pub aggregate: serde_json::Value,
}

/// One commit's row: `index/rows/<xx>/<commit>.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Row {
    /// This row's shape version (§8: a row this build does not know is one stale row).
    pub row_version: u32,
    /// The 40-hex commit id, which must equal the filename (§10.5's damage check).
    pub commit: String,
    /// When this row was last derived. Informational: no key contains a clock (§6.3).
    pub indexed_utc: i64,
    /// See [`Sources`].
    pub sources: Sources,
    /// The history + diffstat section.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history: Option<serde_json::Value>,
    /// The coverage/benchmark/action section, i.e. everything read from `.evidence`. Absent means
    /// this commit has no evidence, and `gaps` says which gap that is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<serde_json::Value>,
    /// See [`MiSection`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mi: Option<MiSection>,
    /// Why a column will render `—`, in one place, so the TUI, `report` and CI read one rule (§6.4).
    pub gaps: Gaps,
}

/// The `gaps` half of a row: which column is empty, and why.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Gaps {
    /// No evidence at all was readable for this commit.
    pub no_evidence: bool,
    /// The evidence that exists could not be read (a different answer, ADR-0008 §5.4).
    pub unreadable_evidence: bool,
    /// No MI number for this tree (a window root with no parent, or nothing measurable in scope).
    pub no_mi: bool,
    /// Files that were seen and not measured, whatever the rung (`mi.counts` has the split).
    pub unanalysed_paths: u64,
    /// The metric model's own gap codes for this commit, when there is a row to read them from.
    ///
    /// `default` because `skip_serializing_if` removes it when empty, and a shape that can be written
    /// without a field must be readable without one — otherwise the row a run published is a row the
    /// next run cannot read, which is a cache that only ever misses.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub codes: Vec<String>,
}

/// Which sections a stored row can be reused for, and which must be re-derived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Freshness {
    /// The bitmap of fresh sections, by [`Section::as_str`].
    bits: u8,
}

impl Freshness {
    /// Everything is stale — a cold index, or a row that will not parse (§10.5: a damaged row is
    /// re-derived, not mourned).
    #[must_use]
    pub const fn none() -> Self {
        Self { bits: 0 }
    }

    /// Whether section `section` may be copied forward from the stored row.
    #[must_use]
    pub const fn is_fresh(&self, section: Section) -> bool {
        self.bits & (1 << section.index()) != 0
    }

    /// Record a section as reusable.
    fn mark(&mut self, section: Section) {
        self.bits |= 1 << section.index();
    }
}

/// Decide what can be reused, from the stored row and what this run resolved.
///
/// Four tests, one per section, and each one is a statement about *why* that section is still true:
///
/// * `history` — same tree, same parents, same author instant, and the same `history.first_parent`
///   digest (which parent a delta is taken against is a fact about the number, §7.4).
/// * `diffstat` — same tree, same `paths.*` digest: the classifier's answer is a function of the
///   commit's path list and nothing else.
/// * `mi` — same tree (blobs are content-addressed, so a tree's file set cannot change under it),
///   same MI config digest, same analyzer versions.
/// * `evidence` — the ledger digest of the files that fed the row is unchanged *and* the benchmark
///   filter digest is unchanged. This is the only input that can move under an old commit, and it is
///   the reason this section has its own digest at all.
#[must_use]
pub fn freshness(
    stored: Option<&Row>,
    commit: &str,
    current: &Sources,
    analyzers: &BTreeMap<String, u32>,
) -> Freshness {
    let Some(row) = stored else {
        return Freshness::none();
    };
    // §10.5: a row whose `commit` is not its filename is damaged and is re-derived; and a row of a
    // shape this build does not know is one stale row, never a whole store's problem (§8).
    if row.row_version != ROW_VERSION || row.commit != commit || !is_commit_id(&row.commit) {
        return Freshness::none();
    }
    let mut fresh = Freshness { bits: 0 };
    let same = |section: Section| {
        row.sources.config_digest.get(section.as_str())
            == current.config_digest.get(section.as_str())
    };

    if same(Section::History) && row.sources.git == current.git && row.history.is_some() {
        fresh.mark(Section::History);
    }
    if same(Section::Diffstat) && row.sources.git.tree == current.git.tree && row.history.is_some()
    {
        fresh.mark(Section::Diffstat);
    }
    // The evidence section is fresh only if something was stored *and* the files that fed it are the
    // files the ledger lists now. `evidence_digest` is empty when the commit has no evidence at all,
    // and "still nothing" is a fresh answer — it is `no_evidence`, and it will not change until a
    // file appears for this SHA.
    if same(Section::Evidence)
        && row.sources.evidence_digest == current.evidence_digest
        && (row.evidence.is_some() || current.evidence_digest.is_empty())
    {
        fresh.mark(Section::Evidence);
    }
    if same(Section::Mi)
        && row.sources.git.tree == current.git.tree
        && row.mi.as_ref().is_some_and(|mi| mi.analyzers == *analyzers)
    {
        fresh.mark(Section::Mi);
    }
    fresh
}

/// The evidence section's JSON: the metric model's row, as the reader produced it (ADR-0008's shape
/// is owned by ADR-0008, so it is stored as the reader serialises it rather than re-declared here).
#[must_use]
pub fn evidence_value(metrics: &CommitMetrics) -> serde_json::Value {
    serde_json::to_value(metrics).unwrap_or(serde_json::Value::Null)
}

/// The `gaps` a row carries, from a freshly read evidence row and the MI aggregate if there is one.
///
/// Delegates to [`gaps_from`] so there is exactly one implementation of "which column is empty and why"
/// — the typed entry point for a row this run just read, the JSON one for a row this run is reusing.
#[must_use]
pub fn gaps_of(metrics: Option<&CommitMetrics>, aggregate: Option<&serde_json::Value>) -> Gaps {
    let stored: Option<serde_json::Value> =
        metrics.map_or_else(|| None, |row| serde_json::to_value(row).ok());
    gaps_from(&stored, aggregate)
}

/// The same decision, from a row's stored `evidence` section instead of a freshly read one.
///
/// A warm re-index reuses the `evidence` section and never calls the metric reader, so the gaps have to
/// be recomputable from what is already on disk — and they are, because the section *is* the serialised
/// `CommitMetrics`. Reading it as JSON rather than round-tripping the struct keeps `metrics`' row type
/// an output type (it has no `Deserialize`: it is derived, not parsed), and recomputing the gaps rather
/// than copying the stored ones is what lets a change to these rules apply to old rows on the next index
/// instead of being frozen into them (§10.5's "a stale row is re-derived, not mourned" applied to a
/// field).
///
/// The two predicates are the metric reader's own, spelled with the model's vocabulary and not with a
/// re-typed literal: `no_evidence` means "nothing ran", `unreadable_evidence` means "something ran and I
/// could not read it" (ADR-0008 §5.4), and the two are different things for a user to do something
/// about.
#[must_use]
pub fn gaps_from(
    evidence: &Option<serde_json::Value>,
    aggregate: Option<&serde_json::Value>,
) -> Gaps {
    let mut gaps = Gaps::default();
    match evidence {
        None => gaps.no_evidence = true,
        Some(value) => {
            gaps.no_evidence = value
                .get("action_success")
                .and_then(serde_json::Value::as_str)
                == Some(ActionState::NoEvidence.as_str());
            gaps.unreadable_evidence = value
                .get("unreadable_records")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0)
                > 0;
            let mut codes: Vec<String> = Vec::new();
            for column in ["coverage", "benchmarks"] {
                if let Some(gap) = value
                    .get("gaps")
                    .and_then(|gaps| gaps.get(column))
                    .and_then(serde_json::Value::as_str)
                {
                    codes.push(format!("{column}:{gap}"));
                }
            }
            codes.sort();
            gaps.codes = codes;
        }
    }
    gaps.no_mi = aggregate.is_none();
    gaps.unanalysed_paths = aggregate
        .and_then(|value| value.get("unanalysed_paths"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    gaps
}

/// The status names a row's `mi.counts` keys are spelled with — re-exported so a test can name a
/// rung without reaching into `mi` (and so this module's `Status` import is load-bearing).
pub const STATUS_NAMES: [&str; 7] = [
    Status::Excluded.as_str(),
    Status::SkippedNoise.as_str(),
    Status::UnsupportedLanguage.as_str(),
    Status::Oversized.as_str(),
    Status::ParseError.as_str(),
    Status::Vacuous.as_str(),
    Status::Ok.as_str(),
];

/// The section digests, in the order `meta.json` and a row print them (§6.4).
#[must_use]
pub fn digest_map(digests: &[(Section, String)]) -> BTreeMap<String, String> {
    digests
        .iter()
        .map(|(section, digest)| (section.as_str().to_owned(), digest.clone()))
        .collect()
}

/// The keys this store's block filenames and row ids are made of (§6.3, §6.4).
pub use keys::is_key;

#[cfg(test)]
mod tests {
    use super::*;

    fn sources() -> Sources {
        Sources {
            git: GitIdentity {
                tree: "b".repeat(40),
                parents: vec!["a".repeat(40)],
                author_utc: 1_760_000_000,
            },
            evidence_digest: String::new(),
            dep_digest: keys::fk1_str("deps"),
            config_digest: Section::ALL
                .iter()
                .map(|section| {
                    (
                        section.as_str().to_owned(),
                        format!("fk1:{}", section.as_str()),
                    )
                })
                .collect(),
        }
    }

    fn row() -> Row {
        Row {
            row_version: ROW_VERSION,
            commit: COMMIT.parse().expect("a 40-hex fixture id"),
            indexed_utc: 1,
            sources: sources(),
            history: Some(serde_json::json!({ "subject": "x" })),
            evidence: None,
            mi: Some(MiSection {
                blocks: BTreeMap::from([("src/a.rs".to_owned(), keys::fk1_str("a"))]),
                tree_block: keys::fk1_str("tree"),
                analyzers: BTreeMap::from([("mi.score".to_owned(), 1u32)]),
                aggregate: serde_json::json!({ "aggregate": 71.4 }),
            }),
            gaps: Gaps::default(),
        }
    }

    const COMMIT: &str = "cccccccccccccccccccccccccccccccccccccccc";

    fn analyzers() -> BTreeMap<String, u32> {
        BTreeMap::from([("mi.score".to_owned(), 1u32)])
    }

    /// Which of the four sections `freshness` would reuse, as names — so a failure prints the set that
    /// moved rather than a bit in a bitmap.
    fn fresh_sections(
        stored: Option<&Row>,
        current: &Sources,
        versions: &BTreeMap<String, u32>,
    ) -> Vec<String> {
        let fresh = freshness(stored, COMMIT, current, versions);
        Section::ALL
            .iter()
            .filter(|section| fresh.is_fresh(**section))
            .map(|section| (*section).as_str().to_owned())
            .collect()
    }

    #[test]
    fn everything_is_stale_without_a_row_or_for_a_shape_this_build_does_not_know() {
        assert!(fresh_sections(None, &sources(), &analyzers()).is_empty());
        let mut newer = row();
        newer.row_version = ROW_VERSION + 1;
        assert!(
            fresh_sections(Some(&newer), &sources(), &analyzers()).is_empty(),
            "§8: one stale row, re-derived; the store is fine"
        );
        let mut renamed = row();
        renamed.commit = "d".repeat(40);
        assert!(
            fresh_sections(Some(&renamed), &sources(), &analyzers()).is_empty(),
            "§10.5: a row whose commit is not its filename is damaged"
        );
    }

    #[test]
    fn a_row_that_matches_in_every_input_is_reused_whole() {
        let stored = row();
        // The fixture stores no `evidence` section and has no evidence either (`evidence_digest`
        // empty), which is the "still nothing measured" case: fresh, because it cannot change until a
        // file appears for this SHA.
        assert_eq!(
            fresh_sections(Some(&stored), &sources(), &analyzers()),
            vec!["history", "diffstat", "evidence", "mi"]
        );
    }

    #[test]
    fn each_section_digest_moves_exactly_its_own_section() {
        for moved in Section::ALL {
            let mut current = sources();
            current
                .config_digest
                .insert(moved.as_str().to_owned(), "fk1:moved".to_owned());
            let expected: Vec<String> = Section::ALL
                .iter()
                .filter(|section| **section != moved)
                .map(|section| (*section).as_str().to_owned())
                .collect();
            assert_eq!(
                fresh_sections(Some(&row()), &current, &analyzers()),
                expected,
                "moving the {moved:?} digest must re-derive that section and reuse the other three"
            );
        }
    }

    #[test]
    fn new_evidence_for_an_old_commit_stales_only_the_evidence_section() {
        let mut current = sources();
        // The ledger found a new file for this commit: its digest moved and nothing else did. This is
        // the ticket's "the one genuinely subtle part" — git is immutable, evidence is not.
        current.evidence_digest = keys::fk1_str("a-newer-run");
        assert_eq!(
            fresh_sections(Some(&row()), &current, &analyzers()),
            vec!["history", "diffstat", "mi"],
            "one row's one section re-derives; the MI and the diffstat are untouched"
        );
    }

    #[test]
    fn a_new_tree_stales_everything_git_derived() {
        let mut current = sources();
        current.git.tree = "d".repeat(40);
        assert_eq!(
            fresh_sections(Some(&row()), &current, &analyzers()),
            vec!["evidence"],
            "blobs, tree and diffstat all moved; the evidence section did not"
        );
    }

    #[test]
    fn an_analyzer_version_bump_stales_the_mi_section_alone() {
        assert_eq!(
            fresh_sections(
                Some(&row()),
                &sources(),
                &BTreeMap::from([("mi.score".to_owned(), 2u32)])
            ),
            vec!["history", "diffstat", "evidence"],
            "§7.3: bumping an analyzer kills its blocks and sections, not the history"
        );
    }

    #[test]
    fn a_missing_history_section_is_never_reused() {
        let mut thin = row();
        thin.history = None;
        let fresh = fresh_sections(Some(&thin), &sources(), &analyzers());
        assert!(!fresh.contains(&"history".to_owned()));
        assert!(!fresh.contains(&"diffstat".to_owned()));
    }

    #[test]
    fn a_row_round_trips_through_the_canonical_spelling() {
        let value = serde_json::to_value(row().sources).expect("serialisable");
        let back: Sources = serde_json::from_str(&value.to_string()).expect("deserialisable");
        assert_eq!(back, row().sources);
    }

    #[test]
    fn an_absent_evidence_section_is_a_gap_and_not_a_zero() {
        let gaps = gaps_of(None, None);
        assert!(gaps.no_evidence);
        assert!(gaps.no_mi);
        assert_eq!(gaps.codes, Vec::<String>::new());
    }

    #[test]
    fn status_names_are_the_ladder_in_order() {
        assert_eq!(STATUS_NAMES[0], "excluded");
        assert_eq!(STATUS_NAMES[6], "ok");
        assert_eq!(STATUS_NAMES.len(), 7, "§by1.7's seven rungs, spelled once");
    }
}
