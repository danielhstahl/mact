//! The timeline, drawn: whole-screen snapshots from `ratatui`'s `TestBackend`.
//!
//! No PTY, no process, no timing: [`fathom::tui::render::draw`] writes into a buffer and the buffer is
//! compared against the screen written out below. A snapshot is a golden, and the goldens live in this
//! file rather than in a side directory for two reasons: a reviewer can read the screen next to the
//! state that produced it, and there is no `--accept` step to run or a stale `.snap` to commit. Each
//! comparison trims trailing cells, so only the characters that carry information are pinned.
//!
//! The states are the ticket's list, one test each: populated, empty, all-unknown row, a message too
//! long for its column, a unicode author, a terminal narrower than the table, a filter active, and an
//! index running. The first is drawn from the mock in the ticket so that "the main screen" and "the
//! screen we ship" are the same picture.
//!
//! What a snapshot is *not* used for: colour. Styles are asserted where they are decided
//! (`fathom::tui::render`'s row styling is band-driven, and ADR-0008 §6.1's rule is that a glyph
//! carries the state and colour only repeats it), and a cell's colour in a golden file would be a
//! test that fails whenever a palette is re-tuned for no change in meaning.

use fathom::metrics::vocab::ActionState;
use fathom::store::Run;
use fathom::tui::entry::{Bench, Churn, Entry, Metric};
use fathom::tui::palette::Palette;
use fathom::tui::plain;
use fathom::tui::state::{App, IndexState, Key};
use fathom::tui::store_view::{Reading, StoreState};
use ratatui::backend::TestBackend;
use ratatui::layout::Position;
use ratatui::Terminal;

/// The instant every fixture row is aged from, so `age` is a constant rather than a clock.
const NOW: i64 = 1_767_225_600;
/// The `HEAD` every fixture claims, which is also the mock's top commit.
const HEAD: &str = "9f31a2c0e1a2b3c4d5e6f708192a3b4c5d6e7f80";

/// The workspace the fixtures are drawn from: `act-mcp` on `master`, `HEAD` at [`HEAD`], and the store
/// state the test is about.
fn workspace(store: StoreState) -> Reading {
    Reading {
        repo: "act-mcp".to_owned(),
        branch: "master".to_owned(),
        head: Some(HEAD.to_owned()),
        store,
        evidence_present: true,
        evidence_path: std::path::PathBuf::from(".evidence"),
        ..Reading::default()
    }
}

/// The ticket's mock, drawn at the width every ADR screen is drawn at.
#[test]
fn a_populated_timeline_is_the_mocks_picture() {
    let app = App::new(populated(), NOW);
    check(
        &app,
        &Palette::interactive(),
        100,
        14,
        r#"
FATHOM  act-mcp  master@9f31a2c                               indexed 412 commits · 3 s · evidence ✓
┌ commit ──────────────────────────────────────────────────────────────────────────────────────────┐
│  SHA       age message                          cov          +/-       MI            bench action│
│▸ 9f31a2c    2h registry: sweep evicted r…    87.4%▲     +412 -98     82.1        ~1.5s +6*      ✓│
│  71cd0ae    5h docs: evidence recipes             —       +61 -3     82.1                —      ✗│
│  3b8e9f1    2d coverage: drop node_modul…    91.2%▲    +140 -310    79.8▼      412.5ms +6*      ✓│
│                                                                                                  │
│                                                                                                  │
│                                                                                                  │
│                                                                                                  │
│                                                                                                  │
└──────────────────────────────────────────────────────────────────────────────────────────────────┘
bench* unit_missing
1/3  d diff   enter detail   b bench   / filter   R re-index   ? help   q quit
"#,
    );
}

