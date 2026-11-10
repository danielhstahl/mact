//! Plumbing shared by every tool: the ADR-0001 §5.2 error payload, and the
//! argument validation the contract pins.
//!
//! ## Why arguments are validated by hand
//!
//! `rmcp` can deserialize `params.arguments` into a struct for you, and this server
//! deliberately does not. A failed serde deserialization is an *rmcp* failure, which
//! surfaces as the JSON-RPC protocol error `-32602`; ADR-0001 §5.1 pins the opposite
//! for this contract — bad tool arguments are a **tool execution error**:
//! `isError: true`, `structuredContent: {"error": {…}}`, `code` ∈
//! `invalid_arguments | unknown_argument`, because that is the shape an agent can
//! read and self-correct from in one round trip. `-32602` is reserved for a bad
//! `params` envelope or an unknown tool name, which is rmcp's to report and is left
//! alone here.
//!
//! So a tool handler takes the arguments as a raw map and calls into this module:
//! [`ToolError::unknown_argument`] for a key the tool does not define (including a
//! caller-supplied `run_id`, which ADR-0001 §8 makes an error rather than a request)
//! and [`ToolError::invalid_arguments`] with one `detail.errors[]` entry per
//! offending key. Unknown keys are reported *and* the known ones still get
//! validated, so a call with two mistakes gets both back instead of fixing them one
//! round trip at a time.
//!
//! ## The error payload
//!
//! [`ToolError`] is the only way a tool in this crate fails. It renders
//! ADR-0001 §5.2 exactly: the text block's first two tokens are `<tool>: <code>:`,
//! and `structuredContent` is exactly `{"error": {code, message, retryable, detail}}`
//! — never the tool's own `outputSchema` shape, which clients only validate on
//! `isError: false`.

pub mod cancel_run;
pub mod list_tasks;
pub mod run_status;
pub mod schemas;

use std::collections::BTreeSet;

use rmcp::{
    handler::server::tool::IntoCallToolResult,
    model::{CallToolResponse, CallToolResult, ContentBlock, JsonObject},
};
use serde_json::{json, Map, Value};

use crate::workspace::ValidationError;

/// One `detail.errors[]` entry (ADR-0001 §5.3's `invalid_arguments` detail).
#[derive(Debug, Clone)]
pub struct ArgumentProblem {
    /// Argument name, e.g. `include_steps`.
    pub path: &'static str,
    /// What is wrong with it.
    pub reason: String,
    /// What it should have been, spelled as the schema says.
    pub expected: &'static str,
}

impl ArgumentProblem {
    /// A problem with the whole argument object rather than one key (e.g. arguments
    /// that are not an object at all).
    pub const ROOT: &'static str = "";

    fn to_value(&self) -> Value {
        json!({ "path": self.path, "reason": self.reason, "expected": self.expected })
    }
}

/// A tool execution error: `isError: true` plus the ADR-0001 §5.2 payload.
///
/// The `Display` of the underlying failure never reaches the wire verbatim; `message`
/// is bounded (ADR-0001 §9) and carries no absolute host path, which is why the
/// constructors take already-safe text.
#[derive(Debug, Clone)]
pub struct ToolError {
    /// Tool name, used for the `<tool>: <code>:` text prefix.
    tool: &'static str,
    /// The `structuredContent.error` object.
    error: Map<String, Value>,
}

impl ToolError {
    /// Build the payload from its four members.
    pub fn new(
        tool: &'static str,
        code: &'static str,
        message: impl Into<String>,
        retryable: bool,
        detail: Map<String, Value>,
    ) -> Self {
        let error = json!({
            "code": code,
            "message": crate::workspace::truncate_bytes(&message.into(), crate::workspace::MAX_ERROR_MESSAGE_BYTES),
            "retryable": retryable,
            "detail": Value::Object(detail),
        });
        Self {
            tool,
            error: error
                .as_object()
                .expect("json! literal is an object")
                .clone(),
        }
    }

    /// Lift an [`ValidationError`] — the validator's codes *are* ADR-0001 §5.3's, so
    /// a tool that runs the gate adds nothing but the tool name.
    pub fn from_validation(tool: &'static str, error: &ValidationError) -> Self {
        Self {
            tool,
            error: error.to_error_value().as_object().expect("object").clone(),
        }
    }

    /// `invalid_arguments`: one `detail.errors[]` entry per problem.
    pub fn invalid_arguments(tool: &'static str, problems: Vec<ArgumentProblem>) -> Self {
        let mut detail = Map::new();
        detail.insert(
            "errors".to_owned(),
            Value::Array(
                problems
                    .iter()
                    .map(ArgumentProblem::to_value)
                    .collect::<Vec<_>>(),
            ),
        );
        let summary = problems
            .first()
            .map(|problem| {
                format!(
                    "argument `{}` is not valid: {}",
                    problem.path, problem.reason
                )
            })
            .unwrap_or_else(|| "arguments are not valid".to_owned());
        Self::new(tool, "invalid_arguments", summary, false, detail)
    }

