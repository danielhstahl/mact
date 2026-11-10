//! Volatile-field normalisation — the one place the e2e suite is allowed to be
//! forgiving, and therefore the place that has to be explicit about it.
//!
//! A `.evidence` tree is written by a real server on a real machine: the run id is
//! the clock plus three random bytes, the commit is whatever `git` just hashed,
//! `host` is this box, the paths live under a fresh temp directory, and the
//! durations and pids are what they are. A golden containing those cannot survive
//! being run twice. A golden that simply *drops* them is a weaker assertion than
//! the contract deserves — which is exactly what the ticket forbids.
//!
//! So the rule: **nothing is deleted; every volatile value is replaced by a named
//! placeholder**, and the substitution list is this file — reviewable, and short.
//! Everything not on the list is compared byte for byte: every coverage count,
//! every benchmark value, every warning, the key order and the record order.
//!
//! | placeholder | replaces | why it varies |
//! |---|---|---|
//! | `{ACT_BINARY}` | the stub `act`'s path (argv[0] of the recorded command line) | where this checkout lives |
//! | `{WORKSPACE}` | the fixture workspace path (as a prefix) | fresh temp dir per test |
//! | `{EVIDENCE}` | the evidence root path (as a prefix) | it lives inside the workspace |
//! | `{SCRATCH}` | the `--run-scratch` root (as a prefix) | fresh temp dir per test |
//! | `{RUN_ID}` | this run's id, and any id of the `YYYYMMDDTHHMMSSZ-xxxxxx` shape | ADR-0002 §1.2: clock + random |
//! | `{COMMIT}` | this run's commit, and any other full 40-lowercase-hex string | the fixture repo's hashes |
//! | `{TS}` | any UTC RFC-3339 instant (`…Z`, optional fraction) | `started_at`, `finished_at`, act's `time` |
//! | `{HOST}` | this machine's hostname (`envelope.host`) | the machine running the test |
//! | `{NUMBER}` | the numeric value of any key in [`NUMERIC_KEYS`] | pid, duration, log size |
//!
//! Path substitution runs first, longest prefix first: `<workspace>/.evidence` is a
//! prefix-extension of `<workspace>`, so replacing the shorter one first would
//! leave a tail hanging off a placeholder.
//!
//! Normalisation is deliberately *not* the identity check on the identity fields:
//! the scenarios also assert that `run_id` and `commit` in the result are the ones
//! the server really produced, so a placeholder cannot hide a wrong value.

use std::path::Path;

/// Keys whose *numeric* value is a fact about this process rather than about the
/// contract. Deliberately short: `exit_code`, `records`, coverage counts and
/// benchmark values are all data, and stay compared.
pub const NUMERIC_KEYS: [&str; 5] = [
    "pid",
    "duration_ms",
    "took_ms",
    "log_bytes",
    "log_stderr_bytes",
];

/// The volatile facts of one scenario, learned from the server rather than guessed.
#[derive(Debug, Clone)]
pub struct Volatile {
    run_id: String,
    commit: String,
    host: String,
    act_binary: String,
    workspace: String,
    evidence: String,
    scratch: String,
}

impl Volatile {
    pub fn new(
        run_id: impl Into<String>,
        commit: impl Into<String>,
        host: impl Into<String>,
        act_binary: &Path,
        workspace: &Path,
        evidence: &Path,
        scratch: &Path,
    ) -> Self {
        let text = |path: &Path| path.display().to_string();
        Self {
            run_id: run_id.into(),
            commit: commit.into(),
            host: host.into(),
            act_binary: text(act_binary),
            workspace: text(workspace),
            evidence: text(evidence),
            scratch: text(scratch),
        }
    }

    /// Normalise one file's content.
    #[must_use]
    pub fn text(&self, raw: &str) -> String {
        let mut text = self.paths(raw);
        text = replace(&text, &self.run_id, "{RUN_ID}");
        text = replace(&text, &self.commit, "{COMMIT}");
        text = replace(&text, &self.host, "{HOST}");
        numbers(&shapes(&text))
    }

