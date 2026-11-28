//! The repository's own workflow files, checked by the parser that will read them at runtime.
//!
//! This exists because two rules about GitHub Actions are invisible at runtime: an expression GitHub
//! accepts but evaluates differently from what it says, and a guard that only works if it is in the right
//! place. Both were real here. `mcp-evidence.yml` carried `if: ${{ !env.ACT }}` on its two driver jobs,
//! which is *not* an error — `env` is simply not among the contexts available to `jobs.<id>.if` (`github`,
//! `needs`, `vars` and `inputs` are) — so GitHub read `env` as empty, `!''` came out true, and the jobs ran
//! every time with the guard doing nothing.
//!
//! The rules are therefore about placement:
//!
//! * **R1** — no job-level `if:` may reference a context that is unavailable there. `env.` and `steps.` are
//!   the two that read plausibly and evaluate empty. A *step* `if:` using them is legal and untouched.
//! * **R2** — every job that starts the MCP action runner (`act-mcp serve`) must refuse the nested case
//!   *before* the step that starts it. `act-mcp` has no nested-run refusal of its own (its act-shaped
//!   failure vocabulary is `act_missing`, `act_spawn_failed`, `act_crashed`), so a driver job the runner
//!   asks for would run, fail with no docker socket, and cost a build to say so. A third driver job written
//!   without the refusal would nest quietly; this says no.
//! * **R3** — every job that starts the runner must build **outside the checkout**. act-mcp passes
//!   `--bind` (ADR-0003 §7.2), whose whole point is that the container's workspace path *equals* the
//!   host's (F8), so the producer job being asked for runs against this same directory: it cleans and
//!   cache-restores `target/` and builds into it with its own `CARGO_BUILD_TARGET`. Anything this job
//!   built there is not shared with the run, it is *inside* the run, and it gets replaced — which is how
//!   `mcp-evidence` came to report a missing `act-mcp` while showing `.fingerprint`, `build` and `deps`
//!   in the directory its binary had been in. `CARGO_TARGET_DIR` pinned outside the workspace is the
//!   fix, and a step that hardcodes `./target/` is the same bug in the other direction: wrong the
//!   moment the pin exists.
//!
//! All three rules are also run against a broken fixture, so none can pass by matching nothing.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Contexts GitHub does not provide to `jobs.<id>.if`, as the substring that gives them away.
const UNAVAILABLE_IN_JOB_IF: [&str; 2] = ["env.", "steps."];

fn workflows_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|crates| crates.parent())
        .expect("the crate sits two levels below the repository root")
        .join(".github/workflows")
}

/// One job: its id, the file line its key sits on (1-based), and the block of lines under it.
#[derive(Debug)]
struct Job {
    id: String,
    line: usize,
    body: String,
}

/// The jobs of one workflow file, in file order.
///
/// Deliberately line-oriented rather than structural: the things being caught are indentation shapes, and
/// a job `if:` is a key the runtime index does not keep. [`act_mcp::workflow_yaml::index_workflow_str`] is
/// called separately, in the first test, to prove the file is valid YAML to the parser the product uses.
fn jobs(text: &str) -> Vec<Job> {
    let mut out: Vec<Job> = Vec::new();
    let mut current: Option<Job> = None;
    let mut inside_jobs = false;
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim_end();
        // A blank line is not a boundary. A workflow body is full of them — between steps, and inside
        // every `run: |` block — and treating one as the end of `jobs:` silently truncated every job
        // to the lines before its first blank, which made both rules below vacuous on the real files
        // while still passing on the fixtures, where nothing is blank. That is the failure mode of a
        // line scanner nobody checks against the file it is for.
        if trimmed.trim().is_empty() {
            if let Some(job) = current.as_mut() {
                job.body.push('\n');
            }
            continue;
        }
        if !inside_jobs {
            inside_jobs = trimmed == "jobs:";
            continue;
        }
        let indent = trimmed.len() - trimmed.trim_start().len();
        if indent <= 2 {
            let is_job_key =
                indent == 2 && trimmed.contains(':') && !trimmed.trim_start().starts_with('#');
            if !is_job_key {
                // A comment at the job indent belongs to whatever job follows it; anything else at or
                // above the jobs key — a top-level key, a comment at column 0 — ends the block.
                if indent == 2 && trimmed.trim_start().starts_with('#') {
                    continue;
                }
                inside_jobs = false;
                continue;
            }
            if let Some(previous) = current.take() {
                out.push(previous);
            }
            current = Some(Job {
                id: trimmed.trim().trim_end_matches(':').to_owned(),
                line: index + 1,
                body: String::new(),
            });
            continue;
        }
        if let Some(job) = current.as_mut() {
            job.body.push_str(line);
            job.push_body_newline();
        }
    }
    if let Some(job) = current.take() {
        out.push(job);
    }
    out
}

impl Job {
    fn push_body_newline(&mut self) {
        self.body.push('\n');
    }
}

