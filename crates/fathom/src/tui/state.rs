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
//! * **The screen asks to index itself, but never indexes.** A reading whose `HEAD` has no row is a
//!   picture of the wrong commit graph, so [`App::startup`] and every [`Event::Read`] answer with
//!   [`Command::Index`] as well — the same command `R` returns, so the auto path and the key path are
//!   one path, and a test can still decide that no index runs. [`App::auto_index`] is the switch, and
//!   the SHA last auto-indexed is remembered so a store that *cannot* catch up (a `--to` behind `HEAD`,
//!   a read-only root that slipped through) indexes once and then only reports.
//! * **Nothing in here can block.** No git, no store, no `SystemTime` read that matters — the state
//!   machine's idea of time is the `now` the caller passes in, which is also what makes the `age`
//!   column testable to the second.

use crate::metrics::Model;

use super::bench::{self, Commit, Evidence, Panel};
use super::detail::{self, Detail, Snapshot};
use super::diff_view::{DiffView, Screen};
use super::engine::Event;
use super::entry::Entry;
use super::palette::Palette;
use super::store_view::{is_stale, Reading, StoreState};

/// How many commits' diff views are kept open at once.
///
/// A screen you back out of and into again should remember where you were, and a cache of every commit
/// ever looked at in a 40 000-commit repository is a megabyte heap with no ceiling. Twelve is "the
/// commits you are comparing", which is what anyone is actually doing, and the least recently opened
/// one goes when the thirteenth arrives.
const DIFF_CACHE: usize = 12;

