//! The string shapes ADR-0002 pins, written out by hand instead of as regexes.
//!
//! Every predicate here is the prose-and-regex of `schema/evidence.schema.json`
//! translated into Rust, because the writer must be able to refuse a value without
//! compiling a regex engine on a hot path (and without a regex crate in the
//! dependency tree: the patterns are few, short, and anchored, so a hand-rolled
//! matcher is shorter than the build-time it would cost).
//!
//! `tests/evidence_contract.rs` is the guard: it runs every fixture line through
//! both matchers — the compiled JSON Schema (`jsonschema`) and these predicates —
//! and fails if they ever disagree about a value. That is what keeps this file from
//! silently drifting away from the schema, which is the failure mode ADR-0002 §7
//! exists to prevent.
//!
//! Naming convention: each function is `is_<schema-noun>` and takes a `&str`.

// The pinned patterns, copied verbatim from `schema/evidence.schema.json`. They are
// here so `records.rs` can hand the same pattern to `schemars` (for the derived
// record schema) without writing it out a second time; the *matchers* below are
// hand-written translations of them, and `tests/evidence_contract.rs` is what proves
// matcher and schema still agree — it reads the patterns out of the schema file and
// runs both over the same values.

/// `run_id` (ADR-0002 §1.2).
pub const RUN_ID_PATTERN: &str = r"^[0-9]{8}T[0-9]{6}Z-[0-9a-f]{6}$";
/// `commit` (ADR-0002 §2.2).
pub const COMMIT_PATTERN: &str = r"^[0-9a-f]{40}$";
/// `workflow`, `task`.
pub const IDENTIFIER_PATTERN: &str = r"^[A-Za-z0-9][A-Za-z0-9 ._/-]*$";
/// `category`, `flags[]`, `labels` keys.
pub const WORD_PATTERN: &str = r"^[a-z][a-z0-9_]*$";
/// `host`.
pub const HOSTNAME_PATTERN: &str = r"^[A-Za-z0-9][A-Za-z0-9._-]*$";
/// `name`.
pub const METRIC_NAME_PATTERN: &str = r"^[A-Za-z][A-Za-z0-9_.:/-]*$";
/// `unit`.
pub const UNIT_PATTERN: &str = r"^[A-Za-z0-9_%/.^+-]+$";
/// `started_at`.
pub const INSTANT_PATTERN: &str =
    r"^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(\.[0-9]{1,6})?Z$";
/// `$defs/relpath`, shared by `path` and `source_path`.
pub const RELPATH_PATTERN: &str =
    r"^[.]{0,2}[A-Za-z0-9][A-Za-z0-9 ._+-]*(/[.]{0,2}[A-Za-z0-9][A-Za-z0-9 ._+-]*)*$";

/// The `run_id` shape of ADR-0002 §1.2: `^[0-9]{8}T[0-9]{6}Z-[0-9a-f]{6}$`.
///
/// Written by hand for the same reason as [`is_commit`]: the shape is fixed-width, so
/// indexing is both the check and the documentation. Deliberately identical to
/// [`crate::runner::RunId::is_valid_shape`] — there is a test asserting the two agree
/// over the same corpus, because a `run_id` the runner generates must be a `run_id`
/// the writer accepts.
#[must_use]
pub fn is_run_id(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() != 23 {
        return false;
    }
    let digit = |i: usize| bytes[i].is_ascii_digit();
    let lower_hex = |i: usize| bytes[i].is_ascii_hexdigit() && !bytes[i].is_ascii_uppercase();
    bytes[8] == b'T'
        && bytes[15] == b'Z'
        && bytes[16] == b'-'
        && (0..8).all(digit)
        && (9..15).all(digit)
        && (17..23).all(lower_hex)
}

