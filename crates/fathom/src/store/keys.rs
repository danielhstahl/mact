//! Key derivation: `fk1`, canonical JSON, and the block key (ADR-0006 §6.3, P6).
//!
//! Everything the store files by content is filed under one of these digests, so this module is
//! where "a key covers every input the value depends on" (rule R1) is either true or a bug. The
//! two spellings below are the ADR's, transcribed rather than re-invented:
//!
//! ```text
//! fk1(s)   = "fk1:" || lowercase hex sha256(UTF-8 bytes of s)      // no trailing newline
//! canon(v) = canonical JSON — object keys in ascending code-point order, no insignificant
//!            whitespace, no nulls                                   // ADR-0002 §6.1
//! ```
//!
//! `canon` is written here rather than borrowed from `serde_json`'s default map ordering on purpose:
//! the ADR's rule drops nulls, and a value that changes shape because a dependency enabled
//! `serde_json`'s `preserve_order` feature is a cache that silently re-keys on someone else's build.
//! Dropping nulls is ADR-0002 §2.1's "unknown is absent, never zero" applied to a digest: absent and
//! `null` must not be two different keys for one value.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// The fingerprint prefix of every key this store writes (§6.3).
pub const FK1_PREFIX: &str = "fk1:";

/// The domain separator of a config digest (§6.3's `fc1`).
pub const CONFIG_DOMAIN: &str = "fc1";

/// The domain separator of a block key (§6.3's `fb1`).
pub const BLOCK_DOMAIN: &str = "fb1";

/// `fk1(s)`: `"fk1:"` and the lowercase hex sha256 of the bytes, no trailing newline.
#[must_use]
pub fn fk1(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(FK1_PREFIX.len() + 64);
    out.push_str(FK1_PREFIX);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// `fk1` over text, which is what every §6.3 pre-image is.
#[must_use]
pub fn fk1_str(text: &str) -> String {
    fk1(text.as_bytes())
}

/// The bare 64 hex characters of a key, without the `fk1:` prefix.
#[must_use]
pub fn bare(key: &str) -> &str {
    key.strip_prefix(FK1_PREFIX).unwrap_or(key)
}

/// Canonical JSON: keys sorted by code point, no whitespace, no nulls (ADR-0002 §6.1).
#[must_use]
pub fn canon(value: &impl Serialize) -> String {
    canon_value(&serde_json::to_value(value).unwrap_or(Value::Null))
}

/// [`canon`] over an already-built [`Value`].
pub fn canon_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => escape(text),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(canon_value).collect();
            format!("[{}]", parts.join(","))
        }
        Value::Object(object) => {
            // Sorted, and nulls dropped: one spelling for "not known" and one order for one object.
            let sorted: BTreeMap<&str, &Value> = object
                .iter()
                .filter(|(_, value)| !value.is_null())
                .map(|(key, value)| (key.as_str(), value))
                .collect();
            let parts: Vec<String> = sorted
                .into_iter()
                .map(|(key, value)| format!("{}:{}", escape(key), canon_value(value)))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
    }
}

/// JSON's own string escaping, through serde_json so the two cannot disagree about a control byte.
fn escape(text: &str) -> String {
    Value::String(text.to_owned()).to_string()
}

/// `config_digest(keys)` (§6.3): `fk1("fc1\0" || canon({ "<dotted.key>": resolved, … }))`.
///
/// Only the *declared* keys, and only their *resolved* values — defaults already applied — which is
/// what makes "a file that omits a key and a file that states the default produce the same digest"
/// (§7.2 rule 2) true rather than hoped for.
#[must_use]
pub fn config_digest(keys: &[&str], values: &BTreeMap<String, Value>) -> String {
    let mut object = Map::new();
    for key in sorted(keys) {
        if let Some(value) = values.get(key) {
            object.insert((*key).to_owned(), value.clone());
        }
    }
    fk1_str(&format!(
        "{CONFIG_DOMAIN}\0{}",
        canon_value(&Value::Object(object))
    ))
}

/// The digest of an analyzer's transitive dependencies (§6.3): the sorted `id@version` list.
#[must_use]
pub fn dep_digest(dependencies: &[(&str, u32)]) -> String {
    let mut spelled: Vec<String> = dependencies
        .iter()
        .map(|(id, version)| format!("{id}@{version}"))
        .collect();
    spelled.sort();
    fk1_str(&canon_value(&Value::Array(
        spelled.into_iter().map(Value::String).collect(),
    )))
}

