//! `fathom index` — the incremental store, asserted end to end (mact-by1.10's "Done when").
//!
//! Every test here is about *cost* or about *repair*, because those are the two properties the store
//! exists to have and both are invisible in a single row's contents:
//!
//! * **The budget, by count rather than by wall clock.** ADR-0006 §11.4 is explicitly unsure about
//!   2026 hardware, so the assertions are counters: a cold index analyses every in-scope file, a warm
//!   re-index of an unchanged workspace analyses none and writes no row at all, and a new commit costs
//!   exactly the blobs it touched. One generous time ceiling sits on the 1 000-commit case as a
//!   tripwire for a structural mistake, never as a benchmark to tune against.
//! * **Repair.** `.evidence` is append-only, so an old commit can gain a record and its row must move;
//!   and an index killed halfway must leave a store that is usable, reported, and completed by the next
//!   run rather than a store that has to be deleted (ADR-0006 §10.2, §10.5).
//!
//! The counters come from the library (`Run`), which is also what `run.json` carries, so what is
//! asserted here is what `fathom index --status` prints (`tests/cli.rs` pins the printing).

mod common;

use std::path::{Path, PathBuf};

use clap::Parser;
use serde_json::Value;

use fathom::boot::{Access, Boot};
use fathom::cli::Cli;
use fathom::config::Config;
use fathom::history::Query;
use fathom::store::row::Row;
use fathom::store::{lock, Indexer, Store, StoreError};

use common::{canonical, Repo};

/// A workspace plus its cache root, both under one temporary directory.
struct Fixture {
    repo: Repo,
    cache: PathBuf,
    /// The evidence root, so a test can drop a record in it the way act-mcp would.
    evidence: PathBuf,
}

impl Fixture {
    fn new(dir: &Path, name: &str) -> Self {
        let root = dir.join(name);
        let repo = Repo::init(&root.join("workspace"));
        Self {
            repo,
            cache: root.join("cache"),
            evidence: root.join("workspace/.evidence"),
        }
    }

    /// The workspace's own `.evidence`, created (act-mcp would have made it; its absence is only a
    /// warning, and a test that writes a record needs the directory to exist).
    fn with_evidence(self) -> Self {
        std::fs::create_dir_all(&self.evidence).expect("create the evidence root");
        self
    }

    /// Everything `fathom index` would resolve, from argv nobody typed. The `Cli` is parsed rather
    /// than constructed so the flags keep one definition (`--cache-dir` here is the same flag a user
    /// passes, and `Boot` is the same boot a command runs on, probe and all).
    fn boot(&self) -> Boot {
        let cli = Cli::parse_from([
            "fathom",
            "--cache-dir",
            &self.cache.display().to_string(),
            &self.repo.path().display().to_string(),
        ]);
        Boot::validate(&canonical(self.repo.path()), &cli.common, Access::Write)
            .expect("a fixture workspace boots for a writer")
    }

    /// An indexer for this fixture with the default window and config.
    fn indexer(&self) -> Indexer<'_> {
        self.indexer_with(Query::new(), "manual")
    }

    fn indexer_with(&self, query: Query, trigger: &str) -> Indexer<'_> {
        // `Indexer` borrows the `Boot` it was resolved from, and a fixture hands one out per call, so
        // the boot has to outlive the call: leaking one per test is cheaper than a lifetime argument
        // through the whole fixture, and a test process ends either way.
        let boot: &Boot = Box::leak(Box::new(self.boot()));
        Indexer::new(boot, Config::default(), query, trigger).expect("a default config compiles")
    }

    fn store(&self) -> Option<Store> {
        Store::open(&self.cache).expect("the cache root is fathom's or empty")
    }

    fn row(&self, sha: &str) -> Option<Row> {
        let store = self.store().expect("a store after any index");
        store.read_json::<Row>(&store.row_path(sha))
    }

    fn counts(&self, path: &str) -> (usize, u64) {
        let store = self.store().expect("a store after any index");
        let counts = store.counts();
        match path {
            "rows" => (counts.rows, counts.row_bytes),
            "blocks" => (counts.blocks, counts.block_bytes),
            _ => unreachable!("ask for rows or blocks"),
        }
    }
}

/// A source file's content: enough real Python that the MI ladder has something to measure.
fn source(seed: usize) -> String {
    let mut text = String::new();
    for index in 0..20_u64 {
        text.push_str(&format!(
            "def branch_{seed}_{index}(a, b):\n    if a > b and a != b and a >= 0:\n        return a + b + \
             {index}\n    return {seed}\n"
        ));
    }
    text
}

