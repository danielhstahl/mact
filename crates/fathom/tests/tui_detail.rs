//! The commit detail view (mact-by1.14): three panes over one commit, one shared file selection, and the
//! caveats the aggregate metrics hide.
//!
//! Two kinds of test, and the split is deliberate.
//!
//! The **state** tests drive a [`Detail`] directly, because the interesting claims are decisions — what a
//! sort does with an unknown, which pane a cursor belongs to, what an empty pane says — and a decision
//! asserted through a screen is a decision asserted twice. The **screen** tests draw the whole frame into
//! `ratatui`'s `TestBackend` and compare the rows, because the ticket's acceptance criterion is a picture:
//! three panes, side by side, with the caveats visible without pressing anything.
//!
//! Nothing here touches a store or a `git`: a [`Snapshot`] is handed in the same way the engine hands one
//! after a read, which is also what makes the tests independent of what this repository happens to have
//! indexed.

use crossterm::event::KeyCode;
use fathom::metrics::vocab::ActionState;
use fathom::tui::detail::{
    self, CoverageRow, Detail, DiffRow, Filter, HotspotRow, Pane, Pooled, RunLine, Snapshot, Sort,
};
use fathom::tui::entry::{Bench, Churn, Entry, Metric};
use fathom::tui::keys::{classify, key};
use fathom::tui::palette::Palette;
use fathom::tui::plain;
use fathom::tui::state::{App, Command, Key};
use fathom::tui::store_view::{Reading, StoreState};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

/// The clock every fixture is aged from.
const NOW: i64 = 1_767_225_600;
/// The commit under test, and its first parent.
const SHA: &str = "3b8e9f1000000000000000000000000000000000";
const PARENT: &str = "71cd0ae000000000000000000000000000000000";

fn palette() -> Palette {
    Palette::resolve(Some("xterm-256color"), false, false, false)
}

// ------------------------------------------------------------- the fixture

/// A commit that dropped 40 lines of coverage in one file, gained a hotspot in another, deleted a
/// generated file, and has one run excluded by retention — every shape the view has to explain, in one
/// fixture.
fn snapshot() -> Snapshot {
    Snapshot {
        sha: SHA.to_owned(),
        subject: "coverage: stop ignoring node_modules in the lcov walk".to_owned(),
        parent: Some(PARENT.to_owned()),
        tree: "c0ffee0000000000000000000000000000000000".to_owned(),
        pooled: Pooled {
            percent: Some(87.4),
            hit: 3_581,
            found: 4_097,
            measure: "lines".to_owned(),
            gap: None,
            runs: vec!["run-a".to_owned()],
            secondary: vec!["branches 71.0%".to_owned()],
        },
        files: vec![
            file("src/registry.rs", 340, 400, Some(88.0), &["run-a"]),
            file("src/patch.rs", 501, 545, Some(95.4), &["run-a"]),
            // Measured by the run, not touched by the commit: the everyday case the ticket names as
            // "files outside the workspace", and the reason the mark exists.
            file("src/unrelated.rs", 90, 100, None, &["run-a"]),
            // A generated file the coverage walk used to swallow: found == 0 is vacuous, not 0.0%.
            file("dist/bundle.js", 0, 0, None, &["run-a"]),
        ],
        runs: vec![
            RunLine {
                run_id: "run-a".to_owned(),
                run_status: "success".to_owned(),
                source: "lcov".to_owned(),
                measure: "lines".to_owned(),
                hit: 3_581,
                found: 4_097,
                total_source: "total_record".to_owned(),
                used: true,
            },
            RunLine {
                run_id: "run-old".to_owned(),
                run_status: "cancelled".to_owned(),
                source: "lcov".to_owned(),
                measure: "lines".to_owned(),
                hit: 10,
                found: 4_000,
                total_source: "sum_of_files".to_owned(),
                used: false,
            },
        ],
        diff: vec![
            change("src/registry.rs", "modify", 40, 12, false, "source"),
            change("src/patch.rs", "modify", 120, 28, false, "source"),
            change("dist/bundle.js", "delete", 0, 3_100, false, "noise"),
        ],
        mi: vec![
            HotspotRow {
                path: "src/registry.rs".to_owned(),
                status: "ok".to_owned(),
                mi: Some(41.2),
                delta: Some(-2.5),
                loc: Some(402),
                volume: Some(5_200.0),
                cyclomatic: Some(61),
                vocabulary: Some(140),
                language: Some("rust".to_owned()),
            },
            HotspotRow {
                path: "src/patch.rs".to_owned(),
                status: "ok".to_owned(),
                mi: Some(96.8),
                delta: Some(0.0),
                loc: Some(60),
                volume: Some(120.0),
                cyclomatic: Some(4),
                vocabulary: Some(30),
                language: Some("rust".to_owned()),
            },
            HotspotRow {
                path: "dist/bundle.js".to_owned(),
                status: "skipped_noise".to_owned(),
                mi: None,
                delta: None,
                loc: None,
                volume: None,
                cyclomatic: None,
                vocabulary: None,
                language: Some("javascript".to_owned()),
            },
        ],
        analyzers: [("jsapi".to_owned(), 1)].into_iter().collect(),
        aggregate: Some(82.1),
        aggregate_files: 61,
        aggregate_loc: 12_004,
        unanalysed_paths: 3,
        caveats: vec!["coverage_partial".to_owned()],
        excluded: vec![("run-old".to_owned(), "cancelled".to_owned())],
        filter: Filter {
            source_files: 2,
            noise_files: 1,
            noise_add: 0,
            noise_del: 3_100,
            excluded_files: 1,
            truncated: false,
            renames: true,
            add: 160,
            del: 40,
            add_all: 160,
            del_all: 3_140,
        },
        mi_uncached: 0,
        no_mi: false,
        total_only: false,
    }
}

