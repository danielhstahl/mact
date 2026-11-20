//! Running `git`, and what it costs when it will not.
//!
//! ## Why a subprocess, again
//!
//! `git` is invoked as a binary through [`std::process::Command`], not linked as `git2`/libgit2.
//! That is not a preference discovered here: ADR-0003 refuses libgit2, `crates/act-mcp/Cargo.toml`
//! refuses it, ADR-0005 §6 lists "git via `std::process::Command`" as the allowed dependency and
//! names `git2`/`libgit2-sys` as forbidden for this crate, and ADR-0006 §5 rejects a second C
//! build dependency for the store on the same grounds. The reasoning, in one place so nobody
//! re-litigates it: act shells out to git anyway; every gate fathom needs (`rev-parse`, `log
//! --numstat`, `diff-tree`) is a CLI subcommand; libgit2 would add a C toolchain to five release
//! targets (two of them musl) for no gain; and the CLI's `--numstat -z` grammar is what the ADR's
//! numbers are *defined* against, so reading git's own bytes is the reference, not an approximation
//! of one.
//!
//! ## What a reader owes the repository
//!
//! * **No writes.** `GIT_OPTIONAL_LOCKS=0` means fathom never refreshes the index or takes a
//!   `commit-graph` lock it does not need; nothing here runs a subcommand that can write
//!   (`gc`, `repack`, `maintenance`, `fetch`) — this crate reads.
//! * **No prompts.** `GIT_TERMINAL_PROMPT=0`, so a repo with a broken remote cannot stop a
//!   terminal.
//! * **No pager, no colour, a stable locale.** `GIT_PAGER=cat`, `--no-color`, `LC_ALL=C`.
//! * **A user's config cannot move a number.** ADR-0006 §11.3 promises two runs on one repository
//!   produce byte-identical rows, and `diff.detectCopies = always` in someone's `~/.gitconfig`
//!   would otherwise change which files one commit claims. So the diff calls pass explicit `-c`
//!   overrides for the settings that alter the output this parser reads, rather than inheriting
//!   whatever the machine happens to have. Global config is *not* cleared: `safe.directory` and
//!   `credential.*` are real settings real people need, and the neutralising `-c` flags cover the
//!   ones that could change a byte of the diffstat.
//! * **Bounded memory.** [`Git::stream_tokens`] hands out one `NUL`-terminated token at a time
//!   while `git` is still running, so a repository with a 200 000-file commit costs one entry, not
//!   one transcript of the whole walk. Nothing in the diffstat path asks git for patch text:
//!   `--numstat` and `--raw` are counts and modes, never content. Content is a separate, named
//!   surface — [`Git::blobs`] hands one blob at a time to the store's analyzer, which is the only
//!   caller and is bounded the same way (one blob in memory, and only for blobs the metric cache
//!   has never seen).
//!
//! ## Grammar note: why `NUL`
//!
//! Every stream this module produces is `NUL`-separated, because `NUL` is the one byte that cannot
//! appear in a commit object (git stores commit headers and messages as C strings), so a field
//! split on `NUL` cannot be desynchronised by message text, an author name or — with `-z` — a file
//! name. Control bytes like `0x1e` are used only as *record* markers, placed by fathom's own
//! `--format` and never inside user data.

use std::{
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    thread,
};

use thiserror::Error;

/// The oldest `git` this reader will produce numbers with.
///
/// 2.31 (2021-03) is where `--diff-merges=` landed, and that flag is not a nicety: ADR-0008 §6.3
/// pins a merge commit's diff as *against its first parent*, and `--diff-merges=first-parent` is
/// the only spelling that asks for that diff while still walking every parent. Below it fathom
/// would have to guess a rule, which is what the refusal exists to avoid.
pub const MIN_GIT: GitVersion = GitVersion {
    major: 2,
    minor: 31,
    patch: 0,
};

/// A parsed `git --version`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct GitVersion {
    /// Major.
    pub major: u32,
    /// Minor.
    pub minor: u32,
    /// Patch (the third dotted number; a `2.39.0.windows.1` suffix is dropped).
    pub patch: u32,
}

impl std::fmt::Display for GitVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl GitVersion {
    /// Parse the first line of `git --version`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let head = text.lines().next()?.trim();
        let head = head
            .strip_prefix("git version ")
            .or_else(|| head.strip_prefix("git version"))
            .unwrap_or(head)
            .trim();
        let mut numbers = head.split('.').map(str::trim);
        let major = numbers.next()?.parse().ok()?;
        let minor = numbers.next().unwrap_or("0").parse().unwrap_or(0);
        // The patch slot can carry a suffix (`0-rc0`, `0.windows.1`); take the leading digits.
        let patch_text = numbers.next().unwrap_or("0");
        let patch_digits: String = patch_text
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        Some(Self {
            major,
            minor,
            patch: patch_digits.parse().unwrap_or(0),
        })
    }
}

