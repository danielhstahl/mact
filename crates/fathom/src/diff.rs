//! One commit's patch, read the way a diff view needs it: streamed, capped, and honest about both.
//!
//! The timeline's one named interaction is `d` on a commit, and this module is what answers it. It is
//! a sibling of [`crate::history`] rather than a part of it: the history reader is the *stat* reader —
//! it deliberately never asks git for patch text, because a timeline of 500 commits must not read a
//! repository's content — and this one asks for exactly one commit's content, on demand, with a cap.
//!
//! ## The three calls, and why they are three
//!
//! ```text
//! git log -1 --format=… --raw --numstat -z   the file list: path, kind, mode, +/-, blob ids, scope
//! git show --format= -p --diff-merges=first-parent --root <sha>   the patch text itself
//! ```
//!
//! The first is [`crate::history::Reader`] run over a one-commit window, and it is what the file pane's
//! per-file `+`/`−` come from — the same reader, the same classifier and the same rename detection that
//! produced the timeline row's `+`/`−` column, so **the two panes cannot disagree about what a commit
//! did** (the ADR-0007 §7.1 argument, one level down). Re-deriving the counts by counting `+` and `−`
//! characters in the patch would give different numbers for a binary file (no hunks), for a `\ No
//! newline at end of file` (one more `−` than git counts), and for a rename (git's `--numstat` says
//! what the pair cost; the patch says what the diff looks like).
//!
//! The patch is matched to the stat list **by position**: `--raw` and `-p` walk the same diff queue, so
//! the *n*th `diff --git` header is the *n*th `--raw` record. Position is what the parser trusts and the
//! parsed path is only the check on it, because `diff --git a/x b/y` is not reliably splittable when a
//! path contains a space (git quotes it only for *unusual* bytes, and a space is not one). A commit
//! whose two answers disagree is reported in [`CommitDiff::warnings`] and rendered from the stat list
//! with a note, never guessed past.
//!
//! ## The caps, and what honesty costs at them
//!
//! Two byte caps, both named in the header line when they bite:
//!
//! | cap | flag | default | stops |
//! |---|---|---|---|
//! | whole commit | `--max-diff-bytes` | 1 MiB | the `git show` run itself — the child is killed, not finished and ignored |
//! | one file | `--max-diff-bytes-per-file` | 128 KiB | that file's patch text; the run continues with the next file |
//!
//! plus [`MAX_PATCH_LINE_BYTES`], which is a memory bound rather than a display one: [`Git::stream_lines`]
//! never holds more than one line, and a line longer than that is cut and marked.
//!
//! The view says so when any of them bites, states the limit and names the flag to raise it, because a
//! silently cut diff is worse than no diff: it reads as "this commit only touched that". That is
//! [`CommitDiff::truncation`] and [`CommitDiff::banner`], and it is why [`CommitDiff::bytes_streamed`] is
//! reported as a *floor* on the commit's size rather than as a total — the run was stopped, so git never
//! told us where its output ended.
//!
//! ## The diff base is the first parent, and the row says which it was
//!
//! ADR-0008 §6.3, and it is not a display detail: a merge commit's patch is against its **first**
//! parent (`--diff-merges=first-parent`, the same flag the history reader needs for the same rule), a
//! root commit — or a grafted shallow boundary, which is the same shape — is diffed against the empty
//! tree (`--root`), and [`CommitDiff::diff_base`] carries which of the two the header must say. A root
//! commit is never "vs parent", and a merge is never judged against the branch it merged *in*.
//!
//! ## What is not here
//!
//! No syntax highlighting (v1 renders hunk headers, adds, deletes, context and file headers, and says
//! so in the view's own status line), no word-level diff, no working-tree content, and no writing:
//! `git show`/`git diff-tree` are read-only plumbing and `Git::command` keeps `GIT_OPTIONAL_LOCKS=0`.

use std::fmt::Write as _;

use thiserror::Error;

use crate::classify::{Classifier, Scope};
use crate::git::{Git, GitError, StreamStats};
use crate::history::{self, ChangeKind, DiffBase, Query};

