//! The column model and the spelling of one row: values in, display strings out.
//!
//! This is the only place in the crate that turns a measurement into text, and it is deliberately
//! glyph-driven and stateless, because three surfaces print the same table (the TUI, `--plain`, and —
//! from mact-by1.13 — `fathom report`) and they must not invent three dialects:
//!
//! ```text
//! SHA      AGE  MESSAGE                     COV      +/-         MI     BENCH        ACTION
//! 9f31a2c  2h   registry: sweep evicted…    87.4%    +412 -98    82.1   ~job_dur     ✓
//! 71cd0ae  5h   docs: evidence recipes      —        +61 -3      82.1   —            ✗
//! 3b8e9f1  1d   coverage: drop node_modules 91.2%▲   +140 -310   79.8▼  412.5ms +6*  ✓
//! ```
//!
//! The rules it implements are ADR-0008 §6.1 (three states per number, one `—` glyph, arrows only
//! with both sides, a `*` only with caveats behind it, ascii for `--plain`) and ADR-0008 §4.5 (the one
//! benchmark group). It reads the glyph table out of `schema/metric-model.json` through
//! [`crate::metrics::Model`] rather than typing the characters in twice, so the table the ADR checker
//! pins and the screen that prints it cannot drift apart.

use crate::metrics::model::Glyphs;
use crate::metrics::vocab::{ActionState, Column as MetricColumn};
use crate::metrics::Model;

use super::entry::{Bench, Churn, Delta, Entry, Metric};

/// Which of the eight columns, in the order they are drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnKind {
    /// The 7-hex SHA.
    Sha,
    /// How long ago the commit was authored.
    Age,
    /// The message head.
    Message,
    /// The pooled coverage percentage.
    Coverage,
    /// The `+`/`−` churn.
    Churn,
    /// The per-commit maintainability aggregate.
    Mi,
    /// The one benchmark group (§4.5).
    Bench,
    /// The action state glyph.
    Action,
}

/// Where a column's text sits inside its width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    /// Padded on the right.
    Left,
    /// Padded on the left.
    Right,
}

/// One column's layout facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Column {
    /// Which column.
    pub kind: ColumnKind,
    /// The header text, as the mock draws it (`cov`, `+/-`, `MI`, …).
    pub header: &'static str,
    /// Alignment inside the column.
    pub align: Align,
    /// The width it is given when the terminal can afford it — the width of the widest cell *and* of
    /// the header, because a header clipped to its column is a header that lies about its column. Fixed
    /// rather than fitted-to-content on
    /// purpose: a table whose columns change width as you scroll is a table you cannot read, and the
    /// cost is that an outlier cell is truncated instead of squeezing its neighbours.
    pub width: usize,
    /// Drop order under pressure: the *lowest* number is dropped first. `Sha` and `Message` are out of
    /// the running because a timeline without a SHA or a message is not a timeline.
    pub drop_first: u8,
}

/// The eight columns, left to right, with the budget each one claims.
///
/// The widths are what the cells actually spell at: coverage needs `100.0%▲*` (8), churn needs
/// `+12345 -9876` (12), and `bench` is the widest — which is also why it is the first thing dropped
/// when the terminal is narrow: it is the only column whose content is a *choice* (§4.5), so losing it
/// loses the least information per column.
pub const COLUMNS: [Column; 8] = [
    Column {
        kind: ColumnKind::Sha,
        header: "SHA",
        align: Align::Left,
        width: 7,
        drop_first: u8::MAX,
    },
    Column {
        kind: ColumnKind::Age,
        header: "age",
        align: Align::Right,
        width: 5,
        drop_first: 40,
    },
    Column {
        kind: ColumnKind::Message,
        header: "message",
        align: Align::Left,
        width: 0,
        drop_first: u8::MAX - 1,
    },
    Column {
        kind: ColumnKind::Coverage,
        header: "cov",
        align: Align::Right,
        width: 9,
        drop_first: 50,
    },
    Column {
        kind: ColumnKind::Churn,
        header: "+/-",
        align: Align::Right,
        width: 12,
        drop_first: 30,
    },
    Column {
        kind: ColumnKind::Mi,
        header: "MI",
        align: Align::Right,
        width: 8,
        drop_first: 20,
    },
    Column {
        kind: ColumnKind::Bench,
        header: "bench",
        align: Align::Right,
        width: 16,
        drop_first: 10,
    },
    Column {
        kind: ColumnKind::Action,
        header: "action",
        align: Align::Right,
        width: 6,
        drop_first: 60,
    },
];