/// One coverage row. `parent_hit` is the same path's rate at the first parent, as a percent.
fn file(path: &str, hit: u64, found: u64, parent: Option<f64>, runs: &[&str]) -> CoverageRow {
    let percent = (found > 0).then(|| f64::from(hit as u32) * 100.0 / f64::from(found as u32));
    CoverageRow {
        path: path.to_owned(),
        hit,
        found,
        percent,
        parent,
        delta: match (percent, parent) {
            (Some(now), Some(before)) => Some(now - before),
            _ => None,
        },
        // The fixture's touched set is the diff's paths, which is what the store reader computes too.
        touched: matches!(path, "src/registry.rs" | "src/patch.rs" | "dist/bundle.js"),
        runs: runs.iter().map(|run| (*run).to_owned()).collect(),
    }
}

/// One diff row.
fn change(path: &str, kind: &str, add: u64, del: u64, binary: bool, scope: &str) -> DiffRow {
    DiffRow {
        path: path.to_owned(),
        old_path: None,
        kind: kind.to_owned(),
        add,
        del,
        binary,
        scope: scope.to_owned(),
        noise: scope != "source",
    }
}

/// A screen holding one snapshot, laid out for a 100x30 terminal like the screen tests use.
fn view() -> Detail {
    fitted(snapshot())
}

/// [`Detail::read`] plus the layout a 30-row frame gives it, because a pane that was never told its height
/// draws one row and every assertion below would be about that instead of about the data.
fn fitted(snapshot: Snapshot) -> Detail {
    let mut view = Detail::read(snapshot);
    view.set_height(detail::pane_height(30));
    view
}

/// A timeline with three rows, so a round trip has somewhere to return to.
fn app() -> App {
    let entries = (0..3)
        .map(|index| Entry {
            sha: if index == 0 {
                SHA.to_owned()
            } else {
                format!("{index:040x}")
            },
            parent: None,
            author_utc: NOW - i64::from(index) * 3_600,
            committer_utc: NOW - i64::from(index) * 3_600,
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
            store: StoreState::Present { rows: 3 },
            entries,
            ..Reading::default()
        },
        NOW,
    );
    app.set_size(30, 100);
    app
}

fn press(app: &mut App, code: KeyCode) -> Vec<Command> {
    app.on_key(classify(key(code)))
}

/// Draw the detail screen and return its rows, trailing cells trimmed.
fn draw(app: &App, width: u16, height: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| fathom::tui::render::draw(app, frame, &palette()))
        .unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .chunks(usize::from(width))
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .map(|line| line.trim_end().to_owned())
        .collect()
}

fn joined(app: &App, width: u16, height: u16) -> String {
    draw(app, width, height).join("\n")
}

/// The same screen reached the way `enter` reaches it: open, then the engine's answer arrives.
fn opened() -> App {
    let mut app = app();
    let commands = app.open_detail(SHA);
    assert_eq!(commands, vec![Command::Detail(SHA.to_owned())]);
    app.apply(fathom::tui::engine::Event::Detail {
        sha: SHA.to_owned(),
        outcome: Ok(Box::new(snapshot())),
    });
    app
}

// ------------------------------------------------------------- the three panes