/// Everything that can stop `git` answering.
///
/// One line naming the offending value plus a `fix:` that changes it — the shape every refusal in
/// this workspace uses, and the reason a missing repository is a `GitError` a caller can match on
/// rather than a panic in someone's terminal.
#[allow(
    missing_docs,
    reason = "the #[error] format string documents each variant and its fields"
)]
#[derive(Debug, Error)]
pub enum GitError {
    /// `git` is not on `PATH`, or could not be started.
    #[error("cannot run git: {reason}\n  fix: install git, or point --workspace at a checkout git can read")]
    Unavailable { reason: String },

    /// The directory is not inside a git repository (bare or not).
    #[error("{path} is not a git repository (git rev-parse said: {reason})\n  fix: pass --workspace at a git work tree; fathom draws one row per commit and has nothing to draw without one")]
    NotAWorkTree { path: PathBuf, reason: String },

    /// The repository exists and has no commits yet — unborn `HEAD`, a fresh `git init`.
    #[error("{path} is a git repository with no commits yet\n  fix: nothing to index until the first commit exists; fathom's answer for an empty repository is an empty history, not an error about one")]
    EmptyRepository { path: PathBuf },

    /// `git` is older than [`MIN_GIT`].
    #[error("git {found} is too old; fathom needs git {minimum} or newer for --diff-merges=first-parent (ADR-0008 §6.3's diff-vs-first-parent rule)\n  fix: upgrade git, or point --workspace at a checkout git {minimum} can read")]
    UnsupportedGit { found: String, minimum: GitVersion },

    /// `--from`/`--to` named something git does not know.
    #[error("not a revision this repository knows: {revision}\n  fix: pass a SHA, HEAD, a branch or a tag, or a relative form like HEAD~200")]
    UnknownRevision { revision: String },

    /// A git subcommand failed for a reason that is not one of the cases above.
    #[error("`git {argv}` failed ({status}){stderr}\n  fix: run the same command in {workspace} to see what git says")]
    Failed {
        argv: String,
        status: String,
        stderr: String,
        workspace: PathBuf,
    },

    /// git's output did not have the shape this parser reads — a grammar change, or a repository
    /// that is not what `--raw --numstat -z` promises. Reported, never guessed past.
    #[error("cannot read git's output: {detail}\n  fix: this is fathom's bug for this repository; report it with the command line and the commit")]
    Parse { detail: String },

    /// A file or directory the reader needed could not be read.
    #[error("cannot read {context}: {reason}")]
    Io { context: String, reason: String },
}

/// A repository, its top level, and the `git` that reads it.
#[derive(Debug, Clone)]
pub struct Git {
    /// The directory the caller passed, canonicalised.
    pub workspace: PathBuf,
    /// `git rev-parse --show-toplevel` (the repository root; for a bare repository, `workspace`).
    pub toplevel: PathBuf,
    /// Whether the repository is bare (`git rev-parse --is-bare-repository`).
    pub bare: bool,
    /// The version of the `git` binary in use.
    pub version: GitVersion,
}

/// What one streamed git run cost.
///
/// `max_token_bytes` is the reader's own proof that it never held a whole-repo diff: the largest
/// single thing it ever allocated is one `--numstat` entry, and `crates/fathom/tests` asserts the
/// figure on a synthetic repository.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamStats {
    /// Total bytes of git's stdout read.
    pub bytes: u64,
    /// The largest token seen, after the `NUL` separator is stripped.
    pub max_token_bytes: usize,
    /// How many tokens came out.
    pub tokens: usize,
    /// git's own warnings, verbatim and newline-joined — empty in the ordinary case.
    pub stderr: String,
}

