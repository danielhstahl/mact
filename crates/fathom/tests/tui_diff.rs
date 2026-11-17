//! The diff screen: the `d` interaction, the file pane's keys, and what the two panes say.
//!
//! Two kinds of test live here, and they are deliberately different machines.
//!
//! The **state** half is `App` with a [`DiffView`] in front of it: keys in through
//! [`fathom::tui::keys::classify`], commands and field changes out. It covers what the ticket names —
//! `d` opens, `Esc`/`d` closes, `n`/`p` cycle the files, scrolling is per file, `t` changes which files
//! are in view *and* the totals in the header — plus the parts of the interaction that are easy to get
//! wrong and hard to see: the timeline's selection is untouched by a trip through a diff, a view survives
//! being backed out of and re-entered, a read that finishes after its view was evicted is dropped, and no
//! key on the diff screen can move the commit cursor.
//!
//! The **drawn** half is `ratatui`'s `TestBackend` again, in the same style as `tests/tui_render.rs`:
//! whole-screen goldens for the states the ticket lists — a populated diff, a rename, a binary file, a
//! hidden file after `t`, a truncated diff with its banner, a refusal, and a terminal too narrow for the
//! file pane. The goldens are printed by `FATHOM_SNAPSHOT_UPDATE=1` rather than by a `.snap` file, so a
//! re-record is a decision a reviewer sees in the diff.
//!
//! The patch text in these fixtures is written out by hand instead of read from git, so that a screen is
//! pinned to *this* text; `tests/diff_reader.rs` is where git's real output is checked.

use crossterm::event::{KeyCode, KeyModifiers};

use fathom::classify::Scope;
use fathom::diff::{CommitDiff, FileDiff, Limits, Line, LineKind, Totals, Truncation};
use fathom::history::{ChangeKind, DiffBase};
use fathom::metrics::vocab::ActionState;
use fathom::tui::diff_view::{DiffView, Screen};
use fathom::tui::engine::Event;
use fathom::tui::entry::{Bench, Churn, Entry, Metric};
use fathom::tui::keys::{classify, key, key_with};
use fathom::tui::palette::Palette;
use fathom::tui::state::{App, Command, Key};
use fathom::tui::store_view::{Reading, StoreState};
use ratatui::backend::TestBackend;
use ratatui::layout::Position;
use ratatui::Terminal;

/// The instant every fixture row is aged from.
const NOW: i64 = 1_767_225_600;
/// The commit the timeline is standing on when `d` is pressed.
const SHA: &str = "3b8e9f1000000000000000000000000000000000";
const PARENT: &str = "8ac22d100000000000000000000000000000000";

// ---------------------------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------------------------

/// A line of patch text, by kind.
fn line(kind: LineKind, text: &str) -> Line {
    Line {
        kind,
        text: text.to_owned(),
        cut: false,
    }
}

/// A file with a modify patch of three hunks, the shape almost every test needs: long enough that the
/// pane has to scroll at the heights these tests draw at, which is what makes the scroll assertions
/// assertions.
fn file(path: &str, add: u64, del: u64) -> FileDiff {
    let mut lines = vec![
        line(LineKind::Meta, &format!("diff --git a/{path} b/{path}")),
        line(LineKind::Meta, "index 1111111..2222222 100644"),
        line(LineKind::Meta, &format!("--- a/{path}")),
        line(LineKind::Meta, &format!("+++ b/{path}")),
    ];
    for hunk in 0..3 {
        let from = 1 + hunk * 10;
        lines.push(line(
            LineKind::Hunk,
            &format!("@@ -{from},4 +{},5 @@", from + 1),
        ));
        lines.push(line(LineKind::Context, " use std::fmt;"));
        lines.push(line(LineKind::Context, " use std::path::Path;"));
        lines.push(line(LineKind::Delete, "-use std::io;"));
        lines.push(line(LineKind::Add, "+use std::io::Read;"));
        lines.push(line(LineKind::Add, "+use std::io::Write;"));
        lines.push(line(LineKind::Context, " "));
    }
    FileDiff {
        path: path.to_owned(),
        old_path: None,
        kind: ChangeKind::Modify,
        add,
        del,
        binary: false,
        scope: CLASSIFIER.classify(path),
        lines,
        hunks: 3,
        cut: false,
        unseen: false,
        dropped: 0,
    }
}

/// A file with no patch text of its own, for the `kind`-and-counts cases.
fn bare(path: &str, kind: ChangeKind, add: u64, del: u64, binary: bool) -> FileDiff {
    FileDiff {
        path: path.to_owned(),
        old_path: None,
        kind,
        add,
        del,
        binary,
        scope: CLASSIFIER.classify(path),
        lines: Vec::new(),
        hunks: 0,
        cut: false,
        unseen: false,
        dropped: 0,
    }
}

/// The classifier the fixtures are scoped with — the embedded default, i.e. the same answer the
/// timeline's `+`/`−` column is built from, so a fixture cannot disagree with itself.
static CLASSIFIER: std::sync::LazyLock<fathom::classify::Classifier> =
    std::sync::LazyLock::new(fathom::classify::Classifier::default);