/// The ticket's picture: three named panes over one commit, each with its own column titles and its own
/// footer, and no pane empty unless it has a reason.
#[test]
fn the_detail_screen_draws_three_panes_with_their_own_footers() {
    let app = opened();
    let screen = joined(&app, 100, 30);
    for want in [
        "coverage by file",
        "MI hotspots",
        "diffstat",
        "87.4% lines",
        "src/registry.rs",
        "41.2",
        "modify",
    ] {
        assert!(screen.contains(want), "missing {want}:\n{screen}");
    }
    // One border per pane, plus the frame's own: three panes is four rules on the left edge.
    let frames = draw(&app, 100, 30)
        .iter()
        .filter(|line| line.starts_with('┌') || line.starts_with('│'))
        .count();
    assert!(
        frames >= 12,
        "three stacked panes draw many frame rows: {frames}"
    );
}

/// The caveats are never a hover or a keystroke away. The pool disagrees with the file records, a run was
/// excluded, and a caveat code came from the evidence — all three are on screen with no key pressed.
#[test]
fn the_caveats_are_on_the_screen_before_any_key_is_pressed() {
    let screen = joined(&opened(), 100, 30);
    // The pooled total covers more lines than the file records name: said in words, not as a mystery.
    assert!(
        screen.contains("outside this workspace") || screen.contains("account for all"),
        "the pool/records relationship is never left implicit:\n{screen}"
    );
    assert!(
        screen.contains("coverage_partial"),
        "the evidence's caveat codes are printed, not swallowed:\n{screen}"
    );
    assert!(
        screen.contains("run-old") && screen.contains("excluded by retention"),
        "a run retention excluded is named with its status:\n{screen}"
    );
    assert!(
        screen.contains("unused"),
        "the footer distinguishes the runs that fed the pool from those that did not:\n{screen}"
    );
}

/// A file the run measured that this commit did not touch is a row with a mark, not a missing row and not
/// a row that looks like it belongs to the commit.
#[test]
fn a_measured_file_this_commit_never_touched_is_marked_not_hidden() {
    let view = view();
    let frame = view.frame(Pane::Coverage, 96, &palette());
    let row = frame
        .rows
        .iter()
        .find(|row| row.cells[0].contains("unrelated"))
        .expect("the untouched file is still listed");
    assert!(
        row.cells[0].trim_start().starts_with('·') || row.cells[0].trim_start().starts_with('-'),
        "and it carries the not-touched mark: {:?}",
        row.cells[0]
    );
    let touched = frame
        .rows
        .iter()
        .find(|row| row.cells[0].contains("registry"))
        .expect("the touched file is listed");
    assert!(
        !touched.cells[0].trim_start().starts_with(['·', '-']),
        "a touched file carries no mark: {:?}",
        touched.cells[0]
    );
}

// ------------------------------------------------------------- one shared selection

/// The cursor belongs to one pane; the *file* belongs to all three. This is the ticket's "shared file
/// selection", and it is the only behaviour that makes three panes better than one.
#[test]
fn moving_the_cursor_in_one_pane_moves_the_highlight_in_all_three() {
    let mut view = view();
    view.focus_pane(Pane::Coverage);
    let first = view.path().clone();
    view.move_cursor(1);
    let wanted = view.path().clone();
    assert_ne!(first, wanted, "the cursor actually moved");
    for pane in Pane::ALL {
        let frame = view.frame(pane, 96, &palette());
        assert!(
            frame.rows.iter().any(|row| row.marked),
            "{:?} highlights the selected file",
            pane
        );
        assert_eq!(
            frame.rows.iter().filter(|row| row.cursor).count(),
            usize::from(pane == Pane::Coverage),
            "only the focused pane draws a cursor in {:?} — the cursor is one thing, the highlight is \
             three",
            pane
        );
    }
}

/// A file one pane knows and another does not is said, not dropped: a highlight that silently vanishes is
/// how a reader starts doubting the other panes.
#[test]
fn a_pane_that_does_not_know_the_selected_file_says_so() {
    // A commit that touched a file the run never reported on: entirely normal when a job changed docs,
    // and the coverage pane's silence would otherwise read as 0%.
    let mut bare = snapshot();
    bare.files.clear();
    bare.pooled = Pooled {
        percent: None,
        gap: Some("no_evidence".to_owned()),
        ..Pooled::default()
    };
    let mut view = Detail::read(bare);
    view.set_height(detail::pane_height(30));
    view.focus_pane(Pane::Diffstat);
    view.jump(true);
    let selected = view.path().unwrap_or_default();
    assert!(
        !selected.is_empty(),
        "the diffstat pane has a file to select"
    );
    let frame = view.frame(Pane::Coverage, 96, &palette());
    let text: Vec<&str> = frame.rows.iter().map(|row| row.cells[0].as_str()).collect();
    assert!(
        text.iter().any(|line| line.contains("no_evidence")),
        "the pane still says why it is empty: {text:?}"
    );
    assert!(
        text.iter()
            .any(|line| line.contains("no coverage record for")),
        "and it names the file the reader is holding highlighted in the pane over: {text:?}"
    );
}