/// A commit that touches `files` files, each written fresh.
fn touch(repo: &Repo, message: &str, files: &[&str]) -> String {
    for (index, file) in files.iter().enumerate() {
        repo.write(file, &source(index + message.len()));
    }
    repo.commit(message)
}

// ---------------------------------------------------------------------------
// the three workspaces the ticket names
// ---------------------------------------------------------------------------

#[test]
fn an_empty_repository_indexes_to_an_empty_store_without_an_error() {
    let scratch = tempfile::tempdir().expect("scratch");
    let fixture = Fixture::new(scratch.path(), "empty");
    // `git init` and nothing else: unborn HEAD, no tree at all.
    let run = fixture
        .indexer()
        .run()
        .expect("an empty repo is not a failure");

    assert_eq!(run.commits_seen, 0);
    assert_eq!(run.rows_written, 0);
    assert_eq!(run.analyses, 0);
    assert_eq!(run.status, "ok");
    // The store exists and is guarded, so the next commit indexes into something rather than
    // creating it under a fingerprint nobody checked.
    let store = fixture.store().expect("the store was created");
    assert!(store.file("meta.json").is_file());
    assert_eq!(fixture.counts("rows").0, 0);
}

#[test]
fn a_workspace_with_no_evidence_directory_indexes_and_records_the_gap() {
    let scratch = tempfile::tempdir().expect("scratch");
    let fixture = Fixture::new(scratch.path(), "no-evidence");
    let head = touch(&fixture.repo, "one", &["src/a.py", "src/b.py"]);

    let run = fixture
        .indexer()
        .run()
        .expect("absent evidence is a gap, not a fault");
    assert_eq!(run.commits_seen, 1);
    assert_eq!(run.evidence_rows_read, 0);

    let row = fixture.row(&head).expect("the row is there");
    assert!(row.evidence.is_none(), "no evidence section at all");
    assert!(row.gaps.no_evidence, "and the gap says so");
    assert!(
        !row.gaps.unreadable_evidence,
        "nothing ran, which is not the same gap"
    );
    assert!(row.mi.is_some(), "git history alone still produces an MI");
}

/// Whether the checkout under test is a shallow clone, i.e. one fetched with `--depth`.
/// `actions/checkout@v4` defaults to `fetch-depth: 1`, so a workflow that runs this suite without
/// saying otherwise hands it a repository with one commit in it.
fn shallow_checkout(workspace: &Path) -> bool {
    std::process::Command::new("git")
        .args([
            "-C",
            &workspace.display().to_string(),
            "rev-parse",
            "--is-shallow-repository",
        ])
        .output()
        .is_ok_and(|out| {
            out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "true"
        })
}

#[test]
fn it_indexes_this_repository_into_a_temporary_store() {
    // The ticket's first "Done when" line, on the real thing: a repository with merges, renames,
    // binary files, generated paths and a long history — the shapes a synthetic fixture never has.
    // The store goes to a temporary directory: indexing this repository must not touch its own
    // working tree (ADR-0006 §4.2).
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/fathom sits two levels under the repo root");
    // "On the real thing" is the precondition, so check that the checkout *has* the real thing.
    // A shallow clone has one commit and nothing to index; the honest report is that the test could
    // not run, not that the repository failed. CI fetches full history for exactly this reason
    // (`.github/workflows/ci.yml`'s `fetch-depth: 0`).
    if shallow_checkout(workspace) {
        eprintln!(
            "note: skipping; {} is a shallow clone (`git rev-parse --is-shallow-repository`), so \n            there is no history here to index — fetch full depth to run this",
            workspace.display()
        );
        return;
    }
    let scratch = tempfile::tempdir().expect("scratch");
    let cli = Cli::parse_from([
        "fathom",
        "--cache-dir",
        &scratch.path().display().to_string(),
        &workspace.display().to_string(),
    ]);
    let boot =
        Boot::validate(workspace, &cli.common, Access::Write).expect("this repo is a workspace");
    let mut query = Query::new();
    query.depth = 200;
    let run = Indexer::new(&boot, Config::default(), query, "manual")
        .expect("config")
        .run()
        .expect("this repository indexes");
    assert!(
        run.commits_seen > 1,
        "this repo has history: {:?}",
        run.commits_seen
    );
    assert_eq!(run.rows_written, run.commits_seen, "cold: one row each");
    assert!(run.analyses > 0, "cold: something was analysed");
    assert!(
        scratch.path().join("index").join("rows").is_dir(),
        "the store went where --cache-dir said"
    );
    assert_eq!(
        Store::open(scratch.path())
            .expect("scratch store")
            .expect("and was created there")
            .counts()
            .rows,
        usize::try_from(run.rows_written).expect("a row count"),
        "every row landed under --cache-dir and none in the workspace itself (P4)"
    );
}