/// The commit under test: two source files and one lockfile, so the filter has something to hide.
fn commit() -> CommitDiff {
    let mut diff = CommitDiff {
        sha: SHA.to_owned(),
        short_id: "3b8e9f1".to_owned(),
        subject: "coverage: count the tests that came after the fix".to_owned(),
        parents: vec![PARENT.to_owned()],
        diff_base: DiffBase::Parent {
            sha: PARENT.to_owned(),
        },
        files: vec![
            file("src/metrics/coverage.rs", 82, 31),
            file("src/metrics/mod.rs", 4, 1),
            file("Cargo.lock", 12, 3),
        ],
        totals: Totals::default(),
        truncation: None,
        bytes_retained: 4_096,
        bytes_streamed: 4_096,
        limits: Limits::default(),
        warnings: Vec::new(),
    };
    recompute(&mut diff);
    diff
}

/// Fill in `totals` from the files, split the way the reader splits them, so a fixture cannot contradict
/// its own file list.
fn recompute(diff: &mut CommitDiff) {
    let mut totals = Totals::default();
    for f in &diff.files {
        totals.files_all += 1;
        totals.add_all += f.add;
        totals.del_all += f.del;
        if f.scope == Scope::Source {
            totals.files += 1;
            totals.add += f.add;
            totals.del += f.del;
        } else {
            totals.hidden += 1;
        }
    }
    diff.totals = totals;
}

/// An app over `count` commits, one of which is [`SHA`], with the diff view already open on it.
fn app_with(count: usize) -> App {
    let entries = (0..count)
        .map(|index| Entry {
            sha: if index == 0 {
                SHA.to_owned()
            } else {
                format!("{index:040x}")
            },
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
            head: Some(SHA.to_owned()),
            entries,
            store: StoreState::Present { rows: count },
            last_run: None,
            evidence_present: true,
            evidence_path: std::path::PathBuf::from(".evidence"),
            ..Reading::default()
        },
        NOW,
    );
    app.set_size(100, 20);
    app
}

/// Open the diff view on the selected commit, already read — the state after `d` and the engine's answer.
fn open(app: &mut App, diff: CommitDiff) -> Vec<Command> {
    let commands = app.on_key(classify(key(KeyCode::Char('d'))));
    let sha = app.reading.entries[app.selection].sha.clone();
    deliver(app, &sha, Ok(diff));
    commands
}

/// Put a diff on screen that has already been read, whether or not a view is open: what `--diff REV`
/// does, and what a test uses when it wants to look at a second commit without the trip back.
fn show(app: &mut App, diff: CommitDiff) {
    let mut view = DiffView::open(&diff.sha);
    view.loading = false;
    view.diff = Some(diff);
    view.settle();
    app.show_diff(view);
}

/// Hand the app a diff read's outcome, the way the loop would.
fn deliver(app: &mut App, sha: &str, outcome: Result<CommitDiff, String>) {
    app.apply(Event::Diff {
        sha: sha.to_owned(),
        outcome: outcome.map(Box::new),
    });
}

fn press(app: &mut App, code: KeyCode) -> Vec<Command> {
    app.on_key(classify(key(code)))
}

fn press_with(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> Vec<Command> {
    app.on_key(classify(key_with(code, modifiers)))
}

/// The open view, unwrapped: every test below has one open.
fn view(app: &App) -> &DiffView {
    app.diff_view().expect("a diff view is open")
}

fn view_mut(app: &mut App) -> &mut DiffView {
    app.diff_view_mut().expect("a diff view is open")
}

// ---------------------------------------------------------------------------------------------
// the interaction
// ---------------------------------------------------------------------------------------------

#[test]
fn d_opens_the_diff_of_the_selected_commit_and_nothing_else_changes() {
    let mut app = app_with(4);
    press(&mut app, KeyCode::Down);
    let selection = app.selection;
    let commands = app.on_key(classify(key(KeyCode::Char('d'))));

    assert_eq!(
        commands,
        vec![Command::Diff(app.reading.entries[selection].sha.clone())],
        "`d` asks for exactly one read, of the selected commit"
    );
    assert!(matches!(&app.screen, Screen::Diff(sha) if sha == &app.reading.entries[selection].sha));
    assert_eq!(app.selection, selection, "the timeline cursor did not move");
    let view = view(&app);
    assert!(view.loading, "and the pane says what is waiting");
    assert_eq!(view.sha, app.reading.entries[selection].sha);
}

#[test]
fn d_with_nothing_selected_says_so_instead_of_reading_a_guess() {
    let mut app = app_with(0);
    let commands = app.on_key(classify(key(KeyCode::Char('d'))));
    assert!(commands.is_empty(), "nothing was asked for");
    assert_eq!(app.screen, Screen::Timeline);
    assert!(
        app.message.is_some(),
        "and the refusal is on the status line"
    );
}

#[test]
fn esc_and_d_both_leave_and_the_view_survives_the_trip() {
    for leave in [KeyCode::Esc, KeyCode::Char('d')] {
        let mut app = app_with(3);
        open(&mut app, commit());
        view_mut(&mut app).select_file(1);
        view_mut(&mut app).scroll_by(3);
        let before = (
            view(&app).file,
            view(&app).scroll(),
            view(&app).shown_position(),
        );

        app.on_key(classify(key(leave)));
        assert_eq!(app.screen, Screen::Timeline, "{leave:?} closes the diff");
        assert!(
            app.diffs.contains_key(SHA),
            "{leave:?} leaves the view where it was, ready to come back"
        );

        // Coming back in does not re-read it and does not move the reader's place.
        let commands = app.on_key(classify(key(KeyCode::Char('d'))));
        assert!(
            commands.is_empty(),
            "a cached diff is not read twice: {commands:?}"
        );
        assert_eq!(app.screen, Screen::Diff(SHA.to_owned()));
        let after = (
            view(&app).file,
            view(&app).scroll(),
            view(&app).shown_position(),
        );
        assert_eq!(before, after, "same file, same line, same counter");
    }
}

#[test]
fn a_trip_through_a_diff_leaves_the_timeline_exactly_as_it_was() {
    let mut app = app_with(6);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Char('/'));
    press(&mut app, KeyCode::Char('c'));
    press(&mut app, KeyCode::Esc); // leave the filter, empty needle and all
    let before = (
        app.selection,
        app.offset,
        app.filter.clone(),
        app.reading.entries.len(),
    );

    open(&mut app, commit());
    for code in [
        KeyCode::Down,
        KeyCode::Up,
        KeyCode::PageDown,
        KeyCode::PageUp,
        KeyCode::Home,
        KeyCode::End,
        KeyCode::Char('n'),
        KeyCode::Char('p'),
        KeyCode::Char('t'),
        KeyCode::Tab,
        KeyCode::BackTab,
    ] {
        press(&mut app, code);
    }
    press(&mut app, KeyCode::Esc);

    assert_eq!(
        (
            app.selection,
            app.offset,
            app.filter.clone(),
            app.reading.entries.len()
        ),
        before,
        "not one of those keys may move the commit cursor, scroll the table or touch the filter"
    );
}

