//! The Rust-side half of ADR-0001 §10: the schemas the server advertises and the
//! checked-in contract must be the same document.
//!
//! `schema/tool-contract.schema.json` is what a client validates against, and the
//! `outputSchema` in `tools/list` is what the MCP client in front of an agent
//! validates against; if they ever differ, one of them is a lie. The Python checker
//! (`scripts/validate_tool_contract.py`) keeps the schema and the *docs* in step and
//! runs the fixtures against the schema; nothing but this test keeps the schema and
//! the **Rust structs** in step, and ADR-0001's verification table asks for exactly
//! that (the `tool_schema_parity` check).
//!
//! ## What "equal" means here
//!
//! Two documents, normalized:
//!
//! * `$ref`s resolved against the document's own `$defs`, because schemars factors
//!   reused types out and the pinned file hand-writes them inline (and vice versa);
//! * `title` and `description` dropped — prose is prose, and the pinned file's prose
//!   is written for humans (`"ADVISORY best-effort guess so a caller can pick a
//!   benchmark or coverage job…"`), which is not a structural property;
//! * `format` dropped, because `u32` says `"format": "uint32"` while the contract
//!   states the bounds it cares about (`minimum`/`maximum`) and nothing else;
//! * empty `required` arrays dropped, and non-empty ones sorted — `required` is a
//!   set, not an ordering;
//! * `$schema`/`$id`/`$defs` dropped once everything is inlined.
//!
//! Everything that survives normalization is load-bearing: property names, types,
//! `required`, `additionalProperties`, string bounds and patterns, array `maxItems`,
//! `enum` members and `not` constraints. That is what the wire actually promises.

use act_mcp::server::ActMcp;
use serde_json::{json, Map, Value};

/// Load the checked-in contract.
fn contract() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/schema/tool-contract.schema.json"
    );
    let text = std::fs::read_to_string(path).expect("the contract schema is in the tree");
    serde_json::from_str(&text).expect("the contract schema is JSON")
}

/// The tools this build registers, each with the `$defs` its schemas must equal.
/// `(tool name, $defs for its args, $defs for its result, how to render it)`.
type ToolRow = (
    &'static str,
    &'static str,
    &'static str,
    fn() -> rmcp::model::Tool,
);

const TOOLS: [ToolRow; 3] = [
    (
        "list_tasks",
        "list_tasks_args",
        "list_tasks_result",
        ActMcp::list_tasks_tool_attr,
    ),
    (
        "run_status",
        "run_status_args",
        "run_status_result",
        ActMcp::run_status_tool_attr,
    ),
    (
        "cancel_run",
        "cancel_run_args",
        "cancel_run_result",
        ActMcp::cancel_run_tool_attr,
    ),
];

/// The advertised tool `name`, as `tools/list` would render it.
fn advertised(name: &str) -> rmcp::model::Tool {
    TOOLS
        .iter()
        .find(|(tool, ..)| *tool == name)
        .unwrap_or_else(|| panic!("`{name}` is one of the tools under parity"))
        .3()
}

/// The pinned `$defs/<name>`, normalized against the whole document.
fn pinned(name: &str) -> Value {
    let document = contract();
    let definition = document
        .pointer(&format!("/$defs/{name}"))
        .unwrap_or_else(|| panic!("`$defs/{name}` exists in the contract"))
        .clone();
    normalize(&definition, &document)
}

/// The advertised schema for `tool` (`inputSchema` or `outputSchema`), normalized.
fn advertised_schema(name: &str, which: &str) -> Value {
    let tool = advertised(name);
    let raw = match which {
        "input" => Value::Object(tool.input_schema.as_ref().clone()),
        "output" => Value::Object(
            tool.output_schema
                .as_ref()
                .unwrap_or_else(|| panic!("`{name}` advertises an outputSchema"))
                .as_ref()
                .clone(),
        ),
        other => panic!("unknown schema {other}"),
    };
    let root = raw.clone();
    normalize(&raw, &root)
}

