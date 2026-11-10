//! `list_tasks` — "what may I run here, and which job is the interesting one?"
//!
//! The discovery half of ADR-0001 §6.1. Its whole purpose is that a caller — an
//! agent, usually — never guesses a `workflow`/`task` pair: it lists, picks a leaf
//! job, and passes what it saw to `run_workflow`. So the output is judged by one
//! question: *can the caller act on it without another round trip*.
//!
//! ## Contract (ADR-0001 §6.1)
//!
//! * `list_tasks({workflow?, include_steps?})` → `list_tasks_result`:
//!   `workflows_dir`, `workflow_count`, `workflows[]` (`name`, `file`, `display?`,
//!   `job_count`, `jobs[]`, `parse_error?`) and `warnings[]`. Each job carries `id`,
//!   `name?`, `needs[]`, `runs_on[]`, `matrix_axes[]`, `uses_composite?` and
//!   `category_hint`.
//! * Read-only, **takes no workspace run lock** (see the section below), spawns no
//!   container, and needs no docker and no `act`. That makes it the cheapest
//!   end-to-end smoke call the server has, which is exactly why its failure modes are
//!   spelled out here rather than left to the reader.
//! * Its error codes are only `invalid_arguments`, `unknown_argument`,
//!   `workspace_not_found`, `workflows_dir_missing` and — when `workflow` names a
//!   file — `workflow_not_found` / `ambiguous_workflow`. Not `not_a_git_repository`:
//!   discovery reads files, it resolves no commit, so a git-less checkout is still a
//!   listable repository. Not `act_missing` / `docker_unreachable` either: those are
//!   run preconditions, and refusing to say what jobs exist because a container
//!   daemon is down would be the worst possible failure mode for the tool whose job
//!   is to say what jobs exist.
//! * **One malformed file never fails the call.** It is returned with
//!   `parse_error: {code, message}`, `code` taken verbatim from
//!   [`crate::workflow_yaml::WorkflowYamlError::code`] — the four members the contract
//!   pins, and this tool adds no fifth — and `warnings` gets `parse_error`. A caller
//!   that ignores `warnings` still sees the file, so nothing is silently omitted.
//!
//! ## No workspace lock, and why that is safe
//!
//! ADR-0003/ADR-0004 give a workspace a single-flight lock for act runs. `list_tasks`
//! neither takes it nor waits for it: it opens files under `.github/workflows`
//! read-only, runs no subprocess (unless the operator turns on the opt-in act
//! cross-check below, which is itself read-only), and writes nothing — not a log, not
//! an evidence file, not a temp file. Two consequences worth stating, because they are
//! the reasons the ticket asks for it: a supervisor can ask "what is in this repo"
//! while a 40-minute benchmark is mid-flight, and a caller can use `list_tasks` as a
//! liveness probe without serializing behind a run. It runs on the blocking pool
//! (`tokio::task::spawn_blocking`) rather than on an I/O worker, because a cold NFS
//! `readdir` is exactly the kind of slow read an async handler must not do inline.
//!
//! ## `category_hint` is advice
//!
//! [`category_hint`] is the single, documented heuristic that guesses which category a
//! caller probably wants (`benchmark | coverage | other`). It is advisory: the hint
//! never changes what a run does, and `run_workflow` still requires `category`
//! explicitly. The tool description says so, because a hint an agent treats as an
//! answer is worse than no hint.
//!
//! ## The opt-in act cross-check
//!
//! `act`'s loader and this module's YAML reader can disagree: a file that parses
//! happily here may be one act refuses to load, and a caller handed those job ids
//! would only learn at `run_workflow` time. When the operator passes
//! `--list-tasks-act-cross-check`, [`cross_check_with_act`] asks act itself
//! (`act list`) and adds [`WARNING_UNRESOLVABLE_WORKFLOW`] when act lists no job for a
//! file this server indexed. Which files is a log line, not a payload field: `warnings`
//! is a set of codes with nowhere to put act's own text. Nothing else about the entry
//! changes — act's opinion is a caveat about the workspace, not a reclassification of
//! the file, and `parse_error.code` is a closed enum this tool may not widen on its own
//! (ADR-0001 §10 makes that a coordinated spec change). The cross-check is off by
//! default, and not an accident:
//!
//! * act has no machine-readable listing. `act list` honours neither `--json` nor
//!   `--list-options` (its `--json` flag selects the *run* log format), so the answer
//!   is the human table, whose columns are tabwriter-padded. That parse is version
//!   fragile by nature, so it must never be load-bearing for a call that is supposed
//!   to be the cheapest thing on the server.
//! * it spawns a subprocess per call, which is the one thing about `list_tasks` that
//!   is not free.
//!
//! Whatever happens there — act missing, non-zero exit, timeout, unparseable table,
//! rows that match no file of ours — the cross-check degrades to the
//! [`WARNING_ACT_VIEW_UNAVAILABLE`] warning and the YAML-derived answer stands.

use std::{sync::Arc, time::Duration};

use rmcp::model::JsonObject;
use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::{
    config::Config,
    tools::{ArgumentProblem, ToolError},
    workflow_yaml::JobIndex,
    workflows::{self, WorkflowFile},
    workspace::{display_path, ValidationError, WORKFLOWS_DIR},
};

/// The tool's name, as advertised and as it appears in the `error` text prefix.
pub const TOOL: &str = "list_tasks";

/// `tools/list` title.
pub const TITLE: &str = "List workflows and job ids in this workspace";

/// The word the description uses to mark the hint as advice, and the word a test
/// asserts is present: an agent that skims for capitals finds this one.
pub const CATEGORY_HINT_ADVISORY_WORD: &str = "ADVISORY";

/// The advertised description. Part of the contract (ADR-0001 §1: a description that
/// states its error codes and what is advisory), so it is one readable sentence per
/// concern rather than a blob.
pub const DESCRIPTION: &str = "Enumerate this workspace's GitHub Actions workflows and the job ids (tasks) inside each: file path, display name, and per job its id, name, `needs`, `runs-on` labels, `strategy.matrix` axis names, and whether it references an action or reusable workflow (`uses_composite`). Optional `workflow` returns just that workflow's jobs, resolved by the same rules `run_workflow` uses; `include_steps` adds each job's step `name`/`uses` summary. Each job also carries a `category_hint` (benchmark | coverage | other) which is ADVISORY only - a documented guess from job names and known coverage/benchmark commands, never authoritative: you still pass `category` explicitly to `run_workflow`. Read-only and lock-free: it takes no workspace run lock, starts no container and needs no docker or act, so it is safe to call while a benchmark is mid-flight and is the cheapest smoke call this server makes. A workflow file that will not parse is returned inside this successful result with `parse_error`, and `warnings` says `parse_error`; one broken file never fails the call. Errors: `invalid_arguments`, `unknown_argument`, `workspace_not_found`, `workflows_dir_missing`, and (with `workflow`) `workflow_not_found` or `ambiguous_workflow`.";

/// ADR-0001 §9: `list_tasks` workflows / jobs / steps caps.
pub const MAX_WORKFLOWS: usize = 1024;
/// ADR-0001 §9: jobs per workflow.
pub const MAX_JOBS: usize = 1024;
/// ADR-0001 §9: steps per job, when `include_steps` is on.
pub const MAX_STEPS: usize = 256;
/// `$defs/list_tasks_result` item caps on the per-job lists.
const MAX_NEEDS: usize = 64;
/// `runs_on` items.
const MAX_RUNS_ON: usize = 16;
/// `matrix_axes` items.
const MAX_MATRIX_AXES: usize = 16;
/// `warnings` items (ADR-0001 §9: 64, unique).
const MAX_WARNINGS: usize = 64;
/// `display` / step-name length bound, so one absurd `name:` cannot bloat the result.
const MAX_NAME_BYTES: usize = 255;

