//! Turning a `KeyEvent` into what the timeline means by it.
//!
//! The mapping is a separate function — and a separate file — for one reason: the ticket's
//! "input-handling unit tests for each key" should cover the *keys*, not just the state machine behind
//! them. A test that only fed [`crate::tui::state::Key::Down`] to `App` would still pass if `j` stopped
//! being down, so the tests in `tests/tui_keys.rs` start from a `KeyEvent` and end at a state change.
//!
//! Two rules the mapping keeps:
//!
//! * **Ctrl-C is a key, not a signal.** Raw mode means the terminal delivers it as `Char('c') +
//!   CONTROL`, so quitting cleanly (restore the screen, release the index lock) is the normal path and
//!   a `SIGHUP` story is not needed.
//! * **A bare printable character is `Text`, not a letter.** With the filter closed, `x` does nothing;
//!   with it open, `x` is a needle character. That is why `d`, `b`, `R` and `?` are matched *before*
//!   the catch-all and why `/` can never eat itself.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::state::Key;

/// The one mapping from crossterm to [`Key`].
///
/// `KeyRelease` events (delivered by some terminals on Windows) are ignored so a key does not act
/// twice; everything else falls through [`Key::Ignored`], which is the ticket's "anything not listed
/// here is not in this ticket", made a value rather than a comment.
#[must_use]
pub fn classify(event: KeyEvent) -> Key {
    if event.kind == KeyEventKind::Release {
        return Key::Ignored;
    }
    if event.modifiers.contains(KeyModifiers::CONTROL) {
        return match event.code {
            KeyCode::Char('c') => Key::Quit,
            _ => Key::Ignored,
        };
    }
    match event.code {
        KeyCode::Down | KeyCode::Char('j') => Key::Down,
        KeyCode::Up | KeyCode::Char('k') => Key::Up,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::Home | KeyCode::Char('g') => Key::Top,
        // `G` is shift+g on purpose: `g` alone jumps to the top, which is the vim pair the ticket names.
        KeyCode::End | KeyCode::Char('G') => Key::Bottom,
        KeyCode::Char('q') => Key::Quit,
        KeyCode::Char('?') => Key::Help,
        KeyCode::Char('/') => Key::Filter,
        KeyCode::Char('R') => Key::Reindex,
        KeyCode::Esc => Key::Escape,
        KeyCode::Char('d') => Key::Diff,
        KeyCode::Enter => Key::Detail,
        KeyCode::Char('b') => Key::Bench,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Char(character) => Key::Text(character),
        _ => Key::Ignored,
    }
}

/// A `KeyEvent` for a bare key, spelled once so tests and callers agree on the modifiers.
#[must_use]
pub fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// A `KeyEvent` with modifiers, for the Ctrl-C case.
#[must_use]
pub fn key_with(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, modifiers)
}