/// How many detail snapshots are kept, for the same reason and the same bound as [`DIFF_CACHE`].
const DETAIL_CACHE: usize = 6;

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
    /// `s` in the detail view: the focused pane's next sort order.
    SortCycle,
    /// `b`.
    Bench,
    /// `f`: show the runs retention excluded, and the reason each one was.
    ExcludedRuns,
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
    /// Ask whether `HEAD` moved, and re-read only if it did — the loop's refresh tick
    /// ([`crate::tui::REFRESH_MS`]), never a key, and cheaper than [`Command::Read`] by design.
    Check,
    /// Start an index run.
    Index,
    /// Ask a running index to stop.
    Cancel,
    /// Read one commit's patch ([`crate::diff`]) — the `d` key, and nothing else asks for it.
    Diff(String),
    /// Read one commit's detail snapshot ([`super::store_view::detail`]) — the `enter` key.
    Detail(String),
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
    /// Which screen is up: the timeline, the diff of one named commit, or a commit's bench panel.
    pub screen: Screen,
    /// Whether this screen may ask for an index without being asked (the `--no-auto-index` off switch).
    ///
    /// Off by default here, because the state machine does not decide policy for its own sake: the
    /// caller (`crate::tui::run`) turns it on for the interactive screen when the cache root is
    /// writable, and leaves it off for `--plain`, for a pipe and for every test that never meant to
    /// index. A read-only workspace cannot index, so nothing is lost by asking.
    pub auto_index: bool,
    /// The `HEAD` this screen has already auto-indexed, so one new commit costs one run.
    ///
    /// A counter's job, not a clock's: it is the guard against the loop where an index cannot make the
    /// store stop being stale (`--to HEAD~5`, a commit outside the walk, a refused write) and every
    /// re-read therefore asks for another run. `R` deliberately does not consult or set it — a key
    /// typed twice means it should run twice.
    auto_indexed: Option<String>,
    /// The bench panel, if one is open.
    ///
    /// One panel rather than a cache of them, because building it costs nothing: every number in it came
    /// off the reading this screen already holds, so re-opening a commit re-derives its rows in a few
    /// hundred field reads instead of paying for a map and an eviction rule. What the reader was doing on
    /// the panel — which row, whether the overlay was up — is kept for the one commit worth keeping,
    /// which is the one they are still looking at.
    pub bench: Option<Panel>,
    /// The declared regression band, percent of value (`benchmarks.regression_band_percent`).
    ///
    /// A view policy, so it is passed in rather than read here: the panel prints it in a column header,
    /// and a band nobody can see is a band nobody can interpret.
    pub bench_band: f64,
    /// Every diff view this run has opened, by full SHA, so backing out and in keeps the reader's place.
    pub diffs: std::collections::BTreeMap<String, DiffView>,
    /// Every detail view this run has opened, by full SHA (mact-by1.14).
    ///
    /// Cached like the diff views because a snapshot costs a row read, a parent read and one block
    /// read per touched file: cheap once, pointless twice, and a screen that re-reads on every `enter`
    /// loses the reader's place in the panes for no reason.
    pub details: std::collections::BTreeMap<String, Detail>,
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
            auto_index: false,
            auto_indexed: None,
            bench: None,
            bench_band: bench::DEFAULT_BAND_PERCENT,
            diffs: std::collections::BTreeMap::new(),
            details: std::collections::BTreeMap::new(),
            touches: 0,
            model: Model::embedded(),
            visible: Vec::new(),
        };
        app.recompute();
        app
    }

    /// The commands the screen asks for the moment it opens.
    ///
    /// One question, answered without waiting for a keypress: is the store behind this checkout, and is
    /// this a screen allowed to fix that? A reader who commits, opens `fathom`, and sees yesterday's
    /// numbers with a `stale` mark in the header has been handed a chore, so the chore is started here
    /// and says what it is on the status line. `R` still works, and still means "again, right now".
    #[must_use]
    pub fn startup(&mut self) -> Vec<Command> {
        self.auto_index_now()
    }

    /// Whether this reading is behind the checkout in a way this screen may repair.
    ///
    /// The three refusals, in the order they are cheap to answer:
    ///
    /// * the switch is off (`--no-auto-index`, `--plain`, a test that asked for nothing);
    /// * a run of this screen's is already going — `R` says "an index is already running" and Esc
    ///   cancels, and an automatic second ask would only be the same refusal spelled harder;
    /// * the store is not *behind*: absent (nothing has ever been indexed here, which is a bigger
    ///   decision and stays an explicit `R`) or already current ([`is_stale`]'s narrow answer).
    ///
    /// Then one guard: this very `HEAD` was already auto-indexed and did not catch up, so it will not be
    /// tried again until the checkout moves or someone presses `R`.
    ///
    /// A cache root another Fathom is writing is not refused here: the run itself answers that, fast
    /// and in the lock file's own words, and asking twice a second whether somebody else is holding a
    /// lock would be a second protocol over the same `flock`.
    #[must_use]
    fn auto_index_now(&mut self) -> Vec<Command> {
        if !self.auto_index || self.index.is_running() || !is_stale(&self.reading) {
            return Vec::new();
        }
        let Some(head) = self.reading.head.clone() else {
            return Vec::new();
        };
        if self.auto_indexed.as_deref() == Some(head.as_str()) {
            return Vec::new();
        }
        self.auto_indexed = Some(head.clone());
        self.begin_index(Some(format!(
            "HEAD {} is newer than the store — re-indexing (Esc cancels, --no-auto-index turns this \
             off)",
            bench::short(&head)
        )))
    }

    /// Put the screen in "our run" and ask the engine for it. The one door both `R` and the automatic
    /// path walk through, so the header, the progress line and the cancellation behave identically.
    fn begin_index(&mut self, why: Option<String>) -> Vec<Command> {
        self.index = IndexState::Running {
            phase: "starting".to_owned(),
            done: 0,
            total: 0,
            ours: true,
        };
        if why.is_some() {
            self.message = why;
        }
        vec![Command::Index]
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
        if let Some(panel) = self.bench.as_mut() {
            panel.set_height(bench::pane_height(height));
        }
        for detail in self.details.values_mut() {
            detail.set_height(detail::pane_height(height));
        }
    }

    /// Open the bench panel of one commit.
    ///
    /// No [`Command`] comes back from this and none can: the panel is a function of rows the store already
    /// handed this screen, so unlike `d` — which asks the engine for a patch — `b` answers in the frame it
    /// was pressed in. That is the whole reason the panel reads the store's rows and not the evidence
    /// tree.
    pub fn open_bench(&mut self, sha: &str) {
        if self.bench.as_ref().is_none_or(|panel| panel.sha != sha) {
            let panel = self.build_bench(sha);
            self.bench = Some(panel);
        }
        self.screen = Screen::Bench(sha.to_owned());
        if let Some(panel) = self.bench.as_mut() {
            panel.set_height(bench::pane_height(self.height));
        }
    }

    /// Leave the bench panel. The panel itself survives, which is what keeps the reader's place.
    pub fn close_bench(&mut self) {
        self.screen = Screen::Timeline;
    }

    /// The panel `--bench REV` opens on the command line: the same screen `b` opens, resolved before the
    /// first frame is drawn.
    ///
    /// A revision the window does not contain is a refusal the caller prints, not a panel of unknowns:
    /// `--from`/`--to` and `--limit` decide what this screen can know, and saying so is more useful than
    /// drawing an empty table for a commit that was never indexed here.
    pub fn show_bench(&mut self, sha: &str) -> Result<(), String> {
        if !self.reading.entries.iter().any(|entry| entry.sha == sha) {
            return Err(format!(
                "{} is not in this window \u{b7} the store has {} row(s); widen with --from/--to or drop \
                 --limit",
                bench::short(sha),
                self.reading.entries.len()
            ));
        }
        self.open_bench(sha);
        Ok(())
    }

    /// Build the panel for one commit from the reading: its own row, plus the commits below it.
    fn build_bench(&self, sha: &str) -> Panel {
        let evidence = Evidence {
            present: self.reading.evidence_present,
            path: self.reading.evidence_path.clone(),
        };
        let commit = self
            .reading
            .entries
            .iter()
            .find(|entry| entry.sha == sha)
            .map(|entry| entry.detail.clone())
            .unwrap_or_else(|| Commit {
                // A commit with no row at all: the empty state, named by SHA, with its next steps.
                sha: sha.to_owned(),
                gap: Some("no_evidence".to_owned()),
                ..Commit::default()
            });
        let older: Vec<Commit> = self
            .reading
            .entries
            .iter()
            .skip_while(|entry| entry.sha != sha)
            .skip(1)
            .take(bench::HISTORY_WINDOW.saturating_sub(1))
            .map(|entry| entry.detail.clone())
            .collect();
        Panel::open(&commit, &older, &evidence, self.bench_band)
    }

    /// The open diff view, if the diff screen is up.
    #[must_use]
    pub fn diff_view(&self) -> Option<&DiffView> {
        match &self.screen {
            Screen::Timeline | Screen::Bench(_) | Screen::Detail(_) => None,
            Screen::Diff(sha) => self.diffs.get(sha),
        }
    }

    /// The open diff view, mutably.
    pub fn diff_view_mut(&mut self) -> Option<&mut DiffView> {
        match &self.screen {
            Screen::Timeline | Screen::Bench(_) | Screen::Detail(_) => None,
            Screen::Diff(sha) => self.diffs.get_mut(sha),
        }
    }

    /// The open bench panel, if the bench screen is up.
    #[must_use]
    pub fn bench_view(&self) -> Option<&Panel> {
        match &self.screen {
            Screen::Timeline | Screen::Diff(_) | Screen::Detail(_) => None,
            Screen::Bench(sha) => self.bench.as_ref().filter(|panel| panel.sha == *sha),
        }
    }

    /// The open bench panel, mutably.
    pub fn bench_view_mut(&mut self) -> Option<&mut Panel> {
        match &self.screen {
            Screen::Timeline | Screen::Diff(_) | Screen::Detail(_) => None,
            Screen::Bench(sha) => self.bench.as_mut().filter(|panel| panel.sha == *sha),
        }
    }

    /// Open the detail view of one commit, returning the commands the loop must run — one
    /// [`Command::Detail`] the first time, nothing after, because the snapshot is cached.
    ///
    /// The read is the engine's job for the same reason the patch is: a snapshot is a row, a parent row,
    /// and one block per file the commit touched, and a keypress must not wait on any of them.
    pub fn open_detail(&mut self, sha: &str) -> Vec<Command> {
        self.touches += 1;
        let touch = self.touches;
        let mut commands = Vec::new();
        if let Some(view) = self.details.get_mut(sha) {
            view.touched = touch;
        } else {
            let mut view = Detail::open(sha);
            view.set_height(detail::pane_height(self.height));
            self.details.insert(sha.to_owned(), view);
            commands.push(Command::Detail(sha.to_owned()));
            self.evict_details(sha);
        }
        self.screen = Screen::Detail(sha.to_owned());
        commands
    }

    /// Leave the detail view. The snapshot stays cached, which is the point of the cache.
    pub fn close_detail(&mut self) {
        self.screen = Screen::Timeline;
    }

    /// Show a detail snapshot that has already been read — `--detail REV`, which opens the screen
    /// `enter` opens without the read `enter` would have asked the engine for.
    pub fn show_detail(&mut self, snapshot: Snapshot) {
        self.touches += 1;
        let sha = snapshot.sha.clone();
        let mut view = Detail::read(snapshot);
        view.touched = self.touches;
        view.set_height(detail::pane_height(self.height));
        self.details.insert(sha.clone(), view);
        self.evict_details(&sha);
        self.screen = Screen::Detail(sha);
    }

    /// The open detail view, if the detail screen is up.
    #[must_use]
    pub fn detail_view(&self) -> Option<&Detail> {
        match &self.screen {
            Screen::Timeline | Screen::Diff(_) | Screen::Bench(_) => None,
            Screen::Detail(sha) => self.details.get(sha),
        }
    }

    /// The open detail view, mutably.
    pub fn detail_view_mut(&mut self) -> Option<&mut Detail> {
        match &self.screen {
            Screen::Timeline | Screen::Diff(_) | Screen::Bench(_) => None,
            Screen::Detail(sha) => self.details.get_mut(sha),
        }
    }

    /// Forget the least recently opened detail views once the cache is over its bound.
    fn evict_details(&mut self, keep: &str) {
        if self.details.len() <= DETAIL_CACHE {
            return;
        }
        let mut oldest: Option<(u64, String)> = None;
        for (sha, view) in &self.details {
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
            self.details.remove(&sha);
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

        match &self.screen {
            Screen::Timeline => self.on_timeline_key(key),
            Screen::Diff(sha) => {
                let sha = sha.clone();
                self.on_diff_key(&sha, key)
            }
            Screen::Bench(sha) => {
                let sha = sha.clone();
                self.on_bench_key(&sha, key)
            }
            Screen::Detail(sha) => {
                let sha = sha.clone();
                self.on_detail_key(&sha, key)
            }
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
                    commands.extend(self.begin_index(None));
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
            // The ticket's named interaction, and the one that answers "what exactly got worse?": a
            // three-pane view of the commit's coverage, MI and diffstat. The read goes to the engine, so
            // pressing `enter` on a commit whose blocks are not in memory still cannot delay a key.
            Key::Detail => {
                let selected = self.selected().map(|entry| entry.sha.clone());
                match selected {
                    Some(sha) => commands.extend(self.open_detail(&sha)),
                    None => {
                        self.message =
                            Some("nothing is selected, so there is nothing to open".to_owned());
                    }
                }
            }
            // The ticket's second named interaction, and the cheap one: every number the panel shows is
            // already in the reading, so there is nothing to ask the engine for.
            Key::Bench => match self.selected().map(|entry| entry.sha.clone()) {
                Some(sha) => self.open_bench(&sha),
                None => {
                    self.message =
                        Some("nothing is selected, so there is nothing to benchmark".to_owned());
                }
            },
            Key::ExcludedRuns => {
                self.message = Some(
                    "f shows a commit's excluded runs \u{b7} b opens its benchmark panel"
                        .to_owned(),
                )
            }
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
            Key::SortCycle => {
                // Not this screen's key, and `timeline_letter` does not map `s` at all, so this arm is
                // reached only if a caller hands it in directly. The answer is the same one the diff and
                // bench screens give: the key exists, on another screen.
                self.message = Some(
                    "s sorts a pane inside the commit detail view \u{b7} enter opens one"
                        .to_owned(),
                );
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
            Key::Detail => commands.extend(self.open_detail(sha)),
            // `b` from a diff is the same commit's other panel, and it costs nothing: no read, no
            // command, one screen swap.
            Key::Bench => self.open_bench(sha),
            Key::ExcludedRuns => {
                self.message = Some(
                    "f belongs to the benchmark panel \u{b7} b opens it for this commit".to_owned(),
                );
            }
            Key::SortCycle => {
                self.message = Some(
                    "s sorts a pane inside the commit detail view \u{b7} enter opens one"
                        .to_owned(),
                );
            }
            Key::Reindex => {
                self.message =
                    Some("re-index is the timeline's key \u{b7} Esc goes back to it".to_owned());
            }
            Key::Backspace | Key::Text(_) | Key::Ignored => {}
        }
        commands
    }

    /// The bench panel's answer to a key.
    ///
    /// `b` and `Esc` both leave, which is the same habit the diff view teaches. Nothing here moves the
    /// timeline's selection, and nothing here can block: the panel's rows were built when it opened, so
    /// every key on this screen is a field write and a clamp.
    fn on_bench_key(&mut self, sha: &str, key: Key) -> Vec<Command> {
        let mut commands = Vec::new();
        let page = isize::try_from(usize::from(bench::pane_height(self.height))).unwrap_or(1);
        match bench_letter(key) {
            Key::Quit => {
                self.quit = true;
                commands.push(Command::Quit);
            }
            Key::Escape | Key::Bench => self.close_bench(),
            Key::Help => self.help = !self.help,
            Key::Down => self.scroll_bench(1),
            Key::Up => self.scroll_bench(-1),
            Key::PageDown => self.scroll_bench(page),
            Key::PageUp => self.scroll_bench(-page),
            Key::Top => {
                if let Some(panel) = self.bench_view_mut() {
                    panel.jump(false);
                }
            }
            Key::Bottom => {
                if let Some(panel) = self.bench_view_mut() {
                    panel.jump(true);
                }
            }
            Key::ExcludedRuns => {
                if let Some(panel) = self.bench_view_mut() {
                    let show = !panel.show_excluded;
                    panel.set_show_excluded(show);
                    if show && panel.commit.excluded.is_empty() {
                        self.message =
                            Some("no run at this commit was excluded by retention".to_owned());
                    }
                }
            }
            // `d` on the panel is the same commit's diff, so the two detail screens are one hop apart
            // rather than one trip back through the timeline.
            Key::Diff => commands.extend(self.open_diff(sha)),
            Key::Filter => {
                self.message = Some(
                    "search inside the panel is a later ticket \u{b7} / filters the timeline"
                        .to_owned(),
                );
            }
            Key::Detail => commands.extend(self.open_detail(sha)),
            Key::Reindex => {
                self.message =
                    Some("re-index is the timeline's key \u{b7} Esc goes back to it".to_owned());
            }
            Key::NextFile | Key::PrevFile | Key::ToggleFiles => {
                self.message = Some(
                    "n/p/t belong to the diff view \u{b7} d opens it for this commit".to_owned(),
                );
            }
            Key::SortCycle => {
                self.message = Some(
                    "s sorts a pane inside the commit detail view \u{b7} enter opens one"
                        .to_owned(),
                );
            }
            Key::Backspace | Key::Text(_) | Key::Ignored => {}
        }
        commands
    }

    /// The commit detail view's answer to a key (mact-by1.14).
    ///
    /// The cursor belongs to one pane and the *file* belongs to all three: `j`/`k` move inside the focused
    /// pane, and whichever file the cursor lands on is what the other two panes highlight. That is the
    /// whole reason the three are drawn together, so it is the first behaviour a key test has to pin.
    fn on_detail_key(&mut self, sha: &str, key: Key) -> Vec<Command> {
        let mut commands = Vec::new();
        let key = detail_letter(key);
        // The screen is only up while its own SHA is the screen's SHA, so this borrow cannot clash with
        // the `self` methods the navigation arms below call.
        if let Some(view) = self.details.get_mut(sha) {
            self.touches += 1;
            view.touched = self.touches;
            match key {
                // `Tab`/`Shift-Tab` (and `n`/`p`) choose a *pane* here, not a file: the file is chosen by
                // the cursor, and the other panes follow it.
                Key::NextFile => view.cycle_pane(1),
                Key::PrevFile => view.cycle_pane(-1),
                Key::Down => view.move_cursor(1),
                Key::Up => view.move_cursor(-1),
                Key::Top => view.jump(true),
                Key::Bottom => view.jump(false),
                Key::SortCycle => view.cycle_sort(),
                Key::ToggleFiles => view.toggle_inputs(),
                _ => {}
            }
        }
        match key {
            // `enter` twice is the round trip the ticket is judged on: open from the timeline, back out to
            // the same selected commit. Nothing here moves the timeline's cursor.
            Key::Escape | Key::Detail => self.close_detail(),
            Key::Diff => commands.extend(self.open_diff(sha)),
            Key::Bench => self.open_bench(sha),
            Key::Help => self.help = !self.help,
            Key::Quit => commands.push(Command::Quit),
            Key::PageDown | Key::PageUp => {
                // A page is a pane-full of rows, not a screen-full: the frame spends four rows on the
                // heading, the status pair and the key line, and paging by the frame's height would skip
                // the rows that fell off a pane's bottom.
                let step =
                    isize::try_from(usize::from(detail::pane_height(self.height))).unwrap_or(1);
                if let Some(view) = self.details.get_mut(sha) {
                    view.move_cursor(if matches!(key, Key::PageUp) {
                        -step
                    } else {
                        step
                    });
                }
            }
            Key::Reindex => {
                self.message =
                    Some("re-index is the timeline's key \u{b7} Esc goes back to it".to_owned());
            }
            Key::ExcludedRuns => {
                self.message = Some(
                    "f belongs to the benchmark panel \u{b7} b opens it for this commit".to_owned(),
                );
            }
            Key::Filter => {
                self.message = Some(
                    "search inside the detail view is a later ticket \u{b7} / filters the timeline"
                        .to_owned(),
                );
            }
            Key::Text(other) => {
                self.message = Some(format!(
                    "{other} is not a key in the detail view \u{b7} ? lists them"
                ));
            }
            // Already handled above, on the cached view itself: these are the keys that move a cursor or
            // a focus, and they have no `self`-level work to do.
            Key::Down
            | Key::Up
            | Key::Top
            | Key::Bottom
            | Key::NextFile
            | Key::PrevFile
            | Key::SortCycle
            | Key::ToggleFiles
            | Key::Backspace
            | Key::Ignored => {}
        }
        commands
    }

    /// Move the panel's row selection.
    fn scroll_bench(&mut self, delta: isize) {
        if let Some(panel) = self.bench_view_mut() {
            panel.move_selection(delta);
        }
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
                // An open panel is a view of the rows it was built from, so a re-read rebuilds it — with
                // the reader's place kept, because a frame that jumps the cursor while you are reading a
                // row is a frame that makes you start over.
                if let Screen::Bench(sha) = &self.screen {
                    let sha = sha.clone();
                    let place = self
                        .bench
                        .as_ref()
                        .map(|panel| (panel.selection, panel.show_excluded));
                    // Not `open_bench` on its own: that keeps an existing panel, which is right for a key
                    // and wrong here, where the point of the re-read is that the rows changed. Drop it and
                    // let the new reading build the panel; the reader's place is put back below.
                    self.bench = None;
                    self.open_bench(&sha);
                    if let (Some((selection, excluded)), Some(panel)) = (place, self.bench.as_mut())
                    {
                        panel.select(selection);
                        panel.set_show_excluded(excluded);
                    }
                }
                // A re-read is also the moment a new commit becomes visible: `git commit` in the other
                // half of a split terminal moves `HEAD`, the tick notices, the reading arrives, and the
                // store catches up without anyone remembering to press `R`.
                self.auto_index_now()
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
            // The snapshot lands in the cached view whether or not this screen is still up: the reader may
            // have backed out while the reads were in flight, and the next `enter` should find the answer
            // already there. An evicted view drops the answer, as a diff's does.
            Event::Detail { sha, outcome } => {
                let Some(view) = self.details.get_mut(&sha) else {
                    return Vec::new();
                };
                match outcome {
                    Ok(snapshot) => view.set_snapshot(*snapshot),
                    Err(reason) => view.set_error(reason),
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
    pub fn status_line(&self, palette: &Palette) -> String {
        if let Some(message) = &self.message {
            return message.clone();
        }
        match &self.screen {
            Screen::Timeline => self.timeline_status(),
            Screen::Diff(_) => self.diff_view().map_or_else(String::new, DiffView::status),
            Screen::Bench(_) => self
                .bench_view()
                .map_or_else(String::new, |panel| panel.status(palette)),
            Screen::Detail(_) => self.detail_view().map_or_else(
                || "commit detail: reading \u{2026}".to_owned(),
                |view| view.status(palette),
            ),
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

/// What the commit detail view means by a typed character.
///
/// `enter` and `Esc` arrive as named keys and are handled in [`App::on_detail_key`]; the letters are
/// mapped here so the help overlay and the "not a key" message can both mean what they say. `Tab` and
/// `Shift-Tab` arrive as `n`/`p`'s named keys and choose a *pane*, which is why they are not here.
fn detail_letter(key: Key) -> Key {
    match key {
        Key::Text(character) => match character {
            'j' => Key::Down,
            'k' => Key::Up,
            'g' => Key::Top,
            'G' => Key::Bottom,
            'q' => Key::Quit,
            '?' => Key::Help,
            's' => Key::SortCycle,
            // `i` for the MI inputs; `t` also works because it is the "show what the pane was hiding"
            // key everywhere else in this front end, and a reader who reaches for it is not wrong.
            'i' | 't' => Key::ToggleFiles,
            'n' => Key::NextFile,
            'p' => Key::PrevFile,
            'd' => Key::Diff,
            'b' => Key::Bench,
            'f' => Key::ExcludedRuns,
            'R' => Key::Reindex,
            other => Key::Text(other),
        },
        other => other,
    }
}

/// What the bench panel means by a typed character.
///
/// `f` is this screen's own key — the failed-run overlay — and it is a letter rather than a modifier
/// because every other key on this screen is a plain letter and a reader reaching for `Ctrl` has
/// already been told the wrong thing about this terminal.
fn bench_letter(key: Key) -> Key {
    match key {
        Key::Text(character) => match character {
            'j' => Key::Down,
            'k' => Key::Up,
            'g' => Key::Top,
            'G' => Key::Bottom,
            'q' => Key::Quit,
            '?' => Key::Help,
            '/' => Key::Filter,
            'b' => Key::Bench,
            'd' => Key::Diff,
            'f' => Key::ExcludedRuns,
            // `s` belongs to the detail view; this screen still has to say so rather than be quiet.
            's' => Key::SortCycle,
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
            // `s` belongs to the detail view; the screens that do not sort still have to say so.
            's' => Key::SortCycle,
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
