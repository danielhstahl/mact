//! The `category` argument, validated in Rust rather than only advertised in a schema.
//!
//! `category` is the one `run_workflow` argument with an **open** vocabulary (ADR-0001 §2.4:
//! `benchmark` and `coverage` are the two known values; anything else shape-legal runs the
//! job and produces no evidence), and it is also the argument the epic uses to choose an
//! output *file*. Those two facts together are the whole reason this module exists: an open
//! vocabulary that reaches a filesystem path is the classic path-traversal shape, and a JSON
//! `pattern` in `tools/list` is an advertisement, not a control — a client that never read
//! the schema, or a hand-written `curl`, is not stopped by it.
//!
//! ## The two rules
//!
//! 1. **Shape first.** [`is_valid_category`] implements, in Rust, exactly the pattern
//!    [`schemas::category`] advertises (`^[a-z][a-z0-9_]*$`, 1..=64 bytes). That pattern
//!    contains no `.`, no `/`, and no leading `-` by construction, so a category that passes
//!    this check cannot be a path, a traversal, or an option. [`SCHEMA_PATTERN`] is the
//!    string both sides name, and `the_rust_check_mirrors_the_advertised_schema` fails if
//!    they ever drift.
//! 2. **A validated category still does not build a path.** The output file name comes from
//!    [`Kind`], the *closed* `benchmark | coverage` enum (ADR-0002 §2.2), reached through
//!    [`kind_of`]. An unknown-but-legal category (`fuzz`, say) runs the job and publishes
//!    nothing — which is what ADR-0001 §2.4 says it should do — so there is no code path in
//!    this crate in which a raw category string is joined onto a path. The evidence writer's
//!    own [`EvidenceRoot::checked_child`] then refuses `..` and symlink escapes as the second
//!    line of defence.
//!
//! mact-f1w.14 (`run_workflow`) calls [`validate`] where it parses arguments, exactly as
//! `list_tasks` parses its own; the unit tests here and the category tests in
//! `tests/hardening.rs` are the proof that the check refuses the shapes that matter.
//!
//! [`schemas::category`]: super::schemas::category
//! [`Kind`]: crate::evidence::records::Kind
//! [`EvidenceRoot::checked_child`]: crate::evidence::EvidenceRoot

use serde_json::Value;

use crate::evidence::records::Kind;

use super::ArgumentProblem;

/// The pattern `category` must match, spelled as JSON Schema spells it.
///
/// Both the schema (`tools::schemas::category`) and the Rust check ([`is_valid_category`])
/// name this string; a test asserts they agree, so there is one truth and it is here.
pub const SCHEMA_PATTERN: &str = r"^[a-z][a-z0-9_]*$";

/// `maxLength` from the same schema.
pub const MAX_CATEGORY_BYTES: usize = 64;

/// The `expected` text of an argument problem, spelled with the pattern in it.
pub const EXPECTED: &str =
    "a category: lowercase letters, digits and underscores, ^[a-z][a-z0-9_]*$, at most 64 bytes";

/// The two categories that produce evidence, spelled as the schema spells them.
pub const KNOWN_CATEGORIES: [&str; 2] = [Kind::Benchmark.as_str(), Kind::Coverage.as_str()];

/// Whether `text` is a shape-legal category.
///
/// Hand-written rather than a regex on purpose: the crate has no regex dependency, the rule
/// is four lines of byte arithmetic, and a hand-written check cannot be fed a pattern that
/// backtracks.
#[must_use]
pub fn is_valid_category(text: &str) -> bool {
    let bytes = text.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_CATEGORY_BYTES
        && bytes[0].is_ascii_lowercase()
        && bytes[1..]
            .iter()
            .all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'_'))
}

/// Which evidence kind a category names, `None` for an unknown-but-legal one.
///
/// This is the only mapping from the caller's vocabulary to anything that reaches a
/// filesystem path, and it is a total function into a two-variant enum: the path is built
/// from [`Kind::as_str`], never from the caller's text.
#[must_use]
pub fn kind_of(category: &str) -> Option<Kind> {
    if is_valid_category(category) {
        Kind::parse(category).ok()
    } else {
        None
    }
}

/// Refuse a category that is not shape-legal, in the shape ADR-0001 §5.3 wants.
///
/// # Errors
///
/// [`ArgumentProblem`] naming the `category` key, what is wrong with it, and the pattern it
/// should have matched.
pub fn validate(category: &str) -> Result<&str, ArgumentProblem> {
    if is_valid_category(category) {
        return Ok(category);
    }
    Err(ArgumentProblem {
        path: "category",
        reason: describe(category),
        expected: EXPECTED,
    })
}