/// The narrower a message column may be squeezed before columns start being dropped instead.
pub const MESSAGE_MIN: usize = 8;

/// One column that survived the width budget: which one, how wide, and where it starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    /// Which column.
    pub kind: ColumnKind,
    /// How its text is aligned inside it — copied from the column, so a caller that only has the slot
    /// still pads the cell the way the column asked.
    pub align: Align,
    /// Where its text starts, in cells from the left edge of the drawing area.
    pub x: usize,
    /// How many cells it may use.
    pub width: usize,
}

/// Fit the eight columns into `width` cells, or say that they cannot fit.
///
/// The message column absorbs every spare cell — it is the only column whose content a reader is
/// actually scanning — and when there are not enough cells for even a short message, whole columns are
/// dropped in [`Column::drop_first`] order rather than letting the table clip mid-cell. Deterministic
/// and content-independent: the same width always gives the same table, so scrolling does not shuffle
/// the columns and a snapshot test can pin one narrow terminal exactly.
#[must_use]
pub fn layout(width: usize) -> Option<Vec<Slot>> {
    let mut dropped: Vec<ColumnKind> = Vec::new();
    loop {
        let fixed: Vec<&Column> = COLUMNS
            .iter()
            .filter(|column| column.kind != ColumnKind::Message && !dropped.contains(&column.kind))
            .collect();
        // One space between every pair of slots: `fixed.len()` fixed columns plus the message column
        // make `fixed.len() + 1` slots and therefore `fixed.len()` gaps.
        let used: usize = fixed.iter().map(|column| column.width).sum::<usize>() + fixed.len();
        // The columns that may be dropped at all: everything except the two that define a row.
        let droppable = |columns: &[&Column]| {
            columns
                .iter()
                .copied()
                .filter(|column| column.drop_first < u8::MAX)
                .min_by_key(|column| column.drop_first)
                .map(|column| column.kind)
        };
        let Some(message) = width.checked_sub(used) else {
            if let Some(kind) = droppable(&fixed) {
                dropped.push(kind);
                continue;
            }
            return None;
        };
        if message >= MESSAGE_MIN {
            let mut slots = Vec::with_capacity(fixed.len() + 1);
            let mut x = 0;
            for column in COLUMNS.iter() {
                if column.kind == ColumnKind::Message {
                    slots.push(Slot {
                        kind: column.kind,
                        align: column.align,
                        x,
                        width: message,
                    });
                    x += message + 1;
                    continue;
                }
                if dropped.contains(&column.kind) {
                    continue;
                }
                slots.push(Slot {
                    kind: column.kind,
                    align: column.align,
                    x,
                    width: column.width,
                });
                x += column.width + 1;
            }
            return Some(slots);
        }
        // The message would be squeezed too thin: spend a column's worth of cells instead, cheapest
        // first, and try again.
        dropped.push(droppable(&fixed)?);
    }
}

/// The eight display strings for one row, in [`COLUMNS`] order.
///
/// `now` is the wall clock the age column counts back from, passed in rather than read here so a test
/// can pin an instant and so a frame calls the clock once rather than once per row.
#[must_use]
pub fn cells(entry: &Entry, glyphs: &Glyphs, now: i64) -> [String; COLUMNS.len()] {
    [
        entry.short(),
        super::entry::age(now.saturating_sub(entry.author_utc)),
        entry.subject.clone(),
        metric_cell(&entry.coverage, "%", glyphs),
        churn_cell(&entry.churn, glyphs),
        metric_cell(&entry.mi, "", glyphs),
        bench_cell(&entry.bench, glyphs),
        action_cell(entry.action, glyphs),
    ]
}

/// The column headers, spelled the way the mock spells them (lower-case words, `MI` apart).
#[must_use]
pub fn header_cells() -> [&'static str; COLUMNS.len()] {
    COLUMNS.map(|column| column.header)
}

/// `87.4%`, `91.2%▲`, `0.0%` for a measured zero, `—` for unknown, `—*` when the gap behind the `—`
/// is itself caveated (ADR-0008 §6.1's `*` row: on an unknown column it speaks about the gap).
fn metric_cell(metric: &Metric, suffix: &str, glyphs: &Glyphs) -> String {
    let Some(value) = metric.value else {
        let mut text = glyphs.unknown.clone();
        if metric.caveat {
            text.push_str(&glyphs.caveat_marker);
        }
        return text;
    };
    // The number at its stored precision — one decimal, because coverage and the MI aggregate are both
    // stored at exactly one (`percent_decimals: 1`, ADR-0007 §4.3's `round_storage`). More digits on
    // screen would be digits the derivation never produced; fewer would be a second rounding.
    let mut text = format!("{value:.1}");
    if !suffix.is_empty() {
        text.push_str(suffix);
    }
    if let Some(arrow) = delta_glyph(metric.delta, glyphs) {
        text.push_str(&arrow);
    }
    if metric.caveat {
        text.push_str(&glyphs.caveat_marker);
    }
    text
}

