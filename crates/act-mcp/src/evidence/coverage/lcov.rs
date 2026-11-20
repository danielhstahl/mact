//! The `lcov.info` tracefile parser (mact-f1w.10, ADR-0002 §4.2).
//!
//! Only the records ADR-0002 promises are acted on: `SF:`, `DA:`, `LF:`, `LH:`,
//! `BRDA:`, `BRF:`, `BRH:` and `end_of_record`. Everything else a real gcov
//! tracefile contains (`TN:`, `FN:`, `FNDA:`, `FNF:`, `FNH:`, `FNL:`, `FNM:`) is
//! skipped on purpose — functions are a benchmark-shaped pair for a language that
//! reports them, and the coverage record's `functions_*` pair stays istanbul-only in v1
//! (ADR-0002 §4.2 "reserved in v1"). Skipping rather than failing is what keeps a
//! toolchain that writes `FN:` lines from losing its coverage.
//!
//! ## Line counting, in the order the format offers it
//!
//! A record may carry both `LF`/`LH` (the summary gcov writes) and the `DA:` lines those
//! numbers summarise. `LF`/`LH` win when both are present, because they are what the
//! tool calls the file's line coverage; `DA:` lines are only counted when the summary is
//! absent, which is the shape `llvm-cov export --format=lcov` writes.
//!
//! `DA:<line>,-` — a hit count of `-` — is gcov for "this line is not executable"
//! (a comment, a closing brace, a `#[derive]`). Those lines count as neither found nor
//! hit; counting them is how a zero-coverage file looks 60% covered.
//!
//! ## Repeated `SF` blocks
//!
//! ADR-0002 §4.2 pins it: repeated `SF` blocks for one path are **summed per path**.
//! That is what a merged `lcov -a a.info -b merged.info` file looks like, and it is what
//! `genhtml` does with them. The first block to name a path fixes where that path appears
//! in the output; later blocks add to it and do not reorder it, so the record order is
//! the order the tracefile introduced each file.

use std::collections::HashMap;

use super::{Pairs, ParsedSource};

/// What one parsed `lcov.info` holds, before paths are normalised.
#[derive(Debug, Clone, Default)]
pub(crate) struct LcovReport {
    /// One entry per source file, in the order the tracefile first named it.
    pub(crate) sources: Vec<ParsedSource>,
    /// Sum over every `SF` block parsed, including blocks whose path is unusable: the
    /// total is what the tracefile measured, file records are only what we can name.
    pub(crate) totals: Pairs,
    /// `SF` blocks that named nothing, or named something with no counts at all.
    pub(crate) malformed_blocks: usize,
    /// Lines that carried a recognised prefix with an unparsable payload.
    pub(crate) malformed_lines: usize,
    /// Whether parsing stopped at the record cap.
    pub(crate) truncated: bool,
}

/// The accumulator for one `SF` block, between its `SF:` line and its `end_of_record`
/// (or the next `SF:`, or EOF: a tracefile whose last record lacks `end_of_record` is
/// still a tracefile).
#[derive(Debug, Default)]
struct Block {
    /// The raw `SF:` value, verbatim until normalisation.
    path: Option<String>,
    /// `DA:` lines: line number → summed hit count. Only lines gcov called executable.
    lines: HashMap<u64, u64>,
    line_found: Option<u64>,
    line_hit: Option<u64>,
    branch_found: Option<u64>,
    branch_hit: Option<u64>,
    /// `BRDA:` tallies, used when no `BRF:`/`BRH:` summary was written.
    brda_found: u64,
    brda_hit: u64,
    saw_branch_data: bool,
}

impl Block {
    /// The counts this block contributes, in the order of preference the module docs
    /// give: the `LF`/`LH` (or `BRF`/`BRH`) summaries, else the `DA:`/`BRDA:` tallies.
    fn pairs(&self) -> Pairs {
        let lines = match (self.line_found, self.line_hit) {
            (Some(found), Some(hit)) => Some((found, hit)),
            _ if !self.lines.is_empty() => Some((
                self.lines.len() as u64,
                self.lines.values().filter(|hits| **hits > 0).count() as u64,
            )),
            _ => None,
        };
        let branches = match (self.branch_found, self.branch_hit) {
            (Some(found), Some(hit)) => Some((found, hit)),
            _ if self.saw_branch_data => Some((self.brda_found, self.brda_hit)),
            _ => None,
        };
        Pairs {
            lines,
            branches,
            ..Pairs::NONE
        }
    }
}

