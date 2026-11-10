//! `act-mcp validate-evidence`: the checker that keeps the contract from rotting.
//!
//! ADR-0002 §7 makes this command the strict half of the compatibility policy: a
//! *writer* may only emit what it was built to emit, and a *reader* must refuse what it
//! cannot understand — "unknown field, unknown `kind`, unknown `schema_version` are all
//! hard errors". This module is that reader, aimed at a tree instead of at one run, so
//! CI can run it over a workspace's whole `.evidence` and say "this data is what the
//! ADR says it is".
//!
//! It mirrors [`scripts/validate_evidence.py`](../../../scripts/validate_evidence.py)
//! deliberately: same discovery rules, same byte rules, same `path:line: message`
//! output, same `--stats`, same exit-code meanings (0 clean, 1 problems, 2 the command
//! was asked about nothing or could not load a schema). Two checkers that disagree is
//! worse than one, so `tests/evidence_contract.rs` runs both over the fixture tree and
//! asserts they reach the same verdicts.
//!
//! ## What is checked, and what is not
//!
//! * `benchmark/**.jsonl` and `coverage/**.jsonl` are record files: byte rules, one
//!   strict JSON object per line, and the pinned record schema.
//! * `runs/<run_id>/meta.json` is checked against `schema/run-meta.schema.json`.
//! * `runs/<run_id>/act.jsonl` and everything under `incoming/` are **skipped**:
//!   ADR-0002 §1.1 puts them outside the cross-commit contract (`act.jsonl` is act's own
//!   format and carries no `schema_version`; `incoming/` is scratch that may vanish at
//!   any time), and a scanner walks `benchmark/` and `coverage/` only.
//! * Dot-files and `*.tmp` are skipped: they are in-flight temp files (§1.1, §6.2).
//! * Not checked, on purpose: `hit <= found` and "does this total match the sum of the
//!   file records". The schema cannot express the first (ADR-0002 §4.2 calls it an
//!   extractor rule) and the second is an aggregation, which §4.4 makes a scanner's job,
//!   not the contract's.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use jsonschema::Validator;
use serde_json::Value;
use thiserror::Error;

use crate::evidence::records::{Kind, SCHEMA_VERSION};
use crate::evidence::schema::{self, SchemaProblem};

/// The subtrees ADR-0002 §1.1 keeps out of the cross-commit contract, so their JSONL is
/// not validated as evidence records.
pub const NON_CONTRACT_DIRS: [&str; 2] = ["runs", "incoming"];

/// A problem with one file, printed the way a compiler prints a diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// The file, as the caller named it.
    pub path: PathBuf,
    /// The 1-based line, or `None` for a whole-file complaint (a torn write, a BOM).
    pub line: Option<usize>,
    /// What is wrong, one sentence, no location in it.
    pub message: String,
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.line {
            Some(line) => write!(f, "{}:{line}: {}", self.path.display(), self.message),
            None => write!(f, "{}: {}", self.path.display(), self.message),
        }
    }
}

/// What one kind was seen how many times under which commit — `--stats`.
///
/// Keyed by `(kind, commit)` and printed sorted, which is what makes the output
/// diffable between runs (the point of printing it at all).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Stats(BTreeMap<(String, String), usize>);

impl Stats {
    /// Count one record.
    pub fn count(&mut self, kind: &str, commit: &str) {
        *self
            .0
            .entry((kind.to_owned(), commit.to_owned()))
            .or_insert(0) += 1;
    }

    /// Total records counted.
    #[must_use]
    pub fn total(&self) -> usize {
        self.0.values().sum()
    }

    /// The `(kind, commit, count)` triples, sorted.
    #[must_use]
    pub fn rows(&self) -> Vec<(&str, &str, usize)> {
        self.0
            .iter()
            .map(|((kind, commit), count)| (kind.as_str(), commit.as_str(), *count))
            .collect()
    }

