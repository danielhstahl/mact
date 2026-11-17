//! The benchmark panel: what one commit's rows are, what each cell may claim, and what `b` does.
//!
//! The state half is [`fathom::tui::bench::Panel`] built from store-shaped JSON — the same parser the
//! store's rows go through ([`fathom::tui::bench::Commit::from_evidence`]), so a fixture cannot be a
//! friendlier invention than the real thing. The tests are the ticket's list, one each: a unit-missing
//! metric, a unit that changed between commits, an `act_log`-only metric, a failed run's numbers
//! excluded (and visible when the overlay is on), a single-sample "insufficient", and the no-benchmark
//! empty state. Beside them sit the rules those six depend on and would otherwise break silently: which
//! way a sparkline may join points, what a spread of one run does not prove, how far a history walks,
//! and which column a narrow terminal is allowed to drop.
//!
//! The drawn half is `ratatui`'s `TestBackend` again, in the style of `tests/tui_render.rs` and
//! `tests/tui_diff.rs`: whole-screen goldens for the populated panel, the empty state, and a terminal
//! too narrow for the whole table. Goldens are re-recorded with `FATHOM_SNAPSHOT_UPDATE=1` rather than
//! kept in a side file, so accepting a change is a decision visible in a diff.

use std::path::PathBuf;

use crossterm::event::KeyCode;
use fathom::metrics::vocab::ActionState;
use fathom::tui::bench::{Commit, Evidence, Panel, DEFAULT_BAND_PERCENT, HISTORY_WINDOW};
use fathom::tui::diff_view::Screen;
use fathom::tui::entry::{Bench, Churn, Entry, Metric};
use fathom::tui::palette::Palette;
use fathom::tui::state::{App, Command};
use fathom::tui::store_view::{Reading, StoreState};
use ratatui::backend::TestBackend;
use ratatui::layout::Position;
use ratatui::Terminal;
use serde_json::{json, Value};

/// The instant every fixture row is aged from.
const NOW: i64 = 1_767_225_600;
/// The evidence root the fixtures claim.
const EVIDENCE: &str = "/repo/.evidence";

// ---------------------------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------------------------

/// One benchmark group, in the shape a stored row's `benchmarks[]` entry has.
///
/// Every field the row shape makes required is present, because the panel drops a group it cannot read
/// rather than drawing it as zero — a fixture that omitted `samples` would test the drop path by
/// accident instead of the row.
fn group(name: &str, labels: &[(&str, &str)], unit: Option<&str>, value: f64) -> Value {
    let mut set = serde_json::Map::new();
    for (key, value) in labels {
        set.insert((*key).to_owned(), Value::String((*value).to_owned()));
    }
    let mut row = json!({
        "name": name,
        "labels": Value::Object(set),
        "comparable": unit.is_some(),
        "context": false,
        "value": value,
        "samples": 1,
        "min": value,
        "max": value,
        "run_count": 1,
        "run_id": "20261112T004203Z-a1b2c3",
        "source": "job_file",
        "flags": [],
    });
    if let Some(unit) = unit {
        row["unit"] = json!(unit);
    }
    row
}

/// The same group with the spread a real set of runs gives it: `n` samples, `runs` runs, `min..max`.
fn spread(row: &mut Value, samples: usize, runs: usize, min: f64, max: f64) -> Value {
    row["samples"] = json!(samples);
    row["run_count"] = json!(runs);
    row["min"] = json!(min);
    row["max"] = json!(max);
    row.clone()
}

/// One commit's evidence section, from its groups.
fn evidence(groups: Vec<Value>, extra: Value) -> Value {
    let mut root = json!({
        "commit": "ignored",
        "action_success": "success",
        "gaps": {},
        "benchmarks": groups,
    });
    if let Value::Object(extra) = extra {
        for (key, value) in extra {
            root[key] = value;
        }
    }
    root
}

/// One commit's panel input.
fn commit(sha: &str, groups: Vec<Value>) -> Commit {
    Commit::from_evidence(sha, Some(&evidence(groups, json!({}))))
}

/// The same, with extra fields on the evidence section (`excluded_runs`, `gaps`, `caveats`, …).
fn commit_with(sha: &str, groups: Vec<Value>, extra: Value) -> Commit {
    Commit::from_evidence(sha, Some(&evidence(groups, extra)))
}

/// A panel over one commit with the given older commits, at the default band.
fn panel(at: &Commit, older: &[Commit]) -> Panel {
    Panel::open(
        at,
        older,
        &Evidence {
            present: true,
            path: PathBuf::from(EVIDENCE),
        },
        DEFAULT_BAND_PERCENT,
    )
}

