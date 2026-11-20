//! The git reader: a rev range in, one typed entry per commit out (mact-by1.8).
//!
//! This is the history the timeline scrolls through, and the `+`/`−` column of one row. It is a
//! pure read of the repository: no cache, no clock, no evidence, no server — the store
//! (mact-by1.10) writes what this returns, and the screens (mact-by1.9/.11/.14) render it.
//!
//! ## Two `git` processes per read, never two per commit
//!
//! ```text
//! git rev-parse --verify <rev>            resolve --to / --from, once each
//! git log --format=<NUL-separated>        the commit list: sha, parents, people, times, head, trailers
//! git log -M --raw --numstat -z …         the diffstat of every commit in that same list
//! ```
//!
//! Both walks are passed the identical resolved SHAs, revs and `--max-count`, so the two lists are
//! the same commits in the same order and zip by index; a repository that makes them disagree is a
//! [`Error::Parse`], not a silently wrong row. Two processes rather than one because the metadata
//! stream has no diff in it at all (so the cheap call stays cheap and the expensive one streams),
//! and two-per-commit would be 5 000 process spawns for a 5 000-commit repository — the difference
//! between opening a repository and filing a complaint about it.
//!
//! ## What one commit carries
//!
//! Full 40-hex SHA, the parent list in first-parent order, author name and email, author and
//! committer UTC instants (`%at`/`%ct`, epoch seconds: immune to locale and to `log.date`), the
//! message head truncated at [`SUBJECT_MAX_BYTES`] on a character boundary, the trailer map when
//! git finds trailers, and the diffstat: `n_add`, `n_del`, `n_files`, `n_binary`, and the per-file
//! list with change kind, old path when renamed or copied, old/new modes and per-file `+`/`−`.
//!
//! ## The diff is against the first parent, and the row says so
//!
//! ADR-0008 §6.3 pins it: a commit's diff base is its **first parent** — the line history was
//! merged *onto*, which is `git show`'s own default and the diff a reviewer of a merge actually
//! wants. [`git::MIN_GIT`] exists because that is the oldest git with `--diff-merges=first-parent`,
//! the one flag that asks for that diff *while still walking every parent* (`--first-parent` would
//! narrow the walk too, and ADR-0008 §6.3 says explicitly that it must not change which parent a
//! delta is against). A root commit — or a grafted commit at the boundary of a shallow clone, which
//! is the same shape — has no parent, so its diff is against the empty tree (`--root`), and
//! [`Commit::diff_base`] says which of the two it was rather than letting a renderer guess. A merge
//! commit's extra parents are displayable and are never a diff base.
//!
//! ## Noise is labelled, never hidden
//!
//! The per-file list is classified by [`crate::classify`] — the same classifier ADR-0007 §7.1 gives
//! the MI column — so the headline totals and the MI column describe one file set:
//!
//! | total | counts |
//! |---|---|
//! | `n_add` / `n_del` / `n_files` | `Scope::Source` paths only — the headline, and the only numbers the `+`/`−` column draws |
//! | `noise_add` / `noise_del` / `noise_files` | `Scope::Noise`: lockfiles, vendored trees, generated files, and tests while `paths.include_tests` is false |
//! | `excluded_add` / `excluded_del` / `excluded_files` | `Scope::Excluded`: `.evidence/**`, `.fathom/**`, and anything outside a narrowed `paths.source` |
//! | `n_add_all` / `n_del_all` / `all_files` | everything git counted, which is what `git diff --numstat` answers and what a test can check |
//!
//! A 4 000-line `Cargo.lock` does not disappear and does not get called code. The first three rows
//! sum to the fourth by construction, and `crates/fathom/tests/git_history.rs` asserts the fourth
//! equals git for every commit in the fixture.
//!
//! ## Bounds, and what happens at them
//!
//! * **Depth.** `query.depth` (from `history.depth`, default 500) is a cap, and when it cut the list
//!   [`History::older`] says `true` instead of the caller assuming the list started at the root.
//!   The cap is asked for as `depth + 1` commits and the extra one is what proves the cut.
//! * **Per-file entries.** At most `options.max_file_entries` are *kept*, and
//!   [`Diffstat::files_truncated`] counts the ones that were dropped. The totals still count every
//!   file: the cap bounds memory, not correctness.
//! * **Memory.** Both git calls are streamed token by token ([`git::Git::stream_tokens`]), and
//!   nothing asks git for patch text, so no file content — let alone a whole repository's diff —
//!   is ever in this process. [`History::stats`] carries `max_token_bytes`, which is the number the
//!   budget test asserts on.
//!
//! ## The cases that must not panic
//!
//! | repository | answer |
//! |---|---|
//! | not a git repository | [`GitError::NotAWorkTree`], naming the path |
//! | `git init` with no commits | [`GitError::EmptyRepository`] — an empty history is a fact about the repo, and "unborn HEAD" is not a crash |
//! | detached `HEAD` | reads normally; `HEAD` resolves like any other ref |
//! | shallow clone | the grafted boundary commit diffs against the empty tree and says `DiffBase::EmptyTree` |
//! | `git worktree` checkout | reads normally; `rev-parse --show-toplevel` is the work tree root |
//! | bare repository | reads normally — history is in the objects, not the checkout |
//! | a `--to`/`--from` git does not know | [`GitError::UnknownRevision`] |
//! | git older than [`git::MIN_GIT`] | [`GitError::UnsupportedGit`], with the flag it needs named |
//!
//! git's own warnings (an inexact rename detection that hit `diff.renameLimit`, say) are carried out
//! verbatim in [`History::warnings`] rather than swallowed: a rename that git declined to find
//! changes which rows say `rename`, and a silent `add` + `delete` pair is the kind of number that
//! gets believed.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde::Serialize;
use thiserror::Error;

use crate::{
    classify::{Classifier, GlobError, Scope},
    config::Config,
    git::{self, Git, GitError, GitVersion, StreamStats},
};

/// The message head's cap, in bytes: a row's `subject` is display text, not data.
///
/// A squash of a hundred dependency bumps can carry a multi-kilobyte head, and ADR-0006 §6.2's row
/// shape treats `subject` as the one-line label a screen draws, so it is cut here rather than at
/// every renderer. The cut is on a UTF-8 character boundary — a truncated subject has to stay
/// serialisable — and [`Commit::subject_truncated`] records that the cut happened, because a head
/// that ends mid-word and says nothing about it reads like a repository problem.
pub const SUBJECT_MAX_BYTES: usize = 512;

/// The default hard cap on per-file entries kept per commit.
///
/// Not a `fathom.toml` key: ADR-0006 §7.2 declares every key a reader is allowed to read, and a
/// value that changes what a stored row contains without changing its digest is exactly the bug §7.3
/// exists to prevent. It is an option on [`Reader`] so a test — and a pathological repository — can
/// turn it, and dropping entries is reported by [`Diffstat::files_truncated`] either way.
pub const DEFAULT_MAX_FILE_ENTRIES: usize = 4_096;