/// The job-level `if:` keys of one job, as `(file line, expression)`.
///
/// A job property sits at the shallowest indent of the body; a step's `if:` sits under a `- ` item whatever
/// indent it is written at, so step conditions are excluded by shape rather than by name.
fn job_level_if_lines(job: &Job) -> Vec<(usize, String)> {
    let plain = |line: &&str| !line.trim().is_empty() && !line.trim_start().starts_with('#');
    let not_a_step = |line: &&str| !line.trim_start().starts_with("- ");
    let Some(job_indent) = job
        .body
        .lines()
        .filter(plain)
        .filter(not_a_step)
        .map(|line| line.len() - line.trim_start().len())
        .min()
    else {
        return Vec::new();
    };
    // The body starts on the line after the job key.
    let first = job.line + 1;
    job.body
        .lines()
        .enumerate()
        .filter(|(_, line)| plain(line) && not_a_step(line))
        .filter(|(_, line)| line.len() - line.trim_start().len() == job_indent)
        .filter_map(|(offset, line)| {
            line.trim()
                .strip_prefix("if:")
                .map(|rest| (first + offset, rest.trim().to_owned()))
        })
        .collect()
}

/// The steps of one job body, in order, each one the text from its `- ` item to the next.
fn steps(job: &Job) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in job.body.lines() {
        if line.trim_start().starts_with("- ") {
            out.push(line.to_owned());
        } else if let Some(last) = out.last_mut() {
            last.push('\n');
            last.push_str(line);
        }
    }
    out
}

/// R1.
fn job_level_if_violations(file: &str, text: &str) -> Vec<String> {
    let mut problems = Vec::new();
    for job in jobs(text) {
        for (line, expression) in job_level_if_lines(&job) {
            for context in UNAVAILABLE_IN_JOB_IF {
                if expression.contains(context) {
                    problems.push(format!(
                        "{file}:{line}: job `{}` gates itself on `{expression}`: `{context}` is not one \
                         of the contexts `jobs.<id>.if` gets (`github`, `needs`, `vars`, `inputs`), so \
                         GitHub reads it as empty and the condition is silently constant. Put the check in \
                         a step, where `env` is real.",
                        job.id
                    ));
                }
            }
        }
    }
    problems
}

/// R2.
fn nested_runner_problems(file: &str, text: &str) -> Vec<String> {
    let mut problems = Vec::new();
    for job in jobs(text) {
        let steps = steps(&job);
        let Some(at) = steps.iter().position(|step| step.contains("act-mcp serve")) else {
            continue;
        };
        if !steps[..at]
            .iter()
            .any(|step| step.contains("${ACT") || step.contains("${{ ACT"))
        {
            problems.push(format!(
                "{file}: job `{}` starts the action runner (`act-mcp serve`) with nothing before it that \
                 refuses the nested case: a step testing `ACT` has to come first, or asking the runner for \
                 this job costs a full toolchain and build before it fails.",
                job.id
            ));
        }
    }
    problems
}

/// R3.
fn shared_target_problems(file: &str, text: &str) -> Vec<String> {
    let mut problems = Vec::new();
    for job in jobs(text) {
        if !job.body.contains("act-mcp serve") {
            continue;
        }

        let pin = job.body.lines().find_map(|line| {
            line.trim()
                .strip_prefix("CARGO_TARGET_DIR:")
                .map(|value| value.trim().trim_matches('\'').trim_matches('"'))
        });
        match pin {
            None => problems.push(format!(
                "{file}: job `{}` starts the action runner without pinning CARGO_TARGET_DIR outside the \
                 checkout: `--bind` means the job it asks for builds in this same `target/` and replaces \
                 what this job built there. Point CARGO_TARGET_DIR at a path outside the workspace \
                 (`/var/tmp/…`), as `dogfood-local.yml` does.",
                job.id
            )),
            Some(value) => {
                if !value.starts_with('/')
                    || value.contains("github.workspace")
                    || value.contains("runner.workspace")
                {
                    problems.push(format!(
                        "{file}: job `{}` pins CARGO_TARGET_DIR to `{value}`, which is the checkout or \
                         relative to it — the producer job reaches it there, which is the whole problem.",
                        job.id
                    ));
                }
            }
        }

        for (index, line) in job.body.lines().enumerate() {
            if line.contains("./target/") {
                problems.push(format!(
                    "{file}: job `{}` hardcodes `./target/` at `{}` — with CARGO_TARGET_DIR pinned (as \
                     this rule requires) that path is not where the binary is.",
                    job.id,
                    line.trim()
                ));
                let _ = index;
            }
        }
    }
    problems
}

fn workflow_files() -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(workflows_dir())
        .unwrap_or_else(|error| panic!("no workflows at {}: {error}", workflows_dir().display()))
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "yml" || extension == "yaml")
        })
        .collect();
    files.sort();
    files
}