// ---------------------------------------------------------------------------
// the budget, asserted by count
// ---------------------------------------------------------------------------

#[test]
fn a_warm_reindex_of_an_unchanged_workspace_writes_nothing_and_analyses_nothing() {
    let scratch = tempfile::tempdir().expect("scratch");
    let fixture = Fixture::new(scratch.path(), "warm");
    for index in 0..6 {
        touch(
            &fixture.repo,
            &format!("commit {index}"),
            &[&format!("src/mod{index}.py"), "src/common.py"],
        );
    }

    let cold = fixture.indexer().run().expect("cold");
    assert!(cold.commits_seen == 6);
    assert_eq!(cold.rows_written, 6, "cold writes every row");
    assert!(cold.analyses > 0, "cold runs the analyzer: {cold:?}");
    assert!(cold.cache_lookups > cold.cache_hits, "cold cannot hit");

    let warm = fixture.indexer().run().expect("warm");
    assert_eq!(warm.kind, "warm", "the store was there");
    assert_eq!(warm.commits_seen, 6);
    assert_eq!(warm.rows_written, 0, "§11.4: a warm re-index writes no row");
    assert_eq!(warm.bytes_written, 0, "and publishes no bytes");
    assert_eq!(warm.analyses, 0, "and runs no analyzer");
    assert_eq!(warm.blobs_read, 0, "and reads no blob from git");
    assert_eq!(warm.evidence_files_parsed, 0, "and hashes no evidence file");
    assert!(warm.cache_hits > 0 && warm.cache_hits == warm.cache_lookups);
    assert_eq!(warm.hit_ratio_percent(), 100);
    assert!(
        warm.duration_ms < cold.duration_ms * 3 + 200,
        "warm should not be slower than cold: {warm:?} vs {cold:?}"
    );
    assert_eq!(
        fixture.counts("rows").0,
        6,
        "and the store still has its six rows"
    );
}

#[test]
fn a_new_commit_costs_the_blobs_it_touched_and_nothing_else() {
    let scratch = tempfile::tempdir().expect("scratch");
    let fixture = Fixture::new(scratch.path(), "incremental");
    for index in 0..8 {
        touch(
            &fixture.repo,
            &format!("seed {index}"),
            &[
                &format!("src/mod{index}.py"),
                &format!("src/extra{index}.py"),
                &format!("docs/readme{index}.md"),
            ],
        );
    }
    let cold = fixture.indexer().run().expect("cold");
    let analysed = cold.analyses;

    // One commit, two source files. `docs/*.md` is not an analysed language, so the count is the two
    // Python files — the point of the test is that it is *two*, not the tree's file count.
    let head = touch(
        &fixture.repo,
        "touch two",
        &["src/mod3.py", "src/extra7.py"],
    );
    let warm = fixture.indexer().run().expect("warm with one new commit");
    assert_eq!(warm.commits_seen, 9);
    assert_eq!(
        warm.rows_written, 1,
        "the new commit's row, and only that one"
    );
    assert_eq!(warm.commits_added, 1);
    assert_eq!(
        warm.blobs_read, 2,
        "exactly the two blobs the commit changed ({} were analysed cold)",
        analysed
    );
    assert_eq!(warm.analyses, 2);
    let row = fixture.row(&head).expect("the new row");
    assert!(row.mi.as_ref().is_some_and(|mi| !mi.blocks.is_empty()));
}