/// `--format` for the commit-list call: eleven `NUL`-separated fields, led by a `NUL` so each
/// record starts with an empty token.
///
/// `NUL` is the only byte that cannot appear in a commit object, so no message, author name or email
/// can shift this parse. `%x00` is git's own escape — a real `NUL` cannot be passed in an argv
/// element, which is the one git-CLI detail that would otherwise be surprising here.
const META_FORMAT: &str =
    "--format=%x00%H%x00%T%x00%P%x00%at%x00%ct%x00%an%x00%ae%x00%cn%x00%ce%x00%s%x00%(trailers:only,unfold)";

/// The field count of [`META_FORMAT`].
const META_FIELDS: usize = 11;

/// `--format` for the diffstat call: a record marker plus the SHA and nothing else.
///
/// `git log -z` ends a header at `NUL`, so this token is exactly `\x1e<sha>` and the marker never
/// competes with user text.
const DIFF_FORMAT: &str = "--format=%x1e%H";

/// What can stop a history read.
#[allow(
    missing_docs,
    reason = "the #[error] format string documents each variant and its fields"
)]
#[derive(Debug, Error)]
pub enum Error {
    /// git refused, was too old, or was not there.
    #[error(transparent)]
    Git(#[from] GitError),

    /// A glob in `fathom.toml` will not compile, so the file set could not be decided.
    #[error(transparent)]
    Classify(#[from] GlobError),

    /// git's output did not have the shape documented in this module — including the two calls
    /// disagreeing about which commits there are, which is the failure mode a zip-by-index can hit.
    #[error("cannot read git's answer about this history: {detail}\n  fix: this is fathom's bug for this repository; report it with the command line and the commit")]
    Parse { detail: String },
}

/// Whether the diff call asks git for copy detection, and how far it looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Copies {
    /// Renames only (the default). Copy detection is quadratic in the files it may copy *from*, and
    /// a timeline that shows `copy` instead of `add` is worth a deliberate cost, not a free one.
    #[default]
    Off,
    /// `-C`: look for a copy source among the files the commit itself changed.
    SameCommit,
    /// `-C --find-copies-harder`: also consider unchanged files in the parent tree. This is the only
    /// spelling that finds a file copied off an untouched file, and it is the slow one.
    Harder,
}

impl Copies {
    /// The git flags for this setting.
    #[must_use]
    pub fn flags(self) -> &'static [&'static str] {
        match self {
            Self::Off => &[],
            Self::SameCommit => &["-C"],
            Self::Harder => &["-C", "--find-copies-harder"],
        }
    }
}

/// How the reader asks git for a diffstat.
#[derive(Debug, Clone)]
pub struct Options {
    /// `-M`: a rename is one change, not an add and a delete. On by default because a rename read as
    /// two changes is a `+`/`−` column that lies about the size of a commit.
    pub find_renames: bool,
    /// How hard to look for copies.
    pub copies: Copies,
    /// The hard cap on kept per-file entries per commit (see [`DEFAULT_MAX_FILE_ENTRIES`]).
    pub max_file_entries: usize,
    /// The message head's cap.
    pub subject_max_bytes: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            find_renames: true,
            copies: Copies::Off,
            max_file_entries: DEFAULT_MAX_FILE_ENTRIES,
            subject_max_bytes: SUBJECT_MAX_BYTES,
        }
    }
}

/// Which commits to read.
///
/// `from` and `to` are view choices, not config (ADR-0006 §7.4), so they are never in a digest;
/// `first_parent` is the exception the same section names, and it costs a re-index because it
/// changes which commits the window contains at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    /// The newest commit to include. Anything `git rev-parse` accepts: a SHA, `HEAD`, a branch, a
    /// tag, `HEAD~200`.
    pub to: String,
    /// The oldest commit to include, **inclusive**. `None` means "walk until the depth cap".
    pub from: Option<String>,
    /// How many commits to return at most — `history.depth`.
    pub depth: usize,
    /// Follow only first parents (`history.first_parent`, default false).
    pub first_parent: bool,
}

impl Default for Query {
    fn default() -> Self {
        Self {
            to: "HEAD".to_owned(),
            from: None,
            depth: crate::config::DEFAULT_HISTORY_DEPTH as usize,
            first_parent: false,
        }
    }
}

impl Query {
    /// The default window: `HEAD`, `history.depth` commits, every parent walked.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The window `fathom.toml` describes, before any flag overrides it.
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        Self {
            to: "HEAD".to_owned(),
            from: None,
            depth: usize::try_from(config.history.depth).unwrap_or(usize::MAX),
            first_parent: config.history.first_parent,
        }
    }
}

/// The reader: one classifier, one set of options, no other state.
#[derive(Debug, Clone)]
pub struct Reader {
    classifier: Classifier,
    options: Options,
}

impl Reader {
    /// A reader with the default options.
    #[must_use]
    pub fn new(classifier: Classifier) -> Self {
        Self {
            classifier,
            options: Options::default(),
        }
    }

    /// A reader with the options spelled out.
    #[must_use]
    pub const fn with_options(classifier: Classifier, options: Options) -> Self {
        Self {
            classifier,
            options,
        }
    }

    /// The classifier in force — the same object a caller can hand the MI analyzer, which is the
    /// point of ADR-0007 §7.1.
    #[must_use]
    pub const fn classifier(&self) -> &Classifier {
        &self.classifier
    }

    /// Read the history `query` names.
    ///
    /// # Errors
    ///
    /// [`Error::Git`] for a workspace that is not a repository, has no commits, names an unknown
    /// revision, or is read by a git too old for ADR-0008 §6.3's rule; [`Error::Classify`] for a
    /// broken glob; [`Error::Parse`] for output that is not the documented shape.
    pub fn read(&self, workspace: &Path, query: &Query) -> Result<History, Error> {
        let git = Git::open(workspace)?;
        self.read_with(&git, query)
    }

