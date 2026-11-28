//! The commit detail view read out of a real store: `fathom index` in, `store_view::detail` out
//! (mact-by1.14's "everything here is read from the index, not recomputed per keypress", tested as a
//! claim about behaviour rather than a comment).
//!
//! `tests/tui_detail.rs` owns the decisions — what a sort does with an unknown, what an empty pane
//! says, which pane owns the cursor — and builds its commit out of JSON it wrote itself, which is the
//! right way to test a decision and the wrong way to test a derivation. This file is the other half:
//! real `git`, a real `fathom index`, and records copied from the checked-in e2e goldens (the bytes a
//! real `act` run produced), re-stamped onto the fixture's own commits the way
//! `scripts/sample_evidence.py` re-stamps them. Every number asserted below is therefore the number
//! the *reader* derived from contract-valid evidence, and the golden's own counts are parsed back out
//! of the record file rather than retyped here — a test that restates its fixture's numbers proves the
//! parser agrees with the test, not with the contract.
//!
//! The shapes the ticket names — a normal commit, a commit with no coverage records, a total larger
//! than the sum of its file records, a percent-only run, a commit of unanalysable files, a renamed file
//! whose coverage record still names the old path — are six commits of one fixture, so one index run
//! pays for all of them. Each is also a caveat test: what a pane says when it has no number is the part
//! a reader acts on.
//!
//! Nothing here needs a TTY, a network, `act`, or `python3`.

mod common;

use std::path::{Path, PathBuf};

use clap::Parser;
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use serde_json::Value;

use common::{canonical, Repo, FIXED_EPOCH_SECONDS};
use fathom::boot::{Access, Boot};
use fathom::cli::Cli;
use fathom::config::{Config, Languages};
use fathom::history::Query;
use fathom::metrics::round_half_away;
use fathom::store::Indexer;
use fathom::tui::detail::{self, Detail, Pane, Snapshot};
use fathom::tui::palette::Palette;
use fathom::tui::plain;
use fathom::tui::render;
use fathom::tui::state::App;
use fathom::tui::store_view;

/// One small Rust file per coverage-record path, so the MI pane has a real cached analysis to read for
/// the same paths the coverage pane lists. The content only has to lex; what is asserted about the
/// numbers it produces is "they are there and they are not zero", never a hand-computed MI.
const RUST_FILES: [(&str, &str); 4] = [
    (
        "src/parser/tokenizer.rs",
        "pub fn take(input: &str) -> &str {\n    let mut out = input;\n    while let Some(rest) = out.strip_prefix(' ') {\n        out = rest;\n    }\n    if out.is_empty() { \"\" } else { out }\n}\n",
    ),
    (
        "src/parser/mod.rs",
        "pub mod tokenizer;\n\npub fn parse(input: &str) -> Vec<&str> {\n    input.split('\\n').filter(|line| !line.is_empty()).collect()\n}\n",
    ),
    (
        "src/util/debug_helpers.rs",
        "pub fn dump(value: &str) -> String {\n    format!(\"<{value}>\")\n}\n",
    ),
    (
        "src/gen/protobuf_out.rs",
        "pub struct Generated {\n    pub a: u32,\n    pub b: u32,\n}\n\nimpl Generated {\n    pub fn sum(&self) -> u32 { self.a + self.b }\n}\n",
    ),
];

/// The repository root, two levels up from `crates/fathom`.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/fathom sits two levels under the repo root")
        .to_path_buf()
}

/// One checked-in e2e golden — the bytes a real `act` run wrote, and the same ones
/// `scripts/sample_evidence.py` lays.
fn golden(name: &str) -> PathBuf {
    repo_root()
        .join("crates")
        .join("act-mcp")
        .join("tests")
        .join("e2e")
        .join("testdata")
        .join("expected")
        .join(name)
}

/// The four goldens, one per shape this view has to explain.
const LCOV: &str = "coverage.jsonl";
const ISTANBUL: &str = "coverage-istanbul.jsonl";
const LOG_SCRAPE: &str = "coverage-log-scrape.jsonl";
const BENCH: &str = "benchmark.jsonl";

/// Every record line of one golden, parsed.
fn records(name: &str) -> Vec<Value> {
    let text = std::fs::read_to_string(golden(name)).expect("the golden is checked in");
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("the golden is contract-valid JSON"))
        .collect()
}

/// `(path, hit, found)` for every `scope:"file"` record of one golden, at the primary measure.
///
/// Parsed from the golden rather than typed in, so the assertions below compare the store against the
/// contract's own bytes.
fn file_records(name: &str) -> Vec<(String, u64, u64)> {
    records(name)
        .into_iter()
        .filter(|record| record["scope"].as_str() == Some("file"))
        .filter_map(|record| {
            Some((
                record.get("path")?.as_str()?.to_owned(),
                record.get("lines_hit")?.as_u64()?,
                record.get("lines_found")?.as_u64()?,
            ))
        })
        .collect()
}

