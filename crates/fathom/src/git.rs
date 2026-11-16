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
//!   one transcript of the whole walk. Nothing here asks git for patch text: `--numstat` and
//!   `--raw` are counts and modes, never content, so no file body ever enters this process.
//!
//! ## Grammar note: why `NUL`
//!
//! Every stream this module produces is `NUL`-separated, because `NUL` is the one byte that cannot
//! appear in a commit object (git stores commit headers and messages as C strings), so a field
//! split on `NUL` cannot be desynchronised by message text, an author name or — with `-z` — a file
//! name. Control bytes like `0x1e` are used only as *record* markers, placed by fathom's own
//! `--format` and never inside user data.

use std::{
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Command, Stdio},
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
        let mut reader = BufReader::new(stdout);
        let mut token: Vec<u8> = Vec::new();
        let mut failure: Option<E> = None;
        loop {
            token.clear();
            match reader.read_until(0, &mut token) {
                Ok(0) => break,
                Ok(_) => {}
                Err(err) => {
                    failure = Some(E::from(GitError::Io {
                        context: "git's output".to_owned(),
                        reason: err.to_string(),
                    }));
                    break;
                }
            }
            stats.bytes += u64::try_from(token.len()).unwrap_or(u64::MAX);
            if token.last() == Some(&0) {
                token.pop();
            }
            stats.max_token_bytes = stats.max_token_bytes.max(token.len());
            stats.tokens += 1;
            if let Err(err) = on_token(&token) {
                failure = Some(err);
                break;
            }
        }

        // Every exit path waits for the child: an unwaited child is a zombie, and a killed one whose
        // output this thread never drained would have left the stderr thread holding the pipe.
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