    /// [`Reader::read`] against an already-opened repository, so a caller that opened git for its
    /// own reasons (a `doctor` that wants the version, an index that walks twice) pays for
    /// `rev-parse` once.
    ///
    /// # Errors
    ///
    /// As [`Reader::read`].
    pub fn read_with(&self, git: &Git, query: &Query) -> Result<History, Error> {
        let depth = query.depth.max(1);
        let to = match git.resolve(&query.to) {
            Ok(sha) => sha,
            Err(GitError::UnknownRevision { revision }) => {
                // An unborn HEAD is a different answer from a typo: the repository is fine and has
                // simply nothing in it yet.
                if git.is_empty()? {
                    return Err(GitError::EmptyRepository {
                        path: git.toplevel.clone(),
                    }
                    .into());
                }
                return Err(GitError::UnknownRevision { revision }.into());
            }
            Err(other) => return Err(other.into()),
        };
        let from = query
            .from
            .as_deref()
            .map(|revision| git.resolve(revision))
            .transpose()?;
        // `--from` is inclusive, which `--not <from>^` says. A root commit has no `<from>^`; the walk
        // then simply runs to the root, which is the same set.
        let from_has_parent = match &from {
            Some(sha) => git.has_parent(sha)?,
            None => false,
        };
        let revs = git::revs(from.as_deref(), from_has_parent, &to);

        let mut common: Vec<String> = vec![format!("--max-count={}", depth.saturating_add(1))];
        if query.first_parent {
            common.push("--first-parent".to_owned());
        }

        let mut warnings = Vec::new();
        let (mut commits, meta_stats) = self.metadata(git, &revs, &common)?;
        let (diffs, diff_stats) = self.diffs(git, &revs, &common)?;
        // Both runs' streams add up to what this read cost, and `max_token_bytes` across both is
        // the proof that no single allocation ever held more than one record.
        let mut stats = StreamStats::default();
        for run in [meta_stats, diff_stats] {
            stats.bytes += run.bytes;
            stats.tokens += run.tokens;
            stats.max_token_bytes = stats.max_token_bytes.max(run.max_token_bytes);
            warnings.extend(split_warnings(&run.stderr));
        }

        // The two walks were asked for the same commits in the same order. If they did not answer
        // that way, say so: zipping a mismatch by hand is how a row ends up with another commit's
        // diffstat, and a wrong number that never changes is the worst kind.
        if commits.len() != diffs.len() {
            return Err(Error::Parse {
                detail: format!(
                    "git listed {} commits and diffstat-ed {} of them",
                    commits.len(),
                    diffs.len()
                ),
            });
        }
        for (commit, diff) in commits.iter_mut().zip(diffs) {
            if commit.sha != diff.sha {
                return Err(Error::Parse {
                    detail: format!(
                        "the commit list and the diffstat disagree: {} vs {}",
                        commit.sha, diff.sha
                    ),
                });
            }
            // ADR-0008 §6.3, recorded per row: the diff base is the first parent, and a commit with
            // no parent — a root, or a shallow clone's graft — was diffed against the empty tree by
            // `--root`. The renderer never has to guess, and a root has no delta to draw.
            commit.diff_base = match commit.parents.first() {
                Some(first_parent) => DiffBase::Parent {
                    sha: first_parent.clone(),
                },
                None => DiffBase::EmptyTree,
            };
            commit.diff = diff.into_diffstat(&self.classifier, self.options.max_file_entries)?;
        }

        // The cap is asked for as `depth + 1`; the extra commit is not part of the answer, it is the
        // evidence that there was an older commit to find.
        let older = commits.len() > depth;
        if older {
            commits.truncate(depth);
        }

        Ok(History {
            workspace: git.toplevel.clone(),
            git_version: git.version,
            to,
            from,
            depth,
            first_parent: query.first_parent,
            older,
            commits,
            warnings,
            stats,
        })
    }

    /// Call 1: the commit list. No diff, so this stays cheap on any repository.
    fn metadata(
        &self,
        git: &Git,
        revs: &[String],
        common: &[String],
    ) -> Result<(Vec<Commit>, StreamStats), Error> {
        let mut args: Vec<&str> = vec!["log", "--no-color", META_FORMAT];
        for flag in common {
            args.push(flag.as_str());
        }
        args.extend(revs.iter().map(String::as_str));

        let mut parser = MetaParser::default();
        let stats = git.stream_tokens(&args, &mut |token| parser.on_token(token))?;
        let commits = parser.finish();
        Ok((commits, stats))
    }

    /// Call 2: the diffstat of the same commits, streamed.
    fn diffs(
        &self,
        git: &Git,
        revs: &[String],
        common: &[String],
    ) -> Result<(Vec<ParsedCommit>, StreamStats), Error> {
        let mut args: Vec<&str> = vec![
            "log",
            "--no-color",
            "-z",
            "--raw",
            "--numstat",
            // Full blob ids, always. git abbreviates `--raw`'s object ids by default (the length is
            // `core.abbrev`'s, and `--command=` contexts change it), and an abbreviated id is not an
            // identity: the metric cache keys on these OIDs (ADR-0006 §6.3), so a 7-hex prefix would
            // silently turn every cache lookup into a miss and every equality test into a false
            // "the content changed". `--no-abbrev` is what makes `blob_new`/`blob_old` keys.
            "--no-abbrev",
            "--root",
            "--diff-merges=first-parent",
            DIFF_FORMAT,
        ];
        if self.options.find_renames {
            args.push("-M");
        } else {
            args.push("--no-renames");
        }
        args.extend(self.options.copies.flags());
        for flag in common {
            args.push(flag.as_str());
        }
        args.extend(revs.iter().map(String::as_str));

        let mut parser = DiffParser::default();
        let stats = git.stream_tokens(&args, &mut |token| parser.on_token(token))?;
        let commits = parser.finish();
        Ok((commits, stats))
    }
}

/// One read: the window that was walked, and what it cost.
#[derive(Debug, Clone)]
pub struct History {
    /// The repository's top level.
    pub workspace: PathBuf,
    /// The `git` that answered.
    pub git_version: GitVersion,
    /// The resolved tip (`--to`).
    pub to: String,
    /// The resolved oldest commit (`--from`), if one was named.
    pub from: Option<String>,
    /// The cap that was asked for.
    pub depth: usize,
    /// Whether only first parents were walked.
    pub first_parent: bool,
    /// Whether the depth cap cut the list short — the "there are older commits" marker the reader
    /// must show instead of a list that looks complete.
    pub older: bool,
    /// The commits, newest first (`git log`'s order).
    pub commits: Vec<Commit>,
    /// git's own warnings, verbatim.
    pub warnings: Vec<String>,
    /// What the two git calls streamed.
    pub stats: StreamStats,
}

impl History {
    /// One commit's entry.
    #[must_use]
    pub fn commit(&self, sha: &str) -> Option<&Commit> {
        self.commits.iter().find(|commit| commit.sha == sha)
    }

    /// The SHAs, newest first.
    pub fn shas(&self) -> impl Iterator<Item = &str> {
        self.commits.iter().map(Commit::sha)
    }

    /// How many commits came back.
    #[must_use]
    pub fn len(&self) -> usize {
        self.commits.len()
    }

    /// Whether the window was empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.commits.is_empty()
    }
}