/// Whether an advertised schema and this module still describe one rule.
///
/// Used by the parity test; not a validation path itself.
#[must_use]
pub fn schema_matches_advertised(schema: &Value) -> bool {
    schema.get("pattern").and_then(Value::as_str) == Some(SCHEMA_PATTERN)
        && schema
            .get("maxLength")
            .and_then(Value::as_u64)
            .is_some_and(|max| max as usize == MAX_CATEGORY_BYTES)
}

/// The `reason` prose, worded so the caller can fix it without a documentation lookup.
fn describe(category: &str) -> String {
    if category.is_empty() {
        return "must not be empty".to_owned();
    }
    if category.len() > MAX_CATEGORY_BYTES {
        return format!(
            "is {} bytes, over the {MAX_CATEGORY_BYTES}-byte limit",
            category.len()
        );
    }
    // Name the specific danger when the caller tried to use it as a path: this is the error
    // an agent gets after a hallucinated `../`, and the message is where it learns that was
    // the problem.
    if category.contains("..")
        || category.contains('/')
        || category.contains('\\')
        || category.contains('.')
    {
        return format!(
            "{category:?} is not a category: a category never contains a path \
                        separator, a dot, or a dot-dot"
        );
    }
    if category.starts_with(|c: char| !c.is_ascii_lowercase()) {
        return "must start with a lowercase letter".to_owned();
    }
    if !category.is_ascii() {
        return "must be ASCII".to_owned();
    }
    format!("{category:?} does not match `{SCHEMA_PATTERN}`")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::schemas;

    /// The shapes that must pass.
    const LEGAL: [&str; 8] = [
        "benchmark",
        "coverage",
        "other",
        "b",
        "unit_test",
        "bench2",
        "c_i_9",
        "x",
    ];

    /// …and the ones that must not. Each is either a traversal in disguise or a category an
    /// LLM has plausibly tried to send.
    const ILLEGAL: [&str; 15] = [
        "",
        "..",
        "../..",
        "../../etc/passwd",
        "../evil",
        "..%2f..",
        "benchmark/",
        "/etc/passwd",
        "benchmark/../../etc",
        "Benchmark",
        "_benchmark",
        "1benchmark",
        "bench-mark",
        "bench mark",
        "bench\nrm -rf /",
    ];

    #[test]
    fn legal_categories_pass() {
        for category in LEGAL {
            assert!(is_valid_category(category), "{category:?} must be legal");
            assert!(validate(category).is_ok(), "{category:?}");
        }
        // The two known categories are exactly the two that map to an evidence kind.
        for category in KNOWN_CATEGORIES {
            assert!(kind_of(category).is_some(), "{category} must map to a kind");
        }
        assert_eq!(kind_of("benchmark"), Some(Kind::Benchmark));
        assert_eq!(kind_of("coverage"), Some(Kind::Coverage));
        // Legal-but-unknown: runs the job, produces no evidence, and names no path.
        assert!(is_valid_category("fuzz"));
        assert_eq!(kind_of("fuzz"), None);
    }

    /// AC: "a test exists for path traversal via category".
    #[test]
    fn a_category_cannot_be_a_path() {
        for category in ILLEGAL {
            assert!(
                !is_valid_category(category),
                "{category:?} must be refused as a category"
            );
            let problem = validate(category).expect_err("must be refused");
            assert_eq!(problem.path, "category");
            // The prose names the pattern so the caller can self-correct.
            assert!(
                problem.expected.contains(SCHEMA_PATTERN),
                "the fix must show the pattern: {}",
                problem.expected
            );
            assert_eq!(kind_of(category), None, "{category:?} must pick no kind");
        }

        // The over-long one is not in the table because it is long; say it separately.
        let long = format!("b{}", "x".repeat(MAX_CATEGORY_BYTES));
        assert!(!is_valid_category(&long));
        assert!(validate(&long)
            .expect_err("too long")
            .reason
            .contains("over the"));
    }

    /// The advertised schema and the Rust check are one rule, not two that happen to agree.
    #[test]
    fn the_rust_check_mirrors_the_advertised_schema() {
        let advertised = schemas::category();
        assert!(
            schema_matches_advertised(&advertised),
            "tools/list advertises {advertised}, but this module enforces {SCHEMA_PATTERN} \
             with a {MAX_CATEGORY_BYTES}-byte limit"
        );
        assert!(EXPECTED.contains(SCHEMA_PATTERN));
    }

    #[test]
    fn the_refusal_says_which_shape_was_wrong() {
        assert!(validate("").expect_err("empty").reason.contains("empty"));
        assert!(validate("../evil")
            .expect_err("traversal")
            .reason
            .contains("separator"));
        assert!(validate("bench-mark")
            .expect_err("hyphen")
            .reason
            .contains(SCHEMA_PATTERN));
        assert!(validate("Benchmark")
            .expect_err("uppercase")
            .reason
            .contains("lowercase letter"));
        assert!(validate("_x")
            .expect_err("leading underscore")
            .reason
            .contains("lowercase letter"));
    }
}
