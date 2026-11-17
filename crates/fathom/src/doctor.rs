//! `fathom doctor` — why Fathom has nothing to show.
//!
//! A timeline of `—` is a diagnosis, not an answer. The TUI can only say what a row does not know;
//! this command says *why*, and it is the one command in this crate whose job is to be run when
//! something looks wrong. Every check is one line with a status and, when the status is not `ok`, one
//! or more `fix:` lines that change the thing being complained about — the same shape as every
//! refusal in this workspace ([`crate::boot`], [`crate::config`], [`crate::store`]), because a
//! diagnosis you cannot act on is a log line.
//!
//! ## The checks, in the order of a real conversation
//!
//! | name | what it answers |
//! |---|---|
//! | `workspace` | is this a git work tree, does it have commits, does `HEAD` resolve |
//! | `evidence` | does the evidence root exist, is it readable, does it pass `validate-evidence` |
//! | `runs` | advisory: runs that happened and published nothing (ADR-0008 §5.5) |
//! | `workflows` | is there anything to run — a coverage or benchmark job, discovered locally |
//! | `cache` | the store: version, writability, row and block counts, last index age and hit ratio, foreign files |
//! | `languages` | which paths were not analysed, and which config key would change that |
//! | `config` | `fathom.toml`: a key this build does not know, a glob that matches nothing |
//!
//! ## Status and exit code
//!
//! Each check is `ok`, `warn` or `fail`, and the process status is the worst of them
//! ([`crate::exit`]): `ok` → 0, `warn` → 1, `fail` → 2 — the same 0/1/2 scale `fathom report
//! --fail-on` and `act-mcp validate-evidence` use, so one CI step shape covers all three.
//!
//! `doctor` never refuses. A workspace that is not a repository is a `fail` check with a `fix:`, not
//! a [`crate::boot::BootError`] on stderr, because the command you run to explain a broken workspace
//! must not itself be stopped by it. That is what [`unusable`] is for: when even boot could not
//! resolve the workspace, the refusal becomes the report.
//!
//! ## What is advisory
//!
//! Anything derived from `runs/<run_id>/meta.json` is labelled **advisory** and never changes a
//! column (ADR-0008 §5.5): that file is outside the cross-commit contract, may be missing, and may
//! disagree with the evidence. Where they disagree the evidence wins, and this says so.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

use crate::boot::Boot;
use crate::classify::{Classifier, Scope};
use crate::config::Config;
use crate::exit::Exit;
use crate::git::Git;
use crate::mi;
use crate::store::{self, Runbook, Store};

use self::workflows::Discovery;

pub mod workflows;

/// The version of this command's JSON shape. Separate from [`crate::report::SCHEMA_VERSION`]: a
/// consumer that reads reports need not care what doctor says.
pub const SCHEMA_VERSION: u32 = 1;

/// How many detail lines one check prints before saying how many it hid.
///
/// A doctor that prints nine hundred bad lines is a doctor nobody reads; the count is the actionable
/// part and the first few are the sample.
const DETAIL_CAP: usize = 8;

/// How a check went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Nothing to act on.
    Ok,
    /// Worth acting on; the command still worked.
    Warn,
    /// Broken: the workspace, the data or the cache is not usable as it stands.
    Fail,
}

impl Status {
    /// The word printed.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }

    /// The process status this contributes; the worst check wins ([`crate::exit`]).
    #[must_use]
    pub const fn exit(self) -> Exit {
        match self {
            Self::Ok => Exit::Success,
            Self::Warn => Exit::Findings,
            Self::Fail => Exit::Blocked,
        }
    }
}

/// One check's answer.
#[derive(Debug, Clone, Serialize)]
pub struct Check {
    /// The check's name, as printed and as the JSON field.
    pub name: &'static str,
    /// `ok` | `warn` | `fail`.
    pub status: Status,
    /// The one line. Everything else is a detail.
    pub summary: String,
    /// The evidence behind the line, capped at eight entries so one bad directory cannot fill the
    /// terminal; when it is capped, the last entry says how many it hid.
    pub details: Vec<String>,
    /// What to change. Non-empty for every check that is not `ok` — that is the promise this command
    /// makes, and `tests/doctor.rs` asserts it.
    pub fixes: Vec<String>,
}