/// The whole-commit cap's default: 1 MiB of patch text per commit.
///
/// Chosen against the case the ticket names — a 40 000-line reformat — which is several megabytes of
/// patch and is not what a 80×24 terminal can show anyway. It is a *view* limit, so it is a flag
/// ([`MAX_DIFF_BYTES_FLAG`]) and never a `fathom.toml` key: ADR-0006 §7.2 declares every key a reader
/// may read, and a value that changes what a screen shows without changing a digest is a view flag.
pub const DEFAULT_MAX_DIFF_BYTES: usize = 1 << 20;

/// The per-file cap's default: 128 KiB of patch text for one file.
///
/// The reason it is per-file as well as per-commit is that one file is what the diff pane scrolls
/// through: a single generated file would otherwise spend the whole commit budget on lines nobody can
/// read, and the other files in the commit would vanish without a word.
pub const DEFAULT_MAX_DIFF_BYTES_PER_FILE: usize = 1 << 17;

/// The flag that raises the whole-commit cap, spelled once so the banner and the CLI cannot drift.
pub const MAX_DIFF_BYTES_FLAG: &str = "--max-diff-bytes";

/// The flag that raises the per-file cap.
pub const MAX_DIFF_BYTES_PER_FILE_FLAG: &str = "--max-diff-bytes-per-file";

/// The longest single line this reader will hold, in bytes.
///
/// A memory bound, not a display one: the display truncates at the pane's width whatever this is. One
/// minified `vendor/*.js` line is megabytes, and holding it whole to paint 80 cells of it is the
/// difference between a view that reads a big commit and one that swaps.
pub const MAX_PATCH_LINE_BYTES: usize = 64 * 1024;

/// What a caller is willing to hold for one commit's diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The whole-commit cap ([`MAX_DIFF_BYTES_FLAG`]).
    pub total: usize,
    /// The per-file cap ([`MAX_DIFF_BYTES_PER_FILE_FLAG`]).
    pub per_file: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            total: DEFAULT_MAX_DIFF_BYTES,
            per_file: DEFAULT_MAX_DIFF_BYTES_PER_FILE,
        }
    }
}

impl Limits {
    /// The limits two flags ask for, with the defaults filling the gaps.
    ///
    /// A per-file cap larger than the whole-commit cap is meaningless, so it is clamped down rather than
    /// honoured: the commit cap is the one that bounds memory, and a per-file limit above it could only
    /// ever be reached by the commit cap first.
    #[must_use]
    pub fn from_args(total: Option<usize>, per_file: Option<usize>) -> Self {
        let defaults = Self::default();
        let total = total.unwrap_or(defaults.total);
        Self {
            total,
            per_file: per_file.unwrap_or(defaults.per_file).min(total).max(1),
        }
    }
}

