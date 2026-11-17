//! `fathom report` — the timeline table, machine-readable.
//!
//! The ticket's sentence is *"same rows, same ADR-0008 semantics, same ordering as the TUI — one code
//! path, two renderers"*, and this module holds up its half of it literally: the rows come from
//! [`crate::tui::store_view`] (the store's only reader, mact-by1.9) and every cell that has a display
//! spelling is spelled by [`crate::tui::cells::cells`] against `schema/metric-model.json`'s **ascii**
//! column — the column the model itself names for this surface (`display.rules`: "`--plain` and `fathom
//! report --format tsv` use the ascii column of this table"). There is no second derivation here,
//! because a second derivation is a second implementation free to disagree with the first.
//!
//! ## The three formats, and what each one is for
//!
//! * **`json` is the stable one.** The envelope is versioned ([`SCHEMA_VERSION`]) and it is the format
//!   whose bytes a test pins. Unknown is `null` (ADR-0008 §6.1's table: *in JSON `null`*), with the gap
//!   code naming *which* kind of unknown and the caveat codes behind every `*` the other formats print.
//!   Nothing in it depends on the wall clock: two runs of the same store at different times produce
//!   byte-identical output, which is what makes it a fixture rather than a log.
//! * **`tsv`** is the timeline as the screen draws it: [`crate::tui::cells::COLUMNS`] in screen order
//!   (ascii glyphs), plus `indexed` and `gaps`. Unknown is `-` (ADR-0008 §6.1 again: *`-` in TSV/CSV*)
//!   — never `0` and never an empty field, because a spreadsheet reads an empty cell as zero and a zero
//!   in this product is a measurement.
//! * **`csv`** is the same rows with RFC 4180 quoting, for the consumer whose only problem is commas.
//!
//! ## Rows are commits, not rows
//!
//! `fathom tui` draws what the metric cache holds. A headless consumer needs a stronger promise: `git`
//! knows the window, so `report` enumerates the window and answers for **every commit in it** — a
//! commit the index has not reached is a row of unknowns with `"indexed": false`, not a missing row.
//! That is what makes `--only-missing` a queue a runner can work through: a commit that was never
//! indexed has no evidence either, and a sweep that only ever saw indexed commits would skip exactly
//! the commits nobody has looked at.
//!
//! ## Exit codes — the `act-mcp validate-evidence` convention, on purpose
//!
//! | code | meaning |
//! |---|---|
//! | 0 | the report was produced, and the gate (if any) found nothing |
//! | 1 | the report was produced, and `--fail-on` found something — a finding is the product, not a fault |
//! | 2 | the report could not be produced: not a work tree, git refused, the window does not resolve |
//!
//! `validate-evidence` uses 0/1/2 exactly this way, and matching it is the ticket's requirement: one
//! pipeline, two tools, one habit. It is *narrower* than [`crate::exit`]'s table, which is about the
//! binary as a whole: a bad flag is still clap's 2 and every other command still refuses with 1.
//!
//! ## stdout is data
//!
//! Only the report goes to stdout. The "no metric cache" notice, every `fix:` line, the log at `-v` and
//! the findings a gate tripped on are all stderr (`lib.rs`'s rule 1), so `fathom report > timeline.tsv`
//! stays parseable no matter what the workspace looks like.

use std::collections::HashMap;
use std::path::PathBuf;

use clap::ValueEnum;
use serde::Serialize;

use crate::boot::Boot;
use crate::cli::{CommonArgs, ReportArgs};
use crate::config::Config;
use crate::exit::Exit;
use crate::git::Git;
use crate::history::Query;
use crate::metrics::model::Glyphs;
use crate::metrics::vocab::ActionState;
use crate::metrics::Model;
use crate::store::now_utc;
use crate::tui::cells;
use crate::tui::entry::{age, Delta, Entry};
use crate::tui::store_view::{self, Reading, StoreState};
use crate::{warn, FathomError};

/// The version of **this envelope** — not the evidence contract's (ADR-0002 v1) and not the store's
/// ([`crate::store::SCHEMA_VERSION`]). It is in the payload for the reason those two carry theirs: a
/// consumer that cannot read this shape should refuse it rather than guess at it.
pub const SCHEMA_VERSION: u32 = 1;

/// What an unknown reads as in TSV and CSV (ADR-0008 §6.1).
///
/// This is the model's ascii `unknown` glyph, and `tests/report.rs` asserts it still equals
/// `schema/metric-model.json`, so spelling it here is a convenience that cannot drift silently.
pub const UNKNOWN: &str = "-";