impl Check {
    /// A check that passed.
    #[must_use]
    pub fn ok(name: &'static str, summary: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Ok,
            summary: summary.into(),
            details: Vec::new(),
            fixes: Vec::new(),
        }
    }

    /// A check worth the user's attention.
    #[must_use]
    pub fn warn(name: &'static str, summary: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Warn,
            summary: summary.into(),
            details: Vec::new(),
            fixes: vec![fix.into()],
        }
    }

    /// A check that says the workspace is not usable as it stands.
    #[must_use]
    pub fn fail(name: &'static str, summary: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Fail,
            summary: summary.into(),
            details: Vec::new(),
            fixes: vec![fix.into()],
        }
    }

    /// Add one detail line, raising the status if the detail is worse than the check's own verdict.
    ///
    /// A detail never clears a fix line, and never lowers a status: `ok` is what a check says about
    /// the whole, and one `ok` detail cannot undo one `warn`.
    #[must_use]
    pub fn note(mut self, status: Status, detail: impl Into<String>) -> Self {
        let detail = detail.into();
        if detail.is_empty() {
            return self;
        }
        if status > self.status {
            self.status = status;
        }
        self.details.push(detail);
        self
    }

    /// Add a detail *and* the fix it implies, which is how a check with several independent causes
    /// keeps each cause's way out next to it.
    #[must_use]
    pub fn cause(
        mut self,
        status: Status,
        detail: impl Into<String>,
        fix: impl Into<String>,
    ) -> Self {
        self = self.note(status, detail);
        self.fixes.push(fix.into());
        self
    }

    /// Add a fix line.
    #[must_use]
    pub fn fix(mut self, fix: impl Into<String>) -> Self {
        self.fixes.push(fix.into());
        self
    }

    /// Cap the details, and make good on the promise that a non-`ok` check carries a way out.
    #[must_use]
    fn finish(mut self) -> Self {
        if self.details.len() > DETAIL_CAP {
            let hidden = self.details.len() - DETAIL_CAP;
            self.details.truncate(DETAIL_CAP);
            self.details.push(format!("… and {hidden} more"));
        }
        // Two foreign files in one cache root are one thing to do about, not two: repeated fix lines
        // are the noise that makes a report unreadable.
        self.fixes.dedup();
        self.fixes.retain(|fix| !fix.is_empty());
        if self.status != Status::Ok && self.fixes.is_empty() {
            self.fixes
                .push("see the details above; every one of them names what to change".to_owned());
        }
        self
    }
}

/// Everything doctor found.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    /// The version of the binary that ran.
    pub fathom_version: &'static str,
    /// [`SCHEMA_VERSION`].
    pub schema_version: u32,
    /// The workspace, canonicalised.
    pub workspace: PathBuf,
    /// The worst check — the reason for the exit code.
    pub status: Status,
    /// The checks, in the order they were run.
    pub checks: Vec<Check>,
}

impl Report {
    /// Assemble a report: cap each check's details, then take the worst status.
    #[must_use]
    pub fn new(workspace: PathBuf, checks: Vec<Check>) -> Self {
        let checks: Vec<Check> = checks.into_iter().map(Check::finish).collect();
        let status = checks
            .iter()
            .map(|check| check.status)
            .max()
            .unwrap_or(Status::Ok);
        Self {
            fathom_version: env!("CARGO_PKG_VERSION"),
            schema_version: SCHEMA_VERSION,
            workspace,
            status,
            checks,
        }
    }

    /// How many checks got each status, for the footer line.
    #[must_use]
    pub fn tally(&self) -> BTreeMap<&'static str, usize> {
        let mut tally = BTreeMap::new();
        for check in &self.checks {
            *tally.entry(check.status.as_str()).or_default() += 1;
        }
        tally
    }

    /// The process status: the worst check's.
    #[must_use]
    pub fn exit(&self) -> Exit {
        self.status.exit()
    }

    /// The check by name, for a test that wants one check without knowing its index.
    #[must_use]
    pub fn check(&self, name: &str) -> Option<&Check> {
        self.checks.iter().find(|check| check.name == name)
    }

    /// The machine form: the same checks, the same order, one JSON document.
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut text = serde_json::to_string_pretty(self)
            .unwrap_or_else(|_| String::from("{\"schema_version\":0}\n"));
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text
    }

    /// The human form: one line per check, details indented, `fix:` under whatever is not `ok`, and a
    /// footer with the tally and the exit code.
    #[must_use]
    pub fn to_text(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        let _ = writeln!(
            out,
            "fathom {} doctor \u{b7} {}",
            self.fathom_version,
            self.workspace.display()
        );
        for check in &self.checks {
            let _ = writeln!(
                out,
                "{:<4} {:<10} {}",
                check.status.as_str(),
                check.name,
                check.summary
            );
            for detail in &check.details {
                for line in detail.lines() {
                    let _ = writeln!(out, "     \u{b7} {line}");
                }
            }
            for fix in &check.fixes {
                for (index, line) in fix.lines().enumerate() {
                    let _ = writeln!(
                        out,
                        "{} {}",
                        if index == 0 { "  fix:" } else { "      " },
                        line.trim()
                    );
                }
            }
        }
        let tally = self
            .tally()
            .into_iter()
            .map(|(status, count)| format!("{count} {status}"))
            .collect::<Vec<_>>()
            .join(" \u{b7} ");
        let _ = writeln!(out, "{tally} \u{b7} exit {}", self.exit().code());
        out
    }

    /// Render in the form asked for.
    #[must_use]
    pub fn render(&self, json: bool) -> String {
        if json {
            self.to_json()
        } else {
            self.to_text()
        }
    }
}

/// What `doctor` was handed: the booted workspace, the config it resolved, and — when the config
/// refused — the refusal instead of a config.
#[derive(Debug)]
pub struct Input<'a> {
    /// The booted workspace and its two roots.
    pub boot: &'a Boot,
    /// The resolved `fathom.toml`, or the defaults when the file refused to parse (in which case
    /// `config_error` says so — a check must not silently use values the user did not choose).
    pub config: &'a Config,
    /// Why the config was refused, if it was. That is a `fail` check of its own.
    pub config_error: Option<String>,
}