/// Warning: at least one workflow file failed to parse (listed with `parse_error`).
pub const WARNING_PARSE_ERROR: &str = "parse_error";
/// Warning: the cross-check asked act, and act refused to load at least one file this
/// server indexed — so those job ids are likely unusable with `-j`. A caveat only: no
/// workflow entry is relabelled, because `parse_error.code` is a closed enum
/// (`workflow_not_found` in `docs/tool-contract.md` is where act's own refusal at run
/// time is specified).
pub const WARNING_UNRESOLVABLE_WORKFLOW: &str = "unresolvable_workflow";
/// Warning: the act cross-check was requested but act did not give a usable answer.
pub const WARNING_ACT_VIEW_UNAVAILABLE: &str = "act_view_unavailable";
/// Warning: a cap was hit and whole elements were dropped (ADR-0002 §5.1 vocabulary).
pub const WARNING_TRUNCATED: &str = "truncated";
/// Warning: a file was left out because its name is not expressible as a `workflow`
/// argument (e.g. `.disabled.yml` — GitHub ignores those too).
pub const WARNING_SKIPPED_WORKFLOW: &str = "skipped_workflow";

/// How long `act list` may take during the opt-in cross-check.
pub const ACT_LIST_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// arguments
// ---------------------------------------------------------------------------

/// Validated `list_tasks` arguments.
///
/// Built by [`ListTasksArgs::parse`], never by serde: see the module note in
/// [`crate::tools`] about why argument shape is checked by hand (an `isError: true`
/// tool result instead of JSON-RPC `-32602`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListTasksArgs {
    /// Return just this workflow's jobs (any of ADR-0001 §2.2's four forms).
    pub workflow: Option<String>,
    /// Also return each job's step summary (`name`, `uses`).
    pub include_steps: bool,
}

/// The argument names this tool defines, in schema order.
pub const ARGUMENT_NAMES: [&str; 2] = ["workflow", "include_steps"];

impl ListTasksArgs {
    /// Validate `params.arguments` as far as the contract requires.
    ///
    /// Unknown keys are rejected first (they mean the caller is talking to the wrong
    /// tool or has a typo in a required-looking name), then every known key is checked
    /// so all shape problems come back in one call rather than one per round trip.
    pub fn parse(arguments: Option<&JsonObject>) -> Result<Self, ToolError> {
        crate::tools::reject_unknown_arguments(TOOL, arguments, &ARGUMENT_NAMES)?;
        let empty = JsonObject::new();
        let arguments = arguments.unwrap_or(&empty);

        let mut problems = Vec::new();
        let mut workflow = None;
        let mut include_steps = false;

        match arguments.get("workflow") {
            None | Some(Value::Null) => {}
            Some(Value::String(text)) => match WorkflowNameShape::check(text) {
                Ok(()) => workflow = Some(text.to_owned()),
                Err(reason) => problems.push(ArgumentProblem {
                    path: "workflow",
                    reason,
                    expected: "a workflow path, file name, basename or declared `name:` (no `..`, \
                              no leading `/` or `-`, at most 255 bytes)",
                }),
            },
            Some(other) => problems.push(ArgumentProblem {
                path: "workflow",
                reason: format!("expected a string, got {}", kind_of(other)),
                expected: "a workflow path, file name, basename or declared `name:`",
            }),
        }

        match arguments.get("include_steps") {
            None | Some(Value::Null) => {}
            Some(Value::Bool(flag)) => include_steps = *flag,
            Some(other) => problems.push(ArgumentProblem {
                path: "include_steps",
                reason: format!("expected a boolean, got {}", kind_of(other)),
                expected: "true or false",
            }),
        }

        if !problems.is_empty() {
            return Err(ToolError::invalid_arguments(TOOL, problems));
        }
        Ok(Self {
            workflow,
            include_steps,
        })
    }
}

/// The advertised `inputSchema`, mirroring `$defs/list_tasks_args`.
///
/// Hand-written rather than derived because the pinned `$defs/workflow_name` is a
/// `not` + `anyOf` of two patterns (path form or bare name), which no derive of a
/// `String` field expresses — and because `WorkflowNameShape::check` is the runtime
/// half of the same rule, so keeping both spellings in one file is the point.
/// `tests/tool_schema_parity.rs` compares this against the checked-in schema.
pub fn input_schema() -> Arc<JsonObject> {
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "required": [],
        "properties": {
            "workflow": workflow_name_schema(),
            "include_steps": {
                "description": "Also return each job's step summary (name, uses). Off by default: a big \
                                workflow's step list is the bulk of the payload.",
                "type": "boolean",
                "default": false
            }
        }
    });
    let mut object = schema
        .as_object()
        .expect("literal schema is an object")
        .clone();
    // The pinned $defs entry carries a description of its own; the tool's description
    // already says what the arguments are, and rmcp strips root descriptions anyway.
    object.remove("description");
    Arc::new(object)
}

/// The pinned `$defs/workflow_name`, spelled once and used by the schema and — as
/// [`WorkflowNameShape::check`] — by the runtime check.
fn workflow_name_schema() -> Value {
    json!({
        "type": "string",
        "minLength": 1,
        "maxLength": 255,
        "not": { "pattern": "\\.\\." },
        "anyOf": [
            { "pattern": r"^[A-Za-z0-9][A-Za-z0-9 ._/-]*$" },
            { "pattern": r"^\.github/workflows/[A-Za-z0-9][A-Za-z0-9 ._/-]*$" }
        ]
    })
}

/// The runtime half of `$defs/workflow_name`.
///
/// ADR-0001 §2.2 chose that character class plus "no two consecutive dots" so that
/// `..`, an absolute path and a leading `-` (the argv-injection opening move) are all
/// inexpressible. Checked here rather than relied upon from resolution, so a bad name
/// is `invalid_arguments` before any path is joined.
struct WorkflowNameShape;

impl WorkflowNameShape {
    /// `Ok(())` or a sentence naming what is wrong.
    fn check(text: &str) -> Result<(), String> {
        if text.is_empty() {
            return Err("the workflow name is empty".to_owned());
        }
        if text.len() > 255 {
            return Err(format!(
                "the workflow name is {} bytes, over 255",
                text.len()
            ));
        }
        if text.contains("..") {
            return Err("a workflow name may not contain two consecutive dots".to_owned());
        }
        let bare = format!("{WORKFLOWS_DIR}/");
        let remainder = text.strip_prefix(bare.as_str()).unwrap_or(text);
        if !remainder.starts_with(|first: char| first.is_ascii_alphanumeric()) {
            return Err(format!(
                "`{text}` must start with a letter or digit (after the optional `.github/workflows/` prefix)"
            ));
        }
        if !remainder
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || " ._/-".contains(character))
        {
            return Err(format!(
                "`{text}` uses a character outside letters, digits and ` ._/-`"
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// result shape
// ---------------------------------------------------------------------------

/// `list_tasks_result` — the whole `structuredContent` of a successful call.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListTasksResult {
    /// `.github/workflows`, workspace-relative (never a host path).
    #[schemars(schema_with = "relpath_schema")]
    pub workflows_dir: String,
    /// How many workflow entries `workflows` holds, malformed files included.
    #[schemars(range(min = 0, max = 1024))]
    pub workflow_count: u32,
    /// One entry per workflow file, sorted by `file`.
    #[schemars(length(max = 1024))]
    pub workflows: Vec<WorkflowEntry>,
    /// Machine-readable caveats, unique, capped at 64. Open vocabulary.
    #[schemars(schema_with = "warnings_schema")]
    pub warnings: Vec<String>,
}

/// One workflow file.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkflowEntry {
    /// Workflow identity a `run_workflow` call may pass: the file basename without
    /// its extension.
    #[schemars(schema_with = "workflow_display_schema")]
    pub name: String,
    /// Workspace-relative path, e.g. `.github/workflows/coverage.yml`.
    #[schemars(schema_with = "relpath_schema")]
    pub file: String,
    /// The file's `name:` key, when it declares one.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "optional_name_schema")]
    pub display: Option<String>,
    /// How many entries `jobs` holds (`0` when `parse_error` is set).
    #[schemars(range(min = 0, max = 1024))]
    pub job_count: u32,
    /// The declared jobs, in file order.
    #[schemars(length(max = 1024))]
    pub jobs: Vec<JobEntry>,
    /// Why this one file could not be read — data inside a successful call.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "parse_error_schema")]
    pub parse_error: Option<ParseError>,
}

/// A file-level parse failure, from the `parse_error` code namespace.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ParseError {
    /// `invalid_yaml | not_a_map | jobs_missing | unsupported_yaml` — the closed enum
    /// the contract pins; see [`crate::workflow_yaml::WorkflowYamlError::code`].
    #[schemars(schema_with = "parse_code_schema")]
    pub code: String,
    /// One sentence, with the parser's position when it has one.
    #[schemars(length(min = 1, max = 1024))]
    pub message: String,
}

/// One job.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JobEntry {
    /// The job id — pass this as `run_workflow`'s `task`.
    #[schemars(schema_with = "task_id_schema")]
    pub id: String,
    /// The job's `name:` key, when it declares one.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "optional_name_schema")]
    pub name: Option<String>,
    /// Job ids this job depends on. Omitted when it has none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[schemars(schema_with = "needs_schema")]
    pub needs: Vec<String>,
    /// The `runs-on` labels, one per label.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[schemars(schema_with = "label_list_schema")]
    pub runs_on: Vec<String>,
    /// Names of the `strategy.matrix` axes.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[schemars(schema_with = "label_list_schema")]
    pub matrix_axes: Vec<String>,
    /// `true` when the job or one of its steps `uses:` a remote or local reusable
    /// workflow/action act must fetch or check out to resolve. Omitted when false.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "uses_composite_schema")]
    pub uses_composite: Option<bool>,
    /// ADVISORY guess — see [`category_hint`]. `benchmark | coverage | other`.
    #[schemars(schema_with = "category_hint_schema")]
    pub category_hint: String,
    /// The step summary, only when `include_steps` was requested.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "steps_schema")]
    pub steps: Option<Vec<StepEntry>>,
}

