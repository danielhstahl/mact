//! The patch reader, against real repositories: what `git show` answers, what the caps cost, and what
//! the view is told when either is short.
//!
//! Every test here builds a throwaway repository with real `git` (the `common` fixture, the same one
//! `tests/git_history.rs` uses) and then asks [`fathom::diff`] for one commit, because the claims under
//! test are claims about git's own bytes:
//!
//! * the diff base is the **first parent** (ADR-0008 §6.3), so a merge shows what was merged *into* it
//!   and a root commit says `EmptyTree` instead of inventing a parent;
//! * the file list is the **history reader's** answer — same paths, same `+`/`−`, same scopes — so the
//!   diff view's list and the timeline's `+`/`−` column cannot disagree about what a commit did;
//! * a rename carries both paths, a delete has no phantom context, and a binary file is marked rather
//!   than measured;
//! * the two byte caps stop the run and are **reported**, with the flag that raises them: a diff that
//!   was cut and says nothing is the failure this whole module exists to avoid.
//!
//! The caps get the loudest tests because they are the parts that could silently lie. The huge-commit
//! test is the ticket's: a generated 40 000-line reformat, read with a small cap, must be bounded in
//! memory and must say so on the same screen.

mod common;

use std::path::Path;

use fathom::classify::{Classifier, Scope};
use fathom::diff::{self, Limits, MAX_DIFF_BYTES_FLAG, MAX_DIFF_BYTES_PER_FILE_FLAG};
use fathom::git::Git;
use fathom::history::{ChangeKind, DiffBase};

use common::Repo;

/// The fixture: every shape the diff view has to render, in one repository.
///
/// The `TempDir` is held by the builder so the objects outlive the calls that read them.
struct Fixture {
    _dir: tempfile::TempDir,
    repo: Repo,
    /// The root commit: three adds, one of them binary.
    root: String,
    /// One modify, one rename, one delete, one noise file.
    mixed: String,
    /// A merge of a branch that brought its own file.
    merge: String,
    /// A commit that changes nothing.
    empty: String,
    /// A commit that touches a file with a tab and one with CRLF line endings.
    awkward: String,
}

impl Fixture {
    fn git(&self) -> Git {
        Git::open(self.repo.path()).expect("the fixture is a repository")
    }

    fn read(&self, rev: &str) -> diff::CommitDiff {
        self.read_with(rev, Limits::default())
    }

    fn read_with(&self, rev: &str, limits: Limits) -> diff::CommitDiff {
        diff::commit(&self.git(), &Classifier::default(), rev, limits).unwrap_or_else(|why| {
            panic!("reading {rev}: {why}");
        })
    }
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("scratch dir");
    let repo = Repo::init(&dir.path().join("repo"));

    // 1 — the root commit: everything is an add, and one file is binary.
    repo.write("src/lib.rs", "one\ntwo\nthree\n");
    repo.write("Cargo.lock", "lock\nlines\nhere\n");
    repo.write_bytes("blob.bin", &[0x00, 0x01, 0x02, b'x', 0xff, 0xfe]);
    let root = repo.commit("the first commit");

    // 2 — a file that is about to be deleted, so the next commit can delete it.
    repo.write("doomed.txt", "goes\naway\n");
    repo.commit("a file with a short future");

    // 3 — one commit with every shape at once: a rename git detects (with a change on top of it), a
    // delete, and a lockfile change the classifier calls noise.
    repo.git(&["mv", "src/lib.rs", "src/core.rs"]);
    repo.write("src/core.rs", "one\nTWO\nthree\nfour\n");
    repo.write("Cargo.lock", "lock\nlines\nchanged\n");
    repo.remove("doomed.txt");
    let mixed = repo.commit("rename, modify, delete");

    // 4 — awkward content: a tab and CRLF endings, which must come back byte for byte.
    repo.write("tabs.rs", "no tab\n\tindented\n");
    repo.write("crlf.txt", "one\r\ntwo\r\n");
    let awkward = repo.commit("tabs and crlf");

