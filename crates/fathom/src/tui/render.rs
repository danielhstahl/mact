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
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget, Wrap};
use ratatui::Frame;

use crate::mi::score::Band;

use super::cells::{self, Align, ColumnKind, Slot};
use super::diff_view::{DiffView, Screen};
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

    // Two rows of status when the bench panel is up. Its status line is a sentence about the selected
    // metric — the verdict, the samples behind the number, the spread, how much of the history is
    // missing, which run it all came from — and one row at a hundred columns cut it off mid-clause, which
    // is the one place a reader goes to check whether a cell is a regression. The other two screens'
    // status lines are one row long by construction, so they do not pay for the extra row.
    let status_rows = 1 + u16::from(matches!(app.screen, Screen::Bench(_)));
    let [header, body, status, keys] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(status_rows),
        Constraint::Length(1),
    ])
    .areas(area);

    // Both screens spend the same four rows — heading, body, status, keys — so a resize behaves the same
    // way whichever one is up, and the diff view's pane height is derived from the same arithmetic the
    // scroll clamp uses.
    match &app.screen {
        Screen::Timeline => {
            draw_header(app, frame, header, palette);
            draw_table(app, frame, body, palette);
        }
        Screen::Diff(_) => {
            let view = app.diff_view();
            let (left, right) = view.map_or_else(
                || ("DIFF".to_owned(), String::new()),
                |view| view.header(palette),
            );
            draw_heading(&left, &right, frame, header, palette);
            draw_panes(view, frame, body, palette);
        }
        Screen::Bench(_) => {
            let panel = app.bench_view();
            let (left, right) = panel.map_or_else(
                || ("BENCH".to_owned(), String::new()),
                |panel| panel.header(palette),
            );
            draw_heading(&left, &right, frame, header, palette);
            draw_bench(app, frame, body, palette);
        }
    }
    draw_status(app, frame, status, palette);
    draw_keys(app, frame, keys, palette);

    if app.help {
        draw_help(app, frame, palette, area);
    }
}

/// `FATHOM  act-mcp  main@9f31a2c          indexed 412 commits · 3s · evidence ✓`
///
/// The two halves get a horizontal split so the facts stay right-aligned and the repository name stays
/// put; if the terminal is too narrow for both, each is clipped in its own cell rather than one pushing
/// the other off screen.
fn draw_header(app: &App, frame: &mut Frame, area: Rect, palette: &Palette) {
    draw_heading(&app.headline(), &app.facts(), frame, area, palette);
}

/// One heading row: bold text on the left, the facts right-aligned on the right.
fn draw_heading(left: &str, right: &str, frame: &mut Frame, area: Rect, palette: &Palette) {
    let wanted = Span::raw(left).width();
    let right_wanted = Span::raw(right).width();
    // The right half is the one that gets cut first, and the reason is the shape of both headers: on the
    // timeline the left is `FATHOM act-mcp master@9f31a2c` and on the diff screen it is a whole commit
    // subject, while the right is a list of facts a reader's eye returns to the same place for. So the
    // facts keep their room and the subject is what gives way — with a floor, because a heading that has
    // vanished entirely tells you you are in a very narrow terminal and nothing else.
    let floor = 12.min(usize::from(area.width));
    let room = usize::from(area.width);
    // The right half may claim at most half the row, whatever it wanted: the bench panel's fact list is
    // long by nature (which workflows ran, what retention dropped, the superseded and unreadable counts),
    // and a heading whose left half is `BENCH  3b8e…` has stopped saying which commit this is — the one
    // thing the heading exists to say.
    let gutter = if right_wanted == 0 {
        0
    } else {
        right_wanted.min(usize::from(area.width) / 2) + 1
    };
    let head_width = u16::try_from(
        (wanted + 1)
            .min(room.saturating_sub(gutter).max(floor).min(room))
            .min(room),
    )
    .unwrap_or(u16::MAX);
    let [l, r] =
        Layout::horizontal([Constraint::Length(head_width), Constraint::Min(0)]).areas(area);

    let left = if wanted > usize::from(head_width) {
        super::plain::fit(
            left,
            usize::from(head_width),
            Align::Left,
            palette.ellipsis(),
        )
    } else {
        left.to_owned()
    };
    Paragraph::new(Line::from(Span::styled(
        left,
        Style::default().add_modifier(Modifier::BOLD),
    )))
    .render(l, frame.buffer_mut());
    // Right-aligned, as the mock draws it: the repository's identity is a label and the facts are the
    // answer, and an answer that ends at the right edge is one eye can find again on every frame. A
    // fact list too long for what is left loses its tail, which is the least important half.
    let right = if right_wanted > usize::from(area.width) - usize::from(head_width) {
        super::plain::fit(
            right,
            usize::from(area.width) - usize::from(head_width),
            Align::Left,
            palette.ellipsis(),
        )
    } else {
        right.to_owned()
    };
    Paragraph::new(Line::from(Span::styled(
        right,
        Style::default().add_modifier(if palette.colour {
            Modifier::ITALIC
        } else {
            Modifier::DIM
        }),
    )))
    .alignment(Alignment::Right)
    .render(r, frame.buffer_mut());
}