/// What can stop a commit's patch being read.
#[allow(
    missing_docs,
    reason = "the #[error] format string documents each variant and its fix line"
)]
#[derive(Debug, Error)]
pub enum Error {
    /// git refused, was too old, or was not there.
    #[error(transparent)]
    Git(#[from] GitError),

    /// The one-commit history read (the file list) failed.
    #[error(transparent)]
    History(#[from] history::Error),

    /// A glob in `fathom.toml` will not compile, so a file's scope could not be decided and the file list
    /// could not be filtered.
    #[error(transparent)]
    Classify(#[from] crate::classify::GlobError),

    /// git answered the patch call with nothing where a commit was named — an answer, not a crash, but
    /// not something the view can draw a file list for either.
    #[error("cannot read the diff of {revision}: {detail}\n  fix: `git show {revision}` in the workspace says what git thinks of that revision")]
    Unreadable { revision: String, detail: String },
}

/// One commit's diff: the file list, the patch text that fits, and what the caps cost.
#[derive(Debug, Clone)]
pub struct CommitDiff {
    /// The full SHA the diff was read for.
    pub sha: String,
    /// Seven hex, ADR-0008 §6.2's display form.
    pub short_id: String,
    /// The message head.
    pub subject: String,
    /// Every parent, first-parent first. More than one means a merge, which the header says out loud.
    pub parents: Vec<String>,
    /// What the diff was taken against — ADR-0008 §6.3 again, so the header never has to guess.
    pub diff_base: DiffBase,
    /// The file list, in git's own order, every scope present.
    pub files: Vec<FileDiff>,
    /// The commit's own totals, by scope: the headline (source) set and the all-files set the `t`
    /// toggle switches the header to.
    pub totals: Totals,
    /// The caps' bill, when they bit.
    pub truncation: Option<Truncation>,
    /// Patch text kept, in bytes. The promise the huge-commit test asserts: `<= limits.total`.
    pub bytes_retained: usize,
    /// git's output read before the run stopped. A floor on the commit's real size, never a total.
    pub bytes_streamed: u64,
    /// The limits that were in force, because a banner that quotes a limit must quote *this* one.
    pub limits: Limits,
    /// git's own warnings, plus this module's own findings about a disagreeing pair of calls.
    pub warnings: Vec<String>,
}

impl CommitDiff {
    /// Whether this commit changed nothing at all — an empty commit, or a merge with nothing to show.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// How many files are in the headline (source) set — the one the timeline's `+`/`−` column drew and
    /// the one the file pane shows until `t` says otherwise.
    #[must_use]
    pub fn source_files(&self) -> usize {
        self.files
            .iter()
            .filter(|file| file.scope == Scope::Source)
            .count()
    }

    /// How many files the default view hides: noise (lockfiles, vendored, tests) and excluded.
    #[must_use]
    pub fn hidden_files(&self) -> usize {
        self.files.len().saturating_sub(self.source_files())
    }

    /// The honest line about the caps, or `None` when nothing was cut.
    ///
    /// Every form names what was cut, the limit that did it, and the flag that raises it, and none of
    /// them claims to know the size of what it did not read.
    #[must_use]
    pub fn banner(&self) -> Option<String> {
        let cut = self.truncation.as_ref()?;
        let mut line = String::new();
        if cut.total_cut {
            // The configured cap rather than the bytes kept: "cut at 7 KiB" beside a flag set to 8 KiB
            // reads like an off-by-one, and the cap is the number the reader can act on. What was kept is
            // said by the file the pane is on, not by the bill for the whole commit.
            let _ = write!(
                line,
                "diff cut at the {} limit ({MAX_DIFF_BYTES_FLAG}) \u{b7} {} of {} files not shown, {} cut short",
                bytes(cut.limits.total),
                cut.files_unseen,
                self.files.len(),
                cut.files_cut
            );
        } else {
            let _ = write!(
                line,
                "{} file{} cut at {} each ({MAX_DIFF_BYTES_PER_FILE_FLAG})",
                cut.files_cut,
                if cut.files_cut == 1 { "" } else { "s" },
                bytes(cut.limits.per_file)
            );
        }
        let _ = write!(line, " \u{b7} raise the flag to see the rest");
        Some(line)
    }
}

/// One commit's totals, split the way the two views need them.
///
/// The headline numbers are the timeline's, computed by the history reader over `Scope::Source` only;
/// the `all` set is what `t` shows, and `noise` is the difference the view has to be able to point at.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    /// Headline lines added.
    pub add: u64,
    /// Headline lines deleted.
    pub del: u64,
    /// Headline files.
    pub files: usize,
    /// Files outside the headline set, i.e. what the default view hides.
    pub hidden: usize,
    /// Every file git counted.
    pub files_all: usize,
    /// Every line added, whatever the scope.
    pub add_all: u64,
    /// Every line deleted, whatever the scope.
    pub del_all: u64,
}

impl Totals {
    /// The totals for the file set in view: the headline set, or everything.
    #[must_use]
    pub const fn shown(&self, show_all: bool) -> (u64, u64, usize) {
        if show_all {
            (self.add_all, self.del_all, self.files_all)
        } else {
            (self.add, self.del, self.files)
        }
    }
}

/// What the byte caps cost on this commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Truncation {
    /// Whether the whole-commit cap closed the `git show` run.
    pub total_cut: bool,
    /// How many files the per-file cap cut short.
    pub files_cut: usize,
    /// How many files in the list the patch text never reached.
    pub files_unseen: usize,
    /// Patch text kept, in bytes.
    pub retained: usize,
    /// git's output read before the run stopped, which is a floor and not a total.
    pub streamed: u64,
    /// The limits that were in force.
    pub limits: Limits,
}

/// One file in one commit: what the stat call said, and the patch text that fits.
#[derive(Debug, Clone)]
pub struct FileDiff {
    /// The path after the change.
    pub path: String,
    /// The path before it, for a rename or a copy — the pane renders it as `old -> new`.
    pub old_path: Option<String>,
    /// Add, modify, delete, rename, copy, type change.
    pub kind: ChangeKind,
    /// Lines added, from `--numstat`. Zero for a binary file, and that is not a claim that nothing
    /// changed, which is what [`FileDiff::binary`] is for.
    pub add: u64,
    /// Lines deleted.
    pub del: u64,
    /// Whether git counted no lines because the content is binary.
    pub binary: bool,
    /// Which scope this path is in — the same classifier answer the timeline row carries, and what the
    /// `t` toggle filters on.
    pub scope: crate::classify::Scope,
    /// The patch text, in order, capped. Empty for a binary file, a pure rename, or a file the
    /// whole-commit cap never reached.
    pub lines: Vec<Line>,
    /// How many hunks came through.
    pub hunks: usize,
    /// Whether the per-file cap cut this file's text short.
    pub cut: bool,
    /// Whether the whole-commit cap meant this file's text never started.
    pub unseen: bool,
    /// How much of this file's patch text was dropped after the per-file cap bit, in bytes. Counted so
    /// the view can say "and N more bytes we did not keep" rather than going quiet.
    pub dropped: usize,
}

impl FileDiff {
    /// The pane's label: `old -> new` for a rename, then the counts or the binary mark.
    #[must_use]
    pub fn label(&self) -> String {
        let path = match &self.old_path {
            Some(old) => format!("{old} -> {}", self.path),
            None => self.path.clone(),
        };
        // The ticket's mark is `[binary]`, and it replaces the counts rather than joining them: a
        // binary file's `+0 -0` is git saying "I did not count", and drawing it as a measurement is
        // the one thing the numstat `-`/`-` cannot survive.
        let counts = if self.binary {
            "[binary]".to_owned()
        } else {
            format!("+{} -{}", self.add, self.del)
        };
        format!("{path} {counts}")
    }