/// The flat formats' columns, in order.
///
/// The first eight are [`crate::tui::cells::COLUMNS`] in screen order with machine-readable names
/// (`coverage`, not `cov`); the last two are headless-only: `indexed` separates "no evidence" from
/// "never indexed", and `gaps` carries the codes that say which kind of unknown a `-` is.
pub const COLUMNS: [&str; 10] = [
    "sha", "age", "message", "coverage", "churn", "mi", "bench", "action", "indexed", "gaps",
];

/// `--format`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    /// The versioned envelope; the stable one.
    Json,
    /// Tab-separated: the timeline's own columns.
    Tsv,
    /// Comma-separated, RFC 4180 quoting.
    Csv,
}

impl Format {
    /// The flag's own spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Tsv => "tsv",
            Self::Csv => "csv",
        }
    }
}

/// `--fail-on <what>`: the gate a CI step asks for, and the only thing that can make this command
/// exit 1.
///
/// Every variant is a question about one row measured against that row's own first parent — the same
/// comparison the timeline's arrows show — so a gate and a screen can never disagree about whether a
/// commit moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum FailOn {
    /// Coverage fell against the first parent.
    CoverageDrop,
    /// The maintainability aggregate fell against the first parent.
    MiDrop,
    /// A benchmark group moved outside `benchmarks.regression_band_percent` against the same group on
    /// the first parent.
    BenchmarkRegression,
    /// The action column says `failed` or `mixed`: some workflow that ran here did not pass.
    ActionFailure,
}

impl FailOn {
    /// The flag's spelling, and the `check` field's value in the JSON findings.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CoverageDrop => "coverage-drop",
            Self::MiDrop => "mi-drop",
            Self::BenchmarkRegression => "benchmark-regression",
            Self::ActionFailure => "action-failure",
        }
    }
}

/// One gate tripped on one commit.
#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    /// The full SHA (ADR-0008 §6.2: identity is never truncated).
    pub sha: String,
    /// Which `--fail-on` value asked, in its flag spelling.
    pub check: String,
    /// What moved, with both numbers and the commit it was measured against.
    pub detail: String,
}

impl Finding {
    /// The one-line form stderr prints when a gate trips.
    #[must_use]
    pub fn line(&self) -> String {
        format!("{}  {}: {}", short(&self.sha), self.check, self.detail)
    }
}

/// The gate's state, as the envelope reports it.
#[derive(Debug, Clone, Serialize)]
pub struct Gate {
    /// The flags that were asked for, in the order given. Empty means the gate is off and exit 1 is
    /// unreachable: a report nobody asked to be judged is not a report that passed.
    pub fail_on: Vec<String>,
    /// `off` | `clean` | `findings`.
    pub status: &'static str,
    /// The band `benchmark-regression` judged against (`benchmarks.regression_band_percent`).
    pub band_percent: f64,
    /// Every finding, newest commit first, in `--fail-on` order within a commit.
    pub findings: Vec<Finding>,
}

/// The window the rows describe — echoed, because a report that does not say what it looked at cannot
/// be compared with another one.
#[derive(Debug, Clone, Serialize)]
pub struct Window {
    /// The oldest commit included, `null` for "`history.depth` back from `to`".
    pub from: Option<String>,
    /// The newest commit included; `null` means `HEAD`, which is what the flag defaults to.
    pub to: Option<String>,
    /// The `--limit`, if any.
    pub limit: Option<usize>,
    /// Whether only first parents were walked (`history.first_parent` / `--first-parent`).
    pub first_parent: bool,
    /// Whether only commits with no evidence were kept.
    pub only_missing: bool,
}

/// What the metric cache said about itself, in the two facts a headless reader needs.
#[derive(Debug, Clone, Serialize)]
pub struct StoreField {
    /// `present` | `absent` | `refused`.
    pub state: &'static str,
    /// Row files the store holds (`null` when there is no store to count).
    pub rows: Option<usize>,
    /// Why a store was refused, in the store's own words (ADR-0006 §8 names the path to delete).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
}

/// Counts the surface has to state, so a cut is visible rather than implied.
#[derive(Debug, Clone, Serialize)]
pub struct Counts {
    /// Rows in the report.
    pub commits: usize,
    /// Commits the window had that `--limit` hid.
    pub hidden: usize,
    /// Rows with at least one piece of evidence.
    pub with_evidence: usize,
    /// Rows with none — what `--only-missing` keeps, counted even when it did not filter, so a reader
    /// knows the queue is not empty.
    pub missing_evidence: usize,
    /// Rows with no row in the metric cache at all.
    pub not_indexed: usize,
}

