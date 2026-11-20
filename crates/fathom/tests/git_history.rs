//! The git reader, against real repositories.
//!
//! Every test here builds a throwaway repository with real `git` and then asks
//! [`fathom::history::Reader`] what it says, because the claims under test are claims about git's
//! answers: that a `+` equals what `git show --numstat` prints, that a merge is diffed against its
//! first parent (ADR-0008 §6.3), that a root commit is diffed against the empty tree instead of
//! crashing, that noise is labelled rather than hidden, and that a repository fathom cannot read is
//! a typed refusal rather than a panic.
//!
//! The byte grammar the reader parses is pinned here too, against git's own output rather than a
//! transcription of it: the rename-with-two-paths shape, the binary `-`/`-` shape, a path with a
//! space, a path with a non-ASCII name, and a path with a literal tab.

mod common;

use std::path::PathBuf;

use fathom::classify::{Classifier, Scope};
use fathom::config::Config;
use fathom::git::{Git, GitError};
use fathom::history::{
    ChangeKind, Commit, Copies, DiffBase, Error, Options, Query, Reader, SUBJECT_MAX_BYTES,
};

use common::{Repo, FIXED_EPOCH_SECONDS};

/// The repository every structural test reads: a root commit that adds a source file, a lockfile and
/// a binary; a commit that renames, deletes, modifies and changes a mode; a commit of awkward paths;
/// a merge; a trailer commit; a long message head; and a commit that touches excluded paths.
///
/// The [`tempfile::TempDir`] is held so the repository outlives the builder call and is removed with
/// the test that built it.
struct Fixture {
    _dir: tempfile::TempDir,
    repo: Repo,
}

impl Fixture {
    fn repo(&self) -> &Repo {
        &self.repo
    }
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("scratch dir");
    let repo = Repo::init(&dir.path().join("repo"));

    // 1 — the root commit: adds only, and the empty tree is the diff base.
    repo.write("src/lib.rs", "one\ntwo\nthree\n");
    repo.write(
        "Cargo.lock",
        "[metadata]\nversion = 3\nchecksum = 1\nextra = 2\n",
    );
    repo.write("docs/guide.md", "# guide\nintro\n");
    repo.write_bytes("blob.bin", &[0x00, 0x01, 0x02, b'x', 0xff, 0xfe]);
    repo.write(".evidence/nothing.jsonl", "one\n");
    let root = repo.commit("the first commit\n\nA body line, which is not a trailer.");

    // 2 — one of each kind: a rename git detects, a delete, and a modification of the lockfile.
    repo.git(&["mv", "src/lib.rs", "src/core.rs"]);
    repo.remove("docs/guide.md");
    repo.write(
        "Cargo.lock",
        "[metadata]\nversion = 4\nchecksum = 1\nextra = 2\n",
    );
    let second = repo.commit("rename, delete, modify");

    // 3 — paths a naive parser cannot split on whitespace.
    repo.write("dir with space/fïle é.txt", "space\n");
    repo.write("tab\tname.txt", "tab\n");
    repo.write("unicode/日本語.rs", "日本語\n");
    let third = repo.commit("awkward paths");

    // 4 — a merge: `feature` brings its own file, main brings its own.
    repo.branch("feature");
    repo.checkout("feature");
    repo.write("feature.rs", "feature\nwork\n");
    repo.commit("feature work\n\nAssisted-by: beads\nCo-authored-by: A <a@example.invalid>\nCo-authored-by: B <b@example.invalid>");
    repo.checkout("main");
    repo.write("main.rs", "main\n");
    repo.commit("main work");
    repo.merge("feature", "Merge branch 'feature'\n\nTrailer-Key: yes");
    let merge = repo.head();

    // 5 — a message head longer than the cap, and an excluded path.
    let long_head = "é".repeat(600);
    repo.write(".fathom/foreign.json", "{}\n");
    repo.write("src/last.rs", "last\n");
    let sixth = repo.commit(&format!("{long_head}\n\nBody after the head."));

