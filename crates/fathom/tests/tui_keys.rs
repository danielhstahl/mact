//! The key surface: from a `KeyEvent` to what the screen did, one test per key the ticket lists.
//!
//! Every test starts at a [`crossterm::event::KeyEvent`] and ends at a change in [`App`], because the
//! pair is the contract: a test that fed [`Key::Down`] straight to `App` would still pass if `j` stopped
//! being "down", and a test that only pressed `j` would still pass if the selection escaped the list.
//! So each one does both — classify, then apply — and the clamps are asserted at both ends rather than
//! at one.
//!
//! The keys are the ticket's list and nothing else: `j`/`k`, `↑`/`↓`, `PgDn`/`PgUp`, `g`/`G`,
//! `Home`/`End`, `q`, Ctrl-C, `?`, `/`, `R`, `Esc`, `d`, `enter`, `b`. Anything else is
//! [`Key::Ignored`] on purpose — a key that does something nobody wrote down is how a TUI acquires a
//! second, undocumented surface.

use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};

use fathom::metrics::vocab::ActionState;
use fathom::tui::entry::{Bench, Churn, Entry, Metric};
use fathom::tui::keys::{classify, key, key_with};
use fathom::tui::state::{App, Command, Key};
use fathom::tui::store_view::{Reading, StoreState};

/// The clock the fixtures are aged from.
const NOW: i64 = 1_767_225_600;

/// Press one key, from the byte a terminal would have sent.
fn press(app: &mut App, code: KeyCode) -> Vec<Command> {
    app.on_key(classify(key(code)))
}

