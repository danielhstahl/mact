//! Unit tests for the validation gate.
//!
//! Every test builds its own throwaway git repository (real `git`, so the SHAs are
//! real) and installs a stub for the act/docker probes, so the whole suite runs on
//! a machine with neither installed — which is the acceptance criterion, and also
//! the reason the probes are a trait.

use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use super::*;
use crate::fixtures::{workflow_body, TestRepo};
use serde_json::Value;

// ------------------------------------------------------------------- test doubles

/// A [`Environment`] that runs real `git` unless told otherwise, and never touches
/// the real act or docker.
/// A canned answer for one `git` invocation.
type GitReply = Box<dyn Fn(&[&str]) -> Result<CommandOutput, CommandError> + Send + Sync>;

#[derive(Default)]
struct StubEnvironment {
    /// Canned `git` answers. `None` means "ask the real git in the real repo",
    /// which is what most tests want; the ones about classification (ambiguity,
    /// bare repositories, git dying) supply answers a repository cannot cheaply
    /// produce.
    git_reply: Option<GitReply>,
    act: Option<Result<PathBuf, ProgramError>>,
    act_version: Option<String>,
    runtime: Option<Result<ContainerRuntime, ContainerProbeError>>,
    /// `env_var` answers. Empty means "this variable is not set", which is its own answer.
    vars: Vec<(String, String)>,
    /// The files this environment can read, for `read_text_file` — act config, in practice.
    files: Vec<(PathBuf, String)>,
    git_calls: AtomicUsize,
    act_calls: AtomicUsize,
    docker_calls: AtomicUsize,
    log: Mutex<Vec<Vec<String>>>,
}

impl StubEnvironment {
    fn new() -> Self {
        Self {
            // A machine with act installed looks like this; the tests that care
            // about it being absent override it.
            act: Some(Ok(PathBuf::from("/usr/local/bin/act"))),
            runtime: Some(Ok(ContainerRuntime {
                program: "docker".to_owned(),
                version: Some("27.1.1".to_owned()),
            })),
            ..Self::default()
        }
    }

    fn answering_git<F>(mut self, replies: F) -> Self
    where
        F: Fn(&[&str]) -> Result<CommandOutput, CommandError> + Send + Sync + 'static,
    {
        self.git_reply = Some(Box::new(replies));
        self
    }

    fn without_act(mut self) -> Self {
        self.act = Some(Err(ProgramError::NotFound {
            requested: "act".to_owned(),
        }));
        self
    }

    fn act_unexecutable(mut self) -> Self {
        self.act = Some(Err(ProgramError::NotExecutable {
            requested: "/usr/local/bin/act".to_owned(),
        }));
        self
    }

    fn reporting_act_version(mut self, banner: &str) -> Self {
        self.act_version = Some(banner.to_owned());
        self
    }

    fn docker_timeout(mut self) -> Self {
        self.runtime = Some(Err(ContainerProbeError::Timeout(
            "the container runtime did not answer `docker info` within 3s".to_owned(),
        )));
        self
    }

    fn docker_error(mut self) -> Self {
        self.runtime = Some(Err(ContainerProbeError::Error(
            "Cannot connect to the Docker daemon at unix:///run/docker.sock.".to_owned(),
        )));
        self
    }

    fn docker_absent(mut self) -> Self {
        self.runtime = Some(Err(ContainerProbeError::NotFound(
            "no container runtime client: `docker` was not found".to_owned(),
        )));
        self
    }

    /// Pretend `name=value` is in the server's environment (and so in act's).
    fn with_env_var(mut self, name: &str, value: &str) -> Self {
        self.vars.push((name.to_owned(), value.to_owned()));
        self
    }

    /// Pretend `path` exists and contains `text`, which is how a test makes an actrc that
    /// the act child would also find.
    fn with_file(mut self, path: impl Into<PathBuf>, text: &str) -> Self {
        self.files.push((path.into(), text.to_owned()));
        self
    }

    /// Every `git` argument vector this stub was asked for, in order.
    fn git_log(&self) -> Vec<Vec<String>> {
        self.log.lock().expect("log lock").clone()
    }

    fn git_calls(&self) -> usize {
        self.git_calls.load(Ordering::Relaxed)
    }

    fn act_calls(&self) -> usize {
        self.act_calls.load(Ordering::Relaxed)
    }

    fn docker_calls(&self) -> usize {
        self.docker_calls.load(Ordering::Relaxed)
    }

    /// Whether any `git` invocation was one of the network subcommands that
    /// ADR-0003 §3.2 forbids forever.
    fn touched_the_network(&self) -> bool {
        self.git_log().iter().any(|arguments| {
            arguments.first().is_some_and(|verb| {
                matches!(
                    verb.as_str(),
                    "fetch" | "pull" | "clone" | "push" | "ls-remote"
                )
            })
        })
    }
}

impl Environment for StubEnvironment {
    fn git(
        &self,
        workspace: &Path,
        args: &[&str],
        timeout: Duration,
    ) -> Result<CommandOutput, CommandError> {
        self.git_calls.fetch_add(1, Ordering::Relaxed);
        self.log
            .lock()
            .expect("log lock")
            .push(args.iter().map(|arg| arg.to_string()).collect());
        match &self.git_reply {
            Some(reply) => reply(args),
            None => SystemEnvironment.git(workspace, args, timeout),
        }
    }

    fn resolve_program(&self, requested: &Path) -> Result<PathBuf, ProgramError> {
        self.act_calls.fetch_add(1, Ordering::Relaxed);
        if let Some(answer) = &self.act {
            return answer.clone();
        }
        SystemEnvironment.resolve_program(requested)
    }

    fn program_version(&self, _program: &Path) -> Option<String> {
        self.act_version.clone()
    }

    fn container_runtime(
        &self,
        socket: Option<&Path>,
        timeout: Duration,
    ) -> Result<ContainerRuntime, ContainerProbeError> {
        self.docker_calls.fetch_add(1, Ordering::Relaxed);
        self.runtime
            .clone()
            .unwrap_or_else(|| SystemEnvironment.container_runtime(socket, timeout))
    }

    fn env_var(&self, key: &str) -> Option<String> {
        self.vars
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
    }

    fn read_text_file(&self, path: &Path) -> Option<String> {
        self.files
            .iter()
            .find(|(found, _)| found == path)
            .map(|(_, text)| text.clone())
    }
}

/// A clock the test drives, so the 30 s probe TTL is a number and not a nap.
#[derive(Default)]
struct FakeClock {
    now: AtomicU64,
}

impl FakeClock {
    fn advance_millis(&self, millis: u64) {
        self.now.fetch_add(millis, Ordering::Relaxed);
    }
}

