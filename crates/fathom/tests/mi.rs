//! The MI analyzer, graded against ADR-0007's golden vectors.
//!
//! Three kinds of test, each here because a different mistake hides somewhere else:
//!
//! * **The golden vectors** (`crates/act-mcp/tests/testdata/mi/vectors/`) are hand-counted token
//!   tables with V, G, LOC, MI and the band re-derived by `scripts/mi_reference.py`. The Rust front
//!   end is compared at the *table* level, not only at the final MI: a tokeniser that got the volume
//!   right by accident — a `>` counted twice, a `;` counted twice, an attribute counted as tokens —
//!   would still produce a plausible MI, and the occurrence table is what catches it.
//! * **The aggregation fixtures** (`aggregates/`) are commit-sized populations with hand-computed
//!   weighted means, hotspots, watchlist transitions and the delta. The case the ticket names — a
//!   12-line file must not outvote a 900-line one — is one of them, and it is asserted both ways: the
//!   aggregate is what the LOC weight says, and it is *not* what a per-file mean would have said.
//! * **Properties and reality**: order-independence, byte-identical repeats, the ladder's gap rungs,
//!   and a run over this repository's own source, which is a sanity check and not a golden because
//!   the tree moves.
//!
//! None of these need `git`, `act`, a network or a TTY: the analyzer takes `(path, bytes)`, and every
//! fixture here is a file on disk.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use fathom::mi::{self, Analyzer, CommitAggregate, Definition, FileMetrics, Status};
use serde_json::Value;

/// The repository root, two levels up from `crates/fathom`.
fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/fathom sits two levels under the repo root")
        .to_path_buf()
}

/// ADR-0007's golden vectors, which live beside the crate that first needed them (ADR-0005 §9).
fn testdata() -> PathBuf {
    repo().join("crates/act-mcp/tests/testdata/mi")
}

