//! Last-resort coverage scraping out of act's own captured log (mact-f1w.10,
//! ADR-0002 §4.3, §5.1).
//!
//! This is the only source allowed to write `reported_percent`, and it is reached only
//! when no payload file exists at all — a number read out of log text may be a summary, a
//! step's warning, or a percentage from an earlier build, so it is never allowed to
//! overwrite a parsed file's counts (the caller's rule, and the reason this module is
//! asked for only after discovery came up empty).
//!
//! ## What is scanned
//!
//! `act.jsonl` is act's `--json` log: one event object per line. A step's stdout arrives in
//! its `msg` field (on the lines that also carry `raw_output: true`) — the two names this
//! scanner looks up first, `Output` and `Error`, are **not act's**: no release in the supported
//! range emits either (the same finding mact-dfu deleted from `benchmark::act_log`; the
//! `resolved_gaps` entry in `tests/realdata/golden/act-shape.json` has the detail). They stay
//! here because they are not load-bearing in either direction — a JSON line carrying neither
//! name is searched as whole-line text, which is exactly how a real act line reaches this
//! scanner — and taking them out changes what a published `scraped_line` quotes, so that is a
//! change this module makes with its own fixtures and goldens, not as a drive-by.
//!
//! Coverage text is therefore searched per line, split again on newlines because one field may
//! hold a whole table; a line that is not JSON is searched as itself, which is what makes the
//! scraper work on a log written by `--nocolor`-style plain act output too. Redaction already
//! happened at the log's source (mact-f1w.8), so nothing here can launder a secret into evidence.
//!
//! ## The three shapes recognised
//!
//! | log text | `reported_measure` |
//! |---|---|
//! | `Coverage: 87.4%`, `overall coverage 87.4 %` | `lines` |
//! | `Lines: 91.2%`, `Branch coverage: 80%`, `stmts 90%` | the named measure |
//! | `All files | 87.4 | …`, `TOTAL | 87.4` | the table header's column if a header was seen, else `unknown` |
//!
//! A bare `Coverage: 87.4%` is read as **line** coverage: a tool that prints one
//! unqualified percentage prints the one its threshold gate uses, and in every reporter
//! observed (coverage.py's `Coverage: n%`, `nyc --check-coverage`, `jest --coverage`'s
//! global threshold message) that is lines. `expected/coverage-log-scrape.jsonl` pins
//! exactly this record, and ADR-0002 §4.2's `unknown` instruction is honoured for the
//! genuinely ambiguous case, which is a table column nobody labelled.
//!
//! The **last** match in the log wins: a run prints per-file percentages on the way to its
//! summary, and the summary is the last thing it prints.

use std::{
    io::{self, BufRead, BufReader, Read},
    path::Path,
};

use crate::evidence::records::ReportedMeasure;

/// The longest single line of log text a `scraped_line` may quote, so a record cannot be
/// ballooned by one chatty table row.
const MAX_QUOTED_LINE: usize = 240;
/// How far past the word "coverage" (or a metric word) a percentage may sit and still be
/// read as belonging to it — enough for `coverage is 87.4%`, short enough to stop a match
/// jumping to an unrelated number later on the line.
const LOOKAHEAD: usize = 16;

/// What the scrape found.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Scraped {
    /// The percentage, already checked finite and inside `0..=100`.
    pub(crate) percent: f64,
    /// Which measure it refers to — never a guess where the log gave nothing to go on.
    pub(crate) measure: ReportedMeasure,
    /// The log text it came from, verbatim and trimmed, for `extra.scraped_line`.
    pub(crate) line: String,
}

/// What one scan pass produced.
#[derive(Debug, Default)]
pub(crate) struct Scrape {
    /// The winning match, if any line looked like coverage.
    pub(crate) found: Option<Scraped>,
    /// The scan stopped at the byte budget, so the log may hold a later summary.
    pub(crate) truncated: bool,
}

/// Scan act's captured log for a coverage percentage.
///
/// `budget` bounds the bytes read (act's log can be `--max-run-log-bytes`, 64 MiB by
/// default); the file is streamed, so the cost is a line at a time either way, and
/// hitting the budget is reported rather than silently truncating the search.
pub(crate) fn scrape(path: &Path, budget: u64) -> io::Result<Scrape> {
    let file = std::fs::File::open(path)?;
    let mut reader = BufReader::new(file.take(budget));
    let mut scrape = Scrape::default();
    // The column a table's numbers come from, learned from its header row.
    let mut table: Option<(usize, ReportedMeasure)> = None;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        for text in candidate_lines(&line) {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Some(header) = table_header(trimmed) {
                table = Some(header);
            }
            if let Some(found) = match_percent(trimmed, table) {
                scrape.found = Some(found);
            }
        }
    }
    // `read_line` returning 0 at the cap, with bytes left unread, is the truncation case.
    let remaining = std::fs::metadata(path)
        .map(|len| len.len() > budget)
        .unwrap_or(false);
    scrape.truncated = remaining;
    Ok(scrape)
}