/// The `commit` shape of ADR-0002 §2.2: `^[0-9a-f]{40}$`.
///
/// Exactly 40, exactly lowercase. Short SHAs are refused because the path and this
/// field are the join key a cross-commit scan relies on, and an uppercase SHA is a
/// different string in every filesystem and every `git log` diff.
#[must_use]
pub fn is_commit(text: &str) -> bool {
    text.len() == 40
        && text
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

/// `^[A-Za-z0-9][A-Za-z0-9 ._/-]*$`, ≤ 255 — `workflow` and `task`.
///
/// The alphabet is small on purpose: these two land in `meta.json` and in a tool
/// result, and nothing may smuggle a shell metacharacter or a path separator with a
/// meaning into them. `workflow` in particular is an *identity*, never a path.
#[must_use]
pub fn is_identifier(text: &str) -> bool {
    is_bounded(
        text,
        1,
        255,
        &|first| first.is_ascii_alphanumeric(),
        &|rest| rest.is_ascii_alphanumeric() || matches!(rest, ' ' | '.' | '_' | '/' | '-'),
    )
}

/// `^[a-z][a-z0-9_]*$` — `category`, `flags[]` and the keys of `labels`.
///
/// Open vocabularies still have a spelling: the point of `^[a-z][a-z0-9_]*$` is that a
/// consumer can print an unknown value without guessing what it means, and that a
/// `group by` over `labels` keys never has to handle a value that differs from
/// another only by case.
///
/// `flags` additionally caps its length at 32; that bound lives with the flag list,
/// not here, because `category` allows up to 64.
#[must_use]
pub fn is_word(text: &str) -> bool {
    is_bounded(text, 1, 64, &|first| first.is_ascii_lowercase(), &|rest| {
        rest.is_ascii_lowercase() || rest.is_ascii_digit() || rest == '_'
    })
}

/// `^[a-z][a-z0-9_]*$` with the ≤ 32 bound `flags[]` and `labels` values live under.
#[must_use]
pub fn is_short_word(text: &str) -> bool {
    text.len() <= 32 && is_word(text)
}

/// `^[A-Za-z0-9][A-Za-z0-9._-]*$` — `host`.
///
/// Lowercasing happens where the hostname is *obtained* (there is no `gethostname` in
/// `std`), not here: the schema allows uppercase, the ADR says the writer lowercases.
#[must_use]
pub fn is_hostname(text: &str) -> bool {
    is_bounded(
        text,
        1,
        255,
        &|first| first.is_ascii_alphanumeric(),
        &|rest| rest.is_ascii_alphanumeric() || matches!(rest, '.' | '_' | '-'),
    )
}

/// The `name` shape: `^[A-Za-z][A-Za-z0-9_.:/-]*$`, ≤ 128.
///
/// `name` is a metric name verbatim from a job (or one of the built-in `*_duration_s`
/// names) and, on a coverage record, a label. It must start with a letter so it can
/// never be mistaken for a number or a path, and `/` is allowed because metric names
/// like `bench/encode` are what jobs actually print.
#[must_use]
pub fn is_metric_name(text: &str) -> bool {
    is_bounded(
        text,
        1,
        128,
        &|first| first.is_ascii_alphabetic(),
        &|rest| rest.is_ascii_alphanumeric() || matches!(rest, '_' | '.' | ':' | '/' | '-'),
    )
}

/// The `unit` shape: `^[A-Za-z0-9_%/.^+-]+$`, 1..=32.
///
/// Verbatim from the job — act-mcp never converts a unit, so the alphabet has to be
/// wide enough for `ops/s`, `µs`-free ASCII spellings, `%`, and `count^2`. The empty
/// string is refused on purpose: "no unit reported" is absence plus the
/// `unit_missing` flag (ADR-0002 §4.1), not `unit: ""`.
#[must_use]
pub fn is_unit(text: &str) -> bool {
    is_bounded(
        text,
        1,
        32,
        &|first| {
            first.is_ascii_alphanumeric()
                || matches!(first, '_' | '%' | '/' | '.' | '^' | '+' | '-')
        },
        &|rest| {
            rest.is_ascii_alphanumeric() || matches!(rest, '_' | '%' | '/' | '.' | '^' | '+' | '-')
        },
    )
}

/// A UTC instant: `^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(\.[0-9]{1,6})?Z$`.
///
/// Always `Z`, never `+00:00` — the whole contract is UTC, and a spelled-out offset is
/// how two records from one run end up disagreeing about when it started. Only the
/// shape is checked here; "is 2027-02-30 a date" is left to the schema and to
/// [`UtcInstant::from_datetime`][super::records::UtcInstant::from_datetime], which is
/// the only way a real clock reaches a record.
#[must_use]
pub fn is_instant(text: &str) -> bool {
    let bytes = text.as_bytes();
    let digit = |i: usize| bytes.get(i).is_some_and(u8::is_ascii_digit);
    let date_and_time = bytes.len() >= 20
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18]
            .iter()
            .all(|&i| digit(i));
    if !date_and_time {
        return false;
    }
    match &text[19..] {
        // No fractional part: the `Z` sits right after the seconds.
        "Z" => true,
        // A fractional part of 1..=6 digits, then the `Z`.
        rest => match rest
            .strip_prefix('.')
            .and_then(|tail| tail.strip_suffix('Z'))
        {
            Some(digits) => {
                !digits.is_empty()
                    && digits.len() <= 6
                    && digits.bytes().all(|byte| byte.is_ascii_digit())
            }
            None => false,
        },
    }
}