#[test]
fn a_config_edit_invalidates_the_sections_it_declares_and_nothing_else() {
    // §7.3's payoff, asserted by counters: a `benchmarks.deny` edit is declared by the evidence
    // section only, so a run after it must not touch a single block. (`paths.noise` is the other
    // row of §7.3's table and is asserted in `a_noise_edit_leaves_the_history_section_alone`.)
    let scratch = tempfile::tempdir().expect("scratch");
    let fixture = Fixture::new(scratch.path(), "digests");
    touch(&fixture.repo, "one", &["src/a.py"]);
    let cold = fixture.indexer().run().expect("cold");

    let mut config = Config::default();
    config.benchmarks.deny = vec!["slow-suite".to_owned()];
    let boot = fixture.boot();
    let after_deny = Indexer::new(&boot, config, Query::new(), "manual")
        .expect("config")
        .run()
        .expect("warm");
    assert_eq!(after_deny.blobs_read, 0, "no block may be recomputed");
    assert_eq!(after_deny.analyses, 0, "no analyzer run either");
    assert_eq!(
        after_deny.evidence_rows_read, 0,
        "and nothing to re-read here"
    );
    // The row *is* rewritten, and that is the honest cost: its `sources.config_digest.evidence`
    // moved, so the row no longer describes the config it was written under. §7.3's table asks for
    // "a re-read of `.evidence`, no analysis at all", which is what the two counters above are.
    assert_eq!(
        after_deny.rows_written, 1,
        "the row's digests moved, so the row moves"
    );
    assert!(cold.cache_lookups > 0);
}

#[test]
fn a_noise_edit_leaves_the_history_section_alone_and_rekeys_the_metric_blocks() {
    let scratch = tempfile::tempdir().expect("scratch");
    let fixture = Fixture::new(scratch.path(), "noise");
    touch(&fixture.repo, "one", &["src/a.py", "fixtures/b.py"]);
    let cold = fixture.indexer().run().expect("cold");
    assert!(cold.analyses >= 2, "both python files are in scope cold");

    let mut config = Config::default();
    config.paths.noise.push("fixtures/**".to_owned());
    let boot = fixture.boot();
    let noisy = Indexer::new(&boot, config, Query::new(), "manual")
        .expect("config")
        .run()
        .expect("warm after the edit");

    // The MI changed (`fixtures/b.py` left the mean), so the row is rewritten; the blobs are re-read
    // because every metric block's key covers `paths.noise` (see `store::index`'s note on §7.3).
    assert!(noisy.rows_written >= 1, "the aggregate moved");
    assert!(
        noisy.analyses < cold.analyses,
        "a file that became noise is not analysed at all: {:?} vs {:?}",
        noisy.analyses,
        cold.analyses
    );
    let row = fixture.row(&fixture.repo.head()).expect("the row");
    assert!(
        row.history.is_some(),
        "history is still there — it did not move"
    );
    assert!(
        row.mi
            .as_ref()
            .is_some_and(|mi| !mi.blocks.contains_key("fixtures/b.py")),
        "the now-noise file has no block: {:?}",
        row.mi
            .as_ref()
            .map(|mi| mi.blocks.keys().collect::<Vec<_>>())
    );
}

#[test]
fn a_thousand_commit_repository_indexes_within_budget() {
    let scratch = tempfile::tempdir().expect("scratch");
    let fixture = Fixture::new(scratch.path(), "thousand");
    // One `git fast-import` stream (a thousand commits built one `git commit` at a time would spend
    // its budget on process spawns and measure nothing), and a *fixed* 40-file tree: a thousand
    // commits in a repository that looks like a repository, rather than a fixture whose tree grows by
    // two files per commit and measures the tree instead of the indexer.
    fixture.repo.fast_import_fixed_set(1_000, 40, 30);

    let mut query = Query::new();
    query.depth = 1_000;
    let started = std::time::Instant::now();
    let cold = fixture
        .indexer_with(query.clone(), "manual")
        .run()
        .expect("cold");
    let cold_wall = started.elapsed();

    assert_eq!(cold.commits_seen, 1_000);
    assert_eq!(cold.rows_written, 1_000);
    assert_eq!(fixture.counts("rows").0, 1_000);

    let started = std::time::Instant::now();
    let warm = fixture.indexer_with(query, "manual").run().expect("warm");
    let warm_wall = started.elapsed();

    // §11.4's warm row, exactly: the ledger stats every evidence file (none), nothing is read,
    // nothing is analysed, nothing is written.
    assert_eq!(warm.rows_written, 0);
    assert_eq!(warm.bytes_written, 0);
    assert_eq!(warm.analyses, 0);
    assert_eq!(warm.blobs_read, 0);
    assert_eq!(warm.evidence_files_parsed, 0);
    assert!(warm.cache_hits > 0);
    assert_eq!(warm.cache_misses, 0, "warm: every lookup hit");
    assert_eq!(warm.hit_ratio_percent(), 100);
    assert!(
        warm.cache_hits >= 1_000,
        "one tree block per commit, all of them hits: {}",
        warm.cache_hits
    );
    println!(
        "budget: cold {} ms ({} analyses, {} rows, {} blocks), warm {} ms ({} cache hits, 0 written)",
        cold_wall.as_millis(),
        cold.analyses,
        cold.rows_written,
        fixture.counts("blocks").0,
        warm_wall.as_millis(),
        warm.cache_hits
    );
    // A ceiling, not a benchmark: three orders of magnitude above what this measures on a laptop, so
    // it fails only on a structural mistake — a `git` process per commit, a re-walk of the whole range
    // per commit, an analyzer run per unchanged file.
    assert!(
        cold_wall < std::time::Duration::from_secs(180),
        "cold index took {} ms",
        cold_wall.as_millis()
    );
    // ADR-0006 §11.4 refuses to pin a wall-clock number — "confident about the access pattern, not
    // about 2026 hardware" — so the only clock assertion is the direction, and the strong claim is the
    // counter block above. What is left here is the tripwire for "warm was accidentally made the same
    // cost as cold".
    assert!(
        warm_wall < cold_wall,
        "warm ({} ms) must be cheaper than cold ({} ms)",
        warm_wall.as_millis(),
        cold_wall.as_millis()
    );
}

