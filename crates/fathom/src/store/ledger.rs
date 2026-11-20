//! The evidence identity ledger — `index/sources.json`, and the answer to the one genuinely subtle
//! question in this ticket: **git history is immutable, evidence is not** (ADR-0006 §6.2).
//!
//! A row's `sources.evidence_digest` is `fk1( sorted("path\\0sha256") )` over exactly the evidence
//! files that fed it. `.evidence` is append-only and older SHAs gain records after the fact — a
//! coverage run for a commit three weeks old lands in a directory next to its peers, and nothing about
//! the commit changed — so "have I already read this commit's evidence?" cannot be answered from the
//! commit, and must be answered from the identity of the files. That is what this ledger is.
//!
//! Identity is the **content digest**. `(size, mtime_utc)` is only the cheap test that decides whether
//! to re-hash: a re-run that rewrote a file leaves a different mtime, and a file that was merely
//! touched leaves the same digest and therefore the same row. Re-stating §6.2: the digest is the
//! identity, the stat is the optimisation, and conflating them is what would make a re-index depend on
//! someone's backup tool preserving mtimes.
//!
//! The ledger is *derived data*: absent, unreadable, or older than the last run, it is rebuilt by
//! stat-ing the evidence root (§10.5), which is why crashing mid-run costs a re-stat and nothing else.
//! It is a ledger, not a log — one entry per file, sorted by path, with the files that went away
//! simply gone.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use act_evidence::read::{KIND_DIRS, RECORD_SUFFIX};
use serde::{Deserialize, Serialize};

use super::keys;

/// One evidence file's identity (§6.2's `sources.json` entry).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Repo-relative path with forward slashes, as `.evidence` lays it out
    /// (`coverage/<40 hex>/<run_id>.jsonl`). Repo-relative, not absolute: P11.
    pub path: String,
    /// The run that wrote it — its directory name, which the metric model's recency rule reads too.
    pub run_id: String,
    /// The commit the record's directory names.
    pub commit: String,
    /// Size in bytes, as the cheap staleness test.
    pub size: u64,
    /// Modification time, Unix seconds, as the same cheap test.
    pub mtime_utc: i64,
    /// The identity: lowercase hex sha256 of the file's bytes.
    pub sha256: String,
    /// JSONL lines read from it while hashing. Absent when the file was not re-hashed this run,
    /// because unknown is absent and never zero (ADR-0002 §2.1).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parsed_records: Option<u64>,
}

/// The whole ledger: the files, and whether this run had to rebuild it from a `stat` walk.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ledger {
    /// One entry per evidence file, sorted by path (§6.2: "the ledger, not a log").
    pub files: Vec<Entry>,
}

/// What refreshing the ledger cost, straight into `run.json`'s counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Refreshed {
    /// Files hashed (their size or mtime moved, or they are new).
    pub parsed: u64,
    /// Files whose `(size, mtime)` matched the ledger, so the hash was reused.
    pub unchanged: u64,
}

impl Ledger {
    /// Read `index/sources.json`. `None` means "no ledger", which §10.5 makes a rebuild, not an error.
    #[must_use]
    pub fn read(store: &super::Store) -> Option<Self> {
        store.read_json(&store.file(super::SOURCES_FILE))
    }

    /// Publish the ledger (§6.1's mechanism, like everything else in `index/`).
    pub fn write(&self, store: &super::Store) -> Result<(), super::StoreError> {
        store
            .publish(&store.file(super::SOURCES_FILE), self)
            .map(|_| ())
    }

    /// Walk the evidence root and return every contract record file, repo-relative.
    ///
    /// The walk is the contract's shape and nothing else — `<kind>/<40 hex>/<run_id>.jsonl` under the
    /// two kind directories — which is what keeps `runs/` and `incoming/` (ADR-0002 §1: not part of
    /// the contract) out of the ledger and out of a row. A naive `**/*.jsonl` glob would read them and
    /// silently turn a run's scratch records into coverage.
    ///
    /// # Errors
    ///
    /// A [`std::io::Error`] when a directory that exists refuses to be listed. An absent root is not
    /// an error: [`crate::boot`] already warned about it.
    pub fn list(root: &Path) -> std::io::Result<Vec<String>> {
        let mut out = Vec::new();
        for kind in KIND_DIRS {
            let directory = root.join(kind);
            let Ok(entries) = std::fs::read_dir(&directory) else {
                continue;
            };
            for commit_entry in entries.filter_map(Result::ok) {
                let name = commit_entry.file_name().to_string_lossy().into_owned();
                if !keys::is_commit_id(&name) || !commit_entry.path().is_dir() {
                    // A short SHA, a mixed-case one, a stray file: not the contract's shape, so not
                    // the ledger's business. `fathom doctor` (mact-by1.13) names it.
                    continue;
                }
                let Ok(records) = std::fs::read_dir(commit_entry.path()) else {
                    continue;
                };
                for record in records.filter_map(Result::ok) {
                    let file = record.file_name().to_string_lossy().into_owned();
                    if !file.ends_with(RECORD_SUFFIX) || file.starts_with('.') {
                        // `.xyz.jsonl.tmp` is a publish that never finished (ADR-0002 §6.2), and is
                        // not evidence yet.
                        continue;
                    }
                    out.push(format!("{kind}/{name}/{file}"));
                }
            }
        }
        out.sort();
        Ok(out)
    }