/// The texts worth searching in one `act.jsonl` line.
fn candidate_lines(line: &str) -> Vec<String> {
    let trimmed = line.trim_end_matches(['\n', '\r']);
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        let mut texts = Vec::new();
        for key in ["Output", "Error"] {
            if let Some(text) = value.get(key).and_then(|value| value.as_str()) {
                texts.extend(text.split('\n').map(str::to_owned));
            }
        }
        if !texts.is_empty() {
            return texts;
        }
    }
    vec![trimmed.to_owned()]
}

/// Which column of a table a percentage column is, from a header row like
/// `File | % Stmts | % Branch | % Funcs | % Lines | Uncovered Line #s`.
///
/// Returns the 1-based cell index and the measure it names, so the data row is read at
/// the same column rather than at "the first number", which is a different metric in
/// every tool's table.
fn table_header(line: &str) -> Option<(usize, ReportedMeasure)> {
    if !line.contains('|') {
        return None;
    }
    for (index, cell) in line.split('|').enumerate().skip(1) {
        let cell = cell.to_lowercase();
        let measure =
            if cell.contains("stmt") || cell.contains("statements") || cell.contains("funcs") {
                // `Funcs` is grouped with statements only because istanbul's header
                // lists both and the first metric column is what a data row's first
                // number comes from; a row that matches a later column matches it below.
                if cell.contains("func") && !cell.contains("stmt") {
                    ReportedMeasure::Functions
                } else {
                    ReportedMeasure::Statements
                }
            } else if cell.contains("branch") {
                ReportedMeasure::Branches
            } else if cell.contains("line") {
                ReportedMeasure::Lines
            } else {
                continue;
            };
        return Some((index, measure));
    }
    None
}

/// The percentage a table row carries, read from the column the header named (or from the
/// first cell after the label when no header was seen, in which case the measure is
/// honestly `unknown`).
fn table_percent(line: &str, column: Option<(usize, ReportedMeasure)>) -> Option<Scraped> {
    let lower = line.to_lowercase();
    let label = ["all files", "total"]
        .iter()
        .find(|label| lower.starts_with(**label))?;
    let after = &lower[label.len()..];
    if !after.trim_start().starts_with('|') {
        return None;
    }
    let cells: Vec<&str> = line.split('|').collect();
    let wanted = column.map_or(1, |(index, _)| index);
    let cell = cells.get(wanted)?.trim();
    let percent = number_percent(cell)?;
    let measure = column.map_or(ReportedMeasure::Unknown, |(_, measure)| measure);
    Some(Scraped {
        percent,
        measure,
        line: truncate(line),
    })
}

/// A percentage a line states, and which measure it states it for.
fn match_percent(line: &str, table: Option<(usize, ReportedMeasure)>) -> Option<Scraped> {
    if let Some(found) = table_percent(line, table) {
        return Some(found);
    }
    let lower = line.to_lowercase();
    // A named metric beats the unqualified word: `Branch coverage: 80%` is about
    // branches, not lines, even though it says "coverage".
    for (needle, measure) in [
        ("statements", ReportedMeasure::Statements),
        ("stmts", ReportedMeasure::Statements),
        ("branches", ReportedMeasure::Branches),
        ("branch", ReportedMeasure::Branches),
        ("functions", ReportedMeasure::Functions),
        ("funcs", ReportedMeasure::Functions),
        ("lines", ReportedMeasure::Lines),
    ] {
        if let Some(at) = lower.find(needle) {
            if let Some(percent) = percent_after(&lower, at + needle.len()) {
                return Some(Scraped {
                    percent,
                    measure,
                    line: truncate(line),
                });
            }
        }
    }
    if let Some(at) = lower.find("coverage") {
        if let Some(percent) = percent_after(&lower, at + "coverage".len()) {
            return Some(Scraped {
                percent,
                measure: ReportedMeasure::Lines,
                line: truncate(line),
            });
        }
    }
    None
}

/// Read `<number>[%]` starting at `from`, skipping the connector a log writes between the
/// label and the number (`:`, `=`, `is`, `was`, spaces). A character outside that allow-list
/// — a minus sign, a comma, a pipe — ends the search, because "−1%" and "3 files, 2 failed"
/// are not percentages a coverage record may carry.
fn percent_after(hay: &str, from: usize) -> Option<f64> {
    let mut rest = hay.get(from..)?;
    let mut skipped = 0usize;
    loop {
        let (index, ch) = rest.char_indices().next()?;
        if ch.is_ascii_digit() {
            break;
        }
        if !(ch.is_alphabetic() || ch.is_whitespace() || ch == ':' || ch == '=') {
            return None;
        }
        skipped += 1;
        if skipped > LOOKAHEAD {
            return None;
        }
        rest = &rest[index + ch.len_utf8()..];
    }
    // Take the number, then demand the `%`: without the sign a "3 files, 2 failed" line
    // would look like coverage.
    let digits: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    if digits.is_empty() {
        return None;
    }
    let after = &rest[digits.len()..];
    if !after.trim_start().starts_with('%') {
        return None;
    }
    number_percent(&format!("{digits}%"))
}