/// The diff screen's two panes: the file list on the left, the patch on the right, in one bordered box
/// with a shared divider — the mock's shape, drawn as one box rather than two so the border columns line
/// up at every width.
fn draw_panes(view: Option<&DiffView>, frame: &mut Frame, area: Rect, palette: &Palette) {
    let block = Block::default().borders(Borders::ALL);
    let inner = block.inner(area);
    block.render(area, frame.buffer_mut());
    if inner.width < 4 || inner.height < 1 {
        return;
    }
    let total = usize::from(inner.width);
    // The files pane is a third of the screen, clamped: narrower than that and a path never fits, wider
    // and the patch is the thing being squeezed out. Below `FILES_MIN + 20` cells there is no honest
    // two-pane drawing, so the files pane goes away and the patch takes the row.
    let wanted = (total / 3).clamp(super::diff_view::FILES_MIN, 34);
    let show_files = total >= super::diff_view::FILES_MIN + 20 && total - wanted >= 20;
    let divider = if show_files {
        let x = usize::from(inner.x) + wanted;
        let buffer = frame.buffer_mut();
        for row in 0..inner.height {
            buffer.set_string(
                u16::try_from(x).unwrap_or(u16::MAX),
                inner.y.saturating_add(row),
                "\u{2502}",
                Style::default().add_modifier(Modifier::DIM),
            );
        }
        buffer.set_string(
            u16::try_from(x).unwrap_or(u16::MAX),
            area.y,
            "\u{252c}",
            Style::default().add_modifier(Modifier::DIM),
        );
        buffer.set_string(
            u16::try_from(x).unwrap_or(u16::MAX),
            area.y.saturating_add(area.height.saturating_sub(1)),
            "\u{2534}",
            Style::default().add_modifier(Modifier::DIM),
        );
        let style = Style::default().add_modifier(Modifier::BOLD);
        buffer.set_stringn(
            area.x + 2,
            area.y,
            " files ",
            usize::from(wanted.saturating_sub(2) as u16),
            style,
        );
        buffer.set_stringn(
            u16::try_from(x + 2).unwrap_or(u16::MAX),
            area.y,
            " diff ",
            usize::MAX,
            style,
        );
        Some(x)
    } else {
        let style = Style::default().add_modifier(Modifier::BOLD);
        frame
            .buffer_mut()
            .set_stringn(area.x + 2, area.y, " diff ", usize::MAX, style);
        None
    };

    let Some(view) = view else {
        return;
    };
    let buffer = frame.buffer_mut();

    // --- the file pane ---------------------------------------------------------------------------
    let files_width = divider.map_or(total, |x| x - usize::from(inner.x));
    if show_files {
        let rows = view.rows(files_width, palette, true);
        for (row, text) in rows.into_iter().enumerate().take(usize::from(inner.height)) {
            let selected = view
                .shown()
                .get(row)
                .is_some_and(|index| *index == view.file);
            let mut style = Style::default();
            if selected {
                style = style.add_modifier(Modifier::BOLD);
            }
            if !palette.colour && !selected {
                style = style.add_modifier(Modifier::DIM);
            }
            buffer.set_stringn(
                inner.x,
                inner
                    .y
                    .saturating_add(u16::try_from(row).unwrap_or(u16::MAX)),
                text,
                files_width,
                style,
            );
        }
    }

    // --- the patch -------------------------------------------------------------------------------
    let patch_x = divider.map_or(usize::from(inner.x), |x| x + 1);
    let patch_width = total
        .saturating_sub(patch_x - usize::from(inner.x) + 1)
        .max(1);
    let lines = view.pane_lines();
    let scroll = view.scroll();
    if lines.is_empty() && view.loading {
        // Where the patch will go, not where the pane begins: the file list is the one thing that can
        // already be answered while `git show` is running, and this is not it.
        buffer.set_stringn(
            u16::try_from(patch_x).unwrap_or(u16::MAX),
            inner.y,
            format!("reading {} \u{2026}", super::diff_view::short(&view.sha)),
            patch_width,
            Style::default().add_modifier(Modifier::DIM),
        );
        return;
    }
    for row in 0..usize::from(inner.height) {
        let Some(index) = scroll.checked_add(row) else {
            break;
        };
        let Some((kind, text)) = lines.get(index) else {
            break;
        };
        let painted = super::diff_view::pane_text(text, patch_width, palette);
        buffer.set_stringn(
            u16::try_from(patch_x).unwrap_or(u16::MAX),
            inner
                .y
                .saturating_add(u16::try_from(row).unwrap_or(u16::MAX)),
            painted,
            patch_width,
            line_style(*kind, palette),
        );
    }
}

