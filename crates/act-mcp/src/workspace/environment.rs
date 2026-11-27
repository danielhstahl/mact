//! The seam between validation and the outside world.
//!
//! Every effect this crate's validation layer can perform — run `git`, look a
//! program up on `PATH`, ask a container runtime if it is alive — goes through
//! [`Environment`], and every one of them is bounded by a timeout. That exists for
//! two reasons:
//!
//! * **Testability.** A unit test installs a stub and asserts on both the verdict
//!   and the *call count*, which is how "shape is validated before any subprocess"
//!   and "the probe is cached for 30 s" become assertions instead of hopes.
//! * **Safety.** The subprocesses are `git` and `docker`, launched in a directory
//!   somebody else pointed the server at, with a scrubbed environment and no
//!   shell interpolation (ADR-0003 §7.8).
//!
//! Nothing here may spawn `act`: mact-f1w.6 is a *validation* layer, and act
//! belongs to the runner (mact-f1w.8). That is why [`Environment::program_version`]
//! has a default that does nothing — the wiring ticket supplies the real prober.

use std::{
    collections::BTreeMap,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use thiserror::Error;

/// How long a container-runtime probe may take before it is called unreachable.
///
/// Deliberately far shorter than [`crate::config`]’s git budget: `docker info`
/// against a live daemon is milliseconds, and a hung daemon must not be able to
/// hold up the pre-start budget for as long as a `git status` may.
pub const DEFAULT_CONTAINER_PROBE_TIMEOUT_SECS: u32 = 3;

/// [`DEFAULT_CONTAINER_PROBE_TIMEOUT_SECS`] as a [`Duration`].
pub const DEFAULT_CONTAINER_PROBE_TIMEOUT: Duration =
    Duration::from_secs(DEFAULT_CONTAINER_PROBE_TIMEOUT_SECS as u64);

/// The most an operator may ask for with `--container-probe-timeout-secs`.
///
/// The ceiling exists because the probe is on the pre-start path: ADR-0003 §10 bounds that whole
/// path, and an unbounded runtime probe would let a daemon that never answers hold every later
/// refusal for its length — and the failed probe is then cached for [`DEFAULT_PROBE_TTL`]. A busy
/// daemon (a cold image pull in flight) is the case that needs more than the default; a minute is
/// far beyond it without letting the gate outlive the budget it sits inside.
pub const MAX_CONTAINER_PROBE_TIMEOUT: Duration = Duration::from_secs(60);

/// Result of one bounded subprocess: exit code plus captured output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    /// Exit code, or `None` when the process died on a signal.
    pub code: Option<i32>,
    /// Captured stdout (as bytes: paths and file contents are not guaranteed UTF-8).
    pub stdout: Vec<u8>,
    /// Captured stderr.
    pub stderr: Vec<u8>,
}

impl CommandOutput {
    /// A successful command printing `text` on stdout — the constructor test
    /// doubles reach for.
    pub fn stdout_text(text: impl AsRef<str>) -> Self {
        Self {
            code: Some(0),
            stdout: text.as_ref().as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }

    /// A command that exited non-zero, with `text` on stderr.
    pub fn failure(code: i32, text: impl AsRef<str>) -> Self {
        Self {
            code: Some(code),
            stdout: Vec::new(),
            stderr: text.as_ref().as_bytes().to_vec(),
        }
    }

    /// Whether the command reported success.
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// stdout, lossily decoded. Everything we read (`rev-parse`, `ls-tree`,
    /// workflow files) is text in practice; lossy beats failing on a stray byte.
    pub fn stdout_lossy(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// stderr, lossily decoded.
    pub fn stderr_lossy(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }

    /// First non-empty trimmed line of stdout, which is the shape most read-only
    /// `git` subcommands answer in.
    pub fn first_line(&self) -> String {
        first_non_empty_line(&self.stdout_lossy())
    }

    /// First non-empty trimmed line of stderr, for error text.
    pub fn first_error_line(&self) -> String {
        first_non_empty_line(&self.stderr_lossy())
    }
}

fn first_non_empty_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_owned()
}

/// Why a subprocess could not produce a result at all.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CommandError {
    /// The program could not be spawned (missing, not executable, EACCES …).
    #[error("could not run `{program}`: {reason}")]
    Spawn {
        /// Program name or path we tried to run.
        program: String,
        /// OS-level explanation.
        reason: String,
    },