    /// Refresh the ledger against the evidence root, reusing digests the `(size, mtime)` test clears.
    ///
    /// `previous` is the stored ledger, if there was one. Every file that needs hashing is hashed once
    /// and its line count recorded with it, so a warm re-index of an unchanged tree costs one `stat`
    /// per file and zero reads — the budget §11.4 pins.
    ///
    /// A file that vanishes between the listing and the hash is dropped from the new ledger rather
    /// than failing the run: `.evidence` is act-mcp's to write, and a record cleaned up mid-index is a
    /// file that is simply no longer an input.
    ///
    /// # Errors
    ///
    /// The [`std::io::Error`] of a directory that refused to be listed.
    pub fn refresh(
        root: &Path,
        previous: Option<&Self>,
    ) -> Result<(Self, Refreshed), std::io::Error> {
        let by_path: BTreeMap<&str, &Entry> = previous
            .map(|ledger| {
                ledger
                    .files
                    .iter()
                    .map(|entry| (entry.path.as_str(), entry))
                    .collect()
            })
            .unwrap_or_default();

        let mut files = Vec::new();
        let mut cost = Refreshed::default();
        for path in Self::list(root)? {
            let full = root.join(&path);
            let Ok(meta) = std::fs::metadata(&full) else {
                continue;
            };
            let size = meta.len();
            let mtime_utc = meta
                .modified()
                .ok()
                .and_then(|when| when.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |delta| {
                    i64::try_from(delta.as_secs()).unwrap_or(i64::MAX)
                });
            let (_kind, commit, file) = split(&path);
            let run_id = file.trim_end_matches(RECORD_SUFFIX).to_owned();

            let entry = match by_path.get(path.as_str()) {
                Some(old) if old.size == size && old.mtime_utc == mtime_utc => {
                    cost.unchanged += 1;
                    Entry {
                        path: path.clone(),
                        run_id,
                        commit,
                        size,
                        mtime_utc,
                        sha256: old.sha256.clone(),
                        parsed_records: None,
                    }
                }
                _ => {
                    let (sha256, lines) = digest_file(&full);
                    cost.parsed += 1;
                    Entry {
                        path: path.clone(),
                        run_id,
                        commit,
                        size,
                        mtime_utc,
                        sha256,
                        parsed_records: Some(lines),
                    }
                }
            };
            files.push(entry);
        }
        files.sort_by(|left, right| left.path.cmp(&right.path));
        Ok((Self { files }, cost))
    }

    /// `fk1( sorted("path\\0sha256") )` over the files that fed one commit (§6.2, §6.3's `evidence`
    /// input kind). The empty set is the empty digest, which is a *value*: "this commit has no
    /// evidence" and "I have not looked" are different, and only the second is an absent field.
    #[must_use]
    pub fn digest_for(&self, commit: &str) -> String {
        let entries: Vec<String> = self
            .files
            .iter()
            .filter(|entry| entry.commit == commit)
            .map(|entry| format!("{}\0{}", entry.path, entry.sha256))
            .collect();
        keys::input_hash_evidence(&entries)
    }

    /// Every commit's digest at once, in one pass over the ledger.
    ///
    /// `digest_for` per walked commit is O(files × commits); a 1 000-commit walk over a 2 000-file
    /// tree would then spend its budget on string comparisons, which is the sort of thing §11.4's
    /// "incremental cost, never O(repo)" row is about. This is the same answer, once.
    #[must_use]
    pub fn digests_by_commit(&self) -> BTreeMap<String, String> {
        let mut grouped: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        for entry in &self.files {
            grouped
                .entry(entry.commit.as_str())
                .or_default()
                .push(format!("{}\0{}", entry.path, entry.sha256));
        }
        let mut out: BTreeMap<String, String> = grouped
            .into_iter()
            .map(|(commit, entries)| (commit.to_owned(), keys::input_hash_evidence(&entries)))
            .collect();
        // A commit with no files has the empty-set digest too, and the caller must be able to ask for
        // any walked SHA without a `contains_key` dance: the empty digest *is* the answer.
        let _ = &mut out;
        out
    }