/// The arrow glyph, if one is owed. `None` delta prints nothing at all: an arrow requires both sides
/// (ADR-0008 §6.1), and `·` is reserved for a real tie at the stored precision.
fn delta_glyph(delta: Option<Delta>, glyphs: &Glyphs) -> Option<String> {
    delta.map(|delta| match delta {
        Delta::Up => glyphs.delta_up.clone(),
        Delta::Down => glyphs.delta_down.clone(),
        Delta::Flat => glyphs.delta_flat.clone(),
    })
}

fn churn_cell(churn: &Churn, glyphs: &Glyphs) -> String {
    if churn.is_unknown() {
        return glyphs.unknown.clone();
    }
    // Totals, noise included and labelled: ADR-0007 §7.1 makes the `+`/`−` column and the MI column
    // answer to one classifier, and ADR-0006 §7.2 makes noise counted-but-shown rather than hidden.
    // The split itself is the detail view's (mact-by1.14); the cell stays `+412 -98`.
    let add = churn.add.unwrap_or(0);
    let del = churn.del.unwrap_or(0);
    format!("+{add} -{del}")
}

/// `412.5ms +6*`, `~job_dur_s`, `—` — ADR-0008 §4.5's cell in the order the ADR gives it: context
/// prefix, value, `+N` for the rest, `*` for the column's caveats.
fn bench_cell(bench: &Bench, glyphs: &Glyphs) -> String {
    if !bench.is_known() {
        let mut text = glyphs.unknown.clone();
        if bench.caveat {
            text.push_str(&glyphs.caveat_marker);
        }
        return text;
    }
    let mut text = String::new();
    if bench.context {
        text.push_str(&glyphs.context);
    }
    text.push_str(&bench.value);
    if let Some(arrow) = delta_glyph(bench.delta, glyphs) {
        text.push_str(&arrow);
    }
    if bench.rest > 0 {
        text.push_str(&format!(" +{}", bench.rest));
    }
    if bench.caveat {
        text.push_str(&glyphs.caveat_marker);
    }
    text
}

/// The action column: one of four glyphs, and never a colour-only signal (ADR-0008 §5.3, §6.1).
#[must_use]
pub fn action_cell(state: ActionState, glyphs: &Glyphs) -> String {
    match state {
        ActionState::Success => glyphs.success.clone(),
        ActionState::Failed => glyphs.failed.clone(),
        ActionState::Mixed => glyphs.mixed.clone(),
        ActionState::NoEvidence => glyphs.no_evidence.clone(),
    }
}

/// Which caveat codes on this row qualify which column, read out of the model.
///
/// Per-column membership lives in `schema/metric-model.json` (`caveat_codes.coverage` /
/// `.benchmarks`), which is ADR-0008 §6.1's `column` column; re-typing the twelve in Rust would be a
/// second table free to drift.
#[must_use]
pub fn caveats_for<'c>(
    model: &'c Model,
    column: MetricColumn,
    present: &'c [String],
) -> Vec<&'c str> {
    let declared: &[String] = match column {
        MetricColumn::Coverage => &model.caveat_codes.coverage,
        MetricColumn::Benchmarks => &model.caveat_codes.benchmarks,
    };
    present
        .iter()
        .filter(|code| declared.contains(code))
        .map(String::as_str)
        .collect()
}

/// The line under the table that makes a `*` explainable, and that ADR-0007 §4.4 *requires* whenever
/// `unanalysed_paths > 0`.
///
/// Empty output means there is genuinely nothing to say about this row, which is why every marker on
/// the table has a clause in this line: a `*` with nothing behind it is the bug §6.1 names.
#[must_use]
pub fn caveat_line(entry: &Entry, model: &Model) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(gap) = entry.coverage.gap.as_deref() {
        parts.push(format!("cov unknown {gap}"));
    }
    if let Some(gap) = entry.bench.gap.as_deref() {
        parts.push(format!("bench unknown {gap}"));
    }
    if let Some(gap) = entry.mi.gap.as_deref() {
        parts.push(format!("MI unknown {gap}"));
    }
    let coverage = caveats_for(model, MetricColumn::Coverage, &entry.caveats);
    if !coverage.is_empty() {
        parts.push(format!("cov* {}", coverage.join(",")));
    }
    let benchmarks = caveats_for(model, MetricColumn::Benchmarks, &entry.caveats);
    if !benchmarks.is_empty() {
        parts.push(format!("bench* {}", benchmarks.join(",")));
    }
    if entry.unanalysed_paths > 0 {
        let noun = if entry.unanalysed_paths == 1 {
            "path"
        } else {
            "paths"
        };
        parts.push(format!("{} {noun} not analysed", entry.unanalysed_paths));
    }
    if entry.churn.truncated {
        parts.push("file list truncated".to_owned());
    }
    parts.join("  ·  ")
}

