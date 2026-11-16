//! argv construction — the only place in the codebase that knows act's command line.
//!
//! [`build_argv`] is **pure**: no filesystem, no clock, no environment lookup. That is
//! what makes it golden-testable, and ADR-0003 §7.4's command line is the golden value —
//! the test `adr_0003_7_4_golden_command_line_is_the_default_argv` asserts it token for
//! token, and `tests/runner_execution.rs` proves the stub `act` receives exactly that
//! vector, so the golden is not a private opinion of this module.
//!
//! The rules it exists to enforce (ADR-0003 §7.1):
//!
//! 1. absolute paths only (`--directory`, `--workflows`, the `--*-file` flags), so act's
//!    own relative-path resolution (F1) never has an opinion;
//! 2. never a flag that does not exist across the supported act range (0.2.62..=0.2.97),
//!    because an unknown flag is a fatal cobra error that looks like a failing workflow;
//! 3. never a value-less `--secret` (F11: act prompts on stdin and hangs until the
//!    pre-start budget expires) — in fact never `--secret` at all: secrets travel in the
//!    `--secret-file` (ADR-0001 §2.7), so no secret value is ever in `ps` output;
//! 4. accumulate-able flags (`--input`, `--var`, `--matrix`, `--env`) emitted in sorted
//!    key order, so argv is byte-stable and `meta.json.act.argv` is diffable;
//! 5. no caller-controlled passthrough of arbitrary flags (ADR-0003 §14.6): argv is a
//!    function of the call and the operator's configuration, nothing else.
//!
//! ## Where the ticket and the accepted ADRs disagreed
//!
//! The ticket asked for `-W`/`-j`/`--no-recurse`, an optional `--container-daemon-socket`,
//! and `--secret k=v`. The ADRs are the accepted contract and are explicit, so:
//!
//! * `-W`/`-j` are spelled `--workflows`/`--job` — same flags, long form, which is what
//!   ADR-0003 §7.2 pins and what `meta.json.act.argv` therefore records;
//! * `--no-recurse` is **not** emitted: it only governs recursion into a *directory*
//!   given to `--workflows`, and we always pass one resolved file. Emitting it would
//!   change the argv mact-f1w.15 asserts against for no behavioural change;
//! * `--container-daemon-socket` is on ADR-0003 §7.5's never-passed list (mact-f1w.13/.19's
//!   lever, deliberately left at act's default), so the runner never emits it; `Config`'s
//!   value of that name is only the socket the reachability probe dials (mact-f1w.6);
//! * `--secret k=v` is replaced by the secret file — a strictly stronger version of the
//!   ticket's "secrets never logged" requirement, since it also keeps them out of `ps`;
//! * `--eventpath`/the positional event name are kept as an opt-in [`ActEvent`] (`None` by
//!   default) because the ticket asked for the seam while ADR-0003 §7.5 declines an event
//!   concept in the wire contract. Nothing in the tool surface sets it today and the
//!   default argv is unchanged.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use super::RunSpec;

/// act's flag for a JSON event payload (F1: resolved against `--directory` when
/// relative, so it is only ever passed absolute).
pub const EVENTPATH_FLAG: &str = "--eventpath";

/// act's flag for the dry-run mode the ticket asks for.
pub const DRYRUN_FLAG: &str = "--dryrun";

/// An event for act to run the workflow against.
///
/// `None` — the default, and everything the wire contract produces — means "act's own
/// default": `push`, or the workflow's single event. See the module docs for why the
/// seam exists at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActEvent {
    /// The positional event name (`act pull_request`).
    Name(String),
    /// `--eventpath <file>`: a JSON payload to feed the run.
    Payload(PathBuf),
}