/// One step, only advertised when `include_steps` is on.
#[derive(Debug, Clone, Default, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StepEntry {
    /// The step's `name:`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "optional_name_schema")]
    pub name: Option<String>,
    /// The action or reusable workflow the step `uses:`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "optional_name_schema")]
    pub uses: Option<String>,
}

// The schema fragments above are the pinned contract's fragments, restated in Rust so
// the derived `outputSchema` and `schema/tool-contract.schema.json` cannot drift
// silently (tests/tool_schema_parity.rs compares them). Descriptions are the prose
// half of the pinned file and are not compared.

/// `$defs/relpath`.
fn relpath_schema(_: &mut SchemaGenerator) -> Schema {
    schema(json!({
        "type": "string",
        "minLength": 1,
        "maxLength": 512,
        "pattern": r"^[.]{0,2}[A-Za-z0-9][A-Za-z0-9 ._+-]*(/[.]{0,2}[A-Za-z0-9][A-Za-z0-9 ._+-]*)*$"
    }))
}

/// `$defs/task_id`.
fn task_id_schema(_: &mut SchemaGenerator) -> Schema {
    schema(json!({
        "type": "string",
        "minLength": 1,
        "maxLength": 255,
        "not": { "pattern": "\\.\\." },
        "pattern": r"^[A-Za-z0-9][A-Za-z0-9 ._/-]*$"
    }))
}

/// The `name` field of a workflow entry: like `task_id` but without the dot rule, and
/// without a `type` list for null, exactly as pinned.
fn workflow_display_schema(_: &mut SchemaGenerator) -> Schema {
    schema(json!({
        "type": "string",
        "minLength": 1,
        "maxLength": 255,
        "pattern": r"^[A-Za-z0-9][A-Za-z0-9 ._/-]*$"
    }))
}

/// A short optional display string (`display`, job `name`, step `name`/`uses`).
fn optional_name_schema(_: &mut SchemaGenerator) -> Schema {
    schema(json!({ "type": "string", "minLength": 1, "maxLength": 255 }))
}

/// `needs[]`.
fn needs_schema(_: &mut SchemaGenerator) -> Schema {
    schema(json!({
        "type": "array",
        "maxItems": MAX_NEEDS,
        "items": {
            "type": "string",
            "minLength": 1,
            "maxLength": 255,
            "not": { "pattern": "\\.\\." },
            "pattern": r"^[A-Za-z0-9][A-Za-z0-9 ._/-]*$"
        }
    }))
}

/// `runs_on[]` / `matrix_axes[]`.
fn label_list_schema(_: &mut SchemaGenerator) -> Schema {
    schema(json!({
        "type": "array",
        "maxItems": MAX_RUNS_ON,
        "items": { "type": "string", "minLength": 1, "maxLength": 64 }
    }))
}

/// `warnings[]` — `$defs/warning_code` items, unique, capped.
fn warnings_schema(_: &mut SchemaGenerator) -> Schema {
    schema(json!({
        "type": "array",
        "maxItems": MAX_WARNINGS,
        "uniqueItems": true,
        "items": {
            "type": "string",
            "minLength": 1,
            "maxLength": 64,
            "pattern": r"^[a-z][a-z0-9_]*$"
        }
    }))
}

/// The `category_hint` enum, spelled exactly as pinned (a bare `enum`).
fn category_hint_schema(_: &mut SchemaGenerator) -> Schema {
    schema(json!({ "enum": ["benchmark", "coverage", "other"] }))
}

/// The `parse_error.code` enum: exactly the members `$defs/list_tasks_result` pins —
/// `tests/tool_schema_parity.rs` fails if this list and that file disagree, which is
/// the point (a fifth member is a coordinated spec change, not a local edit).
fn parse_code_schema(_: &mut SchemaGenerator) -> Schema {
    schema(json!({
        "enum": ["invalid_yaml", "not_a_map", "jobs_missing", "unsupported_yaml"]
    }))
}

/// Turn a literal into a `Schema`.
fn schema(raw: Value) -> Schema {
    Schema::try_from(raw).expect("the literal contract fragments are schemas")
}

/// An optional field spelled the contract's way: **absent**, never `null`.
///
/// schemars models `Option<T>` as `T | null`, but ADR-0002 §2.1 forbids `null`
/// anywhere in a payload and every optional member in
/// `schema/tool-contract.schema.json/$defs/list_tasks_result` is expressed by leaving
/// the key out of `required` and `skip_serializing_if`-ing it away. This is what makes
/// the advertised schema say the same thing; `tests/tool_schema_parity.rs` is the
/// check, and it fails loudly on the `null` this removes.
fn not_null(input: Schema) -> Schema {
    let mut value = input.to_value();
    let Some(object) = value.as_object_mut() else {
        return Schema::try_from(value).expect("an object schema stays a schema");
    };
    if let Some(any_of) = object.remove("anyOf") {
        // `{anyOf: [{...}, null]}` collapses back to its one real member, keeping the
        // sibling keywords (`description`) that sat next to the `anyOf`.
        let members: Vec<Value> = any_of
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|member| member.get("type") != Some(&Value::String("null".to_owned())))
            .collect();
        match members.as_slice() {
            [only] => {
                if let Some(only) = only.as_object() {
                    object.extend(only.clone());
                }
            }
            many => {
                object.insert("anyOf".to_owned(), Value::Array(many.to_vec()));
            }
        }
    }
    if let Some(types) = object.get("type").and_then(Value::as_array).cloned() {
        let kept: Vec<Value> = types
            .into_iter()
            .filter(|kind| kind.as_str() != Some("null"))
            .collect();
        object.insert(
            "type".to_owned(),
            match kept.as_slice() {
                [one] => one.clone(),
                many => Value::Array(many.to_vec()),
            },
        );
    }
    Schema::try_from(value).expect("removing a null keeps a valid schema")
}

/// `parse_error` as the contract defines it, minus schemars' `| null`.
fn parse_error_schema(generator: &mut SchemaGenerator) -> Schema {
    not_null(<ParseError as JsonSchema>::json_schema(generator))
}

/// `uses_composite`: `true` or absent.
fn uses_composite_schema(_: &mut SchemaGenerator) -> Schema {
    schema(json!({"type": "boolean"}))
}

/// `steps`: the step array, capped, or absent.
fn steps_schema(generator: &mut SchemaGenerator) -> Schema {
    let mut steps = not_null(<Vec<StepEntry> as JsonSchema>::json_schema(generator));
    steps
        .ensure_object()
        .insert("maxItems".to_owned(), json!(MAX_STEPS));
    steps
}

// ---------------------------------------------------------------------------
// the category hint
// ---------------------------------------------------------------------------