/// The coverage column as data.
#[derive(Debug, Clone, Serialize)]
pub struct CoverageCell {
    /// The pooled percent, `null` when nothing measured it.
    pub percent: Option<f64>,
    /// `up` | `down` | `flat` against the first parent, `null` with no both-sided comparison.
    pub delta: Option<&'static str>,
    /// Whether a caveat sits behind the number.
    pub caveat: bool,
    /// The gap code, when there is no number.
    pub gap: Option<String>,
}

/// The churn column as data.
#[derive(Debug, Clone, Serialize)]
pub struct ChurnCell {
    /// Lines added, `null` when the row has no diffstat.
    pub add: Option<u64>,
    /// Lines deleted.
    pub del: Option<u64>,
    /// Files touched.
    pub files: Option<u64>,
    /// The noise half of `add`, counted in it and labelled (ADR-0006 §7.2).
    pub noise_add: u64,
    /// The noise half of `del`.
    pub noise_del: u64,
    /// Whether the reader's per-file cap made `files` a floor.
    pub truncated: bool,
}

/// The MI column as data.
#[derive(Debug, Clone, Serialize)]
pub struct MiCell {
    /// The LOC-weighted aggregate, at its stored precision.
    pub score: Option<f64>,
    /// `up` | `down` | `flat` against the first parent.
    pub delta: Option<&'static str>,
    /// Whether unanalysed paths qualify this number (ADR-0007 §4.4).
    pub caveat: bool,
    /// The gap code, when there is no aggregate.
    pub gap: Option<String>,
    /// ADR-0007 §4.4's counter: paths the analyser could not measure.
    pub unanalysed_paths: u64,
}

/// The benchmark column as data: one group (ADR-0008 §4.5's choice) and the count behind it.
#[derive(Debug, Clone, Serialize)]
pub struct BenchCell {
    /// The group's name; `null` when the commit has no benchmark group at all (the gap code says why),
    /// because an empty string is a name a consumer would have to special-case.
    pub name: Option<String>,
    /// The group's canonical label set, so a comparison is always inside one group; `null` with no group.
    /// Spelled as the reader spells it — a JSON object as text — because it is a join key, not prose.
    pub labels: Option<String>,
    /// The value at four significant digits with its unit, as the screen spells it (`412.5ms`); `null`
    /// with no group.
    pub text: Option<String>,
    /// The same number unformatted, `null` for a group this build cannot compare (no unit).
    pub value: Option<f64>,
    /// Whether the group's unit makes it comparable at all (ADR-0008 §4.3).
    pub comparable: bool,
    /// Whether it is `act_log`-derived context, which is never a regression signal (§4.4).
    pub context: bool,
    /// How many groups the row carries in total, this one included.
    pub groups: usize,
    /// `up` | `down` | `flat` against the same group on the first parent.
    pub delta: Option<&'static str>,
    /// Whether a caveat sits behind the number.
    pub caveat: bool,
    /// The gap code, when there is no group.
    pub gap: Option<String>,
}

/// The screen's own cell text, for a consumer that wants what a human would have seen.
///
/// This is [`crate::tui::cells::cells`]'s output for the same row and the same glyph table, and
/// `tests/report.rs` uses it to assert the two surfaces agree on one fixture.
#[derive(Debug, Clone, Serialize)]
pub struct DisplayCells {
    /// The `cov` column.
    pub coverage: String,
    /// The `+/-` column.
    pub churn: String,
    /// The `MI` column.
    pub mi: String,
    /// The `bench` column.
    pub bench: String,
    /// The `action` column.
    pub action: String,
}

