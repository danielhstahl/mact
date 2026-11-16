//! The per-commit aggregate: ADR-0007 §4, as a pure function of one commit's file set.
//!
//! A commit's MI is an aggregate and not a measurement, and the module keeps that distinction in the
//! types: [`aggregate`] takes the files a tree contained and returns a number that is a
//! **LOC-weighted** mean over the files that produced a measurement, plus everything a reader needs
//! in order to distrust it appropriately — the file and LOC totals it is a mean over, the counts per
//! reason, the number of paths that were not analysed, the worst-N hotspots, and the watchlist.
//!
//! Two rules do most of the work and both are here to obey a specific failure:
//!
//! * **the weight is LOC** (P8), because a per-file mean lets a 12-line file outvote a 900-line one.
//!   `aggregates/weighted_versus_per_file_mean.json` is the ticket's own case: `(95*12 + 61*900)/912
//!   = 61.4`, where the per-file mean would have reported 78.0.
//! * **accumulation order is pinned** to path bytes ascending, and the aggregate is a pure function
//!   of the commit's own file set (P8, `recompute_rule`). The carry-forward recurrence — parent's
//!   sums, minus the removed, plus the added — is cheaper and is refused: float addition is only
//!   deterministic given an order, so a recurrence would make the stored digit depend on the order
//!   commits were indexed in, and `fathom doctor` could never re-derive a row to check it. Indexes
//!   are allowed to be slow and reproducible; they are not allowed to be either.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use super::definition::Definition;
use super::score::{band_of, round_storage, Band, Status};
use super::FileMetrics;

/// One worst-list entry: the file, its MI, the LOC that weighted it, and what it moved by.
///
/// `delta` is additive to ADR-0007 §4.4's field list (§6.4 permits additive fields) and it is there
/// because the ticket asks hotspots for `(path, mi, loc, delta)`: a hotspot you cannot tell a
/// regression from a long-standing file is a hotspot you have to go and `git log` by hand.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Hotspot {
    /// Repo-relative path.
    pub path: String,
    /// The stored MI.
    pub mi: f64,
    /// The LOC weight it carried.
    pub loc: u64,
    /// `MI_1(mi − parent's mi for this path)`, absent when the path is new or the parent had no
    /// number for it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta: Option<f64>,
}

/// One commit's MI section: everything ADR-0007 §4.4 says the row carries, and nothing the view
/// would have to invent.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CommitAggregate {
    /// The LOC-weighted mean, at the stored rounding. Absent when no file in the tree produced a
    /// measurement — which is *unknown*, never 0 (§4.3's `empty_population`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aggregate: Option<f64>,
    /// Why the aggregate is absent, in words, when it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aggregate_basis: Option<String>,
    /// The band of the aggregate. Absent with it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub band: Option<Band>,
    /// Σ LOC over the files that produced a measurement — the weight the mean divided by.
    pub loc: u64,
    /// How many files that was. A mean over six files and a mean over six thousand must not look the
    /// same, which is why this is reported beside the number (§4.3).
    pub files: u64,
    /// Every reason, with how many files landed there.
    pub counts: BTreeMap<String, u64>,
    /// The paths not analysed: the reasons the model flags as caveats, summed. A view MUST render
    /// the caveat line when this is non-zero (§4.4).
    pub unanalysed_paths: u64,
    /// Worst N by MI ascending, LOC descending, path ascending.
    pub hotspots: Vec<Hotspot>,
    /// The paths below the amber edge, so a view can grey them without re-deriving a band.
    pub amber_paths: Vec<String>,
    /// The watchlist: paths below the red edge.
    pub sub_threshold_set: Vec<String>,
    /// `delta_vs_parent`: the difference of the two *rounded* aggregates, so a reader can recompute
    /// it from two stored rows. Absent for a root commit or a parent with no aggregate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta_vs_parent: Option<f64>,
    /// Watchlist membership gained against parent #1.
    pub sub_threshold_added: Vec<String>,
    /// Watchlist membership lost against parent #1.
    pub sub_threshold_removed: Vec<String>,
}

impl CommitAggregate {
    /// Whether this commit has an MI at all — the difference between `—` and a number.
    #[must_use]
    pub const fn has_mi(&self) -> bool {
        self.aggregate.is_some()
    }
}

/// Files per band, over the files that produced a measurement.
///
/// ADR-0007 §5's `revisit_when` asks the first full-corpus run to publish its histogram next to the
/// band edges, and a view that needs a distribution must not invent one: this is the count the
/// corpus run publishes, computed from the declared edges and nowhere else.
#[must_use]
pub fn band_tally(files: &[FileMetrics]) -> BTreeMap<&'static str, u64> {
    let mut tally: BTreeMap<&'static str, u64> = BTreeMap::new();
    for file in files {
        if let Some(band) = file.band {
            *tally.entry(band.as_str()).or_insert(0) += 1;
        }
    }
    tally
}

/// Sort order the aggregate accumulates in: path bytes ascending (P8). Rust compares `str` byte-wise,
/// so sorting on the `&str` *is* the pinned order, and the sort is total because two files cannot
/// share a path in one tree.
fn path_order(a: &&FileMetrics, b: &&FileMetrics) -> std::cmp::Ordering {
    a.path.as_bytes().cmp(b.path.as_bytes())
}