/// The empty states, each with its next step named. Four of them are one line apart in code and a
/// reader should be able to tell them apart on screen, which is what this pins.
#[test]
fn nothing_indexed_yet_says_so_and_names_the_key() {
    let mut reading = workspace(StoreState::Absent);
    reading.evidence_present = false;
    check(
        &App::new(reading, NOW),
        &Palette::interactive(),
        100,
        14,
        r#"
FATHOM  act-mcp  master@9f31a2c                                             not indexed · evidence —
┌ commit ──────────────────────────────────────────────────────────────────────────────────────────┐
│  SHA       age message                          cov          +/-       MI            bench action│
│nothing is indexed in this workspace yet                                                          │
│  press R to build the store, or run `fathom index` in another terminal                           │
│  and there is no evidence root at .evidence · start the server (`act-mcp serve`) and call `run_wo│
│                                                                                                  │
│                                                                                                  │
│                                                                                                  │
│                                                                                                  │
│                                                                                                  │
└──────────────────────────────────────────────────────────────────────────────────────────────────┘

d diff   enter detail   b bench   / filter   R re-index   ? help   q quit
"#,
    );
}

#[test]
fn a_repository_with_no_commits_says_commit_something() {
    let mut reading = workspace(StoreState::Absent);
    reading.branch = "(none)".to_owned();
    reading.head = None;
    reading.evidence_present = false;
    check(
        &App::new(reading, NOW),
        &Palette::interactive(),
        100,
        11,
        r#"
FATHOM  act-mcp  (none)@—                                                   not indexed · evidence —
┌ commit ──────────────────────────────────────────────────────────────────────────────────────────┐
│  SHA       age message                          cov          +/-       MI            bench action│
│this repository has no commits yet                                                                │
│  commit something, then press R to index it                                                      │
│                                                                                                  │
│                                                                                                  │
│                                                                                                  │
└──────────────────────────────────────────────────────────────────────────────────────────────────┘

d diff   enter detail   b bench   / filter   R re-index   ? help   q quit
"#,
    );
}

/// The "none of the commits have evidence" state: a *populated* table of `—` rows, plus the line that
/// names the tool and the call to make. This is the state a fresh workspace actually lands in, and it
/// is the one a dashboard usually gets wrong by printing zeros.
#[test]
fn an_all_unknown_row_is_a_row_of_glyphs_and_a_next_step() {
    let mut reading = workspace(StoreState::Present { rows: 1 });
    reading.evidence_present = false;
    let mut entry = bare(
        "9f31a2c0e1a2b3c4d5e6f708192a3b4c5d6e7f80",
        2 * 3_600,
        "wip: nothing measured",
    );
    entry.indexed = false;
    entry.action = ActionState::NoEvidence;
    reading.entries = vec![entry];
    check(
        &App::new(reading, NOW),
        &Palette::interactive(),
        100,
        10,
        r#"
FATHOM  act-mcp  master@9f31a2c                                       indexed 1 commits · evidence —
┌ commit ──────────────────────────────────────────────────────────────────────────────────────────┐
│  SHA       age message                          cov          +/-       MI            bench action│
│▸ 9f31a2c    2h wip: nothing measured              —            —        —                —      —│
│                                                                                                  │
│                                                                                                  │
│                                                                                                  │
└──────────────────────────────────────────────────────────────────────────────────────────────────┘
none of the 1 commits shown has evidence in .evidence · start `act-mcp serve` and call `run_workflow
1/1  d diff   enter detail   b bench   / filter   R re-index   ? help   q quit
"#,
    );
}

/// A message that does not fit is cut at its own column's edge, and neither its own columns nor its
/// neighbour's move: the cells after it start where the layout put them.
#[test]
fn a_long_message_is_cut_at_the_column_edge() {
    let mut reading = workspace(StoreState::Present { rows: 1 });
    let long = "refactor: the evidence reader now walks coverage, benchmark, and action records in one pass and \
                keeps a per-SHA ledger so a warm re-index costs a stat per file";
    let mut entry = bare("9f31a2c0e1a2b3c4d5e6f708192a3b4c5d6e7f80", 60, long);
    entry.coverage = Metric::measured(87.4);
    entry.churn = Churn {
        add: Some(412),
        del: Some(98),
        files: Some(9),
        noise_add: 0,
        noise_del: 0,
        truncated: false,
    };
    reading.entries = vec![entry];
    let row = data_row(
        screen(&App::new(reading, NOW), &Palette::interactive(), 100, 8),
        0,
    );
    // The message column is `MESSAGE_MIN`-plus at this width: 26 cells, starting after the marker,
    // the SHA and the age (3 + 7 + 1 + 5 + 1).
    assert_eq!(
        cells(&row, 17, 43),
        "refactor: the evidence re…",
        "the message is cut at its column, not at its neighbour"
    );
    assert!(
        row.contains("87.4%") && row.contains("+412 -98"),
        "and the metric columns are still there, at their own offsets:\n{row}"
    );
}

