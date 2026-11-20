//! Boot-time validation: what `fathom` refuses before it can do anything else.
//!
//! Nothing here starts a subprocess, reads `.evidence`, or draws a frame. The ticket's line is the
//! rule: **boot-time validation only**, each refusal a typed error with an actionable message, in
//! the shape `act_mcp::config` uses — one line naming the offending value, then a `fix:` line that
//! changes it. What is checked, and what a failure costs:
//!
//! | check | failure |
//! |---|---|
//! | the workspace exists and is a directory | refusal |
//! | the workspace is a git work tree | refusal — every row fathom draws is a commit |
//! | the evidence root exists | **warning**: an unmeasured repo is a legitimate thing to open |
//! | the cache root and the evidence root do not nest | refusal — deleting the cache would destroy evidence |
//! | the cache root exists (create it), `LAYOUT.md` is there (write it if absent) | refusal if it cannot be created |
//! | the cache root is writable | refusal for a writer (`fathom index`), warning for a reader |
//!
//! The last line is ADR-0006 §10.1's split, kept exactly: a build agent that mounted the workspace
//! read-only can still read a store someone else built, so read-only commands carry on and say so.
//!
//! The roots themselves are resolved here and never from `fathom.toml` (ADR-0006 §3.1): flag, then
//! environment, then `<workspace>/.evidence` / `<workspace>/.fathom`.

use std::{
    borrow::Cow,
    path::{Component, Path, PathBuf},
};

use thiserror::Error;

use crate::cli::CommonArgs;

/// The evidence root's default directory name (ADR-0002 §1.1), the same one `act-mcp` writes.
pub const EVIDENCE_DIR_NAME: &str = ".evidence";
/// The cache root's default directory name (ADR-0006 §3.1).
pub const CACHE_DIR_NAME: &str = ".fathom";
/// The lock file inside the cache root (§4.1). Created as the write probe and never unlinked (§9 L3).
pub const LOCK_FILE_NAME: &str = "fathom.lock";
/// The on-disk contract written into the cache root if absent (§3.2 P2's third consequence).
pub const LAYOUT_FILE_NAME: &str = "LAYOUT.md";

/// The committed copy of the cache-root contract, embedded so there is exactly one copy of it in
/// this repository (`.fathom/LAYOUT.md` is tracked; `scripts/validate_fathom_layout.py` C3 fails
/// the build if a second `LAYOUT.md` ever appears, so fathom must not grow one).
const LAYOUT_MD: &str = include_str!("../../../.fathom/LAYOUT.md");

/// Whether this command intends to write the cache. ADR-0006 §10.1 splits the two: `fathom index`
/// refuses an unwritable root, `fathom tui` / `fathom report` / `index --status` read a store
/// someone else built and warn instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// The command writes the store (`fathom index`).
    Write,
    /// The command only reads it (`fathom tui`, `fathom report`, `fathom index --status`).
    Read,
}

impl Access {
    /// `true` for [`Access::Write`].
    #[must_use]
    pub const fn is_write(self) -> bool {
        matches!(self, Self::Write)
    }
}

/// Every boot-time refusal.
///
/// Same convention as `act_mcp::config::ConfigError`: the `#[error(...)]` string is the
/// documentation of the variant — it is what the user reads — so `missing_docs` is allowed on the
/// enum rather than padded with a copy of the same sentence.
#[allow(
    missing_docs,
    reason = "the #[error] format string documents each variant"
)]
#[derive(Debug, Error)]
pub enum BootError {
    #[error("workspace does not exist: {path}\n  fix: pass a workspace that is a checkout, e.g. fathom ~/src/this-repo")]
    WorkspaceMissing { path: PathBuf },

    #[error("workspace is not a directory: {path}")]
    WorkspaceNotADirectory { path: PathBuf },