/// One commit, in the envelope's vocabulary.
///
/// Every field that can be unknown is an `Option` and serde writes `null` for it (ADR-0008 §6.1). The
/// `display` block is the exception and the point: it is the *screen's* text for the same row.
#[derive(Debug, Clone, Serialize)]
pub struct Commit {
    /// The full SHA.
    pub sha: String,
    /// The seven-hex display form (ADR-0008 §6.2).
    pub short: String,
    /// The first parent, `null` for a root commit — which is also why its deltas are `null`.
    pub parent: Option<String>,
    /// The message head.
    pub subject: String,
    /// The author name, as written.
    pub author: String,
    /// Author instant, Unix seconds — the clock the `age` column counts back from.
    pub author_utc: i64,
    /// Committer instant, Unix seconds — the clock the ordering uses.
    pub committer_utc: i64,
    /// Whether the metric cache holds a row for this commit. `false` is "never indexed": every column
    /// is unknown, and the reason is this field rather than a gap code.
    pub indexed: bool,
    /// The action state (`success` | `failed` | `mixed` | `no_evidence`), ADR-0008 §5.3.
    pub action: String,
    /// The coverage column (ADR-0008 §3).
    pub coverage: CoverageCell,
    /// The `+`/`−` column (ADR-0007 §7.1).
    pub churn: ChurnCell,
    /// The MI column (ADR-0007 §4).
    pub mi: MiCell,
    /// The one benchmark group (ADR-0008 §4.5).
    pub bench: BenchCell,
    /// The gap codes on this row, sorted: `not_indexed` for a commit the store never reached, then
    /// `<column>:<gap code>` for each column that came back unknown.
    pub gaps: Vec<String>,
    /// The caveat codes behind the `*`s, in the model's spelling.
    pub caveats: Vec<String>,
    /// The screen's text for the five metric columns — the same strings `fathom tui` draws.
    pub display: DisplayCells,
}

/// The envelope.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    /// The version of the binary that wrote this (`CARGO_PKG_VERSION`, one line, no metadata).
    pub fathom_version: &'static str,
    /// [`SCHEMA_VERSION`].
    pub schema_version: u32,
    /// The workspace the rows came from, canonicalised — the same path `fathom index --status` prints.
    pub workspace: PathBuf,
    /// The window.
    pub window: Window,
    /// The metric cache's state.
    pub store: StoreField,
    /// The counts.
    pub counts: Counts,
    /// The gate.
    pub gate: Gate,
    /// The rows, newest first.
    pub commits: Vec<Commit>,
}

impl Report {
    /// The envelope as JSON: two-space indent, keys in declaration order, one trailing newline.
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut text = serde_json::to_string_pretty(self)
            .unwrap_or_else(|_| String::from("{\"schema_version\":0}\n"));
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text
    }

    /// The rows as tab-separated lines with a header row — the timeline, de-dressed.
    #[must_use]
    pub fn to_tsv(&self) -> String {
        let mut out = COLUMNS.join("\t");
        out.push('\n');
        for row in &self.rows() {
            out.push_str(&row.join("\t"));
            out.push('\n');
        }
        out
    }

    /// The same rows as CSV (RFC 4180 quoting, `\n` line endings).
    #[must_use]
    pub fn to_csv(&self) -> String {
        let mut out = COLUMNS
            .iter()
            .map(|column| csv_field(column))
            .collect::<Vec<_>>()
            .join(",");
        out.push('\n');
        for row in &self.rows() {
            out.push_str(
                &row.iter()
                    .map(|field| csv_field(field))
                    .collect::<Vec<_>>()
                    .join(","),
            );
            out.push('\n');
        }
        out
    }

    /// The flat rows: [`COLUMNS`] spelled for one report, in order.
    ///
    /// The `age` cell is the only clock-derived value anywhere in this module, and it is deliberately
    /// absent from the JSON: a pinned golden file may not expire overnight.
    #[must_use]
    pub fn rows(&self) -> Vec<Vec<String>> {
        let now = now_utc();
        self.commits
            .iter()
            .map(|commit| {
                let mut row = vec![
                    commit.short.clone(),
                    age(now.saturating_sub(commit.author_utc)),
                    commit.subject.clone(),
                    commit.display.coverage.clone(),
                    commit.display.churn.clone(),
                    commit.display.mi.clone(),
                    commit.display.bench.clone(),
                    commit.display.action.clone(),
                    commit.indexed.to_string(),
                    if commit.gaps.is_empty() {
                        UNKNOWN.to_owned()
                    } else {
                        commit.gaps.join(";")
                    },
                ];
                // No field is ever empty in either flat format: an empty field is what a spreadsheet
                // reads as a zero, and a zero here claims a measurement (ADR-0008 §6.1).
                for field in &mut row {
                    if field.is_empty() {
                        field.push_str(UNKNOWN);
                    }
                }
                row
            })
            .collect()
    }

    /// Render in the format asked for.
    #[must_use]
    pub fn render(&self, format: Format) -> String {
        match format {
            Format::Json => self.to_json(),
            Format::Tsv => self.to_tsv(),
            Format::Csv => self.to_csv(),
        }
    }
}

/// Quote one CSV field when it needs it (RFC 4180: a `"` is doubled, and a field containing a comma, a
/// quote or a line ending is quoted). Leading and trailing whitespace is quoted too, because a reader
/// that trims it silently changes the message.
#[must_use]
pub fn csv_field(text: &str) -> String {
    let needs_quotes = text.is_empty()
        || text.contains(',')
        || text.contains('"')
        || text.contains('\n')
        || text.contains('\r')
        || text.trim() != text;
    if needs_quotes {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text.to_owned()
    }
}

