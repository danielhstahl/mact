//! The `.evidence` contract, checked from outside the writer.
//!
//! `src/evidence`'s unit tests prove each half separately. What this binary proves is the
//! join: that the bytes the writer publishes are the bytes the fixtures pin, that both
//! checkers (the Rust one and `scripts/validate_evidence.py`, the independent
//! implementation the ADR's verification table names) reach the same verdict about the
//! same tree, and that the layout on disk is the one
//! [`docs/adr/0002-evidence-jsonl-contract.md`](../docs/adr/0002-evidence-jsonl-contract.md)
//! §1.1 draws.
//!
//! The fixture corpus is `tests/e2e/testdata/{expected,invalid}`, shared with the Python
//! checker on purpose: one corpus, two implementations, no room for either to drift.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::Command,
};

use act_mcp::evidence::{
    validate::{validate_tree, ValidateOptions},
    EvidenceRoot, RunRecords,
};

/// Every expected record fixture: one per shape the contract pins (ADR-0002 §4).
const EXPECTED_RECORDS: [&str; 5] = [
    "benchmark",
    "benchmark-failed",
    "coverage",
    "coverage-istanbul",
    "coverage-log-scrape",
];

fn testdata() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/e2e/testdata")
}

fn expected(name: &str) -> PathBuf {
    testdata().join("expected").join(format!("{name}.jsonl"))
}

fn invalid_dir() -> PathBuf {
    testdata().join("invalid")
}

fn read(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_else(|reason| panic!("read {}: {reason}", path.display()))
}

/// Publish every expected fixture into one fresh evidence root, the way a job would:
/// parse the payload, then hand the batch to the writer once.
///
/// The returned root holds the `TempDir`, which the caller must keep alive.
struct Published {
    /// Kept so the evidence root has somewhere to live that the test owns.
    dir: tempfile::TempDir,
    root: PathBuf,
}

async fn publish_everything() -> Published {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join(".evidence");
    let writer = EvidenceRoot::new(&root).expect("evidence root");
    for name in EXPECTED_RECORDS {
        let bytes = read(&expected(name));
        let records = RunRecords::from_jsonl(&bytes)
            .unwrap_or_else(|reason| panic!("{name}.jsonl should parse: {reason}"));
        // One batch per published file is what the ADR's one-file-per-run rule makes
        // natural: a run's records travel together.
        let published = writer
            .publish(&records)
            .await
            .unwrap_or_else(|reason| panic!("publish {name}: {reason}"))
            .expect("a non-empty batch publishes something");
        assert_eq!(
            read(&published.path),
            bytes,
            "{name}.jsonl did not round-trip byte for byte"
        );
    }
    Published { dir, root }
}

#[tokio::test]
async fn the_writer_reproduces_every_expected_fixture_byte_for_byte() {
    // The assertion is inside `publish_everything` where the diff is named; this test
    // exists so a failure says which contract it broke.
    publish_everything().await;
}

