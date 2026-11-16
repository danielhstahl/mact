//! The loop's two promises, measured: a keypress is never queued behind a slow source, and a burst of
//! records is never drawn one frame each.
//!
//! The ticket asks for both in the same breath — "typing and scrolling never block on git or tree
//! analysis" and "coalesce, cap redraw rate, never render a frame per record" — and they are normally
//! traded against each other, so they get two tests rather than one comment.
//!
//! The injected slow source is a [`Work`] stub, not a sleep in production code: [`SlowWork::read`] takes
//! as long as a test says it takes, and the assertion is about [`run_loop`] — it would read the same
//! against a real 40 000-commit walk, and it does not depend on how loaded the machine running the test
//! happens to be. Timings are therefore stated as *much less than*, never as an absolute number of
//! milliseconds: the assertion is a ratio (the key was answered in a fraction of one read), with a floor
//! generous enough that a loaded CI runner is not a flake.
//!
//! The frame half is counted rather than felt: [`run_loop`] returns how many frames it painted, so a loop
//! that painted one frame per event would be visible as a number.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fathom::tui::engine::{Engine, Event, Work, TICK_MS};
use fathom::tui::state::{App, Command, Key};
use fathom::tui::store_view::Reading;
use fathom::tui::{run_loop, Driver, Input};

/// How long the stub's `read` takes. Long enough that any keypress handled after the loop started is
/// unambiguously *not* waiting for it.
const SLOW_READ: Duration = Duration::from_millis(400);

/// A reading with `count` rows in it, so a movement key has somewhere to move.
fn reading_with(count: usize) -> Reading {
    let template = fathom::tui::entry::Entry {
        sha: String::new(),
        parent: None,
        author_utc: 0,
        committer_utc: 0,
        subject: String::new(),
        author: "Loop RS".to_owned(),
        coverage: fathom::tui::entry::Metric::UNKNOWN,
        churn: fathom::tui::entry::Churn::UNKNOWN,
        mi: fathom::tui::entry::Metric::UNKNOWN,
        unanalysed_paths: 0,
        bench: fathom::tui::entry::Bench::missing(None),
        action: fathom::metrics::vocab::ActionState::NoEvidence,
        indexed: true,
        caveats: Vec::new(),
    };
    let entries = (0..count)
        .map(|index| {
            let mut entry = template.clone();
            entry.sha = format!("{index:040x}");
            entry.subject = format!("commit {index}");
            entry
        })
        .collect();
    Reading {
        repo: "act-mcp".to_owned(),
        entries,
        ..Reading::default()
    }
}

/// A source that is slow on purpose, and honest about being cancelled.
struct SlowWork {
    read: Duration,
    /// How long a stub index run lasts if nothing cancels it.
    index: Duration,
    /// What `progress` reports while an index is running, so the header's counter has a source.
    ticks: std::sync::Mutex<u32>,
}

impl SlowWork {
    fn new() -> Self {
        Self {
            read: SLOW_READ,
            index: Duration::from_secs(5),
            ticks: std::sync::Mutex::new(0),
        }
    }
}

impl Work for SlowWork {
    fn read(&self) -> Reading {
        std::thread::sleep(self.read);
        Reading {
            repo: "slow".to_owned(),
            branch: "master".to_owned(),
            ..Reading::default()
        }
    }

    fn progress(&self) -> Option<(String, u64, u64)> {
        let mut ticks = self.ticks.lock().ok()?;
        *ticks += 1;
        Some(("rows".to_owned(), u64::from(*ticks), 1_000))
    }

