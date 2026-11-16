//! `run_status` — "where is my run, and how much of the log can I see?".
//!
//! The polling half of ADR-0004 option B (§10). Two modes, one tool: with `run_id` it answers
//! that run's [`run_view`](https://…/run_view), without it a bounded, newest-first listing so a
//! caller that lost its id can still find its way back. Both answers come **from memory**:
//! `run_status` never blocks, never starts anything, and never reads `act.jsonl` or
//! `.evidence/runs/` off disk (§7.1, §7.4). That is what makes a 1 s poll cheap enough to be the
//! progress channel of a server that deliberately has no streams in v1.
//!
//! Three rules this tool lives or dies by:
//!
//! * **the state machine is the only truth.** `status`/`phase`/`terminal` are projections of the
//!   `RunState` the run task published (§17.1); nothing here infers state from timestamps.
//! * **the log tail is capped in bytes, whole lines from the end, and redacted at the source** —
//!   so the answer cannot leak a secret or a host path (ADR-0001 §2.8, §5.2).
//! * **an unknown `run_id` is `run_not_found`, not an empty run** (§7.4). The durable trace is
//!   `.evidence/runs/<run_id>/`, and this tool says so rather than reading it: mixing "live and
//!   cancellable" with "happened once" is exactly the ambiguity §7.4 refuses. An empty *listing*,
//!   by contrast, is a success: `{runs: [], active_count: 0, truncated: false}`.

use serde_json::{json, Value};

use rmcp::model::JsonObject;

use crate::{
    registry::{RunNotFound, RunSupervisor},
    runner::RunId,
    tools::{schemas, ArgumentProblem, ToolError},
};

/// The tool's name.
pub const TOOL: &str = "run_status";

/// `tools/list` title.
pub const TITLE: &str = "Poll a run, or list this server's recent runs";

/// The default `log_tail_bytes` (ADR-0001 §9).
pub const DEFAULT_LOG_TAIL_BYTES: usize = 4096;
/// The default and maximum listing size (ADR-0001 §9: 50 newest first).
pub const DEFAULT_LIMIT: usize = crate::registry::LISTING_CAP;
/// The `expected` prose for a bad `limit`, which is derived once so the three places that
/// need it agree.
const LISTING_EXPECTED: &str = "an integer 1..=50 (the listing cap)";

/// The advertised description; part of the contract (ADR-0001 §1).
pub const DESCRIPTION: &str = "Poll a run: `run_status(run_id)` returns that run's `run_view` - \
`status` (pending | running | success | failed | cancelled | timed_out | error), `terminal`, the \
display-only `phase` (queued, preparing_tree, starting_act, running, cancelling, \
extracting_evidence, publishing_evidence, done), timestamps, `duration_ms`, `exit_code`, \
`evidence_files`, `warnings`, the terminal `error` when there is one, and a redacted `log_tail` \
capped by `log_tail_bytes` (0 is the cheap poll: status only). With no `run_id` it returns a \
bounded listing instead - `{runs: [...], active_count, truncated}` newest first, at most 50, no \
log or evidence detail - which is how a caller recovers a lost `run_id`. Never blocks, never \
starts work, never reads disk: everything is answered from in-memory run state, so after a server \
restart the listing is empty and any older `run_id` answers `run_not_found`; that run's durable \
record is still on disk under `.evidence/runs/<run_id>/`. Errors: `invalid_arguments`, \
`unknown_argument`, `run_not_found`.";

/// The argument names, in schema order.
pub const ARGUMENT_NAMES: [&str; 3] = ["run_id", "log_tail_bytes", "limit"];

/// Validated `run_status` arguments (parsed by hand — see [`crate::tools`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunStatusArgs {
    /// The run to report; `None` selects listing mode.
    pub run_id: Option<RunId>,
    /// How many bytes of log tail to return (0 is legal: status only).
    pub log_tail_bytes: usize,
    /// Listing mode only: how many runs to return.
    pub limit: usize,
}

impl Default for RunStatusArgs {
    fn default() -> Self {
        Self {
            run_id: None,
            log_tail_bytes: DEFAULT_LOG_TAIL_BYTES,
            limit: DEFAULT_LIMIT,
        }
    }
}