    #[error(
        "workspace {path} is not a git work tree (no `.git` directory or file)\n  \
         fix: every row fathom draws is a commit, so point it at a checkout — fathom <path/to/repo>"
    )]
    NotAGitWorkTree { path: PathBuf },

    #[error("cannot read the current working directory: {reason}\n  fix: pass the workspace path explicitly, e.g. fathom ~/src/this-repo")]
    NoCurrentDirectory { reason: String },

    #[error("cache root is not a directory: {path}\n  fix: remove or rename it, or pass --cache-dir <path>; fathom will not follow a symlink out of the cache root (ADR-0006 §4.2)")]
    CacheRootNotADirectory { path: PathBuf },

    #[error("evidence root is not a directory: {path}\n  fix: point --evidence-dir at a directory, or omit it to use {EVIDENCE_DIR_NAME} in the workspace")]
    EvidenceRootNotADirectory { path: PathBuf },

    #[error("cannot create the cache root {path}: {reason}\n  fix: make the parent writable, or pass --cache-dir <path> on a filesystem you can write (ADR-0006 §3)")]
    CacheRootUnusable { path: PathBuf, reason: String },

    #[error(
        "cache root {cache} is inside the evidence root {evidence}\n  \
         fix: keep them separate — the cache is `rm -rf`-able and the evidence is not (ADR-0006 §3.1)"
    )]
    CacheInsideEvidence { cache: PathBuf, evidence: PathBuf },

    #[error(
        "evidence root {evidence} is inside the cache root {cache}\n  \
         fix: keep them separate — deleting a cache must never delete evidence (ADR-0006 §3.1)"
    )]
    EvidenceInsideCache { evidence: PathBuf, cache: PathBuf },

    #[error(
        "cannot write the cache root {path}: {reason}\n  \
         fix: pass --cache-dir <path> on a filesystem you can write (ADR-0006 §3), or make the \
         directory writable; `fathom index` needs a writable store and will not pretend otherwise"
    )]
    CacheRootNotWritable { path: PathBuf, reason: String },
}

/// The resolved evidence root: a path, and whether it is there yet.
#[derive(Debug, Clone)]
pub struct EvidenceRoot {
    /// Canonicalised root.
    pub path: PathBuf,
    /// Whether it exists. `false` is a warning, never a refusal: an unmeasured repository is a
    /// legitimate thing to open, and every column renders the unknown glyph rather than a zero.
    pub present: bool,
}

impl EvidenceRoot {
    /// The root as a `status`-line value.
    #[must_use]
    pub fn state(&self) -> &'static str {
        if self.present {
            "present"
        } else {
            "absent"
        }
    }
}

/// The resolved cache root, with the results of the boot checks that touch the filesystem.
#[derive(Debug, Clone)]
pub struct CacheRoot {
    /// Canonicalised root (created at boot if it did not exist).
    pub path: PathBuf,
    /// Whether the write probe (`O_RDWR` on `fathom.lock`, ADR-0006 §10.1) succeeded.
    pub writable: bool,
    /// Why the writes refused, when they did — the OS's own words, kept verbatim so a user can
    /// search for them.
    pub write_refusals: Vec<String>,
    /// Whether `LAYOUT.md` was written by this run (as opposed to already being there).
    pub layout_written: bool,
}

impl CacheRoot {
    /// The root as a `status`-line value.
    #[must_use]
    pub fn state(&self) -> &'static str {
        if self.writable {
            "writable"
        } else {
            "read-only"
        }
    }
}

/// Everything boot validation answers, in one value the commands and `--status` read.
#[derive(Debug, Clone)]
pub struct Boot {
    /// The canonicalised workspace.
    pub workspace: PathBuf,
    /// The repository top level, found by walking up from the workspace (`None` only when the
    /// workspace is not in a repository at all, which [`Boot::validate`] refuses).
    pub git_root: Option<PathBuf>,
    /// The evidence root (read-only for fathom, ADR-0002).
    pub evidence: EvidenceRoot,
    /// The cache root (disposable, written by `fathom index`).
    pub cache: CacheRoot,
    /// Non-fatal findings, already phrased as a line plus a `fix:`. Printed on stderr by the
    /// command layer, never on stdout: stdout is data.
    pub warnings: Vec<String>,
}