    // 5 — a merge: `feature` brings its own file onto main's own.
    repo.branch("feature");
    repo.checkout("feature");
    repo.write("feature.rs", "feature\nwork\n");
    repo.commit("feature work");
    repo.checkout("main");
    repo.write("main.rs", "main\n");
    repo.commit("main work");
    repo.merge("feature", "Merge branch 'feature'");
    let merge = repo.head();

    let empty = repo.commit_empty("an empty commit");
    Fixture {
        _dir: dir,
        repo,
        root,
        mixed,
        merge,
        empty,
        awkward,
    }
}

#[test]
fn a_single_file_change_is_the_patch_git_shows() {
    let fixture = fixture();
    let diff = fixture.read(&fixture.awkward);

    assert_eq!(diff.files.len(), 2, "tabs.rs and crlf.txt");
    let tabs = diff
        .files
        .iter()
        .find(|file| file.path == "tabs.rs")
        .expect("tabs.rs is in the commit");
    assert_eq!(tabs.kind, ChangeKind::Add, "it is a new file here");
    assert!(!tabs.binary);
    assert_eq!(tabs.scope, Scope::Source);
    assert_eq!(tabs.add, 2, "two lines added, per --numstat");
    assert_eq!(tabs.del, 0);
    assert_eq!(tabs.hunks, 1, "one hunk header parsed");

    // The text is git's own, signs and all: nothing here is re-rendered, so nothing here can drift.
    let lines: Vec<&str> = diff
        .files
        .iter()
        .find(|file| file.path == "tabs.rs")
        .expect("tabs.rs")
        .lines
        .iter()
        .map(|line| line.text.as_str())
        .collect();
    assert_eq!(
        lines.first().copied(),
        Some("diff --git a/tabs.rs b/tabs.rs")
    );
    assert!(
        lines.iter().any(|line| line.starts_with("@@ -0,0")),
        "an added file's hunk header counts from zero: {lines:?}"
    );
    assert!(lines.contains(&"+no tab"), "{lines:?}");
    let kinds: Vec<_> = diff
        .files
        .iter()
        .find(|file| file.path == "tabs.rs")
        .expect("tabs.rs")
        .lines
        .iter()
        .map(|line| line.kind)
        .collect();
    assert!(
        !kinds.contains(&fathom::diff::LineKind::Delete)
            && !kinds.contains(&fathom::diff::LineKind::Context),
        "an added file has no deletes and no phantom context: {kinds:?}"
    );
}

#[test]
fn tabs_and_crlf_come_back_byte_for_byte() {
    let fixture = fixture();
    let diff = fixture.read(&fixture.awkward);
    let tabs = diff
        .files
        .iter()
        .find(|file| file.path == "tabs.rs")
        .expect("tabs.rs");
    let crlf = diff
        .files
        .iter()
        .find(|file| file.path == "crlf.txt")
        .expect("crlf.txt");

    assert!(
        tabs.lines.iter().any(|line| line.text == "+\tindented"),
        "the tab is git's tab, not four spaces: {:?}",
        tabs.lines
    );
    assert!(
        crlf.lines.iter().any(|line| line.text.ends_with("\r")),
        "a CRLF file's lines still carry the CR; the view marks it, the reader does not invent it",
    );
}

#[test]
fn a_root_commit_diffs_against_the_empty_tree_and_says_so() {
    let fixture = fixture();
    let diff = fixture.read(&fixture.root);
    assert_eq!(diff.diff_base, DiffBase::EmptyTree);
    assert!(diff.parents.is_empty(), "a root has no parents to name");
    assert!(
        diff.files.iter().all(|file| file.del == 0),
        "everything is an add against nothing"
    );
    assert!(
        diff.files
            .iter()
            .flat_map(|file| &file.lines)
            .any(|line| line.text == "--- /dev/null"),
        "git's own /dev/null header is what makes an add an add"
    );
}