/// The ticket's mock commit: a comparable metric with two samples over three runs, an `act_log`
/// context metric, a unitless one, and a metric only the newest commit reports.
fn mock_commits() -> (Commit, Vec<Commit>) {
    let throughput = |value: f64| {
        spread(
            &mut group(
                "tokenizer_throughput",
                &[("job", "hyperfine-bench")],
                Some("ops/s"),
                value,
            ),
            2,
            3,
            value - 400.0,
            value + 400.0,
        )
    };
    let duration = |value: f64| {
        let mut row = group(
            "job_duration_s",
            &[("job", "hyperfine-bench")],
            Some("s"),
            value,
        );
        row["source"] = json!("act_log");
        row["context"] = json!(true);
        spread(&mut row, 2, 3, value, value + 0.1)
    };
    let newest = commit(
        "3b8e9f100000000000000000000000000000000",
        vec![
            throughput(184_666.55),
            duration(91.8),
            group("queue_depth", &[("job", "hyperfine-bench")], None, 3.0),
            group(
                "cold_start_ms",
                &[("job", "hyperfine-bench"), ("step", "Run benchmark")],
                Some("ms"),
                412.5,
            ),
        ],
    );
    let older: Vec<Commit> = vec![
        commit(
            "8ac22d100000000000000000000000000000000",
            vec![
                throughput(179_000.0),
                duration(90.0),
                group("queue_depth", &[("job", "hyperfine-bench")], None, 3.0),
            ],
        ),
        commit(
            "71cd0ae00000000000000000000000000000000",
            vec![throughput(178_500.0), duration(89.0)],
        ),
    ];
    (newest, older)
}

/// The panel for the mock, already open and already tall enough to see all four rows.
fn mock_panel() -> Panel {
    let (newest, older) = mock_commits();
    let mut opened = panel(&newest, &older);
    opened.set_height(9);
    opened
}

// ---------------------------------------------------------------------------------------------
// what a row is
// ---------------------------------------------------------------------------------------------