    /// The program was killed because it passed its deadline.
    #[error("`{program}` did not exit within {timeout:?} and was killed")]
    Timeout {
        /// Program name or path we ran.
        program: String,
        /// The deadline it missed.
        timeout: Duration,
    },
}

impl CommandError {
    /// Whether this failure is worth retrying unchanged (a timeout may be a busy
    /// machine; a missing binary is not).
    pub fn retryable(&self) -> bool {
        matches!(self, Self::Timeout { .. })
    }
}

/// Why a program could not be resolved on `PATH`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProgramError {
    /// Nothing by that name exists on `PATH` (or the given path does not exist).
    #[error("`{requested}` was not found")]
    NotFound {
        /// What was asked for.
        requested: String,
    },

    /// The file exists but is not executable.
    #[error("`{requested}` exists but is not executable")]
    NotExecutable {
        /// What was asked for.
        requested: String,
    },
}

/// A container runtime that answered the reachability probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerRuntime {
    /// Program used to probe it (`docker`).
    pub program: String,
    /// Reported version, when the probe could read one.
    pub version: Option<String>,
}

/// Why no container runtime answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainerProbeError {
    /// No runtime client is installed.
    NotFound(String),
    /// The client ran but the daemon did not answer in time.
    Timeout(String),
    /// The daemon answered with an error.
    Error(String),
}

impl ContainerProbeError {
    /// The `reason` token ADR-0001 §5.3 pins for `docker_unreachable`.
    pub fn reason(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "not_found",
            Self::Timeout(_) => "timeout",
            Self::Error(_) => "error",
        }
    }

    /// The `detail` string that goes next to [`Self::reason`].
    pub fn detail(&self) -> String {
        match self {
            Self::NotFound(text) | Self::Timeout(text) | Self::Error(text) => text.clone(),
        }
    }
}

/// Everything validation may ask of the machine it runs on.
///
/// Implementations must be `Send + Sync` and must bound every subprocess: the
/// validator is called on a request path with a pre-start budget (ADR-0004 §4.4).
pub trait Environment: Send + Sync {
    /// Run `git -C <workspace> --no-optional-locks <args>` with a scrubbed
    /// environment (ADR-0003 §7.8) and kill it at `timeout`.
    ///
    /// Only read-only subcommands may be issued from validation; the environment
    /// flag is what makes that claim checkable rather than aspirational.
    fn git(
        &self,
        workspace: &Path,
        args: &[&str],
        timeout: Duration,
    ) -> Result<CommandOutput, CommandError>;

    /// Resolve a program the way `which` does: `PATH` search for a bare name,
    /// direct check for a path, and the result must be executable.
    fn resolve_program(&self, requested: &Path) -> Result<PathBuf, ProgramError>;

    /// Ask the container runtime whether it is alive (`docker info`, or the same
    /// against `socket` when the operator pinned one).
    fn container_runtime(
        &self,
        socket: Option<&Path>,
        timeout: Duration,
    ) -> Result<ContainerRuntime, ContainerProbeError>;

    /// Reported version of `program`, if this environment can obtain one.
    ///
    /// The default is `None` on purpose: getting `act`'s version means *running*
    /// `act`, which this module must not do. The wiring ticket (mact-f1w.14) may
    /// install a prober that already has act's `--version` output to hand; the
    /// validator then enforces [`crate::workspace::MINIMUM_ACT_VERSION`].
    fn program_version(&self, _program: &Path) -> Option<String> {
        None
    }

    /// One variable of *this process's* environment, as the act child would inherit it.
    ///
    /// Not for `git` — that runs with a scrubbed environment (ADR-0003 §7.8) and does not
    /// need asking. This exists for act's config discovery, which is about the environment
    /// act-mcp hands down, so the default reads the real process environment and a test
    /// overrides it the way it overrides everything else here.
    fn env_var(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }

    /// A small text file, if it is there and readable.
    ///
    /// `None` covers "absent", "a directory", "EACCES" and "not UTF-8" together, because
    /// that is how act sees them too: act reads its config files with an error it discards,
    /// so an actrc the server cannot read is exactly an actrc that does not exist. The
    /// default is `None` on purpose, as with [`Environment::program_version`]: a stub that
    /// wants act to find a config file has to say so.
    fn read_text_file(&self, _path: &Path) -> Option<String> {
        None
    }
}

/// How much of a config file is read before it is assumed not to be a config file.
pub(super) const MAX_CONFIG_FILE_BYTES: u64 = 64 * 1024;