    /// The path prefixes, longest first.
    fn paths(&self, raw: &str) -> String {
        let mut text = replace(raw, &self.scratch, "{SCRATCH}");
        text = replace(&text, &self.evidence, "{EVIDENCE}");
        text = replace(&text, &self.workspace, "{WORKSPACE}");
        replace(&text, &self.act_binary, "{ACT_BINARY}")
    }
}

/// Every occurrence of `needle` becomes `holder`. An empty needle replaces nothing
/// (a scenario without an evidence override still has a non-empty workspace path,
/// but the helper must not turn every byte into a holder).
fn replace(input: &str, needle: &str, holder: &str) -> String {
    if needle.is_empty() {
        return input.to_owned();
    }
    input.replace(needle, holder)
}

/// The shape-based substitutions (run ids, instants, commits) in one left-to-right
/// pass, so a longer shape wins over a shorter one starting at the same byte.
fn shapes(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if let Some(len) = run_id_at(bytes, i) {
            out.extend_from_slice(b"{RUN_ID}");
            i += len;
            continue;
        }
        if let Some(len) = instant_at(bytes, i) {
            out.extend_from_slice(b"{TS}");
            i += len;
            continue;
        }
        if let Some(len) = commit_at(bytes, i) {
            out.extend_from_slice(b"{COMMIT}");
            i += len;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| input.to_owned())
}

fn is_digit(byte: u8) -> bool {
    byte.is_ascii_digit()
}

fn is_hex(byte: u8) -> bool {
    byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')
}

/// `YYYYMMDDTHHMMSSZ-xxxxxx` — ADR-0002 §1.2's run id, 23 bytes.
fn run_id_at(bytes: &[u8], at: usize) -> Option<usize> {
    const LEN: usize = 23;
    if at + LEN > bytes.len() || bounded(bytes, at, at + LEN) {
        return None;
    }
    let b = &bytes[at..at + LEN];
    if b[..8].iter().all(|byte| is_digit(*byte))
        && b[8] == b'T'
        && b[9..15].iter().all(|byte| is_digit(*byte))
        && b[15] == b'Z'
        && b[16] == b'-'
        && b[17..].iter().all(|byte| is_hex(*byte))
    {
        Some(LEN)
    } else {
        None
    }
}

/// `YYYY-MM-DDTHH:MM:SS[.fff…]Z` — every instant the contract writes (ADR-0002
/// §2.2: UTC with `Z`, milliseconds when there are any).
fn instant_at(bytes: &[u8], at: usize) -> Option<usize> {
    if at + 20 > bytes.len() {
        // "2026-11-10T00:42:03Z" is the shortest legal instant.
        return None;
    }
    let b = &bytes[at..];
    let digits = |from: usize, to: usize| b[from..to].iter().all(|byte| is_digit(*byte));
    if !(digits(0, 4)
        && b[4] == b'-'
        && digits(5, 7)
        && b[7] == b'-'
        && digits(8, 10)
        && b[10] == b'T'
        && digits(11, 13)
        && b[13] == b':'
        && digits(14, 16)
        && b[16] == b':'
        && digits(17, 19))
    {
        return None;
    }
    let mut i = 19;
    if b[i] == b'.' {
        i += 1;
        let start = i;
        while i < b.len() && i - start < 9 && is_digit(b[i]) {
            i += 1;
        }
        if i == start {
            return None;
        }
    }
    if i < b.len() && b[i] == b'Z' {
        Some(i + 1)
    } else {
        None
    }
}

/// A full lowercase 40-hex commit — and nothing glued to a longer token.
fn commit_at(bytes: &[u8], at: usize) -> Option<usize> {
    const LEN: usize = 40;
    if at + LEN > bytes.len() || bounded(bytes, at, at + LEN) {
        return None;
    }
    bytes[at..at + LEN]
        .iter()
        .all(|byte| is_hex(*byte))
        .then_some(LEN)
}