/// One row per group, in `(name, labels, unit)` order — the same order the timeline's one-cell choice
/// sorts by, so the panel's first row and the timeline's cell are the same metric.
#[test]
fn one_row_per_group_in_the_readers_own_order() {
    let opened = mock_panel();
    let names: Vec<String> = opened
        .lines
        .iter()
        .map(|line| format!("{} {}", line.id.name, line.id.unit_or_missing()))
        .collect();
    assert_eq!(
        names,
        [
            "cold_start_ms ms",
            "job_duration_s s",
            "queue_depth unit_missing",
            "tokenizer_throughput ops/s",
        ],
        "sorted by name, and the label set breaks ties: {names:?}"
    );
    let line = opened
        .lines
        .iter()
        .find(|line| line.id.name == "tokenizer_throughput")
        .expect("the throughput row");
    assert_eq!(line.id.labels, r#"{"job":"hyperfine-bench"}"#);
    assert_eq!(line.value().expect("measured").text, "184666.55");
    assert_eq!(line.missing, 0, "every commit in the window reported it");
}

/// `queue_depth` arrives with no unit. The honest answer to "what is 3?" is "unknown", so the row shows
/// the number, says it has no unit, and gets no delta and no sparkline (ADR-0008 §4.3).
#[test]
fn a_metric_with_no_unit_is_displayable_and_never_comparable() {
    let mut opened = mock_panel();
    let line = opened
        .lines
        .iter()
        .find(|line| line.id.name == "queue_depth")
        .expect("the unitless row is still a row");
    let palette = Palette::interactive();
    assert_eq!(
        line.value_text(&palette, 14),
        "3 n=1",
        "3 is a fine number to show a human, and the cell also says what it is a median of"
    );
    assert!(
        line.metric_text(&palette).contains("(unit missing)"),
        "and the screen says it: {}",
        line.metric_text(&palette)
    );
    assert_eq!(
        line.verdict,
        Some(fathom::tui::bench::Verdict::NotComparable),
        "no unit means no baseline comparison, whatever the history looks like"
    );
    assert_eq!(
        line.history_text(&palette, 22),
        "",
        "no sparkline for a group nothing may compare — the column stays empty rather than drawing a shape"
    );
    opened.select(
        opened
            .lines
            .iter()
            .position(|l| l.id.name == "queue_depth")
            .unwrap(),
    );
    assert!(
        opened
            .status(&palette)
            .contains("no unit: display only, not comparable"),
        "and the status line says so: {}",
        opened.status(&palette)
    );
}

/// A metric whose unit changed between commits is two metrics: the panel must not draw `ms` against
/// `s`, and the older one must not become a baseline, a sparkline point, or a silently merged row.
#[test]
fn a_unit_change_makes_two_metrics_and_no_trend_between_them() {
    let now = commit(
        "aaaaaaaa00000000000000000000000000000000",
        vec![group("queue_depth", &[("job", "bench")], Some("ms"), 4.0)],
    );
    let before = commit(
        "bbbbbbbb00000000000000000000000000000000",
        vec![group("queue_depth", &[("job", "bench")], Some("s"), 3.0)],
    );
    let opened = panel(&now, std::slice::from_ref(&before));
    assert_eq!(
        opened.lines.len(),
        2,
        "one row per unit, not one row per name"
    );
    let ms = opened
        .lines
        .iter()
        .find(|line| line.id.unit.as_deref() == Some("ms"))
        .expect("the ms row");
    let s = opened
        .lines
        .iter()
        .find(|line| line.id.unit.as_deref() == Some("s"))
        .expect("the s row");
    assert_eq!(
        ms.verdict,
        Some(fathom::tui::bench::Verdict::NoBaseline),
        "the s commit is not a baseline for the ms one"
    );
    assert_eq!(
        ms.missing, 1,
        "the older commit is unknown for *this* group"
    );
    assert_eq!(
        ms.values(),
        vec![4.0],
        "and it is not a point on this sparkline"
    );
    assert_eq!(
        s.measured, None,
        "the s group is not reported at this commit"
    );
    assert_eq!(
        s.last_seen.as_ref().map(|(sha, ago)| (sha.as_str(), *ago)),
        Some(("bbbbbbb", 1)),
        "…but it is not nothing: it says where it was last seen"
    );
}

/// `act_log` metrics describe the run, not the code (ADR-0008 §4.4): they are shown, marked `~`, and
/// the band never calls them a regression.
#[test]
fn an_act_log_metric_is_context_and_the_band_does_not_judge_it() {
    let opened = mock_panel();
    let line = opened
        .lines
        .iter()
        .find(|line| line.id.name == "job_duration_s")
        .expect("the act_log row");
    let palette = Palette::interactive();
    assert!(line.is_context());
    assert!(
        line.metric_text(&palette)
            .starts_with(&palette.glyphs.context),
        "the context prefix is on the metric, not on the value: {}",
        line.metric_text(&palette)
    );
    match line.verdict {
        Some(fathom::tui::bench::Verdict::Moved { context, .. }) => {
            assert!(context, "the verdict knows it is context");
        }
        ref other => panic!("a context metric with a baseline still gets a delta, got {other:?}"),
    }
    // And it is a big move, which is exactly the case the exemption exists for: a run that took
    // twice as long is not a commit that got slower.
    let status = {
        let mut moved = opened.clone();
        moved.select(1);
        moved.status(&palette)
    };
    assert!(
        status.contains("never a regression signal")
            || status.contains("act_log: describes the run"),
        "the status line says what context means: {status}"
    );
}

/// The spread column and the run count beside it are the only place the reader learns how much the
/// runs disagree. One run with `min == max` is not a stable metric, and the panel must not imply one.
#[test]
fn a_spread_from_one_run_is_an_unmeasured_noise_floor_not_stability() {
    let flat = spread(
        &mut group("cold_start_ms", &[("job", "bench")], Some("ms"), 412.5),
        2,
        1,
        412.5,
        412.5,
    );
    let before_row = spread(
        &mut group("cold_start_ms", &[("job", "bench")], Some("ms"), 400.0),
        2,
        1,
        400.0,
        400.0,
    );
    let now = commit("aaaaaaaa00000000000000000000000000000000", vec![flat]);
    let before = commit("bbbbbbbb00000000000000000000000000000000", vec![before_row]);
    let opened = panel(&now, std::slice::from_ref(&before));
    let line = opened.lines.first().expect("one row");
    assert_eq!(
        line.verdict,
        Some(fathom::tui::bench::Verdict::Insufficient(
            fathom::tui::bench::Short::NoiseFloor
        )),
        "two samples that agreed in one run is not a measured noise floor"
    );
    let palette = Palette::interactive();
    assert_eq!(
        line.spread_text(&palette, 19),
        "[412.5] 1 run",
        "min == max prints one number, and the run count beside it is what tells you why"
    );
    assert!(opened
        .status(&palette)
        .contains("the noise floor is unknown"));
}

/// One sample is not a distribution: no percentage, and the cell says `n=1` rather than `·0.0%`.
#[test]
fn one_sample_is_insufficient_and_never_a_percentage() {
    let now = commit(
        "aaaaaaaa00000000000000000000000000000000",
        vec![group(
            "cold_start_ms",
            &[("job", "bench")],
            Some("ms"),
            500.0,
        )],
    );
    let before = commit(
        "bbbbbbbb00000000000000000000000000000000",
        vec![group(
            "cold_start_ms",
            &[("job", "bench")],
            Some("ms"),
            400.0,
        )],
    );
    let opened = panel(&now, std::slice::from_ref(&before));
    let line = opened.lines.first().expect("one row");
    assert_eq!(
        line.verdict,
        Some(fathom::tui::bench::Verdict::Insufficient(
            fathom::tui::bench::Short::Samples(1)
        ))
    );
    let palette = Palette::interactive();
    assert_eq!(
        line.verdict.as_ref().map(|v| v.text(&palette)),
        Some("n=1".to_owned())
    );
    assert!(
        !opened.status(&palette).contains('%'),
        "no percentage anywhere on the line: {}",
        opened.status(&palette)
    );
    assert!(opened
        .status(&palette)
        .contains("insufficient samples (n=1)"));
}

/// The band is declared, printed, and only ever a percent of the value — no invented significance.
#[test]
fn a_delta_is_judged_against_the_declared_band_and_says_which_one() {
    let now = commit(
        "aaaaaaaa00000000000000000000000000000000",
        vec![{
            let mut g = group("cold_start_ms", &[("job", "bench")], Some("ms"), 440.0);
            g["samples"] = json!(3);
            g["min"] = json!(430.0);
            g["max"] = json!(450.0);
            g["run_count"] = json!(2);
            g
        }],
    );
    let before = commit(
        "bbbbbbbb00000000000000000000000000000000",
        vec![group(
            "cold_start_ms",
            &[("job", "bench")],
            Some("ms"),
            400.0,
        )],
    );
    let palette = Palette::interactive();
    let opened = panel(&now, std::slice::from_ref(&before));
    let line = opened.lines.first().expect("one row");
    match line.verdict {
        Some(fathom::tui::bench::Verdict::Moved {
            percent, outside, ..
        }) => {
            assert!((percent - 10.0).abs() < 0.001, "440 against 400 is +10%");
            assert!(outside, "10% is outside a 5% band");
        }
        ref other => panic!("expected a moved verdict, got {other:?}"),
    }
    assert!(line
        .verdict
        .as_ref()
        .is_some_and(|v| v.text(&palette).ends_with('!')));
    assert!(
        opened.headers()[3].contains("5%"),
        "the band is in the column title"
    );
    assert!(opened
        .status(&palette)
        .contains("OUTSIDE THE DECLARED BAND"));

    // Inside the band: the same screen, no exclamation, and no claim of importance either way.
    let inside = commit(
        "aaaaaaaa00000000000000000000000000000000",
        vec![{
            let mut g = group("cold_start_ms", &[("job", "bench")], Some("ms"), 408.0);
            g["samples"] = json!(3);
            g["min"] = json!(400.0);
            g["max"] = json!(415.0);
            g["run_count"] = json!(2);
            g
        }],
    );
    let opened = panel(&inside, std::slice::from_ref(&before));
    assert!(
        opened
            .lines
            .first()
            .and_then(|line| line.verdict.as_ref())
            .is_some_and(|v| !v.text(&palette).contains('!')),
        "2% is inside the band: {}",
        opened.lines[0]
            .verdict
            .as_ref()
            .map(|v| v.text(&palette))
            .unwrap_or_default()
    );
    // …and the band is configurable, and says so in the same title.
    let wider = Panel::open(
        &inside,
        std::slice::from_ref(&before),
        &Evidence::default(),
        1.0,
    );
    assert!(wider.headers()[3].contains("1%"));
    assert!(wider.status(&palette).contains("band 1%"));
}

// ---------------------------------------------------------------------------------------------
// history
// ---------------------------------------------------------------------------------------------

/// A sparkline is scaled to its own min and max, which is exactly how it lies, so the cell prints the
/// axis next to it. And a history with a hole in it is not a line: the panel draws the points it has
/// as no line at all, keeps the axis, and says how much was unknown.
#[test]
fn a_history_prints_its_axis_and_a_hole_does_not_become_a_line() {
    let palette = Palette::interactive();
    // One metric, four commits, newest first: 160 here, and 100 four commits back.
    let chain = |values: &[Option<f64>]| -> Vec<Commit> {
        values
            .iter()
            .enumerate()
            .map(|(index, value)| {
                let short = char::from(b'a' + u8::try_from(index).unwrap_or(0)).to_string();
                let groups: Vec<Value> = value.map_or_else(Vec::new, |value| {
                    vec![spread(
                        &mut group("x", &[], Some("ops/s"), value),
                        2,
                        2,
                        value - 1.0,
                        value + 1.0,
                    )]
                });
                commit(&format!("{}{}", short.repeat(8), "0".repeat(32)), groups)
            })
            .collect()
    };
    let four = chain(&[Some(160.0), Some(110.0), Some(120.0), Some(100.0)]);
    let opened = panel(&four[0], &four[1..]);
    let line = opened.lines.first().expect("one row");
    let cell = line.history_text(&palette, 24);
    // Oldest first across the window: 100, 120, 110, 160 scaled between its own min and max.
    assert!(
        cell.starts_with("\u{2581}\u{2583}\u{2582}\u{2588}"),
        "four points scaled to their own 100..160: {cell}"
    );
    assert!(
        cell.ends_with("100..160"),
        "the axis travels with the shape, because a sparkline alone is a shape that lies: {cell}"
    );
    assert_eq!(line.history_note(), None, "four commits, all four reported");

    // The same ends with the two middle commits missing the metric entirely.
    let holey = chain(&[Some(160.0), None, None, Some(100.0)]);
    let opened = panel(&holey[0], &holey[1..]);
    let line = opened.lines.first().expect("one row");
    let cell = line.history_text(&palette, 24);
    assert!(
        !cell.contains('\u{2581}') && !cell.contains('\u{2588}'),
        "two points two commits apart are not a line: {cell}"
    );
    assert!(cell.contains("100..160"), "the axis survives: {cell}");
    assert_eq!(
        line.history_note().as_deref(),
        Some("unknown at 2 of 4 commits"),
        "and the partiality is stated in words, not drawn as a shape"
    );
    assert!(opened
        .status(&palette)
        .contains("unknown at 2 of 4 commits"));
}

/// A history walks a declared window and says how far it walked: a panel that silently walked 40 000
/// commits would be a panel that took a minute to open, and one that walked three would be a panel whose
/// sparkline is a coin flip. The number is in the column title, so a reader can tell which they got.
#[test]
fn a_history_walks_the_declared_window_and_says_so() {
    let now = commit(
        "aaaaaaaa00000000000000000000000000000000",
        vec![group("x", &[], Some("ops/s"), 10.0)],
    );
    let older: Vec<Commit> = (0..(HISTORY_WINDOW * 2))
        .map(|count| {
            let index = u32::try_from(count).unwrap_or(0);
            commit(
                &format!("{index:040x}"),
                vec![group("x", &[], Some("ops/s"), 10.0 + f64::from(index))],
            )
        })
        .collect();
    let opened = panel(&now, &older);
    assert_eq!(opened.window, HISTORY_WINDOW, "and no further");
    let line = opened.lines.first().expect("one row");
    assert_eq!(line.history.len(), HISTORY_WINDOW);
    assert!(
        opened.headers()[4].contains(&format!("history (last {HISTORY_WINDOW})")),
        "the column title carries the window: {}",
        opened.headers()[4]
    );
}

// ---------------------------------------------------------------------------------------------
// runs that were not used, and metrics that stopped existing
// ---------------------------------------------------------------------------------------------

/// A failed run's numbers are not in any row, and `f` says which run that means — with its `run_id`,
/// which is the ticket's test, and which only exists because the row kept the identity of what it threw
/// away (ADR-0008 §3.7).
#[test]
fn a_failed_runs_numbers_are_out_of_every_row_and_f_names_the_run() {
    let good = {
        let mut row = group("x", &[], Some("ops/s"), 100.0);
        row["run_id"] = json!("20261112T004203Z-a1b2c3");
        row
    };
    let at = commit_with(
        "aaaaaaaa00000000000000000000000000000000",
        vec![good],
        json!({
            "excluded_runs": [
                {"run_id": "20261112T011740Z-12ab9f", "status": "failure", "reason": "run_status_excluded"},
                {"run_id": "20261112T014502Z-77cd10", "status": "cancelled", "reason": "run_truncated"}
            ],
            "superseded_runs": [{"run_id": "20261112T003000Z-000000", "status": "success", "reason": "superseded"}]
        }),
    );
    let mut opened = panel(&at, &[]);
    let palette = Palette::interactive();
    // Off by default: the header counts them and offers the key, and nothing else changes.
    assert!(!opened.show_excluded);
    assert!(
        opened.notice().is_none(),
        "the table is the successful run's, silently and correctly"
    );
    let (_, right) = opened.header(&palette);
    assert!(
        right.contains("2 excluded runs") && right.contains('f') && right.contains("1 superseded"),
        "the header counts them and offers the key: {right}"
    );
    opened.set_show_excluded(true);
    let notice = opened.notice().expect("the overlay says something");
    assert!(
        notice.contains("20261112T011740Z-12ab9f") && notice.contains("20261112T014502Z-77cd10"),
        "every excluded run is named by its run_id: {notice}"
    );
    assert!(notice.contains("not in any row above"));
    let (_, right) = opened.header(&palette);
    assert!(right.contains("2 excluded runs shown"), "{right}");
}

/// A metric reported by older commits and not by this one gets its own row saying so, with the last
/// commit that had it — and when the reason is in a caveat the row carries, the row says that too.
#[test]
fn a_metric_this_commit_stopped_reporting_keeps_a_row_that_explains_itself() {
    let now = commit(
        "aaaaaaaa00000000000000000000000000000000",
        vec![group("x", &[], Some("ops/s"), 100.0)],
    );
    let before = commit_with(
        "bbbbbbbb00000000000000000000000000000000",
        vec![
            group("x", &[], Some("ops/s"), 99.0),
            group(
                "step_duration_s",
                &[("step", "Run benchmark")],
                Some("s"),
                12.0,
            ),
        ],
        json!({"caveats": ["act_version_too_old"]}),
    );
    let opened = panel(&now, std::slice::from_ref(&before));
    assert_eq!(opened.lines.len(), 2);
    let absent = opened
        .lines
        .iter()
        .find(|line| line.id.name == "step_duration_s")
        .expect("the absent metric still has a row");
    assert_eq!(absent.measured, None);
    let palette = Palette::interactive();
    assert_eq!(absent.value_text(&palette, 14), palette.glyphs.unknown);
    let cell = absent.history_text(&palette, 24);
    assert!(
        cell.contains("last seen bbbbbbb"),
        "the history cell says where it went: {cell}"
    );
    let status = {
        let mut opened = opened.clone();
        opened.select(1);
        opened.status(&palette)
    };
    assert!(status.contains("not reported at this commit"), "{status}");
    assert!(
        status.contains("act 0.2.76"),
        "the caveat that explains it is on the line: {status}"
    );
}

// ---------------------------------------------------------------------------------------------
// the empty state
// ---------------------------------------------------------------------------------------------

/// "No benchmarks" is three different nothings and the panel says which, names the workflow that would
/// produce one, and never once uses the word `unknown` — the timeline already spent that word on
/// commits with no analysis, and reusing it here would make the two mean the same thing.
#[test]
fn the_empty_state_names_the_workflow_and_the_next_step() {
    let palette = Palette::plain();
    let at = "aaaaaaaa00000000000000000000000000000000";
    let opened = Panel::open(
        &commit_with(
            at,
            vec![],
            json!({
                "gaps": {"benchmarks": "no_benchmark_run"},
                "action_detail": [{"kind": "coverage", "workflow": "full-analysis", "task": "repo"}]
            }),
        ),
        &[],
        &Evidence {
            present: true,
            path: PathBuf::from(EVIDENCE),
        },
        DEFAULT_BAND_PERCENT,
    );
    assert!(opened.lines.is_empty());
    let step = opened.next_step().join("\n");
    let text = format!("{}\n{step}", opened.header(&palette).0);
    assert!(
        text.contains("no benchmark evidence") || text.to_lowercase().contains("no benchmark"),
        "{text}"
    );
    assert!(
        !text.contains('\u{2014}'),
        "the unknown glyph belongs to the timeline's coverage cell, not to this: {text}"
    );
    assert!(
        step.contains("benchmark workflow") && step.contains("run_workflow"),
        "the next step names what would produce a number: {step}"
    );
    assert!(
        step.contains("full-analysis/repo"),
        "and what did run here: {step}"
    );
    assert!(
        step.contains(&format!("{EVIDENCE}/benchmark/{at}")),
        "and where it looked: {step}"
    );

    // The other two nothings, in their own words.
    let excluded = Panel::open(
        &commit_with(
            at,
            vec![],
            json!({"gaps": {"benchmarks": "all_runs_excluded"}}),
        ),
        &[],
        &Evidence::default(),
        DEFAULT_BAND_PERCENT,
    );
    assert!(excluded
        .next_step()
        .join("\n")
        .contains("none of them ended `success`"));
    let filtered = Panel::open(
        &commit_with(
            at,
            vec![],
            json!({"gaps": {"benchmarks": "config_filtered"}}),
        ),
        &[],
        &Evidence::default(),
        DEFAULT_BAND_PERCENT,
    );
    assert!(filtered.next_step().join("\n").contains("benchmarks.allow"));
    // And a commit the store has no row for at all is the same shape, not a crash.
    let bare = Panel::open(
        &Commit::default(),
        &[],
        &Evidence::default(),
        DEFAULT_BAND_PERCENT,
    );
    assert!(bare.lines.is_empty());
    assert!(!bare.next_step().is_empty());
}

// ---------------------------------------------------------------------------------------------
// the screen and its keys
// ---------------------------------------------------------------------------------------------

/// Press one key, from the byte a terminal would have sent.
fn press(app: &mut App, code: KeyCode) -> Vec<Command> {
    app.on_key(fathom::tui::keys::classify(fathom::tui::keys::key(code)))
}

/// The shape of a timeline entry, with the two fields the fixtures actually vary left empty.
fn blank() -> Entry {
    Entry {
        sha: String::new(),
        parent: None,
        author_utc: NOW,
        committer_utc: NOW,
        subject: String::new(),
        author: "Loop RS".to_owned(),
        coverage: Metric::UNKNOWN,
        churn: Churn::UNKNOWN,
        mi: Metric::UNKNOWN,
        unanalysed_paths: 0,
        bench: Bench::missing(None),
        detail: Commit::default(),
        action: ActionState::NoEvidence,
        indexed: true,
        caveats: Vec::new(),
    }
}

/// A timeline over the mock's three commits, newest first, each carrying its own benchmark detail —
/// which is the whole point of the panel being a view over stored rows.
fn app_with_bench() -> App {
    let (newest, older) = mock_commits();
    let subjects = ["drop node_modules", "an older commit", "the oldest"];
    let entries: Vec<Entry> = std::iter::once(newest)
        .chain(older)
        .enumerate()
        .map(|(index, mut detail)| {
            detail.subject = subjects[index].to_owned();
            Entry {
                sha: detail.sha.clone(),
                subject: subjects[index].to_owned(),
                detail,
                ..blank()
            }
        })
        .collect();
    let head = entries[0].sha.clone();
    let mut app = App::new(
        Reading {
            repo: "act-mcp".to_owned(),
            branch: "master".to_owned(),
            head: Some(head),
            store: StoreState::Present {
                rows: entries.len(),
            },
            evidence_present: true,
            evidence_path: PathBuf::from(EVIDENCE),
            entries,
            ..Reading::default()
        },
        NOW,
    );
    app.set_size(100, 20);
    app
}

/// `b` opens the panel on the selected commit and asks for nothing: no git, no evidence walk, no MCP
/// call — the rows were published by `fathom index` and are already in memory. The timeline's cursor
/// is not the panel's, and `Esc` hands the screen back exactly as it was left.
#[test]
fn b_opens_the_panel_on_the_selected_commit_and_esc_gives_the_timeline_back() {
    let mut app = app_with_bench();
    press(&mut app, KeyCode::Char('j'));
    let before = (app.selection, app.offset);
    let sha = app.selected().expect("a row").sha.clone();
    assert_eq!(sha, "8ac22d100000000000000000000000000000000");

    let commands = press(&mut app, KeyCode::Char('b'));
    assert!(
        commands.is_empty(),
        "opening the panel starts no work at all"
    );
    assert!(
        matches!(app.screen, Screen::Bench(ref open) if open == &sha),
        "{:?}",
        app.screen
    );
    let panel = app.bench_view().expect("the panel is up");
    assert_eq!(panel.sha, sha);
    assert_eq!(
        panel.subject, "an older commit",
        "the heading is the commit's own"
    );
    assert_eq!(panel.window, 2, "itself plus the one commit below it");
    assert_eq!(
        panel.lines.len(),
        3,
        "cold_start_ms is not in this commit's rows"
    );
    assert_eq!(
        (app.selection, app.offset),
        before,
        "the timeline's cursor is untouched by opening a panel"
    );

    // The panel has its own cursor, and the timeline's does not move while it is used.
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.bench_view().expect("panel").selection, 1);
    assert_eq!(app.selection, before.0, "the timeline did not move");
    press(&mut app, KeyCode::Char('k'));
    press(&mut app, KeyCode::Char('k'));
    assert_eq!(
        app.bench_view().expect("panel").selection,
        0,
        "and it clamps at the top instead of wrapping"
    );
    press(&mut app, KeyCode::Char('G'));
    assert_eq!(
        app.bench_view().expect("panel").selection,
        2,
        "G goes to the last row"
    );

    // `f` toggles the overlay, and when there is nothing excluded it says so rather than redrawing.
    press(&mut app, KeyCode::Char('f'));
    assert!(app.bench_view().expect("panel").show_excluded);
    assert!(app
        .message
        .clone()
        .expect("a line")
        .contains("no run at this commit was excluded"));
    press(&mut app, KeyCode::Char('f'));
    assert!(!app.bench_view().expect("panel").show_excluded);

    // Esc: the timeline, where it was. The panel is kept, so `b` returns to the same row.
    press(&mut app, KeyCode::Esc);
    assert!(matches!(app.screen, Screen::Timeline));
    assert_eq!(
        (app.selection, app.offset),
        before,
        "exactly where it was left"
    );
    press(&mut app, KeyCode::Char('b'));
    assert_eq!(
        app.bench_view().expect("panel").selection,
        2,
        "the same panel, the same row — the reader was reading that one"
    );
    // `b` closes it too, like the key that opened it.
    press(&mut app, KeyCode::Char('b'));
    assert!(matches!(app.screen, Screen::Timeline));
}