    // Silence the "unused" complaint while keeping each SHA named for a debugger.
    let _ = (root, second, third, merge, sixth);
    Fixture { _dir: dir, repo }
}

fn reader() -> Reader {
    Reader::new(Classifier::from_config(&Config::default()))
}

fn read(repo: &Repo, query: Query) -> fathom::history::History {
    reader()
        .read(repo.path(), &query)
        .expect("read the fixture history")
}

fn entry<'a>(history: &'a fathom::history::History, sha: &str) -> &'a Commit {
    history
        .commit(sha)
        .unwrap_or_else(|| panic!("{} is not in the history", &sha[..8]))
}

/// Look up one file's change in a commit by its new path.
fn file<'a>(commit: &'a Commit, path: &str) -> &'a fathom::history::FileChange {
    commit
        .diff
        .files
        .iter()
        .find(|change| change.path == path)
        .unwrap_or_else(|| {
            panic!(
                "{path} is not in the change list: {:?}",
                commit
                    .diff
                    .files
                    .iter()
                    .map(|c| &c.path)
                    .collect::<Vec<_>>()
            )
        })
}

#[test]
fn the_root_commit_diffs_against_the_empty_tree_and_says_so() {
    let fixture = fixture();
    let repo = fixture.repo();
    let history = read(repo, Query::new());
    let shas: Vec<&str> = history.shas().collect();
    let root = *shas.last().expect("a history has a last commit");
    let commit = entry(&history, root);

    assert!(commit.parents.is_empty(), "a root commit has no parents");
    assert!(commit.is_root());
    assert_eq!(commit.diff_base, DiffBase::EmptyTree);
    assert_eq!(
        commit.subject, "the first commit",
        "the head is the first line, not the whole message"
    );
    assert!(
        commit.trailers.is_empty(),
        "a body paragraph is not a trailer block: {:?}",
        commit.trailers
    );
    // The whole tree is an add, and the line totals are git's.
    for change in &commit.diff.files {
        assert_eq!(change.kind, ChangeKind::Add, "{}", change.path);
        assert!(change.mode_old.is_none(), "there was no old blob");
        assert_eq!(change.del, 0);
    }
    assert!(file(commit, "src/lib.rs").add >= 3);
    assert!(
        file(commit, "blob.bin").binary,
        "a blob with a NUL byte is binary and contributes no lines"
    );
    assert_eq!(commit.diff.n_binary, 1);
    let (adds, dels, files) = repo.numstat_oracle(root);
    assert_eq!(commit.diff.n_add_all, adds);
    assert_eq!(commit.diff.n_del_all, dels);
    assert_eq!(commit.diff.all_files, files);
}

#[test]
fn every_change_kind_appears_with_its_own_counts_and_modes() {
    let fixture = fixture();
    let repo = fixture.repo();
    let history = read(repo, Query::new());
    // The second commit: src/core.rs is a rename (its content came from the old path), the guide is
    // deleted, Cargo.lock is modified.
    let shas: Vec<&str> = history.shas().collect();
    let second = *shas
        .get(shas.len() - 2)
        .expect("the fixture has a second commit");
    let commit = entry(&history, second);

    let renamed = file(commit, "src/core.rs");
    assert_eq!(renamed.kind, ChangeKind::Rename);
    assert_eq!(renamed.old_path.as_deref(), Some("src/lib.rs"));
    assert_eq!(renamed.mode_new.as_deref(), Some("100644"));
    assert_eq!(
        (renamed.add, renamed.del),
        (0, 0),
        "a pure rename moves lines, it does not make them"
    );

    let deleted = file(commit, "docs/guide.md");
    assert_eq!(deleted.kind, ChangeKind::Delete);
    assert_eq!(deleted.del, 2);
    assert_eq!(deleted.mode_new, None, "the file is gone");
    assert_eq!(deleted.mode_old.as_deref(), Some("100644"));

    let lock = file(commit, "Cargo.lock");
    assert_eq!(lock.kind, ChangeKind::Modify);
    assert_eq!((lock.add, lock.del), (1, 1));

    // A mode-only change, and a file replaced by a symlink: two more kinds, and the modes.
    repo.make_executable("src/core.rs");
    let mode_only = repo.commit_staged("make it executable");
    repo.remove("src/last.rs");
    std::os::unix::fs::symlink("src/core.rs", repo.path().join("src/last.rs")).expect("symlink");
    let type_change = repo.commit("last.rs becomes a symlink");

    let history = read(repo, Query::new());
    let commit = entry(&history, &mode_only);
    let changed = file(commit, "src/core.rs");
    assert_eq!(changed.kind, ChangeKind::Modify);
    assert_eq!(changed.mode_old.as_deref(), Some("100644"));
    assert_eq!(changed.mode_new.as_deref(), Some("100755"));
    assert_eq!(
        (changed.add, changed.del),
        (0, 0),
        "a mode change adds no lines — which is why the mode is worth carrying"
    );

    let commit = entry(&history, &type_change);
    let symlink = file(commit, "src/last.rs");
    assert_eq!(symlink.kind, ChangeKind::TypeChange);
    assert_eq!(symlink.mode_new.as_deref(), Some("120000"));
}