impl ActEvent {
    /// Whether a [`ActEvent::Name`] is spellable as a GitHub event name.
    ///
    /// `push`, `pull_request`, `workflow_dispatch`, `release`, repository-dispatch names.
    /// Anything else is a typo worth refusing **before** act is spawned, and a name
    /// starting with `-` would be read by act as a flag.
    #[must_use]
    pub fn is_valid_name(name: &str) -> bool {
        !name.is_empty()
            && name.len() <= 100
            && !name.starts_with('-')
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    }
}

/// Build act's command line for one run: `[program, flag, value, …]`.
///
/// Element 0 is the program, so a caller may `Command::new(&argv[0]).args(&argv[1..])`.
/// Order is normative (ADR-0003 §7.2: "order is normative (it makes `act.argv`
/// diffable)"). The extras this module adds after the ADR's base list are appended in the
/// order below and **only when non-empty**, so a run that uses none of them produces
/// exactly ADR-0003 §7.2's argv:
///
/// ```text
/// … --input k=v    (sorted)   # ADR-0003 §7.2 / ADR-0001 §2.7
/// … --var   k=v    (sorted)   \  the ticket's caller maps, appended after the base
/// … --matrix k:v   (sorted)   /  list so the base stays byte-identical without them
/// … --env   k=v    (sorted)   /
/// … --eventpath <p> | <event name>   # opt-in (ActEvent)
/// … --dryrun                      # opt-in validation mode
/// … --defaultbranch <branch>      # worktree mode only (ADR-0003 §7.2: last)
/// ```
#[must_use]
pub fn build_argv(spec: &RunSpec) -> Vec<String> {
    let mut argv = vec![spec.act_binary.to_string_lossy().into_owned()];

    // ADR-0003 §7.2, in its order. Everything is unconditional except --bind, which
    // --act-no-bind (§7.7) turns off, and --use-gitignore (kept as a field for the same
    // degraded-mode reasoning).
    push_pair(&mut argv, "--directory", &spec.tree);
    push_pair(&mut argv, "--workflows", &spec.workflow_file);
    push_pair_str(&mut argv, "--job", &spec.job);
    if spec.bind {
        argv.push("--bind".to_owned());
    }
    argv.push("--reuse=false".to_owned());
    argv.push("--rm".to_owned());
    argv.push(format!(
        "--pull={}",
        if spec.pull { "true" } else { "false" }
    ));
    argv.push("--rebuild".to_owned());
    if spec.use_gitignore {
        argv.push("--use-gitignore".to_owned());
    }
    argv.push("--json".to_owned());
    argv.push("--quiet=false".to_owned());

    // The secret file always exists (ADR-0003 §7.7: it always carries GITHUB_TOKEN, so
    // act never shells out to the operator's `gh`), and the inert file is what stops a
    // repository's own `.env`/`.vars`/`.input` from changing what is measured (§7.3).
    push_pair(&mut argv, "--secret-file", &spec.secret_file_path());
    if spec.project_config_files_inert() {
        for flag in ["--var-file", "--env-file", "--input-file"] {
            push_pair(&mut argv, flag, &spec.inert_file_path());
        }
    }

    // ADR-0002 §3.1: the payload transport, told to the job through act's own --env.
    push_pair_str(
        &mut argv,
        "--env",
        &format!("ACTMCP_EVIDENCE_OUT={}", spec.payload_dir.to_string_lossy()),
    );
    push_pair_str(
        &mut argv,
        "--env",
        &format!("ACTMCP_RUN_ID={}", spec.run_id.as_str()),
    );

    // The ticket's caller/operator maps, sorted, appended after the ADR base.
    push_key_values(&mut argv, "--input", &spec.inputs, spec);
    push_key_values(&mut argv, "--var", &spec.vars, spec);
    for (key, value) in &spec.matrix {
        push_pair_str(
            &mut argv,
            "--matrix",
            &format!("{}:{}", key, expand_run_id(value, spec)),
        );
    }
    push_key_values(&mut argv, "--env", &spec.extra_env, spec);

    match &spec.event {
        Some(ActEvent::Payload(path)) => push_pair(&mut argv, EVENTPATH_FLAG, path),
        Some(ActEvent::Name(name)) => argv.push(name.clone()),
        None => {}
    }

    if spec.dry_run {
        argv.push(DRYRUN_FLAG.to_owned());
    }
    if let Some(branch) = &spec.default_branch {
        push_pair_str(&mut argv, "--defaultbranch", branch);
    }

    argv
}

