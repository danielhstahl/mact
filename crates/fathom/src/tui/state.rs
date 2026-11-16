//! The screen's state and the keys that move it: no terminal, no clock, no files.
//!
//! [`App`] is the whole behaviour of the timeline and it is a pure state machine: a key in, a state
//! change plus a list of [`Command`]s out. That split is what makes the ticket's "input-handling unit
//! tests for each key" ordinary tests rather than terminal puppetry, and it is what keeps the drawing
//! code free of policy — `if` does not appear in [`super::render`] for anything a user can cause.
//!
//! Three decisions are worth naming because they are the ones a reader would otherwise re-litigate:
//!
//! * **Keys answer with commands, the driver executes them.** `R` does not index; it returns
//!   [`Command::Index`] and the loop hands it to the engine, so the loop (and a test) decides whether
//!   an index runs on a thread, now, or never.
//! * **Selection is over the filtered list**, and every movement clamps rather than wrapping. The
//!   ticket asks for "selection clamp at both ends": a timeline where `G` overshoots is a timeline that
//!   draws an empty row.
//! * **Nothing in here can block.** No git, no store, no `SystemTime` read that matters — the state
//!   machine's idea of time is the `now` the caller passes in, which is also what makes the `age`
//!   column testable to the second.

use crate::metrics::Model;

use super::diff_view::{DiffView, Screen};
use super::engine::Event;
use super::entry::Entry;
use super::store_view::{is_stale, Reading, StoreState};

/// How many commits' diff views are kept open at once.
///
/// A screen you back out of and into again should remember where you were, and a cache of every commit
/// ever looked at in a 40 000-commit repository is a megabyte heap with no ceiling. Twelve is "the
/// commits you are comparing", which is what anyone is actually doing, and the least recently opened
/// one goes when the thirteenth arrives.
const DIFF_CACHE: usize = 12;

/// A key, already reduced to what the timeline means by it.
///
/// The mapping from a `KeyEvent` lives in [`super::keys`]; this enum is the ticket's key surface and
/// nothing else, which is how "anything not listed here is not in this ticket" stays checkable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// `j` or `↓`.
    Down,
    /// `k` or `↑`.
    Up,
    /// `PgDn`.
    PageDown,
    /// `PgUp`.
    PageUp,
    /// `g` or `Home`.
    Top,
    /// `G` or `End`.
    Bottom,
    /// `q` or Ctrl-C.
    Quit,
    /// `?`.
    Help,
    /// `/`.
    Filter,
    /// `R`.
    Reindex,
    /// `Esc`: close help, cancel a running index, or clear the filter — in that order.
    Escape,
    /// `d`.
    Diff,
    /// `enter`.
    Detail,
    /// `b`.
    Bench,
    /// `n` or `Tab`: the next file in the diff view.
    NextFile,
    /// `p` or `Shift-Tab`: the previous file in the diff view.
    PrevFile,
    /// `t`: show the files the path filters hide, and the totals that go with them.
    ToggleFiles,
    /// `backspace`, which means "delete a filter character" while the filter is open.
    Backspace,
    /// A printable character, which means "append to the filter" while the filter is open.
    Text(char),
    /// Anything else, deliberately not a no-op-shaped hole: unknown keys are ignored on purpose.
    Ignored,
}

/// What the loop is told to do next. The state machine never does these itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Read the store again (the first frame, and after an index).
    Read,
    /// Start an index run.
    Index,
    /// Ask a running index to stop.
    Cancel,
    /// Read one commit's patch ([`crate::diff`]) — the `d` key, and nothing else asks for it.
    Diff(String),
    /// Leave the alternate screen.
    Quit,
}

/// Which end `g`/`G` (or `Home`/`End`) jumps to. A private helper for [`App::jump`], spelled as a
/// type so the two call sites cannot be transposed.
#[derive(Debug, Clone, Copy)]
enum Jump {
    /// The newest commit.
    Top,
    /// The oldest commit in the window.
    Bottom,
}