/// Run every check.
#[must_use]
pub fn run(input: &Input) -> Report {
    let boot = input.boot;
    let git = Git::open(&boot.workspace).ok();
    let head = git.as_ref().and_then(|git| git.resolve("HEAD").ok());
    let paths: Vec<String> = git
        .as_ref()
        .and_then(|git| {
            let tree = git.tree_of("HEAD").ok()?;
            git.ls_tree(&tree).ok()
        })
        .map(|entries| entries.into_iter().map(|entry| entry.path).collect())
        .unwrap_or_default();

    let checks = vec![
        workspace(boot, git.as_ref(), head.as_deref()),
        evidence(boot),
        runs(boot),
        workflows(boot),
        cache(boot, head.as_deref()),
        languages(input, &paths),
        config_check(input, &paths),
    ];
    Report::new(boot.workspace.clone(), checks)
}

/// The report for a workspace that never booted: one `fail` check that carries the refusal.
///
/// `doctor` is the command you run when the workspace is wrong, so a refusal cannot be allowed to end
/// it — the message becomes the check, with its own `fix:` line intact.
#[must_use]
pub fn unusable(workspace: &Path, refusal: &str) -> Report {
    let (summary, fix) = match refusal.split_once("fix:") {
        Some((summary, fix)) => (summary.trim().to_owned(), fix.trim().to_owned()),
        None => (refusal.trim().to_owned(), String::new()),
    };
    let summary = summary
        .trim_matches(|c: char| c == '\n' || c.is_whitespace())
        .to_owned();
    let check = if fix.is_empty() {
        Check::fail(
            "workspace",
            summary,
            "point fathom at a checkout it can read; every row is a commit",
        )
    } else {
        Check::fail("workspace", summary, fix)
    };
    Report::new(
        workspace.to_path_buf(),
        vec![
            check,
            Check::warn(
                "evidence",
                "not checked: the workspace could not be opened",
                "fix the workspace line above and run doctor again",
            ),
        ],
    )
}

/// Is this a git work tree, does it have commits, and does `HEAD` resolve.
fn workspace(boot: &Boot, git: Option<&Git>, head: Option<&str>) -> Check {
    let Some(git) = git else {
        return Check::fail(
            "workspace",
            format!("{} is not a git work tree git can read", boot.workspace.display()),
            "every row fathom draws is a commit: pass a checkout, e.g. `fathom doctor ~/src/this-repo`",
        );
    };
    let mut check = Check::ok(
        "workspace",
        format!("git work tree at {}", git.toplevel.display()),
    );
    match head {
        Some(sha) => {
            check = check.note(
                Status::Ok,
                format!("HEAD {}", sha.chars().take(7).collect::<String>()),
            );
        }
        None => {
            check = check.cause(
                Status::Warn,
                "HEAD does not resolve to a commit",
                "an unborn branch has nothing to show: make a commit, or check one out",
            );
        }
    }
    match git.is_empty() {
        Ok(true) => {
            check = check.cause(
                Status::Warn,
                "the repository has no commits yet",
                "a timeline of nothing is what an empty repository has: make a first commit, then \
                 `fathom index`",
            );
        }
        Ok(false) => {
            let count = git
                .run(&["rev-list", "--count", "HEAD"])
                .ok()
                .and_then(|out| String::from_utf8_lossy(&out).trim().parse::<usize>().ok());
            check = check.note(
                Status::Ok,
                format!(
                    "{} commit(s) in HEAD's history",
                    count.map_or_else(|| "some".to_owned(), |count| count.to_string())
                ),
            );
        }
        Err(reason) => {
            check = check.note(Status::Warn, format!("git could not answer: {reason}"));
        }
    }
    match git
        .run(&["symbolic-ref", "--short", "HEAD"])
        .ok()
        .map(|out| String::from_utf8_lossy(&out).trim().to_owned())
    {
        Some(branch) if !branch.is_empty() => {
            check = check.note(Status::Ok, format!("on `{branch}`"))
        }
        _ => check = check.note(Status::Ok, "HEAD is detached or unnamed"),
    }
    check
}

/// The evidence root: does it exist, is it readable, and does it validate.
///
/// The validation is `act-mcp validate-evidence`, run as the sibling binary when this build can find
/// one (next to this executable, or on `PATH`) and otherwise as the same checker through
/// `act_evidence::validate` — the contract crate both CLIs are front ends for. Re-implementing the
/// record checks here would give two checkers free to disagree, which is the failure ADR-0002 §7
/// exists to prevent.
fn evidence(boot: &Boot) -> Check {
    let root = &boot.evidence.path;
    if !boot.evidence.present {
        return Check::warn(
            "evidence",
            format!(
                "no evidence root at {} — nothing has run for this workspace",
                root.display()
            ),
            "run a workflow with act-mcp (it owns that directory), or point --evidence-dir / \
             $EVIDENCE_DIR at the tree that has the records",
        );
    }
    if let Err(reason) = std::fs::read_dir(root) {
        return Check::fail(
            "evidence",
            format!("cannot read {} ({reason})", root.display()),
            "fix the permissions on the evidence root, or point --evidence-dir at a readable tree",
        );
    }
    match validate(root) {
        Validation::Clean { which, summary } => {
            Check::ok("evidence", summary).note(Status::Ok, which)
        }
        Validation::Problems {
            which,
            summary,
            problems,
        } => {
            let mut check = Check::fail(
                "evidence",
                summary,
                "fix or delete the offending line — the checker names file:line — then run \
                 `fathom index` so the rows re-derive",
            )
            .note(Status::Fail, which);
            for problem in problems {
                check = check.note(Status::Fail, problem);
            }
            check
        }
        Validation::Nothing(reason) => Check::warn(
            "evidence",
            format!(
                "{} exists but holds no contract records ({reason})",
                root.display()
            ),
            "nothing has run yet, or the runs published nothing: run a coverage or benchmark job \
             with act-mcp, then `fathom index`",
        ),
        Validation::Unavailable(reason) => Check::warn(
            "evidence",
            format!("{} exists; it was not validated ({reason})", root.display()),
            "install act-mcp, or build it in this workspace (`cargo build -p act-mcp`), and run \
             `act-mcp validate-evidence` on this tree",
        ),
    }
}

