//! `cancel_run` — the stop button, and the only control tool that may block.
//!
//! ADR-0004 §5.2 pins what happens for each state, and the shape of that table is worth
//! stating before the code: **a cancel is a request, not a write.** The tool records the
//! request and asks the runner to tear the process group down; the run's own task performs
//! the state changes (§3.5). The block is bounded by `--cancel-wait-secs` (default 30 s, never
//! more than 300 s, always ≥ `--term-grace-secs` so the common case returns a terminal
//! receipt rather than a promise), and a *second* caller never blocks at all: the first
//! caller owns the teardown.
//!
//! The five rows that matter, all of them non-errors except `run_not_found`:
//!
//! | state at arrival | the receipt |
//! |--- |--- |
//! | `Queued`/`PreparingTree`/`ActStarting` | `cancelled`, terminal, `kill: {"method": "none"}` — nothing was ever spawned |
//! | `ActRunning` | `cancelled` + `kill: {method, took_ms}` once reaped; `cancelled`/`terminal: false` if the teardown outlived `--cancel-wait-secs` |
//! | `Cancelling` | `cancelled`/`terminal: false`, no `kill`, the *first* request's `cancel_requested_at` |
//! | `Harvesting`/`Publishing` | no-op: current `status`, `terminal: false`, warning `already_terminal` — act already exited and the evidence is determined (§5.4) |
//! | any terminal state | `already_terminal: true` with the original status, no timestamp, no kill (T20) |
//!
//! The mid-kill run reads differently through the two tools on purpose (§16.1): `run_status`
//! says `running`/`cancelling`/`terminal: false` because it is the polling surface, and this
//! says `cancelled`/`terminal: false` because it is the answer to "did *my* cancel take?".

use std::sync::Arc;

use rmcp::model::JsonObject;
use serde_json::{json, Value};

use crate::{
    registry::{CancelReceipt, RunNotFound, RunSupervisor},
    runner::RunId,
    tools::{schemas, ArgumentProblem, ToolError},
};

/// The tool's name.
pub const TOOL: &str = "cancel_run";

/// `tools/list` title.
pub const TITLE: &str = "Cancel a run by id (idempotent)";

/// The advertised description; part of the contract (ADR-0001 §1).
pub const DESCRIPTION: &str =
    "Stop a run by `run_id`. Idempotent and never an error for a run that \
is already finished: a terminal run answers `already_terminal: true` with its original status. \
Cancelling a run whose act process is alive signals its whole process group (SIGTERM, then \
SIGKILL after `--term-grace-secs`) so act's own cleanup runs and its containers go with it, and \
the receipt reports how it died as `kill: {method: term|kill, took_ms}`; a run cancelled before \
act was spawned reports `kill: {method: \"none\"}`. The call blocks up to `--cancel-wait-secs` \
(30 s by default) for the terminal commit, then reports `terminal: false` and leaves you to poll \
with `run_status`. A cancel arriving after act exited is a no-op reported as `already_terminal`, \
because the outcome and the evidence were already determined. Note the asymmetry in what a \
cancelled run leaves behind: a caller cancel **skips evidence extraction** (you asked for the \
measurement to stop, so nothing is published under `.evidence/` and its `evidence_files` is \
empty), while a `timed_out` run publishes what it collected - keep a slow run's numbers by \
raising `timeout_secs` instead. Errors: \
`invalid_arguments`, `unknown_argument`, `run_not_found`.";

/// The argument names.
pub const ARGUMENT_NAMES: [&str; 1] = ["run_id"];

/// Validated `cancel_run` arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelRunArgs {
    /// The run to stop.
    pub run_id: RunId,
}

impl CancelRunArgs {
    /// Validate `params.arguments`.
    ///
    /// # Errors
    ///
    /// [`ToolError`] (`unknown_argument` / `invalid_arguments`) for anything the contract does
    /// not define.
    pub fn parse(arguments: Option<&JsonObject>) -> Result<Self, ToolError> {
        crate::tools::reject_unknown_arguments(TOOL, arguments, &ARGUMENT_NAMES)?;
        let empty = JsonObject::new();
        let arguments = arguments.unwrap_or(&empty);
        let mut problems = Vec::new();
        let mut run_id = None;
        match arguments.get("run_id") {
            None => problems.push(ArgumentProblem {
                path: "run_id",
                reason: "the argument is required".to_owned(),
                expected: "a run_id as returned by run_workflow",
            }),
            Some(Value::String(text)) => match RunId::parse(text) {
                Ok(parsed) => run_id = Some(parsed),
                Err(_) => problems.push(ArgumentProblem {
                    path: "run_id",
                    reason: format!("`{text}` is not a run id"),
                    expected: "YYYYMMDDTHHMMSSZ-<6 lowercase hex>, as returned by run_workflow",
                }),
            },
            Some(other) => problems.push(ArgumentProblem {
                path: "run_id",
                reason: format!("expected a string, got {}", kind_of(other)),
                expected: "a run_id string",
            }),
        }
        if !problems.is_empty() {
            return Err(ToolError::invalid_arguments(TOOL, problems));
        }
        Ok(Self {
            // The only way `run_id` is `None` here is the `None` arm above, which pushed a
            // problem and returned already.
            run_id: run_id.expect("validated above"),
        })
    }
}