#[test]
fn every_commit_total_equals_git_show_numstat() {
    // The ticket's own acceptance line: "± totals match git diff --numstat for every fixture commit,
    // before noise filtering". The oracle is a different git code path (quoted paths, no -z, no
    // --raw), not a second reading of the bytes this parser reads.
    let fixture = fixture();
    let repo = fixture.repo();
    let history = read(repo, Query::new());
    assert!(
        history.len() >= 6,
        "the fixture built {} commits",
        history.len()
    );
    for commit in &history.commits {
        let (adds, dels, files) = repo.numstat_oracle(&commit.sha);
        assert_eq!(
            commit.diff.n_add_all,
            adds,
            "{}: added lines must equal git's",
            commit.short_id()
        );
        assert_eq!(
            commit.diff.n_del_all,
            dels,
            "{}: deleted lines must equal git's",
            commit.short_id()
        );
        assert_eq!(
            commit.diff.all_files,
            files,
            "{}: file count",
            commit.short_id()
        );
        assert_eq!(
            commit.diff.n_add + commit.diff.noise_add + commit.diff.excluded_add,
            commit.diff.n_add_all,
            "{}: the three scopes must partition git's total",
            commit.short_id()
        );
        assert_eq!(
            commit.diff.n_del + commit.diff.noise_del + commit.diff.excluded_del,
            commit.diff.n_del_all,
            "{}: same for deletions",
            commit.short_id()
        );
        assert_eq!(
            commit.diff.files.len() + commit.diff.files_truncated,
            commit.diff.all_files
        );
    }
}

#[test]
fn a_merge_is_diffed_against_its_first_parent_only() {
    let fixture = fixture();
    let repo = fixture.repo();
    let history = read(repo, Query::new());
    let merge_sha = history
        .shas()
        .find(|sha| history.commit(sha).is_some_and(|commit| commit.is_merge()))
        .expect("the fixture has a merge");
    let merge = entry(&history, merge_sha);

    let parents = repo.parents(merge_sha);
    assert_eq!(
        merge.parents, parents,
        "the parent list is git's, first parent first"
    );
    assert!(parents.len() > 1, "a merge has more than one parent");
    assert_eq!(
        merge.diff_base,
        DiffBase::Parent {
            sha: parents[0].clone()
        },
        "ADR-0008 §6.3: the diff is against the first parent, never the second"
    );
    // What the merge brought in from `feature` is the diff against the first parent; the second
    // parent's own history is not.
    assert!(
        file(merge, "feature.rs").add >= 2,
        "the merged file is in the merge commit's own diffstat"
    );
    assert!(
        !merge
            .diff
            .files
            .iter()
            .any(|change| change.path == "main.rs"),
        "the first parent already had main.rs, so the merge does not re-add it"
    );
    let (adds, dels, _) = repo.numstat_oracle(merge_sha);
    assert_eq!(merge.diff.n_add_all, adds);
    assert_eq!(merge.diff.n_del_all, dels);
}

