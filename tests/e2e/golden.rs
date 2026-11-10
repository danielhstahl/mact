//! Golden `.evidence` trees: what the whole pipeline is expected to leave on disk.
//!
//! One directory per scenario under `tests/e2e/testdata/scenarios/<scenario>/`:
//!
//! ```text
//! tree.txt                 one "<lines>\t<path>" row per file, sorted
//! tree/<path>              that file's normalised content, same layout as the tree
//! ```
//!
//! The paths inside the golden are the *normalised* ones, so a run's own file names
//! appear as `coverage/{COMMIT}/{RUN_ID}.jsonl` and the golden survives a re-run at
//! any commit at any time of day (see [`crate::normalize`]).
//!
//! `tree.txt` is the first half of the assertion — the file *set* and each file's
//! line count — and it fails before a content diff would, which is the more useful
//! message when a run publishes one record too many or a directory too few.
//!
//! ## Regenerating
//!
//! ```sh
//! E2E_UPDATE_GOLDEN=1 cargo test --test e2e
//! ```
//!
//! Rewriting a golden is a deliberate act: the goldens are the contract's second
//! opinion, so a regenerated one has to be *read* before it is committed, not just
//! noticed as green.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use crate::normalize::Volatile;

/// Setting this to `1` writes the goldens instead of checking them.
pub const UPDATE_ENV: &str = "E2E_UPDATE_GOLDEN";

/// Where the checked-in trees live.
pub fn scenarios_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("e2e")
        .join("testdata")
        .join("scenarios")
}

/// Whether this run is a golden rewrite.
pub fn updating() -> bool {
    matches!(
        std::env::var(UPDATE_ENV).ok().as_deref(),
        Some("1") | Some("true")
    )
}

/// Every file under `evidence`, normalised: `normalised path -> normalised content`,
/// plus the `tree.txt` index.
pub fn snapshot(evidence: &Path, volatile: &Volatile) -> BTreeMap<String, String> {
    let mut files: BTreeMap<String, String> = BTreeMap::new();
    let mut rows: Vec<(String, usize)> = Vec::new();
    let mut missing = Vec::new();
    walk(
        evidence,
        Path::new(""),
        &mut |relative, content| {
            let path = volatile.text(&relative.display().to_string());
            let text = volatile.text(&content);
            rows.push((path.clone(), text.lines().count()));
            files.insert(format!("tree/{path}"), text);
        },
        &mut missing,
    );
    assert!(
        missing.is_empty(),
        "the evidence tree changed while it was being read: {missing:?}"
    );
    rows.sort();
    let mut listing = String::new();
    for (path, lines) in &rows {
        listing.push_str(&format!("{lines}\t{path}\n"));
    }
    files.insert("tree.txt".to_owned(), listing);
    files
}

/// Compare a snapshot with `scenarios/<scenario>/`, or write it when
/// [`UPDATE_ENV`] is set.
pub fn check(scenario: &str, snapshot: &BTreeMap<String, String>) {
    let directory = scenarios_dir().join(scenario);
    if updating() {
        if directory.exists() {
            std::fs::remove_dir_all(&directory)
                .unwrap_or_else(|e| panic!("clearing {}: {e}", directory.display()));
        }
        write_all(&directory, snapshot);
        println!(
            "e2e: wrote {} golden file(s) to {}",
            snapshot.len(),
            directory.display()
        );
        return;
    }
    let expected = read_all(&directory, scenario);
    let mut problems: Vec<String> = Vec::new();
    for path in expected.keys() {
        if snapshot.get(path).is_none() {
            problems.push(format!(
                "the run did not produce {path}, which the golden has (a missing file is as \
                 much a change as a changed one)"
            ));
        }
    }
    for (path, content) in snapshot {
        let Some(written) = expected.get(path) else {
            problems.push(format!(
                "{path} is new: the run wrote something the golden has never seen"
            ));
            continue;
        };
        if written != content {
            problems.push(format!(
                "{path} differs from the golden\n{}",
                first_difference(written, content)
            ));
        }
    }
    assert!(
        problems.is_empty(),
        "{} e2e golden problem(s) for scenario `{scenario}`:\n  {}\n\n\
         If the change is intended, read it and re-record with \
         `{UPDATE_ENV}=1 cargo test --test e2e`.",
        problems.len(),
        problems.join("\n  ")
    );
}

/// Every file under `directory`, keyed by its directory-relative path.
fn read_all(directory: &Path, scenario: &str) -> BTreeMap<String, String> {
    assert!(
        directory.is_dir(),
        "no golden directory for scenario `{scenario}` at {}; record one with \
         `{UPDATE_ENV}=1 cargo test --test e2e` and read it before committing",
        directory.display()
    );
    let mut files = BTreeMap::new();
    let mut missing = Vec::new();
    walk(
        directory,
        Path::new(""),
        &mut |relative, content| {
            files.insert(relative.display().to_string(), content);
        },
        &mut missing,
    );
    assert!(missing.is_empty(), "unreadable golden: {missing:?}");
    files
}

fn write_all(directory: &Path, files: &BTreeMap<String, String>) {
    for (path, content) in files {
        let target = directory.join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .unwrap_or_else(|e| panic!("creating {}: {e}", parent.display()));
        }
        std::fs::write(&target, content)
            .unwrap_or_else(|e| panic!("writing {}: {e}", target.display()));
    }
}

/// Depth-first, sorted, so the walk is deterministic; `on_file` sees the
/// directory-relative path and the file's text.
fn walk(
    directory: &Path,
    prefix: &Path,
    on_file: &mut dyn FnMut(&Path, String),
    missing: &mut Vec<String>,
) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    for name in names {
        let path = directory.join(&name);
        let relative = prefix.join(&name);
        if path.is_dir() {
            walk(&path, &relative, on_file, missing);
        } else if path.is_file() {
            match std::fs::read(&path) {
                Ok(bytes) => on_file(&relative, String::from_utf8_lossy(&bytes).into_owned()),
                Err(error) => missing.push(format!("{}: {error}", path.display())),
            }
        }
    }
}

/// The first differing line, both sides, truncated to something a test log can show.
fn first_difference(expected: &str, actual: &str) -> String {
    let left: Vec<&str> = expected.lines().collect();
    let right: Vec<&str> = actual.lines().collect();
    for index in 0..left.len().max(right.len()) {
        let a = left.get(index).copied().unwrap_or("<missing>");
        let b = right.get(index).copied().unwrap_or("<missing>");
        if a != b {
            return format!(
                "    line {} golden: {}\n    line {} actual:  {}",
                index + 1,
                clip(a),
                index + 1,
                clip(b)
            );
        }
    }
    "    (no differing line — a trailing-newline or byte-order difference)".to_owned()
}

fn clip(line: &str) -> String {
    const MAX: usize = 400;
    if line.len() <= MAX {
        return line.to_owned();
    }
    let cut = line
        .char_indices()
        .nth(MAX)
        .map(|(i, _)| i)
        .unwrap_or(line.len());
    format!("{}…(truncated)", &line[..cut])
}