#[test]
fn a_merge_is_diffed_against_its_first_parent() {
    let fixture = fixture();
    let diff = fixture.read(&fixture.merge);
    let parents = fixture.repo.parents(&fixture.merge);
    assert_eq!(parents.len(), 2, "the fixture merged a branch");
    assert_eq!(diff.parents, parents, "every parent is displayable");
    assert_eq!(
        diff.diff_base,
        DiffBase::Parent {
            sha: parents[0].clone()
        },
        "ADR-0008 \u{a7}6.3: the first parent, which is the line history was merged onto"
    );
    // `main.rs` was the first parent's own commit and is therefore *not* in this diff; `feature.rs`
    // is what the merge brought in. Reading the second parent would show the opposite.
    let paths: Vec<&str> = diff.files.iter().map(|file| file.path.as_str()).collect();
    assert!(paths.contains(&"feature.rs"), "{paths:?}");
    assert!(!paths.contains(&"main.rs"), "{paths:?}");
}

#[test]
fn a_rename_shows_both_paths_a_delete_has_no_context_and_a_binary_file_is_marked() {
    let fixture = fixture();
    let diff = fixture.read(&fixture.mixed);
    let by_path = |want: &str| {
        diff.files
            .iter()
            .find(|file| file.path == want)
            .unwrap_or_else(|| panic!("{} not in {:?}", want, diff.files))
    };

    let renamed = by_path("src/core.rs");
    assert_eq!(renamed.kind, ChangeKind::Rename);
    assert_eq!(renamed.old_path.as_deref(), Some("src/lib.rs"));

    let deleted = by_path("doomed.txt");
    assert_eq!(deleted.kind, ChangeKind::Delete);
    assert_eq!(deleted.add, 0, "a delete adds nothing");
    assert_eq!(deleted.del, 2);
    assert!(
        deleted
            .lines
            .iter()
            .any(|line| line.text == "+++ /dev/null"),
        "and git says so, which is what stops the pane showing context that is not there"
    );
    assert!(
        !deleted
            .lines
            .iter()
            .any(|line| line.kind == fathom::diff::LineKind::Context),
        "a two-line delete has nothing to give context: {:?}",
        deleted.lines
    );

    let binary = by_path("Cargo.lock");
    assert!(!binary.binary, "a lockfile is text");
    let binary = fixture
        .read(&fixture.root)
        .files
        .into_iter()
        .find(|file| file.path == "blob.bin")
        .expect("blob.bin");
    assert!(binary.binary, "git's -/- is a binary file");
    assert_eq!(binary.add, 0);
    assert_eq!(binary.hunks, 0, "and there are no hunks to show");
    assert_eq!(
        binary.no_text(),
        Some("[binary] no hunks to show: fathom does not print binary content")
    );
    assert_eq!(
        binary.label(),
        "blob.bin [binary]",
        "and the file pane says `[binary]` rather than a count"
    );
}

#[test]
fn the_file_list_is_the_history_readers_own_answer() {
    // The claim the ticket makes about the two panes: the file list's `+`/`−` come from mact-by1.8, the
    // same reader the timeline row came from. Not a re-count of `+` and `-` characters in the patch.
    let fixture = fixture();
    let git = fixture.git();
    let classifier = Classifier::default();
    let history = fathom::history::Reader::new(classifier.clone())
        .read_with(
            &git,
            &fathom::history::Query {
                to: fixture.mixed.clone(),
                from: None,
                depth: 1,
                first_parent: false,
            },
        )
        .expect("one commit of history");
    let one = history.commits.first().expect("one commit");
    let diff = fixture.read(&fixture.mixed);

    let from_history: Vec<(String, u64, u64, Scope)> = one
        .diff
        .files
        .iter()
        .map(|file| (file.path.clone(), file.add, file.del, file.scope))
        .collect();
    let from_patch: Vec<(String, u64, u64, Scope)> = diff
        .files
        .iter()
        .map(|file| (file.path.clone(), file.add, file.del, file.scope))
        .collect();
    assert_eq!(
        from_history, from_patch,
        "the diff view's file list and the timeline row must be the same answer"
    );

    // And the totals are git's, checked by a third call (`git show --numstat`, un-`-z`, quoted paths).
    let (adds, dels, files) = fixture.repo.numstat_oracle(&fixture.mixed);
    assert_eq!(diff.totals.add_all, adds);
    assert_eq!(diff.totals.del_all, dels);
    assert_eq!(diff.totals.files_all, files);
}

