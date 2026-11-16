//! The argv contract, asserted from outside the process — the `fathom` equivalent of
//! `crates/act-mcp/tests/boot.rs`.
//!
//! What this suite pins, and why each is worth a test:
//!
//! * **the surface** (`--help` lists the subcommands and every shared flag, `-V` prints one line):
//!   mact-by1.16 will gate on this file's shape, so the contract is asserted here first.
//! * **the exit codes** ([`fathom::exit`]): 0 for `index --status`, 1 for a typed refusal, 2 for a
//!   bad flag (clap's, never re-implemented), 3 for a subcommand whose engine is a later ticket. A
//!   stub that exited 0 would let CI assert a report had been produced.
//! * **the stream split**: a refusal, a warning and a `not yet wired` all go to stderr, and stdout
//!   stays empty until a command genuinely has data — so `fathom report > timeline.tsv` cannot be
//!   poisoned by a log line (ADR-0005 §6).
//! * **the refusals** themselves: a workspace that is not a checkout, a cache root nested in the
//!   evidence root, an unwritable cache for a writer, a `fathom.toml` that names a key this build
//!   does not know — each one line plus a `fix:`, none of them a panic.
//!
//! Nothing here needs a TTY, a network, `git`, or `act`. A "git work tree" is a directory with a
//! `.git` in it, which is exactly what `fathom::boot` checks.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The binary under test, from the same build as this test binary.
const FATHOM: &str = env!("CARGO_BIN_EXE_fathom");

/// One run of the binary, with the three things a caller asks about.
struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Run {
    fn assert_code(&self, expected: i32) -> &Self {
        assert_eq!(
            self.code,
            Some(expected),
            "exit {} — stdout:\n{}\nstderr:\n{}",
            self.code.unwrap_or(-1),
            self.stdout,
            self.stderr
        );
        self
    }

    /// No panic in sight: a `todo!()`/`unimplemented!()`/unwrap failure prints this, and a stub that
    /// panics in a user's terminal is the failure the ticket named.
    fn assert_not_panicked(&self) -> &Self {
        for needle in ["panicked", "unreachable", "not implemented", "TODO"] {
            assert!(
                !self.stderr.contains(needle),
                "stderr contains {needle:?}:\n{}",
                self.stderr
            );
        }
        self
    }
}

/// Run `fathom` with the ambient environment scrubbed, so a developer's `RUST_LOG` or `EVIDENCE_DIR`
/// cannot change what these tests observe.
fn fathom(args: &[&str]) -> Run {
    let mut command = Command::new(FATHOM);
    for key in ["RUST_LOG", "EVIDENCE_DIR", "FATHOM_DIR", "FATHOM_CONFIG"] {
        command.env_remove(key);
    }
    command.args(args);
    exec(command)
}

