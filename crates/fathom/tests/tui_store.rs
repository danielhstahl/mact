//! The store → screen path, end to end: real `git`, a real `fathom index`, and the rows that come back
//! out of `.fathom` as the timeline draws them.
//!
//! Everything else in this crate's TUI tests is synthetic on purpose — a snapshot needs a fixture it
//! controls. This file is the opposite: it proves the two halves are joined, that a row's numbers are
//! the numbers `git` itself reports (`Repo::numstat_oracle`), and that the degenerate states the ticket
//! lists (single commit, nothing indexed, the checkout ahead of the store) come out of the real reader
//! rather than only out of a mock.
//!
//! The clock is the fixture's, not the machine's: [`FIXED_EPOCH_SECONDS`] is when every fixture commit
//! is stamped, so the `age` column is a constant and a snapshot does not go stale overnight.

mod common;

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use clap::Parser;

use common::{canonical, Repo, FIXED_EPOCH_SECONDS};
use fathom::boot::{Access, Boot};
use fathom::cli::Cli;
use fathom::config::Config;
use fathom::history::Query;
use fathom::metrics::vocab::ActionState;
use fathom::store::{Indexer, Runbook, Store};
use fathom::tui::engine::{StoreWork, Work, WorkError};
use fathom::tui::palette::Palette;
use fathom::tui::plain;
use fathom::tui::state::App;
use fathom::tui::store_view::{self, is_stale, StoreState};

/// A workspace, its cache root, and the argv `fathom` would have resolved them from.
struct Fixture {
    repo: Repo,
    cache: PathBuf,
}

impl Fixture {
    fn new(dir: &Path, name: &str) -> Self {
        let root = dir.join(name);
        let repo = Repo::init(&root.join("workspace"));
        Self {
            repo,
            cache: root.join("cache"),
        }
    }

    /// The same `Boot` `fathom index` and `fathom tui` would build, flags and probe included.
    fn boot(&self) -> Boot {
        let cli = Cli::parse_from([
            "fathom",
            "--cache-dir",
            &self.cache.display().to_string(),
            &self.repo.path().display().to_string(),
        ]);
        // `Write` because the fixture's own `index` runs against the same boot, and a cache root that
        // was never created is a lock file with nowhere to live. The screen's own boot is a reader; what
        // this file is testing is the rows, not the probe.
        Boot::validate(&canonical(self.repo.path()), &cli.common, Access::Write)
            .expect("a fixture workspace boots")
    }

    /// Run the index the way `fathom index` does.
    fn index(&self) {
        let boot = self.boot();
        let boot: &Boot = Box::leak(Box::new(boot));
        Indexer::new(boot, Config::default(), Query::new(), "manual")
            .expect("the default config compiles")
            .run()
            .expect("the index runs");
    }

    /// The timeline as the reader sees it, with the screen's own clock.
    fn read(&self) -> store_view::Reading {
        store_view::read(&self.boot(), None)
    }
}

/// Three commits, each touching a different file, so the churn column has three different answers.
fn three_commits(repo: &Repo) -> Vec<String> {
    repo.write("src/lib.rs", "pub fn a() -> u32 { 1 }\n");
    let first = repo.commit("library: the first function");
    repo.write(
        "src/lib.rs",
        "pub fn a() -> u32 {\n    let b = 2;\n    let c = 3;\n    b + c\n}\n",
    );
    repo.write("src/other.rs", "pub fn b() {}\n");
    let second = repo.commit("library: two more bindings and a second file");
    repo.write("README.md", "# act-mcp\n\nA timeline of commits.\n");
    let third = repo.commit("docs: name the project");
    vec![first, second, third]
}