impl Boot {
    /// Resolve the workspace argument: `.` when absent, canonicalised either way.
    ///
    /// # Errors
    ///
    /// [`BootError::WorkspaceMissing`], [`BootError::WorkspaceNotADirectory`] or
    /// [`BootError::NoCurrentDirectory`].
    pub fn resolve_workspace(argument: Option<&Path>) -> Result<PathBuf, BootError> {
        let given = argument.unwrap_or(Path::new("."));
        let path = resolve_path(given)?;
        if !path.exists() {
            return Err(BootError::WorkspaceMissing { path });
        }
        if !path.is_dir() {
            return Err(BootError::WorkspaceNotADirectory { path });
        }
        Ok(path)
    }

    /// The repository top level for `path`: the first directory that carries a `.git` (a directory
    /// in an ordinary checkout, a file in a `git worktree`), `path` included.
    ///
    /// Deliberately not `git rev-parse --show-toplevel`: no command runs in this ticket (ADR-0006
    /// §3.1's *named* top level is what by1.8's git reader will produce; this is the boot-time
    /// approximation of it, and it is the same rule git applies walking up).
    #[must_use]
    pub fn git_root(path: &Path) -> Option<PathBuf> {
        let mut current = Some(path);
        while let Some(directory) = current {
            let dot_git = directory.join(".git");
            if dot_git.symlink_metadata().is_ok() {
                return Some(directory.to_path_buf());
            }
            current = directory.parent();
        }
        None
    }

