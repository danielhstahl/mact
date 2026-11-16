//! `fathom.toml`: where it is found, what it may contain, and what a bad one costs.
//!
//! ADR-0006 §7 is the contract this module implements; the rules that show up in the code as
//! behaviour rather than as prose are these:
//!
//! * **One file, found root-ward.** `--config`, else `$FATHOM_CONFIG`, else `fathom.toml` in the
//!   workspace or any ancestor up to and including the git top level, else the built-in defaults.
//!   The search stops at the repository on purpose: a `fathom.toml` in `/home/dev` must not
//!   silently change what a repository's numbers mean.
//! * **A broken file is a refusal, never a fallback.** A parse error names the path and the
//!   line/column. Falling back to defaults would change every cache key while looking like an
//!   ordinary index run, which is the worst kind of wrong.
//! * **Unknown keys are an error, not a shrug.** Every section is `deny_unknown_fields`, so a
//!   renamed or retired key fails with the keys it *did* expect, and a noise glob that stopped
//!   being read is a build failure rather than a silently unlabelled diffstat.
//! * **Every key is read through this struct.** Resolved values — defaults applied — are what the
//!   analyzers get, because ADR-0006 §7.3's digests are defined over resolved values: a file that
//!   omits a key and a file that states the default must produce the same digest.
//! * **The roots are not config.** `--cache-dir`/`$FATHOM_DIR`, `--evidence-dir`/`$EVIDENCE_DIR`
//!   and `--config` are plumbing (ADR-0006 §3.1, §7.4) and are resolved in [`crate::boot`], not
//!   here. Letting the config file name the cache root would create a loop in which the wrong value
//!   hides the file that fixes it.
//!
//! ADR-0006 §7.1 also answers the ticket's "every flag overridable by env": it is `FATHOM_DIR`,
//! `FATHOM_CONFIG`, `EVIDENCE_DIR` and `RUST_LOG` — per-key variables like `FATHOM_PATHS_NOISE` are
//! refused, because a value in a CI job's environment is a value that never reaches the digest of
//! the run that used it.

use std::{
    fmt,
    path::{Path, PathBuf},
};

use serde::Deserialize;
use thiserror::Error;

/// The file name discovered root-ward (§7.1 step 3).
pub const CONFIG_FILE_NAME: &str = "fathom.toml";

/// Every way `fathom.toml` can refuse to be used.
///
/// Each variant prints one line naming the offending value plus a `fix:` line that changes it —
/// the shape `act_mcp::config::ConfigError` uses, kept deliberately so the two CLIs in this
/// workspace explain themselves the same way. The `#[error(...)]` string is the documentation of
/// the variant, hence the allow.
#[allow(
    missing_docs,
    reason = "the #[error] format string documents each variant and its fields"
)]
#[derive(Debug, Error)]
pub enum ConfigError {
    /// `--config` named a path that is not there. Never "use the defaults instead".
    #[error("config file does not exist: {path}\n  fix: pass --config <path> at an existing fathom.toml, or drop the flag to search for one")]
    Missing { path: PathBuf },

    /// The path exists but is not a file (a directory called `fathom.toml`, a dangling symlink).
    #[error("config file is not a regular file: {path}")]
    NotAFile { path: PathBuf },

    /// The file could not be read at all (permissions, an I/O error).
    #[error("cannot read config file {path}: {reason}")]
    Unreadable { path: PathBuf, reason: String },

    /// The file is not valid TOML, or names a key this build does not know.
    #[error("cannot parse config file {path}: {reason}\n  fix: the keys and defaults are ADR-0006 §7.2; every key is one of the tables [history], [paths], [languages], [index], [benchmarks], [cache]")]
    Invalid { path: PathBuf, reason: String },

    /// A path that has to be absolute-per-convention was empty.
    #[error("--config was empty; pass a path, or drop the flag to search for {CONFIG_FILE_NAME}")]
    EmptyPath,
}