/// What the contract checker said about a tree.
#[derive(Debug, Clone)]
pub enum Validation {
    /// The tree is clean.
    Clean {
        /// Which checker answered, spelled out.
        which: String,
        /// The checker's own summary line.
        summary: String,
    },
    /// The tree has findings.
    Problems {
        /// Which checker answered, spelled out.
        which: String,
        /// The checker's own summary line.
        summary: String,
        /// The findings, `path:line: message`, as the checker printed them.
        problems: Vec<String>,
    },
    /// There was nothing to check — which for an evidence root means "no records", the common case
    /// in a repository that has never been measured.
    Nothing(String),
    /// The checker itself could not run (no binary, and the in-process checker refused too).
    Unavailable(String),
}

/// Which checker answered, spelled out in every verdict: a substituted checker is fine, an
/// unlabelled one is a bug report waiting to happen.
const IN_PROCESS: &str = "validated by act-evidence inside fathom (no act-mcp binary found)";

/// Run the contract checker over one tree.
#[must_use]
pub fn validate(root: &Path) -> Validation {
    if let Some(binary) = act_mcp_binary() {
        if let Ok(output) = Command::new(&binary)
            .arg("validate-evidence")
            .arg(root)
            .output()
        {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            // Findings are `path:line: message` on stderr; act-mcp's tracing shares that stream, so
            // only the lines that name a file are findings.
            let problems: Vec<String> = String::from_utf8_lossy(&output.stderr)
                .lines()
                .filter(|line| line.contains(".json"))
                .map(str::to_owned)
                .collect();
            let which = format!(
                "validated by {} (exit {})",
                binary.display(),
                output.status.code().unwrap_or(2)
            );
            return match output.status.code().unwrap_or(2) {
                0 => Validation::Clean {
                    which,
                    summary: if stdout.is_empty() {
                        "evidence validates".to_owned()
                    } else {
                        stdout
                    },
                },
                1 => Validation::Problems {
                    which,
                    summary: if stdout.is_empty() {
                        format!("validate-evidence found {} problem(s)", problems.len())
                    } else {
                        stdout
                    },
                    problems,
                },
                // 2 is validate-evidence's "the command could not run / nothing to validate" — for
                // an evidence root that has no records, which is a warning about the repository, not
                // about the tool.
                _ => Validation::Nothing(if stdout.is_empty() {
                    "nothing to validate".to_owned()
                } else {
                    one_line(&stdout)
                }),
            };
        }
    }
    // The same checker, in-process: `act-mcp validate-evidence` is a front end over exactly this call
    // in `act-evidence`, so the verdict is the one CI gets, and a read-only box with no act-mcp
    // installed still gets an answer instead of a shrug.
    let options = act_evidence::validate::ValidateOptions {
        paths: vec![root.to_path_buf()],
        schema: None,
        meta_schema: None,
    };
    match act_evidence::validate::validate_tree(&options) {
        Ok(report) if report.is_ok() => Validation::Clean {
            which: IN_PROCESS.to_owned(),
            summary: report.summary(),
        },
        Ok(report) => Validation::Problems {
            which: IN_PROCESS.to_owned(),
            summary: report.summary(),
            problems: report.problems.iter().map(ToString::to_string).collect(),
        },
        Err(act_evidence::validate::ValidateError::NothingToValidate { .. }) => {
            Validation::Nothing("no benchmark/ or coverage/ JSONL under it".to_owned())
        }
        Err(other) => Validation::Unavailable(other.to_string()),
    }
}

/// Where the sibling `act-mcp` binary is, if there is one: next to this executable first (the normal
/// case in a workspace build), then on `PATH`.
#[must_use]
pub fn act_mcp_binary() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for name in ["act-mcp", "act-mcp.exe"] {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    // `PATH`, the installed case: checked by running `--version`, which is cheap and rules out a
    // directory or a stale non-executable file of that name.
    Command::new("act-mcp")
        .arg("--version")
        .output()
        .is_ok()
        .then(|| PathBuf::from("act-mcp"))
}