/// Where the index is, as far as this screen knows.
///
/// `Running` carries what the lock file and the progress callback reported (§9 L3), which is what the
/// header prints while a run is going on — including a run this screen did not start, because a lock
/// body is readable by any fathom on the same cache root.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum IndexState {
    /// Nothing running.
    #[default]
    Idle,
    /// A run in progress.
    Running {
        /// Which phase (`walk`, `rows`, `prune`).
        phase: String,
        /// Commits done so far.
        done: u64,
        /// Commits in the walk, when known.
        total: u64,
        /// Whether this screen asked for it (`false` = another fathom's run).
        ours: bool,
    },
}

impl IndexState {
    /// Whether a run is going on.
    #[must_use]
    pub const fn is_running(&self) -> bool {
        matches!(self, Self::Running { .. })
    }
}

/// The whole screen state.
#[derive(Debug, Clone)]
pub struct App {
    /// The last reading this screen was given.
    pub reading: Reading,
    /// Where the index is.
    pub index: IndexState,
    /// Selected row *within* [`App::visible`].
    pub selection: usize,
    /// First visible row drawn — kept here rather than in the widget so scrolling is testable.
    pub offset: usize,
    /// The `/` filter text while the prompt is open, `None` when it is closed.
    pub filter_prompt: Option<String>,
    /// The filter in force, applied to every rendering. Distinct from the prompt so Esc can abort an
    /// edit without losing the filter that was already working.
    pub filter: Option<String>,
    /// Whether the `?` overlay is up.
    pub help: bool,
    /// The one-line message under the table (a refusal, a notice, a "not wired yet" line).
    pub message: Option<String>,
    /// Rows the table may draw, which sets a page for PgUp/PgDn.
    pub height: u16,
    /// Columns the table may draw.
    pub width: u16,
    /// The clock the age column counts back from, supplied by the caller each frame.
    pub now: i64,
    /// Whether the loop should stop.
    pub quit: bool,
    /// Which screen is up: the timeline, or the diff of one named commit.
    pub screen: Screen,
    /// Every diff view this run has opened, by full SHA, so backing out and in keeps the reader's place.
    pub diffs: std::collections::BTreeMap<String, DiffView>,
    /// A counter, not a clock: it orders the diff views for eviction without reading time (rule: nothing
    /// in here can block, and nothing here tells the time).
    touches: u64,
    /// The metric model, parsed once per screen rather than once per status line: the caveat line
    /// needs the per-column code lists, and re-parsing `schema/metric-model.json` to explain a `*`
    /// would be the same mistake [`super::palette`] names for the glyphs.
    model: Model,
    visible: Vec<usize>,
}

impl App {
    /// A screen over one reading, selection parked at the top.
    #[must_use]
    pub fn new(reading: Reading, now: i64) -> Self {
        let mut app = Self {
            reading,
            index: IndexState::Idle,
            selection: 0,
            offset: 0,
            filter_prompt: None,
            filter: None,
            help: false,
            message: None,
            height: 24,
            width: 80,
            now,
            quit: false,
            screen: Screen::Timeline,
            diffs: std::collections::BTreeMap::new(),
            touches: 0,
            model: Model::embedded(),
            visible: Vec::new(),
        };
        app.recompute();
        app
    }

    /// The size the table may use, from a frame's inner area.
    pub fn set_size(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
        self.clamp_view();
        // An open diff view has its own pane to fit; the scroll offsets clamp against it.
        if let Some(view) = self.diff_view_mut() {
            let pane = super::diff_view::pane_height(height);
            view.set_height(pane);
        }
    }

    /// The open diff view, if the diff screen is up.
    #[must_use]
    pub fn diff_view(&self) -> Option<&DiffView> {
        match &self.screen {
            Screen::Timeline => None,
            Screen::Diff(sha) => self.diffs.get(sha),
        }
    }

    /// The open diff view, mutably.
    pub fn diff_view_mut(&mut self) -> Option<&mut DiffView> {
        match &self.screen {
            Screen::Timeline => None,
            Screen::Diff(sha) => self.diffs.get_mut(sha),
        }
    }

