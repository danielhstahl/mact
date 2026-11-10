//! Boot configuration: what `act-mcp serve` accepts, and what it refuses.
//!
//! Everything here is checked **at boot** so a misconfigured server fails loudly
//! and legibly instead of failing later inside a tool call. Nothing in this
//! module touches the network.
//!
//! Flag/env precedence: a flag always wins over its env var, because clap's
//! `env` attribute only fills in a value that was not given on the command line.

use std::{
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    path::{Path, PathBuf},
};

use clap::Args;
use thiserror::Error;

/// Default bind address. Loopback only, always: act-mcp has **no
/// authentication** (mact-f1w.5), and `act` runs arbitrary container images, so
/// a routable bind is not a mode we ship. `--bind` to anything else is rejected
/// by [`Config::build`]; the rest of the hardening is mact-f1w.13.
pub const DEFAULT_BIND: &str = "127.0.0.1:8080";

/// Default `act` binary name, looked up on `PATH` with [`which`] at boot.
pub const DEFAULT_ACT_BINARY: &str = "act";

/// Resolved, validated, immutable boot configuration.
///
/// Shared with the service factory as an `Arc<Config>`; per-session state stays
/// cheap and `Clone` (see [`crate::server::ActMcp`]).
#[derive(Debug, Clone)]
pub struct Config {
    /// Repo root being acted on. Guaranteed to exist and to contain `.github`.
    pub workspace: PathBuf,
    /// Where durable `.evidence` JSONL lands. Defaults to
    /// `<workspace>/.evidence`; see ADR-0002 §1.1. Not created at boot — the
    /// evidence writer (mact-f1w.9) creates directories on demand.
    pub evidence_dir: PathBuf,
    /// Resolved bind address (always loopback).
    pub bind: SocketAddr,
    /// Resolved `act` executable (always an absolute path that exists).
    pub act_binary: PathBuf,
    /// Optional scratch dir for act log capture (mact-f1w.8). `None` means the
    /// runner will use `<evidence-dir>/runs`.
    pub log_dir: Option<PathBuf>,
}

/// Reasons `act-mcp serve` refuses to start.
///
/// The `Display` text is user-facing — it is what the CLI prints on stderr and
/// what an MCP host will show — so every variant names the offending value and
/// the way to fix it. The `#[error(...)]` string is the documentation of the
/// variant, hence the allow below.
#[allow(
    missing_docs,
    reason = "the #[error] format string documents each variant"
)]
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("workspace does not exist: {path}\n  fix: pass --workspace <path> at the repository you want to act on")]
    WorkspaceMissing { path: PathBuf },

    #[error("workspace is not a directory: {path}")]
    WorkspaceNotADirectory { path: PathBuf },

    #[error(
        "workspace {path} has no `.github` directory, so there are no GitHub Actions workflows to run\n  fix: point --workspace at the repository root that contains .github/workflows/*.yml"
    )]
    WorkspaceMissingGithub { path: PathBuf },

    #[error(
        "could not parse --bind {value:?}: expected host:port such as 127.0.0.1:8080 ({reason})"
    )]
    BindUnresolved { value: String, reason: String },

    #[error(
        "--bind {addr} is not a loopback address, and act-mcp has no authentication\n  fix: bind 127.0.0.1 or ::1 — act runs arbitrary containers behind this endpoint"
    )]
    BindNotLoopback { addr: SocketAddr },

    #[error(
        "act binary not found: `{requested}`\n  fix: install act (https://github.com/nektos/act) or pass --act-binary /absolute/path/to/act"
    )]
    ActBinaryMissing { requested: String },

    #[error("--log-dir exists but is not a directory: {path}")]
    LogDirNotADirectory { path: PathBuf },

    #[error("cannot read the current working directory: {reason}\n  fix: pass --workspace <path> explicitly")]
    NoCurrentDirectory { reason: String },
}

/// `act-mcp serve` arguments.
///
/// Public so integration tests and embedders can build it; the binary is a thin
/// wrapper over [`Config::build`] + [`crate::server::serve`].
#[derive(Debug, Clone, Args)]
pub struct ServeArgs {
    /// Repository to act on. Must exist and contain `.github`.
    #[arg(
        long,
        value_name = "PATH",
        env = "ACT_MCP_WORKSPACE",
        help = "Repository root to act on; must contain .github/workflows. Default: current directory."
    )]
    pub workspace: Option<PathBuf>,

    /// Address to bind. Loopback only.
    #[arg(
        long,
        value_name = "ADDR",
        default_value = DEFAULT_BIND,
        env = "ACT_MCP_BIND",
        help = "Address to listen on; loopback only (act-mcp has no auth). Port 0 asks the kernel for a free port; the real address is logged at startup."
    )]
    pub bind: String,

    /// Durable evidence root.
    #[arg(
        long,
        value_name = "PATH",
        // EVIDENCE_DIR is the env fallback named in ADR-0002 §1.1.
        env = "EVIDENCE_DIR",
        help = "Evidence root; defaults to <workspace>/.evidence."
    )]
    pub evidence_dir: Option<PathBuf>,

    /// `act` executable to run.
    #[arg(
        long,
        value_name = "PATH",
        default_value = DEFAULT_ACT_BINARY,
        env = "ACT_MCP_ACT_BINARY",
        help = "act binary to invoke; resolved on PATH at boot."
    )]
    pub act_binary: PathBuf,

    /// Scratch dir for act log capture.
    #[arg(
        long,
        value_name = "PATH",
        env = "ACT_MCP_LOG_DIR",
        help = "Directory for act log capture; defaults to <evidence-dir>/runs (mact-f1w.8)."
    )]
    pub log_dir: Option<PathBuf>,
}