/// The pooled total one golden's `scope:"total"` record claims, as `(hit, found)`.
fn total_record(name: &str) -> (u64, u64) {
    records(name)
        .into_iter()
        .find(|record| record["scope"].as_str() == Some("total"))
        .and_then(|record| {
            Some((
                record.get("lines_hit")?.as_u64()?,
                record.get("lines_found")?.as_u64()?,
            ))
        })
        .unwrap_or((0, 0))
}

/// The `run_id` one golden's records carry after [`Fixture::lay`] re-stamped them.
fn run_id(name: &str, suffix: &str) -> String {
    let stamp = records(name)
        .first()
        .and_then(|record| record["run_id"].as_str())
        .expect("a run id")
        .split('-')
        .next()
        .expect("a run id's timestamp half")
        .to_owned();
    format!("{stamp}-{suffix}")
}

/// The model's own one-decimal spelling of one rate, so a rate asserted here is spelled the way the
/// reader spells it (ADR-0008 §3.1's rounding, not a test's `format!`).
fn rate(hit: u64, found: u64) -> Option<f64> {
    (found > 0).then(|| round_half_away(f64::from(hit as u32) * 100.0 / f64::from(found as u32), 1))
}
// --------------------------------------------------------------------------- the fixture

/// A workspace, a cache root and an evidence root, none of them the developer's.
struct Fixture {
    repo: Repo,
    cache: PathBuf,
    evidence: PathBuf,
}

impl Fixture {
    fn new(parent: &Path, name: &str) -> Self {
        let root = canonical(&parent.join(name));
        std::fs::create_dir_all(&root).expect("create the fixture root");
        let evidence = root.join("evidence");
        std::fs::create_dir_all(&evidence).expect("create the evidence root");
        Self {
            repo: Repo::init(&root.join("workspace")),
            cache: root.join("cache"),
            evidence,
        }
    }

    /// The same `Boot` `fathom index` and `fathom tui` would resolve, flags included.
    fn boot(&self) -> Boot {
        let cli = Cli::parse_from([
            "fathom",
            "--cache-dir",
            &self.cache.display().to_string(),
            "--evidence-dir",
            &self.evidence.display().to_string(),
            &self.repo.path().display().to_string(),
        ]);
        // `Write` because the fixture indexes into this root; the detail read opens the store as a
        // reader and changes nothing.
        Boot::validate(self.repo.path(), &cli.common, Access::Write)
            .expect("the fixture workspace boots")
    }

    /// Index the fixture with the built-in defaults.
    fn index(&self) {
        self.index_with(Config::default());
    }

    /// Index with one config, the boot leaked the way `fathom index` leaks it for the run's lifetime.
    fn index_with(&self, config: Config) {
        let boot = self.boot();
        let boot: &Boot = Box::leak(Box::new(boot));
        Indexer::new(boot, config, Query::new(), "manual")
            .expect("the config compiles")
            .run()
            .expect("the index runs");
    }

    /// The timeline as the reader sees it — the read `fathom tui` does first, so a screen can be drawn
    /// over a snapshot exactly as `enter` draws it.
    fn reading(&self) -> store_view::Reading {
        store_view::read(&self.boot(), None)
    }

    /// What `enter` asks the engine for, asked directly.
    fn detail(&self, sha: &str) -> Result<Snapshot, String> {
        store_view::detail(&self.boot(), sha)
    }

    /// The snapshot as an open, laid-out view (a pane that was never told its height draws one row).
    fn view(&self, sha: &str) -> Detail {
        let mut view = Detail::read(self.detail(sha).expect("the commit has a row"));
        view.set_height(detail::pane_height(30));
        view
    }

    /// The same snapshot as a drawn screen, at a width wide enough that no footer is cut — so a test can
    /// assert that the audit trail reaches the picture and not only the data.
    fn screen(&self, sha: &str) -> String {
        let mut app = App::new(self.reading(), FIXED_EPOCH_SECONDS);
        app.set_size(44, 200);
        app.show_detail(self.detail(sha).expect("the commit has a row"));
        draw(&app)
    }

    /// File one golden's records under `sha`, at the `run_id` `suffix`.
    ///
    /// Only the two fields a re-stamp may be trusted to change are touched — `commit`, the join key the
    /// file is filed under, and the `run_id` that file is named after — plus whatever `edit` does. That
    /// is the discipline `scripts/sample_evidence.py` states for itself: every measured field stays the
    /// golden's own, so a number asserted below is a number `act` produced.
    fn lay(&self, sha: &str, kind: &str, name: &str, suffix: &str, edit: impl Fn(&mut Value)) {
        let mut run = String::new();
        let lines: Vec<String> = records(name)
            .into_iter()
            .map(|mut record| {
                let stamp = format!(
                    "{}-{suffix}",
                    record["run_id"]
                        .as_str()
                        .expect("a run id")
                        .split('-')
                        .next()
                        .expect("a run id's timestamp half")
                );
                run = stamp.clone();
                record["commit"] = Value::String(sha.to_owned());
                record["run_id"] = Value::String(stamp);
                edit(&mut record);
                record.to_string()
            })
            .collect();
        let dir = self.evidence.join(kind).join(sha);
        std::fs::create_dir_all(&dir).expect("create the record directory");
        std::fs::write(dir.join(format!("{run}.jsonl")), lines.join("\n") + "\n")
            .expect("write the records");
    }