/// A unicode author name and a CJK subject must not shift a column, because the width used is the
/// display width and not the byte count.
#[test]
fn a_unicode_author_and_subject_keep_the_columns_aligned() {
    let mut reading = workspace(StoreState::Present { rows: 2 });
    let mut first = bare(
        "9f31a2c0e1a2b3c4d5e6f708192a3b4c5d6e7f80",
        60,
        "修复: 缓存目录的解析",
    );
    first.author = "Renée Ångström".to_owned();
    first.coverage = Metric::measured(87.4);
    first.churn = Churn {
        add: Some(12),
        del: Some(3),
        files: Some(2),
        noise_add: 0,
        noise_del: 0,
        truncated: false,
    };
    let mut second = bare(
        "71cd0ae0e1a2b3c4d5e6f708192a3b4c5d6e7f80",
        120,
        "plain ascii subject",
    );
    second.coverage = Metric::measured(86.7);
    second.churn = first.churn;
    reading.entries = vec![first, second];
    let drawn = screen(&App::new(reading, NOW), &Palette::interactive(), 100, 8);
    let cov_cells: Vec<String> = (0..2)
        .map(|row| {
            let line = data_row(drawn.clone(), row);
            // The `cov` column: 9 cells from 44, whatever the message column held.
            cells(&line, 44, 53)
        })
        .collect();
    assert_eq!(
        cov_cells,
        vec!["87.4%".to_owned(), "86.7%".to_owned()],
        "the cov column starts at the same cell in a CJK row and an ascii row:\n{drawn}"
    );
}

/// A terminal narrower than the table drops whole columns in a pinned order instead of clipping.
#[test]
fn a_narrow_terminal_drops_columns_in_the_pinned_order() {
    let app = App::new(populated(), NOW);
    // Only the header line: the key line under the table says "b bench" whatever the columns are.
    let headers = |width| {
        screen(&app, &Palette::interactive(), width, 8)
            .lines()
            .find(|line| line.contains("SHA"))
            .unwrap_or_default()
            .to_owned()
    };
    let wide = headers(100);
    assert!(
        ["bench", "age", "MI", "cov", "+/-", "action"]
            .iter()
            .all(|header| wide.contains(header)),
        "a wide terminal gets all eight:\n{wide}"
    );
    let middle = headers(62);
    assert!(
        middle.contains("cov") && !middle.contains("bench") && !middle.contains("MI"),
        "at 62 cells bench and MI are gone and cov is not:\n{middle}"
    );
    let narrow = headers(30);
    assert!(
        narrow.contains("SHA") && narrow.contains("message") && !narrow.contains("cov"),
        "at 30 cells only the two columns that define a row survive:\n{narrow}"
    );
    // Two different refusals, and they mean different things: *too small* is a terminal that cannot
    // hold a header and a key line at all, *too narrow* is one that can hold them and no table.
    let hopeless = screen(&app, &Palette::interactive(), 40, 5);
    assert!(
        hopeless.contains("terminal too small"),
        "below the smallest drawable screen the app says so instead of drawing a broken table:\n{hopeless}"
    );
    let cramped = screen(&app, &Palette::interactive(), 18, 8);
    assert!(
        cramped.contains("terminal too nar"),
        // Clipped to the box's own width at 18 cells, which is the point of drawing it there.
        "at 18 cells the chrome fits and no column set does, and the screen says which it is:\n{cramped}"
    );
}

/// `/` then text: the count is live, the rows shown are only the matches, and the status line is the
/// prompt rather than the caveats.
#[test]
fn a_filter_in_flight_shows_its_count_and_its_rows() {
    let mut app = App::new(populated(), NOW);
    app.set_size(100, 10);
    app.on_key(Key::Filter);
    for character in "docs".chars() {
        app.on_key(Key::Text(character));
    }
    check(
        &app,
        &Palette::interactive(),
        100,
        10,
        r#"
FATHOM  act-mcp  master@9f31a2c            indexed 412 commits · 3 s · evidence ✓ · filter /docs 1/3
┌ commit ──────────────────────────────────────────────────────────────────────────────────────────┐
│  SHA       age message                          cov          +/-       MI            bench action│
│▸ 71cd0ae    5h docs: evidence recipes             —       +61 -3     82.1                —      ✗│
│                                                                                                  │
│                                                                                                  │
│                                                                                                  │
└──────────────────────────────────────────────────────────────────────────────────────────────────┘
filter: /docs  enter accepts · esc discards
1/1  d diff   enter detail   b bench   / filter   R re-index   ? help   q quit
"#,
    );
}

/// A live index: the header carries the phase and the counter, and the rows already published are on
/// screen — which is the difference between "this is everything" and "this is what exists so far".
#[test]
fn an_index_in_progress_is_said_in_the_header() {
    let mut app = App::new(populated(), NOW);
    app.index = IndexState::Running {
        phase: "rows".to_owned(),
        done: 118,
        total: 412,
        ours: true,
    };
    let screen = screen(&app, &Palette::interactive(), 100, 8);
    assert!(
        screen.contains("indexing… rows 118/412"),
        "the header should carry the live counter:\n{screen}"
    );
    assert!(
        screen.contains("9f31a2c"),
        "and the rows that are already published stay on screen:\n{screen}"
    );
}