/// `Tab` walks the panes forwards, `Shift-Tab` backwards, and both wrap: three panes is a ring, and a
/// focus that sticks at one end teaches the reader to press the key twice to find out if it worked.
#[test]
fn tab_walks_the_three_panes_and_both_ends_wrap() {
    let mut view = view();
    assert_eq!(
        view.focus,
        Pane::Coverage,
        "the screen opens on the coverage pane"
    );
    view.cycle_pane(1);
    assert_eq!(view.focus, Pane::Hotspots, "Tab goes to the next pane");
    view.cycle_pane(-1);
    assert_eq!(
        view.focus,
        Pane::Coverage,
        "Shift-Tab comes back — the sign of the step is the direction"
    );
    view.cycle_pane(-1);
    assert_eq!(
        view.focus,
        Pane::Diffstat,
        "and past the first pane it wraps to the last"
    );
    for _ in 0..3 {
        view.cycle_pane(1);
    }
    assert_eq!(
        view.focus,
        Pane::Diffstat,
        "three forwards from anywhere is one lap"
    );
    for _ in 0..3 {
        view.cycle_pane(-1);
    }
    assert_eq!(
        view.focus,
        Pane::Diffstat,
        "and three backwards is the same lap run the other way"
    );

    // And through the key surface, because a ring the state machine draws but `Tab` cannot walk is
    // still a line as far as the reader is concerned.
    let mut app = opened();
    assert_eq!(
        app.detail_view().map(|view| view.focus),
        Some(Pane::Coverage)
    );
    press(&mut app, KeyCode::BackTab);
    assert_eq!(
        app.detail_view().map(|view| view.focus),
        Some(Pane::Diffstat),
        "Shift-Tab from the coverage pane lands on the diffstat"
    );
    press(&mut app, KeyCode::Tab);
    assert_eq!(
        app.detail_view().map(|view| view.focus),
        Some(Pane::Coverage),
        "and Tab walks it back"
    );
}

/// The cursor is a position in the pane's *drawn* order, so the file it names must be the file the
/// frame draws the cursor on — never the row with the same index in the order the store happened to
/// hand over. Every sort is checked, because a selection that disagrees with the picture is worse than
/// no selection at all.
#[test]
fn the_selected_path_is_the_row_the_focused_pane_draws_the_cursor_on() {
    let mut view = view();
    view.focus_pane(Pane::Coverage);
    for _ in 0..3 {
        view.cycle_sort();
        view.move_cursor(1);
        let selected = view.path().expect("the pane has rows to select");
        let frame = view.frame(Pane::Coverage, 96, &palette());
        let cursor = frame
            .rows
            .iter()
            .find(|row| row.cursor)
            .expect("exactly one row carries the cursor");
        assert!(
            cursor.cells[0].contains(&selected),
            "the cursor and the shared selection name one file: {:?} vs {selected}",
            cursor.cells[0]
        );
    }
}

/// A rename is one file with two names, and the highlight belongs to the file. The diff names the new
/// path; a coverage record that predates the move still names the old one, and a pane whose highlight
/// falls off the screen over a spelling is a pane the reader stops trusting.
#[test]
fn a_renamed_file_is_one_file_in_all_three_panes_whatever_each_called_it() {
    let mut renamed = snapshot();
    // git's rename row replaces the plain modify row for the same file, so the two panes now disagree
    // by exactly as much as a rename makes them disagree.
    let row = DiffRow {
        path: "src/registry.rs".to_owned(),
        old_path: Some("src/registry-old.rs".to_owned()),
        kind: "rename".to_owned(),
        add: 4,
        del: 0,
        binary: false,
        scope: "source".to_owned(),
        noise: false,
    };
    for existing in &mut renamed.diff {
        if existing.path == "src/registry.rs" {
            *existing = row.clone();
        }
    }
    // And the run measured the file before the move, so its row carries the name it had then — with
    // `touched` left false to prove the mark below comes from the rename rule and not from it.
    for file in &mut renamed.files {
        if file.path == "src/registry.rs" {
            file.path = "src/registry-old.rs".to_owned();
            file.touched = false;
        }
    }
    let mut view = Detail::read(renamed);
    view.set_height(detail::pane_height(30));
    view.focus_pane(Pane::Diffstat);
    view.jump(true);
    assert_eq!(
        view.path().as_deref(),
        Some("src/registry.rs"),
        "the rename row is the last row in path order"
    );
    let frame = view.frame(Pane::Coverage, 96, &palette());
    let marked: Vec<&str> = frame
        .rows
        .iter()
        .filter(|row| row.marked)
        .map(|row| row.cells[0].as_str())
        .collect();
    assert!(
        marked.iter().any(|cell| cell.contains("registry-old")),
        "the pane that knows the old name still highlights the file: {marked:?}"
    );
    assert!(
        !frame
            .rows
            .iter()
            .any(|row| row.cells[0].contains("no coverage record")),
        "and it does not also claim to have no record for it: {:?}",
        frame
            .rows
            .iter()
            .map(|row| &row.cells[0])
            .collect::<Vec<_>>()
    );
}