impl Git {
    /// Open the repository at or above `workspace`, and check the binary can do what this reader
    /// needs.
    ///
    /// # Errors
    ///
    /// [`GitError::Unavailable`] if git cannot be started, [`GitError::NotAWorkTree`] if the
    /// directory is not inside a repository, [`GitError::UnsupportedGit`] if the binary is older
    /// than [`MIN_GIT`].
    pub fn open(workspace: &Path) -> Result<Self, GitError> {
        let version = GitVersion::parse(
            String::from_utf8_lossy(&Self::run_raw(workspace, &["--version"])?).as_ref(),
        )
        .ok_or_else(|| GitError::Parse {
            detail: "`git --version` did not look like a version".to_owned(),
        })?;
        if version < MIN_GIT {
            return Err(GitError::UnsupportedGit {
                found: version.to_string(),
                minimum: MIN_GIT,
            });
        }
        let workspace =
            std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
        let bare = match Self::run_raw(&workspace, &["rev-parse", "--is-bare-repository"]) {
            Ok(out) => out.trim_ascii_end() == b"true",
            Err(err) => {
                return Err(GitError::NotAWorkTree {
                    path: workspace,
                    reason: match err {
                        GitError::Failed { stderr, .. } => first_line(stderr.trim()),
                        other => other.to_string(),
                    },
                });
            }
        };
        let toplevel = if bare {
            workspace.clone()
        } else {
            let out = Self::run_raw(&workspace, &["rev-parse", "--show-toplevel"])?;
            PathBuf::from(String::from_utf8_lossy(out.trim_ascii()).into_owned())
        };
        Ok(Self {
            workspace,
            toplevel,
            bare,
            version,
        })
    }

    /// Run git where a non-zero exit is an answer rather than a fault.
    ///
    /// `--verify --quiet` and `rev-list --max-count=1` are the plumbing spells for "does this
    /// exist?", and their contract is the exit status. Mapping that through [`Git::run`] would turn
    /// an ordinary "no" into a [`GitError::Failed`] with an empty stderr and a `fix:` nobody needs.
    fn try_run(&self, args: &[&str]) -> Option<Vec<u8>> {
        match self.run(args) {
            Ok(stdout) => Some(stdout),
            Err(GitError::Failed { .. }) => None,
            // git itself could not start, or the version probe failed: that is still a fault, and
            // the caller's `?` never sees it because a "does this exist" question must not be able
            // to produce one. Report it as a `None` and let the resolution refuse by name.
            Err(_) => None,
        }
    }

    /// Resolve a revision (a SHA, `HEAD`, a branch, `HEAD~200`) to a full lowercase SHA.
    ///
    /// # Errors
    ///
    /// [`GitError::UnknownRevision`] when git does not know the name — including `HEAD` in a
    /// repository with no commits, which [`Git::is_empty`] names precisely.
    pub fn resolve(&self, revision: &str) -> Result<String, GitError> {
        // `--end-of-options` keeps a `--to` that starts with `-` an argument, never an option: a
        // revision is user input and git's option parser is not the place to discover that.
        let out = self
            .try_run(&[
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                &format!("{revision}^{{commit}}"),
            ])
            .ok_or_else(|| GitError::UnknownRevision {
                revision: revision.to_owned(),
            })?;
        let sha = String::from_utf8_lossy(out.trim_ascii()).into_owned();
        if !is_sha(&sha) {
            return Err(GitError::UnknownRevision {
                revision: revision.to_owned(),
            });
        }
        Ok(sha)
    }

    /// Whether the repository has no commits at all yet.
    ///
    /// # Errors
    ///
    /// [`GitError`] if git cannot answer.
    pub fn is_empty(&self) -> Result<bool, GitError> {
        Ok(self
            .try_run(&["rev-list", "--all", "--max-count=1"])
            .map(|out| out.trim_ascii().is_empty())
            .unwrap_or(false))
    }

    /// Whether this commit has a first parent — i.e. whether it is a root commit.
    ///
    /// The root commit is the one case the diff rule has to state separately (ADR-0006 §"the root
    /// commit diff is against the empty tree, not a crash"), and `<sha>^` is how git answers the
    /// question without parsing anything.
    ///
    /// # Errors
    ///
    /// [`GitError`] if git cannot answer.
    pub fn has_parent(&self, sha: &str) -> Result<bool, GitError> {
        Ok(self
            .try_run(&[
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                &format!("{sha}^"),
            ])
            .map(|out| is_sha(String::from_utf8_lossy(out.trim_ascii()).as_ref()))
            .unwrap_or(false))
    }

    /// Run git and capture stdout whole. For the small, bounded calls (`rev-parse`, `--version`).
    ///
    /// # Errors
    ///
    /// [`GitError`] for any failure, including a non-zero exit.
    pub fn run(&self, args: &[&str]) -> Result<Vec<u8>, GitError> {
        Self::run_raw(&self.toplevel, args)
    }