/// `$defs/relpath`: forward-slash-relative, no leading `/`, no `~`, no `.` or `..`
/// segment, ≤ 512 characters.
///
/// The pinned pattern is
/// `^[.]{0,2}[A-Za-z0-9][A-Za-z0-9 ._+-]*(/[.]{0,2}[A-Za-z0-9][A-Za-z0-9 ._+-]*)*$`:
/// a segment may start with at most two dots, and must then continue with an
/// alphanumeric. That is what makes `.evidence` a legal first segment while `..` is
/// not — the traversal ADR-0002 §6.3 refuses in paths is refused here in *values*
/// too, so a scanner can never be talked into `path: "../../etc/passwd"`.
#[must_use]
pub fn is_relpath(text: &str) -> bool {
    if text.is_empty() || text.len() > 512 {
        return false;
    }
    let segment_ok = |segment: &str| {
        let bytes = segment.as_bytes();
        // Only *leading* dots count towards the `[.]{0,2}` the pattern allows; a dot
        // anywhere else belongs to the segment body ("a..b" is an odd but legal name).
        let dots = bytes
            .iter()
            .take(3)
            .take_while(|&&byte| byte == b'.')
            .count();
        if dots == 3 || bytes.len() == dots {
            return false;
        }
        let body = &segment[dots..];
        let mut chars = body.chars();
        // The schema demands an alphanumeric first, then anything from a small set.
        chars
            .next()
            .is_some_and(|first| first.is_ascii_alphanumeric())
            && chars.all(|rest| {
                rest.is_ascii_alphanumeric() || matches!(rest, ' ' | '.' | '_' | '+' | '-')
            })
    };
    text.split('/').all(segment_ok)
}

/// `^[a-z][a-z0-9_]*$` capped at 128 — the *values* of `labels`.
///
/// Label values are always strings (a consumer must never type-switch to `group by`)
/// and ≤ 128 bytes, but they are free text: `matrix: "1.83, x64"` is legal, so this
/// is a length bound and a NUL/control guard, not a pattern.
#[must_use]
pub fn is_label_value(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 128
        && !text.chars().any(|character| (character as u32) < 0x20)
}