fn exec(mut command: Command) -> Run {
    let output: Output = command
        .output()
        .expect("the binary is built by `cargo test`");
    Run {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// A workspace: a directory with a `.git` (what `fathom::boot` accepts), canonicalised because the
/// roots fathom reports are canonicalised and a test should compare like with like.
fn workspace(dir: &Path) -> PathBuf {
    let repo = dir.join("repo");
    std::fs::create_dir_all(repo.join(".git")).expect("make a work tree");
    std::fs::create_dir_all(repo.join("workflows")).expect("make a directory");
    repo.canonicalize().expect("canonical scratch path")
}

// ---------------------------------------------------------------------------
// the surface
// ---------------------------------------------------------------------------

#[test]
fn help_names_every_subcommand_and_every_shared_flag() {
    let long = fathom(&["--help"]);
    long.assert_code(0);
    for expected in [
        "tui",
        "index",
        "report",
        "--evidence-dir",
        "--cache-dir",
        "--from",
        "--to",
        "--first-parent",
        "--config",
        "--verbose",
        "--version",
    ] {
        assert!(
            long.stdout.contains(expected),
            "`fathom --help` did not mention {expected:?}:\n{}",
            long.stdout
        );
    }
    // The default mode is spelled out, because `fathom` alone is the mode a user learns first.
    assert!(
        long.stdout.contains("[WORKSPACE]") && long.stdout.to_lowercase().contains("default"),
        "`fathom --help` should say the bare form is the TUI:\n{}",
        long.stdout
    );
}

#[test]
fn each_subcommand_has_its_own_help_and_takes_a_workspace() {
    for subcommand in ["tui", "index", "report"] {
        let help = fathom(&[subcommand, "--help"]);
        help.assert_code(0);
        assert!(
            help.stdout.contains("[WORKSPACE]"),
            "`fathom {subcommand} --help` should document its [WORKSPACE] argument:\n{}",
            help.stdout
        );
    }
    let index = fathom(&["index", "--help"]);
    assert!(
        index.stdout.contains("--status"),
        "`fathom index --help` should document --status:\n{}",
        index.stdout
    );
}

#[test]
fn version_is_one_line_and_matches_the_manifest() {
    let short = fathom(&["-V"]);
    let long = fathom(&["--version"]);
    short.assert_code(0);
    long.assert_code(0);
    let declared = env!("CARGO_PKG_VERSION");
    assert_eq!(short.stdout.trim(), format!("fathom {declared}"));
    assert_eq!(long.stdout.trim(), format!("fathom {declared}"));
    // `propagate_version`: a subcommand that lost it shows up here rather than as a confused reader.
    for subcommand in ["tui", "index", "report"] {
        let with_subcommand = fathom(&[subcommand, "--version"]);
        with_subcommand.assert_code(0);
        assert!(
            with_subcommand.stdout.contains(declared),
            "`fathom {subcommand} --version` did not report the version:\n{}",
            with_subcommand.stdout
        );
    }
}

// ---------------------------------------------------------------------------
// bad argv
// ---------------------------------------------------------------------------

#[test]
fn a_bad_flag_exits_two_with_claps_message_and_nothing_on_stdout() {
    let bad = fathom(&["--not-a-flag"]);
    bad.assert_code(2);
    assert!(
        bad.stderr.contains("unexpected argument"),
        "expected clap's own message, got:\n{}",
        bad.stderr
    );
    assert!(
        bad.stdout.is_empty(),
        "stdout must stay empty: {}",
        bad.stdout
    );

    // A value missing for a flag that needs one is the same class of mistake.
    fathom(&["--cache-dir"]).assert_code(2);
    // An unrecognised token is *not* clap's error, and that is deliberate: `fathom [WORKSPACE]` is
    // the default mode, so a bare word is a workspace path and `fathom nonsense-subcommand` is a
    // refusal about a path (exit 1), not about argv. Only a malformed flag is exit 2 here.
    let treated_as_a_path = fathom(&["nonsense-subcommand"]);
    assert_eq!(treated_as_a_path.code, Some(1));
    assert!(
        treated_as_a_path.stderr.contains("does not exist"),
        "{}",
        treated_as_a_path.stderr
    );
}

// ---------------------------------------------------------------------------
// the refusals
// ---------------------------------------------------------------------------

#[test]
fn a_workspace_that_is_not_there_is_a_typed_refusal_not_a_panic() {
    let run = fathom(&["/not/a/repo"]);
    run.assert_code(1).assert_not_panicked();
    assert!(run.stderr.contains("does not exist"), "{}", run.stderr);
    assert!(run.stderr.contains("fix:"), "{}", run.stderr);
    assert!(
        run.stdout.is_empty(),
        "stdout must stay empty: {}",
        run.stdout
    );
}

#[test]
fn a_directory_that_is_not_a_work_tree_is_refused_by_every_mode() {
    let scratch = tempfile::tempdir().expect("scratch");
    let plain = scratch.path().join("plain");
    std::fs::create_dir_all(&plain).expect("make a directory");

    for arguments in [
        vec![plain.display().to_string()],
        vec!["tui".to_owned(), plain.display().to_string()],
        vec!["index".to_owned(), plain.display().to_string()],
        vec!["report".to_owned(), plain.display().to_string()],
        vec![
            "index".to_owned(),
            "--status".to_owned(),
            plain.display().to_string(),
        ],
    ] {
        let as_refs: Vec<&str> = arguments.iter().map(String::as_str).collect();
        let run = fathom(&as_refs);
        run.assert_code(1).assert_not_panicked();
        assert!(
            run.stderr.contains("not a git work tree"),
            "{arguments:?} should refuse a non-repository:\n{}",
            run.stderr
        );
        assert!(run.stderr.contains("fix:"), "{arguments:?}: {}", run.stderr);
    }
}

#[test]
fn the_two_roots_may_not_nest() {
    let scratch = tempfile::tempdir().expect("scratch");
    let repo = workspace(scratch.path());

    let nested_cache = fathom(&[
        "--cache-dir",
        &repo.join(".evidence").join("cache").display().to_string(),
        &repo.display().to_string(),
    ]);
    nested_cache.assert_code(1).assert_not_panicked();
    assert!(
        nested_cache.stderr.contains("is inside the evidence root"),
        "{}",
        nested_cache.stderr
    );

    let nested_evidence = fathom(&[
        "--evidence-dir",
        &repo.join(".fathom").join("evidence").display().to_string(),
        &repo.display().to_string(),
    ]);
    nested_evidence.assert_code(1);
    assert!(
        nested_evidence.stderr.contains("is inside the cache root"),
        "{}",
        nested_evidence.stderr
    );
}

#[test]
#[cfg(unix)]
fn an_unwritable_cache_root_refuses_index_and_warns_everyone_else() {
    use std::os::unix::fs::PermissionsExt as _;

    let scratch = tempfile::tempdir().expect("scratch");
    let repo = workspace(scratch.path());
    let cache = repo.join(".fathom");
    std::fs::create_dir_all(&cache).expect("cache dir");
    std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o555)).expect("read-only");

    let report = fathom(&["report", &repo.display().to_string()]);
    // Still `not yet wired`, not a refusal: a read-only workspace is a build agent, and it may read
    // a store someone else wrote (ADR-0006 §10.1).
    report.assert_code(3);
    assert!(
        report.stderr.contains("cannot write the cache root"),
        "expected the read-only warning:\n{}",
        report.stderr
    );

    let index = fathom(&["index", &repo.display().to_string()]);
    index.assert_code(1).assert_not_panicked();
    assert!(index.stderr.contains("fix:"), "{}", index.stderr);

    // `--status` reads: it proceeds, and says the cache is read-only.
    let status = fathom(&["index", "--status", &repo.display().to_string()]);
    status.assert_code(0);
    assert!(status.stdout.contains("cache_state\tread-only"));

    std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o755)).expect("restore");
}