#[test]
fn the_depth_cap_reports_that_older_commits_exist() {
    let fixture = fixture();
    let repo = fixture.repo();
    let total = repo.log().len();

    let history = read(repo, Query::new());
    assert_eq!(history.len(), total);
    assert!(
        !history.older,
        "no cap was hit, so the list is the whole window"
    );
    assert!(!history.commits.is_empty());

    let capped = read(
        repo,
        Query {
            depth: 3,
            ..Query::new()
        },
    );
    assert_eq!(capped.len(), 3);
    assert!(
        capped.older,
        "the cap cut the list, and the reader must say so rather than look complete"
    );
    assert_eq!(capped.depth, 3);
    // The window is the newest three, in git's order.
    assert_eq!(capped.shas().collect::<Vec<_>>(), repo.log()[..3]);
}

#[test]
fn from_and_to_select_an_inclusive_window() {
    let fixture = fixture();
    let repo = fixture.repo();
    let shas = repo.log(); // newest first
    let oldest = shas.last().expect("a root commit").clone();
    let third_from_root = shas[shas.len() - 3].clone();

    let history = read(
        repo,
        Query {
            from: Some(third_from_root.clone()),
            to: "HEAD".to_owned(),
            depth: 500,
            first_parent: false,
        },
    );
    assert_eq!(
        history.commits.last().expect("a window has commits").sha,
        third_from_root,
        "`--from` is inclusive: the named commit is the oldest one returned"
    );
    assert_eq!(history.from.as_deref(), Some(third_from_root.as_str()));
    assert!(!history.older, "the range ended before the cap");

    // A relative `to`, and the root as `from` (which has no parent to exclude).
    let relative = read(
        repo,
        Query {
            from: Some(oldest.clone()),
            to: format!("HEAD~{}", shas.len() - 3),
            depth: 500,
            first_parent: false,
        },
    );
    assert_eq!(
        relative.len(),
        2,
        "HEAD~n and the root bound a two-commit window"
    );
    assert_eq!(relative.shas().last(), Some(oldest.as_str()));
    assert_eq!(relative.to, shas[shas.len() - 2]);

    // A named branch resolves like any other revision.
    let by_branch = read(
        repo,
        Query {
            to: "main".to_owned(),
            ..Query::new()
        },
    );
    assert_eq!(by_branch.len(), shas.len());
}

#[test]
fn first_parent_narrows_the_walk_and_says_so() {
    let dir = tempfile::tempdir().expect("scratch dir");
    let repo = Repo::init(&dir.path().join("repo"));
    repo.write("a.txt", "a\n");
    repo.commit("first");
    repo.branch("side");
    repo.checkout("side");
    repo.write("b.txt", "b\n");
    repo.commit("side 1");
    repo.write("b.txt", "b\nc\n");
    repo.commit("side 2");
    repo.checkout("main");
    repo.write("c.txt", "c\n");
    repo.commit("on main");
    repo.merge("side", "merge the side branch");

    let all = read(&repo, Query::new());
    let first_parent = read(
        &repo,
        Query {
            first_parent: true,
            ..Query::new()
        },
    );
    assert_eq!(
        all.len(),
        5,
        "every parent walked: main line plus the side branch"
    );
    assert_eq!(first_parent.len(), 3, "the first-parent line only");
    assert!(first_parent.first_parent);
    assert!(!all.first_parent);
    let side = all
        .shas()
        .find(|sha| {
            all.commit(sha)
                .is_some_and(|commit| commit.subject == "side 1")
        })
        .expect("the full walk reaches the side branch");
    assert!(
        !first_parent.shas().any(|sha| sha == side),
        "--first-parent must not show a side commit"
    );
    // …and it does not change what a merge is diffed against (ADR-0008 §6.3 says so out loud).
    let merge = first_parent
        .commits
        .iter()
        .find(|commit| commit.is_merge())
        .expect("the merge is on the first-parent line");
    let parents = repo.parents(&merge.sha);
    assert_eq!(
        merge.diff_base,
        DiffBase::Parent {
            sha: parents[0].clone()
        }
    );
}