#[test]
fn every_workflow_parses_and_sits_inside_the_two_rules() {
    let files = workflow_files();
    assert!(files.len() >= 5, "only {} workflow files?", files.len());
    let mut problems: Vec<String> = Vec::new();
    for path in &files {
        let text = std::fs::read_to_string(path).expect("read the workflow");
        if let Err(error) = act_mcp::workflow_yaml::index_workflow_str(&text) {
            problems.push(format!(
                "{}: the runtime parser refuses it: {error:?}",
                path.display()
            ));
        }
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        problems.extend(job_level_if_violations(&name, &text));
        problems.extend(nested_runner_problems(&name, &text));
        problems.extend(shared_target_problems(&name, &text));
    }
    assert!(
        problems.is_empty(),
        "{} workflow problem(s):\n{}",
        problems.len(),
        problems.join("\n")
    );
}

/// The exact bug this file exists for, replayed: a job-level `env` guard and a serve with no refusal.
#[test]
fn the_guards_fire_on_the_shape_they_were_written_for() {
    let broken = r#"name: Broken
on: push
jobs:
  driver:
    name: A driver whose guard does nothing
    if: ${{ !env.ACT }}
    runs-on: ubuntu-24.04
    steps:
      - name: Start the MCP action runner
        run: ./target/release/act-mcp serve --workspace "$PWD" &
"#;
    let violations = job_level_if_violations("broken.yml", broken);
    assert_eq!(
        violations.len(),
        1,
        "the guard must be named exactly once: {violations:?}"
    );
    assert!(
        violations[0].contains("driver") && violations[0].contains("env."),
        "{}",
        violations[0]
    );
    assert!(
        violations[0].contains("broken.yml:6"),
        "and named at its own line: {}",
        violations[0]
    );
    assert_eq!(
        nested_runner_problems("broken.yml", broken).len(),
        1,
        "a job that serves without refusing must be caught"
    );
    let shared = shared_target_problems("broken.yml", broken);
    assert!(
        shared.iter().any(|p| p.contains("CARGO_TARGET_DIR"))
            && shared.iter().any(|p| p.contains("./target/")),
        "serving without a pin, and hardcoding ./target, must both be caught: {shared:?}"
    );

    // A pin that points back into the checkout is the same hole with a variable named on it.
    let circular = r#"name: Circular
on: push
jobs:
  driver:
    runs-on: ubuntu-24.04
    env:
      CARGO_TARGET_DIR: ${{ github.workspace }}/target
    steps:
      - name: Start the MCP action runner
        run: "$CARGO_TARGET_DIR/release/act-mcp serve" &
"#;
    let circular_problems = shared_target_problems("circular.yml", circular);
    assert_eq!(
        circular_problems.len(),
        1,
        "a pin inside the workspace must be refused: {circular_problems:?}"
    );
}

/// And the shape that replaced it must pass, including the two legal things it sits next to: a step-level
/// `if:` on `env`, and a step `if:` on `failure()`.
#[test]
fn the_fixed_shape_passes_both_rules() {
    let fixed = r#"name: Fixed
on: push
jobs:
  driver:
    name: A driver that refuses first
    runs-on: ubuntu-24.04
    env:
      ACT_VERSION: '0.2.97'
      CARGO_TARGET_DIR: /var/tmp/guard-target
    steps:
      - name: Refuse a nested runner
        run: if [[ -n "${ACT:-}" ]]; then exit 1; fi
      - name: Show the log
        if: failure()
        run: cat log
      - name: Install act ${{ env.ACT_VERSION }}
        run: echo installing
      - name: Start the MCP action runner
        if: ${{ env.ACT }}
        run: "$CARGO_TARGET_DIR/release/act-mcp serve" &
"#;
    assert!(job_level_if_violations("fixed.yml", fixed).is_empty());
    assert!(nested_runner_problems("fixed.yml", fixed).is_empty());
    assert!(
        shared_target_problems("fixed.yml", fixed).is_empty(),
        "{:?}",
        shared_target_problems("fixed.yml", fixed)
    );
}

/// `jobs()` keys on the `jobs:` block. A line scanner's favourite mistake is reading a top-level key —
/// `env:`, `on:`, `permissions:` — as a job, so the mistake is asserted against rather than trusted.
#[test]
fn the_job_scan_does_not_mistake_other_blocks_for_jobs() {
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for path in workflow_files() {
        let text = std::fs::read_to_string(&path).expect("read");
        for job in jobs(&text) {
            assert!(
                job.body.contains("runs-on") || job.body.contains("uses"),
                "{}: `{}` at line {} is not a job",
                path.display(),
                job.id,
                job.line
            );
            *seen.entry(job.id).or_default() += 1;
        }
    }
    assert!(
        seen.contains_key("coverage-run"),
        "the driver job should be found: {seen:?}"
    );
    for key in [
        "push",
        "pull_request",
        "workflow_dispatch",
        "env",
        "concurrency",
        "permissions",
        "jobs",
    ] {
        assert!(
            !seen.contains_key(key),
            "`{key}` was read as a job: {seen:?}"
        );
    }
}