/// Runs that happened and published nothing — the one question the cross-commit contract cannot
/// answer, and ADR-0008 §5.5's permission to say it **advisory**.
fn runs(boot: &Boot) -> Check {
    let directory = boot.evidence.path.join("runs");
    let Ok(entries) = std::fs::read_dir(&directory) else {
        return Check::ok(
            "runs",
            "no runs/ directory (advisory: nothing has been run here)",
        );
    };
    let mut directories = 0_usize;
    let mut notes: Vec<String> = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        if !entry.path().is_dir() {
            continue;
        }
        directories += 1;
        let name: String = entry
            .file_name()
            .to_string_lossy()
            .chars()
            .take(24)
            .collect();
        let Ok(text) = std::fs::read_to_string(entry.path().join("meta.json")) else {
            notes.push(format!(
                "{name}: no readable meta.json — act-mcp died mid-run, so there is nothing to read"
            ));
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            notes.push(format!("{name}: meta.json does not parse"));
            continue;
        };
        let status = value
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("?")
            .to_owned();
        let published = value
            .get("evidence")
            .and_then(serde_json::Value::as_array)
            .map_or(0, Vec::len);
        // One line per run, not one per observation: a run that failed *and* published nothing is one
        // fact, and repeating it twice is how a nine-run directory turns into twenty lines.
        if published == 0 {
            notes.push(format!(
                "{name}: status `{status}`, published no evidence — the run existed and the columns \
                 stay unknown"
            ));
        } else if status != "success" {
            notes.push(format!("{name}: status `{status}` (advisory)"));
        }
    }
    let mut check = Check::ok(
        "runs",
        format!(
            "{directories} run director{} under {} (advisory, per ADR-0008 §5.5)",
            if directories == 1 { "y" } else { "ies" },
            directory.display()
        ),
    );
    for note in notes {
        check = check.note(Status::Warn, note);
    }
    if check.status != Status::Ok {
        check = check.fix(
            "advisory only: read runs/<run_id>/act.jsonl for what happened. Fathom's columns come \
             from the JSONL a run published, and this one published none",
        );
    }
    check
}

/// Is there anything to run.
///
/// The discovery `list_tasks` would do, from the files on disk: `.github/workflows` parsed locally
/// (no server, no act, no network — fathom may have none of those) against the same marker lists
/// `list_tasks` uses for its advisory `category_hint`. "No evidence" is usually "nothing to run", and
/// this is the line that says so.
fn workflows(boot: &Boot) -> Check {
    let root = boot
        .git_root
        .clone()
        .unwrap_or_else(|| boot.workspace.clone());
    let directory = Discovery::default_dir(&root);
    if !directory.is_dir() {
        return Check::warn(
            "workflows",
            format!(
                "no {} — there is nothing here for act-mcp to run",
                directory.display()
            ),
            "add a workflow that publishes coverage or benchmark evidence, e.g. \
             .github/workflows/coverage.yml running `cargo llvm-cov` or `hyperfine`, then run it \
             with act-mcp",
        );
    }
    let discovery = Discovery::scan(&directory);
    if discovery.files == 0 {
        return Check::warn(
            "workflows",
            format!("{} exists but holds no workflow files", directory.display()),
            "add at least one .yml/.yaml workflow that produces coverage or benchmark evidence",
        );
    }
    let mut check = Check::ok(
        "workflows",
        format!(
            "{}: {} workflow(s), {} job(s); coverage {}, benchmark {}",
            directory.display(),
            discovery.files,
            discovery.jobs,
            if discovery.coverage {
                "found"
            } else {
                "not found"
            },
            if discovery.benchmark {
                "found"
            } else {
                "not found"
            }
        ),
    );
    for note in &discovery.notes {
        check = check.note(Status::Warn, note.clone());
    }
    if !discovery.coverage && !discovery.benchmark {
        check = check.cause(
            Status::Warn,
            "no workflow name, job name or step command mentions coverage or a benchmark tool",
            "fathom has no numbers until something produces them: name a job so discovery can see \
             it (`name: Coverage`, `name: Benchmarks`) and make it publish lcov.info or a \
             benchmark JSONL",
        );
    }
    check
}