    /// Open the diff of one commit. Returns the commands the loop must run — one [`Command::Diff`] the
    /// first time a commit is opened and nothing at all after that, because the view is cached.
    ///
    /// The timeline's selection is not touched, in either direction: `d` out of a diff lands on the row
    /// `d` left from, which is what makes "peek at the diff" a safe thing to press.
    pub fn open_diff(&mut self, sha: &str) -> Vec<Command> {
        self.touches += 1;
        let touch = self.touches;
        let mut commands = Vec::new();
        if let Some(view) = self.diffs.get_mut(sha) {
            view.touched = touch;
        } else {
            let mut view = DiffView::open(sha);
            view.set_height(super::diff_view::pane_height(self.height));
            self.diffs.insert(sha.to_owned(), view);
            commands.push(Command::Diff(sha.to_owned()));
            self.evict_diffs(sha);
        }
        self.screen = Screen::Diff(sha.to_owned());
        commands
    }

    /// Leave the diff view. The view stays cached, which is the whole point of the cache.
    pub fn close_diff(&mut self) {
        self.screen = Screen::Timeline;
    }

    /// Show a diff view that has already been read — `--diff REV` on the command line, which opens the
    /// same screen `d` opens without the read `d` would have asked the engine for.
    pub fn show_diff(&mut self, mut view: DiffView) {
        self.touches += 1;
        view.touched = self.touches;
        view.set_height(super::diff_view::pane_height(self.height));
        let sha = view.sha.clone();
        self.diffs.insert(sha.clone(), view);
        self.evict_diffs(&sha);
        self.screen = Screen::Diff(sha);
    }

    /// Forget the least recently opened diff views once the cache is over its bound.
    fn evict_diffs(&mut self, keep: &str) {
        if self.diffs.len() <= DIFF_CACHE {
            return;
        }
        let mut oldest: Option<(u64, String)> = None;
        for (sha, view) in &self.diffs {
            if sha == keep {
                continue;
            }
            if oldest
                .as_ref()
                .is_none_or(|(touch, _)| view.touched < *touch)
            {
                oldest = Some((view.touched, sha.clone()));
            }
        }
        if let Some((_, sha)) = oldest {
            self.diffs.remove(&sha);
        }
    }

    /// The rows on show: the filter's answer over `reading.entries`, in the reading's order.
    #[must_use]
    pub fn visible(&self) -> &[usize] {
        &self.visible
    }

    /// The selected entry, if there is one.
    #[must_use]
    pub fn selected(&self) -> Option<&Entry> {
        self.visible
            .get(self.selection)
            .and_then(|index| self.reading.entries.get(*index))
    }

    /// Re-derive `visible` after the filter or the entries changed, then keep the view legal.
    fn recompute(&mut self) {
        let needle = self
            .filter_prompt
            .as_ref()
            .or(self.filter.as_ref())
            .map(|text| text.to_lowercase())
            .filter(|text| !text.is_empty());
        self.visible = match needle {
            None => (0..self.reading.entries.len()).collect(),
            Some(needle) => (0..self.reading.entries.len())
                .filter(|index| {
                    self.reading
                        .entries
                        .get(*index)
                        .is_some_and(|entry| entry.haystack().contains(&needle))
                })
                .collect(),
        };
        self.clamp_view();
    }

    /// Keep `selection` and `offset` inside the row list and the viewport.
    fn clamp_view(&mut self) {
        if self.visible.is_empty() {
            self.selection = 0;
            self.offset = 0;
            return;
        }
        if self.selection >= self.visible.len() {
            self.selection = self.visible.len() - 1;
        }
        let page = usize::from(self.height.max(1));
        if self.offset + page > self.visible.len() {
            self.offset = self.visible.len().saturating_sub(page);
        }
        if self.selection < self.offset {
            self.offset = self.selection;
        }
        if self.selection >= self.offset + page {
            self.offset = self.selection + 1 - page;
        }
    }

    /// The page a PgUp/PgDn moves by: one screen of rows, always at least one.
    #[must_use]
    pub fn page(&self) -> usize {
        usize::from(self.height.max(1)).max(1)
    }

