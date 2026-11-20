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
    // A recipe whose input sits on the same line as its closing quote (recipe 4) leaks the input path
    // into `joined`, so the closing quote is the last one *before* the input, not the last one.
    let input = joined
        .find("\"$EV")
        .or_else(|| joined.find("\"${RECORDS"))
        .unwrap_or(joined.len());
    let close = joined[..input]
        .rfind('\'')
        .expect("and closed before the input files");
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

// ---------------------------------------------------------------------------------------------
// mact-by1.12: the benchmark panel
//
// The ticket's "done when" is that the panel matches recipes 4, 5 and 6 for the sample tree, and those
// three recipes say three different things, which is the point:
//
// * recipe 4 is the *grouping* — `(name, labels)` over the whole tree, no `run_status` filter, newest
//   run by `run_id`. It is therefore also the one that shows a dead workflow's numbers: `job_duration_s`
//   reads 300.9 there because the failed run is the newest. The panel must agree with its rows and
//   disagree with its values, in exactly that direction and for exactly that reason.
// * recipe 5 is the spread, successful runs only, per commit.
// * recipe 6 is the value: median of the samples, per commit, successful runs only.
//
// So the panel's rows are recipe 4's identities, its values are recipe 6's, and its spreads are
// recipe 5's. Any other combination is a panel that has quietly started plotting a failed run.
// ---------------------------------------------------------------------------------------------

/// One commit's panel, built from the reader's row the way the store builds one.
fn panel_rows(readset: &fathom::metrics::Readset, sha: &str) -> Vec<fathom::tui::bench::Commit> {
    let commit = readset
        .metrics(sha)
        .expect("the sample tree has a row for every commit it names");
    let evidence =
        serde_json::to_value(commit).expect("a row serialises to the store's evidence section");
    let parsed = fathom::tui::bench::Commit::from_evidence(sha, Some(&evidence));
    assert_eq!(
        parsed.groups.len(),
        commit.benchmarks.len(),
        "the panel's parser must not drop a group the reader produced"
    );
    vec![parsed]
}

#[test]
fn recipe_4s_groups_are_the_panels_rows_and_its_values_are_not() {
    let root = sample_tree();
    let files: Vec<PathBuf> = record_files(&root)
        .into_iter()
        .filter(|path| path.starts_with(root.join("benchmark")))
        .collect();
    let (flags, program) = recipe(4);
    let table = jq(&flags, &program, &files);
    if table.is_empty() {
        return;
    }
    let readset = fathom::metrics::Reader::new()
        .read(&root)
        .expect("the tree reads");
    let newest: String = readset
        .rows()
        .last()
        .expect("the tree has rows")
        .commit
        .clone();
    let commit = &panel_rows(&readset, &newest)[0];

    // The identities, in the recipe's own order: `(name, labels)` with the unit as `unit_missing` when
    // there is none. The panel keys on `(name, labels, unit)` — one key wider, and the extra width is
    // the whole point of §4.3.
    let mut theirs: Vec<(String, String, String)> = Vec::new();
    for line in table.lines() {
        let column: Vec<&str> = line.split('\t').collect();
        assert_eq!(column.len(), 5, "recipe 4 prints five columns: {line}");
        theirs.push((
            column[0].to_owned(),
            column[1].to_owned(),
            column[3].to_owned(),
        ));
    }
    let mine: Vec<(String, String, String)> = commit
        .groups
        .iter()
        .map(|group| {
            (
                group.id.name.clone(),
                group.id.labels.clone(),
                group.id.unit_or_missing(),
            )
        })
        .collect();
    assert_eq!(
        mine.len(),
        theirs.len(),
        "the panel has one row per recipe-4 group"
    );
    for their in &theirs {
        assert!(
            mine.contains(their),
            "recipe 4's group {their:?} is missing from the panel: {mine:?}"
        );
    }

    // …and the values disagree by design, in the direction the doc calls out. Recipe 4 has no
    // `run_status` filter, so its `job_duration_s` is the failed run's 300.9; the panel's is the
    // successful run's 91.8, because retention is what makes a benchmark comparable at all (§9).
    let recipe_four: Vec<(String, String)> = table
        .lines()
        .map(|line| {
            let column: Vec<&str> = line.split('\t').collect();
            (column[0].to_owned(), column[2].to_owned())
        })
        .collect();
    let job = commit
        .groups
        .iter()
        .find(|group| group.id.name == "job_duration_s")
        .expect("the sample tree reports the job duration");
    let theirs = recipe_four
        .iter()
        .find(|(name, _)| name == "job_duration_s")
        .expect("recipe 4 prints it too")
        .1
        .clone();
    assert_eq!(
        (theirs.as_str(), job.value.text.as_str()),
        ("300.9", "91.8"),
        "recipe 4's newest run is the failed one; the panel's newest *selected* run is not"
    );
}

