//! The same table, for a terminal that cannot be trusted with a screen.
//!
//! `--plain` is the ticket's escape hatch: `TERM=dumb`, `ssh` from a phone, `fathom tui | less`,
//! `fathom tui > timeline.txt`, or a TUI that simply will not draw on this machine. It prints what the
//! TUI shows — same rows, same columns, same unknown glyphs, same status line — as text, and it exits.
//!
//! Three things it deliberately shares with the interactive surface rather than duplicating:
//!
//! * **the column model and the cell spelling** ([`super::cells`]), so a cell cannot mean one thing on
//!   screen and another in a log.
//! * **the ascii glyph column** (ADR-0008 §6.1's second table), because a CI log that needs a unicode
//!   terminal is a log nobody reads: `-`, `^`, `v`, `=`, `pass`, `fail`, `mix`.
//! * **the state machine's own words** ([`super::state::App::next_step`] and friends), so the
//!   actionable-next-step lines are not a second, slightly-different set for the plain path.
//!
//! And one thing it does not share: it does no work off-thread. There is no UI to protect from a slow
//! `git`, and pretending otherwise would be a thread and a channel for nobody.

use std::fmt::Write as _;

use ratatui::text::Span;

use super::cells::{self, Align, ColumnKind, Slot};
use super::palette::Palette;
use super::state::App;

/// The width of the plain table, in cells.
///
/// Fixed rather than probed: the whole reason for this surface is a terminal that cannot be asked what
/// it is, and 100 is the width every one of the ADR's own mock screens is drawn at. A narrower column
/// set is produced by the same [`cells::layout`] the TUI uses, so the dropped columns are the same ones
/// the TUI would drop at that width.
pub const WIDTH: usize = 100;

/// Render the whole screen as text: header, table, status, one `\n` per line.
#[must_use]
pub fn table(app: &App, palette: &Palette, width: usize) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{}",
        prose(palette, &format!("{}  {}", app.headline(), app.facts()))
    );

    if app.visible().is_empty() {
        for line in app.next_step() {
            let _ = writeln!(out, "{}", prose(palette, &line));
        }
        return out;
    }

    let Some(slots) = cells::layout(width) else {
        let _ = writeln!(out, "terminal too narrow for a timeline");
        return out;
    };

    let ellipsis = palette.ellipsis();
    let header = cells::header_cells().map(str::to_owned);
    let _ = writeln!(out, "{}", line(&slots, &header, ellipsis));
    for index in app.visible() {
        let entry = &app.reading.entries[*index];
        let texts = cells::cells(entry, &palette.glyphs, app.now);
        let _ = writeln!(out, "{}", line(&slots, &texts, ellipsis));
    }

    let status = app.status_line();
    if !status.is_empty() {
        let _ = writeln!(out, "{}", prose(palette, &status));
    }
    let _ = writeln!(
        out,
        "{}",
        prose(
            palette,
            &format!(
                "{} of {} commits shown \u{b7} d diff, enter detail, b bench are siblings of this \
                 ticket \u{b7} q quits",
                app.visible().len(),
                app.reading.entries.len()
            )
        )
    );
    out
}

/// One of the state machine's prose lines, in this surface's alphabet.
///
/// The cells need no conversion (the glyph table already chose its alphabet); the sentences do, and the
/// rewrite lives in [`super::state::ascii_line`] so the TUI and the plain surface cannot drift into
/// saying the same thing two different ways.
fn prose(palette: &Palette, line: &str) -> String {
    if palette.unicode() {
        line.to_owned()
    } else {
        super::state::ascii_line(line)
    }
}

/// Join one row's cells at their slot offsets, truncating to the slot width and padding to its
/// alignment — the same geometry [`super::render`] draws into the buffer.
fn line(slots: &[Slot], texts: &[String; 8], ellipsis: char) -> String {
    let mut row = String::new();
    let mut column = 0_usize;
    for slot in slots {
        while column < slot.x {
            row.push(' ');
            column += 1;
        }
        let text = index_of(slot.kind)
            .and_then(|index| texts.get(index))
            .cloned()
            .unwrap_or_default();
        let fitted = fit(&text, slot.width, slot.align, ellipsis);
        row.push_str(&fitted);
        column += slot.width;
    }
    row.trim_end().to_owned()
}

/// [`ColumnKind`] → index in the eight-cell row.
fn index_of(kind: ColumnKind) -> Option<usize> {
    [
        ColumnKind::Sha,
        ColumnKind::Age,
        ColumnKind::Message,
        ColumnKind::Coverage,
        ColumnKind::Churn,
        ColumnKind::Mi,
        ColumnKind::Bench,
        ColumnKind::Action,
    ]
    .iter()
    .position(|candidate| *candidate == kind)
}

/// Cut or pad `text` to `width` cells, counting display width rather than bytes.
///
/// A unicode author name or a CJK message must not shift a column: `Span::width` is the same
/// display-width function ratatui uses on the interactive path, which is what makes the two surfaces
/// line up character for character.
/// A cell that does not fit is cut on a character boundary and marked with `ellipsis`, so a truncated
/// message is never read as a whole one; a cell that fits is padded on the far side.
#[must_use]
pub fn fit(text: &str, width: usize, align: Align, ellipsis: char) -> String {
    if width == 0 {
        return String::new();
    }
    if Span::raw(text).width() <= width {
        return match align {
            Align::Left => format!("{text:<width$}"),
            Align::Right => format!("{text:>width$}"),
        };
    }
    // Truncate on character boundaries by display width, one cell short of the column to make room for
    // the marker, then pad the short side back out.
    let mut kept = String::new();
    let mut used = 0;
    for character in text.chars() {
        let step = Span::raw(character.to_string()).width();
        if used + step > width.saturating_sub(1) {
            break;
        }
        kept.push(character);
        used += step;
    }
    kept.push(ellipsis);
    match align {
        Align::Left => format!("{kept:<width$}"),
        Align::Right => format!("{kept:>width$}"),
    }
}