/// The `?` overlay: every key this screen answers, and what the glyphs mean.
#[test]
fn the_help_overlay_lists_the_key_surface_and_the_glyphs() {
    let mut app = App::new(populated(), NOW);
    app.on_key(Key::Help);
    let screen = screen(&app, &Palette::interactive(), 100, 26);
    for needle in [
        "PgDn / PgUp",
        "R",
        "re-index",
        "the selected commit's diff",
        "unknown: nothing was measured here",
        "versus the first parent",
    ] {
        assert!(
            screen.contains(needle),
            "the help should say {needle:?}:\n{screen}"
        );
    }
    assert!(
        !screen.contains("mact-by1.11"),
        "`d` is wired now, so the help must not still be promising that it is somebody else's ticket:\n\
         {screen}"
    );
    assert!(app.help);
    app.on_key(Key::Escape);
    assert!(!app.help, "esc closes the overlay");
}

/// `--plain` prints the same rows, in the same order, in a 7-bit alphabet — cells *and* prose.
#[test]
fn the_plain_table_is_the_same_table_in_ascii() {
    let app = App::new(populated(), NOW);
    let table = plain::table(&app, &Palette::plain(), plain::WIDTH);
    assert!(table.contains("FATHOM  act-mcp  master@9f31a2c"));
    assert!(table.contains("87.4%^"), "ascii delta arrow:\n{table}");
    assert!(table.contains("pass"), "ascii action glyph:\n{table}");
    assert!(
        table.contains("evidence yes"),
        "the evidence mark is a word, not a box:\n{table}"
    );
    for unicode in ['—', '·', '✓', '…', '“'] {
        assert!(
            !table.contains(unicode),
            "no {unicode:?} in the plain surface:\n{table}"
        );
    }
    for sha in ["9f31a2c", "71cd0ae", "3b8e9f1"] {
        assert!(table.contains(sha), "{sha} missing from:\n{table}");
    }
}

/// A window the user cut with `--limit` says how much it cut.
///
/// The store's count and the screen's count are different numbers, and only one of them is what the user
/// is looking at; the header prints both rather than letting the smaller one impersonate the bigger.
#[test]
fn a_limited_window_says_what_it_hid() {
    let mut reading = populated();
    reading.entries.truncate(2);
    reading.hidden = 410;
    let app = App::new(reading, NOW);
    assert!(
        app.facts()
            .starts_with("indexed 2 of 412 commits (--limit)"),
        "{}",
        app.facts()
    );
    assert!(
        !populated_facts().starts_with("indexed 2 of"),
        "no limit means no counting: {}",
        populated_facts()
    );

    fn populated_facts() -> String {
        App::new(populated(), NOW).facts()
    }
}

// ---------------------------------------------------------------------------------------------
// the fixtures
// ---------------------------------------------------------------------------------------------

/// The ticket's three rows: one caveated and rising, one with no coverage and a failed action, and one
/// with a measured benchmark and a falling MI.
fn populated() -> Reading {
    let mut first = bare(
        "9f31a2c0e1a2b3c4d5e6f708192a3b4c5d6e7f80",
        2 * 3_600,
        "registry: sweep evicted runs",
    );
    first.parent = Some("71cd0ae0e1a2b3c4d5e6f708192a3b4c5d6e7f80".to_owned());
    first.coverage = Metric::measured(87.4);
    first.coverage.delta = Some(fathom::tui::entry::Delta::Up);
    first.churn = Churn {
        add: Some(412),
        del: Some(98),
        files: Some(9),
        noise_add: 0,
        noise_del: 0,
        truncated: false,
    };
    first.mi = Metric::measured(82.1);
    first.bench = Bench {
        name: "job_duration".to_owned(),
        labels: "{}".to_owned(),
        value: "1.5s".to_owned(),
        comparable: true,
        context: true,
        rest: 6,
        caveat: true,
        delta: None,
        gap: None,
        raw: Some(1.5),
    };
    first.action = ActionState::Success;
    first.caveats = vec!["unit_missing".to_owned()];

    let mut second = bare(
        "71cd0ae0e1a2b3c4d5e6f708192a3b4c5d6e7f80",
        5 * 3_600,
        "docs: evidence recipes",
    );
    second.coverage.gap = Some("no_evidence".to_owned());
    second.churn = Churn {
        add: Some(61),
        del: Some(3),
        files: Some(1),
        noise_add: 0,
        noise_del: 0,
        truncated: false,
    };
    second.mi = Metric::measured(82.1);
    second.unanalysed_paths = 1;
    second.bench = Bench::missing(Some("no_evidence"));
    second.action = ActionState::Failed;

    let mut third = bare(
        "3b8e9f10e1a2b3c4d5e6f708192a3b4c5d6e7f80",
        2 * 86_400,
        "coverage: drop node_modules",
    );
    third.coverage = Metric::measured(91.2);
    third.coverage.delta = Some(fathom::tui::entry::Delta::Up);
    third.churn = Churn {
        add: Some(140),
        del: Some(310),
        files: Some(4),
        noise_add: 120,
        noise_del: 300,
        truncated: false,
    };
    third.mi = Metric::measured(79.8);
    third.mi.delta = Some(fathom::tui::entry::Delta::Down);
    third.bench = Bench {
        name: "tokenizer_throughput".to_owned(),
        labels: "{}".to_owned(),
        value: "412.5ms".to_owned(),
        comparable: true,
        context: false,
        rest: 6,
        caveat: true,
        delta: None,
        gap: None,
        raw: Some(412.5),
    };
    third.action = ActionState::Success;
    third.caveats = vec!["unit_missing".to_owned()];

    Reading {
        entries: vec![first, second, third],
        store: StoreState::Present { rows: 412 },
        // The mock's `3s`: the last index's own duration, not a clock reading.
        last_run: Some(Run {
            duration_ms: 3_000,
            ..Run::default()
        }),
        ..workspace(StoreState::Present { rows: 412 })
    }
}

