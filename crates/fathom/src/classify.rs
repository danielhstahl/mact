//! The one path classifier the `+`/`−` column and the MI column share (ADR-0007 §7.1).
//!
//! A `+` column that counts files the MI column ignores — or the other way round — makes the two
//! columns of one row describe two different commits, so ADR-0007 §7.1 answers both columns with a
//! single classifier and this module is that answer, written once. It reads ADR-0006 §7.2's three
//! glob lists out of the resolved [`crate::Config`] plus ADR-0007 §7.2's one new key
//! (`paths.include_tests`), and it returns one of three scopes:
//!
//! ```text
//! exclude  >  noise  >  (source)
//! ```
//!
//! * [`Scope::Excluded`] — matched `paths.exclude`. Never walked at all: `.git/**`, `.fathom/**`,
//!   `.evidence/**` by default. A tool must not analyse its own cache or read the writer's output
//!   as if it were the repository.
//! * [`Scope::Noise`] — matched `paths.noise`, or a test file while `paths.include_tests` is false.
//!   Noise is **labelled, never hidden**: the lines still land in `noise_add`/`noise_del`, so a
//!   4,000-line lockfile is never invisible and never called code.
//! * [`Scope::Source`] — matched `paths.source`: the file set both columns headline with.
//!
//! A path matched by *none* of the three lists is `Excluded`, not source: `paths.source` is a declared
//! candidate set (ADR-0006 §7.1), and ADR-0007's `extension_rule` is explicit that a file outside the
//! analysed set is "`excluded`, i.e. not source at all, and is NOT counted as unanalysed". In the
//! shipped configuration that arm is unreachable, because `paths.source` defaults to `**/*` and every
//! path noise and exclude do not name is source — including a PDF or a CI workflow, which are counted
//! rather than silently dropped. Narrow the list and the arm becomes the difference between a path in
//! the headline and a path in `*_all` only: out of the headline, never vanished.
//!
//! ## Glob dialect (ADR-0006 §7.2, one sentence long)
//!
//! Matched against repo-relative paths with forward slashes, case-sensitive, `**` crosses `/`, a
//! trailing `/` matches a directory subtree, no negation. This code adds the segment rules:
//!
//! * a pattern is split on `/` and matched segment by segment;
//! * a whole segment of `**` matches zero or more segments, so `**/*.lock` matches `Cargo.lock` as
//!   well as `vendor/Cargo.lock`, and `**/tests/**` matches `tests/x.rs` as well as
//!   `crates/fathom/tests/x.rs`;
//! * inside a segment, `*` matches any run of characters, `?` matches exactly one, `[a-z]`/`[!a-z]`
//!   are classes (leading `!` or `^` negates), and `\` escapes the next character;
//! * a glob with no `/` is anchored at the repository root, so `Cargo.lock` is the root lockfile and
//!   nothing else — `**/Cargo.lock` is the spelling for "at any depth". Anchoring is the same rule
//!   that makes `target/**` mean one directory; a pattern that matched at any depth would make the
//!   default list mean something nobody wrote.
//!
//! The dialect is deliberately not gitignore's (gitignore has anchoring-by-slash-count, negation
//! lines and last-match-wins, and ADR-0006 §7.2 refuses negation outright: "order-dependent globs in
//! a config file are a support ticket"), and it is deliberately not `globset`'s: a matcher this
//! small, with the semantics pinned above and a test block beside it, is cheaper than a new
//! dependency and a second dialect to learn. `crates/fathom/tests/classify.rs` is the executable
//! version of that sentence: it runs the shipped defaults through the public API.
//!
//! ## Where the defaults come from
//!
//! `paths.noise` and `paths.exclude` are the [`crate::Config`] defaults (ADR-0006 §7.2), and the
//! test globs are `schema/mi-definition.json`'s `scope.tests_default.test_globs` — loaded rather
//! than restated, the same habit [`crate::metrics::Model`] has for `schema/metric-model.json`.
//! `tests/classify.rs` asserts the config's noise default equals the model's list, which is C11 of
//! ADR-0007's verification table ("MI and the diffstat share one classifier and one noise list")
//! enforced in Rust rather than only in python.