/// The six input kinds of §6.3, as stored strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputKind(pub &'static str);

impl InputKind {
    /// Content only: path-independent.
    pub const BLOB: Self = Self("blob");
    /// The repo-relative path and nothing else.
    pub const PATH: Self = Self("path");
    /// Path plus content, for a value whose *language* comes from the path (ADR-0007 P13).
    pub const BLOB_PATH: Self = Self("blob_path");
    /// A git tree oid.
    pub const TREE: Self = Self("tree");
    /// A git commit oid.
    pub const COMMIT: Self = Self("commit");
    /// The sorted `path\0sha256` list of the evidence files a value was derived from.
    pub const EVIDENCE: Self = Self("evidence");

    /// The stored spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

/// `input_hash` for [`InputKind::Blob`]: `fk1(bytes)`, content only.
#[must_use]
pub fn input_hash_blob(bytes: &[u8]) -> String {
    fk1(bytes)
}

/// `input_hash` for [`InputKind::BlobPath`]: `"bp:" || path || "\0" || fk1(bytes)`.
#[must_use]
pub fn input_hash_blob_path(path: &str, bytes: &[u8]) -> String {
    format!("bp:{path}\0{}", fk1(bytes))
}

/// `input_hash` for [`InputKind::Path`]: `"p:" || repo-relative path`.
#[must_use]
pub fn input_hash_path(path: &str) -> String {
    format!("p:{path}")
}

/// `input_hash` for [`InputKind::Tree`]: `"t:" || tree oid`.
#[must_use]
pub fn input_hash_tree(tree: &str) -> String {
    format!("t:{tree}")
}

/// `input_hash` for [`InputKind::Commit`]: `"c:" || commit oid`.
#[must_use]
pub fn input_hash_commit(commit: &str) -> String {
    format!("c:{commit}")
}

/// `input_hash` for [`InputKind::Evidence`]: `fk1(canon(sorted(["path\0sha256", …])))`.
///
/// This is the one input that can change *under* a row, because `.evidence` is append-only while git
/// is immutable: a re-run of act-mcp for an old commit adds a file, its sha256 enters this list, and
/// the row that consumed it is stale from that moment (ADR-0006 §6.2).
#[must_use]
pub fn input_hash_evidence(entries: &[String]) -> String {
    let mut sorted = entries.to_vec();
    sorted.sort();
    fk1_str(&canon_value(&Value::Array(
        sorted.into_iter().map(Value::String).collect(),
    )))
}

/// `block_key` (§6.3), the whole pre-image in one string.
#[must_use]
#[allow(clippy::implicit_hasher)] // the six pieces are positional; a map here would be a disguise
pub fn block_key(
    analyzer_id: &str,
    analyzer_version: u32,
    dep_digest: &str,
    config_digest: &str,
    input_kind: &str,
    input_hash: &str,
) -> String {
    fk1_str(&format!(
        "{BLOCK_DOMAIN}\0{analyzer_id}\0{analyzer_version}\0{dep_digest}\0{config_digest}\0{input_kind}\0{input_hash}"
    ))
}

/// Whether a string is a well-formed key: `fk1:` and 64 lowercase hex.
#[must_use]
pub fn is_key(key: &str) -> bool {
    let Some(hex) = key.strip_prefix(FK1_PREFIX) else {
        return false;
    };
    hex.len() == 64
        && hex
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

/// Whether a string is a 40-hex commit id (a row's filename, §6.4's `commit == filename`).
#[must_use]
pub fn is_commit_id(text: &str) -> bool {
    text.len() == 40
        && text
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

/// The two-hex bucket a key or id lives under (§4.1's `<xx>`), so no directory holds the whole store.
#[must_use]
pub fn bucket(text: &str) -> String {
    let hex = bare(text);
    hex.chars().take(2).collect()
}

/// The declared keys, in ascending order — the order §6.3's `canon` requires, applied once here.
fn sorted<'a>(keys: &[&'a str]) -> Vec<&'a str> {
    let mut out = keys.to_vec();
    out.sort_unstable();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fk1_is_the_sha256_of_the_bytes_with_no_trailing_newline() {
        // sha256("") — the empty input is the case where a stray newline is easiest to hide.
        assert_eq!(
            fk1(b""),
            "fk1:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_ne!(fk1(b"x"), fk1(b"x\n"), "the ADR's rule, executed");
        assert!(is_key(&fk1(b"x")));
        assert!(!is_key(&fk1(b"x").to_uppercase()));
    }

    #[test]
    fn canon_sorts_keys_drops_nulls_and_keeps_no_whitespace() {
        let mut outer = Map::new();
        outer.insert("b".to_owned(), Value::from(1));
        outer.insert("a".to_owned(), Value::from(2));
        outer.insert("z".to_owned(), Value::Null);
        let mut inner = Map::new();
        inner.insert("y".to_owned(), Value::from("s"));
        inner.insert("x".to_owned(), Value::Null);
        outer.insert("n".to_owned(), Value::Object(inner));
        assert_eq!(
            canon_value(&Value::Object(outer)),
            r#"{"a":2,"b":1,"n":{"y":"s"}}"#,
            "nulls are dropped, at every depth"
        );
    }

    #[test]
    fn canon_of_a_struct_drops_skipped_and_null_members() {
        #[derive(Serialize)]
        struct Row {
            b: u32,
            a: Option<u32>,
        }
        assert_eq!(canon(&Row { b: 1, a: None }), r#"{"b":1}"#);
        assert_eq!(canon(&Row { b: 1, a: Some(2) }), r#"{"a":2,"b":1}"#);
    }

    #[test]
    fn a_config_digest_moves_only_with_the_declared_keys() {
        let mut values: BTreeMap<String, Value> = BTreeMap::new();
        let mut insert = |key: &str, value: &str| {
            values.insert(key.to_owned(), Value::from(value.to_owned()));
        };
        insert("paths.noise", "a");
        insert("paths.exclude", "b");
        insert("benchmarks.deny", "c");
        let declared = ["paths.noise", "paths.exclude"];
        let before = config_digest(&declared, &values);
        // A key nobody declared cannot move the digest…
        let mut with_undeclared = values.clone();
        with_undeclared.insert(
            "benchmarks.deny".to_owned(),
            Value::from("changed".to_owned()),
        );
        assert_eq!(
            config_digest(&declared, &with_undeclared),
            before,
            "the digest covers the declared keys and nothing else (§7.2 rule 2)"
        );
        // …and the order the keys were declared in cannot either (§6.3: keys sorted).
        assert_eq!(
            config_digest(&["paths.exclude", "paths.noise"], &values),
            before
        );
        let mut moved = values.clone();
        moved.insert("paths.noise".to_owned(), Value::from("a2".to_owned()));
        assert_ne!(config_digest(&declared, &moved), before);
    }

    #[test]
    fn dep_digest_is_order_independent() {
        assert_eq!(
            dep_digest(&[("mi.tokenize", 1), ("other", 2)]),
            dep_digest(&[("other", 2), ("mi.tokenize", 1)])
        );
        assert_ne!(
            dep_digest(&[("mi.tokenize", 1)]),
            dep_digest(&[("mi.tokenize", 2)])
        );
    }

    #[test]
    fn block_keys_separate_the_six_inputs() {
        let base = block_key("mi.score", 1, "d", "c", "blob_path", "h");
        assert_eq!(base, block_key("mi.score", 1, "d", "c", "blob_path", "h"));
        assert_ne!(base, block_key("mi.score", 2, "d", "c", "blob_path", "h"));
        assert_ne!(base, block_key("mi.score", 1, "d2", "c", "blob_path", "h"));
        assert_ne!(base, block_key("mi.score", 1, "d", "c2", "blob_path", "h"));
        assert_ne!(base, block_key("mi.score", 1, "d", "c", "blob", "h"));
        assert_ne!(base, block_key("mi.score", 1, "d", "c", "blob_path", "h2"));
        assert!(is_key(&base));
    }

    #[test]
    fn evidence_hash_is_a_set_not_a_sequence() {
        let a = "a\0dead".to_owned();
        let b = "b\0feed".to_owned();
        assert_eq!(
            input_hash_evidence(&[a.clone(), b.clone()]),
            input_hash_evidence(&[b, a])
        );
    }

    #[test]
    fn buckets_are_the_first_two_hex_and_strips_the_prefix() {
        assert_eq!(bucket("fk1:abcdef"), "ab");
        assert_eq!(bucket("abcdef0123456789"), "ab");
        assert!(is_commit_id(&"a".repeat(40)));
        assert!(!is_commit_id(&"A".repeat(40)));
        assert!(!is_commit_id(&"a".repeat(39)));
    }
}
