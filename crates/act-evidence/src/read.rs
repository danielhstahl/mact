//! Reading a whole `.evidence` tree the way a downstream scanner does: the two contract
//! directories only, typed records where they parse, and a *named reason* for every line that
//! does not.
//!
//! ADR-0005 §7 draws the line this module sits on: `act-evidence::read` may answer "which files
//! exist for this commit, give me their typed records, and tell me what you skipped and why", and
//! it may not answer "what is this commit's coverage". Everything here is therefore a fact about
//! *bytes and records* — which paths are records, which versions this build can read, which line
//! disagrees with the directory it was found in. Nothing here computes a percentage, groups a
//! benchmark, or decides what a `run_status` means: those are ADR-0008's, and they belong to the
//! reader that calls this (fathom).
//!
//! Three rules are the whole module, and all three come from ADR-0002:
//!
//! 1. **Only `<kind>/<full-sha>/<run_id>.jsonl` is read** (§1.1). `runs/` and `incoming/` are
//!    never opened: `runs/<run_id>/act.jsonl` is act's own log and *not* a record, and it parses
//!    as plausible JSON — a scanner that globbed `**/*.jsonl` would silently aggregate a log.
//!    [`read_root`] walks two named directories and nothing else, which is what makes that
//!    mistake unmakeable rather than discouraged.
//! 2. **A dot-file is a crashed publish, never a record** (§1.1: the writer publishes with
//!    temp file + `fsync` + `rename`, so a `.<run_id>.jsonl.<rand>.tmp` left behind is the corpse
//!    of a run that died mid-publish). It is refused and named, not read.
//! 3. **Refuse what you cannot read** (§7, and §11's version policy). A `schema_version` newer
//!    than [`crate::SCHEMA_VERSION`] is skipped *and counted*, with the file and the version named
//!    in the refusal; nothing is guessed at, coerced, or upgraded in place.
//!
//! ## What a refusal is
//!
//! Every line this module does not hand back produced a [`Refusal`] naming the location, a
//! [`Reason`] and a human-readable detail. Refusals are not errors and do not stop the scan: a
//! tree with four bad lines in one file yields four refusals and the good records beside them,
//! which is the §7 policy ("permissive on values, strict on shape, skip with a count") applied by
//! the only crate that is allowed to apply it.

use std::{
    collections::BTreeMap,
    fmt,
    path::{Path, PathBuf},
};

use serde_json::Value;
use thiserror::Error;

use crate::{shapes, Envelope, EvidenceRecord, Kind};

/// The suffix of a record file, and nothing else is a candidate (ADR-0002 §1.1).
pub const RECORD_SUFFIX: &str = ".jsonl";

/// The two contract directories, in walk order (ADR-0002 §1.1).
///
/// Order is part of the observable behaviour: a reader that pools records in walk order sees
/// `benchmark` before `coverage`, and the ADR-0008 reference implementation walks in this order
/// too. `runs` and `incoming` are absent from this list by construction, which is the point.
pub const KIND_DIRS: [&str; 2] = ["benchmark", "coverage"];

/// The envelope fields every line must carry (ADR-0002 §2.2, the "always there ✅" column).
///
/// Checked for *presence* here rather than by a serde error because the point of this module is
/// naming why a line was refused: `envelope field 'host' is absent` is an answer, and serde's
/// message through two levels of `flatten` is not.
const REQUIRED_ENVELOPE: [&str; 11] = [
    "schema_version",
    "kind",
    "run_id",
    "commit",
    "workflow",
    "task",
    "category",
    "run_status",
    "tool",
    "started_at",
    "host",
];