use std::fmt;

use serde::Deserialize;
use thiserror::Error;

use crate::config::Config;

/// `schema/mi-definition.json`, loaded for the two scope fields the classifier executes.
pub const MI_DEFINITION_JSON: &str = include_str!("../../../schema/mi-definition.json");

/// The config keys this classifier reads, and no others (ADR-0006 §7.3's rule that an analyzer
/// declares the keys it reads; ADR-0007 §7.2's `diff.per_file` row is the same list).
pub const DECLARED_CONFIG_KEYS: [&str; 4] = [
    "paths.source",
    "paths.noise",
    "paths.exclude",
    "paths.include_tests",
];

/// Which of the three scopes a path falls in. Ordinal order is the precedence order: excluded
/// beats noise beats source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    /// Matched `paths.exclude`: not walked, not counted in a headline, counted in `*_all`.
    Excluded,
    /// Matched `paths.noise`, or a test file while `paths.include_tests` is false.
    Noise,
    /// In scope for both columns.
    Source,
}

impl Scope {
    /// The spelling used in a stored row and in `doctor` output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Excluded => "excluded",
            Self::Noise => "noise",
            Self::Source => "source",
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl serde::Serialize for Scope {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// Why a glob could not be compiled.
#[allow(
    missing_docs,
    reason = "the #[error] format string documents each variant and its fields"
)]
#[derive(Debug, Error)]
pub enum GlobError {
    /// A `[...]` class was never closed.
    #[error("bad glob {glob}: unterminated character class\n  fix: a class is [abc] or [a-z], optionally [!...] to negate")]
    UnterminatedClass { glob: String },

    /// A class range ran backwards, which matches nothing.
    #[error("bad glob {glob}: the character range {low}-{high} is empty\n  fix: a range ascends, as in [a-z]; a literal dash goes last, as in [ab-]")]
    EmptyRange { glob: String, low: char, high: char },
}

/// The shared classifier: three glob lists, the test-file rule, and no other state.
#[derive(Debug, Clone)]
pub struct Classifier {
    source: Vec<Glob>,
    noise: Vec<Glob>,
    exclude: Vec<Glob>,
    tests: Vec<Glob>,
    include_tests: bool,
    /// Whether `paths.include_tests` is off, i.e. whether test paths are noise because they are
    /// tests. `doctor` prints that sentence (ADR-0007 §7.2), so it is stored rather than inferred.
    tests_excluded: bool,
}

impl Default for Classifier {
    fn default() -> Self {
        Self::from_config(&Config::default())
    }
}

impl Classifier {
    /// A classifier from the resolved `fathom.toml`, with `schema/mi-definition.json`'s test globs.
    ///
    /// # Errors
    ///
    /// [`GlobError`] if a configured glob is not compilable. A broken glob in `fathom.toml` is a
    /// refusal rather than a glob that silently matches nothing: a noise list that stopped matching
    /// is a diffstat that quietly started calling a lockfile code.
    pub fn try_from_config(config: &Config) -> Result<Self, GlobError> {
        let model = ScopeModel::embedded();
        Self::new(
            &config.paths.source,
            &config.paths.noise,
            &config.paths.exclude,
            &model.test_globs,
            config.paths.include_tests,
        )
    }