#[test]
fn q_and_ctrl_c_quit_from_either_screen() {
    let mut app = app_with(3);
    open(&mut app, commit());
    assert_eq!(press(&mut app, KeyCode::Char('q')), vec![Command::Quit]);
    let mut app = app_with(3);
    open(&mut app, commit());
    assert_eq!(
        press_with(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL),
        vec![Command::Quit]
    );
}

#[test]
fn a_read_that_arrives_after_its_view_was_evicted_is_dropped() {
    let mut app = app_with(3);
    open(&mut app, commit());
    // Open enough other commits that this one is out of the cache, then let its answer land.
    for index in 0..14 {
        let sha = format!("{index:040x}");
        app.show_diff(DiffView::open(&sha));
    }
    assert!(!app.diffs.contains_key(SHA));
    let commands = app.apply(Event::Diff {
        sha: SHA.to_owned(),
        outcome: Ok(Box::new(commit())),
    });
    assert!(
        commands.is_empty(),
        "nobody is asking, so nothing is drawn or re-read: {commands:?}"
    );
}

#[test]
fn a_refusal_is_the_pane_and_the_status_line_not_an_empty_screen() {
    let mut app = app_with(2);
    press(&mut app, KeyCode::Char('d'));
    let commands = app.apply(Event::Diff {
        sha: app.reading.entries[0].sha.clone(),
        outcome: Err("not a revision: nope — fix: press d on a row of the timeline".to_owned()),
    });
    assert!(
        commands.is_empty(),
        "a refusal does not trigger another read"
    );
    let view = view(&app);
    assert!(!view.loading, "the read finished, one way or the other");
    assert!(
        view.status().contains("not a revision"),
        "{:?}",
        view.status()
    );
    assert_eq!(
        view.pane_lines().len(),
        1,
        "and the pane says the same thing"
    );
}

// ---------------------------------------------------------------------------------------------
// the file pane's keys
// ---------------------------------------------------------------------------------------------

#[test]
fn n_and_p_cycle_the_files_and_wrap_at_both_ends() {
    let mut app = app_with(2);
    open(&mut app, commit());
    assert_eq!(view(&app).file, 0);
    // Cargo.lock is hidden by the default filter, so the cycle is over the two source files only.
    assert_eq!(view(&app).shown(), vec![0, 1]);

    press(&mut app, KeyCode::Char('n'));
    assert_eq!(view(&app).file, 1);
    press(&mut app, KeyCode::Char('n'));
    assert_eq!(
        view(&app).file,
        0,
        "and wraps at the end rather than sticking"
    );
    press(&mut app, KeyCode::Char('p'));
    assert_eq!(view(&app).file, 1, "and at the beginning");

    // Tab and Shift-Tab are the same command: they cannot be needle characters, so they are commands
    // even while a filter is being typed, and the diff screen has no filter to protect.
    press(&mut app, KeyCode::Tab);
    assert_eq!(view(&app).file, 0);
    press_with(&mut app, KeyCode::BackTab, KeyModifiers::SHIFT);
    assert_eq!(view(&app).file, 1);
    assert_eq!(classify(key(KeyCode::Tab)), Key::NextFile);
}