/// Why a path or a line was not handed back as a record.
///
/// These are facts about bytes and the layout contract, deliberately not ADR-0008's drop-reason
/// vocabulary (that one includes retention policy — `run_status_excluded`, `config_filtered` —
/// which is the caller's business). The mapping from here to there is one arrow each and is tested
/// in fathom.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Reason {
    /// The path is not `<kind>/<40-hex>/<run_id>.jsonl`: wrong suffix, wrong depth, or a file
    /// where a record file cannot live.
    NotARecordPath,
    /// A dot-file: `.<run_id>.jsonl.<random>.tmp` is a crashed publish (ADR-0002 §1.1).
    TempFile,
    /// A directory under `<kind>/` whose name is not 40 lowercase hex — a short SHA, a mixed-case
    /// SHA, or a foreign name (ADR-0002 §1.1).
    DirectoryNotSha,
    /// The file's last byte is not LF, so its final line may be a torn write (ADR-0002 §6.1).
    TornWrite,
    /// `schema_version` is newer than this build reads (ADR-0002 §7: skip and count, never guess).
    SchemaUnsupported,
    /// Not strict JSON, not one object, a missing envelope field, a `kind` that disagrees with its
    /// directory, a line that does not fit the variant its `kind` names, or an unreadable file.
    Malformed,
    /// The record's `commit` is not the directory it was read from. The directory is trusted and
    /// the record is refused (ADR-0002 §1.1) — a hand-moved tree stays visible as one.
    CommitMismatch,
    /// `run_id` does not match ADR-0002 §1.2, so it can be neither sorted nor selected.
    BadRunId,
}

impl Reason {
    /// The stable snake-case code. Refusals are printed and grepped, so the spelling is an API.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::NotARecordPath => "not_a_record_path",
            Self::TempFile => "temp_file",
            Self::DirectoryNotSha => "directory_not_sha",
            Self::TornWrite => "torn_write",
            Self::SchemaUnsupported => "schema_unsupported",
            Self::Malformed => "malformed",
            Self::CommitMismatch => "commit_mismatch",
            Self::BadRunId => "bad_run_id",
        }
    }

    /// Whether the refusal means "a record was there and this build could not read it".
    ///
    /// The distinction is what ADR-0008 §5.4 needs: a commit whose records are all unreadable is
    /// *unknown for a different reason* than a commit nothing ran on, and both render `—`.
    /// Layout refusals (a dot-file, a non-SHA directory) are not unreadable records: there was
    /// no record there to read.
    #[must_use]
    pub const fn is_unreadable_record(self) -> bool {
        matches!(
            self,
            Self::TornWrite
                | Self::SchemaUnsupported
                | Self::Malformed
                | Self::CommitMismatch
                | Self::BadRunId
        )
    }
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

/// One refused path or line, with enough in it to act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// The commit directory the refusal sits under, when it sits under one. `None` for a directory
    /// whose name is not a SHA — there is no commit to attribute it to.
    pub sha: Option<String>,
    /// Root-relative path, forward slashes, `:<line>` appended for a line-level refusal.
    pub location: String,
    /// Why.
    pub reason: Reason,
    /// The sentence half: what specifically was wrong, with the offending value quoted.
    pub detail: String,
    /// The run the line named, when the line was JSON enough to say.
    pub run_id: Option<String>,
    /// The run status the line named, when it was JSON enough to say.
    pub run_status: Option<String>,
}

impl fmt::Display for Refusal {
    /// `location: code: detail` — the shape `validate-evidence` and `doctor` print, so one habit
    /// reads all three.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}: {}", self.location, self.reason, self.detail)
    }
}

/// One record that came back, plus where it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    /// The typed record, parsed by [`EvidenceRecord::from_value`] — the contract crate's own types,
    /// which is the only deserializer any consumer is allowed to use (ADR-0002 §7, ADR-0005 P2).
    pub record: EvidenceRecord,
    /// Root-relative path, forward slashes.
    pub path: String,
    /// 1-based line within that file.
    pub line: u32,
}

impl Record {
    /// `path:line`, for a refusal or an explain line.
    #[must_use]
    pub fn location(&self) -> String {
        format!("{}:{}", self.path, self.line)
    }