    /// Lay one golden with every `lines_hit` raised to its own `lines_found`: a commit whose run
    /// measured everything, which is what makes the next commit's fall visible.
    fn lay_at_full(&self, sha: &str, name: &str, suffix: &str) {
        self.lay(sha, "coverage", name, suffix, |record| {
            if let Some(found) = record.get("lines_found").and_then(Value::as_u64) {
                if record.get("lines_hit").and_then(Value::as_u64).is_some() {
                    record["lines_hit"] = Value::from(found);
                }
            }
        });
    }
}

/// Draw one screen into the test backend and hand back its text.
fn draw(app: &App) -> String {
    let width = 200_usize;
    let mut terminal = Terminal::new(TestBackend::new(width as u16, 44)).unwrap();
    terminal
        .draw(|frame| render::draw(app, frame, &palette()))
        .unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .chunks(width)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .map(|line| line.trim_end().to_owned())
        .collect::<Vec<String>>()
        .join("\n")
}

/// The palette the screens resolve for a 256-colour terminal with no colour disabled.
fn palette() -> Palette {
    Palette::resolve(Some("xterm-256color"), false, false, false)
}

/// The six shapes, in one repository, so one index run answers for all of them.
///
/// ```text
/// c1  the four measured files appear              lcov, every file at 100 %
/// c2  one of them gets worse                    lcov, the golden's own counts
/// c3  one of them is renamed                    lcov, records still naming the old path
/// c4  a benchmark ran and nothing else          benchmark records only
/// c5  a total claiming more than it records     istanbul summary: 2210 found, 677 recorded
/// c6  a scraped percentage, nothing else        log scrape: `Coverage: 87.4%`
/// ```
struct Tree {
    fx: Fixture,
    baseline: String,
    worse: String,
    renamed: String,
    bench_only: String,
    summary_only: String,
    scrape_only: String,
}

impl Tree {
    fn build(parent: &Path) -> Self {
        let fx = Fixture::new(parent, "detail-tree");
        for (path, body) in RUST_FILES {
            fx.repo.write(path, body);
        }
        let baseline = fx.repo.commit("parser: the four files the run measures");
        fx.lay_at_full(&baseline, LCOV, "aaaa01");

        // A real edit to one file, so `c2`'s diff names one path and its coverage record says that path
        // lost a seventh of its lines since its parent.
        fx.repo.write(
            "src/parser/tokenizer.rs",
            "pub fn take(input: &str) -> &str {\n    let mut out = input;\n    while let Some(rest) = out.strip_prefix(' ') {\n        out = rest;\n    }\n    if out.is_empty() {\n        return \"\";\n    }\n    out.trim_end()\n}\n",
        );
        let worse = fx
            .repo
            .commit("tokenizer: trim the trailing whitespace instead of looping");
        fx.lay(&worse, "coverage", LCOV, "aaaa02", |_| {});

        // A rename with no edit: git's `-M` sees the move, and the coverage walk still reports the path
        // it had before — the ticket's rename-in-coverage case.
        fx.repo
            .copy("src/util/debug_helpers.rs", "src/util/trace.rs");
        fx.repo.remove("src/util/debug_helpers.rs");
        let renamed = fx.repo.commit("util: rename debug_helpers to trace");
        fx.lay(&renamed, "coverage", LCOV, "aaaa03", |_| {});

        fx.repo.write("src/parser/mod.rs", "pub mod tokenizer;\n\npub fn parse(input: &str) -> Vec<&str> {\n    input.lines().filter(|line| !line.trim().is_empty()).collect()\n}\n");
        let bench_only = fx.repo.commit("bench: the hyperfine job ran here");
        fx.lay(&bench_only, "benchmark", BENCH, "bbbb01", |_| {});

        fx.repo
            .write("tools/count.sh", "#!/bin/sh\ngrep -c . \"$1\"\n");
        let summary_only = fx
            .repo
            .commit("coverage: the node job reports a summary only");
        fx.lay(&summary_only, "coverage", ISTANBUL, "cccc01", |_| {});

        fx.repo.write("tools/scraped.sh", "#!/bin/sh\necho done\n");
        let scrape_only = fx
            .repo
            .commit("coverage: an ad-hoc job that only prints a percentage");
        fx.lay(&scrape_only, "coverage", LOG_SCRAPE, "dddd01", |_| {});

        Self {
            fx,
            baseline,
            worse,
            renamed,
            bench_only,
            summary_only,
            scrape_only,
        }
    }
}

fn tree() -> (tempfile::TempDir, Tree) {
    let scratch = tempfile::tempdir().expect("scratch");
    let tree = Tree::build(scratch.path());
    tree.fx.index();
    (scratch, tree)
}

// --------------------------------------------------------------------------- a normal commit