/// `d` from the panel is the same commit's diff: the two detail screens are one hop apart, not one trip
/// back through the timeline.
#[test]
fn d_on_the_panel_is_that_commits_diff() {
    let mut app = app_with_bench();
    press(&mut app, KeyCode::Char('b'));
    let sha = app.bench_view().expect("panel").sha.clone();
    let commands = press(&mut app, KeyCode::Char('d'));
    assert_eq!(
        commands,
        vec![Command::Diff(sha.clone())],
        "one read of one patch"
    );
    assert!(
        matches!(app.screen, Screen::Diff(ref open) if open == &sha),
        "{:?}",
        app.screen
    );
}

/// A re-read rebuilds an open panel from the new rows — and keeps the reader's row and overlay, because
/// a frame that jumps the cursor mid-row makes you start over.
#[test]
fn a_re_read_rebuilds_an_open_panel_and_keeps_the_readers_place() {
    let mut app = app_with_bench();
    press(&mut app, KeyCode::Char('b'));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('f'));
    let sha = app.bench_view().expect("panel").sha.clone();

    // The index published a run for the same commit with one more metric in it.
    let mut reading = app.reading.clone();
    reading.entries[0].detail = commit_with(
        &sha,
        vec![
            group(
                "cold_start_ms",
                &[("job", "hyperfine-bench")],
                Some("ms"),
                400.0,
            ),
            group(
                "tokenizer_throughput",
                &[("job", "hyperfine-bench")],
                Some("ops/s"),
                200_000.0,
            ),
        ],
        json!({"excluded_runs": [{"run_id": "r-1", "status": "failure", "reason": "run_status_excluded"}]}),
    );
    assert_eq!(
        app.apply(fathom::tui::engine::Event::Read(Box::new(reading))),
        Vec::new(),
        "a re-read asks for nothing"
    );
    let panel = app.bench_view().expect("still open");
    assert_eq!(panel.sha, sha, "same commit");
    assert_eq!(
        panel.lines.len(),
        4,
        "rebuilt from the new row: its two groups, plus the two the older commits still report and \
         this one no longer does"
    );
    assert_eq!(
        panel.selection.min(panel.lines.len() - 1),
        1,
        "and still on the second row"
    );
    assert!(panel.show_excluded, "and still with the overlay up");
    assert!(panel
        .notice()
        .expect("now there is a run to name")
        .contains("r-1"));
}

