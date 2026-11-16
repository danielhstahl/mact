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

use super::engine::Event;
use super::entry::Entry;
use super::store_view::{is_stale, Reading, StoreState};

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
    /// `backspace`, which means "delete a filter character" while the filter is open.
    Backspace,
    /// A printable character, which means "append to the filter" while the filter is open.
    Text(char),
    /// Anything else, deliberately not a no-op-shaped hole: unknown keys are ignored on purpose.
    Ignored,
}

/// What the loop is told to do next. The state machine never does these itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Read the store again (the first frame, and after an index).
    Read,
    /// Start an index run.
    Index,
    /// Ask a running index to stop.
    Cancel,
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
        let mut commands = Vec::new();

        // The help overlay swallows Esc; every other key still works, because a reader who left `?`
        // up and wants to scroll should be allowed to.
        if self.help && key == Key::Escape {
            self.help = false;
            return commands;
        }

        // The filter prompt owns text keys and Backspace, and Esc/Enter close it.
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

        match key {
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
            Key::Diff => self.message = Some(sibling("diff", "mact-by1.11")),
            Key::Detail => self.message = Some(sibling("detail", "mact-by1.14")),
            Key::Bench => self.message = Some(sibling("bench", "mact-by1.12")),
            Key::Backspace | Key::Text(_) | Key::Ignored => {}
        }
        commands
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
    #[must_use]
    pub fn status_line(&self) -> String {
        if let Some(message) = &self.message {
            return message.clone();
        }
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