/// The shape a workspace in use has: four measured files, one of which fell since its parent.
///
/// Everything the coverage pane prints is checked against the record file the run wrote — the counts
/// per path, the pooled total, the run id and its `run_status` — because that is what "the pane is a
/// view over the index" means in practice. The order is the ticket's default: worst first.
#[test]
fn a_normal_commits_rows_are_the_records_it_ran_and_the_parent_it_had() {
    let (_scratch, tree) = tree();
    let snapshot = tree.fx.detail(&tree.worse).expect("the commit is indexed");

    // The pooled number is the totals' pooled arithmetic, not a mean of per-file rates, and it is the
    // number the timeline row shows too — `docs/evidence-format.md`'s worked example, 3581/4120.
    let (hit, found) = total_record(LCOV);
    assert_eq!(
        (hit, found),
        (3_581, 4_120),
        "the golden is the worked example"
    );
    assert_eq!(snapshot.pooled.percent, rate(hit, found));
    assert_eq!(snapshot.pooled.percent, Some(86.9));
    assert_eq!((snapshot.pooled.hit, snapshot.pooled.found), (hit, found));
    assert_eq!(snapshot.pooled.measure, "lines");
    assert_eq!(snapshot.pooled.runs, vec![run_id(LCOV, "aaaa02")]);

    // One row per `scope:"file"` record, with the record's own counts.
    let want = file_records(LCOV);
    assert_eq!(snapshot.files.len(), want.len(), "one row per file record");
    for (path, hit, found) in &want {
        let row = snapshot
            .files
            .iter()
            .find(|file| &file.path == path)
            .unwrap_or_else(|| panic!("{path} is listed"));
        assert_eq!((row.hit, row.found), (*hit, *found), "{path}'s counts");
        assert_eq!(
            row.percent,
            rate(*hit, *found),
            "{path}'s rate is its own counts' rate, and a file with 0 of 88 lines is a measured 0.0 %, \
             not an unknown"
        );
        assert_eq!(
            row.runs,
            vec![run_id(LCOV, "aaaa02")],
            "{path}'s audit trail"
        );
    }

    // Worst first, on rates the records actually contain.
    let view = tree.fx.view(&tree.worse);
    let order = view.order(Pane::Coverage);
    let paths: Vec<&str> = order
        .iter()
        .map(|index| snapshot.files[*index].path.as_str())
        .collect();
    assert_eq!(
        paths.first().copied(),
        Some("src/util/debug_helpers.rs"),
        "0.0 % is the worst rate here: {paths:?}"
    );
    assert_eq!(
        paths.last().copied(),
        Some("src/gen/protobuf_out.rs"),
        "100.0 % is the best: {paths:?}"
    );

    // The file this commit changed fell against its parent, and the delta says by how much. Every other
    // row is a file the run measured that this commit never touched.
    let fell = snapshot
        .files
        .iter()
        .find(|file| file.path == "src/parser/tokenizer.rs")
        .expect("the changed file is measured");
    assert!(fell.touched, "it is one of the commit's own files");
    assert_eq!(fell.parent, Some(100.0), "at the parent every line was hit");
    let delta = fell.delta.expect("two rates make a delta");
    assert!(
        delta < -15.0 && delta > -17.0,
        "2199/2617 against 100 % is a fall of about 16 points, not {delta}"
    );
    for file in &snapshot.files {
        assert_eq!(
            file.touched,
            file.path == "src/parser/tokenizer.rs",
            "only the touched file carries no mark"
        );
    }

    // And the diffstat pane is the commit's own file list, which is the only reason the two panes' path
    // columns mean one thing.
    assert_eq!(
        snapshot
            .diff
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<&str>>(),
        vec!["src/parser/tokenizer.rs"],
        "one file changed, and the pane says which"
    );
    assert_eq!(snapshot.parent.as_deref(), Some(tree.baseline.as_str()));
}

/// The footer is the audit trail, so it is pinned over records no test wrote: run id, its
/// `run_status`, the counts it pooled, and whether the file records account for the total.
#[test]
fn the_coverage_footer_names_the_run_that_produced_the_number() {
    let (_scratch, tree) = tree();
    let view = tree.fx.view(&tree.worse);
    let footer = view.footer(Pane::Coverage, &palette());
    let run = run_id(LCOV, "aaaa02");
    for want in [
        run.as_str(),
        "success",
        "3581/4120",
        "the file records account for all 4120 of the total's lines",
    ] {
        assert!(footer.contains(want), "footer is missing {want}: {footer}");
    }
    // The lcov run published its own `scope:"total"` record, so the footer does not claim the reader
    // added the rows up — `summed` is a claim about the derivation, and here it would be false.
    assert!(
        !footer.contains("summed"),
        "the run's own total was used: {footer}"
    );
    // And the sentence reaches the drawn frame, because a footer only in the data is a footer nobody
    // reads.
    let screen = tree.fx.screen(&tree.worse);
    for want in ["coverage by file", "86.9% lines", run.as_str(), "success"] {
        assert!(screen.contains(want), "screen is missing {want}:\n{screen}");
    }
}