    /// Check the workspace, resolve both roots, and prepare the cache root.
    ///
    /// The workspace is canonicalised here, so every path this returns — and every path a refusal
    /// names — is a place rather than a spelling, whatever the caller passed.
    ///
    /// `access` decides whether an unwritable cache root is a refusal ([`Access::Write`]) or a
    /// warning ([`Access::Read`]); everything else is the same set of checks.
    ///
    /// # Errors
    ///
    /// Any [`BootError`] whose check failed. Warnings are returned, not errors: an unmeasured
    /// repository and a read-only workspace are both states a user needs to be *told* about.
    pub fn validate(
        workspace: &Path,
        arguments: &CommonArgs,
        access: Access,
    ) -> Result<Self, BootError> {
        let mut warnings = Vec::new();

        // Resolve the workspace first, with the same rule the two roots get below. The CLI hands us
        // an already-canonical path (`resolve_workspace`), but `validate` is public and called
        // directly too, and macOS' `$TMPDIR` lives under `/var`, which is a symlink to
        // `/private/var`: an unresolved workspace would leave `workspace` and `git_root` spelled one
        // way and `evidence`/`cache` — which do go through `resolve_path` — spelled the other, in the
        // same `--status` output and in the same warning line. A path the caller can `cd` to either
        // way is a bug, not a platform quirk (docs/testing.md §2).
        let workspace = resolve_path(workspace)?;

        let git_root = Self::git_root(&workspace).ok_or_else(|| BootError::NotAGitWorkTree {
            path: workspace.clone(),
        })?;

        // --- the evidence root: read-only for fathom, and its absence is only a warning --------
        let evidence_path = resolve_path(
            arguments
                .evidence_dir
                .as_deref()
                .unwrap_or(&workspace.join(EVIDENCE_DIR_NAME)),
        )?;
        let evidence = match evidence_path.symlink_metadata() {
            Ok(meta) if meta.is_dir() => EvidenceRoot {
                path: evidence_path,
                present: true,
            },
            Ok(_) => {
                return Err(BootError::EvidenceRootNotADirectory {
                    path: evidence_path,
                });
            }
            Err(_) => {
                warnings.push(format!(
                    "no evidence directory in this workspace: {evidence_path}\n  \
                     fix: run the server once (`act-mcp serve --workspace {workspace}`) to write \
                     it, or pass --evidence-dir; coverage, benchmarks and action success will read \
                     as unknown, not as zero",
                    evidence_path = evidence_path.display(),
                    workspace = workspace.display(),
                ));
                EvidenceRoot {
                    path: evidence_path,
                    present: false,
                }
            }
        };

        // --- the cache root: disposable, and fathom owns exactly the paths of §4.1 -------------
        let cache_path = resolve_path(
            arguments
                .cache_dir
                .as_deref()
                .unwrap_or(&workspace.join(CACHE_DIR_NAME)),
        )?;
        if cache_path
            .symlink_metadata()
            .is_ok_and(|meta| !meta.is_dir())
        {
            return Err(BootError::CacheRootNotADirectory {
                path: cache_path.clone(),
            });
        }
        if is_within(&cache_path, &evidence.path) {
            return Err(BootError::CacheInsideEvidence {
                cache: cache_path,
                evidence: evidence.path.clone(),
            });
        }
        if is_within(&evidence.path, &cache_path) {
            return Err(BootError::EvidenceInsideCache {
                evidence: evidence.path,
                cache: cache_path,
            });
        }

        create_directory(&cache_path)?;

        // Two writes happen at boot: `LAYOUT.md` when it is absent, and the write probe — an
        // `O_RDWR` open of `fathom.lock`, not `access(2)` (ACLs, read-only mounts and squash-on-NFS
        // all make `access` a liar) and not a scratch file (§10.1). Taking the lock itself is
        // mact-by1.10's writer story; this ticket only asks whether the write *could* happen.
        // Either may fail on a read-only workspace, which is a state a reader survives (§10.1).
        let mut refusals: Vec<String> = Vec::new();
        let mut layout_written = false;
        match write_layout_if_absent(&cache_path) {
            Ok(written) => layout_written = written,
            Err(reason) => refusals.push(reason),
        }
        if let Err(reason) = probe_writable(&cache_path) {
            refusals.push(reason);
        }
        if !refusals.is_empty() && access.is_write() {
            return Err(BootError::CacheRootNotWritable {
                path: cache_path.clone(),
                reason: refusals.join("; "),
            });
        }
        let writable = refusals.is_empty();
        if !writable {
            warnings.push(format!(
                "cannot write the cache root {cache}: {reason}\n  \
                 fix: fathom will read the existing store; pass --cache-dir <path> or make the \
                 directory writable to index into it",
                cache = cache_path.display(),
                reason = refusals.join("; "),
            ));
        };

        Ok(Self {
            workspace,
            git_root: Some(git_root),
            evidence,
            cache: CacheRoot {
                path: cache_path,
                writable,
                write_refusals: refusals,
                layout_written,
            },
            warnings,
        })
    }

    /// The `key<TAB>value` lines `fathom index --status` prints about the workspace itself.
    ///
    /// `stdout` is data in this crate (ADR-0005 §6), so this is a table and not prose: a `key<TAB>value`
    /// line per fact, which is what `fathom report > timeline.tsv` will grow into.
    #[must_use]
    pub fn status_lines(&self) -> Vec<(String, String)> {
        vec![
            ("workspace".to_owned(), self.workspace.display().to_string()),
            (
                "git_root".to_owned(),
                self.git_root
                    .as_ref()
                    .map_or_else(|| "none".to_owned(), |root| root.display().to_string()),
            ),
            (
                "evidence_root".to_owned(),
                self.evidence.path.display().to_string(),
            ),
            (
                "evidence_state".to_owned(),
                self.evidence.state().to_owned(),
            ),
            (
                "cache_root".to_owned(),
                self.cache.path.display().to_string(),
            ),
            ("cache_state".to_owned(), self.cache.state().to_owned()),
            (
                "cache_layout".to_owned(),
                if self.cache.layout_written {
                    "written".to_owned()
                } else {
                    "present".to_owned()
                },
            ),
            // Deliberately no `store` line here. Boot can only answer where the cache root is and whether
            // it can be written; whether `index/` holds a store this build reads is `store_status`'s
            // answer (and `store.rows`/`store.layout`/… come from the store itself). A placeholder here
            // used to print `store<TAB>absent` above the real `store<TAB>present`, and a `--status` that
            // contradicts itself in its own first column is how a reader concludes the cache never
            // updated — one question, one answering line.
        ]
    }
}