/// A patch line's style. Colour repeats what the sign character already says (ADR-0008 §6.1's rule, the
/// diff's own version of it): `+` is green *and* a `+`, so a terminal with colour off still reads the
/// diff correctly and there is no syntax highlighting pretending to understand the code.
fn line_style(kind: crate::diff::LineKind, palette: &Palette) -> Style {
    use crate::diff::LineKind;
    let style = Style::default();
    if !palette.colour {
        return match kind {
            LineKind::Hunk => style.add_modifier(Modifier::BOLD),
            LineKind::Meta | LineKind::Context => style.add_modifier(Modifier::DIM),
            LineKind::Add | LineKind::Delete => style,
        };
    }
    match kind {
        LineKind::Add => style.fg(Color::Green),
        LineKind::Delete => style.fg(Color::Red),
        LineKind::Hunk => style.fg(Color::Cyan).add_modifier(Modifier::BOLD),
        LineKind::Meta => style.add_modifier(Modifier::DIM),
        LineKind::Context => style,
    }
}

/// The benchmark panel's table: one bordered box, a title row of column names, and the rows the
/// panel has.
///
/// No policy lives here: [`super::bench::Panel`] decided which rows exist, what each cell says and which
/// columns the width can afford, and this function puts those strings in a buffer. The empty state is
/// drawn in the table's body for the same reason the timeline draws its own: a bordered box with nothing
/// in it is a dead end, and the lines that name the next step belong where the eye already is.
fn draw_bench(app: &App, frame: &mut Frame, area: Rect, palette: &Palette) {
    let block = Block::default().borders(Borders::ALL).title(Span::styled(
        " benchmark ",
        Style::default().add_modifier(Modifier::BOLD),
    ));
    let inner = block.inner(area);
    block.render(area, frame.buffer_mut());
    let Some(panel) = app.bench_view() else {
        return;
    };
    let buffer = frame.buffer_mut();
    if panel.lines.is_empty() {
        let text = panel
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
                y: inner.y,
                width: inner.width,
                height: inner.height,
            },
            buffer,
        );
        return;
    }
    // One cell of gutter on each side, which is the budget `Panel::rows` was written against.
    let width = usize::from(inner.width.saturating_sub(2));
    buffer.set_stringn(
        inner.x + 1,
        inner.y,
        panel.header_row(width, palette),
        usize::from(inner.width),
        Style::default().add_modifier(Modifier::UNDERLINED),
    );
    let body = usize::from(inner.height.saturating_sub(1));
    let rows = panel.rows(width, palette, true);
    for row in 0..body {
        let index = panel.offset + row;
        let Some(text) = rows.get(index) else {
            break;
        };
        let selected = index == panel.selection;
        let mut style = Style::default();
        if selected {
            style = style.add_modifier(Modifier::BOLD);
            if palette.highlight {
                style = style.add_modifier(Modifier::REVERSED);
            }
        }
        buffer.set_stringn(
            inner.x + 1,
            inner
                .y
                .saturating_add(1)
                .saturating_add(u16::try_from(row).unwrap_or(u16::MAX)),
            text,
            usize::from(inner.width),
            style,
        );
    }
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
    let line = app.status_line(palette);
    let style = if palette.colour {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().add_modifier(Modifier::DIM)
    };
    // Two-row statuses wrap; one-row statuses are cut, as they always were. The difference is deliberate
    // and it is the bench panel that asked for it: a wrapped clause is readable where an ellipsis is a
    // mystery, but a wrap on a one-row box does not gain a row, it loses the partial word at the edge —
    // so the timeline and the diff view keep the cut they were drawn and tested with.
    let mut text = Paragraph::new(Line::from(Span::styled(line, style)));
    if area.height > 1 {
        text = text.wrap(Wrap { trim: false });
    }
    text.render(area, frame.buffer_mut());
}