/// `fathom report [WORKSPACE]`: build, print, and decide the exit code.
///
/// Printing is the only side effect; everything else is [`build`], which a test can call without a
/// process, a TTY or a pipe.
///
/// # Errors
///
/// [`crate::FathomError`] when the report could not be produced at all, which the caller maps to exit 2
/// rather than 1 ([`crate::exit`]).
pub fn run(
    boot: &Boot,
    config: &Config,
    common: &CommonArgs,
    args: &ReportArgs,
) -> Result<Exit, FathomError> {
    let report = build(boot, config, common, args)?;
    print!("{}", report.render(args.format));
    for finding in &report.gate.findings {
        // The findings are data only in the JSON: a TSV consumer must not inherit prose, so the human
        // line goes to stderr with every other diagnostic.
        warn!("finding: {}", finding.line());
    }
    Ok(if report.gate.status == "findings" {
        Exit::Findings
    } else {
        Exit::Success
    })
}

/// Build the report: the store read the way the screen reads it, the window `git` enumerates, the two
/// reconciled, the gate evaluated.
///
/// # Errors
///
/// [`crate::FathomError`] if `git` cannot answer what the window is. Without the commit list there is
/// no report, and inventing one is the failure this command exists to avoid.
pub fn build(
    boot: &Boot,
    config: &Config,
    common: &CommonArgs,
    args: &ReportArgs,
) -> Result<Report, FathomError> {
    // The store read, with no limit: the limit is applied after the window and the `--only-missing`
    // filter, because `--limit 5` means "the five newest rows of what I asked for", not "the five
    // newest rows of the store, then see what survives". This is the same call the TUI makes; passing
    // `None` and truncating below is the only difference, and it is what lets a delta still find a
    // parent that `--limit` would otherwise have cut off.
    let reading = store_view::read(boot, None);
    let query = crate::cli::window(config, common);
    let model = Model::embedded();
    let glyphs: &Glyphs = &model.display.ascii;

    let all = rows_for(&reading, &query, boot)?;
    let filtered: Vec<&Entry> = all
        .iter()
        .filter(|entry| !args.only_missing || !has_evidence(entry))
        .collect();
    let hidden = filtered.len().saturating_sub(
        common
            .limit
            .map_or(filtered.len(), |limit| limit.get().min(filtered.len())),
    );
    let shown: Vec<&Entry> = filtered
        .iter()
        .copied()
        .take(common.limit.map_or(usize::MAX, |limit| limit.get()))
        .collect();

    let by_sha: HashMap<&str, &Entry> = all
        .iter()
        .map(|entry| (entry.sha.as_str(), entry))
        .collect();
    let gate = gate(
        &shown,
        &by_sha,
        &args.fail_on,
        config.benchmarks.regression_band_percent,
    );

    let store = store_field(&reading);
    match &reading.store {
        // ADR-0006 §10.1 asks for exactly this notice, on stderr, for a reader: every column will be
        // `—`, and a reader who is not told why will spend the afternoon looking at the evidence tree.
        StoreState::Absent => warn!(
            "no metric cache in this workspace ({}) \n  fix: run `fathom index {}` and then run this again",
            boot.cache.path.display(),
            boot.workspace.display()
        ),
        StoreState::Refused(reason) => {
            warn!("the metric store refused to open: {}", one_line(reason));
        }
        StoreState::Present { .. } => {}
    }

    let commits: Vec<Commit> = shown.iter().map(|entry| commit(entry, glyphs)).collect();
    let with_evidence = shown.iter().filter(|entry| has_evidence(entry)).count();
    let not_indexed = shown.iter().filter(|entry| !entry.indexed).count();

    Ok(Report {
        fathom_version: env!("CARGO_PKG_VERSION"),
        schema_version: SCHEMA_VERSION,
        workspace: boot.workspace.clone(),
        window: Window {
            from: common.from.clone(),
            to: common.to.clone(),
            limit: common.limit.map(|limit| limit.get()),
            first_parent: query.first_parent,
            only_missing: args.only_missing,
        },
        store,
        counts: Counts {
            commits: commits.len(),
            hidden,
            with_evidence,
            missing_evidence: shown.len() - with_evidence,
            not_indexed,
        },
        gate,
        commits,
    })
}