/// The cache root and the store inside it.
///
/// Everything ADR-0006 §10 promises `doctor` can answer: does the root exist, is it writable, does
/// the store's version match this build, how many rows and blocks, when did it last run and how much
/// of that run the metric cache saved, who (if anyone) is indexing right now, and which files under
/// the root are not Fathom's (§4.2 — the anti-mystery list).
fn cache(boot: &Boot, head: Option<&str>) -> Check {
    let root = &boot.cache.path;
    if !root.is_dir() {
        return Check::warn(
            "cache",
            format!("no cache root at {}", root.display()),
            format!("run `fathom index {}`", boot.workspace.display()),
        );
    }
    let mut check = Check::ok(
        "cache",
        format!("cache root {} ({})", root.display(), boot.cache.state()),
    );
    if !boot.cache.writable {
        for refusal in &boot.cache.write_refusals {
            check = check.note(
                Status::Warn,
                format!("cannot write the cache root: {refusal}"),
            );
        }
        check = check.fix(
            "a reader can read a store someone else wrote, but `fathom index` cannot: pass \
             --cache-dir / $FATHOM_DIR on a filesystem you can write, or make this one writable \
             (ADR-0006 §10.1)",
        );
    }
    if !root.join(crate::boot::LAYOUT_FILE_NAME).is_file() {
        check = check.cause(
            Status::Warn,
            "no LAYOUT.md under the cache root — the on-disk contract of ADR-0006 §3.2 is missing",
            "`fathom index` writes it; if the root is read-only, see the writability line above",
        );
    }
    // Foreign entries (§4.2): everything under the root that this program did not write, listed and
    // left alone.
    for entry in std::fs::read_dir(root).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if ["LAYOUT.md", "fathom.lock", "index"].contains(&name.as_str()) {
            continue;
        }
        check = check.cause(
            Status::Warn,
            format!("`{name}` is not written by Fathom; Fathom neither reads nor deletes it"),
            "nothing, unless you know it is dead: a shared cache root is legal (ADR-0006 §4.2)",
        );
    }
    if let Some(lock) = store::lock::read_body(root) {
        // The body is advisory and the file is never unlinked (§9 L3), so a body whose pid is gone on
        // this host is a trace of a finished or killed run, not a live one. Saying "an index is
        // running" about a dead pid is exactly the mystery §4.2's list exists to remove.
        if lock.names_dead_local_pid() {
            check = check.note(
                Status::Ok,
                format!(
                    "fathom.lock's last holder (pid {} on {}, phase `{}`, {} commit(s)) is gone: an \
                     advisory trace of a run that ended, not a live index",
                    lock.pid, lock.hostname, lock.phase, lock.commits_done
                ),
            );
        } else {
            check = check.cause(
                Status::Warn,
                format!(
                    "fathom.lock says an index is running here right now: pid {} on {}, phase `{}`, \
                     {} commit(s) done",
                    lock.pid, lock.hostname, lock.phase, lock.commits_done
                ),
                format!(
                    "let it finish: a second writer would only be told `index_busy`, and the store is \
                     coherent while it runs (§9 L1–L4). The body is advisory and is never cleared \
                     (§9 L3), so if `ps -p {pid}` finds no fathom on {}, nothing is running and there \
                     is nothing to do",
                    lock.hostname,
                    pid = lock.pid
                ),
            );
        }
    }
    match Store::open(root) {
        Ok(None) => check.cause(
            Status::Warn,
            "no store yet: nothing has been indexed in this cache root",
            "`fathom index` builds it; every column stays unknown until it does",
        ),
        Err(reason) => check.cause(
            Status::Fail,
            format!(
                "the store refused to open: {}",
                one_line(&reason.to_string())
            ),
            reason
                .to_string()
                .lines()
                .find(|line| line.contains("fix:"))
                .map(|line| line.replace("fix:", "").trim().to_owned())
                .unwrap_or_else(|| {
                    "the store is disposable: rm -rf the index and index again".to_owned()
                }),
        ),
        Ok(Some(store)) => store_notes(check, &store, head),
    }
}

/// What a store that opened says about itself.
fn store_notes(mut check: Check, store: &Store, head: Option<&str>) -> Check {
    let counts = store.counts();
    check = check
        .note(
            Status::Ok,
            format!(
                "store layout {} schema {} (writer {} {}, config digest {})",
                store.meta.layout_version,
                store.meta.schema_version,
                store.meta.writer.name,
                store.meta.writer.version,
                short(store.meta.config.digest.as_str())
            ),
        )
        .note(
            Status::Ok,
            format!(
                "{} row(s), {} block(s), {} + {} bytes on disk",
                counts.rows, counts.blocks, counts.row_bytes, counts.block_bytes
            ),
        );
    if counts.temporaries > 0 {
        check = check.note(
            Status::Warn,
            format!(
                "{} dot-temporar(y/ies) under rows//blocks/ — an interrupted publish's footprint; \
                 the next index re-publishes over them",
                counts.temporaries
            ),
        );
    }
    if counts.foreign > 0 {
        check = check.note(
            Status::Warn,
            format!(
                "{} entr(y/ies) under rows//blocks/ that are neither rows nor blocks \
                 (ADR-0006 §10.5 names the path to delete)",
                counts.foreign
            ),
        );
    }
    check = damaged_rows(store, check);
    let Some(book) = Runbook::read(&store.index) else {
        return check.cause(
            Status::Warn,
            "no run.json: this store has no last run to report (§10.2's counters are gone)",
            "nothing to fix if the rows are right; the next `fathom index` writes it again",
        );
    };
    let run = &book.last;
    let seen = format!("{} commit(s) seen", run.commits_seen);
    let finished = human_age(crate::store::now_utc() - run.finished_utc);
    check = check.note(
        Status::Ok,
        format!(
            "last index {} ({}, {}) {} \u{b7} {} row(s) written, cache hit ratio {}% \u{b7} {finished} \
             ago",
            run.kind,
            run.trigger,
            run.status,
            seen,
            run.rows_written,
            run.hit_ratio_percent(),
        ),
    );
    if run.status != "ok" {
        check = check.cause(
            Status::Warn,
            format!(
                "the last index run reported `{}`{}",
                run.status,
                run.error
                    .as_deref()
                    .map_or_else(String::new, |why| format!(": {why}"))
            ),
            "the store is still usable (§6.1 publishes whole files); run `fathom index` again",
        );
    }
    if run.kind == "warm" && run.cache_lookups > 0 && run.cache_hits == 0 {
        check = check.cause(
            Status::Warn,
            "a warm run hit the metric cache 0 times",
            "the config digests keep moving: diff fathom.toml between the two runs (ADR-0006 §7.3 \
             says which key invalidates which section)",
        );
    }
    if let Some(head) = head {
        if !store_has_row(store, head) {
            check = check.cause(
                Status::Warn,
                format!("HEAD {} has no row: the store is behind the checkout", short(head)),
                "`fathom index` catches up; the timeline will show HEAD as an unknown row until it does",
            );
        }
    }
    check
}