/// Parse `text` — the whole `lcov.info` body — into per-file counts.
///
/// `max_files` is the record cap (ADR-0002 §3.3's spirit for a format that has metrics
/// of the same shape: a cap so a pathological payload cannot balloon one evidence file).
/// Reaching it stops the parse and sets [`LcovReport::truncated`].
pub(crate) fn parse(text: &str, max_files: usize) -> LcovReport {
    let mut report = LcovReport::default();
    // First-seen order plus an index into `sources`, because `IndexMap` is not a
    // dependency of this crate and the contract wants the tracefile's order.
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut block = Block::default();
    let mut open = false;

    for raw in text.lines() {
        let line = raw.trim_matches(|c: char| c == '\r' || c.is_whitespace());
        if line.is_empty() {
            continue;
        }
        // A `SF:` immediately after a `SF:` is legal (gcov writes it when a file appears
        // twice in one run), so the flush happens on both `SF:` and `end_of_record`.
        if let Some(rest) = line.strip_prefix("SF:") {
            if open {
                flush(&mut report, &mut index, &block, max_files);
                if report.truncated {
                    break;
                }
            }
            block = Block::default();
            open = true;
            let path = rest.trim();
            if path.is_empty() {
                report.malformed_lines += 1;
            } else {
                block.path = Some(path.to_owned());
            }
            continue;
        }
        if line == "end_of_record" {
            if open {
                flush(&mut report, &mut index, &block, max_files);
                block = Block::default();
                open = false;
            } else {
                // An `end_of_record` with no open block is noise, not a broken file.
                continue;
            }
            if report.truncated {
                break;
            }
            continue;
        }
        // `TN:` names the test a following group of records belongs to, and gcov writes it
        // before the first `SF:`; the function records name a pair this contract does not
        // carry for lcov (see the module docs). All are skipped wherever they appear.
        if line.starts_with("TN:")
            || line.starts_with("FN:")
            || line.starts_with("FNDA:")
            || line.starts_with("FNF:")
            || line.starts_with("FNH:")
            || line.starts_with("FNL:")
            || line.starts_with("FNM:")
        {
            continue;
        }
        // Everything that is left only means something inside a record; gcov writes nothing
        // else outside one, so a stray line before the first `SF:` is counted and ignored.
        if !open {
            report.malformed_lines += 1;
            continue;
        }
        if let Some(rest) = line.strip_prefix("DA:") {
            match parse_da(rest) {
                // gcov's `-` says "this line is not executable", which is not the same
                // fact as "this line was executed zero times": only the second one is a
                // line the file failed to cover.
                Some(Datum::Counted(line_no, hits)) => {
                    *block.lines.entry(line_no).or_insert(0) += hits;
                }
                Some(Datum::NotExecutable) => {}
                None => report.malformed_lines += 1,
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("LF:") {
            match unsigned(rest) {
                Some(value) => block.line_found = Some(value),
                None => report.malformed_lines += 1,
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("LH:") {
            match unsigned(rest) {
                Some(value) => block.line_hit = Some(value),
                None => report.malformed_lines += 1,
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("BRF:") {
            match unsigned(rest) {
                Some(value) => block.branch_found = Some(value),
                None => report.malformed_lines += 1,
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("BRH:") {
            match unsigned(rest) {
                Some(value) => block.branch_hit = Some(value),
                None => report.malformed_lines += 1,
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("BRDA:") {
            block.saw_branch_data = true;
            match parse_brda(rest) {
                Brda::Taken(taken) => {
                    block.brda_found += 1;
                    if taken {
                        block.brda_hit += 1;
                    }
                }
                Brda::Unreachable => {}
                Brda::Bad => report.malformed_lines += 1,
            }
            continue;
        }
        // Nothing matched, so the line carries a prefix this parser has never heard of. One
        // such line is a tool we have not met; a file made of them is not an lcov file, and
        // the count of them is what says so (`lcov-malformed.info` is that case).
        report.malformed_lines += 1;
    }
    if open && !report.truncated {
        flush(&mut report, &mut index, &block, max_files);
    }
    report
}

/// Fold one finished block into the report: summed per path, first-seen order kept.
fn flush(report: &mut LcovReport, index: &mut HashMap<String, usize>, block: &Block, max: usize) {
    let Some(raw) = block.path.as_ref().filter(|path| !path.is_empty()) else {
        report.malformed_blocks += 1;
        return;
    };
    let pairs = block.pairs();
    if pairs.is_empty() {
        // An `SF` block with no line and no branch counts says nothing measurable.
        report.malformed_blocks += 1;
        return;
    }
    match index.get(raw) {
        Some(at) => report.sources[*at].pairs.add(&pairs),
        None => {
            if report.sources.len() >= max {
                report.truncated = true;
                return;
            }
            index.insert(raw.clone(), report.sources.len());
            report.sources.push(ParsedSource {
                raw_path: raw.clone(),
                pairs,
            });
        }
    }
    // The total covers every block, including one whose path repeats: that is the sum
    // ADR-0002 §4.2 asks for, and it is computed here so a dropped path still counts.
    report.totals.add(&pairs);
}

/// `DA:<line>,<hits>[,<branch condition>]`, where `<hits>` may be `-`.
enum Datum {
    /// The line is executable and was hit `u64` times (0 included).
    Counted(u64, u64),
    /// `DA:<line>,-`: gcov's "not executable" marker, and the shape `llvm-cov` writes for
    /// a line it instrumented but never gave a count.
    NotExecutable,
}

fn parse_da(rest: &str) -> Option<Datum> {
    let mut fields = rest.split(',');
    let line_no = unsigned(fields.next()?)?;
    let hits = match fields.next()?.trim() {
        "-" => return Some(Datum::NotExecutable),
        "" | "0" => 0,
        other => unsigned(other)?,
    };
    Some(Datum::Counted(line_no, hits))
}

/// What a `BRDA:` row says.
enum Brda {
    /// A reachable branch, taken or not.
    Taken(bool),
    /// The `-` in the count position: a branch expression that was never reachable, which
    /// lcov leaves out of its branch totals entirely.
    Unreachable,
    /// Not a branch record at all.
    Bad,
}

/// `BRDA:<block>,<branch>,<branch expression>,<times taken>` — the fourth field is the
/// count, and the `-` that marks an unreachable branch sits in that position. Writers that
/// emit the older three-field form put the count third, so the count is "the last field".
fn parse_brda(rest: &str) -> Brda {
    if rest.split(',').count() < 3 {
        return Brda::Bad;
    }
    let count = rest.split(',').next_back().unwrap_or("").trim();
    if count == "-" {
        return Brda::Unreachable;
    }
    unsigned(count).map_or(Brda::Bad, |hits| Brda::Taken(hits > 0))
}

/// A non-negative decimal integer, or `None` for anything else (including a negative,
/// which gcov never writes and which must not silently become its absolute value).
fn unsigned(text: &str) -> Option<u64> {
    text.trim().parse::<u64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four-file tracefile the golden is built from, inlined so the parser's own
    /// edge cases stay readable next to the parser.
    const TRACE: &str =
        "SF:src/parser/tokenizer.rs\nLF:2617\nLH:2199\nBRF:896\nBRH:708\nend_of_record\n";

    fn one(text: &str) -> Pairs {
        let report = parse(text, 5000);
        assert_eq!(report.sources.len(), 1, "fixture names one file");
        report.sources[0].pairs
    }

    #[test]
    fn summary_lines_win_over_da_lines() {
        let pairs = one("SF:a.rs\nLF:10\nLH:4\nDA:1,1\nDA:2,1\nDA:3,-\nend_of_record\n");
        assert_eq!(pairs.lines, Some((10, 4)));
    }

    #[test]
    fn da_only_records_count_executable_lines() {
        // Three executable lines, one of them hit; the `-` line is not executable and
        // so appears in neither number.
        let pairs = one("SF:a.rs\nDA:1,0\nDA:2,3\nDA:7,-\nDA:9,0\nend_of_record\n");
        assert_eq!(pairs.lines, Some((3, 1)));
    }

    #[test]
    fn repeated_da_lines_for_one_line_are_summed() {
        let pairs = one("SF:a.rs\nDA:1,0\nDA:1,2\nDA:2,0\nend_of_record\n");
        assert_eq!(pairs.lines, Some((2, 1)));
    }

    #[test]
    fn brda_tallies_are_used_without_a_branch_summary() {
        // The real gcov shape: `BRDA:<block>,<branch>,<expression>,<times taken>`, where a
        // never-reachable branch expression carries `-` in the count position.
        let pairs =
            one("SF:a.rs\nLF:1\nLH:1\nBRDA:0,0,0,5\nBRDA:0,0,1,0\nBRDA:1,0,0,-\nend_of_record\n");
        // The `-` row is not a branch the file failed to cover: it is not counted at all.
        assert_eq!(pairs.branches, Some((2, 1)));
    }

    #[test]
    fn branch_summary_wins_over_brda_tallies() {
        let pairs = one("SF:a.rs\nLF:1\nLH:1\nBRF:9\nBRH:2\nBRDA:0,0,1,1\nend_of_record\n");
        assert_eq!(pairs.branches, Some((9, 2)));
    }

    #[test]
    fn zero_coverage_and_branchless_records_survive() {
        let report = parse(
            "SF:zero.rs\nLF:88\nLH:0\nBRF:24\nBRH:0\nend_of_record\nSF:gen.rs\nLF:4\nLH:4\nend_of_record\n",
            5000,
        );
        assert_eq!(report.sources[0].pairs.lines, Some((88, 0)));
        assert_eq!(report.sources[0].pairs.branches, Some((24, 0)));
        assert_eq!(
            report.sources[1].pairs.branches, None,
            "no branch data at all"
        );
    }

    #[test]
    fn repeated_sf_blocks_are_summed_per_path_and_keep_first_order() {
        let report = parse(
            "SF:a.rs\nLF:10\nLH:5\nend_of_record\nSF:b.rs\nLF:1\nLH:1\nend_of_record\nSF:a.rs\nLF:6\nLH:1\nend_of_record\n",
            5000,
        );
        assert_eq!(report.sources.len(), 2);
        assert_eq!(report.sources[0].raw_path, "a.rs");
        assert_eq!(report.sources[0].pairs.lines, Some((16, 6)));
        assert_eq!(report.totals.lines, Some((17, 7)));
    }

    #[test]
    fn crlf_and_a_missing_final_newline_are_both_fine() {
        let unix = TRACE.replace('\n', "\r\n");
        assert_eq!(
            parse(&unix, 5000).sources[0].pairs.lines,
            Some((2617, 2199))
        );
        let torn = TRACE.trim_end_matches('\n');
        assert_eq!(
            parse(torn, 5000).sources[0].pairs.lines,
            Some((2617, 2199)),
            "a tracefile whose last line has no LF still ends its record at EOF"
        );
    }

    #[test]
    fn record_without_end_of_record_is_still_read() {
        let report = parse("SF:a.rs\nLF:3\nLH:1\n", 5000);
        assert_eq!(report.sources[0].pairs.lines, Some((3, 1)));
    }

    #[test]
    fn function_records_are_skipped_not_rejected() {
        let report = parse(
            "TN:unit\nSF:a.rs\nFNF:2\nFNH:1\nFN:1:f\nFNDA:3:f\nLF:1\nLH:1\nend_of_record\n",
            5000,
        );
        assert_eq!(report.malformed_lines, 0);
        assert_eq!(report.sources[0].pairs.lines, Some((1, 1)));
    }

    #[test]
    fn unparsable_payloads_are_counted_and_the_run_keeps_its_evidence() {
        let report = parse(
            "SF:a.rs\nLF:abc\nLH:-3\nDA:x,1\nZZZ:1\nend_of_record\n",
            5000,
        );
        assert_eq!(report.malformed_lines, 4);
        assert!(report.sources.is_empty(), "the record has no usable counts");
        assert_eq!(report.malformed_blocks, 1);
    }

    #[test]
    fn the_record_cap_stops_the_parse_and_says_so() {
        let mut text = String::new();
        for n in 0..12 {
            text.push_str(&format!("SF:src/f{n}.rs\nLF:1\nLH:1\nend_of_record\n"));
        }
        let report = parse(&text, 10);
        assert!(report.truncated);
        assert_eq!(report.sources.len(), 10);
    }
}
