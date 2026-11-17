//! The diff view: what it knows, what its keys do to it, and the words it prints.
//!
//! [`DiffView`] is to the diff screen what [`super::state::App`] is to the timeline — a pure state
//! machine with no terminal, no clock and no `git`. The patch text arrives from the engine
//! ([`super::engine::Event::Diff`]) and is handed in whole; everything here decides *where the reader
//! is looking* and *what the screen says about it*.
//!
//! Four decisions are worth naming:
//!
//! * **The view is per-commit and it is kept.** `App` caches one `DiffView` per commit for the length of
//!   the run, so `d` out and `d` back lands on the same file at the same line rather than at the top.
//!   Scroll offsets are per *file* within that view, because the common thing to do is read three files
//!   of a commit, back out, and come back to the one you stopped on.
//! * **Which files are in view is a filter, not a different commit.** The default is the headline
//!   (`Scope::Source`) set — the same set the timeline row's `+`/`−` counted — and `t` shows the
//!   lockfiles, vendored trees and tests as well. The header's totals switch with it, because a `+`/`−`
//!   column and a file list that disagree about what counts is the disagreement the ticket calls out.
//! * **Every cap is said out loud.** [`CommitDiff::banner`] on the status line, a per-file note at the
//!   end of that file's text, and a named reason for a file whose patch was never read.
//! * **A patch line is displayed, not re-edited.** [`visible_text`] makes tabs, carriage returns and
//!   other control bytes visible (so a `a\tb` cannot be read as `a b`, and a file containing an ANSI
//!   escape cannot paint the terminal that is showing it); nothing else about git's text is changed. No
//!   syntax highlighting: v1 distinguishes hunk header, add, delete, context and file header, and the
//!   help overlay says so rather than leaving the omission to be discovered.

use std::collections::BTreeMap;

use crate::classify::Scope;
use crate::diff::{CommitDiff, FileDiff, LineKind};

use super::cells::Align;
use super::palette::Palette;
use super::plain::fit;

/// The marker column of the file pane: two cells, `▸` and a space (or `> ` on the ascii surface).
const MARKER: &str = "▸ ";
/// [`MARKER`]'s ascii twin.
const MARKER_ASCII: &str = "> ";
/// The files-pane flag that says a path is in the view only because `t` is on. It is one cell wide and
/// followed by a space, so the marked paths line up with the unmarked ones.
const HIDDEN_FLAG: &str = "~";

/// The narrowest files pane worth drawing: a path column that cannot hold a path is a decoration.
pub const FILES_MIN: usize = 18;

/// The diff pane's height for a frame of `height` rows.
///
/// The frame spends one row on the heading, one on the status line, one on the key line, and two on the
/// pane's borders. Both the drawing and the scroll clamp need the same number, which is why it is a
/// function and not a subtraction in two places.
#[must_use]
pub const fn pane_height(height: u16) -> u16 {
    let body = height.saturating_sub(5);
    if body == 0 {
        1
    } else {
        body
    }
}

/// Which screen the loop is painting.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Screen {
    /// The commit timeline.
    #[default]
    Timeline,
    /// The diff of one commit, named by its full SHA.
    Diff(String),
    /// The benchmark panel of one commit, named by its full SHA (mact-by1.12).
    Bench(String),
}

/// Where the reader is inside one commit's diff.
#[derive(Debug, Clone)]
pub struct DiffView {
    /// The commit this view is about, as a full SHA.
    pub sha: String,
    /// A read is in flight (or was never started).
    pub loading: bool,
    /// The read's refusal, when it failed. Shown rather than swallowed: an empty pane with no reason is
    /// the same screen as a commit that changed nothing.
    pub error: Option<String>,
    /// The patch, once there is one.
    pub diff: Option<CommitDiff>,
    /// Selected file, as an index into [`DiffView::all_files`], not into the filtered list — so turning
    /// `t` on and off cannot move the cursor.
    pub file: usize,
    /// Scroll offset per file, so moving off a file and back does not lose your place.
    pub scrolls: BTreeMap<usize, usize>,
    /// Whether the noise-filtered paths are shown too (`t`).
    pub show_all: bool,
    /// The diff pane's height in lines, which is what `bottom` and the scroll clamp need.
    pub height: u16,
    /// The clock-free "last opened" counter `App` uses to evict the least recently viewed commit.
    pub touched: u64,
}

impl DiffView {
    /// A view for a commit whose patch has not been read yet.
    #[must_use]
    pub fn open(sha: &str) -> Self {
        Self {
            sha: sha.to_owned(),
            loading: true,
            error: None,
            diff: None,
            file: 0,
            scrolls: BTreeMap::new(),
            show_all: false,
            height: 1,
            touched: 0,
        }
    }

    /// The frame's height for this pane, remembered so the keys can clamp against it.
    pub fn set_height(&mut self, height: u16) {
        self.height = height.max(1);
        self.clamp_scroll();
    }