/// The store's state, in the facts the envelope carries.
fn store_field(reading: &Reading) -> StoreField {
    match &reading.store {
        StoreState::Absent => StoreField {
            state: "absent",
            rows: None,
            refusal: None,
        },
        StoreState::Present { rows } => StoreField {
            state: "present",
            rows: Some(*rows),
            refusal: None,
        },
        StoreState::Refused(reason) => StoreField {
            state: "refused",
            rows: None,
            refusal: Some(reason.clone()),
        },
    }
}

/// Every commit in the window, joined to its row when the store has one.
///
/// `git log` is asked for the window `--from`/`--to`/`history.depth` name — the window `fathom index`
/// walked — and each entry is joined to the store's rows by SHA. A commit with no row becomes
/// [`Entry::unindexed`]: every column unknown, `indexed: false`, and the identity fields (subject,
/// author, both clocks) straight from git, because those are git's to give and a report that lists a
/// commit it cannot name is worse than no report.
///
/// A repository with no commits is the empty answer, not an error: an unborn `HEAD` has no rows and no
/// findings, and exit 0 there is what lets a CI step run this before the first push.
fn rows_for(reading: &Reading, query: &Query, boot: &Boot) -> Result<Vec<Entry>, FathomError> {
    let git = Git::open(&boot.workspace)?;
    if git.is_empty()? {
        return Ok(Vec::new());
    }
    let by_sha: HashMap<&str, &Entry> = reading
        .entries
        .iter()
        .map(|entry| (entry.sha.as_str(), entry))
        .collect();
    Ok(window_entries(&git, query)?
        .into_iter()
        .map(|stub| {
            by_sha
                .get(stub.sha.as_str())
                .map_or_else(|| stub.unindexed(), |entry| (*entry).clone())
        })
        .collect())
}

/// One commit as `git log` describes it, before the store is consulted.
#[derive(Debug, Clone)]
struct Stub {
    sha: String,
    parent: Option<String>,
    author: String,
    author_utc: i64,
    committer_utc: i64,
    subject: String,
}

impl Stub {
    /// The all-unknown row for a commit the index never reached.
    fn unindexed(&self) -> Entry {
        Entry::unindexed(
            self.sha.clone(),
            self.parent.clone(),
            self.author.clone(),
            self.author_utc,
            self.committer_utc,
            self.subject.clone(),
        )
    }
}

/// The window, newest first, in `git log`'s own order.
///
/// One subprocess, `%x00`-separated fields, and the same `from`/`to`/`--first-parent`/depth semantics
/// `fathom index` used to build the store — [`crate::cli::window`] is the single definition of a window,
/// shared by the index, the screen and this command, which is the only reason the three can be compared.
fn window_entries(git: &Git, query: &Query) -> Result<Vec<Stub>, FathomError> {
    let to = git.resolve(&query.to)?;
    let from = query
        .from
        .as_deref()
        .map(|revision| git.resolve(revision))
        .transpose()?;
    let mut args: Vec<String> = vec![
        "log".to_owned(),
        format!("--max-count={}", query.depth.max(1)),
        // `%x1e` starts each record and `%x00` separates its fields, which puts the record boundary
        // where the fields cannot be confused with it. A subject is one line by definition and an author
        // with a newline in it is a fixture nobody has, but the field-count guard below refuses rather
        // than guesses either way.
        "--format=%x1e%H%x00%P%x00%an%x00%at%x00%ct%x00%s".to_owned(),
    ];
    if query.first_parent {
        args.push("--first-parent".to_owned());
    }
    // `from` is inclusive, which git spells `--not <from>^` (the parent's ancestry is excluded and
    // `from` itself stays); the shared helper gives a root `from`'s missing parent the same answer the
    // index gets, so both commands walk exactly the same set.
    let from_has_parent = from
        .as_deref()
        .is_some_and(|sha| git.has_parent(sha).unwrap_or(false));
    args.extend(crate::git::revs(from.as_deref(), from_has_parent, &to));

    let command: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = git.run(&command)?;
    let text = String::from_utf8_lossy(&out).into_owned();
    let mut stubs = Vec::new();
    // `--format` writes the record and then a newline, so every record but the last is
    // `…subject\x00\n`; the sixth field's terminator is where the newline landed, and trimming it off
    // the tail is the whole parse.
    // U+001E (record separator) between records, written as an escape the MI's Rust lexer reads.
    for record in text
        .split('\u{1e}')
        .filter(|record| !record.trim().is_empty())
    {
        let record = record.trim_end();
        let fields: Vec<&str> = record.split('\0').collect();
        // A record with fewer fields than the format asked for is a field that contained a separator;
        // skip it rather than invent a row, and let the row count disagree loudly in the report.
        if fields.len() < 6 {
            continue;
        }
        stubs.push(Stub {
            sha: fields[0].to_owned(),
            parent: fields[1].split_whitespace().next().map(str::to_owned),
            author: fields[2].to_owned(),
            author_utc: fields[3].trim().parse().unwrap_or(0),
            committer_utc: fields[4].trim().parse().unwrap_or(0),
            subject: fields[5].to_owned(),
        });
    }
    Ok(stubs)
}