/// Whether a candidate is glued to more alphanumerics, in which case it is part of a
/// longer token and must be left alone.
fn bounded(bytes: &[u8], start: usize, end: usize) -> bool {
    let before =
        start > 0 && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'-');
    let after = end < bytes.len() && bytes[end].is_ascii_alphanumeric();
    before || after
}

/// `"pid": 31457` → `"pid": {NUMBER}`, once per key in [`NUMERIC_KEYS`].
fn numbers(input: &str) -> String {
    let mut text = input.to_owned();
    for key in NUMERIC_KEYS {
        let marker = format!("\"{key}\":");
        let mut out = String::with_capacity(text.len());
        let mut rest = text.as_str();
        while let Some(at) = rest.find(&marker) {
            out.push_str(&rest[..at + marker.len()]);
            let mut tail = &rest[at + marker.len()..];
            let leading = tail.len() - tail.trim_start().len();
            out.push_str(&tail[..leading]);
            tail = &tail[leading..];
            let digits = tail
                .bytes()
                .take_while(|byte| byte.is_ascii_digit() || *byte == b'-')
                .count();
            if digits > 0 {
                out.push_str("{NUMBER}");
                tail = &tail[digits..];
            }
            rest = tail;
        }
        out.push_str(rest);
        text = out;
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn volatile() -> Volatile {
        Volatile::new(
            "20261110T004203Z-a1b2c3",
            "2f7c1a4b9e0d3c6a5b8f1e2d4c6a8b0f3e5d7c91",
            "build-box-07",
            Path::new("/checkout/tests/e2e/testdata/bin/act"),
            Path::new("/tmp/e2e/workspace"),
            Path::new("/tmp/e2e/workspace/.evidence"),
            Path::new("/tmp/e2e/scratch"),
        )
    }

    #[test]
    fn a_record_normalises_its_identity_its_instant_and_its_host() {
        let line = r#"{"run_id":"20261110T004203Z-a1b2c3","commit":"2f7c1a4b9e0d3c6a5b8f1e2d4c6a8b0f3e5d7c91","started_at":"2026-11-10T01:15:07.004Z","host":"build-box-07","duration_ms":61233,"exit_code":0,"lines_hit":2199}"#;
        assert_eq!(
            volatile().text(line),
            r#"{"run_id":"{RUN_ID}","commit":"{COMMIT}","started_at":"{TS}","host":"{HOST}","duration_ms":{NUMBER},"exit_code":0,"lines_hit":2199}"#
        );
    }

    #[test]
    fn paths_become_placeholders_and_the_longest_prefix_wins() {
        let line = r#""log":"/tmp/e2e/workspace/.evidence/runs/20261110T004203Z-a1b2c3/act.jsonl","secret":"/tmp/e2e/scratch/20261110T004203Z-a1b2c3/secrets.env","source_path":".evidence/incoming/20261110T004203Z-a1b2c3/lcov.info""#;
        assert_eq!(
            volatile().text(line),
            r#""log":"{EVIDENCE}/runs/{RUN_ID}/act.jsonl","secret":"{SCRATCH}/{RUN_ID}/secrets.env","source_path":".evidence/incoming/{RUN_ID}/lcov.info""#
        );
    }

    #[test]
    fn an_unknown_run_id_of_the_same_shape_is_still_normalised() {
        // The concurrency scenarios have two runs and only one of them is `self.run_id`.
        assert_eq!(
            volatile().text("run 20261110T093000Z-ffee00 is busy"),
            "run {RUN_ID} is busy"
        );
    }

    #[test]
    fn a_short_hex_string_is_not_a_commit() {
        assert_eq!(
            volatile().text("\"run_id\":\"20261110T004203Z-a1b2c3\",\"note\":\"deadbee\""),
            "\"run_id\":\"{RUN_ID}\",\"note\":\"deadbee\""
        );
    }

    #[test]
    fn pids_and_durations_become_numbers_but_exit_codes_do_not() {
        assert_eq!(
            volatile().text("\"pid\": 31457, \"took_ms\":1204, \"exit_code\": 3"),
            "\"pid\": {NUMBER}, \"took_ms\":{NUMBER}, \"exit_code\": 3"
        );
    }
}