/// The categories `category_hint` may report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CategoryHint {
    /// Probably a benchmark job.
    Benchmark,
    /// Probably a coverage job.
    Coverage,
    /// Nothing recognizable.
    Other,
}

impl CategoryHint {
    /// The wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Benchmark => "benchmark",
            Self::Coverage => "coverage",
            Self::Other => "other",
        }
    }
}

/// Markers that mean "this job collects coverage", matched case-insensitively as
/// substrings.
///
/// These are the project's *known* coverage commands and artifacts: the file names
/// ADR-0002 §3.2 looks for (`lcov.info`, `coverage-summary.json`) and the tools that
/// produce them. Adding a tool here is the whole of "supports another coverage
/// reporter" for hinting purposes.
const COVERAGE_MARKERS: &[&str] = &[
    "coverage",
    "lcov",
    "coveralls",
    "codecov",
    "llvm-cov",
    "tarpaulin",
    "kcov",
    "gcov",
    "nyc",
    "istanbul",
    "--coverage",
    "--cov",
];

/// Markers that mean "this job measures performance".
///
/// `bench` is intentionally a prefix, not a word: `cargo bench`, `hyperfine-bench`,
/// `run-benchmarks` all mean the same thing here, and a false positive is an advisory
/// hint rather than a wrong run.
const BENCHMARK_MARKERS: &[&str] = &[
    "benchmark",
    "bench",
    "hyperfine",
    "criterion",
    "benchmark.json",
    "benchmark.jsonl",
];

/// The one heuristic that turns a job into a `category_hint`. Nothing else in the
/// codebase guesses a category, and `run_workflow` never calls this: ADR-0001 §2.4
/// keeps `category` caller-supplied, so this is advice for a human or an agent
/// picking a job, and nothing more.
///
/// ## How it decides
///
/// Text is lowercased, then examined in this order — first level that contains a
/// marker wins, and **coverage beats benchmark** at each level because coverage tool
/// names are distinctive while `bench` is a substring of half the English language:
///
/// 1. the job **id** (the strongest signal: whoever named the job knew what it was),
/// 2. the job's `name:` key,
/// 3. any step's `name:`, its `uses:` action, or the first
///    [`crate::workflow_yaml::MAX_STEP_RUN_BYTES`] bytes of its `run:` script — which
///    is where "the project's known coverage commands" actually show up
///    (`cargo llvm-cov`, `lcov --list`, `./gradlew jacocoTestReport` and friends).
///
/// No marker at any level → [`CategoryHint::Other`]. A job that only says `build`
/// stays `other` even when its workflow is called `Benchmark`: the *job* is what a
/// caller picks, so the *job* is what the hint describes.
pub fn category_hint(job: &JobIndex) -> CategoryHint {
    for haystack in std::iter::once(job.id.as_str())
        .chain(job.name.as_deref())
        .chain(job.steps.iter().flat_map(|step| {
            step.name
                .as_deref()
                .into_iter()
                .chain(step.uses.as_deref())
                .chain(step.run_snippet.as_deref())
        }))
    {
        let text = haystack.to_lowercase();
        if COVERAGE_MARKERS.iter().any(|marker| text.contains(marker)) {
            return CategoryHint::Coverage;
        }
        if BENCHMARK_MARKERS.iter().any(|marker| text.contains(marker)) {
            return CategoryHint::Benchmark;
        }
    }
    CategoryHint::Other
}

// ---------------------------------------------------------------------------
// the call
// ---------------------------------------------------------------------------

/// Accumulates warnings in first-seen order, deduped and capped.
#[derive(Debug, Default)]
struct Warnings(Vec<String>);

impl Warnings {
    fn add(&mut self, code: &str) {
        if !self.0.iter().any(|seen| seen == code) {
            self.0.push(code.to_owned());
        }
    }

    fn finish(mut self) -> Vec<String> {
        if self.0.len() > MAX_WARNINGS {
            self.0.truncate(MAX_WARNINGS);
        }
        self.0
    }
}

/// One workflow file as the caller will see it.
#[derive(Debug)]
pub struct Entry {
    /// The advertised entry.
    pub entry: WorkflowEntry,
    /// The file behind it (needed by [`cross_check_with_act`]).
    pub file: WorkflowFile,
}

/// Answer a `list_tasks` call.
///
/// Pure and blocking: reads the filesystem, resolves nothing through git, spawns
/// nothing, takes no lock. `Ok` is a contract-shaped [`ListTasksResult`]; the only
/// `Err`s are the four codes ADR-0001 §6.1 allows this tool.
pub fn list_tasks(config: &Config, args: &ListTasksArgs) -> Result<ListTasksResult, ToolError> {
    let workspace = config.workspace.as_path();
    if !workspace.is_dir() {
        return Err(workflows_dir_error(
            "workspace_not_found",
            &display_path(workspace, workspace),
            "the workspace directory is missing or is not a directory",
        ));
    }

    let files = workflows::index_directory(workspace).map_err(|error| {
        // A file that cannot be *read* is still "this workflows dir is not usable",
        // which is the only code §6.1 gives this tool for it.
        let (path, reason) = match error {
            workflows::IndexError::Unresolvable => (
                WORKFLOWS_DIR.to_owned(),
                "`.github/workflows` is absent or could not be resolved".to_owned(),
            ),
            workflows::IndexError::ReadDir { reason } => (
                WORKFLOWS_DIR.to_owned(),
                format!("it could not be read: {reason}"),
            ),
            workflows::IndexError::ReadFile { file, reason } => {
                (file, format!("the file could not be read: {reason}"))
            }
        };
        workflows_dir_error("workflows_dir_missing", &path, &reason)
    })?;

    if files.is_empty() {
        return Err(workflows_dir_error(
            "workflows_dir_missing",
            WORKFLOWS_DIR,
            "no `.yml` or `.yaml` file was found — this is not an Actions repository",
        ));
    }

    // `workflow` narrows the answer. Resolution is the *same* function
    // `run_workflow` uses, so anything advertised here is a `workflow` value that
    // works, and anything it rejects here it would reject there.
    let selected: Vec<&WorkflowFile> = match &args.workflow {
        None => files.iter().collect(),
        Some(requested) => match workflows::resolve(&files, requested) {
            Ok(file) => vec![file],
            Err(error) => {
                return Err(match error {
                    workflows::ResolveError::NotFound { .. } => ToolError::from_validation(
                        TOOL,
                        &ValidationError::WorkflowNotFound {
                            workflow: requested.clone(),
                            workflows_dir: WORKFLOWS_DIR,
                            available: ValidationError::option_list(
                                files.iter().map(|file| file.stem.clone()).collect(),
                            ),
                            reason: None,
                        },
                    ),
                    workflows::ResolveError::Ambiguous { candidates } => {
                        ToolError::from_validation(
                            TOOL,
                            &ValidationError::AmbiguousWorkflow {
                                workflow: requested.clone(),
                                candidates: ValidationError::option_list(candidates),
                            },
                        )
                    }
                });
            }
        },
    };

    let mut warnings = Warnings::default();
    let mut entries: Vec<Entry> = Vec::new();
    for file in selected {
        if entries.len() >= MAX_WORKFLOWS {
            warnings.add(WARNING_TRUNCATED);
            break;
        }
        // A file whose basename cannot be *passed back* as a `workflow` argument is
        // not a workflow the caller can run (GitHub ignores dot-files here too), so
        // it is left out — but loudly, never silently.
        if !name_is_expressible(&file.stem) {
            warnings.add(WARNING_SKIPPED_WORKFLOW);
            continue;
        }
        if file.parse_error.is_some() {
            warnings.add(WARNING_PARSE_ERROR);
        }
        let jobs = job_entries(file, args.include_steps, &mut warnings);
        let parse_error = file.parse_error.as_ref().map(|error| ParseError {
            code: error.code().to_owned(),
            message: clamp(&error.message(), 1024),
        });
        entries.push(Entry {
            file: file.clone(),
            entry: WorkflowEntry {
                name: clamp(&file.stem, MAX_NAME_BYTES),
                file: file.relpath.clone(),
                display: file
                    .declared_name
                    .as_deref()
                    .map(|name| clamp(name, MAX_NAME_BYTES)),
                job_count: jobs.len() as u32,
                jobs,
                parse_error,
            },
        });
    }

    Ok(ListTasksResult {
        workflows_dir: WORKFLOWS_DIR.to_owned(),
        workflow_count: entries.len() as u32,
        workflows: entries.into_iter().map(|entry| entry.entry).collect(),
        warnings: warnings.finish(),
    })
}