/// `--bench REV` is the same screen, and a revision outside the window is a sentence, not an empty
/// table: the window is what `--from`/`--to`/`--limit` decided, and the panel cannot know more.
#[test]
fn bench_rev_outside_the_window_is_a_sentence_not_an_empty_table() {
    let mut app = app_with_bench();
    let inside = app.reading.entries[2].sha.clone();
    assert!(app.show_bench(&inside).is_ok());
    assert!(matches!(app.screen, Screen::Bench(ref sha) if sha == &inside));
    let refusal = app
        .show_bench("ffffffff00000000000000000000000000000000")
        .expect_err("not in the window");
    assert!(
        refusal.contains("not in this window") && refusal.contains("--limit"),
        "{refusal}"
    );
    assert!(
        matches!(app.screen, Screen::Bench(ref sha) if sha == &inside),
        "the refusal did not move the screen off the panel it was on"
    );
}

// ---------------------------------------------------------------------------------------------
// drawn
// ---------------------------------------------------------------------------------------------

/// Draw the app to a `TestBackend` and read the cells back out, trailing spaces trimmed.
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

fn tidy(text: &str) -> String {
    text.lines()
        .map(|line| line.trim_end().to_owned())
        .collect::<Vec<_>>()
        .join("\n")
        .trim_matches('\n')
        .to_owned()
}