    /// A classifier from the resolved `fathom.toml`, panicking on a glob that will not compile.
    ///
    /// Present for callers that have no error channel (a `Default`, a test); production callers
    /// reach for [`Classifier::try_from_config`].
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        Self::try_from_config(config).expect("a default-shaped config compiles")
    }

    /// The pieces, spelled out: the test globs come from the model and the rest from config, but a
    /// checker and a test want to build one without either file.
    ///
    /// # Errors
    ///
    /// [`GlobError`] for any uncompilable glob in any list.
    pub fn new(
        source: &[String],
        noise: &[String],
        exclude: &[String],
        tests: &[String],
        include_tests: bool,
    ) -> Result<Self, GlobError> {
        Ok(Self {
            source: compile(source)?,
            noise: compile(noise)?,
            exclude: compile(exclude)?,
            tests: compile(tests)?,
            include_tests,
            tests_excluded: !include_tests,
        })
    }

    /// The scope of one repo-relative path (forward slashes, no leading `/`).
    #[must_use]
    pub fn classify(&self, path: &str) -> Scope {
        let path = strip_leading(path);
        // ADR-0006 §7.2's precedence, evaluated in that order and short-circuiting: a path under
        // `.evidence/` is excluded even if a noise glob also matches it, because "excluded" is the
        // stronger claim about the tool, not about the file.
        if self.exclude.iter().any(|glob| glob.matches(path)) {
            return Scope::Excluded;
        }
        if self.noise.iter().any(|glob| glob.matches(path)) {
            return Scope::Noise;
        }
        if !self.include_tests && self.tests.iter().any(|glob| glob.matches(path)) {
            return Scope::Noise;
        }
        if self.source.iter().any(|glob| glob.matches(path)) {
            return Scope::Source;
        }
        // `paths.source` defaults to `**/*`, so reaching here means the user narrowed the candidate
        // set on purpose. A path outside it is not noise (nothing about it says "generated"); it is
        // out of scope for the headline, which is what `Excluded` means to a renderer: `—`, counted
        // in `*_all`, never silently folded into `source`.
        Scope::Excluded
    }

    /// Whether a path is in the headline set.
    #[must_use]
    pub fn is_source(&self, path: &str) -> bool {
        self.classify(path) == Scope::Source
    }

    /// Whether `paths.include_tests` is off, which is ADR-0007 §7.2's "a default that hides half the
    /// repository must at least announce itself" — `doctor` prints the sentence from this.
    #[must_use]
    pub const fn tests_are_excluded(&self) -> bool {
        self.tests_excluded
    }
}

fn strip_leading(path: &str) -> &str {
    path.strip_prefix("./")
        .unwrap_or(path)
        .strip_prefix('/')
        .unwrap_or(path)
}

fn compile(globs: &[String]) -> Result<Vec<Glob>, GlobError> {
    globs.iter().map(|glob| Glob::compile(glob)).collect()
}

/// One compiled glob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Glob {
    /// The text it was compiled from, for `doctor` and for an error message.
    pub text: String,
    segments: Vec<Segment>,
}

/// One `/`-separated piece of a glob.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    /// `**`: zero or more path segments.
    AnySegments,
    /// A pattern over exactly one segment.
    Pattern(Vec<Piece>),
}

/// One piece of a within-segment pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    Literal(char),
    /// `*`: any run of characters, including none.
    Many,
    /// `?`: exactly one character.
    One,
    /// `[...]`, and `negated` when it began with `!` or `^`.
    Class {
        negated: bool,
        ranges: Vec<(char, char)>,
    },
}

impl Glob {
    /// Compile one glob of the dialect in the module docs.
    ///
    /// # Errors
    ///
    /// [`GlobError`] for an unterminated class or an empty range.
    pub fn compile(text: &str) -> Result<Self, GlobError> {
        let mut raw: Vec<&str> = text.split('/').collect();
        // A trailing `/` means "this directory's whole subtree" (ADR-0006 §7.2), which is exactly
        // `dir/**`; the empty last segment gitignore would leave behind is replaced by the `**`.
        if raw.len() > 1 && raw.last().is_some_and(|last| last.is_empty()) {
            raw.pop();
            raw.push("**");
        }
        let mut segments = Vec::with_capacity(raw.len());
        for segment in raw {
            if segment == "**" {
                segments.push(Segment::AnySegments);
                continue;
            }
            segments.push(Segment::Pattern(compile_segment(text, segment)?));
        }
        Ok(Self {
            text: text.to_owned(),
            segments,
        })
    }