/// `--limit` cuts the screen, not the store, and says so.
///
/// The distinction is the ticket's: a reader who sees two rows and a header that says "2 of 3" knows the
/// third is a click of `G` away, and a reader who sees two rows and "indexed 2 commits" has been told a
/// falsehood about their own repository.
#[test]
fn a_limit_cuts_the_rows_and_says_which_ones_it_hid() {
    let scratch = tempfile::tempdir().expect("scratch");
    let fixture = Fixture::new(scratch.path(), "limited");
    let shas = three_commits(&fixture.repo);
    fixture.index();

    let reading = store_view::read(&fixture.boot(), NonZeroUsize::new(2));
    assert_eq!(
        reading.store,
        StoreState::Present { rows: 3 },
        "the store's own count survives the cut"
    );
    assert_eq!(reading.entries.len(), 2, "the screen's does not");
    assert_eq!(
        reading.entries[0].sha, shas[2],
        "the newest commit is the first row"
    );
    assert_eq!(
        reading.entries[1].sha, shas[1],
        "and the limit keeps the newest N, not an arbitrary 2"
    );
    assert_eq!(reading.hidden, 1);

    let app = App::new(reading, FIXED_EPOCH_SECONDS);
    assert!(
        app.facts().contains("indexed 2 of 3 commits (--limit)"),
        "the header owns the cut: {}",
        app.facts()
    );
}

#[test]
fn an_indexed_repository_reads_back_as_rows_of_real_numbers() {
    let scratch = tempfile::tempdir().expect("scratch");
    let fixture = Fixture::new(scratch.path(), "populated");
    let shas = three_commits(&fixture.repo);
    fixture.index();

    let reading = fixture.read();
    assert_eq!(
        reading.branch, "main",
        "the header's branch is git's answer"
    );
    assert_eq!(reading.head.as_deref(), Some(shas[2].as_str()));
    assert_eq!(reading.store, StoreState::Present { rows: 3 });
    assert_eq!(reading.entries.len(), 3, "one row per commit, newest first");
    assert_eq!(reading.entries[0].sha, shas[2]);
    assert_eq!(reading.entries[0].subject, "docs: name the project");
    assert_eq!(reading.entries[2].sha, shas[0]);
    assert!(
        !is_stale(&reading),
        "HEAD has a row, so the store is not behind the checkout"
    );

    // The `+/-` column against git's own `--numstat`, for every row: the reader is allowed to add a
    // noise split on top, never to disagree about the totals.
    for entry in &reading.entries {
        let (add, del, files) = fixture.repo.numstat_oracle(&entry.sha);
        assert_eq!(entry.churn.add, Some(add), "{} add", entry.short());
        assert_eq!(entry.churn.del, Some(del), "{} del", entry.short());
        assert_eq!(
            entry.churn.files,
            Some(files as u64),
            "{} files",
            entry.short()
        );
        assert!(entry.indexed, "a row from the store is an indexed row");
    }

    // No `.evidence` in this fixture, so the evidence-backed columns are unknown and say why — and the
    // MI, which comes from the tree rather than from evidence, is a number.
    for entry in &reading.entries {
        assert!(
            !entry.coverage.is_known(),
            "no evidence, so no coverage: {:?}",
            entry.coverage
        );
        assert!(!entry.bench.is_known(), "and no benchmark group");
        assert_eq!(entry.action, ActionState::NoEvidence);
    }
    let library = &reading.entries[1];
    assert!(
        library.mi.is_known(),
        "a tree of two rust files is measurable: {:?}",
        library.mi
    );

    // The root commit has no parent, so it has no arrows anywhere (ADR-0008 §6.3), and its child does.
    assert!(
        reading.entries[2].parent.is_none(),
        "the root has no parent"
    );
    assert!(
        reading.entries[2].mi.delta.is_none(),
        "and therefore no MI arrow: {:?}",
        reading.entries[2].mi
    );
    assert_eq!(reading.entries[1].parent.as_deref(), Some(shas[0].as_str()));
    assert!(
        reading.entries[1].mi.delta.is_some(),
        "a measured parent buys the child an arrow"
    );
    assert!(
        reading.entries[0].mi.delta.is_some(),
        "the docs commit's tree differs from the library's: MI moved"
    );

    // …and the whole screen draws it, through the same column model the TUI uses.
    let table = plain::table(
        &App::new(reading, FIXED_EPOCH_SECONDS),
        &Palette::plain(),
        120,
    );
    assert!(table.contains("docs: name the project"), "{table}");
    assert!(table.contains("indexed 3 commits"), "{table}");
}