// ---------------------------------------------------------------------------
// what works, and what is honestly not wired yet
// ---------------------------------------------------------------------------

#[test]
fn the_default_mode_and_explicit_tui_agree() {
    let scratch = tempfile::tempdir().expect("scratch");
    let repo = workspace(scratch.path());
    let bare = fathom(&[&repo.display().to_string()]);
    let explicit = fathom(&["tui", &repo.display().to_string()]);

    bare.assert_code(3).assert_not_panicked();
    explicit.assert_code(3).assert_not_panicked();
    assert!(bare.stderr.contains("not yet wired"), "{}", bare.stderr);
    assert!(
        bare.stderr.contains("mact-by1.9"),
        "the line should name the ticket that owns the screen:\n{}",
        bare.stderr
    );
    assert!(
        bare.stdout.is_empty(),
        "a stub must not write stdout: {}",
        bare.stdout
    );
    assert_eq!(
        bare.stderr, explicit.stderr,
        "bare `fathom` *is* `fathom tui`"
    );
}

#[test]
fn report_is_not_wired_and_leaves_stdout_empty_for_the_tsv_consumer() {
    let scratch = tempfile::tempdir().expect("scratch");
    let repo = workspace(scratch.path());
    let run = fathom(&["report", &repo.display().to_string()]);
    run.assert_code(3).assert_not_panicked();
    assert!(run.stderr.contains("not yet wired"), "{}", run.stderr);
    assert!(run.stderr.contains("mact-by1.13"), "{}", run.stderr);
    assert!(
        run.stdout.is_empty(),
        "`fathom report > timeline.tsv` must not inherit a log line; got {:?}",
        run.stdout
    );
}