/// How long the output-collecting thread gets after the child is reaped.
pub(super) const READER_GRACE: Duration = Duration::from_millis(250);

/// The real [`Environment`]: spawns `git` and `docker`, statelessly.
///
/// Stateless so the validator holds one for the process lifetime and no probe
/// state leaks between it and the cache in [`crate::workspace::Validator`].
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemEnvironment;

impl Environment for SystemEnvironment {
    fn git(
        &self,
        workspace: &Path,
        args: &[&str],
        timeout: Duration,
    ) -> Result<CommandOutput, CommandError> {
        let mut argv: Vec<std::ffi::OsString> = vec![
            std::ffi::OsString::from("-C"),
            workspace.as_os_str().to_os_string(),
            std::ffi::OsString::from("--no-optional-locks"),
        ];
        argv.extend(args.iter().map(std::ffi::OsString::from));
        // ADR-0003 §7.8: only PATH, HOME, LC_ALL=C and GIT_TERMINAL_PROMPT=0.
        // Inherited GIT_* (GIT_DIR, GIT_WORK_TREE, GIT_INDEX_FILE, …) are dropped
        // so the verdict depends on the workspace path and nothing else, and a
        // credential helper cannot block a call that is supposed to be validation.
        run(Path::new("git"), &argv, timeout, |command| {
            scrub_environment(command);
            command.env("GIT_TERMINAL_PROMPT", "0");
        })
    }

    /// act's config files, read the way act reads them: exist-or-not, no error kept.
    ///
    /// Bounded by `MAX_CONFIG_FILE_BYTES` because a config file is small or it is not a
    /// config file, and `read_to_string` on a FIFO someone pointed at this path would
    /// otherwise be the validator's problem.
    fn read_text_file(&self, path: &Path) -> Option<String> {
        let metadata = std::fs::metadata(path).ok()?;
        if !metadata.is_file() || metadata.len() > MAX_CONFIG_FILE_BYTES {
            return None;
        }
        std::fs::read_to_string(path).ok()
    }

    fn resolve_program(&self, requested: &Path) -> Result<PathBuf, ProgramError> {
        match which::which(requested) {
            Ok(found) => Ok(found),
            Err(_) => {
                // Distinguish "not installed" from "installed but not executable":
                // the remedies are different, and `act_missing` says which.
                let exists = requested
                    .parent()
                    .is_some_and(|parent| !parent.as_os_str().is_empty() && parent.exists())
                    && requested.exists();
                if exists {
                    Err(ProgramError::NotExecutable {
                        requested: requested.display().to_string(),
                    })
                } else {
                    Err(ProgramError::NotFound {
                        requested: requested.display().to_string(),
                    })
                }
            }
        }
    }

    fn container_runtime(
        &self,
        socket: Option<&Path>,
        timeout: Duration,
    ) -> Result<ContainerRuntime, ContainerProbeError> {
        let program = std::path::Path::new("docker");
        let docker = match self.resolve_program(program) {
            Ok(found) => found,
            Err(err) => {
                return Err(ContainerProbeError::NotFound(format!(
                    "no container runtime client: {err}"
                )));
            }
        };
        let endpoint = socket.map(|path| {
            let text = path.display().to_string();
            // Accept either a bare socket path or a full `unix://` endpoint.
            if text.contains("://") {
                text
            } else {
                format!("unix://{text}")
            }
        });
        let mut argv: Vec<std::ffi::OsString> = Vec::new();
        if let Some(endpoint) = &endpoint {
            argv.push(std::ffi::OsString::from("-H"));
            argv.push(std::ffi::OsString::from(endpoint));
        }
        argv.push(std::ffi::OsString::from("info"));
        argv.push(std::ffi::OsString::from("--format"));
        argv.push(std::ffi::OsString::from("{{.ServerVersion}}"));

        // One read of the environment, used for both the rule and the wording, so the refusal can
        // never name a different endpoint than the one this call dialled.
        let env = runtime_env_from(std::env::vars());
        let dialled = dialled_endpoint(socket, &env);
        match run(&docker, &argv, timeout, |command| {
            command.env_clear();
            command.envs(env);
        }) {
            Err(CommandError::Timeout { timeout, .. }) => {
                Err(ContainerProbeError::Timeout(format!(
                "the container runtime did not answer `docker info` within {}s (asked {dialled})",
                timeout.as_secs_f64()
            )))
            }
            Err(other) => Err(ContainerProbeError::Error(format!(
                "{other} (asked {dialled})"
            ))),
            Ok(output) if output.success() => Ok(ContainerRuntime {
                program: "docker".to_owned(),
                version: (!output.first_line().is_empty()).then(|| output.first_line()),
            }),
            Ok(output) => Err(ContainerProbeError::Error(format!(
                "`docker info` exited {}: {} (asked {dialled})",
                output.code.map(i64::from).unwrap_or(-1),
                output.first_error_line()
            ))),
        }
    }
}

