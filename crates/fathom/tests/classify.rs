//! The classifier's shipped defaults, run through the public API at the level a consumer sees them.
//!
//! The unit tests beside the matcher check each rule in isolation against a fixture config built from
//! ADR-0006 §7.2's *illustrative* lists. These check what the defaults that actually ship do —
//! `Config`'s three lists plus `schema/mi-definition.json`'s test globs, and nothing else — so the
//! dialect rules the module doc states are pinned by something that fails when a default is edited,
//! and so ADR-0006 §7.2's one-sentence precedence ("excluded beats noise beats source") is fixed at
//! the boundary rather than only inside a private function's test.
//!
//! Two rules are easy to get subtly wrong and are worth the length:
//!
//! * **`paths.source` ships as `**/*`** (ADR-0006 §7.1), so every path that noise and exclude do not
//!   name is source, and a file type nobody predicted — a PDF, a CI workflow — is counted rather
//!   than silently dropped. Narrow that list and the same paths become `Excluded`: out of the headline,
//!   still counted in `*_all`. Both halves are asserted below, because the second is the one that
//!   surprises someone reading `classify()`'s last arm.
//! * **Anchoring.** A pattern with no `/` is anchored at the repository root, so `Cargo.toml` names the
//!   root manifest only and `**/Cargo.toml` is the spelling for "any depth". The default list cannot
//!   show this — `**/*` matches everything — so the test builds the config that can.

use fathom::classify::{Classifier, Scope, ScopeModel};
use fathom::config::{Config, DEFAULT_EXCLUDE_GLOBS, DEFAULT_NOISE_GLOBS};

/// A source list narrow enough to make fall-through visible, since the shipped default cannot.
fn source_only_rust() -> Config {
    let mut config = Config::default();
    config.paths.source = vec!["**/*.rs".to_owned()];
    config
}

#[test]
fn the_shipped_defaults_are_the_ones_the_adr_and_the_model_name() {
    // ADR-0006 §7.1's table, one row at a time: a default that drifts silently changes every row's
    // diffstat, and the cache key does not warn about it.
    assert_eq!(
        Config::default().paths.source,
        vec!["**/*".to_owned()],
        "`paths.source` is a candidate set, not a language list — the language filter is ADR-0007's, \
         not the diffstat's"
    );
    // ADR-0007 C11, in Rust: MI and the diffstat share one classifier and one noise list, so the
    // config default and the model document may not name two different ones.
    let model = ScopeModel::embedded();
    assert_eq!(
        Config::default().paths.noise,
        model.noise_globs,
        "one noise list"
    );
    assert_eq!(model.noise_globs, DEFAULT_NOISE_GLOBS.to_vec());
    assert_eq!(
        DEFAULT_EXCLUDE_GLOBS,
        [".git/**", ".fathom/**", ".evidence/**"]
    );
    assert!(
        model.test_globs.iter().any(|glob| glob == "**/tests/**"),
        "the model's test globs are the ones ADR-0007 §7 declares, and they are what the classifier \
         uses rather than a Rust-side copy"
    );
}

#[test]
fn the_default_lists_sort_every_shape_of_path_into_the_right_scope() {
    let classifier = Classifier::default();
    let scope = |path: &str| classifier.classify(path);

    // Source: everything the noise and exclude lists do not name, because the candidate set is `**/*`.
    assert_eq!(scope("src/main.rs"), Scope::Source);
    assert_eq!(scope("crates/fathom/src/store.rs"), Scope::Source);
    assert_eq!(scope("web/app/page.tsx"), Scope::Source);
    assert_eq!(scope("scripts/build.py"), Scope::Source);
    assert_eq!(scope("Cargo.toml"), Scope::Source);
    assert_eq!(scope("crates/fathom/Cargo.toml"), Scope::Source);
    assert_eq!(
        scope("docs/report.pdf"),
        Scope::Source,
        "a file type no default names is a file of the repository, and is counted"
    );
    assert_eq!(scope(".github/workflows/ci.yml"), Scope::Source);
    assert_eq!(scope("assets/diagram.png"), Scope::Source);

    // Noise: labelled, never hidden. Each of these matches a source pattern too, and noise wins, so
    // the lines still reach `noise_add`/`noise_del` instead of vanishing.
    assert_eq!(scope("Cargo.lock"), Scope::Noise);
    assert_eq!(scope("vendor/Cargo.lock"), Scope::Noise);
    assert_eq!(scope("target/debug/fathom"), Scope::Noise);
    assert_eq!(scope("node_modules/pkg/index.js"), Scope::Noise);
    assert_eq!(scope(".venv/lib/site.py"), Scope::Noise);
    assert_eq!(scope("web/app.min.js"), Scope::Noise);
    assert_eq!(
        scope("src/generated/api.rs"),
        Scope::Noise,
        "`**/generated/**` beats `**/*.rs`: generated code is labelled, not counted as ours"
    );
    assert_eq!(
        scope("target/debug/build.rs"),
        Scope::Noise,
        "and a .rs under target is build output whatever its extension claims"
    );

    // Tests, by the model's globs, with `paths.include_tests` off — which is a default that hides half
    // a repository's files from the headline and so has to announce itself.
    assert_eq!(scope("tests/reader.rs"), Scope::Noise);
    assert_eq!(scope("crates/fathom/tests/history.rs"), Scope::Noise);
    assert_eq!(scope("scripts/test_build.py"), Scope::Noise);
    assert_eq!(scope("src/git_test.rs"), Scope::Noise);
    assert!(classifier.tests_are_excluded());

    // Excluded: the three directories a tool has no business walking.
    assert_eq!(scope(".git/HEAD"), Scope::Excluded);
    assert_eq!(scope(".fathom/index.sqlite"), Scope::Excluded);
    assert_eq!(scope(".evidence/night.jsonl"), Scope::Excluded);
}

