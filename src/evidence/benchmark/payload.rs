//! The benchmark payload: the file a job wrote through the `--bind` mount (ADR-0002 §3.1,
//! §3.2, §3.3).
//!
//! Two shapes, chosen by filename, because §3.2 pins the names and a sniffed format is a
//! third guess about a file the job could not describe itself:
//!
//! | name | shape |
//! |---|---|
//! | `benchmark.json` | `{"metrics":[…]}`, or a bare top-level array as shorthand |
//! | `benchmark.jsonl` | one metric entry per line, for `jq` pipelines |
//!
//! The file is untrusted input, so the read is bounded (§3.3: 8 MiB) and a file that cannot
//! be read, is too large, or is not parseable is a *reported condition* — the caller still
//! gets act's own timings, and the run keeps the status act gave it.

use std::{io::Read, path::Path};

use serde_json::Value;

/// Which of §3.3's two shapes a payload file holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Format {
    /// `{"metrics":[…]}` or a bare array.
    Json,
    /// One metric entry per line.
    Jsonlines,
}

impl Format {
    /// The name `meta.json` records a payload under.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Jsonlines => "jsonl",
        }
    }

    /// The format §3.3 assigns to a file called `name`.
    pub(crate) fn for_name(name: &str) -> Option<Self> {
        match name {
            "benchmark.json" => Some(Self::Json),
            "benchmark.jsonl" => Some(Self::Jsonlines),
            _ => None,
        }
    }
}

/// What a payload read produced.
#[derive(Debug)]
pub(crate) struct Parsed {
    /// The candidate entries, in the file's own order. Each still has to pass
    /// [`super::entry::parse`]; this is only the JSON layer.
    pub(crate) entries: Vec<Value>,
    /// Lines or entries that were present and unusable at the JSON layer.
    pub(crate) malformed: usize,
    /// Why the file as a whole was unusable, when it was: `meta.json`'s human half.
    pub(crate) detail: Option<String>,
}

/// Why a payload could not be read at all.
#[derive(Debug, Clone)]
pub(crate) enum ReadError {
    /// The file is over the §3.3 size cap; its size is named.
    TooLarge(u64),
    /// Open or read failed (permissions, a dangling symlink, a directory).
    Unreadable(String),
}

/// Read a payload, refusing to read more than `budget` bytes.
pub(crate) fn read(path: &Path, format: Format, budget: u64) -> Result<Parsed, ReadError> {
    let body = read_bounded(path, budget)?;
    Ok(match format {
        Format::Json => parse_json(&body),
        Format::Jsonlines => parse_jsonl(&body),
    })
}

/// `{"metrics":[…]}`, or a bare array. Anything else is a file that is not a benchmark
/// payload, and is named as one rather than quietly treated as an empty list of metrics.
fn parse_json(body: &str) -> Parsed {
    if body.trim().is_empty() {
        return Parsed {
            entries: Vec::new(),
            malformed: 0,
            detail: Some("the payload file was empty".to_owned()),
        };
    }
    let value: Value = match serde_json::from_str(body) {
        Ok(value) => value,
        Err(reason) => {
            return Parsed {
                entries: Vec::new(),
                malformed: 1,
                detail: Some(format!("not parseable JSON: {reason}")),
            }
        }
    };
    let (entries, shape) = match value {
        // `shape` is about the *file*: right shape and no metrics is a job that measured
        // nothing, which is not a malformed thing and must not be counted as one.
        Value::Array(items) => (items, true),
        Value::Object(mut fields) => match fields.remove("metrics") {
            Some(Value::Array(items)) => (items, true),
            _ => (Vec::new(), false),
        },
        _ => (Vec::new(), false),
    };
    if !shape {
        return Parsed {
            entries,
            malformed: 1,
            detail: Some(
                "a benchmark.json is an object with a `metrics` array, or a bare array of \
                 metric entries (§3.3)"
                    .to_owned(),
            ),
        };
    }
    if entries.is_empty() {
        return Parsed {
            entries,
            malformed: 0,
            detail: Some("the payload's `metrics` array was empty".to_owned()),
        };
    }
    Parsed {
        entries,
        malformed: 0,
        detail: None,
    }
}

/// One metric entry per line. A bad line costs that metric and nothing else, which is the
/// advantage §3.3 names for this shape.
fn parse_jsonl(body: &str) -> Parsed {
    let mut parsed = Parsed {
        entries: Vec::new(),
        malformed: 0,
        detail: None,
    };
    for line in body.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(value) => parsed.entries.push(value),
            Err(_) => parsed.malformed += 1,
        }
    }
    if parsed.entries.is_empty() && parsed.malformed > 0 {
        parsed.detail = Some("no line of the payload parsed as a JSON metric entry".to_owned());
    }
    parsed
}