/// Read a child pipe to EOF.
fn drain<R: Read>(pipe: Option<R>) -> Vec<u8> {
    let mut buffer = Vec::new();
    if let Some(mut pipe) = pipe {
        let _ = pipe.read_to_end(&mut buffer);
    }
    buffer
}

/// The environment every scrubbed probe starts from: three keys, nothing else (ADR-0003 §7.8).
const BASE_ENV_KEYS: [&str; 3] = ["PATH", "HOME", "LC_ALL"];

/// Keys the container-runtime probe carries on top of [`BASE_ENV_KEYS`].
///
/// ADR-0003 §7.6 passes act's environment through untouched *precisely because* `DOCKER_HOST` and
/// `DOCKER_CONTEXT` are how a daemon is found at all on a rootless, colima, podman-socket or TCP
/// setup — and this probe is the gate that stands in front of that same act. With them stripped it
/// dialled the runtime default while act would have dialled the operator's endpoint, so a commit act
/// could run was refused as `docker_unreachable`, with a message that could not say which daemon had
/// been asked. An allowlist, not a `DOCKER_` prefix: a variable that happens to start with
/// `DOCKER_` and carries a credential stays where it was.
const RUNTIME_ENV_KEYS: [&str; 5] = [
    "DOCKER_HOST",
    "DOCKER_CONTEXT",
    "DOCKER_CERT_PATH",
    "DOCKER_TLS_VERIFY",
    "DOCKER_CONFIG",
];

fn scrub_environment(command: &mut Command) {
    command.env_clear();
    for key in BASE_ENV_KEYS {
        if let Ok(value) = std::env::var(key) {
            command.env(key, value);
        }
    }
    command.env("LC_ALL", "C");
}

/// The environment of the container-runtime probe: [`scrub_environment`]'s base plus
/// [`RUNTIME_ENV_KEYS`], as a pure function of the environment it starts from so the rule can be
/// checked without spawning anything.
fn runtime_env_from(
    source: impl IntoIterator<Item = (String, String)>,
) -> BTreeMap<String, String> {
    let mut picked = BTreeMap::new();
    for (key, value) in source {
        if BASE_ENV_KEYS.contains(&key.as_str()) || RUNTIME_ENV_KEYS.contains(&key.as_str()) {
            picked.insert(key, value);
        }
    }
    // The scrub's last word, exactly as `scrub_environment` leaves it: a locale the caller set is
    // not allowed to change what `docker info` prints.
    picked.insert("LC_ALL".to_owned(), "C".to_owned());
    picked
}

/// What the probe dialled, in the words the refusal prints.
///
/// Without this the detail cannot answer the one question that matters: a `docker_unreachable` from a
/// socket nobody configured reads identically to one from the operator's own daemon, and the two have
/// opposite fixes — "your docker is down" versus "you told the gate to ask somewhere else".
fn dialled_endpoint(pinned: Option<&Path>, env: &BTreeMap<String, String>) -> String {
    if let Some(path) = pinned {
        return format!(
            "the pinned socket `{}` (--container-daemon-socket)",
            path.display()
        );
    }
    if let Some(host) = env.get("DOCKER_HOST") {
        return format!("`$DOCKER_HOST={host}`");
    }
    if let Some(context) = env.get("DOCKER_CONTEXT") {
        return format!("the docker context `{context}` (`$DOCKER_CONTEXT`)");
    }
    "the runtime default (`/var/run/docker.sock`)".to_owned()
}