#[test]
fn t_shows_the_hidden_files_and_the_header_totals_go_with_them() {
    let mut app = app_with(2);
    open(&mut app, commit());
    let (add_before, del_before, files_before) = commit().totals.shown(false);
    let (add_all, del_all, files_all) = commit().totals.shown(true);
    assert_eq!(view(&app).shown().len(), 2, "the lockfile is hidden");
    assert!(view(&app).status().contains("hidden by the filters"));

    press(&mut app, KeyCode::Char('t'));
    assert!(view(&app).show_all);
    assert_eq!(view(&app).shown().len(), 3, "and `t` shows it");
    let (_, right) = view(&app).header(&Palette::interactive());
    assert!(
        right.contains(&format!("+{add_all} -{del_all}"))
            && right.contains(&format!("{files_all} files")),
        "the header's numbers are the set the list is showing: {right}"
    );
    assert!(
        add_all > add_before && del_all > del_before && files_all > files_before,
        "and the two sets really do differ in this fixture"
    );
    assert!(view(&app).status().contains("t hides"));

    press(&mut app, KeyCode::Char('t'));
    assert!(!view(&app).show_all);
    assert_eq!(
        view(&app).file,
        0,
        "the cursor was on a source file, so turning the filter off leaves it where it was"
    );
}

#[test]
fn turning_the_filter_off_from_a_file_it_just_hid_leaves_the_cursor_in_the_list() {
    let mut app = app_with(2);
    open(&mut app, commit());
    press(&mut app, KeyCode::Char('t'));
    // The lockfile is the third file, and it is only reachable while `t` is on.
    view_mut(&mut app).select_file(2);
    assert_eq!(view(&app).shown_position(), Some(2));
    press(&mut app, KeyCode::Char('t'));
    assert!(!view(&app).show_all);
    assert!(
        view(&app).shown().contains(&view(&app).file),
        "the cursor may not stay on a file the list is no longer showing: {}",
        view(&app).file
    );
    assert_eq!(view(&app).shown_position(), Some(0));
    let (_, right) = view(&app).header(&Palette::interactive());
    assert!(
        !right.contains('—'),
        "and the counter says a real position, not a dash: {right}"
    );
}

#[test]
fn scroll_is_per_file_and_survives_the_walk_between_files() {
    let mut app = app_with(2);
    open(&mut app, commit());
    let height = usize::from(view(&app).height);
    assert!(height > 1, "the pane has room to scroll in");

    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Down);
    assert_eq!(view(&app).scroll(), 2);
    assert_eq!(view(&app).scrolls.len(), 1, "one file's offset so far");

    press(&mut app, KeyCode::Char('n'));
    assert_eq!(view(&app).file, 1);
    assert_eq!(
        view(&app).scroll(),
        0,
        "the next file starts at its own top"
    );
    press(&mut app, KeyCode::Down);
    assert_eq!(view(&app).scrolls.len(), 2, "and keeps its own offset");

    press(&mut app, KeyCode::Char('p'));
    assert_eq!(
        view(&app).scroll(),
        2,
        "the first file is still where it was left"
    );
}

#[test]
fn scrolling_clamps_at_both_ends_and_page_is_the_panes_height() {
    let mut app = app_with(2);
    app.set_size(100, 12);
    open(&mut app, commit());
    let lines = view(&app).pane_lines().len();
    let height = usize::from(view(&app).height);
    let last = lines.saturating_sub(height);
    assert!(
        last > height,
        "the fixture must be tall enough to page through"
    );

    for _ in 0..40 {
        press(&mut app, KeyCode::Down);
    }
    assert_eq!(
        view(&app).scroll(),
        last,
        "the bottom is the last full screen"
    );
    assert_eq!(view(&app).scroll(), view(&app).last_scroll());
    // `End` is the same place by another route.
    press(&mut app, KeyCode::Home);
    assert_eq!(view(&app).scroll(), 0);
    press(&mut app, KeyCode::End);
    assert_eq!(view(&app).scroll(), last);
    for _ in 0..40 {
        press(&mut app, KeyCode::Up);
    }
    assert_eq!(view(&app).scroll(), 0, "and it does not go negative");

    // PageDown on the diff screen scrolls a pane rather than a table.
    press(&mut app, KeyCode::PageDown);
    assert_eq!(view(&app).scroll(), height);
    press(&mut app, KeyCode::PageUp);
    assert_eq!(view(&app).scroll(), 0);
}

#[test]
fn a_short_file_does_not_scroll_at_all() {
    let mut app = app_with(2);
    let mut diff = commit();
    diff.files[1].lines.truncate(3);
    open(&mut app, diff);
    view_mut(&mut app).select_file(1);
    for _ in 0..10 {
        press(&mut app, KeyCode::Down);
    }
    assert_eq!(
        view(&app).last_scroll(),
        0,
        "the file is shorter than the pane"
    );
    assert_eq!(view(&app).scroll(), 0, "so there is nothing to scroll");
}