    fn index(
        &self,
        cancel: Arc<AtomicBool>,
    ) -> Result<fathom::store::Run, fathom::tui::engine::WorkError> {
        // The same shape the real indexer has: check the flag at a commit boundary, and stop in one.
        let started = Instant::now();
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(fathom::tui::engine::WorkError::Cancelled);
            }
            if started.elapsed() >= self.index {
                return Ok(fathom::store::Run::default());
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

/// A driver with no terminal: keys from a queue, events from an engine, and a counter for the frames it
/// was asked to paint.
struct Scripted {
    engine: Option<Engine>,
    keys: VecDeque<Input>,
    /// Canned events, handed out once, in order, before anything the engine has.
    canned: VecDeque<Event>,
    paints: usize,
    /// When each `input` call returned, relative to `start`.
    answered: Vec<Duration>,
    start: Instant,
    /// Keep polling until the engine reports an index that ended, then quit.
    until_index_ends: bool,
    saw_index_end: bool,
    saw_cancel: bool,
    execute_commands: bool,
}

impl Scripted {
    fn new(keys: Vec<Key>) -> Self {
        Self {
            engine: None,
            keys: keys.into_iter().map(Input::Key).collect(),
            canned: VecDeque::new(),
            paints: 0,
            answered: Vec::new(),
            start: Instant::now(),
            until_index_ends: false,
            saw_index_end: false,
            saw_cancel: false,
            execute_commands: true,
        }
    }

    fn with_engine(mut self, engine: Engine) -> Self {
        self.engine = Some(engine);
        self
    }
}

impl Driver for Scripted {
    fn input(&mut self, timeout: Duration) -> Input {
        let _ = timeout;
        // The scripted keys first, in order: the loop must see the same presses a user would have made,
        // and only then fall back to whatever the test is waiting for.
        if let Some(input) = self.keys.pop_front() {
            self.answered.push(self.start.elapsed());
            return input;
        }
        if self.until_index_ends {
            if self.saw_index_end {
                return Input::Key(Key::Quit);
            }
            std::thread::sleep(Duration::from_millis(5));
            return Input::Timeout;
        }
        Input::Key(Key::Quit)
    }

    fn events(&mut self) -> Vec<Event> {
        let mut events: Vec<Event> = self.canned.drain(..).collect();
        if let Some(engine) = &self.engine {
            for event in engine.drain() {
                if matches!(
                    event,
                    Event::IndexFinished(_) | Event::IndexCancelled | Event::IndexFailed(_)
                ) {
                    self.saw_index_end = true;
                }
                if matches!(event, Event::IndexCancelled) {
                    self.saw_cancel = true;
                }
                events.push(event);
            }
        }
        events
    }

    fn execute(&mut self, commands: &[Command]) {
        if !self.execute_commands {
            return;
        }
        let Some(engine) = &self.engine else {
            return;
        };
        for command in commands {
            match command {
                Command::Read => engine.request_read(),
                Command::Index => engine.request_index(),
                Command::Cancel => engine.cancel(),
                Command::Quit => {}
            }
        }
    }

    fn paint(&mut self, _app: &App) -> std::io::Result<()> {
        self.paints += 1;
        Ok(())
    }

    fn size(&self) -> (u16, u16) {
        (100, 24)
    }
}

#[test]
fn a_keypress_is_answered_while_the_slow_source_is_still_reading() {
    let engine = Engine::start(Arc::new(SlowWork::new()));
    // The read is already in flight when the loop starts: that is the case the loop has to survive — a
    // first frame that is already stale, and a user who starts scrolling immediately.
    engine.request_read();
    let keys = std::iter::repeat_n(Key::Down, 40)
        .chain(std::iter::once(Key::Quit))
        .collect();
    let mut driver = Scripted::new(keys).with_engine(engine);
    let mut app = App::new(reading_with(500), 0);

    let started = Instant::now();
    let paints = run_loop(&mut app, &mut driver).expect("the scripted driver cannot fail to paint");
    let elapsed = started.elapsed();

    assert_eq!(app.selection, 40, "every keypress was handled");
    assert!(
        elapsed < SLOW_READ / 4,
        "40 keypresses took {elapsed:?} while one read was still sleeping {SLOW_READ:?} \
         (the last key was answered at {:?})",
        driver.answered.last()
    );
    assert!(
        elapsed < Duration::from_millis(TICK_MS * 40 + 100),
        "the loop never waited on the source: {elapsed:?} for 40 keys at a {TICK_MS} ms tick"
    );
    assert!(paints > 0, "and it still drew the screen");

    // The slow read was not skipped: it is on its way, and the screen will take it when it lands.
    let mut waited = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        waited.extend(driver.events());
        if waited.iter().any(|event| matches!(event, Event::Read(_))) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("the read never arrived: {} events", waited.len());
}

#[test]
fn a_burst_of_records_is_one_frame_not_one_hundred() {
    // 400 readings in one drain — what an index that published 400 rows produces, coalesced by the
    // engine into the handful of reads it actually did.
    let canned: Vec<Event> = (0..400)
        .map(|index| {
            Event::Read(Box::new(Reading {
                repo: format!("repo {index}"),
                ..Reading::default()
            }))
        })
        .collect();
    let mut driver = Scripted::new(vec![Key::Quit]);
    driver.canned = canned.into_iter().collect();
    let mut app = App::new(Reading::default(), 0);

    let paints = run_loop(&mut app, &mut driver).expect("painting a buffer cannot fail");
    assert_eq!(
        paints, 1,
        "400 events, one frame: the loop drains, marks dirty, and paints once"
    );
    assert_eq!(
        app.reading.repo, "repo 399",
        "and the last one is the screen's"
    );
}

#[test]
fn a_hundred_keypresses_do_not_become_a_hundred_frames() {
    let keys = std::iter::repeat_n(Key::Down, 100)
        .chain(std::iter::once(Key::Quit))
        .collect();
    let mut driver = Scripted::new(keys);
    let mut app = App::new(reading_with(500), 0);
    let paints = run_loop(&mut app, &mut driver).expect("painting a buffer cannot fail");
    assert_eq!(app.selection, 100, "every key moved the selection");
    assert!(
        paints <= 3,
        "100 keypresses in a tight loop are at most a couple of frames at a {} ms floor: {paints}",
        fathom::tui::FRAME_MS
    );
}

#[test]
fn an_idle_screen_paints_nothing_at_all() {
    // Nothing but timeouts: no event, no key, no resize. A screen that repainted on a timer would show
    // up here as a frame count nobody asked for.
    let mut driver = Scripted::new(Vec::new());
    driver.keys = std::iter::repeat_n(Input::Timeout, 200)
        .chain(std::iter::once(Input::Key(Key::Quit)))
        .collect();
    // Nothing to execute and nothing to wait for: the loop is being asked to do nothing, repeatedly.
    driver.execute_commands = false;
    let mut app = App::new(Reading::default(), 0);
    let paints = run_loop(&mut app, &mut driver).expect("painting a buffer cannot fail");
    assert_eq!(
        paints, 1,
        "the first frame, and nothing until something changed"
    );
}

#[test]
fn esc_stops_a_running_index_on_its_commit_boundary() {
    let engine = Engine::start(Arc::new(SlowWork::new()));
    let keys = vec![Key::Reindex, Key::Escape];
    let mut driver = Scripted::new(keys)
        .with_engine(engine)
        .with_until_index_ends();
    let mut app = App::new(Reading::default(), 0);

    let started = Instant::now();
    run_loop(&mut app, &mut driver).expect("the scripted driver cannot fail to paint");

    assert!(
        !app.index.is_running(),
        "the run is over as far as the screen is concerned"
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "esc landed on the stub's next boundary, not at the end of its 5 s: {:?}",
        started.elapsed()
    );
    assert!(
        driver.saw_cancel,
        "the engine reported a cancellation, not a completion"
    );
}

/// The same event, seen by the screen rather than by the thread that produced it: a cancelled run is a
/// *reported* run — the store is usable as it stands, and the screen says so and asks for a re-read.
#[test]
fn a_cancelled_index_is_reported_by_the_screen() {
    let mut app = App::new(reading_with(3), 0);
    app.on_key(Key::Reindex);
    assert!(app.index.is_running());
    let commands = app.apply(Event::IndexCancelled);
    assert_eq!(
        commands,
        vec![Command::Read],
        "the screen re-reads what the run left behind"
    );
    assert!(!app.index.is_running());
    assert!(
        app.message.unwrap_or_default().contains("cancel"),
        "and names what happened"
    );
}

impl Scripted {
    fn with_until_index_ends(mut self) -> Self {
        self.until_index_ends = true;
        self
    }
}

#[test]
fn a_resize_is_a_state_change_not_a_source_read() {
    let mut app = App::new(Reading::default(), 0);
    let mut driver = Scripted::new(vec![Key::Quit]);
    driver.keys.push_front(Input::Resized);
    driver.execute_commands = false;
    driver
        .canned
        .push_back(Event::Notice("a notice".to_owned()));
    let paints = run_loop(&mut app, &mut driver).expect("painting a buffer cannot fail");
    assert_eq!(
        (app.width, app.height),
        (100, 24),
        "the driver's size was taken"
    );
    assert_eq!(
        paints, 1,
        "the notice and the resize were coalesced into the one frame"
    );
}

/// A notice is the status line until the next keypress explains itself; that is the whole life of an
/// event message, and it is worth pinning because a status line that never expires is noise.
#[test]
fn an_event_notice_lives_until_the_next_key() {
    let mut app = App::new(reading_with(3), 0);
    assert!(app.apply(Event::Notice("a notice".to_owned())).is_empty());
    assert_eq!(app.message.as_deref(), Some("a notice"));
    app.on_key(Key::Down);
    assert_eq!(app.message, None, "the next key owns the line");
}