/// Spawn, capture and bound one subprocess.
///
/// stdout/stderr are drained on a helper thread so a chatty command cannot fill a
/// 64 KiB pipe and deadlock us mid-read while we wait; the wait itself polls
/// `try_wait` because `std` has no wait-with-timeout.
fn run(
    program: &Path,
    args: &[std::ffi::OsString],
    timeout: Duration,
    configure: impl FnOnce(&mut Command),
) -> Result<CommandOutput, CommandError> {
    let program_name = program.display().to_string();
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure(&mut command);

    // No `kill_on_drop` in `std`: every exit path below either waits for the child
    // or kills it before returning, so no child outlives this function.
    let mut child = command.spawn().map_err(|err| CommandError::Spawn {
        program: program_name.clone(),
        reason: err.to_string(),
    })?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (sender, receiver) = mpsc::channel();
    let reader = thread::spawn(move || {
        // Read until EOF, which is what makes a full pipe non-fatal.
        let output = (drain(stdout), drain(stderr));
        // A hung reader must not outlive the child; dropping the sender makes the
        // receiver below fall back to empty output instead.
        let _ = sender.send(output);
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(2));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                // Detach the reader rather than joining it: whatever we just killed
                // may have left a grandchild holding the pipes (`sh -c 'sleep 5'`
                // is the trivial case), and joining would let *that* decide how long
                // the caller waits — which is exactly what the timeout forbids. The
                // reader exits on its own once the pipes close.
                drop(receiver);
                return Err(CommandError::Timeout {
                    program: program_name,
                    timeout,
                });
            }
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                drop(receiver);
                return Err(CommandError::Spawn {
                    program: program_name,
                    reason: err.to_string(),
                });
            }
        }
    };

    // The child is gone, so the pipes should be closed and the reader finishing.
    // The grace period is short on purpose: a killed process's *descendants* may
    // still hold the pipes open (sh -c 'sleep 5' is the easy example), and waiting
    // for them would defeat the timeout we just enforced. Falling back to empty
    // output after it costs us the tail of a command we already killed.
    let (stdout, stderr) = receiver.recv_timeout(READER_GRACE).unwrap_or_default();
    drop(reader);
    drop(receiver);
    Ok(CommandOutput {
        code: status.code(),
        stdout,
        stderr,
    })
}

/// Monotonic-ish clock in milliseconds, as a seam so the probe cache is testable.
pub trait Clock: Send + Sync {
    /// Milliseconds since an arbitrary, fixed origin. Only differences matter.
    fn now_millis(&self) -> u64;
}

/// The real clock ([`UNIX_EPOCH`] for origin).
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_millis(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX)
    }
}

/// `major.minor.patch`, just enough to compare an act version against a floor.
///
/// A semver crate would be a second vocabulary for one comparison; act's own
/// versions are plain triples.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SemVer {
    /// Major component.
    pub major: u32,
    /// Minor component.
    pub minor: u32,
    /// Patch component.
    pub patch: u32,
}

impl std::fmt::Display for SemVer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl std::str::FromStr for SemVer {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let trimmed = text.trim().trim_start_matches('v');
        let mut parts = trimmed.split('.');
        let mut next = |label: &str| -> Result<u32, String> {
            parts
                .next()
                .and_then(|part| part.parse::<u32>().ok())
                .ok_or_else(|| format!("{label} is not a number (expected major.minor.patch)"))
        };
        let version = Self {
            major: next("major")?,
            minor: next("minor")?,
            patch: next("patch")?,
        };
        if parts.next().is_some() {
            return Err(format!("`{text}` has more than three components"));
        }
        Ok(version)
    }
}