/// The ticket's "cache stale": the checkout moved and the store did not, and the header says so rather
/// than showing an old timeline as a current one.
#[test]
fn a_checkout_ahead_of_the_store_is_marked_stale() {
    let scratch = tempfile::tempdir().expect("scratch");
    let fixture = Fixture::new(scratch.path(), "stale");
    three_commits(&fixture.repo);
    fixture.index();
    assert!(!is_stale(&fixture.read()));

    fixture.repo.write("src/new.rs", "pub fn new() {}\n");
    let newest = fixture.repo.commit("library: one more file");
    let reading = fixture.read();
    assert_eq!(reading.head.as_deref(), Some(newest.as_str()));
    assert!(is_stale(&reading), "HEAD has no row");
    assert_eq!(
        reading.store,
        StoreState::Present { rows: 3 },
        "the store did not vanish"
    );

    let app = App::new(reading, FIXED_EPOCH_SECONDS);
    assert!(
        app.facts().contains("stale"),
        "the header says the picture is behind: {}",
        app.facts()
    );
    // The rows it does have are still on screen: stale is a warning, not an empty table.
    assert_eq!(app.visible().len(), 3);
}

/// A repository with one commit is neither an empty repo nor a normal one: it has no parent, so every
/// delta column is blank by rule, and the screen must not invent one.
#[test]
fn a_single_commit_repository_is_a_row_of_its_own() {
    let scratch = tempfile::tempdir().expect("scratch");
    let fixture = Fixture::new(scratch.path(), "single");
    fixture.repo.write("README.md", "# one\n");
    let only = fixture.repo.commit("the first commit");
    fixture.index();

    let reading = fixture.read();
    assert_eq!(reading.entries.len(), 1);
    assert_eq!(reading.entries[0].sha, only);
    assert!(reading.entries[0].parent.is_none());
    assert!(reading.entries[0].mi.delta.is_none());
    let table = plain::table(
        &App::new(reading, FIXED_EPOCH_SECONDS),
        &Palette::plain(),
        120,
    );
    assert!(table.contains("the first commit"), "{table}");
    assert!(
        !table.contains("no commits"),
        "one commit is not none: {table}"
    );
}

/// Nothing has ever been indexed: the store is absent, and the screen's answer is the key to press.
#[test]
fn a_workspace_that_has_never_been_indexed_says_press_r() {
    let scratch = tempfile::tempdir().expect("scratch");
    let fixture = Fixture::new(scratch.path(), "unindexed");
    three_commits(&fixture.repo);

    let reading = fixture.read();
    assert_eq!(reading.store, StoreState::Absent);
    assert!(reading.entries.is_empty());
    assert!(
        !is_stale(&reading),
        "with no rows at all, staleness is not the story"
    );
    let app = App::new(reading, FIXED_EPOCH_SECONDS);
    let step = app.next_step().join("\n");
    assert!(step.contains("press R"), "{step}");
    assert!(step.contains("no evidence root"), "{step}");
}