    /// Every file the commit touched, in git's order.
    #[must_use]
    pub fn all_files(&self) -> &[FileDiff] {
        self.diff.as_ref().map_or(&[], |diff| &diff.files)
    }

    /// The indices of the files the current filter shows: the headline set, or everything.
    #[must_use]
    pub fn shown(&self) -> Vec<usize> {
        (0..self.all_files().len())
            .filter(|index| self.show_all || self.all_files()[*index].scope == Scope::Source)
            .collect()
    }

    /// Where [`DiffView::file`] sits in the filtered list, for the `[2/7 files]` counter.
    #[must_use]
    pub fn shown_position(&self) -> Option<usize> {
        self.shown().iter().position(|index| *index == self.file)
    }

    /// The file the diff pane is drawing, if there is one.
    #[must_use]
    pub fn current(&self) -> Option<&FileDiff> {
        self.all_files().get(self.file)
    }

    /// The scroll offset for the selected file.
    #[must_use]
    pub fn scroll(&self) -> usize {
        self.scrolls.get(&self.file).copied().unwrap_or(0)
    }

    /// How far the selected file can scroll before it runs out of lines.
    #[must_use]
    pub fn last_scroll(&self) -> usize {
        self.pane_lines()
            .len()
            .saturating_sub(usize::from(self.height.max(1)))
    }

    /// Move the file selection by `step`, wrapping at both ends (`n`/`p`, `Tab`/`Shift-Tab`).
    ///
    /// Wrapping is deliberate: the ticket says *cycle*, and a cursor that stops at the end of a
    /// seven-file commit makes the user count presses to get back to the first file.
    pub fn cycle_file(&mut self, step: isize) {
        let shown = self.shown();
        if shown.is_empty() {
            self.file = 0;
            return;
        }
        let position = shown
            .iter()
            .position(|index| *index == self.file)
            .unwrap_or(0);
        let count = isize::try_from(shown.len()).unwrap_or(isize::MAX);
        let moved = (((isize::try_from(position).unwrap_or(0) + step) % count) + count) % count;
        self.file = shown[usize::try_from(moved).unwrap_or(0)];
        self.clamp_scroll();
    }

    /// Select a file by index into the full list (used by a caller that knows which one it wants).
    pub fn select_file(&mut self, index: usize) {
        if index < self.all_files().len() {
            self.file = index;
            self.clamp_scroll();
        }
    }

    /// Turn the noise filter on or off (`t`), keeping the selection somewhere the filter still shows.
    ///
    /// The easy implementation lets the cursor point at a file that is no longer in the list, which draws
    /// a diff pane with no highlighted row and a header counter that says `—/7`. The cursor moves to the
    /// first file the filter does show instead.
    pub fn set_show_all(&mut self, show: bool) {
        self.show_all = show;
        let shown = self.shown();
        if !shown.contains(&self.file) {
            if let Some(first) = shown.first() {
                self.file = *first;
            }
        }
        self.clamp_scroll();
    }

    /// Called when a patch has just arrived: park the cursor and the scroll inside what is actually here.
    pub fn settle(&mut self) {
        if self.all_files().is_empty() {
            self.file = 0;
        } else if self.file >= self.all_files().len() || !self.shown().contains(&self.file) {
            self.file = self.shown().first().copied().unwrap_or(0);
        }
        self.clamp_scroll();
    }

    /// Scroll the diff pane by `delta` lines, clamped to the file's own length.
    pub fn scroll_by(&mut self, delta: isize) {
        let current = i64::try_from(self.scroll()).unwrap_or(i64::MAX);
        let step = i64::try_from(delta.abs()).unwrap_or(i64::MAX);
        let next = if delta < 0 {
            current - step.min(current)
        } else {
            current + step
        };
        self.scrolls
            .insert(self.file, usize::try_from(next).unwrap_or(usize::MAX));
        self.clamp_scroll();
    }

    /// Top of this file's text.
    pub fn scroll_top(&mut self) {
        self.scrolls.insert(self.file, 0);
    }

    /// Bottom of this file's text.
    pub fn scroll_bottom(&mut self) {
        let last = self.last_scroll();
        self.scrolls.insert(self.file, last);
    }

    /// Keep the offset inside the selected file, after any change to the file, the text or the height.
    fn clamp_scroll(&mut self) {
        let last = self.last_scroll();
        if self.scroll() > last {
            self.scrolls.insert(self.file, last);
        }
    }