    /// The `--stats` body: one `kind<TAB>commit<TAB>count` line per triple, matching
    /// `scripts/validate_evidence.py --stats`.
    #[must_use]
    pub fn to_string_pretty(&self) -> String {
        let mut out = String::new();
        for (kind, commit, count) in self.rows() {
            out.push_str(&format!("{kind}\t{commit}\t{count}\n"));
        }
        out
    }
}

/// What a validation run found.
#[derive(Debug, Default, Clone)]
pub struct Report {
    /// Evidence JSONL files read.
    pub record_files: usize,
    /// `meta.json` files read.
    pub meta_files: usize,
    /// Records that validated cleanly.
    pub records: usize,
    /// Everything wrong, in the order found (file order, then line order).
    pub problems: Vec<Problem>,
    /// Per-kind, per-commit counts of the clean records.
    pub stats: Stats,
}

impl Report {
    /// Whether the tree is clean.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.problems.is_empty()
    }

    /// The summary line, printed on stdout so `... && echo ok` reads sensibly.
    #[must_use]
    pub fn summary(&self) -> String {
        let files = self.record_files + self.meta_files;
        if self.is_ok() {
            format!("OK {files} file(s), {} record(s) valid", self.stats.total())
        } else {
            format!("FAIL {} problem(s) in {files} file(s)", self.problems.len())
        }
    }

    /// The exit status `validate-evidence` should return for this report.
    ///
    /// 0 clean, 1 findings: the same contract `scripts/validate_evidence.py` has,
    /// because a CI step should not care which of the two checkers ran. (2 is reserved
    /// for "the command could not run", which is [`ValidateError`], not a report.)
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        if self.problems.is_empty() {
            0
        } else {
            1
        }
    }
}

/// Why validation could not run at all (as opposed to running and finding problems).
#[allow(missing_docs, reason = "the #[error] string documents each variant")]
#[derive(Debug, Error)]
pub enum ValidateError {
    #[error("nothing to validate under {} — pass a path, or create the evidence tree first", .paths.iter().map(|path| path.display().to_string()).collect::<Vec<_>>().join(", "))]
    NothingToValidate { paths: Vec<PathBuf> },

    #[error("could not use the schema at {path}: {reason}")]
    Schema { path: String, reason: String },

    #[error("could not read {path}: {reason}")]
    Read { path: PathBuf, reason: String },
}

/// What to validate, and with which schemas.
#[derive(Debug, Clone, Default)]
pub struct ValidateOptions<'a> {
    /// Files and directories to check. A directory is walked; a dot-file or `*.tmp` is
    /// skipped; `runs/` and `incoming/` subtrees are skipped for record files.
    pub paths: Vec<PathBuf>,
    /// Validate against this record schema instead of the embedded one.
    pub schema: Option<&'a Path>,
    /// Validate `meta.json` against this manifest schema instead of the embedded one.
    pub meta_schema: Option<&'a Path>,
}

/// Validate everything named in `options`.
///
/// # Errors
///
/// [`ValidateError`] when the command itself could not do its job (nothing matched the
/// paths given, a schema was unreadable). Findings about the data are *not* errors
/// here: they are in [`Report::problems`].
pub fn validate_tree(options: &ValidateOptions<'_>) -> Result<Report, ValidateError> {
    let record_validator = validator(
        options.schema,
        schema::evidence_validator,
        "schema/evidence.schema.json",
    )?;
    let meta_validator = validator(
        options.meta_schema,
        schema::run_meta_validator,
        "schema/run-meta.schema.json",
    )?;

    let (record_files, meta_files) = discover(&options.paths);
    if record_files.is_empty() && meta_files.is_empty() {
        return Err(ValidateError::NothingToValidate {
            paths: options.paths.clone(),
        });
    }

    let mut report = Report::default();
    for path in record_files {
        check_record_file(&path, &record_validator, &mut report);
    }
    for path in meta_files {
        check_meta_file(&path, &meta_validator, &mut report);
    }
    Ok(report)
}

