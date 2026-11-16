//! Drawing the timeline with ratatui: layout, cells, and colours that never carry the only signal.
//!
//! Nothing in this module decides policy. It reads [`App`], asks [`super::cells`] for the text of each
//! cell, and writes characters into the buffer — which is what lets the snapshot tests in
//! `tests/tui_render.rs` assert a whole screen without a PTY, and what keeps a layout change from
//! quietly changing a measurement.
//!
//! Three constraints are honoured in the drawing rather than in the data:
//!
//! * **no colour-only signal** (ADR-0008 §6.1). Every state has a glyph and the colour only repeats it:
//!   a `✗` stays a `✗` under `NO_COLOR=1`, and the selected row is marked with `▸` *and* reverse video,
//!   so a terminal that ignores either still shows where the cursor is.
//! * **no band invented here.** The MI colour comes from [`Palette::band`], i.e. from ADR-0007 §5.1's
//!   declared edges. Coverage has no declared edges, so coverage is never colour-coded: a red 60%
//!   chosen by the view would be a claim the derivation never made.
//! * **unknown cells are quiet.** An all-unknown row is dimmed rather than zeroed, so "nothing ran"
//!   does not compete for attention with "the numbers are bad".

use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget};
use ratatui::Frame;

use crate::mi::score::Band;

use super::cells::{self, Align, ColumnKind, Slot};
use super::entry::Entry;
use super::palette::Palette;
use super::state::App;

/// The marker column: two cells, `▸ ` on the selected row and two blanks otherwise.
const MARKER: &str = "▸ ";
/// The ascii twin, for the same terminals that get `-` instead of `—`.
const MARKER_ASCII: &str = "> ";

/// The smallest terminal this screen will try to draw in: a line of header, a bordered table of one
/// row, a status line and the key line. Below it there is nothing worth painting.
const MIN_WIDTH: u16 = 12;
/// [`MIN_WIDTH`]'s other half.
const MIN_HEIGHT: u16 = 7;

/// Draw one frame.
pub fn draw(app: &App, frame: &mut Frame, palette: &Palette) {
    let area = frame.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        Paragraph::new(Line::from(Span::styled(
            format!("fathom: terminal too small (need {MIN_WIDTH}x{MIN_HEIGHT})"),
            Style::default().add_modifier(Modifier::BOLD),
        )))
        .render(area, frame.buffer_mut());
        return;
    }

    let [header, body, status, keys] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);

    draw_header(app, frame, header, palette);
    draw_table(app, frame, body, palette);
    draw_status(app, frame, status, palette);
    draw_keys(app, frame, keys, palette);

    if app.help {
        draw_help(frame, palette, area);
    }
}

/// `FATHOM  act-mcp  main@9f31a2c          indexed 412 commits · 3s · evidence ✓`
///
/// The two halves get a horizontal split so the facts stay right-aligned and the repository name stays
/// put; if the terminal is too narrow for both, each is clipped in its own cell rather than one pushing
/// the other off screen.
fn draw_header(app: &App, frame: &mut Frame, area: Rect, palette: &Palette) {
    let headline = app.headline();
    let facts = app.facts();
    let wanted = Span::raw(&headline).width();
    // One cell of gutter between the two halves, and the gutter is the first thing to go when the
    // terminal cannot afford both: a fact squashed to one character is worse than no fact.
    let head_width = u16::try_from((wanted + 1).min(usize::from(area.width))).unwrap_or(u16::MAX);
    let [left, right] =
        Layout::horizontal([Constraint::Length(head_width), Constraint::Min(0)]).areas(area);

    Paragraph::new(Line::from(Span::styled(
        headline,
        Style::default().add_modifier(Modifier::BOLD),
    )))
    .render(left, frame.buffer_mut());
    // Right-aligned, as the mock draws it: the repository's identity is a label and the facts are the
    // answer, and an answer that ends at the right edge is one eye can find again on every frame. A
    // fact list too long for what is left loses its tail, which is the least important half.
    Paragraph::new(Line::from(Span::styled(
        facts,
        Style::default().add_modifier(if palette.colour {
            Modifier::ITALIC
        } else {
            Modifier::DIM
        }),
    )))
    .alignment(Alignment::Right)
    .render(right, frame.buffer_mut());
}