// ------------------------------------------------------------- sorting

/// Every pane sorts, and every sort is named in that pane's title — a reader who cannot see the order
/// cannot read the table.
#[test]
fn each_pane_sorts_and_says_which_order_it_used() {
    let mut view = view();
    for pane in Pane::ALL {
        view.focus_pane(pane);
        let first = view.title(pane, &palette()).0;
        assert!(
            first.contains(Sort::default_for(pane).label()),
            "{:?} names its sort in its title: {first}",
            pane
        );
        let mut seen = vec![first];
        // The coverage and MI panes offer three orders, the diffstat four — `s` cycles each pane's own
        // list and nothing else, so a pane never offers a sort its data cannot answer.
        let offers = usize::from(matches!(pane, Pane::Diffstat)) + 3;
        for _ in 1..offers {
            view.cycle_sort();
            let title = view.title(pane, &palette()).0;
            assert!(
                !seen.contains(&title),
                "{:?} repeated an order after {offers} presses: {title}",
                pane
            );
            seen.push(title);
        }
        view.cycle_sort();
        assert_eq!(
            view.title(pane, &palette()).0,
            seen[0],
            "{:?}'s cycle wraps back to its default",
            pane
        );
    }
}

/// "Worst first" is a claim about measurements. An absent measurement is not the worst thing in the
/// column, so `None` sorts last and an unknown never headlines a hotspot list.
#[test]
fn unknown_values_sort_last_never_first() {
    let mut view = view();
    view.focus_pane(Pane::Hotspots);
    view.sorts[Pane::Hotspots.index()] = Sort::Worst;
    let frame = view.frame(Pane::Hotspots, 96, &palette());
    let gap_first = frame
        .rows
        .first()
        .is_some_and(|row| row.cells[1].trim() == "—" || row.cells[1].trim() == "!");
    assert!(
        !gap_first,
        "the unanalysed file must not lead a worst-first MI list: {:?}",
        frame.rows.first().map(|row| &row.cells)
    );
    // The gap is still there, one row down, with its reason beside it.
    assert!(
        frame
            .rows
            .iter()
            .any(|row| row.cells[2].contains("not analysed") || row.cells[2].contains("no cached")),
        "an unanalysed file says why where the score would be: {:?}",
        frame
            .rows
            .iter()
            .map(|row| &row.cells[2])
            .collect::<Vec<_>>()
    );
}

/// "Stable and deterministic" is the ticket's phrase, and it means two testable things: every tie is
/// broken by the path bytes, and asking twice is the same answer. A table that reshuffles between
/// frames cannot be read by eye, and "worst first" is only worth anything if row 3 stays row 3.
#[test]
fn equal_values_are_broken_by_path_and_a_second_render_reshuffles_nothing() {
    let mut tied = snapshot();
    // One rate, one size, one delta across the whole pane: every comparison the worst-first sort makes
    // is a tie, so only the tie-break can order these rows.
    for row in &mut tied.files {
        row.percent = Some(90.0);
        row.hit = 90;
        row.found = 100;
        row.delta = None;
        row.parent = Some(90.0);
    }
    let view = Detail::read(tied);
    let order = view.order(Pane::Coverage);
    let paths: Vec<&str> = order
        .iter()
        .map(|index| {
            view.snapshot
                .as_ref()
                .expect("the view was read")
                .files
                .get(*index)
                .expect("the order indexes the rows")
                .path
                .as_str()
        })
        .collect();
    let mut sorted = paths.clone();
    sorted.sort_unstable();
    assert_eq!(
        paths, sorted,
        "every tie is broken by the path bytes: {paths:?}"
    );
    assert_eq!(
        view.order(Pane::Coverage),
        order,
        "asking twice is the same answer"
    );
    let cells = |frame: detail::Frame| {
        frame
            .rows
            .into_iter()
            .map(|row| row.cells)
            .collect::<Vec<[String; 4]>>()
    };
    assert_eq!(
        cells(view.frame(Pane::Coverage, 96, &palette())),
        cells(view.frame(Pane::Coverage, 96, &palette())),
        "two renders of one snapshot are one picture"
    );
}