    /// The shared envelope.
    #[must_use]
    pub fn envelope(&self) -> &Envelope {
        self.record.envelope()
    }

    /// The variant tag.
    #[must_use]
    pub fn kind(&self) -> Kind {
        self.record.kind()
    }
}

/// Everything one walk of one evidence root produced.
#[derive(Debug, Clone, Default)]
pub struct Scan {
    root: PathBuf,
    root_present: bool,
    files: usize,
    lines: usize,
    by_commit: BTreeMap<String, Vec<Record>>,
    refusals: Vec<Refusal>,
    unreadable: BTreeMap<String, usize>,
}

impl Scan {
    /// The root this scan walked.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Whether the root existed at all. A missing evidence root is *not* an error (ADR-0008 §2's
    /// "tolerate reality": a workspace with no `.evidence` is a workspace of unknown commits, and
    /// every column renders `—`), but the caller is entitled to know the difference.
    #[must_use]
    pub fn root_present(&self) -> bool {
        self.root_present
    }

    /// How many record files were opened.
    #[must_use]
    pub fn files(&self) -> usize {
        self.files
    }

    /// How many non-blank record lines were attempted.
    #[must_use]
    pub fn lines(&self) -> usize {
        self.lines
    }

    /// How many lines came back as typed records.
    #[must_use]
    pub fn records(&self) -> usize {
        self.by_commit.values().map(Vec::len).sum()
    }

    /// The commits the tree has records for, in sorted order.
    ///
    /// Note what this is *not*: it is not the list of commits that exist. `.evidence` never says
    /// that (ADR-0008 §2.4); only the git walk does. This is the list of commits that have
    /// *readable evidence*, which is a different question with a different answer.
    pub fn commits(&self) -> impl Iterator<Item = &str> {
        self.by_commit.keys().map(String::as_str)
    }

    /// Every readable record under `sha`, in walk order.
    #[must_use]
    pub fn records_for(&self, sha: &str) -> &[Record] {
        self.by_commit.get(sha).map_or(&[], Vec::as_slice)
    }

    /// Every readable record in the tree, in walk order.
    pub fn all_records(&self) -> impl Iterator<Item = &Record> {
        self.by_commit.values().flatten()
    }

    /// Every refusal, in walk order.
    #[must_use]
    pub fn refusals(&self) -> &[Refusal] {
        &self.refusals
    }

    /// The refusals under one commit directory.
    pub fn refusals_for<'a>(&'a self, sha: &str) -> impl Iterator<Item = &'a Refusal> + 'a {
        let sha = sha.to_owned();
        self.refusals
            .iter()
            .filter(move |refusal| refusal.sha.as_deref() == Some(sha.as_str()))
    }

    /// How many *records* were present under this commit but could not be read.
    ///
    /// A dot-file does not count: a crashed publish is not a record the reader failed on.
    #[must_use]
    pub fn unreadable(&self, sha: &str) -> usize {
        self.unreadable.get(sha).copied().unwrap_or(0)
    }

    /// Every commit with at least one unreadable record.
    pub fn unreadable_commits(&self) -> impl Iterator<Item = (&str, usize)> {
        self.unreadable
            .iter()
            .map(|(sha, count)| (sha.as_str(), *count))
    }

    /// Count a refusal, both for the list and for the per-commit unreadable tally.
    fn refuse(&mut self, refusal: Refusal) {
        if refusal.reason.is_unreadable_record() {
            if let Some(sha) = refusal.sha.clone() {
                *self.unreadable.entry(sha).or_insert(0) += 1;
            }
        }
        self.refusals.push(refusal);
    }
}