/// The resolved configuration: the built-in defaults, overridden by the one file that was found.
///
/// `Default` is §7.2's "default" column verbatim, so "what does fathom do with no file at all" is
/// answerable from the source without a round trip through a shell.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The file these values came from, or `None` for the built-in defaults (§7.1 step 4). Not a
    /// key of the file — it is the answer to `doctor`'s "where did this value come from".
    #[serde(skip)]
    pub path: Option<PathBuf>,

    /// `history.*` — the walk set. Not a cache key: no cached value may depend on which *other*
    /// commits are in the window (§7.2's last bullet).
    #[serde(default)]
    pub history: History,
    /// `paths.*` — the three glob lists, whose precedence is `exclude > noise > source`.
    #[serde(default)]
    pub paths: Paths,
    /// `languages.*` — which front ends run (ADR-0007 §6.5).
    #[serde(default)]
    pub languages: Languages,
    /// `index.*` — ingest limits.
    #[serde(default)]
    pub index: Index,
    /// `benchmarks.*` — which metric ids survive into a row. Filter, not policy: ADR-0008 owns what
    /// a number means.
    #[serde(default)]
    pub benchmarks: Benchmarks,
    /// `cache.*` — prune policy. A policy, not an input, so it is in no digest.
    #[serde(default)]
    pub cache: Cache,
}

/// `history.*` (§7.2): the window, and which parent a row is compared against.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct History {
    /// How many commits to walk. `1..`; clamped up to 1 because a walk of zero is a typo.
    #[serde(deserialize_with = "depth_at_least_one")]
    pub depth: u32,
    /// Which refs to walk from. `["HEAD"]` unless the repo is a mirror or a bot's checkout.
    pub refs: Vec<String>,
    /// Follow only the first parent. The one config key a flag also overrides (`--first-parent`),
    /// because ADR-0008's per-commit delta depends on which parent a row is compared against
    /// (§7.4) — which is why the flag costs a re-index and says so.
    pub first_parent: bool,
}

impl Default for History {
    fn default() -> Self {
        Self {
            depth: DEFAULT_HISTORY_DEPTH,
            refs: vec!["HEAD".to_owned()],
            first_parent: false,
        }
    }
}

/// `history.depth`'s default: five hundred commits, a working session's worth at a busy cadence.
pub const DEFAULT_HISTORY_DEPTH: u32 = 500;

/// `paths.*` (§7.2). Matched against repo-relative paths with forward slashes, case-sensitive,
/// `**` crosses `/`, no negation, precedence `exclude > noise > (source ∩ an enabled language)`.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Paths {
    /// The candidate set for MI and the diffstat.
    pub source: Vec<String>,
    /// The list that decides *both* which files MI analyses and which paths' `+`/`-` count toward a
    /// row's headline diffstat — shared on purpose, so a lockfile is noise in one column and not
    /// the other (the ADR's §7.2 property).
    pub noise: Vec<String>,
    /// Never walked at all. `.git`, `.fathom` and `.evidence` by default: the tool does not analyse
    /// its own cache or the server's output.
    pub exclude: Vec<String>,
    /// ADR-0007 §7's addition: test files are excluded from MI and the diffstat unless asked for.
    pub include_tests: bool,
}

impl Default for Paths {
    fn default() -> Self {
        Self {
            source: vec!["**/*".to_owned()],
            noise: DEFAULT_NOISE_GLOBS
                .iter()
                .map(|g| (*g).to_owned())
                .collect(),
            exclude: DEFAULT_EXCLUDE_GLOBS
                .iter()
                .map(|g| (*g).to_owned())
                .collect(),
            include_tests: false,
        }
    }
}

/// `paths.noise`'s default (§7.2). A lockfile or a vendored tree is *labelled*, not hidden: the
/// lines still land in `history.noise_add`/`noise_del`.
pub const DEFAULT_NOISE_GLOBS: [&str; 8] = [
    "target/**",
    "node_modules/**",
    "**/*.lock",
    "Cargo.lock",
    "**/generated/**",
    "**/*.min.*",
    ".venv/**",
    "vendor/**",
];

/// `paths.exclude`'s default (§7.2).
pub const DEFAULT_EXCLUDE_GLOBS: [&str; 3] = [".git/**", ".fathom/**", ".evidence/**"];

/// `languages.*` (§7.2), the set ADR-0007 §6.5 ships a front end for.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Languages {
    /// Which languages are analysed. An entry outside ADR-0007's set is a `doctor` warning today
    /// and a refusal there.
    pub enabled: Vec<String>,
}

impl Default for Languages {
    fn default() -> Self {
        Self {
            enabled: DEFAULT_LANGUAGES.iter().map(|l| (*l).to_owned()).collect(),
        }
    }
}

/// `languages.enabled`'s default: ADR-0007 §6.5's v1 set (`schema/mi-definition.json`'s
/// `enabled_in_v1`), and nothing else until a front end and golden vectors exist for a language.
pub const DEFAULT_LANGUAGES: [&str; 2] = ["rust", "python"];