    fn move_selection(&mut self, step: isize) {
        if self.visible.is_empty() {
            return;
        }
        let last = self.visible.len() - 1;
        self.selection = if step < 0 {
            self.selection
                .saturating_sub(usize::try_from(-step).unwrap_or(usize::MAX))
        } else {
            self.selection
                .saturating_add(usize::try_from(step).unwrap_or(usize::MAX))
                .min(last)
        };
        self.clamp_view();
    }

    fn jump(&mut self, to: Jump) {
        if self.visible.is_empty() {
            return;
        }
        self.selection = match to {
            Jump::Top => 0,
            Jump::Bottom => self.visible.len() - 1,
        };
        self.clamp_view();
    }

    /// Apply a key. The returned commands are everything the loop must go and do.
    pub fn on_key(&mut self, key: Key) -> Vec<Command> {
        // A message is a one-key press: it explains the key that caused it and then gets out of the
        // way, so the status line never shows a stale answer to an earlier question.
        self.message = None;

        // The help overlay swallows Esc on either screen; every other key still works, because a reader
        // who left `?` up and wants to scroll should be allowed to.
        if self.help && key == Key::Escape {
            self.help = false;
            return Vec::new();
        }

        let open = match &self.screen {
            Screen::Timeline => None,
            Screen::Diff(sha) => Some(sha.clone()),
        };
        match open {
            None => self.on_timeline_key(key),
            Some(sha) => self.on_diff_key(&sha, key),
        }
    }

    /// The timeline's answer to a key, including the `/` prompt that owns printable keys first.
    fn on_timeline_key(&mut self, key: Key) -> Vec<Command> {
        let mut commands = Vec::new();

        // The filter prompt owns text keys and Backspace, and Esc/Enter close it. A letter that means
        // something else on this screen is, while the prompt is open, a needle character: `/registry`
        // has to be typeable.
        if self.filter_prompt.is_some() {
            match key {
                Key::Text(character) => {
                    if let Some(prompt) = self.filter_prompt.as_mut() {
                        prompt.push(character);
                    }
                    self.recompute();
                    return commands;
                }
                Key::Backspace => {
                    if let Some(prompt) = self.filter_prompt.as_mut() {
                        prompt.pop();
                    }
                    self.recompute();
                    return commands;
                }
                Key::Detail => {
                    // `enter` accepts what is typed. An empty filter means "no filter".
                    let text = self.filter_prompt.take().unwrap_or_default();
                    self.filter = if text.is_empty() { None } else { Some(text) };
                    self.recompute();
                    return commands;
                }
                Key::Escape => {
                    self.filter_prompt = None;
                    self.recompute();
                    return commands;
                }
                _ => {}
            }
        }

        match timeline_letter(key) {
            Key::Down => self.move_selection(1),
            Key::Up => self.move_selection(-1),
            Key::PageDown => {
                self.move_selection(isize::try_from(self.page()).unwrap_or(isize::MAX))
            }
            Key::PageUp => self.move_selection(-isize::try_from(self.page()).unwrap_or(isize::MAX)),
            Key::Top => self.jump(Jump::Top),
            Key::Bottom => self.jump(Jump::Bottom),
            Key::Quit => {
                self.quit = true;
                commands.push(Command::Quit);
            }
            Key::Help => self.help = !self.help,
            Key::Filter => {
                // Re-open with the live filter text so `/` edits rather than replaces.
                self.filter_prompt = Some(self.filter.clone().unwrap_or_default());
            }
            Key::Reindex => {
                if self.index.is_running() {
                    self.message = Some("an index is already running — Esc cancels it".to_owned());
                } else {
                    self.index = IndexState::Running {
                        phase: "starting".to_owned(),
                        done: 0,
                        total: 0,
                        ours: true,
                    };
                    commands.push(Command::Index);
                }
            }
            Key::Escape => {
                if self.index.is_running() {
                    commands.push(Command::Cancel);
                    self.message = Some("cancelling the index…".to_owned());
                } else if self.filter.is_some() {
                    self.filter = None;
                    self.recompute();
                }
            }
            // The ticket's one named interaction. The read is not done here: `Command::Diff` goes to the
            // engine, so pressing `d` on a commit whose patch takes a moment still cannot delay a key.
            Key::Diff => {
                let selected = self.selected().map(|entry| entry.sha.clone());
                match selected {
                    Some(sha) => commands.extend(self.open_diff(&sha)),
                    None => {
                        self.message =
                            Some("nothing is selected, so there is nothing to diff".to_owned());
                    }
                }
            }
            Key::Detail => self.message = Some(sibling("detail", "mact-by1.14")),
            Key::Bench => self.message = Some(sibling("bench", "mact-by1.12")),
            // `n`/`p`/`t` are the diff view's keys. On the timeline they are not nothing, exactly: they
            // are keys the reader pressed a screen too early, and the answer says which screen wants them.
            Key::NextFile | Key::PrevFile => {
                self.message =
                    Some("n/p choose a file inside a diff \u{b7} d opens one".to_owned());
            }
            Key::ToggleFiles => {
                self.message =
                    Some("t shows the files a diff is hiding \u{b7} d opens one first".to_owned());
            }
            Key::Backspace | Key::Text(_) | Key::Ignored => {}
        }
        commands
    }