/// Why a whole scan could not run, as opposed to a line that was refused.
///
/// Narrow on purpose. A missing root, a missing `coverage/`, an empty directory and a tree with
/// only one kind are all *answers*, not errors; only a directory the OS refuses to list, or a
/// root that exists and is not a directory, is an error here.
#[derive(Debug, Error)]
pub enum ReadError {
    /// The root exists and is not a directory.
    #[error("{path} is not a directory\n  fix: point the reader at an evidence root (the directory holding benchmark/ and coverage/), not at a file inside it")]
    NotADirectory {
        /// What was not a directory.
        path: PathBuf,
    },

    /// A directory could not be listed (permissions, EIO).
    #[error("cannot list {path}: {reason}")]
    Unreadable {
        /// The directory that refused to be listed.
        path: PathBuf,
        /// The OS message, verbatim.
        reason: String,
    },
}

/// Walk `root` and read every record it holds.
///
/// `root` is an evidence root — the directory holding `benchmark/` and `coverage/`, normally
/// `<workspace>/.evidence`. Missing directories are not errors; an absent root scans to an empty
/// [`Scan`] with [`Scan::root_present`] false, because a workspace that has never run a workflow
/// has an unknown timeline rather than a broken one.
///
/// # Errors
///
/// [`ReadError`] when the root is a file, or when a directory that exists cannot be listed. A bad
/// *line* is never an error: it is a [`Refusal`].
///
/// # Examples
///
/// ```no_run
/// let scan = act_evidence::read::read_root(".evidence".as_ref()).expect("root readable");
/// for refusal in scan.refusals() {
///     println!("{refusal}");
/// }
/// ```
pub fn read_root(root: &Path) -> Result<Scan, ReadError> {
    let mut scan = Scan {
        root: root.to_path_buf(),
        ..Default::default()
    };
    if !root.exists() {
        return Ok(scan);
    }
    if !root.is_dir() {
        return Err(ReadError::NotADirectory {
            path: root.to_path_buf(),
        });
    }
    scan.root_present = true;

    for kind_dir in KIND_DIRS {
        for sha_dir in list_sorted(&root.join(kind_dir))? {
            let sha = sha_dir
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !sha_dir.is_dir() {
                // A stray file at `<kind>/<something>` is not a record and not a commit; the
                // layout says one file per run per commit directory, and this is neither. Ignored
                // rather than refused, because there is no commit to attribute it to and no
                // consumer whose question it answers.
                continue;
            }
            if !shapes::is_commit(&sha) {
                scan.refuse(Refusal {
                    sha: None,
                    location: display_path(&sha_dir, root),
                    reason: Reason::DirectoryNotSha,
                    detail: format!("expected 40 lowercase hex, got {sha:?}"),
                    run_id: None,
                    run_status: None,
                });
                continue;
            }
            for file in list_sorted(&sha_dir)? {
                if !file.is_file() {
                    continue;
                }
                let path = display_path(&file, root);
                let name = file
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if name.starts_with('.') {
                    scan.refuse(Refusal {
                        sha: Some(sha.clone()),
                        location: path,
                        reason: Reason::TempFile,
                        detail: "a dot-file is a crashed publish, never a record (ADR-0002 §1.1)"
                            .to_owned(),
                        run_id: None,
                        run_status: None,
                    });
                    continue;
                }
                if !name.ends_with(RECORD_SUFFIX) {
                    scan.refuse(Refusal {
                        sha: Some(sha.clone()),
                        location: path,
                        reason: Reason::NotARecordPath,
                        detail: format!("{name:?} is not a *.{RECORD_SUFFIX} record file"),
                        run_id: None,
                        run_status: None,
                    });
                    continue;
                }
                read_file(&mut scan, &sha, &file, &path, kind_dir);
            }
        }
    }
    Ok(scan)
}

