//! The `age` column keeps up with the clock (mact-s1p).
//!
//! The column counts back from a wall clock, so the clock belongs to the loop rather than to the
//! process: a `now` read once when the screen opened froze the whole table, and the commit that landed
//! at 14:00 was still `5m` old at 19:00 however long the tab stayed open. What this file pins is the
//! fix in both directions — a row's age moves when the clock crosses the step its text is spelled in,
//! and *only* then, so a screen with nothing to say still paints zero frames
//! ([`fathom::tui::state::App::age_deadline`], [`fathom::tui::entry::age_next_change`]).
//!
//! The clock is wound by the driver rather than slept through: each tick states how many seconds passed
//! between two passes of the loop, which makes "one frame per change of text" an assertion about the
//! loop's decisions instead of a race with this machine's clock. One tick still sleeps past
//! [`FRAME_MS`] — the loop's frame floor is real, and a test that outran it would be counting coalesced
//! frames rather than chosen ones. The first tick of every test is `0`, because the opening frame is
//! drawn before any time has passed.

use std::collections::VecDeque;
use std::time::Duration;

use fathom::metrics::vocab::ActionState;
use fathom::tui::entry::{Bench, Churn, Entry, Metric};
use fathom::tui::palette::Palette;
use fathom::tui::state::{App, Command, Key};
use fathom::tui::store_view::Reading;
use fathom::tui::{run_loop, Driver, Input, FRAME_MS};

/// The instant the screen opened, and the author instant of the newest fixture row.
const OPENED: i64 = 1_767_225_600;
/// A day of fixture ages.
const DAY: i64 = 86_400;
/// The width every screen here is drawn at.
const WIDTH: u16 = 100;

/// One timeline row, authored at `author_utc`.
///
/// Every metric column is unknown: this file is about the one column that is not.
fn row(index: usize, author_utc: i64) -> Entry {
    Entry {
        sha: format!("{index:040x}"),
        parent: None,
        author_utc,
        committer_utc: author_utc,
        subject: format!("commit {index}"),
        author: "Loop RS".to_owned(),
        coverage: Metric::UNKNOWN,
        churn: Churn::UNKNOWN,
        mi: Metric::UNKNOWN,
        unanalysed_paths: 0,
        bench: Bench::missing(None),
        detail: fathom::tui::bench::Commit::default(),
        action: ActionState::NoEvidence,
        indexed: true,
        caveats: Vec::new(),
    }
}

/// A timeline of `rows`, newest first as the store spells it.
fn reading(rows: Vec<Entry>) -> Reading {
    Reading {
        repo: "act-mcp".to_owned(),
        entries: rows,
        ..Reading::default()
    }
}

/// A driver with no terminal and a clock the test winds.
///
/// Each tick is one pass of a loop that found nothing to do but wait, and `seconds` of wall clock gone
/// by while it waited; when the ticks run out the loop is quit, so every frame in [`Wound::drawn`] is a
/// frame the loop decided to paint.
struct Wound {
    ticks: VecDeque<i64>,
    clock: i64,
    /// The `age` cell as drawn at each frame, oldest first: what a reader watching the column saw.
    drawn: Vec<String>,
}

impl Wound {
    fn new(ticks: &[i64]) -> Self {
        Self {
            ticks: ticks.iter().copied().collect(),
            clock: OPENED,
            drawn: Vec::new(),
        }
    }

    /// The `age` cell of the top row at `now`, spelled the way the table spells it.
    fn age_cell(app: &App, now: i64) -> String {
        let glyphs = Palette::interactive().glyphs;
        fathom::tui::cells::cells(
            app.reading.entries.first().expect("a row to read"),
            &glyphs,
            now,
        )[1]
        .clone()
    }
}

impl Driver for Wound {
    fn input(&mut self, _timeout: Duration) -> Input {
        let Some(seconds) = self.ticks.pop_front() else {
            return Input::Key(Key::Quit);
        };
        self.clock += seconds;
        // Past the loop's frame floor, so a frame it owes is one it is allowed to draw now.
        std::thread::sleep(Duration::from_millis(FRAME_MS + 5));
        Input::Timeout
    }

    fn events(&mut self) -> Vec<fathom::tui::engine::Event> {
        Vec::new()
    }

    fn execute(&mut self, _commands: &[Command]) {}

    fn paint(&mut self, app: &App) -> std::io::Result<()> {
        self.drawn.push(Self::age_cell(app, app.now));
        Ok(())
    }

    fn size(&self) -> (u16, u16) {
        (WIDTH, 24)
    }

    fn now(&self) -> i64 {
        self.clock
    }
}

#[test]
fn the_age_column_moves_with_the_clock() {
    // One commit, authored at the moment the screen opened: the only thing that changes here is `now`.
    let mut app = App::new(reading(vec![row(0, OPENED)]), OPENED);
    // A minute, four minutes past the open, then two hours: a frame per change of text, and no more.
    let mut driver = Wound::new(&[0, 61, 179, 7_140]);

    let paints = run_loop(&mut app, &mut driver).expect("a scripted paint cannot fail");

    assert_eq!(
        driver.drawn,
        vec!["0m", "1m", "4m", "2h"],
        "the column says what the clock says, and changes only when the text changes"
    );
    assert_eq!(
        paints, 4,
        "one frame per change of text — the clock moving is not itself a reason to paint"
    );
    assert_eq!(
        app.now,
        OPENED + 7_380,
        "and the screen holds the last clock it was given"
    );
}

