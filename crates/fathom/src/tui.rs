//! The timeline screen: one row per commit, five metric columns, and nothing on the UI thread.
//!
//! This is the ticket's "main screen", and the module layout *is* the architecture:
//!
//! ```text
//! tui.rs        the loop, the terminal's lifecycle, the --plain fork, the driver contract
//!   state.rs    what the screen knows and what the keys do to it — pure, testable on its own
//!   detail      the commit detail view: three panes over one commit, one shared path, three sorts
//!   diff_view   the diff screen's own state: per-commit view state, the file filter, and what the caps say
//!   bench       the benchmark panel's own state: one row per metric group, its history and its verdict
//!   cells.rs    values → text: ADR-0008 §6.1's glyphs, §4.5's one bench group, the width budget
//!   entry.rs    a commit reduced to what the timeline can say: unknown is `None`, never 0
//!   store_view  the only reader of `.fathom` and of `git`, and the only place either is touched
//!   engine.rs   the worker thread: jobs out, events in, cancellation, live index progress
//!   render.rs   ratatui drawing — no policy, no I/O
//!   plain.rs    the same table as text, for the terminal that cannot be trusted
//!   keys.rs     KeyEvent → Key, so the key tests cover the mapping and not only the meaning
//!   palette.rs  which glyphs and which colours this surface may use
//! ```
//!
//! ## Why the loop looks like this
//!
//! The ticket asks for two things that are usually traded against each other — never delay a keypress,
//! and never render a frame per record — and they are separated here instead of traded: input is polled
//! on a short tick ([`engine::TICK_MS`]) so a keypress is handled within one tick, and painting is gated
//! separately by [`FRAME_MS`] and by a dirty flag, so 500 published rows become a handful of frames and
//! 500 keypresses become at most ~15 frames a second. The loop is one pass of *drain → one input → drain
//! → maybe paint*; there is no `select`, because the runtime `select` needs is forbidden in this crate
//! (ADR-0005 §6: no tokio, ever) and a thread plus two channels is exactly what that ADR says the TUI's
//! synchronous file and git reads deserve instead.
//!
//! ## The rules this module keeps
//!
//! * **stdout is the screen, stderr is the diagnosis** (`lib.rs`'s rule 1). The TUI owns the terminal
//!   while it runs and gives it back before anything else prints.
//! * **the store is the only data source.** No evidence walk, no MI analysis, no `git log` on this
//!   thread. A commit with no row is an unknown row, which is what lets a half-indexed repository render
//!   honestly instead of waiting.
//! * **an index run is the store layer's business.** `R` asks [`store::Indexer`] to run on the worker
//!   thread; the screen watches the lock file and the rows directory, cancels on a commit boundary, and
//!   never re-implements either half.

pub mod bench;
pub mod cells;
pub mod detail;
pub mod diff_view;
pub mod engine;
pub mod entry;
pub mod keys;
pub mod palette;
pub mod plain;
pub mod render;
pub mod state;
pub mod store_view;

use std::io::{self, IsTerminal as _};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event as TermEvent, KeyEventKind};
use ratatui::DefaultTerminal;

use crate::boot::Boot;
use crate::cli::CommonArgs;
use crate::config::Config;
use crate::diff as patch;
use crate::exit::Exit;
use crate::git::Git;
use crate::store::{self, now_utc};
use crate::{info, warn};

use self::diff_view::DiffView;
use self::engine::{Engine, Event, StoreWork};
use self::palette::Palette;
use self::state::{App, Command, Key};

/// The minimum interval between two frames.
///
/// ~15 fps: fast enough that scrolling and a live index counter look alive, slow enough that a burst of
/// events cannot turn into a frame each. The cap is a wall-clock floor, not a sleep — an idle screen
/// repaints zero times.
pub const FRAME_MS: u64 = 66;

/// What the driver found to say, with a name for "nothing yet".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    /// A key the state machine cares about (or `Ignored`, which is still a keypress).
    Key(Key),
    /// The terminal changed size; ask it again and repaint.
    Resized,
    /// The poll window elapsed with nothing in it.
    Timeout,
}

/// Everything the loop may ask the outside world for.
///
/// This is the seam the responsiveness test drives: a scripted driver supplies keys and events with no
/// terminal and no files, so "a slow source does not delay a keypress" is a statement about the loop
/// rather than about this machine's disk.
pub trait Driver {
    /// Wait up to `timeout` for the next input.
    fn input(&mut self, timeout: Duration) -> Input;
    /// Everything the background produced since the last call, without waiting.
    fn events(&mut self) -> Vec<Event>;
    /// Do what the state machine asked for.
    fn execute(&mut self, commands: &[Command]);
    /// Draw the frame.
    fn paint(&mut self, app: &App) -> io::Result<()>;
    /// The current terminal size.
    fn size(&self) -> (u16, u16);
}