#[test]
fn noise_paths_are_hidden_by_default_and_shown_by_the_toggle() {
    let fixture = fixture();
    let diff = fixture.read(&fixture.mixed);
    let lock = diff
        .files
        .iter()
        .find(|file| file.path == "Cargo.lock")
        .expect("the lockfile is in the commit and in the list");
    assert_eq!(lock.scope, Scope::Noise, "the classifier says so");
    assert!(diff.hidden_files() > 0, "and the default view hides it");

    // The header's two totals, which is exactly the disagreement the `t` toggle exists to prevent:
    // the headline set and the all-files set, and the pane shows whichever is named.
    let (add, del, files) = diff.totals.shown(false);
    let (add_all, del_all, files_all) = diff.totals.shown(true);
    assert!(files_all > files, "the lockfile is a file");
    assert!(
        add_all >= add && del_all >= del,
        "the totals widen with the list, never narrow: {add}/{del} vs {add_all}/{del_all}"
    );
    assert!(
        add_all > add || del_all > del,
        "and the lockfile actually changed lines, or this fixture proves nothing: {add}/{del} vs \
         {add_all}/{del_all}"
    );
}

#[test]
fn a_commit_that_changes_nothing_says_so_rather_than_showing_an_empty_pane() {
    let fixture = fixture();
    let diff = fixture.read(&fixture.empty);
    assert!(diff.is_empty());
    assert!(diff.files.is_empty());
    assert!(
        diff.truncation.is_none(),
        "nothing was cut: there was nothing to cut"
    );
}

#[test]
fn the_whole_commit_cap_stops_the_run_and_names_the_flag() {
    let scratch = tempfile::tempdir().expect("scratch dir");
    let repo = common::Repo::init(&scratch.path().join("repo"));
    // Four files, each with a patch far bigger than the cap: the run has to stop in the first one.
    for index in 0..4 {
        repo.write(
            &format!("src/big{index}.rs"),
            &(0..20_000)
                .map(|line| format!("fn f{line}() {{}}\n"))
                .collect::<String>(),
        );
    }
    let sha = repo.commit("four huge files");

    let limits = Limits {
        total: 8_192,
        per_file: 8_192,
    };
    let diff = fathom::diff::commit(
        &Git::open(repo.path()).expect("repo"),
        &Classifier::default(),
        &sha,
        limits,
    )
    .expect("a capped read is a read, not a failure");

    assert!(
        diff.bytes_retained <= limits.total,
        "the cap is a promise about memory: kept {} bytes of a {}-byte read",
        diff.bytes_retained,
        diff.bytes_streamed
    );
    assert!(
        diff.bytes_streamed >= diff.bytes_retained as u64,
        "retained {} of streamed {}",
        diff.bytes_retained,
        diff.bytes_streamed
    );
    let banner = diff.banner().expect("a cut diff must say so");
    assert!(
        banner.contains(MAX_DIFF_BYTES_FLAG) && banner.contains("8 KiB"),
        "the banner names the limit and the flag that raises it: {banner}"
    );
    assert!(
        banner.contains("not shown"),
        "and how much of the commit never appeared: {banner}"
    );
    let cut = diff.truncation.expect("truncation is reported");
    assert!(cut.total_cut);
    assert!(cut.files_unseen > 0, "three of the four never started");
    // Every file the run never reached is still in the list, and says why it is empty.
    let unseen = diff
        .files
        .iter()
        .find(|file| file.unseen)
        .expect("at least one file was never read");
    assert!(
        unseen
            .no_text()
            .is_some_and(|note| note.contains("whole-commit byte cap")),
        "an unseen file is explained, not blank: {:?}",
        unseen.no_text()
    );
}