impl Clock for FakeClock {
    fn now_millis(&self) -> u64 {
        self.now.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------- test scaffolding

/// A repository plus the policy that points at it.
struct Fixture {
    dir: tempfile::TempDir,
    repo: TestRepo,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = TestRepo::init(&dir.path().join("repo"));
        Self { dir, repo }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn policy(&self) -> ValidationPolicy {
        ValidationPolicy {
            workspace_root: self.repo.path().to_path_buf(),
            evidence_dir: self.repo.path().join(".evidence"),
            act_binary: PathBuf::from("act"),
            minimum_act_version: MINIMUM_ACT_VERSION
                .parse()
                .expect("the pinned floor parses"),
            isolation: IsolationPolicy::Hybrid,
            allow_dirty_worktree: false,
            git_timeout: Duration::from_secs(10),
            container_daemon_socket: None,
            container_probe_timeout: Duration::from_millis(50),
            probe_ttl: DEFAULT_PROBE_TTL,
        }
    }
}

/// The three arguments that are always fine, with the commit left to the test.
fn request(workflow: &str, task: &str, commit: &str) -> RunRequest {
    RunRequest::new(workflow, task, "benchmark", commit)
}

fn head_request(fixture: &Fixture) -> RunRequest {
    request("ci", "build", &fixture.repo.head())
}

/// A validator over real `git` with stubbed act/docker probes.
fn validator_for(fixture: &Fixture, stub: &Arc<StubEnvironment>) -> Validator {
    Validator::new(fixture.policy(), Arc::clone(stub) as Arc<dyn Environment>)
}

fn expect_err(result: Result<ValidatedRunTarget, ValidationError>) -> ValidationError {
    result.expect_err("validation should have rejected this call")
}

// ------------------------------------------------------------------------- shapes

#[test]
fn a_shape_rejection_runs_no_subprocess_at_all() {
    let fixture = Fixture::new();
    let stub = Arc::new(StubEnvironment::new());
    let validator = validator_for(&fixture, &stub);

    let bad_commits = [
        "zzzzzzz",                                   // not hex
        "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefe", // 41 hex: over-long
        "abc123",                                    // 6 hex: too short
        "-abc1234",                                  // leading dash (argv injection)
        " abc1234 ",                                 // whitespace
        "..abc1234",                                 // traversal characters
        "~abc1234",                                  // home expansion
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "",
    ];
    for commit in bad_commits {
        let error = expect_err(validator.validate(&request("ci", "build", commit)));
        assert_eq!(error.code(), "invalid_arguments", "{commit:?} → {error}");
        let detail = error.detail();
        let errors = detail["errors"].as_array().expect("errors array");
        assert!(
            errors.iter().any(|entry| entry["path"] == "commit"),
            "{commit:?} should be blamed on `commit`: {errors:?}"
        );
        assert!(!error.retryable());
    }
    assert_eq!(
        stub.git_calls(),
        0,
        "shape rejection must not touch git: {:?}",
        stub.git_log()
    );
    assert_eq!(stub.act_calls(), 0, "shape rejection must not probe act");
}

#[test]
fn a_shape_error_lists_every_offending_argument() {
    let fixture = Fixture::new();
    let validator = validator_for(&fixture, &Arc::new(StubEnvironment::new()));
    let error =
        expect_err(validator.validate(&RunRequest::new("../../x", "a b..c", "Coverage", "nope")));
    let detail = error.detail();
    let paths: Vec<&str> = detail["errors"]
        .as_array()
        .expect("errors")
        .iter()
        .filter_map(|entry| entry["path"].as_str())
        .collect();
    assert_eq!(paths, vec!["commit", "workflow", "task", "category"]);
    assert!(error.message().contains("commit"));
}

#[test]
fn workflow_names_that_cannot_be_a_workflow_are_rejected_by_shape() {
    let fixture = Fixture::new();
    let stub = Arc::new(StubEnvironment::new());
    let validator = validator_for(&fixture, &stub);
    let commit = fixture.repo.head();
    for workflow in [
        "../outside.yml",
        ".github/workflows/../../etc/passwd",
        "/etc/passwd",
        "-C",
        "--help",
        "",
        "a\nb",
        "workflow;rm -rf /",
        "$(whoami)",
    ] {
        let error = expect_err(validator.validate(&request(workflow, "build", &commit)));
        assert_eq!(error.code(), "invalid_arguments", "{workflow:?} → {error}");
    }
    assert_eq!(stub.git_calls(), 0, "shape rules run before any subprocess");
}

#[test]
fn job_ids_and_categories_are_shape_checked_too() {
    let fixture = Fixture::new();
    let validator = validator_for(&fixture, &Arc::new(StubEnvironment::new()));
    let commit = fixture.repo.head();
    assert_eq!(
        expect_err(validator.validate(&request("ci", "../build", &commit))).code(),
        "invalid_arguments"
    );
    for category in ["Coverage", "1x", "", "with-dash", &"a".repeat(65)] {
        let error =
            expect_err(validator.validate(&RunRequest::new("ci", "build", category, &commit)));
        assert_eq!(error.code(), "invalid_arguments", "{category:?} → {error}");
    }
    // An unknown-but-legal category is *not* an error: ADR-0001 §2.4 pins `category`
    // open, so `fuzzing` is a run with a label, not a rejection.
    let fuzzing = RunRequest::new("ci", "build", "fuzzing", &commit);
    let target = validator
        .validate(&fuzzing)
        .expect("an unknown category still runs");
    assert_eq!(target.category, "fuzzing");
}

// --------------------------------------------------------------------------- commit

#[test]
fn a_head_commit_resolves_to_the_full_lowercase_sha() {
    let fixture = Fixture::new();
    let validator = validator_for(&fixture, &Arc::new(StubEnvironment::new()));
    let target = validator
        .validate(&head_request(&fixture))
        .expect("a clean repo with a workflow validates");
    assert_eq!(target.commit_full_sha, fixture.repo.head());
    assert_eq!(target.commit_full_sha.len(), 40);
    assert_eq!(target.mode, IsolationMode::HeadOnly);
    assert!(target.warnings.is_empty(), "{:?}", target.warnings);
}

#[test]
fn an_abbreviation_and_a_tag_object_both_resolve_to_the_full_id() {
    let fixture = Fixture::new();
    let validator = validator_for(&fixture, &Arc::new(StubEnvironment::new()));
    let head = fixture.repo.head();

    let short = &head[..8];
    let target = validator
        .validate(&request("ci", "build", short))
        .expect("an abbreviation resolves");
    assert_eq!(target.commit_full_sha, head, "R3 prints the full id");

    fixture.repo.tag("v1.0.0");
    let tag_object = fixture.repo.git(&["rev-parse", "v1.0.0"]);
    assert_ne!(tag_object, head, "an annotated tag is its own object");
    let target = validator
        .validate(&request("ci", "build", &tag_object))
        .expect("a tag object peels to its commit");
    assert_eq!(target.commit_full_sha, head);
}

#[test]
fn an_unknown_commit_is_commit_unknown_and_nothing_is_ever_fetched() {
    let fixture = Fixture::new();
    let stub = Arc::new(StubEnvironment::new());
    let validator = validator_for(&fixture, &stub);
    let error = expect_err(validator.validate(&request(
        "ci",
        "build",
        "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
    )));
    assert_eq!(error.code(), "commit_unknown");
    assert_eq!(error.detail()["reason"], Value::from("not_found"));
    assert_eq!(
        error.detail()["commit"],
        Value::from("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"),
        "detail carries what the caller typed"
    );
    assert!(
        !stub.touched_the_network(),
        "validation must never fetch: {:?}",
        stub.git_log()
    );
}

#[test]
fn an_object_that_is_not_a_commit_says_so() {
    let fixture = Fixture::new();
    let blob = fixture.repo.git(&["hash-object", "-w", "README.md"]);
    assert_eq!(blob.len(), 40);
    let validator = validator_for(&fixture, &Arc::new(StubEnvironment::new()));
    let error = expect_err(validator.validate(&request("ci", "build", &blob)));
    assert_eq!(error.code(), "commit_unknown");
    assert_eq!(
        error.detail()["reason"],
        Value::from("not_a_commit"),
        "a blob is not a commit, and the caller should be told that rather than 'not found'"
    );
}

#[test]
fn an_ambiguous_abbreviation_lists_candidates_and_never_fetches() {
    // Real ambiguity means two refs whose ids share a prefix, which cannot be
    // manufactured without brute-forcing hashes; the classification is what is
    // under test, so git is answered directly (ADR-0003 §3.1 R3→R4→R5).
    let fixture = Fixture::new();
    let prefix = "1a2b3c4";
    let stub_root = fixture.repo.path().display().to_string();
    let stub = Arc::new(StubEnvironment::new().answering_git(move |args| match args.first().copied() {
        Some("rev-parse") if args.contains(&"--show-toplevel") => {
            Ok(CommandOutput::stdout_text(stub_root.clone()))
        }
        Some("rev-parse") if args.contains(&"HEAD") => {
            Ok(CommandOutput::stdout_text("4".repeat(40)))
        }
        Some("rev-parse") => Ok(CommandOutput::failure(1, "")),
        Some("cat-file") if args.contains(&"-t") => Ok(CommandOutput::failure(1, "")),
        Some("for-each-ref") => Ok(CommandOutput::stdout_text(format!(
            "{prefix}aabbbbccccddddeeeeffff0000111122 refs/heads/a\n{prefix}99888877776666555544443333222211 refs/tags/b\n"
        ))),
        other => panic!("unexpected git call: {other:?}"),
    }));
    let validator = validator_for(&fixture, &stub);
    let error = expect_err(validator.validate(&request("ci", "build", prefix)));
    assert_eq!(error.code(), "commit_ambiguous");
    let candidates = error.detail()["candidates"]
        .as_array()
        .expect("candidates")
        .iter()
        .map(|value| value.as_str().expect("string").to_owned())
        .collect::<Vec<_>>();
    assert_eq!(candidates.len(), 2, "{candidates:?}");
    assert!(candidates[0].starts_with(prefix));
    assert!(error.message().contains("longer prefix"));
    assert!(!stub.touched_the_network());
}

#[test]
fn a_git_failure_is_retryable_and_names_the_step() {
    let fixture = Fixture::new();
    let root = fixture.repo.path().display().to_string();
    let stub = Arc::new(StubEnvironment::new().answering_git(move |args| {
        if args.contains(&"--show-toplevel") {
            return Ok(CommandOutput::stdout_text(root.clone()));
        }
        if args.contains(&"--end-of-options") {
            return Err(CommandError::Timeout {
                program: "git".to_owned(),
                timeout: Duration::from_secs(30),
            });
        }
        Ok(CommandOutput::stdout_text(""))
    }));
    let validator = validator_for(&fixture, &stub);
    let error = expect_err(validator.validate(&request("ci", "build", "abc1234")));
    assert_eq!(error.code(), "internal_error");
    assert!(error.retryable(), "a hung git is worth another try");
    assert!(error.message().contains("commit resolution"), "{error}");
}

// ----------------------------------------------------------------------- workspace

#[test]
fn a_missing_workspace_is_workspace_not_found() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut policy = ValidationPolicy {
        workspace_root: dir.path().join("nope"),
        ..fixture_policy()
    };
    policy.evidence_dir = dir.path().join("evidence");
    let validator = Validator::new(
        policy,
        Arc::new(StubEnvironment::new()) as Arc<dyn Environment>,
    );
    let error = expect_err(validator.validate(&request("ci", "build", "abc1234")));
    assert_eq!(error.code(), "workspace_not_found");
    assert_eq!(
        error.detail()["reason"],
        Value::from("the directory does not exist")
    );
}

#[test]
fn a_directory_that_is_not_a_git_repository_is_rejected() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join(".github/workflows")).expect("create workflows");
    std::fs::write(
        dir.path().join(".github/workflows/ci.yml"),
        workflow_body("build"),
    )
    .expect("write workflow");
    let policy = ValidationPolicy {
        workspace_root: crate::fixtures::canonical(dir.path()),
        evidence_dir: dir.path().join(".evidence"),
        ..fixture_policy()
    };
    let validator = Validator::new(
        policy,
        Arc::new(StubEnvironment::new()) as Arc<dyn Environment>,
    );
    let error = expect_err(validator.validate(&request("ci", "build", "abc1234")));
    assert_eq!(error.code(), "not_a_git_repository");
}