/// `i` turns the score's inputs on, because "41.2" is not an explanation of a score.
#[test]
fn the_mi_pane_can_show_what_a_score_is_made_of() {
    let mut view = view();
    view.focus_pane(Pane::Hotspots);
    assert_eq!(view.columns(Pane::Hotspots)[2], "LOC");
    view.toggle_inputs();
    assert_eq!(view.columns(Pane::Hotspots)[2], "V/G/LOC");
    let frame = view.frame(Pane::Hotspots, 96, &palette());
    let row = frame
        .rows
        .iter()
        .find(|row| row.cells[0].contains("registry"))
        .expect("the row is still there");
    assert!(
        row.cells[2].contains("V=") && row.cells[2].contains("G=") && row.cells[2].contains("LOC="),
        "the inputs are the Halstead and graph numbers, not a second score: {:?}",
        row.cells[2]
    );
}

// ------------------------------------------------------------- empty states

/// No evidence at all is an empty state with the next command in it, not three empty tables.
#[test]
fn a_commit_with_no_evidence_says_what_to_run() {
    let mut bare = snapshot();
    bare.files.clear();
    bare.runs.clear();
    bare.pooled = Pooled {
        percent: None,
        gap: Some("no_evidence".to_owned()),
        ..Pooled::default()
    };
    let view = fitted(bare);
    let frame = view.frame(Pane::Coverage, 96, &palette());
    let text: String = frame
        .rows
        .iter()
        .map(|row| row.cells[0].as_str())
        .collect::<Vec<&str>>()
        .join(" ");
    assert!(
        text.contains("no_evidence"),
        "the gap code is named: {text}"
    );
    assert!(
        text.contains("run_workflow") || text.contains("act-mcp serve"),
        "and the next command is named with it: {text}"
    );
}

/// A run that reported only a percentage is the case ADR-0008 §3.6 refuses to turn into a number, and the
/// pane has to say that rather than print `—` and move on.
#[test]
fn a_percent_only_run_is_refused_out_loud() {
    let mut bare = snapshot();
    bare.files.clear();
    bare.pooled = Pooled {
        percent: None,
        gap: Some("only_percent_only".to_owned()),
        ..Pooled::default()
    };
    let view = fitted(bare);
    let text: String = view
        .frame(Pane::Coverage, 96, &palette())
        .rows
        .iter()
        .map(|row| row.cells[0].as_str())
        .collect::<Vec<&str>>()
        .join(" ");
    assert!(
        text.contains("scraped percentage"),
        "the refusal explains itself: {text}"
    );
}

/// A root commit has no parent, which is not the same as a parent whose numbers are missing: the diffstat
/// footer distinguishes them.
#[test]
fn a_root_commit_says_it_has_no_parent() {
    let mut root = snapshot();
    root.parent = None;
    let view = Detail::read(root);
    let footer = view.footer(Pane::Diffstat, &palette());
    assert!(
        footer.contains("empty tree") && footer.contains("root commit"),
        "a root commit's baseline is named: {footer}"
    );
    let header = view.header_line(&palette()).1;
    assert!(
        header.contains("root commit"),
        "and the heading says so too: {header}"
    );
}

// ------------------------------------------------------------- footers and provenance

/// The MI footer is the provenance line ADR-0006 §8 asks for: analyzer and version, what the aggregate was
/// a mean over, and how many paths went unanalysed.
#[test]
fn the_mi_footer_is_the_provenance_line() {
    let view = view();
    let footer = view.footer(Pane::Hotspots, &palette());
    for want in [
        "analyzers jsapi v1",
        "aggregate 82.1",
        "61 file(s)",
        "12004 LOC",
        "3 path(s) in the tree were not analysed",
        "tree c0ffee0",
    ] {
        assert!(footer.contains(want), "footer is missing {want}: {footer}");
    }
}

/// The coverage footer answers "which runs, were they used, and do their totals add up" in one line,
/// because a pooled percentage with no runs named is a number with no source.
#[test]
fn the_coverage_footer_names_every_run_and_what_it_did() {
    let footer = view().footer(Pane::Coverage, &palette());
    assert!(
        footer.contains("run-a"),
        "the pooled run is named: {footer}"
    );
    assert!(
        footer.contains("3581/4097"),
        "with the counts the pooled number came from: {footer}"
    );
    assert!(
        footer.contains("unused") && footer.contains("summed"),
        "and which runs and which totals the pool actually used: {footer}"
    );
    assert!(
        footer.contains("branches 71.0%"),
        "a second measure is listed and not pooled with the first: {footer}"
    );
}