/// The key line, always visible: the keys of whichever screen is up, and nothing else.
fn draw_keys(app: &App, frame: &mut Frame, area: Rect, palette: &Palette) {
    let style = if palette.colour {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let text = match &app.screen {
        Screen::Timeline => {
            // `3/412`: where the cursor is and how deep the window is. Not the scroll offset — a reader
            // who wants to know "am I near the top" asks the row number, and one number per screen is one
            // number.
            let position = if app.visible().is_empty() {
                String::new()
            } else {
                format!("{}/{}  ", app.selection + 1, app.visible().len())
            };
            format!(
                "{position}d diff   enter detail   b bench   / filter   R re-index   ? help   q quit"
            )
        }
        Screen::Diff(_) => {
            // The diff view's own keys, in the order a reader uses them: read, move, change file, leave.
            let position = app
                .diff_view()
                .and_then(DiffView::shown_position)
                .map_or_else(String::new, |index| format!("{}  ", index + 1));
            format!(
                "{position}j/k scroll   n/p file   t all files   g/G ends   d/Esc back   ? help   q quit"
            )
        }
        Screen::Bench(_) => {
            // The panel's own keys, and `f` where the diff view's `t` is: the key that shows what the
            // derivation left out.
            let panel = app.bench_view();
            let position = panel.map_or_else(String::new, |panel| {
                if panel.lines.is_empty() {
                    String::new()
                } else {
                    format!("{}/{}  ", panel.selection + 1, panel.lines.len())
                }
            });
            let runs = panel.map_or_else(String::new, |panel| {
                if panel.show_excluded {
                    "[f] excluded shown   ".to_owned()
                } else {
                    "[f] excluded hidden  ".to_owned()
                }
            });
            format!("{position}{runs}j/k row   g/G ends   d diff   b/Esc back   ? help   q quit")
        }
    };
    Paragraph::new(Line::from(Span::styled(text, style))).render(area, frame.buffer_mut());
}

/// The `?` overlay: every key this screen answers, and what the glyphs mean.
///
/// The legend is here because ADR-0008 §6.1 pins four action glyphs, three arrows and a caveat marker,
/// and a reader who meets `◐` for the first time has nowhere else to ask. Which keys are listed depends
/// on which screen is up: a legend for the screen you are not on is noise, and the diff view's own notes
/// (first parent, no syntax highlighting in v1, wide lines cut not wrapped) are the things a reader asks
/// about a diff and has no other place to ask.
fn draw_help(app: &App, frame: &mut Frame, palette: &Palette, area: Rect) {
    let glyphs = &palette.glyphs;
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        "keys",
        Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
    )));
    if matches!(app.screen, Screen::Bench(_)) {
        draw_help_box(frame, palette, area, bench_help(app, palette));
        return;
    }
    if matches!(app.screen, Screen::Diff(_)) {
        for (key, what) in [
            ("j / k, PgDn / PgUp", "scroll this file's patch"),
            ("g / G", "top / bottom of this file"),
            (
                "n / p, Tab / Shift-Tab",
                "next / previous file; the list wraps",
            ),
            (
                "t",
                "show the files the path filters hide, and the totals that go with them",
            ),
            ("d / Esc", "back to the timeline, on the row you left"),
            ("/", "search inside a diff: a later ticket"),
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
            "the diff",
            Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        )));
        for note in [
            "the patch is git show against the commit's first parent; a root commit diffs against the empty tree",
            "no syntax highlighting in v1: hunk header, add, delete, context and file header are distinguished and nothing else",
            "long lines are cut with a marker, never wrapped, so a wrapped line is never mistaken for two",
            "tabs, carriage returns and control bytes are shown as marks, so what you read is what the file holds",
            "the +/- column and this file list count the same files; t widens both",
        ] {
            lines.push(Line::from(Span::styled(
                format!("  {note}"),
                Style::default().add_modifier(Modifier::DIM),
            )));
        }
        draw_help_box(frame, palette, area, lines);
        return;
    }
    for (key, what) in [
        ("j / k, up / down", "one commit newer / older"),
        ("PgDn / PgUp", "one screen"),
        ("g / G, Home / End", "newest / oldest commit"),
        (
            "/",
            "filter by sha, author or message; enter applies, esc clears",
        ),
        ("R", "re-index now; esc cancels a running index"),
        ("d", "the selected commit's diff; d or esc comes back"),
        ("enter", "commit detail (mact-by1.14, not wired yet)"),
        ("b", "this commit's benchmark panel; b or esc comes back"),
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

    draw_help_box(frame, palette, area, lines);
}