#[cfg(test)]
mod tests {
    use crate::metrics::vocab::ActionState;
    use crate::metrics::Model;
    use crate::tui::cells::{caveat_line, cells, header_cells, layout, ColumnKind, COLUMNS};
    use crate::tui::entry::{Bench, Churn, Delta, Entry, Metric};

    fn model() -> Model {
        Model::embedded()
    }

    fn unicode() -> crate::metrics::model::Glyphs {
        model().display.unicode
    }

    fn ascii() -> crate::metrics::model::Glyphs {
        model().display.ascii
    }

    fn entry() -> Entry {
        Entry {
            sha: "9f31a2c0e1a2b3c4d5e6f708192a3b4c5d6e7f80".to_owned(),
            parent: Some("71cd0ae0e1a2b3c4d5e6f708192a3b4c5d6e7f80".to_owned()),
            author_utc: 1_767_225_600,
            committer_utc: 1_767_225_600,
            subject: "registry: sweep evicted runs".to_owned(),
            author: "Loop RS".to_owned(),
            coverage: Metric {
                value: Some(87.4),
                delta: Some(Delta::Up),
                caveat: false,
                gap: None,
            },
            churn: Churn {
                add: Some(412),
                del: Some(98),
                files: Some(9),
                noise_add: 0,
                noise_del: 0,
                truncated: false,
            },
            mi: Metric {
                value: Some(82.1),
                delta: None,
                caveat: false,
                gap: None,
            },
            unanalysed_paths: 0,
            detail: crate::tui::bench::Commit::default(),
            bench: Bench {
                name: "tokenizer_throughput".to_owned(),
                labels: "{}".to_owned(),
                value: "412.5ms".to_owned(),
                comparable: true,
                context: false,
                rest: 6,
                caveat: true,
                delta: None,
                gap: None,
                raw: Some(412.5),
            },
            action: ActionState::Success,
            indexed: true,
            caveats: vec!["unit_missing".to_owned()],
        }
    }

    #[test]
    fn a_populated_row_prints_all_eight_columns() {
        let row = cells(&entry(), &unicode(), 1_767_225_600 + 2 * 3_600);
        assert_eq!(row[0], "9f31a2c");
        assert_eq!(row[1], "2h");
        assert_eq!(row[2], "registry: sweep evicted runs");
        assert_eq!(
            row[3], "87.4%▲",
            "the arrow belongs to the number it describes"
        );
        assert_eq!(row[4], "+412 -98");
        assert_eq!(row[5], "82.1");
        assert_eq!(row[6], "412.5ms +6*", "ADR-0008 §4.5's order: value, +N, *");
        assert_eq!(row[7], "✓");
    }

    #[test]
    fn the_ascii_column_is_what_a_dumb_terminal_gets() {
        let row = cells(&entry(), &ascii(), 1_767_225_600 + 2 * 3_600);
        assert_eq!(row[3], "87.4%^");
        assert_eq!(row[7], "pass");
        let mut failed = entry();
        failed.action = ActionState::Failed;
        assert_eq!(cells(&failed, &ascii(), 1_767_225_600)[7], "fail");
        let mut mixed = entry();
        mixed.action = ActionState::Mixed;
        assert_eq!(cells(&mixed, &ascii(), 1_767_225_600)[7], "mix");
    }

    #[test]
    fn unknown_is_one_glyph_in_every_column_and_zero_is_never_it() {
        let mut row = entry();
        row.coverage = Metric::missing("no_evidence");
        row.mi = Metric::UNKNOWN;
        row.churn = Churn::UNKNOWN;
        row.bench = Bench::missing(Some("config_filtered"));
        row.action = ActionState::NoEvidence;
        let texts = cells(&row, &unicode(), 1_767_225_600);
        let unknown = unicode().unknown;
        assert_eq!(texts[3], unknown);
        assert_eq!(texts[4], unknown);
        assert_eq!(texts[5], unknown);
        assert_eq!(texts[6], unknown);
        assert_eq!(texts[7], unknown);

        // A measured zero is not an unknown, and prints as zero.
        let mut zero = entry();
        zero.coverage = Metric::measured(0.0);
        zero.churn = Churn {
            add: Some(0),
            del: Some(0),
            files: Some(0),
            noise_add: 0,
            noise_del: 0,
            truncated: false,
        };
        let texts = cells(&zero, &unicode(), 1_767_225_600);
        assert_eq!(texts[3], "0.0%");
        assert_eq!(texts[4], "+0 -0");
    }