// ---------------------------------------------------------------------------
// evidence is append-only: the one input that moves under an old commit
// ---------------------------------------------------------------------------

/// One coverage record, in the shape ADR-0008 §3 says act-mcp writes: `<kind>/<sha>/<run_id>.jsonl`.
fn coverage_record(sha: &str, run_id: &str, hit: u64, found: u64) -> String {
    let record = serde_json::json!({
        "schema_version": 1,
        "kind": "coverage",
        "run_id": run_id,
        "commit": sha,
        "workflow": "coverage",
        "task": "coverage",
        "category": "coverage",
        "run_status": "success",
        "tool": { "name": "act-mcp", "version": "0.1.0", "act_version": "0.2.68" },
        "started_at": "2026-11-10T12:00:00.000Z",
        "duration_ms": 60_000,
        "exit_code": 0,
        "host": "build-box-07",
        "scope": "file",
        "path": "src/a.py",
        "lines_hit": hit,
        "lines_found": found,
        "source": "lcov",
        "source_path": format!(".evidence/incoming/{run_id}/lcov.info")
    });
    format!("{record}\n")
}

#[test]
fn evidence_that_arrives_later_for_an_old_commit_updates_that_row() {
    // The ticket's "genuinely subtle part": git history is immutable and `.evidence` is not, so an
    // old SHA can gain a record after its row was written, and the row must notice. The freshness
    // rule that makes it work is the per-commit evidence digest over the ledger — which is also why
    // a re-index with nothing new reads nothing (§11.4's `evidence_files_parsed = 0`).
    let scratch = tempfile::tempdir().expect("scratch");
    let fixture = Fixture::new(scratch.path(), "late-evidence").with_evidence();
    let old = touch(&fixture.repo, "old", &["src/a.py"]);
    let _new = touch(&fixture.repo, "new", &["src/b.py"]);

    let before = fixture.indexer().run().expect("cold");
    assert_eq!(before.evidence_files_parsed, 0);
    assert!(fixture.row(&old).expect("row").gaps.no_evidence);

    // act-mcp publishes a new run at the *older* commit: append-only, and the reason mtime on the
    // directory is not an answer.
    let dir = fixture.evidence.join("coverage").join(&old).to_path_buf();
    std::fs::create_dir_all(&dir).expect("the sha directory is the contract's");
    std::fs::write(
        dir.join("20261110T120000Z-cc0006.jsonl"),
        coverage_record(&old, "20261110T120000Z-cc0006", 150, 300),
    )
    .expect("write the record");

    let after = fixture.indexer().run().expect("warm with new evidence");
    assert_eq!(
        after.evidence_files_parsed, 1,
        "one file hashed, not the tree"
    );
    assert_eq!(after.evidence_rows_read, 1, "one commit re-derived");
    assert_eq!(after.rows_written, 1, "exactly that row");
    assert_eq!(after.analyses, 0, "no metric work at all");
    assert_eq!(after.blobs_read, 0);

    let row = fixture.row(&old).expect("the row");
    assert!(!row.gaps.no_evidence, "the gap is gone");
    let evidence: Value = row.evidence.clone().expect("an evidence section");
    assert_eq!(
        evidence.get("action_success").and_then(Value::as_str),
        Some("success"),
        "{evidence}"
    );

    // And the ledger's fast path: the same tree again reads nothing.
    let third = fixture.indexer().run().expect("warm again");
    assert_eq!(third.evidence_files_parsed, 0);
    assert_eq!(third.rows_written, 0);
}

