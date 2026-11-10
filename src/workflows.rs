//! Which workflow files this workspace has, and which one a name means.
//!
//! [`index_directory`] walks `<tree>/.github/workflows` and hands back one
//! [`WorkflowFile`] per `*.yml`/`*.yaml`, indexed by [`crate::workflow_yaml`];
//! [`resolve`] answers ADR-0001 §2.2's four accepted `workflow` forms against that
//! list. Both are pure filesystem reads: no git, no act, no container runtime, no
//! lock — which is what lets the validator (mact-f1w.6) and `list_tasks`
//! (mact-f1w.7) share one answer instead of agreeing about two.
//!
//! Sharing matters more than it looks. ADR-0001 §5.2 promises that a resolution
//! failure *names the valid options*, so the discovery path and the run path must
//! disagree about nothing: a job id `list_tasks` advertises is a job id
//! `run_workflow` accepts, from the same file, chosen by the same precedence.
//!
//! Two rules the walking has to keep, both from ADR-0001 §2.2:
//!
//! * a file counts only if it canonicalizes **under** `.github/workflows`, so a
//!   symlink pointing out of that directory is not a workflow of this repository
//!   and is not offered as an option either; and
//! * results are sorted by relpath, so `available` lists, `list_tasks` output and
//!   golden tests are stable across filesystems.

use std::{io::Read, path::PathBuf};

use crate::{
    workflow_yaml::{self, WorkflowYamlError},
    workspace::WORKFLOWS_DIR,
};

/// One indexed `.github/workflows` file.
#[derive(Debug, Clone)]
pub struct WorkflowFile {
    /// Workspace-relative forward-slash path, e.g. `.github/workflows/ci.yml`.
    pub relpath: String,
    /// Path on disk (working tree), or the workspace-anchored equivalent when the
    /// file was read out of a commit tree that is not materialized yet.
    pub on_disk: PathBuf,
    /// e.g. `ci.yml`.
    pub file_name: String,
    /// e.g. `ci` — the display name, and the form callers usually pass.
    pub stem: String,
    /// The `name:` key the file declares, if it declares one.
    pub declared_name: Option<String>,
    /// The declared jobs and their details (empty when `parse_error` is set).
    pub index: workflow_yaml::WorkflowIndex,
    /// Why the file could not be indexed, if it could not.
    pub parse_error: Option<WorkflowYamlError>,
}

impl WorkflowFile {
    /// Index `bytes` as the workflow file that lives at `relpath`.
    ///
    /// A parse failure is *kept*, not returned: one unreadable file must not break
    /// discovery (ADR-0001 §6.1), and `run_workflow` needs the file's identity even
    /// when it has to explain why the job id is not in it.
    pub fn new(relpath: impl Into<String>, on_disk: PathBuf, bytes: &[u8]) -> Self {
        let relpath = relpath.into();
        let file_name = relpath
            .rsplit('/')
            .next()
            .unwrap_or(relpath.as_str())
            .to_owned();
        let stem = match file_name.rfind('.') {
            Some(index) if index > 0 => file_name[..index].to_owned(),
            _ => file_name.clone(),
        };
        let (index, parse_error) = match workflow_yaml::index_workflow(bytes) {
            Ok(index) => (index, None),
            Err(error) => (workflow_yaml::WorkflowIndex::default(), Some(error)),
        };
        let declared_name = index.declared_name.clone();
        Self {
            relpath,
            on_disk,
            file_name,
            stem,
            declared_name,
            index,
            parse_error,
        }
    }

    /// Whether this file is the one a `workflow` argument names by path or by
    /// (file) name — ADR-0001 §2.2's forms 1 to 3.
    pub fn matches(&self, requested: &str) -> bool {
        requested == self.relpath
            || requested == self.file_name
            || requested == self.stem
            || requested.strip_prefix(&format!("{WORKFLOWS_DIR}/")) == Some(self.file_name.as_str())
    }

    /// Form 4 of ADR-0001 §2.2: the `name:` key declared inside the file.
    pub fn matches_declared_name(&self, requested: &str) -> bool {
        self.declared_name.as_deref() == Some(requested)
    }
}

/// Why a workflows directory could not be walked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexError {
    /// `.github/workflows` is not there or cannot be resolved.
    Unresolvable,
    /// The directory disappeared or is unreadable mid-walk.
    ReadDir {
        /// What the OS said.
        reason: String,
    },
    /// One file could not be read.
    ReadFile {
        /// Workspace-relative file name.
        file: String,
        /// What the OS said.
        reason: String,
    },
}

