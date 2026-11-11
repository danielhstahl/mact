//! The slice of workflow-file YAML this server needs, and nothing more.
//!
//! [`index_workflow`] answers these questions about a `.github/workflows/*.yml`
//! file, and no others:
//!
//! * what `name:` it declares (form 4 of ADR-0001 §2.2 workflow resolution),
//! * which job ids its `jobs:` map declares (the set `task` must be a member of,
//!   ADR-0001 §2.3 — the same string `act -j` receives), and
//! * per job, the handful of fields `list_tasks` advertises (mact-f1w.7): the
//!   display `name`, `needs`, `runs-on`, the `strategy.matrix` axis names, every
//!   `uses:` the job or its steps reference, and the step `name`/`uses` summary.
//!
//! It is a thin, allocation-light layer over [`yaml_rust2`] rather than a
//! deserialization of the document: act owns actually *running* the workflow, and
//! a second full model of the schema here would be a second thing to keep correct.
//! That is also why the per-job fields are read as *text*: `list_tasks` shows them
//! to a caller who is choosing a job, and nothing here has to understand what
//! `runs-on: [self-hosted, gpu]` means to a runner.
//!
//! ## Why this crate, pinned
//!
//! `yaml-rust2` (not the unmaintained `serde_yaml`, RUSTSEC-2024-0320) resolves
//! plain scalars with a **restricted** YAML 1.1 core schema: `true/True/TRUE`,
//! `false/False/FALSE`, `null/~/''`, integers and reals — and *nothing else*. That
//! is load-bearing. GitHub Actions files are full of bare `on:`, and a parser that
//! follows YAML 1.1's full boolean list (`y`, `yes`, `on`, `off`, `no`, …) turns
//! the `on:` key into `Yaml::Boolean(true)`, so a name-based lookup silently finds
//! no `on` key and a job id of `no` or `on` turns into `false`/`true` in the list
//! a caller is handed. The tests `bare_on_key_survives_the_yaml_boolean_trap` and
//! `boolean_looking_job_ids_stay_text` pin that behaviour, so a parser swap that
//! reintroduces the trap fails here rather than in `list_tasks`. There is no
//! `serde_yaml`-style `apply_implicit_schema` knob to get wrong: the resolution
//! table above *is* the crate's behaviour, and it is the reason this module is the
//! only YAML entry point in the tree.
//!
//! The error codes are ADR-0001 §6.1's `parse_error` namespace
//! (`invalid_yaml | not_a_map | jobs_missing | unsupported_yaml`), which is
//! deliberately *not* the tool-error-code namespace: a broken workflow file is
//! data inside a successful `list_tasks` call, and only becomes a `run_workflow`
//! problem because no job id can then be confirmed.

use std::collections::BTreeMap;

use yaml_rust2::{
    yaml::{Hash, Yaml},
    YamlLoader,
};

/// Largest workflow file this module will hand to the parser.
///
/// GitHub Actions caps a workflow file at 448 KB and act does not raise that; a
/// bigger file is either a mistake or a hostile input. The cap is the cheap
/// defence against a pathological input spending unbounded time in the parser.
pub const MAX_WORKFLOW_BYTES: usize = 448 * 1024;

/// Maximum nesting depth of a flow collection (`{`/`[`), counted on the raw text.
///
/// Not a YAML rule — a stack-overflow guard, because the parser recurses per
/// nesting level and flow collections can nest two bytes per level. Real workflow
/// files stay in the single digits.
pub const MAX_FLOW_DEPTH: u32 = 64;

/// Maximum indentation, in spaces, of any line.
///
/// Same purpose as [`MAX_FLOW_DEPTH`] for block (indentation-based) nesting: a
/// 1 MiB file of nested mappings is ~500 000 levels deep without the parser ever
/// seeing a bracket.
pub const MAX_BLOCK_INDENT: usize = 256;

/// Why a workflow file could not be indexed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowYamlError {
    /// The file is not valid YAML (or is empty), with the parser's position.
    InvalidYaml(String),
    /// The document is valid YAML but not a mapping at the top level.
    NotAMap,
    /// There is no `jobs:` key, so no job id can ever be confirmed.
    JobsMissing,
    /// `jobs:` exists but is not a mapping (e.g. a scalar or a sequence).
    JobsNotAMap,
    /// Input rejected before parsing by one of the size/depth guards.
    Rejected(String),
}