    /// Run git and hand each `NUL`-terminated token of stdout to `on_token` while git still runs.
    ///
    /// The callback decides when the run stops: an `Err` kills the child and travels out unchanged,
    /// which is how a parse failure on a huge repository costs one commit rather than the whole
    /// walk.
    ///
    /// # Errors
    ///
    /// [`GitError`] from git, from the spawn, or from the callback.
    pub fn stream_tokens<E>(
        &self,
        args: &[&str],
        on_token: &mut impl FnMut(&[u8]) -> Result<(), E>,
    ) -> Result<StreamStats, E>
    where
        E: From<GitError>,
    {
        self.stream_pipe(args, &mut |mut reader, stats| {
            let mut token: Vec<u8> = Vec::new();
            loop {
                token.clear();
                match reader.read_until(0, &mut token) {
                    Ok(0) => return None,
                    Ok(_) => {}
                    Err(err) => {
                        return Some(E::from(GitError::Io {
                            context: "git's output".to_owned(),
                            reason: err.to_string(),
                        }))
                    }
                }
                stats.bytes += u64::try_from(token.len()).unwrap_or(u64::MAX);
                if token.last() == Some(&0) {
                    token.pop();
                }
                stats.max_token_bytes = stats.max_token_bytes.max(token.len());
                stats.tokens += 1;
                if let Err(err) = on_token(&token) {
                    return Some(err);
                }
            }
        })
    }