#[test]
fn a_view_reopened_after_a_re_read_lands_inside_what_is_actually_there() {
    // The case the counters cannot see: a diff is read, the reader scrolls to file 5, and a re-read (or
    // a narrower terminal) leaves fewer files or fewer lines. `settle` is what stops the pane drawing
    // nothing because the cursor is off the end.
    let mut app = app_with(2);
    open(&mut app, commit());
    view_mut(&mut app).select_file(9);
    view_mut(&mut app).scroll_by(500);
    view_mut(&mut app).settle();
    assert!(view(&app).file < view(&app).all_files().len());
    assert!(view(&app).scroll() <= view(&app).last_scroll());
}

// ---------------------------------------------------------------------------------------------
// what the view says about itself
// ---------------------------------------------------------------------------------------------

#[test]
fn the_header_names_the_base_the_way_the_reader_found_it() {
    let mut app = app_with(2);
    open(&mut app, commit());
    let (left, right) = view(&app).header(&Palette::interactive());
    assert!(
        left.contains("3b8e9f1") && left.contains("coverage:"),
        "{left}"
    );
    assert!(
        right.contains("vs parent 8ac22d1") && right.contains("1/2 files"),
        "{right}"
    );

    // A root commit has no parent to be "vs parent", and a merge says how many parents it has.
    let mut root = commit();
    root.parents = Vec::new();
    root.diff_base = DiffBase::EmptyTree;
    show(&mut app, root);
    assert!(view(&app).base().contains("empty tree (root commit)"));

    let mut merge = commit();
    merge.parents = vec![PARENT.to_owned(); 3];
    show(&mut app, merge);
    assert_eq!(view(&app).base(), "vs parent 8ac22d1 (merge of 3 parents)");
}

#[test]
fn a_binary_file_is_marked_in_the_list_and_explained_in_the_pane() {
    let mut app = app_with(2);
    let mut diff = commit();
    diff.files[1] = bare("assets/logo.png", ChangeKind::Add, 0, 0, true);
    diff.files[1].lines = vec![
        line(
            LineKind::Meta,
            "diff --git a/assets/logo.png b/assets/logo.png",
        ),
        line(LineKind::Meta, "new file mode 100644"),
        line(
            LineKind::Meta,
            "Binary files /dev/null and b/assets/logo.png differ",
        ),
    ];
    recompute(&mut diff);
    open(&mut app, diff);
    let rows = view(&app).rows(60, &Palette::interactive(), true);
    assert!(
        rows.iter().any(|row| row.contains("[binary]")),
        "the list says `[binary]` where the counts would have lied: {rows:?}"
    );
    assert!(
        !rows.iter().any(|row| row.contains("+0 -0")),
        "and not the `+0 -0` git's numstat gives back: {rows:?}"
    );
    view_mut(&mut app).select_file(1);
    assert!(
        view(&app)
            .pane_lines()
            .iter()
            .any(|(_, text)| text.contains("does not print binary content")),
        "the pane says what it is not showing: {:?}",
        view(&app).pane_lines()
    );
}

#[test]
fn a_rename_shows_both_paths_and_a_mode_only_change_says_why_it_has_no_text() {
    let mut app = app_with(2);
    let mut diff = commit();
    // A pure rename: git detects it, and there is no hunk to show, only git's own header lines.
    let mut renamed = bare("src/core.rs", ChangeKind::Rename, 0, 0, false);
    renamed.old_path = Some("src/legacy.rs".to_owned());
    renamed.lines = vec![
        line(LineKind::Meta, "diff --git a/src/legacy.rs b/src/core.rs"),
        line(LineKind::Meta, "similarity index 100%"),
        line(LineKind::Meta, "rename from src/legacy.rs"),
        line(LineKind::Meta, "rename to src/core.rs"),
    ];
    let mode = bare("scripts/run.sh", ChangeKind::TypeChange, 0, 0, false);
    diff.files = vec![renamed, mode];
    recompute(&mut diff);
    open(&mut app, diff);

    let rows = view(&app).rows(60, &Palette::interactive(), true);
    assert!(
        rows[0].contains("src/legacy.rs") && rows[0].contains("src/core.rs"),
        "a rename is shown as both paths, because the diff is about the move: {rows:?}"
    );
    assert!(
        !view(&app)
            .pane_lines()
            .iter()
            .any(|(kind, _)| *kind == LineKind::Hunk),
        "and there is no hunk to draw"
    );

    // The other empty case: a file the reader has counts for and text for none of. It is drawn as its
    // own refusal, because a blank pane is indistinguishable from a bug.
    view_mut(&mut app).select_file(1);
    assert!(
        view(&app).status().contains("no line content to show"),
        "{:?}",
        view(&app).status()
    );
}