/// Build the job list of one workflow, applying the contract's caps (whole elements
/// dropped, `truncated` stamped when any is).
fn job_entries(file: &WorkflowFile, include_steps: bool, warnings: &mut Warnings) -> Vec<JobEntry> {
    let mut jobs = Vec::new();
    for job in &file.index.jobs {
        if jobs.len() >= MAX_JOBS {
            warnings.add(WARNING_TRUNCATED);
            break;
        }
        let needs = clamp_list(&job.needs, MAX_NEEDS, warnings);
        let runs_on = clamp_list(&job.runs_on, MAX_RUNS_ON, warnings);
        let matrix_axes = clamp_list(&job.matrix_axes, MAX_MATRIX_AXES, warnings);
        let steps = include_steps.then(|| {
            let steps = clamp_list(&job.steps, MAX_STEPS, warnings);
            steps
                .into_iter()
                .map(|step| StepEntry {
                    name: step.name.map(|name| clamp(&name, MAX_NAME_BYTES)),
                    uses: step.uses.map(|uses| clamp(&uses, MAX_NAME_BYTES)),
                })
                .collect::<Vec<_>>()
        });
        jobs.push(JobEntry {
            id: clamp(&job.id, MAX_NAME_BYTES),
            name: job.name.as_deref().map(|name| clamp(name, MAX_NAME_BYTES)),
            needs,
            runs_on,
            matrix_axes,
            uses_composite: job.uses_external().then_some(true),
            category_hint: category_hint(job).as_str().to_owned(),
            steps,
        });
    }
    jobs
}

/// Drop whole elements past `max`, stamping `truncated` (ADR-0001 §5.2's rule for
/// bounded lists: truncate the list, never cut an element in half).
fn clamp_list<T: Clone>(values: &[T], max: usize, warnings: &mut Warnings) -> Vec<T> {
    if values.len() <= max {
        return values.to_vec();
    }
    warnings.add(WARNING_TRUNCATED);
    values[..max].to_vec()
}

/// A `workflows_dir_missing`-shaped error: `detail.path` is workspace-relative, never
/// a host path (ADR-0001 §5.2).
fn workflows_dir_error(code: &'static str, path: &str, reason: &str) -> ToolError {
    let mut detail = Map::new();
    detail.insert("path".to_owned(), json!(path));
    detail.insert("reason".to_owned(), json!(reason));
    ToolError::new(
        TOOL,
        code,
        format!("`{path}` is not a usable workflows directory ({reason})"),
        // ADR-0001 §5.3 marks this code non-retryable: the workspace does not
        // sprout workflows between calls on its own.
        false,
        detail,
    )
}

/// What a JSON value is, in prose an agent can act on.
fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Whether a workflow's basename can be advertised as `name` and passed back.
fn name_is_expressible(stem: &str) -> bool {
    WorkflowNameShape::check(stem).is_ok()
}

/// Truncate to `max` bytes on a character boundary.
fn clamp(text: &str, max: usize) -> String {
    crate::workspace::truncate_bytes(text, max)
}

// ---------------------------------------------------------------------------
// the opt-in act cross-check
// ---------------------------------------------------------------------------

/// What act's own listing said.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ActView {
    /// Relpaths (`.github/workflows/<file>`) act loaded a job from.
    pub loaded: Vec<String>,
    /// Job ids act listed, in the order it listed them.
    pub jobs: Vec<String>,
}

/// Ask act which workflows it can load, and warn about the ones it does not list.
///
/// Only ever called when the operator asked for it
/// (`--list-tasks-act-cross-check`). Never fails the call and never edits `result`
/// beyond `warnings`: every failure mode — no act binary, non-zero exit, timeout,
/// output that is not the expected table, rows that match none of our files — ends in
/// [`WARNING_ACT_VIEW_UNAVAILABLE`] and leaves the YAML-derived answer standing.
pub async fn cross_check_with_act(config: &Config, result: &mut ListTasksResult) {
    let act = &config.act_binary;
    if !act.is_file() {
        push_warning(result, WARNING_ACT_VIEW_UNAVAILABLE);
        return;
    }
    // `act list` (alias `act l`) prints the table; `--workflows` accepts the
    // workflows directory, `--directory` points act at the repository. No `--json`:
    // act's list subcommand has none (its `--json` is the run log format), so the
    // table is what there is to parse.
    let mut command = tokio::process::Command::new(act);
    command
        .arg("list")
        .arg("--directory")
        .arg(&config.workspace)
        .arg("--workflows")
        .arg(config.workspace.join(WORKFLOWS_DIR))
        .stdin(std::process::Stdio::null());
    let spawned = tokio::time::timeout(ACT_LIST_TIMEOUT, command.output()).await;
    let output = match spawned {
        Ok(Ok(output)) => output,
        Ok(_) | Err(_) => {
            push_warning(result, WARNING_ACT_VIEW_UNAVAILABLE);
            return;
        }
    };
    if !output.status.success() {
        push_warning(result, WARNING_ACT_VIEW_UNAVAILABLE);
        return;
    }
    let view = parse_act_list(&String::from_utf8_lossy(&output.stdout));
    if view.loaded.is_empty() {
        // Nothing could be lined up with our files: inconclusive, not evidence that
        // act refuses everything.
        push_warning(result, WARNING_ACT_VIEW_UNAVAILABLE);
        return;
    }

    let loaded: Vec<&str> = view.loaded.iter().map(String::as_str).collect();
    let refused: Vec<&str> = result
        .workflows
        .iter()
        .filter(|workflow| workflow.parse_error.is_none())
        .filter(|workflow| !loaded.contains(&workflow.file.as_str()))
        .map(|workflow| workflow.file.as_str())
        .collect();
    if !refused.is_empty() {
        // Named in the log because the payload says only `unresolvable_workflow`: the
        // warning vocabulary is a set of codes, with nowhere per-file to put act's text.
        tracing::debug!(
            target: "act_mcp::tools::list_tasks",
            refused = ?refused,
            "act listed no job for these files; the listing stands as YAML read it"
        );
        push_warning(result, WARNING_UNRESOLVABLE_WORKFLOW);
    }
}

/// Parse `act list`'s table.
///
/// act writes it with Go's `text/tabwriter`, so columns are space-padded, aligned
/// copies of `Stage`, `Job`, `Dependencies`, `Workflow` — no tabs survive the padding,
/// and a job id or workflow path contains no run of two spaces. That makes a run of
/// two or more spaces the only safe column separator, and it is why the parser counts
/// fields instead of looking for offsets:
///
/// ```text
/// Stage  Job       Dependencies  Workflow
/// -      build     -             .github/workflows/ci.yml
/// ```
///
/// A row counts only if its `Workflow` column names a workflow file (a `.yml`/`.yaml`
/// name, or anything with `workflows/` in it) — act prints warnings and progress text
/// on stdout too, and a line that happens to have four whitespace-separated words is
/// not a job row. Relpaths are then matched against our files by
/// `normalize_act_workflow_column`; a row that names a file we do not have is
/// ignored rather than guessed at. A header row is skipped, and so is a row whose job
/// column is `-` or empty: act writes that for a workflow file with no jobs, which is
/// not evidence the file is unloadable.
pub fn parse_act_list(text: &str) -> ActView {
    let mut view = ActView::default();
    for line in text.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line
            .split([' ', '\t'])
            .filter(|field| !field.is_empty())
            .collect();
        // With tabwriter padding gone on single-space columns, counting fields is
        // ambiguous; require the four columns and drop anything else.
        if fields.len() < 4 {
            continue;
        }
        let (stage, job, _dependencies, workflow) = (fields[0], fields[1], fields[2], fields[3]);
        if stage == "Stage" && job == "Job" {
            continue; // header
        }
        if job == "-" || job.is_empty() {
            continue;
        }
        let Some(relpath) = normalize_act_workflow_column(workflow) else {
            continue;
        };
        view.jobs.push(job.to_owned());
        if !view.loaded.contains(&relpath) {
            view.loaded.push(relpath);
        }
    }
    view
}