/// Read one record file into `scan`: refusals for the file-level problems, records for the lines
/// that parse, and per-line refusals for the rest.
fn read_file(scan: &mut Scan, sha: &str, file: &Path, path: &str, kind_dir: &str) {
    scan.files += 1;
    let text = match std::fs::read(file) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(err) => {
            scan.refuse(Refusal {
                sha: Some(sha.to_owned()),
                location: path.to_owned(),
                reason: Reason::Malformed,
                detail: format!("cannot read file: {err}"),
                run_id: None,
                run_status: None,
            });
            return;
        }
    };

    // A torn tail is reported once, for the file; the partial line still goes through the same
    // per-line path below and will be refused as the bad JSON it is. That is deliberate: the
    // file's *other* records are whole and readable, and losing them because the last line was
    // cut off would throw away real data over one broken line.
    if !text.is_empty() && !text.ends_with('\n') {
        scan.refuse(Refusal {
            sha: Some(sha.to_owned()),
            location: path.to_owned(),
            reason: Reason::TornWrite,
            detail: "last byte is not LF: torn write (ADR-0002 §6.1)".to_owned(),
            run_id: None,
            run_status: None,
        });
    }

    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        scan.lines += 1;
        let line_no = u32::try_from(index + 1).unwrap_or(u32::MAX);
        let location = format!("{path}:{line_no}");
        match parse_line(line, sha, kind_dir) {
            Ok(record) => scan
                .by_commit
                .entry(sha.to_owned())
                .or_default()
                .push(Record {
                    record,
                    path: path.to_owned(),
                    line: line_no,
                }),
            Err((reason, detail)) => {
                // A refusal that quoted a run is worth its run id: `--explain` and `doctor` can
                // then point at the run directory that produced it. Parsing the line a second
                // time is cheaper than threading a half-parsed value through every arm.
                let identity = serde_json::from_str::<Value>(line).ok();
                let quoted = |field: &str| {
                    identity
                        .as_ref()
                        .and_then(|value| value.get(field))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                };
                scan.refuse(Refusal {
                    sha: Some(sha.to_owned()),
                    location,
                    reason,
                    detail,
                    run_id: quoted("run_id"),
                    run_status: quoted("run_status"),
                });
            }
        }
    }
}

/// Parse one line into a typed record, or say why it is not one.
///
/// `sha` is the directory the file sits in and `kind_dir` the contract directory above it; both
/// are asserted against the record, because the *path* is the join key a scanner keys on
/// (ADR-0002 §1.1) and a tree that has been hand-moved must show up as refusals rather than as
/// one commit's evidence attributed to another.
///
/// The order is the order the refusals get better at explaining: JSON first, then the version
/// this build can read, then the envelope's presence, then the two path-vs-record assertions,
/// and only then the contract's own deserializer. The last step is the authority on the payload;
/// the steps before it exist to *name* what is wrong instead of returning serde's message.
fn parse_line(line: &str, sha: &str, kind_dir: &str) -> Result<EvidenceRecord, (Reason, String)> {
    let value: Value = serde_json::from_str(line)
        .map_err(|err| (Reason::Malformed, format!("not strict JSON: {err}")))?;
    let object = value
        .as_object()
        .ok_or_else(|| (Reason::Malformed, "line is not a JSON object".to_owned()))?;

    if let Some(found) = object.get("schema_version").and_then(Value::as_i64) {
        if found > i64::from(crate::SCHEMA_VERSION) {
            return Err((
                Reason::SchemaUnsupported,
                format!(
                    "schema_version {found} is newer than this build reads ({})",
                    crate::SCHEMA_VERSION
                ),
            ));
        }
    }

    envelope_problem(&value, kind_dir)?;

    let record = EvidenceRecord::from_value(&value)
        .map_err(|err| (Reason::Malformed, format!("line does not parse: {err}")))?;

    // The two path assertions are made *after* the typed parse so the comparison is on the
    // contract's own newtypes rather than on whatever JSON happened to say.
    let envelope = record.envelope();
    if envelope.commit.as_str() != sha {
        return Err((
            Reason::CommitMismatch,
            format!(
                "record commit {:?} != directory {sha:?}",
                envelope.commit.as_str()
            ),
        ));
    }
    if !shapes::is_run_id(envelope.run_id.as_str()) {
        return Err((
            Reason::BadRunId,
            format!(
                "run_id {:?} is not YYYYMMDDTHHMMSSZ-<6 hex>",
                envelope.run_id.as_str()
            ),
        ));
    }
    Ok(record)
}