/// `87.4`, `87.4%`, ` 87.4 % ` → the number, when it is a percentage in range.
fn number_percent(text: &str) -> Option<f64> {
    let text = text.trim().trim_end_matches('%').trim();
    if text.is_empty() {
        return None;
    }
    let percent = text.parse::<f64>().ok()?;
    percent.is_finite().then_some(percent)?;
    (0.0..=100.0).contains(&percent).then_some(percent)
}

fn truncate(text: &str) -> String {
    if text.len() <= MAX_QUOTED_LINE {
        return text.to_owned();
    }
    let mut cut = MAX_QUOTED_LINE;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &text[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn act_log(lines: &[&str]) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().expect("temp log");
        let body: String = lines
            .iter()
            .map(|line| {
                format!("{{\"Action\":\"output\",\"Run\":\"job\",\"Output\":\"{line}\"}}\n")
            })
            .collect();
        std::fs::write(file.path(), body).expect("write log");
        file
    }

    fn scraped(text: &str) -> Option<Scraped> {
        let file = act_log(&[text]);
        scrape(file.path(), 1 << 20)
            .expect("scrape")
            .found
            .map(|found| Scraped {
                percent: found.percent,
                measure: found.measure,
                line: found.line,
            })
    }

    #[test]
    fn a_plain_coverage_percentage_is_line_coverage() {
        let found = scraped("Coverage: 87.4%").expect("matched");
        assert_eq!(found.percent, 87.4);
        assert_eq!(found.measure, ReportedMeasure::Lines);
        assert_eq!(found.line, "Coverage: 87.4%");
    }

    #[test]
    fn a_named_metric_names_the_measure() {
        assert_eq!(
            scraped("Branch coverage: 80%").expect("matched").measure,
            ReportedMeasure::Branches
        );
        assert_eq!(
            scraped("Stmts: 91.5 %").expect("matched").measure,
            ReportedMeasure::Statements
        );
        assert_eq!(
            scraped("functions is 12.0%").expect("matched").measure,
            ReportedMeasure::Functions
        );
    }

    #[test]
    fn an_unlabelled_table_column_is_unknown() {
        let found = scraped("All files | 87.4 | 100.0 | 92.1").expect("matched");
        assert_eq!(found.percent, 87.4);
        assert_eq!(found.measure, ReportedMeasure::Unknown);
    }

    #[test]
    fn a_table_header_says_which_column_the_number_is() {
        let file = act_log(&[
            "File | % Stmts | % Branch | % Funcs | % Lines | Uncovered Line #s",
            "All files | 87.4 | 80.0 | 90.0 | 86.0 |",
        ]);
        let found = scrape(file.path(), 1 << 20)
            .expect("scrape")
            .found
            .expect("matched");
        assert_eq!(found.measure, ReportedMeasure::Statements);
        assert_eq!(found.percent, 87.4);
    }

    #[test]
    fn the_last_percentage_wins() {
        let file = act_log(&["Coverage: 12.5%", "src/a.js 50%", "Coverage: 99.1%"]);
        let found = scrape(file.path(), 1 << 20)
            .expect("scrape")
            .found
            .expect("matched");
        assert_eq!(
            found.percent, 99.1,
            "the summary is the last thing a run prints"
        );
    }

    #[test]
    fn a_percentage_inside_a_multi_line_output_field_is_found() {
        let file = act_log(&["step start\\nCoverage: 42.0%\\nstep done"]);
        assert_eq!(
            scrape(file.path(), 1 << 20)
                .expect("scrape")
                .found
                .unwrap()
                .percent,
            42.0
        );
    }

    #[test]
    fn numbers_that_are_not_percentages_are_ignored() {
        for text in [
            "3 files, 2 failed",
            "elapsed 12 s",
            "Coverage threshold not met",
            "pct 100",
        ] {
            assert!(
                scraped(text).is_none(),
                "{text:?} must not read as coverage"
            );
        }
    }

    #[test]
    fn out_of_range_percentages_are_not_stored() {
        assert!(scraped("Coverage: 104.5%").is_none());
        assert!(scraped("Coverage: -1%").is_none());
    }

    #[test]
    fn a_plain_text_log_is_scanned_too() {
        let file = tempfile::NamedTempFile::new().expect("temp log");
        std::fs::write(file.path(), "Act  0.2.68  | Coverage: 61.0%\n").expect("write");
        let found = scrape(file.path(), 1 << 20)
            .expect("scrape")
            .found
            .expect("matched");
        assert_eq!(found.percent, 61.0);
    }

    #[test]
    fn a_log_beyond_the_budget_says_it_was_truncated() {
        let file = act_log(&["Coverage: 10.0%", &"x".repeat(4096)]);
        let scrape = scrape(file.path(), 16).expect("scrape");
        assert!(scrape.truncated);
    }
}