/// The LOC-weighted mean and its company, over one commit's analysed file set.
///
/// The population is every file in scope in that commit's tree — not only the files the commit
/// touched — because a sub-threshold set that files join and leave only exists in a whole-tree
/// population, and a touched-files-only number would make a one-file commit incomparable with a
/// fifty-file one (ADR-0007 §4.2).
#[must_use]
pub fn aggregate(definition: &Definition, files: &[FileMetrics]) -> CommitAggregate {
    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();
    for file in files {
        *counts.entry(file.status.as_str()).or_insert(0) += 1;
    }
    let unanalysed_paths: u64 = counts
        .iter()
        .filter(|(status, _)| definition.counts_as_unanalysed(status))
        .map(|(_, n)| *n)
        .sum();

    let mut usable: Vec<&FileMetrics> = files
        .iter()
        .filter(|file| file.status == Status::Ok && file.mi.is_some())
        .collect();
    usable.sort_by(path_order);

    let mut total_loc = 0u64;
    let mut weighted = 0f64;
    for file in &usable {
        let loc = file.loc.unwrap_or(0);
        total_loc += loc;
        weighted += file.mi.unwrap_or(0.0) * (loc as f64);
    }

    let below_watch = definition.sub_threshold_below();
    let mut out = CommitAggregate {
        aggregate: None,
        aggregate_basis: None,
        band: None,
        loc: total_loc,
        files: usable.len() as u64,
        counts: counts
            .into_iter()
            .map(|(status, n)| (status.to_owned(), n))
            .collect(),
        unanalysed_paths,
        hotspots: Vec::new(),
        amber_paths: Vec::new(),
        sub_threshold_set: Vec::new(),
        delta_vs_parent: None,
        sub_threshold_added: Vec::new(),
        sub_threshold_removed: Vec::new(),
    };

    if usable.is_empty() || total_loc == 0 {
        out.aggregate_basis = Some("no measurable file in scope".to_owned());
        return out;
    }

    let mean = round_storage(definition, weighted / (total_loc as f64));
    out.aggregate = Some(mean);
    out.band = Some(band_of(definition, mean));

    let mut sorted_by_watch: Vec<&FileMetrics> = usable.clone();
    sorted_by_watch.sort_by(path_order);
    out.amber_paths = sorted_by_watch
        .iter()
        .filter(|file| file.mi.unwrap_or(0.0) < definition.bands.amber_below)
        .map(|file| file.path.clone())
        .collect();
    out.sub_threshold_set = sorted_by_watch
        .iter()
        .filter(|file| file.mi.unwrap_or(0.0) < below_watch)
        .map(|file| file.path.clone())
        .collect();

    // Worst-first: MI ascending, then LOC descending so a big file outranks a small one at the same
    // score, then path so the tie is broken by the filesystem and not by the order rows arrived in.
    let mut ranked = usable;
    ranked.sort_by(|a, b| {
        a.mi.unwrap_or(0.0)
            .partial_cmp(&b.mi.unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.loc.unwrap_or(0).cmp(&a.loc.unwrap_or(0)))
            .then_with(|| path_order(a, b))
    });
    out.hotspots = ranked
        .into_iter()
        .take(definition.aggregation.hotspots_default)
        .map(|file| Hotspot {
            path: file.path.clone(),
            mi: file.mi.unwrap_or(0.0),
            loc: file.loc.unwrap_or(0),
            delta: None,
        })
        .collect();
    out
}

/// The row's whole `mi` section: the aggregate plus what only a comparison with the first parent can
/// say — the delta, the watchlist transitions, and each hotspot's own movement.
///
/// `parent` is `None` for a root commit, which is why `delta_vs_parent` is *absent* there and not
/// `0.0`: a root commit did not fail to change, it had nothing to change from (§4.3, and ADR-0002
/// §2.1).
#[must_use]
pub fn commit_row(
    definition: &Definition,
    files: &[FileMetrics],
    parent: Option<&[FileMetrics]>,
) -> CommitAggregate {
    let mut row = aggregate(definition, files);
    let Some(parent_files) = parent else {
        return row;
    };
    let before = aggregate(definition, parent_files);
    row.delta_vs_parent = match (row.aggregate, before.aggregate) {
        (Some(now), Some(then)) => Some(round_storage(definition, now - then)),
        _ => None,
    };

    let here: BTreeSet<&str> = row.sub_threshold_set.iter().map(String::as_str).collect();
    let there: BTreeSet<&str> = before
        .sub_threshold_set
        .iter()
        .map(String::as_str)
        .collect();
    // `added` and `removed` are set differences, emitted in path-bytes order — the order the ADR
    // pins for these two lists, spelled out rather than left to a `BTreeSet` accident.
    let paths_in = |set: &BTreeSet<&str>| -> Vec<String> {
        let mut paths: Vec<&str> = set.iter().copied().collect();
        paths.sort_by_key(|path| path.as_bytes());
        paths.into_iter().map(str::to_owned).collect()
    };
    let added: BTreeSet<&str> = here.difference(&there).copied().collect();
    let removed: BTreeSet<&str> = there.difference(&here).copied().collect();
    row.sub_threshold_added = paths_in(&added);
    row.sub_threshold_removed = paths_in(&removed);

    let mut parent_mi: BTreeMap<&str, f64> = BTreeMap::new();
    for file in parent_files {
        if let Some(mi) = file.mi {
            parent_mi.insert(file.path.as_str(), mi);
        }
    }
    for hotspot in &mut row.hotspots {
        if let Some(then) = parent_mi.get(hotspot.path.as_str()).copied() {
            hotspot.delta = Some(round_storage(definition, hotspot.mi - then));
        }
    }
    row
}