#[test]
fn a_rewritten_evidence_file_moves_the_row_it_belongs_to() {
    let scratch = tempfile::tempdir().expect("scratch");
    let fixture = Fixture::new(scratch.path(), "rewritten-evidence").with_evidence();
    let sha = touch(&fixture.repo, "one", &["src/a.py"]);
    let dir = fixture.evidence.join("coverage").join(&sha);
    std::fs::create_dir_all(&dir).expect("create the run directory");
    let file = dir.join("20261110T120000Z-cc0006.jsonl");
    std::fs::write(
        &file,
        coverage_record(&sha, "20261110T120000Z-cc0006", 150, 300),
    )
    .expect("write");
    let first = fixture.indexer().run().expect("cold");
    assert_eq!(first.evidence_rows_read, 1);

    // The same run id, different content: impossible for a real act-mcp publish (each run is a new
    // file), but it is the shortest way to prove the digest is over the *bytes* and not over the
    // filename.
    std::fs::write(
        &file,
        coverage_record(&sha, "20261110T120000Z-cc0006", 30, 300),
    )
    .expect("rewrite");
    let second = fixture.indexer().run().expect("warm");
    assert_eq!(
        second.evidence_files_parsed, 1,
        "size or mtime moved → re-hashed"
    );
    assert_eq!(second.rows_written, 1, "and the row moved with it");
}

// ---------------------------------------------------------------------------
// crash, contention, and what prune is allowed to delete
// ---------------------------------------------------------------------------

#[test]
fn a_killed_index_leaves_a_usable_store_that_the_next_run_completes() {
    let scratch = tempfile::tempdir().expect("scratch");
    let fixture = Fixture::new(scratch.path(), "crashed");
    for index in 0..5 {
        touch(
            &fixture.repo,
            &format!("c{index}"),
            &[&format!("src/m{index}.py")],
        );
    }

    // The publish seam stands in for `kill -9`: everything published before it is real, everything
    // after it never happened, and the run reports itself as interrupted (§10.2).
    let mut doomed = fixture.indexer();
    doomed.fail_publish_after = Some(4);
    let error = doomed.run().expect_err("the injected failure");
    assert!(error.to_string().contains("publish"), "{error}");

    let book =
        fathom::store::Runbook::read(&fixture.cache.join("index")).expect("run.json survived");
    assert!(book.last.interrupted, "an interrupted index says so");
    assert_eq!(book.last.status, "error");
    assert!(book.last.error.is_some());

    // The store is usable: every row that exists parses. (No torn file is possible — §6.1 — so this
    // is the whole of the "never half-rendered" claim, checked rather than assumed.)
    let store = fixture.store().expect("the store is a store");
    let published = fixture.counts("rows").0;
    assert!(published < 5, "it stopped early: {published} rows");
    assert!(published > 0, "and did real work before it stopped");
    for path in fathom::store::walk(&store.rows_dir()) {
        let text = std::fs::read_to_string(&path).expect("readable");
        serde_json::from_str::<Row>(&text)
            .unwrap_or_else(|reason| panic!("{} is not a row any more: {reason}", path.display()));
    }

    // The next index completes it: the missing rows appear, the published ones are not rewritten.
    let repair = fixture.indexer().run().expect("the repair run");
    assert_eq!(repair.commits_seen, 5);
    assert_eq!(repair.rows_written, 5 - u64::try_from(published).unwrap());
    assert_eq!(fixture.counts("rows").0, 5);
    let clean = fathom::store::Runbook::read(&fixture.cache.join("index")).expect("run.json");
    assert!(!clean.last.interrupted);
    assert!(
        clean.previous.interrupted,
        "the crashed run is still visible as `previous`"
    );
}

#[test]
fn a_finished_index_is_not_a_dead_holder_for_the_next_one() {
    let scratch = tempfile::tempdir().expect("scratch");
    let fixture = Fixture::new(scratch.path(), "finished-then-warm");
    touch(&fixture.repo, "one", &["src/a.py"]);
    fixture.indexer().run().expect("the cold index");

    // What a successful run leaves behind: the lock file and its body (§9 L3), a pid that has since
    // exited, and a published finish. Read as a death, that is a warning printed on every re-index —
    // which is what the TUI did on every commit, since the warm run that HEAD movement triggers is
    // precisely the run that reads the body of the run before it.
    assert!(
        lock::read_body(&fixture.cache).is_some(),
        "the body survives the run that wrote it"
    );
    let (held, stale) = lock::acquire(&fixture.cache).expect("the root is free again");
    assert!(
        stale.is_none(),
        "nothing died: a finished run left a trace: {stale:?}"
    );
    drop(held);

    // The warm run itself — the one that used to open with the warning — completes normally.
    touch(&fixture.repo, "two", &["src/b.py"]);
    let warm = fixture.indexer().run().expect("the warm index");
    assert_eq!(warm.commits_added, 1, "and it did the new commit");
}