#[test]
fn a_cut_diff_says_so_in_the_status_line_and_at_the_end_of_the_file() {
    let mut app = app_with(2);
    let mut diff = commit();
    diff.limits = Limits {
        total: 32_768,
        per_file: 8_192,
    };
    diff.truncation = Some(Truncation {
        total_cut: false,
        files_cut: 1,
        files_unseen: 0,
        retained: 30_000,
        streamed: 120_000,
        limits: diff.limits,
    });
    diff.files[0].cut = true;
    diff.files[0].dropped = 96_000;
    open(&mut app, diff);

    let status = view(&app).status();
    assert!(status.contains("--max-diff-bytes-per-file"), "{status}");
    assert!(status.contains("8 KiB"), "{status}");
    let tail = view(&app)
        .pane_lines()
        .last()
        .expect("the file's own lines")
        .1
        .clone();
    assert!(
        tail.contains("more bytes of this file not kept") && tail.contains("93 KiB"),
        "the pane ends with the bill: {tail}"
    );

    // And the whole-commit form, where files never got read at all.
    let mut app = app_with(2);
    let mut diff = commit();
    diff.truncation = Some(Truncation {
        total_cut: true,
        files_cut: 1,
        files_unseen: 1,
        retained: 4_000,
        streamed: 4_100,
        limits: Limits {
            total: 4_096,
            per_file: 4_096,
        },
    });
    diff.files[2].unseen = true;
    show(&mut app, diff);
    assert!(
        view(&app).status().contains("1 of 3 files not shown")
            && view(&app).status().contains("--max-diff-bytes"),
        "{:?}",
        view(&app).status()
    );
    view_mut(&mut app).set_show_all(true);
    view_mut(&mut app).select_file(2);
    assert!(
        view(&app)
            .pane_lines()
            .iter()
            .any(|(_, text)| text.contains("whole-commit byte cap")),
        "an unread file explains itself: {:?}",
        view(&app).pane_lines()
    );
}

#[test]
fn a_commit_that_changed_nothing_says_that_instead_of_showing_an_empty_pane() {
    let mut app = app_with(2);
    let mut empty = commit();
    empty.files = Vec::new();
    recompute(&mut empty);
    open(&mut app, empty);
    assert!(view(&app)
        .rows(60, &Palette::interactive(), true)
        .is_empty());
    assert!(view(&app).status().contains("changes nothing"));

    let mut merge = commit();
    merge.files = Vec::new();
    merge.parents = vec![PARENT.to_owned(), PARENT.to_owned()];
    recompute(&mut merge);
    show(&mut app, merge);
    assert!(view(&app).status().contains("brings nothing"));
}

#[test]
fn control_bytes_in_a_patch_are_made_visible_rather_than_paid() {
    let mut app = app_with(2);
    let mut diff = commit();
    diff.files[0].lines = vec![
        line(LineKind::Add, "+let a = \"x\u{9}y\";"),
        line(LineKind::Add, "+crlf\r"),
        line(LineKind::Add, "+esc\u{1b}[31mred"),
    ];
    open(&mut app, diff);
    let lines: Vec<String> = view(&app)
        .pane_lines()
        .into_iter()
        .map(|(_, text)| fathom::tui::diff_view::pane_text(&text, 80, &Palette::interactive()))
        .collect();
    assert!(
        lines[0].contains('␉'),
        "a tab in a diff is shown as a tab, not as the spaces it happens to move to: {lines:?}"
    );
    assert!(
        lines[1].contains('␍'),
        "a CRLF line ends visibly: {lines:?}"
    );
    assert!(
        !lines[2].contains('\u{1b}'),
        "an escape sequence in a file cannot paint this terminal: {lines:?}"
    );
    // The ascii surface spells them with caret notation instead.
    let ascii = fathom::tui::diff_view::pane_text("+crlf\r", 20, &Palette::plain());
    assert!(ascii.contains("^M"), "{ascii}");
}

// ---------------------------------------------------------------------------------------------
// drawn
// ---------------------------------------------------------------------------------------------

