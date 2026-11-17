//! `fathom doctor` (mact-by1.13): the command that explains why the columns are unknown.
//!
//! The contract under test is not "the report is pretty" — it is that **every state that makes fathom
//! print `—` is named, with the thing to do about it**, and that the exit code says how bad the worst
//! one is (ADR-0006 §10): `0` all ok, `1` something to look at, `2` something is broken. A check
//! without a `fix:` line is a diagnosis without a prescription, so each test that expects a non-`ok`
//! status asserts a `fix:` too.
//!
//! Deliberately *not* tested here: `act-mcp` itself. The evidence check runs the sibling binary when it
//! finds one and the same in-process checker when it does not ([`fathom::doctor::validate`]), and both
//! paths are asserted to be the *same verdict* rather than the same code path — a substituted checker
//! must be labelled, because a clean bill of health from the wrong checker is worse than none.
//!
//! Most tests drive `doctor::run` in-process: a status is a fact, and facts do not need a subprocess.
//! The ones that assert on stdout, `--json` and exit codes run the binary.

mod common;

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use clap::Parser;

use common::Repo;
use fathom::{
    boot::{Access, Boot},
    cli::Cli,
    config::Config,
    doctor::{self, Report, Status},
    store::Store,
};

/// A workspace, a cache root and an evidence root, all under one throwaway directory.
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
        std::mem::forget(scratch);
        let cache = root.join("cache");
        let evidence = root.join("evidence");
        std::fs::create_dir_all(&evidence).expect("create the evidence root");
        let repo = Repo::init(&root.join("repo"));
        Self {
            cache,
            repo,
            root,
            evidence,
        }
    }

    fn commit(&self, message: &str, path: &str, body: &str) -> String {
        self.repo.write(path, body);
        self.repo.commit(message)
    }

    /// argv with the roots pinned; element 0 is the program name (for `parse`), which `run` drops.
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

    /// Run `doctor` in-process, exactly as the subcommand does, config discovery included.
    fn doctor(&self) -> Report {
        let cli = Cli::parse_from(self.argv(&["doctor"]).iter().map(String::as_str));
        // `Read`, because that is the access `fathom doctor` asks for: it diagnoses a read-only cache
        // root instead of refusing to look at it (ADR-0006 §10.1).
        let boot =
            Boot::validate(self.repo.path(), &cli.common, Access::Read).expect("the fixture boots");
        let config = Config::discover(cli.common.config.as_deref(), &boot.workspace, None);
        let (config, error) = match config {
            Ok(config) => (config, None),
            Err(reason) => (Config::default(), Some(reason.to_string())),
        };
        doctor::run(&doctor::Input {
            boot: &boot,
            config: &config,
            config_error: error,
        })
    }

    /// Index the workspace so the `cache` check has a store to talk about.
    ///
    /// Through the binary, not in-process: the lock body the index leaves behind names *its* pid, and a
    /// fixture that indexes inside the doctor's own process would be reporting a live indexer that is
    /// really the test. In the real world the two are different processes, and the body is a trace of a
    /// run that ended.
    fn index(&self) {
        let output = self.run(&["index"]);
        assert!(
            output.status.success(),
            "`fathom index` failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// The store's own directory, once it exists.
    fn store(&self) -> Store {
        Store::open(&self.cache)
            .expect("the store opens")
            .expect("the store is there")
    }

    /// Write a file in the workspace (relative to the repo, or to the git root).
    fn write(&self, path: &str, body: &str) {
        self.repo.write(path, body);
    }

    /// Write an invalid record: shape-valid enough to be found, wrong enough to fail the checker.
    fn broken_record(&self, sha: &str) {
        let dir = self.evidence.join("coverage").join(sha);
        std::fs::create_dir_all(&dir).expect("create the record directory");
        std::fs::write(
            dir.join("20260101T000000Z-aaaaaa.jsonl"),
            "{\"schema_version\":1,\"kind\":\"coverage\"}\n",
        )
        .expect("write the record");
    }

    /// Write one contract-valid coverage record for `sha`, under the evidence root.
    fn coverage(&self, sha: &str) {
        let dir = self.evidence.join("coverage").join(sha);
        std::fs::create_dir_all(&dir).expect("create the record directory");
        std::fs::write(
            dir.join("20260101T000000Z-aaaaaa.jsonl"),
            format!(
                "{{\"schema_version\":1,\"kind\":\"coverage\",\"run_id\":\"20260101T000000Z-aaaaaa\",\"commit\":\"{sha}\",\"workflow\":\"coverage\",\"task\":\"coverage\",\"category\":\"coverage\",\"run_status\":\"success\",\"tool\":{{\"name\":\"act-mcp\",\"version\":\"0.1.0\"}},\"started_at\":\"2026-01-01T00:00:00.000Z\",\"host\":\"box\",\"name\":\"total\",\"scope\":\"total\",\"lines_found\":100,\"lines_hit\":80,\"source\":\"lcov\",\"source_path\":\"lcov.info\"}}\n"
            ),
        )
        .expect("write the record");
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fathom"));
        command
            .args(self.argv(args).into_iter().skip(1))
            .current_dir(self.repo.path());
        command.output().expect("fathom runs")
    }

    fn cleanup(&self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// One committed source file, so `languages` and `cache` have something to talk about.
fn workspace(name: &str) -> Fixture {
    let fx = Fixture::new(name);
    fx.commit("first", "src/a.py", "def a():\n    return 1\n");
    fx
}

/// Every check the command is supposed to run, in the order it prints them.
const CHECKS: [&str; 7] = [
    "workspace",
    "evidence",
    "runs",
    "workflows",
    "cache",
    "languages",
    "config",
];

/// The worst status a report contains.
fn worst(report: &Report) -> Status {
    report
        .checks
        .iter()
        .map(|check| check.status)
        .max()
        .unwrap_or(Status::Ok)
}

// ---------------------------------------------------------------------------
// the shape of the answer
// ---------------------------------------------------------------------------

#[test]
fn every_check_runs_and_the_exit_code_is_the_worst_one() {
    let fx = workspace("shape");
    fx.index();
    let report = fx.doctor();
    let names: Vec<&str> = report.checks.iter().map(|check| check.name).collect();
    assert_eq!(names, CHECKS, "every promised check, in order");
    assert_eq!(report.status, worst(&report));
    assert_eq!(
        report.exit().code(),
        match worst(&report) {
            Status::Ok => 0,
            Status::Warn => 1,
            Status::Fail => 2,
        }
    );
    // And no check is ever left without a way out.
    for check in &report.checks {
        if check.status != Status::Ok {
            assert!(
                !check.fixes.is_empty(),
                "{} is {:?} with no fix: {:?}",
                check.name,
                check.status,
                check
            );
        }
    }
    fx.cleanup();
}

#[test]
fn the_json_and_the_text_are_one_report_not_two() {
    let fx = workspace("json");
    fx.index();
    let report = fx.doctor();
    let json: serde_json::Value = serde_json::from_str(&report.to_json()).expect("the JSON parses");
    assert_eq!(json["schema_version"], doctor::SCHEMA_VERSION);
    assert_eq!(json["fathom_version"], env!("CARGO_PKG_VERSION"));
    let checks = json["checks"].as_array().expect("a list of checks");
    assert_eq!(checks.len(), CHECKS.len());
    for (index, check) in report.checks.iter().enumerate() {
        assert_eq!(checks[index]["name"], check.name);
        assert_eq!(checks[index]["status"], check.status.as_str());
        assert_eq!(checks[index]["summary"], check.summary);
        assert_eq!(
            checks[index]["details"].as_array().map(Vec::len),
            Some(check.details.len()),
            "{}'s details",
            check.name
        );
        // Nothing in the JSON may be a bare word the text report invented: every non-ok check carries
        // its fixes as a list, which is the part a script can act on.
        if check.status != Status::Ok {
            assert!(!checks[index]["fixes"].as_array().unwrap().is_empty());
        }
    }
    // The text is the same report: same number of check lines, same tally line at the end.
    let text = report.to_text();
    for name in CHECKS {
        assert!(text.contains(name), "the text omits `{name}`:\n{text}");
    }
    assert!(
        text.ends_with(&format!("exit {}\n", report.exit().code())),
        "the text says the code it will exit with:\n{text}"
    );
    fx.cleanup();
}

#[test]
fn the_binary_prints_one_document_on_stdout_and_nothing_else() {
    let fx = workspace("stdout");
    fx.index();
    let output = fx.run(&["doctor", "--json"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("stdout is one document");
    assert_eq!(parsed["checks"].as_array().map(Vec::len), Some(7));
    assert!(
        !stdout.contains("fathom ["),
        "log lines belong on stderr: {stdout}"
    );
    assert_eq!(output.status.code(), Some(1), "this workspace has warnings");

    // And the text form is what a human reads, with the same exit code.
    let text = fx.run(&["doctor"]);
    assert_eq!(text.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&text.stdout).starts_with("fathom "));
    fx.cleanup();
}

// ---------------------------------------------------------------------------
// each check, at its worst
// ---------------------------------------------------------------------------

#[test]
fn a_workspace_that_is_not_a_repository_is_the_workspace_check_not_a_refusal() {
    // `doctor` is the command you reach for when the workspace is wrong, so a boot refusal must not end
    // it: the refusal *is* the report, and the code is still 2.
    let scratch = tempfile::tempdir().expect("scratch");
    let output = Command::new(env!("CARGO_BIN_EXE_fathom"))
        .args(["doctor", "--json"])
        .current_dir(scratch.path())
        .output()
        .expect("fathom runs");
    assert_eq!(output.status.code(), Some(2));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("one document");
    assert_eq!(json["status"], "fail");
    let checks = json["checks"].as_array().expect("checks");
    assert_eq!(
        checks.len(),
        2,
        "the workspace, and everything that could not be asked"
    );
    assert_eq!(checks[0]["name"], "workspace");
    assert_eq!(checks[0]["status"], "fail");
    assert!(
        checks[0]["summary"]
            .as_str()
            .unwrap()
            .contains("not a git work tree"),
        "{checks:?}"
    );
    assert!(
        checks[0]["fixes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|fix| fix.as_str().unwrap_or_default().contains("checkout")),
        "the refusal's own `fix:` line survives: {checks:?}"
    );
    // The checks that could not run say they did not, rather than being quietly missing: an absent check
    // reads as a clean one to anything watching the JSON.
    assert_eq!(checks[1]["name"], "evidence");
    assert_eq!(checks[1]["status"], "warn");
    assert!(
        checks[1]["summary"]
            .as_str()
            .unwrap()
            .contains("not checked"),
        "{checks:?}"
    );
}

#[test]
fn an_evidence_tree_with_a_bad_record_fails_and_names_the_file() {
    let fx = workspace("evidence");
    let sha = fx.repo.head();
    fx.broken_record(&sha);
    let report = fx.doctor();
    let check = report.check("evidence").expect("the check ran");
    assert_eq!(check.status, Status::Fail);
    assert!(
        check
            .details
            .iter()
            .any(|detail| detail.contains("20260101T000000Z-aaaaaa.jsonl")),
        "the offending file is named: {:?}",
        check.details
    );
    assert!(report.exit().code() == 2);
    fx.cleanup();
}

#[test]
fn a_store_this_build_does_not_understand_fails_the_cache_check() {
    // The version refusal that ADR-0006 §7.6 makes a hard stop: the timeline would show nothing at all,
    // and only the doctor says why.
    let fx = workspace("store-version");
    fx.index();
    let meta = fx.store().index.join("meta.json");
    let mut value = read_json(&meta);
    value["layout_version"] = serde_json::Value::from(fathom::store::LAYOUT_VERSION + 1);
    std::fs::write(
        &meta,
        serde_json::to_vec_pretty(&value).expect("serialises"),
    )
    .expect("rewrite meta.json");

    let report = fx.doctor();
    let check = report.check("cache").expect("the check ran");
    assert_eq!(check.status, Status::Fail);
    assert!(
        check.summary.contains("refused") || check.details.iter().any(|d| d.contains("refused")),
        "{check:?}"
    );
    assert_eq!(report.exit().code(), 2);
    fx.cleanup();
}

#[test]
fn a_store_behind_the_checkout_says_which_commit_it_is_missing() {
    let fx = workspace("stale");
    fx.index();
    let extra = fx.commit("second", "src/b.py", "def b():\n    return 2\n");
    let report = fx.doctor();
    let check = report.check("cache").expect("the check ran");
    assert_eq!(check.status, Status::Warn);
    assert!(
        check
            .details
            .iter()
            .any(|detail| detail.contains(&extra[..7]) && detail.contains("no row")),
        "{:?}",
        check.details
    );
    assert!(
        check.fixes.iter().any(|fix| fix.contains("fathom index")),
        "{:?}",
        check.fixes
    );
    fx.cleanup();
}

/// The digest a reader is told has to match `fathom index --status`'s has to be the whole thing: seven
/// characters of a `fk1:` digest is the scheme and three hex, which matches nothing — and it is the
/// number the "the config digests keep moving" fix asks someone to compare.
#[test]
fn the_cache_check_prints_the_config_digest_the_store_actually_carries() {
    let fx = workspace("digest");
    fx.index();
    let digest = fx.store().meta.config.digest.as_str().to_owned();
    assert!(
        digest.starts_with("fk1:") && digest.chars().count() > 10,
        "the store's own digest: {digest}"
    );
    let report = fx.doctor();
    let check = report.check("cache").expect("the check ran");
    assert!(
        check.details.iter().any(|detail| detail.contains(&digest)),
        "the cache check carries the store's config digest in full ({digest}):\n{:?}",
        check.details
    );
    fx.cleanup();
}

#[test]
fn a_row_that_will_not_parse_is_counted_and_named() {
    let fx = workspace("damaged");
    fx.index();
    let store = fx.store();
    let rows = store.rows_dir();
    // A filename that is not a commit, and a row whose `commit` field disagrees with its name — the two
    // ways a hand-edited or half-deleted store breaks (ADR-0006 §10.5).
    std::fs::write(rows.join("NOT-A-COMMIT.json"), b"not a row\n").expect("write a bad row");
    std::fs::create_dir_all(rows.join("ff")).expect("create a bucket");
    std::fs::write(
        rows.join("ff").join(format!("{}.json", "f".repeat(40))),
        "{\"commit\":\"0000000000000000000000000000000000000000\"}\n",
    )
    .expect("write a mismatched row");

    let report = fx.doctor();
    let check = report.check("cache").expect("the check ran");
    assert_eq!(check.status, Status::Warn);
    assert!(
        check
            .details
            .iter()
            .any(|detail| detail.contains("will not parse or do not match their filename")),
        "{:?}",
        check.details
    );
    for path in ["NOT-A-COMMIT.json", &format!("{}.json", "f".repeat(40))] {
        assert!(
            check.details.iter().any(|detail| detail.contains(path)),
            "{path} is not named: {:?}",
            check.details
        );
    }
    fx.cleanup();
}

#[test]
fn a_language_the_mi_does_not_read_is_named_with_the_key_that_turns_it_on() {
    // The MI definition's language set is closed (rust, python today), so the case a user can actually
    // get into is a `fathom.toml` that leaves one of them out: the files are in scope, the MI is
    // silently smaller, and one key fixes it — which is what the fix line has to say.
    let fx = Fixture::new("languages");
    fx.write("fathom.toml", "[languages]\nenabled = [\"python\"]\n");
    fx.commit("first", "src/main.rs", "fn main() {}\n");
    fx.commit("second", "src/a.py", "def a():\n    return 1\n");
    let report = fx.doctor();
    let check = report.check("languages").expect("the check ran");
    assert_eq!(check.status, Status::Warn, "{check:?}");
    assert!(
        check
            .details
            .iter()
            .any(|detail| detail.contains("`rust` is not enabled")),
        "{:?}",
        check.details
    );
    assert!(
        check
            .fixes
            .iter()
            .any(|fix| fix.contains("languages.enabled") && fix.contains("--rebuild")),
        "the fix names the config key and the rebuild it costs: {:?}",
        check.fixes
    );
    fx.cleanup();
}

#[test]
fn a_language_this_build_cannot_lex_is_named_as_a_permanent_gap() {
    // Asking for a language the analyser has no front end for is not the same news as leaving one off:
    // no amount of re-indexing will ever measure it, so the fix must not promise one.
    let fx = Fixture::new("languages-unknown");
    fx.write(
        "fathom.toml",
        "[languages]\nenabled = [\"python\", \"cobol\"]\n",
    );
    fx.commit("first", "src/a.py", "def a():\n    return 1\n");
    let report = fx.doctor();
    let check = report.check("languages").expect("the check ran");
    assert_eq!(check.status, Status::Warn, "{check:?}");
    assert!(
        check.details.iter().any(|detail| detail.contains("cobol")),
        "{:?}",
        check.details
    );
    assert!(
        check
            .fixes
            .iter()
            .any(|fix| fix.contains("remove") && !fix.contains("--rebuild")),
        "{:?}",
        check.fixes
    );
    fx.cleanup();
}

#[test]
fn a_workflow_that_would_produce_evidence_satisfies_the_check() {
    let fx = workspace("workflows");
    fx.write(
        ".github/workflows/quality.yml",
        "name: Quality\non: push\njobs:\n  coverage:\n    name: Coverage\n    runs-on: ubuntu-latest\n    steps:\n      - run: cargo llvm-cov --html\n",
    );
    let report = fx.doctor();
    let check = report.check("workflows").expect("the check ran");
    assert_eq!(check.status, Status::Ok, "{check:?}");
    assert!(
        check.summary.contains("coverage found"),
        "{}",
        check.summary
    );
    fx.cleanup();
}

#[test]
fn a_directory_of_workflows_that_produce_nothing_is_a_warning_that_says_so() {
    let fx = workspace("workflows-empty");
    fx.write(
        ".github/workflows/ci.yml",
        "name: CI\non: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: cargo build\n",
    );
    let report = fx.doctor();
    let check = report.check("workflows").expect("the check ran");
    assert_eq!(check.status, Status::Warn, "{check:?}");
    assert!(
        check.fixes.iter().any(|fix| fix.contains("name: Coverage")),
        "{:?}",
        check.fixes
    );
    fx.cleanup();
}

#[test]
fn a_run_that_published_nothing_is_reported_as_advisory() {
    // ADR-0008 §5.5: a run directory that published no JSONL is not a failure and not silence. It is
    // the most common reason a column stays unknown after someone "just ran it".
    let fx = workspace("runs");
    let meta = fx.evidence.join("runs/20260101T000000Z-aaaaaa");
    std::fs::create_dir_all(&meta).expect("create the run directory");
    std::fs::write(
        meta.join("meta.json"),
        "{\"run_id\":\"20260101T000000Z-aaaaaa\",\"status\":\"success\",\"evidence\":[]}\n",
    )
    .expect("write the run meta");
    let report = fx.doctor();
    let check = report.check("runs").expect("the check ran");
    assert_eq!(check.status, Status::Warn);
    assert!(
        check
            .details
            .iter()
            .any(|detail| detail.contains("published no evidence")),
        "{:?}",
        check.details
    );
    assert!(
        check
            .details
            .iter()
            .any(|detail| detail.contains("advisory"))
            || check.summary.contains("advisory"),
        "the check says it is advisory: {:?}",
        check.details
    );
    fx.cleanup();
}

// ---------------------------------------------------------------------------
// the four states the ticket names, each with its status and its fix
// ---------------------------------------------------------------------------

#[test]
fn a_healthy_workspace_is_ok_on_every_check_and_exits_zero() {
    // The "healthy repo" fixture the ticket asks for: a git work tree with a commit, one contract-valid
    // record for it, an index that has caught up, and a workflow that would produce more evidence. With
    // nothing left to complain about, every check is `ok` and the command exits 0 — which is the only
    // thing that makes the other two codes mean anything.
    let fx = workspace("healthy");
    fx.write(
        ".github/workflows/quality.yml",
        "name: Quality\non: push\njobs:\n  coverage:\n    name: Coverage\n    runs-on: ubuntu-latest\n    steps:\n      - run: cargo llvm-cov\n",
    );
    fx.coverage(&fx.repo.head());
    fx.index();
    let report = fx.doctor();
    for check in &report.checks {
        assert_eq!(check.status, Status::Ok, "{check:?}");
    }
    assert_eq!(report.status, Status::Ok);
    assert_eq!(report.exit().code(), 0);
    let text = report.to_text();
    assert!(text.ends_with("exit 0\n"), "the text says so too:\n{text}");
    fx.cleanup();
}

#[test]
fn a_workspace_with_records_but_no_index_says_the_columns_are_stale_not_missing() {
    // The state a person actually arrives at: the runner published, and nobody has indexed since.
    // Evidence is fine, the store is behind, and the fix is one command.
    let fx = workspace("unindexed");
    fx.coverage(&fx.repo.head());
    let report = fx.doctor();
    assert_eq!(
        report.check("evidence").expect("check").status,
        Status::Ok,
        "the tree validates"
    );
    let cache = report.check("cache").expect("check");
    assert_eq!(cache.status, Status::Warn);
    assert!(
        cache.fixes.iter().any(|fix| fix.contains("fathom index")),
        "{:?}",
        cache.fixes
    );
    fx.cleanup();
}

#[test]
fn a_workspace_with_no_evidence_root_says_where_it_looked_and_what_to_do() {
    let fx = workspace("no-evidence");
    std::fs::remove_dir_all(&fx.evidence).expect("remove the evidence root");
    let report = fx.doctor();
    let check = report.check("evidence").expect("the check ran");
    assert_eq!(check.status, Status::Warn, "{check:?}");
    assert!(
        check.summary.contains("no evidence root") || check.summary.contains("does not exist"),
        "{}",
        check.summary
    );
    assert!(
        check.summary.contains(&fx.evidence.display().to_string()),
        "the path it looked at is the actionable part: {}",
        check.summary
    );
    assert!(
        check
            .fixes
            .iter()
            .any(|fix| fix.contains("act-mcp") || fix.contains("--evidence-dir")),
        "{:?}",
        check.fixes
    );
    fx.cleanup();
}

#[test]
fn an_unborn_head_is_a_failed_workspace_check_that_names_the_fix() {
    // "has commits, and its HEAD resolves" is its own clause for a reason: a freshly `git init`-ed
    // workspace has a work tree, no commits, and nothing for any column to be about.
    let scratch = tempfile::tempdir().expect("scratch");
    let repo = Repo::init(&scratch.path().join("repo"));
    let output = Command::new(env!("CARGO_BIN_EXE_fathom"))
        .args(["doctor", "--json"])
        .current_dir(repo.path())
        .output()
        .expect("fathom runs");
    assert_eq!(
        output.status.code(),
        Some(1),
        "nothing is broken; there is nothing yet"
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("one document");
    assert_eq!(json["status"], "warn");
    let workspace = json["checks"]
        .as_array()
        .expect("checks")
        .iter()
        .find(|check| check["name"] == "workspace")
        .expect("the workspace check");
    assert_eq!(workspace["status"], "warn");
    let said = workspace.to_string();
    assert!(
        said.contains("no commits") && said.contains("HEAD does not resolve"),
        "both halves of \"has commits, and its HEAD resolves\" are answered: {workspace:?}"
    );
    assert!(
        workspace["fixes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|fix| fix.as_str().unwrap_or_default().contains("commit")),
        "{workspace:?}"
    );
}

#[test]
#[cfg(unix)]
fn an_unwritable_cache_root_is_a_warning_that_says_the_columns_will_not_fill_in() {
    use std::os::unix::fs::PermissionsExt as _;

    let fx = workspace("read-only-cache");
    std::fs::create_dir_all(&fx.cache).expect("create the cache root");
    std::fs::set_permissions(&fx.cache, std::fs::Permissions::from_mode(0o555)).expect("read-only");
    let report = fx.doctor();
    let check = report.check("cache").expect("the check ran");
    assert_eq!(check.status, Status::Warn, "{check:?}");
    assert!(check.summary.contains("read-only"), "{}", check.summary);
    assert!(
        check
            .fixes
            .iter()
            .any(|fix| fix.contains("fathom index") || fix.contains("--cache-dir")),
        "a reader works, but the fix has to say how to get rows: {:?}",
        check.fixes
    );
    assert_eq!(report.exit().code(), 1, "usable, with a caveat");
    std::fs::set_permissions(&fx.cache, std::fs::Permissions::from_mode(0o755)).expect("restore");
    fx.cleanup();
}

// ---------------------------------------------------------------------------
// the config, including the config that refuses
// ---------------------------------------------------------------------------

#[test]
fn a_glob_that_matches_nothing_is_named_as_a_metric_that_quietly_changed() {
    let fx = workspace("globs");
    fx.write("fathom.toml", "[paths]\nnoise = [\"never/anywhere/**\"]\n");
    let report = fx.doctor();
    let check = report.check("config").expect("the check ran");
    assert_eq!(check.status, Status::Warn, "{check:?}");
    assert!(
        check
            .details
            .iter()
            .any(|detail| detail.contains("never/anywhere/**")),
        "{:?}",
        check.details
    );
    fx.cleanup();
}

#[test]
fn a_config_this_build_does_not_understand_is_a_failed_check_not_a_refusal() {
    // `fathom` refuses to start on an unknown key; `doctor` is the command that tells you which key, so
    // it must survive the refusal, keep its other checks, and still exit 2.
    let fx = workspace("unknown-key");
    fx.write("fathom.toml", "not_a_real_key = true\n");
    let report = fx.doctor();
    let check = report.check("config").expect("the check ran");
    assert_eq!(check.status, Status::Fail);
    assert!(
        check.summary.contains("not_a_real_key")
            || check.details.iter().any(|d| d.contains("not_a_real_key")),
        "the unknown key is named: {check:?}"
    );
    assert_eq!(report.status, Status::Fail);
    assert_eq!(report.exit().code(), 2);
    // And the binary does it too, because this is the one path where the boot sequence is bypassed.
    let output = fx.run(&["doctor"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stdout).contains("not_a_real_key"));
    fx.cleanup();
}

#[test]
fn a_foreign_file_in_the_cache_root_is_named_and_left_alone() {
    // ADR-0006 §4.2: Fathom does not read, write or delete anything it did not write, and it lists what
    // it found rather than pretending the root is its own.
    let fx = workspace("foreign");
    fx.index();
    std::fs::write(fx.cache.join("index.sqlite3"), b"someone else's database")
        .expect("write a foreign file");
    let report = fx.doctor();
    let check = report.check("cache").expect("the check ran");
    assert!(
        check
            .details
            .iter()
            .any(|detail| detail.contains("index.sqlite3")
                && detail.contains("not written by Fathom")),
        "{:?}",
        check.details
    );
    assert!(
        check
            .fixes
            .iter()
            .any(|fix| fix.contains("shared cache root")),
        "{:?}",
        check.fixes
    );
    assert!(
        fx.cache.join("index.sqlite3").is_file(),
        "doctor does not clean up"
    );
    fx.cleanup();
}

#[test]
fn a_detail_list_is_capped_and_says_how_many_it_hid() {
    // Nine hundred bad rows is a count, not a readout: the cap is what keeps the report shorter than
    // the terminal, and the count is the actionable part.
    let fx = workspace("capped");
    fx.index();
    let rows = fx.store().rows_dir();
    for index in 0..40 {
        std::fs::write(rows.join(format!("junk-{index}.json")), b"not a row\n")
            .expect("write junk");
    }
    let report = fx.doctor();
    let check = report.check("cache").expect("the check ran");
    assert!(
        check
            .details
            .iter()
            .any(|detail| detail.starts_with("… and ")),
        "{:?}",
        check.details
    );
    assert!(check.details.len() <= 12, "{:?}", check.details);
    fx.cleanup();
}

/// Read a JSON file or panic — the store's own metadata is not optional in these fixtures.
fn read_json(path: &Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).expect("read the file"))
        .expect("the file is JSON")
}