#[test]
fn the_per_file_cap_stops_one_file_and_keeps_reading_the_rest() {
    let scratch = tempfile::tempdir().expect("scratch dir");
    let repo = common::Repo::init(&scratch.path().join("repo"));
    repo.write(
        "generated.h",
        &(0..30_000)
            .map(|line| format!("int g{line};\n"))
            .collect::<String>(),
    );
    repo.write("src/small.rs", "one\n");
    let sha = repo.commit("one generated file and one small one");

    // A file cap far below the commit cap: the generated file is cut, and the small file after it is
    // still read in full. This is why there are two caps and not one.
    let limits = Limits {
        total: 1 << 20,
        per_file: 4_096,
    };
    let diff = fathom::diff::commit(
        &Git::open(repo.path()).expect("repo"),
        &Classifier::default(),
        &sha,
        limits,
    )
    .expect("the read succeeds and reports what it cut");

    let generated = diff
        .files
        .iter()
        .find(|file| file.path == "generated.h")
        .expect("generated.h");
    assert!(generated.cut, "the generated file was cut");
    assert!(
        generated.dropped > 100_000,
        "and the view can say how much it dropped: {}",
        generated.dropped
    );
    let small = diff
        .files
        .iter()
        .find(|file| file.path == "src/small.rs")
        .expect("src/small.rs");
    assert!(!small.unseen && !small.cut, "the next file was still read");
    assert_eq!(small.hunks, 1);

    let banner = diff.banner().expect("a cut diff says so");
    assert!(
        banner.contains(MAX_DIFF_BYTES_PER_FILE_FLAG) && banner.contains("4 KiB"),
        "{banner}"
    );
}

#[test]
fn a_huge_commit_renders_in_bounded_memory_and_shows_the_banner() {
    // The ticket's own test: a generated fixture larger than the cap, read through the same call the
    // `d` key makes, and the assertion is on the bytes the process is holding afterwards.
    let scratch = tempfile::tempdir().expect("scratch dir");
    let repo = common::Repo::init(&scratch.path().join("repo"));
    let before: String = (0..40_000).map(|line| format!("line {line}\n")).collect();
    repo.write("reformat.txt", &before);
    repo.commit("40 000 lines, as they were");
    let after: String = (0..40_000).map(|line| format!("LINE  {line}\n")).collect();
    repo.write("reformat.txt", &after);
    repo.write("notes.md", "and one small file\n");
    let sha = repo.commit("the 40 000-line reformat");

    let diff = fixture_limits_read(repo.path(), &sha, 64 * 1024);
    let cut = diff
        .truncation
        .expect("a 40 000-line reformat will not fit");
    assert!(cut.total_cut || cut.files_cut > 0);
    assert!(
        diff.bytes_retained <= 64 * 1024,
        "{} bytes retained",
        diff.bytes_retained
    );
    assert!(
        diff.bytes_streamed < 200_000,
        "the run stopped instead of reading a multi-megabyte patch into memory: {}",
        diff.bytes_streamed
    );
    assert!(
        diff.banner()
            .is_some_and(|banner| banner.contains("KiB") || banner.contains("MiB")),
        "the banner states the cap in the units the reader is thinking in"
    );
    // And the small file in the same commit is still there to be read, because that is the point.
    assert!(diff.files.iter().any(|file| file.path == "notes.md"));
}

/// Read one commit of a scratch repository with a whole-commit cap of `total` bytes.
fn fixture_limits_read(path: &Path, sha: &str, total: usize) -> diff::CommitDiff {
    diff::commit(
        &Git::open(path).expect("repo"),
        &Classifier::default(),
        sha,
        Limits {
            total,
            per_file: total,
        },
    )
    .expect("the read succeeds even when it had to stop early")
}