    /// Whether the diff pane has nothing to draw for this file and has to say why.
    #[must_use]
    pub fn no_text(&self) -> Option<&'static str> {
        if self.binary {
            // First, and ahead of the `lines.is_empty()` test below: a binary file's patch *does* have
            // lines (git's own header and `Binary files … differ`), and saying nothing about the content
            // it cannot show is the whole point of the note.
            return Some("[binary] no hunks to show: fathom does not print binary content");
        }
        if self.unseen {
            // The whole-commit cap closed the run before this file. Saying nothing here would read as
            // "this file is empty", which is the exact lie the caps section exists to prevent.
            return Some("not read: the whole-commit byte cap closed the diff before this file");
        }
        if !self.lines.is_empty() {
            return None;
        }
        // A pure rename or a mode-only change really does have no hunks; git's own header lines are the
        // content, and there were none.
        Some("no hunks: this change has no line content to show")
    }
}

/// One line of patch text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    /// What kind of line it is, which is the only thing the renderer needs for colour.
    pub kind: LineKind,
    /// The line exactly as git printed it, sign character and all — the diff, not a paraphrase of it.
    pub text: String,
    /// Whether [`MAX_PATCH_LINE_BYTES`] cut this line short.
    pub cut: bool,
}

/// A patch line's role, decided by git's own grammar and by where in a file's section it sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// A file header line: `diff --git`, `index`, `new file mode`, `--- a/x`, `+++ b/x`, `rename from`,
    /// `Binary files … differ`. Anything before the first `@@`.
    Meta,
    /// `@@ -1183,6 +1183,12 @@ impl CoverageTotal {`
    Hunk,
    /// A `+` line.
    Add,
    /// A `-` line.
    Delete,
    /// A ` ` line, and git's `\ No newline at end of file`.
    Context,
}

