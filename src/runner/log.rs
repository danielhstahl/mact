//! Log capture primitives: secret redaction, the bounded in-memory tail, and the
//! byte-budget rule for the on-disk log.
//!
//! Three separate bounds, on purpose, because they answer three different questions
//! (ADR-0004 §7.1):
//!
//! | bound | default | who reads it |
//! |---|---|---|
//! | on-disk `act.jsonl` cap | 64 MiB (`--max-run-log-bytes`) | the disk; exceeding it truncates **with a marker** |
//! | in-memory tail ring | 64 KiB (`--log-tail-ring-bytes`) | `log_tail`, which is capped at the same 65536 the wire allows (ADR-0001 §2.8) |
//! | per-line cap | 1 MiB | memory: a "line" with no newline cannot make act-mcp hold an unbounded buffer |
//!
//! Redaction happens **here**, at the source, before a line reaches either the file or
//! the ring (ADR-0001 §2.7: "redaction happens at the source, not by filtering on the
//! way out"). Two redactors are built per run: the disk one redacts secret values only
//! (`act.jsonl` is the operator's audit file and keeps real paths), the tail one also
//! substitutes the workspace and evidence roots with `<WORKSPACE>` / `<EVIDENCE>`
//! (ADR-0001 §3.4), because the ring feeds a wire field.

use std::borrow::Cow;
use std::collections::{BTreeSet, VecDeque};
use std::path::Path;

/// What a redacted secret value becomes. Public so `.13` (hardening) and the e2e
/// harness (`.15`) can assert on it instead of guessing a spelling.
pub const REDACTION_MARKER: &str = "***REDACTED***";

/// Largest single log line kept before it is split. act's `--json` lines are small;
/// this exists for the pathological case (a job `cat`ting a 200 MiB file into a step),
/// where "one line" would otherwise be the buffer size.
pub const MAX_LOG_LINE_BYTES: usize = 1_048_576;

/// Wire bound on `log_tail` (ADR-0001 §2.8 / `$defs/log_tail_bytes` maximum), which is
/// also the default ring size — the reason `run_status` can answer a tail from memory.
pub const MAX_LOG_TAIL_BYTES: usize = 65_536;

/// A byte-level find-and-replace set, applied to every log line before it is stored.
///
/// Bytes rather than `str` because act copies container output verbatim and container
/// output need not be valid UTF-8; a redactor that failed on invalid UTF-8 would leak
/// the secret it was standing on. Needles are applied longest-first so an overlapping
/// pair cannot leave a partial remnant, and empty needles are dropped (an empty needle
/// would match everywhere and eat the log).
#[derive(Debug, Clone, Default)]
pub struct Redactor {
    pairs: Vec<(Vec<u8>, Vec<u8>)>,
}

