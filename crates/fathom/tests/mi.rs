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
        ("b.rs", 20, 24.9),
        ("c.rs", 30, 25.0),
        ("d.rs", 40, 49.9),
        ("e.rs", 50, 50.0),
        ("f.rs", 60, 99.0),
    ];
    let files: Vec<FileMetrics> = rows
        .iter()
        .map(|(path, loc, mi)| {
            FileMetrics::from_stored(path, Status::Ok, Some(*loc), Some(*mi), &definition)
        })
        .collect();
    let row = mi::aggregate(&definition, &files);

    // The watchlist is exactly the set below the red edge, and amber is the set below the amber edge:
    // edges belong to the better band, so 25.0 is amber and 50.0 is green.
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
        ]
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
/// the tree moves, and which prints the histogram ADR-0007 §5's `revisit_when` asks the first
/// corpus run to publish. Run with `cargo test -- --nocapture` to read it.
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
    assert!(
        measured.iter().any(|file| file.mi.unwrap_or(0.0) >= 60.0),
        "nothing in the tree reaches green, which would mean the scale is broken, not the code"
    );

    // The one shape assertion that survives the granularity caveat: MI falls as a file grows, so the
    // small files in a real tree must outscore the big ones. If they do not, the lexer is counting
    // something it should not.
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
        small > large,
        "files under 60 lines mean {}, files of 300+ mean {}",
        small,
        large
    );

    // The histogram ADR-0007 §5 asks the first corpus run to publish, on stdout for whoever runs the
    // suite with --nocapture. It is also the evidence behind the `revisit_when` note this work adds.
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
    println!("  worst hotspots:");
    for hotspot in &row.hotspots {
        println!(
            "    {:>6} loc {:>5}  {}",
            hotspot.mi, hotspot.loc, hotspot.path
        );
    }
}