/// Press one key with modifiers.
fn press_with(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> Vec<Command> {
    app.on_key(classify(key_with(code, modifiers)))
}

/// Type a filter, one character at a time, from the bytes a terminal would have sent.
///
/// Every character goes through [`classify`] rather than straight in as [`Key::Text`], because that is
/// the difference between testing the filter and testing that the filter can be typed at all: `registry`
/// contains `g` and `t`, which are commands on this screen, and a mapping that ate them would make the
/// word untypeable — which is exactly the bug the two-layer key model exists to avoid.
fn type_filter(app: &mut App, text: &str) {
    press(app, KeyCode::Char('/'));
    for character in text.chars() {
        press(app, KeyCode::Char(character));
    }
}

/// A screen over `count` commits, five rows tall, so paging has something to page through.
fn app_with(count: usize) -> App {
    let entries = (0..count)
        .map(|index| Entry {
            sha: format!("{index:040x}"),
            parent: None,
            author_utc: NOW - i64::try_from(index).unwrap_or(0) * 3_600,
            committer_utc: NOW - i64::try_from(index).unwrap_or(0) * 3_600,
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
        })
        .collect();
    let mut app = App::new(
        Reading {
            repo: "act-mcp".to_owned(),
            branch: "master".to_owned(),
            head: Some("0000000000000000000000000000000000000000".to_owned()),
            entries,
            store: StoreState::Present { rows: count },
            last_run: None,
            evidence_present: true,
            evidence_path: std::path::PathBuf::from(".evidence"),
            ..Reading::default()
        },
        NOW,
    );
    app.set_size(100, 5);
    app
}

// ---------------------------------------------------------------------------------------------
// the mapping, before the meaning
// ---------------------------------------------------------------------------------------------

#[test]
fn every_documented_key_is_mapped_and_nothing_else_is() {
    // The structural half: the keys with no character to misread. A printable character is *not* here,
    // because a letter means different things on the two screens and only the screen decides (see
    // `letters_are_the_screens_to_decide`, and `keys.rs` for why that split is not tidiness).
    for (code, expected) in [
        (KeyCode::Down, Key::Down),
        (KeyCode::Up, Key::Up),
        (KeyCode::PageDown, Key::PageDown),
        (KeyCode::PageUp, Key::PageUp),
        (KeyCode::Home, Key::Top),
        (KeyCode::End, Key::Bottom),
        (KeyCode::Esc, Key::Escape),
        (KeyCode::Enter, Key::Detail),
        (KeyCode::Backspace, Key::Backspace),
        // Tab and Shift-Tab are the diff view's next/previous file, and no filter can contain them.
        (KeyCode::Tab, Key::NextFile),
        (KeyCode::BackTab, Key::PrevFile),
        (KeyCode::F(1), Key::Ignored),
    ] {
        assert_eq!(classify(key(code)), expected, "{code:?}");
    }
    assert_eq!(classify(key(KeyCode::Char('x'))), Key::Text('x'));
    assert_eq!(classify(key(KeyCode::Char('d'))), Key::Text('d'));
    // Ctrl-C is a key in raw mode, not a signal: quitting cleanly is the normal path.
    assert_eq!(
        classify(key_with(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        Key::Quit
    );
    // Some terminals send a press and a release; only the press acts.
    let mut release = key(KeyCode::Char('j'));
    release.kind = KeyEventKind::Release;
    assert_eq!(classify(release), Key::Ignored);
}

#[test]
fn letters_are_the_screens_to_decide() {
    // The same byte, two answers, and the `App` in front of each screen is what picks. This is the whole
    // reason the mapping has two halves: `/registry` has to be typeable, and `t` has to be a command in
    // the view it belongs to.
    let mut timeline = app_with(3);
    press(&mut timeline, KeyCode::Char('j'));
    assert_eq!(timeline.selection, 1, "j moves on the timeline");
    // The letters that mean something in the diff view do nothing here, exactly like `x` does: they are
    // needle characters first, which is what lets `/registry` be typed at all.
    for code in [KeyCode::Char('n'), KeyCode::Char('p'), KeyCode::Char('t')] {
        press(&mut timeline, code);
        assert_eq!(
            timeline.selection, 1,
            "{code:?} moves nothing on the timeline"
        );
        assert!(timeline.message.is_none(), "{code:?} says nothing here");
    }
    // `Tab` cannot be a needle, so it is a command already at the structural layer, and on this screen
    // it answers by saying which screen wants it.
    press(&mut timeline, KeyCode::Tab);
    assert!(timeline
        .message
        .clone()
        .is_some_and(|line| line.contains("d opens one")));
    // `d` is the one of them that does have a meaning here, and it opens a diff rather than scrolling
    // one: the ticket's named interaction, and the only new key the timeline had to give up.
    assert_eq!(press(&mut timeline, KeyCode::Char('d')).len(), 1);
    assert!(matches!(
        timeline.screen,
        fathom::tui::diff_view::Screen::Diff(_)
    ));

    let mut with_diff = app_with(3);
    press(&mut with_diff, KeyCode::Char('d'));
    assert!(matches!(
        with_diff.screen,
        fathom::tui::diff_view::Screen::Diff(_)
    ));
    press(&mut with_diff, KeyCode::Char('j'));
    press(&mut with_diff, KeyCode::Char('t'));
    assert!(
        with_diff.diff_view().is_some_and(|view| view.show_all),
        "t is the diff view's noise toggle"
    );
}

// ---------------------------------------------------------------------------------------------
// moving
// ---------------------------------------------------------------------------------------------

#[test]
fn j_k_and_the_arrows_move_one_row_and_clamp_at_both_ends() {
    let mut app = app_with(3);
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.selection, 1);
    press(&mut app, KeyCode::Down);
    assert_eq!(app.selection, 2);
    // Past the last row: the cursor stops, it does not wrap and it does not point at nothing.
    for _ in 0..10 {
        press(&mut app, KeyCode::Char('j'));
    }
    assert_eq!(app.selection, 2, "the selection clamps at the last commit");
    press(&mut app, KeyCode::Char('k'));
    assert_eq!(app.selection, 1);
    press(&mut app, KeyCode::Up);
    assert_eq!(app.selection, 0);
    for _ in 0..10 {
        press(&mut app, KeyCode::Char('k'));
    }
    assert_eq!(app.selection, 0, "and at the first");
    assert_eq!(app.offset, 0, "and the view follows the clamp");
}

#[test]
fn page_up_and_down_move_one_screen_and_stop_at_the_ends() {
    let mut app = app_with(50);
    assert_eq!(app.page(), 5, "the page is the table's height");
    press(&mut app, KeyCode::PageDown);
    assert_eq!(app.selection, 5);
    press(&mut app, KeyCode::PageDown);
    assert_eq!(app.selection, 10);
    for _ in 0..20 {
        press(&mut app, KeyCode::PageDown);
    }
    assert_eq!(app.selection, 49);
    press(&mut app, KeyCode::PageUp);
    assert_eq!(app.selection, 44);
    for _ in 0..20 {
        press(&mut app, KeyCode::PageUp);
    }
    assert_eq!(app.selection, 0);
}

#[test]
fn g_and_g_jump_to_the_two_ends_and_survive_an_empty_list() {
    let mut app = app_with(50);
    press(&mut app, KeyCode::Char('G'));
    assert_eq!(app.selection, 49);
    assert!(app.offset > 0, "the view scrolled with the cursor");
    press(&mut app, KeyCode::Char('g'));
    assert_eq!(app.selection, 0);
    assert_eq!(app.offset, 0);
    press(&mut app, KeyCode::End);
    assert_eq!(app.selection, 49);
    press(&mut app, KeyCode::Home);
    assert_eq!(app.selection, 0);

    // The same keys on a screen with nothing in it: no panic, no movement, no message.
    let mut empty = app_with(0);
    for code in [KeyCode::Char('G'), KeyCode::Char('g'), KeyCode::Char('j')] {
        press(&mut empty, code);
    }
    assert_eq!(empty.selection, 0);
    assert!(empty.visible().is_empty());
}

// ---------------------------------------------------------------------------------------------
// quitting, help, siblings
// ---------------------------------------------------------------------------------------------

#[test]
fn q_and_ctrl_c_quit_and_say_so_once() {
    for (code, modifiers) in [
        (KeyCode::Char('q'), KeyModifiers::NONE),
        (KeyCode::Char('c'), KeyModifiers::CONTROL),
    ] {
        let mut app = app_with(3);
        let commands = press_with(&mut app, code, modifiers);
        assert!(app.quit, "{code:?} should quit");
        assert_eq!(commands, vec![Command::Quit], "and the loop is told once");
    }
}

#[test]
fn help_opens_on_the_same_key_it_closes_and_esc_also_closes_it() {
    let mut app = app_with(3);
    press(&mut app, KeyCode::Char('?'));
    assert!(app.help);
    // With the overlay up, scrolling still works: a reader who left `?` open is still reading.
    press(&mut app, KeyCode::Char('j'));
    assert!(app.help && app.selection == 1);
    press(&mut app, KeyCode::Esc);
    assert!(!app.help, "esc closes the overlay");
    press(&mut app, KeyCode::Char('?'));
    assert!(app.help);
    press(&mut app, KeyCode::Char('?'));
    assert!(!app.help, "and the key that opened it closes it");
}

/// `enter` still belongs to a sibling ticket (mact-by1.14's commit detail). `d` and `b` are this epic's
/// and are wired for real; what is left here is wired to an answer, not to nothing: the line names the
/// ticket, so "it did not respond" is never the impression.
#[test]
fn the_sibling_keys_answer_with_the_ticket_that_owns_them() {
    let mut app = app_with(3);
    let commands = press(&mut app, KeyCode::Enter);
    assert!(commands.is_empty(), "enter starts no work");
    let message = app.message.clone().unwrap_or_default();
    assert!(
        message.contains("mact-by1.14") && message.contains("not wired yet"),
        "enter should name mact-by1.14: {message}"
    );
    // The message is one keypress's worth of life: the next key clears it.
    let mut app = app_with(3);
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Char('j'));
    assert!(
        !app.message
            .clone()
            .unwrap_or_default()
            .contains("mact-by1.14"),
        "a stale explanation of an earlier key is noise: {:?}",
        app.message
    );
}

#[test]
fn an_undocumented_key_does_nothing_at_all() {
    let mut app = app_with(3);
    let before = (
        app.selection,
        app.offset,
        app.help,
        app.quit,
        app.message.clone(),
    );
    assert!(press(&mut app, KeyCode::Char('x')).is_empty());
    assert_eq!(
        (
            app.selection,
            app.offset,
            app.help,
            app.quit,
            app.message.clone()
        ),
        before,
        "a letter with no meaning here changes nothing at all"
    );
    // `Tab` is not undocumented any more — it is the diff view's next file — so on the timeline it
    // answers with where it belongs rather than with silence. Nothing moves either way.
    press(&mut app, KeyCode::Tab);
    assert_eq!(
        (app.selection, app.offset, app.help, app.quit),
        (before.0, before.1, before.2, before.3)
    );
    assert!(app
        .message
        .clone()
        .is_some_and(|line| line.contains("d opens one")));
}

// ---------------------------------------------------------------------------------------------
// the filter
// ---------------------------------------------------------------------------------------------

#[test]
fn the_filter_is_live_while_it_is_being_typed_and_committed_by_enter() {
    // Three rows: one findable by message, one by author, one by nothing, so a filter has to be
    // matching *something specific* rather than matching the substring everything shares.
    let mut app = app_with(3);
    app.reading.entries[1].subject = "registry: sweep evicted runs".to_owned();
    app.reading.entries[2].author = "Zephyrine".to_owned();
    let mut app = App::new(app.reading.clone(), NOW);
    app.set_size(100, 5);

    type_filter(&mut app, "registry");
    assert!(
        app.filtering(),
        "the prompt is open, so the screen is filtering live"
    );
    assert_eq!(app.visible().len(), 1, "and the count moved while typing");
    assert_eq!(
        app.filter_count(),
        "1/3",
        "the header's count is shown/total"
    );
    // Backspace takes the needle back one character at a time and the count follows.
    for _ in 0..5 {
        press(&mut app, KeyCode::Backspace);
    }
    assert_eq!(app.visible().len(), 1, "\"reg\" still matches the one row");
    for _ in 0..3 {
        press(&mut app, KeyCode::Backspace);
    }
    assert_eq!(app.visible().len(), 3, "an empty needle is no needle");
    press(&mut app, KeyCode::Esc);
    assert_eq!(
        app.filter_prompt, None,
        "the prompt is closed, so the next `/` opens a fresh one"
    );
    // A SHA works, a full SHA works, an author works, all case-insensitively.
    type_filter(&mut app, "ZEPHYRINE");
    assert_eq!(app.visible().len(), 1);
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.filter, Some("ZEPHYRINE".to_owned()));
    assert_eq!(app.filter_prompt, None, "enter closes the prompt");
    // `/` re-opens the live text, so the key edits the filter rather than replacing it.
    press(&mut app, KeyCode::Char('/'));
    assert_eq!(app.filter_prompt.as_deref(), Some("ZEPHYRINE"));
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.filter_prompt, None, "esc discards the edit");
    assert_eq!(
        app.filter.as_deref(),
        Some("ZEPHYRINE"),
        "and keeps what worked"
    );
    // …which a second esc clears entirely.
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.filter, None);
    assert_eq!(app.visible().len(), 3);
}

