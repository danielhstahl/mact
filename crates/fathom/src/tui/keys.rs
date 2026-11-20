//! Turning a `KeyEvent` into what the screens mean by it.
//!
//! The mapping is a separate function — and a separate file — for one reason: the ticket's
//! "input-handling unit tests for each key" should cover the *keys*, not just the state machine behind
//! them. A test that only fed [`crate::tui::state::Key::Down`] to `App` would still pass if `j` stopped
//! being down, so the tests in `tests/tui_keys.rs` start from a `KeyEvent` and end at a state change.
//!
//! ## Two layers, because one cannot serve both screens
//!
//! ```text
//! KeyEvent ──▶ classify ──▶ Key            structural: what the terminal sent
//! Key      ──▶ App::on_key ─▶ screen meaning
//! ```
//!
//! A printable character comes out of [`classify`] as [`Key::Text`] and only the screen knows what it
//! means. That is not tidiness: the timeline's `/` filter has to accept the word `registry`, which is
//! full of letters that are also commands (`g`, `t`, `r`), so if `classify` had turned `t` into
//! [`Key::ToggleFiles`] the filter could not have been typed at all — and the diff view, where `t`
//! *is* a command and there is no filter, needs the opposite answer. Two screens, one key each, is
//! exactly the reason the second decision belongs to the screen.
//!
//! Two rules the mapping keeps:
//!
//! * **Ctrl-C is a key, not a signal.** Raw mode means the terminal delivers it as `Char('c') +
//!   CONTROL`, so quitting cleanly (restore the screen, release the index lock) is the normal path and
//!   a `SIGHUP` story is not needed.
//! * **Non-printable keys are unambiguous, so they are mapped here.** `Esc`, `Enter`, `Tab`, `Home`, the
//!   arrows: no screen needs a character for those, and no filter can contain them.

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
        KeyCode::Down => Key::Down,
        KeyCode::Up => Key::Up,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::Home => Key::Top,
        KeyCode::End => Key::Bottom,
        KeyCode::Esc => Key::Escape,
        KeyCode::Enter => Key::Detail,
        KeyCode::Backspace => Key::Backspace,
        // `Tab`/`Shift-Tab` are the diff view's next/previous file. They are commands here rather than
        // `Text` because a filter cannot contain a tab, so nothing is lost by deciding now.
        KeyCode::Tab => Key::NextFile,
        KeyCode::BackTab => Key::PrevFile,
        // Every printable character is a needle first and a command second; see the module header.
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