#[test]
fn a_subdirectory_of_a_repository_is_rejected_because_the_root_is_what_act_needs() {
    let fixture = Fixture::new();
    let package = fixture.repo.path().join("packages/tool");
    std::fs::create_dir_all(package.join(".github/workflows")).expect("create nested workflows");
    std::fs::write(
        package.join(".github/workflows/ci.yml"),
        workflow_body("build"),
    )
    .expect("write workflow");
    let policy = ValidationPolicy {
        workspace_root: crate::fixtures::canonical(&package),
        ..fixture_policy()
    };
    let validator = Validator::new(
        policy,
        Arc::new(StubEnvironment::new()) as Arc<dyn Environment>,
    );
    let error = expect_err(validator.validate(&request("ci", "build", "abc1234")));
    assert_eq!(error.code(), "not_a_git_repository");
    assert!(error.message().contains("repository root"), "{error}");
}

#[test]
fn a_workspace_path_that_is_a_symlink_outside_the_accepted_root_is_rejected() {
    let fixture = Fixture::new();
    let elsewhere = fixture.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).expect("create elsewhere");
    let link = fixture.path().join("link");
    std::os::unix::fs::symlink(&elsewhere, &link).expect("create symlink");

    // The policy was handed the symlink rather than the canonical root: resolving it
    // lands outside the accepted root, so it is not our repository.
    let policy = ValidationPolicy {
        workspace_root: link.clone(),
        evidence_dir: link.join(".evidence"),
        ..fixture_policy()
    };
    let validator = Validator::new(
        policy,
        Arc::new(StubEnvironment::new()) as Arc<dyn Environment>,
    );
    let error = expect_err(validator.validate(&request("ci", "build", "abc1234")));
    assert_eq!(error.code(), "workspace_not_found");
    assert!(
        error.message().contains("symlink"),
        "the reason should say why: {error}"
    );
}

#[test]
fn a_missing_or_empty_workflows_directory_is_rejected() {
    // Absent.
    let fixture = Fixture::new();
    fixture.repo.remove_workflows_dir();
    let validator = validator_for(&fixture, &Arc::new(StubEnvironment::new()));
    let error = expect_err(validator.validate(&head_request(&fixture)));
    assert_eq!(error.code(), "workflows_dir_missing");

    // Present but holding no workflow file.
    let fixture = Fixture::new();
    fixture.repo.remove_all_workflows();
    fixture.repo.write("notes.txt", "not a workflow\n");
    fixture.repo.commit_all("no workflows");
    let validator = validator_for(&fixture, &Arc::new(StubEnvironment::new()));
    let error = expect_err(validator.validate(&head_request(&fixture)));
    assert_eq!(error.code(), "workflows_dir_missing");
    assert_eq!(error.detail()["path"], Value::from(".github/workflows"));
}

#[test]
fn a_workflows_directory_symlinked_out_of_the_workspace_is_rejected() {
    let fixture = Fixture::new();
    let outside = fixture.path().join("outside-workflows");
    std::fs::create_dir_all(&outside).expect("create outside dir");
    std::fs::write(outside.join("ci.yml"), workflow_body("build")).expect("write outside workflow");

    let workflows = fixture.repo.workflows_dir();
    std::fs::remove_dir_all(&workflows).expect("remove workflows dir");
    std::os::unix::fs::symlink(&outside, &workflows)
        .expect("symlink .github/workflows out of the repo");

    let validator = validator_for(&fixture, &Arc::new(StubEnvironment::new()));
    let error = expect_err(validator.validate(&head_request(&fixture)));
    assert_eq!(error.code(), "workflows_dir_missing");
    assert!(error.message().contains("outside"), "{error}");
}

/// A policy for tests that build their own directory instead of a [`Fixture`].
fn fixture_policy() -> ValidationPolicy {
    let dir = tempfile::tempdir().expect("tempdir");
    ValidationPolicy {
        workspace_root: dir.path().to_path_buf(),
        evidence_dir: dir.path().join(".evidence"),
        act_binary: PathBuf::from("act"),
        minimum_act_version: MINIMUM_ACT_VERSION
            .parse()
            .expect("the pinned floor parses"),
        isolation: IsolationPolicy::Hybrid,
        allow_dirty_worktree: false,
        git_timeout: Duration::from_secs(10),
        container_daemon_socket: None,
        container_probe_timeout: Duration::from_millis(50),
        probe_ttl: DEFAULT_PROBE_TTL,
    }
}

// ------------------------------------------------------------------ workflow / task

#[test]
fn all_four_workflow_forms_resolve_to_the_same_file() {
    let fixture = Fixture::new();
    let validator = validator_for(&fixture, &Arc::new(StubEnvironment::new()));
    let commit = fixture.repo.head();
    let expected = fixture
        .repo
        .workflows_dir()
        .join("ci.yml")
        .canonicalize()
        .expect("canonical workflow path");

    for form in [
        ".github/workflows/ci.yml", // form 1: workspace-relative path
        "ci.yml",                   // form 2: file name with extension
        "ci",                       // form 3: basename without extension
        "Fixture CI",               // form 4: the `name:` key the file declares
    ] {
        let target = validator
            .validate(&request(form, "lint", &commit))
            .unwrap_or_else(|error| panic!("form {form:?} should resolve: {error}"));
        assert_eq!(target.workflow_file, expected, "form {form:?}");
        assert_eq!(target.workflow_display, "ci");
        assert_eq!(target.workflow_relpath, ".github/workflows/ci.yml");
        assert_eq!(target.job_id, "lint");
    }
}