/// Resolve a validator: an explicit `--schema` path if given, else the embedded copy.
fn validator(
    override_path: Option<&Path>,
    embedded: fn() -> Result<&'static Validator, schema::SchemaError>,
    label: &str,
) -> Result<std::borrow::Cow<'static, Validator>, ValidateError> {
    match override_path {
        None => embedded()
            .map(std::borrow::Cow::Borrowed)
            .map_err(|reason| ValidateError::Schema {
                path: label.to_owned(),
                reason: reason.to_string(),
            }),
        Some(path) => {
            let text = std::fs::read_to_string(path).map_err(|reason| ValidateError::Schema {
                path: path.display().to_string(),
                reason: reason.to_string(),
            })?;
            schema::compile(&text)
                .map(std::borrow::Cow::Owned)
                .map_err(|reason| ValidateError::Schema {
                    path: path.display().to_string(),
                    reason: reason.to_string(),
                })
        }
    }
}

// ---------------------------------------------------------------------------
// discovery
// ---------------------------------------------------------------------------

/// Pick the files to check out of `paths`, applying the skip rules of ADR-0002 §1.1.
///
/// Returns `(record files, meta.json files)`, each sorted so a checker's output order
/// is reproducible.
fn discover(paths: &[PathBuf]) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut records = Vec::new();
    let mut metas = Vec::new();
    for start in paths {
        let candidates: Vec<PathBuf> = match start.symlink_metadata() {
            Ok(metadata) if metadata.is_dir() => walk(start),
            Ok(_) => vec![start.clone()],
            Err(_) => Vec::new(),
        };
        for candidate in candidates {
            let name = candidate
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            // §1.1: readers MUST skip dot-files and anything that is not a `.jsonl` or
            // a `meta.json`; the in-flight temp file is a dot-file ending in `.tmp`.
            if name.starts_with('.') || name.ends_with(".tmp") || name.is_empty() {
                continue;
            }
            if name == "meta.json" {
                metas.push(candidate);
                continue;
            }
            if name.ends_with(".jsonl") && !inside_non_contract_dir(&candidate) {
                records.push(candidate);
            }
        }
    }
    records.sort();
    metas.sort();
    (records, metas)
}

/// Every file under `root`, symlinks not followed (a symlinked subtree is exactly the
/// thing ADR-0002 §6.3 refuses to write through, so it is not a thing to read either).
fn walk(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(metadata) = entry.path().symlink_metadata() else {
                continue;
            };
            if metadata.is_dir() {
                pending.push(path);
            } else if metadata.is_file() {
                found.push(path);
            }
        }
    }
    found
}

/// Whether `path` sits under a `runs/` or `incoming/` directory.
fn inside_non_contract_dir(path: &Path) -> bool {
    // Only a *parent* directory component counts, so a file literally named
    // `runs.jsonl` is still a record file.
    path.parent().is_some_and(|parent| {
        parent.components().any(|component| {
            component.as_os_str().to_string_lossy() == "runs"
                || component.as_os_str().to_string_lossy() == "incoming"
        })
    })
}

// ---------------------------------------------------------------------------
// record files
// ---------------------------------------------------------------------------