/// Parse the version out of an `act --version` banner.
///
/// Observed shapes across the supported range are `act version 0.2.68` and
/// `act version v0.2.63`; some builds append a commit line. Anything we cannot
/// read is `None`, which the validator treats as "not probed", never as "too old".
pub fn parse_act_version(text: &str) -> Option<SemVer> {
    text.split_whitespace()
        .filter_map(|token| token.parse::<SemVer>().ok())
        .next()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_orders_componentwise_not_lexically() {
        let parse = |text: &str| text.parse::<SemVer>().expect("valid version");
        assert!(parse("0.2.100") > parse("0.2.62"));
        assert!(parse("0.10.0") > parse("0.9.9"));
        assert!(parse("1.0.0") > parse("0.99.99"));
        assert_eq!(parse("v0.2.62"), parse("0.2.62"));
    }

    #[test]
    fn semver_rejects_rubbish_and_extra_components() {
        for text in ["", "0", "0.2", "0.2.x", "0.2.62.1", "-1.2.3", "0.-1.2"] {
            assert!(text.parse::<SemVer>().is_err(), "{text:?} parsed");
        }
    }

    #[test]
    fn act_version_banner_is_parsed() {
        assert_eq!(
            parse_act_version("act version 0.2.68"),
            Some("0.2.68".parse().expect("valid"))
        );
        assert_eq!(
            parse_act_version("act version v0.2.63\nhttps://github.com/nektos/act\n"),
            Some("0.2.63".parse().expect("valid"))
        );
        assert_eq!(parse_act_version("act version development"), None);
        assert_eq!(parse_act_version(""), None);
    }

    #[test]
    fn git_runs_read_only_and_uses_the_scrubbed_environment() {
        let dir = tempfile::tempdir().expect("tempdir");
        crate::fixtures::TestRepo::init(dir.path().join("repo").as_path());
        let repo = dir.path().join("repo");
        let output = SystemEnvironment
            .git(
                &repo,
                &["rev-parse", "--is-bare-repository"],
                Duration::from_secs(5),
            )
            .expect("git runs");
        assert!(output.success());
        assert_eq!(output.first_line(), "false");

        // A GIT_* from our own environment must not reach git: pointing GIT_DIR at
        // a nonexistent directory changes the answer only if it leaked through.
        std::env::set_var("GIT_DIR", "/nonexistent-git-dir-act-mcp");
        let output = SystemEnvironment
            .git(
                &repo,
                &["rev-parse", "--is-bare-repository"],
                Duration::from_secs(5),
            )
            .expect("git runs");
        std::env::remove_var("GIT_DIR");
        assert!(output.success(), "{output:?}");
    }

    #[test]
    fn a_command_that_hangs_is_killed_at_its_deadline() {
        let started = Instant::now();
        let shell = Path::new("/bin/sh");
        let error = run(
            shell,
            &[
                std::ffi::OsString::from("-c"),
                std::ffi::OsString::from("sleep 5"),
            ],
            Duration::from_millis(50),
            |_| {},
        )
        .expect_err("must time out");
        assert!(matches!(error, CommandError::Timeout { .. }), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the timeout must actually bound the call ({}ms elapsed)",
            started.elapsed().as_millis()
        );
    }

    #[test]
    fn output_larger_than_a_pipe_buffer_is_still_captured() {
        let shell = Path::new("/bin/sh");
        let output = run(
            shell,
            &[
                std::ffi::OsString::from("-c"),
                std::ffi::OsString::from("head -c 200000 /dev/zero | tr '\\0' 'x'"),
            ],
            Duration::from_secs(10),
            |_| {},
        )
        .expect("runs");
        assert_eq!(output.stdout.len(), 200_000);
    }

    #[test]
    fn resolving_a_missing_program_distinguishes_it_from_a_non_executable_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("nope");
        let err = SystemEnvironment
            .resolve_program(&missing)
            .expect_err("must not resolve");
        assert!(matches!(err, ProgramError::NotFound { .. }), "{err}");

        let not_executable = dir.path().join("act");
        std::fs::write(&not_executable, b"not a program").expect("write");
        #[cfg(unix)]
        std::fs::set_permissions(
            &not_executable,
            std::os::unix::fs::PermissionsExt::from_mode(0o644),
        )
        .expect("chmod");
        let err = SystemEnvironment
            .resolve_program(&not_executable)
            .expect_err("must not resolve");
        assert!(matches!(err, ProgramError::NotExecutable { .. }), "{err}");

        let ok = dir.path().join("act-ok");
        std::fs::write(&ok, b"#!/bin/sh\nexit 0\n").expect("write");
        #[cfg(unix)]
        std::fs::set_permissions(&ok, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("chmod");
        assert_eq!(
            SystemEnvironment.resolve_program(&ok).expect("resolves"),
            which::which(&ok).expect("which agrees")
        );
    }

    #[test]
    fn the_runtime_probe_carries_the_endpoint_it_is_asked_to_dial() {
        // The probe is the gate in front of act, and act is given its environment untouched
        // (ADR-0003 §7.6) because `DOCKER_HOST`/`DOCKER_CONTEXT` are how a daemon is found at all on
        // a rootless, colima, podman-socket or TCP setup. Stripping them made the gate ask a
        // different daemon than the run would use, and refuse a commit act could have run.
        let source = || {
            [
                ("PATH".to_owned(), "/usr/bin".to_owned()),
                ("HOME".to_owned(), "/home/operator".to_owned()),
                ("LC_ALL".to_owned(), "en_US.UTF-8".to_owned()),
                (
                    "DOCKER_HOST".to_owned(),
                    "unix:///run/user/1000/docker.sock".to_owned(),
                ),
                ("DOCKER_CONTEXT".to_owned(), "colima".to_owned()),
                (
                    "DOCKER_CERT_PATH".to_owned(),
                    "/home/operator/certs".to_owned(),
                ),
                ("DOCKER_TLS_VERIFY".to_owned(), "1".to_owned()),
                (
                    "DOCKER_CONFIG".to_owned(),
                    "/home/operator/.docker".to_owned(),
                ),
                // Everything else stays out — including anything that merely looks docker-shaped and
                // anything that smells like a credential.
                ("DOCKER_PASSWORD".to_owned(), "hunter2".to_owned()),
                ("AWS_SECRET_ACCESS_KEY".to_owned(), "hunter2".to_owned()),
                ("SSH_AUTH_SOCK".to_owned(), "/tmp/sock".to_owned()),
                ("GITHUB_TOKEN".to_owned(), "hunter2".to_owned()),
            ]
        };
        let picked = runtime_env_from(source());
        for key in [
            "PATH",
            "HOME",
            "DOCKER_HOST",
            "DOCKER_CONTEXT",
            "DOCKER_CERT_PATH",
            "DOCKER_TLS_VERIFY",
            "DOCKER_CONFIG",
        ] {
            assert!(
                picked.contains_key(key),
                "{key} must reach the probe: {picked:?}"
            );
        }
        assert_eq!(
            picked.get("DOCKER_HOST").expect("host"),
            "unix:///run/user/1000/docker.sock"
        );
        // The scrub's locale wins, exactly as it does for `git`: a caller's locale must not be able
        // to change what `docker info` prints.
        assert_eq!(picked.get("LC_ALL").expect("locale"), "C");
        for key in [
            "DOCKER_PASSWORD",
            "AWS_SECRET_ACCESS_KEY",
            "SSH_AUTH_SOCK",
            "GITHUB_TOKEN",
        ] {
            assert!(
                !picked.contains_key(key),
                "{key} must not reach the probe: {picked:?}"
            );
        }
        // And an environment with nothing docker-shaped in it is not an error: the runtime default is
        // what gets dialled, which is the behaviour that predates this.
        let plain = runtime_env_from([("PATH".to_owned(), "/bin".to_owned())]);
        assert_eq!(
            plain.keys().collect::<Vec<_>>(),
            vec!["LC_ALL", "PATH"],
            "no DOCKER_* set is not an error"
        );
    }

    #[test]
    fn a_refusal_names_the_daemon_it_dialled() {
        // "your container runtime is down" and "the gate was told to ask somewhere else" look
        // identical without this, and they are opposite problems.
        let empty = BTreeMap::new();
        assert_eq!(
            dialled_endpoint(None, &empty),
            "the runtime default (`/var/run/docker.sock`)"
        );
        let with_host =
            BTreeMap::from([("DOCKER_HOST".to_owned(), "tcp://build-box:2376".to_owned())]);
        assert_eq!(
            dialled_endpoint(None, &with_host),
            "`$DOCKER_HOST=tcp://build-box:2376`",
            "a host beats a context, the way docker itself orders them"
        );
        let with_context = BTreeMap::from([("DOCKER_CONTEXT".to_owned(), "colima".to_owned())]);
        assert_eq!(
            dialled_endpoint(None, &with_context),
            "the docker context `colima` (`$DOCKER_CONTEXT`)"
        );
        assert_eq!(
            dialled_endpoint(Some(Path::new("/run/docker.sock")), &with_host),
            "the pinned socket `/run/docker.sock` (--container-daemon-socket)",
            "the pin is what was asked for, so the pin is what gets named"
        );
    }

    #[test]
    fn the_container_probe_never_hangs_a_test_and_never_needs_a_daemon() {
        // No docker on PATH is a *result*, not a panic; a bogus socket must not be
        // able to block longer than the timeout it was given.
        let result = SystemEnvironment.container_runtime(
            Some(Path::new("/nonexistent/docker.sock")),
            Duration::from_millis(200),
        );
        assert!(
            matches!(
                result,
                Err(ContainerProbeError::NotFound(_)
                    | ContainerProbeError::Timeout(_)
                    | ContainerProbeError::Error(_))
            ),
            "{result:?}"
        );
    }
}