#[tokio::test]
async fn the_published_tree_is_the_layout_adr_0002_draws() {
    let published = publish_everything().await;
    let root = &published.root;

    assert!(
        root.join("README.md").is_file(),
        "the stub README is written once"
    );
    for kind in ["benchmark", "coverage", "runs"] {
        assert!(root.join(kind).is_dir(), "{kind}/ is part of the layout");
    }
    // One file per run, under the full SHA, named by run_id.
    let mut files = Vec::new();
    walk(root, &mut files);
    let names: BTreeSet<String> = files
        .iter()
        .map(|path| {
            path.strip_prefix(root)
                .expect("under the root")
                .to_string_lossy()
                .replace('\\', "/")
                .to_owned()
        })
        .collect();
    assert_eq!(
        names,
        [
            "README.md",
            "benchmark/2f7c1a4b9e0d3c6a5b8f1e2d4c6a8b0f3e5d7c91/20261110T004203Z-a1b2c3.jsonl",
            "benchmark/2f7c1a4b9e0d3c6a5b8f1e2d4c6a8b0f3e5d7c91/20261110T041530Z-bbc0dd.jsonl",
            "coverage/2f7c1a4b9e0d3c6a5b8f1e2d4c6a8b0f3e5d7c91/20261110T011507Z-d4e5f6.jsonl",
            "coverage/2f7c1a4b9e0d3c6a5b8f1e2d4c6a8b0f3e5d7c91/20261110T021244Z-0f9e8d.jsonl",
            "coverage/2f7c1a4b9e0d3c6a5b8f1e2d4c6a8b0f3e5d7c91/20261110T030900Z-77aa55.jsonl",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<String>>(),
        "unexpected files under the evidence root"
    );
    // And the tree validates: the checker is a consumer of the layout, not a bystander.
    let report = validate_tree(&ValidateOptions {
        paths: vec![root.clone()],
        ..Default::default()
    })
    .expect("validate");
    assert!(
        report.is_ok(),
        "the writer's own output must validate:\n{}",
        report
            .problems
            .iter()
            .map(|problem| problem.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert_eq!(
        report.stats.total(),
        20,
        "the five fixtures hold 20 records: mact-f1w.12 re-derived the benchmark pair from \
         its input fixtures, which added the per-step duration records"
    );
    // And nothing escaped: the writer created one directory and nothing beside it
    // (this is not a git repo, so even the `.gitignore` maintenance is a no-op here).
    let top: Vec<String> = std::fs::read_dir(published.dir.path())
        .expect("read tempdir")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(top, [".evidence".to_owned()]);
    drop(published);
}

#[test]
fn the_rust_checker_accepts_the_expected_fixtures_in_place() {
    // The same corpus, validated where it lies: the flat fixture directory must validate
    // too, because that is how the e2e harness points at it.
    let report = validate_tree(&ValidateOptions {
        paths: vec![testdata().join("expected")],
        ..Default::default()
    })
    .expect("validate");
    assert!(
        report.is_ok(),
        "expected fixtures must be clean:\n{}",
        report
            .problems
            .iter()
            .map(|problem| problem.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert_eq!(report.record_files, 5);
    assert_eq!(
        report.meta_files, 1,
        "expected/meta.json is the manifest fixture"
    );
    assert_eq!(
        report.stats.total(),
        20,
        "the Python checker counts 20 records here"
    );
}

#[test]
fn the_rust_checker_names_a_problem_in_every_invalid_fixture() {
    // One corpus, and every file in it must be refused: a fixture that neither checker
    // catches is a fixture that teaches nothing.
    let dir = invalid_dir();
    let report = validate_tree(&ValidateOptions {
        paths: vec![dir.clone()],
        ..Default::default()
    })
    .expect("validate");
    assert!(!report.is_ok(), "the invalid corpus must not validate");

    let flagged: BTreeSet<PathBuf> = report
        .problems
        .iter()
        .map(|problem| problem.path.clone())
        .collect();
    let mut missing = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("read invalid fixtures") {
        let path = entry.expect("entry").path();
        if path.extension().is_some_and(|ext| ext == "jsonl") && !flagged.contains(&path) {
            missing.push(path.file_name().expect("name").to_owned());
        }
    }
    assert!(
        missing.is_empty(),
        "no problem reported for {:?}",
        missing
            .iter()
            .map(|name| name.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        report.exit_code(),
        1,
        "findings are exit 1, so CI can branch on the checker it has"
    );
}

#[tokio::test]
async fn runs_and_incoming_subtrees_are_not_read_as_records() {
    // ADR-0002 §1.1: `runs/` and `incoming/` hold per-run material, not the cross-commit
    // contract, so a broken act.jsonl there must not fail an evidence audit.
    let published = publish_everything().await;
    let run_id = "20261110T004203Z-a1b2c3";
    let runs = published.root.join("runs").join(run_id);
    let incoming = published.root.join("incoming").join(run_id);
    std::fs::create_dir_all(&runs).expect("runs dir");
    std::fs::create_dir_all(&incoming).expect("incoming dir");
    std::fs::write(runs.join("act.jsonl"), "not json at all\n").expect("write act log");
    std::fs::write(incoming.join("benchmark.jsonl"), "{oops\n").expect("write payload");

    let report = validate_tree(&ValidateOptions {
        paths: vec![published.root.clone()],
        ..Default::default()
    })
    .expect("validate");
    assert!(
        report.is_ok(),
        "runs/ and incoming/ are not contract records:\n{}",
        report
            .problems
            .iter()
            .map(|problem| problem.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    );
    drop(published);
}

#[test]
fn validation_refuses_to_claim_success_when_there_is_nothing_to_check() {
    // A vacuous pass is the failure mode a CI step must not have: `.evidence` missing
    // entirely is an error (exit 2), not a green tick.
    let empty = tempfile::tempdir().expect("tempdir");
    let error = validate_tree(&ValidateOptions {
        paths: vec![empty.path().to_path_buf()],
        ..Default::default()
    })
    .expect_err("nothing to validate is not a pass");
    assert!(matches!(
        error,
        act_mcp::evidence::ValidateError::NothingToValidate { .. }
    ));
}

// ---------------------------------------------------------------------------------------
// Cross-check against scripts/validate_evidence.py, the second implementation.
// ---------------------------------------------------------------------------------------

/// A Python that can `import jsonschema`, discovered the way `tests/e2e/run.sh` discovers
/// `act`: explicit override first, then the plain interpreter, then `uv` as the network
//     fallback. `None` means this machine cannot run the Python checker at all, in which
/// case the cross-check prints a note instead of failing an unrelated checkout.
fn python_with_jsonschema() -> Option<(String, Vec<String>)> {
    let candidates: Vec<(String, Vec<String>)> = match std::env::var("ACT_MCP_VALIDATE_PYTHON") {
        Ok(program) => vec![(program, Vec::new())],
        Err(_) => vec![
            ("python3".to_owned(), Vec::new()),
            (
                "uv".to_owned(),
                vec![
                    "run".to_owned(),
                    "--with".to_owned(),
                    "jsonschema".to_owned(),
                    "--".to_owned(),
                    "python3".to_owned(),
                ],
            ),
        ],
    };
    candidates.into_iter().find(|(program, prefix)| {
        let mut args = prefix.clone();
        args.push("-c".to_owned());
        args.push("import jsonschema".to_owned());
        Command::new(program)
            .args(&args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
}

/// Run the Python checker over `paths`, returning its exit code and stderr.
fn python_checker(program: &str, prefix: &[String], paths: &[&Path]) -> (Option<i32>, String) {
    let mut command = Command::new(program);
    command
        .args(prefix)
        .arg("scripts/validate_evidence.py")
        .current_dir(env!("CARGO_MANIFEST_DIR"));
    for path in paths {
        command.arg(path);
    }
    let output = command
        .stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .output()
        .expect("run the python checker");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    (output.status.code(), stderr)
}

/// The files a checker complained about, out of `file:line: message` diagnostics.
fn complained_about(stderr: &str) -> BTreeSet<String> {
    stderr
        .lines()
        .filter_map(|line| line.split_once(":").map(|(path, _)| path.to_owned()))
        .filter(|path| path.ends_with(".jsonl"))
        .collect()
}

#[tokio::test]
async fn the_python_checker_accepts_what_the_writer_wrote() {
    // The strongest statement of the two-checker rule in ADR-0002 §5/§7: a record the
    // writer produced must be accepted by an implementation that shares no code with it.
    let Some((program, prefix)) = python_with_jsonschema() else {
        eprintln!("note: no python3 with jsonschema (and no uv); skipping the cross-checker test");
        return;
    };
    let published = publish_everything().await;
    let (code, stderr) = python_checker(&program, &prefix, &[&published.root]);
    assert_eq!(
        code,
        Some(0),
        "the python checker rejected the writer's output:\n{stderr}"
    );
    drop(published);
}

#[test]
fn both_checkers_refuse_the_same_invalid_files() {
    let Some((program, prefix)) = python_with_jsonschema() else {
        eprintln!("note: no python3 with jsonschema (and no uv); skipping the cross-checker test");
        return;
    };
    let dir = invalid_dir();
    let (code, stderr) = python_checker(&program, &prefix, &[&dir]);
    assert_eq!(
        code,
        Some(1),
        "the python checker must fail the invalid corpus"
    );
    let python = complained_about(&stderr);
    let report = validate_tree(&ValidateOptions {
        paths: vec![dir.clone()],
        ..Default::default()
    })
    .expect("validate");
    let rust: BTreeSet<String> = report
        .problems
        .iter()
        .map(|problem| problem.path.to_string_lossy().into_owned())
        .collect();
    assert!(
        !python.is_empty(),
        "no diagnostics parsed from the python checker:\n{stderr}"
    );
    assert_eq!(
        python, rust,
        "the two checkers must agree about which files are bad"
    );
}

/// Every file under `root`, recursively, sorted.
fn walk(root: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(root).expect("read dir") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            walk(&path, out);
        } else {
            out.push(path);
        }
    }
    out.sort();
}