    /// The diff view's answer to a key. `d` and `Esc` both leave; nothing here can move the timeline's
    /// selection, which is the promise the round trip is judged on.
    fn on_diff_key(&mut self, sha: &str, key: Key) -> Vec<Command> {
        let mut commands = Vec::new();
        if let Some(view) = self.diffs.get_mut(sha) {
            self.touches += 1;
            view.touched = self.touches;
        }
        // A page on this screen is a pane-full of patch text, not a screen-full: the frame's other four
        // rows are the heading, the status line, the key line and the pane borders, and paging by the
        // frame's height would skip the lines that fell off the bottom of the pane.
        let page =
            isize::try_from(usize::from(super::diff_view::pane_height(self.height))).unwrap_or(1);
        match diff_letter(key) {
            Key::Quit => {
                self.quit = true;
                commands.push(Command::Quit);
            }
            Key::Escape | Key::Diff => self.close_diff(),
            Key::Help => self.help = !self.help,
            Key::Down => self.scroll_diff(1),
            Key::Up => self.scroll_diff(-1),
            Key::PageDown => self.scroll_diff(page),
            Key::PageUp => self.scroll_diff(-page),
            Key::Top => {
                if let Some(view) = self.diff_view_mut() {
                    view.scroll_top();
                }
            }
            Key::Bottom => {
                if let Some(view) = self.diff_view_mut() {
                    view.scroll_bottom();
                }
            }
            Key::NextFile => self.cycle_diff_file(1),
            Key::PrevFile => self.cycle_diff_file(-1),
            Key::ToggleFiles => {
                if let Some(view) = self.diff_view_mut() {
                    view.set_show_all(!view.show_all);
                }
            }
            // The mock's own words: `/ search (later)`. The honest answer is that it is not here yet,
            // and the ticket that says so is named so the key does not read as broken.
            Key::Filter => {
                self.message = Some(
                    "search inside a diff is a later ticket \u{b7} / filters the timeline"
                        .to_owned(),
                );
            }
            Key::Detail => self.message = Some(sibling("detail", "mact-by1.14")),
            Key::Bench => self.message = Some(sibling("bench", "mact-by1.12")),
            Key::Reindex => {
                self.message =
                    Some("re-index is the timeline's key \u{b7} Esc goes back to it".to_owned());
            }
            Key::Backspace | Key::Text(_) | Key::Ignored => {}
        }
        commands
    }

    /// Scroll the open diff view, if one is open.
    fn scroll_diff(&mut self, delta: isize) {
        if let Some(view) = self.diff_view_mut() {
            view.scroll_by(delta);
        }
    }

    /// Move to the next or previous file in the open diff view.
    fn cycle_diff_file(&mut self, step: isize) {
        if let Some(view) = self.diff_view_mut() {
            view.cycle_file(step);
        }
    }