/// Read at most `budget + 1` bytes, so "exactly at the cap" and "over the cap" are told apart.
fn read_bounded(path: &Path, budget: u64) -> Result<String, ReadError> {
    let len = std::fs::metadata(path).map(|meta| meta.len()).ok();
    if let Some(len) = len.filter(|len| *len > budget) {
        return Err(ReadError::TooLarge(len));
    }
    let mut file =
        std::fs::File::open(path).map_err(|reason| ReadError::Unreadable(reason.to_string()))?;
    let mut body = String::new();
    let read = file
        .by_ref()
        .take(budget.saturating_add(1))
        .read_to_string(&mut body);
    if let Err(reason) = read {
        // Includes "not valid UTF-8", which for an untrusted file is a fact about the file.
        return Err(ReadError::Unreadable(reason.to_string()));
    }
    if body.len() as u64 > budget {
        return Err(ReadError::TooLarge(body.len() as u64));
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn written(body: &str) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().expect("temp payload");
        std::fs::write(file.path(), body).expect("write payload");
        file
    }

    #[test]
    fn the_object_form_and_the_bare_array_form_are_both_accepted() {
        let object = written(r#"{"metrics":[{"name":"a","value":1}]}"#);
        let parsed = read(object.path(), Format::Json, 1 << 20).expect("read");
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.malformed, 0);

        let array = written(r#"[{"name":"a","value":1},{"name":"b","value":2}]"#);
        let parsed = read(array.path(), Format::Json, 1 << 20).expect("read");
        assert_eq!(parsed.entries.len(), 2);
    }

    #[test]
    fn a_file_that_is_not_a_payload_says_so() {
        for body in ["{}", r#"{"metrics":{}}"#, "\"just a string\""] {
            let file = written(body);
            let parsed = read(file.path(), Format::Json, 1 << 20).expect("read");
            assert!(parsed.entries.is_empty(), "{body} is not a payload");
            assert!(parsed.detail.is_some(), "{body} must explain itself");
        }
    }

    #[test]
    fn a_broken_line_of_a_jsonl_payload_costs_only_itself() {
        let file =
            written("{\"name\":\"a\",\"value\":1}\nnot json\n{\"name\":\"b\",\"value\":2}\n\n");
        let parsed = read(file.path(), Format::Jsonlines, 1 << 20).expect("read");
        assert_eq!(parsed.entries.len(), 2);
        assert_eq!(parsed.malformed, 1);
    }

    #[test]
    fn a_payload_that_measured_nothing_is_empty_not_malformed() {
        let file = written(r#"{"metrics":[]}"#);
        let parsed = read(file.path(), Format::Json, 1 << 20).expect("read");
        assert!(parsed.entries.is_empty());
        assert_eq!(parsed.malformed, 0);
        assert!(parsed.detail.is_some());
    }

    #[test]
    fn an_empty_payload_is_reported_and_fails_nothing() {
        let file = written("");
        let parsed = read(file.path(), Format::Json, 1 << 20).expect("read");
        assert!(parsed.entries.is_empty());
        assert_eq!(parsed.malformed, 0, "empty is not malformed");
        assert!(parsed.detail.is_some());
    }

    #[test]
    fn over_the_size_cap_is_refused_before_it_is_read() {
        let file = written(&"x".repeat(64));
        let error = read(file.path(), Format::Json, 32).expect_err("must refuse");
        assert!(matches!(error, ReadError::TooLarge(64)));
    }

    #[test]
    fn a_non_utf8_payload_is_unreadable_not_fatal() {
        let file = tempfile::NamedTempFile::new().expect("temp");
        std::fs::write(file.path(), [0xff, 0xfe, 0x00, 0x01]).expect("write");
        let error = read(file.path(), Format::Json, 1 << 20).expect_err("must refuse");
        assert!(matches!(error, ReadError::Unreadable(_)));
    }

    #[test]
    fn the_format_follows_the_pinned_filename() {
        assert_eq!(Format::for_name("benchmark.json"), Some(Format::Json));
        assert_eq!(Format::for_name("benchmark.jsonl"), Some(Format::Jsonlines));
        assert_eq!(Format::for_name("bench.json"), None);
    }
}
