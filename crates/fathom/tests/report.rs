//! The headless surface: `fathom report` (mact-by1.13).
//!
//! Three things are asserted here, and each is here because the other two cannot see it:
//!
//! * **The envelope** (ADR-0006 §10.1): two golden JSON documents, so a renamed or dropped field is a
//!   failing test rather than a broken dashboard downstream. `report.json` is the hand-built fixture
//!   (unknowns, gap codes, a mixed action verdict); `report-sample-tree.json` is the tree
//!   `scripts/sample_evidence.py` lays — records a real `act` run produced, re-stamped onto the
//!   fixture's own commits, so the shape a workspace in use has is pinned too. Nothing in either may
//!   depend on the clock or on where the fixture was unpacked, which is the same substitution act-mcp's
//!   e2e goldens make for the same reason (§7.2: two runs must be diffable).
//! * **One source of truth**: a cell the report prints is a cell the timeline prints. §10.1 forbids a
//!   second formatter, so the flat formats are compared against `fathom tui --plain` cell for cell,
//!   and unknown is `-` there and `null` in JSON — never an empty field, which a spreadsheet would read
//!   as a measured zero (ADR-0008 §6.1).
//! * **The exit codes**: `0` clean, `1` a gate finding, `2` could not run — the codes
//!   `act-mcp validate-evidence` already uses (ADR-0005 §10.2). A gate that prints the right table and
//!   exits `0` anyway is worse than no gate, because CI goes green.
//!
//! The cache and evidence roots are always passed explicitly: a developer's own `.fathom` would
//! otherwise be the fixture's store, and every count in the golden would be a function of their laptop.

mod common;

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use clap::Parser;

use common::Repo;
use fathom::{
    boot::{Access, Boot},
    cli::{Cli, Command as Subcommand, CommonArgs},
    config::Config,
    history::Query,
    report::{self, Format, Report},
    store::Indexer,
};

/// The repository root, two levels up from `crates/fathom`.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/fathom sits two levels under the repo root")
        .to_path_buf()
}

/// Where the checked-in goldens live.
fn golden(name: &str) -> PathBuf {
    repo_root()
        .join("crates")
        .join("fathom")
        .join("tests")
        .join("golden")
        .join(name)
}

/// Setting this to `1` rewrites the goldens instead of checking them. Read what it writes before
/// committing it: the golden is the second opinion, not the rubber stamp.
fn updating() -> bool {
    matches!(
        std::env::var("FATHOM_UPDATE_GOLDEN").ok().as_deref(),
        Some("1") | Some("true")
    )
}