    /// Apply an event from the engine. Returns commands the loop must act on.
    pub fn apply(&mut self, event: Event) -> Vec<Command> {
        match event {
            Event::Read(reading) => {
                // Keep the selection's commit rather than its row number: a re-read that grew the list
                // at the top must not move the cursor out from under the reader.
                let pinned = self.selected().map(|entry| entry.sha.clone());
                self.reading = *reading;
                self.recompute();
                if let Some(sha) = pinned {
                    if let Some(index) = self
                        .visible()
                        .iter()
                        .find(|index| self.reading.entries[**index].sha == sha)
                        .copied()
                    {
                        self.selection = index;
                        self.clamp_view();
                    }
                }
                Vec::new()
            }
            Event::IndexStarted => {
                if !self.index.is_running() {
                    self.index = IndexState::Running {
                        phase: "walk".to_owned(),
                        done: 0,
                        total: 0,
                        ours: false,
                    };
                }
                Vec::new()
            }
            Event::IndexProgress { phase, done, total } => {
                let ours = matches!(&self.index, IndexState::Running { ours, .. } if *ours);
                self.index = IndexState::Running {
                    phase,
                    done,
                    total,
                    ours,
                };
                Vec::new()
            }
            Event::IndexFinished(run) => {
                self.index = IndexState::Idle;
                self.message = Some(format!(
                    "{} index: {} rows written, {} commits seen, in {}",
                    run.kind,
                    run.rows_written,
                    run.commits_seen,
                    duration(run.duration_ms)
                ));
                vec![Command::Read]
            }
            Event::IndexFailed(reason) => {
                self.index = IndexState::Idle;
                self.message = Some(reason);
                vec![Command::Read]
            }
            Event::IndexCancelled => {
                self.index = IndexState::Idle;
                self.message =
                    Some("index cancelled — the store is usable as it stands".to_owned());
                vec![Command::Read]
            }
            Event::Notice(line) => {
                self.message = Some(line);
                Vec::new()
            }
            Event::Diff { sha, outcome } => {
                // The view may have been evicted while the read was running (twelve is the bound); then
                // the answer is simply no longer anyone's question, and the text is dropped.
                let Some(view) = self.diffs.get_mut(&sha) else {
                    return Vec::new();
                };
                view.loading = false;
                match outcome {
                    Ok(diff) => {
                        view.diff = Some(*diff);
                        view.settle();
                    }
                    Err(reason) => view.error = Some(reason),
                }
                Vec::new()
            }
        }
    }

    /// The filter's live count, as the header prints it (`3/412`).
    #[must_use]
    pub fn filter_count(&self) -> String {
        format!("{}/{}", self.visible.len(), self.reading.entries.len())
    }

    /// Whether a filter is doing anything right now.
    #[must_use]
    pub fn filtering(&self) -> bool {
        self.filter_prompt.is_some() || self.filter.is_some()
    }

    /// The left half of the header: `FATHOM  act-mcp  main@9f31a2c`.
    #[must_use]
    pub fn headline(&self) -> String {
        let head = self
            .reading
            .head
            .as_deref()
            .map(|sha| sha.chars().take(7).collect::<String>())
            .unwrap_or_else(|| "—".to_owned());
        format!(
            "FATHOM  {}  {}@{}",
            self.reading.repo, self.reading.branch, head
        )
    }