/// The half of the MI pane that `i` turns on: the cached analysis is read, so "why does this score 41"
/// is answered without a re-analysis, and the footer names the analyzer that produced it.
#[test]
fn the_hotspots_pane_is_the_cached_analysis_with_its_inputs_and_its_stamp() {
    let (_scratch, tree) = tree();
    let snapshot = tree
        .fx
        .detail(&tree.baseline)
        .expect("the commit is indexed");
    assert_eq!(
        snapshot.mi.len(),
        RUST_FILES.len(),
        "every file this commit added is scored"
    );
    for (path, _body) in RUST_FILES {
        let row = snapshot
            .mi
            .iter()
            .find(|file| file.path == path)
            .unwrap_or_else(|| panic!("{path} has a hotspot row"));
        assert_eq!(row.status, "ok", "{path} was analysed");
        assert!(row.mi.is_some(), "{path} has a score, not a gap");
        assert!(row.loc.unwrap_or(0) > 0, "{path} was weighed by its LOC");
        assert!(
            row.delta.is_none(),
            "the root commit has no parent to have got worse against"
        );
    }
    let mut view = tree.fx.view(&tree.baseline);
    assert_eq!(view.columns(Pane::Hotspots)[2], "LOC");
    view.toggle_inputs();
    assert_eq!(view.columns(Pane::Hotspots)[2], "V/G/LOC");
    let frame = view.frame(Pane::Hotspots, 96, &palette());
    let row = frame
        .rows
        .iter()
        .find(|row| row.cells[0].contains("tokenizer"))
        .expect("the row is still there");
    assert!(
        row.cells[2].trim_start().starts_with("V=")
            && row.cells[2].contains("G=")
            && row.cells[2].contains("LOC=")
            && !row.cells[2].contains("V=?"),
        "the inputs came from the cached block: {:?}",
        row.cells[2]
    );

    let footer = view.footer(Pane::Hotspots, &palette());
    // The stamp line is the analyzer identity as the store spells it, and its versions come from the
    // definition rather than a literal here: a bump must move the footer, not break this test.
    let stamp = fathom::mi::Definition::embedded().stamp();
    let stamp_line = format!(
        "analyzers mi.aggregate v{v},mi.score v{v}",
        v = stamp.version
    );
    assert_eq!(stamp.score, "mi.score", "the footer names the scorer's id");
    for want in [stamp_line.as_str(), "aggregate", "4 file(s)", "tree "] {
        assert!(footer.contains(want), "footer is missing {want}: {footer}");
    }
    assert!(
        !footer.contains("were not analysed"),
        "nothing in this tree went unanalysed, so nothing is claimed: {footer}"
    );
    // And because it is the root commit, the diffstat's footer says what it diffed against when there
    // was no parent — the empty tree, which git calls a baseline and a reader needs called out.
    let diffstat = view.footer(Pane::Diffstat, &palette());
    assert!(
        diffstat.contains("the empty tree (root commit)"),
        "the baseline is named: {diffstat}"
    );
}

// --------------------------------------------------------------------------- the shapes with no number

/// A benchmark ran and no coverage job did: the coverage pane has nothing, and says which gap code that
/// is — while the two panes that do not need evidence keep answering.
#[test]
fn a_commit_with_no_coverage_records_says_which_gap_it_is_and_keeps_answering() {
    let (_scratch, tree) = tree();
    let snapshot = tree
        .fx
        .detail(&tree.bench_only)
        .expect("the commit is indexed");
    assert!(
        snapshot.files.is_empty(),
        "no coverage record, so no row: {:?}",
        snapshot.files
    );
    assert!(snapshot.pooled.percent.is_none());
    let gap = snapshot.pooled.gap.clone().expect("and a code for why");
    let view = tree.fx.view(&tree.bench_only);
    let text: String = view
        .frame(Pane::Coverage, 96, &palette())
        .rows
        .iter()
        .map(|row| row.cells[0].as_str())
        .collect::<Vec<&str>>()
        .join(" ");
    assert!(
        text.contains(&gap),
        "the code is printed, not swallowed: {text}"
    );
    assert!(
        text.contains("run_workflow") || text.contains("act-mcp serve"),
        "and the next command is named with it: {text}"
    );
    // And the screen as a whole is not blank: the diffstat came from `git` and the MI from the cache,
    // and neither of them is waiting on a coverage run.
    assert_eq!(
        snapshot.diff.len(),
        1,
        "the commit still changed a file, and the diffstat pane says so"
    );
    assert_eq!(snapshot.mi.len(), 1, "and that file is still analysable");
    let screen = tree.fx.screen(&tree.bench_only);
    for want in [
        "coverage by file",
        "MI hotspots",
        "diffstat",
        "src/parser/mod.rs",
    ] {
        assert!(screen.contains(want), "screen is missing {want}:\n{screen}");
    }
}

