//! The slice of workflow-file YAML that validation needs.
//!
//! [`index_workflow`] answers exactly two questions about a `.github/workflows/*.yml`
//! file, and nothing else:
//!
//! * what `name:` it declares (form 4 of ADR-0001 §2.2 workflow resolution), and
//! * which job ids its `jobs:` map declares (the set `task` must be a member of,
//!   ADR-0001 §2.3 — the same string `act -j` receives).
//!
//! It is a thin, allocation-light layer over [`yaml_rust2`] rather than a
//! deserialization of the document: act owns actually *running* the workflow, and
//! a second full model of the schema here would be a second thing to keep correct.
//! The same crate is the base `list_tasks` (mact-f1w.7) builds its richer job index
//! on, so there is one YAML parser in the tree.
//!
//! The error codes are ADR-0001 §6.1's `parse_error` namespace
//! (`invalid_yaml | not_a_map | jobs_missing | unsupported_yaml`), which is
//! deliberately *not* the tool-error-code namespace: a broken workflow file is
//! data inside a successful `list_tasks` call, and only becomes a `run_workflow`
//! problem because no job id can then be confirmed.

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

/// What validation needs to know about one workflow file.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorkflowIndex {
    /// The `name:` key the file declares, if any (ADR-0001 §2.2 form 4).
    pub declared_name: Option<String>,
    /// Job ids in declaration order, exactly as `jobs:` lists them.
    pub jobs: Vec<String>,
}

impl WorkflowIndex {
    /// Whether `task` is one of the declared job ids.
    pub fn has_job(&self, task: &str) -> bool {
        self.jobs.iter().any(|job| job == task)
    }
}

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
    for (key, _value) in jobs_map {
        if let Some(job) = scalar_to_string(key) {
            index.jobs.push(job);
        }
    }
    Ok(index)
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
        assert_eq!(got.jobs, vec!["zulu", "alpha"]);
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
        assert_eq!(got.jobs, vec!["build"]);
    }

    #[test]
    fn quoted_and_numeric_job_ids_survive() {
        let text = "\
jobs:
  \"2024\": {}
  'with spaces': {}
  plain: {}
";
        assert_eq!(index(text).jobs, vec!["2024", "with spaces", "plain"]);
    }

    #[test]
    fn flow_style_job_map_is_read_too() {
        let text =
            "name: flow\njobs: { one: {runs-on: ubuntu-latest}, two: {runs-on: ubuntu-latest} }\n";
        let got = index(text);
        assert_eq!(got.declared_name.as_deref(), Some("flow"));
        assert_eq!(got.jobs, vec!["one", "two"]);
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
        assert_eq!(got.jobs, vec!["test"]);
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
        assert_eq!(got.jobs, vec!["build"]);
    }

    #[test]
    fn crlf_line_endings_parse() {
        let text = "name: crlf\r\non: push\r\njobs:\r\n  build:\r\n    runs-on: ubuntu-latest\r\n";
        assert_eq!(index(text).jobs, vec!["build"]);
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
        assert_eq!(got.jobs, vec!["build"]);
    }
}
