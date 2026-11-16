//! The recipes in `docs/evidence-format.md` §12, run against the same tree the reader reads.
//!
//! The document's recipes are the contract seen from the *reader's* side: whoever writes a scanner
//! is told "group by `[.name, (.labels // {})]`, filter `run_status == "success"`, recompute the
//! rate from the raw counts". Those are exactly the rules ADR-0008 implements in Rust, in one place
//! (P9). So the jq programs and the Rust reader must agree on the same tree, or one of them is
//! lying about the data.
//!
//! The programs are not copied into this test — they are extracted from
//! [`scripts/evidence_recipes.sh`], which is the copy the document says it was copied from, and the
//! extraction is asserted to match the fenced block in the document. A recipe that only works in
//! documentation is a bug; a recipe that disagrees with the reader is a contradiction.
//!
//! Skips loudly when `jq` is absent, the way the python checkers do: `jq` (1.7.1) is a CI tool here,
//! not a build dependency.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
};

use fathom::metrics::{Caveat, Reader};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/fathom sits two levels under the repo root")
        .to_path_buf()
}

fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

/// The three-commit tree the docs are written against, rebuilt into a scratch directory.
fn sample_tree() -> PathBuf {
    static TREE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    TREE.get_or_init(|| {
        let staging = std::env::temp_dir().join(format!("fathom-recipes-{}", std::process::id()));
        let output = Command::new(which("python3").expect("python3 lays out the sample tree"))
            .arg(repo().join("scripts/sample_evidence.py").as_os_str())
            .arg("-o")
            .arg(&staging)
            .arg("-q")
            .output()
            .expect("scripts/sample_evidence.py runs");
        assert!(
            output.status.success(),
            "sample_evidence failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        staging
    })
    .clone()
}

/// The text of recipe `number` out of `scripts/evidence_recipes.sh`.
///
/// Returns the jq flags and the program. The invocation is everything from the `jq` line up to, but
/// not including, the line that names the input files.
fn recipe(number: usize) -> (String, String) {
    let script = std::fs::read_to_string(repo().join("scripts/evidence_recipes.sh"))
        .expect("scripts/evidence_recipes.sh is readable");
    let marker = format!("# --- {number}. ");
    let start = script
        .lines()
        .position(|line| line.starts_with(&marker))
        .unwrap_or_else(|| panic!("no recipe {number} in evidence_recipes.sh"));
    let mut program: Vec<&str> = Vec::new();
    let mut flags = String::new();
    let mut started = false;
    for line in script.lines().skip(start + 1) {
        let trimmed = line.trim();
        if !started {
            // Comments and the `COMMIT=$(ls …)` line precede the invocation.
            if let Some(rest) = trimmed.strip_prefix("jq ") {
                flags = rest
                    .split_whitespace()
                    .take_while(|arg| arg.starts_with('-'))
                    .collect::<Vec<_>>()
                    .join(" ");
                // A single-line recipe puts the program on the `jq` line itself; keep it.
                let quote = rest.find('\'').expect("a jq program is single-quoted");
                program.push(&rest[quote..]);
                started = true;
            }
            continue;
        }
        if trimmed.starts_with("\"$EV") || trimmed.starts_with("\"${RECORDS") {
            break;
        }
        program.push(line);
    }
    assert!(
        !program.is_empty(),
        "recipe {number} has no jq program to extract"
    );
    let joined = program.join("\n");
    let open = joined.find('\'').expect("a jq program is single-quoted");
    let close = joined.rfind('\'').expect("and closed");
    let text = joined[open + 1..close].to_owned();
    // The document claims its blocks are the script's, verbatim. That claim is cheap to check and
    // is the only thing stopping the two from drifting apart.
    let document = std::fs::read_to_string(repo().join("docs/evidence-format.md"))
        .expect("docs/evidence-format.md is readable");
    assert!(
        document.contains(&text),
        "recipe {number}'s program is not in docs/evidence-format.md any more"
    );
    (flags, text)
}

/// Run a jq program over `files`, returning stdout.
fn jq(flags: &str, program: &str, files: &[PathBuf]) -> String {
    let jq_path = match which("jq") {
        Some(path) => path,
        None => {
            eprintln!("SKIP: jq not on PATH (CI installs 1.7.1); recipes are not asserted here");
            return String::new();
        }
    };
    let mut command = Command::new(jq_path);
    for flag in flags.split_whitespace() {
        command.arg(flag);
    }
    let output = command.arg(program).args(files).output().expect("jq runs");
    assert!(
        output.status.success(),
        "jq failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("jq prints text")
}

/// The scanner-visible record files, the way `RECORDS` is built: only `benchmark/` and
/// `coverage/`, never `runs/` or `incoming/` (ADR-0002 §1.1).
fn record_files(root: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = Vec::new();
    for kind in ["benchmark", "coverage"] {
        let kind_dir = root.join(kind);
        let Ok(entries) = std::fs::read_dir(&kind_dir) else {
            continue;
        };
        for sha_dir in entries.filter_map(|entry| entry.ok()) {
            let path = sha_dir.path();
            if !path.is_dir() {
                continue;
            }
            let Ok(records) = std::fs::read_dir(&path) else {
                continue;
            };
            for file in records.filter_map(|entry| entry.ok()) {
                let file = file.path();
                if file.is_file()
                    && file.file_name().is_some_and(|name| {
                        let name = name.to_string_lossy().into_owned();
                        name.ends_with(".jsonl") && !name.starts_with('.')
                    })
                {
                    files.push(file);
                }
            }
        }
    }
    files.sort();
    files
}

#[test]
fn recipe_2_s_raw_counts_are_the_pooled_rate() {
    let root = sample_tree();
    let mut commits: Vec<String> = std::fs::read_dir(root.join("coverage"))
        .expect("the sample tree has coverage")
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            entry
                .path()
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .collect();
    commits.sort();
    let commit = commits.first().expect("at least one commit").clone();

    let files: Vec<PathBuf> = record_files(&root)
        .into_iter()
        .filter(|path| path.starts_with(root.join("coverage").join(&commit)))
        .collect();
    let (flags, program) = recipe(2);
    let table = jq(&flags, &program, &files);
    if table.is_empty() {
        return;
    }

    let readset = Reader::new()
        .read_commits(&root, std::slice::from_ref(&commit))
        .expect("the sample tree reads");
    let row = readset.metrics(&commit).expect("a row");
    let block = row.coverage.clone().expect("coverage");

    // The recipe excludes percent-only rows because there are no counts to recompute from; the
    // reader excludes them because ADR-0002 §4.3 forbids inventing a denominator. Same rows out.
    let mut hit: u64 = 0;
    let mut found: u64 = 0;
    let mut per_run_rates: Vec<f64> = Vec::new();
    let mut printed_runs: Vec<String> = Vec::new();
    for line in table.lines() {
        let columns: Vec<&str> = line.split('\t').collect();
        assert_eq!(columns.len(), 6, "recipe 2 prints six columns: {line}");
        printed_runs.push(columns[0].to_owned());
        assert_eq!(columns[1], "success", "recipe 2 prints the run's status");
        hit += columns[3].parse::<u64>().expect("hit is a count");
        found += columns[4].parse::<u64>().expect("found is a count");
        per_run_rates.push(columns[5].parse().expect("pct is a number"));
    }
    assert_eq!(
        printed_runs, block.runs,
        "the runs the recipe prints are the runs the reader pooled"
    );
    assert_eq!(
        hit, block.hit,
        "the recipe's Σ hit is the reader's numerator"
    );
    assert_eq!(
        found, block.found,
        "the recipe's Σ found is the reader's denominator"
    );
    assert_eq!(
        block.percent,
        round_half_away(100.0 * hit as f64 / found as f64, 1),
        "ADR-0008 §3.1: the number is rate(Σhit, Σfound), and the recipe's own columns prove it"
    );

    // …and it is emphatically not the mean of the per-run percentages the same recipe prints. The
    // sample tree's two runs are 86.92 and 86.15; the pooled rate is 86.7, not 86.535.
    let mean = per_run_rates.iter().sum::<f64>() / per_run_rates.len() as f64;
    assert!(
        (mean - block.percent).abs() > 0.05,
        "a mean of rates ({mean}) must not coincide with the pooled rate ({}) here, \
         or this test has stopped distinguishing them",
        block.percent
    );
}

#[test]
fn recipe_5_s_spread_is_the_readers_spread() {
    let root = sample_tree();
    let files: Vec<PathBuf> = record_files(&root)
        .into_iter()
        .filter(|path| path.starts_with(root.join("benchmark")))
        .collect();
    let (flags, program) = recipe(5);
    let table = jq(&flags, &program, &files);
    if table.is_empty() {
        return;
    }

    let readset = Reader::new().read(&root).expect("the sample tree reads");

    // Recipe 5 folds every commit of one (metric, labels) into a single line, oldest commit first.
    // The typed rows are keyed per commit, so rebuild that shape: one line per group, commits in
    // `readset.rows()` order (a BTreeMap over SHAs, i.e. ascending, which is `group_by(.commit)`).
    // The label set is part of the key but not part of the printed line, exactly as in the recipe.
    let mut mine: BTreeMap<(String, String, String), Vec<String>> = BTreeMap::new();
    for row in readset.rows() {
        for group in &row.benchmarks {
            let unit = group
                .unit
                .clone()
                .unwrap_or_else(|| "unit_missing".to_owned());
            let entry = format!(
                "{}: [{},{}] x{}",
                row.commit.get(..7).unwrap_or(&row.commit),
                group.min,
                group.max,
                group.samples
            );
            mine.entry((
                group.name.clone(),
                unit,
                serde_json::to_string(&group.labels).unwrap_or_default(),
            ))
            .or_default()
            .push(entry);
        }
    }
    let mut mine: Vec<String> = mine
        .into_iter()
        .map(|((name, unit, _labels), entries)| format!("{name}\t{unit}\t{}", entries.join("  ")))
        .collect();
    mine.sort();
    let mut theirs: Vec<String> = table.lines().map(str::to_owned).collect();
    theirs.sort();
    assert_eq!(
        mine, theirs,
        "the reader's per-metric per-commit spread must equal recipe 5's"
    );
}

#[test]
fn recipe_7_s_advisory_records_are_caveats_and_never_a_pooled_number() {
    let root = sample_tree();
    let (flags, program) = recipe(7);
    let table = jq(&flags, &program, &record_files(&root));
    if table.is_empty() {
        return;
    }
    let readset = Reader::new().read(&root).expect("the sample tree reads");

    let mut named: Vec<(String, String)> = Vec::new();
    for line in table.lines() {
        let columns: Vec<&str> = line.split('\t').collect();
        assert_eq!(columns.len(), 5, "recipe 7 prints five columns: {line}");
        named.push((columns[0].to_owned(), columns[3].to_owned()));
    }
    assert!(
        !named.is_empty(),
        "the sample tree has scraped records, so recipe 7 must print them"
    );

    for (run_id, flags) in &named {
        for row in readset.rows() {
            if let Some(block) = &row.coverage {
                assert!(
                    !block.runs.contains(run_id),
                    "run {run_id} is advisory ({flags}) and may not be pooled into {}'s rate",
                    row.commit
                );
            }
            if flags.contains("percent_only") {
                assert!(
                    row.has_caveat(Caveat::PercentOnly),
                    "and it must be caveated on the row that could have used it"
                );
            }
        }
    }
}

/// ADR-0008 §3.2's rounding, spelled out so a reader of this test can check it by hand.
fn round_half_away(value: f64, decimals: u32) -> f64 {
    let factor = 10f64.powi(decimals as i32);
    (value * factor + 0.5).floor() / factor
}