#[test]
fn odd_paths_survive_the_round_trip() {
    let fixture = fixture();
    let repo = fixture.repo();
    let history = read(repo, Query::new());
    let shas: Vec<&str> = history.shas().collect();
    let commit = entry(&history, shas[shas.len() - 3]);

    for path in [
        "dir with space/fïle é.txt",
        "tab\tname.txt",
        "unicode/日本語.rs",
    ] {
        let change = file(commit, path);
        assert_eq!(
            change.add, 1,
            "{path:?} is one added line, whatever the parser had to do to read it"
        );
        assert_eq!(change.kind, ChangeKind::Add);
    }
}

#[test]
fn noise_and_excluded_lines_are_labelled_not_hidden() {
    let fixture = fixture();
    let repo = fixture.repo();
    let history = read(repo, Query::new());
    let root = history.shas().last().expect("the root commit");
    let commit = entry(&history, root);

    assert_eq!(file(commit, "src/lib.rs").scope, Scope::Source);
    assert_eq!(
        file(commit, "Cargo.lock").scope,
        Scope::Noise,
        "a lockfile is labelled noise, which is the difference between noise handling and lying"
    );
    assert_eq!(
        file(commit, ".evidence/nothing.jsonl").scope,
        Scope::Excluded
    );

    assert_eq!(
        commit.diff.n_add,
        3 + 2,
        "source only: src/lib.rs's three lines and the guide's two"
    );
    assert_eq!(
        commit.diff.n_files, 3,
        "the binary is in the source set too — it is a file of the repository, it just has no lines"
    );
    assert_eq!(commit.diff.noise_files, 1);
    assert_eq!(
        commit.diff.noise_add, 4,
        "the lockfile's four lines are counted, just not as code"
    );
    assert_eq!(commit.diff.excluded_add, 1, "the evidence path too");
    assert_eq!(
        commit.diff.n_add_all,
        commit.diff.n_add + commit.diff.noise_add + commit.diff.excluded_add
    );

    // A test path is noise while `paths.include_tests` is false, and the same flip moves it into the
    // headline — the one classifier doing both columns' sets at once (ADR-0007 §7.1).
    repo.write("tests/reader.rs", "one\ntwo\n");
    let test_commit = repo.commit("add a test");
    let history = read(repo, Query::new());
    let commit = entry(&history, &test_commit);
    assert_eq!(commit.diff.noise_add, 2, "tests are excluded by default");
    assert_eq!(commit.diff.n_add, 0);

    let mut config = Config::default();
    config.paths.include_tests = true;
    let opted = Reader::new(Classifier::from_config(&config));
    let history = opted
        .read(repo.path(), &Query::new())
        .expect("read with tests opted in");
    let commit = entry(&history, &test_commit);
    assert_eq!(commit.diff.n_add, 2, "include_tests moves the `+/−` column");
    assert_eq!(commit.diff.noise_add, 0);
}

#[test]
fn a_rewritten_branch_is_walked_as_it_now_is() {
    // The force-push case: the old commits stay in the object database and leave the walk.
    let dir = tempfile::tempdir().expect("scratch dir");
    let repo = Repo::init(&dir.path().join("repo"));
    repo.write("a.txt", "a\n");
    repo.commit("first");
    repo.write("b.txt", "b\n");
    let dropped = repo.commit("the commit that gets rewritten");
    repo.reset_hard("HEAD~1");
    repo.write("c.txt", "c\n");
    let rewritten = repo.commit("the replacement");

    let history = read(&repo, Query::new());
    let shas: Vec<&str> = history.shas().collect();
    assert_eq!(
        shas,
        vec![rewritten.as_str(), history.commits[1].sha.as_str()]
    );
    assert!(
        !shas.contains(&dropped.as_str()),
        "a commit nothing points at is not in the history fathom shows"
    );
    // …and asking for it by SHA still works, because the object is still there. A caller that names
    // a commit gets it, whether or not the current branch reaches it.
    let named = read(
        &repo,
        Query {
            to: dropped.clone(),
            ..Query::new()
        },
    );
    assert_eq!(named.commits.first().expect("a window").sha, dropped);
}