/// `index.*` (§7.2): ingest limits.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Index {
    /// A larger file is `oversized` and not analysed. `1..`; clamped up to 1.
    #[serde(deserialize_with = "cap_at_least_one")]
    pub file_size_cap_mib: u32,
}

impl Default for Index {
    fn default() -> Self {
        Self {
            file_size_cap_mib: DEFAULT_FILE_SIZE_CAP_MIB,
        }
    }
}

/// `index.file_size_cap_mib`'s default (§7.2).
pub const DEFAULT_FILE_SIZE_CAP_MIB: u32 = 2;

/// `benchmarks.*` (§7.2): which metric ids survive into a row. Allow-list empty means allow all —
/// fathom does not hide evidence by default — and deny wins over allow.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Benchmarks {
    /// Metric ids to keep; empty means every metric.
    pub allow: Vec<String>,
    /// Metric ids to drop; beats `allow`.
    pub deny: Vec<String>,
}

/// `cache.*` (§7.2): prune policy. Read by prune (mact-by1.10), in no digest.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Cache {
    /// Unreferenced blocks are collected past this many.
    pub block_cap: u32,
    /// Prune as part of `fathom index`.
    pub prune_on_index: bool,
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            block_cap: DEFAULT_BLOCK_CAP,
            prune_on_index: true,
        }
    }
}

/// `cache.block_cap`'s default (§7.2).
pub const DEFAULT_BLOCK_CAP: u32 = 200_000;

/// `history.depth` refused `0` with the same one-line-plus-fix shape as every other refusal.
fn depth_at_least_one<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = u32::deserialize(deserializer)?;
    if raw == 0 {
        return Err(serde::de::Error::custom(
            "history.depth must be >= 1; 0 would walk no commits",
        ));
    }
    Ok(raw)
}

/// `index.file_size_cap_mib` refused `0` for the same reason.
fn cap_at_least_one<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = u32::deserialize(deserializer)?;
    if raw == 0 {
        return Err(serde::de::Error::custom(
            "index.file_size_cap_mib must be >= 1; 0 would mark every file oversized",
        ));
    }
    Ok(raw)
}

impl Config {
    /// The configuration for a workspace, following §7.1's precedence.
    ///
    /// `explicit` is `--config` (clap has already folded `$FATHOM_CONFIG` into it, which is the
    /// same precedence order and the same "never fall back" rule); `workspace` is the resolved
    /// workspace directory and `git_root` its repository top level, if it has one — the bound the
    /// root-ward search stops after.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] for any refusal in the module docs: missing explicit file, a directory, an
    /// unreadable file, or a file that is not valid TOML / names a key this build does not know.
    pub fn discover(
        explicit: Option<&Path>,
        workspace: &Path,
        git_root: Option<&Path>,
    ) -> Result<Self, ConfigError> {
        if let Some(path) = explicit {
            if path.as_os_str().is_empty() {
                return Err(ConfigError::EmptyPath);
            }
            return Self::read(path);
        }

        for directory in ancestors(workspace, git_root) {
            let candidate = directory.join(CONFIG_FILE_NAME);
            match candidate.symlink_metadata() {
                Ok(meta) if meta.is_file() => return Self::read(&candidate),
                // A directory or a dangling symlink where the file would be: that is the collision
                // case, and §7.1's "never fall back" applies to a found-but-broken file.
                Ok(_) => return Err(ConfigError::NotAFile { path: candidate }),
                Err(_) => continue,
            }
        }

        // No file anywhere: the built-in defaults, whose `path` stays `None` because there is
        // genuinely no file — which is what `status` prints as `config<TAB>built-in defaults`.
        Ok(Self::default())
    }

    /// Read and parse one file. Errors name the path, and TOML's own line/column travels with them.
    ///
    /// # Errors
    ///
    /// See [`ConfigError`].
    pub fn read(path: &Path) -> Result<Self, ConfigError> {
        match path.symlink_metadata() {
            Err(err) => {
                return Err(if err.kind() == std::io::ErrorKind::NotFound {
                    ConfigError::Missing {
                        path: path.to_path_buf(),
                    }
                } else {
                    ConfigError::Unreadable {
                        path: path.to_path_buf(),
                        reason: err.to_string(),
                    }
                });
            }
            Ok(meta) if !meta.is_file() => {
                return Err(ConfigError::NotAFile {
                    path: path.to_path_buf(),
                });
            }
            Ok(_) => {}
        }
        let text = std::fs::read_to_string(path).map_err(|err| ConfigError::Unreadable {
            path: path.to_path_buf(),
            reason: err.to_string(),
        })?;
        let mut config: Self = toml::from_str(&text).map_err(|err| ConfigError::Invalid {
            path: path.to_path_buf(),
            reason: describe_toml_error(&err),
        })?;
        config.path = Some(path.to_path_buf());
        Ok(config)
    }