impl Default for ServeArgs {
    fn default() -> Self {
        Self {
            workspace: None,
            bind: DEFAULT_BIND.to_owned(),
            evidence_dir: None,
            act_binary: PathBuf::from(DEFAULT_ACT_BINARY),
            log_dir: None,
        }
    }
}

impl Config {
    /// Resolve and validate CLI args into a boot configuration.
    ///
    /// Check order: workspace exists → workspace has `.github` → bind resolves
    /// and is loopback → `act` resolves → log dir is usable. The order is
    /// deliberate: the cheapest and most likely-mistaken check runs first, so
    /// the first error a user sees is the one they caused.
    pub fn build(args: &ServeArgs) -> Result<Self, ConfigError> {
        let workspace = Self::resolve_workspace(args)?;
        let bind = Self::resolve_bind(&args.bind)?;
        let act_binary = Self::resolve_act(&args.act_binary)?;

        let evidence_dir = args
            .evidence_dir
            .clone()
            .unwrap_or_else(|| workspace.join(".evidence"));

        if let Some(log_dir) = &args.log_dir {
            if log_dir.exists() && !log_dir.is_dir() {
                return Err(ConfigError::LogDirNotADirectory {
                    path: log_dir.clone(),
                });
            }
        }

        Ok(Self {
            workspace,
            evidence_dir,
            bind,
            act_binary,
            log_dir: args.log_dir.clone(),
        })
    }

    fn resolve_workspace(args: &ServeArgs) -> Result<PathBuf, ConfigError> {
        let raw = match args.workspace.clone() {
            Some(path) => path,
            None => std::env::current_dir().map_err(|e| ConfigError::NoCurrentDirectory {
                reason: e.to_string(),
            })?,
        };
        Self::validate_workspace(&raw)
    }

    /// Workspace gate: it must exist and it must contain a `.github` directory.
    ///
    /// Deliberately minimal at this stage — workflow/task resolution, the HEAD
    /// commit gate and docker presence land in mact-f1w.6 (ADR-0003).
    pub(crate) fn validate_workspace(path: &Path) -> Result<PathBuf, ConfigError> {
        if !path.exists() {
            return Err(ConfigError::WorkspaceMissing {
                path: path.to_path_buf(),
            });
        }
        if !path.is_dir() {
            return Err(ConfigError::WorkspaceNotADirectory {
                path: path.to_path_buf(),
            });
        }
        if !path.join(".github").is_dir() {
            return Err(ConfigError::WorkspaceMissingGithub {
                path: path.to_path_buf(),
            });
        }
        // Canonicalise once, here, so every later path comparison (worktree
        // paths, evidence paths, `../` smuggling checks in later tickets) is
        // made against a single absolute, symlink-free root.
        Ok(std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()))
    }

    fn resolve_bind(value: &str) -> Result<SocketAddr, ConfigError> {
        // `127.0.0.1:8080` and `[::1]:8080` parse directly; `localhost:8080`
        // needs a lookup. Anything that resolves to port 0 is rejected below,
        // so a bare hostname cannot silently become "pick a random port".
        let addrs: Vec<SocketAddr> = if let Ok(addr) = value.parse::<SocketAddr>() {
            vec![addr]
        } else {
            match value.to_socket_addrs() {
                Ok(iter) => iter.collect(),
                Err(err) => {
                    return Err(ConfigError::BindUnresolved {
                        value: value.to_owned(),
                        reason: err.to_string(),
                    });
                }
            }
        };
        let addr = addrs
            .into_iter()
            .next()
            .ok_or_else(|| ConfigError::BindUnresolved {
                value: value.to_owned(),
                reason: "no address returned".to_owned(),
            })?;

        // Port 0 is allowed and useful: the kernel picks a free port and
        // `serve` logs the real one. A typo like `--bind 127.0.0.1` (no port)
        // never gets here — it fails to resolve above.
        if !is_loopback(addr.ip()) {
            return Err(ConfigError::BindNotLoopback { addr });
        }
        Ok(addr)
    }

    fn resolve_act(requested: &Path) -> Result<PathBuf, ConfigError> {
        let requested_text = requested.display().to_string();
        // `which` accepts either a bare name (PATH search) or a path, so
        // `--act-binary ./act` and `--act-binary act` both mean something.
        which::which(requested).map_err(|_| ConfigError::ActBinaryMissing {
            requested: requested_text,
        })
    }
}