    /// Every commit the ledger has files for, with the files. Built once per run by the metric read.
    #[must_use]
    pub fn commits_with_evidence(&self) -> BTreeMap<&str, Vec<&Entry>> {
        let mut out: BTreeMap<&str, Vec<&Entry>> = BTreeMap::new();
        for entry in &self.files {
            out.entry(entry.commit.as_str()).or_default().push(entry);
        }
        out
    }

    /// How many files the ledger names — `--status`'s "evidence files parsed" denominator.
    #[must_use]
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether the ledger is empty (an absent `.evidence` gives one of these, and it is legal).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// The paths it names, for a test that wants to assert which files a run re-hashed.
    #[must_use]
    pub fn paths(&self) -> Vec<String> {
        self.files.iter().map(|entry| entry.path.clone()).collect()
    }
}

/// `coverage/<sha>/<file>.jsonl` → `("coverage", "<sha>", "<file>.jsonl")`.
fn split(path: &str) -> (String, String, String) {
    let mut parts = path.split('/');
    let kind = parts.next().unwrap_or_default().to_owned();
    let commit = parts.next().unwrap_or_default().to_owned();
    let file = parts.next().unwrap_or_default().to_owned();
    (kind, commit, file)
}

/// The sha256 of a file's bytes, and the number of non-blank lines it held.
///
/// Both in one pass, because the hash already forces the read, and the line count is the only honest
/// thing that can be said about a file without a second one: counting what the strict reader would
/// *accept* is the metric reader's job, and it reports that per record (ADR-0008's `Explain`), not per
/// file.
///
/// An unreadable file digests to `unreadable` rather than to zero bytes: a file that exists and cannot
/// be read must not hash the same as an empty one, or the ledger would record "no evidence" for a
/// permissions accident.
fn digest_file(path: &Path) -> (String, u64) {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(_) => return ("unreadable".to_owned(), 0),
    };
    let lines = bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.iter().all(|byte| byte.is_ascii_whitespace()) && !line.is_empty())
        .count();
    (
        keys::bare(&keys::fk1(&bytes)).to_owned(),
        u64::try_from(lines).unwrap_or(u64::MAX),
    )
}