/// The diffstat footer is the filter's bill: what the headline counted, what the noise subset took out,
/// and what `paths.exclude` removed before the walk ever saw it.
#[test]
fn the_diffstat_footer_is_the_filters_bill() {
    let footer = view().footer(Pane::Diffstat, &palette());
    for want in [
        "paths.*",
        "2 file(s) in the headline",
        "1 noise/test file(s)",
        "renames detected",
        "parent 71cd0ae",
    ] {
        assert!(footer.contains(want), "footer is missing {want}: {footer}");
    }
}

// ------------------------------------------------------------- keys and the round trip

/// `enter` opens the detail screen, `Esc` returns to the timeline on the row the reader left, and the
/// cached snapshot means the second `enter` reads nothing.
#[test]
fn enter_and_esc_are_a_round_trip_and_the_snapshot_is_cached() {
    let mut app = app();
    assert_eq!(
        press(&mut app, KeyCode::Enter),
        vec![Command::Detail(SHA.to_owned())]
    );
    app.apply(fathom::tui::engine::Event::Detail {
        sha: SHA.to_owned(),
        outcome: Ok(Box::new(snapshot())),
    });
    assert!(matches!(
        app.screen,
        fathom::tui::diff_view::Screen::Detail(_)
    ));
    assert_eq!(press(&mut app, KeyCode::Esc), Vec::<Command>::new());
    assert!(matches!(
        app.screen,
        fathom::tui::diff_view::Screen::Timeline
    ));
    assert_eq!(app.selection, 0, "the timeline's cursor never moved");
    assert!(
        press(&mut app, KeyCode::Enter).is_empty(),
        "the second open is a cache hit, not a second read"
    );
}

/// The keys the ticket names — `Tab`, `s`, `i`, `j`/`k` — act on the detail screen and on nothing else.
#[test]
fn the_detail_keys_act_on_the_focused_pane_only() {
    let mut app = opened();
    let before = app
        .detail_view()
        .map(|view| (view.focus, view.sorts[1], view.show_inputs, view.cursor[1]))
        .unwrap();
    press(&mut app, KeyCode::Tab);
    press(&mut app, KeyCode::Char('s'));
    press(&mut app, KeyCode::Char('i'));
    press(&mut app, KeyCode::Char('j'));
    let after = app
        .detail_view()
        .map(|view| (view.focus, view.sorts[1], view.show_inputs, view.cursor[1]))
        .unwrap();
    assert_ne!(before.0, after.0, "Tab moved the focus to the next pane");
    assert_ne!(before.1, after.1, "s re-sorted the pane that was focused");
    assert_ne!(before.2, after.2, "i turned the MI inputs on");
    assert_ne!(
        before.3, after.3,
        "j moved the cursor in the focused pane only"
    );
    assert_eq!(
        app.detail_view().map(|view| view.cursor[0]),
        Some(0),
        "the coverage pane's cursor did not move"
    );
    assert_eq!(app.selection, 0, "and the timeline did not move at all");
}

/// `d` and `b` from the detail screen are the same commit's other two views, one hop away.
#[test]
fn the_detail_view_hops_to_the_other_two_views() {
    let mut app = opened();
    assert_eq!(
        press(&mut app, KeyCode::Char('d')),
        vec![Command::Diff(SHA.to_owned())]
    );
    assert!(matches!(
        app.screen,
        fathom::tui::diff_view::Screen::Diff(_)
    ));
    let mut app = opened();
    press(&mut app, KeyCode::Char('b'));
    assert!(matches!(
        app.screen,
        fathom::tui::diff_view::Screen::Bench(_)
    ));
}

/// `q` quits from any pane, and an unmapped letter says so instead of doing nothing silently.
#[test]
fn quit_works_and_an_unknown_letter_says_so() {
    let mut app = opened();
    assert_eq!(press(&mut app, KeyCode::Char('q')), vec![Command::Quit]);
    let mut app = opened();
    press(&mut app, KeyCode::Char('ž'));
    let message = app.message.clone().unwrap_or_default();
    assert!(
        message.contains("not a key") && message.contains("?"),
        "an unknown letter names the help key: {message}"
    );
}

/// A snapshot that failed to arrive is a screen with a fix in it, not a hang and not a blank frame.
#[test]
fn a_refused_read_is_a_frame_with_a_fix_in_it() {
    let mut app = app();
    press(&mut app, KeyCode::Enter);
    app.apply(fathom::tui::engine::Event::Detail {
        sha: SHA.to_owned(),
        outcome: Err("nothing is indexed here — press R, or run `fathom index`".to_owned()),
    });
    let screen = joined(&app, 100, 30);
    assert!(
        screen.contains("press R") && screen.contains("fathom index"),
        "the refusal is drawn where the panes would be:\n{screen}"
    );
}