/// The bordered table: one header line of column names and the visible window of rows.
fn draw_table(app: &App, frame: &mut Frame, area: Rect, palette: &Palette) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            " commit ",
            Style::default().add_modifier(Modifier::BOLD),
        ))
        .title_alignment(Alignment::Left);
    let inner = block.inner(area);
    block.render(area, frame.buffer_mut());

    // The marker column spends two cells of the table's width; every slot offset is relative to what
    // is left.
    let table_width = usize::from(inner.width.saturating_sub(2));
    let Some(slots) = cells::layout(table_width) else {
        Paragraph::new(Line::from(Span::styled(
            "terminal too narrow for a timeline",
            Style::default().add_modifier(Modifier::DIM),
        )))
        .render(inner, frame.buffer_mut());
        return;
    };

    let buffer = frame.buffer_mut();
    let header_texts = slot_texts(&cells::header_cells().map(str::to_owned), &slots);
    draw_row(
        buffer,
        inner,
        palette,
        &RowPaint {
            line: 0,
            slots: &slots,
            texts: &header_texts,
            style: Style::default().add_modifier(Modifier::UNDERLINED),
            selected: false,
        },
    );

    let height = usize::from(inner.height.saturating_sub(1));
    if app.visible().is_empty() {
        // The empty and degenerate states, in the table's own body: no store, no commits, no rows, no
        // match. Each line names the next action rather than mourning the absence.
        let text = app
            .next_step()
            .into_iter()
            .map(|line| {
                Line::from(Span::styled(
                    line,
                    Style::default().add_modifier(Modifier::DIM),
                ))
            })
            .collect::<Vec<Line>>();
        Paragraph::new(text).render(
            Rect {
                x: inner.x,
                y: inner.y + 1,
                width: inner.width,
                height: inner.height.saturating_sub(1),
            },
            buffer,
        );
        return;
    }

    for row in 0..height {
        let index = app.offset + row;
        if index >= app.visible().len() {
            break;
        }
        let entry = &app.reading.entries[app.visible()[index]];
        let row_cells = cells::cells(entry, &palette.glyphs, app.now);
        let texts = slot_texts(&row_cells, &slots);
        let selected = index == app.selection;
        draw_row(
            buffer,
            inner,
            palette,
            &RowPaint {
                line: row + 1,
                slots: &slots,
                texts: &texts,
                style: row_style(entry, palette, selected),
                selected,
            },
        );
    }
}

/// Pick the cells the surviving columns asked for, in slot order.
///
/// [`cells::cells`] always returns all eight; the layout may have dropped some, and zipping the two in
/// the order they came would shift every surviving column left by one the moment a narrow terminal
/// dropped `bench`.
fn slot_texts(all: &[String; 8], slots: &[Slot]) -> Vec<String> {
    slots
        .iter()
        .map(|slot| {
            let index = COLUMNS_ORDER
                .iter()
                .position(|kind| *kind == slot.kind)
                .unwrap_or(0);
            all.get(index).cloned().unwrap_or_default()
        })
        .collect()
}

/// [`ColumnKind`] in [`cells::COLUMNS`] order — the index that ties a slot to a cell.
const COLUMNS_ORDER: [ColumnKind; 8] = [
    ColumnKind::Sha,
    ColumnKind::Age,
    ColumnKind::Message,
    ColumnKind::Coverage,
    ColumnKind::Churn,
    ColumnKind::Mi,
    ColumnKind::Bench,
    ColumnKind::Action,
];

/// What every cell in one row shares, gathered so `draw_row` takes three arguments and not eight.
struct RowPaint<'a> {
    /// Which line of the table body this is; the header is line 0.
    line: usize,
    /// The columns that survived the width budget.
    slots: &'a [Slot],
    /// Their text, in slot order.
    texts: &'a [String],
    /// The row's base style, band and caveats already resolved.
    style: Style,
    /// Whether this is the row the cursor is on.
    selected: bool,
}

/// One row of cells, `line` lines below the table's top border.
fn draw_row(
    buffer: &mut ratatui::buffer::Buffer,
    inner: Rect,
    palette: &Palette,
    row: &RowPaint<'_>,
) {
    let RowPaint {
        line,
        slots,
        texts,
        style,
        selected,
    } = *row;
    let y = inner
        .y
        .saturating_add(u16::try_from(line).unwrap_or(u16::MAX));
    let marker = if palette.unicode() {
        MARKER
    } else {
        MARKER_ASCII
    };
    let marker_text = if selected { marker } else { "  " };
    let x = inner.x;
    let cell_style = if selected && palette.highlight {
        style.add_modifier(Modifier::REVERSED)
    } else {
        style
    };
    buffer.set_stringn(x, y, marker_text, 2, cell_style);

    let base = usize::from(x) + 2;
    let right_edge = usize::from(inner.x) + usize::from(inner.width);
    for (slot, text) in slots.iter().zip(texts.iter()) {
        // Clip to the slot *first*, with the same display-width arithmetic `--plain` uses: a message
        // longer than its column otherwise paints over its neighbours, and a cell that paints over a
        // measurement is a cell that deletes it. The cut is marked, and the padding is part of the same
        // fit, so a right-aligned column starts where the column says it does whatever the text's own
        // width is.
        let fitted = super::plain::fit(text, slot.width, slot.align, palette.ellipsis());
        let text_width = Span::raw(&fitted).width();
        let column_x = base + slot.x;
        let start = match slot.align {
            Align::Left => column_x,
            Align::Right => column_x + slot.width.saturating_sub(text_width),
        };
        // Never draw past the border, whatever the arithmetic above decided.
        let room = right_edge.saturating_sub(start).max(1);
        buffer.set_stringn(
            u16::try_from(start).unwrap_or(u16::MAX),
            y,
            &fitted,
            room,
            cell_style,
        );
    }
}