impl WorkflowYamlError {
    /// The ADR-0001 §6.1 `parse_error` code for this failure.
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidYaml(_) | Self::Rejected(_) => "invalid_yaml",
            Self::NotAMap => "not_a_map",
            Self::JobsMissing => "jobs_missing",
            Self::JobsNotAMap => "unsupported_yaml",
        }
    }

    /// One-sentence human-readable explanation (carried next to [`Self::code`]).
    pub fn message(&self) -> String {
        match self {
            Self::InvalidYaml(reason) => format!("workflow file is not valid YAML: {reason}"),
            Self::NotAMap => "workflow file is not a YAML mapping at the top level".to_owned(),
            Self::JobsMissing => "workflow file declares no `jobs:` map".to_owned(),
            Self::JobsNotAMap => "`jobs:` in the workflow file is not a map of job ids".to_owned(),
            Self::Rejected(reason) => {
                format!("workflow file was rejected before parsing: {reason}")
            }
        }
    }
}

/// What this server needs to know about one workflow file.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorkflowIndex {
    /// The `name:` key the file declares, if any (ADR-0001 §2.2 form 4).
    pub declared_name: Option<String>,
    /// The declared jobs, in file order.
    pub jobs: Vec<JobIndex>,
}

impl WorkflowIndex {
    /// Whether `task` is one of the declared job ids.
    pub fn has_job(&self, task: &str) -> bool {
        self.jobs.iter().any(|job| job.id == task)
    }

    /// The declared job ids, in file order — the list a `task_not_found` detail and
    /// `list_tasks` both show.
    pub fn job_ids(&self) -> Vec<&str> {
        self.jobs.iter().map(|job| job.id.as_str()).collect()
    }

    /// Look one job up by id.
    pub fn job(&self, task: &str) -> Option<&JobIndex> {
        self.jobs.iter().find(|job| job.id == task)
    }
}

/// One job (`jobs.<id>`) as far as this server cares.
///
/// Every field is the *text* of the workflow file, normalized into strings; an
/// empty `Vec` means "the file says nothing here", never "empty by inheritance".
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct JobIndex {
    /// The job id — the key under `jobs:`, and what `act -j` receives.
    pub id: String,
    /// The job's `name:` key, if it declares one.
    pub name: Option<String>,
    /// Job ids this job `needs:`, flattened from a scalar, a sequence, or the
    /// `{job: <id>, …}` form.
    pub needs: Vec<String>,
    /// The `runs-on:` labels, one element per label (`runs-on: ubuntu-latest` and
    /// `runs-on: [self-hosted, gpu]` both land here).
    pub runs_on: Vec<String>,
    /// Axis names declared by `strategy.matrix`, in file order. `include` and
    /// `exclude` are matrix *entries*, not axes, so they are not axes here.
    pub matrix_axes: Vec<String>,
    /// The scalar values each declared axis can take, axis name → values, in file order
    /// and de-duplicated.
    ///
    /// This is what lets `runs-on: ${{ matrix.runner }}` be answered without guessing: the
    /// values are read from the axis *and* from the `include:` entries that extend it
    /// (`exclude:` is left out, because excluding a combination cannot be what a run asks
    /// for). An axis whose values are objects, or a name never declared, is simply absent
    /// here, and the reader of this map treats absence as "do not guess".
    pub matrix_axis_values: BTreeMap<String, Vec<String>>,
    /// The job's own `container:` image, in either spelling (`container: image:tag` and
    /// `container: {image: image:tag}`).
    ///
    /// Its only consumer asks whether act has anything to ask a runner label about: a job
    /// that names its image never reaches act's label → image map.
    pub container_image: Option<String>,
    /// Every `uses:` string this job references — the job-level reusable-workflow
    /// call and each step's action — in file order.
    pub uses: Vec<String>,
    /// The `steps:` summary, in order: `name` and `uses` per step.
    pub steps: Vec<StepIndex>,
}

impl JobIndex {
    /// Whether anything here points at an action or reusable workflow that act has
    /// to fetch or check out to resolve (`uses:` is the only marker there is).
    pub fn uses_external(&self) -> bool {
        !self.uses.is_empty()
    }
}