/// Whether a commit has any evidence at all.
///
/// Coverage, benchmarks and the action verdict all come from `.evidence`; `churn` and `MI` come from git
/// and the blobs, so they are not evidence and do not count. A commit the index never reached has none
/// by definition, and belongs in the queue.
#[must_use]
pub fn has_evidence(entry: &Entry) -> bool {
    entry.has_evidence()
}

/// The gate: every `--fail-on` question, asked of every shown row.
///
/// The comparisons are the ones the timeline's arrows already encode (`coverage.delta`, `mi.delta`,
/// `bench.delta`), so a gate and a screen cannot disagree: a row with no arrow has no parent on both
/// sides and can never be a finding. `benchmark-regression` is the one that needs a number rather than
/// a direction, because the metric model deliberately refuses to know whether a bigger number is a
/// better commit (ADR-0008 §4.5) — so the gate asks the panel's question, "is this move outside the
/// declared band", and leaves the judgement about *worse* to whoever set the band.
fn gate(rows: &[&Entry], by_sha: &HashMap<&str, &Entry>, fail_on: &[FailOn], band: f64) -> Gate {
    let mut findings = Vec::new();
    if !fail_on.is_empty() {
        for entry in rows {
            let parent = entry
                .parent
                .as_deref()
                .and_then(|sha| by_sha.get(sha).copied());
            for check in fail_on {
                if let Some(detail) = finding(entry, parent, *check, band) {
                    findings.push(Finding {
                        sha: entry.sha.clone(),
                        check: check.as_str().to_owned(),
                        detail,
                    });
                }
            }
        }
    }
    Gate {
        fail_on: fail_on
            .iter()
            .map(|check| check.as_str().to_owned())
            .collect(),
        status: if fail_on.is_empty() {
            "off"
        } else if findings.is_empty() {
            "clean"
        } else {
            "findings"
        },
        band_percent: band,
        findings,
    }
}

/// One question about one row: `None` for "did not happen", `Some(..)` for a sentence about what did.
fn finding(entry: &Entry, parent: Option<&Entry>, check: FailOn, band: f64) -> Option<String> {
    match check {
        FailOn::CoverageDrop => (entry.coverage.delta == Some(Delta::Down)).then(|| {
            parent.map_or_else(
                || {
                    format!(
                        "coverage {:.1}%, down on its parent",
                        entry.coverage.value.unwrap_or_default()
                    )
                },
                |parent| {
                    format!(
                        "coverage {:.1}% was {:.1}% at {}",
                        entry.coverage.value.unwrap_or_default(),
                        parent.coverage.value.unwrap_or_default(),
                        short(entry.parent.as_deref().unwrap_or(""))
                    )
                },
            )
        }),
        FailOn::MiDrop => (entry.mi.delta == Some(Delta::Down)).then(|| {
            parent.map_or_else(
                || {
                    format!(
                        "MI {:.1}, down on its parent",
                        entry.mi.value.unwrap_or_default()
                    )
                },
                |parent| {
                    format!(
                        "MI {:.1} was {:.1} at {}",
                        entry.mi.value.unwrap_or_default(),
                        parent.mi.value.unwrap_or_default(),
                        short(entry.parent.as_deref().unwrap_or(""))
                    )
                },
            )
        }),
        FailOn::BenchmarkRegression => {
            let now = entry.bench.raw.filter(|_| entry.bench.comparable)?;
            let parent = parent.filter(|parent| parent.bench.comparable)?;
            let before = parent.bench.raw?;
            if entry.bench.context || !moves_outside_band(now, before, band) {
                return None;
            }
            let percent = if before == 0.0 {
                f64::INFINITY
            } else {
                (now - before) / before.abs() * 100.0
            };
            Some(format!(
                "{} for `{}` was {} at {} ({:+.1}%, band {band:.1}%)",
                entry.bench.value,
                entry.bench.name,
                parent.bench.value,
                short(entry.parent.as_deref().unwrap_or("")),
                percent
            ))
        }
        FailOn::ActionFailure => matches!(entry.action, ActionState::Failed | ActionState::Mixed)
            .then(|| format!("action is `{}`", entry.action.as_str())),
    }
}