/// The `<tree>/.evidence/incoming/<run_id>` default of ADR-0003 §7.2.
#[must_use]
pub fn default_payload_dir(tree: &Path, run_id: &str) -> PathBuf {
    tree.join(".evidence").join("incoming").join(run_id)
}

/// Whether `argv` (as built by [`build_argv`]) contains `needle` as a whole token.
#[must_use]
pub fn argv_contains(argv: &[String], needle: &str) -> bool {
    argv.iter().any(|token| token == needle)
}

/// Whether `argv` carries `flag` immediately followed by `value`.
#[must_use]
pub fn argv_carries(argv: &[String], flag: &str, value: &str) -> bool {
    argv.windows(2)
        .any(|pair| pair[0] == flag && pair[1] == value)
}

fn push_pair(argv: &mut Vec<String>, flag: &str, path: &Path) {
    argv.push(flag.to_owned());
    argv.push(path.to_string_lossy().into_owned());
}

fn push_pair_str(argv: &mut Vec<String>, flag: &str, value: &str) {
    argv.push(flag.to_owned());
    argv.push(value.to_owned());
}

fn push_key_values(
    argv: &mut Vec<String>,
    flag: &str,
    map: &BTreeMap<String, String>,
    spec: &RunSpec,
) {
    for (key, value) in map {
        push_pair_str(
            argv,
            flag,
            &format!("{}={}", key, expand_run_id(value, spec)),
        );
    }
}