#[test]
fn an_unknown_workflow_lists_the_options_that_do_exist() {
    let fixture = Fixture::new();
    fixture
        .repo
        .write_workflow("bench.yml", &workflow_body("bench"));
    let validator = validator_for(&fixture, &Arc::new(StubEnvironment::new()));
    let error = expect_err(validator.validate(&request("benuh", "build", &fixture.repo.head())));
    assert_eq!(error.code(), "workflow_not_found");
    let detail = error.detail();
    assert_eq!(detail["workflow"], Value::from("benuh"));
    assert_eq!(detail["workflows_dir"], Value::from(".github/workflows"));
    assert_eq!(
        detail["available"].as_array().expect("available").len(),
        2,
        "both workflows are offered: {detail:?}"
    );
    assert!(
        detail["available"]
            .as_array()
            .expect("available")
            .contains(&Value::from("bench")),
        "{detail:?}"
    );
}

#[test]
fn a_bare_name_matching_two_files_is_ambiguous_never_a_coin_flip() {
    let fixture = Fixture::new();
    fixture
        .repo
        .write_workflow("bench.yml", &workflow_body("bench"));
    fixture
        .repo
        .write_workflow("bench.yaml", &workflow_body("bench2"));
    fixture.repo.commit_all("two spellings of one name");
    let validator = validator_for(&fixture, &Arc::new(StubEnvironment::new()));

    let error = expect_err(validator.validate(&request("bench", "bench", &fixture.repo.head())));
    assert_eq!(error.code(), "ambiguous_workflow");
    let candidates = error.detail()["candidates"]
        .as_array()
        .expect("candidates")
        .iter()
        .map(|value| value.as_str().expect("string").to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        candidates,
        vec![
            ".github/workflows/bench.yaml",
            ".github/workflows/bench.yml"
        ]
    );

    // …and passing the file name resolves it, which is the remedy the error gives.
    let target = validator
        .validate(&request("bench.yaml", "bench2", &fixture.repo.head()))
        .expect("an explicit file name disambiguates");
    assert_eq!(target.workflow_relpath, ".github/workflows/bench.yaml");
}

#[test]
fn two_files_declaring_the_same_name_key_are_ambiguous_too() {
    let fixture = Fixture::new();
    fixture.repo.write_workflow(
        "one.yml",
        "name: Shared\njobs:\n  a:\n    runs-on: ubuntu-latest\n",
    );
    fixture.repo.write_workflow(
        "two.yml",
        "name: Shared\njobs:\n  b:\n    runs-on: ubuntu-latest\n",
    );
    fixture.repo.commit_all("duplicate name keys");
    let validator = validator_for(&fixture, &Arc::new(StubEnvironment::new()));
    let error = expect_err(validator.validate(&request("Shared", "a", &fixture.repo.head())));
    assert_eq!(error.code(), "ambiguous_workflow");
}

#[test]
fn a_workflow_file_that_is_a_symlink_out_of_the_workflows_dir_is_not_a_workflow() {
    let fixture = Fixture::new();
    let outside = fixture.path().join("outside.txt");
    std::fs::write(&outside, "passwords: yes\n").expect("write outside file");
    let link = fixture.repo.workflows_dir().join("evil.yml");
    std::os::unix::fs::symlink(&outside, &link).expect("symlink a workflow out of the tree");

    let validator = validator_for(&fixture, &Arc::new(StubEnvironment::new()));
    let commit = fixture.repo.head();
    let error = expect_err(validator.validate(&request("evil", "build", &commit)));
    assert_eq!(error.code(), "workflow_not_found");
    assert!(
        !error.detail()["available"]
            .as_array()
            .expect("available")
            .contains(&Value::from("evil")),
        "an escaping file must not be offered as an option: {:?}",
        error.detail()
    );
    // The escaping file is invisible everywhere, not merely unresolvable by name.
    let explicit =
        expect_err(validator.validate(&request(".github/workflows/evil.yml", "build", &commit)));
    assert_eq!(explicit.code(), "workflow_not_found");
    // …and the honest workflow still resolves.
    assert!(validator.validate(&request("ci", "build", &commit)).is_ok());
}

#[test]
fn an_unknown_task_lists_the_job_ids_that_exist() {
    let fixture = Fixture::new();
    let validator = validator_for(&fixture, &Arc::new(StubEnvironment::new()));
    let error = expect_err(validator.validate(&request("ci", "builds", &fixture.repo.head())));
    assert_eq!(error.code(), "task_not_found");
    let detail = error.detail();
    assert_eq!(detail["task"], Value::from("builds"));
    assert_eq!(
        detail["workflow_file"],
        Value::from(".github/workflows/ci.yml")
    );
    assert_eq!(
        detail["jobs"].as_array().expect("jobs"),
        &vec![Value::from("build"), Value::from("lint")],
        "jobs come back in file order so the caller can find their typo"
    );
}

#[test]
fn a_workflow_that_does_not_parse_cannot_confirm_a_task() {
    let fixture = Fixture::new();
    fixture
        .repo
        .write_workflow("broken.yml", "name: broken\njobs: [unclosed\n");
    fixture.repo.commit_all("broken workflow");
    let validator = validator_for(&fixture, &Arc::new(StubEnvironment::new()));
    let error = expect_err(validator.validate(&request("broken", "any", &fixture.repo.head())));
    assert_eq!(error.code(), "task_not_found");
    let reason = error.detail()["reason"]
        .as_str()
        .expect("reason")
        .to_owned();
    assert!(reason.contains("invalid_yaml"), "{reason}");
    assert!(reason.contains("could not be indexed"), "{reason}");
}

#[test]
fn a_non_head_commit_is_resolved_against_its_own_tree_not_the_working_tree() {
    // The seam ADR-0003 §4 leaves open, and the behaviour that makes it honest:
    // `bench.yml` exists only in the newer commit, so running the older commit must
    // not see it.
    let fixture = Fixture::new();
    let older = fixture.repo.head();
    fixture
        .repo
        .add_workflow_and_commit("bench.yml", &workflow_body("bench"));

    let validator = validator_for(&fixture, &Arc::new(StubEnvironment::new()));
    let target = validator
        .validate(&request("bench", "bench", &fixture.repo.head()))
        .expect("HEAD has the newer workflow");
    assert_eq!(target.mode, IsolationMode::HeadOnly);

    let error = expect_err(validator.validate(&request("bench", "bench", &older)));
    assert_eq!(
        error.code(),
        "workflow_not_found",
        "the older commit's tree has no bench workflow"
    );

    let target = validator
        .validate(&request("ci", "build", &older))
        .expect("the older commit still has ci");
    assert_eq!(target.mode, IsolationMode::Worktree);
    assert!(target.has_warning(WARNING_ISOLATION_WORKTREE));
    assert_eq!(target.workflow_relpath, ".github/workflows/ci.yml");
    assert_eq!(target.commit_full_sha, older);
    // The worktree does not exist yet, so the file is anchored at the workspace root
    // and must be re-anchored by whoever builds the tree.
    assert_eq!(
        target.workflow_in(Path::new("/scratch/run/worktree")),
        PathBuf::from("/scratch/run/worktree/.github/workflows/ci.yml")
    );
}

#[test]
fn a_non_head_commit_with_no_workflows_at_all_reports_the_missing_workflows_dir() {
    let fixture = Fixture::new();
    let older = fixture.repo.head();
    fixture.repo.remove_workflows_dir();
    fixture
        .repo
        .write("README.md", "# no workflows here anymore\n");
    fixture.repo.commit_all("drop workflows");
    // The workspace gate now has no workflows directory either, so the first thing
    // that fails is the workspace gate (step 4), not the tree lookup.
    let validator = validator_for(&fixture, &Arc::new(StubEnvironment::new()));
    let error = expect_err(validator.validate(&request("ci", "build", &older)));
    assert_eq!(error.code(), "workflows_dir_missing");
}

// -------------------------------------------------------------- environment probes