impl Redactor {
    /// Redact secret values, and nothing else. This is the one that guards
    /// `act.jsonl`.
    pub fn secrets<'a>(values: impl IntoIterator<Item = &'a str>) -> Self {
        Self::build(values.into_iter().map(|value| {
            (
                value.as_bytes().to_vec(),
                REDACTION_MARKER.as_bytes().to_vec(),
            )
        }))
    }

    /// Secret values **plus** `<WORKSPACE>` / `<EVIDENCE>` substitution — the shape
    /// `log_tail` must be in (ADR-0001 §3.4).
    pub fn secrets_and_paths<'a>(
        values: impl IntoIterator<Item = &'a str>,
        workspace: &Path,
        evidence_dir: &Path,
    ) -> Self {
        let mut pairs: Vec<(Vec<u8>, Vec<u8>)> = vec![
            (absolute(workspace).into_bytes(), b"<WORKSPACE>".to_vec()),
            (absolute(evidence_dir).into_bytes(), b"<EVIDENCE>".to_vec()),
        ];
        pairs.extend(values.into_iter().map(|value| {
            (
                value.as_bytes().to_vec(),
                REDACTION_MARKER.as_bytes().to_vec(),
            )
        }));
        Self::build(pairs)
    }

    fn build(pairs: impl IntoIterator<Item = (Vec<u8>, Vec<u8>)>) -> Self {
        let mut kept: Vec<(Vec<u8>, Vec<u8>)> = pairs
            .into_iter()
            .filter(|(needle, _)| !needle.is_empty())
            .collect();
        // Longest needle first, then by text, so the result does not depend on the
        // order the caller happened to iterate secrets in.
        kept.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
        // A duplicate needle would replace the output of its own first application;
        // keeping the first is enough.
        let mut seen = BTreeSet::new();
        kept.retain(|(needle, _)| seen.insert(needle.clone()));
        Self { pairs: kept }
    }

    /// Whether this redactor would change nothing (the common case: no secrets).
    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }

    /// Apply every pair to `hay`. Returns a borrow when nothing matched, which is the
    /// overwhelmingly common case.
    pub fn apply<'a>(&self, hay: &'a [u8]) -> Cow<'a, [u8]> {
        if self.pairs.iter().all(|(needle, _)| !contains(hay, needle)) {
            return Cow::Borrowed(hay);
        }
        let mut current = hay.to_vec();
        for (needle, replacement) in &self.pairs {
            if contains(&current, needle) {
                current = replace_all(&current, needle, replacement);
            }
        }
        Cow::Owned(current)
    }

    /// The same rule for text that is already valid UTF-8 (a log tail being handed to
    /// the wire).
    pub fn apply_str<'a>(&self, text: &'a str) -> Cow<'a, str> {
        match self.apply(text.as_bytes()) {
            Cow::Borrowed(_) => Cow::Borrowed(text),
            Cow::Owned(bytes) => Cow::Owned(String::from_utf8_lossy(&bytes).into_owned()),
        }
    }
}

fn absolute(path: &Path) -> String {
    path.to_string_lossy().trim_end_matches('/').to_owned()
}

/// Sub-slice membership (a byte `needle`, not a single byte).
fn contains(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > hay.len() {
        return false;
    }
    hay.windows(needle.len()).any(|window| window == needle)
}

/// Non-overlapping replace-all over bytes.
fn replace_all(hay: &[u8], needle: &[u8], with: &[u8]) -> Vec<u8> {
    if needle.is_empty() {
        return hay.to_vec();
    }
    let mut out = Vec::with_capacity(hay.len());
    let mut index = 0;
    while index < hay.len() {
        if hay[index..].starts_with(needle) {
            out.extend_from_slice(with);
            index += needle.len();
        } else {
            out.push(hay[index]);
            index += 1;
        }
    }
    out
}

/// The last `capacity` bytes of a log, kept across both of act's streams.
///
/// Bytes, not lines: line boundaries are the caller's concern (`tail` hands back a
/// byte slice and [`fit_log_tail`] does the line/UTF-8 discipline). A ring, not a
/// `Vec<String>`, because `log_tail_bytes` is a **byte** budget and a line list would
/// let one long line blow past it.
#[derive(Debug)]
pub struct LogRing {
    capacity: usize,
    bytes: std::collections::VecDeque<u8>,
    total: u64,
    /// Whether `bytes[0]` is the first byte of a line. Maintained on eviction, because
    /// `tail_whole` cannot reconstruct it afterwards.
    head_at_line_start: bool,
}