/// The `Esc` the TUI raises is the store layer's cancellation handle, and it behaves the way §10.2 says
/// anything that stops early must: the rows already written are whole, and the run reports itself.
#[test]
fn a_cancelled_index_leaves_a_usable_store_and_says_so() {
    let scratch = tempfile::tempdir().expect("scratch");
    let fixture = Fixture::new(scratch.path(), "cancelled");
    three_commits(&fixture.repo);
    fixture.index();
    let rows_before = fixture.read().store;

    let cancel = Arc::new(AtomicBool::new(true));
    let work = StoreWork::new(
        fixture.boot(),
        Config::default(),
        Query::new(),
        None,
        fathom::diff::Limits::default(),
    );
    let outcome = work.index(Arc::clone(&cancel));
    let Err(WorkError::Cancelled) = outcome else {
        panic!("a flag raised before the run started must cancel it, not be ignored: {outcome:?}");
    };

    // The store is not merely still there: the *previous* run's rows are intact, and the cancelled run
    // published its counters with `interrupted` set, which is what `--status` prints.
    assert_eq!(fixture.read().store, rows_before);
    let store = Store::open(&fixture.cache)
        .expect("the store opens")
        .expect("a store was written by the first index");
    let book = Runbook::read(&store.index).expect("the cancelled run published run.json");
    assert_eq!(book.last.trigger, "tui", "the TUI's own trigger spelling");
    assert_eq!(book.last.status, "error");
    assert!(
        book.last
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("cancel"),
        "and named the reason: {:?}",
        book.last.error
    );
    assert!(
        cancel.load(Ordering::Relaxed),
        "the flag is the caller's, untouched"
    );
}

/// The header's `branch@HEAD` is asked of `git` once per full read; the polls that fill an index run do
/// not ask again, and must not lose what was already known. A `main@9f31a2c` that turns into `(none)@—`
/// for the length of a re-index is the screen stating something it has no evidence for.
#[test]
fn a_reindex_never_loses_the_branch_and_head_in_the_header() {
    let scratch = tempfile::tempdir().expect("scratch");
    let fixture = Fixture::new(scratch.path(), "identity");
    three_commits(&fixture.repo);
    let head = fixture.repo.head();
    let work = StoreWork::new(
        fixture.boot(),
        Config::default(),
        Query::new(),
        None,
        fathom::diff::Limits::default(),
    );

    // The order the screen does it in: a full read, then the polls an index run publishes.
    let full = work.read();
    assert_eq!(full.branch, "main", "the fixture's own branch: {full:?}");
    assert_eq!(full.head.as_deref(), Some(head.as_str()));
    let polled = work.poll().expect("the engine can poll");
    assert_eq!(polled.branch, full.branch, "a poll borrows the branch");
    assert_eq!(polled.head, full.head, "and the head");

    // A full read *after* a poll is the identity again — the ordering that used to poison it.
    let again = work.read();
    assert_eq!(again.branch, "main", "a poll may not overwrite the truth");
    assert_eq!(again.head.as_deref(), Some(head.as_str()));

    // And the cold start the seed exists for: the screen's first read happens before the engine has
    // produced one of its own, so a poll before that would otherwise say `(none)@—`.
    let cold = StoreWork::new(
        fixture.boot(),
        Config::default(),
        Query::new(),
        None,
        fathom::diff::Limits::default(),
    )
    .with_identity(&full);
    let first_poll = cold.poll().expect("the engine can poll");
    assert_eq!(
        first_poll.branch, "main",
        "a seeded poll keeps the header it was seeded with"
    );
    assert_eq!(first_poll.head.as_deref(), Some(head.as_str()));
}