/// The envelope-and-scope pre-check: `Ok(())` when this line is worth deserializing, `Err` naming
/// what is missing or contradicting.
///
/// This is deliberately *only* the checks that need more than one field to state (a `kind` that
/// disagrees with its directory, a `total` record that carries a `path`) plus presence — the
/// payload itself is [`EvidenceRecord::from_value`]'s business, so there is no second copy of the
/// record shape here (ADR-0005 P2: one implementation of reading `.evidence`).
fn envelope_problem(value: &Value, expect_kind: &str) -> Result<(), (Reason, String)> {
    let object = value
        .as_object()
        .ok_or_else(|| (Reason::Malformed, "line is not a JSON object".to_owned()))?;
    for field in REQUIRED_ENVELOPE {
        if !object.contains_key(field) {
            return Err((
                Reason::Malformed,
                format!("envelope field {field:?} is absent (ADR-0002 §2.2)"),
            ));
        }
    }
    if !object
        .get("schema_version")
        .is_some_and(|version| version.is_i64() || version.is_u64())
    {
        return Err((
            Reason::Malformed,
            "schema_version is not an integer".to_owned(),
        ));
    }
    let kind = object
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| (Reason::Malformed, "kind is not a string".to_owned()))?;
    if Kind::parse(kind).is_err() {
        return Err((
            Reason::Malformed,
            format!("unknown kind {kind:?} (the enum is benchmark | coverage)"),
        ));
    }
    if kind != expect_kind {
        return Err((
            Reason::Malformed,
            format!("kind {kind:?} inside a {expect_kind}/ directory"),
        ));
    }

    if kind == "coverage" {
        let scope = object.get("scope").and_then(Value::as_str);
        if !matches!(scope, Some("total") | Some("file")) {
            return Err((
                Reason::Malformed,
                format!("coverage scope {scope:?} is not total | file"),
            ));
        }
        if scope == Some("file") && !object.get("path").is_some_and(Value::is_string) {
            return Err((
                Reason::Malformed,
                "a file record carries no path".to_owned(),
            ));
        }
        if scope == Some("total") && object.contains_key("path") {
            return Err((
                Reason::Malformed,
                "a total record must not carry path (ADR-0002 §4.2)".to_owned(),
            ));
        }
    } else {
        if !object.get("name").is_some_and(Value::is_string) {
            return Err((Reason::Malformed, "benchmark record has no name".to_owned()));
        }
        let value_field = object.get("value");
        if value_field.is_none_or(|value| !(value.is_i64() || value.is_u64() || value.is_f64())) {
            return Err((
                Reason::Malformed,
                "benchmark record has no numeric value".to_owned(),
            ));
        }
    }
    Ok(())
}

/// Sorted directory entries (empty when the directory is absent — a tree with only one kind of
/// evidence is a normal tree, not a broken one).
fn list_sorted(dir: &Path) -> Result<Vec<PathBuf>, ReadError> {
    let mut entries: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(reader) => reader
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .collect(),
        Err(err) if matches!(err.kind(), std::io::ErrorKind::NotFound) => return Ok(Vec::new()),
        Err(err) => {
            return Err(ReadError::Unreadable {
                path: dir.to_path_buf(),
                reason: err.to_string(),
            });
        }
    };
    entries.sort();
    Ok(entries)
}

/// `path` relative to `root`, with forward slashes, for printing.
fn display_path(path: &Path, root: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    let text = relative.to_string_lossy().replace('\\', "/");
    if text.is_empty() {
        ".".to_owned()
    } else {
        text
    }
}