    /// Run git and hand each **line** of stdout to `on_line` while git still runs.
    ///
    /// Patch text is line-oriented (`NUL` does not separate it the way `--raw -z` separates a
    /// diffstat), so the diff reader needs a line reader and the diffstat path needs the token one;
    /// both are the same promise about memory, which is why both are `stream_pipe` over a
    /// different delimiter. `on_line` gets the line without its `\n`, and `true` for its second
    /// argument when the line was longer than `max_line_bytes` and had to be cut.
    ///
    /// The scan is chunk-at-a-time rather than `read_until(b'\n')` on purpose: `read_until` collects
    /// the whole line before it returns, so one 40 000-column minified line would be one allocation as
    /// big as the file it came from. Here the ceiling is `max_line_bytes` plus one 8 KiB chunk, whatever
    /// the file contains.
    ///
    /// As with [`Git::stream_tokens`], an `Err` from the callback kills the child and travels out
    /// unchanged; that is how a byte cap stops `git show` in the middle of a 40 000-line reformat
    /// instead of after it.
    ///
    /// # Errors
    ///
    /// [`GitError`] from git, from the spawn, or from the callback.
    pub fn stream_lines<E>(
        &self,
        args: &[&str],
        max_line_bytes: usize,
        on_line: &mut impl FnMut(&[u8], bool) -> Result<(), E>,
    ) -> Result<StreamStats, E>
    where
        E: From<GitError>,
    {
        let cap = max_line_bytes.max(1);
        self.stream_pipe(args, &mut |mut reader, stats| {
            let mut pending: Vec<u8> = Vec::new();
            let mut cut = false;
            let mut chunk = [0_u8; 8 * 1024];
            // `None` here means "the callback said stop", which `stream_pipe` turns into the error the
            // caller handed back after killing git.
            let mut emit =
                |pending: &mut Vec<u8>, cut: bool, stats: &mut StreamStats| -> Option<E> {
                    stats.bytes += u64::try_from(pending.len() + 1).unwrap_or(u64::MAX);
                    stats.max_token_bytes = stats.max_token_bytes.max(pending.len());
                    stats.tokens += 1;
                    on_line(pending, cut).err()
                };
            loop {
                let read = match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(read) => read,
                    Err(err) => {
                        return Some(E::from(GitError::Io {
                            context: "git's output".to_owned(),
                            reason: err.to_string(),
                        }))
                    }
                };
                let mut index = 0;
                while index < read {
                    let end = chunk[index..read]
                        .iter()
                        .position(|byte| *byte == b'\n')
                        .map_or(read, |offset| index + offset);
                    let piece = &chunk[index..end];
                    if pending.len() < cap {
                        let take = piece.len().min(cap - pending.len());
                        pending.extend_from_slice(&piece[..take]);
                        if take < piece.len() {
                            cut = true;
                        }
                    } else if !piece.is_empty() {
                        cut = true;
                    }
                    let complete = end < read;
                    if complete {
                        if let Some(err) = emit(&mut pending, cut, stats) {
                            return Some(err);
                        }
                        pending.clear();
                        pending.reserve(64);
                        cut = false;
                    }
                    // `end == read` means the chunk ran out mid-line: keep scanning the next one.
                    index = if complete { end + 1 } else { end };
                }
            }
            if !pending.is_empty() || cut {
                return emit(&mut pending, cut, stats);
            }
            None
        })
    }

    /// The one place a `git` child is spawned for a stream: spawn, drain stderr, run `on_stream`,
    /// wait, and turn a non-zero exit into [`GitError::Failed`].
    ///
    /// `on_stream` returns `Some(err)` to stop the run (the callback said so) and `None` on an
    /// ordinary EOF. Both paths wait for the child, because an unwaited child is a zombie and a
    /// killed one whose output this thread never drained would leave the stderr thread holding a
    /// pipe nobody reads.
    fn stream_pipe<E>(
        &self,
        args: &[&str],
        on_stream: &mut impl FnMut(BufReader<ChildStdout>, &mut StreamStats) -> Option<E>,
    ) -> Result<StreamStats, E>
    where
        E: From<GitError>,
    {
        let mut command = self.command(args);
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|err| {
            E::from(GitError::Unavailable {
                reason: format!("git {}: {err}", args.first().copied().unwrap_or("")),
            })
        })?;
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(E::from(GitError::Unavailable {
                reason: "git's stdout was not piped".to_owned(),
            }));
        };
        let Some(stderr) = child.stderr.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(E::from(GitError::Unavailable {
                reason: "git's stderr was not piped".to_owned(),
            }));
        };

        // git's stderr on a helper thread: `warning: inexact rename detection was skipped` is
        // written to a terminal-sized pipe, and a full pipe would stop git while this thread was
        // waiting for the next token — which is a deadlock that looks like a hung TUI.
        let drain = thread::spawn(move || {
            let mut buffer = Vec::new();
            let mut reader = BufReader::new(stderr);
            // Read until EOF, but stop collecting past 64 KiB: warnings are short, and a git that
            // prints more than that is a git worth killing rather than buffering.
            use std::io::Read;
            let mut chunk = [0_u8; 4096];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        if buffer.len() < 65_536 {
                            buffer.extend_from_slice(&chunk[..read]);
                        }
                    }
                }
            }
            buffer
        });

        let mut stats = StreamStats::default();
        let failure = on_stream(BufReader::new(stdout), &mut stats);

        let status = child.wait().map_err(|err| {
            E::from(GitError::Unavailable {
                reason: format!("waiting for git: {err}"),
            })
        })?;
        let stderr = drain
            .join()
            .map(|buffer| String::from_utf8_lossy(&buffer).into_owned())
            .unwrap_or_default();
        stats.stderr = stderr.trim().to_owned();

        if let Some(err) = failure {
            return Err(err);
        }
        if !status.success() {
            return Err(E::from(GitError::Failed {
                argv: args.join(" "),
                status: status.to_string(),
                stderr: ellipsis(&stats.stderr),
                workspace: self.toplevel.clone(),
            }));
        }
        Ok(stats)
    }

    /// The tree object a revision points at (`git rev-parse <rev>^{tree}`).
    ///
    /// The store's content-addressed file-set analysis is keyed on this id (ADR-0006 §6.3's `tree`
    /// input kind): two commits with one tree have one answer, and the id is what says so without
    /// comparing 400 files. The commit-list stream already carries it (`%T`, see [`crate::history`]);
    /// this is for the case the walk did not name — a window's oldest commit's parent, whose tree is
    /// the compose base for the window's first row.
    ///
    /// # Errors
    ///
    /// [`GitError`] if git does not know the revision or it is not a commit.
    pub fn tree_of(&self, revision: &str) -> Result<String, GitError> {
        let out = self
            .try_run(&[
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                &format!("{revision}^{{tree}}"),
            ])
            .ok_or_else(|| GitError::UnknownRevision {
                revision: revision.to_owned(),
            })?;
        let sha = String::from_utf8_lossy(out.trim_ascii()).into_owned();
        if !is_sha(&sha) {
            return Err(GitError::UnknownRevision {
                revision: revision.to_owned(),
            });
        }
        Ok(sha)
    }

    /// Every regular file in a tree, as `(mode, blob id, repo-relative path)`.
    ///
    /// `git ls-tree -r -z`, which is git's own inventory of one tree: no working tree is consulted,
    /// so the answer describes the commit rather than the checkout (ADR-0006 §4.3). Symlinks and
    /// gitlinks are returned with their modes and left to the caller, which classifies by path and
    /// size like any other blob; `TypeChange`/submodule content is git's, not fathom's to reinterpret.
    ///
    /// The whole listing is returned in one vector: it is bounded by the tree's file count (424 in
    /// this repository, and a pathological monorepo's few hundred thousand entries is tens of
    /// megabytes of text), and the store's compose wants the whole inventory to diff against its
    /// parent's. `--allow-empty` is not needed: an empty tree lists nothing, which is the answer.
    ///
    /// # Errors
    ///
    /// [`GitError`] if git refuses the tree.
    pub fn ls_tree(&self, tree: &str) -> Result<Vec<TreeEntry>, GitError> {
        let out = self.run(&["ls-tree", "-r", "-z", "--", tree])?;
        let mut entries = Vec::new();
        for record in out
            .split(|byte| *byte == 0)
            .filter(|record| !record.is_empty())
        {
            // `<mode> <type> <oid>\t<path>` — the tab is the only legal separator, because a path
            // may contain a space (and with `-z`, may contain anything but a NUL).
            let Some(split) = record.iter().position(|byte| *byte == b'\t') else {
                return Err(GitError::Parse {
                    detail: format!(
                        "`{}` is not an ls-tree record",
                        String::from_utf8_lossy(record)
                    ),
                });
            };
            let head = String::from_utf8_lossy(&record[..split]).into_owned();
            let path = String::from_utf8_lossy(&record[split + 1..]).into_owned();
            let mut fields = head.split_ascii_whitespace();
            let (Some(mode), Some(kind), Some(blob)) =
                (fields.next(), fields.next(), fields.next())
            else {
                return Err(GitError::Parse {
                    detail: format!("`{head}` is not an ls-tree header"),
                });
            };
            if kind != "blob" {
                // A tree entry for a submodule (`commit`) or a symlink (`link`): the metric model has
                // no number for either, and inventing one is the zero ADR-0002 §2.1 refuses.
                continue;
            }
            entries.push(TreeEntry {
                mode: mode.to_owned(),
                blob: blob.to_owned(),
                path,
            });
        }
        Ok(entries)
    }

    /// Open a batch blob reader: one `git cat-file --batch` child, asked for one object at a time.
    ///
    /// One process for a whole index run, and a request/response round trip per blob, is what keeps
    /// reading 400 blobs cost 400 round trips instead of 400 process spawns — and it means the
    /// request is only sent at all for a blob whose analysis is not already in the cache.
    ///
    /// # Errors
    ///
    /// [`GitError`] if git cannot be started.
    pub fn blobs(&self) -> Result<BlobReader, GitError> {
        BlobReader::open(self)
    }

    /// Build the command: the `-c` overrides that make an answer independent of whose machine this
    /// is, and the environment every git child of this crate gets.
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new("git");
        command
            .current_dir(&self.toplevel)
            .stdin(Stdio::null())
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_PAGER", "cat")
            .env("LC_ALL", "C");
        command.args(["-c", "core.quotepath=false"]);
        // Copy detection is opt-in (see `history::Copies`), and `diff.detectCopies` in someone's
        // config would otherwise turn it on for them and relabel an added file as a copy.
        command.args([
            "-c",
            "diff.detectCopies=false",
            "-c",
            "diff.detectCopiesHarder=false",
        ]);
        command.args(args);
        command
    }

    fn run_raw(directory: &Path, args: &[&str]) -> Result<Vec<u8>, GitError> {
        let mut command = Command::new("git");
        command
            .current_dir(directory)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_PAGER", "cat")
            .env("LC_ALL", "C")
            .args(args);
        let output = command.spawn().and_then(|child| child.wait_with_output());
        let output = match output {
            Ok(output) => output,
            Err(err) => {
                return Err(if err.kind() == std::io::ErrorKind::NotFound {
                    GitError::Unavailable {
                        reason: "git was not found on PATH".to_owned(),
                    }
                } else {
                    GitError::Unavailable {
                        reason: format!("git {}: {err}", args.first().copied().unwrap_or("")),
                    }
                });
            }
        };
        if !output.status.success() {
            return Err(GitError::Failed {
                argv: args.join(" "),
                status: output.status.to_string(),
                stderr: ellipsis(String::from_utf8_lossy(&output.stderr).trim()),
                workspace: directory.to_path_buf(),
            });
        }
        Ok(output.stdout)
    }
}

