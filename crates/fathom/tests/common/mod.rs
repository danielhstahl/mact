//! Throwaway git repositories for fathom's integration tests — the fathom equivalent of
//! `act_mcp::fixtures::TestRepo`, which this crate may not depend on (ADR-0005 P4: a reader does not
//! borrow the server, not even in a test; the shared-fixture escape hatch ADR-0005 §6 names is
//! `act-evidence`'s, and nothing here needs it).
//!
//! Real `git`, real objects, real SHAs, because the contract under test is "what `git log --raw
//! --numstat -z` answers". The child environment is pinned the same way act-mcp's fixture pins it:
//! no global or system config, no hooks, no signing, no prompts, and a fixed author/committer clock
//! so a test can assert on an instant rather than on the wall clock.
//!
//! [`Repo::fast_import`] exists because a 1 000-commit fixture built one `git commit` at a time would
//! spend its budget on process spawns and measure nothing; one `fast-import` stream builds it in a
//! fraction of a second.

#![allow(dead_code, reason = "one helper module serves several test binaries")]

use std::{
    io::Write,
    path::{Path, PathBuf},
    process::Command,
};

/// The instant every fixture commit is stamped with, and the epoch second a test asserts back.
pub const FIXED_EPOCH: &str = "2026-01-01T00:00:00Z";
pub const FIXED_EPOCH_SECONDS: i64 = 1_767_225_600;

/// A repository in a temporary directory.
#[derive(Debug, Clone)]
pub struct Repo {
    path: PathBuf,
}

impl Repo {
    /// `git init` at `path` (on `main`, with no commits yet).
    pub fn init(path: &Path) -> Self {
        std::fs::create_dir_all(path).expect("create the fixture directory");
        let repo = Self {
            path: canonical(path),
        };
        repo.git(&["init", "--quiet", "--initial-branch=main"]);
        repo
    }

    /// A handle for a directory that already holds a repository (a clone, a work tree).
    pub fn open(path: &Path) -> Self {
        Self {
            path: canonical(path),
        }
    }