#[test]
fn a_missing_act_is_act_missing_and_is_worth_retrying() {
    let fixture = Fixture::new();
    let validator = validator_for(&fixture, &Arc::new(StubEnvironment::new().without_act()));
    let error = expect_err(validator.validate(&head_request(&fixture)));
    assert_eq!(error.code(), "act_missing");
    assert!(
        error.retryable(),
        "installing act makes the same call succeed"
    );
    let detail = error.detail();
    assert_eq!(detail["reason"], Value::from("not_found"));
    assert_eq!(
        detail["probed"].as_array().expect("probed"),
        &vec![Value::from("act")]
    );
}

#[test]
fn an_unexecutable_act_says_not_executable() {
    let fixture = Fixture::new();
    let validator = validator_for(
        &fixture,
        &Arc::new(StubEnvironment::new().act_unexecutable()),
    );
    let error = expect_err(validator.validate(&head_request(&fixture)));
    assert_eq!(error.code(), "act_missing");
    assert_eq!(error.detail()["reason"], Value::from("not_executable"));
}

#[test]
fn an_act_older_than_the_floor_is_act_missing_with_both_versions() {
    let fixture = Fixture::new();
    let stub = Arc::new(StubEnvironment::new().reporting_act_version("act version 0.2.50"));
    let validator = validator_for(&fixture, &stub);
    let error = expect_err(validator.validate(&head_request(&fixture)));
    assert_eq!(error.code(), "act_missing");
    let detail = error.detail();
    assert_eq!(detail["reason"], Value::from("too_old"));
    assert_eq!(detail["found_version"], Value::from("0.2.50"));
    assert_eq!(detail["minimum_version"], Value::from(MINIMUM_ACT_VERSION));

    // A new-enough act, and an act whose version nobody probed, both pass: not
    // knowing is never treated as being too old (ADR-0003 §7.7).
    let current = Arc::new(
        StubEnvironment::new().reporting_act_version(&format!("act version {MINIMUM_ACT_VERSION}")),
    );
    assert!(validator_for(&fixture, &current)
        .validate(&head_request(&fixture))
        .is_ok());
}

#[test]
fn an_unreachable_container_runtime_is_docker_unreachable() {
    for (stub, reason) in [
        (StubEnvironment::new().docker_timeout(), "timeout"),
        (StubEnvironment::new().docker_error(), "error"),
        (StubEnvironment::new().docker_absent(), "not_found"),
    ] {
        let fixture = Fixture::new();
        let validator = validator_for(&fixture, &Arc::new(stub));
        let error = expect_err(validator.validate(&head_request(&fixture)));
        assert_eq!(error.code(), "docker_unreachable", "{reason}");
        assert!(error.retryable());
        assert_eq!(error.detail()["reason"], Value::from(reason));
        assert!(
            error.detail()["detail"]
                .as_str()
                .is_some_and(|text| !text.is_empty()),
            "the probe's own words belong in detail: {:?}",
            error.detail()
        );
    }
}

#[test]
fn probes_run_before_the_isolation_gate_because_adr_0001_pins_that_order() {
    let fixture = Fixture::new();
    fixture.repo.make_dirty();
    let validator = validator_for(&fixture, &Arc::new(StubEnvironment::new().docker_timeout()));
    let error = expect_err(validator.validate(&head_request(&fixture)));
    assert_eq!(
        error.code(),
        "docker_unreachable",
        "step 7 (probes) precedes step 8 (isolation)"
    );
}

#[test]
fn the_environment_probe_is_cached_for_its_ttl_and_refreshed_after_it() {
    let fixture = Fixture::new();
    let stub = Arc::new(StubEnvironment::new());
    let clock = Arc::new(FakeClock::default());
    let validator = Validator::new(fixture.policy(), Arc::clone(&stub) as Arc<dyn Environment>)
        .with_clock(Arc::clone(&clock) as Arc<dyn Clock>);

    for _ in 0..5 {
        validator
            .validate(&head_request(&fixture))
            .expect("still valid");
    }
    assert_eq!(stub.docker_calls(), 1, "a burst must not re-probe docker");
    assert_eq!(stub.act_calls(), 1, "a burst must not re-probe PATH");

    clock.advance_millis(29_999);
    assert!(validator.validate(&head_request(&fixture)).is_ok());
    assert_eq!(stub.docker_calls(), 1, "still inside the probe TTL window");

    clock.advance_millis(2);
    assert!(validator.validate(&head_request(&fixture)).is_ok());
    assert_eq!(stub.docker_calls(), 2, "past the TTL the probe runs again");

    // …and an explicit invalidation (a failed spawn, say) does the same immediately.
    validator.invalidate_probes();
    assert!(validator.validate(&head_request(&fixture)).is_ok());
    assert_eq!(stub.docker_calls(), 3);
}

#[test]
fn a_failed_probe_is_cached_too_so_a_burst_does_not_re_dial_the_daemon() {
    let fixture = Fixture::new();
    let stub = Arc::new(StubEnvironment::new().docker_timeout());
    let validator = validator_for(&fixture, &stub);
    for _ in 0..3 {
        assert_eq!(
            expect_err(validator.validate(&head_request(&fixture))).code(),
            "docker_unreachable"
        );
    }
    assert_eq!(
        stub.docker_calls(),
        1,
        "the failure is cached like the success"
    );
}

// ------------------------------------------------------------------ isolation gate

#[test]
fn a_dirty_work_tree_fails_the_head_gate_and_the_operator_flag_downgrades_it() {
    // ADR-0003 §12.2 T6.
    let fixture = Fixture::new();
    fixture.repo.make_dirty();
    let validator = validator_for(&fixture, &Arc::new(StubEnvironment::new()));
    let error = expect_err(validator.validate(&head_request(&fixture)));
    assert_eq!(error.code(), "isolation_failed");
    assert_eq!(error.detail()["reason"], Value::from("dirty_worktree"));
    assert_eq!(error.detail()["mode"], Value::from("head_only"));
    assert_eq!(
        error.detail()["head"],
        Value::from(fixture.repo.head()),
        "detail.head is always present"
    );
    assert_eq!(
        error.detail()["requested"],
        Value::from(fixture.repo.head())
    );

    let mut permissive = fixture.policy();
    permissive.allow_dirty_worktree = true;
    let validator = Validator::new(
        permissive,
        Arc::new(StubEnvironment::new()) as Arc<dyn Environment>,
    );
    let target = validator
        .validate(&head_request(&fixture))
        .expect("the operator said a dirty tree is fine");
    assert!(target.has_warning(WARNING_DIRTY_WORKTREE));
}

#[test]
fn untracked_files_are_not_dirt() {
    // ADR-0003 §4.2 G5: build output, node_modules and .evidence/ itself are
    // untracked, and a gate that rejected them would reject every real workspace.
    let fixture = Fixture::new();
    fixture
        .repo
        .add_untracked(".evidence/benchmark/deadbeef/dead.jsonl");
    fixture.repo.add_untracked("node_modules/package/index.js");
    let validator = validator_for(&fixture, &Arc::new(StubEnvironment::new()));
    let target = validator
        .validate(&head_request(&fixture))
        .expect("untracked files are not dirt");
    assert!(target.warnings.is_empty());
}

#[test]
fn a_sparse_checkout_is_refused_because_it_measures_only_part_of_the_commit() {
    let fixture = Fixture::new();
    fixture.repo.set_config("core.sparseCheckout", "true");
    let validator = validator_for(&fixture, &Arc::new(StubEnvironment::new()));
    let error = expect_err(validator.validate(&head_request(&fixture)));
    assert_eq!(error.code(), "isolation_failed");
    assert_eq!(error.detail()["reason"], Value::from("sparse_checkout"));
}

#[test]
fn submodules_in_the_target_tree_are_refused() {
    let fixture = Fixture::new();
    fixture.repo.add_and_commit(
        ".gitmodules",
        "[submodule \"vendor\"]\n\tpath = vendor\n",
        "add a submodule",
    );
    let validator = validator_for(&fixture, &Arc::new(StubEnvironment::new()));
    let error = expect_err(validator.validate(&head_request(&fixture)));
    assert_eq!(error.code(), "isolation_failed");
    assert_eq!(
        error.detail()["reason"],
        Value::from("submodules_unsupported")
    );
}