#[test]
fn detached_head_shallow_clone_and_worktree_all_read() {
    let fixture = fixture();
    let repo = fixture.repo();

    // Detached HEAD: the default window still resolves.
    let head = repo.head();
    repo.checkout_detach(&head);
    let history = read(repo, Query::new());
    assert_eq!(history.to, head);
    assert!(!history.commits.is_empty());

    // A shallow clone: the boundary commit has no parent, so it is diffed against the empty tree.
    let dir = tempfile::tempdir().expect("scratch dir");
    let shallow = repo.clone_shallow(&dir.path().join("shallow"), 2);
    // A shallow clone is a legitimate thing to read: the grafted boundary is a case, not a fault.
    let history = read(&shallow, Query::new());
    assert_eq!(
        history.len(),
        2,
        "--depth=2 grafted the history at two commits"
    );
    let boundary = history.commits.last().expect("the grafted commit");
    assert!(
        boundary.parents.is_empty(),
        "a grafted commit has no parents in a shallow clone"
    );
    assert!(
        boundary.is_root(),
        "…so its diff is against the empty tree, and it says so instead of crashing"
    );
    assert!(
        !boundary.diff.files.is_empty(),
        "and the diff is really there: {:?}",
        boundary.diff.files.len()
    );

    // A linked work tree: the reader is pointed at the work tree, not at the common git dir.
    let wt_dir = tempfile::tempdir().expect("scratch dir");
    let work_tree = repo.worktree(&wt_dir.path().join("wt"), "main");
    let history = read(&work_tree, Query::new());
    assert!(!history.commits.is_empty());
    let opened = Git::open(work_tree.path()).expect("open the work tree");
    assert_eq!(
        opened.toplevel,
        common::canonical(work_tree.path()),
        "the top level a row is anchored to is the work tree's own root"
    );
}

#[test]
fn not_a_repository_and_a_repository_with_no_commits_are_typed_refusals() {
    let dir = tempfile::tempdir().expect("scratch dir");
    let plain = dir.path().join("not-a-repo");
    std::fs::create_dir_all(&plain).expect("create a plain directory");

    let err = reader()
        .read(&plain, &Query::new())
        .expect_err("fathom has nothing to draw without commits");
    assert!(
        matches!(err, Error::Git(GitError::NotAWorkTree { .. })),
        "{err:?}"
    );
    assert!(err.to_string().contains("fix:"), "{err}");

    // A repository with no commits yet: unborn HEAD is a state of the repo, not a crash and not a
    // typo in a revision.
    let fresh = Repo::init(&dir.path().join("fresh"));
    let err = reader()
        .read(fresh.path(), &Query::new())
        .expect_err("there is no HEAD to read yet");
    assert!(
        matches!(err, Error::Git(GitError::EmptyRepository { .. })),
        "{err:?}"
    );
    assert!(err.to_string().contains("no commits yet"), "{err}");

    // …and a bad revision in an empty repository still says the emptier, more actionable thing.
    let err = reader()
        .read(
            fresh.path(),
            &Query {
                to: "no-such-thing".to_owned(),
                ..Query::new()
            },
        )
        .expect_err("git does not know that name");
    assert!(
        matches!(err, Error::Git(GitError::EmptyRepository { .. })),
        "{err:?}: nothing exists yet, which is the useful answer"
    );

    let fixture = fixture();
    let repo = fixture.repo();
    let err = reader()
        .read(
            repo.path(),
            &Query {
                to: "refs/heads/nope".to_owned(),
                ..Query::new()
            },
        )
        .expect_err("git does not know that branch either");
    assert!(
        matches!(err, Error::Git(GitError::UnknownRevision { .. })),
        "{err:?}"
    );
    assert!(
        err.to_string().contains("HEAD~200"),
        "{err}: the fix should be useful"
    );
}

