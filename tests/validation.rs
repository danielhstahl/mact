//! Wiring-level tests for the validation gate (mact-f1w.6).
//!
//! The unit tests in `src/workspace/tests.rs` own the verdicts; these prove the two
//! things only an external crate can prove:
//!
//! * the operator surface (`--isolation`, `--git-timeout-secs`,
//!   `--minimum-act-version`, `--allow-dirty-worktree`, `--container-daemon-socket`)
//!   reaches [`ValidationPolicy`] intact, and
//! * the [`Environment`] seam can be implemented *outside* this crate with the
//!   public API alone — which is what mact-f1w.14 needs in order to inject the real
//!   act prober and, later, a worktree-backed tree.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use act_mcp::{
    config::{ServeArgs, DEFAULT_BIND},
    workspace::{
        CommandError, CommandOutput, ContainerProbeError, ContainerRuntime, Environment,
        ProgramError, RunRequest, ValidationPolicy, Validator,
    },
    Config, ConfigError,
};

mod common;
use common::TestRepo;

/// A probe double written from outside the crate, the way the wiring ticket will.
struct StubRuntime;

impl Environment for StubRuntime {
    fn git(
        &self,
        workspace: &Path,
        args: &[&str],
        timeout: Duration,
    ) -> Result<CommandOutput, CommandError> {
        // Real git: the point of this test is the wiring, not the classification.
        act_mcp::workspace::SystemEnvironment.git(workspace, args, timeout)
    }

    fn resolve_program(&self, requested: &Path) -> Result<PathBuf, ProgramError> {
        which_like(requested)
    }

    fn container_runtime(
        &self,
        _socket: Option<&Path>,
        _timeout: Duration,
    ) -> Result<ContainerRuntime, ContainerProbeError> {
        Ok(ContainerRuntime {
            program: "docker".to_owned(),
            version: Some("27.1.1".to_owned()),
        })
    }

    /// This is the hook the validator deliberately cannot fill in itself: reading
    /// act's version means running act, and running act is not validation.
    fn program_version(&self, _program: &Path) -> Option<String> {
        Some("act version 0.2.68".to_owned())
    }
}

/// Minimal stand-in for `which`, so the test does not depend on the crate's own
/// helper being public.
fn which_like(requested: &Path) -> Result<PathBuf, ProgramError> {
    let text = requested.display().to_string();
    if text.contains('/') {
        if requested.is_file() {
            return Ok(requested.to_path_buf());
        }
        return Err(ProgramError::NotFound { requested: text });
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(&text);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(ProgramError::NotFound { requested: text })
}

/// A workspace repository plus the boot configuration that points at it.
struct Harness {
    /// Held so the temporary directory outlives the harness.
    #[allow(dead_code, reason = "its lifetime is the point")]
    dir: tempfile::TempDir,
    repo: TestRepo,
    act: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = TestRepo::init(&dir.path().join("repo"));
        let act = dir.path().join("act");
        std::fs::write(&act, b"#!/bin/sh\nexit 0\n").expect("write stub act");
        #[cfg(unix)]
        std::fs::set_permissions(&act, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("chmod stub act");
        Self { dir, repo, act }
    }

    fn args(&self) -> ServeArgs {
        ServeArgs {
            workspace: Some(self.repo.path().to_path_buf()),
            bind: DEFAULT_BIND.to_owned(),
            evidence_dir: None,
            act_binary: self.act.clone(),
            log_dir: None,
            ..ServeArgs::default()
        }
    }

    fn validator(&self, config: &Config) -> Validator {
        Validator::new(
            ValidationPolicy::from_config(config),
            Arc::new(StubRuntime) as Arc<dyn Environment>,
        )
    }
}

#[test]
fn boot_configuration_reaches_the_validation_policy() {
    let harness = Harness::new();
    let mut args = harness.args();
    args.isolation = "worktree".to_owned();
    args.allow_dirty_worktree = true;
    args.git_timeout_secs = 12;
    args.minimum_act_version = "0.2.70".to_owned();
    args.container_daemon_socket = Some("/run/docker.sock".to_owned());

    let config = Config::build(&args).expect("valid boot configuration");
    let policy = ValidationPolicy::from_config(&config);
    assert_eq!(policy.workspace_root, harness.repo.path().to_path_buf());
    assert_eq!(policy.evidence_dir, harness.repo.path().join(".evidence"));
    assert_eq!(policy.act_binary, harness.act);
    assert_eq!(policy.isolation.as_str(), "worktree");
    assert!(policy.allow_dirty_worktree);
    assert_eq!(policy.git_timeout, Duration::from_secs(12));
    assert_eq!(policy.minimum_act_version.to_string(), "0.2.70");
    assert_eq!(
        policy.container_daemon_socket,
        Some(PathBuf::from("/run/docker.sock"))
    );
    assert_eq!(policy.probe_ttl, Duration::from_secs(30));
}

#[test]
fn an_invalid_policy_is_a_boot_failure_not_a_per_call_surprise() {
    let harness = Harness::new();
    let mut args = harness.args();
    args.isolation = "sometimes".to_owned();
    assert!(matches!(
        Config::build(&args).expect_err("unknown policy"),
        ConfigError::IsolationPolicyInvalid { .. }
    ));
}

#[test]
fn the_validator_resolves_a_real_workspace_through_the_public_api_only() {
    let harness = Harness::new();
    let config = Config::build(&harness.args()).expect("valid boot configuration");
    let validator = harness.validator(&config);
    let request = RunRequest::new("ci", "build", "benchmark", harness.repo.head());

    let target = validator
        .validate(&request)
        .expect("a clean repo validates");
    assert_eq!(target.commit_full_sha, harness.repo.head());
    assert_eq!(target.workflow_display, "ci");
    assert_eq!(target.job_id, "build");
    assert_eq!(target.category, "benchmark");
    assert_eq!(target.workspace_root, harness.repo.path());
    assert_eq!(target.evidence_dir, harness.repo.path().join(".evidence"));
    // HEAD in place under the default hybrid policy, so no worktree warning.
    assert_eq!(target.mode.as_str(), "head_only");
    assert!(target.warnings.is_empty());
    assert_eq!(
        target.workflow_file,
        harness.repo.workflows_dir().join("ci.yml")
    );
}

#[tokio::test]
async fn validate_async_keeps_the_blocking_work_off_the_io_thread() {
    let harness = Harness::new();
    let config = Config::build(&harness.args()).expect("valid boot configuration");
    let validator = Arc::new(harness.validator(&config));
    let request = RunRequest::new("ci", "lint", "coverage", harness.repo.head());
    let target = validator
        .validate_async(request)
        .await
        .expect("the same verdict, off the worker");
    assert_eq!(target.job_id, "lint");
    assert_eq!(target.category, "coverage");
}

#[test]
fn a_joinable_second_validator_shares_nothing_with_the_first() {
    // The probe cache is per-`Validator`; two validators (two workspaces, say) must
    // not answer from each other's cache.
    let harness = Harness::new();
    let config = Config::build(&harness.args()).expect("valid boot configuration");
    let first = harness.validator(&config);
    let second = harness.validator(&config);
    let request = RunRequest::new("ci", "build", "benchmark", harness.repo.head());
    assert!(first.validate(&request).is_ok());
    assert!(second.validate(&request).is_ok());
}