    /// `unknown_argument`: keys the tool does not define, with the keys it does.
    pub fn unknown_argument(tool: &'static str, unexpected: &[String], expected: &[&str]) -> Self {
        let mut detail = Map::new();
        detail.insert(
            "unexpected".to_owned(),
            Value::Array(
                unexpected
                    .iter()
                    .map(|key| Value::String(key.clone()))
                    .collect(),
            ),
        );
        detail.insert(
            "expected".to_owned(),
            Value::Array(
                expected
                    .iter()
                    .map(|name| Value::String((*name).to_owned()))
                    .collect(),
            ),
        );
        Self::new(
            tool,
            "unknown_argument",
            format!("{tool} does not define {}", quoted_list(unexpected)),
            false,
            detail,
        )
    }

    /// The `error.code`, for logs and tests.
    pub fn code(&self) -> &str {
        self.error
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or_default()
    }

    /// The complete `structuredContent`: `{"error": {…}}` and nothing else.
    pub fn structured_content(&self) -> Value {
        json!({ "error": Value::Object(self.error.clone()) })
    }

    /// The `isError: true` result: ADR-0001 §5.2's fixed text prefix, then the
    /// payload as `structuredContent`.
    pub fn to_result(&self) -> CallToolResult {
        let message = self
            .error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let code = self.code();
        let mut result = CallToolResult::structured_error(self.structured_content());
        // `structured_error` fills `content` with the raw JSON; the contract wants
        // the machine-readable `<tool>: <code>:` sentence first instead, so a client
        // that ignores structuredContent still gets the code in the right order.
        result.content = vec![ContentBlock::text(format!(
            "{}: {code}: {message}",
            self.tool
        ))];
        result
    }
}

impl IntoCallToolResult for ToolError {
    fn into_call_tool_result(self) -> Result<CallToolResponse, rmcp::ErrorData> {
        Ok(self.to_result().into())
    }
}

/// The keys of `arguments`, sorted — the shape every handler validates against.
pub fn argument_keys(arguments: Option<&JsonObject>) -> BTreeSet<String> {
    arguments
        .map(|map| map.keys().cloned().collect())
        .unwrap_or_default()
}

/// Reject keys the tool does not define (`unknown_argument`).
///
/// `expected` is advertised in `detail`, because the point of the error is that the
/// caller can fix it without a documentation lookup.
pub fn reject_unknown_arguments(
    tool: &'static str,
    arguments: Option<&JsonObject>,
    expected: &[&str],
) -> Result<(), ToolError> {
    let unexpected: Vec<String> = argument_keys(arguments)
        .into_iter()
        .filter(|key| !expected.contains(&key.as_str()))
        .collect();
    if unexpected.is_empty() {
        Ok(())
    } else {
        Err(ToolError::unknown_argument(tool, &unexpected, expected))
    }
}

/// Render a list as ``"a", "b" or "c`` for error prose.
fn quoted_list(values: &[String]) -> String {
    match values.len() {
        0 => "no arguments".to_owned(),
        1 => format!("`{}`", values[0]),
        _ => {
            let (head, tail) = values.split_at(values.len() - 1);
            let head = head
                .iter()
                .map(|value| format!("`{value}`"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{head} or `{}`", tail[0])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_list_reads_like_prose() {
        let one = vec!["run_id".to_owned()];
        assert_eq!(quoted_list(&one), "`run_id`");
        let two = vec!["run_id".to_owned(), "path".to_owned()];
        assert_eq!(quoted_list(&two), "`run_id` or `path`");
        let three = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        assert_eq!(quoted_list(&three), "`a`, `b` or `c`");
    }

    #[test]
    fn unknown_argument_names_the_keys_and_the_options() {
        let error = ToolError::unknown_argument(
            "list_tasks",
            &["run_id".to_owned()],
            &["workflow", "include_steps"],
        );
        assert_eq!(error.code(), "unknown_argument");
        let payload = error.structured_content();
        assert_eq!(payload["error"]["code"], json!("unknown_argument"));
        assert_eq!(payload["error"]["retryable"], json!(false));
        assert_eq!(payload["error"]["detail"]["unexpected"], json!(["run_id"]));
        assert_eq!(
            payload["error"]["detail"]["expected"],
            json!(["workflow", "include_steps"])
        );
        assert!(payload["error"]["message"]
            .as_str()
            .unwrap()
            .contains("`run_id`"));
        let result = error.to_result();
        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result.content[0].as_text().expect("text").text,
            "list_tasks: unknown_argument: list_tasks does not define `run_id`"
        );
    }

    #[test]
    fn invalid_arguments_carries_one_entry_per_problem() {
        let error = ToolError::invalid_arguments(
            "list_tasks",
            vec![
                ArgumentProblem {
                    path: "workflow",
                    reason: "not a string".to_owned(),
                    expected: "a workflow name",
                },
                ArgumentProblem {
                    path: "include_steps",
                    reason: "not a boolean".to_owned(),
                    expected: "true or false",
                },
            ],
        );
        let payload = error.structured_content();
        let errors = payload["error"]["detail"]["errors"]
            .as_array()
            .expect("array");
        assert_eq!(errors.len(), 2);
        assert_eq!(errors[1]["path"], json!("include_steps"));
    }
}
