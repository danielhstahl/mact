//! The real-act smoke (mact-f1w.19), as a test.
//!
//! Two tests, answering different questions:
//!
//! * [`the_smoke_script_passes_against_the_stub`] runs every step of
//!   `scripts/smoke-real-act.sh` against the checked-in stub `act` and stub `docker`. It needs
//!   no container runtime, so it runs in ordinary CI — and that is the point: a smoke script
//!   that only ever runs by hand is a script that rots. If the server boot, the MCP calls, the
//!   evidence reading or the summary in that bash file ever break, this goes red.
//! * [`real_act_runs_the_whole_pipeline_and_publishes_evidence`] is the deliverable's gate:
//!   the same script, real mode, real `act`, real runtime. `#[ignore]`d, because CI has
//!   neither (`docs/testing.md` §1) and because pulling an image on every push would make the
//!   suite the thing people skip. Run it by hand, or let the nightly
//!   (`.github/workflows/nightly-real-act.yml`) run it.
//!
//! The script is the subject under test, not a helper: its per-step PASS/FAIL output and exit
//! status are the assertions, so what is checked here is "the smoke ran every step it claims
//! to and said PASS", which is what an operator sees.

use std::{
    path::PathBuf,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

/// Every step the script must report. A step that silently stopped running would otherwise
/// show up as nothing, and nothing is not the same as passing.
const EXPECTED_STEPS: &[&str] = &[
    "prerequisites",
    "fixture workspace",
    "server boot",
    "tools/list",
    "list_tasks",
    "run_workflow(coverage)",
    "run_workflow(benchmark)",
    "run_status",
    "payload crossed the bind mount (F8/V1)",
    "act log captured (coverage)",
    "act --json shape (coverage)",
    "act log captured (benchmark)",
    "act --json shape (benchmark)",
    "validate-evidence",
    "record values",
    "records vs tests/realdata/expected",
];

fn script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("scripts")
        .join("smoke-real-act.sh")
}

/// A fresh scratch `--out` under the system temp dir, unique per call.
///
/// Not a `TempDir` on purpose: `--keep` exists so a human can walk the evidence tree of a
/// failed run, so the script keeps `--out` and these tests remove it themselves on success.
fn scratch_out(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after 1970")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("act-mcp-realdata-{tag}-{nanos}"));
    std::fs::create_dir_all(&path).expect("create the smoke's --out directory");
    path
}

fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

/// Run the smoke script and hand back `(exit code, stdout, stderr)`.
///
/// The report is on stdout and the diagnostics are on stderr, so the two are kept apart: the
/// verdict is "the last line of stdout", which is what an operator reads, and mixing the
/// streams in would let a late stderr note hide it.
fn run_script(out: &PathBuf, extra: &[&str]) -> (i32, String, String) {
    let run = Command::new("bash")
        .arg(script())
        .arg("--bin")
        .arg(env!("CARGO_BIN_EXE_act-mcp"))
        .arg("--out")
        .arg(out)
        .arg("--keep")
        .args(extra)
        .output()
        .expect("scripts/smoke-real-act.sh runs");
    (
        run.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&run.stdout).into_owned(),
        String::from_utf8_lossy(&run.stderr).into_owned(),
    )
}

/// Assert the script reported every step and that none of them failed.
fn assert_reported_all_steps(output: &str, err: &str, code: i32) {
    let all = format!("{output}{err}");
    for step in EXPECTED_STEPS {
        assert!(
            all.contains(step),
            "the smoke never reported the step {step:?}; its output was:\n{all}"
        );
    }
    assert!(
        !output.contains("FAILED"),
        "the script's own summary says FAILED; it is above.\n{all}"
    );
    assert!(
        output.contains("summary"),
        "the script must print its per-step summary block; got:\n{all}"
    );
    assert_eq!(
        code, 0,
        "the smoke script exits non-zero when any step fails"
    );
    assert!(
        output.trim_end().ends_with("PASS"),
        "the last line of stdout must be the verdict; got:\n{all}"
    );
}