#[test]
fn a_filter_that_matches_nothing_says_what_to_do_about_it() {
    let mut app = app_with(3);
    type_filter(&mut app, "nothing-matches-this");
    assert!(app.visible().is_empty(), "nothing matches");
    assert_eq!(app.selection, 0, "and the cursor has nowhere to be");
    let step = app.next_step().join("\n");
    assert!(
        step.contains("nothing-matches-this") && step.contains("esc clears"),
        "the empty table must name the needle and the undo: {step}"
    );
    assert!(
        step.contains("3 commits hidden"),
        "and how far off it is: {step}"
    );
    // Typing still works: the needle is a live thing, and one more character can bring the rows back.
    for _ in 0.."nothing-matches-this".len() {
        press(&mut app, KeyCode::Backspace);
    }
    assert_eq!(
        app.visible().len(),
        3,
        "clearing it one key at a time works"
    );
}

// ---------------------------------------------------------------------------------------------
// the index, and the one key that stops it
// ---------------------------------------------------------------------------------------------

#[test]
fn r_asks_for_an_index_once_and_esc_cancels_the_run_it_started() {
    let mut app = app_with(3);
    let commands = press(&mut app, KeyCode::Char('R'));
    assert_eq!(commands, vec![Command::Index]);
    assert!(
        app.index.is_running(),
        "the header says indexing from the keypress"
    );
    // A second `R` does not start a second run and says why.
    let again = press(&mut app, KeyCode::Char('R'));
    assert!(again.is_empty(), "one run at a time");
    assert!(
        app.message
            .clone()
            .unwrap_or_default()
            .contains("already running"),
        "{:?}",
        app.message
    );
    let commands = press(&mut app, KeyCode::Esc);
    assert_eq!(commands, vec![Command::Cancel], "esc asks it to stop");
}