    /// Whether this glob matches one repo-relative path.
    #[must_use]
    pub fn matches(&self, path: &str) -> bool {
        let path = strip_leading(path);
        if path.is_empty() {
            return false;
        }
        match_segments(&self.segments, &path.split('/').collect::<Vec<_>>())
    }
}

fn compile_segment(glob: &str, segment: &str) -> Result<Vec<Piece>, GlobError> {
    let mut pieces = Vec::new();
    let mut chars = segment.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' => pieces.push(Piece::Many),
            '?' => pieces.push(Piece::One),
            '\\' => {
                if let Some(escaped) = chars.next() {
                    pieces.push(Piece::Literal(escaped));
                }
            }
            '[' => {
                let negated = matches!(chars.peek(), Some('!') | Some('^'));
                if negated {
                    chars.next();
                }
                let mut ranges: Vec<(char, char)> = Vec::new();
                let mut closed = false;
                while let Some(low) = chars.next() {
                    // A `]` closes the class only once the class holds something: POSIX reads `[]]`]
                    // as the one-character class `]`, and a dialect that cannot name `]` is a
                    // dialect nobody will explain correctly.
                    if low == ']' && !ranges.is_empty() {
                        closed = true;
                        break;
                    }
                    if chars.peek() == Some(&'-') {
                        chars.next();
                        match chars.next() {
                            // `[a-]` and `[a-` : the dash is the literal, not a range.
                            Some(']') => {
                                ranges.push((low, low));
                                ranges.push(('-', '-'));
                                closed = true;
                                break;
                            }
                            None => {
                                ranges.push((low, low));
                                ranges.push(('-', '-'));
                            }
                            Some(high) => {
                                if high < low {
                                    return Err(GlobError::EmptyRange {
                                        glob: glob.to_owned(),
                                        low,
                                        high,
                                    });
                                }
                                ranges.push((low, high));
                            }
                        }
                    } else {
                        ranges.push((low, low));
                    }
                }
                if !closed {
                    return Err(GlobError::UnterminatedClass {
                        glob: glob.to_owned(),
                    });
                }
                pieces.push(Piece::Class { negated, ranges });
            }
            other => pieces.push(Piece::Literal(other)),
        }
    }
    Ok(pieces)
}

fn match_segments(segments: &[Segment], path: &[&str]) -> bool {
    match segments.split_first() {
        None => path.is_empty(),
        Some((Segment::AnySegments, rest)) => {
            // `**` eats zero or more segments; trying every split is the whole algorithm, and a
            // glob has a handful of segments so the worst case is a small quadratic.
            (0..=path.len()).any(|taken| match_segments(rest, &path[taken..]))
        }
        Some((Segment::Pattern(pattern), rest)) => {
            let Some((first, tail)) = path.split_first() else {
                return false;
            };
            match_pattern(pattern, &first.chars().collect::<Vec<_>>()) && match_segments(rest, tail)
        }
    }
}

fn match_pattern(pattern: &[Piece], text: &[char]) -> bool {
    match pattern.split_first() {
        None => text.is_empty(),
        Some((Piece::Many, rest)) => {
            (0..=text.len()).any(|taken| match_pattern(rest, &text[taken..]))
        }
        Some((piece, rest)) => {
            let Some((first, tail)) = text.split_first() else {
                return false;
            };
            match_piece(piece, *first) && match_pattern(rest, tail)
        }
    }
}

fn match_piece(piece: &Piece, c: char) -> bool {
    match piece {
        Piece::Literal(want) => c == *want,
        Piece::Many | Piece::One => true,
        Piece::Class { negated, ranges } => {
            let inside = ranges.iter().any(|(low, high)| c >= *low && c <= *high);
            if *negated {
                !inside
            } else {
                inside
            }
        }
    }
}