#[test]
fn a_bare_repository_cannot_run_in_place() {
    // `git init --bare` has no working tree at all, so the workspace gate (G1) is the
    // first thing to fail; the bare check (G3) is defence in depth and is exercised
    // by answering git directly.
    let fixture = Fixture::new();
    let root = fixture.repo.path().display().to_string();
    let head = fixture.repo.head();
    let (stub_root, stub_head) = (root.clone(), head.clone());
    let stub = Arc::new(StubEnvironment::new().answering_git(move |args| {
        match args.first().copied().unwrap_or_default() {
            "rev-parse" if args.contains(&"--show-toplevel") => {
                Ok(CommandOutput::stdout_text(stub_root.clone()))
            }
            "rev-parse" if args.last().is_some_and(|last| *last == "HEAD") => {
                Ok(CommandOutput::stdout_text(stub_head.clone()))
            }
            "rev-parse" if args.contains(&"--is-bare-repository") => {
                Ok(CommandOutput::stdout_text("true"))
            }
            "rev-parse" => Ok(CommandOutput::stdout_text(stub_head.clone())),
            "cat-file" => Ok(CommandOutput::failure(1, "missing object")),
            "status" => Ok(CommandOutput::stdout_text("")),
            other => panic!("unexpected git call: {other:?}"),
        }
    }));
    let mut policy = fixture.policy();
    policy.workspace_root = PathBuf::from(&root);
    let validator = Validator::new(policy, Arc::clone(&stub) as Arc<dyn Environment>);
    let error = expect_err(validator.validate(&request("ci", "build", &head)));
    assert_eq!(error.code(), "isolation_failed");
    assert_eq!(error.detail()["reason"], Value::from("bare_repository"));
}

#[test]
fn the_head_only_policy_refuses_a_non_head_commit_after_resolution() {
    let fixture = Fixture::new();
    let older = fixture.repo.head();
    fixture
        .repo
        .add_and_commit("README.md", "# newer\n", "move HEAD on");

    let mut policy = fixture.policy();
    policy.isolation = IsolationPolicy::HeadOnly;
    let validator = Validator::new(
        policy,
        Arc::new(StubEnvironment::new()) as Arc<dyn Environment>,
    );

    let error = expect_err(validator.validate(&request("ci", "build", &older)));
    assert_eq!(error.code(), "isolation_failed");
    assert_eq!(error.detail()["reason"], Value::from("policy_head_only"));
    assert!(
        error.retryable(),
        "ADR-0001 §5.3 pins isolation_failed retryable"
    );

    // Precedence: ADR-0001 §2.9 runs resolution (step 6) before the isolation gate
    // (step 8), so a caller who typo'd the workflow is told about the workflow even
    // though the policy would also have refused the commit.
    let error = expect_err(validator.validate(&request("nope", "build", &older)));
    assert_eq!(error.code(), "workflow_not_found");
}

#[test]
fn the_worktree_policy_synthesizes_a_tree_even_for_head() {
    let fixture = Fixture::new();
    let mut policy = fixture.policy();
    policy.isolation = IsolationPolicy::Worktree;
    let validator = Validator::new(
        policy,
        Arc::new(StubEnvironment::new()) as Arc<dyn Environment>,
    );
    let target = validator
        .validate(&head_request(&fixture))
        .expect("worktree mode runs HEAD too");
    assert_eq!(target.mode, IsolationMode::Worktree);
    assert!(target.has_warning(WARNING_ISOLATION_WORKTREE));
    // A dirty tree is irrelevant to a synthesized worktree (ADR-0003 §4.4).
    fixture.repo.make_dirty();
    assert!(validator.validate(&head_request(&fixture)).is_ok());
}

#[test]
fn an_unresolvable_head_is_isolation_failed_not_a_crash() {
    // An empty repository has workflows on disk but no HEAD to compare with.
    let fixture = Fixture::new();
    let root = fixture.repo.path().display().to_string();
    let stub_root = root.clone();
    let stub = Arc::new(StubEnvironment::new().answering_git(move |args| {
        let verb = args.first().copied().unwrap_or_default();
        match verb {
            "rev-parse" if args.contains(&"--show-toplevel") => {
                Ok(CommandOutput::stdout_text(stub_root.clone()))
            }
            "rev-parse" if args.last().is_some_and(|last| *last == "HEAD") => {
                Ok(CommandOutput::failure(1, ""))
            }
            "rev-parse" => Ok(CommandOutput::stdout_text("a".repeat(40))),
            "cat-file" => Ok(CommandOutput::failure(1, "missing")),
            "status" => Ok(CommandOutput::stdout_text("")),
            "ls-tree" => Ok(CommandOutput::stdout_text(".github/workflows/ci.yml\0")),
            "show" => Ok(CommandOutput::stdout_text(workflow_body("build"))),
            other => panic!("unexpected git call: {other:?}"),
        }
    }));
    let mut policy = fixture.policy();
    policy.workspace_root = PathBuf::from(&root);
    // Under `hybrid` an unresolvable HEAD is not fatal: the run is simply not
    // provable as "in place", so it goes to a worktree. Under `head-only` there is
    // no alternative tree, and G2 is the failure the ADR pins.
    policy.isolation = IsolationPolicy::HeadOnly;
    let validator = Validator::new(policy.clone(), Arc::clone(&stub) as Arc<dyn Environment>);
    let error = expect_err(validator.validate(&request("ci", "build", &"a".repeat(40))));
    assert_eq!(error.code(), "isolation_failed");
    assert_eq!(error.detail()["reason"], Value::from("head_unresolvable"));
    assert_eq!(error.detail()["head"], Value::from(""));

    policy.isolation = IsolationPolicy::Hybrid;
    let hybrid = Validator::new(policy, stub as Arc<dyn Environment>);
    assert!(
        hybrid
            .validate(&request("ci", "build", &"a".repeat(40)))
            .is_ok(),
        "a worktree does not need HEAD to resolve"
    );
}

// ------------------------------------------------------------------- what it leaves

#[test]
fn validating_a_workspace_leaves_it_untouched() {
    let fixture = Fixture::new();
    let validator = validator_for(&fixture, &Arc::new(StubEnvironment::new()));
    validator
        .validate(&head_request(&fixture))
        .expect("valid run");
    assert_eq!(
        fixture.repo.porcelain(),
        "",
        "validation must not stage or edit"
    );

    // `--no-optional-locks` claim (ADR-0003 D-10): no index.lock, no packed-objects
    // churn, nothing new under .git.
    let mut locks = Vec::new();
    let mut stack = vec![fixture.repo.path().join(".git")];
    while let Some(directory) = stack.pop() {
        for entry in std::fs::read_dir(&directory).expect("read .git") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with(".lock"))
            {
                locks.push(path);
            }
        }
    }
    assert!(
        locks.is_empty(),
        "validation left lock files behind: {locks:?}"
    );
}

#[test]
fn no_error_payload_contains_an_absolute_host_path() {
    let fixture = Fixture::new();
    let host_path = fixture.repo.path().display().to_string();
    let validator = validator_for(&fixture, &Arc::new(StubEnvironment::new()));
    let head = fixture.repo.head();

    let mut cases = vec![
        expect_err(validator.validate(&request("nope", "build", &head))),
        expect_err(validator.validate(&request("ci", "nope", &head))),
        expect_err(validator.validate(&request("ci", "build", &"f".repeat(40)))),
        expect_err(validator.validate(&request("../x", "build", &head))),
    ];

    // A workspace we cannot use, built separately so its own path is the one checked.
    let broken = tempfile::tempdir().expect("tempdir");
    let broken_policy = ValidationPolicy {
        workspace_root: crate::fixtures::canonical(broken.path()),
        evidence_dir: broken.path().join(".evidence"),
        ..fixture_policy()
    };
    let broken_validator = Validator::new(
        broken_policy,
        Arc::new(StubEnvironment::new()) as Arc<dyn Environment>,
    );
    cases.push(expect_err(
        broken_validator.validate(&request("ci", "build", &head)),
    ));
    cases.push(expect_err(
        broken_validator.validate(&RunRequest::new("ci", "build", "bad", &head)),
    ));

    for error in cases {
        let payload = serde_json::to_string(&error.to_error_value()).expect("serializable");
        assert!(
            !payload.contains(&host_path),
            "{} leaked `{host_path}`: {payload}",
            error.code()
        );
        assert!(
            !payload.contains("/tmp/") && !payload.contains("/private/"),
            "{} leaked a host path: {payload}",
            error.code()
        );
        assert!(
            !payload.contains("fixture"),
            "the repository directory name is a host path component too: {payload}"
        );
    }
}