#[test]
fn a_second_fathom_on_the_same_cache_root_is_refused_without_waiting() {
    let scratch = tempfile::tempdir().expect("scratch");
    let fixture = Fixture::new(scratch.path(), "busy");
    touch(&fixture.repo, "one", &["src/a.py"]);
    let holder = lock::acquire(&fixture.cache).expect("take the lock first");

    let error = fixture
        .indexer()
        .run()
        .expect_err("the second writer refuses");
    match error {
        StoreError::Lock(reason) => {
            let text = reason.to_string();
            assert!(text.contains("index_busy"), "{text}");
            assert!(text.contains("phase"), "{text}");
        }
        other => panic!("expected a lock refusal, got {other:?}"),
    }
    // Fail-fast, not queued: the refused attempt wrote nothing, not even a store to read back.
    drop(holder);
    assert!(
        fixture.store().is_none(),
        "nothing was created by a refused run"
    );
}

#[test]
fn prune_removes_rows_outside_the_window_and_leaves_everything_it_did_not_name() {
    let scratch = tempfile::tempdir().expect("scratch");
    let fixture = Fixture::new(scratch.path(), "prune");
    for index in 0..4 {
        // Distinct content each time, or git has nothing to commit and says so.
        fixture
            .repo
            .write("src/a.py", &format!("# commit {index}\n{}", source(index)));
        fixture.repo.commit(&format!("c{index}"));
    }
    let all = fixture.indexer().run().expect("cold");
    assert_eq!(all.rows_written, 4);
    let store = fixture.store().expect("store");

    // §11.2's safety rule, as a test: a file under `rows/` that is not a commit id is left alone,
    // because the cheapest way to lose someone's data with a garbage collector is a pattern that
    // matched something else.
    let foreign = store.rows_dir().join("NOT-A-COMMIT.json");
    std::fs::write(&foreign, b"not a row").expect("write the foreign file");
    let unrelated = store.rows_dir().join("deadbeef.json");
    std::fs::write(&unrelated, b"not a row either").expect("write the short name");

    // A window of two commits: the two older rows are outside it and go; the foreign files stay.
    let mut query = Query::new();
    query.depth = 2;
    let narrow = fixture.indexer_with(query, "manual").run().expect("narrow");
    assert_eq!(narrow.commits_seen, 2);
    assert_eq!(narrow.rows_pruned, 2, "the two rows outside the walk set");
    assert_eq!(rows_of(&store.rows_dir()).len(), 2, "and two survived");
    assert!(
        foreign.is_file(),
        "a name that is not a commit id survives (§11.2)"
    );
    assert!(
        unrelated.is_file(),
        "and so does one that is merely too short"
    );
    std::fs::remove_file(&foreign).expect("clean up");
    std::fs::remove_file(&unrelated).expect("clean up");
}

/// The files under `rows/` whose names are 40-hex commit ids — which is exactly the pattern prune is
/// allowed to delete, so a test that counts rows must count the way prune counts and not "everything
/// ending in `.json`" (a store's `Counts` is deliberately broader than that: `doctor` wants to *see*
/// the foreign entries, §10.5).
fn rows_of(directory: &Path) -> Vec<PathBuf> {
    fathom::store::walk(directory)
        .into_iter()
        .filter(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().trim_end_matches(".json").len() == 40)
                .unwrap_or(false)
        })
        .collect()
}

