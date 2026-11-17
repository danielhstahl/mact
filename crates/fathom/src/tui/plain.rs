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

    let status = app.status_line(palette);
    if !status.is_empty() {
        let _ = writeln!(out, "{}", prose(palette, &status));
    }
    let _ = writeln!(
        out,
        "{}",
        prose(
            palette,
            &format!(
                "{} of {} commits shown \u{b7} d opens a commit's diff \u{b7} enter detail and b \
                 bench are siblings of this ticket \u{b7} q quits",
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

/// The diff view, as text: the same commit, the same file list, every file's patch, and the same words
/// about what the caps cut.
///
/// `fathom tui --plain --diff <rev>` is the no-TTY answer to the same key, and it differs from the
/// screen in exactly two ways, both forced by the surface:
///
/// * **every file's patch is printed, not just the selected one** — there is no selection without a key
///   press, and a diff you can only see one file at a time in a pipe is not a diff;
/// * **git's own bytes go to stdout verbatim** — control characters are marked on the screen, where a
///   stray escape could repaint the terminal, and left alone here, where the output is a file.
///
/// What is *not* different: the header, the file list with its `+`/`−`, the truncation banner, and the
/// refusal lines all come from [`super::diff_view::DiffView`], the same code the screen renders.
pub fn diff(view: &super::diff_view::DiffView, palette: &Palette, width: usize) -> String {
    let mut out = String::new();
    // A text dump has no cursor and no `t`, so the dump is the *whole* commit: every file in it, and the
    // header's totals widened to match, which is the same agreement between the list and the `+`/`-` that
    // the screen keeps by the filter and the key.
    let mut view = view.clone();
    view.show_all = true;
    let (left, right) = view.header(palette);
    let _ = writeln!(out, "{}", prose(palette, &format!("{left}  {right}")));
    if let Some(notice) = view.notice() {
        let _ = writeln!(out, "{}", prose(palette, &notice));
    }

    let files = view.all_files();
    if files.is_empty() {
        return out;
    }
    let _ = writeln!(out, "files");
    for row in view.rows(width.saturating_sub(4), palette, false) {
        let _ = writeln!(out, "{row}");
    }

    for (index, file) in files.iter().enumerate() {
        let _ = writeln!(out);
        let _ = writeln!(out, "--- {}/{} {}", index + 1, files.len(), file.label());
        for line in &file.lines {
            // Verbatim, including the sign character git printed. A `LineKind` is a *styling* decision
            // for the screen; on stdout the text is the patch.
            let _ = writeln!(out, "{}", line.text);
        }
        for note in file.no_text().into_iter() {
            let _ = writeln!(out, "{note}");
        }
        if file.dropped > 0 {
            let _ = writeln!(
                out,
                "\u{2026} and {} more bytes of this file not kept \u{b7} raise {}",
                crate::diff::bytes(file.dropped),
                crate::diff::MAX_DIFF_BYTES_PER_FILE_FLAG
            );
        }
    }
    out
}

/// The benchmark panel, as text.
///
/// `fathom tui --plain --bench <rev>` is `b` for a terminal you cannot press `b` in. Two things differ
/// from the screen, both forced by the surface, and nothing else does:
///
/// * **every row is printed, not the visible window** — there is no scrolling in a pipe, and the panel
///   exists to be read top to bottom;
/// * **there is no selection**, so the status line is the panel's notice (the excluded-run overlay's
///   bill, or the empty state) and the band is spelled out in a footer instead of sitting in a column
///   title the reader has already scrolled past.
///
/// What is identical: every cell, every glyph, the header's workflow facts, the empty state's next
/// steps, and the column model — all of it comes from [`super::bench::Panel`], the same code the screen
/// draws.
pub fn bench(panel: &super::bench::Panel, palette: &Palette, width: usize) -> String {
    let mut out = String::new();
    let (left, right) = panel.header(palette);
    let _ = writeln!(out, "{}", prose(palette, &format!("{left}  {right}")));
    // The empty state's headline is `notice()` and `next_step()`'s first line, the same sentence: the
    // table prints it once, as the first line of what to do about it.
    if let Some(notice) = panel.notice().filter(|_| !panel.lines.is_empty()) {
        let _ = writeln!(out, "{}", prose(palette, &notice));
    }
    if panel.lines.is_empty() {
        for line in panel.next_step() {
            let _ = writeln!(out, "{}", prose(palette, &line));
        }
        return out;
    }
    let content = width.saturating_sub(2);
    let _ = writeln!(out, "{}", panel.header_row(content, palette));
    for row in panel.rows(content, palette, false) {
        let _ = writeln!(out, "{row}");
    }
    let _ = writeln!(
        out,
        "{}",
        prose(
            palette,
            &format!(
                "{} metric group(s) shown \u{b7} band {}% of value \u{b7} history walks the last {} \
                 commit(s)",
                panel.lines.len(),
                panel.band_percent,
                panel.window
            )
        )
    );
    out
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