// --------------------------------------------------------- contract-level assertions

/// One example of every variant this module can produce, for the contract checks.
fn every_error() -> Vec<ValidationError> {
    vec![
        ValidationError::InvalidArguments {
            summary: "commit: not hex".to_owned(),
            errors: vec![ArgumentError::new(
                "commit",
                "not hex",
                "7 to 40 hex characters",
            )],
        },
        ValidationError::WorkspaceNotFound {
            path: "repo".to_owned(),
            reason: "the directory does not exist".to_owned(),
        },
        ValidationError::NotAGitRepository {
            path: "repo".to_owned(),
            reason: "rev-parse failed".to_owned(),
        },
        ValidationError::WorkflowsDirMissing {
            path: ".github/workflows".to_owned(),
            reason: "no workflow files".to_owned(),
        },
        ValidationError::ActMissing {
            probed: vec!["act".to_owned()],
            reason: ActMissingReason::NotFound,
            found_version: None,
            minimum_version: Some(MINIMUM_ACT_VERSION.to_owned()),
        },
        ValidationError::ActMissing {
            probed: vec!["act".to_owned()],
            reason: ActMissingReason::TooOld,
            found_version: Some("0.1.0".to_owned()),
            minimum_version: Some(MINIMUM_ACT_VERSION.to_owned()),
        },
        ValidationError::DockerUnreachable {
            reason: "timeout".to_owned(),
            detail: "did not answer".to_owned(),
        },
        ValidationError::WorkflowNotFound {
            workflow: "nope".to_owned(),
            workflows_dir: WORKFLOWS_DIR,
            available: vec!["ci".to_owned()],
            reason: None,
        },
        ValidationError::AmbiguousWorkflow {
            workflow: "bench".to_owned(),
            candidates: vec![".github/workflows/bench.yml".to_owned()],
        },
        ValidationError::TaskNotFound {
            workflow_file: ".github/workflows/ci.yml".to_owned(),
            task: "nope".to_owned(),
            jobs: vec!["build".to_owned()],
            reason: None,
        },
        ValidationError::CommitUnknown {
            commit: "abc1234".to_owned(),
            reason: CommitUnknownReason::NotFound,
        },
        ValidationError::CommitAmbiguous {
            commit: "abc1234".to_owned(),
            candidates: vec!["a".repeat(40), "b".repeat(40)],
        },
        ValidationError::IsolationFailed {
            mode: IsolationMode::HeadOnly,
            requested: "a".repeat(40),
            head: "b".repeat(40),
            reason: "dirty_worktree",
        },
        ValidationError::Internal {
            reason: "git timed out".to_owned(),
        },
    ]
}

#[test]
fn error_codes_and_retryable_flags_match_the_pinned_contract() {
    let schema_text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/schema/tool-contract.schema.json"
    ))
    .expect("the tool contract schema is part of the repository");
    let schema: Value = serde_json::from_str(&schema_text).expect("schema parses");
    let codes = schema["$defs"]["error_code"]["enum"]
        .as_array()
        .expect("error_code enum");
    let registry = schema["x-error-codes"]
        .as_object()
        .expect("x-error-codes registry");

    let mut exercised = Vec::new();
    for error in every_error() {
        let code = error.code();
        exercised.push(code.to_owned());
        assert!(
            codes.iter().any(|pinned| pinned == code),
            "`{code}` is not a member of $defs/error_code"
        );
        let entry = &registry[code];
        assert_eq!(
            entry["retryable"],
            Value::from(error.retryable()),
            "`{code}` disagrees with x-error-codes on retryable"
        );
        assert_eq!(
            entry["is_error"],
            Value::from(true),
            "every code this module can produce is a tool-execution error"
        );
    }

    // The codes this module owns (ADR-0001 "Binding on the other decisions") must all
    // be exercised, so renaming one is a test failure rather than dead code.
    for owned in [
        "invalid_arguments",
        "workspace_not_found",
        "not_a_git_repository",
        "workflows_dir_missing",
        "act_missing",
        "docker_unreachable",
        "workflow_not_found",
        "ambiguous_workflow",
        "task_not_found",
        "commit_unknown",
        "commit_ambiguous",
        "isolation_failed",
    ] {
        assert!(
            exercised.iter().any(|code| code == owned),
            "no test constructs the `{owned}` variant"
        );
    }
}

#[test]
fn an_error_serializes_to_the_pinned_error_shape() {
    let error = ValidationError::WorkflowNotFound {
        workflow: "bench".to_owned(),
        workflows_dir: WORKFLOWS_DIR,
        available: vec!["ci".to_owned()],
        reason: None,
    };
    let payload = error.to_error_value();
    assert_eq!(payload["code"], Value::from("workflow_not_found"));
    assert_eq!(payload["retryable"], Value::from(false));
    assert_eq!(payload["message"], Value::from(error.message()));
    // Exactly the four keys `$defs/error` allows, and the per-code keys it documents.
    assert_eq!(payload.as_object().expect("object").len(), 4);
    let detail = payload["detail"].as_object().expect("detail");
    assert_eq!(detail.len(), 3);
    assert!(detail.contains_key("workflow"));
    assert!(detail.contains_key("workflows_dir"));
    assert!(detail.contains_key("available"));
}

#[test]
fn option_lists_are_sorted_deduped_and_bounded_and_detail_stays_under_its_budget() {
    let flooded = ValidationError::WorkflowNotFound {
        workflow: "none".to_owned(),
        workflows_dir: WORKFLOWS_DIR,
        available: (0..400)
            .map(|index| format!("workflow-{index:03}"))
            .collect(),
        reason: None,
    };
    let detail = flooded.detail();
    let available = detail["available"].as_array().expect("available");
    assert!(
        available.len() <= MAX_LISTED_OPTIONS,
        "{} options is more than the budget",
        available.len()
    );
    let compact = serde_json::to_string(&detail).expect("compact json");
    assert!(
        compact.len() <= MAX_ERROR_DETAIL_BYTES,
        "detail is {} bytes, over budget",
        compact.len()
    );

    let long_message = ValidationError::Internal {
        reason: "x".repeat(4000),
    };
    assert!(long_message.message().len() <= MAX_ERROR_MESSAGE_BYTES);
}

// ---------------------------------------------------------------- the tree seam

#[test]
fn the_default_tree_selector_is_adr_0003_s_mode_table() {
    let selector = DefaultTreeSelector;
    let head = "a".repeat(40);
    let other = "b".repeat(40);
    let cases = [
        (
            IsolationPolicy::HeadOnly,
            Some(head.as_str()),
            head.as_str(),
            IsolationMode::HeadOnly,
            true,
        ),
        (
            IsolationPolicy::HeadOnly,
            Some(head.as_str()),
            other.as_str(),
            IsolationMode::HeadOnly,
            true,
        ),
        (
            IsolationPolicy::HeadOnly,
            None,
            other.as_str(),
            IsolationMode::HeadOnly,
            true,
        ),
        (
            IsolationPolicy::Hybrid,
            Some(head.as_str()),
            head.as_str(),
            IsolationMode::HeadOnly,
            true,
        ),
        (
            IsolationPolicy::Hybrid,
            Some(head.as_str()),
            other.as_str(),
            IsolationMode::Worktree,
            false,
        ),
        // No HEAD to compare with is not "the same commit": hybrid cannot prove it.
        (
            IsolationPolicy::Hybrid,
            None,
            head.as_str(),
            IsolationMode::Worktree,
            false,
        ),
        (
            IsolationPolicy::Worktree,
            Some(head.as_str()),
            head.as_str(),
            IsolationMode::Worktree,
            false,
        ),
    ];
    for (policy, head_value, commit, expected_mode, workspace_is_root) in cases {
        let plan = selector.select(policy, head_value, commit);
        assert_eq!(plan.mode, expected_mode, "{policy} / {commit}");
        assert_eq!(
            matches!(plan.root, TreeRoot::Workspace),
            workspace_is_root,
            "{policy} / {commit}: {:?}",
            plan.root
        );
    }
}