/// Read one commit's diff: the file list from the history reader, the patch text from `git show`.
///
/// `rev` is anything `git rev-parse` takes (`git show`'s own contract); it is resolved to a full SHA
/// before anything is displayed, so a short SHA in an argument never reaches a heading as if it were an
/// identity.
///
/// # Errors
///
/// [`Error::History`] for a repository with no commits, an unknown revision, or a git too old for
/// ADR-0008 §6.3's rule (the same refusals [`crate::history`] makes, because this is that reader with a
/// one-commit window); [`Error::Git`] if the patch call itself fails.
pub fn commit(
    git: &Git,
    classifier: &Classifier,
    rev: &str,
    limits: Limits,
) -> Result<CommitDiff, Error> {
    let query = Query {
        to: rev.to_owned(),
        from: None,
        // One commit: the stat call's window is the commit whose patch is being read, and nothing else.
        depth: 1,
        first_parent: false,
    };
    let mut history = history::Reader::new(classifier.clone()).read_with(git, &query)?;
    let one = history
        .commit(&history.to)
        .or_else(|| history.commits.first())
        .ok_or_else(|| Error::Unreadable {
            revision: rev.to_owned(),
            detail: "git named no commit for that revision".to_owned(),
        })?;

    // Everything the heading needs, taken out of the borrow before the patch stream starts: the reader
    // below owns `files` mutably, and git's warnings are the history read's own last words.
    let short_id = one.short_id().to_owned();
    let sha = one.sha.clone();
    let subject = one.subject.clone();
    let parents = one.parents.clone();
    let diff_base = one.diff_base.clone();

    let mut files: Vec<FileDiff> = one
        .diff
        .files
        .iter()
        .map(|change| FileDiff {
            path: change.path.clone(),
            old_path: change.old_path.clone(),
            kind: change.kind,
            add: change.add,
            del: change.del,
            binary: change.binary,
            scope: change.scope,
            lines: Vec::new(),
            hunks: 0,
            cut: false,
            unseen: true,
            dropped: 0,
        })
        .collect();
    let stat = &one.diff;
    let totals = Totals {
        add: stat.n_add,
        del: stat.n_del,
        files: stat.n_files,
        hidden: stat.noise_files + stat.excluded_files,
        files_all: stat.all_files,
        add_all: stat.n_add_all,
        del_all: stat.n_del_all,
    };
    let git_warnings = std::mem::take(&mut history.warnings);

    // --- the patch text ---------------------------------------------------------------------------
    // `--format=` with an empty format is what keeps the commit header (and its message) out of the
    // stream: the view shows the subject in its own heading, and a message that happened to start with
    // `diff --git` must not be parsed as a file. `-M` matches what the stat call used, so a rename is
    // one entry in both answers, and `--no-ext-diff`/`--no-textconv` mean no user filter is ever run
    // over a repository's content by a tool that is only trying to show it.
    let args: Vec<&str> = vec![
        "show",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "--format=",
        "--root",
        "--diff-merges=first-parent",
        "-M",
        "--end-of-options",
        sha.as_str(),
    ];

    let mut reader = PatchReader {
        files: &mut files,
        classifier,
        cursor: 0,
        current: None,
        current_cost: 0,
        header: true,
        retained: 0,
        seen: 0,
        total_cut: false,
        warnings: Vec::new(),
        limits,
    };
    // The whole-commit cap is enforced by stopping `git` itself, and "stop the run" travels out through
    // the callback's error slot. `Io` with an empty `context` is this module's own sentinel and is never
    // shown to anyone: `reader.total_cut` is what says what actually happened.
    let stats = git.stream_lines(&args, MAX_PATCH_LINE_BYTES, &mut |line, cut| {
        reader.on_line(line, cut).map_err(|reason| GitError::Io {
            context: String::new(),
            reason,
        })
    });
    let git_stderr = match stats {
        Ok(stats) => stats,
        Err(GitError::Io { reason, .. }) => {
            // The cap stopped the run. That is an outcome, not a failure, and every counter below is
            // still what the run managed before the child was killed.
            let _ = reason;
            StreamStats::default()
        }
        Err(other) => return Err(other.into()),
    };

    let retained = reader.retained;
    // What git got as far as printing, counted here rather than taken from the run: on the capped path
    // the run ends in an error and its stats go with it, and this is the number the view quotes.
    let streamed = reader.seen;
    let mut found = std::mem::take(&mut reader.warnings);
    if !git_stderr.stderr.is_empty() && !history.warnings.contains(&git_stderr.stderr) {
        found.push(git_stderr.stderr.clone());
    }
    let total_cut = reader.total_cut;
    // The reader's mutable borrow has to end before the file list can be counted again.
    drop(reader);
    let files_cut = files.iter().filter(|file| file.cut).count();
    let files_unseen = files.iter().filter(|file| file.unseen).count();
    let truncation = if total_cut || files_cut > 0 {
        Some(Truncation {
            total_cut,
            files_cut,
            files_unseen,
            retained,
            streamed,
            limits,
        })
    } else {
        None
    };

    // git's warnings from both calls, plus this module's own findings. A rename git declined to find
    // (`inexact rename detection was skipped`) changes which rows say `rename`, and that is a fact about
    // the diff a reader is entitled to know.
    let warnings = git_warnings.into_iter().chain(found).collect::<Vec<_>>();
    Ok(CommitDiff {
        short_id,
        sha,
        subject,
        parents,
        diff_base,
        files,
        totals,
        truncation,
        // The retained count is the memory promise; `bytes_streamed` is what git got as far as printing,
        // which after a cap is a floor on the commit's size and never a total.
        bytes_retained: retained,
        bytes_streamed: streamed,
        limits,
        warnings,
    })
}