/// The benchmark panel's read path, end to end: a record act-mcp wrote, the row `fathom index` derived
/// from it, the `Entry` the store hands the screen, and the panel built on top — with no fixture in
/// between to disagree with the real row shape.
///
/// This is the test that says the panel is a *view*. Everything in `tests/tui_bench.rs` builds its commit
/// from JSON it wrote itself, which is the right way to test a parser and the wrong way to prove the
/// parser is pointed at the right keys. Here the JSON comes from the reader, through the ledger, and out
/// again, so a panel that reads `benchmarks[].median` instead of `benchmarks[].value`, or forgets that
/// the failed run is not a row, fails here as well as there.
#[test]
fn a_benchmark_record_reaches_the_panel_through_the_store() {
    let scratch = tempfile::tempdir().expect("scratch");
    let fixture = Fixture::new(scratch.path(), "bench-through-the-store");
    let shas = three_commits(&fixture.repo);
    let at = &shas[1];

    // One successful run with two samples of the same metric, and a later failed run whose number is
    // bigger than both. Retention is what makes the two of them comparable, so the panel's value must be
    // the successful run's median and the failed run must appear only as a named exclusion.
    let dir = fixture.repo.path().join(".evidence/benchmark").join(at);
    std::fs::create_dir_all(&dir).expect("the sha directory is the contract's");
    std::fs::write(
        dir.join("20261110T040000Z-aaaaaa.jsonl"),
        benchmark_record(at, "20261110T040000Z-aaaaaa", "success", 100.0, 1)
            + &benchmark_record(at, "20261110T040000Z-aaaaaa", "success", 200.0, 2),
    )
    .expect("write the successful run");
    std::fs::write(
        dir.join("20261110T050000Z-bbbbbb.jsonl"),
        benchmark_record(at, "20261110T050000Z-bbbbbb", "failed", 999.0, 1),
    )
    .expect("write the failed run");
    fixture.index();

    let reading = fixture.read();
    let entry = reading
        .entries
        .iter()
        .find(|entry| entry.sha == *at)
        .expect("the commit is on the timeline");
    assert_eq!(
        entry.detail.groups.len(),
        1,
        "one group, from the run that ended `success`"
    );
    let group = &entry.detail.groups[0];
    println!("EXCLUDED: {:?}", entry.detail.excluded);
    assert_eq!(group.id.name, "tokenizer_throughput");
    assert_eq!(group.id.unit.as_deref(), Some("ops/s"));
    assert_eq!(group.value.value, 150.0, "the median of the two samples");
    assert_eq!(group.samples, 2);
    assert_eq!((group.min.value, group.max.value), (100.0, 200.0));
    assert_eq!(group.run_id, "20261110T040000Z-aaaaaa");
    assert_eq!(
        entry.detail.excluded.len(),
        1,
        "the failed run is an identity with a reason, not a number"
    );

    // And the panel on top of it, in the shape the store gave: this commit's row as `at`, the older rows
    // below it, nothing else.
    let older: Vec<fathom::tui::bench::Commit> = reading
        .entries
        .iter()
        .skip_while(|entry| entry.sha != *at)
        .skip(1)
        .map(|entry| entry.detail.clone())
        .collect();
    let opened = fathom::tui::bench::Panel::open(
        &entry.detail,
        &older,
        &fathom::tui::bench::Evidence {
            present: reading.evidence_present,
            path: reading.evidence_path.clone(),
        },
        5.0,
    );
    assert_eq!(opened.lines.len(), 1);
    let line = opened.lines.first().expect("the row");
    assert_eq!(line.value_text(&Palette::interactive(), 14), "150 n=2");
    assert_eq!(
        line.verdict,
        Some(fathom::tui::bench::Verdict::NoBaseline),
        "no earlier commit in this window has the metric, and the panel says so rather than drawing a \
         flat line"
    );
}

/// One benchmark record in the ADR-0002 shape, with the run's identity, status and sample index varied.
///
/// The `sample` field is not decoration: ADR-0008 §4.1's duplicate rule keys a measurement on
/// `(run, metric identity, sample)`, so two records without it are one measurement published twice and
/// the second is dropped — which is exactly how a fixture silently ends up testing a one-sample run.
fn benchmark_record(sha: &str, run_id: &str, status: &str, value: f64, sample: u64) -> String {
    let record = serde_json::json!({
        "schema_version": 1,
        "kind": "benchmark",
        "run_id": run_id,
        "commit": sha,
        "workflow": "benchmark",
        "task": "hyperfine-bench",
        "category": "benchmark",
        "run_status": status,
        "tool": { "name": "act-mcp", "version": "0.1.0", "act_version": "0.2.68" },
        "started_at": "2026-11-10T04:00:00.000Z",
        "duration_ms": 61_000,
        "exit_code": if status == "success" { 0 } else { 1 },
        "host": "build-box-07",
        "name": "tokenizer_throughput",
        "value": value,
        "unit": "ops/s",
        "labels": { "job": "hyperfine-bench" },
        "sample": sample,
        "source": "job_file"
    });
    format!("{record}\n")
}