    /// `true` when these values came from a file rather than from the built-in defaults.
    #[must_use]
    pub fn from_file(&self) -> bool {
        self.path.is_some()
    }

    /// The one-line `key<TAB>value` view `fathom index --status` prints, so "why is this number
    /// what it is" is answerable without reading the file or the code (§7.1).
    pub fn status_lines(&self) -> Vec<(String, String)> {
        vec![
            (
                "config".to_owned(),
                self.path.as_ref().map_or_else(
                    || "built-in defaults".to_owned(),
                    |path| path.display().to_string(),
                ),
            ),
            ("history.depth".to_owned(), self.history.depth.to_string()),
            (
                "history.refs".to_owned(),
                if self.history.refs.is_empty() {
                    "(empty)".to_owned()
                } else {
                    self.history.refs.join(",")
                },
            ),
            (
                "history.first_parent".to_owned(),
                self.history.first_parent.to_string(),
            ),
            ("paths.noise".to_owned(), self.paths.noise.join(",")),
            ("paths.exclude".to_owned(), self.paths.exclude.join(",")),
            (
                "paths.include_tests".to_owned(),
                self.paths.include_tests.to_string(),
            ),
            (
                "languages.enabled".to_owned(),
                self.languages.enabled.join(","),
            ),
            (
                "index.file_size_cap_mib".to_owned(),
                self.index.file_size_cap_mib.to_string(),
            ),
            (
                "benchmarks.allow".to_owned(),
                if self.benchmarks.allow.is_empty() {
                    "(all)".to_owned()
                } else {
                    self.benchmarks.allow.join(",")
                },
            ),
            (
                "benchmarks.deny".to_owned(),
                if self.benchmarks.deny.is_empty() {
                    "(none)".to_owned()
                } else {
                    self.benchmarks.deny.join(",")
                },
            ),
            (
                "cache.block_cap".to_owned(),
                self.cache.block_cap.to_string(),
            ),
            (
                "cache.prune_on_index".to_owned(),
                self.cache.prune_on_index.to_string(),
            ),
        ]
    }
}

impl fmt::Display for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.path {
            Some(path) => write!(f, "{}", path.display()),
            None => write!(f, "built-in defaults (no {CONFIG_FILE_NAME} found)"),
        }
    }
}

/// `workspace` itself, then each ancestor, stopping after `git_root` (§7.1) — or at the
/// filesystem root when the workspace is not in a repository.
fn ancestors(workspace: &Path, git_root: Option<&Path>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut current = Some(workspace.to_path_buf());
    while let Some(directory) = current {
        let hit_root = git_root.is_some_and(|root| root == directory);
        out.push(directory.clone());
        if hit_root {
            break;
        }
        current = directory.parent().map(Path::to_path_buf);
    }
    out
}