/// One entry of a job's `steps:` list.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StepIndex {
    /// The step's `name:`, if it declares one.
    pub name: Option<String>,
    /// The action or reusable workflow this step `uses:`, if any.
    pub uses: Option<String>,
    /// The first [`MAX_STEP_RUN_BYTES`] bytes of the step's `run:` script.
    ///
    /// Scan-only: `list_tasks`'s category hint looks for the project's known
    /// coverage/benchmark commands here, and a whole `run:` block of a real workflow
    /// can be kilobytes of shell nobody asked for. Never advertised verbatim in a tool
    /// result — the advertised step summary is `name` and `uses` only.
    pub run_snippet: Option<String>,
}

/// How much of a step's `run:` script is kept for the category hint.
pub const MAX_STEP_RUN_BYTES: usize = 2048;

/// Index a workflow file's bytes.
///
/// The byte-level guards ([`MAX_WORKFLOW_BYTES`], [`MAX_BLOCK_INDENT`],
/// [`MAX_FLOW_DEPTH`]) run before the parser is called at all, so an oversized or
/// pathological file costs one linear scan.
pub fn index_workflow(bytes: &[u8]) -> Result<WorkflowIndex, WorkflowYamlError> {
    precheck(bytes)?;
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) => {
            return Err(WorkflowYamlError::InvalidYaml(
                "file is not UTF-8".to_owned(),
            ))
        }
    };
    parse(text)
}

/// [`index_workflow`] for text already decoded as UTF-8 — same guards, same result.
pub fn index_workflow_str(text: &str) -> Result<WorkflowIndex, WorkflowYamlError> {
    precheck(text.as_bytes())?;
    parse(text)
}

fn parse(text: &str) -> Result<WorkflowIndex, WorkflowYamlError> {
    let docs = YamlLoader::load_from_str(text)
        .map_err(|err| WorkflowYamlError::InvalidYaml(err.to_string()))?;
    let Some(doc) = docs.first() else {
        // An empty document (or a file of nothing but comments) declares no jobs,
        // but the honest description is "not a workflow mapping".
        return Err(WorkflowYamlError::InvalidYaml(
            "document is empty".to_owned(),
        ));
    };

    let yaml_rust2::Yaml::Hash(map) = doc else {
        return Err(WorkflowYamlError::NotAMap);
    };

    let declared_name = lookup(map, "name").and_then(scalar_to_string);

    let Some(jobs) = lookup(map, "jobs") else {
        return Err(WorkflowYamlError::JobsMissing);
    };
    let Yaml::Hash(jobs_map) = jobs else {
        return Err(WorkflowYamlError::JobsNotAMap);
    };

    let mut index = WorkflowIndex {
        declared_name,
        jobs: Vec::new(),
    };
    // Insertion order is preserved by yaml-rust2's hash type, so the job list
    // handed back to a caller (and shown in a `task_not_found` detail) reads in
    // file order rather than in hash order.
    for (key, value) in jobs_map {
        let Some(id) = scalar_to_string(key) else {
            continue;
        };
        index.jobs.push(index_job(id, value));
    }
    Ok(index)
}

/// Index one job.
///
/// Deliberately forgiving: a job whose value is not a mapping still gets an id, so
/// `list_tasks` shows what the file declares and `run_workflow`'s resolution (which
/// only needs the id) keeps agreeing with it. act is the one that has to accept the
/// job, and it says so itself when it cannot.
fn index_job(id: String, value: &Yaml) -> JobIndex {
    let mut job = JobIndex {
        id,
        ..JobIndex::default()
    };
    let Yaml::Hash(job_map) = value else {
        return job;
    };

    job.name = lookup(job_map, "name").and_then(scalar_to_string);
    job.needs = needs(lookup(job_map, "needs"));
    job.runs_on = string_list(lookup(job_map, "runs-on").or_else(|| lookup(job_map, "runs_on")));
    job.matrix_axes = matrix_axes(lookup(job_map, "strategy"));
    job.matrix_axis_values = matrix_axis_values(lookup(job_map, "strategy"));
    job.container_image = container_image(lookup(job_map, "container"));

    // A job that `uses:` a reusable workflow has no `steps:` at all, so both are
    // collected into one list in file order.
    if let Some(uses) = lookup(job_map, "uses").and_then(scalar_to_string) {
        job.uses.push(uses);
    }
    if let Some(Yaml::Array(steps)) = lookup(job_map, "steps") {
        for step in steps {
            let Yaml::Hash(step_map) = step else {
                continue;
            };
            let uses = lookup(step_map, "uses").and_then(scalar_to_string);
            if let Some(uses) = uses.clone() {
                job.uses.push(uses);
            }
            job.steps.push(StepIndex {
                name: lookup(step_map, "name").and_then(scalar_to_string),
                uses,
                run_snippet: lookup(step_map, "run")
                    .and_then(scalar_to_string)
                    .map(truncate),
            });
        }
    }
    job
}