/// Does the store hold a row for one commit — the cheapest staleness test there is.
fn store_has_row(store: &Store, sha: &str) -> bool {
    store
        .rows_dir()
        .join(store::keys::bucket(sha))
        .join(format!("{sha}.json"))
        .is_file()
}

/// Count rows that will not parse, or whose `commit` is not their filename (ADR-0006 §10.5: `doctor`
/// counts them as `warn` and names the path; the index re-derives them).
fn damaged_rows(store: &Store, mut check: Check) -> Check {
    use crate::store::row::Row;
    let mut damaged = Vec::new();
    for path in store::walk(&store.rows_dir()).iter().filter(|path| {
        path.file_name()
            .is_some_and(|name| name.to_string_lossy().ends_with(".json"))
    }) {
        let named = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        match store.read_json::<Row>(path) {
            None => damaged.push(path.display().to_string()),
            Some(row) if row.commit != named => damaged.push(format!(
                "{} (says it is {})",
                path.display(),
                short(&row.commit)
            )),
            Some(_) => {}
        }
    }
    if damaged.is_empty() {
        return check;
    }
    check = check.cause(
        Status::Warn,
        format!(
            "{} row file(s) will not parse or do not match their filename",
            damaged.len()
        ),
        "the next index re-derives them; delete the named files to make the warning go away now",
    );
    for path in damaged {
        check = check.note(Status::Warn, path);
    }
    check
}