/// The revs that mean "walk `to`, stop after `from`, include `from`".
///
/// `--not <from>^` excludes `from`'s ancestry and nothing else, which is what makes `--from`
/// inclusive; it is `git log <from>..<to>` with `from` put back. A root `from` has no `<from>^` for
/// git to exclude, so the walk simply runs to the root — which is the same answer, asked a way git
/// accepts.
#[must_use]
pub fn revs(from: Option<&str>, from_has_parent: bool, to: &str) -> Vec<String> {
    match (from, from_has_parent) {
        (Some(from), true) => vec![to.to_owned(), "--not".to_owned(), format!("{from}^")],
        _ => vec![to.to_owned()],
    }
}

/// Whether a string is a full lowercase SHA-1.
#[must_use]
pub fn is_sha(text: &str) -> bool {
    text.len() == 40
        && text.bytes().all(|byte| byte.is_ascii_hexdigit())
        && text == text.to_lowercase()
}

fn first_line(text: &str) -> String {
    text.lines()
        .next()
        .filter(|line| !line.trim().is_empty())
        .unwrap_or("no output")
        .trim()
        .to_owned()
}

/// Keep a git stderr short without cutting it off unexplained.
fn ellipsis(text: &str) -> String {
    const MAX: usize = 600;
    let mut end = text.len().min(MAX);
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut kept = text[..end].to_owned();
    if kept.len() < text.len() {
        kept.push_str(" …");
    }
    if kept.is_empty() {
        String::new()
    } else {
        format!(": {kept}")
    }
}