/// Resolve refs and drop the prose/noise keys — see the module docs.
fn normalize(value: &Value, root: &Value) -> Value {
    match value {
        Value::Object(map) => {
            if let Some(reference) = map.get("$ref").and_then(Value::as_str) {
                let pointer = reference
                    .strip_prefix('#')
                    .unwrap_or(reference)
                    .replace("~1", "/")
                    .replace("~0", "~");
                let target = root
                    .pointer(if pointer.is_empty() { "/" } else { &pointer })
                    .unwrap_or_else(|| panic!("unresolvable $ref `{reference}`"));
                return normalize(target, root);
            }
            let mut out = Map::new();
            for (key, value) in map {
                if matches!(
                    key.as_str(),
                    "title"
                        | "description"
                        | "format"
                        | "$schema"
                        | "$id"
                        | "$defs"
                        | "x-status"
                        | "x-error-codes"
                        | "x-run-status"
                        | "x-mcp-protocol-version"
                        | "x-nullability"
                ) {
                    continue;
                }
                if key == "required" {
                    let Value::Array(items) = value else {
                        panic!("`required` is an array in both documents");
                    };
                    if items.is_empty() {
                        continue;
                    }
                    let mut names: Vec<String> = items
                        .iter()
                        .map(|item| {
                            item.as_str()
                                .expect("required names are strings")
                                .to_owned()
                        })
                        .collect();
                    names.sort();
                    out.insert(
                        key.clone(),
                        Value::Array(names.into_iter().map(Value::from).collect()),
                    );
                    continue;
                }
                out.insert(key.clone(), normalize(value, root));
            }
            Value::Object(out)
        }
        Value::Array(items) => {
            Value::Array(items.iter().map(|item| normalize(item, root)).collect())
        }
        other => other.clone(),
    }
}

/// Every registered tool's `inputSchema` is `$defs/<tool>_args`, member for member.
#[test]
fn every_input_schema_matches_the_pinned_contract() {
    for (tool, args, ..) in TOOLS {
        assert_eq!(
            advertised_schema(tool, "input"),
            pinned(args),
            "`{tool}` inputSchema drifted from $defs/{args}"
        );
    }
}

/// Every registered tool's `outputSchema` is `$defs/<tool>_result`, member for member.
///
/// This is the check that keeps the closed sets closed — `parse_error.code`'s four
/// members, `run_status`'s `status`/`phase` words, `cancel_run`'s kill receipt: a fifth
/// member added in Rust without a coordinated spec bump fails here rather than in a
/// client's schema validation.
#[test]
fn every_output_schema_matches_the_pinned_contract() {
    for (tool, _args, result, _) in TOOLS {
        assert_eq!(
            advertised_schema(tool, "output"),
            pinned(result),
            "`{tool}` outputSchema drifted from $defs/{result}"
        );
    }
}

/// The advertised schemas are not just equal to the contract; they are sane on their
/// own: object roots, `additionalProperties: false`, and no `null` types anywhere
/// (ADR-0002 §2.1's no-null rule, which the schemas express by omission).
#[test]
fn advertised_schemas_are_self_consistent() {
    for (name, ..) in TOOLS {
        let tool = advertised(name);
        let input = Value::Object(tool.input_schema.as_ref().clone());
        let output = tool
            .output_schema
            .as_ref()
            .unwrap_or_else(|| panic!("`{name}` advertises an outputSchema"));
        let output = Value::Object(output.as_ref().clone());
        for schema in [("input", &input), ("output", &output)] {
            assert_eq!(
                schema.1.get("type"),
                Some(&json!("object")),
                "{} schema must have an object root",
                schema.0
            );
            assert_eq!(
                schema.1.get("additionalProperties"),
                Some(&json!(false)),
                "{} must reject unknown members",
                schema.0
            );
            let text = schema.1.to_string();
            assert!(
                !text.contains("\"null\""),
                "{} advertises a nullable type somewhere: {text}",
                schema.0
            );
        }
    }
}