/// Which paths were not analysed, and why.
///
/// The ladder is the MI analyser's own ([`mi::Analyzer::analyse`]'s first four rungs) run over
/// `git ls-tree` rather than the working tree: a row describes a commit, and a checkout with local
/// edits would have the doctor diagnosing files that are in no commit (ADR-0006 §4.3).
fn languages(input: &Input, paths: &[String]) -> Check {
    let config = input.config;
    let Ok(analyzer) = mi::Analyzer::from_config(config) else {
        return Check::fail(
            "languages",
            format!(
                "the MI analyser refused to start: {}",
                one_line(&input.config_error.clone().unwrap_or_default())
            ),
            "fix `languages.enabled` / `index.file_size_cap_mib`: ADR-0007 §6.5 lists the \
             languages this build has a front end for",
        );
    };
    if paths.is_empty() {
        return Check::warn(
            "languages",
            "no tree to list: there are no commits, or git could not read HEAD",
            "fix the workspace check above first",
        );
    }
    let definition = analyzer.definition();
    let classifier: &Classifier = analyzer.classifier();
    let enabled = analyzer.enabled();
    let mut counted: BTreeMap<String, usize> = BTreeMap::new();
    let mut analysed = 0_usize;
    let mut disabled: Vec<String> = Vec::new();
    let mut off_paths = 0_usize;
    let mut count = |reason: String| {
        *counted.entry(reason).or_default() += 1;
    };
    for path in paths {
        let scope = classifier.classify(path);
        if scope == Scope::Excluded {
            count("excluded (paths.exclude, or outside paths.source)".to_owned());
            continue;
        }
        if scope == Scope::Noise {
            count(
                if classifier.tests_are_excluded() {
                    "noise (paths.noise, or a test file while paths.include_tests = false)"
                } else {
                    "noise (paths.noise)"
                }
                .to_owned(),
            );
            continue;
        }
        let Some(language) = definition.language_for_path(path) else {
            // Not a source extension at all, and ADR-0007 §6.5 is explicit that this is not an
            // unanalysed-path caveat: every Markdown file in the repository would otherwise be one.
            count("no source extension (not counted as unanalysed)".to_owned());
            continue;
        };
        if !enabled.contains(language) {
            count(format!("`{language}` is not enabled"));
            off_paths += 1;
            if !disabled.contains(&language.to_owned()) {
                disabled.push(language.to_owned());
            }
            continue;
        }
        analysed += 1;
    }
    let mut check = Check::ok(
        "languages",
        format!(
            "{} of {} path(s) in HEAD are in scope for the MI; {} language(s) enabled ({})",
            analysed,
            paths.len(),
            enabled.len(),
            enabled
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    );
    for (reason, count) in &counted {
        check = check.note(Status::Ok, format!("{count} \u{b7} {reason}"));
    }
    if classifier.tests_are_excluded() {
        check = check.note(
            Status::Ok,
            "paths.include_tests = false, so test files are `noise` and carry no MI — ADR-0007 §7.2 \
             says a default that hides half the repository must announce itself, which is what this \
             line is",
        );
    }
    // A language the config asked for that this build cannot lex is the config's mistake. A language
    // this build *can* lex that the config left out is the user's choice, and only a `warn` because it
    // is one key away from being measured.
    let unknown: Vec<&String> = config
        .languages
        .enabled
        .iter()
        .filter(|want| !definition.languages.contains_key(want.as_str()))
        .collect();
    for language in &unknown {
        check = check.note(
            Status::Warn,
            format!(
                "`languages.enabled` names `{language}`, which schema/mi-definition.json does not \
                 (ADR-0007 §6.5's set is closed)",
            ),
        );
    }
    let off: Vec<&String> = disabled
        .iter()
        .filter(|language| definition.languages.contains_key(language.as_str()))
        .collect();
    if !off.is_empty() {
        // In scope, readable by this build, and not measured: the MI is smaller than the user thinks,
        // which is a `warn` rather than a footnote — and one key and one rebuild from fixed.
        return check.cause(
            Status::Warn,
            format!(
                "{off_paths} path(s) in HEAD are source this build can read and `languages.enabled` \
                 leaves out ({})",
                off.iter()
                    .map(|language| format!("`{language}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            format!(
                "add {} to `languages.enabled` in fathom.toml, then `fathom index --rebuild` — the \
                 MI blocks are keyed on that key, so every block re-derives",
                off.iter()
                    .map(|language| language.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        );
    }
    if !unknown.is_empty() {
        return check.fix(
            "remove the unknown languages from `languages.enabled`; this build has no front end for \
             them and will never measure them",
        );
    }
    check
}

/// `fathom.toml`: which file, and the keys and globs that do nothing.
///
/// An unknown key is already a refusal at boot (`deny_unknown_fields`), so if doctor ran at all the
/// file parsed — unless it did not, in which case [`Input::config_error`] carries the refusal and
/// this check says which key is unknown instead of silently keeping the defaults it was handed.
fn config_check(input: &Input, paths: &[String]) -> Check {
    let config = input.config;
    if let Some(reason) = &input.config_error {
        return Check::fail(
            "config",
            format!("fathom.toml refused this build: {}", one_line(reason)),
            "fix the key the message names (ADR-0006 §7.2 lists the keys that exist); every other \
             check on this page ran with the built-in defaults, not with your file",
        );
    }
    let mut check = Check::ok(
        "config",
        match &config.path {
            Some(path) if config.from_file() => format!("read {}", path.display()),
            _ => "no fathom.toml found; every value is this build's default (ADR-0006 §7.1)"
                .to_owned(),
        },
    );
    if !config.from_file() {
        check = check.note(
            Status::Ok,
            "the search looked in the workspace and every directory above it, then the git root",
        );
    }
    if !config.from_file() {
        // The built-in noise and exclude lists are general (`.venv/**`, `vendor/**`, `.git/**`) and
        // match nothing in most repositories; auditing the defaults would make every workspace's
        // `config` check a `warn` about keys nobody typed. Only a file's own globs are audited.
        return check.note(
            Status::Ok,
            "globs were not audited: there is no fathom.toml, and the built-in defaults are general \
             by design (ADR-0006 §7.2)",
        );
    }
    if paths.is_empty() {
        return check.note(
            Status::Warn,
            "globs were not checked: there is no tree to match them against",
        );
    }
    // A glob that matches nothing in this repository is a glob whose owner thinks it is doing
    // something. ADR-0006 §7.2 is the list, and each dead one is named with the key to delete it from.
    let mut dead: Vec<String> = Vec::new();
    for (key, globs) in [
        ("paths.source", &config.paths.source),
        ("paths.noise", &config.paths.noise),
        ("paths.exclude", &config.paths.exclude),
    ] {
        for glob in globs {
            match crate::classify::Glob::compile(glob) {
                Ok(compiled) => {
                    if !paths.iter().any(|path| compiled.matches(path)) {
                        dead.push(format!("`{key}` entry `{glob}` matches no path in HEAD"));
                    }
                }
                Err(reason) => {
                    dead.push(format!("`{key}` entry `{glob}` will not compile: {reason}"))
                }
            }
        }
    }
    for dead in &dead {
        check = check.note(Status::Warn, dead.clone());
    }
    if !dead.is_empty() {
        return check.fix(
            "delete or fix the globs that match nothing: a noise or exclude entry that stopped \
             matching is a metric that quietly changed meaning (ADR-0006 §7.2)",
        );
    }
    check.note(
        Status::Ok,
        format!(
            "{} source / {} noise / {} exclude glob(s), all matching at least one path",
            config.paths.source.len(),
            config.paths.noise.len(),
            config.paths.exclude.len()
        ),
    )
}

/// First line, for embedding a message that carries a `fix:` of its own on the second line.
fn one_line(text: &str) -> String {
    text.lines().next().unwrap_or(text).trim().to_owned()
}

/// Seven hex, for a digest or a SHA inside a sentence.
fn short(text: &str) -> String {
    text.chars().take(7).collect()
}

/// A duration as `doctor` spells it: the timeline's `age` ladder with seconds at the bottom, because
/// a last index run that finished nine seconds ago should say so.
fn human_age(seconds: i64) -> String {
    let seconds = seconds.max(0);
    if seconds < 60 {
        return format!("{seconds}s");
    }
    crate::tui::entry::age(seconds)
}