/// One pass of the loop, as a function so a test can call it with a scripted driver.
///
/// Returns the number of frames painted, which is how `tests/tui_responsive.rs` asserts the frame
/// discipline instead of trusting a comment: an event burst that paints once per event is visible as a
/// number.
pub fn run_loop(app: &mut App, driver: &mut dyn Driver) -> io::Result<usize> {
    let frame = Duration::from_millis(FRAME_MS);
    let tick = Duration::from_millis(engine::TICK_MS);
    let mut paints = 0_usize;
    let mut last_paint: Option<Instant> = None;
    let mut dirty = true;

    let (width, height) = driver.size();
    app.set_size(width, height);

    while !app.quit {
        // Drain before input so a burst of index events is applied (and coalesced into one frame)
        // before anything is drawn.
        for event in driver.events() {
            let commands = app.apply(event);
            driver.execute(&commands);
            dirty = true;
        }

        match driver.input(tick) {
            Input::Key(key) => {
                let commands = app.on_key(key);
                driver.execute(&commands);
                dirty = true;
            }
            Input::Resized => {
                let (width, height) = driver.size();
                app.set_size(width, height);
                dirty = true;
            }
            Input::Timeout => {}
        }

        // Drain again: an event that arrived while the key was being handled is this frame's business,
        // and the next `input` call would otherwise have to wait a tick to be noticed.
        for event in driver.events() {
            let commands = app.apply(event);
            driver.execute(&commands);
            dirty = true;
        }

        if dirty && last_paint.is_none_or(|since| since.elapsed() >= frame) {
            driver.paint(app)?;
            last_paint = Some(Instant::now());
            dirty = false;
            paints += 1;
        }
    }
    Ok(paints)
}

/// The interactive driver: ratatui on the alternate screen, crossterm for input, one engine thread.
pub struct TerminalDriver {
    terminal: DefaultTerminal,
    engine: Engine,
    palette: Palette,
}

impl TerminalDriver {
    /// Take over the terminal and start the engine.
    #[must_use]
    pub fn new(terminal: DefaultTerminal, engine: Engine, palette: Palette) -> Self {
        Self {
            terminal,
            engine,
            palette,
        }
    }
}

impl Driver for TerminalDriver {
    fn input(&mut self, timeout: Duration) -> Input {
        // Resize is folded into the key poll rather than polled separately: one wait, one answer, and
        // `Event::Resize` wins when both are pending because a resize invalidates the frame anyway.
        if event::poll(timeout).unwrap_or(false) {
            match event::read() {
                Ok(TermEvent::Key(key)) if key.kind == KeyEventKind::Press => {
                    return Input::Key(keys::classify(key));
                }
                Ok(TermEvent::Resize(_, _)) => return Input::Resized,
                Ok(_) => return Input::Timeout,
                Err(_) => return Input::Timeout,
            }
        }
        Input::Timeout
    }

    fn events(&mut self) -> Vec<Event> {
        self.engine.drain()
    }

    fn execute(&mut self, commands: &[Command]) {
        for command in commands {
            match command {
                Command::Read => self.engine.request_read(),
                Command::Index => self.engine.request_index(),
                Command::Cancel => self.engine.cancel(),
                Command::Diff(sha) => self.engine.request_diff(sha),
                Command::Detail(sha) => self.engine.request_detail(sha),
                Command::Quit => {}
            }
        }
    }

    fn paint(&mut self, app: &App) -> io::Result<()> {
        self.terminal
            .draw(|frame| render::draw(app, frame, &self.palette))?;
        Ok(())
    }

    fn size(&self) -> (u16, u16) {
        self.terminal
            .size()
            .map_or((80, 24), |size| (size.width, size.height))
    }
}

/// A driver that never touches a terminal, for tests and for `--plain`.
pub struct NullDriver;

impl Driver for NullDriver {
    fn input(&mut self, timeout: Duration) -> Input {
        let _ = timeout;
        Input::Timeout
    }

    fn events(&mut self) -> Vec<Event> {
        Vec::new()
    }

    fn execute(&mut self, commands: &[Command]) {
        let _ = commands;
    }