/// Run the tool against a supervisor.
///
/// # Errors
///
/// [`RunNotFound`] — the only error this tool has; everything else is an idempotent receipt.
pub async fn run(
    supervisor: &RunSupervisor,
    args: &CancelRunArgs,
) -> Result<CancelReceipt, RunNotFound> {
    supervisor.cancel(args.run_id.as_str()).await
}

/// The `run_not_found` error for a `cancel_run` call (§7.4).
#[must_use]
pub fn run_not_found(not_found: &RunNotFound) -> ToolError {
    ToolError::new(
        TOOL,
        "run_not_found",
        not_found.message(),
        false,
        not_found.detail(),
    )
}

/// The advertised `inputSchema` (the resolved `$defs/cancel_run_args`).
#[must_use]
pub fn input_schema() -> Arc<JsonObject> {
    let schema = json!({
        "description": "tools/call params.arguments for cancel_run.",
        "type": "object",
        "additionalProperties": false,
        "required": ["run_id"],
        "properties": { "run_id": schemas::run_id() }
    });
    Arc::new(
        schema
            .as_object()
            .expect("the literal cancel_run schema is an object")
            .clone(),
    )
}

/// The advertised `outputSchema` (the resolved `$defs/cancel_run_result`).
///
/// The two `allOf` rules are the schema's way of enforcing §16.2: `already_terminal: true`
/// forbids `cancel_requested_at` and `kill`, and `already_terminal: false` requires the
/// timestamp.
#[must_use]
pub fn output_schema() -> Arc<JsonObject> {
    let schema = json!({
        "description": "structuredContent for cancel_run. Cancelling a terminal run is NOT an \
                        error: cancel is idempotent and reports what was already true.",
        "type": "object",
        "additionalProperties": false,
        "required": ["run_id", "status", "terminal", "already_terminal"],
        "properties": {
            "run_id": schemas::run_id(),
            "status": schemas::status(),
            "terminal": { "type": "boolean" },
            "already_terminal": {
                "description": "True when the run was already terminal before this call; \
                                cancel_requested_at is then absent and status is the old terminal \
                                status, not cancelled.",
                "type": "boolean"
            },
            "cancel_requested_at": schemas::timestamp(),
            "kill": {
                "description": "How the act process group was taken down (ADR-0004 owns the \
                                mechanics). method 'none' for a run cancelled before act was \
                                exec'd; this object is absent only for an already_terminal run \
                                (ADR-0004 section 16.2).",
                "type": "object",
                "additionalProperties": false,
                "required": ["method"],
                "properties": {
                    "method": { "enum": ["term", "kill", "none"] },
                    "took_ms": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 300000
                    }
                }
            },
            "warnings": schemas::warnings()
        },
        "allOf": [
            {
                "if": {
                    "properties": { "already_terminal": { "const": true } },
                    "required": ["already_terminal"]
                },
                "then": {
                    "not": {
                        "anyOf": [
                            { "required": ["cancel_requested_at"] },
                            { "required": ["kill"] }
                        ]
                    }
                }
            },
            {
                "if": {
                    "properties": { "already_terminal": { "const": false } },
                    "required": ["already_terminal"]
                },
                "then": { "required": ["cancel_requested_at"] }
            }
        ]
    });
    Arc::new(
        schema
            .as_object()
            .expect("the literal cancel_run schema is an object")
            .clone(),
    )
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(value: Value) -> JsonObject {
        value.as_object().expect("object arguments").clone()
    }

    #[test]
    fn run_id_is_required_and_shape_checked() {
        let missing = CancelRunArgs::parse(Some(&arguments(json!({})))).unwrap_err();
        assert_eq!(missing.code(), "invalid_arguments");
        assert_eq!(
            missing.structured_content()["error"]["detail"]["errors"][0]["path"],
            "run_id"
        );
        let malformed =
            CancelRunArgs::parse(Some(&arguments(json!({"run_id": "yesterday"})))).unwrap_err();
        assert_eq!(malformed.code(), "invalid_arguments");
        assert!(malformed.structured_content()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("run_id"));
    }

    #[test]
    fn an_unknown_argument_is_rejected_before_anything_else() {
        let error = CancelRunArgs::parse(Some(&arguments(json!({
            "run_id": "20261110T004203Z-a1b2c3",
            "force": true
        }))))
        .unwrap_err();
        assert_eq!(error.code(), "unknown_argument");
        assert_eq!(
            error.structured_content()["error"]["detail"]["unexpected"],
            json!(["force"])
        );
    }

    #[test]
    fn a_valid_run_id_parses() {
        let args = CancelRunArgs::parse(Some(&arguments(
            json!({"run_id": "20261110T004203Z-a1b2c3"}),
        )))
        .expect("valid");
        assert_eq!(args.run_id.as_str(), "20261110T004203Z-a1b2c3");
    }
}