/// The shape of the seam the wiring ticket fills in: once a real tree exists,
/// validation reads *that* directory instead of guessing from the commit's tree.
#[test]
fn a_materialized_tree_is_read_instead_of_the_commit_tree() {
    let fixture = Fixture::new();
    let older = fixture.repo.head();
    fixture
        .repo
        .add_workflow_and_commit("newer.yml", &workflow_body("newer"));

    // A real detached worktree at the older commit, exactly as ADR-0003 §5 creates it.
    let worktree = fixture.path().join("scratch/run/worktree");
    fixture.repo.git(&[
        "worktree",
        "add",
        "--detach",
        &worktree.display().to_string(),
        &older,
    ]);
    assert!(worktree.join(".github/workflows/ci.yml").is_file());
    assert!(
        !worktree.join(".github/workflows/newer.yml").exists(),
        "the worktree is the older tree"
    );
    // The validator reports the *resolved* path of the file it read — resolving it is how it
    // proves the file is in the tree it measured (`workflows::index_directory`, ADR-0001
    // §2.2) — so the comparison has to be resolved against resolved. A temp directory can be
    // spelled through a symlink (macOS' `$TMPDIR` lives under `/var`, a link into
    // `/private`), and then the two spellings are different strings for one directory.
    let worktree_canonical = worktree.canonicalize().expect("the worktree exists");

    struct Materialized(PathBuf);
    impl TreeSelector for Materialized {
        fn select(&self, _policy: IsolationPolicy, _head: Option<&str>, _commit: &str) -> TreePlan {
            TreePlan {
                mode: IsolationMode::Worktree,
                root: TreeRoot::Materialized {
                    root: self.0.clone(),
                },
            }
        }
    }

    let validator = validator_for(&fixture, &Arc::new(StubEnvironment::new()))
        .with_tree_selector(Arc::new(Materialized(worktree.clone())));

    let target = validator
        .validate(&request("ci", "build", &older))
        .expect("the worktree's own workflow file resolves");
    assert_eq!(
        target.workflow_file,
        worktree_canonical.join(".github/workflows/ci.yml"),
        "the file that was inspected is the file in the tree being measured"
    );
    // Relpaths stay workspace-relative, so the run reports the same path it would
    // have reported in `head_only` mode — and they stay that way however the tree was
    // spelled, because `Materialized` above was handed the path as written, not the
    // resolved one: an absolute relpath here would both read wrong in the payload and
    // leak a host path ADR-0001 §2.2 keeps out of it.
    assert_eq!(target.workflow_relpath, ".github/workflows/ci.yml");
    assert_eq!(target.mode, IsolationMode::Worktree);

    let error = expect_err(validator.validate(&request("newer", "newer", &older)));
    assert_eq!(
        error.code(),
        "workflow_not_found",
        "a newer-commit workflow is not in this tree, and saying so is the point"
    );
}

// ------------------------------------------------------- the runner-label pre-flight

/// A workflow asking for a runner label act's own defaults never had.
fn label_workflow(job: &str, runs_on: &str, matrix: &str) -> String {
    format!(
        "on:\n  push:\njobs:\n  {job}:\n    runs-on: {runs_on}\n{matrix}    steps:\n      - run: echo {job}\n"
    )
}

#[test]
fn a_runner_label_with_no_image_warns_and_a_mapped_one_does_not() {
    let fixture = Fixture::new();
    fixture
        .repo
        .add_workflow_and_commit("ci24.yml", &label_workflow("check", "ubuntu-24.04", ""));
    let head = fixture.repo.head();

    // A server with no act config anywhere: act would be about to pull an image named `""`,
    // and the run would die in `Set up job` saying `invalid reference format`.
    let bare = fixture.policy();
    let validator = Validator::new(
        bare.clone(),
        Arc::new(StubEnvironment::new().with_env_var("HOME", "/home/build"))
            as Arc<dyn Environment>,
    );
    let target = validator
        .validate(&request("ci24", "check", &head))
        .expect("the workflow and commit resolve");
    assert!(
        target.has_warning(WARNING_RUNS_ON_UNMAPPED),
        "an unmapped label must be said out loud: {:?}",
        target.warnings
    );

    // The same workflow, with the mapping the operator was supposed to have to add.
    let configured = Validator::new(
        fixture.policy(),
        Arc::new(
            StubEnvironment::new()
                .with_env_var("HOME", "/home/build")
                .with_file(
                    "/home/build/.config/act/actrc",
                    "-P ubuntu-24.04=catthehacker/ubuntu:act-24.04\n",
                ),
        ) as Arc<dyn Environment>,
    );
    let target = configured
        .validate(&request("ci24", "check", &head))
        .expect("the same call, configured");
    assert!(
        !target.has_warning(WARNING_RUNS_ON_UNMAPPED),
        "act would find an image, so there is nothing to warn about: {:?}",
        target.warnings
    );

    // And the baseline every other fixture workflow uses, unchanged.
    let baseline = validator_for(&fixture, &Arc::new(StubEnvironment::new()))
        .validate(&head_request(&fixture))
        .expect("the default fixture workflow resolves");
    assert!(
        !baseline.has_warning(WARNING_RUNS_ON_UNMAPPED),
        "`ubuntu-latest` is in act's own table: {:?}",
        baseline.warnings
    );
}

#[test]
fn a_matrix_runner_label_is_checked_against_the_values_the_file_declares() {
    let fixture = Fixture::new();
    fixture.repo.add_workflow_and_commit(
        "matrix.yml",
        &label_workflow(
            "check",
            "${{ matrix.runner }}",
            "    strategy:\n      matrix:\n        runner: [ubuntu-latest, macos-14]\n",
        ),
    );
    let head = fixture.repo.head();

    let stub = StubEnvironment::new().with_env_var("HOME", "/home/build");
    let validator = Validator::new(
        fixture.policy(),
        Arc::new(stub.with_file(
            // One line of actrc silences one axis value: the check is per would-be job,
            // and `macos-14` is the only one still without an image.
            "/home/build/.config/act/actrc",
            "-P macos-14=ghcr.io/example/macos-14:1\n",
        )) as Arc<dyn Environment>,
    );
    let target = validator
        .validate(&request("matrix", "check", &head))
        .expect("the matrix workflow resolves");
    assert!(
        !target.has_warning(WARNING_RUNS_ON_UNMAPPED),
        "{:?}",
        target.warnings
    );

    let validator = Validator::new(
        fixture.policy(),
        Arc::new(StubEnvironment::new().with_env_var("HOME", "/home/build"))
            as Arc<dyn Environment>,
    );
    let target = validator
        .validate(&request("matrix", "check", &head))
        .expect("the same call, unconfigured");
    assert!(target.has_warning(WARNING_RUNS_ON_UNMAPPED));
}

#[test]
fn a_job_naming_its_own_image_or_the_host_is_left_alone() {
    let fixture = Fixture::new();
    fixture.repo.add_workflow_and_commit(
        "quiet.yml",
        &label_workflow(
            "boxed",
            "ubuntu-24.04",
            "    container: {image: example/toolchain:1}\n",
        ),
    );
    fixture.repo.add_workflow_and_commit(
        "host.yml",
        &label_workflow("bare", "[self-hosted, gpu]", ""),
    );
    let head = fixture.repo.head();

    let stub = Arc::new(StubEnvironment::new().with_env_var("HOME", "/home/build"));
    let validator = validator_for(&fixture, &stub);
    for (workflow, task) in [("quiet", "boxed"), ("host", "bare")] {
        let target = validator
            .validate(&request(workflow, task, &head))
            .unwrap_or_else(|error| panic!("{workflow} should resolve: {error}"));
        assert!(
            !target.has_warning(WARNING_RUNS_ON_UNMAPPED),
            "{workflow} does not need a runner image: {:?}",
            target.warnings
        );
    }
}