#[test]
fn narrowing_paths_source_moves_the_unmatched_paths_out_of_the_headline() {
    // `classify()`'s last arm. With the shipped `**/*` it is unreachable; narrow the list and it is the
    // difference between a PDF in the headline and a PDF in `*_all` only — out of the headline, never
    // silently dropped.
    let classifier = Classifier::from_config(&source_only_rust());
    assert_eq!(classifier.classify("src/lib.rs"), Scope::Source);
    assert_eq!(classifier.classify("docs/report.pdf"), Scope::Excluded);
    assert_eq!(classifier.classify("web/app.ts"), Scope::Excluded);
    assert_eq!(
        classifier.classify("Cargo.lock"),
        Scope::Noise,
        "narrowing the candidate set does not promote noise into excluded"
    );
}

#[test]
fn a_bare_pattern_is_anchored_at_the_repository_root() {
    // The same rule that makes `target/**` mean one directory. Asserted through a narrowed source
    // list because the shipped `**/*` matches every path and would hide the difference.
    let mut config = source_only_rust();
    config.paths.source = vec!["Cargo.toml".to_owned()];
    let classifier = Classifier::from_config(&config);
    assert_eq!(classifier.classify("Cargo.toml"), Scope::Source);
    assert_eq!(
        classifier.classify("crates/fathom/Cargo.toml"),
        Scope::Excluded,
        "a bare pattern is root-anchored; `**/Cargo.toml` is the spelling for any depth"
    );

    config.paths.source = vec!["**/Cargo.toml".to_owned()];
    let classifier = Classifier::from_config(&config);
    assert_eq!(
        classifier.classify("Cargo.toml"),
        Scope::Source,
        "and `**` matches zero segments, so the any-depth spelling matches the root manifest too"
    );
}

#[test]
fn excluded_beats_noise_beats_source() {
    // ADR-0006 §7.2's precedence, on paths that match two or three lists at once.
    let classifier = Classifier::default();
    assert_eq!(
        classifier.classify(".fathom/store.rs"),
        Scope::Excluded,
        "`.git/**`, `.fathom/**`, `.evidence/**` are never walked, whatever else matches"
    );
    assert_eq!(
        classifier.classify("target/debug/lib.rs"),
        Scope::Noise,
        "noise beats source, and the path is counted rather than dropped"
    );

    // The shipped lists do not put one pattern in two lists, so build the overlap and check the order:
    // a repository that excludes what its noise list also names gets the order the sentence states,
    // not the order the lists happen to be written in.
    let mut config = Config::default();
    config.paths.exclude.push("vendor/**".to_owned());
    assert_eq!(
        Classifier::from_config(&config).classify("vendor/legacy.rs"),
        Scope::Excluded
    );
    assert_eq!(
        Classifier::default().classify("vendor/legacy.rs"),
        Scope::Noise,
        "the default noise list already names `vendor/**`, so without the exclusion the same path is \
         noise: precedence, not accident"
    );
}

#[test]
fn include_tests_moves_a_path_from_the_noise_column_into_the_source_column() {
    // ADR-0007 §7.1, the sentence the whole shared-classifier design turns on: one object answers for
    // the diffstat and for MI, and one config key flips both together.
    let path = "crates/fathom/tests/history.rs";
    let mut config = Config::default();
    assert_eq!(
        Classifier::from_config(&config).classify(path),
        Scope::Noise
    );
    assert!(Classifier::from_config(&config).tests_are_excluded());

    config.paths.include_tests = true;
    let classifier = Classifier::from_config(&config);
    assert_eq!(classifier.classify(path), Scope::Source);
    assert!(!classifier.tests_are_excluded());
    assert!(classifier.is_source("tests/reader.rs"));
    assert_eq!(
        classifier.classify("Cargo.lock"),
        Scope::Noise,
        "and the rest of the noise list is untouched by the flip"
    );
    assert_eq!(
        classifier.classify("target/debug/lib.rs"),
        Scope::Noise,
        "noise that is not about tests stays noise"
    );

    // With a narrowed candidate set, a test file the source list does not name stays out of the
    // headline even with the flag on — the flip removes the test rule, it does not add a source rule.
    let mut narrowed = source_only_rust();
    narrowed.paths.include_tests = true;
    narrowed.paths.source = vec!["src/**/*.rs".to_owned()];
    assert_eq!(
        Classifier::from_config(&narrowed).classify(path),
        Scope::Excluded,
        "`crates/fathom/tests/history.rs` matches no source pattern once the list is that narrow"
    );
}