/// Loopback test covering `127.0.0.0/8`, `::1`, and IPv4-mapped `::ffff:127.0.0.1`.
pub fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.is_loopback(),
            None => v6.is_loopback(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args_with(workspace: &Path, act: &Path) -> ServeArgs {
        ServeArgs {
            workspace: Some(workspace.to_path_buf()),
            bind: DEFAULT_BIND.to_owned(),
            evidence_dir: None,
            act_binary: act.to_path_buf(),
            log_dir: None,
        }
    }

    /// A workspace that passes the gate: a dir with `.github/workflows` in it.
    fn fixture_workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(".github/workflows")).expect("create .github");
        dir
    }

    /// An executable stub standing in for `act` (mact-f1w.8 replaces it with a
    /// real runner; nothing invokes it yet).
    fn stub_act(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"#!/bin/sh\nexit 0\n").expect("write stub");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        path
    }

    #[test]
    fn build_accepts_a_valid_workspace_and_resolves_defaults() {
        let workspace = fixture_workspace();
        let tmp = tempfile::tempdir().expect("tempdir");
        let act = stub_act(tmp.path(), "act");
        let config = Config::build(&args_with(workspace.path(), &act)).expect("valid config");

        assert_eq!(
            config.workspace,
            std::fs::canonicalize(workspace.path()).expect("canonical")
        );
        assert_eq!(config.evidence_dir, config.workspace.join(".evidence"));
        assert_eq!(config.bind.to_string(), DEFAULT_BIND);
        assert_eq!(config.act_binary, act);
        assert!(config.log_dir.is_none());
    }

    #[test]
    fn build_rejects_a_missing_workspace() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let act = stub_act(tmp.path(), "act");
        let missing = tmp.path().join("nope");
        let err = Config::build(&args_with(&missing, &act)).expect_err("must fail");
        assert!(matches!(err, ConfigError::WorkspaceMissing { .. }));
        assert!(err.to_string().contains("fix:"));
    }

    #[test]
    fn build_rejects_a_workspace_without_dot_github() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tmp = tempfile::tempdir().expect("tempdir");
        let act = stub_act(tmp.path(), "act");
        let err = Config::build(&args_with(dir.path(), &act)).expect_err("must fail");
        assert!(matches!(err, ConfigError::WorkspaceMissingGithub { .. }));
        assert!(err.to_string().contains(".github"));
    }

    #[test]
    fn build_rejects_a_missing_act_binary() {
        let workspace = fixture_workspace();
        let tmp = tempfile::tempdir().expect("tempdir");
        let err = Config::build(&args_with(workspace.path(), &tmp.path().join("no-act")))
            .expect_err("must fail");
        assert!(matches!(err, ConfigError::ActBinaryMissing { .. }));
        assert!(err.to_string().contains("nektos/act"));
    }

    #[test]
    fn bind_must_be_loopback() {
        for value in ["0.0.0.0:8080", "10.1.2.3:8080", "8.8.8.8:1194"] {
            let err = Config::resolve_bind(value).expect_err("must reject non-loopback");
            assert!(
                matches!(err, ConfigError::BindNotLoopback { .. }),
                "{value} → {err}"
            );
        }
    }

    #[test]
    fn bind_accepts_loopback_forms_and_rejects_portless_or_rubbish() {
        assert_eq!(
            Config::resolve_bind("[::1]:8080")
                .expect("v6 loopback")
                .port(),
            8080
        );
        assert!(Config::resolve_bind("localhost:8080").is_ok());
        // No port at all is a typo, not "port 0".
        assert!(matches!(
            Config::resolve_bind("127.0.0.1"),
            Err(ConfigError::BindUnresolved { .. })
        ));
        // Port 0 means "any free port"; `serve` logs the port it got.
        assert_eq!(
            Config::resolve_bind("127.0.0.1:0")
                .expect("port 0 is legal")
                .port(),
            0
        );
        assert!(Config::resolve_bind("not an address").is_err());
    }

    #[test]
    fn loopback_helper_covers_mapped_v6() {
        let parse = |text: &str| text.parse::<IpAddr>().expect("literal ip address");
        assert!(is_loopback(parse("127.5.6.7")));
        assert!(!is_loopback(parse("192.168.0.1")));
        assert!(is_loopback(parse("::1")));
        assert!(is_loopback(parse("::ffff:127.0.0.1")));
        assert!(!is_loopback(parse("::ffff:8.8.8.8")));
    }
}