/// Expand the `{run_id}` placeholder in a caller-supplied value.
///
/// A job that wants its run identity in a `--var` must not have to know it in advance,
/// and a caller must not be able to *choose* a run id (ADR-0002 §1.2), so the server
/// fills the placeholder in here.
fn expand_run_id(value: &str, spec: &RunSpec) -> String {
    value.replace("{run_id}", spec.run_id.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{ProjectConfigFiles, RunId, RunSpec};

    const SCRATCH: &str = "/run/user/1000/act-mcp-1000/20261110T004203Z-a1b2c3";
    const RUN: &str = "20261110T004203Z-a1b2c3";

    /// The ADR-0003 §7.4 scenario verbatim: workspace `/srv/ws`, run
    /// `20261110T004203Z-a1b2c3`, `head_only`, one input.
    fn scenario() -> RunSpec {
        let run_id = RunId::parse(RUN).expect("golden run id");
        let tree = PathBuf::from("/srv/ws");
        let mut spec = RunSpec::new(
            run_id,
            "act",
            tree.clone(),
            tree.join(".github/workflows/coverage.yml"),
            "coverage",
        );
        spec.scratch_dir = PathBuf::from(SCRATCH);
        spec.inputs = BTreeMap::from([("fuzz_rounds".to_owned(), "4".to_owned())]);
        spec
    }

    /// ADR-0003 §7.4's golden command line, tokenised.
    fn golden() -> Vec<String> {
        shlex(&format!(
            "act --directory /srv/ws \
             --workflows /srv/ws/.github/workflows/coverage.yml \
             --job coverage --bind --reuse=false --rm --pull=false --rebuild --use-gitignore \
             --json --quiet=false \
             --secret-file {SCRATCH}/secrets.env \
             --var-file {SCRATCH}/inert.conf \
             --env-file {SCRATCH}/inert.conf \
             --input-file {SCRATCH}/inert.conf \
             --env ACTMCP_EVIDENCE_OUT=/srv/ws/.evidence/incoming/{RUN} \
             --env ACTMCP_RUN_ID={RUN} \
             --input fuzz_rounds=4"
        ))
    }

    /// Whitespace tokeniser good enough for a golden line with no quoting.
    fn shlex(line: &str) -> Vec<String> {
        line.split_whitespace()
            .filter(|token| *token != "\\")
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn adr_0003_7_4_golden_command_line_is_the_default_argv() {
        assert_eq!(build_argv(&scenario()), golden());
    }

    #[test]
    fn a_benchmark_run_differs_from_a_coverage_run_only_in_job_and_file() {
        let mut spec = scenario();
        spec.job = "benchmark".to_owned();
        spec.workflow_file = PathBuf::from("/srv/ws/.github/workflows/benchmark.yml");
        spec.inputs = BTreeMap::new();

        let mut expected = golden();
        expected.truncate(expected.len() - 2); // the trailing `--input fuzz_rounds=4`
        expected[4] = "/srv/ws/.github/workflows/benchmark.yml".to_owned();
        expected[6] = "benchmark".to_owned();
        assert_eq!(build_argv(&spec), expected);
    }

    #[test]
    fn vars_inputs_and_matrix_are_sorted_and_come_after_the_base_argv() {
        let mut spec = scenario();
        spec.inputs = BTreeMap::from([
            ("zeta".to_owned(), "1".to_owned()),
            ("alpha".to_owned(), "2".to_owned()),
        ]);
        spec.vars = BTreeMap::from([("maven_opts".to_owned(), "-Xmx2g".to_owned())]);
        spec.matrix = BTreeMap::from([("node".to_owned(), "20".to_owned())]);
        spec.extra_env = BTreeMap::from([("RUST_BACKTRACE".to_owned(), "1".to_owned())]);

        let mut expected = golden();
        expected.truncate(expected.len() - 2); // this spec replaced the scenario's input
        expected.extend(shlex(
            "--input alpha=2 --input zeta=1 --var maven_opts=-Xmx2g --matrix node:20 --env RUST_BACKTRACE=1",
        ));
        assert_eq!(build_argv(&spec), expected);
    }

    #[test]
    fn a_run_id_placeholder_is_filled_by_the_server_not_the_caller() {
        let mut spec = scenario();
        spec.vars = BTreeMap::from([("out".to_owned(), "/tmp/{run_id}/out.json".to_owned())]);
        assert!(argv_carries(
            &build_argv(&spec),
            "--var",
            &format!("out=/tmp/{RUN}/out.json")
        ));
    }

    #[test]
    fn dryrun_appends_the_flag_act_spells_dryrun() {
        let mut spec = scenario();
        spec.dry_run = true;
        let argv = build_argv(&spec);
        assert_eq!(argv.last().map(String::as_str), Some(DRYRUN_FLAG));
        assert_eq!(argv.len(), golden().len() + 1);
    }

    #[test]
    fn worktree_mode_changes_the_three_tree_paths_and_appends_defaultbranch() {
        let tree = format!("{SCRATCH}/worktree");
        let mut spec = scenario();
        spec.tree = PathBuf::from(&tree);
        spec.workflow_file = PathBuf::from(&tree).join(".github/workflows/coverage.yml");
        spec.payload_dir = default_payload_dir(Path::new(&tree), spec.run_id.as_str());
        spec.default_branch = Some("main".to_owned());

        let argv = build_argv(&spec);
        assert_eq!(argv[2], tree);
        assert_eq!(argv[4], format!("{tree}/.github/workflows/coverage.yml"));
        assert!(argv_carries(
            &argv,
            "--env",
            &format!("ACTMCP_EVIDENCE_OUT={tree}/.evidence/incoming/{RUN}")
        ));
        assert_eq!(&argv[argv.len() - 2..], ["--defaultbranch", "main"]);
    }

    #[test]
    fn act_no_bind_drops_only_the_bind_flag() {
        let mut spec = scenario();
        spec.bind = false;
        let argv = build_argv(&spec);
        assert!(!argv_contains(&argv, "--bind"));
        assert_eq!(golden().len(), argv.len() + 1);
        assert!(argv_contains(&argv, "--pull=false"), "--pull is one token");
    }

    #[test]
    fn discover_mode_passes_no_inert_project_files_but_keeps_the_secret_file() {
        let mut spec = scenario();
        spec.project_config_files = ProjectConfigFiles::Discover;
        let argv = build_argv(&spec);
        for flag in ["--var-file", "--env-file", "--input-file"] {
            assert!(
                !argv_contains(&argv, flag),
                "{flag} passed in discover mode"
            );
        }
        assert!(argv_carries(
            &argv,
            "--secret-file",
            &format!("{SCRATCH}/secrets.env")
        ));
        // Losing the inert file must not shift anything else.
        assert_eq!(argv.len(), golden().len() - 6);
    }

    #[test]
    fn event_seam_is_opt_in_and_sits_before_dryrun_and_defaultbranch() {
        let mut spec = scenario();
        spec.event = Some(ActEvent::Payload(PathBuf::from("/tmp/event.json")));
        spec.inputs = BTreeMap::new();
        spec.dry_run = true;
        spec.default_branch = Some("main".to_owned());
        let argv = build_argv(&spec);
        let index = argv
            .iter()
            .position(|token| token == EVENTPATH_FLAG)
            .expect("--eventpath present when an event payload is given");
        assert_eq!(argv[index + 1], "/tmp/event.json");
        assert_eq!(
            &argv[argv.len() - 5..],
            [
                EVENTPATH_FLAG,
                "/tmp/event.json",
                DRYRUN_FLAG,
                "--defaultbranch",
                "main"
            ]
        );

        let mut spec = scenario();
        spec.event = Some(ActEvent::Name("pull_request".to_owned()));
        assert_eq!(
            build_argv(&spec).last().map(String::as_str),
            Some("pull_request")
        );

        // And with no event at all the golden argv is untouched.
        assert_eq!(build_argv(&scenario()), golden());
    }

    #[test]
    fn event_names_that_are_not_names_are_refused_before_spawn() {
        for good in [
            "push",
            "pull_request",
            "workflow_dispatch",
            "release",
            "a.b-c_1",
        ] {
            assert!(ActEvent::is_valid_name(good), "{good} should be valid");
        }
        for bad in ["", "--json", "rm -rf /", "-x", "push\n"] {
            assert!(!ActEvent::is_valid_name(bad), "{bad:?} should be invalid");
        }
    }

    #[test]
    fn never_passed_flags_and_secret_values_never_reach_argv() {
        let mut spec = scenario();
        spec.secrets = BTreeMap::from([
            ("GITHUB_TOKEN".to_owned(), "ghp_secret_value".to_owned()),
            ("NPM_TOKEN".to_owned(), "npm_secret_value".to_owned()),
        ]);
        let argv = build_argv(&spec);
        for forbidden in [
            "--secret",
            "-s",
            "--no-recurse",
            "--container-daemon-socket",
            "--act-arg",
            "--network",
            "-P",
            "--platform",
            "--default-image",
            "--artifact-server-path",
            "--container-options",
            "--container-working-directory",
            "ghp_secret_value",
            "npm_secret_value",
        ] {
            assert!(
                !argv.iter().any(|token| token == forbidden),
                "`{forbidden}` reached argv: {argv:?}"
            );
        }
        assert!(
            !argv.iter().any(|token| token.contains("secret_value")),
            "a secret value reached argv: {argv:?}"
        );
    }

    #[test]
    fn argv_helpers_agree_with_the_golden() {
        let argv = golden();
        assert!(argv_contains(&argv, "--json"));
        assert!(argv_carries(&argv, "--job", "coverage"));
        assert!(!argv_carries(&argv, "--job", "build"));
        assert!(!argv_contains(&argv, "--json=false"));
    }
}