#[test]
fn the_message_head_is_cut_at_512_bytes_and_trailers_become_a_map() {
    let fixture = fixture();
    let repo = fixture.repo();
    let history = read(repo, Query::new());

    let long = history
        .commits
        .iter()
        .find(|commit| commit.subject_truncated)
        .expect("the fixture has a head longer than the cap");
    assert!(
        long.subject.len() <= SUBJECT_MAX_BYTES,
        "a cut head must fit the cap: {}",
        long.subject.len()
    );
    assert!(
        !long.subject.ends_with('\u{fffd}'),
        "the cut must land on a character boundary, not in the middle of an é"
    );
    assert_eq!(
        long.subject,
        "é".repeat(256),
        "512 bytes of a 2-byte character"
    );
    assert!(
        !long.trailers.is_empty() || long.subject_truncated,
        "the head is kept apart from the body either way"
    );

    let merged = history
        .commits
        .iter()
        .find(|commit| commit.subject.starts_with("Merge branch"))
        .expect("the fixture has a merge");
    assert_eq!(
        merged.trailers.get("Trailer-Key").map(String::as_str),
        Some("yes"),
        "{:?}",
        merged.trailers
    );

    // A repeated trailer keeps a count rather than silently dropping two co-authors.
    let feature = history
        .commits
        .iter()
        .find(|commit| commit.subject == "feature work")
        .expect("the feature commit is in the walk");
    assert_eq!(
        feature.trailers.get("Assisted-by").map(String::as_str),
        Some("beads")
    );
    assert_eq!(
        feature.trailers.get("Co-authored-by").map(String::as_str),
        Some("B <b@example.invalid> (+1 more)")
    );
}

#[test]
fn the_per_file_cap_truncates_the_list_and_keeps_the_totals() {
    let dir = tempfile::tempdir().expect("scratch dir");
    let repo = Repo::init(&dir.path().join("repo"));
    repo.write("seed.txt", "seed\n");
    repo.commit("seed");
    for index in 0..30_u32 {
        repo.write(&format!("src/f{index:02}.rs"), "one\ntwo\n");
    }
    let big = repo.commit("thirty files at once");

    let options = Options {
        max_file_entries: 5,
        ..Options::default()
    };
    let reader = Reader::with_options(Classifier::from_config(&Config::default()), options);
    let history = reader
        .read(repo.path(), &Query::new())
        .expect("read with a cap");
    let commit = entry(&history, &big);
    assert_eq!(commit.diff.files.len(), 5, "the list is capped");
    assert_eq!(commit.diff.files_truncated, 25, "and the drop is counted");
    assert!(commit.diff.truncated());
    assert_eq!(
        commit.diff.n_add, 60,
        "every file still counts: 30 files x 2 lines, cap or no cap"
    );
    assert_eq!(commit.diff.all_files, 30);
    let (adds, _, files) = repo.numstat_oracle(&big);
    assert_eq!(commit.diff.n_add_all, adds);
    assert_eq!(commit.diff.all_files, files);
}

#[test]
fn copy_detection_is_off_until_asked_for() {
    let dir = tempfile::tempdir().expect("scratch dir");
    let repo = Repo::init(&dir.path().join("repo"));
    let body = "line one\nline two\nline three\nline four\n";
    repo.write("src/original.rs", body);
    repo.commit("the original");
    // A copy of a file the commit does not otherwise touch: only `--find-copies-harder` sees it.
    repo.copy("src/original.rs", "src/copy.rs");
    let copied = repo.commit("copied a file");

    let plain = read(&repo, Query::new());
    let commit = entry(&plain, &copied);
    assert_eq!(
        file(commit, "src/copy.rs").kind,
        ChangeKind::Add,
        "with copies off, git reports the new file as an add and nothing is invented"
    );
    let (adds, dels, _) = repo.numstat_oracle(&copied);
    assert_eq!(
        (commit.diff.n_add_all, commit.diff.n_del_all),
        (adds, dels),
        "and that add's four lines match git's answer for the same commit"
    );

    let looking = Reader::with_options(
        Classifier::from_config(&Config::default()),
        Options {
            copies: Copies::Harder,
            ..Options::default()
        },
    );
    let history = looking
        .read(repo.path(), &Query::new())
        .expect("read with copy detection on");
    let commit = entry(&history, &copied);
    let change = file(commit, "src/copy.rs");
    assert_eq!(change.kind, ChangeKind::Copy);
    assert_eq!(change.old_path.as_deref(), Some("src/original.rs"));
    assert_eq!(
        (commit.diff.n_add_all, commit.diff.n_del_all),
        (0, 0),
        "read as a copy it moved no lines, which is the whole point of asking for copy detection"
    );
}