/// The contract's most misread fact: a run's total counts files no file record names, so the total is
/// *larger* than the sum of the rows under it, and the pane says so in the same breath as the number.
///
/// The istanbul golden is exactly this shape — 2210 lines in its total, 677 across its two file
/// records — so this is not a fixture built to make the caveat fire.
#[test]
fn a_total_larger_than_the_sum_of_its_file_records_is_explained_not_hidden() {
    let (_scratch, tree) = tree();
    let snapshot = tree
        .fx
        .detail(&tree.summary_only)
        .expect("the commit is indexed");
    let want = file_records(ISTANBUL);
    let sum_found: u64 = want.iter().map(|(_path, _hit, found)| found).sum();
    let (hit, found) = total_record(ISTANBUL);
    assert_eq!((snapshot.pooled.hit, snapshot.pooled.found), (hit, found));
    assert!(
        sum_found < found,
        "the golden must be the disagreeing shape: {sum_found} vs {found}"
    );
    assert_eq!(
        snapshot.files.iter().map(|file| file.found).sum::<u64>(),
        sum_found,
        "the rows add up to the file records, not to the total"
    );
    let note = tree
        .fx
        .view(&tree.summary_only)
        .footer(Pane::Coverage, &palette());
    assert!(
        note.contains(&format!(
            "{rest} measured line(s)",
            rest = found - sum_found
        )) && note.contains("outside this workspace")
            && note.contains("larger than the sum of the records"),
        "the disagreement is stated in the pane's own words: {note}"
    );
    // The istanbul run also reported statements, branches and functions. They are printed and never
    // pooled with `lines`, because they are different units (ADR-0008 §3.5).
    assert!(
        !snapshot.pooled.secondary.is_empty(),
        "the secondary measures survive to the footer"
    );
    assert!(
        note.contains("not pooled with this measure"),
        "and say so: {note}"
    );
}

/// A scraped percentage is a caveat, not a measurement (ADR-0008 §3.6). The pane refuses it out loud
/// and still names the run it refused.
#[test]
fn a_percent_only_run_is_refused_and_still_named() {
    let (_scratch, tree) = tree();
    let snapshot = tree
        .fx
        .detail(&tree.scrape_only)
        .expect("the commit is indexed");
    assert!(
        snapshot.pooled.percent.is_none(),
        "87.4 % scraped from a log line is never a number here"
    );
    assert_eq!(snapshot.pooled.gap.as_deref(), Some("only_percent_only"));
    let view = tree.fx.view(&tree.scrape_only);
    let text: String = view
        .frame(Pane::Coverage, 96, &palette())
        .rows
        .iter()
        .map(|row| row.cells[0].as_str())
        .collect::<Vec<&str>>()
        .join(" ");
    assert!(
        text.contains("scraped percentage"),
        "the refusal explains itself: {text}"
    );
    let footer = view.footer(Pane::Coverage, &palette());
    // Nothing fed the number, and the footer says that rather than leaving the run bill blank: the
    // scrape is a caveat on the row (`percent_only`, `low_confidence`) and not a run this build will
    // pool, and the two facts together are the answer to "why is there no percentage here".
    assert!(
        footer.contains("no run fed a coverage number here"),
        "{footer}"
    );
    assert!(
        footer.contains("percent_only") && footer.contains("low_confidence"),
        "the run's own flags are on the line: {footer}"
    );
}

/// A commit whose files this build cannot analyse is a pane of reasons, never a pane of dashes.
///
/// `languages.enabled` is the honest lever: python is in the model, and this config turns it off,
/// which is the same rung (`unsupported_language`) a language with no lexer at all lands on.
#[test]
fn a_commit_of_files_this_build_cannot_analyse_says_why_for_each_one() {
    let scratch = tempfile::tempdir().expect("scratch");
    let fx = Fixture::new(scratch.path(), "unsupported");
    fx.repo.write(
        "tools/report.py",
        "def report(rows):\n    return [row for row in rows if row]\n",
    );
    fx.repo.write(
        "tools/paths.py",
        "def trim(text):\n    return text.strip()\n",
    );
    let sha = fx
        .repo
        .commit("tools: the two scripts no front end will read");
    fx.index_with(Config {
        languages: Languages {
            enabled: vec!["rust".to_owned()],
        },
        ..Config::default()
    });

    let snapshot = fx.detail(&sha).expect("the commit is indexed");
    assert_eq!(
        snapshot.mi.len(),
        2,
        "both files are listed, neither is dropped"
    );
    for row in &snapshot.mi {
        assert!(row.mi.is_none(), "{} has no score to print", row.path);
        assert_eq!(
            row.status, "unsupported_language",
            "{} is a counted gap, not an accident",
            row.path
        );
    }
    let view = Detail::read(snapshot);
    let frame = view.frame(Pane::Hotspots, 96, &palette());
    let reasons: Vec<&str> = frame.rows.iter().map(|row| row.cells[2].as_str()).collect();
    assert!(
        reasons.iter().all(|reason| reason.contains("not analysed")),
        "every row says why where its score would be: {reasons:?}"
    );
    assert!(
        reasons.iter().any(|reason| reason.contains("python")),
        "and names the language the reader could go and enable: {reasons:?}"
    );
    // The pane is not silently incomplete: the row count is the diff's, the footer counts the gaps, and
    // an aggregate of nothing is `—` rather than a mean of zero files.
    let footer = view.footer(Pane::Hotspots, &palette());
    assert!(
        footer.contains("2 path(s) in the tree were not analysed")
            || footer.contains("this tree has no aggregate"),
        "the footer carries the incompleteness: {footer}"
    );
}