    fn paint(&mut self, _app: &App) -> io::Result<()> {
        Ok(())
    }

    fn size(&self) -> (u16, u16) {
        (100, 24)
    }
}

/// Run `fathom tui` (or bare `fathom`), after boot has validated the workspace.
///
/// The fork is the first thing: `--plain`, or a stdout that is not a terminal, or a `TERM` that says
/// `dumb`, takes the text path and never initialises the alternate screen. A user piped into `less` gets
/// a table, not 400 lines of escape sequences, and a user whose terminal is broken in a way ratatui
/// disagrees with still has the escape hatch the flag names.
///
/// # Errors
///
/// A [`crate::FathomError`] if the terminal refuses to be initialised — reported as a refusal, with the
/// `--plain` line as the fix.
pub fn run(
    boot: &Boot,
    config: &Config,
    arguments: &CommonArgs,
) -> Result<Exit, crate::FathomError> {
    let term = std::env::var("TERM").ok();
    let no_color = std::env::var("NO_COLOR").is_ok_and(|value| !value.is_empty());
    let not_a_tty = !std::io::stdout().is_terminal();
    let plain = arguments.plain
        || not_a_tty
        || term
            .as_deref()
            .is_none_or(|value| value.is_empty() || value == "dumb");
    if plain && !arguments.plain {
        // Say it on stderr: the table on stdout is the answer, the reason is a diagnosis.
        warn!(
            "not drawing a TUI (stdout is not a terminal, or TERM is dumb); printing the plain table \
             — pass --plain to make it explicit"
        );
    }

    let query = crate::cli::window(config, arguments);
    let limits = crate::cli::diff_limits(arguments);
    let now = now_utc();

    if plain {
        let palette = Palette::resolve(term.as_deref(), no_color, true, arguments.no_color);
        // `--diff REV` is the same key, asked on the command line: with no terminal to press `d` in, the
        // commit has to be named instead, and the diff it prints is the same rendering, header and caps
        // and all (mact-by1.11's no-TTY fallback).
        if let Some(rev) = &arguments.diff {
            let view = requested_diff(boot, config, rev, limits)?;
            crate::exit::print_data(&plain::diff(&view, &palette, plain::WIDTH))?;
            return Ok(Exit::Success);
        }
        // `--detail REV` is `enter` on a commit, asked on the command line and printed as text: all three
        // panes, every row, footers and caveats included (mact-by1.14's no-TTY fallback).
        if let Some(rev) = &arguments.detail {
            let sha = resolve_rev(boot, rev)?;
            match store_view::detail(boot, &sha) {
                Ok(snapshot) => crate::exit::print_data(&plain::detail(
                    &detail_view(snapshot),
                    &palette,
                    plain::WIDTH,
                ))?,
                // A refusal is one line, as every other plain-mode refusal is.
                Err(reason) => {
                    crate::exit::print_data(&format!("{}\n", reason.replace('\n', " ")))?;
                }
            }
            return Ok(Exit::Success);
        }
        // `--bench REV` is `b` for a terminal you cannot press `b` in, and it is the same rendering:
        // the panel is built by the same state machine the screen paints (mact-by1.12's no-TTY fallback).
        if let Some(rev) = &arguments.bench {
            let mut app = App::new(store_view::read(boot, arguments.limit), now);
            app.bench_band = config.benchmarks.regression_band_percent;
            let sha = resolve_rev(boot, rev)?;
            if app.show_bench(&sha).is_err() {
                crate::exit::print_data(&(app.status_line(&palette).replace('\n', " ") + "\n"))?;
                return Ok(Exit::Success);
            }
            if let Some(panel) = app.bench.as_ref() {
                crate::exit::print_data(&plain::bench(panel, &palette, plain::WIDTH))?;
            }
            return Ok(Exit::Success);
        }
        let reading = store_view::read(boot, arguments.limit);
        let mut app = App::new(reading, now);
        app.bench_band = config.benchmarks.regression_band_percent;
        crate::exit::print_data(&plain::table(&app, &palette, plain::WIDTH))?;
        return Ok(Exit::Success);
    }

    let palette = Palette::resolve(term.as_deref(), no_color, false, arguments.no_color);
    // The first read happens before the screen is taken over: it is what the very first frame shows, and
    // making the user stare at a blank alternate screen while a store is read would be its own bug.
    let reading = store_view::read(boot, arguments.limit);
    let mut app = App::new(reading, now);
    app.bench_band = config.benchmarks.regression_band_percent;
    if let Some(notice) = app.reading.notice.take() {
        app.message = Some(notice);
    }
    // `--bench REV` on an interactive run opens the panel `b` opens, on the commit the shell named.
    if let Some(rev) = &arguments.bench {
        let sha = resolve_rev(boot, rev)?;
        // A refusal is a status line, not a failed run: the timeline underneath is still the answer to
        // everything else the user asked for.
        if let Err(reason) = app.show_bench(&sha) {
            app.message = Some(reason);
        }
    }
    // `--detail REV` opens the same three panes `enter` opens, on the commit the shell named — read before
    // the screen is taken over, so the first frame is the answer and not a loading pane.
    if let Some(rev) = &arguments.detail {
        let sha = resolve_rev(boot, rev)?;
        match store_view::detail(boot, &sha) {
            Ok(snapshot) => app.show_detail(snapshot),
            Err(reason) => app.message = Some(reason),
        }
    }
    // `--diff REV` on an interactive run opens the diff view on that commit, read here for the same
    // reason the rows are read here: the first frame should already be the answer.
    if let Some(rev) = &arguments.diff {
        let view = requested_diff(boot, config, rev, limits)?;
        app.show_diff(view);
    }
    info!(
        "drawing {} rows for {}",
        app.reading.entries.len(),
        boot.workspace.display()
    );

    let terminal = ratatui::try_init().map_err(|reason| {
        // The refusal carries its own fix line, which is what `main` prints: a terminal too broken for
        // ratatui is not a reason to lose the table, only a reason to print it as text.
        crate::FathomError::Terminal(format!(
            "cannot take over this terminal: {reason}\n  fix: `fathom tui --plain` prints the same \
             table as text (and `fathom index --status` says what this build has indexed)"
        ))
    })?;
    let engine = Engine::start(Arc::new(StoreWork::new(
        boot.clone(),
        config.clone(),
        query,
        arguments.limit,
        limits,
    )));
    let mut driver = TerminalDriver::new(terminal, engine, palette);
    let outcome = run_loop(&mut app, &mut driver);
    // Restore first, then report: a stderr line printed while the alternate screen is still up is a line
    // the user never sees.
    ratatui::restore();
    outcome.map_err(|reason| {
        crate::FathomError::Terminal(format!(
            "the screen failed: {reason}\n  fix: `fathom tui --plain` prints the same table as text"
        ))
    })?;
    Ok(Exit::Success)
}

