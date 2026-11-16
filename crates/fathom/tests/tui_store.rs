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
    let work = StoreWork::new(fixture.boot(), Config::default(), Query::new(), None);
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