#[test]
fn index_is_not_wired_and_index_status_is() {
    let scratch = tempfile::tempdir().expect("scratch");
    let repo = workspace(scratch.path());

    let write = fathom(&["index", &repo.display().to_string()]);
    write.assert_code(3).assert_not_panicked();
    assert!(write.stderr.contains("mact-by1.10"), "{}", write.stderr);

    let status = fathom(&["index", "--status", &repo.display().to_string()]);
    status.assert_code(0);
    // Real TSV, and the facts the boot checks own.
    for key in [
        "workspace",
        "git_root",
        "evidence_root",
        "evidence_state",
        "cache_root",
        "cache_state",
        "store",
        "config",
        "history.depth",
        "window.to",
    ] {
        assert!(
            status
                .stdout
                .lines()
                .any(|line| line.split('\t').next() == Some(key)),
            "`index --status` should print a `{key}` line:\n{}",
            status.stdout
        );
    }
    assert!(
        status.stdout.contains("store\tabsent"),
        "there is no metric store until mact-by1.10:\n{}",
        status.stdout
    );
    assert!(
        status.stdout.lines().all(|line| line.contains('\t')),
        "every status line is a `key<TAB>value` pair:\n{}",
        status.stdout
    );
}

#[test]
fn an_unmeasured_workspace_warns_and_still_boots() {
    let scratch = tempfile::tempdir().expect("scratch");
    let repo = workspace(scratch.path());
    let status = fathom(&["index", "--status", &repo.display().to_string()]);
    status.assert_code(0);
    assert!(status.stdout.contains("evidence_state\tabsent"));
    assert!(
        status.stderr.contains("no evidence directory"),
        "an unmeasured repo is worth a warning:\n{}",
        status.stderr
    );
    assert!(status.stderr.contains("fix:"), "{}", status.stderr);
}

#[test]
fn the_cache_root_is_created_with_its_layout_and_lock_file() {
    let scratch = tempfile::tempdir().expect("scratch");
    let repo = workspace(scratch.path());
    let cache = repo.join(".fathom");
    assert!(!cache.exists(), "the workspace starts without a cache root");

    fathom(&["index", "--status", &repo.display().to_string()]).assert_code(0);

    assert!(cache.is_dir(), "the cache root is created at boot");
    let layout = cache.join("LAYOUT.md");
    assert!(
        layout.is_file(),
        "the cache root says what it is (ADR-0006 §3.2)"
    );
    assert!(
        std::fs::read_to_string(&layout)
            .expect("read LAYOUT.md")
            .contains("safe to delete"),
        "the on-disk contract should be the reviewed one"
    );
    assert!(
        cache.join("fathom.lock").is_file(),
        "the write probe is the lock file"
    );
}

// ---------------------------------------------------------------------------
// roots and config: flag, then env, then the default
// ---------------------------------------------------------------------------

#[test]
fn the_evidence_and_cache_roots_follow_flag_then_env_then_default() {
    let scratch = tempfile::tempdir().expect("scratch");
    let repo = workspace(scratch.path());
    let alternate = scratch.path().join("alternate");
    std::fs::create_dir_all(&alternate).expect("an alternate root");
    let alternate = alternate.canonicalize().expect("canonical scratch path");
    // Two roots that do not nest, because nesting them is a refusal of its own (ADR-0006 §3.1).
    let elsewhere = alternate.join("evidence");
    let other_cache = alternate.join("cache");

    // Defaults.
    let status = fathom(&["index", "--status", &repo.display().to_string()]);
    assert!(status.stdout.contains(&format!(
        "evidence_root\t{}",
        repo.join(".evidence").display()
    )));
    assert!(status
        .stdout
        .contains(&format!("cache_root\t{}", repo.join(".fathom").display())));

    // Flags.
    let status = fathom(&[
        "--evidence-dir",
        &elsewhere.display().to_string(),
        "--cache-dir",
        &other_cache.display().to_string(),
        "index",
        "--status",
        &repo.display().to_string(),
    ]);
    assert!(status
        .stdout
        .contains(&format!("evidence_root\t{}", elsewhere.display())));
    assert!(status
        .stdout
        .contains(&format!("cache_root\t{}", other_cache.display())));

    // Environment — the names the server uses for the evidence root, and ADR-0006's for the cache.
    let mut command = Command::new(FATHOM);
    command
        .env_remove("RUST_LOG")
        .env_remove("FATHOM_CONFIG")
        .env("EVIDENCE_DIR", &elsewhere)
        .env("FATHOM_DIR", &other_cache)
        .args(["index", "--status"])
        .arg(&repo);
    let status = exec(command);
    status.assert_code(0);
    assert!(status
        .stdout
        .contains(&format!("evidence_root\t{}", elsewhere.display())));
    assert!(status
        .stdout
        .contains(&format!("cache_root\t{}", other_cache.display())));

    // A flag beats the environment, which is the whole point of `--evidence-dir`.
    let mut command = Command::new(FATHOM);
    command
        .env_remove("RUST_LOG")
        .env_remove("FATHOM_CONFIG")
        .env("EVIDENCE_DIR", &elsewhere)
        .args(["--evidence-dir"])
        .arg(repo.join(".evidence").as_os_str())
        .arg("index")
        .arg("--status")
        .arg(&repo);
    let status = exec(command);
    status.assert_code(0);
    assert!(status.stdout.contains(&format!(
        "evidence_root\t{}",
        repo.join(".evidence").display()
    )));
}