/// Every JSON document of a directory, in filename order, so a failure list is deterministic.
fn documents(dir: &Path) -> Vec<(String, Value)> {
    let mut out: Vec<(String, Value)> = Vec::new();
    for entry in
        fs::read_dir(dir).unwrap_or_else(|error| panic!("reading {}: {error}", dir.display()))
    {
        let path = entry.expect("a directory entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let text = fs::read_to_string(&path).expect("read the fixture");
        let parsed: Value = serde_json::from_str(&text)
            .unwrap_or_else(|error| panic!("{} is not JSON: {error}", path.display()));
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .expect("a name")
            .to_owned();
        out.push((name, parsed));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn occurrences(table: &Value) -> BTreeMap<String, u64> {
    table
        .as_object()
        .expect("an occurrences table")
        .iter()
        .map(|(token, n)| (token.clone(), n.as_u64().expect("a count")))
        .collect()
}

fn u64_of(value: &Value) -> u64 {
    value.as_u64().expect("a count")
}

/// The sum of an occurrence table: Halstead `N`, or the operand count.
fn total(table: &BTreeMap<String, u64>) -> u64 {
    table.values().sum::<u64>()
}

/// The oracle's numeric normalisation: `4` and `4.0` are the same claim about a fixture.
fn normalise(value: &Value) -> Value {
    match value {
        Value::Number(number) => Value::String(format!("{:?}", number.as_f64().unwrap_or(0.0))),
        Value::Array(items) => Value::Array(items.iter().map(normalise).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, inner)| (key.clone(), normalise(inner)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Re-derive one vector's token tables in Rust and return every way it disagrees.
///
/// The vectors name a *language*, so the front end is chosen by name and not by the file extension:
/// a vector whose fixture and language disagreed would otherwise pass by accident.
fn check_vector(definition: &Definition, name: &str, vector: &Value) -> Vec<String> {
    let mut problems = Vec::new();
    let language = vector["language"].as_str().expect("a language");
    let source = vector["source"].as_str().expect("a source");
    let path = testdata().join("vectors").join(source);
    let path = path
        .canonicalize()
        .unwrap_or_else(|error| panic!("{name}: {}: {error}", path.display()));
    let text = fs::read_to_string(&path).expect("read the fixture source");
    let expect = &vector["expect"];
    let derived = vector
        .get("derived")
        .and_then(Value::as_bool)
        .unwrap_or(true);

    let counts = match fathom::mi::tokenise(definition, language, &text) {
        None => {
            problems.push(format!(
                "{name}: this build has no front end for {language}"
            ));
            return problems;
        }
        Some(Ok(counts)) => counts,
        Some(Err(error)) => {
            if derived {
                problems.push(format!(
                    "{name}: the front end refused a vector it must measure: {error}"
                ));
            } else if expect["status"] != "parse_error" {
                problems.push(format!(
                    "{name}: expected {}, got parse_error: {error}",
                    expect["status"]
                ));
            }
            return problems;
        }
    };

    if !derived {
        // A gap vector: the interesting claim is which rung it fell on, plus whatever the ladder can
        // know from a clean lex — that a `vacuous` file really has no LOC or no vocabulary, and that
        // a `parse_error` really does not lex (handled above).
        let base = definition.languages[language].decision_points.base;
        let measurement = fathom::mi::measure(definition, &counts, Status::Ok, base);
        let want = expect["status"].as_str().expect("a status");
        if measurement.status.as_str() != want {
            problems.push(format!(
                "{name}: vector says {want}, the ladder says {}",
                measurement.status
            ));
        }
        if want == "vacuous" {
            let no_lines = counts.statements == 0;
            let no_vocabulary = counts.operator_unique() < 2;
            if !(no_lines || no_vocabulary) {
                problems.push(format!(
                    "{name}: claims vacuous but measured LOC {} and vocabulary {}",
                    counts.statements,
                    counts.operator_unique()
                ));
            }
        }
        for key in ["mi", "volume", "cyclomatic", "band"] {
            if expect.get(key).is_some_and(|value| !value.is_null()) {
                problems.push(format!("{name}: a gap vector must carry no {key}"));
            }
        }
        return problems;
    }

    // 1. the hand-counted tables, exactly: totals, vocabulary, and every occurrence.
    for (key, got) in [
        ("operators", counts.operators.clone()),
        ("operands", counts.operands.clone()),
    ] {
        let table = &expect[key];
        let want = occurrences(&table["occurrences"]);
        if want != got {
            let missing: Vec<(&String, &u64)> =
                want.iter().filter(|(t, _)| !got.contains_key(*t)).collect();
            let extra: Vec<(&String, &u64)> =
                got.iter().filter(|(t, _)| !want.contains_key(*t)).collect();
            let differ: Vec<String> = want
                .iter()
                .filter_map(|(token, n)| {
                    got.get(token)
                        .filter(|have| *have != n)
                        .map(|have| format!("{token}: {have} vs {n}"))
                })
                .collect();
            problems.push(format!(
                "{name}: {key} tables differ — missing {missing:?}, unexpected {extra:?}, miscounted {differ:?}"
            ));
        }
        if u64_of(&table["total"]) != total(&got) || u64_of(&table["unique"]) != got.len() as u64 {
            problems.push(format!(
                "{name}: {key} total/unique do not match the occurrence table"
            ));
        }
    }
    let want_decisions = occurrences(&expect["decision_points"]);
    if want_decisions != counts.decision_points {
        problems.push(format!(
            "{name}: decision points {:?}, front end says {:?}",
            want_decisions, counts.decision_points
        ));
    }
    if u64_of(&expect["statements"]) != counts.statements {
        problems.push(format!(
            "{name}: LOC claims {}, front end counts {}",
            u64_of(&expect["statements"]),
            counts.statements
        ));
    }

    // 2. the arithmetic, re-derived from those counts.
    let base = definition.languages[language].decision_points.base;
    let measurement = fathom::mi::measure(definition, &counts, Status::Ok, base);
    if measurement.status != Status::Ok {
        problems.push(format!("{name}: the ladder says {}", measurement.status));
        return problems;
    }
    let volume = expect["volume"].as_f64().expect("a volume");
    if measurement.volume != Some(volume) {
        problems.push(format!(
            "{name}: volume claims {volume:?}, V = N*log2(n) gives {:?}",
            measurement.volume
        ));
    }
    if u64_of(&expect["cyclomatic"]) != measurement.cyclomatic.unwrap_or(0) {
        problems.push(format!(
            "{name}: cyclomatic claims {}, G = 1 + decision points gives {:?}",
            u64_of(&expect["cyclomatic"]),
            measurement.cyclomatic
        ));
    }
    let raw = expect["mi_raw"].as_f64().expect("an mi_raw");
    if !fathom::mi::score::mi_raw_agrees(raw, measurement.mi_raw.unwrap_or(0.0)) {
        problems.push(format!(
            "{name}: MI_raw claims {raw:?}, the formula gives {:?} (a different libm gets {} of relative room and none at all in the stored value)",
            measurement.mi_raw, fathom::mi::score::MI_RAW_TOLERANCE
        ));
    }
    let mi = expect["mi"].as_f64().expect("an mi");
    if measurement.mi != Some(mi) {
        problems.push(format!(
            "{name}: MI claims {mi}, the formula gives {:?}",
            measurement.mi
        ));
    }
    let band = expect["band"].as_str().expect("a band");
    if measurement.band.map(|b| b.as_str()) != Some(band) {
        problems.push(format!(
            "{name}: band claims {band}, the bands give {:?}",
            measurement.band
        ));
    }
    // P14: a fixture sitting on a rounding or band edge depends on the host's math library, so it is
    // not allowed to exist. The vectors are checked for it here as well as by the python checker,
    // because this is the implementation whose boundary they pin.
    let distance = fathom::mi::score::boundary_distance(definition, &measurement);
    if distance < 1.0 {
        problems.push(format!(
            "{name}: MI {mi} sits within the boundary slack ({distance})"
        ));
    }
    problems
}

#[test]
fn every_golden_vector_rederives_in_rust() {
    let definition = Definition::embedded();
    let mut problems = Vec::new();
    let mut measured = 0;
    for (name, vector) in &documents(&testdata().join("vectors")) {
        let found = check_vector(&definition, name, vector);
        if found.is_empty() {
            measured += 1;
        }
        problems.extend(found);
    }
    assert!(
        problems.is_empty(),
        "{} vector(s) disagree with the Rust front end:\n{}",
        problems.len(),
        problems.join("\n")
    );
    assert!(
        measured >= 12,
        "expected the whole vector set, measured {measured}"
    );
}

#[test]
fn the_reformat_pair_measures_identically() {
    // The bytes differ; the token stream may not. A reformat that moves MI puts `cargo fmt` on a
    // maintainability trend line, which is the harm §3.4 exists to prevent.
    let definition = Definition::embedded();
    for (name, vector) in &documents(&testdata().join("vectors")) {
        let Some(sources) = vector.get("sources_equal").and_then(Value::as_array) else {
            continue;
        };
        assert_eq!(sources.len(), 2, "{name}: a pair is two files");
        let tables: Vec<(String, u64, u64)> = sources
            .iter()
            .map(|source| {
                let path = testdata().join("src").join(source.as_str().unwrap());
                let text = fs::read_to_string(&path).expect("read the pair member");
                let counts =
                    fathom::mi::tokenise(&definition, vector["language"].as_str().unwrap(), &text)
                        .expect("a front end for the vector's language")
                        .expect("lex the pair member");
                (
                    source.as_str().unwrap().to_owned(),
                    counts.operator_total(),
                    counts.statements,
                )
            })
            .collect();
        assert_ne!(
            fs::read_to_string(testdata().join("src").join(sources[0].as_str().unwrap()))
                .expect("read"),
            fs::read_to_string(testdata().join("src").join(sources[1].as_str().unwrap()))
                .expect("read"),
            "{name}: the pair members are the same file, which makes the pair vacuous"
        );
        assert_eq!(
            tables[0].1, tables[1].1,
            "{name}: operator counts differ between the pair"
        );
        assert_eq!(
            tables[0].2, tables[1].2,
            "{name}: LOC differs between the pair"
        );
    }
}

/// Rebuild a fixture row into a [`FileMetrics`] — the way a stored row is rebuilt in production.
fn fixture_files(rows: &Value, definition: &Definition) -> Vec<FileMetrics> {
    rows.as_array()
        .expect("a file list")
        .iter()
        .map(|row| {
            let status: Status = row["status"]
                .as_str()
                .expect("a status")
                .parse()
                .expect("a known reason");
            FileMetrics::from_stored(
                row["path"].as_str().expect("a path"),
                status,
                row.get("loc").and_then(Value::as_u64),
                row.get("mi").and_then(Value::as_f64),
                definition,
            )
        })
        .collect()
}

/// The fixture comparison the oracle does: every key the fixture claims, and nothing else.
fn compare_expect(row: &CommitAggregate, expect: &Value) -> Vec<String> {
    let mut derived = serde_json::to_value(row).expect("a row serialises");
    // Per-hotspot `delta` is additive to the fixture's three fields (ADR-0006 §6.4 permits additive
    // fields, and the ticket asks for it), so the fixtures stay the contract and the extra is extra.
    if let Some(hotspots) = derived.get_mut("hotspots").and_then(Value::as_array_mut) {
        for hotspot in hotspots {
            if let Some(object) = hotspot.as_object_mut() {
                object.remove("delta");
            }
        }
    }
    let mut problems = Vec::new();
    for (key, want) in expect.as_object().expect("an expect block") {
        let got = derived.get(key).cloned().unwrap_or(Value::Null);
        if normalise(want) != normalise(&got) {
            problems.push(format!(
                "{key}: fixture claims {}, derived {}",
                serde_json::to_string(want).unwrap_or_default(),
                serde_json::to_string(&got).unwrap_or_default()
            ));
        }
    }
    problems
}

#[test]
fn the_aggregate_fixtures_rederive_in_rust() {
    let definition = Definition::embedded();
    let mut problems = Vec::new();
    let mut checked = 0;
    for (name, fixture) in &documents(&testdata().join("aggregates")) {
        let files = fixture_files(&fixture["files"], &definition);
        let parent = fixture
            .get("parent_files")
            .map(|rows| fixture_files(rows, &definition));
        let row = mi::commit_row(&definition, &files, parent.as_deref());
        for problem in compare_expect(&row, &fixture["expect"]) {
            problems.push(format!("{name}: {problem}"));
        }
        checked += 1;
    }
    assert!(
        problems.is_empty(),
        "{} aggregate assertion(s) disagree:\n{}",
        problems.len(),
        problems.join("\n")
    );
    assert_eq!(checked, 2, "both commit fixtures should be checked");
}

/// The case the ticket names, asserted from the fixture and again from first principles.
#[test]
fn a_small_file_does_not_outvote_a_large_one() {
    let definition = Definition::embedded();
    let build = |pairs: &[(&str, u64, f64)]| {
        pairs
            .iter()
            .map(|(path, loc, mi)| {
                FileMetrics::from_stored(path, Status::Ok, Some(*loc), Some(*mi), &definition)
            })
            .collect::<Vec<_>>()
    };
    let files = build(&[("src/mod.rs", 12, 95.0), ("src/big.rs", 900, 61.0)]);
    let row = mi::aggregate(&definition, &files);
    assert_eq!(row.aggregate, Some(61.4), "(95*12 + 61*900)/912 = 61.4473");
    assert_eq!(row.loc, 912);
    assert_eq!(row.files, 2);
    // The lie the weight exists to prevent, asserted as a number: a per-file mean would have reported
    // 78.0, letting a twelve-line module speak for a nine-hundred-line one.
    let per_file = fathom::mi::round_storage(&definition, (95.0 + 61.0) / 2.0);
    assert_eq!(per_file, 78.0);
    assert_ne!(row.aggregate, Some(per_file));
    // and the delta against a parent that lost its worst file
    let parent = build(&[
        ("src/mod.rs", 12, 95.0),
        ("src/big.rs", 900, 60.0),
        ("src/gone.rs", 40, 12.0),
    ]);
    let row = mi::commit_row(&definition, &files, Some(&parent));
    assert_eq!(row.delta_vs_parent, Some(3.0));
    assert_eq!(row.sub_threshold_removed, vec!["src/gone.rs".to_owned()]);
    assert!(row.sub_threshold_added.is_empty());
    assert_eq!(row.hotspots[0].path, "src/big.rs", "worst first");
}

/// The properties the aggregation rules have to satisfy as properties, not only as fixtures (C8).
#[test]
fn the_aggregation_rules_hold_as_properties() {
    let definition = Definition::embedded();
    let amber = definition.bands.amber_below;
    let red = definition.sub_threshold_below();
    let rows = [
        ("a.rs", 10u64, 10.0f64),
        ("b.rs", 20, red - 0.1),
        ("c.rs", 30, red),
        ("d.rs", 40, amber - 0.1),
        ("e.rs", 50, amber),
        ("f.rs", 60, 99.0),
    ];
    let files: Vec<FileMetrics> = rows
        .iter()
        .map(|(path, loc, mi)| {
            FileMetrics::from_stored(path, Status::Ok, Some(*loc), Some(*mi), &definition)
        })
        .collect();
    let row = mi::aggregate(&definition, &files);

    // The watchlist is exactly the set below the red edge, and amber is the set below the amber edge.
    // The rows are placed at the *declared* edges rather than at numbers, because an exact edge
    // belongs to the better band: `red` itself is amber, and `amber` itself is green. Hard-coding the
    // numbers here would have been a second copy of the edges, which is what §5.1 refuses.
    assert_eq!(
        row.sub_threshold_set,
        vec!["a.rs".to_owned(), "b.rs".to_owned()]
    );
    assert_eq!(
        row.amber_paths,
        vec![
            "a.rs".to_owned(),
            "b.rs".to_owned(),
            "c.rs".to_owned(),
            "d.rs".to_owned()
        ],
        "`amber_paths` is every path below the amber edge, the red ones included: the view greys the\n\
         whole set and does not re-derive a band"
    );
    assert!(red < amber);
    // Hotspots are worst-first, at most the declared N, and tie-broken by LOC descending then path.
    assert_eq!(row.hotspots.len(), definition.aggregation.hotspots_default);
    assert!(row.hotspots.windows(2).all(|pair| pair[0].mi <= pair[1].mi));
    assert_eq!(row.hotspots[0].path, "a.rs");
    // `loc` is the sum of the counted files, and the counts cover every reason.
    let mut mixed = files.clone();
    mixed.push(FileMetrics::from_stored(
        "g.rs",
        Status::Vacuous,
        Some(0),
        None,
        &definition,
    ));
    mixed.push(FileMetrics::from_stored(
        "h.go",
        Status::UnsupportedLanguage,
        None,
        None,
        &definition,
    ));
    mixed.push(FileMetrics::from_stored(
        "i.rs",
        Status::ParseError,
        None,
        None,
        &definition,
    ));
    mixed.push(FileMetrics::from_stored(
        "j.rs",
        Status::SkippedNoise,
        None,
        None,
        &definition,
    ));
    let mixed_row = mi::aggregate(&definition, &mixed);
    assert_eq!(mixed_row.loc, row.loc, "gaps carry no LOC into the weight");
    assert_eq!(mixed_row.files, row.files);
    assert_eq!(
        mixed_row.unanalysed_paths, 3,
        "vacuous, unsupported_language and parse_error are the caveats; noise is not"
    );
    assert_eq!(mixed_row.counts.get("ok"), Some(&6));
    // An empty population is unknown, not zero, and not empty-but-zero.
    let empty = mi::aggregate(
        &definition,
        &[FileMetrics::from_stored(
            "k.rs",
            Status::Excluded,
            None,
            None,
            &definition,
        )],
    );
    assert_eq!(empty.aggregate, None);
    assert_eq!(empty.loc, 0);
    assert_eq!(empty.hotspots.len(), 0);
    assert_eq!(
        empty.aggregate_basis.as_deref(),
        Some("no measurable file in scope")
    );
}

#[test]
fn the_aggregate_does_not_depend_on_the_order_rows_arrived_in() {
    // The ADR refuses the carry-forward recurrence for exactly this reason: float addition is only
    // deterministic given the order, so a pinned accumulation order has to be a *fact*, not an
    // accident of the query.
    let definition = Definition::embedded();
    let files: Vec<FileMetrics> = [
        ("src/a.rs", 7u64, 33.3f64),
        ("src/b.rs", 91, 61.25),
        ("src/c.rs", 3, 99.9),
        ("src/d.rs", 400, 12.05),
        ("src/e.rs", 16, 55.9),
    ]
    .iter()
    .map(|(path, loc, mi)| {
        FileMetrics::from_stored(path, Status::Ok, Some(*loc), Some(*mi), &definition)
    })
    .collect();
    let baseline = serde_json::to_vec(&mi::aggregate(&definition, &files)).expect("serialises");
    let mut rotated = files.clone();
    for _ in 0..files.len() {
        rotated.rotate_left(1);
        let other = serde_json::to_vec(&mi::aggregate(&definition, &rotated)).expect("serialises");
        assert_eq!(baseline, other, "shuffling the rows moved the aggregate");
    }
    let mut reversed = files;
    reversed.reverse();
    assert_eq!(
        baseline,
        serde_json::to_vec(&mi::aggregate(&definition, &reversed)).expect("serialises")
    );
}

#[test]
fn a_tree_with_one_broken_file_still_produces_a_number() {
    // The refusal is per file, and the commit keeps its number: a broken file must neither poison the
    // aggregate nor be quietly left out of it.
    let analyzer = Analyzer::default();
    let definition = Definition::embedded();
    let good = "pub fn add(a: u8, b: u8) -> u8 {\n    a + b\n}\n";
    let broken = "pub fn broken(who: &str) -> String {\n    format!(\"hello \n}\n";
    let files = vec![
        analyzer.analyse("src/ok.rs", good.as_bytes()),
        analyzer.analyse("src/broken.rs", broken.as_bytes()),
        analyzer.analyse("src/notes.rs", b"// a file of prose only\n"),
        analyzer.analyse("src/main.go", b"package main\n"),
        analyzer.analyse("src/helper.py", "def f(x):\n    return x + 1\n".as_bytes()),
    ];
    let statuses: Vec<&str> = files.iter().map(|file| file.status.as_str()).collect();
    assert_eq!(
        statuses,
        vec!["ok", "parse_error", "vacuous", "excluded", "ok"],
        "one rung per file, in ladder order, and a gap that is not a zero"
    );
    assert!(files[1].detail.is_some(), "a refusal names its reason");
    assert!(files
        .iter()
        .all(|file| file.mi.is_some() || file.status != Status::Ok));
    let json = serde_json::to_string(&files).expect("serialises");
    assert!(
        !json.contains("\"mi\":null"),
        "unknown is absent, never null: {json}"
    );
    let row = mi::aggregate(&definition, &files);
    assert!(row.has_mi(), "two measurable files are enough for a number");
    assert_eq!(row.files, 2);
    assert_eq!(
        row.unanalysed_paths, 2,
        "parse_error and vacuous are caveats; the `.go` file is not source at all"
    );
}

#[test]
fn a_language_without_a_front_end_is_unsupported_and_counted() {
    // Add a language to the model without writing its lexer — the state a future `.go` entry would
    // be in — and the analyzer must say so out loud instead of measuring nothing quietly.
    let mut raw: Value =
        serde_json::from_str(mk_definition_json()).expect("the shipped definition parses");
    raw["languages"]["go"] = serde_json::json!({
        "display": "Go",
        "extensions": [".go"],
        "enabled_in_v1": false,
        "operator_keywords": ["func", "if", "for"],
        "operand_keywords": ["nil", "true"],
        "operator_punctuation": ["+", ":="],
        "ignored_punctuation": ["(", ")", ";"],
        "decision_points": {"base": 1, "rules": [{"id": "if", "token": "if", "weight": 1}]},
        "statements": {"formula": "LOC = statements"}
    });
    raw["scope"]["source_extensions"][".go"] = Value::String("go".to_owned());
    let definition =
        Definition::from_json(&raw.to_string()).expect("the patched definition parses");
    let analyzer = Analyzer::new(
        definition.clone(),
        fathom::classify::Classifier::default(),
        ["rust", "python", "go"],
        2 * 1024 * 1024,
    );
    let metrics = analyzer.analyse("pkg/main.go", b"func main() {}\n");
    assert_eq!(metrics.status, Status::UnsupportedLanguage);
    assert_eq!(metrics.language.as_deref(), Some("go"));
    assert_eq!(metrics.mi, None);
    let row = mi::aggregate(&definition, &[metrics]);
    assert_eq!(
        row.unanalysed_paths, 1,
        "the view has to be able to say why the column is empty"
    );
    assert!(!row.has_mi());
    assert_eq!(
        fathom::mi::tokenise(&definition, "go", "func main() {}"),
        None
    );
}

fn mk_definition_json() -> &'static str {
    include_str!("../../../schema/mi-definition.json")
}

#[test]
fn two_runs_over_one_tree_are_byte_identical() {
    let analyzer = Analyzer::default();
    let definition = Definition::embedded();
    let sources: Vec<(String, Vec<u8>)> = repository_sources(&[("crates", &["rs"])])
        .into_iter()
        .take(40)
        .collect();
    let first: Vec<String> = sources
        .iter()
        .map(|(path, bytes)| {
            serde_json::to_string(&analyzer.analyse(path, bytes)).expect("serialises")
        })
        .collect();
    let second: Vec<String> = sources
        .iter()
        .map(|(path, bytes)| {
            serde_json::to_string(&analyzer.analyse(path, bytes)).expect("serialises")
        })
        .collect();
    assert_eq!(first, second, "same tree, two runs, identical MI");
    let files: Vec<FileMetrics> = sources
        .iter()
        .map(|(path, bytes)| analyzer.analyse(path, bytes))
        .collect();
    let mut shuffled = files.clone();
    let half = shuffled.len() / 2;
    shuffled.rotate_left(half);
    assert_eq!(
        serde_json::to_vec(&mi::aggregate(&definition, &files)).expect("serialises"),
        serde_json::to_vec(&mi::aggregate(&definition, &shuffled)).expect("serialises"),
        "the same tree, reordered, gave a different aggregate"
    );
}

/// Walk the repository's own source. `groups` is `(directory, extensions)`.
fn repository_sources(groups: &[(&str, &[&str])]) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    for (directory, extensions) in groups {
        let root = repo().join(directory);
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name.starts_with('.') || name == "target" || name == "node_modules" {
                    continue;
                }
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                if !extensions.contains(&extension) {
                    continue;
                }
                let relative = path
                    .strip_prefix(repo())
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push((relative, fs::read(&path).expect("read the source")));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// The sanity run the ticket asks for: this repository's own source, which is not a golden because
/// the tree moves, and which prints the histograms ADR-0007 §5.3 publishes. Run with
/// `cargo test -- --nocapture` to read them. What the assertions pin changed when §4.1 made the
/// scoring unit a function: the shape this test used to assert — small files outscoring big ones — was
/// the *bug* wearing a property's clothes, and a corpus that clamps to 0.0 at the bottom of the scale
/// is the failure it is here to notice.
#[test]
fn the_repositorys_own_source_measures_plausibly() {
    let analyzer = Analyzer::default();
    let definition = Definition::embedded();
    let sources = repository_sources(&[("crates", &["rs"]), ("scripts", &["py"])]);
    assert!(sources.len() > 40, "found only {} files", sources.len());
    let files: Vec<FileMetrics> = sources
        .iter()
        .map(|(path, bytes)| analyzer.analyse(path, bytes))
        .collect();

    let refused: Vec<String> = files
        .iter()
        .filter(|file| {
            matches!(
                file.status,
                Status::ParseError | Status::UnsupportedLanguage | Status::Oversized
            )
        })
        .map(|file| {
            format!(
                "{}: {}",
                file.path,
                file.detail.as_deref().unwrap_or(file.status.as_str())
            )
        })
        .collect();
    assert!(
        refused.is_empty(),
        "this repository's own source must lex:\n{}",
        refused.join("\n")
    );

    let measured: Vec<&FileMetrics> = files
        .iter()
        .filter(|file| file.status == Status::Ok)
        .collect();
    assert!(
        measured.len() > 30,
        "only {} measurable files",
        measured.len()
    );
    for file in &measured {
        let mi = file.mi.expect("an ok file has an MI");
        assert!((0.0..=100.0).contains(&mi), "{}: mi {mi}", file.path);
        assert!(
            file.loc.unwrap_or(0) > 0,
            "{}: an ok file has LOC",
            file.path
        );
        assert!(
            file.halstead_volume.unwrap_or(0.0) >= 1.0,
            "{}: V",
            file.path
        );
    }

    let row = mi::aggregate(&definition, &files);
    let aggregate = row.aggregate.expect("a real repository has an MI");
    assert!((0.0..=100.0).contains(&aggregate));
    assert!(row.hotspots.windows(2).all(|pair| pair[0].mi <= pair[1].mi));
    assert_eq!(row.files, measured.len() as u64);
    assert_eq!(
        row.unanalysed_paths, 0,
        "nothing here should be unanalysable"
    );

    // Everything the corrected scoring unit is asserted on. Each of these was false, or was a
    // tautology, under per-file scoring.
    let clamped: Vec<&str> = measured
        .iter()
        .filter(|file| file.mi.unwrap_or(1.0) == 0.0)
        .map(|file| file.path.as_str())
        .collect();
    assert!(
        clamped.is_empty(),
        "{} files sit on the floor of the scale, which is what scoring a whole file as one unit did:\n{}",
        clamped.len(),
        clamped.join("\n")
    );
    let by_block = measured
        .iter()
        .filter(|file| file.unit == Some(fathom::mi::Unit::Block))
        .count();
    assert!(
        by_block * 10 >= measured.len() * 9,
        "only {by_block} of {} measurable files are scored by function blocks; the splitter is\n\
         failing to find definitions it should find",
        measured.len()
    );
    // Every block-scored file's MI must be exactly the mean of the blocks `blocks_of` reports, which
    // is the difference between a documented rule and a number nobody can re-derive.
    for file in measured
        .iter()
        .filter(|f| f.unit == Some(fathom::mi::Unit::Block))
    {
        let bytes = sources
            .iter()
            .find(|(path, _)| path == &file.path)
            .map(|(_, bytes)| bytes.as_slice())
            .expect("the corpus came from these sources");
        let blocks = analyzer
            .blocks_of(&file.path, bytes)
            .expect("a measurable file has blocks");
        // The same fold `score_units` does: a block with no MI is out of the mean *and* out of its
        // weight — it is a block that was not measured, not a block that measured zero.
        let mut weighted = 0_f64;
        let mut weight = 0_f64;
        for block in &blocks {
            let (Some(mi), Some(loc)) = (block.mi, block.loc) else {
                continue;
            };
            weighted += mi * definition.unit_weight(loc);
            weight += definition.unit_weight(loc);
        }
        assert_eq!(
            file.mi,
            Some(fathom::mi::round_storage(&definition, weighted / weight)),
            "{}: the file's MI is not the mean of its own blocks",
            file.path
        );
        for block in &blocks {
            assert!(
                block.mi.is_none() || (0.0..=100.0).contains(&block.mi.unwrap()),
                "{}::{} has MI {:?}",
                file.path,
                block.name,
                block.mi
            );
        }
    }

    // The one size relation worth keeping, stated as the weak thing it is: a tree of small, plain
    // functions must outscore a tree of big ones. Under §4.1 the relation is a tendency and not a
    // law — a 40-line file of one dense function is worse than a 400-line file of forty plain ones —
    // so it is printed rather than asserted, and the assertion below is the one that actually means
    // something: the unit must span the bands, or the bands describe nothing.
    let mean = |mi_values: &[f64]| {
        fathom::mi::round_storage(
            &definition,
            mi_values.iter().sum::<f64>() / mi_values.len() as f64,
        )
    };
    let smalls: Vec<f64> = measured
        .iter()
        .filter(|file| file.loc.unwrap_or(0) < 60)
        .map(|file| file.mi.unwrap_or(0.0))
        .collect();
    let larges: Vec<f64> = measured
        .iter()
        .filter(|file| file.loc.unwrap_or(0) >= 300)
        .map(|file| file.mi.unwrap_or(0.0))
        .collect();
    let (small, large) = (mean(&smalls), mean(&larges));
    assert!(
        (30.0..=90.0).contains(&aggregate),
        "a whole repository at {aggregate} reads as a verdict rather than a measurement: the scale\n\
         has drifted, check the unit rule of §4.1 before blaming the code"
    );

    // Every scoring unit in the tree, which is the population §5.3 publishes and §5.1's edges are
    // calibrated against.
    let mut units: Vec<f64> = Vec::new();
    for file in &files {
        let bytes = sources
            .iter()
            .find(|(path, _)| path == &file.path)
            .map(|(_, bytes)| bytes.as_slice())
            .unwrap_or_default();
        if let Some(blocks) = analyzer.blocks_of(&file.path, bytes) {
            units.extend(blocks.iter().filter_map(|block| block.mi));
        }
    }
    units.sort_by(|a, b| a.partial_cmp(b).expect("an MI is ordered"));
    let percentile = |q: f64| units[((units.len() as f64 - 1.0) * q).round() as usize];
    assert!(
        units.len() > 1000,
        "only {} scoring units across the corpus?",
        units.len()
    );
    let units_below = |edge: f64| units.iter().filter(|mi| **mi < edge).count();
    assert!(
        units_below(definition.bands.red_below) > 0,
        "no scoring unit in the corpus is red: the red band describes nothing, so §5.1 is stale"
    );
    assert!(
        units
            .iter()
            .filter(|mi| **mi >= definition.bands.amber_below)
            .count()
            > units.len() / 2,
        "most of the corpus is not green: the scale has moved under the edges"
    );

    // The histograms ADR-0007 §5.3 publishes, on stdout for whoever runs the suite with --nocapture.
    let tally = mi::band_tally(&files);
    let mut deciles: BTreeMap<usize, u64> = BTreeMap::new();
    for file in &measured {
        *deciles
            .entry(((file.mi.unwrap_or(0.0) / 10.0).floor() as usize).min(9))
            .or_insert(0) += 1;
    }
    println!(
        "fathom mi: {} files in scope, {} measured",
        files.len(),
        measured.len()
    );
    println!(
        "  aggregate {aggregate} ({}), loc {}",
        row.band.map(|band| band.as_str()).unwrap_or("none"),
        row.loc
    );
    println!(
        "  bands: green {} / amber {} / red {}",
        tally.get("green").copied().unwrap_or(0),
        tally.get("amber").copied().unwrap_or(0),
        tally.get("red").copied().unwrap_or(0)
    );
    println!(
        "  under 60 lines: mean {} over {} files; 300+ lines: mean {} over {} files",
        small,
        smalls.len(),
        large,
        larges.len()
    );
    println!(
        "  scoring units: {} blocks, MI p05 {} p25 {} p50 {} p75 {} p95 {}; bands green {} / amber {} / red {}",
        units.len(),
        percentile(0.05),
        percentile(0.25),
        percentile(0.50),
        percentile(0.75),
        percentile(0.95),
        units.len() - units.iter().filter(|mi| **mi < definition.bands.amber_below).count(),
        units_below(definition.bands.amber_below) - units_below(definition.bands.red_below),
        units_below(definition.bands.red_below)
    );
    println!(
        "  {} of {} measurable files are scored by blocks, the rest as a whole file",
        by_block,
        measured.len()
    );
    println!(
        "  clamped to 0.0: {} files",
        measured
            .iter()
            .filter(|file| file.mi.unwrap_or(1.0) == 0.0)
            .count()
    );
    let mut sorted: Vec<f64> = measured.iter().map(|file| file.mi.unwrap()).collect();
    sorted.sort_by(|a, b| a.partial_cmp(b).expect("an MI is ordered"));
    println!("  median MI {}", sorted[sorted.len() / 2]);
    for band in [mi::Band::Green, mi::Band::Amber, mi::Band::Red] {
        let lines: Vec<u64> = measured
            .iter()
            .filter(|file| file.band == Some(band))
            .map(|file| file.loc.unwrap_or(0))
            .collect();
        if let (Some(min), Some(max)) = (lines.iter().min(), lines.iter().max()) {
            println!(
                "  {}: {} files, logical LOC from {min} to {max}",
                band.as_str(),
                lines.len()
            );
        } else {
            println!("  {}: no files", band.as_str());
        }
    }
    let width = *deciles.values().max().unwrap_or(&1);
    for (bucket, count) in &deciles {
        println!(
            "  {:>3}-{:<3} {:>4} {}",
            bucket * 10,
            bucket * 10 + 9,
            count,
            "#".repeat((*count as usize * 40 / usize::try_from(width).unwrap_or(1)).max(1))
        );
    }
    println!("  worst hotspots (files, by MI):");
    for hotspot in &row.hotspots {
        println!(
            "    {:>6} loc {:>5}  {}",
            hotspot.mi, hotspot.loc, hotspot.path
        );
    }
    // The ten worst *scoring units*, which is what ADR-0007 §5.3 publishes as the table of findings:
    // a hotspot list names files, and the thing a reader can act on is a function.
    let mut worst: Vec<(f64, u64, String, String)> = Vec::new();
    for file in &measured {
        let bytes = sources
            .iter()
            .find(|(path, _)| path == &file.path)
            .map(|(_, bytes)| bytes.as_slice())
            .unwrap_or_default();
        if let Some(blocks) = analyzer.blocks_of(&file.path, bytes) {
            for block in blocks {
                if let (Some(mi), Some(loc)) = (block.mi, block.loc) {
                    worst.push((mi, loc, file.path.clone(), block.name));
                }
            }
        }
    }
    worst.sort_by(|a, b| a.0.partial_cmp(&b.0).expect("an MI is ordered"));
    println!("  densest scoring units:");
    for (mi, loc, path, name) in worst.into_iter().take(10) {
        println!("    {mi:>6} loc {loc:>5}  {path}::{name}");
    }
}

/// The scoring unit is a function, and a file's MI is the LOC-weighted mean of its blocks
/// (ADR-0007 §4.1). This is the shape that scored 0.0 — the bottom of the scale, a pile rather than a
/// tail — when the whole file was the unit: one plain accessor next to one dense function, which is
/// what most of a real module looks like.
#[test]
fn a_file_is_the_mean_of_its_function_blocks() {
    let analyzer = Analyzer::default();
    let definition = Definition::embedded();
    let source = concat!(
        "impl Gate {\n",
        "    pub fn name(&self) -> &str {\n",
        "        self.name.as_str()\n",
        "    }\n",
        "}\n",
        "\n",
        "fn classify(raw: &str, flags: u8) -> Kind {\n",
        "    let mut kind = Kind::Other;\n",
        "    if raw.is_empty() {\n",
        "        return Kind::Empty;\n",
        "    }\n",
        "    for line in raw.lines() {\n",
        "        if line.starts_with('#') {\n",
        "            continue;\n",
        "        } else if line.starts_with(\"//\") {\n",
        "            continue;\n",
        "        }\n",
        "        if flags & 1 != 0 && line.len() > 3 {\n",
        "            kind = Kind::Header;\n",
        "        } else if flags & 2 != 0 || line.is_ascii() {\n",
        "            kind = Kind::Body;\n",
        "        }\n",
        "        match kind {\n",
        "            Kind::Header => break,\n",
        "            Kind::Body => {}\n",
        "            Kind::Empty => return Kind::Other,\n",
        "            Kind::Other => {}\n",
        "        }\n",
        "    }\n",
        "    kind\n",
        "}\n",
    );
    let blocks = analyzer
        .blocks_of("src/two.rs", source.as_bytes())
        .expect("rust has a front end");
    let names: Vec<&str> = blocks.iter().map(|block| block.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["name", "classify"],
        "one block per function, in source order"
    );
    let metrics = analyzer.analyse("src/two.rs", source.as_bytes());
    assert_eq!(metrics.status, Status::Ok);
    assert_eq!(metrics.unit, Some(fathom::mi::Unit::Block));
    assert_eq!(metrics.blocks, Some(2));

    let mut weighted = 0_f64;
    let mut weight = 0_f64;
    for block in &blocks {
        let (mi, loc) = (
            block.mi.expect("a real function measures"),
            block.loc.unwrap(),
        );
        weighted += mi * definition.unit_weight(loc);
        weight += definition.unit_weight(loc);
    }
    let mean = fathom::mi::round_storage(&definition, weighted / weight);
    assert_eq!(
        metrics.mi,
        Some(mean),
        "the file's MI is its blocks' mean, to the stored digit"
    );

    // The old rule, run beside the new one on the same bytes: scoring the file as one unit was not a
    // little off, it was a different metric — the dense function's decision count and the file's size
    // both charged against the whole file.
    let (_, whole) = analyzer
        .measure_blob("src/two.rs", source.as_bytes())
        .expect("the same blob measures as a whole");
    assert!(
        whole.mi.unwrap_or(100.0) < mean,
        "whole-file {:?} vs per-function {mean}: scoring the file as one unit is a different and\
         lower metric, because it charges the dense function's decisions against the whole file's\n\
         length",
        whole.mi
    );

    // The file's own `loc`, `V` and `G` stay whole-file facts: they are what the reader inspects and
    // what §4.3 weights the mean by. Only `mi` changed provenance.
    assert_eq!(
        metrics.loc, whole.loc,
        "`loc` is still the file's logical LOC"
    );
    assert_eq!(metrics.halstead_volume, whole.volume);
    assert_eq!(metrics.cyclomatic, whole.cyclomatic);
}

/// A block that does not measure is excluded from its file's mean rather than scored 0 — §3.5's
/// never-borrow-a-number rule, one level down. A one-line `fn` with nothing in it is the case: its
/// vocabulary is one operator, so `ln(n)` has no answer.
#[test]
fn an_unmeasurable_block_is_out_of_the_mean_and_not_in_it_as_zero() {
    let analyzer = Analyzer::default();
    let source = "fn stub() {}\n\nfn real(a: u8) -> u8 {\n    a + 1\n}\n";
    let blocks = analyzer
        .blocks_of("src/stub.rs", source.as_bytes())
        .expect("blocks");
    assert_eq!(blocks.len(), 2);
    assert_eq!(
        blocks[0].status,
        Status::Vacuous,
        "`fn stub() {{}}` has no vocabulary to take a log of"
    );
    assert_eq!(blocks[0].mi, None, "and so no MI, and never 0.0");
    let metrics = analyzer.analyse("src/stub.rs", source.as_bytes());
    assert_eq!(metrics.blocks, Some(1), "one block fed the mean, not two");
    assert_eq!(
        metrics.mi, blocks[1].mi,
        "and the mean of one block is that block"
    );
}

/// A blob that declares no function is its own scoring unit, and says which rule produced its number
/// rather than pretending every number comes from the same place.
#[test]
fn a_blob_with_no_function_is_its_own_unit() {
    let analyzer = Analyzer::default();
    let types = "pub struct Point {\n    pub x: u32,\n    pub y: u32,\n}\n\npub enum Kind {\n    A,\n    B,\n    C,\n}\n";
    let metrics = analyzer.analyse("src/types.rs", types.as_bytes());
    assert_eq!(
        metrics.status,
        Status::Ok,
        "a module of types is still measurable"
    );
    assert_eq!(metrics.unit, Some(fathom::mi::Unit::File));
    assert_eq!(
        metrics.blocks, None,
        "no blocks fed a mean, so there is no count to report"
    );
    let (_, whole) = analyzer
        .measure_blob("src/types.rs", types.as_bytes())
        .expect("measures");
    assert_eq!(
        metrics.mi, whole.mi,
        "and the number is the whole-blob one, not a mean of nothing"
    );

    let script =
        "import sys\n\ntotal = 0\nfor arg in sys.argv[1:]:\n    total += len(arg)\nprint(total)\n";
    let metrics = analyzer.analyse("scripts/count.py", script.as_bytes());
    assert_eq!(
        metrics.unit,
        Some(fathom::mi::Unit::File),
        "a Python script of top-level statements has no function to score"
    );
    assert!(metrics.mi.is_some(), "and it is a measurement, not a gap");
}

/// The unit rule must not put `cargo fmt` on a trend line: reformatting a function moves its logical
/// lines not at all, and moves its block boundaries not at all, so the file's MI must not move.
#[test]
fn a_reformatted_file_scores_identically_under_the_unit_rule() {
    let analyzer = Analyzer::default();
    let tight =
        "fn f(a: u8) -> u8 {\n    if a > 1 { a } else { 0 }\n}\nfn g() -> bool {\n    true\n}\n";
    let loose = "fn f(a: u8) -> u8 {\n\n    if a > 1 {\n        a\n    } else {\n        0\n    }\n\n}\n\nfn g() -> bool {\n    // a comment costs nothing either\n    true\n}\n";
    let before = analyzer.analyse("src/f.rs", tight.as_bytes());
    let after = analyzer.analyse("src/f.rs", loose.as_bytes());
    assert_eq!(
        before.mi, after.mi,
        "the same two functions, wrapped differently: {:?} vs {:?}",
        before.mi, after.mi
    );
    assert_eq!(before.blocks, after.blocks);
}

/// The disclosure §4.1 has to carry: a file's MI is the mean of its *functions*, so code that is not
/// inside one is not weighed. A Python script that is mostly top-level assignments with one small `def`
/// is the shape this rule cannot see, and the honest answer is that the row says so — one block beside
/// sixty statements, which is a tell on the screen — rather than that the rule notices.
#[test]
fn module_level_code_is_not_weighed_and_the_row_says_so() {
    let analyzer = Analyzer::default();
    let definition = Definition::embedded();
    let mut script = String::new();
    for index in 0..60 {
        script.push_str(&format!("SETTING_{index} = {index}\n"));
    }
    script.push_str("def go() -> int:\n    return 1\n");
    let metrics = analyzer.analyse("scripts/mostly_data.py", script.as_bytes());
    assert_eq!(metrics.status, Status::Ok);
    assert_eq!(metrics.unit, Some(fathom::mi::Unit::Block));
    assert_eq!(metrics.blocks, Some(1), "one `def` in six dozen statements");
    let blocks = analyzer
        .blocks_of("scripts/mostly_data.py", script.as_bytes())
        .expect("blocks");
    let weighed = blocks[0].loc.expect("the `def` measured");
    assert!(
        metrics.loc.unwrap() > 3 * weighed,
        "the file is far bigger than the code that was weighed: {:?} logical lines vs {weighed}",
        metrics.loc
    );
    assert_eq!(
        metrics.mi, blocks[0].mi,
        "and the file's number *is* that one function's"
    );
    assert!(
        metrics.mi.unwrap_or(0.0) >= definition.bands.amber_below,
        "it reads green, which is the disclosure and not a surprise: {:?}",
        metrics.mi
    );
}