#[test]
fn a_clock_that_has_not_crossed_a_step_earns_no_frame() {
    // Fifty seconds in four bites: the text never changes, so the screen must not either. This is the
    // half of mact-s1p that keeps the fix from becoming "repaint once a second forever".
    let mut app = App::new(reading(vec![row(0, OPENED)]), OPENED);
    let mut driver = Wound::new(&[0, 10, 10, 10, 20]);

    let paints = run_loop(&mut app, &mut driver).expect("a scripted paint cannot fail");

    let mut said = driver.drawn.clone();
    said.dedup();
    assert_eq!(
        said,
        vec!["0m"],
        "fifty seconds of clock, and not one new thing to print"
    );
    assert!(
        paints <= 2,
        "the opening frame, plus at most the frame the `q` that ends the loop is drawn into: {paints}"
    );
    assert_eq!(
        app.now,
        OPENED + 50,
        "the clock moved; it simply had nothing to say"
    );
}

#[test]
fn a_future_dated_row_does_not_buy_a_frame_a_minute() {
    // Clock skew and a mistyped `--date` both put a commit in the future, and `age` spells that `0m`
    // until the clock arrives. A deadline that ignored the skew would repaint every minute, on a screen
    // whose only news is that nothing has happened yet.
    let mut app = App::new(reading(vec![row(0, OPENED + 3_600)]), OPENED);
    let mut driver = Wound::new(&[0, 60, 60, 60]);

    let paints = run_loop(&mut app, &mut driver).expect("a scripted paint cannot fail");

    let mut said = driver.drawn.clone();
    said.dedup();
    assert_eq!(
        said,
        vec!["0m"],
        "still `0m` on every frame the screen drew"
    );
    assert!(
        paints <= 2,
        "a commit an hour in the future has an hour and a minute to wait, so it may not ask for a frame \
         a minute: {paints}"
    );
}

#[test]
fn the_freshest_row_on_the_page_sets_the_pace() {
    // A fresh row, a day-old one and a week-old one are on the page: the fresh row's step is what the
    // next frame waits for, and a row scrolled off the bottom is not waited for at all, because
    // scrolling dirties the screen by itself.
    let mut app = App::new(
        reading(vec![
            row(0, OPENED),
            row(1, OPENED - 3 * DAY),
            row(2, OPENED - 7 * DAY),
        ]),
        OPENED,
    );
    assert_eq!(
        app.age_deadline(),
        Some(OPENED + 60),
        "the newest row has 60 s of `0m` left, and it is the soonest news on the page"
    );

    app.set_size(WIDTH, 2);
    app.offset = 1;
    assert_eq!(
        app.age_deadline(),
        Some(OPENED + DAY),
        "scrolled past the fresh row, the page's next change is a day away"
    );
}

#[test]
fn a_page_with_no_rows_has_no_age_to_keep() {
    // No store, no commits, or a filter nothing matches: there is no cell on it that a clock could move.
    assert_eq!(App::new(Reading::default(), OPENED).age_deadline(), None);

    let mut app = App::new(reading(vec![row(0, OPENED)]), OPENED);
    let commands = app.on_key(Key::Filter);
    assert!(commands.is_empty());
    app.on_key(Key::Text('x'));
    assert!(
        app.age_deadline().is_none(),
        "a filter that matches nothing leaves no age on the page"
    );
}

#[test]
fn a_screen_that_draws_no_age_owes_the_clock_nothing() {
    // The diff, the bench panel and the detail view cover the table, so a minute rolling over is not news
    // on them: no deadline, and therefore no frame for the clock to ask for.
    let sha = format!("{:040x}", 0);
    for (what, screen) in [
        ("diff", fathom::tui::diff_view::Screen::Diff(sha.clone())),
        ("bench", fathom::tui::diff_view::Screen::Bench(sha.clone())),
        ("detail", fathom::tui::diff_view::Screen::Detail(sha)),
    ] {
        let mut app = App::new(reading(vec![row(0, OPENED)]), OPENED);
        app.screen = screen;
        assert_eq!(
            app.age_deadline(),
            None,
            "the {what} screen draws no age cell"
        );
    }
}

#[test]
fn the_drawn_screen_carries_the_new_age() {
    // The loop's bookkeeping is not the point; the cell is. Draw the screen the loop ended on and read
    // the row out of the buffer, so "it refreshed" means it refreshed on the terminal.
    use ratatui::backend::TestBackend;
    use ratatui::layout::Position;
    use ratatui::Terminal;

    let mut app = App::new(reading(vec![row(0, OPENED)]), OPENED);
    let mut driver = Wound::new(&[0, 7_380]);
    run_loop(&mut app, &mut driver).expect("a scripted paint cannot fail");

    app.set_size(WIDTH, 12);
    let mut terminal = Terminal::new(TestBackend::new(WIDTH, 12))
        .expect("a TestBackend cannot fail to initialise");
    terminal
        .draw(|frame| fathom::tui::render::draw(&app, frame, &Palette::interactive()))
        .expect("drawing cannot fail on a TestBackend");
    let buffer = terminal.backend().buffer();
    let drawn = (0..12_u16)
        .map(|y| {
            (0..WIDTH)
                .map(|x| {
                    buffer
                        .cell(Position::new(x, y))
                        .map_or(" ", |cell| {
                            // One `char` per cell, with a double-width filler spelled as a space.
                            if cell.symbol().is_empty() {
                                " "
                            } else {
                                cell.symbol()
                            }
                        })
                        .to_owned()
                })
                .collect::<String>()
        })
        .find(|line| line.contains("0000000"))
        .expect("the row was drawn")
        .trim_end()
        .to_owned();

    assert!(
        drawn.contains("2h"),
        "the screen says two hours for a two-hour-old commit: {drawn}"
    );
    assert!(
        !drawn.contains("0m"),
        "and not the age it had when the screen opened: {drawn}"
    );
}