#[test]
fn the_command_line_prints_the_same_diff_to_a_pipe() {
    // `fathom tui --plain --diff <rev>` is the same read through the same view, run as a process so that
    // what is asserted is what a user pipes: exit status, stdout, and nothing on stderr.
    let fixture = fixture();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_fathom"))
        .args([
            "tui",
            "--plain",
            "--diff",
            &fixture.mixed,
            &fixture.repo.path().display().to_string(),
        ])
        .output()
        .expect("the binary is built for integration tests");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "status {:?}\nstderr: {stderr}\nstdout: {stdout}",
        output.status.code()
    );
    // stderr is not empty and should not be: this fixture has no `.evidence`, so boot says so once, the
    // same warning any `fathom` run in a bare repository makes. What must not be there is a complaint
    // about the diff itself.
    assert!(!stderr.contains("diff"), "stderr: {stderr}");
    assert!(stdout.starts_with("DIFF  "), "{stdout}");
    let parent = fixture.repo.parents(&fixture.mixed).remove(0);
    assert!(
        stdout.contains(&format!("vs parent {}", &parent[..7])),
        "the base is named, so the numbers have a meaning: {stdout}"
    );
    assert!(
        stdout.contains("diff --git a/src/core.rs b/src/core.rs")
            || stdout.contains("similarity index"),
        "the rename is shown, with its detection: {stdout}"
    );
    assert!(
        stdout.contains("Cargo.lock"),
        "including the noise file: {stdout}"
    );
    assert!(
        stdout.contains("deleted file mode 100644") && stdout.contains("-away"),
        "and the delete's lines are there: {stdout}"
    );

    // And the caps are the same knobs from the command line: a tiny one changes the output and says so.
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_fathom"))
        .args([
            "tui",
            "--plain",
            "--max-diff-bytes",
            "200",
            "--diff",
            &fixture.mixed,
            &fixture.repo.path().display().to_string(),
        ])
        .output()
        .expect("the binary is built for integration tests");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        stdout.contains("diff cut at the 200 B limit (--max-diff-bytes)"),
        "the bill is printed and names the flag: {stdout}"
    );
}

#[test]
fn an_unknown_revision_is_reported_by_the_command_line_as_a_failure_not_an_empty_screen() {
    let fixture = fixture();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_fathom"))
        .args([
            "tui",
            "--plain",
            "--diff",
            "not-a-revision",
            &fixture.repo.path().display().to_string(),
        ])
        .output()
        .expect("the binary is built for integration tests");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        !output.status.success(),
        "a bad revision is a failed command, not an empty screen: {stdout}"
    );
    assert!(
        stdout.trim().is_empty(),
        "and it prints no half-a-diff to be piped into something: {stdout}"
    );
    assert!(
        stderr.contains("not a revision") && stderr.contains("fix:"),
        "the refusal goes where the CLI sends refusals, with its fix: {stderr}"
    );
}

#[test]
fn a_revision_git_does_not_know_is_a_refusal_with_a_fix() {
    let fixture = fixture();
    let why = diff::commit(
        &fixture.git(),
        &Classifier::default(),
        "not-a-revision-anywhere",
        Limits::default(),
    )
    .expect_err("git does not know that revision");
    let text = why.to_string();
    assert!(text.contains("not a revision"), "{text}");
    assert!(
        text.contains("fix:"),
        "every refusal carries its fix: {text}"
    );
}

#[test]
fn a_path_no_parser_can_split_is_still_matched_to_its_own_entry() {
    // `b/` inside a path defeats any split of `diff --git a/x b/y`, which is why the patch is matched to
    // the stat list by position and the path is only the check on it.
    let scratch = tempfile::tempdir().expect("scratch dir");
    let repo = common::Repo::init(&scratch.path().join("repo"));
    repo.write("b/x b/y.txt", "one\n");
    repo.write("dir with space/fïle é.txt", "one\n");
    repo.write("tab\tname.txt", "one\n");
    let sha = repo.commit("three names a naive parser cannot split");

    let diff = fathom::diff::commit(
        &Git::open(repo.path()).expect("repo"),
        &Classifier::default(),
        &sha,
        Limits::default(),
    )
    .expect("the read works");
    assert!(
        diff.warnings.is_empty(),
        "no file was invented or lost: {:?}",
        diff.warnings
    );
    let paths: Vec<&str> = diff.files.iter().map(|file| file.path.as_str()).collect();
    assert_eq!(
        paths,
        vec!["b/x b/y.txt", "dir with space/fïle é.txt", "tab\tname.txt"]
    );
    for file in &diff.files {
        assert_eq!(file.hunks, 1, "{} lost its hunk", file.path);
        assert!(!file.lines.is_empty());
    }
}