// ---------------------------------------------------------------- the automatic re-index

/// The screen indexes a store that is behind the checkout, without being asked twice.
///
/// The complaint this file's last two tests exist to keep honest: commit, open `fathom`, and the newest
/// row was unknown until you remembered `R`. Opening the screen is the ask.
#[test]
fn a_store_behind_the_checkout_is_indexed_when_the_screen_opens() {
    let scratch = tempfile::tempdir().expect("scratch");
    let fixture = Fixture::new(scratch.path(), "auto-open");
    three_commits(&fixture.repo);
    fixture.index();

    fixture.repo.write("src/new.rs", "pub fn new() {}\n");
    fixture.repo.commit("library: one more file");

    let mut app = App::new(fixture.read(), FIXED_EPOCH_SECONDS);
    app.auto_index = true;
    assert_eq!(
        app.startup(),
        vec![fathom::tui::state::Command::Index],
        "a stale store is indexed on open"
    );
    assert!(
        app.startup().is_empty(),
        "and only once per commit: the second ask would be a second run of the same thing"
    );

    // A screen that may not write does not ask.
    let mut quiet = App::new(fixture.read(), FIXED_EPOCH_SECONDS);
    assert!(
        quiet.startup().is_empty(),
        "auto-index is off unless the caller turns it on"
    );
    quiet.auto_index = false;
    assert!(quiet.startup().is_empty());
}

/// A re-read that finds a new `HEAD` is the other door: the commit happened in the other half of a
/// split terminal, and the tick noticed.
#[test]
fn a_new_head_arriving_by_re_read_starts_one_index_and_not_a_second() {
    use fathom::tui::engine::Event;
    use fathom::tui::state::Command;

    let scratch = tempfile::tempdir().expect("scratch");
    let fixture = Fixture::new(scratch.path(), "auto-head");
    three_commits(&fixture.repo);
    fixture.index();

    let mut app = App::new(fixture.read(), FIXED_EPOCH_SECONDS);
    app.auto_index = true;
    assert!(app.startup().is_empty(), "the store is current at open");

    fixture.repo.write("src/new.rs", "pub fn new() {}\n");
    let newest = fixture.repo.commit("library: one more file");
    let reading = fixture.read();
    assert_eq!(reading.head.as_deref(), Some(newest.as_str()));
    assert_eq!(
        app.apply(Event::Read(Box::new(reading.clone()))),
        vec![Command::Index],
        "the reading that is behind asks for the run"
    );
    assert!(
        app.apply(Event::Read(Box::new(reading))).is_empty(),
        "the same HEAD is not indexed twice over"
    );

    // While a run is going — this screen's or another fathom's — nobody asks for another.
    let mut running = App::new(fixture.read(), FIXED_EPOCH_SECONDS);
    running.auto_index = true;
    running.index = fathom::tui::state::IndexState::Running {
        phase: "rows".to_owned(),
        done: 1,
        total: 3,
        ours: true,
    };
    assert!(running.startup().is_empty(), "a run in flight is enough");
}

/// The tick's question, answered against a real repository: one `git rev-parse`, and `true` only when
/// the checkout actually moved.
#[test]
fn the_refresh_tick_only_asks_git_whether_head_moved() {
    use fathom::tui::engine::{StoreWork, Work};

    let scratch = tempfile::tempdir().expect("scratch");
    let fixture = Fixture::new(scratch.path(), "tick");
    three_commits(&fixture.repo);
    fixture.index();

    let boot = fixture.boot();
    let work = StoreWork::new(
        boot.clone(),
        Config::default(),
        Query::new(),
        None,
        fathom::diff::Limits::default(),
    )
    .with_identity(&fixture.read());
    assert!(
        !work.head_moved(),
        "nothing has happened, so the tick costs one subprocess and no re-read"
    );

    fixture.repo.write("src/new.rs", "pub fn new() {}\n");
    fixture.repo.commit("library: one more file");
    assert!(
        work.head_moved(),
        "a new commit is what the tick exists to notice"
    );
}