/// One commit, as git describes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Commit {
    /// The full 40-hex lowercase SHA (ADR-0008 §6.2: the key is never truncated).
    pub sha: String,
    /// The commit's tree, as git's own 40-hex object id.
    ///
    /// Free from `git log` (`%T`), and the store keys its content-addressed analysis of a commit's
    /// file set on it (ADR-0006 §6.3's `tree` input kind, §6.4's `sources.git.tree`): two commits with
    /// the same tree have the same MI, and that is only knowable if the tree travelled with the commit.
    pub tree: String,
    /// The parents, first-parent first.
    pub parents: Vec<String>,
    /// Author name, as written.
    pub author_name: String,
    /// Author email, as written.
    pub author_email: String,
    /// Author instant, Unix seconds (UTC by definition; `%at` is immune to locale).
    pub author_utc: i64,
    /// Committer name.
    pub committer_name: String,
    /// Committer email.
    pub committer_email: String,
    /// Committer instant, Unix seconds.
    pub committer_utc: i64,
    /// The message head, cut at [`SUBJECT_MAX_BYTES`] on a character boundary.
    pub subject: String,
    /// Whether the head was cut.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub subject_truncated: bool,
    /// The trailer map, empty when the message has no trailers.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub trailers: BTreeMap<String, String>,
    /// What this commit's diff was taken against — ADR-0008 §6.3, stated per row rather than
    /// assumed by the renderer.
    pub diff_base: DiffBase,
    /// The diffstat.
    pub diff: Diffstat,
}

impl Commit {
    /// The SHA.
    #[must_use]
    pub fn sha(&self) -> &str {
        &self.sha
    }

    /// Seven hex: the display form ADR-0008 §6.2 pins, and nothing else may truncate.
    #[must_use]
    pub fn short_id(&self) -> &str {
        let end = self
            .sha
            .char_indices()
            .nth(7)
            .map_or(self.sha.len(), |(i, _)| i);
        &self.sha[..end]
    }

    /// Whether this commit has more than one parent.
    #[must_use]
    pub fn is_merge(&self) -> bool {
        self.parents.len() > 1
    }

    /// Whether the diff base is the empty tree — a root commit, or a grafted shallow boundary.
    #[must_use]
    pub const fn is_root(&self) -> bool {
        matches!(self.diff_base, DiffBase::EmptyTree)
    }
}

/// What a commit's diff was taken against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DiffBase {
    /// A root commit, or a shallow clone's grafted boundary: there is no parent, so the diff is
    /// against the empty tree and every file in the commit is an add.
    EmptyTree,
    /// A normal commit: the diff is against this, its **first** parent (ADR-0008 §6.3), whatever
    /// else the commit has.
    Parent {
        /// The first parent's SHA.
        sha: String,
    },
}

/// One commit's diffstat: the kept file list, and the four totals.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct Diffstat {
    /// The kept per-file entries, in git's order. At most `Options::max_file_entries`.
    pub files: Vec<FileChange>,
    /// How many entries were dropped by the cap. Non-zero is a truncation flag with a count.
    #[serde(skip_serializing_if = "is_zero")]
    pub files_truncated: usize,
    /// Headline (`Scope::Source`) files.
    pub n_files: usize,
    /// Headline lines added — the `+` the timeline draws.
    pub n_add: u64,
    /// Headline lines deleted.
    pub n_del: u64,
    /// Headline files git reported as binary (`-`/`-`), which includes submodule pointer moves.
    pub n_binary: usize,
    /// Noise files.
    pub noise_files: usize,
    /// Noise lines added: counted, labelled, never in the headline.
    pub noise_add: u64,
    /// Noise lines deleted.
    pub noise_del: u64,
    /// Excluded files.
    pub excluded_files: usize,
    /// Excluded lines added.
    pub excluded_add: u64,
    /// Excluded lines deleted.
    pub excluded_del: u64,
    /// Every file git counted, whatever its scope.
    pub all_files: usize,
    /// Every line added, whatever its scope: equals `git diff --numstat` for this commit.
    pub n_add_all: u64,
    /// Every line deleted, whatever its scope.
    pub n_del_all: u64,
}

impl Diffstat {
    /// Whether the per-file list was cut short.
    #[must_use]
    pub const fn truncated(&self) -> bool {
        self.files_truncated > 0
    }
}

/// One file in one commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileChange {
    /// The path after the change, repo-relative with forward slashes. For a delete, the path that
    /// went away.
    pub path: String,
    /// The path before the change, present only for a rename or a copy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    /// What kind of change this was.
    pub kind: ChangeKind,
    /// The old mode, e.g. `100644`, absent when there was no old blob. Cheap: it comes from
    /// `--raw`, which the reader already asks for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode_old: Option<String>,
    /// The new mode, absent when the file is gone.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode_new: Option<String>,
    /// Lines added, 0 when git counted none (a pure rename, or a binary file).
    pub add: u64,
    /// Lines deleted.
    pub del: u64,
    /// Whether git counted no lines because the content is binary (`-`/`-`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub binary: bool,
    /// Which of the three scopes this path is in — the classifier's answer, stored per file so a
    /// detail view can say why a file is not in the headline.
    pub scope: Scope,
    /// The blob this commit moved the path *to*, as git's own object id; absent for a delete, whose
    /// `--raw` field is the all-zeros placeholder. Free (it is field 4 of the `--raw` record this
    /// reader already parsed), and the store leans on it: "did this path's content change?" is
    /// answerable from a SHA comparison, so an incremental index reads a blob only when its content
    /// actually moved (ADR-0006 §6.3's `blob` input kind is a content address, and git's oid is the
    /// same fact about the same bytes, taken from the source that already named the path).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blob_new: Option<String>,
    /// The blob the path was at before this commit; absent for an add, and for a change whose old
    /// side git reported as the all-zeros placeholder.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blob_old: Option<String>,
}

/// What happened to one file.
///
/// Binary is a *flag* on [`FileChange`] and not one of these variants, because a deleted binary file
/// is a delete and a binary file at once, and a kind list that picks one loses the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    /// `A`.
    Add,
    /// `M`.
    Modify,
    /// `D`.
    Delete,
    /// `R`: git matched it to a deleted file.
    Rename,
    /// `C`: git matched it to a file it also saw in this commit (see [`Copies`]).
    Copy,
    /// `T`: file mode changed from regular to symlink or gitlink, or the reverse.
    TypeChange,
    /// Anything git answers that is not one of the above, with its status letter.
    Other(char),
}

impl ChangeKind {
    /// The stored spelling.
    #[must_use]
    pub fn as_str(self) -> String {
        match self {
            Self::Add => "add".to_owned(),
            Self::Modify => "modify".to_owned(),
            Self::Delete => "delete".to_owned(),
            Self::Rename => "rename".to_owned(),
            Self::Copy => "copy".to_owned(),
            Self::TypeChange => "type_change".to_owned(),
            Self::Other(letter) => format!("other_{letter}"),
        }
    }
}

// --------------------------------------------------------------------------- parse: metadata