impl LogRing {
    /// A ring holding the last `capacity` bytes. `capacity` of 0 disables the tail
    /// (which is what `log_tail_bytes: 0` means on the wire).
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            // No `with_capacity`: an empty run should not cost 64 KiB × 2 per run
            // before it has written anything.
            bytes: VecDeque::new(),
            total: 0,
            head_at_line_start: true,
        }
    }

    /// The configured byte budget.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Total bytes ever pushed, including what the ring already dropped. This is *not*
    /// the on-disk byte count — the file has its own counter and its own cap.
    pub fn total_bytes(&self) -> u64 {
        self.total
    }

    /// Bytes the ring has thrown away to stay in budget.
    pub fn dropped_bytes(&self) -> u64 {
        self.total.saturating_sub(self.bytes.len() as u64)
    }

    /// Append bytes, evicting from the front when over budget.
    pub fn push(&mut self, chunk: &[u8]) {
        self.total = self.total.saturating_add(chunk.len() as u64);
        if self.capacity == 0 || chunk.is_empty() {
            return;
        }
        if chunk.len() >= self.capacity {
            let start = chunk.len() - self.capacity;
            // The byte just before the new head, from wherever it lives: this chunk, the
            // bytes being evicted, or nowhere (nothing was ever pushed, so the head really
            // does start a line).
            let before_head = if start > 0 {
                chunk[start - 1]
            } else {
                *self.bytes.back().unwrap_or(&b'\n')
            };
            self.bytes.clear();
            self.bytes.extend(chunk[start..].iter());
            self.head_at_line_start = before_head == b'\n';
            return;
        }
        self.bytes.extend(chunk);
        let excess = self.bytes.len().saturating_sub(self.capacity);
        if excess > 0 {
            self.head_at_line_start = self.bytes[excess - 1] == b'\n';
            self.bytes.drain(0..excess);
        }
    }

    /// The last `min(max_bytes, len)` bytes, trimmed so they start at a line boundary.
    ///
    /// The window can start mid-line two ways — the ring evicted half a line, or
    /// `max_bytes` cuts into the middle of one. `drop_partial_first_line` repairs both, but
    /// only when the window really does begin mid-line: cutting exactly at a line boundary
    /// must keep that first line, not throw it away.
    ///
    /// Without this, a `log_tail` could begin with half a JSON record, which is worse than
    /// useless to whatever parses it.
    pub fn tail_whole(&self, max_bytes: usize) -> Vec<u8> {
        let take = max_bytes.min(self.bytes.len());
        if take == 0 {
            return Vec::new();
        }
        let start = self.bytes.len() - take;
        let starts_a_line = match start {
            0 => self.head_at_line_start,
            index => self.bytes[index - 1] == b'\n',
        };
        let window: Vec<u8> = self.bytes.range(start..).copied().collect();
        if starts_a_line {
            return window;
        }
        let trimmed = drop_partial_first_line(&window);
        // A window that is nothing but one cut-off line has no whole line left in it. Give
        // the fragment rather than nothing: a partial line still says the run was alive,
        // and it is the same rule `fit_log_tail` applies to its own fallback.
        if trimmed.is_empty() {
            window
        } else {
            trimmed.to_vec()
        }
    }

    /// The last `min(max_bytes, len)` bytes, oldest first — possibly beginning mid-line.
    pub fn tail(&self, max_bytes: usize) -> Vec<u8> {
        let take = max_bytes.min(self.bytes.len());
        self.bytes.iter().rev().take(take).rev().copied().collect()
    }
}

/// Cut `text` down to at most `max_bytes`, keeping the **end** and whole lines.
///
/// ADR-0001 §2.8: truncation "keeps whole lines and cuts at a UTF-8 boundary", and the
/// tail always holds the end of the log, because the end is where the reason a run failed
/// lives. Steps:
///
/// 1. if it fits, hand back the whole thing;
/// 2. otherwise grow a suffix line by line from the end for as long as it fits, so the
///    result is a whole number of lines and never exceeds the budget;
/// 3. if even the final line is too long for the budget, fall back to the last
///    `max_bytes` slid forward to a UTF-8 boundary — a partial line beats `""`, which is
///    the worst possible answer on a failed run.
pub fn fit_log_tail(text: &str, max_bytes: usize) -> String {
    if max_bytes == 0 || text.is_empty() {
        return String::new();
    }
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let bytes = text.as_bytes();
    // Every position at which a line begins, ascending. A newline at the very end of the
    // text does not begin a line, so it is not a candidate.
    let mut line_starts: Vec<usize> = vec![0];
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' && index + 1 < bytes.len() {
            line_starts.push(index + 1);
        }
    }
    // Ascending start == descending suffix length: the first one that fits is the longest
    // whole-line suffix.
    let best = line_starts
        .iter()
        .find(|start| bytes.len() - **start <= max_bytes)
        .copied();
    if let Some(start) = best {
        return text[start..].to_owned();
    }
    let mut start = bytes.len() - max_bytes;
    while start < bytes.len() && is_utf8_continuation(bytes[start]) {
        start += 1;
    }
    text[start..].to_owned()
}

fn is_utf8_continuation(byte: u8) -> bool {
    byte & 0b1100_0000 == 0b1000_0000
}