#[test]
fn the_diff_screen_is_the_two_panes_the_ticket_drew() {
    let mut app = app_with(3);
    open(&mut app, commit());
    check(
        &app,
        &Palette::interactive(),
        100,
        16,
        r#"
DIFF  3b8e9f1  coverage: count the tests that came after th… vs parent 8ac22d1 · +86 -32 · 1/2 files
┌─ files ────────────────────────┬─ diff ──────────────────────────────────────────────────────────┐
│▸ src/metrics/coverage.… +82 -31│diff --git a/src/metrics/coverage.rs b/src/metrics/coverage.rs   │
│  src/metrics/mod.rs       +4 -1│index 1111111..2222222 100644                                    │
│                                │--- a/src/metrics/coverage.rs                                    │
│                                │+++ b/src/metrics/coverage.rs                                    │
│                                │@@ -1,4 +2,5 @@                                                  │
│                                │ use std::fmt;                                                   │
│                                │ use std::path::Path;                                            │
│                                │-use std::io;                                                    │
│                                │+use std::io::Read;                                              │
│                                │+use std::io::Write;                                             │
│                                │                                                                 │
└────────────────────────────────┴─────────────────────────────────────────────────────────────────┘
1 noise/vendored file hidden by the filters · t shows them (the header's +/- goes with them)
1  j/k scroll   n/p file   t all files   g/G ends   d/Esc back   ? help   q quit
"#,
    );
}

#[test]
fn the_same_screen_with_t_on_shows_the_lockfile_and_the_wider_totals() {
    let mut app = app_with(3);
    open(&mut app, commit());
    press(&mut app, KeyCode::Char('t'));
    check(
        &app,
        &Palette::interactive(),
        100,
        16,
        r#"
DIFF  3b8e9f1  coverage: count the tests that came af… vs parent 8ac22d1 · +98 -35 · 1/3 files (all)
┌─ files ────────────────────────┬─ diff ──────────────────────────────────────────────────────────┐
│▸ src/metrics/coverage.… +82 -31│diff --git a/src/metrics/coverage.rs b/src/metrics/coverage.rs   │
│  src/metrics/mod.rs       +4 -1│index 1111111..2222222 100644                                    │
│  ~ Cargo.lock            +12 -3│--- a/src/metrics/coverage.rs                                    │
│                                │+++ b/src/metrics/coverage.rs                                    │
│                                │@@ -1,4 +2,5 @@                                                  │
│                                │ use std::fmt;                                                   │
│                                │ use std::path::Path;                                            │
│                                │-use std::io;                                                    │
│                                │+use std::io::Read;                                              │
│                                │+use std::io::Write;                                             │
│                                │                                                                 │
└────────────────────────────────┴─────────────────────────────────────────────────────────────────┘
showing all 3 files · t hides the 1 noise/vendored again
1  j/k scroll   n/p file   t all files   g/G ends   d/Esc back   ? help   q quit
"#,
    );
}

#[test]
fn a_truncated_diff_draws_its_banner_over_the_panes() {
    let mut app = app_with(3);
    let mut diff = commit();
    diff.limits = Limits {
        total: 16_384,
        per_file: 8_192,
    };
    diff.truncation = Some(Truncation {
        total_cut: true,
        files_cut: 1,
        files_unseen: 1,
        retained: 16_300,
        streamed: 16_400,
        limits: diff.limits,
    });
    diff.files[2].unseen = true;
    diff.files[1].cut = true;
    diff.files[1].dropped = 40_000;
    show(&mut app, diff);
    check(
        &app,
        &Palette::interactive(),
        100,
        14,
        r#"
DIFF  3b8e9f1  coverage: count the tests that came after th… vs parent 8ac22d1 · +86 -32 · 1/2 files
┌─ files ────────────────────────┬─ diff ──────────────────────────────────────────────────────────┐
│▸ src/metrics/coverage.… +82 -31│diff --git a/src/metrics/coverage.rs b/src/metrics/coverage.rs   │
│  src/metrics/mod.rs       +4 -1│index 1111111..2222222 100644                                    │
│                                │--- a/src/metrics/coverage.rs                                    │
│                                │+++ b/src/metrics/coverage.rs                                    │
│                                │@@ -1,4 +2,5 @@                                                  │
│                                │ use std::fmt;                                                   │
│                                │ use std::path::Path;                                            │
│                                │-use std::io;                                                    │
│                                │+use std::io::Read;                                              │
└────────────────────────────────┴─────────────────────────────────────────────────────────────────┘
diff cut at the 16 KiB limit (--max-diff-bytes) · 1 of 3 files not shown, 1 cut short · raise the fl
1  j/k scroll   n/p file   t all files   g/G ends   d/Esc back   ? help   q quit
"#,
    );
}

#[test]
fn a_refusal_is_drawn_in_the_pane_that_could_not_fill() {
    let mut app = app_with(3);
    press(&mut app, KeyCode::Char('d'));
    let sha = app.reading.entries[0].sha.clone();
    app.apply(Event::Diff {
        sha,
        outcome: Err(
            "not a revision: nope \u{2014} fix: press d on a row of the timeline".to_owned(),
        ),
    });
    check(
        &app,
        &Palette::interactive(),
        100,
        12,
        r#"
DIFF  3b8e9f1 —                         not a revision: nope — fix: press d on a row of the timeline
┌─ files ────────────────────────┬─ diff ──────────────────────────────────────────────────────────┐
│                                │not a revision: nope — fix: press d on a row of the timeline     │
│                                │                                                                 │
│                                │                                                                 │
│                                │                                                                 │
│                                │                                                                 │
│                                │                                                                 │
│                                │                                                                 │
└────────────────────────────────┴─────────────────────────────────────────────────────────────────┘
not a revision: nope — fix: press d on a row of the timeline
j/k scroll   n/p file   t all files   g/G ends   d/Esc back   ? help   q quit
"#,
    );
}

#[test]
fn a_read_that_has_not_arrived_says_so_in_both_panes() {
    let mut app = app_with(3);
    press(&mut app, KeyCode::Char('d'));
    check(
        &app,
        &Palette::interactive(),
        100,
        12,
        r#"
DIFF  3b8e9f1 —                                                                    reading 3b8e9f1 …
┌─ files ────────────────────────┬─ diff ──────────────────────────────────────────────────────────┐
│                                │reading 3b8e9f1 …                                                │
│                                │                                                                 │
│                                │                                                                 │
│                                │                                                                 │
│                                │                                                                 │
│                                │                                                                 │
│                                │                                                                 │
└────────────────────────────────┴─────────────────────────────────────────────────────────────────┘
reading 3b8e9f1 — git show — the pane fills when it answers
j/k scroll   n/p file   t all files   g/G ends   d/Esc back   ? help   q quit
"#,
    );
}

#[test]
fn a_narrow_terminal_squeezes_both_panes_and_says_so() {
    let mut app = app_with(3);
    open(&mut app, commit());
    // At 44 columns there is still room for a path column and a patch column, and what gets cut instead
    // is the prose: the header's subject, the paths, the status line and the key line. A pane that
    // silently disappeared would be worse than one whose paths end in an ellipsis.
    check(
        &app,
        &Palette::interactive(),
        44,
        12,
        r#"
DIFF  3b8e9f1  cover…vs parent 8ac22d1 · +8…
┌─ files ──────────┬─ diff ────────────────┐
│▸ src/met… +82 -31│diff --git a/src/metr… │
│  src/metri… +4 -1│index 1111111..222222… │
│                  │--- a/src/metrics/cov… │
│                  │+++ b/src/metrics/cov… │
│                  │@@ -1,4 +2,5 @@        │
│                  │ use std::fmt;         │
│                  │ use std::path::Path;  │
└──────────────────┴───────────────────────┘
1 noise/vendored file hidden by the filters
1  j/k scroll   n/p file   t all files   g/G
"#,
    );
}

#[test]
fn the_help_overlay_on_the_diff_screen_answers_the_questions_a_diff_asks() {
    let mut app = app_with(3);
    open(&mut app, commit());
    app.help = true;
    let drawn = tidy(&screen(&app, &Palette::interactive(), 100, 24));
    for needle in ["first parent", "syntax", "wrap", "d", "t"] {
        assert!(
            drawn.contains(needle),
            "the help should say {needle:?} on the diff screen:\n{drawn}"
        );
    }
}

#[test]
fn the_plain_surface_prints_the_same_commit_and_every_file_of_it() {
    // `fathom tui --plain --diff <rev>` is the same view without a terminal, so it prints the header, the
    // file list and *every* file's patch — there is no key press to select one, and a diff you can only
    // read one file at a time in a pipe is not a diff.
    let mut app = app_with(3);
    open(&mut app, commit());
    let text = fathom::tui::plain::diff(view(&app), &Palette::plain(), 100);
    let lines: Vec<&str> = text.lines().collect();
    assert!(
        lines[0].starts_with("DIFF  3b8e9f1  coverage:"),
        "the same heading the screen draws: {lines:?}"
    );
    assert!(
        lines[0].contains("+98 -35") && lines[0].contains("(all)"),
        "the dump is the whole commit, so its totals are the whole commit's: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("~ Cargo.lock") && line.contains("+12 -3")),
        "and the file the screen hides by default is here: {lines:?}"
    );
    for path in [
        "src/metrics/coverage.rs",
        "src/metrics/mod.rs",
        "Cargo.lock",
    ] {
        assert!(
            text.contains(&format!("diff --git a/{path} b/{path}")),
            "{path}'s patch is printed"
        );
    }
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.starts_with("--- ") && line.contains('+'))
            .count(),
        3,
        "one rule per file's patch, in git's order: {lines:?}"
    );
    // The caps' bill is on the dump too, when there is one.
    let mut cut = commit();
    cut.truncation = Some(Truncation {
        total_cut: true,
        files_cut: 0,
        files_unseen: 1,
        retained: 4_000,
        streamed: 4_100,
        limits: Limits {
            total: 4_096,
            per_file: 4_096,
        },
    });
    cut.files[2].unseen = true;
    show(&mut app, cut);
    let text = fathom::tui::plain::diff(view(&app), &Palette::plain(), 100);
    assert!(
        text.contains("diff cut at the 4 KiB limit (--max-diff-bytes)"),
        "{text}"
    );
    assert!(
        text.contains("not read: the whole-commit byte cap"),
        "and the file that was never read says so: {text}"
    );
}

