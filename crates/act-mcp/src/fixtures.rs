//! Throwaway-repository fixtures, shared by this crate's unit tests and its
//! integration tests.
//!
//! [`TestRepo`] builds a real git repository in a temporary directory — real
//! `git`, real objects, real SHAs — because the cheapest way to test a validator
//! whose contract is "what `git rev-parse` answers" is to ask `git`. It is a
//! library module rather than a `tests/common` copy because the unit tests inside
//! `src/` need the same helper the e2e suite (mact-f1w.15) will: `tests/common`
//! re-exports it so both have one definition.
//!
//! Nothing here touches the network: the commands issued are `init`, `add`,
//! `commit`, `tag`, `config` and `rev-parse`, and the child environment is pinned
//! so a developer's global git config, GPG signing or hooks cannot change a
//! result.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

/// The workflow file every [`TestRepo`] starts with: one job, `build`.
pub const DEFAULT_WORKFLOW: &str = "ci.yml";

/// A git repository in a temporary directory, with one commit and one workflow.
#[derive(Debug, Clone)]
pub struct TestRepo {
    path: PathBuf,
}

impl TestRepo {
    /// Create `path`, `git init` it, and commit `.github/workflows/ci.yml`
    /// (one job: `build`) plus a `README.md`, so `HEAD` exists and the workspace
    /// gate has something to find.
    ///
    /// Panics on failure: a test without its fixture has nothing to assert on.
    pub fn init(path: &Path) -> Self {
        std::fs::create_dir_all(path).expect("create fixture directory");
        let repo = Self {
            path: canonical(path),
        };
        repo.git(&["init", "--quiet"]);
        repo.write_workflow(DEFAULT_WORKFLOW, DEFAULT_WORKFLOW_BODY);
        repo.write("README.md", "# fixture\n");
        repo.git(&["add", "-A"]);
        repo.git(&["commit", "--quiet", "-m", "initial commit"]);
        repo
    }

    /// A repository with a bare `.github/workflows` directory and nothing else:
    /// present, but with no workflow files in it.
    pub fn init_empty_workflows(path: &Path) -> Self {
        let repo = Self::init(path);
        repo.remove_all_workflows();
        repo.write("notes.txt", "not a workflow\n");
        repo.commit_all("drop the workflows");
        repo
    }

    /// The repository root (already canonicalized, so path comparisons against
    /// [`std::fs::canonicalize`] hold on every platform).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `.github/workflows` inside this repository.
    pub fn workflows_dir(&self) -> PathBuf {
        self.path.join(".github/workflows")
    }