/// The row's base style: colour repeats the glyphs, and an all-unknown row is dimmed.
fn row_style(entry: &Entry, palette: &Palette, selected: bool) -> Style {
    let mut style = Style::default();
    if selected {
        style = style.add_modifier(Modifier::BOLD);
    }
    if !palette.colour {
        return style;
    }
    if let Some(mi) = entry.mi.value {
        style = style.fg(match palette.band(mi) {
            Band::Green => Color::Green,
            Band::Amber => Color::Yellow,
            Band::Red => Color::Red,
        });
    } else if entry.all_unknown() {
        style = style.add_modifier(Modifier::DIM);
    }
    style
}

/// The one-line explanation: a message, the filter prompt, or the selected row's caveats.
fn draw_status(app: &App, frame: &mut Frame, area: Rect, palette: &Palette) {
    let line = app.status_line();
    let style = if palette.colour {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().add_modifier(Modifier::DIM)
    };
    Paragraph::new(Line::from(Span::styled(line, style))).render(area, frame.buffer_mut());
}

/// The key line, always visible: the ticket's tight surface and nothing else.
fn draw_keys(app: &App, frame: &mut Frame, area: Rect, palette: &Palette) {
    // `3/412`: where the cursor is and how deep the window is. Not the scroll offset — a reader who
    // wants to know "am I near the top" asks the row number, and one number per screen is one number.
    let position = if app.visible().is_empty() {
        String::new()
    } else {
        format!("{}/{}  ", app.selection + 1, app.visible().len())
    };
    let text = format!(
        "{position}d diff   enter detail   b bench   / filter   R re-index   ? help   q quit"
    );
    let style = if palette.colour {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    Paragraph::new(Line::from(Span::styled(text, style))).render(area, frame.buffer_mut());
}

/// The `?` overlay: every key this screen answers, and what the glyphs mean.
///
/// The legend is here because ADR-0008 §6.1 pins four action glyphs, three arrows and a caveat marker,
/// and a reader who meets `◐` for the first time has nowhere else to ask.
fn draw_help(frame: &mut Frame, palette: &Palette, area: Rect) {
    let glyphs = &palette.glyphs;
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        "keys",
        Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
    )));
    for (key, what) in [
        ("j / k, up / down", "one commit newer / older"),
        ("PgDn / PgUp", "one screen"),
        ("g / G, Home / End", "newest / oldest commit"),
        (
            "/",
            "filter by sha, author or message; enter applies, esc clears",
        ),
        ("R", "re-index now; esc cancels a running index"),
        (
            "d",
            "diff of the selected commit (mact-by1.11, not wired yet)",
        ),
        ("enter", "commit detail (mact-by1.14, not wired yet)"),
        ("b", "benchmark panel (mact-by1.12, not wired yet)"),
        ("?", "this help"),
        ("q / Ctrl-C", "quit"),
    ] {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{key:<20}"),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(what),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "glyphs",
        Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
    )));
    let arrows = format!(
        "{} {} {}",
        glyphs.delta_up, glyphs.delta_down, glyphs.delta_flat
    );
    for (glyph, what) in [
        (
            glyphs.unknown.as_str(),
            "unknown: nothing was measured here (a 0 is always a measurement)",
        ),
        (
            glyphs.caveat_marker.as_str(),
            "this column has caveats; the line under the table names them",
        ),
        (
            arrows.as_str(),
            "versus the first parent: up, down, equal at the stored precision",
        ),
        (
            glyphs.context.as_str(),
            "a run-level metric: context, not a measurement of the code",
        ),
        (
            glyphs.success.as_str(),
            "every action group's newest run passed",
        ),
        (glyphs.failed.as_str(), "none of them did"),
        (glyphs.mixed.as_str(), "some passed and some did not"),
    ] {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{glyph:<20}"),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(what),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!(
            "MI bands: green >= {}, amber >= {} (ADR-0007 5.1) · glyph table: schema/metric-model.json",
            palette.amber_edge(),
            palette.red_edge()
        ),
        Style::default().add_modifier(Modifier::DIM),
    )));

    let block = Block::default().borders(Borders::ALL).title(Span::styled(
        " help ( ? or esc ) ",
        Style::default().add_modifier(Modifier::BOLD),
    ));
    let width = area.width.saturating_sub(2).min(84);
    let height = area
        .height
        .saturating_sub(2)
        .min(u16::try_from(lines.len() + 2).unwrap_or(u16::MAX));
    if width < 10 || height < 5 {
        return;
    }
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    Clear.render(popup, frame.buffer_mut());
    Paragraph::new(lines)
        .block(block)
        .render(popup, frame.buffer_mut());
}

/// The slots the table's own width allowed, for a test or a caller that wants to assert the layout.
#[must_use]
pub fn slots_for(width: u16) -> Vec<Slot> {
    cells::layout(usize::from(width.saturating_sub(2))).unwrap_or_default()
}
