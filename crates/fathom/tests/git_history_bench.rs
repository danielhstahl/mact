//! The budget test: a synthetic 1 000-commit repository, read the way the timeline will read it.
//!
//! ADR-0006 §11.4 is deliberately honest about wall clock — "confident about the access pattern,
//! not about 2026 hardware" — so the assertions here are the two kinds that survive a slower machine
//! and a faster one, plus one timing figure that is recorded rather than asserted tightly:
//!
//! 1. **Stream shape.** The largest single allocation the reader made is one change record, and the
//!    bytes it streamed are proportional to the *entries*, not to the repository's content. That is
//!    "allocates no whole-repo diff", made checkable: the fixture's blobs are fat on purpose, so a
//!    reader that asked git for patch text would move megabytes through [`StreamStats`] and fail
//!    here rather than in someone's TUI.
//! 2. **Incremental cost.** Reading a 50-commit window must cost a small fraction of reading the
//!    whole 1 000 — ADR-0006 §11.4's "O(new commits × files they touched), never O(repo)".
//! 3. **Wall clock, generously.** A ceiling loose enough that no reasonable machine fails it and
//!    tight enough that an accidental O(n²) or a per-commit `git` spawn would: the git work itself
//!    measures in tens of milliseconds, so a ceiling in seconds catches a structural mistake and
//!    never a noisy neighbour. The measured figure is printed, which is what §11.4 asks for — a
//!    number in the record, not vibes.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use fathom::classify::Classifier;
use fathom::config::{Config, DEFAULT_HISTORY_DEPTH};
use fathom::git::StreamStats;
use fathom::history::{Query, Reader};

use common::Repo;

/// The ceiling on a 1 000-commit read, debug build, one git invocation per stream.
///
/// Deliberately two orders of magnitude above the measured figure: this is a tripwire for a
/// structural mistake (a `git` process per commit would be minutes; a per-commit re-walk of the
/// whole range would be worse), not a benchmark to tune against.
const THOUSAND_COMMIT_CEILING: Duration = Duration::from_secs(30);

/// The ceiling on one streamed token. One change record — mode fields, two 40-hex blob ids and a
/// path — is far under a kilobyte; a patch would be a blob.
const MAX_TOKEN_BYTES: usize = 4096;

/// Bytes per commit the two streams may cost, given two files per commit in the fixture. A record is
/// a couple of hundred bytes; a file's content would be tens of kilobytes.
const BYTES_PER_COMMIT_BUDGET: u64 = 8_192;

fn synthetic(dir: &Path, commits: usize) -> Repo {
    let repo = Repo::init(&dir.join("synthetic"));
    // 40 lines of body per file, twice per commit: enough content that "we never read the blobs" is
    // a claim with teeth, and enough work that rename detection has something to look at.
    repo.fast_import(commits, 40);
    repo
}

fn reader() -> Reader {
    Reader::new(Classifier::from_config(&Config::default()))
}

#[test]
fn a_thousand_commits_read_within_budget_and_stream_only_entries() {
    let dir = tempfile::tempdir().expect("scratch dir");
    let repo = synthetic(dir.path(), 1_000);
    assert_eq!(repo.log().len(), 1_000, "the fixture is a thousand commits");

    // Building the repository is not what is being measured.
    let started = Instant::now();
    let history = reader()
        .read(
            repo.path(),
            &Query {
                depth: 1_000,
                ..Query::new()
            },
        )
        .expect("read a thousand commits");
    let elapsed = started.elapsed();

    // The figure ADR-0006 §11.4 says to record rather than assert tightly. `cargo test -- --nocapture`
    // prints it; a CI log that loses it loses the only number the ADR wants from this test.
    println!(
        "fathom git budget: {} commits in {:?} — {} bytes streamed, largest token {} bytes, {} files listed",
        history.len(),
        elapsed,
        history.stats.bytes,
        history.stats.max_token_bytes,
        history.commits.iter().map(|commit| commit.diff.files.len()).sum::<usize>(),
    );

    assert_eq!(history.len(), 1_000);
    assert!(
        !history.older,
        "the depth covered the whole window, so nothing was silently cut"
    );
    assert!(
        elapsed < THOUSAND_COMMIT_CEILING,
        "{} commits took {elapsed:?}, over the {THOUSAND_COMMIT_CEILING:?} ceiling",
        history.len()
    );
    // 1. the stream shape, which is "no whole-repo diff" made checkable.
    let stats: &StreamStats = &history.stats;
    assert!(
        stats.max_token_bytes <= MAX_TOKEN_BYTES,
        "the largest thing the reader allocated was {} bytes; a record is a few hundred, so this is \
         git's output being read whole or a blob being read at all",
        stats.max_token_bytes
    );
    assert!(
        stats.bytes <= 1_000 * BYTES_PER_COMMIT_BUDGET,
        "{} bytes for 1 000 commits is more than the entries can account for",
        stats.bytes
    );
    assert!(
        stats.bytes >= 1_000,
        "the streams produced {} bytes, which cannot be a real read",
        stats.bytes
    );

    // Every commit came back with its files, and the arithmetic still partitions.
    for commit in &history.commits {
        assert_eq!(commit.diff.files.len(), 2, "two files per fixture commit");
        assert_eq!(
            commit.diff.n_add + commit.diff.noise_add + commit.diff.excluded_add,
            commit.diff.n_add_all
        );
    }
}