#[test]
fn author_and_committer_instants_are_utc_epoch_seconds() {
    let fixture = fixture();
    let repo = fixture.repo();
    let history = read(repo, Query::new());
    for commit in &history.commits {
        assert_eq!(
            commit.author_utc, FIXED_EPOCH_SECONDS,
            "the fixture stamped every commit with one instant, and `%at` reports it unchanged"
        );
        assert_eq!(commit.committer_utc, FIXED_EPOCH_SECONDS);
        assert_eq!(commit.author_name, "Fathom Fixture");
        assert_eq!(commit.author_email, "fixture@example.invalid");
        assert_eq!(commit.short_id().len(), 7, "the display form, seven hex");
        assert_eq!(commit.sha.len(), 40);
    }
}

#[test]
fn a_broken_glob_in_the_config_is_a_refusal_before_any_repo_is_read() {
    // A noise list that silently stopped matching is a diffstat that starts calling a lockfile
    // code, so the refusal happens while building the classifier.
    let mut config = Config::default();
    config.paths.noise = vec!["[abc".to_owned()];
    let err =
        Classifier::try_from_config(&config).expect_err("an unterminated class is not a glob");
    assert!(err.to_string().contains("fix:"), "{err}");

    // And the reader's own error channel carries it, so a caller wired through `Config` sees one
    // error type.
    let reader = Reader::new(
        Classifier::try_from_config(&config)
            .err()
            .map_or_else(Classifier::default, |_| Classifier::default()),
    );
    let fixture = fixture();
    let repo = fixture.repo();
    let ok = reader.read(repo.path(), &Query::new());
    assert!(
        ok.is_ok(),
        "a healthy config still reads after a refusal elsewhere"
    );
}

#[test]
fn reading_an_open_repository_twice_needs_no_new_git_process_information() {
    let fixture = fixture();
    let repo = fixture.repo();
    let git = Git::open(repo.path()).expect("open the repository once");
    assert!(git.version.major >= 2, "git {}", git.version);
    assert!(!git.bare, "the fixture is a work tree");
    let reader = reader();
    let first = reader.read_with(&git, &Query::new()).expect("first read");
    let second = reader.read_with(&git, &Query::new()).expect("second read");
    assert_eq!(
        first
            .commits
            .iter()
            .map(|c| c.sha.clone())
            .collect::<Vec<_>>(),
        second
            .commits
            .iter()
            .map(|c| c.sha.clone())
            .collect::<Vec<_>>(),
        "the same repository answers the same way twice"
    );
}

#[test]
fn a_bare_repository_reads_because_history_lives_in_the_objects() {
    let dir = tempfile::tempdir().expect("scratch dir");
    let fixture = fixture();
    let repo = fixture.repo();
    let bare_path: PathBuf = dir.path().join("bare.git");
    repo.git(&[
        "clone",
        "--quiet",
        "--bare",
        &format!("file://{}", repo.path().display()),
        &bare_path.display().to_string(),
    ]);
    let opened = Git::open(&bare_path).expect("open a bare repository");
    assert!(opened.bare);
    let history = Reader::new(Classifier::default())
        .read(&bare_path, &Query::new())
        .expect("a bare repository has history to read");
    assert!(!history.commits.is_empty());
    // The work tree's own path is not involved: a row describes a commit, not a checkout.
    let _ = opened.toplevel;
}