#[test]
fn recipe_6s_median_is_the_panels_value() {
    let root = sample_tree();
    let commits: Vec<String> = {
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
        commits
    };
    let readset = fathom::metrics::Reader::new()
        .read(&root)
        .expect("the tree reads");
    for commit_id in commits {
        let files: Vec<PathBuf> = record_files(&root)
            .into_iter()
            .filter(|path| path.starts_with(root.join("benchmark").join(&commit_id)))
            .collect();
        let (flags, program) = recipe(6);
        let table = jq(&flags, &program, &files);
        if table.is_empty() {
            return;
        }
        let commit = &panel_rows(&readset, &commit_id)[0];
        assert_eq!(
            table.lines().count(),
            commit.groups.len(),
            "one panel row per recipe-6 group at {commit_id}"
        );
        for line in table.lines() {
            let column: Vec<&str> = line.split('\t').collect();
            assert_eq!(column.len(), 5, "recipe 6 prints five columns: {line}");
            let group = commit
                .groups
                .iter()
                .find(|group| {
                    group.id.name == column[0]
                        && group.id.labels == column[1]
                        && group.id.unit_or_missing() == column[4]
                })
                .unwrap_or_else(|| panic!("no panel row for recipe 6's {} {line}", column[0]));
            let unit = column[4];
            // `n` and the median, cell for cell, in the stored spelling both of them print.
            assert_eq!(
                group.samples.to_string(),
                column[2],
                "samples of {} at {commit_id}",
                group.id.name
            );
            assert_eq!(
                group.value.text, column[3],
                "the panel's value is recipe 6's median of the samples, not a re-rounding of it"
            );
            assert_eq!(
                group.comparable,
                unit != "unit_missing",
                "a group with no unit is displayable and never comparable (ADR-0008 4.3)"
            );
        }
    }
}

#[test]
fn recipe_5s_spread_is_the_panels_spread() {
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
    let readset = fathom::metrics::Reader::new()
        .read(&root)
        .expect("the tree reads");
    // Recipe 5 folds one `(metric, labels)` across every commit onto a single line and does not print
    // the label set at all — three `step_duration_s` lines that differ only in what the labels said.
    // So the honest comparison is the whole line, rebuilt from the panel's own groups in the recipe's
    // own format, with the label set back in the sort key (labels are part of a group's identity even
    // where the recipe's output cannot show them).
    let mut mine: BTreeMap<(String, String, String), Vec<String>> = BTreeMap::new();
    for row in readset.rows() {
        let commit = &panel_rows(&readset, &row.commit)[0];
        for group in &commit.groups {
            let entry = format!(
                "{}: [{},{}] x{}",
                group_short(&row.commit),
                group.min.text,
                group.max.text,
                group.samples
            );
            mine.entry((
                group.id.name.clone(),
                group.id.unit_or_missing(),
                group.id.labels.clone(),
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
        "every panel cell — `[min,max]` and the sample count beside it — is recipe 5's, and the \
         successful runs are the only ones in either"
    );
}

/// Seven hex, as recipe 5 shortens a commit for width.
fn group_short(sha: &str) -> String {
    fathom::tui::bench::short(sha)
}