/// One regular file in one tree, from [`Git::ls_tree`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    /// The mode, e.g. `100644` or `100755`.
    pub mode: String,
    /// The blob's git object id.
    pub blob: String,
    /// The repo-relative path, with forward slashes.
    pub path: String,
}

/// A long-lived `git cat-file --batch` child, asked for one blob at a time.
///
/// Why a child and a `Drop`: the store reads a blob only when its analysis is not already cached, and
/// a cold index of a real repository asks for hundreds of them. Spawning git per blob would dominate
/// the run; spawning one per commit would still pay a process for every commit that touched a file.
/// One child, one request/response round trip per blob, and the process is reaped on drop, which is
/// the whole lifecycle — `--batch` is read-only plumbing and holds no repository lock (`git cat-file`
/// takes none, and `GIT_OPTIONAL_LOCKS=0` is already in `Git::command`'s environment).
///
/// The request/response is deliberately synchronous — write one id, read one object — because that
/// ordering cannot deadlock on a full pipe: git cannot fill its own stdout with an object this side
/// has not asked for yet.
#[derive(Debug)]
pub struct BlobReader {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    /// Blobs served, and bytes read, for a `-v` line and a budget assertion.
    stats: StreamStats,
}

impl BlobReader {
    fn open(git: &Git) -> Result<Self, GitError> {
        let mut command = git.command(&["cat-file", "--batch"]);
        command.stdin(Stdio::piped()).stdout(Stdio::piped());
        let mut child = command.spawn().map_err(|err| GitError::Unavailable {
            reason: format!("git cat-file --batch: {err}"),
        })?;
        let stdin = child.stdin.take().ok_or_else(|| GitError::Unavailable {
            reason: "git cat-file's stdin was not piped".to_owned(),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| GitError::Unavailable {
            reason: "git cat-file's stdout was not piped".to_owned(),
        })?;
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            stats: StreamStats::default(),
        })
    }

    /// What this reader has cost so far.
    #[must_use]
    pub const fn stats(&self) -> &StreamStats {
        &self.stats
    }

    /// The bytes of one blob, or `None` when git says the object is not there.
    ///
    /// A missing object is an answer, not a fault: a tree can name a blob that a half-fetched
    /// repository does not have, and the caller's job is to record the gap, not to abort the run over
    /// one file.
    ///
    /// `cap` is the caller's ingest limit (`index.file_size_cap_mib`): a larger blob's body is *not*
    /// allocated — it is streamed past, because the framing has to be consumed either way — and
    /// `None` comes back for it. That is how "one blob in memory at a time" survives a repository that
    /// keeps a 400 MB fixture in a tree: the size is known from the header, before the bytes.
    ///
    /// # Errors
    ///
    /// [`GitError`] if the child dies or the framing does not parse.
    pub fn blob(&mut self, oid: &str, cap: u64) -> Result<Option<Vec<u8>>, GitError> {
        if !is_sha(oid) {
            return Err(GitError::Parse {
                detail: format!("`{oid}` is not a 40-hex object id"),
            });
        }
        writeln!(self.stdin, "{oid}").map_err(|err| GitError::Io {
            context: "git cat-file's stdin".to_owned(),
            reason: err.to_string(),
        })?;
        self.stdin.flush().map_err(|err| GitError::Io {
            context: "git cat-file's stdin".to_owned(),
            reason: err.to_string(),
        })?;

        let mut header = Vec::new();
        self.stdout
            .read_until(b'\n', &mut header)
            .map_err(|err| GitError::Io {
                context: "git cat-file's output".to_owned(),
                reason: err.to_string(),
            })?;
        if header.is_empty() {
            return Err(GitError::Io {
                context: "git cat-file's output".to_owned(),
                reason: "the child closed its output".to_owned(),
            });
        }
        self.stats.bytes += u64::try_from(header.len()).unwrap_or(u64::MAX);
        let head = String::from_utf8_lossy(&header).trim_end().to_owned();
        if head.ends_with(" missing") {
            self.stats.tokens += 1;
            return Ok(None);
        }
        let size: u64 = head
            .rsplit(' ')
            .next()
            .and_then(|tail| tail.parse().ok())
            .ok_or_else(|| GitError::Parse {
                detail: format!("`{head}` is not a cat-file --batch header"),
            })?;
        // One blob in memory at a time, and the cap is the caller's (`index.file_size_cap_mib`), which
        // is applied here — from the header, before the bytes are allocated — so an oversized blob
        // costs the run nothing it did not already know.
        if size > cap {
            let mut discard = [0_u8; 65_536];
            let mut left = size;
            while left > 0 {
                let take = usize::try_from(left.min(discard.len() as u64)).unwrap_or(discard.len());
                self.stdout
                    .read_exact(&mut discard[..take])
                    .map_err(|err| GitError::Io {
                        context: format!("git cat-file's output for {oid}"),
                        reason: err.to_string(),
                    })?;
                left -= take as u64;
            }
            let mut newline = [0_u8; 1];
            self.stdout
                .read_exact(&mut newline)
                .map_err(|err| GitError::Io {
                    context: format!("git cat-file's output for {oid}"),
                    reason: err.to_string(),
                })?;
            self.stats.tokens += 1;
            return Ok(None);
        }
        let mut body = vec![0_u8; usize::try_from(size).unwrap_or(usize::MAX)];
        self.stdout
            .read_exact(&mut body)
            .map_err(|err| GitError::Io {
                context: format!("git cat-file's output for {oid}"),
                reason: err.to_string(),
            })?;
        // git terminates each object with a single `\n`.
        let mut newline = [0_u8; 1];
        self.stdout
            .read_exact(&mut newline)
            .map_err(|err| GitError::Io {
                context: format!("git cat-file's output for {oid}"),
                reason: err.to_string(),
            })?;
        self.stats.bytes += size;
        self.stats.bytes += 1;
        self.stats.tokens += 1;
        self.stats.max_token_bytes = self.stats.max_token_bytes.max(body.len());
        Ok(Some(body))
    }
}