/// A rename is the join the ticket names by name: `git` names the new path, the coverage walk still
/// names the old one, and both panes must still be about one file.
#[test]
fn a_renamed_file_matches_the_record_that_named_its_old_path() {
    let (_scratch, tree) = tree();
    let snapshot = tree
        .fx
        .detail(&tree.renamed)
        .expect("the commit is indexed");
    let row = snapshot
        .diff
        .iter()
        .find(|file| file.path == "src/util/trace.rs")
        .expect("the rename's new path is in the diffstat");
    assert_eq!(
        row.old_path.as_deref(),
        Some("src/util/debug_helpers.rs"),
        "git found the move, and the pane keeps both names"
    );
    assert_eq!(row.kind, "rename");
    // The coverage row still carries the name the record used — and it is marked as one of this
    // commit's files, which is only true because the rename is followed.
    let coverage = snapshot
        .files
        .iter()
        .find(|file| file.path == "src/util/debug_helpers.rs")
        .expect("the record names the old path");
    assert!(
        coverage.touched,
        "the file under its old name is this commit's own file"
    );
    // And the shared selection crosses the rename: point at the new path, and the pane that knows only
    // the old one highlights it instead of claiming it has no record for it.
    let mut view = tree.fx.view(&tree.renamed);
    view.focus_pane(Pane::Diffstat);
    view.jump(true);
    assert_eq!(view.path().as_deref(), Some("src/util/trace.rs"));
    let frame = view.frame(Pane::Coverage, 96, &palette());
    let marked: Vec<&str> = frame
        .rows
        .iter()
        .filter(|row| row.marked)
        .map(|row| row.cells[0].as_str())
        .collect();
    assert!(
        marked.iter().any(|cell| cell.contains("debug_helpers")),
        "the highlight follows the file across its rename: {marked:?}"
    );
    assert!(
        !frame
            .rows
            .iter()
            .any(|row| row.cells[0].contains("no coverage record")),
        "and the pane does not also say it is missing: {:?}",
        frame
            .rows
            .iter()
            .map(|row| row.cells[0].as_str())
            .collect::<Vec<&str>>()
    );
}

/// Two runs, one path: the file row pools their *counts* and derives the rate once, exactly like the
/// pooled headline one level up (ADR-0008 §3.1 applied per file). A mean of the two runs' percentages
/// is a different number and would be a second derivation, which is what the model refuses.
#[test]
fn two_runs_that_measured_one_file_pool_its_counts_and_name_both_runs() {
    let scratch = tempfile::tempdir().expect("scratch");
    let fx = Fixture::new(scratch.path(), "two-runs");
    fx.repo
        .write("lib/index.js", "export function main() {\n  return 1;\n}\n");
    let sha = fx
        .repo
        .commit("web: two coverage jobs measured the same file");
    // The istanbul run as-is (`lib/index.js`, 601/640), plus the lcov run with every one of its paths
    // rewritten onto that same file, so the two runs report one path with different counts.
    fx.lay(&sha, "coverage", ISTANBUL, "cccc02", |_| {});
    fx.lay(&sha, "coverage", LCOV, "eeee02", |record| {
        if record["scope"].as_str() == Some("file") {
            record["path"] = Value::String("lib/index.js".to_owned());
        }
    });
    fx.index();

    let snapshot = fx.detail(&sha).expect("the commit is indexed");
    let row = snapshot
        .files
        .iter()
        .find(|file| file.path == "lib/index.js")
        .expect("the shared path is one row, not two");
    let (lcov_hit, lcov_found) = (
        file_records(LCOV)
            .iter()
            .map(|(_p, hit, _f)| hit)
            .sum::<u64>(),
        file_records(LCOV)
            .iter()
            .map(|(_p, _h, found)| found)
            .sum::<u64>(),
    );
    let istanbul = file_records(ISTANBUL)
        .into_iter()
        .find(|(path, _hit, _found)| path == "lib/index.js")
        .expect("the istanbul golden names lib/index.js");
    assert_eq!(
        (row.hit, row.found),
        (lcov_hit + istanbul.1, lcov_found + istanbul.2),
        "the counts are the two runs' sums"
    );
    assert_eq!(
        row.percent,
        rate(row.hit, row.found),
        "and the rate is derived once from those sums"
    );
    assert_eq!(
        row.runs,
        vec![run_id(LCOV, "eeee02"), run_id(ISTANBUL, "cccc02")],
        "both runs are named on the row that pools them"
    );
}

// --------------------------------------------------------------------------- what the view may not do