/// The commit-list parser.
///
/// `META_FORMAT` starts with a `NUL`, so the stream begins with one empty token and every commit
/// after it is exactly [`META_FIELDS`] tokens — position, not content, is what separates one field
/// from the next, which matters because a field may legitimately be empty (a root commit has no
/// parents, and a message can have no head at all).
#[derive(Default)]
struct MetaParser {
    fields: Vec<String>,
    started: bool,
    done: Vec<Commit>,
}

impl MetaParser {
    fn on_token(&mut self, token: &[u8]) -> Result<(), Error> {
        if !self.started {
            self.started = true;
            if token.is_empty() {
                return Ok(());
            }
        }
        self.fields
            .push(String::from_utf8_lossy(token).into_owned());
        if self.fields.len() == META_FIELDS {
            return self.flush();
        }
        Ok(())
    }

    fn finish(mut self) -> Vec<Commit> {
        // The caller has no error channel here only if `finish` cannot fail; it can, so the last
        // record is validated the same way and dropped only on a shape this module says is a bug.
        if !self.fields.is_empty() && self.flush().is_err() {
            self.done.pop();
        }
        self.done
    }

    fn flush(&mut self) -> Result<(), Error> {
        let fields = std::mem::take(&mut self.fields);
        if fields.len() != META_FIELDS {
            return Err(Error::Parse {
                detail: format!(
                    "expected {META_FIELDS} fields per commit, git gave {}",
                    fields.len()
                ),
            });
        }
        let mut fields = fields.into_iter();
        let sha = fields.next().unwrap_or_default();
        if !git::is_sha(&sha) {
            return Err(Error::Parse {
                detail: format!("`{sha}` is not a 40-hex commit id"),
            });
        }
        let tree = fields.next().unwrap_or_default();
        if !git::is_sha(&tree) {
            return Err(Error::Parse {
                detail: format!("`{tree}` is not a 40-hex tree id"),
            });
        }
        let parents = fields.next().unwrap_or_default();
        let parents = if parents.trim().is_empty() {
            Vec::new()
        } else {
            parents.split_whitespace().map(str::to_owned).collect()
        };
        let author_utc = fields.next().unwrap_or_default();
        let committer_utc = fields.next().unwrap_or_default();
        let mut commit = Commit {
            sha,
            tree,
            parents,
            author_name: fields.next().unwrap_or_default(),
            author_email: fields.next().unwrap_or_default(),
            author_utc: author_utc.trim().parse().unwrap_or_default(),
            committer_name: fields.next().unwrap_or_default(),
            committer_email: fields.next().unwrap_or_default(),
            committer_utc: committer_utc.trim().parse().unwrap_or_default(),
            subject: String::new(),
            subject_truncated: false,
            trailers: BTreeMap::new(),
            // Filled by the diff pass, which is the call that actually knows what it diffed
            // against (a root commit has no parent, and `--root` is what made its diff possible).
            diff_base: DiffBase::EmptyTree,
            diff: Diffstat::default(),
        };
        let subject = fields.next().unwrap_or_default();
        let (head, truncated) = truncate_bytes(&subject, SUBJECT_MAX_BYTES);
        commit.subject = head;
        commit.subject_truncated = truncated;
        let trailers = fields.next().unwrap_or_default();
        commit.trailers = parse_trailers(&trailers);
        self.done.push(commit);
        Ok(())
    }
}

/// The trailer map, out of what `%(trailers:only,unfold)` printed.
///
/// git has already decided which lines are trailers — the last paragraph, every line shaped like a
/// token — so "trivial" here means: split each line at the first `:`, trim, keep the ones that
/// parse. A repeated key (`Co-authored-by`, the common case) keeps the **last** value with the
/// count of how many there were appended, because a map is what a renderer wants and silently
/// dropping three co-authors into one is what a renderer must not do.
#[must_use]
pub fn parse_trailers(text: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty()
            || !key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            continue;
        }
        let value = value.trim();
        let count = seen.entry(key.to_owned()).or_insert(0);
        *count += 1;
        map.insert(key.to_owned(), value.to_owned());
    }
    for (key, count) in seen {
        if count > 1 {
            if let Some(value) = map.get_mut(&key) {
                value.push_str(&format!(" (+{} more)", count - 1));
            }
        }
    }
    map
}

/// Cut a string to `max` bytes on a character boundary, act-mcp's rule.
#[must_use]
pub fn truncate_bytes(text: &str, max: usize) -> (String, bool) {
    if text.len() <= max {
        return (text.to_owned(), false);
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), true)
}

// ----------------------------------------------------------------------------- parse: diffs

/// One raw `--raw` record: the modes, the two blob ids, the status and the path(s).
#[derive(Debug, Clone)]
struct RawEntry {
    mode_old: String,
    mode_new: String,
    blob_old: Option<String>,
    blob_new: Option<String>,
    status: char,
    old_path: Option<String>,
    path: String,
}

/// One `--numstat` record. `None` is git's `-`: binary content, and a submodule pointer move.
#[derive(Debug, Clone)]
struct NumstatEntry {
    add: Option<u64>,
    del: Option<u64>,
    old_path: Option<String>,
    path: String,
}

/// One commit's two blocks, before they are zipped into a [`Diffstat`].
#[derive(Debug, Clone)]
pub struct ParsedCommit {
    /// The commit the block belongs to.
    pub sha: String,
    raw: Vec<RawEntry>,
    num: Vec<NumstatEntry>,
}