/// One row with every column unknown; each test sets only what it is about.
fn bare(sha: &str, age_seconds: i64, subject: &str) -> Entry {
    Entry {
        sha: sha.to_owned(),
        parent: None,
        author_utc: NOW - age_seconds,
        committer_utc: NOW - age_seconds,
        subject: subject.to_owned(),
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
    }
}

// ---------------------------------------------------------------------------------------------
// the snapshot harness
// ---------------------------------------------------------------------------------------------

/// The cells `[from, to)` of a drawn line, trimmed.
///
/// Cell-indexed rather than byte-indexed: [`screen`] puts exactly one `char` in each cell (a
/// double-width glyph is followed by a filler), so a range of cells is a range of `char`s even in a row
/// that carries CJK — where byte slicing would land somewhere else entirely.
fn cells(line: &str, from: usize, to: usize) -> String {
    line.chars()
        .skip(from)
        .take(to - from)
        .collect::<String>()
        .trim()
        .to_owned()
}

/// One data row of a drawn screen, `row` rows below the header line, as owned text.
///
/// The rows come out in the app's own order, because the table body is the only part of the screen that
/// carries a SHA at column 3 — counting from the top of the buffer would make every geometry assertion
/// in this file depend on the header's height.
fn data_row(screen: String, row: usize) -> String {
    screen
        .lines()
        .filter(|line| line.chars().nth(3).is_some_and(|c| c.is_ascii_hexdigit()))
        .nth(row)
        .expect("the table drew the row the app was given")
        .to_owned()
}

/// Draw `app` at `width`×`height` and return the screen as text, one line per row.
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
                    buffer
                        .cell(Position::new(x, y))
                        .map_or(" ", |cell| {
                            // A double-width glyph writes its character into the first cell and an
                            // empty filler into the second. One `char` per cell is what lets a test
                            // index a column by cell rather than by byte.
                            if cell.symbol().is_empty() {
                                " "
                            } else {
                                cell.symbol()
                            }
                        })
                        .to_owned()
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Compare a whole screen against a golden, ignoring trailing cells and blank leading/trailing lines.
#[track_caller]
fn check(app: &App, palette: &Palette, width: u16, height: u16, expected: &str) {
    let drawn = screen(app, palette, width, height);
    let have = tidy(&drawn);
    let want = tidy(expected);
    if have != want && std::env::var("FATHOM_SNAPSHOT_UPDATE").is_ok() {
        // One command to re-record a screen, printed rather than written: a golden that changes is a
        // decision, not a side effect.
        println!("--- expected by this test ---\n{want}\n--- drawn now ---\n{have}");
    }
    assert_eq!(have, want, "\ndrawn:\n{have}");
}

/// Trim each line's trailing cells and drop the first and last blank lines, so a golden can be written
/// as an indented raw string without counting spaces.
fn tidy(text: &str) -> String {
    text.lines()
        .map(|line| line.trim_end().to_owned())
        .collect::<Vec<_>>()
        .join("\n")
        .trim_matches('\n')
        .to_owned()
}