/// The store's own clock, exposed so a caller does not reach into `store` for one call.
#[must_use]
pub fn now() -> i64 {
    now_utc()
}

/// Whether a cache root is being indexed by anyone right now (§9 L3's advisory lock).
#[must_use]
pub fn index_in_progress(boot: &Boot) -> Option<store::lock::LockBody> {
    store::lock::read_body(&boot.cache.path)
}

/// A detail view that already holds its snapshot, for `--plain --detail REV`.
#[must_use]
fn detail_view(snapshot: self::detail::Snapshot) -> self::detail::Detail {
    let mut view = self::detail::Detail::read(snapshot);
    view.loading = false;
    view
}

/// Resolve a `--diff`/`--bench` revision to a full SHA, with the same refusal lines every other
/// git-facing door in this crate prints.
fn resolve_rev(boot: &Boot, rev: &str) -> Result<String, crate::FathomError> {
    let git = Git::open(&boot.workspace).map_err(patch::Error::Git)?;
    Ok(git.resolve(rev).map_err(patch::Error::Git)?)
}

/// Resolve `--diff REV` to a commit and read its patch before the screen exists.
///
/// Three `git` calls, bounded by the byte caps, and the same refusal lines every other git-facing door in
/// this crate prints: `fathom tui --diff nosuchrev` is a typed refusal with a `fix:`, not a screen with
/// an empty pane on it.
fn requested_diff(
    boot: &Boot,
    config: &Config,
    rev: &str,
    limits: patch::Limits,
) -> Result<DiffView, crate::FathomError> {
    let git = Git::open(&boot.workspace).map_err(patch::Error::Git)?;
    let sha = git.resolve(rev).map_err(patch::Error::Git)?;
    let mut view = DiffView::open(&sha);
    view.loading = false;
    match store_view::diff(boot, config, &sha, limits) {
        Ok(read) => {
            view.diff = Some(read);
            view.settle();
        }
        // One line: the status line is one line, and a `fix:` on a second row is a row of the screen
        // painted over with a sentence.
        Err(reason) => view.error = Some(reason.to_string().replace('\n', " ")),
    }
    Ok(view)
}