/// Whether a benchmark's move is outside the band the config declared.
///
/// The panel's own arithmetic ([`crate::tui::bench`]'s verdict): a percent of the *baseline's* value
/// compared with the band. A zero baseline cannot be a percentage, so this answers `false` rather than
/// inventing one — the band is a percentage and a division by zero is not a measurement.
#[must_use]
pub fn moves_outside_band(now: f64, before: f64, band: f64) -> bool {
    if before == 0.0 || !before.is_finite() || !now.is_finite() {
        return false;
    }
    ((now - before) / before.abs() * 100.0).abs() > band
}

/// Seven hex, for a sentence that names a commit.
fn short(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// `None` for an empty string, so the flat model's blank becomes the envelope's `null`.
fn none_if_empty(text: &str) -> Option<String> {
    (!text.is_empty()).then(|| text.to_owned())
}

/// First line, for a store refusal whose second line is already a `fix:` of its own.
fn one_line(text: &str) -> String {
    text.lines().next().unwrap_or(text).trim().to_owned()
}

/// One store row becomes one envelope commit.
///
/// Every `Option` here is `None` exactly when the screen shows `—`, which is the whole point of ADR-0008
/// §6.1's three states: `None` becomes `null`, and a `0` stays a measured zero.
fn commit(entry: &Entry, glyphs: &Glyphs) -> Commit {
    // `now` is 0 on purpose: the age cell is a clock value and the JSON carries no clock. `cells` is
    // called once per row for the five metric columns and the age column is simply not used.
    let texts = cells::cells(entry, glyphs, 0);
    let mut gaps: Vec<String> = Vec::new();
    if !entry.indexed {
        gaps.push("not_indexed".to_owned());
    }
    for (column, gap) in [
        ("coverage", entry.coverage.gap.as_deref()),
        ("benchmarks", entry.bench.gap.as_deref()),
        ("mi", entry.mi.gap.as_deref()),
    ] {
        if let Some(code) = gap {
            gaps.push(format!("{column}:{code}"));
        }
    }
    gaps.sort();

    Commit {
        sha: entry.sha.clone(),
        short: entry.short(),
        parent: entry.parent.clone(),
        subject: entry.subject.clone(),
        author: entry.author.clone(),
        author_utc: entry.author_utc,
        committer_utc: entry.committer_utc,
        indexed: entry.indexed,
        action: entry.action.as_str().to_owned(),
        coverage: CoverageCell {
            percent: entry.coverage.value,
            delta: delta(entry.coverage.delta),
            caveat: entry.coverage.caveat,
            gap: entry.coverage.gap.clone(),
        },
        churn: ChurnCell {
            add: entry.churn.add,
            del: entry.churn.del,
            files: entry.churn.files,
            noise_add: entry.churn.noise_add,
            noise_del: entry.churn.noise_del,
            truncated: entry.churn.truncated,
        },
        mi: MiCell {
            score: entry.mi.value,
            delta: delta(entry.mi.delta),
            caveat: entry.mi.caveat,
            gap: entry.mi.gap.clone(),
            unanalysed_paths: entry.unanalysed_paths,
        },
        bench: BenchCell {
            // `null` rather than `""`: the envelope spells unknown the same way in every column, and an
            // empty name is the one spelling of unknown a consumer would read as a name.
            name: none_if_empty(entry.bench.name.as_str()),
            labels: none_if_empty(entry.bench.labels.as_str()),
            text: none_if_empty(entry.bench.value.as_str()),
            value: entry.bench.raw,
            comparable: entry.bench.comparable,
            context: entry.bench.context,
            groups: entry.bench.groups(),
            delta: delta(entry.bench.delta),
            caveat: entry.bench.caveat,
            gap: entry.bench.gap.clone(),
        },
        gaps,
        caveats: entry.caveats.clone(),
        display: DisplayCells {
            coverage: texts[3].clone(),
            churn: texts[4].clone(),
            mi: texts[5].clone(),
            bench: texts[6].clone(),
            action: texts[7].clone(),
        },
    }
}

/// A delta as the envelope's word: `up` | `down` | `flat`, and `null` for "no arrow", which is a
/// different sentence from `flat` (ADR-0008 §6.1).
fn delta(delta: Option<Delta>) -> Option<&'static str> {
    delta.map(|delta| match delta {
        Delta::Up => "up",
        Delta::Down => "down",
        Delta::Flat => "flat",
    })
}