/// Check one evidence JSONL file: byte rules first, then one schema check per line,
/// then the rules the layout imposes that a per-line schema cannot see.
fn check_record_file(path: &Path, validator: &Validator, report: &mut Report) {
    report.record_files += 1;
    let raw = match std::fs::read(path) {
        Ok(raw) => raw,
        Err(reason) => {
            report.problems.push(Problem {
                path: path.to_path_buf(),
                line: None,
                message: format!("could not be read: {reason}"),
            });
            return;
        }
    };
    for message in byte_rules(&raw) {
        report.problems.push(Problem {
            path: path.to_path_buf(),
            line: None,
            message: message.to_owned(),
        });
    }
    let Ok(text) = std::str::from_utf8(&raw) else {
        // `byte_rules` already said so; the lines cannot be checked further.
        return;
    };

    let mut seen: IdentitySets = IdentitySets::default();
    for (index, chunk) in text.split_inclusive('\n').enumerate() {
        let line = index + 1;
        let body = chunk.strip_suffix('\n').unwrap_or(chunk);
        if body.is_empty() {
            report.problems.push(Problem {
                path: path.to_path_buf(),
                line: Some(line),
                message: "blank line".to_owned(),
            });
            continue;
        }
        let value: Value = match serde_json::from_str(body) {
            Ok(value) => value,
            Err(reason) => {
                report.problems.push(Problem {
                    path: path.to_path_buf(),
                    line: Some(line),
                    message: format!("not a JSON value: {reason}"),
                });
                continue;
            }
        };
        let object = match value.as_object() {
            Some(object) => object,
            None => {
                report.problems.push(Problem {
                    path: path.to_path_buf(),
                    line: Some(line),
                    message: "line is not a JSON object".to_owned(),
                });
                continue;
            }
        };

        // §7: a version this build does not know is `unsupported_schema_version`, and it
        // is reported on its own rather than as a cascade of "every field is unknown".
        if let Some(found) = object.get("schema_version").and_then(Value::as_i64) {
            if i64::from(SCHEMA_VERSION) != found {
                report.problems.push(Problem {
                    path: path.to_path_buf(),
                    line: Some(line),
                    message: format!(
                        "unsupported_schema_version: record carries schema_version {found}, and this build knows only {SCHEMA_VERSION} (ADR-0002 §7)"
                    ),
                });
                continue;
            }
        }

        let before = report.problems.len();
        for SchemaProblem { path: at, message } in schema::problems(validator, &value) {
            report.problems.push(Problem {
                path: path.to_path_buf(),
                line: Some(line),
                message: format!("{at}: {message}"),
            });
        }
        for message in layout_rules(path, object) {
            report.problems.push(Problem {
                path: path.to_path_buf(),
                line: Some(line),
                message,
            });
        }
        if report.problems.len() == before {
            let kind = object
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("<unknown>");
            let commit = object
                .get("commit")
                .and_then(Value::as_str)
                .unwrap_or("<unknown>");
            report.stats.count(kind, commit);
            report.records += 1;
        }
        seen.observe(object);
    }
    for message in seen.single_run_problems(path) {
        report.problems.push(Problem {
            path: path.to_path_buf(),
            line: None,
            message,
        });
    }
}

/// The byte-level rules of ADR-0002 §6.1, which a JSON Schema cannot express.
fn byte_rules(raw: &[u8]) -> Vec<&'static str> {
    let mut problems = Vec::new();
    if raw.starts_with(b"\xef\xbb\xbf") {
        problems.push("file starts with a UTF-8 BOM");
    }
    if raw.contains(&0x0d) {
        problems.push("file contains CR; evidence is LF-only");
    }
    if raw.contains(&0) {
        problems.push("file contains a NUL byte");
    }
    if std::str::from_utf8(raw).is_err() {
        problems.push("file is not valid UTF-8");
        return problems;
    }
    if raw.is_empty() {
        problems.push("file is empty; a run that produced nothing writes no file (ADR-0002 §1.3)");
        return problems;
    }
    if !raw.ends_with(b"\n") {
        problems.push(
            "file does not end with LF; a missing final newline means a torn write (ADR-0002 §6.1)",
        );
    }
    problems
}