/// Index every workflow file under `tree`'s `.github/workflows`, recursively,
/// sorted by relpath.
///
/// `tree` may be the workspace, a materialized worktree, or any directory with the
/// same layout: relpaths are relative to `tree`, so the same string comes out
/// whichever tree a run would actually use.
pub fn index_directory(tree: &std::path::Path) -> Result<Vec<WorkflowFile>, IndexError> {
    let directory = tree.join(WORKFLOWS_DIR);
    let canonical = directory
        .canonicalize()
        .map_err(|_| IndexError::Unresolvable)?;
    let mut files = Vec::new();
    let mut stack = vec![canonical.clone()];
    while let Some(directory) = stack.pop() {
        let read = match std::fs::read_dir(&directory) {
            Ok(read) => read,
            Err(error) => {
                return Err(IndexError::ReadDir {
                    reason: error.to_string(),
                });
            }
        };
        for item in read.flatten() {
            let path = item.path();
            // `metadata` follows symlinks, which is the point: a symlinked directory
            // is descended into only if its files then canonicalize back under
            // `.github/workflows`, and a symlink pointing out of the workspace is
            // dropped below.
            let metadata = match path.metadata() {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };
            if metadata.is_dir() {
                stack.push(path);
                continue;
            }
            if !is_workflow_file(&path) {
                continue;
            }
            let resolved = match path.canonicalize() {
                Ok(resolved) => resolved,
                Err(_) => continue,
            };
            // ADR-0001 §2.2: resolution must end *under* `.github/workflows`.
            if !resolved.starts_with(&canonical) {
                continue;
            }
            let relpath = relative_path(tree, &resolved);
            let bytes = match read_bounded(&resolved) {
                Ok(bytes) => bytes,
                Err(error) => {
                    return Err(IndexError::ReadFile {
                        file: workflow_display_name(&relpath),
                        reason: error.to_string(),
                    });
                }
            };
            files.push(WorkflowFile::new(relpath, resolved, &bytes));
        }
    }
    files.sort_by(|left, right| left.relpath.cmp(&right.relpath));
    Ok(files)
}

/// Why [`resolve`] could not pick one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// Nothing matches. `available` is the list ADR-0001 §5.2 requires the error to
    /// name, and `reason` explains a near-miss (an explicit path that is not there).
    NotFound {
        /// Stems that *are* available, sorted and deduped.
        available: Vec<String>,
        /// Optional explanation of a near miss.
        reason: Option<String>,
    },
    /// Two or more files match one bare name (`bench.yml` + `bench.yaml`), which is
    /// never a precedence coin flip.
    Ambiguous {
        /// The matching relpaths, sorted and deduped.
        candidates: Vec<String>,
    },
}

/// Resolve one `workflow` argument through ADR-0001 §2.2's four forms.
pub fn resolve<'a>(
    files: &'a [WorkflowFile],
    requested: &str,
) -> Result<&'a WorkflowFile, ResolveError> {
    let not_found = |reason: Option<String>| ResolveError::NotFound {
        available: files.iter().map(|file| file.stem.clone()).collect(),
        reason,
    };

    // Form 1: a path. An explicit path that is not there is not silently retried as
    // a bare name — the caller said which file they meant.
    let explicit = requested
        .strip_prefix(&format!("{WORKFLOWS_DIR}/"))
        .map(str::to_owned)
        .or_else(|| requested.contains('/').then(|| requested.to_owned()));
    if let Some(relative) = explicit {
        let wanted = format!("{WORKFLOWS_DIR}/{relative}");
        return files
            .iter()
            .find(|file| file.relpath == wanted)
            .ok_or_else(|| {
                not_found(Some(
                    "no file at that path under `.github/workflows`".to_owned(),
                ))
            });
    }

    // Forms 2 and 3: file name with extension, then basename without it. Two files
    // matching one bare name is ambiguous, never a coin flip.
    for field in ["file_name", "stem"] {
        let matches: Vec<&WorkflowFile> = files
            .iter()
            .filter(|file| match field {
                "file_name" => requested == file.file_name,
                _ => requested == file.stem,
            })
            .collect();
        match matches.len() {
            0 => continue,
            1 => return Ok(matches[0]),
            _ => {
                return Err(ResolveError::Ambiguous {
                    candidates: matches.iter().map(|file| file.relpath.clone()).collect(),
                })
            }
        }
    }

    // Form 4: the `name:` key, if exactly one file declares it.
    let declared: Vec<&WorkflowFile> = files
        .iter()
        .filter(|file| file.matches_declared_name(requested))
        .collect();
    match declared.len() {
        0 => Err(not_found(None)),
        1 => Ok(declared[0]),
        _ => Err(ResolveError::Ambiguous {
            candidates: declared.iter().map(|file| file.relpath.clone()).collect(),
        }),
    }
}

/// Whether `directory` holds at least one workflow file, recursively.
pub fn has_workflow_files(directory: &std::path::Path) -> bool {
    let mut stack = vec![directory.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&current) else {
            continue;
        };
        for item in read.flatten() {
            let path = item.path();
            let Ok(metadata) = path.metadata() else {
                continue;
            };
            if metadata.is_dir() {
                stack.push(path);
            } else if is_workflow_file(&path) {
                return true;
            }
        }
    }
    false
}