/// The ticket's "read from the index, not recomputed per keypress", tested as a fact about the world:
/// delete the evidence tree and the answer does not change one field.
///
/// This is the only test in the file that could fail for the honest reason — if a future detail view
/// walked `.evidence` to fill a column, every assertion below would go red the moment the tree did.
#[test]
fn the_snapshot_survives_its_evidence_being_deleted() {
    let (_scratch, tree) = tree();
    let before = tree.fx.detail(&tree.worse).expect("the commit is indexed");
    std::fs::remove_dir_all(&tree.fx.evidence).expect("the evidence root is the fixture's own");
    let after = tree
        .fx
        .detail(&tree.worse)
        .expect("the store still answers");
    assert_eq!(
        after.pooled, before.pooled,
        "the pooled number did not need the tree"
    );
    assert_eq!(after.files, before.files, "nor did the per-file rows");
    assert_eq!(after.mi, before.mi, "nor did the hotspots");
    assert_eq!(after.filter, before.filter, "nor did the diffstat");
}

/// An empty evidence root is not an empty screen: two of the three panes answer from `git` and the
/// cache, and the third says what to run.
#[test]
fn an_empty_evidence_root_is_a_screen_with_a_next_step_in_it() {
    let scratch = tempfile::tempdir().expect("scratch");
    let fx = Fixture::new(scratch.path(), "no-evidence");
    fx.repo.write(
        "src/lib.rs",
        "pub fn a() -> u32 {\n    let b = 1;\n    b + 1\n}\n",
    );
    let sha = fx
        .repo
        .commit("library: the only commit, and nothing ever ran here");
    fx.index();

    let snapshot = fx.detail(&sha).expect("the commit is indexed");
    assert_eq!(snapshot.pooled.gap.as_deref(), Some("no_evidence"));
    assert!(snapshot.files.is_empty() && snapshot.runs.is_empty());
    assert_eq!(
        snapshot.diff.len(),
        1,
        "the diffstat pane is `git`'s answer and needs no evidence at all"
    );
    assert_eq!(
        snapshot.mi.len(),
        1,
        "neither does the MI pane, which reads the cached analysis"
    );
    assert!(
        snapshot.mi[0].mi.is_some(),
        "and it is a real score: {:?}",
        snapshot.mi[0]
    );

    let mut app = App::new(fx.reading(), FIXED_EPOCH_SECONDS);
    app.set_size(44, 200);
    app.show_detail(snapshot);
    let screen = draw(&app);
    for want in [
        "no_evidence",
        "run_workflow",
        "coverage by file",
        "MI hotspots",
        "diffstat",
        "src/lib.rs",
    ] {
        assert!(screen.contains(want), "screen is missing {want}:\n{screen}");
    }
}

/// A SHA the store has never heard of is a refusal with a fix, drawn as a frame — not a hang, not a
/// blank pane, not a panic.
#[test]
fn a_commit_that_is_not_in_the_store_is_a_refusal_with_a_fix() {
    let (_scratch, tree) = tree();
    let absent = "000000000000000000000000000000000000abcd";
    let reason = tree
        .fx
        .detail(absent)
        .expect_err("an unindexed commit cannot be read");
    for want in ["0000000", "no row in this store", "press R", "--from/--to"] {
        assert!(reason.contains(want), "refusal is missing {want}: {reason}");
    }
    // And the view that refusal produces still draws: the message, and the keys that do something
    // about it.
    let view = Detail::refusal(absent, reason.clone());
    let notice = view.notice().join(" ");
    assert!(notice.contains(&reason), "{notice}");
    assert!(
        notice.contains("r re-reads") && notice.contains("R indexes"),
        "{notice}"
    );
    let text = plain::detail(&view, &palette(), plain::WIDTH);
    assert!(text.contains("no row in this store"), "{text}");

    // And the other refusal this view can make: a workspace with no store at all, which names the
    // command that creates one because pressing `R` in a screen that has no rows yet is not a fix.
    let scratch = tempfile::tempdir().expect("scratch");
    let bare = Fixture::new(scratch.path(), "no-store");
    bare.repo
        .write("src/lib.rs", "pub fn a() -> u32 {\n    1\n}\n");
    let sha = bare.repo.commit("library: never indexed");
    let reason = bare.detail(&sha).expect_err("there is no store to read");
    for want in ["nothing is indexed", "fathom index"] {
        assert!(reason.contains(want), "refusal is missing {want}: {reason}");
    }
}

/// `--plain --detail REV` prints the same three panes, every row, with the same footers — the pipe's
/// version of the screen, painted by the same code the screen paints.
#[test]
fn plain_mode_prints_the_same_three_panes_with_the_run_bill() {
    let (_scratch, tree) = tree();
    let text = plain::detail(&tree.fx.view(&tree.worse), &palette(), plain::WIDTH);
    for want in [
        "coverage by file",
        "MI hotspots",
        "diffstat",
        "86.9% lines",
        "src/parser/tokenizer.rs",
        "src/gen/protobuf_out.rs",
        &run_id(LCOV, "aaaa02"),
        "success",
    ] {
        assert!(
            text.contains(want),
            "plain output is missing {want}:\n{text}"
        );
    }
    // Every row, not the drawn window: the four coverage records, and the untouched ones carry their
    // mark.
    for (path, _hit, _found) in file_records(LCOV) {
        assert!(text.contains(&path), "{path} is printed: {text}");
    }
}