    /// The right half of the header: what is indexed, how long that took, and whether the evidence is
    /// there at all — plus the live index marker, which is the difference between "this is everything"
    /// and "this is what exists so far".
    #[must_use]
    pub fn facts(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        match &self.reading.store {
            StoreState::Present { rows } => {
                // A `--limit` cut is said out loud: 5 rows on screen and 412 in the store is the
                // difference between a window and a lie.
                parts.push(if self.reading.hidden == 0 {
                    format!("indexed {rows} commits")
                } else {
                    format!(
                        "indexed {} of {rows} commits (--limit)",
                        self.reading.entries.len()
                    )
                });
            }
            StoreState::Absent => parts.push("not indexed".to_owned()),
            StoreState::Refused(_) => parts.push("store refused".to_owned()),
        }
        if let Some(run) = &self.reading.last_run {
            parts.push(duration(run.duration_ms));
            if run.status != "ok" {
                parts.push("last run errored".to_owned());
            }
        }
        parts.push(format!(
            "evidence {}",
            if self.reading.evidence_present {
                "✓"
            } else {
                "—"
            }
        ));
        if is_stale(&self.reading) {
            parts.push("stale".to_owned());
        }
        if let IndexState::Running {
            phase, done, total, ..
        } = &self.index
        {
            let counter = if *total > 0 {
                format!("{done}/{total}")
            } else {
                done.to_string()
            };
            parts.push(format!("indexing… {phase} {counter}"));
        }
        if self.filtering() {
            parts.push(format!(
                "filter /{} {}",
                self.filter_text(),
                self.filter_count()
            ));
        }
        parts.join(" · ")
    }

    fn filter_text(&self) -> String {
        self.filter_prompt
            .clone()
            .or_else(|| self.filter.clone())
            .unwrap_or_default()
    }

    /// The line under the table: a message wins, then the live filter prompt, then the selected row's
    /// caveats, then the epic-wide hint ("nothing here has evidence"), then nothing.
    ///
    /// On the diff screen the view's own status line answers instead — the caps, the hidden files, the
    /// refusal — because "why is this pane empty" is the question under a diff.
    #[must_use]
    pub fn status_line(&self) -> String {
        if let Some(message) = &self.message {
            return message.clone();
        }
        match &self.screen {
            Screen::Timeline => self.timeline_status(),
            Screen::Diff(_) => self.diff_view().map_or_else(String::new, DiffView::status),
        }
    }

    /// The timeline's own status line, once the diff view's is not in the way.
    #[must_use]
    fn timeline_status(&self) -> String {
        if let Some(prompt) = &self.filter_prompt {
            return format!("filter: /{prompt}  enter accepts \u{b7} esc discards");
        }
        let line = self
            .selected()
            .map(|entry| super::cells::caveat_line(entry, &self.model))
            .unwrap_or_default();
        if !line.is_empty() {
            return line;
        }
        self.hint_line().unwrap_or_default()
    }

    /// The one line worth saying about the whole set: no commit in it has any evidence.
    ///
    /// This is the ticket's "none of the commits have evidence" state, which is a *populated* table of
    /// `—` rows rather than an empty one, so it belongs on the status line and not in the empty-state
    /// block. It names the tool and the call to make, because "no data" without a next step is the
    /// reason people stop trusting dashboards.
    #[must_use]
    pub fn hint_line(&self) -> Option<String> {
        if self.visible.is_empty() {
            return None;
        }
        if self
            .visible()
            .iter()
            .all(|index| self.reading.entries[*index].all_unknown())
        {
            return Some(format!(
                "none of the {} commits shown has evidence in {} \u{b7} start `act-mcp serve` and call \
                 `run_workflow` for these commits (`list_tasks` names the workflow and task), then press \
                 R",
                self.visible.len(),
                self.reading.evidence_path.display()
            ));
        }
        None
    }