/// The streaming patch parser: git's line grammar, the two byte caps, and the position match to the
/// stat list.
///
/// It owns the cursor rather than a slice index into `files` because the two calls can disagree about
/// how many files there are: a patch header that matches nothing in the list gets its own entry
/// (counted, labelled, and reported in `warnings`) rather than shifting every later file by one.
struct PatchReader<'a> {
    files: &'a mut Vec<FileDiff>,
    /// For the one case that needs it: a patch section the stat list does not name still has to be
    /// given a scope, and the scope is the classifier's answer and nobody else's.
    classifier: &'a Classifier,
    cursor: usize,
    /// Which entry of `files` the lines currently being read belong to.
    current: Option<usize>,
    /// Bytes of patch text already kept for the current file, so the per-file cap is a counter and not a
    /// sum over every line kept so far.
    current_cost: usize,
    /// Whether the current file's section is still its header (before the first `@@`).
    header: bool,
    retained: usize,
    /// Every byte of git's output this run read, kept or skipped: `retained` is what was kept, and the
    /// gap between them is what the caps cost.
    seen: u64,
    total_cut: bool,
    limits: Limits,
    warnings: Vec<String>,
}

impl PatchReader<'_> {
    /// One line of `git show -p` output. `Err(())` means the whole-commit cap was reached and the run
    /// should stop; everything after that is unseen, which is what the banner counts.
    fn on_line(&mut self, raw: &[u8], line_cut: bool) -> Result<(), String> {
        let text = String::from_utf8_lossy(raw).into_owned();
        let cost = text.len() + 1;
        self.seen += cost as u64;

        if let Some(path) = patch_file_header(&text) {
            self.current = Some(self.assign(&text, path));
            self.header = true;
            self.current_cost = 0;
        }
        let Some(index) = self.current else {
            // Anything before the first `diff --git` is not part of the diff. With `--format=` there
            // should be nothing there at all; ignoring it is cheaper than trusting that.
            return Ok(());
        };

        let kind = self.classify(&text);
        if kind == LineKind::Hunk {
            self.header = false;
            self.files[index].hunks += 1;
        }

        if self.retained + cost > self.limits.total {
            // The commit cap. Stop the run; the banner will say how many files never started.
            self.total_cut = true;
            self.files[index].cut = true;
            return Err(format!("diff reached its {}-byte cap", self.limits.total));
        }
        let file = &mut self.files[index];
        if !file.lines.is_empty() && self.current_cost + cost > self.limits.per_file {
            // The file cap: this file's text stops here and the run carries on with the next one. The
            // bytes are still counted, so the view can say what it dropped rather than going quiet.
            file.cut = true;
            file.dropped += cost;
            return Ok(());
        }
        file.unseen = false;
        self.retained += cost;
        self.current_cost += cost;
        file.lines.push(Line {
            kind,
            text,
            cut: line_cut,
        });
        Ok(())
    }

    /// Which entry of the stat list this patch section is: the next one in order when the header's tail
    /// is that entry's path, a later one when the order disagrees, and a new entry when nothing matches.
    ///
    /// The test is "does the header line end with this path" rather than "does the parsed path equal it",
    /// because a path containing ` b/` (yes, that is a legal file name) defeats any split of
    /// `diff --git a/x b/y`. The order is the fact; the path is the check on it.
    fn assign(&mut self, header: &str, tail: &str) -> usize {
        let tail = unquote_path(tail);
        if let Some(candidate) = self.files.get(self.cursor) {
            if tail.is_empty() || self.header_names(header, candidate) {
                let index = self.cursor;
                self.cursor += 1;
                return index;
            }
        }
        // Out of order: look forward for the match rather than guessing.
        if let Some(found) = self
            .files
            .iter()
            .skip(self.cursor)
            .position(|file| !tail.is_empty() && self.header_names(header, file))
            .map(|offset| offset + self.cursor)
        {
            self.warnings.push(format!(
                "git's patch and its diffstat disagreed about the order of {}",
                self.files[found].path
            ));
            self.cursor = found + 1;
            return found;
        }
        // Nothing in the stat list. Add it, so a file that is in the diff is never invisible.
        let path = if tail.is_empty() { header } else { &tail };
        self.warnings.push(format!(
            "git's patch named {path}, which its diffstat did not list"
        ));
        self.files.push(FileDiff {
            path: path.to_owned(),
            old_path: None,
            kind: ChangeKind::Modify,
            add: 0,
            del: 0,
            binary: false,
            scope: self.classifier.classify(path),
            lines: Vec::new(),
            hunks: 0,
            cut: false,
            unseen: false,
            dropped: 0,
        });
        let index = self.files.len() - 1;
        self.cursor = index + 1;
        index
    }

    /// Whether a `diff --git` header is about this entry, judged on the file *name* rather than the
    /// whole path.
    ///
    /// A path containing a tab is the reason: git prints
    /// `diff --git "a/tab\tname.txt" "b/tab\tname.txt"`, quoting the whole `a/<path>` word with C
    /// escapes — control characters are always quoted, and `core.quotepath=false` only lifts the quoting
    /// of non-ASCII bytes. So the header holds either the path verbatim or its escaped form, and neither
    /// a split on `" b/"` nor a suffix test can compare it to the `-z` path the stat call gave. Checking
    /// both spellings catches a gross mismatch, which is all this check is for; the positional match is
    /// what actually decides, because both calls walk the same queue in the same order.
    fn header_names(&self, header: &str, file: &FileDiff) -> bool {
        names(header, &file.path)
            || file
                .old_path
                .as_deref()
                .is_some_and(|old| names(header, old))
    }

    /// git's own grammar, with the one refinement the file header needs: `--- a/x` and `+++ b/x` are
    /// header lines, not a delete and an add, and they are only ever seen before the first `@@`.
    fn classify(&self, text: &str) -> LineKind {
        if self.header {
            return if text.starts_with("@@") {
                LineKind::Hunk
            } else {
                LineKind::Meta
            };
        }
        if text.starts_with("@@") {
            LineKind::Hunk
        } else if text.starts_with('+') {
            LineKind::Add
        } else if text.starts_with('-') {
            LineKind::Delete
        } else {
            // A ` ` context line, and `\ No newline at end of file`, which belongs with the line it
            // qualifies and is not a change of its own.
            LineKind::Context
        }
    }
}