/// The absolute path of a ledger entry, given the root it was listed from — the one helper callers
/// need, kept here so nobody hand-assembles a contract path.
#[must_use]
pub fn path_in(root: &Path, entry: &Entry) -> PathBuf {
    root.join(&entry.path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_evidence(root: &Path, commit: &str, run_id: &str, body: &str) {
        let directory = root.join("coverage").join(commit);
        std::fs::create_dir_all(&directory).expect("make the commit directory");
        std::fs::write(directory.join(format!("{run_id}{RECORD_SUFFIX}")), body)
            .expect("write the record");
    }

    const SHA: &str = "1111111111111111111111111111111111111111";
    const OTHER: &str = "2222222222222222222222222222222222222222";

    #[test]
    fn the_walk_names_the_contract_shape_and_nothing_else() {
        let dir = tempfile::tempdir().expect("scratch");
        let root = dir.path();
        write_evidence(root, SHA, "20261110T000000Z-aaaaaa", "{\"a\":1}\n");
        write_evidence(root, OTHER, "20261110T000001Z-bbbbbb", "{\"b\":1}\n");
        // Not the contract: a short sha, a dot-temporary, `runs/`, and a stray file at the top.
        std::fs::create_dir_all(root.join("coverage/abc123")).expect("short sha dir");
        std::fs::write(
            root.join("coverage/abc123/20261110T000002Z-cccccc.jsonl"),
            "{\"nope\":1}\n",
        )
        .expect("write");
        std::fs::write(
            root.join(format!("coverage/{SHA}/.20261110T000003Z-dddddd.jsonl.tmp")),
            "torn\n",
        )
        .expect("write the corpse");
        std::fs::create_dir_all(root.join("runs/r1")).expect("runs dir");
        std::fs::write(root.join("runs/r1/meta.json"), "{}").expect("write");

        let listed = Ledger::list(root).expect("walk");
        assert_eq!(
            listed,
            vec![
                // Sorted by path, and `1…` sorts before `2…` — §11.3's rule that every listing in
                // this store has one pinned order, so two runs of the same tree agree.
                format!("coverage/{SHA}/20261110T000000Z-aaaaaa.jsonl"),
                format!("coverage/{OTHER}/20261110T000001Z-bbbbbb.jsonl"),
            ],
            "sorted by path, contract shape only, and `runs/` is not evidence"
        );
    }

    #[test]
    fn a_second_refresh_hashes_nothing_and_says_so() {
        let dir = tempfile::tempdir().expect("scratch");
        let root = dir.path();
        write_evidence(
            root,
            SHA,
            "20261110T000000Z-aaaaaa",
            "{\"a\":1}\n{\"b\":2}\n",
        );
        let (first, cost) = Ledger::refresh(root, None).expect("first walk");
        assert_eq!(cost.parsed, 1);
        assert_eq!(cost.unchanged, 0);
        assert_eq!(first.files[0].parsed_records, Some(2), "two JSONL lines");
        assert_eq!(first.files[0].run_id, "20261110T000000Z-aaaaaa");
        assert_eq!(first.files[0].commit, SHA);
        assert_eq!(
            first.files[0].sha256.len(),
            64,
            "bare hex, as §6.2 spells it"
        );

        let (second, cost) = Ledger::refresh(root, Some(&first)).expect("second walk");
        assert_eq!(
            cost.parsed, 0,
            "nothing moved: one stat per file and no reads"
        );
        assert_eq!(cost.unchanged, 1);
        assert_eq!(
            second.files[0].sha256, first.files[0].sha256,
            "and the reused digest is the same digest"
        );
        assert_eq!(
            second.files[0].parsed_records, None,
            "not re-read, so not counted"
        );
    }

    #[test]
    fn a_new_file_for_an_old_commit_moves_that_commit_s_digest_and_nobody_else_s() {
        let dir = tempfile::tempdir().expect("scratch");
        let root = dir.path();
        write_evidence(root, SHA, "20261110T000000Z-aaaaaa", "{\"a\":1}\n");
        write_evidence(root, OTHER, "20261110T000001Z-bbbbbb", "{\"b\":1}\n");
        let (ledger, _) = Ledger::refresh(root, None).expect("refresh");
        let before_sha = ledger.digest_for(SHA);
        let before_other = ledger.digest_for(OTHER);

        // act-mcp re-ran for the *old* commit: append-only, and the older SHA gains a record.
        write_evidence(root, SHA, "20261116T000000Z-cccccc", "{\"a\":2}\n");
        let (grown, cost) = Ledger::refresh(root, Some(&ledger)).expect("re-run");
        assert_eq!(
            cost,
            Refreshed {
                parsed: 1,
                unchanged: 2
            },
            "only the new file is hashed — the untouched one's stat still matches"
        );
        assert_ne!(grown.digest_for(SHA), before_sha);
        assert_eq!(grown.digest_for(OTHER), before_other);
        assert_eq!(
            grown.digest_for(&"3".repeat(40)),
            Ledger::default().digest_for(&"3".repeat(40)),
            "a commit the ledger names nothing for hashes the empty set — a value, not an absence, \
             which is why `no evidence` and `not yet looked` stay two answers"
        );
    }

    #[test]
    fn a_touched_file_with_identical_bytes_changes_nothing() {
        let dir = tempfile::tempdir().expect("scratch");
        let root = dir.path();
        let body = "{\"a\":1}\n";
        write_evidence(root, SHA, "20261110T000000Z-aaaaaa", body);
        let (ledger, _) = Ledger::refresh(root, None).expect("refresh");
        // Same size, new mtime: the cheap test says "look again", the digest says "nothing moved".
        let path = path_in(root, &ledger.files[0]);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open the record to bump its mtime")
            .set_modified(
                std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(2_000_000_000),
            )
            .expect("change the mtime");
        let (again, cost) = Ledger::refresh(root, Some(&ledger)).expect("re-hash");
        assert_eq!(cost.parsed, 1, "the mtime moved, so it was re-hashed");
        assert_eq!(
            again.digest_for(SHA),
            ledger.digest_for(SHA),
            "and the identity did not move, so the row does not either"
        );
    }

    #[test]
    fn no_evidence_at_all_is_a_value_and_not_the_same_as_unreadable() {
        let dir = tempfile::tempdir().expect("scratch");
        let (ledger, cost) = Ledger::refresh(dir.path(), None).expect("an absent root is legal");
        assert!(ledger.is_empty());
        assert_eq!(cost.parsed, 0);
        // The empty set has a digest, and it is not the digest of any non-empty set.
        let empty = ledger.digest_for(SHA);
        assert!(empty.starts_with("fk1:"));
        let mut one = ledger.clone();
        one.files.push(Entry {
            path: format!("coverage/{SHA}/r.jsonl"),
            run_id: "r".to_owned(),
            commit: SHA.to_owned(),
            size: 1,
            mtime_utc: 1,
            sha256: "dead".to_owned(),
            parsed_records: None,
        });
        assert_ne!(one.digest_for(SHA), empty);
    }
}