/// The bench panel's legend: its keys, the marks it draws, and the two things about a benchmark that
/// a reader has no other place to ask about (what a sparkline's scale means, and what a spread of one
/// run does not prove).
fn bench_help(app: &App, palette: &Palette) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        "keys",
        Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
    )));
    for (key, what) in [
        ("j / k, PgDn / PgUp", "one metric / a pane of metrics"),
        ("g / G", "first / last metric"),
        (
            "f",
            "show the runs retention excluded, with the reason each was excluded",
        ),
        ("d", "this commit's diff, from here"),
        ("b / Esc", "back to the timeline, on the row you left"),
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
        "reading this panel",
        Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
    )));
    let marks = format!(
        "{} {} {}",
        palette.glyphs.context,
        if palette.unicode() { "\u{2691}" } else { "!" },
        palette.glyphs.unknown
    );
    for note in [
        "the value is the median of the newest successful run's samples; the spread is every retained run's, and the run count in the cell says how much that spread has to work with",
        "one run with min == max is an unmeasured noise floor, so a delta off it is \u{201c}insufficient\u{201d}, never 0%",
        "a metric with no unit is displayable and never comparable: no delta, no sparkline, no shared axis (ADR-0008 4.3)",
        "every sparkline is scaled to its own min and max, which is why the axis is printed beside it",
        "a metric this commit stopped reporting keeps a row saying when it was last seen; absence is never 0 and never carried forward",
    ] {
        lines.push(Line::from(Span::styled(
            format!("  {note}"),
            Style::default().add_modifier(Modifier::DIM),
        )));
    }
    lines.push(Line::from(""));
    for (glyph, what) in [
        (
            marks.as_str(),
            "act_log context, in_band_stdout, and \u{2014} for a metric with no measurement here",
        ),
        (
            &format!("{} {} {}", palette.glyphs.delta_up, palette.glyphs.delta_down, palette.glyphs.delta_flat),
            "the baseline (the newest older commit with this exact group): up, down, equal",
        ),
        ("!", "outside the declared band in the column title; the band is a percent of value, not a threshold of importance"),
    ] {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{glyph:<20}"),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(what),
        ]));
    }
    if let Some(panel) = app.bench_view() {
        lines.push(Line::from(Span::styled(
            format!(
                "band: {}% of value (fathom.toml benchmarks.regression_band_percent)",
                panel.band_percent
            ),
            Style::default().add_modifier(Modifier::DIM),
        )));
    }
    lines
}

/// The help overlay's box: centred, as large as the screen allows and as small as the text needs, and
/// gone entirely on a screen too small to read it in. Shared by both screens' legends so `?` looks the
/// same whichever key line it came from.
fn draw_help_box(frame: &mut Frame, palette: &Palette, area: Rect, lines: Vec<Line>) {
    let _ = palette;
    let block = Block::default().borders(Borders::ALL).title(Span::styled(
        " help ( ? or esc ) ",
        Style::default().add_modifier(Modifier::BOLD),
    ));
    let width = area.width.saturating_sub(2).min(100);
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
        .scroll((0, 0))
        .render(popup, frame.buffer_mut());
}

/// The slots the table's own width allowed, for a test or a caller that wants to assert the layout.
#[must_use]
pub fn slots_for(width: u16) -> Vec<Slot> {
    cells::layout(usize::from(width.saturating_sub(2))).unwrap_or_default()
}