/// Drop the leading partial line of a byte slice (the ring can begin mid-line because
/// it is cut on bytes, not lines).
pub fn drop_partial_first_line(bytes: &[u8]) -> &[u8] {
    match bytes.iter().position(|byte| *byte == b'\n') {
        Some(index) => &bytes[index + 1..],
        None => bytes,
    }
}

/// The truncate-with-marker line written once at the end of a capped `act.jsonl`.
///
/// Valid JSON with a key act never emits (`act_mcp_truncated`), so a log parser can
/// skip it knowingly instead of tripping over it — ADR-0002 §1.3 says `act.jsonl` is
/// act's format, so the marker stays out of act's vocabulary rather than pretending to
/// be an act log line.
pub fn truncation_marker(bytes_written: u64, dropped_lines: u64, cap: u64) -> String {
    let value = serde_json::json!({
        "act_mcp_truncated": true,
        "reason": "max_run_log_bytes",
        "bytes_written": bytes_written,
        "dropped_lines": dropped_lines,
        "limit_bytes": cap,
    });
    // `to_string` of a `json!` literal cannot fail.
    format!("{value}\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redactor_replaces_every_occurrence_of_a_secret() {
        let redactor = Redactor::secrets(["hunter2"]);
        let line = b"login with hunter2 then hunter2 again".to_vec();
        assert_eq!(
            redactor.apply(&line).as_ref(),
            format!("login with {REDACTION_MARKER} then {REDACTION_MARKER} again").as_bytes()
        );
    }

    #[test]
    fn redactor_is_a_no_op_without_needles_and_leaves_the_input_borrowed() {
        let redactor = Redactor::secrets([""]);
        assert!(redactor.is_empty(), "an empty secret value is no needle");
        let line = b"untouched".to_vec();
        assert!(matches!(redactor.apply(&line), Cow::Borrowed(_)));
    }

    #[test]
    fn redactor_survives_invalid_utf8_around_a_secret() {
        let redactor = Redactor::secrets(["s3cr3t"]);
        let mut line = b"prefix ".to_vec();
        line.extend_from_slice(&[0xff, 0xfe]);
        line.extend_from_slice(b" s3cr3t tail");
        let out = redactor.apply(&line);
        assert!(!out.windows(6).any(|window| window == b"s3cr3t"));
        assert!(out.windows(2).any(|window| window == [0xff, 0xfe]));
    }

    #[test]
    fn longest_needle_wins_so_overlapping_secrets_leave_no_remnant() {
        let redactor = Redactor::secrets(["abc", "abcdef"]);
        let out = redactor.apply(b"abcdef");
        assert_eq!(out.as_ref(), REDACTION_MARKER.as_bytes());
    }

    #[test]
    fn paths_are_substituted_for_the_wire_shape() {
        let redactor = Redactor::secrets_and_paths(
            ["tok"],
            Path::new("/srv/ws"),
            Path::new("/srv/ws/.evidence"),
        );
        let text = "wrote /srv/ws/.evidence/runs/x/act.jsonl from /srv/ws with tok";
        assert_eq!(
            redactor.apply_str(text),
            "wrote <EVIDENCE>/runs/x/act.jsonl from <WORKSPACE> with ***REDACTED***"
        );
    }

    #[test]
    fn ring_keeps_only_the_last_capacity_bytes_and_counts_everything() {
        let mut ring = LogRing::new(8);
        ring.push(b"1234");
        ring.push(b"56789");
        assert_eq!(ring.tail(64), b"23456789".to_vec());
        assert_eq!(ring.total_bytes(), 9);
        ring.push(b"abcdefghij");
        assert_eq!(ring.tail(64), b"cdefghij".to_vec());
        assert_eq!(ring.capacity(), 8);
    }

    #[test]
    fn zero_capacity_ring_holds_nothing_but_still_counts() {
        let mut ring = LogRing::new(0);
        ring.push(b"noise");
        assert!(ring.tail(64).is_empty());
        assert_eq!(ring.total_bytes(), 5);
    }

    #[test]
    fn a_rings_window_never_starts_mid_line() {
        // Nothing evicted yet: the head is a real line start and must be kept whole.
        let mut ring = LogRing::new(64);
        ring.push(b"first\n");
        assert_eq!(ring.tail_whole(64), b"first\n".to_vec());

        // Eviction that is not a whole number of lines leaves a fragment, and the fragment
        // has to go: this is the case that would put half a JSON record on the wire.
        let mut ring = LogRing::new(11);
        ring.push(b"aaaa\n");
        ring.push(b"bbbbbb\n");
        assert_eq!(ring.tail(64), b"aaa\nbbbbbb\n".to_vec());
        assert_eq!(ring.tail_whole(64), b"bbbbbb\n".to_vec());

        // Eviction on an exact line boundary keeps the line it landed on.
        let mut ring = LogRing::new(5);
        ring.push(b"aaaa\n");
        ring.push(b"bbbb\n");
        assert_eq!(ring.tail(64), b"bbbb\n".to_vec());
        assert_eq!(ring.tail_whole(64), b"bbbb\n".to_vec());

        // A budget that lands exactly on a line boundary keeps that line: `three\n` is a
        // whole line, and a tail that throws away whole lines to be safe would answer a
        // `run_status` request with nothing at all.
        let mut ring = LogRing::new(64);
        ring.push(b"one\ntwo\nthree\n");
        assert_eq!(ring.tail_whole(6), b"three\n".to_vec());
        // A budget that cuts into a line keeps the fragment, because some of a line beats
        // none of it and the caller's `fit_log_tail` does the final whole-line pass.
        assert_eq!(ring.tail_whole(5), b"hree\n".to_vec());
        assert_eq!(ring.tail_whole(0), Vec::<u8>::new());

        // One line bigger than the whole ring: the kept window starts exactly after the
        // newline the eviction cut on, so the head really is a line start.
        let mut ring = LogRing::new(8);
        ring.push(b"ab\n");
        ring.push(b"0123456789\n");
        assert_eq!(ring.tail(64), b"3456789\n".to_vec());
        assert_eq!(ring.tail_whole(64), b"3456789\n".to_vec());
        assert_eq!(ring.tail_whole(6), b"56789\n".to_vec());
    }

    #[test]
    fn fit_keeps_whole_lines_from_the_end() {
        let text = "line one\nline two\nline three\n";
        assert_eq!(fit_log_tail(text, 11), "line three\n");
        assert_eq!(fit_log_tail(text, 20), "line two\nline three\n");
        // A window holding no *complete* line falls back to the byte window: partial,
        // but never the empty string a failed run would otherwise get.
        assert_eq!(fit_log_tail(text, 5), "hree\n");
        assert_eq!(fit_log_tail(text, 0), "");
        assert_eq!(fit_log_tail(text, 1024), text);
    }

    #[test]
    fn fit_cuts_on_a_utf8_boundary_never_mid_character() {
        let text = "aa\néééé\n";
        for cut in 1..text.len() {
            let tail = fit_log_tail(text, cut);
            assert!(
                std::str::from_utf8(tail.as_bytes()).is_ok(),
                "{cut}-byte tail is not valid UTF-8: {tail:?}"
            );
            assert!(
                tail.len() <= cut,
                "{cut}-byte tail came back as {} bytes",
                tail.len()
            );
        }
    }

    #[test]
    fn fit_returns_something_even_when_one_line_exceeds_the_budget() {
        let text = "x".repeat(40) + "\n";
        let tail = fit_log_tail(&text, 8);
        assert_eq!(
            tail.len(),
            8,
            "an empty tail on a failed run is the worst answer"
        );
        assert!(text.ends_with(&tail));
    }

    #[test]
    fn truncation_marker_is_one_json_line_naming_the_cap() {
        let marker = truncation_marker(67108864, 12, 67108864);
        assert!(marker.ends_with('\n'));
        assert_eq!(marker.matches('\n').count(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&marker).expect("one JSON line");
        assert_eq!(parsed["act_mcp_truncated"], true);
        assert_eq!(parsed["dropped_lines"], 12);
        assert_eq!(parsed["limit_bytes"], 67108864u64);
    }

    #[test]
    fn drop_partial_first_line_lands_on_a_line_start() {
        assert_eq!(drop_partial_first_line(b"mid\nline\n"), b"line\n");
        assert_eq!(drop_partial_first_line(b"whole line"), b"whole line");
        assert!(drop_partial_first_line(b"").is_empty());
    }
}