#[test]
fn esc_with_nothing_running_clears_the_filter_and_then_does_nothing() {
    let mut app = app_with(3);
    assert!(
        press(&mut app, KeyCode::Esc).is_empty(),
        "nothing to cancel"
    );
    app.on_key(Key::Filter);
    app.on_key(Key::Text('x'));
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.filter.as_deref(), Some("x"));
    assert!(
        press(&mut app, KeyCode::Esc).is_empty(),
        "esc cleared the filter instead"
    );
    assert_eq!(app.filter, None);
}

#[test]
fn a_re_read_keeps_the_cursor_on_the_same_commit_not_the_same_row_number() {
    let mut app = app_with(3);
    press(&mut app, KeyCode::Char('j'));
    let selected = app.selected().expect("a row").sha.clone();
    assert_eq!(app.selection, 1);

    // An index published, and the walk found an older commit: the list grew at the *end*. A newest-first
    // timeline that grew at the front is the same problem in the other direction, and the rule is the
    // same either way — the cursor belongs to the commit, not to the row number.
    let mut reading = app_with(3).reading.clone();
    reading.entries.push(Entry {
        sha: "older0000000000000000000000000000000000000000".to_owned(),
        ..reading.entries[0].clone()
    });
    reading.entries[3].sha = "older0000000000000000000000000000000000000000".to_owned();
    let _ = app.apply(fathom::tui::engine::Event::Read(Box::new(reading)));
    assert_eq!(
        app.selected().map(|entry| entry.sha.clone()),
        Some(selected),
        "the cursor stays on the commit it was on"
    );
}

#[test]
fn a_re_read_that_removes_the_selected_commit_leaves_the_cursor_legal() {
    let mut app = app_with(3);
    press(&mut app, KeyCode::Char('G'));
    assert_eq!(app.selection, 2);
    // The window narrowed: two of the three rows are gone, and the selected one with them.
    let mut reading = app_with(1).reading.clone();
    reading.entries.truncate(1);
    let _ = app.apply(fathom::tui::engine::Event::Read(Box::new(reading)));
    assert_eq!(app.selection, 0);
    assert_eq!(app.visible().len(), 1);
    assert!(
        app.selected().is_some(),
        "a screen with a row in it always has a cursor on it"
    );
}