// ------------------------------------------------------------- layout and plain mode

/// Three panes have to survive a short terminal: every pane keeps a border and at least one line, and the
/// panes with more rows get the extra space rather than the first one eating all of it.
#[test]
fn three_panes_share_any_height_without_one_of_them_disappearing() {
    for height in 14..40_u16 {
        let body = detail::pane_height(height);
        let need = [40, 3, 3];
        let panes = Detail::layout(body, need);
        assert!(
            panes.iter().all(|pane| *pane >= 3),
            "height {height} starved a pane: {panes:?}"
        );
        assert_eq!(
            panes.iter().sum::<u16>(),
            body,
            "height {height} was not fully allocated: {panes:?}"
        );
        assert!(
            panes[0] >= panes[1],
            "the pane with 40 rows gets at least as much as the one with 3: {panes:?}",
        );
    }
    // Below that floor the panes still all exist: a pane with zero rows is a pane that silently vanished,
    // which is the failure mode a stacked layout has and a side-by-side one does not.
    for height in 8..14_u16 {
        let body = detail::pane_height(height);
        let panes = Detail::layout(body, [40, 3, 3]);
        assert!(
            panes.iter().all(|pane| *pane >= 1) && panes.iter().sum::<u16>() == body,
            "height {height}: {panes:?}"
        );
        assert!(
            panes[0] >= panes[2],
            "the busiest pane is not the one that lost: {panes:?}"
        );
    }
}

/// A narrow terminal is refused rather than drawn as three unreadable slivers — and the refusal is the
/// frame's own message, so a snapshot test never sees a half-drawn pane.
#[test]
fn a_terminal_too_small_says_so_instead_of_drawing_half_a_pane() {
    let app = opened();
    let screen = joined(&app, 34, 5);
    assert!(
        screen.contains("terminal too small"),
        "below the minimum the frame says what it needs:\n{screen}"
    );
}

/// `--plain --detail REV` prints every row of all three panes — a pipe does not scroll, so the window is
/// the whole table.
#[test]
fn plain_mode_prints_every_row_of_all_three_panes() {
    let text = plain::detail(&view(), &palette(), plain::WIDTH);
    for want in [
        "coverage by file",
        "MI hotspots",
        "diffstat",
        "src/registry.rs",
        "src/unrelated.rs",
        "dist/bundle.js",
        "87.4% lines",
        "41.2",
    ] {
        assert!(
            text.contains(want),
            "plain output is missing {want}:\n{text}"
        );
    }
    // Every row, not the drawn window: four coverage rows, three MI rows, three diff rows.
    for path in [
        "src/registry.rs",
        "src/patch.rs",
        "src/unrelated.rs",
        "dist/bundle.js",
    ] {
        assert!(text.contains(path), "every coverage row is printed: {path}");
    }
}

/// The pooled number is shown once, in the pane's title — repeating it per row would be a second claim
/// about the aggregate that the model does not make.
#[test]
fn the_pooled_number_is_the_panes_header_and_is_never_repeated_per_row() {
    let view = view();
    let frame = view.frame(Pane::Coverage, 96, &palette());
    assert!(
        frame.aside.contains("87.4%"),
        "the pooled number is the pane's aside: {}",
        frame.aside
    );
    let repeats = frame
        .rows
        .iter()
        .filter(|row| row.cells.iter().any(|cell| cell.contains("87.4%")))
        .count();
    assert_eq!(repeats, 0, "no row repeats the aggregate: {frame:?}",);
}

/// The cursor clamps at both ends, and a snapshot that arrives after a resize still fits the frame.
#[test]
fn cursors_clamp_and_a_late_snapshot_respects_the_frame_height() {
    let mut app = app();
    let commands = app.open_detail(SHA);
    assert_eq!(commands, vec![Command::Detail(SHA.to_owned())]);
    for _ in 0..50 {
        app.on_key(Key::Down);
    }
    for _ in 0..50 {
        app.on_key(Key::Up);
    }
    app.apply(fathom::tui::engine::Event::Detail {
        sha: SHA.to_owned(),
        outcome: Ok(Box::new(snapshot())),
    });
    let view = app.detail_view().expect("the view is open");
    assert!(
        view.cursor[0] < view.count(Pane::Coverage),
        "cursor is in range"
    );
    let rows = view.frame(Pane::Coverage, 96, &palette()).rows.len();
    assert!(
        rows <= usize::from(view.heights[0]),
        "a pane never draws more rows than its frame: {rows} > {:?}",
        view.heights[0]
    );
}