/// Whether `path`'s extension marks it a workflow file.
pub fn is_workflow_file(path: &std::path::Path) -> bool {
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some("yml") | Some("yaml")
    )
}

/// Read at most [`workflow_yaml::MAX_WORKFLOW_BYTES`] + 1 bytes; the indexer then
/// rejects anything longer without it having been slurped into memory.
pub fn read_bounded(path: &std::path::Path) -> std::io::Result<Vec<u8>> {
    let mut file = std::fs::File::open(path)?;
    let mut buffer = Vec::new();
    file.by_ref()
        .take((workflow_yaml::MAX_WORKFLOW_BYTES + 1) as u64)
        .read_to_end(&mut buffer)?;
    Ok(buffer)
}

/// `path` relative to `root`, forward slashes.
pub fn relative_path(root: &std::path::Path, path: &std::path::Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// The file name part of a relpath — what an error may name a file by.
pub fn workflow_display_name(relpath: &str) -> String {
    relpath.rsplit('/').next().unwrap_or(relpath).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::TestRepo;

    fn indexed(dir: &std::path::Path) -> Vec<WorkflowFile> {
        index_directory(dir).expect("index")
    }

    #[test]
    fn files_are_listed_sorted_by_relpath() {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = TestRepo::init(&root.path().join("repo"));
        repo.write_workflow("bench.yaml", "on: push\njobs:\n  b:\n    runs-on: x\n");
        repo.write_workflow("zzz.yml", "on: push\njobs:\n  z:\n    runs-on: x\n");
        repo.write(".github/workflows/README.md", "not a workflow\n");
        let files = indexed(repo.path());
        let relpaths: Vec<&str> = files.iter().map(|file| file.relpath.as_str()).collect();
        assert_eq!(
            relpaths,
            vec![
                ".github/workflows/bench.yaml",
                ".github/workflows/ci.yml",
                ".github/workflows/zzz.yml",
            ]
        );
        assert_eq!(files[1].stem, "ci");
        assert_eq!(files[1].file_name, "ci.yml");
        assert_eq!(files[1].declared_name.as_deref(), Some("Fixture CI"));
    }

    #[test]
    fn nested_directories_are_walked_and_relatives_keep_the_layout() {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = TestRepo::init(&root.path().join("repo"));
        repo.write_workflow(
            "reusable/lib.yml",
            "on: workflow_call\njobs:\n  lib:\n    runs-on: x\n",
        );
        let files = indexed(repo.path());
        assert!(files
            .iter()
            .any(|file| file.relpath == ".github/workflows/reusable/lib.yml"));
    }

    #[test]
    fn a_missing_workflows_directory_is_its_own_error() {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = TestRepo::init(&root.path().join("repo"));
        repo.remove_workflows_dir();
        assert_eq!(
            index_directory(repo.path()).expect_err("must fail"),
            IndexError::Unresolvable
        );
    }

    #[test]
    fn resolve_covers_all_four_forms() {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = TestRepo::init(&root.path().join("repo"));
        let files = indexed(repo.path());
        for requested in [".github/workflows/ci.yml", "ci.yml", "ci", "Fixture CI"] {
            let file = resolve(&files, requested)
                .unwrap_or_else(|error| panic!("`{requested}` must resolve: {error:?}"));
            assert_eq!(file.stem, "ci", "{requested}");
        }
    }

    #[test]
    fn resolve_reports_near_misses_with_the_options_that_exist() {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = TestRepo::init(&root.path().join("repo"));
        let files = indexed(repo.path());
        let error = resolve(&files, "nope").expect_err("must not resolve");
        match error {
            ResolveError::NotFound { available, reason } => {
                assert_eq!(available, vec!["ci"]);
                assert_eq!(reason, None);
            }
            other => panic!("unexpected: {other:?}"),
        }
        let error = resolve(&files, ".github/workflows/gone.yml").expect_err("must not resolve");
        assert!(
            matches!(
                &error,
                ResolveError::NotFound {
                    reason: Some(_),
                    ..
                }
            ),
            "{error:?}"
        );
    }

    #[test]
    fn two_files_one_bare_name_is_ambiguous() {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = TestRepo::init(&root.path().join("repo"));
        repo.write_workflow("bench.yml", "on: push\njobs:\n  a:\n    runs-on: x\n");
        repo.write_workflow("bench.yaml", "on: push\njobs:\n  b:\n    runs-on: x\n");
        let files = indexed(repo.path());
        let error = resolve(&files, "bench").expect_err("must be ambiguous");
        assert_eq!(
            error,
            ResolveError::Ambiguous {
                candidates: vec![
                    ".github/workflows/bench.yaml".to_owned(),
                    ".github/workflows/bench.yml".to_owned(),
                ]
            }
        );
        // …but the file name itself still resolves.
        assert_eq!(
            resolve(&files, "bench.yml").expect("resolves").relpath,
            ".github/workflows/bench.yml"
        );
    }
}