/// A workspace, a cache root and an evidence root, none of which touch the developer's.
struct Fixture {
    root: PathBuf,
    repo: Repo,
    cache: PathBuf,
    evidence: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let scratch = tempfile::tempdir().expect("scratch");
        let root = common::canonical(scratch.path()).join(name);
        // The scratch directory is deliberately leaked: `cleanup` removes it, and a test that forgets
        // to call it leaves a directory behind rather than corrupting the next run.
        std::mem::forget(scratch);
        let evidence = root.join("evidence");
        std::fs::create_dir_all(&evidence).expect("create the evidence root");
        let repo = Repo::init(&root.join("repo"));
        Self {
            cache: root.join("cache"),
            root,
            repo,
            evidence,
        }
    }

    /// Write a file and commit it; the commit's hex SHA, as git prints it.
    fn commit(&self, message: &str, path: &str, body: &str) -> String {
        self.repo.write(path, body);
        self.repo.commit(message)
    }

    /// argv for the binary: roots pinned, then the caller's words. Element 0 is the program name,
    /// which is what `Cli::parse_from` wants and what a `Command` must not be given.
    fn argv(&self, args: &[&str]) -> Vec<String> {
        let mut argv = vec![
            "fathom".to_owned(),
            "--evidence-dir".to_owned(),
            self.evidence.display().to_string(),
            "--cache-dir".to_owned(),
            self.cache.display().to_string(),
        ];
        argv.extend(args.iter().map(|arg| (*arg).to_owned()));
        argv
    }

    /// The same argv without the program name, for spawning.
    fn spawn_args(&self, args: &[&str]) -> Vec<String> {
        self.argv(args).into_iter().skip(1).collect()
    }

    /// The boot this workspace would run under, with the same probe `fathom` runs.
    fn boot(&self, common: &CommonArgs) -> Boot {
        // `Write`, because the fixture's own `index` runs against the same roots; a cache root that
        // was never created is a lock file with nowhere to live (ADR-0006 §9).
        Boot::validate(self.repo.path(), common, Access::Write)
            .expect("the fixture workspace boots")
    }

    /// Parse argv the way the binary does, so the defaults under test are the real ones.
    fn parse(&self, args: &[&str]) -> Cli {
        Cli::parse_from(self.argv(args).iter().map(String::as_str))
    }

    /// Index the fixture, then build the report the argv describes.
    fn report(&self, args: &[&str]) -> Report {
        let cli = self.parse(args);
        let boot = self.boot(&cli.common);
        let boot: &Boot = Box::leak(Box::new(boot));
        Indexer::new(boot, Config::default(), Query::new(), "manual")
            .expect("the default config compiles")
            .run()
            .expect("the index runs");
        self.report_without_index(args)
    }

    /// Build without indexing: the store stays as it is (absent, or behind the checkout).
    fn report_without_index(&self, args: &[&str]) -> Report {
        let cli = self.parse(args);
        let args = match cli.command {
            Some(Subcommand::Report(args)) => args,
            _ => panic!("the argv did not parse as `fathom report`"),
        };
        let boot = self.boot(&cli.common);
        report::build(&boot, &Config::default(), &cli.common, &args).expect("the report builds")
    }

    /// Run the real binary — the only way to assert on stdout, stderr and exit code together.
    ///
    /// Asserts the run did not refuse: a fixture that fails to be a workspace would otherwise show up
    /// as an unexpected exit code somewhere else.
    fn run(&self, args: &[&str]) -> std::process::Output {
        let output = self.try_run(args);
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains("fathom: "),
            "the fixture should not be refusing: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    /// Run the binary and hand back the raw output, for the tests whose whole subject is a refusal.
    fn try_run(&self, args: &[&str]) -> std::process::Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fathom"));
        command
            .args(self.spawn_args(args))
            .current_dir(self.repo.path());
        command.output().expect("fathom runs")
    }

    fn index(&self) {
        let output = self.run(&["index"]);
        assert!(
            output.status.success(),
            "`fathom index` failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Write one record under `<kind>/<sha>/<run_id>.jsonl` (ADR-0008 §3's layout).
    fn record(&self, kind: &str, sha: &str, run_id: &str, record: serde_json::Value) {
        let dir = self.evidence.join(kind).join(sha);
        std::fs::create_dir_all(&dir).expect("create the record directory");
        std::fs::write(dir.join(format!("{run_id}.jsonl")), format!("{record}\n"))
            .expect("write the record");
    }

    /// One coverage record: `hit` of `found` lines, from workflow `coverage`.
    fn coverage(&self, sha: &str, run_id: &str, hit: u64, found: u64) {
        self.record(
            "coverage",
            sha,
            run_id,
            serde_json::json!({
                "schema_version": 1, "kind": "coverage", "run_id": run_id, "commit": sha,
                "workflow": "coverage", "task": "coverage", "category": "coverage",
                "run_status": "success",
                "tool": { "name": "act-mcp", "version": "0.1.0", "act_version": "0.2.68" },
                "started_at": "2026-01-01T00:00:00.000Z", "duration_ms": 60_000, "exit_code": 0,
                "host": "build-box-07", "scope": "file", "path": "src/a.py",
                "lines_hit": hit, "lines_found": found, "source": "lcov",
                "source_path": format!(".evidence/incoming/{run_id}/lcov.info")
            }),
        );
    }

    /// One coverage record whose *workflow failed*: it moves the action verdict and, because a
    /// non-success record is dropped from the metric (ADR-0008 §4.2), moves nothing else.
    fn coverage_failed(&self, sha: &str, run_id: &str) {
        self.record(
            "coverage",
            sha,
            run_id,
            serde_json::json!({
                "schema_version": 1, "kind": "coverage", "run_id": run_id, "commit": sha,
                "workflow": "nightly", "task": "coverage-nightly", "category": "coverage",
                "run_status": "failed",
                "tool": { "name": "act-mcp", "version": "0.1.0", "act_version": "0.2.68" },
                "started_at": "2026-01-01T00:00:00.000Z", "duration_ms": 60_000, "exit_code": 1,
                "host": "build-box-07", "scope": "file", "path": "src/a.py",
                "lines_hit": 0, "lines_found": 0, "source": "lcov",
                "source_path": format!(".evidence/incoming/{run_id}/lcov.info")
            }),
        );
    }

    /// One benchmark record: `name` = `value` `unit`, from workflow `benchmark`.
    fn benchmark(&self, sha: &str, run_id: &str, name: &str, value: f64) {
        self.record(
            "benchmark",
            sha,
            run_id,
            serde_json::json!({
                "schema_version": 1, "kind": "benchmark", "run_id": run_id, "commit": sha,
                "workflow": "benchmark", "task": "hyperfine-bench", "category": "benchmark",
                "run_status": "success",
                "tool": { "name": "act-mcp", "version": "0.1.0", "act_version": "0.2.68" },
                "started_at": "2026-01-01T00:00:00.000Z", "duration_ms": 30_511, "exit_code": 0,
                "host": "build-box-07", "name": name, "value": value, "unit": "ms",
                "labels": { "job": "hyperfine-bench" }, "source": "job_file"
            }),
        );
    }

    fn cleanup(&self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Three commits with nothing published for any of them: the shape a workspace is in before its first
/// coverage job has run.
fn plain_commits(name: &str) -> ThreeCommits {
    let fx = Fixture::new(name);
    let oldest = fx.commit("first", "src/a.py", "def a():\n    return 1\n");
    let middle = fx.commit("second", "src/b.py", "def b():\n    return 2\n");
    let newest = fx.commit("third", "src/c.py", "def c():\n    return 3\n");
    ThreeCommits {
        fx,
        oldest,
        middle,
        newest,
    }
}

/// Three commits: coverage and a benchmark on the first two (the second's coverage falls, its
/// benchmark moves 4×), nothing on the newest, and one failed workflow on the middle commit.
struct ThreeCommits {
    fx: Fixture,
    oldest: String,
    middle: String,
    newest: String,
}

fn three_commits(name: &str) -> ThreeCommits {
    let fx = Fixture::new(name);
    let oldest = fx.commit("first", "src/a.py", "def a():\n    return 1\n");
    fx.coverage(&oldest, "20260101T000000Z-aaaaaa", 50, 100);
    fx.benchmark(&oldest, "20260101T000000Z-bbbbbb", "parse_time_ms", 100.0);

    let middle = fx.commit("second", "src/b.py", "def b():\n    return 2\n");
    fx.coverage(&middle, "20260101T000100Z-aaaaaa", 25, 100);
    fx.benchmark(&middle, "20260101T000100Z-bbbbbb", "parse_time_ms", 400.0);
    fx.coverage_failed(&middle, "20260101T000100Z-cccccc");

    let newest = fx.commit("third", "src/c.py", "def c():\n    return 3\n");
    ThreeCommits {
        fx,
        oldest,
        middle,
        newest,
    }
}

/// Seven hex — the same truncation the envelope applies.
fn short(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// The text a golden compares: volatile paths and the config digest replaced.
fn substitute(text: &str, fx: &Fixture) -> String {
    let mut text = text
        .replace(&fx.root.display().to_string(), "@ROOT@")
        .replace(&fx.cache.display().to_string(), "@CACHE@")
        .replace(&fx.evidence.display().to_string(), "@EVIDENCE@");
    // The store's config digest depends on which config files are reachable from the fixture, which
    // is a property of the machine it was unpacked on, not of the report.
    while let Some(from) = text.find("fk1:") {
        let rest = &text[from + 4..];
        let end = rest
            .find(|c: char| !(c.is_ascii_hexdigit() || c == '.'))
            .unwrap_or(rest.len());
        text.replace_range(from..from + 4 + end, "fk1:@DIGEST@");
    }
    text
}

/// Compare against a golden, or rewrite it.
fn golden_check(name: &str, actual: &str, fx: &Fixture) {
    let path = golden(name);
    let actual = substitute(actual, fx);
    if updating() {
        std::fs::write(&path, format!("{actual}\n")).expect("write the golden");
        println!("rewrote {}", path.display());
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read golden {}: {error}", path.display()));
    assert_eq!(
        expected.trim_end(),
        actual.trim_end(),
        "the envelope changed. Read the diff; if it is intended, re-run with FATHOM_UPDATE_GOLDEN=1 \
         and read the new golden before committing it."
    );
}

// ---------------------------------------------------------------------------
// the envelope
// ---------------------------------------------------------------------------

#[test]
fn the_json_envelope_matches_the_golden_file() {
    let t = three_commits("golden");
    let report = t.fx.report(&["report"]);
    golden_check("report.json", &report.to_json(), &t.fx);

    assert_eq!(report.schema_version, 1);
    assert_eq!(report.fathom_version, env!("CARGO_PKG_VERSION"));
    assert_eq!(report.counts.commits, 3);
    assert_eq!(report.counts.not_indexed, 0, "all three are indexed");
    assert_eq!(report.counts.with_evidence, 2);
    assert_eq!(report.counts.missing_evidence, 1);
    assert_eq!(report.gate.status, "off");
    assert_eq!(report.gate.fail_on, Vec::<String>::new());
    assert_eq!(report.store.state, "present");
    assert_eq!(report.store.rows, Some(3));
    t.fx.cleanup();
}

// ---------------------------------------------------------------------------
// the envelope, over the sample tree
// ---------------------------------------------------------------------------

/// Where `scripts/sample_evidence.py` lays the checked-in e2e goldens, once per test process.
///
/// The same tree `docs/evidence-format.md`'s jq recipes and ADR-0008 §3.8 are written against — which
/// is what makes it the right input for a golden: records a real `act` run produced, not records
/// invented to make a test pass. Skips (loudly) where python3 is not installed, the way the other
/// tests that need it skip.
fn sample_tree() -> Option<PathBuf> {
    static TREE: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    TREE.get_or_init(|| {
        let python = which("python3")?;
        let staging = common::canonical(&std::env::temp_dir())
            .join(format!("fathom-report-sample-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&staging);
        let output = Command::new(python)
            .arg(repo_root().join("scripts/sample_evidence.py").as_os_str())
            .arg("-o")
            .arg(&staging)
            .arg("-q")
            .output()
            .expect("scripts/sample_evidence.py runs");
        assert!(
            output.status.success(),
            "sample_evidence.py failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Some(staging)
    })
    .clone()
}

fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

/// Copy the sample tree into this fixture's evidence root, re-stamped onto its own commits.
///
/// The script writes three synthetic commits, because no scan can be exercised by one commit; a report
/// needs the records to name commits its repository actually has, so the synthetic SHAs are replaced
/// oldest-to-oldest, in the order their run ids sort. Re-stamping `commit` is the same trick the
/// script itself uses to get three commits out of one captured run, and every other field stays the
/// goldens' own.
fn lay_sample_tree(fx: &Fixture, shas: &[String]) {
    let tree = sample_tree().expect("the sample tree");
    for kind in ["coverage", "benchmark"] {
        let root = tree.join(kind);
        let mut commits: Vec<(String, PathBuf)> = std::fs::read_dir(&root)
            .expect("the sample tree has coverage/")
            .filter_map(Result::ok)
            .filter(|entry| entry.path().is_dir())
            .map(|entry| {
                let path = entry.path();
                let first = std::fs::read_dir(&path)
                    .expect("a record directory holds records")
                    .filter_map(Result::ok)
                    .map(|file| file.file_name().to_string_lossy().into_owned())
                    .min()
                    .unwrap_or_default();
                (first, path)
            })
            .collect();
        // Run ids sort chronologically (ADR-0002 §1.2), so the earliest run in a commit's directory
        // orders the commits the way `git log` will not agree with — which is the point.
        commits.sort();
        for ((_, path), sha) in commits.iter().zip(shas) {
            let synthetic = path
                .file_name()
                .expect("a commit directory")
                .to_string_lossy();
            let into = fx.evidence.join(kind).join(sha.as_str());
            std::fs::create_dir_all(&into).expect("create the record directory");
            for file in std::fs::read_dir(path).expect("read the record directory") {
                let file = file.expect("a record").path();
                let text = std::fs::read_to_string(&file).expect("read the record");
                std::fs::write(
                    into.join(file.file_name().expect("a record file name")),
                    text.replace(synthetic.as_ref(), sha),
                )
                .expect("write the re-stamped record");
            }
        }
    }
}

#[test]
fn the_golden_is_the_sample_evidence_tree_over_real_commits() {
    if sample_tree().is_none() {
        eprintln!("SKIP: python3 not on PATH; the sample tree is the CI form of this golden");
        return;
    }
    let t = plain_commits("sample-golden");
    lay_sample_tree(
        &t.fx,
        &[t.oldest.clone(), t.middle.clone(), t.newest.clone()],
    );
    let report = t.fx.report(&["report"]);
    golden_check("report-sample-tree.json", &report.to_json(), &t.fx);

    // Every commit has coverage and a benchmark group now, so nothing is unknown and no gap code is
    // set: this is the shape a workspace in use has, and the golden is where its exact spelling lives.
    assert_eq!(report.counts.with_evidence, 3, "{:?}", report.commits);
    assert!(report.commits.iter().all(|row| row.indexed));
    for row in &report.commits {
        assert!(row.coverage.percent.is_some(), "{row:?}");
        assert!(
            row.bench.value.is_some(),
            "{}",
            row.bench.name.as_deref().unwrap_or("")
        );
        assert!(row.gaps.is_empty(), "{row:?}");
    }
    // And the tree is contract-valid, which is the script's own promise and the reason the golden is
    // worth anything: `doctor`'s evidence check is the same checker `act-mcp validate-evidence` is.
    match fathom::doctor::validate(&t.fx.evidence) {
        fathom::doctor::Validation::Clean { summary, .. } => {
            assert!(!summary.is_empty(), "the checker said nothing")
        }
        other => panic!("the sample tree must validate: {other:?}"),
    }
    t.fx.cleanup();
}

#[test]
fn nothing_in_the_report_depends_on_the_clock() {
    // §7.2: two runs at different times must diff cleanly. The timeline prints a human age; the report
    // prints the epoch and lets the caller format it, which is the only way this JSON is stable. The
    // flat formats are the exception the ticket allows: they are for a human reading a terminal.
    let t = three_commits("clock");
    let once = t.fx.report(&["report"]);
    let twice = t.fx.report(&["report"]);
    assert_eq!(once.to_json(), twice.to_json());
    for forbidden in ["\"age\"", "\"now\"", "\"generated_at\"", "\"age_seconds\""] {
        assert!(
            !once.to_json().contains(forbidden),
            "the envelope carries {forbidden}, which is derived from the clock"
        );
    }
    // And the instants that *are* there are instants, not words.
    assert_eq!(
        once.commits[0].author_utc,
        common::FIXED_EPOCH_SECONDS,
        "the fixture's own clock, as an epoch second"
    );
    t.fx.cleanup();
}

#[test]
fn a_commit_the_index_never_reached_is_a_row_of_unknowns_that_says_so() {
    // Git is the source of truth for which commits exist (ADR-0008 §2.4), so a commit with no row is a
    // row of unknowns with `indexed: false` — not a missing row, which a dashboard would read as a gap
    // in the repository rather than a gap in the cache.
    let t = three_commits("behind");
    let report = t.fx.report_without_index(&["report"]);
    assert_eq!(report.store.state, "absent");
    assert_eq!(report.store.rows, None);
    assert_eq!(report.counts.not_indexed, 3);
    assert!(report.commits.iter().all(|row| !row.indexed));
    // The identity fields come from git, which is the only thing that can name them.
    let head = &report.commits[0];
    assert_eq!(head.short, short(&t.newest));
    assert_eq!(head.subject, "third");
    assert_eq!(head.parent, Some(t.middle.clone()));
    assert_eq!(head.coverage.percent, None);
    assert_eq!(head.churn.add, None);
    assert!(head.gaps.contains(&"not_indexed".to_owned()));
    t.fx.cleanup();
}

// ---------------------------------------------------------------------------
// unknown, in three spellings
// ---------------------------------------------------------------------------

#[test]
fn unknown_is_null_in_json_and_a_dash_in_the_flat_formats() {
    let t = three_commits("unknown");
    let report = t.fx.report(&["report"]);
    let head = report.commits.first().expect("HEAD is the first row");

    assert_eq!(head.short, short(&t.newest));
    assert_eq!(head.coverage.percent, None);
    assert_eq!(
        head.coverage.delta, None,
        "no arrow is not the same as flat"
    );
    assert!(report.to_json().contains("\"percent\": null"));

    for line in report.render(Format::Tsv).lines().skip(1) {
        let cells: Vec<&str> = line.split('\t').collect();
        assert!(
            !cells.iter().any(|cell| cell.is_empty()),
            "an empty field in TSV: {line:?}"
        );
        assert!(
            !cells.iter().any(|cell| cell.chars().any(|c| c == '—')),
            "the unicode glyph must never reach a flat format: {line:?}"
        );
    }
    let csv = report.render(Format::Csv);
    assert!(
        !csv.contains(",,") && !csv.ends_with(",\n"),
        "an empty CSV field: {csv}"
    );
    t.fx.cleanup();
}

#[test]
fn csv_quotes_what_a_comma_would_break() {
    let fx = Fixture::new("csv");
    let sha = fx.commit(
        "a message, with a comma \"and\" quotes",
        "src/a.py",
        "x = 1\n",
    );
    fx.coverage(&sha, "20260101T000000Z-aaaaaa", 50, 100);
    let report = fx.report(&["report", "--format", "csv"]);
    let csv = report.render(Format::Csv);
    assert!(
        csv.contains("\"a message, with a comma \"\"and\"\" quotes\""),
        "{csv}"
    );
    // And it parses back as one row of the right width, which is the only promise RFC 4180 keeps.
    let fields = csv_fields(csv.lines().nth(1).expect("a data row"));
    assert_eq!(fields.len(), report::COLUMNS.len(), "{csv}");
    assert_eq!(fields[2], "a message, with a comma \"and\" quotes");
    fx.cleanup();
}

/// A small RFC 4180 reader, used only to prove the writer round-trips.
fn csv_fields(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                current.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => fields.push(std::mem::take(&mut current)),
            other => current.push(other),
        }
    }
    fields.push(current);
    fields
}

#[test]
fn the_tsv_agrees_with_the_plain_timeline_cell_for_cell() {
    // §10.1's one-source-of-truth rule, asserted rather than asserted-about: `report --format tsv` and
    // `fathom tui --plain` must show the same rows in the same order with the same cells. Both run the
    // real binary against one workspace and one cache, so the only difference between them can be the
    // renderer.
    let t = three_commits("crosscheck");
    t.fx.index();
    let plain = Command::new(env!("CARGO_BIN_EXE_fathom"))
        .args(t.fx.spawn_args(&["tui", "--plain"]))
        .current_dir(t.fx.repo.path())
        .output()
        .expect("fathom runs");
    assert!(
        plain.status.success(),
        "{}",
        String::from_utf8_lossy(&plain.stderr)
    );
    let tsv = t.fx.run(&["report", "--format", "tsv"]);
    assert!(tsv.status.success());

    let plain_rows = String::from_utf8_lossy(&plain.stdout)
        .lines()
        // The screen's own header line and column header precede the rows.
        .filter(|line| line.starts_with("FATHOM ") || line.starts_with("SHA "))
        .count();
    assert_eq!(plain_rows, 2, "one facts line, one column header");

    let plain_rows: Vec<Vec<String>> = String::from_utf8_lossy(&plain.stdout)
        .lines()
        .skip_while(|line| !line.starts_with("SHA "))
        .skip(1)
        // A data row's first cell is the seven-hex short; the screen's status and footer lines after
        // the table are prose, and this is the least clever way to stop at the table's end.
        .filter(|line| {
            let first = line.split_whitespace().next().unwrap_or_default();
            first.len() == 7 && first.bytes().all(|b| b.is_ascii_hexdigit())
        })
        .map(|line| line.split_whitespace().map(str::to_owned).collect())
        .collect();
    let report_rows = t.fx.report(&["report", "--format", "tsv"]).rows();
    assert_eq!(
        plain_rows.len(),
        report_rows.len(),
        "the screen showed {plain_rows:?}"
    );

    for (screen, row) in plain_rows.iter().zip(&report_rows) {
        // The screen's columns are sha, age, message, cov, +/-, MI, bench, action; the report's are
        // those eight plus `indexed` and `gaps`. The screen right-aligns and the message is one word in
        // this fixture, so whitespace-splitting the screen's row recovers its cells — except `+/-`,
        // which is two tokens in the screen and one cell in the report.
        assert_eq!(screen.len(), 9, "screen row: {screen:?}");
        assert_eq!(screen[0], row[0], "sha");
        assert_eq!(screen[1], row[1], "age");
        assert_eq!(screen[2], row[2], "message");
        assert_eq!(screen[3], row[3], "coverage");
        assert_eq!(format!("{} {}", screen[4], screen[5]), row[4], "churn");
        assert_eq!(screen[6], row[5], "mi");
        assert_eq!(screen[7], row[6], "bench");
        assert_eq!(screen[8], row[7], "action");
        assert_eq!(row[8], "true", "this fixture is fully indexed");
    }
    t.fx.cleanup();
}

#[test]
fn the_unknown_glyph_is_the_models_and_the_flat_header_is_the_screen_plus_two() {
    // The module doc claims `-` is the model's own ascii `unknown`, and that the flat header is the
    // screen's eight columns plus `indexed` and `gaps`. Both are one rename away from being false,
    // and both are things a downstream consumer reads.
    let model = fathom::metrics::Model::embedded();
    assert_eq!(
        report::UNKNOWN,
        model.display.ascii.unknown.as_str(),
        "the ascii column of schema/metric-model.json is what a flat format must print"
    );
    assert_eq!(
        report::COLUMNS.len(),
        fathom::tui::cells::COLUMNS.len() + 2,
        "the screen's columns, in the screen's order, then the two headless-only ones"
    );
    assert_eq!(&report::COLUMNS[8..], ["indexed", "gaps"]);
}

// ---------------------------------------------------------------------------
// --only-missing
// ---------------------------------------------------------------------------

#[test]
fn only_missing_is_the_queue_an_act_mcp_sweep_would_consume() {
    // "No evidence at all" is not "no row": a commit the index never reached belongs on the list as much
    // as one the index saw and found nothing about.
    let t = three_commits("missing");
    let report = t.fx.report(&["report", "--only-missing"]);
    assert!(report.window.only_missing);
    assert_eq!(
        report.commits.len(),
        1,
        "only the newest commit has nothing: {:?}",
        report
            .commits
            .iter()
            .map(|row| &row.short)
            .collect::<Vec<_>>()
    );
    assert_eq!(report.commits[0].short, short(&t.newest));
    assert_eq!(report.counts.commits, 1);
    assert_eq!(report.counts.hidden, 0, "the filter is not the limit");
    assert_eq!(report.counts.missing_evidence, 1);

    // What it hid is said: `--limit` cuts rows and `--only-missing` cuts candidates, and both numbers
    // are in the document so a reader can tell a short list from a truncated one.
    let both = t.fx.report(&["report", "--only-missing", "--limit", "1"]);
    assert_eq!(both.counts.commits, 1);
    assert_eq!(both.counts.hidden, 0);
    t.fx.cleanup();
}

#[test]
fn only_missing_includes_a_commit_the_index_never_reached() {
    let t = three_commits("missing-unindexed");
    // No index at all: every commit is both unknown and missing.
    let report = t.fx.report_without_index(&["report", "--only-missing"]);
    assert_eq!(report.commits.len(), 3);
    assert!(report.commits.iter().all(|row| !row.indexed));
    assert_eq!(report.counts.not_indexed, 3);
    t.fx.cleanup();
}

// ---------------------------------------------------------------------------
// the gate
// ---------------------------------------------------------------------------

#[test]
fn coverage_drop_names_the_commit_and_the_commit_it_was_measured_against() {
    let t = three_commits("gate-coverage");
    let report = t.fx.report(&["report", "--fail-on", "coverage-drop"]);
    assert_eq!(report.gate.status, "findings");
    assert_eq!(report.gate.fail_on, vec!["coverage-drop".to_owned()]);
    assert_eq!(report.gate.findings.len(), 1, "{:?}", report.gate.findings);
    let finding = &report.gate.findings[0];
    assert_eq!(finding.sha, t.middle);
    assert_eq!(finding.check, "coverage-drop");
    assert_eq!(
        finding.detail,
        format!("coverage 25.0% was 50.0% at {}", short(&t.oldest))
    );
    // The gate filters nothing: every row is still listed, so a CI log shows the context of the finding.
    assert_eq!(report.commits.len(), 3);
    // The human line, for stderr.
    assert!(
        finding.line().contains(&short(&t.middle)),
        "{}",
        finding.line()
    );
    t.fx.cleanup();
}

#[test]
fn a_gate_with_nothing_to_say_is_clean_and_prints_nothing() {
    let t = three_commits("gate-clean");
    let report = t.fx.report(&["report", "--fail-on", "mi-drop"]);
    assert_eq!(report.gate.status, "clean");
    assert!(report.gate.findings.is_empty());
    assert_eq!(
        report.gate.band_percent, 5.0,
        "the default band, spelled out"
    );
    t.fx.cleanup();
}

#[test]
fn benchmark_regression_and_action_failure_are_the_other_two_gates() {
    let t = three_commits("gate-rest");
    let benchmark =
        t.fx.report(&["report", "--fail-on", "benchmark-regression"]);
    assert_eq!(benchmark.gate.status, "findings");
    assert!(
        benchmark.gate.findings[0].detail.contains("parse_time_ms"),
        "{:?}",
        benchmark.gate.findings
    );

    let action = t.fx.report(&["report", "--fail-on", "action-failure"]);
    assert_eq!(action.gate.status, "findings");
    assert_eq!(action.gate.findings[0].check, "action-failure");
    assert_eq!(action.gate.findings[0].sha, t.middle);

    // Several gates at once, in one comma-separated flag or several: both spellings are accepted,
    // because both are the obvious thing to type.
    let both = t.fx.report(&[
        "report",
        "--fail-on",
        "coverage-drop,action-failure",
        "--fail-on",
        "mi-drop",
    ]);
    assert_eq!(both.gate.fail_on.len(), 3);
    assert_eq!(
        both.gate.findings.len(),
        2,
        "coverage-drop and action-failure: {:?}",
        both.gate.findings
    );
    t.fx.cleanup();
}

#[test]
fn a_move_that_was_never_measured_is_never_a_finding() {
    // The gate is a CI gate: a false positive on a commit whose parent was never benchmarked is how
    // people stop trusting it. HEAD has no benchmark at all, so `benchmark-regression` must stay quiet
    // about it — unknown is not a regression (ADR-0008 §6.1).
    let t = three_commits("gate-unknown");
    let report =
        t.fx.report(&["report", "--fail-on", "benchmark-regression"]);
    assert!(
        !report
            .gate
            .findings
            .iter()
            .any(|finding| finding.sha == t.newest),
        "{:?}",
        report.gate.findings
    );
    // And a move inside the band is not one either: the band is the config's, and it is carried in the
    // document so the finding can be re-checked without the config.
    assert_eq!(report.gate.band_percent, 5.0);
    t.fx.cleanup();
}

// ---------------------------------------------------------------------------
// the process: stdout, stderr, exit codes
// ---------------------------------------------------------------------------

#[test]
fn the_exit_code_is_the_gate_and_the_data_never_leaves_stdout() {
    let t = three_commits("exit");
    t.fx.index();

    let clean = t.fx.run(&["report"]);
    assert_eq!(clean.status.code(), Some(0), "no gate asked");
    assert!(serde_json::from_slice::<serde_json::Value>(&clean.stdout).is_ok());

    let findings = t.fx.run(&["report", "--fail-on", "coverage-drop"]);
    assert_eq!(findings.status.code(), Some(1), "a finding is 1");
    let stderr = String::from_utf8_lossy(&findings.stderr);
    assert!(stderr.contains("coverage-drop"), "{stderr}");
    // The document is unchanged by the gate: the findings line is stderr-only, so a TSV consumer never
    // inherits prose.
    assert!(serde_json::from_slice::<serde_json::Value>(&findings.stdout).is_ok());
    assert!(
        !stderr.contains("\"commits\""),
        "stderr must not look like the document: {stderr}"
    );

    let clean_gate = t.fx.run(&["report", "--fail-on", "mi-drop"]);
    assert_eq!(clean_gate.status.code(), Some(0), "nothing to say");

    // The flat formats carry the same codes.
    let tsv =
        t.fx.run(&["report", "--format", "tsv", "--fail-on", "action-failure"]);
    assert_eq!(tsv.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&tsv.stdout)
            .lines()
            .nth(1)
            .expect("one data row")
            .split('\t')
            .count(),
        report::COLUMNS.len()
    );
    t.fx.cleanup();
}

#[test]
fn a_workspace_that_cannot_be_read_exits_two_and_prints_nothing_on_stdout() {
    // `2` means `could not run`, which is not the same news as `the gate found something`, and CI has to
    // be able to tell them apart.
    let scratch = tempfile::tempdir().expect("scratch");
    let output = Command::new(env!("CARGO_BIN_EXE_fathom"))
        .arg("report")
        .current_dir(scratch.path())
        .output()
        .expect("fathom runs");
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty(), "nothing was produced");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("fathom: "), "{stderr}");
    assert!(stderr.contains("fix:"), "{stderr}");
}

#[test]
fn a_window_git_cannot_resolve_is_a_refusal_not_an_empty_report() {
    // A typo in `--from` that produced an empty table would be read as "nothing to see here" by
    // whatever is watching; it is a refusal, and it exits 2 like every other unrunnable report.
    let t = three_commits("bad-window");
    t.fx.index();
    let output =
        t.fx.try_run(&["report", "--from", "refs/heads/no-such-branch"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    // Same for `--to`, which is the one a renamed branch breaks.
    let output = t.fx.try_run(&["report", "--to", "not-a-revision"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    t.fx.cleanup();
}

#[test]
fn an_empty_repository_is_an_empty_report_and_exit_zero() {
    // Zero commits is the true answer, not a failure: a CI step that runs this before the first push
    // must not go red, and the store has not been built yet in that workspace either.
    let scratch = tempfile::tempdir().expect("scratch");
    let repo = Repo::init(&scratch.path().join("repo"));
    let output = Command::new(env!("CARGO_BIN_EXE_fathom"))
        .arg("report")
        .current_dir(repo.path())
        .output()
        .expect("fathom runs");
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("the envelope");
    assert_eq!(json["counts"]["commits"], 0);
    assert_eq!(json["store"]["state"], "absent");
    assert_eq!(json["commits"].as_array().map(Vec::len), Some(0));
    assert_eq!(json["gate"]["status"], "off");
}

#[test]
fn limit_and_window_narrow_the_rows_and_say_what_they_hid() {
    let t = three_commits("window");
    let report = t.fx.report(&["report", "--limit", "2"]);
    assert_eq!(report.counts.commits, 2);
    assert_eq!(report.counts.hidden, 1);
    assert_eq!(report.window.limit, Some(2));
    assert_eq!(report.commits[0].short, short(&t.newest));
    // A delta survives the cut: the parent of the second row is still resolved from the full window,
    // so `--limit` cannot turn a measured arrow into `null`.
    assert_eq!(report.commits[1].coverage.delta, Some("down"));

    let since = t.fx.report(&["report", "--from", &t.oldest]);
    assert_eq!(since.counts.commits, 3, "`--from` is inclusive");
    assert_eq!(since.window.from, Some(t.oldest.clone()));
    assert_eq!(since.counts.hidden, 0);

    let until = t.fx.report(&["report", "--to", &t.middle]);
    assert_eq!(until.counts.commits, 2);
    assert_eq!(until.window.to, Some(t.middle.clone()));
    assert_eq!(until.commits[0].short, short(&t.middle));
    t.fx.cleanup();
}

#[test]
fn the_window_and_the_flags_that_shaped_it_are_part_of_the_document() {
    // A gate's meaning depends on its window: `--first-parent` changes which commit a delta was
    // measured against, and `--to` changes which commits were looked at at all. A CI log that says
    // "clean" without saying what was examined proves nothing.
    let t = three_commits("flags");
    let report = t.fx.report(&["report", "--first-parent"]);
    assert!(report.window.first_parent);
    assert!(!report.window.only_missing);
    assert!(!t.fx.report(&["report"]).window.first_parent);
    assert_eq!(
        t.fx.report(&["report", "--format", "tsv"]).window.limit,
        None,
        "the format does not change the window"
    );
    t.fx.cleanup();
}