    /// What the screen says when there is nothing to draw, and what the user can do about it.
    ///
    /// Every branch names a key or a command: a blank table is a dead end, and the ticket's line about
    /// "each shows the actionable next step" is the point of this function rather than a decoration.
    #[must_use]
    pub fn next_step(&self) -> Vec<String> {
        if let StoreState::Refused(reason) = &self.reading.store {
            return vec![
                "the metric store refused to open:".to_owned(),
                format!("  {reason}"),
            ];
        }
        if self.visible.is_empty() && self.filtering() {
            // A filter matched nothing. Say the needle back, say how to undo it, and say how far off it
            // is: "no matches" without the count is the message that makes people quit a tool.
            let needle = self.filter_text();
            return vec![
                format!("no commit matches \u{201c}{needle}\u{201d} \u{b7} esc clears the filter"),
                format!(
                    "  {} commits hidden, {} shown",
                    self.reading.entries.len(),
                    self.visible.len()
                ),
            ];
        }
        let mut lines: Vec<String> = Vec::new();
        if self.reading.head.is_none() {
            lines.push("this repository has no commits yet".to_owned());
            lines.push("  commit something, then press R to index it".to_owned());
            return lines;
        }
        match &self.reading.store {
            StoreState::Absent => {
                lines.push("nothing is indexed in this workspace yet".to_owned());
            }
            StoreState::Refused(_) => unreachable!("handled above"),
            StoreState::Present { .. } => {
                lines.push("the store has no rows for this window".to_owned());
            }
        }
        lines.push(
            "  press R to build the store, or run `fathom index` in another terminal".to_owned(),
        );
        if !self.reading.evidence_present {
            lines.push(format!(
                "  and there is no evidence root at {} \u{b7} start the server (`act-mcp serve`) and \
                 call `run_workflow` for these commits; `list_tasks` names the workflow and task to \
                 ask for",
                self.reading.evidence_path.display()
            ));
        }
        lines
    }
}

fn sibling(view: &str, ticket: &str) -> String {
    format!("the {view} view is {ticket} — same epic, not wired yet; it is not lost, it is next")
}

/// What the timeline means by a typed character ([`Key::Text`]), and what it leaves alone.
///
/// This is the second half of [`super::keys::classify`]'s job, and it lives with the screen that has the
/// other half: the `/` prompt has already had its chance at the character by the time this runs, so a
/// filter can contain every letter this screen also uses as a command.
fn timeline_letter(key: Key) -> Key {
    match key {
        Key::Text(character) => match character {
            'j' => Key::Down,
            'k' => Key::Up,
            'g' => Key::Top,
            // `G` is shift+g: `g` alone jumps to the top, which is the vim pair the ticket names.
            'G' => Key::Bottom,
            'q' => Key::Quit,
            '?' => Key::Help,
            '/' => Key::Filter,
            'R' => Key::Reindex,
            'd' => Key::Diff,
            'b' => Key::Bench,
            other => Key::Text(other),
        },
        other => other,
    }
}

/// What the diff view means by a typed character. `d` is the way out, which is why it is here at all.
fn diff_letter(key: Key) -> Key {
    match key {
        Key::Text(character) => match character {
            'j' => Key::Down,
            'k' => Key::Up,
            'g' => Key::Top,
            'G' => Key::Bottom,
            'q' => Key::Quit,
            '?' => Key::Help,
            '/' => Key::Filter,
            'd' => Key::Diff,
            'n' => Key::NextFile,
            'p' => Key::PrevFile,
            't' => Key::ToggleFiles,
            'b' => Key::Bench,
            'R' => Key::Reindex,
            other => Key::Text(other),
        },
        other => other,
    }
}

fn duration(millis: u64) -> String {
    if millis < 1_000 {
        return format!("{millis} ms");
    }
    let seconds = millis / 1_000;
    if seconds < 60 {
        return format!("{seconds} s");
    }
    format!("{}m {:02}s", seconds / 60, seconds % 60)
}

/// The 7-bit twin of a prose line, for `--plain` and `TERM=dumb`.
///
/// The *cells* are already ascii: they come from the glyph table ([`super::palette`], ADR-0008 §6.1's
/// second column). What this rewrites is the prose — the header, the next-step lines, the status line —
/// which is written once, in the TUI's own separators (`·`, `“”`, `…`, `—`, `✓`). A CI log or a phone
/// ssh that cannot show `✓` still needs to know whether the evidence root is there, so the mark is
/// rewritten as the word rather than dropped: the plain surface says strictly more than a box of `?`.
#[must_use]
pub fn ascii_line(text: &str) -> String {
    text.replace('·', "|")
        .replace('…', "...")
        .replace(['\u{201c}', '\u{201d}'], "\"")
        .replace('\u{2014}', "-")
        .replace('\u{2713}', "yes")
        .replace('\u{2717}', "no")
}