    /// Full lowercase SHA of `HEAD`.
    pub fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"])
    }

    /// Write `body` to the workspace-relative path `rel`, creating parent
    /// directories, and return the absolute path.
    pub fn write(&self, rel: &str, body: &str) -> PathBuf {
        let target = self.path.join(rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).expect("create parent directories");
        }
        std::fs::write(&target, body).expect("write fixture file");
        target
    }

    /// Write `.github/workflows/<file_name>` and return its path.
    pub fn write_workflow(&self, file_name: &str, body: &str) -> PathBuf {
        self.write(&format!(".github/workflows/{file_name}"), body)
    }

    /// `git add -A` and commit everything, returning the new `HEAD`.
    pub fn commit_all(&self, message: &str) -> String {
        self.git(&["add", "-A"]);
        self.git(&["commit", "--quiet", "-m", message]);
        self.head()
    }

    /// Write a file, commit it, and return the new `HEAD` — the one call a test
    /// usually wants.
    pub fn add_and_commit(&self, rel: &str, body: &str, message: &str) -> String {
        self.write(rel, body);
        self.commit_all(message)
    }

    /// Write a workflow, commit it, return the new `HEAD`.
    pub fn add_workflow_and_commit(&self, file_name: &str, body: &str) -> String {
        self.write_workflow(file_name, body);
        self.commit_all(&format!("add {file_name}"))
    }

    /// Tag the current `HEAD` (annotated, so resolution has to peel it).
    pub fn tag(&self, name: &str) {
        self.git(&["tag", "-a", name, "-m", "fixture tag"]);
    }

    /// Set a repository-local config value.
    pub fn set_config(&self, key: &str, value: &str) {
        self.git(&["config", key, value]);
    }

    /// Stage a modification to a tracked file *without* committing it, so the
    /// working tree is dirty in the way ADR-0003 §4.2 G5 means.
    pub fn make_dirty(&self) {
        let path = self.path.join("README.md");
        let mut text = std::fs::read_to_string(&path).expect("read README");
        text.push_str("modified\n");
        std::fs::write(&path, text).expect("write README");
    }

    /// Create an untracked file, which ADR-0003 §4.2 deliberately does *not*
    /// count as dirt.
    pub fn add_untracked(&self, rel: &str) {
        self.write(rel, "untracked\n");
    }

    /// Delete every `*.yml`/`*.yaml` under `.github/workflows`, leaving the
    /// directory itself in place.
    pub fn remove_all_workflows(&self) {
        let dir = self.workflows_dir();
        if dir.is_dir() {
            for entry in std::fs::read_dir(&dir).expect("read workflows dir") {
                let entry = entry.expect("dir entry");
                let path = entry.path();
                let extension = path.extension().and_then(|e| e.to_str());
                if matches!(extension, Some("yml") | Some("yaml")) {
                    std::fs::remove_file(&path).expect("remove workflow file");
                }
            }
        }
    }

    /// Remove `.github/workflows` entirely.
    pub fn remove_workflows_dir(&self) {
        let dir = self.workflows_dir();
        if dir.is_dir() {
            std::fs::remove_dir_all(&dir).expect("remove workflows dir");
        }
    }

    /// `git status --porcelain`, trimmed — how a test proves validation changed
    /// nothing.
    pub fn porcelain(&self) -> String {
        self.git(&["status", "--porcelain"]).trim().to_owned()
    }

    /// Run `git -C <repo> <args>` and return trimmed stdout, panicking (with the
    /// stderr) on a non-zero exit.
    pub fn git(&self, args: &[&str]) -> String {
        match self.git_result(args) {
            Ok(stdout) => stdout,
            Err(stderr) => panic!("git {args:?} in {}: {stderr}", self.path.display()),
        }
    }

    /// [`Self::git`] without the panic: `Ok(trimmed stdout)` or `Err(stderr)`.
    pub fn git_result(&self, args: &[&str]) -> Result<String, String> {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(&self.path)
            // Nothing from the developer's environment may change a fixture: no
            // global/system config, no hooks path, no signing, no credential prompt.
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", &self.path)
            .env("LC_ALL", "C")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_AUTHOR_NAME", "act-mcp fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "act-mcp fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
            .args(["-c", "user.name=act-mcp fixture"])
            .args(["-c", "user.email=fixture@example.invalid"])
            .args(["-c", "commit.gpgsign=false"])
            .args(["-c", "init.defaultBranch=main"])
            .args(["-c", "core.hooksPath=/dev/null"])
            .args(args);
        let output = command.output().expect("run git");
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if output.status.success() {
            Ok(stdout)
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
        }
    }
}

/// Canonicalize, falling back to the path itself (mirrors what `Config` does at
/// boot, so tests compare like with like).
pub fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// A workflow body with the job ids named — the fixture every positive test uses.
pub const DEFAULT_WORKFLOW_BODY: &str = "\
name: Fixture CI
on:
  push:
    branches: [main]
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - run: echo building
  lint:
    runs-on: ubuntu-latest
    steps:
      - run: echo linting
";

/// The `list_tasks` acceptance fixture: three workflow files, one with an explicit
/// `name:` and a matrix, one plain whose step `uses:` a remote action, and one that
/// is not valid YAML at all.
///
/// These are the three files ADR-0001 §6.1's fixtures describe ("one with an explicit
/// `name:`, one with a matrix, one malformed"), and
/// `tests/e2e/testdata/tool-io/valid/list_tasks.result.full.json` is the expected
/// answer for them, so the Rust tests and the schema fixtures are pointed at the same
/// repository rather than at three hand-written stories about one.
pub const LIST_TASKS_FIXTURE: [(&str, &str); 3] = [
    ("benchmark.yml", LIST_TASKS_BENCHMARK),
    ("coverage.yml", LIST_TASKS_COVERAGE),
    ("broken.yml", LIST_TASKS_BROKEN),
];

/// `benchmark.yml`: declared `name:`, two jobs, one of them a matrix benchmark.
pub const LIST_TASKS_BENCHMARK: &str = "\
name: Benchmark
on:
  push:
    branches: [main]