impl ParsedCommit {
    /// Zip the `--raw` and `--numstat` blocks into the row's diffstat.
    ///
    /// git emits both blocks over the same changed-file list in the same order, so entry *i* of one
    /// is entry *i* of the other. A commit where the counts differ is a repository this module does
    /// not understand, and says so instead of pairing files up by guesswork.
    fn into_diffstat(
        self,
        classifier: &Classifier,
        max_file_entries: usize,
    ) -> Result<Diffstat, Error> {
        if self.raw.len() != self.num.len() {
            return Err(Error::Parse {
                detail: format!(
                    "{} has {} raw entries and {} numstat entries",
                    self.sha,
                    self.raw.len(),
                    self.num.len()
                ),
            });
        }
        let mut diff = Diffstat::default();
        for (raw, num) in self.raw.iter().zip(self.num.iter()) {
            // The two blocks name the same file; if they ever disagree, stop rather than attribute
            // one file's line counts to another.
            if raw.path != num.path || raw.old_path != num.old_path {
                return Err(Error::Parse {
                    detail: format!(
                        "{}: git's --raw says {} and --numstat says {}",
                        self.sha, raw.path, num.path
                    ),
                });
            }
            let binary = num.add.is_none() && num.del.is_none();
            let add = num.add.unwrap_or(0);
            let del = num.del.unwrap_or(0);
            let scope = classifier.classify(&raw.path);
            diff.all_files += 1;
            diff.n_add_all += add;
            diff.n_del_all += del;
            match scope {
                Scope::Source => {
                    diff.n_files += 1;
                    diff.n_add += add;
                    diff.n_del += del;
                    if binary {
                        diff.n_binary += 1;
                    }
                }
                Scope::Noise => {
                    diff.noise_files += 1;
                    diff.noise_add += add;
                    diff.noise_del += del;
                }
                Scope::Excluded => {
                    diff.excluded_files += 1;
                    diff.excluded_add += add;
                    diff.excluded_del += del;
                }
            }
            if diff.files.len() >= max_file_entries {
                // The cap is on the list, never on the arithmetic above.
                diff.files_truncated += 1;
                continue;
            }
            diff.files.push(FileChange {
                path: raw.path.clone(),
                old_path: raw.old_path.clone(),
                kind: match raw.status {
                    'A' => ChangeKind::Add,
                    'M' => ChangeKind::Modify,
                    'D' => ChangeKind::Delete,
                    'R' => ChangeKind::Rename,
                    'C' => ChangeKind::Copy,
                    'T' => ChangeKind::TypeChange,
                    other => ChangeKind::Other(other),
                },
                mode_old: mode(&raw.mode_old),
                mode_new: mode(&raw.mode_new),
                blob_old: raw.blob_old.clone(),
                blob_new: raw.blob_new.clone(),
                add,
                del,
                binary,
                scope,
            });
        }
        Ok(diff)
    }
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

/// [`blob_oid`] as a refusal: the two blob fields of a `--raw` record, either of which may be the
/// placeholder but neither of which may be anything else.
fn raw_blobs(
    old: &str,
    new: &str,
    record: &str,
) -> Result<(Option<String>, Option<String>), Error> {
    let side = |text: &str| -> Result<Option<String>, Error> {
        if text.chars().all(|c| c == '0') {
            return Ok(None);
        }
        if git::is_sha(text) {
            return Ok(Some(text.to_owned()));
        }
        Err(Error::Parse {
            detail: format!(
                "`{record}` names blob `{text}`, which is not a 40-hex object id — git abbreviated it,                  or this is not a --raw record"
            ),
        })
    };
    Ok((side(old)?, side(new)?))
}

/// `000000` is git's "no blob here", not a mode.
fn mode(raw: &str) -> Option<String> {
    if raw == "000000" || raw.is_empty() {
        None
    } else {
        Some(raw.to_owned())
    }
}
/// A `--raw` or `--numstat` record that has named its kind and still wants path token(s).
///
/// The paths are *positional*: git prints one token for a plain file and two (`old`, `new`) for a
/// rename or a copy, and the only way to know which a token is, is to have read the record header
/// that declared it. Matching on content instead would break the first time a file is named
/// `0\t1\t2` or starts with a colon.
#[derive(Debug)]
enum Pending {
    Raw {
        mode_old: String,
        mode_new: String,
        blob_old: Option<String>,
        blob_new: Option<String>,
        status: char,
        /// How many path tokens are still missing: 1 for an ordinary change, 2 for a rename or copy
        /// (old, then new). Every `--raw` record leaves its paths in the following tokens.
        want: usize,
        /// The old path of a rename or copy, once the first of the two arrived.
        old_path: Option<String>,
    },
    Num {
        add: Option<u64>,
        del: Option<u64>,
        /// A `--numstat` record only goes pending for a rename or copy, so this is 2 then 1.
        want: usize,
        old_path: Option<String>,
    },
}

/// The diffstat stream parser.
///
/// The grammar, one `NUL`-separated token at a time — `crates/fathom/tests/git_history.rs` pins
/// every line of it against a real repository, which is the only honest way to hold a git CLI to its
/// word:
///
/// * `\x1e<sha>` — a commit header. With `-z` the header ends at the `NUL`, so this token is only
///   ever the marker and the id, and never user text.
/// * a token starting with `:` — a `--raw` record: `:old new oldsha newsha STATUS`, followed by one
///   path token, or two (`old`, `new`) when the status is `R` or `C`.
/// * a token containing a tab — a `--numstat` record: `add\tdel\tpath`, where the path is empty for
///   a rename or a copy and the two names follow as separate tokens. `-` where a number goes is
///   git's "binary, no line counts".
/// * an empty or newline-only token — the blank line git puts between a header and its diff.
///
/// git flushes every `--raw` record of a commit before any `--numstat` record, and both blocks walk
/// the same changed-file list in the same order, which is what lets [`ParsedCommit::into_diffstat`]
/// zip them by index instead of pairing paths up by name.
#[derive(Default)]
struct DiffParser {
    current: Option<ParsedCommit>,
    /// The next token carries the newline git prints after the header, which with `-z` lands at the
    /// front of the following token.
    fresh: bool,
    pending: Option<Pending>,
    done: Vec<ParsedCommit>,
}

impl DiffParser {
    fn on_token(&mut self, token: &[u8]) -> Result<(), Error> {
        if let Some(head) = token.strip_prefix(b"\x1e") {
            if self.pending.is_some() {
                return Err(Error::Parse {
                    detail: "a commit header arrived while a change record was still open"
                        .to_owned(),
                });
            }
            self.flush()?;
            let sha = String::from_utf8_lossy(head).into_owned();
            if !git::is_sha(&sha) {
                return Err(Error::Parse {
                    detail: format!("`{sha}` is not a 40-hex commit id"),
                });
            }
            self.current = Some(ParsedCommit {
                sha,
                raw: Vec::new(),
                num: Vec::new(),
            });
            self.fresh = true;
            return Ok(());
        }

        // Two disjoint fields of `self`, borrowed separately so the helpers below stay free functions
        // and the borrow checker can see they do not overlap.
        let current = self.current.as_mut().ok_or_else(|| Error::Parse {
            detail: format!(
                "git printed `{}` before any commit header",
                String::from_utf8_lossy(token)
            ),
        })?;
        let pending = &mut self.pending;
        let mut token = token;
        if self.fresh {
            token = token.strip_prefix(b"\n").unwrap_or(token);
            self.fresh = false;
        }

        // A path belongs to the record that asked for it, and is never interpreted.
        if let Some(held) = pending.take() {
            return finish_pending(current, pending, held, token);
        }

        if token.is_empty() || token.iter().all(|byte| *byte == b'\n') {
            return Ok(());
        }
        if token.first() == Some(&b':') {
            read_raw(current, pending, token)
        } else if token.contains(&b'\t') {
            read_numstat(current, pending, token)
        } else {
            Err(Error::Parse {
                detail: format!(
                    "in {}: `{}` is neither a --raw record nor a --numstat record",
                    current.sha,
                    String::from_utf8_lossy(token)
                ),
            })
        }
    }

    fn flush(&mut self) -> Result<(), Error> {
        if let Some(current) = self.current.take() {
            self.done.push(current);
        }
        Ok(())
    }