/// The rules the *layout* imposes but a per-line schema cannot see: the directory names
/// the commit, the filename names the run, and one file is one run (ADR-0002 §1.2, §6.1).
///
/// These apply only to the pinned shape, `<kind>/<full-sha>/<run_id>.jsonl`. A fixture
/// sitting flat in a directory of scenario labels (`tests/e2e/testdata/expected/`, the
/// e2e harness's own words) is not a layout, and a checker that invents layout rules for
/// directories it was not given would reject valid data.
fn layout_rules(path: &Path, record: &serde_json::Map<String, Value>) -> Vec<String> {
    let mut problems = Vec::new();
    let parts: Vec<String> = path
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect();
    if parts.len() < 3 {
        return problems;
    }
    let kind_dir = &parts[parts.len() - 3];
    if kind_dir != Kind::Benchmark.as_str() && kind_dir != Kind::Coverage.as_str() {
        return problems;
    }
    let sha_dir = &parts[parts.len() - 2];
    let filename = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stem = filename.trim_end_matches(".jsonl").to_owned();

    if sha_dir.len() == 40 && sha_dir.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        if let Some(commit) = record.get("commit").and_then(Value::as_str) {
            if commit != sha_dir {
                problems.push(format!(
                    "record commit {commit} does not match the directory it sits in ({sha_dir})"
                ));
            }
        }
    }
    if let Some(run_id) = record.get("run_id").and_then(Value::as_str) {
        if run_id != stem {
            problems.push(format!(
                "record run_id {run_id:?} does not match the filename {filename:?}"
            ));
        }
    }
    problems
}

/// Which identity values a file used, to prove "one file carries exactly one run"
/// (ADR-0002 §6.1) without blaming every line for the disagreement.
#[derive(Debug, Default)]
struct IdentitySets {
    run_ids: Vec<String>,
    commits: Vec<String>,
    workflows: Vec<String>,
    tasks: Vec<String>,
}

impl IdentitySets {
    fn observe(&mut self, record: &serde_json::Map<String, Value>) {
        for (field, bucket) in [
            ("run_id", &mut self.run_ids),
            ("commit", &mut self.commits),
            ("workflow", &mut self.workflows),
            ("task", &mut self.tasks),
        ] {
            if let Some(value) = record.get(field).and_then(Value::as_str) {
                if !bucket.iter().any(|held| held == value) {
                    bucket.push(value.to_owned());
                }
            }
        }
    }

    fn single_run_problems(&self, path: &Path) -> Vec<String> {
        let mut problems = Vec::new();
        for (field, values) in [
            ("run_id", &self.run_ids),
            ("commit", &self.commits),
            ("workflow", &self.workflows),
            ("task", &self.tasks),
        ] {
            if values.len() > 1 {
                problems.push(format!(
                    "one file must carry exactly one run, but {field} takes {} values {values:?}",
                    values.len()
                ));
            }
        }
        let _ = path;
        problems
    }
}

// ---------------------------------------------------------------------------
// run manifests
// ---------------------------------------------------------------------------

/// Check one `meta.json` against the manifest schema, plus the same
/// unknown-`schema_version` rule the records are held to.
fn check_meta_file(path: &Path, validator: &Validator, report: &mut Report) {
    report.meta_files += 1;
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(reason) => {
            report.problems.push(Problem {
                path: path.to_path_buf(),
                line: None,
                message: format!("could not be read: {reason}"),
            });
            return;
        }
    };
    let value: Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(reason) => {
            report.problems.push(Problem {
                path: path.to_path_buf(),
                line: None,
                message: format!("could not be parsed: {reason}"),
            });
            return;
        }
    };
    if let Some(found) = value.get("schema_version").and_then(Value::as_i64) {
        if i64::from(SCHEMA_VERSION) != found {
            report.problems.push(Problem {
                path: path.to_path_buf(),
                line: None,
                message: format!(
                    "unsupported_schema_version: manifest carries schema_version {found}, and this build knows only {SCHEMA_VERSION} (ADR-0002 §7)"
                ),
            });
            return;
        }
    }
    for SchemaProblem { path: at, message } in schema::problems(validator, &value) {
        report.problems.push(Problem {
            path: path.to_path_buf(),
            line: None,
            message: format!("{at}: {message}"),
        });
    }
}