/// A path as the user will recognise it: absolute, with `.`/`..`/symlink components resolved as far
/// as the filesystem allows.
///
/// A root is canonicalised and used as-is (ADR-0006 §3.1), so `--cache-dir /tmp/x` and
/// `cd /tmp && fathom --cache-dir x` mean the same thing and say the same thing in `doctor`.
/// Non-existent tails are re-appended after canonicalising the deepest existing ancestor, because
/// "does not exist yet" is a normal state for a cache root and must not be an error.
fn resolve_path(path: &Path) -> Result<PathBuf, BootError> {
    let absolute: Cow<'_, Path> = if path.is_absolute() {
        Cow::Borrowed(path)
    } else {
        Cow::Owned(
            std::env::current_dir()
                .map_err(|err| BootError::NoCurrentDirectory {
                    reason: err.to_string(),
                })?
                .join(path),
        )
    };

    // Walk up to the deepest ancestor that exists, canonicalise that, re-append the rest.
    let mut suffix: Vec<&std::ffi::OsStr> = Vec::new();
    let mut cursor: &Path = &absolute;
    let base = loop {
        if cursor.symlink_metadata().is_ok() {
            break cursor
                .canonicalize()
                .map_err(|err| BootError::NoCurrentDirectory {
                    reason: err.to_string(),
                })?;
        }
        match cursor.components().next_back() {
            Some(Component::Normal(name)) => suffix.push(name),
            Some(other) => {
                return Err(BootError::NoCurrentDirectory {
                    reason: format!("cannot resolve {} ({other:?})", cursor.display()),
                });
            }
            None => {
                return Err(BootError::NoCurrentDirectory {
                    reason: format!("cannot resolve an absolute path for {}", absolute.display()),
                });
            }
        }
        cursor = cursor
            .parent()
            .unwrap_or_else(|| Path::new(if cfg!(windows) { "C:\\" } else { "/" }));
    };

    let mut out = base;
    for name in suffix.iter().rev() {
        out.push(name);
    }
    Ok(out)
}

/// `true` when `child` is `parent` or lives under it, compared component-wise on
/// already-canonicalised paths.
fn is_within(child: &Path, parent: &Path) -> bool {
    let child: Vec<_> = child.components().collect();
    let parent: Vec<_> = parent.components().collect();
    child.len() >= parent.len() && child[..parent.len()] == parent[..]
}

/// Create `path` (and any missing parents) at mode `0755`, idempotently.
///
/// The mode is set explicitly (via `DirBuilderExt` rather than `libc::fchmod`, so fathom grows no
/// C-free-adjacent dependency) because a user's `umask` must not make a store unreadable by the
/// next process — ADR-0006 §4.1's reason, and ADR-0002 §1.1's mechanism.
fn create_directory(path: &Path) -> Result<(), BootError> {
    if path.is_dir() {
        return Ok(());
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o755);
    }
    builder
        .create(path)
        .map_err(|err| BootError::CacheRootUnusable {
            path: path.to_path_buf(),
            reason: err.to_string(),
        })
}