fn check(app: &App, palette: &Palette, width: u16, height: u16, expected: &str) -> String {
    let have = screen(app, palette, width, height);
    assert_eq!(tidy(&have), tidy(expected), "\ndrawn:\n{have}");
    have
}

/// The panel as a 100-column terminal draws it: header, table with its axis-bearing titles, the status
/// row for the selected line, and the key line that names `f`. Every cell the state layer computed, in
/// the order it computed them.
#[test]
fn the_panel_at_one_hundred_columns() {
    let mut open = app_with_bench();
    press(&mut open, KeyCode::Char('b'));
    check(
        &open,
        &Palette::interactive(),
        100,
        20,
        r#"
BENCH  3b8e9f1  drop node_modules benchmark: none for this commit · coverage: none for this commit …
┌ benchmark ───────────────────────────────────────────────────────────────────────────────────────┐
│   metric (name · labels)    commit 3b8e9f1      spread(3 runs) vs earlier (5%) history (last 3)  │
│ ▸ cold_start_ms ms · job=h…      412.5 n=1       [412.5] 1 run               — — 1 point         │
│   ~job_duration_s s · job=…       91.8 n=2 [91.8..91.9] 3 runs         ▲+2.0%~ ▁▄█ 89..91.8      │
│   queue_depth (unit missin…          3 n=1           [3] 1 run         not cmp                   │
│   tokenizer_throughput ops…    1.847e5 n=2  [1.843e5..1.851e5]          ▲+3.2% ▁▂█ 1.78e5..1.85e5│
│                                                                                                  │
│                                                                                                  │
│                                                                                                  │
│                                                                                                  │
│                                                                                                  │
│                                                                                                  │
│                                                                                                  │
│                                                                                                  │
│                                                                                                  │
└──────────────────────────────────────────────────────────────────────────────────────────────────┘
cold_start_ms · job=hyperfine-bench step=Run benchmark  ·  no earlier commit in this window has this
metric · — is unknown, not flat  ·  ms = 412.5 (median of 1 sample)  ·  spread 412.5 over 1 run  ·
1/4  [f] excluded hidden  j/k row   g/G ends   d diff   b/Esc back   ? help   q quit
"#,
    );
}

/// The same panel in a terminal that has given up three of its five columns, drawn rather than
/// described: the metric and its value are what survive, the heading keeps enough of the SHA to say
/// which commit this is, and the status sentence wraps onto both rows instead of ending in an ellipsis.
#[test]
fn the_panel_at_thirty_eight_columns() {
    let mut open = app_with_bench();
    press(&mut open, KeyCode::Char('b'));
    check(
        &open,
        &Palette::interactive(),
        38,
        20,
        r#"
BENCH  3b8e9f1  d…benchmark: none for…
┌ benchmark ─────────────────────────┐
│   metric (name · la… commit 3b8e9f1│
│ ▸ cold_start_ms ms …      412.5 n=1│
│   ~job_duration_s s…       91.8 n=2│
│   queue_depth (unit…          3 n=1│
│   tokenizer_through…    1.847e5 n=2│
│                                    │
│                                    │
│                                    │
│                                    │
│                                    │
│                                    │
│                                    │
│                                    │
│                                    │
└────────────────────────────────────┘
cold_start_ms · job=hyperfine-bench
step=Run benchmark  ·  no earlier
1/4  [f] excluded hidden  j/k row   g/
"#,
    );
}

/// A terminal too narrow for the whole table gives up columns in a declared order — `vs earlier`, then
/// `spread`, then `history` — and never loses the metric, its value, or the status line, which carries
/// everything the dropped columns carried.
#[test]
fn a_narrow_terminal_drops_the_columns_the_status_line_already_carries() {
    let mut app = app_with_bench();
    press(&mut app, KeyCode::Char('b'));
    let palette = Palette::interactive();
    for width in [38_u16, 50, 62, 74, 86] {
        let drawn = screen(&app, &palette, width, 20);
        for line in drawn.lines() {
            assert!(
                line.chars().count() <= usize::from(width),
                "a {width}-column terminal printed a wider line: {line:?}"
            );
        }
        assert!(
            drawn.contains("tokenizer_thro") && drawn.contains("1.847e5 n=2"),
            "{width} columns still shows the metric (cut to its column) and its value whole: {drawn}"
        );
    }
    // The narrowest of them has given up all three droppable columns and still explains the row.
    let narrow = screen(&app, &palette, 38, 20);
    assert!(
        !narrow.contains("vs earlier")
            && !narrow.contains("spread(")
            && !narrow.contains("history ("),
        "38 columns: {narrow}"
    );
    let drawn = screen(&app, &palette, 38, 20);
    assert!(
        drawn.lines().any(|line| line.contains("1.847e5 n=2")),
        "the value is still on the row: {drawn}"
    );
}

/// `--plain` gets the same table, the same order and the same status sentence: one rendering, two
/// backends, and no second implementation of the numbers.
#[test]
fn plain_output_is_the_same_panel_without_a_terminal() {
    let mut app = app_with_bench();
    press(&mut app, KeyCode::Char('b'));
    let palette = Palette::plain();
    let panel = app.bench_view().expect("panel");
    let text = fathom::tui::plain::bench(panel, &palette, 100);
    assert!(text.contains("BENCH  3b8e9f1"));
    assert!(text.contains("metric (name"), "{text}");
    assert!(text.contains("tokenizer_throughput"), "{text}");
    assert!(text.contains("+3.2%"), "{text}");
    assert!(
        text.contains("band 5% of value"),
        "the band is stated even with no key to change it: {text}"
    );
    // The sample count is on the value, not only in the status line, because there is no status line
    // here: the plain dump has to carry the number and how many samples it is the median of, or a
    // single-sample measurement reads exactly like a forty-sample one.
    assert!(text.contains("1.847e5 n=2"), "{text}");
    assert!(
        text.contains("not cmp"),
        "and the incomparable row is marked in the text as it is on screen: {text}"
    );
}