/// The two scope fields of `schema/mi-definition.json`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ScopeModel {
    /// `scope.tests_default.test_globs` — ADR-0007 §7's addition, consumed by `diff.per_file` and
    /// `mi.score` alike so both columns' sets move together.
    pub test_globs: Vec<String>,
    /// `scope.noise_globs` — ADR-0006 §7.2's list, loaded so a test can compare it with the config
    /// default instead of trusting the prose.
    pub noise_globs: Vec<String>,
}

impl ScopeModel {
    /// The embedded `schema/mi-definition.json`.
    ///
    /// # Panics
    ///
    /// If the shipped model does not carry `scope.tests_default.test_globs` — which is a build-time
    /// fact about this repository, not a runtime condition, and the model checker in
    /// `scripts/validate_mi_definition.py` is the guard.
    #[must_use]
    pub fn embedded() -> Self {
        Self::from_json(MI_DEFINITION_JSON)
            .expect("schema/mi-definition.json carries its scope block")
    }

    /// `scope.tests_default.test_globs` and `scope.noise_globs` out of one model document.
    ///
    /// # Errors
    ///
    /// [`serde_json::Error`] when the document does not parse or does not carry those two fields.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        #[derive(Deserialize)]
        struct Scope {
            noise_globs: Vec<String>,
            tests_default: Tests,
        }
        #[derive(Deserialize)]
        struct Tests {
            test_globs: Vec<String>,
        }
        #[derive(Deserialize)]
        struct Document {
            scope: Scope,
        }
        let parsed: Document = serde_json::from_str(json)?;
        Ok(Self {
            test_globs: parsed.scope.tests_default.test_globs,
            noise_globs: parsed.scope.noise_globs,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glob(text: &str) -> Glob {
        Glob::compile(text).expect("compile the glob")
    }

    fn hits(text: &str, paths: &[&str]) {
        let compiled = glob(text);
        for path in paths {
            assert!(compiled.matches(path), "{text:?} should match {path:?}");
        }
    }

    fn misses(text: &str, paths: &[&str]) {
        let compiled = glob(text);
        for path in paths {
            assert!(
                !compiled.matches(path),
                "{text:?} should not match {path:?}"
            );
        }
    }

    #[test]
    fn a_star_stays_inside_its_segment() {
        hits("*.rs", &["main.rs", "a.rs"]);
        misses("*.rs", &["src/main.rs", "main.ts", "rs"]);
    }

    #[test]
    fn a_double_star_crosses_slashes_and_may_be_empty() {
        hits(
            "**/*.lock",
            &["Cargo.lock", "vendor/Cargo.lock", "a/b/c/poetry.lock"],
        );
        hits("target/**", &["target/x", "target/a/b/c"]);
        misses("target/**", &["targets/x", "src/target/x"]);
        hits("**/tests/**", &["tests/x.rs", "crates/fathom/tests/x.rs"]);
    }

    #[test]
    fn a_glob_without_a_slash_is_anchored_at_the_root() {
        hits("Cargo.lock", &["Cargo.lock"]);
        misses("Cargo.lock", &["vendor/Cargo.lock", "nested/Cargo.lock"]);
    }

    #[test]
    fn a_trailing_slash_means_the_whole_subtree() {
        hits(".venv/", &[".venv/lib/x.py"]);
        misses(".venv/", &[".venvrc"]);
    }

    #[test]
    fn classes_question_marks_and_escapes_work_as_documented() {
        hits("**/*.min.*", &["a/b/app.min.js", "x.min.map"]);
        hits("f?le.txt", &["file.txt"]);
        misses("f?le.txt", &["fäule.txt", "fire.txt"]);
        hits("[ab]*.rs", &["a_x.rs", "b_y.rs"]);
        misses("[ab]*.rs", &["c_x.rs"]);
        hits("[!ab]*.rs", &["c_x.rs"]);
        misses("[!ab]*.rs", &["a_x.rs"]);
        hits(r"a\*b.rs", &["a*b.rs"]);
        misses(r"a\*b.rs", &["axb.rs"]);
    }

    #[test]
    fn matching_is_case_sensitive() {
        misses("Target/**", &["target/x"]);
        hits("target/**", &["target/x"]);
    }

    #[test]
    fn a_broken_class_is_a_refusal_naming_the_glob() {
        let err = Glob::compile("[abc").expect_err("unterminated class");
        assert!(err.to_string().contains("[abc"), "{err}");
        assert!(err.to_string().contains("fix:"), "{err}");
        Glob::compile("[a-]").expect("a trailing dash is a literal, not a range");
        Glob::compile("[]]").expect("a leading ] is the class's only member");
        let backwards = Glob::compile("[z-a]").expect_err("a backwards range matches nothing");
        assert!(backwards.to_string().contains("z-a"), "{backwards}");
    }

    #[test]
    fn the_default_classifier_labels_the_default_noise_and_the_tests() {
        let classifier = Classifier::default();
        assert_eq!(classifier.classify("src/lib.rs"), Scope::Source);
        assert_eq!(classifier.classify("Cargo.lock"), Scope::Noise);
        assert_eq!(classifier.classify("vendor/vendored.rs"), Scope::Noise);
        assert_eq!(classifier.classify("app.min.js"), Scope::Noise);
        assert_eq!(
            classifier.classify("crates/fathom/tests/cli.rs"),
            Scope::Noise
        );
        assert_eq!(classifier.classify("tests/x.py"), Scope::Noise);
        assert_eq!(classifier.classify("src/test_helpers.rs"), Scope::Source);
        assert_eq!(classifier.classify(".evidence/run.jsonl"), Scope::Excluded);
        assert_eq!(
            classifier.classify(".fathom/index/meta.json"),
            Scope::Excluded
        );
        // Precedence: exclude beats noise, and both beat source.
        assert_eq!(classifier.classify(".evidence/Cargo.lock"), Scope::Excluded);
        assert!(classifier.tests_are_excluded());
    }

    #[test]
    fn include_tests_moves_both_columns_together() {
        let mut config = Config::default();
        config.paths.include_tests = true;
        let classifier = Classifier::try_from_config(&config).expect("compile");
        assert_eq!(
            classifier.classify("crates/fathom/tests/cli.rs"),
            Scope::Source
        );
        assert!(!classifier.tests_are_excluded());
        // Noise is still noise: opting into tests does not opt into lockfiles.
        assert_eq!(classifier.classify("Cargo.lock"), Scope::Noise);
    }

    #[test]
    fn a_path_outside_a_narrowed_source_set_is_out_of_scope() {
        let mut config = Config::default();
        config.paths.source = vec!["**/*.rs".to_owned()];
        let classifier = Classifier::try_from_config(&config).expect("compile");
        assert_eq!(classifier.classify("notes.md"), Scope::Excluded);
        assert_eq!(classifier.classify("src/lib.rs"), Scope::Source);
    }

    #[test]
    fn the_noise_default_in_config_and_in_the_mi_model_are_the_same_list() {
        // ADR-0007 C11, in Rust: one classifier, one noise list, checked rather than asserted.
        let model = ScopeModel::embedded();
        assert_eq!(
            model.noise_globs,
            crate::config::DEFAULT_NOISE_GLOBS.to_vec(),
            "schema/mi-definition.json and crates/fathom/src/config.rs must name one noise list"
        );
        assert!(
            model.test_globs.iter().any(|glob| glob == "**/test_*.py"),
            "the model's test globs are the ones ADR-0007 §7 declares"
        );
    }
}