/// Write `LAYOUT.md` into the cache root if it is not there (never overwrite it), at mode `0644`.
///
/// This is the promise that makes "safe to delete" a fact the user can read: the directory says what
/// it is, in the directory itself. The bytes are the tracked copy in this repository
/// (`.fathom/LAYOUT.md`), so the on-disk contract and the reviewed one cannot drift.
fn write_layout_if_absent(root: &Path) -> Result<bool, String> {
    let path = root.join(LAYOUT_FILE_NAME);
    if path.symlink_metadata().is_ok() {
        return Ok(false);
    }
    std::fs::write(&path, LAYOUT_MD)
        .map_err(|err| format!("{path} ({err})", path = path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644));
    }
    Ok(true)
}

/// The write probe: `O_RDWR` on `fathom.lock`, created at `0644` if absent. Never unlinked (§9 L3).
fn probe_writable(root: &Path) -> Result<(), String> {
    let path = root.join(LOCK_FILE_NAME);
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o644);
    }
    options
        .open(&path)
        .map(|_| ())
        .map_err(|err| format!("{path} ({err})", path = path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A workspace: a directory with a `.git`, which is what `fathom` checks for.
    fn fake_repo(root: &Path) -> PathBuf {
        let repo = root.join("repo");
        std::fs::create_dir_all(repo.join(".git")).expect("make the work tree");
        std::fs::create_dir_all(repo.join("workflows")).expect("make a directory");
        repo.canonicalize().expect("canonical scratch path")
    }

    fn default_arguments() -> CommonArgs {
        CommonArgs {
            evidence_dir: None,
            cache_dir: None,
            from: None,
            to: None,
            first_parent: false,
            config: None,
            plain: false,
            no_color: false,
            limit: None,
            no_auto_index: false,
            diff: None,
            bench: None,
            detail: None,
            max_diff_bytes: None,
            max_diff_bytes_per_file: None,
            verbose: 0,
        }
    }

    #[test]
    fn a_missing_workspace_is_a_refusal_with_a_fix() {
        let dir = tempfile::tempdir().expect("scratch");
        let missing = dir.path().join("not-a-repo");
        let err = Boot::resolve_workspace(Some(&missing)).expect_err("the path is not there");
        assert!(matches!(err, BootError::WorkspaceMissing { .. }));
        assert!(err.to_string().contains("fix:"), "{err}");
    }

    #[test]
    fn a_directory_that_is_not_a_work_tree_is_refused() {
        let dir = tempfile::tempdir().expect("scratch");
        let boot = Boot::validate(dir.path(), &default_arguments(), Access::Read);
        match boot {
            Err(BootError::NotAGitWorkTree { path }) => {
                assert_eq!(path, dir.path().canonicalize().unwrap())
            }
            other => panic!("expected NotAGitWorkTree, got {other:?}"),
        }
    }

    /// macOS' `$TMPDIR` is under `/var`, which is a symlink to `/private/var`, so a temp directory has
    /// two spellings of one place and a test can only reproduce the confusion by making its own
    /// symlink. `Boot` must not report both spellings: `--status` would print a `workspace` that the
    /// `cache_root` beside it is not inside, and the nesting checks below compare paths component-wise.
    #[cfg(unix)] // the symlink is the whole point, and only unix has the `$TMPDIR` that makes it bite
    #[test]
    fn one_directory_with_two_spellings_is_reported_as_one_of_them() {
        let dir = tempfile::tempdir().expect("scratch");
        let base = dir.path().canonicalize().expect("canonical scratch");

        // A work tree, and a second name for it: `link` is a directory to everything that walks it.
        let real = base.join("real");
        std::fs::create_dir_all(real.join(".git")).expect("make the work tree");
        let link = base.join("link");
        std::os::unix::fs::symlink(&real, &link).expect("make the symlink");
        assert_ne!(link, real, "the test needs two spellings to mean anything");

        // Entering through either spelling must give the same answer out: the place, once.
        for entered in [&link, &real] {
            let boot = Boot::validate(entered, &default_arguments(), Access::Read).expect("boot");
            assert_eq!(
                boot.workspace, real,
                "the workspace is the place, not the spelling"
            );
            assert_eq!(boot.git_root.as_deref(), Some(real.as_path()));
            assert_eq!(boot.evidence.path, real.join(EVIDENCE_DIR_NAME));
            assert_eq!(boot.cache.path, real.join(CACHE_DIR_NAME));
        }
    }

    #[test]
    fn a_git_file_counts_because_that_is_what_a_worktree_looks_like() {
        let dir = tempfile::tempdir().expect("scratch");
        let repo = dir.path().join("wt");
        std::fs::create_dir_all(&repo).expect("dir");
        std::fs::write(repo.join(".git"), "gitdir: /elsewhere/.git/worktrees/wt\n")
            .expect("git file");
        let repo = repo.canonicalize().expect("canonical");
        let boot = Boot::validate(&repo, &default_arguments(), Access::Read)
            .expect("a worktree is a work tree");
        assert_eq!(boot.git_root.as_deref(), Some(repo.as_path()));
    }

    #[test]
    fn a_missing_evidence_directory_warns_and_the_run_still_booted() {
        let dir = tempfile::tempdir().expect("scratch");
        let repo = fake_repo(dir.path());
        let boot =
            Boot::validate(&repo, &default_arguments(), Access::Read).expect("unmeasured is legal");
        assert!(!boot.evidence.present);
        assert_eq!(boot.evidence.state(), "absent");
        assert_eq!(boot.warnings.len(), 1, "{:?}", boot.warnings);
        assert!(boot.warnings[0].contains("fix:"), "{}", boot.warnings[0]);
        assert!(boot.cache.writable, "the scratch cache root is writable");
        assert!(boot.cache.layout_written, "LAYOUT.md was written");
        assert!(boot.cache.path.join(LAYOUT_FILE_NAME).is_file());
        assert!(boot.cache.path.join(LOCK_FILE_NAME).is_file());
    }

    #[test]
    fn boot_is_idempotent_and_never_overwrites_layout_md() {
        let dir = tempfile::tempdir().expect("scratch");
        let repo = fake_repo(dir.path());
        let first = Boot::validate(&repo, &default_arguments(), Access::Read).expect("boot");
        assert!(first.cache.layout_written);
        std::fs::write(
            repo.join(CACHE_DIR_NAME).join(LAYOUT_FILE_NAME),
            "hand written\n",
        )
        .expect("overwrite it by hand");
        let second = Boot::validate(&repo, &default_arguments(), Access::Read).expect("boot again");
        assert!(
            !second.cache.layout_written,
            "an existing LAYOUT.md is left alone (§3.2)"
        );
        assert_eq!(
            std::fs::read_to_string(repo.join(CACHE_DIR_NAME).join(LAYOUT_FILE_NAME)).unwrap(),
            "hand written\n"
        );
        assert_eq!(
            first.warnings, second.warnings,
            "the same workspace should say the same thing twice"
        );
        assert!(
            !second
                .warnings
                .iter()
                .any(|line| line.contains("cannot write the cache root")),
            "nothing refused here: {:?}",
            second.warnings
        );
    }

    #[test]
    fn the_two_roots_may_not_nest_in_either_direction() {
        let dir = tempfile::tempdir().expect("scratch");
        let repo = fake_repo(dir.path());

        let mut arguments = default_arguments();
        arguments.cache_dir = Some(repo.join(EVIDENCE_DIR_NAME).join("cache"));
        let err = Boot::validate(&repo, &arguments, Access::Write)
            .expect_err("nested: cache in evidence");
        assert!(
            matches!(err, BootError::CacheInsideEvidence { .. }),
            "{err}"
        );

        let mut arguments = default_arguments();
        arguments.evidence_dir = Some(repo.join(CACHE_DIR_NAME).join("evidence"));
        let err = Boot::validate(&repo, &arguments, Access::Write)
            .expect_err("nested: evidence in cache");
        assert!(
            matches!(err, BootError::EvidenceInsideCache { .. }),
            "{err}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn an_unwritable_cache_root_refuses_a_writer_and_warns_a_reader() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("scratch");
        let repo = fake_repo(dir.path());
        let cache = repo.join(CACHE_DIR_NAME);
        std::fs::create_dir_all(&cache).expect("cache dir");
        std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o555))
            .expect("make the cache read-only");

        // That mode is a request, not a guarantee: root overrides directory modes, and root is the
        // uid every container runs `cargo test` as — act's job container included, where this test
        // failed for no reason but its uid. So probe the precondition instead of naming a uid: if a
        // file can still be created in a 0555 directory, this environment cannot express §10.1's
        // refusal, and asserting anyway would grade the caller's privileges rather than Fathom.
        if std::fs::write(cache.join("probe"), b"x").is_ok() {
            let _ = std::fs::remove_file(cache.join("probe"));
            std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o755))
                .expect("restore, so TempDir can clean up");
            eprintln!(
                "note: skipping; this uid can write to a 0555 directory, so the unwritable cache \
                 root of ADR-0006 §10.1 cannot be produced here"
            );
            return;
        }

        let reader = Boot::validate(&repo, &default_arguments(), Access::Read)
            .expect("a reader proceeds read-only (§10.1)");
        assert!(!reader.cache.writable);
        assert_eq!(reader.cache.state(), "read-only");
        assert!(
            reader
                .warnings
                .iter()
                .any(|line| line.contains("cannot write the cache root")),
            "{:?}",
            reader.warnings
        );

        let writer = Boot::validate(&repo, &default_arguments(), Access::Write)
            .expect_err("index refuses an unwritable store (§10.1)");
        assert!(
            matches!(writer, BootError::CacheRootNotWritable { .. }),
            "{writer}"
        );
        assert!(writer.to_string().contains("fix:"));

        // Leave the scratch tree deletable for TempDir.
        std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o755)).expect("restore");
    }

    #[test]
    fn a_relative_root_resolves_against_the_working_directory_and_is_reported_absolute() {
        let dir = tempfile::tempdir().expect("scratch");
        let repo = fake_repo(dir.path());
        let mut arguments = default_arguments();
        arguments.cache_dir = Some(PathBuf::from("elsewhere"));
        let previous = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(&repo).expect("cd into the scratch repo");
        let boot = Boot::validate(&repo, &arguments, Access::Read).expect("relative cache dir");
        std::env::set_current_dir(&previous).expect("back");
        assert!(boot.cache.path.is_absolute());
        assert!(boot.cache.path.ends_with("elsewhere"));
    }

    #[test]
    fn is_within_is_componentwise_not_textual() {
        assert!(is_within(Path::new("/a/b/c"), Path::new("/a/b")));
        assert!(is_within(Path::new("/a/b"), Path::new("/a/b")));
        assert!(
            !is_within(Path::new("/a/bc"), Path::new("/a/b")),
            "a `/a/bc` prefix is not containment"
        );
    }

    #[test]
    fn status_lines_answer_the_store_question_honestly() {
        let dir = tempfile::tempdir().expect("scratch");
        let repo = fake_repo(dir.path());
        let boot = Boot::validate(&repo, &default_arguments(), Access::Read).expect("boot");
        let lines = boot.status_lines();
        let keys: Vec<&str> = lines.iter().map(|(key, _)| key.as_str()).collect();
        assert!(keys.contains(&"workspace"));
        assert!(keys.contains(&"evidence_state"));
        assert!(keys.contains(&"cache_state"));
        // `store` is the store's own line, printed by `fathom index --status` out of the opened store
        // (or as `absent` when there is none). Two lines claiming to answer it, one of them a hardcoded
        // guess, is a `--status` that contradicts itself.
        assert_eq!(
            keys.iter().filter(|key| **key == "store").count(),
            0,
            "boot does not answer the store question"
        );
    }
}