#[test]
fn a_small_window_costs_a_fraction_of_the_whole_history() {
    // ADR-0006 §11.4's incremental-cost row: O(new commits × files they touched), never O(repo).
    let dir = tempfile::tempdir().expect("scratch dir");
    let repo = synthetic(dir.path(), 1_000);
    let reader = reader();

    let whole = reader
        .read(
            repo.path(),
            &Query {
                depth: 1_000,
                ..Query::new()
            },
        )
        .expect("read the whole window");
    let window = reader
        .read(
            repo.path(),
            &Query {
                to: "HEAD".to_owned(),
                from: Some("HEAD~50".to_owned()),
                depth: 1_000,
                first_parent: false,
            },
        )
        .expect("read a 50-commit window");

    assert_eq!(
        window.len(),
        51,
        "`--from` is inclusive, so 50 back is 51 commits"
    );
    println!(
        "fathom git budget: 51 of 1 000 commits streamed {} bytes against the whole history's {}",
        window.stats.bytes, whole.stats.bytes
    );
    assert!(
        window.stats.bytes * 8 < whole.stats.bytes,
        "a 51-commit window cost {} bytes against the whole history's {}: the walk must not be O(repo)",
        window.stats.bytes,
        whole.stats.bytes
    );
}

#[test]
fn the_default_depth_bounds_the_walk_and_says_there_is_more() {
    // `history.depth` is the bound ADR-0006 §11.1 names, and the timeline must be able to tell a
    // complete window from a capped one.
    let dir = tempfile::tempdir().expect("scratch dir");
    let repo = synthetic(dir.path(), 300);
    let history = reader()
        .read(repo.path(), &Query::new())
        .expect("read with the default depth");
    assert_eq!(
        history.len(),
        usize::try_from(DEFAULT_HISTORY_DEPTH)
            .unwrap_or(500)
            .min(300),
        "the default depth is the config default, and 300 commits fit inside it"
    );
    assert!(!history.older);

    // The 300-commit timing assertion the ticket asks for, at the depth a real window would use.
    let started = Instant::now();
    let capped = reader()
        .read(
            repo.path(),
            &Query {
                depth: 150,
                ..Query::new()
            },
        )
        .expect("read a capped window");
    let elapsed = started.elapsed();
    println!(
        "fathom git budget: 150 of 300 commits in {elapsed:?} ({} bytes)",
        capped.stats.bytes
    );
    assert_eq!(capped.len(), 150);
    assert!(capped.older, "and the reader says the window was cut");
    assert!(elapsed < Duration::from_secs(10), "took {elapsed:?}");
}

#[test]
fn reading_twice_costs_the_same_as_reading_once() {
    // The property the store's warm re-index leans on: nothing here grows with the number of reads.
    let dir = tempfile::tempdir().expect("scratch dir");
    let repo = synthetic(dir.path(), 400);
    let reader = reader();
    let first = reader
        .read(
            repo.path(),
            &Query {
                depth: 400,
                ..Query::new()
            },
        )
        .expect("first read");
    let started = Instant::now();
    let second = reader
        .read(
            repo.path(),
            &Query {
                depth: 400,
                ..Query::new()
            },
        )
        .expect("second read");
    let elapsed = started.elapsed();
    assert_eq!(
        first.stats.bytes, second.stats.bytes,
        "the same question, the same bytes"
    );
    assert_eq!(
        first
            .commits
            .iter()
            .map(|commit| commit.diff.n_add_all)
            .sum::<u64>(),
        second
            .commits
            .iter()
            .map(|commit| commit.diff.n_add_all)
            .sum::<u64>(),
        "and the same numbers: the reader is a function of the repository, not of its own history"
    );
    assert!(
        elapsed < Duration::from_secs(20),
        "second read took {elapsed:?}"
    );
}