#[test]
fn config_is_found_root_ward_and_stops_at_the_git_top_level() {
    let scratch = tempfile::tempdir().expect("scratch");
    let repo = workspace(scratch.path());
    let member = repo.join("crates").join("member");
    std::fs::create_dir_all(&member).expect("a monorepo member");
    std::fs::write(repo.join("fathom.toml"), "[history]\ndepth = 4242\n").expect("write config");

    // From a member directory, the repository's file is the one that answers.
    let status = fathom(&["index", "--status", &member.display().to_string()]);
    status.assert_code(0);
    assert!(
        status.stdout.contains("history.depth\t4242"),
        "{}",
        status.stdout
    );
    assert!(
        status
            .stdout
            .contains(&format!("config\t{}", repo.join("fathom.toml").display())),
        "the file that was used should be named: {}",
        status.stdout
    );

    // A file above the repository is invisible, which is why the search is bounded.
    std::fs::write(
        scratch.path().join("fathom.toml"),
        "[history]\ndepth = 7777\n",
    )
    .expect("write the outer config");
    let status = fathom(&["index", "--status", &member.display().to_string()]);
    assert!(
        status.stdout.contains("history.depth\t4242"),
        "{}",
        status.stdout
    );
}

#[test]
fn an_unknown_config_key_is_a_refusal_that_names_the_key_and_exits_one() {
    let scratch = tempfile::tempdir().expect("scratch");
    let repo = workspace(scratch.path());
    let bad = repo.join("fathom.toml");
    std::fs::write(&bad, "[paths]\nnoize = [\"fixtures/**\"]\n").expect("write a bad config");

    let run = fathom(&["index", "--status", &repo.display().to_string()]);
    run.assert_code(1).assert_not_panicked();
    assert!(run.stderr.contains("noize"), "{}", run.stderr);
    assert!(
        run.stderr.contains("noise"),
        "the expected key should be named:\n{}",
        run.stderr
    );
    assert!(run.stderr.contains("fix:"), "{}", run.stderr);
}

#[test]
fn an_explicit_config_that_is_missing_is_a_refusal_never_a_fallback() {
    let scratch = tempfile::tempdir().expect("scratch");
    let repo = workspace(scratch.path());
    let missing = repo.join("nope.toml");
    let run = fathom(&[
        "--config",
        &missing.display().to_string(),
        "index",
        "--status",
        &repo.display().to_string(),
    ]);
    run.assert_code(1).assert_not_panicked();
    assert!(run.stderr.contains("does not exist"), "{}", run.stderr);

    // `$FATHOM_CONFIG` is the same precedence, so it gets the same refusal.
    let mut command = Command::new(FATHOM);
    command
        .env_remove("RUST_LOG")
        .env("FATHOM_CONFIG", &missing)
        .args(["index", "--status"])
        .arg(&repo);
    exec(command).assert_code(1);
}

#[test]
fn an_explicit_config_wins_over_the_search() {
    let scratch = tempfile::tempdir().expect("scratch");
    let repo = workspace(scratch.path());
    std::fs::write(repo.join("fathom.toml"), "[history]\ndepth = 11\n").expect("repo config");
    let chosen = scratch.path().join("mine.toml");
    std::fs::write(&chosen, "[history]\ndepth = 22\n").expect("other config");

    let status = fathom(&[
        "--config",
        &chosen.display().to_string(),
        "index",
        "--status",
        &repo.display().to_string(),
    ]);
    status.assert_code(0);
    assert!(
        status.stdout.contains("history.depth\t22"),
        "{}",
        status.stdout
    );
}