    fn finish(mut self) -> Vec<ParsedCommit> {
        if self.flush().is_err() {
            self.done.pop();
        }
        self.done
    }
}

fn read_raw(
    current: &mut ParsedCommit,
    pending: &mut Option<Pending>,
    token: &[u8],
) -> Result<(), Error> {
    let text = String::from_utf8_lossy(token).into_owned();
    let mut fields = text[1..].split_ascii_whitespace();
    let (Some(mode_old), Some(mode_new), Some(blob_old), Some(blob_new), Some(status)) = (
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
    ) else {
        return Err(Error::Parse {
            detail: format!("`{text}` is not a --raw record"),
        });
    };
    let letter = status.chars().next().ok_or_else(|| Error::Parse {
        detail: format!("`{text}` has no status letter"),
    })?;
    if letter == 'U' {
        // An unmerged entry is what `--diff-merges=first-parent` exists to prevent; if one ever
        // arrived, guessing which parent it came from is worse than refusing.
        return Err(Error::Parse {
            detail: format!("{} has an unmerged entry `{text}`", current.sha),
        });
    }
    // Every raw record names its paths in the tokens that follow it — one for an ordinary change,
    // two for a rename or a copy — so nothing here can read a path out of the header itself.
    let (blob_old, blob_new) = raw_blobs(blob_old, blob_new, &text)?;
    *pending = Some(Pending::Raw {
        mode_old: mode_old.to_owned(),
        mode_new: mode_new.to_owned(),
        blob_old,
        blob_new,
        status: letter,
        want: usize::from(matches!(letter, 'R' | 'C')) + 1,
        old_path: None,
    });
    Ok(())
}

fn read_numstat(
    current: &mut ParsedCommit,
    pending: &mut Option<Pending>,
    token: &[u8],
) -> Result<(), Error> {
    let text = String::from_utf8_lossy(token).into_owned();
    let mut fields = text.splitn(3, '\t');
    let (Some(add), Some(del), Some(rest)) = (fields.next(), fields.next(), fields.next()) else {
        return Err(Error::Parse {
            detail: format!("`{text}` is not a --numstat record"),
        });
    };
    // A count is a number, or `-` for "no line counts" (binary content, and a submodule pointer
    // move). Both readings are `Option`, so a `-` must not look like a parse failure and a parse
    // failure must not look like a `-`.
    let count = |field: &str| -> Option<Option<u64>> {
        if field == "-" {
            Some(None)
        } else {
            field.parse::<u64>().ok().map(Some)
        }
    };
    let (Some(add), Some(del)) = (count(add), count(del)) else {
        return Err(Error::Parse {
            detail: format!("`{text}` has a count that is neither a number nor `-`"),
        });
    };
    if !rest.is_empty() {
        current.num.push(NumstatEntry {
            add,
            del,
            old_path: None,
            path: rest.to_owned(),
        });
        return Ok(());
    }
    // A rename or a copy: the two names come as their own tokens.
    *pending = Some(Pending::Num {
        add,
        del,
        want: 2,
        old_path: None,
    });
    Ok(())
}

/// Hand a path token to the record that asked for it.
fn finish_pending(
    current: &mut ParsedCommit,
    pending: &mut Option<Pending>,
    held: Pending,
    token: &[u8],
) -> Result<(), Error> {
    let path = text_path(token)?;
    match held {
        Pending::Raw {
            mode_old,
            mode_new,
            blob_old,
            blob_new,
            status,
            want: 1,
            old_path,
        } => current.raw.push(RawEntry {
            mode_old,
            mode_new,
            blob_old,
            blob_new,
            status,
            old_path,
            path,
        }),
        Pending::Raw {
            mode_old,
            mode_new,
            blob_old,
            blob_new,
            status,
            want: 2,
            ..
        } => {
            *pending = Some(Pending::Raw {
                mode_old,
                mode_new,
                blob_old,
                blob_new,
                status,
                want: 1,
                old_path: Some(path),
            });
        }
        Pending::Raw { .. } => {
            return Err(Error::Parse {
                detail: "a --raw record asked for more than two paths".to_owned(),
            })
        }
        Pending::Num {
            add,
            del,
            want: 1,
            old_path,
        } => current.num.push(NumstatEntry {
            add,
            del,
            old_path,
            path,
        }),
        Pending::Num {
            add, del, want: 2, ..
        } => {
            *pending = Some(Pending::Num {
                add,
                del,
                want: 1,
                old_path: Some(path),
            });
        }
        Pending::Num { .. } => {
            return Err(Error::Parse {
                detail: "a --numstat record asked for more than two paths".to_owned(),
            })
        }
    }
    Ok(())
}

/// A path token is bytes, and a path is not required to be UTF-8. `Lossy` is the honest answer: the
/// row is a display and an index key, and a replacement character in a path is a visible signal that
/// the path is odd rather than a silently dropped file.
fn text_path(token: &[u8]) -> Result<String, Error> {
    Ok(String::from_utf8_lossy(token).into_owned())
}

/// git's stderr, as lines worth showing.
fn split_warnings(stderr: &str) -> Vec<String> {
    stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed a literal stream of tokens (the byte grammar, without a repository in the way).
    fn parse_diff(stream: &[Vec<u8>]) -> Result<Vec<ParsedCommit>, Error> {
        let mut parser = DiffParser::default();
        for token in stream {
            parser.on_token(token)?;
        }
        Ok(parser.finish())
    }

    fn tokens(parts: &[&str]) -> Vec<Vec<u8>> {
        parts.iter().map(|part| part.as_bytes().to_vec()).collect()
    }

    #[test]
    fn a_plain_commit_reads_its_one_file() {
        let sha = "f".repeat(40);
        let stream = tokens(&[
            &format!("\x1e{sha}"),
            "\n:000000 100644 0000000 e84fa9b000000000000000000000000000000000 A",
            "notes.md",
            "3\t1\tnotes.md",
        ]);
        let commits = parse_diff(&stream).expect("parse");
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].sha, sha);
        assert_eq!(commits[0].raw.len(), 1);
        assert_eq!(commits[0].num[0].path, "notes.md");
        assert_eq!(commits[0].num[0].add, Some(3));
        assert_eq!(commits[0].num[0].del, Some(1));
    }

    #[test]
    fn a_rename_takes_two_paths_in_each_block() {
        let sha = "e".repeat(40);
        let stream = tokens(&[
            &format!("\x1e{sha}"),
            "\n:100644 100644 94954ab000000000000000000000000000000000 94954ab000000000000000000000000000000000 R100",
            "src/old.rs",
            "src/new.rs",
            "0\t0\t",
            "src/old.rs",
            "src/new.rs",
        ]);
        let commits = parse_diff(&stream).expect("parse");
        assert_eq!(commits[0].raw[0].old_path.as_deref(), Some("src/old.rs"));
        assert_eq!(commits[0].raw[0].path, "src/new.rs");
        assert_eq!(commits[0].num[0].old_path.as_deref(), Some("src/old.rs"));
        assert_eq!(commits[0].num[0].path, "src/new.rs");
    }

    #[test]
    fn a_binary_file_is_two_dashes_and_no_lines() {
        let sha = "d".repeat(40);
        let stream = tokens(&[
            &format!("\x1e{sha}"),
            "\n:000000 100644 0000000 d023287000000000000000000000000000000000 A",
            "blob.bin",
            "-\t-\tblob.bin",
        ]);
        let commits = parse_diff(&stream).expect("parse");
        assert_eq!(commits[0].num[0].add, None);
        assert_eq!(commits[0].num[0].del, None);
    }

    #[test]
    fn a_path_may_contain_a_tab() {
        let sha = "c".repeat(40);
        let stream = tokens(&[
            &format!("\x1e{sha}"),
            "\n:000000 100644 0000000 8d331fb000000000000000000000000000000000 A",
            "tab\tname.txt",
            "1\t0\ttab\tname.txt",
        ]);
        let commits = parse_diff(&stream).expect("parse");
        assert_eq!(commits[0].raw[0].path, "tab\tname.txt");
        assert_eq!(commits[0].num[0].path, "tab\tname.txt");
    }

    #[test]
    fn a_record_that_never_got_its_paths_is_a_refusal_not_a_wrong_row() {
        let sha = "b".repeat(40);
        let stream = tokens(&[
            &format!("\x1e{sha}"),
            "\n:100644 100644 94954ab000000000000000000000000000000000 94954ab000000000000000000000000000000000 R100",
            "only-one-path",
            &format!("\x1e{}", "a".repeat(40)),
        ]);
        let err = parse_diff(&stream).expect_err("a header mid-record is a broken stream");
        assert!(err.to_string().contains("open"), "{err}");
    }

    #[test]
    fn zipping_two_blocks_that_disagree_is_a_refusal() {
        let sha = "a".repeat(40);
        let parsed = ParsedCommit {
            sha: sha.clone(),
            raw: vec![RawEntry {
                mode_old: "100644".to_owned(),
                mode_new: "100644".to_owned(),
                blob_old: None,
                blob_new: None,
                status: 'M',
                old_path: None,
                path: "a.rs".to_owned(),
            }],
            num: vec![
                NumstatEntry {
                    add: Some(1),
                    del: Some(0),
                    old_path: None,
                    path: "a.rs".to_owned(),
                },
                NumstatEntry {
                    add: Some(1),
                    del: Some(0),
                    old_path: None,
                    path: "b.rs".to_owned(),
                },
            ],
        };
        let err = parsed
            .into_diffstat(&Classifier::default(), 10)
            .expect_err("one raw entry cannot cover two numstat entries");
        assert!(err.to_string().contains("raw entries"), "{err}");
    }

    #[test]
    fn the_cap_drops_entries_and_keeps_the_arithmetic() {
        let sha = "9".repeat(40);
        let parsed = ParsedCommit {
            sha,
            raw: (0..5)
                .map(|i| RawEntry {
                    mode_old: "100644".to_owned(),
                    mode_new: "100644".to_owned(),
                    blob_old: None,
                    blob_new: None,
                    status: 'M',
                    old_path: None,
                    path: format!("src/f{i}.rs"),
                })
                .collect(),
            num: (0..5)
                .map(|i| NumstatEntry {
                    add: Some(10),
                    del: Some(1),
                    old_path: None,
                    path: format!("src/f{i}.rs"),
                })
                .collect(),
        };
        let diff = parsed
            .into_diffstat(&Classifier::default(), 2)
            .expect("zip");
        assert_eq!(diff.files.len(), 2);
        assert_eq!(diff.files_truncated, 3);
        assert_eq!(diff.n_add, 50, "the cap bounds the list, not the totals");
        assert_eq!(diff.all_files, 5);
        assert!(diff.truncated());
    }

    #[test]
    fn the_three_scopes_sum_to_what_git_counted() {
        let paths = [
            "src/lib.rs",
            "Cargo.lock",
            ".evidence/anything.jsonl",
            "src/other.rs",
        ];
        let parsed = ParsedCommit {
            sha: "8".repeat(40),
            raw: paths
                .iter()
                .map(|path| RawEntry {
                    mode_old: "100644".to_owned(),
                    mode_new: "100644".to_owned(),
                    blob_old: None,
                    blob_new: None,
                    status: 'M',
                    old_path: None,
                    path: (*path).to_owned(),
                })
                .collect(),
            num: paths
                .iter()
                .map(|path| NumstatEntry {
                    add: Some(10),
                    del: Some(0),
                    old_path: None,
                    path: (*path).to_owned(),
                })
                .collect(),
        };
        let diff = parsed
            .into_diffstat(&Classifier::default(), 10)
            .expect("zip");
        assert_eq!(diff.n_files, 2, "two source files");
        assert_eq!(diff.n_add, 20, "the headline is the source set only");
        assert_eq!(diff.noise_add, 10, "the lockfile is labelled, not hidden");
        assert_eq!(diff.excluded_add, 10, "and so is the evidence path");
        assert_eq!(
            diff.n_add_all, 40,
            "and every line git counted is somewhere"
        );
        assert_eq!(
            diff.n_add + diff.noise_add + diff.excluded_add,
            diff.n_add_all
        );
        assert_eq!(diff.all_files, paths.len());
    }

    #[test]
    fn trailers_read_as_a_map_and_a_head_is_cut_on_a_boundary() {
        let trailers = parse_trailers(
            "Assisted-by: beads\nCo-authored-by: A <a@e>\nCo-authored-by: B <b@e>\n\n",
        );
        assert_eq!(
            trailers.get("Assisted-by").map(String::as_str),
            Some("beads")
        );
        assert_eq!(
            trailers.get("Co-authored-by").map(String::as_str),
            Some("B <b@e> (+1 more)"),
            "a repeated trailer keeps its count instead of losing an author"
        );
        assert!(parse_trailers("body text, no colon here").is_empty());
        assert!(parse_trailers("Not A Trailer: nope").is_empty());

        let long = "é".repeat(400);
        let (head, truncated) = truncate_bytes(&long, SUBJECT_MAX_BYTES);
        assert!(truncated);
        assert!(head.len() <= SUBJECT_MAX_BYTES);
        assert!(!head.chars().next().is_none());
        assert!(
            std::str::from_utf8(head.as_bytes()).is_ok(),
            "cut on a boundary"
        );
    }

    #[test]
    fn a_mode_of_zero_is_no_mode_at_all() {
        assert_eq!(mode("000000"), None);
        assert_eq!(mode("100644").as_deref(), Some("100644"));
        assert_eq!(mode("120000").as_deref(), Some("120000"));
    }
}