    /// The repository root.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Write `content` to `relpath`, creating parent directories.
    pub fn write(&self, relpath: &str, content: &str) {
        let path = self.path.join(relpath);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create the file's directory");
        }
        std::fs::write(path, content).expect("write the file");
    }

    /// Write raw bytes (a fixture that must not be valid UTF-8).
    pub fn write_bytes(&self, relpath: &str, content: &[u8]) {
        let path = self.path.join(relpath);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create the file's directory");
        }
        std::fs::write(path, content).expect("write the file");
    }

    /// Remove a path from the working tree (unstaged; a following [`Repo::commit`] stages it).
    pub fn remove(&self, relpath: &str) {
        std::fs::remove_file(self.path.join(relpath)).expect("remove the file");
    }

    /// Copy a tracked file to a new path (staged by the next [`Repo::commit`]).
    pub fn copy(&self, from: &str, to: &str) {
        let content = std::fs::read(self.path.join(from)).expect("read the source file");
        self.write_bytes(to, &content);
    }

    /// `git add -A` and commit `message`, returning the new SHA.
    pub fn commit(&self, message: &str) -> String {
        self.git(&["add", "-A"]);
        self.commit_staged(message)
    }

    /// Commit what is already staged, returning the new SHA.
    pub fn commit_staged(&self, message: &str) -> String {
        self.git(&["commit", "--quiet", "-m", message]);
        self.head()
    }

    /// `git commit --allow-empty -m …`, for a commit that changes nothing.
    pub fn commit_empty(&self, message: &str) -> String {
        self.git(&["commit", "--quiet", "--allow-empty", "-m", message]);
        self.head()
    }

    /// The SHA `HEAD` points at.
    pub fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"])
    }

    /// Create `name` at the current commit.
    pub fn branch(&self, name: &str) {
        self.git(&["branch", name]);
    }

    /// Check `name` out.
    pub fn checkout(&self, name: &str) {
        self.git(&["checkout", "--quiet", name]);
    }

    /// Check `name` out with a detached `HEAD`.
    pub fn checkout_detach(&self, name: &str) {
        self.git(&["checkout", "--quiet", "--detach", name]);
    }

    /// Move the current branch to `rev`, discarding what was above it — a force push, in local
    /// shape: the old commits stay in the object database and out of the walk.
    pub fn reset_hard(&self, rev: &str) {
        self.git(&["reset", "--hard", rev]);
    }

    /// `git merge --no-ff other`.
    pub fn merge(&self, name: &str, message: &str) {
        self.git(&["merge", "--no-ff", "--quiet", "-m", message, name]);
    }

    /// Stage a mode change (`chmod +x`) without touching content.
    pub fn make_executable(&self, relpath: &str) {
        self.git(&["update-index", "--chmod=+x", relpath]);
    }

    /// A clone of this repository, shallow at `depth` commits.
    pub fn clone_shallow(&self, into: &Path, depth: usize) -> Repo {
        self.git(&[
            "clone",
            "--quiet",
            &format!("--depth={depth}"),
            &format!("file://{}", self.path.display()),
            &into.display().to_string(),
        ]);
        Repo {
            path: canonical(into),
        }
    }

    /// A linked work tree of this repository (`git worktree add`).
    pub fn worktree(&self, into: &Path, branch: &str) -> Repo {
        std::fs::create_dir_all(into.parent().unwrap_or(into)).expect("create the worktree parent");
        self.git(&[
            "worktree",
            "add",
            "--quiet",
            &into.display().to_string(),
            branch,
        ]);
        Repo {
            path: canonical(into),
        }
    }

    /// Resolve a revision to a full SHA.
    pub fn rev(&self, revision: &str) -> String {
        self.git(&["rev-parse", revision])
    }

    /// The parents of `rev`, first-parent first.
    pub fn parents(&self, rev: &str) -> Vec<String> {
        let line = self.git(&["rev-parse", &format!("{rev}^@")]);
        line.split_whitespace().map(str::to_owned).collect()
    }

    /// git's own answer for one commit's line totals, as an independent oracle: `git show --numstat`
    /// — quoted paths, no `-z`, no `--raw`, a different code path from the reader under test. The
    /// first two columns are all that is read, so a path git chose to quote cannot confuse it.
    ///
    /// Returns `(adds, dels, file_lines)`.
    pub fn numstat_oracle(&self, sha: &str) -> (u64, u64, usize) {
        let out = self.git(&[
            "show",
            "--numstat",
            "--format=",
            "--root",
            "--diff-merges=first-parent",
            "-M",
            sha,
        ]);
        let mut adds = 0_u64;
        let mut dels = 0_u64;
        let mut files = 0_usize;
        for line in out.lines() {
            let mut columns = line.split('\t');
            let (Some(add), Some(del)) = (columns.next(), columns.next()) else {
                continue;
            };
            files += 1;
            // `-` is git's "binary, no counts": zero lines, one file.
            adds += add.parse().unwrap_or(0);
            dels += del.parse().unwrap_or(0);
        }
        (adds, dels, files)
    }

    /// Build `commits` commits in one `git fast-import` stream, each touching two files.
    /// Each file's body is `body_lines` lines long on purpose: a reader that asked git for patch
    /// text would show it in [`fathom::git::StreamStats`], and empty files could not tell.
    /// A realistic 1 000-commit shape: a *fixed* set of files, each commit touching one of them,
    /// instead of a new file per commit.
    ///
    /// [`Repo::fast_import`] grows the tree by two files per commit, which no real repository does
    /// and which makes every per-tree table O(commits) — a fixture like that measures the tree, not
    /// the indexer. The budget test wants "1 000 commits in a repo that looks like a repo", which is
    /// what this builds: `files` files, `commits` commits, `commits / files` of them touching each
    /// file, and one `git fast-import` stream so the fixture costs a second rather than a minute.
    pub fn fast_import_fixed_set(&self, commits: usize, files: usize, body_lines: usize) {
        let mut import = Command::new("git")
            .arg("-C")
            .arg(&self.path)
            .args(["fast-import", "--quiet"])
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("LC_ALL", "C")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("start git fast-import");
        let mut stdin = import.stdin.take().expect("fast-import's stdin");
        let filler = "// filler line, so a blob has a body a reader could mistakenly read\n"
            .repeat(body_lines.max(1));
        // The first commit lays the whole tree down; every commit after it rewrites one file.
        for index in 0..commits {
            // Blobs first, always: fast-import will not read a `blob` command in the middle of a
            // `commit`, and the "Unsupported command" it answers with is not a helpful symptom.
            let laid = if index == 0 {
                (0..files).collect::<Vec<usize>>()
            } else {
                vec![index % files]
            };
            let mut marks: Vec<(usize, usize)> = Vec::with_capacity(laid.len());
            for slot in laid {
                let blob = 10_000 + index * files + slot;
                let body = if index == 0 {
                    format!("// src/mod{slot}.py\n{filler}")
                } else {
                    format!("// src/mod{slot}.py, revision {index}\n{filler}")
                };
                write_blob(&mut stdin, blob, &body);
                marks.push((slot, blob));
            }
            let mark = 900_000 + index;
            let message =
                format!("commit {index}\n\nBody line, so a head and a body both exist.\n");
            write!(
                stdin,
                "commit refs/heads/main\nmark :{mark}\ncommitter Fathom Fixture <fixture@example.invalid> 1700000000 +0000\ndata {}\n",
                message.len()
            )
            .expect("write the commit header");
            stdin
                .write_all(message.as_bytes())
                .expect("write the message");
            stdin.write_all(b"\n").expect("terminate the message");
            if index > 0 {
                writeln!(stdin, "from :{}", mark - 1).expect("write the parent");
            }
            for (slot, blob) in marks {
                writeln!(stdin, "M 100644 :{blob} src/mod{slot}.py").expect("write the file");
            }
        }
        stdin.flush().expect("flush fast-import");
        drop(stdin);
        let status = import.wait().expect("fast-import finished");
        assert!(status.success(), "git fast-import failed");
    }

    pub fn fast_import(&self, commits: usize, body_lines: usize) {
        let mut import = Command::new("git")
            .arg("-C")
            .arg(&self.path)
            .args(["fast-import", "--quiet"])
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("LC_ALL", "C")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("start git fast-import");
        let mut stdin = import.stdin.take().expect("fast-import's stdin");
        // One blob per commit, referenced by a mark; the commit line references the marks.
        let filler = "// filler line, so a blob has a body a reader could mistakenly read\n"
            .repeat(body_lines.max(1));
        for index in 0..commits {
            let content = format!("// file {index}\n{filler}");
            let second = format!("// second {index}\n{filler}");
            let first_mark = 2 * index + 1;
            let second_mark = 2 * index + 2;
            write_blob(&mut stdin, first_mark, &content);
            write_blob(&mut stdin, second_mark, &second);
            let message =
                format!("commit {index}\n\nBody line, so a head and a body both exist.\n");
            let mark = 500_000 + index;
            write!(
                stdin,
                "commit refs/heads/main\nmark :{mark}\ncommitter Fathom Fixture <fixture@example.invalid> 1700000000 +0000\ndata {}\n",
                message.len()
            )
            .expect("write the commit header");
            // fast-import wants the raw message bytes and a newline after them.
            stdin
                .write_all(message.as_bytes())
                .expect("write the message");
            stdin.write_all(b"\n").expect("terminate the message");
            if index > 0 {
                writeln!(stdin, "from :{}", mark - 1).expect("write the parent");
            }
            writeln!(stdin, "M 100644 :{first_mark} src/mod{index}.rs").expect("write the file");
            writeln!(stdin, "M 100644 :{second_mark} src/second.rs").expect("write the file");
        }
        stdin.flush().expect("flush fast-import");
        drop(stdin);
        let status = import.wait().expect("fast-import finished");
        assert!(status.success(), "git fast-import failed");
    }

    /// `git log --oneline`, oldest first — a compact shape for a test to assert on.
    pub fn log(&self) -> Vec<String> {
        self.git(&["log", "--format=%H"])
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// Run `git -C <repo> <args>`, panicking with stderr on a non-zero exit.
    pub fn git(&self, args: &[&str]) -> String {
        match self.try_git(args) {
            Ok(stdout) => stdout,
            Err(stderr) => panic!("git {args:?} in {}: {stderr}", self.path.display()),
        }
    }

    /// [`Repo::git`] without the panic.
    pub fn try_git(&self, args: &[&str]) -> Result<String, String> {
        let output = self
            .command(args)
            .output()
            .expect("run git: is git installed?");
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if output.status.success() {
            Ok(stdout)
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
        }
    }

    /// The pinned command every fixture git call runs through.
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(&self.path)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", &self.path)
            .env("LC_ALL", "C")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_AUTHOR_NAME", "Fathom Fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "Fathom Fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .env("GIT_AUTHOR_DATE", FIXED_EPOCH)
            .env("GIT_COMMITTER_DATE", FIXED_EPOCH)
            .args(["-c", "user.name=Fathom Fixture"])
            .args(["-c", "user.email=fixture@example.invalid"])
            .args(["-c", "commit.gpgsign=false"])
            .args(["-c", "core.hooksPath=/dev/null"])
            .args(args);
        command
    }
}

/// One `blob` command in fast-import's grammar.
fn write_blob(stdin: &mut impl Write, mark: usize, content: &str) {
    write!(stdin, "blob\nmark :{mark}\ndata {}\n", content.len()).expect("write the blob header");
    stdin
        .write_all(content.as_bytes())
        .expect("write the blob body");
    stdin.write_all(b"\n").expect("terminate the blob");
}

/// Canonicalise the way [`fathom::git::Git::open`] does, so path comparisons hold.
pub fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}