    #[test]
    fn no_parent_means_no_arrow_and_a_tie_is_a_dot_not_a_blank() {
        let mut row = entry();
        row.coverage.delta = None;
        assert_eq!(cells(&row, &unicode(), 0)[3], "87.4%");
        row.coverage.delta = Some(Delta::Flat);
        assert_eq!(cells(&row, &unicode(), 0)[3], "87.4%·");
    }

    #[test]
    fn a_marker_always_has_a_code_behind_it() {
        let line = caveat_line(&entry(), &model());
        assert!(
            line.contains("bench* unit_missing"),
            "the bench `*` must be explainable: {line}"
        );
        let quiet = Entry {
            caveats: Vec::new(),
            unanalysed_paths: 0,
            ..entry()
        };
        let mut quiet = quiet;
        quiet.bench.caveat = false;
        assert!(caveat_line(&quiet, &model()).is_empty());
    }

    #[test]
    fn unanalysed_paths_get_their_mandatory_line() {
        // ADR-0007 §4.4: the view MUST render the caveat line when this counter is non-zero.
        let mut row = entry();
        row.unanalysed_paths = 180;
        assert!(caveat_line(&row, &model()).contains("180 paths not analysed"));
        row.unanalysed_paths = 1;
        assert!(caveat_line(&row, &model()).contains("1 path not analysed"));
    }

    #[test]
    fn headers_are_the_eight_the_mock_draws() {
        assert_eq!(
            header_cells(),
            ["SHA", "age", "message", "cov", "+/-", "MI", "bench", "action"]
        );
    }

    #[test]
    fn a_wide_terminal_gets_all_eight_columns() {
        let slots = layout(96).expect("a 96-cell terminal fits the table");
        assert_eq!(slots.len(), 8);
        assert_eq!(slots.last().unwrap().kind, ColumnKind::Action);
        let message = slots
            .iter()
            .find(|slot| slot.kind == ColumnKind::Message)
            .expect("message");
        assert_eq!(message.width, 26, "the message absorbs every spare cell");
        // Slots are contiguous and separated by exactly one cell, or the header stops lining up with
        // the cells it labels.
        let mut expected_x = 0;
        for slot in &slots {
            assert_eq!(slot.x, expected_x);
            expected_x += slot.width + 1;
        }
    }

    #[test]
    fn a_narrow_terminal_drops_columns_instead_of_clipping_rows() {
        // Drop order is bench, MI, churn, age, cov, action: the widest and most derivative first, then
        // the convenience column, then the metric columns, and the one-glyph column last because it is
        // the cheapest thing on the row. SHA and message are never dropped — a timeline without a SHA
        // or a message is not a timeline.
        // 78 is the narrowest terminal that gets all eight: 70 cells of fixed columns and gaps, plus
        // MESSAGE_MIN. Each rung below drops exactly one column and is the narrowest width that keeps
        // the one above it, so the ladder below is the drop order read out backwards.
        assert_eq!(
            column_kinds(78),
            ["SHA", "age", "message", "cov", "+/-", "MI", "bench", "action"]
        );
        assert_eq!(
            column_kinds(77),
            ["SHA", "age", "message", "cov", "+/-", "MI", "action"]
        );
        assert_eq!(
            column_kinds(60),
            ["SHA", "age", "message", "cov", "+/-", "action"]
        );
        assert_eq!(column_kinds(51), ["SHA", "age", "message", "cov", "action"]);
        assert_eq!(column_kinds(38), ["SHA", "message", "cov", "action"]);
        assert_eq!(column_kinds(32), ["SHA", "message", "action"]);
        assert_eq!(column_kinds(22), ["SHA", "message"]);
        assert!(
            layout(15).is_none(),
            "under one message's worth of cells there is no table"
        );
        assert!(layout(0).is_none());
    }

    fn column_kinds(width: usize) -> Vec<&'static str> {
        layout(width)
            .map(|slots| {
                slots
                    .iter()
                    .map(|slot| {
                        COLUMNS
                            .iter()
                            .find(|column| column.kind == slot.kind)
                            .map(|column| column.header)
                            .unwrap_or("?")
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}