/// Draw `app` and compare it against a golden, in the style of `tests/tui_render.rs`.
fn check(app: &App, palette: &Palette, width: u16, height: u16, expected: &str) {
    let have = tidy(&screen(app, palette, width, height));
    let want = tidy(expected);
    if have != want && std::env::var("FATHOM_SNAPSHOT_UPDATE").is_ok() {
        println!("--- expected by this test ---\n{want}\n--- drawn now ---\n{have}");
    }
    assert_eq!(have, want, "\ndrawn:\n{have}");
}

/// The screen as text: one `char` per cell, trailing cells trimmed.
fn screen(app: &App, palette: &Palette, width: u16, height: u16) -> String {
    let mut app = app.clone();
    app.set_size(width, height);
    let mut terminal = Terminal::new(TestBackend::new(width, height))
        .expect("a TestBackend cannot fail to initialise");
    terminal
        .draw(|frame| fathom::tui::render::draw(&app, frame, palette))
        .expect("drawing cannot fail on a TestBackend");
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| {
                    buffer.cell(Position::new(x, y)).map_or(" ", |cell| {
                        if cell.symbol().is_empty() {
                            " "
                        } else {
                            cell.symbol()
                        }
                    })
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Trim each line's trailing cells and drop the leading and trailing blank lines.
fn tidy(text: &str) -> String {
    text.lines()
        .map(|line| line.trim_end().to_owned())
        .collect::<Vec<_>>()
        .join("\n")
        .trim_matches('\n')
        .to_owned()
}