/// `first` must satisfy `head`, every following character must satisfy `tail`, and the
/// whole string must sit inside `min..=max` bytes.
///
/// Byte lengths, not `char` counts: every bound in the schema is a `maxLength`, which
/// JSON Schema counts in Unicode code points, but every alphabet here is ASCII, so
/// the two readings agree — and a non-ASCII value fails the alphabet check anyway
/// rather than silently passing a length check.
fn is_bounded(
    text: &str,
    min: usize,
    max: usize,
    head: &dyn Fn(char) -> bool,
    tail: &dyn Fn(char) -> bool,
) -> bool {
    if text.len() < min || text.len() > max {
        return false;
    }
    let mut chars = text.chars();
    chars.next().is_some_and(head) && chars.all(tail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_id_shape() {
        for good in ["20261110T004203Z-a1b2c3", "20260101T000000Z-000000"] {
            assert!(is_run_id(good), "{good} must be accepted");
        }
        for bad in [
            "a1b2c3-20261110T004203Z", // the fixture: unsorted, so unsortable
            "20261110T004203Z-A1B2C3", // uppercase hex
            "20261110T004203Z-a1b2c",  // 5 hex
            "20261110t004203z-a1b2c3", // lowercase separators
            "20261110T004203-a1b2c3",  // no Z
            "",
            "20261110T004203Z-a1b2c34",
        ] {
            assert!(!is_run_id(bad), "{bad:?} must be refused");
        }
    }

    #[test]
    fn commit_shape() {
        assert!(is_commit("2f7c1a4b9e0d3c6a5b8f1e2d4c6a8b0f3e5d7c91"));
        for bad in [
            "2f7c1a4",
            "2F7C1A4B9E0D3C6A5B8F1E2D4C6A8B0F3E5D7C91",
            "2f7c1a4b9e0d3c6a5b8f1e2d4c6a8b0f3e5d7c9", // 39
            "2f7c1a4b9e0d3c6a5b8f1e2d4c6a8b0f3e5d7c911", // 41
            "gh7c1a4b9e0d3c6a5b8f1e2d4c6a8b0f3e5d7c91",
        ] {
            assert!(!is_commit(bad), "{bad:?} must be refused");
        }
    }

    #[test]
    fn instants_are_utc_z_only() {
        for good in [
            "2026-11-10T00:42:03.417Z",
            "2026-11-10T00:42:03Z",
            "2026-11-10T00:42:03.123456Z",
        ] {
            assert!(is_instant(good), "{good} must be accepted");
        }
        for bad in [
            "2026-11-10T01:42:03+01:00",    // the fixture
            "2026-11-10T00:42:03",          // no zone
            "2026-11-10T00:42:03.1234567Z", // 7 fractional digits
            "2026-11-10T00:42:03.z",
            "2026-11-10 00:42:03Z",
            "2026-11-10T00:42:03z",
            "",
        ] {
            assert!(!is_instant(bad), "{bad:?} must be refused");
        }
    }

    #[test]
    fn relpath_allows_dot_prefixed_names_but_not_dotdot() {
        for good in [
            ".evidence/incoming/20261110T004203Z-a1b2c3/benchmark.json",
            "src/parser/tokenizer.rs",
            "a",
            ".hidden/file.txt",
            "..hidden/file.txt",
            "coverage/lcov.info",
            "a b/c-d.e+f",
        ] {
            assert!(is_relpath(good), "{good} must be accepted");
        }
        for bad in [
            "",
            "/home/dev/repo/benchmark.json", // absolute host path
            "/srv/src/app.py",               // container path
            "../../etc/passwd",              // traversal (fixture)
            "a/../../etc/passwd",            // traversal, mid-path (fixture)
            "~/secrets",
            ".../x",
            "src//dup.rs",
            "trailing/",
            "unhealthy\u{1}name",
            "x".repeat(513).as_str(),
        ] {
            assert!(!is_relpath(bad), "{bad:?} must be refused");
        }
    }

    #[test]
    fn small_alphabets() {
        assert!(is_identifier("hyperfine-bench"));
        assert!(is_identifier("coverage istanbul/v2"));
        assert!(!is_identifier("-leading"));
        // Dots are inside the alphabet ("a.b" is a legal job id), so `..` is too: the
        // schema says so and the writer must not disagree.
        assert!(is_identifier("double..dot"));
        assert!(!is_identifier("tab\there"));
        assert!(!is_identifier(""));

        assert!(is_word("unit_missing"));
        assert!(is_word("percent_only"));
        assert!(!is_word("PercentOnly"));
        assert!(!is_word("9 lives"));
        assert!(is_short_word("truncated"));
        assert!(!is_short_word(&"a".repeat(33)));

        assert!(is_metric_name("job_duration_s"));
        assert!(is_metric_name("bench/encode"));
        assert!(!is_metric_name("1xx"));

        assert!(is_unit("ops/s"));
        assert!(is_unit("%"));
        assert!(is_unit("count^2"));
        assert!(!is_unit(""));
        assert!(!is_unit("ms;rm -rf"));

        assert!(is_hostname("build-box-07"));
        assert!(!is_hostname("-lead"));
        assert!(!is_hostname(""));

        assert!(is_label_value("1.83, x64"));
        assert!(!is_label_value(""));
        assert!(!is_label_value(&"v".repeat(129)));
    }
}