impl Drop for BlobReader {
    fn drop(&mut self) {
        // `kill` is the one that guarantees no child outlives the run (a `--batch` child sits waiting
        // for the next object id otherwise), and `wait` makes sure it does not become a zombie. The
        // stdin handle goes with `self`, and git does not care which end closed first.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse_with_the_suffixes_git_ships() {
        assert_eq!(
            GitVersion::parse("git version 2.47.3"),
            Some(GitVersion {
                major: 2,
                minor: 47,
                patch: 3
            })
        );
        assert_eq!(
            GitVersion::parse("git version 2.39.0.windows.1\n"),
            Some(GitVersion {
                major: 2,
                minor: 39,
                patch: 0
            })
        );
        assert_eq!(
            GitVersion::parse("git version 2.31.0-rc0"),
            Some(GitVersion {
                major: 2,
                minor: 31,
                patch: 0
            })
        );
        assert_eq!(GitVersion::parse("not a version"), None);
        assert!(
            GitVersion {
                major: 2,
                minor: 31,
                patch: 0
            } >= MIN_GIT
        );
        assert!(
            GitVersion {
                major: 2,
                minor: 30,
                patch: 6
            } < MIN_GIT
        );
    }

    #[test]
    fn a_sha_is_forty_lowercase_hex_and_nothing_else() {
        assert!(is_sha(&"a".repeat(40)));
        assert!(!is_sha(&"A".repeat(40)));
        assert!(!is_sha(&"a".repeat(39)));
        assert!(!is_sha(&"g".repeat(40)));
        assert!(!is_sha("HEAD"));
    }

    #[test]
    fn a_range_without_from_is_just_the_tip() {
        assert_eq!(revs(None, false, "deadbeef"), vec!["deadbeef".to_owned()]);
        assert_eq!(
            revs(Some("abc123"), true, "deadbeef"),
            vec![
                "deadbeef".to_owned(),
                "--not".to_owned(),
                "abc123^".to_owned()
            ]
        );
        // A root `from` has no parent to exclude: the walk ends at the root by itself.
        assert_eq!(
            revs(Some("abc123"), false, "deadbeef"),
            vec!["deadbeef".to_owned()]
        );
    }
}