impl RunStatusArgs {
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
        let mut args = Self::default();
        let mut problems = Vec::new();

        match arguments.get("run_id") {
            None | Some(Value::Null) => {}
            Some(Value::String(text)) => match RunId::parse(text) {
                Ok(run_id) => args.run_id = Some(run_id),
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

        match arguments.get("log_tail_bytes") {
            None | Some(Value::Null) => {}
            Some(Value::Number(number)) => match number.as_u64() {
                Some(value) if value <= crate::runner::MAX_LOG_TAIL_BYTES as u64 => {
                    // `as usize` is safe: the bound above is the platform-independent 65536.
                    args.log_tail_bytes = value as usize;
                }
                Some(value) => problems.push(ArgumentProblem {
                    path: "log_tail_bytes",
                    reason: format!("{value} exceeds the 65536 byte bound"),
                    expected: "0..=65536 (0 returns no log at all)",
                }),
                None => problems.push(ArgumentProblem {
                    path: "log_tail_bytes",
                    reason: "not a non-negative integer".to_owned(),
                    expected: "0..=65536",
                }),
            },
            Some(other) => problems.push(ArgumentProblem {
                path: "log_tail_bytes",
                reason: format!("expected an integer, got {}", kind_of(other)),
                expected: "0..=65536",
            }),
        }

        match arguments.get("limit") {
            None | Some(Value::Null) => {}
            Some(Value::Number(number)) => match number.as_u64() {
                Some(value) if (1..=crate::registry::LISTING_CAP as u64).contains(&value) => {
                    args.limit = value as usize;
                }
                Some(value) => problems.push(ArgumentProblem {
                    path: "limit",
                    reason: format!("{value} is outside the listing bound"),
                    expected: LISTING_EXPECTED,
                }),
                None => problems.push(ArgumentProblem {
                    path: "limit",
                    reason: "not a positive integer".to_owned(),
                    expected: LISTING_EXPECTED,
                }),
            },
            Some(other) => problems.push(ArgumentProblem {
                path: "limit",
                reason: format!("expected an integer, got {}", kind_of(other)),
                expected: LISTING_EXPECTED,
            }),
        }

        if !problems.is_empty() {
            return Err(ToolError::invalid_arguments(TOOL, problems));
        }
        Ok(args)
    }
}

/// Run the tool against a supervisor.
///
/// # Errors
///
/// [`RunNotFound`] for a run this process does not know; the caller maps it with
/// [`run_not_found`].
pub async fn run(supervisor: &RunSupervisor, args: &RunStatusArgs) -> Result<Value, RunNotFound> {
    match &args.run_id {
        Some(run_id) => {
            let view = supervisor
                .status(run_id.as_str(), args.log_tail_bytes)
                .await?;
            let active = supervisor.active_count().await;
            Ok(json!({
                "run": view.to_value(),
                "active_count": active,
                // A single-run answer is never a truncated page (§7.3 is about the listing).
                "truncated": false,
            }))
        }
        None => {
            let listing = supervisor.listing(args.limit).await;
            Ok(json!({
                "runs": listing
                    .runs
                    .iter()
                    .map(crate::registry::RunSummary::to_value)
                    .collect::<Vec<_>>(),
                "active_count": listing.active_count,
                "truncated": listing.truncated,
            }))
        }
    }
}

/// The `run_not_found` error for a `run_status` call (ADR-0004 §7.4: `isError: true`,
/// `retryable: false`, `detail: {run_id, known_runs}`).
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

/// The advertised `inputSchema` (the resolved `$defs/run_status_args`).
#[must_use]
pub fn input_schema() -> std::sync::Arc<JsonObject> {
    let schema = json!({
        "description": "tools/call params.arguments for run_status. With run_id: that run. Without \
                        it: the bounded recent/active listing.",
        "type": "object",
        "additionalProperties": false,
        "required": [],
        "properties": {
            "run_id": schemas::run_id(),
            "log_tail_bytes": schemas::log_tail_bytes(),
            "limit": {
                "description": "Listing mode only: how many runs to return, newest first.",
                "type": "integer",
                "minimum": 1,
                "maximum": 50,
                "default": 50
            }
        }
    });
    to_object(schema)
}

/// The advertised `outputSchema` (the resolved `$defs/run_status_result`).
#[must_use]
pub fn output_schema() -> std::sync::Arc<JsonObject> {
    let schema = json!({
        "description": "structuredContent for run_status. Exactly one of run / runs is present, and \
                        the server does not validate that for you beyond the schemas: run iff \
                        run_id was supplied.",
        "type": "object",
        "additionalProperties": false,
        "required": ["active_count", "truncated"],
        "properties": {
            "run": schemas::run_view(),
            "runs": {
                "type": "array",
                "maxItems": 50,
                "items": schemas::run_summary()
            },
            "active_count": {
                "description": "Runs currently non-terminal in this server process, whether or not \
                                they are in the returned page.",
                "type": "integer",
                "minimum": 0,
                "maximum": 1024
            },
            "truncated": {
                "description": "True when older finished runs were dropped from the page (retention \
                                window from ADR-0004).",
                "type": "boolean"
            }
        },
        "oneOf": [
            { "required": ["run"], "not": { "required": ["runs"] } },
            { "required": ["runs"], "not": { "required": ["run"] } }
        ]
    });
    to_object(schema)
}

fn to_object(schema: Value) -> std::sync::Arc<JsonObject> {
    std::sync::Arc::new(
        schema
            .as_object()
            .expect("the literal run_status schema is an object")
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
    use serde_json::json;

    fn arguments(value: Value) -> JsonObject {
        value.as_object().expect("object arguments").clone()
    }

    #[test]
    fn defaults_are_the_contracts_defaults() {
        let args = RunStatusArgs::parse(None).expect("no arguments is legal");
        assert_eq!(args.run_id, None);
        assert_eq!(args.log_tail_bytes, DEFAULT_LOG_TAIL_BYTES);
        assert_eq!(args.limit, DEFAULT_LIMIT);
    }

    #[test]
    fn a_malformed_run_id_is_invalid_arguments_naming_the_shape() {
        let error = RunStatusArgs::parse(Some(&arguments(json!({"run_id": "12"})))).unwrap_err();
        assert_eq!(error.code(), "invalid_arguments");
        let payload = error.structured_content();
        assert_eq!(payload["error"]["detail"]["errors"][0]["path"], "run_id");
        assert!(payload["error"]["detail"]["errors"][0]["expected"]
            .as_str()
            .unwrap()
            .contains("YYYYMMDDTHHMMSSZ"));
    }

    #[test]
    fn bounds_are_enforced_not_clamped() {
        for (value, path) in [
            (json!({"log_tail_bytes": 65537}), "log_tail_bytes"),
            (json!({"log_tail_bytes": -1}), "log_tail_bytes"),
            (json!({"limit": 51}), "limit"),
            (json!({"limit": 0}), "limit"),
            (json!({"limit": "50"}), "limit"),
        ] {
            let error = RunStatusArgs::parse(Some(&arguments(value))).unwrap_err();
            assert_eq!(error.code(), "invalid_arguments");
            assert_eq!(
                error.structured_content()["error"]["detail"]["errors"][0]["path"],
                path,
                "{path} was not the reported problem"
            );
        }
        // 0 is legal and means "no log at all" — the cheap poll of ADR-0004 §10.
        assert_eq!(
            RunStatusArgs::parse(Some(&arguments(json!({"log_tail_bytes": 0}))))
                .expect("zero tail is the cheap poll")
                .log_tail_bytes,
            0
        );
    }

    #[test]
    fn a_run_id_of_the_right_shape_parses() {
        let args = RunStatusArgs::parse(Some(&arguments(
            json!({"run_id": "20261110T004203Z-a1b2c3", "limit": 3}),
        )))
        .expect("valid");
        assert_eq!(args.run_id.expect("id").as_str(), "20261110T004203Z-a1b2c3");
        assert_eq!(args.limit, 3);
    }
}