/// Turn act's `Workflow` column into a `.github/workflows/...` relpath.
///
/// `None` means "not a workflow file", which is also how a non-row line is dropped.
fn normalize_act_workflow_column(column: &str) -> Option<String> {
    let column = column.trim();
    if column.is_empty() || column == "-" {
        return None;
    }
    // A path of any shape: keep from `workflows/` on when present, else accept it
    // verbatim if it already looks like our relpath spelling.
    if let Some(index) = column.find("workflows/") {
        return Some(format!(
            "{WORKFLOWS_DIR}/{}",
            &column[index + "workflows/".len()..]
        ));
    }
    if column.contains('/') {
        return None; // some other path we cannot line up
    }
    // A bare name: the file name (act prints the declared `name:` in some versions,
    // but a name never ends in `.yml`, so treat this as a file and let the caller's
    // "did act see my file?" comparison decide).
    if column.ends_with(".yml") || column.ends_with(".yaml") {
        return Some(format!("{WORKFLOWS_DIR}/{column}"));
    }
    None
}

fn push_warning(result: &mut ListTasksResult, code: &str) {
    if !result.warnings.iter().any(|seen| seen == code) && result.warnings.len() < MAX_WARNINGS {
        result.warnings.push(code.to_owned());
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn args_default_to_listing_everything() {
        let args = ListTasksArgs::parse(None).expect("no arguments is legal");
        assert_eq!(args, ListTasksArgs::default());
        assert!(!args.include_steps);
    }

    #[test]
    fn args_are_read_and_validated() {
        let mut arguments = JsonObject::new();
        arguments.insert("workflow".to_owned(), json!("coverage"));
        arguments.insert("include_steps".to_owned(), json!(true));
        let args = ListTasksArgs::parse(Some(&arguments)).expect("valid");
        assert_eq!(args.workflow.as_deref(), Some("coverage"));
        assert!(args.include_steps);
    }

    #[test]
    fn an_unknown_argument_is_unknown_argument() {
        let mut arguments = JsonObject::new();
        arguments.insert("run_id".to_owned(), json!("20261110T011507Z-d4e5f6"));
        let error = ListTasksArgs::parse(Some(&arguments)).expect_err("must be rejected");
        assert_eq!(error.code(), "unknown_argument");
        let payload = error.structured_content();
        assert_eq!(payload["error"]["detail"]["unexpected"][0], json!("run_id"));
        assert_eq!(
            payload["error"]["detail"]["expected"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn bad_argument_shape_names_every_problem_at_once() {
        let mut arguments = JsonObject::new();
        arguments.insert("workflow".to_owned(), json!(7));
        arguments.insert("include_steps".to_owned(), json!("yes"));
        let error = ListTasksArgs::parse(Some(&arguments)).expect_err("must be rejected");
        assert_eq!(error.code(), "invalid_arguments");
        assert_eq!(
            error.structured_content()["error"]["detail"]["errors"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn workflow_names_that_could_escape_are_rejected_before_any_path_is_joined() {
        for bad in [
            "../etc/passwd",
            ".github/workflows/../../etc/passwd.yml",
            "/etc/passwd",
            "-rf",
            "",
            &"x".repeat(256),
        ] {
            let mut arguments = JsonObject::new();
            arguments.insert("workflow".to_owned(), json!(bad));
            let error = ListTasksArgs::parse(Some(&arguments))
                .expect_err(&format!("`{bad}` must be rejected"));
            assert_eq!(error.code(), "invalid_arguments", "`{bad}`");
        }
    }

    #[test]
    fn a_dot_file_is_skipped_with_a_warning_rather_than_advertised() {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = crate::fixtures::TestRepo::init(&root.path().join("repo"));
        repo.write_workflow(
            ".disabled.yml",
            "on: push\njobs:\n  gone:\n    runs-on: x\n",
        );
        let result = call(&repo);
        assert!(
            result
                .workflows
                .iter()
                .all(|workflow| workflow.name != ".disabled"),
            "{:?}",
            result.workflows
        );
        assert!(result
            .warnings
            .contains(&WARNING_SKIPPED_WORKFLOW.to_owned()));
    }

    #[test]
    fn category_hint_reads_benchmark_markers() {
        for (id, name, run) in [
            ("hyperfine-bench", None, None),
            ("bench", None, None),
            ("nightly", Some("Run benchmarks"), None),
            ("job", Some("measure"), Some("cargo bench --release")),
            (
                "job",
                None,
                Some("hyperfine 'target/release/cli' --warmup 3"),
            ),
            ("job", None, Some("criterion through cargo-criterion")),
        ] {
            let job = job(id, name, run, None);
            assert_eq!(
                category_hint(&job),
                CategoryHint::Benchmark,
                "{id} / {name:?} / {run:?}"
            );
        }
    }

    #[test]
    fn category_hint_reads_coverage_markers() {
        for (id, name, run, uses) in [
            ("coverage", None, None, None),
            ("test", Some("Coverage report"), None, None),
            ("test", None, Some("cargo llvm-cov --html"), None),
            ("test", None, Some("lcov --directory . --capture"), None),
            ("test", None, Some("pytest --cov=. --cov-report=xml"), None),
            ("test", None, None, Some("codecov/codecov-action@v5")),
            ("test", None, None, Some("coverallsapp/github-action@v2")),
            (
                "test",
                None,
                Some("node node_modules/.bin/nyc report"),
                None,
            ),
        ] {
            let job = job(id, name, run, uses);
            assert_eq!(
                category_hint(&job),
                CategoryHint::Coverage,
                "{id} / {name:?} / {run:?} / {uses:?}"
            );
        }
    }

    #[test]
    fn category_hint_says_other_when_nothing_matches() {
        for (id, name, run, uses) in [
            ("build", None, Some("cargo build --release"), None),
            ("lint", Some("clippy"), None, None),
            ("deploy", None, Some("./scripts/deploy.sh"), None),
            ("docs", None, None, Some("actions/upload-pages-artifact@v3")),
        ] {
            let job = job(id, name, run, uses);
            assert_eq!(
                category_hint(&job),
                CategoryHint::Other,
                "{id} / {name:?} / {run:?} / {uses:?}"
            );
        }
    }

    #[test]
    fn coverage_wins_when_a_job_matches_both() {
        // Documented precedence: coverage markers are distinctive, `bench` is not.
        let job = job(
            "job",
            None,
            Some("cargo bench --release && cargo llvm-cov"),
            None,
        );
        assert_eq!(category_hint(&job), CategoryHint::Coverage);
    }

    #[test]
    fn the_hint_describes_the_job_not_the_workflow() {
        // A `build` job inside a benchmark workflow is still just a build.
        let text = "\
name: Benchmark
on: push
jobs:
  build:
    runs-on: ubuntu-24.04
    steps:
      - run: cargo build --release
  hyperfine-bench:
    needs: build
    runs-on: ubuntu-24.04
    steps:
      - run: hyperfine './target/release/cli'
";
        let index = crate::workflow_yaml::index_workflow_str(text).expect("parses");
        assert_eq!(
            category_hint(index.job("build").expect("build")),
            CategoryHint::Other
        );
        assert_eq!(
            category_hint(index.job("hyperfine-bench").expect("bench")),
            CategoryHint::Benchmark
        );
    }

    fn job(id: &str, name: Option<&str>, run: Option<&str>, uses: Option<&str>) -> JobIndex {
        JobIndex {
            id: id.to_owned(),
            name: name.map(str::to_owned),
            steps: vec![crate::workflow_yaml::StepIndex {
                name: None,
                uses: uses.map(str::to_owned),
                run_snippet: run.map(str::to_owned),
            }],
            ..JobIndex::default()
        }
    }

    /// A config that points at `repo` and *deliberately* names an act binary that
    /// does not exist: acceptance criterion "the call works with no docker daemon and
    /// no act binary present" is asserted by every test in this module, not by one.
    fn config_for(repo: &crate::fixtures::TestRepo) -> Config {
        Config {
            act_binary: std::path::Path::new("/nonexistent/act").to_path_buf(),
            git_timeout: Duration::from_secs(1),
            ..Config::with_hardened_defaults(
                repo.path().to_path_buf(),
                repo.path().join(".evidence"),
                std::path::Path::new("/nonexistent/act").to_path_buf(),
            )
        }
    }

    // ------------------------------------------------------------- the acceptance case

    /// AC: a fixture repo with three workflow files — one with an explicit `name:`,
    /// one with a matrix, one malformed — returns all three, the malformed one with an
    /// `error` field, and correct job ids for the others.
    #[test]
    fn the_three_file_fixture_returns_all_three_with_the_broken_one_reported() {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = crate::fixtures::TestRepo::init(&root.path().join("repo"));
        repo.with_list_tasks_fixture();

        let result = call(&repo);
        assert_eq!(result.workflow_count, 3, "{result:?}");
        assert_eq!(result.workflows_dir, WORKFLOWS_DIR);
        let by_file: Vec<(&str, &WorkflowEntry)> = result
            .workflows
            .iter()
            .map(|workflow| (workflow.file.as_str(), workflow))
            .collect();
        assert_eq!(
            by_file.iter().map(|(file, _)| *file).collect::<Vec<_>>(),
            vec![
                ".github/workflows/benchmark.yml",
                ".github/workflows/broken.yml",
                ".github/workflows/coverage.yml",
            ]
        );

        let benchmark = by_file[0].1;
        assert_eq!(benchmark.name, "benchmark");
        assert_eq!(benchmark.display.as_deref(), Some("Benchmark"));
        assert_eq!(benchmark.job_count, 2);
        assert_eq!(
            benchmark
                .jobs
                .iter()
                .map(|job| job.id.as_str())
                .collect::<Vec<_>>(),
            vec!["build", "hyperfine-bench"]
        );
        let bench = &benchmark.jobs[1];
        assert_eq!(bench.name.as_deref(), Some("Run hyperfine benchmarks"));
        assert_eq!(bench.needs, vec!["build"]);
        assert_eq!(bench.runs_on, vec!["ubuntu-24.04"]);
        assert_eq!(bench.matrix_axes, vec!["profile"]);
        assert_eq!(bench.uses_composite, Some(true));
        assert_eq!(bench.category_hint, "benchmark");
        assert_eq!(benchmark.jobs[0].category_hint, "other");
        assert_eq!(benchmark.jobs[0].uses_composite, None);
        assert!(benchmark.parse_error.is_none());

        let coverage = by_file[2].1;
        assert_eq!(coverage.name, "coverage");
        assert_eq!(coverage.display, None, "the file declares no `name:`");
        assert_eq!(coverage.jobs[0].category_hint, "coverage");
        assert_eq!(coverage.jobs[0].uses_composite, Some(true));
        assert!(coverage.jobs[0].steps.is_none(), "steps are opt-in");

        let broken = by_file[1].1;
        let error = broken
            .parse_error
            .as_ref()
            .expect("broken.yml reports an error");
        assert_eq!(error.code, "invalid_yaml");
        assert!(
            error.message.contains("line"),
            "the parser's position belongs in the message: {}",
            error.message
        );
        assert_eq!(broken.job_count, 0);
        assert!(broken.jobs.is_empty());
        assert!(result.warnings.contains(&WARNING_PARSE_ERROR.to_owned()));
        assert!(
            !result
                .warnings
                .iter()
                .any(|warning| warning == WARNING_TRUNCATED),
            "{:?}",
            result.warnings
        );
    }

    /// Nothing in the payload may be an absolute host path (ADR-0001 §5.2/§3.4).
    #[test]
    fn no_absolute_host_path_appears_anywhere_in_the_result() {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = crate::fixtures::TestRepo::init(&root.path().join("repo"));
        repo.with_list_tasks_fixture();
        let json = serde_json::to_string(&call(&repo).workflows).expect("serializes");
        assert!(
            !json.contains(&repo.path().display().to_string()),
            "the workspace path leaked: {json}"
        );
        assert!(!json.contains("\"file\":\"/"), "{json}");
    }

    /// The call is defined to work with no act binary and no docker daemon; `call`
    /// already points `act_binary` at a path that does not exist, and this names the
    /// requirement so a future "just probe act" change fails here.
    #[test]
    fn the_call_succeeds_with_no_act_binary_and_no_container_runtime() {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = crate::fixtures::TestRepo::init(&root.path().join("repo"));
        repo.with_list_tasks_fixture();
        let config = config_for(&repo);
        assert!(
            !config.act_binary.exists(),
            "fixture must not have act: {:?}",
            config.act_binary
        );
        let result = list_tasks(&config, &ListTasksArgs::default()).expect("must not need act");
        assert_eq!(result.workflow_count, 3);
        assert_eq!(result.warnings, vec![WARNING_PARSE_ERROR.to_owned()]);
    }

    /// It writes nothing: a discovery call must not dirty the tree it is describing.
    #[test]
    fn listing_leaves_the_repository_untouched() {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = crate::fixtures::TestRepo::init(&root.path().join("repo"));
        repo.with_list_tasks_fixture();
        let before = repo.porcelain();
        call(&repo);
        assert_eq!(repo.porcelain(), before);
    }

    // -------------------------------------------------------------- the YAML 1.1 trap

    /// AC: a workflow using the bare `on:` key — which a YAML 1.1 parser turns into a
    /// `true` key — parses and is listed, including a job id that reads as a boolean.
    #[test]
    fn a_workflow_with_a_bare_on_key_is_listed() {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = crate::fixtures::TestRepo::init(&root.path().join("repo"));
        repo.remove_all_workflows();
        repo.write_workflow("tricky.yml", crate::fixtures::BARE_ON_WORKFLOW);
        let result = call(&repo);
        assert_eq!(result.workflow_count, 1, "{result:?}");
        let workflow = &result.workflows[0];
        assert!(workflow.parse_error.is_none());
        assert_eq!(
            workflow
                .jobs
                .iter()
                .map(|job| job.id.as_str())
                .collect::<Vec<_>>(),
            vec!["on"]
        );
        assert_eq!(workflow.jobs[0].category_hint, "other");
    }

    // ------------------------------------------------------------------- the filter

    #[test]
    fn the_workflow_filter_returns_just_that_workflow() {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = crate::fixtures::TestRepo::init(&root.path().join("repo"));
        repo.with_list_tasks_fixture();
        for requested in ["coverage", "coverage.yml", ".github/workflows/coverage.yml"] {
            let args = ListTasksArgs {
                workflow: Some(requested.to_owned()),
                include_steps: false,
            };
            let result = list_tasks(&config_for(&repo), &args)
                .unwrap_or_else(|error| panic!("`{requested}` must resolve: {error:?}"));
            assert_eq!(result.workflow_count, 1, "`{requested}`");
            assert_eq!(result.workflows[0].file, ".github/workflows/coverage.yml");
        }
        // Form 4: the declared `name:` key.
        let args = ListTasksArgs {
            workflow: Some("Benchmark".to_owned()),
            include_steps: false,
        };
        let result = list_tasks(&config_for(&repo), &args).expect("declared name resolves");
        assert_eq!(result.workflows[0].name, "benchmark");
    }

    #[test]
    fn the_filter_reports_a_missing_workflow_with_the_options_that_exist() {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = crate::fixtures::TestRepo::init(&root.path().join("repo"));
        repo.with_list_tasks_fixture();
        let args = ListTasksArgs {
            workflow: Some("bench".to_owned()),
            include_steps: false,
        };
        let error = list_tasks(&config_for(&repo), &args).expect_err("must fail");
        assert_eq!(error.code(), "workflow_not_found");
        let payload = error.structured_content();
        assert_eq!(payload["error"]["detail"]["workflow"], json!("bench"));
        assert_eq!(
            payload["error"]["detail"]["available"],
            json!(["benchmark", "broken", "coverage"])
        );
        assert_eq!(payload["error"]["retryable"], json!(false));
    }

    #[test]
    fn a_bare_name_matching_two_files_is_ambiguous() {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = crate::fixtures::TestRepo::init(&root.path().join("repo"));
        repo.write_workflow("bench.yml", "on: push\njobs:\n  a:\n    runs-on: x\n");
        repo.write_workflow("bench.yaml", "on: push\njobs:\n  b:\n    runs-on: x\n");
        let args = ListTasksArgs {
            workflow: Some("bench".to_owned()),
            include_steps: false,
        };
        let error = list_tasks(&config_for(&repo), &args).expect_err("must fail");
        assert_eq!(error.code(), "ambiguous_workflow");
        assert_eq!(
            error.structured_content()["error"]["detail"]["candidates"],
            json!([
                ".github/workflows/bench.yaml",
                ".github/workflows/bench.yml"
            ])
        );
    }

    // ----------------------------------------------------------------------- steps

    #[test]
    fn include_steps_returns_the_step_summary_only_when_asked() {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = crate::fixtures::TestRepo::init(&root.path().join("repo"));
        repo.with_list_tasks_fixture();
        let args = ListTasksArgs {
            workflow: Some("coverage".to_owned()),
            include_steps: true,
        };
        let result = list_tasks(&config_for(&repo), &args).expect("call");
        let steps = result.workflows[0].jobs[0]
            .steps
            .as_ref()
            .expect("steps requested");
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].name.as_deref(), Some("Run tests"));
        assert_eq!(steps[0].uses.as_deref(), Some("nick-fields/retry@v3"));
        // The `run:` text a step carries is scanned for markers but never advertised
        // (the pinned step object has only name and uses, and `additionalProperties`
        // is false).
        let json = serde_json::to_string(&steps).expect("serializes");
        assert!(!json.contains("llvm-cov"), "{json}");
    }

    // ------------------------------------------------------------------ workspace state

    #[test]
    fn a_workspace_without_workflows_is_workflows_dir_missing() {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = crate::fixtures::TestRepo::init_empty_workflows(&root.path().join("repo"));
        let error =
            list_tasks(&config_for(&repo), &ListTasksArgs::default()).expect_err("must fail");
        assert_eq!(error.code(), "workflows_dir_missing");
        assert_eq!(
            error.structured_content()["error"]["detail"]["path"],
            json!(WORKFLOWS_DIR)
        );
    }

    #[test]
    fn a_workspace_that_vanished_is_workspace_not_found_not_a_panic() {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = crate::fixtures::TestRepo::init(&root.path().join("repo"));
        let mut config = config_for(&repo);
        let gone = root.path().join("gone");
        config.workspace = gone.clone();
        let error = list_tasks(&config, &ListTasksArgs::default()).expect_err("must fail");
        assert_eq!(error.code(), "workspace_not_found");
    }

    // ------------------------------------------------------------------ act cross-check

    /// A stub `act` that prints the given table (or fails), so the cross-check can be
    /// tested without act installed.
    fn stub_act(dir: &std::path::Path, script: &str) -> std::path::PathBuf {
        let path = dir.join("act");
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).expect("write stub act");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("make stub executable");
        }
        #[cfg(not(unix))]
        {
            let _ = &path;
        }
        path
    }

    const TABLE: &str = r#"printf 'Stage  Job              Dependencies  Workflow\n'
printf -- '-      build            -             .github/workflows/benchmark.yml\n'
printf -- '-      hyperfine-bench  build         .github/workflows/benchmark.yml\n'
printf -- '-      coverage         -             .github/workflows/coverage.yml\n'"#;

    #[tokio::test]
    async fn the_cross_check_leaves_files_act_loaded_alone() {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = crate::fixtures::TestRepo::init(&root.path().join("repo"));
        repo.with_list_tasks_fixture();
        let mut config = config_for(&repo);
        config.act_binary = stub_act(root.path(), TABLE);
        config.list_tasks_act_cross_check = true;

        let mut result = list_tasks(&config, &ListTasksArgs::default()).expect("call");
        cross_check_with_act(&config, &mut result).await;
        // broken.yml already has its own parse_error and is not relabelled; the other
        // two were loaded, so nothing is marked.
        for workflow in &result.workflows {
            let code = workflow
                .parse_error
                .as_ref()
                .map(|error| error.code.as_str());
            assert_eq!(
                code,
                if workflow.name == "broken" {
                    Some("invalid_yaml")
                } else {
                    None
                },
                "{workflow:?}"
            );
        }
        assert!(result
            .warnings
            .iter()
            .all(|warning| warning != WARNING_UNRESOLVABLE_WORKFLOW));
    }

    #[tokio::test]
    async fn the_cross_check_reports_a_file_act_refuses_to_load() {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = crate::fixtures::TestRepo::init(&root.path().join("repo"));
        repo.with_list_tasks_fixture();
        let mut config = config_for(&repo);
        // act lists the benchmark file only: coverage is the "unresolvable" one.
        config.act_binary = stub_act(
            root.path(),
            "printf 'Stage  Job     Dependencies  Workflow\n'\n\
             printf -- '-      build   -             .github/workflows/benchmark.yml\n'",
        );
        config.list_tasks_act_cross_check = true;

        let mut result = list_tasks(&config, &ListTasksArgs::default()).expect("call");
        cross_check_with_act(&config, &mut result).await;
        let coverage = result
            .workflows
            .iter()
            .find(|workflow| workflow.name == "coverage")
            .expect("coverage is still listed, not silently omitted");
        assert!(
            coverage.parse_error.is_none(),
            "act's refusal is a warning, never a rewritten parse_error: {coverage:?}"
        );
        assert!(!coverage.jobs.is_empty(), "and its jobs are still listed");
        assert!(result
            .warnings
            .contains(&WARNING_UNRESOLVABLE_WORKFLOW.to_owned()));
    }

    #[tokio::test]
    async fn the_cross_check_degrades_to_a_warning_when_act_cannot_answer() {
        for (name, script) in [
            ("exits non-zero", "exit 3"),
            ("prints nothing", "true"),
            ("prints a foreign shape", "echo '{not a table at all'"),
        ] {
            let root = tempfile::tempdir().expect("tempdir");
            let repo = crate::fixtures::TestRepo::init(&root.path().join("repo"));
            repo.with_list_tasks_fixture();
            let mut config = config_for(&repo);
            config.act_binary = stub_act(root.path(), script);
            config.list_tasks_act_cross_check = true;
            let mut result = list_tasks(&config, &ListTasksArgs::default()).expect("call");
            cross_check_with_act(&config, &mut result).await;
            assert!(
                result
                    .warnings
                    .contains(&WARNING_ACT_VIEW_UNAVAILABLE.to_owned()),
                "{name}: {:?}",
                result.warnings
            );
            assert_eq!(result.workflow_count, 3, "{name}: the listing stands");
            assert!(
                result
                    .warnings
                    .iter()
                    .all(|warning| warning != WARNING_UNRESOLVABLE_WORKFLOW),
                "{name} must not claim act refused anything: {:?}",
                result.warnings
            );
            // broken.yml is still reported by our own parser, and nothing else is.
            let codes: Vec<&str> = result
                .workflows
                .iter()
                .filter_map(|workflow| workflow.parse_error.as_ref())
                .map(|error| error.code.as_str())
                .collect();
            assert_eq!(codes, vec!["invalid_yaml"], "{name}");
        }
    }

    #[test]
    fn the_act_table_parser_reads_act_own_columns() {
        let view = parse_act_list(
            "Stage  Job       Dependencies  Workflow\n\
             -      build     -             .github/workflows/ci.yml\n\
             -      lint      -             ci.yml\n\
             -      deploy    build         coverage.yaml\n\
             -      -         -             empty.yml\n\
             some warning from act\n",
        );
        assert_eq!(
            view.loaded,
            vec![
                ".github/workflows/ci.yml",
                ".github/workflows/coverage.yaml",
            ],
            "{view:?}"
        );
        assert_eq!(view.jobs, vec!["build", "lint", "deploy"]);
    }

    #[test]
    fn clamping_drops_whole_elements_and_says_so() {
        let values = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        let mut warnings = Warnings::default();
        assert_eq!(clamp_list(&values, 8, &mut warnings), values);
        assert_eq!(clamp_list(&values, 2, &mut warnings), vec!["a", "b"]);
        assert_eq!(warnings.finish(), vec![WARNING_TRUNCATED.to_owned()]);
    }

    fn call(repo: &crate::fixtures::TestRepo) -> ListTasksResult {
        list_tasks(&config_for(repo), &ListTasksArgs::default()).expect("list_tasks must succeed")
    }
}