/// The new path a `diff --git a/x b/y` header names, or `None` for any other line.
///
/// The tail after the last ` b/` is the new path, which is a guess only in the pathological case of a
/// path that contains ` b/` — and the caller uses it as a *check* on positional matching, so the worst
/// it can do is report a mismatch.
fn patch_file_header(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("diff --git ")?;
    if let Some(tail) = rest.rsplit_once(" b/") {
        return Some(tail.1);
    }
    Some(rest)
}

/// Whether a `diff --git` header line names this path.
///
/// git builds the header from `a/<path>` and `b/<path>` as whole words, so those are the two spellings
/// to look for, each of them either verbatim or C-quoted (see [`c_escape`]).
fn names(header: &str, path: &str) -> bool {
    ["a/", "b/"].iter().any(|prefix| {
        let word = format!("{prefix}{path}");
        header.contains(&word) || c_escape(&word).is_some_and(|quoted| header.contains(&quoted))
    })
}

/// git's C-style quoting of a path, when it would quote it at all: `None` for a path that appears
/// verbatim. The rule is git's `quote_c_style`, minus the non-ASCII half that `core.quotepath=false`
/// switches off, since that is what the patch is read with.
fn c_escape(path: &str) -> Option<String> {
    let needs = path
        .bytes()
        .any(|byte| byte < 0x20 || byte == 0x7f || byte == b'"' || byte == b'\\');
    if !needs {
        return None;
    }
    let mut out = String::from('"');
    for byte in path.bytes() {
        match byte {
            b'\t' => out.push_str("\\t"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            0x7f => out.push_str("\\177"),
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            0x20..=0x7e => out.push(byte as char),
            other => out.push_str(&format!("\\{other:03o}")),
        }
    }
    out.push('"');
    Some(out)
}

/// Undo git's C-style quoting of a path, for the one case that needs it: a header whose path no split
/// can name, where the label shown to the user should read like a path and not like a string literal.
///
/// Only the shapes git itself emits are handled: surrounding double quotes with `\t`, `\n`, `\r`, `\"`,
/// `\\` and `\nnn` octal escapes. Anything else is returned unchanged, because a guess here would be a
/// path that looks right and is not.
#[must_use]
fn unquote_path(raw: &str) -> String {
    let Some(body) = raw
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    else {
        return raw.to_owned();
    };
    let bytes = body.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'\\' {
            out.push(bytes[index]);
            index += 1;
            continue;
        }
        index += 1;
        let Some(&escaped) = bytes.get(index) else {
            break;
        };
        index += 1;
        match escaped {
            b't' => out.push(b'\t'),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b'\\' => out.push(b'\\'),
            b'"' => out.push(b'"'),
            digit @ (b'0'..=b'7') => {
                let mut value = u32::from(digit - b'0');
                for _ in 0..2 {
                    if bytes
                        .get(index)
                        .is_some_and(|next| (b'0'..=b'7').contains(next))
                    {
                        value = value * 8 + u32::from(bytes[index] - b'0');
                        index += 1;
                    } else {
                        break;
                    }
                }
                out.push(value as u8);
            }
            other => {
                out.push(b'\\');
                out.push(other);
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A byte count the way a status line has room for it: `812 B`, `128 KiB`, `1.0 MiB`.
#[must_use]
pub fn bytes(count: usize) -> String {
    const KIB: usize = 1024;
    const MIB: usize = 1024 * 1024;
    if count < KIB {
        return format!("{count} B");
    }
    if count < MIB {
        return format!("{} KiB", count / KIB);
    }
    let tenth_mib = count * 10 / MIB;
    format!("{}.{:01} MiB", tenth_mib / 10, tenth_mib % 10)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_header_names_the_tail_path_and_nothing_else_does() {
        assert_eq!(
            patch_file_header("diff --git a/src/evidence/coverage.rs b/src/evidence/coverage.rs"),
            Some("src/evidence/coverage.rs")
        );
        assert_eq!(
            patch_file_header("diff --git a/dir with space/f b/dir with space/f"),
            Some("dir with space/f")
        );
        assert_eq!(patch_file_header("@@ -1,2 +1,3 @@"), None);
    }

    #[test]
    fn byte_counts_are_said_the_way_a_status_line_has_room_for() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(812), "812 B");
        assert_eq!(bytes(128 * 1024), "128 KiB");
        assert_eq!(bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(bytes(1024 * 1024 + 512 * 1024), "1.5 MiB");
    }

    #[test]
    fn a_per_file_cap_above_the_commit_cap_is_clamped_to_it() {
        let limits = Limits::from_args(Some(4096), Some(1 << 20));
        assert_eq!(limits.total, 4096);
        assert_eq!(limits.per_file, 4096);
        // And a flag that names neither gets the documented defaults.
        let defaults = Limits::from_args(None, None);
        assert_eq!(defaults.total, DEFAULT_MAX_DIFF_BYTES);
        assert_eq!(defaults.per_file, DEFAULT_MAX_DIFF_BYTES_PER_FILE);
    }
}