#[test]
fn first_parent_comes_from_config_or_from_the_flag() {
    let scratch = tempfile::tempdir().expect("scratch");
    let repo = workspace(scratch.path());

    let default = fathom(&["index", "--status", &repo.display().to_string()]);
    assert!(default.stdout.contains("history.first_parent\tfalse"));

    std::fs::write(repo.join("fathom.toml"), "[history]\nfirst_parent = true\n")
        .expect("config sets it");
    let from_config = fathom(&["index", "--status", &repo.display().to_string()]);
    assert!(from_config.stdout.contains("history.first_parent\ttrue"));

    std::fs::write(
        repo.join("fathom.toml"),
        "[history]\nfirst_parent = false\n",
    )
    .expect("config off");
    let from_flag = fathom(&[
        "--first-parent",
        "index",
        "--status",
        &repo.display().to_string(),
    ]);
    assert!(
        from_flag.stdout.contains("history.first_parent\ttrue"),
        "the flag overrides the key (ADR-0006 §7.4):\n{}",
        from_flag.stdout
    );
}

#[test]
fn the_window_flags_are_echoed_and_never_cached() {
    let scratch = tempfile::tempdir().expect("scratch");
    let repo = workspace(scratch.path());
    let status = fathom(&[
        "--from",
        "v1.0",
        "--to",
        "main",
        "index",
        "--status",
        &repo.display().to_string(),
    ]);
    status.assert_code(0);
    assert!(
        status.stdout.contains("window.from\tv1.0"),
        "{}",
        status.stdout
    );
    assert!(
        status.stdout.contains("window.to\tmain"),
        "{}",
        status.stdout
    );
}

// ---------------------------------------------------------------------------
// logging
// ---------------------------------------------------------------------------

#[test]
fn verbose_flags_raise_the_level_and_rust_log_overrides_them() {
    let scratch = tempfile::tempdir().expect("scratch");
    let repo = workspace(scratch.path());
    let quiet = fathom(&["index", "--status", &repo.display().to_string()]);
    let verbose = fathom(&["-v", "index", "--status", &repo.display().to_string()]);
    let loudest = fathom(&["-vv", "index", "--status", &repo.display().to_string()]);

    quiet.assert_code(0);
    verbose.assert_code(0);
    loudest.assert_code(0);
    assert!(
        !quiet.stderr.contains("this build reads evidence schema"),
        "debug lines are not for the default level:\n{}",
        quiet.stderr
    );
    assert!(
        verbose.stderr.contains("workspace "),
        "-v should say which workspace it resolved:\n{}",
        verbose.stderr
    );
    assert!(
        !verbose.stderr.contains("this build reads evidence schema"),
        "-v is info; the schema line is debug:\n{}",
        verbose.stderr
    );
    assert!(
        loudest.stderr.contains("this build reads evidence schema"),
        "-vv is trace and should name the contract version:\n{}",
        loudest.stderr
    );
    assert!(
        verbose.stdout.starts_with("workspace\t"),
        "stdout is the TSV, and only the TSV:\n{}",
        verbose.stdout
    );
    assert!(
        !verbose.stdout.contains("git root"),
        "the `-v` diagnostic belongs on stderr:\n{}",
        verbose.stdout
    );

    // `RUST_LOG` is a more specific statement of intent than a flag, so it wins.
    let mut command = Command::new(FATHOM);
    command
        .env("RUST_LOG", "off")
        .env_remove("EVIDENCE_DIR")
        .env_remove("FATHOM_DIR")
        .env_remove("FATHOM_CONFIG")
        .args(["-vv", "index", "--status"])
        .arg(&repo);
    let silenced = exec(command);
    silenced.assert_code(0);
    assert!(
        silenced.stderr.is_empty(),
        "RUST_LOG=off means nothing on stderr:\n{}",
        silenced.stderr
    );

    // A directive that is not a level is reported, not dropped on the floor.
    let mut command = Command::new(FATHOM);
    command
        .env("RUST_LOG", "not-a-level=junk")
        .env_remove("EVIDENCE_DIR")
        .env_remove("FATHOM_DIR")
        .env_remove("FATHOM_CONFIG")
        .args(["index", "--status"])
        .arg(&repo);
    let puzzled = exec(command);
    puzzled.assert_code(0);
    assert!(
        puzzled.stderr.contains("is not a level"),
        "an unparseable directive says so:\n{}",
        puzzled.stderr
    );
}