#[test]
fn the_smoke_script_passes_against_the_stub() {
    for tool in ["jq", "python3", "git", "curl"] {
        if !have(tool) {
            // Loud, not silent: this check is what keeps scripts/smoke-real-act.sh from
            // rotting, so a machine that cannot run it says so on every run.
            eprintln!("SKIP realdata smoke self-check: {tool} is not installed");
            return;
        }
    }
    let out = scratch_out("stub");
    let (code, stdout, stderr) = run_script(&out, &["--stub"]);
    assert_reported_all_steps(&stdout, &stderr, code);
    // The stub must not be allowed to look like it settled the things only a real run can.
    assert!(
        stdout.contains("--stub"),
        "stub mode must announce itself: {stdout}"
    );
    assert!(
        stdout.contains("real act") || stdout.contains("real-act"),
        "stub mode must name the claims it is NOT establishing: {stdout}"
    );
    assert!(
        out.join("repo/.evidence").is_dir(),
        "the smoke left no evidence tree where it said it would"
    );
    std::fs::remove_dir_all(&out).expect("the smoke's --out is ours to clean up");
}

#[test]
fn the_shape_checker_checks_itself() {
    if !have("python3") {
        eprintln!("SKIP realdata report selftest: python3 is not installed");
        return;
    }
    // The shape/normalise logic that decides whether real act diverged from the extractors'
    // assumptions, checked against synthetic captures. Without this, the one component that
    // turns "act changed" into a named gap would itself be the untested thing.
    let report = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("scripts")
        .join("realdata_report.py");
    let run = Command::new("python3")
        .arg(&report)
        .arg("selftest")
        .output()
        .expect("scripts/realdata_report.py runs");
    let out = String::from_utf8_lossy(&run.stdout).into_owned();
    assert_eq!(
        run.status.code().unwrap_or(-1),
        0,
        "scripts/realdata_report.py selftest failed:\n{out}{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(
        out.contains("0 failure(s)"),
        "expected the selftest's own tally:\n{out}"
    );
}

/// The gate this ticket is about. Run it with:
///
/// ```text
/// cargo test --test realdata_smoke -- --ignored --nocapture
/// ```
///
/// It needs `act` on `PATH` and a container runtime answering `docker info`. Expect the first
/// run to be dominated by the image pull, and the second row of the script's table to be the
/// warm number worth comparing against ADR-0004 §4.2's defaults.
#[test]
#[ignore = "needs real act and a container runtime; see docs/testing.md §7 and \
            .github/workflows/nightly-real-act.yml"]
fn real_act_runs_the_whole_pipeline_and_publishes_evidence() {
    if !have("act") {
        panic!(
            "`act` is not on PATH. Install act \u{2265} 0.2.62 (<https://github.com/nektos/act>) \
             or run `scripts/smoke-real-act.sh --act PATH` directly; this test is the real-act gate."
        );
    }
    let out = scratch_out("real");
    let (code, stdout, stderr) = run_script(&out, &[]);
    assert_reported_all_steps(&stdout, &stderr, code);
    // The two categories really produced files on disk, not just a success in a response.
    for kind in ["coverage", "benchmark"] {
        let root = out.join("repo/.evidence").join(kind);
        let files = jsonl_under(&root);
        assert!(
            !files.is_empty(),
            "no {kind} JSONL under {}: a run said success and published nothing\n{stdout}{stderr}",
            root.display()
        );
    }
    std::fs::remove_dir_all(&out).expect("the smoke's --out is ours to clean up");
}

/// Every `*.jsonl` below `root`, recursively.
///
/// Hand-rolled rather than a `walkdir` dev-dependency: one ignored test is not a reason to
/// grow the dependency graph.
fn jsonl_under(root: &std::path::Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(jsonl_under(&path));
        } else if path.extension().is_some_and(|ext| ext == "jsonl") {
            found.push(path);
        }
    }
    found
}