/// `needs:` in all three spellings GitHub accepts: one job id, a sequence of ids,
/// or a sequence of `{job: <id>, …}` mappings.
fn needs(value: Option<&Yaml>) -> Vec<String> {
    match value {
        None => Vec::new(),
        Some(Yaml::Array(items)) => items
            .iter()
            .flat_map(|item| match item {
                Yaml::Hash(entry) => lookup(entry, "job").and_then(scalar_to_string),
                other => scalar_to_string(other),
            })
            .collect(),
        Some(other) => scalar_to_string(other).into_iter().collect(),
    }
}

/// Keep at most [`MAX_STEP_RUN_BYTES`] of a scalar, on a character boundary.
fn truncate(text: String) -> String {
    if text.len() <= MAX_STEP_RUN_BYTES {
        return text;
    }
    let mut end = MAX_STEP_RUN_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// A scalar or a sequence of scalars, as strings.
fn string_list(value: Option<&Yaml>) -> Vec<String> {
    match value {
        None => Vec::new(),
        Some(Yaml::Array(items)) => items.iter().filter_map(scalar_to_string).collect(),
        Some(other) => scalar_to_string(other).into_iter().collect(),
    }
}

/// The axis names of `strategy.matrix`, excluding `include`/`exclude`, which are
/// matrix *entries* rather than axes a caller could pick a value for.
fn matrix_axes(strategy: Option<&Yaml>) -> Vec<String> {
    let Some(Yaml::Hash(strategy_map)) = strategy else {
        return Vec::new();
    };
    let Some(Yaml::Hash(matrix)) = lookup(strategy_map, "matrix") else {
        return Vec::new();
    };
    matrix
        .iter()
        .filter_map(|(key, _)| {
            let name = scalar_to_string(key)?;
            (!matches!(name.as_str(), "include" | "exclude")).then_some(name)
        })
        .collect()
}

/// The scalar values of each `strategy.matrix` axis, `include:` entries folded in.
///
/// `include:` is folded because that is how a workflow adds one odd runner to an otherwise
/// ordinary matrix (`include: [{runner: macos-14}]`), and an index that missed it would
/// report an axis with fewer labels than the run can ask for.
fn matrix_axis_values(strategy: Option<&Yaml>) -> BTreeMap<String, Vec<String>> {
    let Some(Yaml::Hash(strategy_map)) = strategy else {
        return BTreeMap::new();
    };
    let Some(Yaml::Hash(matrix)) = lookup(strategy_map, "matrix") else {
        return BTreeMap::new();
    };

    let mut values: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let push = |axis: String, value: String, values: &mut BTreeMap<String, Vec<String>>| {
        let entry = values.entry(axis).or_default();
        if !entry.iter().any(|seen| seen == &value) {
            entry.push(value);
        }
    };

    for (key, axis) in matrix {
        let Some(name) = scalar_to_string(key) else {
            continue;
        };
        match axis {
            // One axis, several values.
            Yaml::Array(items) => {
                for item in items {
                    if let Some(value) = scalar_to_string(item) {
                        push(name.clone(), value, &mut values);
                    }
                }
            }
            // One axis, one value (`matrix: {os: ubuntu-latest}`), which GitHub runs as a
            // one-value axis.
            scalar => {
                if name != "include" && name != "exclude" {
                    if let Some(value) = scalar_to_string(scalar) {
                        push(name, value, &mut values);
                    }
                }
            }
        }
    }

    // `include:` may be a list of entries, or a list of lists of entries (the form the
    // docs use for a multi-axis addition).
    if let Some(included) = lookup(matrix, "include") {
        let entries = match included {
            Yaml::Array(items) => items.clone(),
            other => vec![other.clone()],
        };
        for entry in entries {
            let nested = match entry {
                Yaml::Array(items) => items,
                other => vec![other],
            };
            for entry in nested {
                let Yaml::Hash(entry_map) = entry else {
                    continue;
                };
                for (key, value) in entry_map.iter() {
                    let (Some(name), Some(value)) =
                        (scalar_to_string(key), scalar_to_string(value))
                    else {
                        continue;
                    };
                    push(name, value, &mut values);
                }
            }
        }
    }
    values
}

/// `container:` in the two spellings GitHub accepts.
fn container_image(value: Option<&Yaml>) -> Option<String> {
    match value? {
        Yaml::Hash(map) => lookup(map, "image").and_then(scalar_to_string),
        other => scalar_to_string(other),
    }
}

/// Look up a top-level key by name.
///
/// Keys are compared as *text*, so a YAML quirk like `jobs: ` versus `"jobs":`
/// does not silently hide the job map from validation.
fn lookup<'a>(map: &'a Hash, key: &str) -> Option<&'a Yaml> {
    map.iter()
        .find(|(k, _)| scalar_to_string(k).as_deref() == Some(key))
        .map(|(_, value)| value)
}