/// TOML's error, in the sentence the user needs.
///
/// `toml_edit`'s `Display` is a five-line diagram: a header naming the line and column, the
/// offending source line, a caret, and the message. The refusal this crate prints is one sentence
/// plus a `fix:` (the `act_mcp::config` shape), so the header and the message are kept — in that
/// order, joined — and the ASCII art dropped. The message half is what carries an unknown key
/// (`unknown field \`noize\`, expected one of \`source\`, \`noise\`, …`), which is §7.1's "the
/// nearest existing key named", spelled by the parser rather than guessed at here.
fn describe_toml_error(err: &toml::de::Error) -> String {
    let text = err.to_string();
    let mut lines = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| !line.starts_with('|') && !line.starts_with('^'));
    let head = lines.next().unwrap_or("parse error");
    match lines.next_back() {
        Some(tail) if tail != head && !tail.starts_with("TOML parse error") => {
            format!("{head}: {tail}")
        }
        _ => head.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).expect("write the fixture");
        path
    }

    #[test]
    fn no_file_at_all_is_the_built_in_defaults() {
        let dir = tempfile::tempdir().expect("scratch dir");
        let config = Config::discover(None, dir.path(), None).expect("defaults are not an error");
        assert_eq!(config, Config::default());
        assert!(!config.from_file());
        assert_eq!(config.history.depth, DEFAULT_HISTORY_DEPTH);
        assert_eq!(config.languages.enabled, DEFAULT_LANGUAGES.to_vec());
        assert!(
            config.paths.noise.contains(&"node_modules/**".to_owned()),
            "the noise default is the ADR's list"
        );
    }

    #[test]
    fn an_explicit_file_is_read_and_named_as_the_source() {
        let dir = tempfile::tempdir().expect("scratch dir");
        let path = write(dir.path(), "mine.toml", "[history]\ndepth = 12\n");
        let config = Config::discover(Some(&path), dir.path(), None).expect("read the file");
        assert_eq!(config.history.depth, 12);
        assert!(!config.history.first_parent);
        assert_eq!(config.path.as_deref(), Some(path.as_path()));
    }

    #[test]
    fn a_missing_explicit_file_is_a_refusal_not_a_fallback() {
        let dir = tempfile::tempdir().expect("scratch dir");
        let missing = dir.path().join("nope.toml");
        let err = Config::discover(Some(&missing), dir.path(), None)
            .expect_err("an explicit file that is not there must not fall back");
        assert!(matches!(err, ConfigError::Missing { .. }));
        assert!(err.to_string().contains("fix:"), "{err}");
    }

    #[test]
    fn unknown_keys_are_an_error_naming_the_keys_that_exist() {
        let dir = tempfile::tempdir().expect("scratch dir");
        let path = write(dir.path(), "bad.toml", "[paths]\nnoize = [\"x/**\"]\n");
        let err = Config::read(&path).expect_err("a typo'd key is not a shrug");
        let message = err.to_string();
        assert!(message.contains("noize"), "{message}");
        assert!(
            message.contains("noise"),
            "{message}: must name the key it expected"
        );
    }

    #[test]
    fn an_unknown_table_is_an_error_too() {
        let dir = tempfile::tempdir().expect("scratch dir");
        let path = write(dir.path(), "bad.toml", "[mi]\nbands = \"v2\"\n");
        let err = Config::read(&path).expect_err("[mi] is not a table of this config");
        assert!(err.to_string().contains("mi"), "{err}");
    }

    #[test]
    fn a_syntax_error_carries_the_path_and_a_position() {
        let dir = tempfile::tempdir().expect("scratch dir");
        let path = write(dir.path(), "bad.toml", "[history\ndepth = \n");
        let err = Config::read(&path).expect_err("not TOML");
        let message = err.to_string();
        assert!(message.contains("bad.toml"), "{message}");
        assert!(
            message.contains("line"),
            "{message}: TOML's position should survive"
        );
    }

    #[test]
    fn a_zero_depth_is_refused_with_a_reason() {
        let dir = tempfile::tempdir().expect("scratch dir");
        let path = write(dir.path(), "zero.toml", "[history]\ndepth = 0\n");
        let err = Config::read(&path).expect_err("a walk of zero commits is a typo");
        assert!(err.to_string().contains("depth"), "{err}");
    }

    #[test]
    fn the_search_is_root_ward_and_stops_after_the_git_top_level() {
        let outer = tempfile::tempdir().expect("scratch dir");
        let repo = outer.path().join("repo");
        let member = repo.join("crates").join("member");
        std::fs::create_dir_all(&member).expect("build the tree");
        std::fs::create_dir_all(repo.join(".git")).expect("a git top level");
        // A file above the repository must be invisible; one at the top level must not be.
        write(outer.path(), CONFIG_FILE_NAME, "[history]\ndepth = 1\n");
        let found = write(&repo, CONFIG_FILE_NAME, "[history]\ndepth = 77\n");

        let from_member =
            Config::discover(None, &member, Some(&repo)).expect("find the repo's file");
        assert_eq!(from_member.history.depth, 77);
        assert_eq!(from_member.path.as_deref(), Some(found.as_path()));

        // With no git top level to stop at, the walk keeps going — so with the repository's own
        // file gone, the one above the repository is reachable again.
        std::fs::remove_file(&found).expect("remove the repo's file");
        let unbounded = Config::discover(None, &member, None).expect("find *a* file");
        assert_eq!(unbounded.history.depth, 1);
    }

    #[test]
    fn the_workspace_file_wins_over_an_ancestor() {
        let outer = tempfile::tempdir().expect("scratch dir");
        let repo = outer.path().join("repo");
        std::fs::create_dir_all(&repo).expect("build the tree");
        write(&repo, CONFIG_FILE_NAME, "[history]\ndepth = 3\n");
        write(outer.path(), CONFIG_FILE_NAME, "[history]\ndepth = 4\n");
        let config = Config::discover(None, &repo, Some(&repo)).expect("nearest wins");
        assert_eq!(config.history.depth, 3);
    }
}