#[test]
fn rebuild_removes_the_index_and_nothing_else_under_the_cache_root() {
    let scratch = tempfile::tempdir().expect("scratch");
    let fixture = Fixture::new(scratch.path(), "rebuild");
    touch(&fixture.repo, "one", &["src/a.py"]);
    fixture.indexer().run().expect("cold");
    let store = fixture.store().expect("store");
    let foreign = fixture.cache.join("index.sqlite3");
    std::fs::write(&foreign, b"someone else's database").expect("write the foreign file");

    let run = fixture
        .indexer_with(Query::new(), "rebuild")
        .rebuild()
        .expect("rebuild");
    assert_eq!(run.kind, "cold", "a rebuild is a cold build by definition");
    assert!(run.commits_added > 0, "everything is new again");
    assert!(
        foreign.is_file(),
        "P4: fathom deletes `index/` and nothing else (§4.2)"
    );
    assert!(
        fixture.cache.join("fathom.lock").exists(),
        "the lock file is never unlinked, not even by --rebuild (§9 L3)"
    );
    assert!(store.index.join("meta.json").is_file());
}

// ---------------------------------------------------------------------------
// the same behaviour, through the binary: the argv contract for the one command
// that writes
// ---------------------------------------------------------------------------

/// One run of the binary built from this test's own crate, with the environment scrubbed so a
/// developer's `FATHOM_DIR`/`EVIDENCE_DIR` cannot move a root out from under an assertion.
fn fathom(args: &[&str]) -> (Option<i32>, String, String) {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_fathom"));
    for key in ["RUST_LOG", "EVIDENCE_DIR", "FATHOM_DIR", "FATHOM_CONFIG"] {
        command.env_remove(key);
    }
    command.args(args);
    let out = command
        .output()
        .expect("the binary is built by `cargo test`");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn the_binary_builds_the_store_and_reports_itself_in_status() {
    let scratch = tempfile::tempdir().expect("scratch");
    let fixture = Fixture::new(scratch.path(), "cli");
    touch(&fixture.repo, "one", &["src/a.py"]);
    touch(&fixture.repo, "two", &["src/b.py"]);
    let repo = fixture.repo.path().display().to_string();
    let cache = fixture.cache.display().to_string();

    // `index` writes the store and prints nothing on stdout: stdout is data, and this command's data
    // is the store (ADR-0005 §6). The progress line the ticket asks for is a log line, so `-v` is what
    // shows it.
    let (code, stdout, stderr) = fathom(&["index", "-v", "--cache-dir", &cache, &repo]);
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert!(stdout.is_empty(), "stdout stays empty: {stdout}");
    assert!(
        stderr.contains("indexing"),
        "a cold index reports progress:\n{stderr}"
    );

    let (code, stdout, _) = fathom(&["index", "--status", "--cache-dir", &cache, &repo]);
    assert_eq!(code, Some(0));
    for key in [
        "store",
        "rows",
        "blocks",
        "last_index.kind",
        "last_index.commits_seen",
        "last_index.rows_written",
        "last_index.cache_hits",
        "last_index.cache_misses",
        "last_index.evidence_files_parsed",
        "last_index.duration_s",
    ] {
        assert!(
            stdout
                .lines()
                .any(|line| line.split('\t').next() == Some(key)),
            "`index --status` should print a `{key}` line:\n{stdout}"
        );
    }
    assert!(stdout.contains("store\tpresent"), "{stdout}");
    assert!(
        stdout.lines().all(|line| line.contains('\t')),
        "every status line is `key<TAB>value`:\n{stdout}"
    );

    // `--rebuild`: cold again, and the same number of rows, because the store is disposable (§3.2).
    let (code, _, stderr) = fathom(&["index", "--rebuild", "--cache-dir", &cache, &repo]);
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    let (_, stdout, _) = fathom(&["index", "--status", "--cache-dir", &cache, &repo]);
    assert!(
        stdout
            .lines()
            .any(|line| line.starts_with("last_index.kind\tcold")),
        "a rebuild is a cold run:\n{stdout}"
    );
}

#[test]
fn a_locked_cache_root_answers_the_busy_message_the_adr_spells() {
    // §10.3's message shape, asserted through the binary: it is what a user reads, and the second
    // writer is a normal thing to happen (two shells, one repo, `fathom index` in both).
    let scratch = tempfile::tempdir().expect("scratch");
    let fixture = Fixture::new(scratch.path(), "cli-busy");
    touch(&fixture.repo, "one", &["src/a.py"]);
    let (_held, _) = lock::acquire(&fixture.cache).expect("hold the lock");
    let (code, stdout, stderr) = fathom(&[
        "index",
        "--cache-dir",
        &fixture.cache.display().to_string(),
        &fixture.repo.path().display().to_string(),
    ]);
    assert_eq!(code, Some(1));
    assert!(stdout.is_empty());
    assert!(stderr.contains("index_busy"), "{stderr}");
    assert!(stderr.contains("another Fathom"), "{stderr}");
}