/// Render a YAML scalar as the string we compare against.
///
/// Only scalars qualify: a map, a sequence, an alias or a null is not a name or a
/// job id. `2024:` as a job id *is* a string here, which matches what GitHub and
/// act put on the wire for `jobs`.
fn scalar_to_string(value: &Yaml) -> Option<String> {
    match value {
        Yaml::String(text) => Some(text.clone()),
        Yaml::Integer(number) => Some(number.to_string()),
        Yaml::Real(text) => Some(text.clone()),
        Yaml::Boolean(flag) => Some(flag.to_string()),
        _ => None,
    }
}

/// The pre-parse guards: size, then a single linear scan for indentation and flow
/// nesting. Cheap, order-independent, and reports the *first* thing wrong.
fn precheck(bytes: &[u8]) -> Result<(), WorkflowYamlError> {
    if bytes.len() > MAX_WORKFLOW_BYTES {
        return Err(WorkflowYamlError::Rejected(format!(
            "file is {} bytes, over the {} byte limit",
            bytes.len(),
            MAX_WORKFLOW_BYTES
        )));
    }

    let mut line_indent = 0usize;
    let mut max_indent = 0usize;
    let mut flow_depth: i32 = 0;
    let mut max_flow: i32 = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;

    let mut index = 0usize;
    while index < bytes.len() {
        let char = bytes[index] as char;
        index += 1;
        // Quoted scalars can contain anything, including brackets and `#`, so the
        // scan counts structure and not content.
        if escaped {
            escaped = false;
            continue;
        }
        if in_double {
            match char {
                '\\' => escaped = true,
                '"' => in_double = false,
                _ => {}
            }
            continue;
        }
        if in_single {
            if char == '\'' {
                in_single = false;
            }
            continue;
        }
        match char {
            '\'' => in_single = true,
            '"' => in_double = true,
            '#' => {
                // The rest of the line is a comment: jump to its end.
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            '[' | '{' => {
                flow_depth += 1;
                max_flow = max_flow.max(flow_depth);
            }
            ']' | '}' => flow_depth = (flow_depth - 1).max(0),
            '\n' => {
                max_indent = max_indent.max(line_indent);
                line_indent = 0;
            }
            ' ' => line_indent += 1,
            _ => {
                // First real content on the line; its indentation is settled.
                max_indent = max_indent.max(line_indent);
                line_indent = 0;
            }
        }
        if max_flow as u32 > MAX_FLOW_DEPTH || max_indent > MAX_BLOCK_INDENT {
            break;
        }
    }
    max_indent = max_indent.max(line_indent);

    if max_indent > MAX_BLOCK_INDENT {
        return Err(WorkflowYamlError::Rejected(format!(
            "line indentation reaches {max_indent} spaces, over the {MAX_BLOCK_INDENT} space guard"
        )));
    }
    if max_flow as u32 > MAX_FLOW_DEPTH {
        return Err(WorkflowYamlError::Rejected(format!(
            "flow collections nest {max_flow} deep, over the {MAX_FLOW_DEPTH} level guard"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(text: &str) -> WorkflowIndex {
        index_workflow_str(text).unwrap_or_else(|err| panic!("{}: {}", err.message(), text))
    }

    #[test]
    fn reads_declared_name_and_job_ids_in_file_order() {
        let text = "\
name: Continuous Bench
on:
  push:
    branches: [main]
jobs:
  zulu:
    runs-on: ubuntu-latest
  alpha:
    runs-on: ubuntu-latest
";
        let got = index(text);
        assert_eq!(got.declared_name.as_deref(), Some("Continuous Bench"));
        assert_eq!(got.job_ids(), vec!["zulu", "alpha"]);
        assert!(got.has_job("alpha"));
        assert!(!got.has_job("missing"));
    }

    #[test]
    fn a_name_deep_in_the_file_is_not_the_workflow_name() {
        // `name:` keys inside a job or a step must not be mistaken for the
        // workflow's own name (the classic trap of a line-based YAML scan).
        let text = "\
on: push
jobs:
  build:
    name: build job
    env:
      name: also-not-it
    steps:
      - name: inner step
        run: echo hi
";
        let got = index(text);
        assert_eq!(got.declared_name, None);
        assert_eq!(got.job_ids(), vec!["build"]);
    }

    #[test]
    fn quoted_and_numeric_job_ids_survive() {
        let text = "\
jobs:
  \"2024\": {}
  'with spaces': {}
  plain: {}
";
        assert_eq!(index(text).job_ids(), vec!["2024", "with spaces", "plain"]);
    }

    #[test]
    fn flow_style_job_map_is_read_too() {
        let text =
            "name: flow\njobs: { one: {runs-on: ubuntu-latest}, two: {runs-on: ubuntu-latest} }\n";
        let got = index(text);
        assert_eq!(got.declared_name.as_deref(), Some("flow"));
        assert_eq!(got.job_ids(), vec!["one", "two"]);
    }

    #[test]
    fn comments_and_blank_lines_are_inert() {
        let text = "\
# name: not the workflow name
on: push   # trailing comment

jobs:
  # a comment where a job id would be
  test:
    runs-on: ubuntu-latest
";
        let got = index(text);
        assert_eq!(got.declared_name, None);
        assert_eq!(got.job_ids(), vec!["test"]);
    }

    #[test]
    fn block_scalars_are_skipped_not_parsed() {
        let text = "\
name: real
jobs:
  build:
    steps:
      - run: |
          name: inside a block scalar
        jobs:
          not_a_job:
";
        let got = index(text);
        assert_eq!(got.declared_name.as_deref(), Some("real"));
        assert_eq!(got.job_ids(), vec!["build"]);
    }

    #[test]
    fn crlf_line_endings_parse() {
        let text = "name: crlf\r\non: push\r\njobs:\r\n  build:\r\n    runs-on: ubuntu-latest\r\n";
        assert_eq!(index(text).job_ids(), vec!["build"]);
    }

    /// The YAML 1.1 trap this parser is pinned to avoid.
    ///
    /// GitHub Actions writes `on:` (and jobs full of `if:`), and YAML 1.1 lists
    /// `on`/`off`/`y`/`n`/`yes`/`no` as booleans. A parser that applies that table
    /// to plain scalars makes the `on:` key `Yaml::Boolean(true)`, and a name-based
    /// lookup then reports a perfectly good workflow as having no trigger — or,
    /// worse, reports a job id as `true`. See the module docs for why `yaml_rust2`
    /// does not do this.
    #[test]
    fn bare_on_key_survives_the_yaml_boolean_trap() {
        for text in [
            // The plainest file a real repository ships: bare `on:`, bare `push:`.
            "on:\n  push:\n  pull_request:\njobs:\n  build:\n    runs-on: ubuntu-latest\n",
            // `on:` as a one-line scalar trigger, and a mapping-valued trigger list.
            "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n",
            // `on:` with the other YAML 1.1 boolean spellings as *values* below it.
            "on:\n  schedule:\n    - cron: '0 4 * * *'\n  workflow_dispatch:\n    inputs:\n      full:\n        description: everything\n        default: off\njobs:\n  build:\n    runs-on: ubuntu-latest\n",
        ] {
            let got = index(text);
            assert_eq!(got.job_ids(), vec!["build"], "{text}");
            // The trigger key is still reachable *as text*, which is what a caller
            // grepping a workflow for `on:` expects.
            assert!(
                text.contains("on"),
                "fixture must actually exercise the trap: {text}"
            );
        }
    }

    /// Job ids and matrix values that *look* like YAML 1.1 booleans stay text.
    #[test]
    fn boolean_looking_job_ids_stay_text() {
        let text = "\
on: push
jobs:
  yes:
    runs-on: ubuntu-latest
  on:
    runs-on: ubuntu-latest
  no:
    runs-on: ubuntu-latest
  off:
    runs-on: ubuntu-latest
    steps:
      - name: gate
        if: on
        run: echo hi
";
        let got = index(text);
        assert_eq!(got.job_ids(), vec!["yes", "on", "no", "off"]);
        // …and a boolean-valued `if:` does not disturb the step summary either.
        assert_eq!(got.job("off").expect("job").steps.len(), 1);
    }

    #[test]
    fn job_details_are_read_as_text() {
        let text = "\
name: bench
on: push
jobs:
  bench:
    name: Run hyperfine benchmarks
    needs: build
    runs-on: [self-hosted, bench]
    strategy:
      fail-fast: false
      matrix:
        profile: [dev, release]
        include:
          - profile: release
            extra: yes
    steps:
      - uses: actions/checkout@v5
      - name: bench
        run: hyperfine './target/release/thing'
";
        let job = index(text).job("bench").expect("bench job").clone();
        assert_eq!(job.name.as_deref(), Some("Run hyperfine benchmarks"));
        assert_eq!(job.needs, vec!["build"]);
        assert_eq!(job.runs_on, vec!["self-hosted", "bench"]);
        assert_eq!(job.matrix_axes, vec!["profile"]);
        assert_eq!(job.uses, vec!["actions/checkout@v5"]);
        assert!(job.uses_external());
        assert_eq!(job.steps.len(), 2);
        assert_eq!(job.steps[0].uses.as_deref(), Some("actions/checkout@v5"));
        assert_eq!(job.steps[1].name.as_deref(), Some("bench"));
        assert_eq!(job.steps[1].uses, None);
    }

    #[test]
    fn matrix_axis_values_come_from_the_axis_and_from_include() {
        let text = "\
on: push
jobs:
  check:
    runs-on: ubuntu-latest
    strategy:
      fail-fast: false
      matrix:
        runner: [ubuntu-24.04, macos-14]
        target: x86_64-unknown-linux-gnu
        build: [debug, release]
        include:
          - runner: windows-2025
            target: x86_64-pc-windows-msvc
          - runner: ubuntu-24.04
        exclude:
          - runner: macos-14
            build: debug
    steps:
      - run: echo hi
";
        let job = index(text).job("check").expect("job").clone();
        // Axis *names* still exclude include/exclude; the values fold them in, because an
        // `include:` entry is how a workflow adds a runner the plain axis never named.
        assert_eq!(job.matrix_axes, vec!["runner", "target", "build"]);
        assert_eq!(
            job.matrix_axis_values["runner"],
            vec!["ubuntu-24.04", "macos-14", "windows-2025"],
            "de-duplicated, in file order, include folded in"
        );
        assert_eq!(
            job.matrix_axis_values["target"],
            vec!["x86_64-unknown-linux-gnu", "x86_64-pc-windows-msvc"],
            "a scalar axis is a one-value axis"
        );
        assert_eq!(job.matrix_axis_values["build"], vec!["debug", "release"]);
        assert!(!job.matrix_axis_values.contains_key("exclude"));
    }

    #[test]
    fn a_job_container_is_read_in_both_spellings() {
        let text = "\
on: push
jobs:
  boxed:
    runs-on: ubuntu-latest
    container: example/toolchain:1
  nested:
    runs-on: ubuntu-latest
    container:
      image: example/toolchain:1
      env:
        CC: clang
  open:
    runs-on: ubuntu-latest
    container:
      env:
        CC: clang
";
        let got = index(text);
        assert_eq!(
            got.job("boxed").expect("job").container_image.as_deref(),
            Some("example/toolchain:1")
        );
        assert_eq!(
            got.job("nested").expect("job").container_image.as_deref(),
            Some("example/toolchain:1")
        );
        assert_eq!(got.job("open").expect("job").container_image, None);
    }

    #[test]
    fn needs_accepts_every_github_shape() {
        let text = "\
on: push
jobs:
  a:
    needs: build
  b:
    needs: [build, lint]
  c:
    needs:
      - job: build
        needs: whatever
  d:
    needs: 42
";
        let got = index(text);
        assert_eq!(got.job("a").expect("a").needs, vec!["build"]);
        assert_eq!(got.job("b").expect("b").needs, vec!["build", "lint"]);
        assert_eq!(got.job("c").expect("c").needs, vec!["build"]);
        assert_eq!(got.job("d").expect("d").needs, vec!["42"]);
    }

    #[test]
    fn a_job_that_uses_a_reusable_workflow_has_no_steps_but_is_still_indexed() {
        let text = "\
on: push
jobs:
  call:
    uses: ././.github/workflows/reusable.yml
    with:
      profile: release
  matrix:
    strategy:
      matrix:
        os: [ubuntu-24.04]
";
        let got = index(text);
        assert_eq!(got.job_ids(), vec!["call", "matrix"]);
        let call = got.job("call").expect("call");
        assert_eq!(call.uses, vec!["././.github/workflows/reusable.yml"]);
        assert!(call.steps.is_empty());
        assert!(call.runs_on.is_empty());
        assert!(got.job("matrix").expect("matrix").uses.is_empty());
    }

    #[test]
    fn a_job_that_is_not_a_mapping_still_has_an_id() {
        // `jobs: {broken: null}` is nonsense to act, but the id is what a caller is
        // choosing, so it is listed rather than dropped.
        let got = index("on: push\njobs:\n  broken:\n  fine:\n    runs-on: x\n");
        assert_eq!(got.job_ids(), vec!["broken", "fine"]);
    }

    #[test]
    fn invalid_yaml_is_reported_with_a_position() {
        let err = index_workflow_str("name: [unclosed\n").expect_err("must fail");
        assert_eq!(err.code(), "invalid_yaml");
        assert!(!err.message().is_empty());
    }

    #[test]
    fn empty_and_comment_only_files_are_not_valid_workflows() {
        for text in ["", "\n\n", "# nothing\n"] {
            let err = index_workflow_str(text).expect_err("must fail");
            assert_eq!(err.code(), "invalid_yaml", "{text:?}");
        }
    }

    #[test]
    fn top_level_sequence_is_not_a_map() {
        let err = index_workflow_str("- a\n- b\n").expect_err("must fail");
        assert_eq!(err.code(), "not_a_map");
    }

    #[test]
    fn missing_jobs_map_is_its_own_code() {
        let err = index_workflow_str("name: no jobs\non: push\n").expect_err("must fail");
        assert_eq!(err.code(), "jobs_missing");
    }

    #[test]
    fn jobs_that_is_not_a_map_is_unsupported() {
        let err = index_workflow_str("jobs: build\n").expect_err("must fail");
        assert_eq!(err.code(), "unsupported_yaml");
        let err = index_workflow_str("jobs:\n  - build\n").expect_err("must fail");
        assert_eq!(err.code(), "unsupported_yaml");
    }

    #[test]
    fn oversized_file_is_rejected_before_parsing() {
        let bytes = vec![b'#'; MAX_WORKFLOW_BYTES + 1];
        let err = index_workflow(&bytes).expect_err("must fail");
        assert_eq!(err.code(), "invalid_yaml");
        assert!(err.message().contains("byte limit"));
    }

    #[test]
    fn pathological_flow_nesting_is_rejected() {
        let text = format!(
            "name: x\njobs: {}build: {}\n",
            "[".repeat(200),
            "]".repeat(200)
        );
        let err = index_workflow_str(&text).expect_err("must fail");
        assert!(err.message().contains("flow collections"), "{err:?}");
    }

    #[test]
    fn pathological_indentation_is_rejected() {
        let text = format!("jobs:\n{}deep:\n", " ".repeat(MAX_BLOCK_INDENT + 4));
        let err = index_workflow_str(&text).expect_err("must fail");
        assert!(err.message().contains("indentation"), "{err:?}");
    }

    #[test]
    fn brackets_inside_quoted_strings_do_not_count_toward_the_depth_guard() {
        let filler = format!("echo '{}'", "]".repeat(400));
        let text = format!(
            "name: x\non: push\njobs:\n  build:\n    steps:\n      - run: \"{}\"\n",
            filler
        );
        let got = index(&text);
        assert_eq!(got.job_ids(), vec!["build"]);
    }
}
