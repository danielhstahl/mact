//! The pinned JSON Schemas, compiled once and shared.
//!
//! ADR-0002 §7 makes the schema file half of the contract, and says an implementation
//! that finds a hole must update *both* halves in the same change. This module is where
//! the two halves meet: the schema is `include_str!`-ed from
//! `schema/evidence.schema.json` (and `schema/run-meta.schema.json`) so
//!
//! * `act-mcp validate-evidence` is standalone — it validates a workspace on any
//!   machine, without a checkout of act-mcp next to it, while still checking against
//!   the *same bytes* CI checks, and
//! * the writer's debug-build check and the CLI's check cannot be two different
//!   schemas by accident.
//!
//! `--schema <path>` overrides the embedded copy (see [`compile`]) for the case where
//! someone is evolving the schema and wants to check data against the draft.

use std::sync::OnceLock;

use jsonschema::Validator;
use serde_json::Value;
use thiserror::Error;

/// `schema/evidence.schema.json`, embedded at compile time. One line of that file is
/// one line of this program's contract.
pub const EVIDENCE_SCHEMA_JSON: &str = include_str!("../../schema/evidence.schema.json");

/// `schema/run-meta.schema.json`, embedded for the same reason. `meta.json` is audit
/// context rather than the load-bearing contract (ADR-0002 §1.3), but a manifest that
/// does not validate is still a bug worth a non-zero exit.
pub const RUN_META_SCHEMA_JSON: &str = include_str!("../../schema/run-meta.schema.json");

/// Why a schema could not be used.
#[allow(missing_docs, reason = "the #[error] string documents each variant")]
#[derive(Debug, Error)]
pub enum SchemaError {
    #[error("could not parse the schema as JSON: {0}")]
    Parse(serde_json::Error),
    #[error("not a usable 2020-12 schema: {0}")]
    Compile(String),
}

/// Compile schema text into a validator.
///
/// # Errors
///
/// [`SchemaError::Parse`] when the text is not JSON, [`SchemaError::Compile`] when it
/// is JSON but not a schema this engine can use.
pub fn compile(text: &str) -> Result<Validator, SchemaError> {
    let value: Value = serde_json::from_str(text).map_err(SchemaError::Parse)?;
    jsonschema::validator_for(&value).map_err(|err| SchemaError::Compile(err.to_string()))
}

/// The embedded record schema, compiled on first use and shared ever after.
///
/// Compilation is the expensive half of schema validation (the regexes, the `$ref`
/// resolution, the `if`/`then` graph), and both the writer's debug check and the CLI
/// want the same compiled form, so it lives in a `OnceLock` rather than being rebuilt
/// per file.
///
/// # Errors
///
/// [`SchemaError`] if the embedded schema is broken — which means somebody edited
/// `schema/evidence.schema.json` into an invalid schema, and every consumer of it
/// should fail loudly rather than skip the check.
pub fn evidence_validator() -> Result<&'static Validator, SchemaError> {
    static VALIDATOR: OnceLock<Result<Validator, String>> = OnceLock::new();
    VALIDATOR
        .get_or_init(|| compile(EVIDENCE_SCHEMA_JSON).map_err(|err| err.to_string()))
        .as_ref()
        .map_err(|reason| SchemaError::Compile(reason.clone()))
}

/// The embedded run-manifest schema, compiled on first use.
///
/// # Errors
///
/// As [`evidence_validator`].
pub fn run_meta_validator() -> Result<&'static Validator, SchemaError> {
    static VALIDATOR: OnceLock<Result<Validator, String>> = OnceLock::new();
    VALIDATOR
        .get_or_init(|| compile(RUN_META_SCHEMA_JSON).map_err(|err| err.to_string()))
        .as_ref()
        .map_err(|reason| SchemaError::Compile(reason.clone()))
}

/// One complaint from the schema engine: where in the instance it points, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaProblem {
    /// The instance path, `/`-joined the way `scripts/validate_evidence.py` prints it
    /// (`<root>` for the document itself).
    pub path: String,
    /// The engine's message, verbatim.
    pub message: String,
}

impl std::fmt::Display for SchemaProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

/// Every way `instance` fails `validator`, in a stable order.
///
/// Sorted by (path, message) rather than in engine order: the engine's order depends
/// on the compiled graph, and a checker whose output shuffles between runs is a checker
/// nobody diffs.
#[must_use]
pub fn problems(validator: &Validator, instance: &Value) -> Vec<SchemaProblem> {
    let mut found: Vec<SchemaProblem> = validator
        .iter_errors(instance)
        .map(|error| SchemaProblem {
            path: instance_path(&error),
            message: error.to_string(),
        })
        .collect();
    found.sort_unstable_by(|a, b| (&a.path, &a.message).cmp(&(&b.path, &b.message)));
    found
}

/// The instance path of an error, spelled like the Python checker's
/// `"/".join(err.absolute_path)`.
fn instance_path(error: &jsonschema::ValidationError<'_>) -> String {
    let mut path = String::new();
    for segment in error.instance_path() {
        if !path.is_empty() {
            path.push('/');
        }
        path.push_str(&segment.to_string());
    }
    if path.is_empty() {
        return "<root>".to_owned();
    }
    path
}

/// The fast path the writer uses: is this record valid against the pinned schema, and
/// if not, why in one line?
///
/// # Errors
///
/// A single sentence naming the first problem, ready to embed in
/// [`super::writer::EvidenceError`].
pub fn validate_record(instance: &Value) -> Result<(), String> {
    validate_against(
        evidence_validator().map_err(|err| err.to_string())?,
        instance,
    )
}

/// `validate_record`, with an explicitly supplied validator (what `--schema` uses).
///
/// # Errors
///
/// As [`validate_record`].
pub fn validate_against(validator: &Validator, instance: &Value) -> Result<(), String> {
    problems(validator, instance)
        .first()
        .map_or(Ok(()), |problem| Err(problem.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_embedded_schemas_compile() {
        // If this fails, `schema/evidence.schema.json` was edited into something the
        // engine refuses — which is exactly the drift ADR-0002 §7 forbids.
        assert!(evidence_validator().is_ok());
        assert!(run_meta_validator().is_ok());
    }

    #[test]
    fn a_record_without_a_required_field_is_reported_with_its_path() {
        let validator = evidence_validator().expect("the embedded schema compiles");
        let record = json!({
            "schema_version": 1,
            "kind": "benchmark",
            "run_id": "20261110T004203Z-a1b2c3",
            "commit": "2f7c1a4b9e0d3c6a5b8f1e2d4c6a8b0f3e5d7c91",
            "workflow": "benchmark",
            "category": "benchmark",
            "run_status": "success",
            "tool": {"name": "act-mcp", "version": "0.1.0"},
            "started_at": "2026-11-10T00:42:03.417Z",
            "host": "build-box-07",
            "name": "x",
            "value": 1,
            "unit": "ms",
            "source": "act_log"
        });
        let found = problems(validator, &record);
        assert!(
            found.iter().any(|problem| problem.message.contains("task")),
            "expected a complaint about the missing `task`, got {found:?}"
        );
    }
}