    /// `vs parent 8ac22d1`, `vs the empty tree (root commit)`, `vs parent 8ac22d1 (merge of 3)` —
    /// ADR-0008 §6.3 in the words the header prints, because "vs parent" on a root commit is a claim
    /// about a parent that does not exist.
    #[must_use]
    pub fn base(&self) -> String {
        let Some(diff) = self.diff.as_ref() else {
            return format!("reading {}", short(&self.sha));
        };
        let base = match &diff.diff_base {
            crate::history::DiffBase::EmptyTree => "vs the empty tree (root commit)".to_owned(),
            crate::history::DiffBase::Parent { sha } => {
                format!("vs parent {}", short(sha))
            }
        };
        match diff.parents.len() {
            0..=1 => base,
            many => format!("{base} (merge of {many} parents)"),
        }
    }

    /// The heading's two halves: `DIFF  3b8e9f1 coverage: drop node_modules` and
    /// `vs parent 8ac22d1 · +140 -310 · 1/7 files`.
    #[must_use]
    pub fn header(&self, palette: &Palette) -> (String, String) {
        let Some(diff) = self.diff.as_ref() else {
            let left = format!("DIFF  {} \u{2014}", short(&self.sha));
            let right = if let Some(error) = &self.error {
                error.clone()
            } else {
                format!("reading {} \u{2026}", short(&self.sha))
            };
            return (left, right);
        };
        let (add, del, files) = diff.totals.shown(self.show_all);
        let position = self
            .shown_position()
            .map_or_else(|| "\u{2014}".to_owned(), |index| (index + 1).to_string());
        let shown = self.shown().len();
        let right = format!(
            "{} \u{b7} +{} -{} \u{b7} {position}/{files} files{of}",
            self.base(),
            add,
            del,
            files = files.max(shown),
            of = if self.show_all {
                " (all)".to_owned()
            } else {
                String::new()
            }
        );
        let _ = palette;
        (
            format!(
                "DIFF  {}  {}",
                diff.short_id,
                if diff.subject.is_empty() {
                    "(no subject)"
                } else {
                    &diff.subject
                }
            ),
            right,
        )
    }

    /// The things that must be said about this diff on any surface: the refusal, the caps' bill, or the
    /// reason the pane is empty. `None` when there is nothing to explain.
    ///
    /// This is the half of [`DiffView::status`] that is not about a key press, which is what the plain
    /// surface prints: `--plain` has no `t`, so a line telling the reader to press it is not advice, it
    /// is a leftover.
    #[must_use]
    pub fn notice(&self) -> Option<String> {
        if let Some(error) = &self.error {
            return Some(error.clone());
        }
        let diff = self.diff.as_ref()?;
        if let Some(banner) = diff.banner() {
            return Some(banner);
        }
        if diff.is_empty() {
            return Some(if diff.parents.len() > 1 {
                "this merge brings nothing its first parent did not already have".to_owned()
            } else {
                "this commit changes nothing: it has no files to diff".to_owned()
            });
        }
        None
    }

    /// The status line: a refusal, then the caps, then what the pane is not showing, then the file's own
    /// note. Never empty, because an unexplained empty pane reads as a bug in the tool rather than a
    /// fact about the commit.
    #[must_use]
    pub fn status(&self) -> String {
        if let Some(notice) = self.notice() {
            return notice;
        }
        let Some(diff) = self.diff.as_ref() else {
            return format!(
                "reading {} \u{2014} git show \u{2014} the pane fills when it answers",
                short(&self.sha)
            );
        };
        if diff.hidden_files() > 0 && !self.show_all {
            return format!(
                "{} noise/vendored file{} hidden by the filters \u{b7} t shows them (the header's \
                 +/- goes with them)",
                diff.hidden_files(),
                if diff.hidden_files() == 1 { "" } else { "s" }
            );
        }
        if self.show_all && diff.hidden_files() > 0 {
            return format!(
                "showing all {} files \u{b7} t hides the {} noise/vendored again",
                diff.files.len(),
                diff.hidden_files()
            );
        }
        if let Some(note) = self.current().and_then(FileDiff::no_text) {
            return note.to_owned();
        }
        if self.current().is_some_and(|file| file.cut) {
            return format!(
                "this file's patch was cut at {} \u{b7} raise {} to see the rest",
                crate::diff::bytes(diff.limits.per_file),
                crate::diff::MAX_DIFF_BYTES_PER_FILE_FLAG
            );
        }
        String::new()
    }