jobs:
  build:
    runs-on: ubuntu-24.04
    steps:
      - run: cargo build --release
  hyperfine-bench:
    name: Run hyperfine benchmarks
    needs: build
    runs-on: ubuntu-24.04
    strategy:
      fail-fast: false
      matrix:
        profile: [dev, release]
    steps:
      - uses: actions/download-artifact@v5
      - name: Bench
        run: hyperfine \"./target/${{ matrix.profile }}/cli\" --warmup 3
";

/// `coverage.yml`: no declared name, one job whose step `uses:` a remote action.
pub const LIST_TASKS_COVERAGE: &str = "\
on:
  pull_request:
jobs:
  coverage:
    runs-on: ubuntu-24.04
    steps:
      - name: Run tests
        uses: nick-fields/retry@v3
        with:
          command: cargo llvm-cov --lcov --output-path lcov.info
";

/// `broken.yml`: valid file name, invalid YAML (`key: a: b`).
pub const LIST_TASKS_BROKEN: &str = "\
name: broken
on: push
jobs:
  broken:
    runs-on: ubuntu-latest
    steps:
      - name: broken step
        run: echo hi
    extra: this: is not yaml
";

impl TestRepo {
    /// Make this repository the [`LIST_TASKS_FIXTURE`]: the three workflow files and
    /// nothing else under `.github/workflows`.
    pub fn with_list_tasks_fixture(&self) -> &Self {
        self.remove_all_workflows();
        for (file, body) in LIST_TASKS_FIXTURE {
            self.write_workflow(file, body);
        }
        self
    }
}

/// A workflow whose trigger is the bare `on:` key — the YAML 1.1 boolean trap that
/// pins the parser choice (see [`crate::workflow_yaml`]).
pub const BARE_ON_WORKFLOW: &str = "\
on:
  push:
  workflow_dispatch:
    inputs:
      full:
        description: everything
        default: off
jobs:
  on:
    runs-on: ubuntu-latest
    steps:
      - run: echo 'a job id that reads as a YAML boolean'
";

/// A second workflow, for tests about which file a name resolves to.
pub fn workflow_body(job: &str) -> String {
    format!(
        "on:\n  push:\njobs:\n  {job}:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo {job}\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_repo_has_one_commit_and_a_workflow() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = TestRepo::init(&dir.path().join("repo"));
        assert_eq!(repo.git(&["rev-list", "--count", "HEAD"]), "1");
        assert_eq!(repo.head().len(), 40);
        assert!(repo.workflows_dir().join(DEFAULT_WORKFLOW).is_file());
        assert!(repo.porcelain().is_empty(), "fixture must be clean");
        assert!(repo.git_result(&["cat-file", "-t", &repo.head()]).unwrap() == "commit");
    }

    #[test]
    fn commits_move_head_and_adding_a_workflow_is_visible_to_git() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = TestRepo::init(&dir.path().join("repo"));
        let before = repo.head();
        let after = repo.add_workflow_and_commit("bench.yml", &workflow_body("bench"));
        assert_ne!(before, after);
        assert_eq!(after.len(), 40);
        assert!(repo
            .git(&["ls-tree", "--name-only", "-r", "HEAD", ".github/workflows"])
            .contains("bench.yml"));
    }

    #[test]
    fn an_annotated_tag_resolves_to_its_commit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = TestRepo::init(&dir.path().join("repo"));
        repo.tag("v1.0.0");
        let peeled = repo
            .git_result(&["rev-parse", "--verify", "v1.0.0^{commit}"])
            .expect("tag peels to a commit");
        assert_eq!(peeled, repo.head());
    }

    #[test]
    fn dirty_and_untracked_are_distinguishable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = TestRepo::init(&dir.path().join("repo"));
        repo.add_untracked("scratch.txt");
        assert!(
            repo.porcelain().contains("??"),
            "untracked should show as untracked"
        );
        repo.make_dirty();
        assert!(repo.porcelain().contains("M"));
        // …and `--untracked-files=no`, the gate's invocation, sees only the M.
        assert_eq!(
            repo.git(&["status", "--porcelain", "--untracked-files=no"])
                .trim(),
            "M README.md"
        );
    }
}