    /// The file pane's rows, one per shown file, already fitted to `width` cells.
    ///
    /// `marker` is the two-cell cursor column the screen draws; the plain surface passes `false`, which
    /// keeps the column (so the paths line up with the counts) without pretending a text dump has a
    /// cursor.
    #[must_use]
    pub fn rows(&self, width: usize, palette: &Palette, marker: bool) -> Vec<String> {
        let cursor = if palette.unicode() {
            MARKER
        } else {
            MARKER_ASCII
        };
        self.shown()
            .into_iter()
            .map(|index| {
                let file = &self.all_files()[index];
                let selected = marker && index == self.file;
                // One column, and the flag is not part of the path: `~ Cargo.lock` says "this one is
                // here because you asked", where `~Cargo.lock` reads as a filename.
                let flag = if file.scope == Scope::Source {
                    ""
                } else {
                    HIDDEN_FLAG
                };
                let gap = if flag.is_empty() { "" } else { " " };
                let counts = if file.binary {
                    "[binary]".to_owned()
                } else {
                    format!("+{} -{}", file.add, file.del)
                };
                // The counts are the reason the pane exists, so they are kept whole and the path is
                // what gives way: a `+82 -310` truncated to `+82 -3` is worse than a long path cut.
                let head = if selected { cursor } else { "  " };
                let room = width.saturating_sub(
                    head.chars().count()
                        + flag.chars().count()
                        + gap.chars().count()
                        + counts.chars().count()
                        + 1,
                );
                let path = fit(&file.path, room, Align::Left, palette.ellipsis());
                let path = match &file.old_path {
                    Some(old) => format!(
                        "{} \u{2192} {}",
                        fit(old, room / 2, Align::Left, palette.ellipsis()),
                        path
                    ),
                    None => path,
                };
                format!("{head}{flag}{gap}{path} {counts}")
            })
            .collect()
    }

    /// The diff pane's rows for the selected file, before scrolling: git's own lines, made visible,
    /// plus the note lines the caps and the empty cases earn.
    #[must_use]
    pub fn pane_lines(&self) -> Vec<(LineKind, String)> {
        let Some(file) = self.current() else {
            // No file to show: the refusal is the pane's content, and everything else — a read still
            // running, a commit with no files — leaves the pane empty and lets the status line say why.
            // An empty `(Meta, "")` row would be a row that is not there: `last_scroll` would count it,
            // and the pane would look scrolled by one.
            return match (self.error.as_ref(), self.diff.as_ref()) {
                (Some(_), _) | (None, Some(_)) => vec![(LineKind::Meta, self.status())],
                (None, None) => Vec::new(),
            };
        };
        let mut out: Vec<(LineKind, String)> = file
            .lines
            .iter()
            .map(|line| {
                (
                    line.kind,
                    if line.cut {
                        format!("{}\u{2026}", line.text)
                    } else {
                        line.text.clone()
                    },
                )
            })
            .collect();
        if let Some(note) = file.no_text() {
            out.push((LineKind::Meta, note.to_owned()));
        }
        if file.dropped > 0 {
            out.push((
                LineKind::Meta,
                format!(
                    "\u{2026} and {} more bytes of this file not kept \u{b7} raise {}",
                    crate::diff::bytes(file.dropped),
                    crate::diff::MAX_DIFF_BYTES_PER_FILE_FLAG
                ),
            ));
        }
        if file.unseen {
            if let Some(diff) = self.diff.as_ref() {
                if let Some(banner) = diff.banner() {
                    out.push((LineKind::Meta, banner));
                }
            }
        }
        out
    }
}

/// Seven hex, ADR-0008 §6.2's display form, for a SHA that may be shorter than seven in a test.
#[must_use]
pub fn short(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// A patch line with its control bytes made visible.
///
/// A tab in a diff is not a tab in a terminal: `a\tb` and `a  b` are different lines and a viewer that
/// collapses them says something false about the code. So tabs, carriage returns, escapes and every
/// other C0 control get a visible spelling — `␉ ␍ ␛` where unicode is on, `^I ^M ^[` where it is not —
/// and a `CRLF` line ending ends with a mark instead of silently moving the cursor. This also means a
/// file whose content contains an ANSI escape cannot paint the terminal that is showing it.
#[must_use]
pub fn visible_text(text: &str, unicode: bool) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        let code = u32::from(character);
        // 0x7f (DEL) joins the C0 range here: a caret notation has no spelling for it, and a raw DEL
        // in a terminal is at best invisible and at worst a bell.
        if code == 0x7f || (code < 0x20 && character != '\n') {
            if unicode {
                // The C0 control pictures block: U+2400 + the control's code.
                out.push(char::from_u32(0x2400 + code).unwrap_or('\u{fffd}'));
            } else {
                out.push('^');
                out.push(char::from_u32(code + 64).unwrap_or('?'));
            }
            continue;
        }
        out.push(character);
    }
    out
}

/// `text`, sanitised and cut to `width` cells with a marker rather than wrapped.
///
/// Wrapping is refused on purpose: a wrapped diff line is indistinguishable from two diff lines, and a
/// view of a code change that invents lines is worse than one that admits it cut. The marker is the
/// palette's own ellipsis, so `--plain` and a dumb terminal get `~`.
#[must_use]
pub fn pane_text(text: &str, width: usize, palette: &Palette) -> String {
    fit(
        &visible_text(text, palette.unicode()),
        width,
        Align::Left,
        palette.ellipsis(),
    )
}
