//! The pinned `$defs` of [`schema/tool-contract.schema.json`](../../schema/tool-contract.schema.json),
//! spelled once in Rust so every tool advertises the same shapes.
//!
//! `list_tasks` hand-writes its own schema (its `workflow` argument has a runtime twin,
//! `crate::tools::list_tasks`' private `WorkflowNameShape`, so prose and check belong in one
//! file).
//! The run-control tools share so many members — `run_id`, `run_view`, `run_summary`, the
//! warning list — that duplicating them per tool would be four places to keep in step instead
//! of one.
//!
//! Every function here returns the **resolved** form of one `$defs` member: no `$ref`s,
//! because an advertised `inputSchema`/`outputSchema` travels to the client on its own and a
//! reference to a document it does not have is a broken promise. `tests/tool_schema_parity.rs`
//! normalizes both sides (refs resolved, prose dropped) and compares member by member, so
//! drift fails a test rather than a client.

use serde_json::{json, Value};

/// `$defs/run_id`.
#[must_use]
pub fn run_id() -> Value {
    json!({
        "description": "Server-generated run identity; shape pinned by ADR-0002 section 1.2 so ls \
                        order is chronological. Never supplied by a caller.",
        "type": "string",
        "pattern": r"^[0-9]{8}T[0-9]{6}Z-[0-9a-f]{6}$"
    })
}

/// `$defs/timestamp`.
#[must_use]
pub fn timestamp() -> Value {
    json!({
        "description": "UTC instant, always Z (never +00:00), fractional seconds optional; writers \
                        canonicalise to millisecond precision. Same rule as ADR-0002 section 2.2.",
        "type": "string",
        "pattern": r"^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(\.[0-9]{1,6})?Z$"
    })
}

/// `$defs/relpath`.
#[must_use]
pub fn relpath() -> Value {
    json!({
        "description": "Workspace-relative (or evidence-root-relative) path, forward slashes, no \
                        leading /, no . or .. segment, no absolute host path anywhere in a tool \
                        result.",
        "type": "string",
        "minLength": 1,
        "maxLength": 512,
        "pattern": r"^[.]{0,2}[A-Za-z0-9][A-Za-z0-9 ._+-]*(/[.]{0,2}[A-Za-z0-9][A-Za-z0-9 ._+-]*)*$"
    })
}

/// `$defs/workflow_name`.
///
/// The `not` + `anyOf` pair is what makes `..`, an absolute path and a leading `-`
/// inexpressible rather than merely discouraged; the Rust twin of this shape is
/// `crate::workspace`'s `workflow_shape`, which is what actually gates a call.
#[must_use]
pub fn workflow_name() -> Value {
    json!({
        "description": "Workflow identity as a caller types it: basename without extension (coverage), \
                        basename with .yml/.yaml, or the workspace-relative path \
                        .github/workflows/<file>. Case sensitive. Two consecutive dots are forbidden \
                        anywhere, so no '..' traversal segment is expressible; a leading '/' or '-' is \
                        not matchable. ADR-0001 section 2.2.",
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

/// `$defs/commit_input` — the commit **as the caller types it** (7..=40 hex, either case).
///
/// Distinct from [`commit_full`], which is what a *result* carries: the request and the
/// answer are different promises, and only the second one is 40 characters.
#[must_use]
pub fn commit_input() -> Value {
    json!({
        "description": "Commit as a caller types it: 7 to 40 hex digits, either case, normalized to \
                        lowercase before resolution. No whitespace, no .., no ~, no leading - \
                        (ADR-0001 section 2.5).",
        "type": "string",
        "minLength": 7,
        "maxLength": 40,
        "pattern": r"^[0-9a-fA-F]{7,40}$"
    })
}

/// `$defs/input_map` — `act --input`, keys sorted so argv is golden-testable.
#[must_use]
pub fn input_map() -> Value {
    string_map(
        "workflow_dispatch-style inputs, passed as act --input key=value in sorted key order so argv \
         is stable and golden-testable.",
        4096,
    )
}

/// `$defs/secret_map` — the `--secret-file` passthrough.
#[must_use]
pub fn secret_map() -> Value {
    string_map(
        "Secret passthrough. Values never appear in argv, in any file under .evidence, in log_tail, \
         or in a tool result; the runner redacts them at the source (mact-f1w.8 / .13).",
        8192,
    )
}

/// The two map arguments differ only in prose and in the per-value byte cap, so the shape is
/// written once; naming the difference is what keeps 4 KiB and 8 KiB from drifting.
fn string_map(description: &str, max_value_bytes: u64) -> Value {
    json!({
        "description": description,
        "type": "object",
        "maxProperties": 32,
        "propertyNames": { "pattern": r"^[A-Za-z_][A-Za-z0-9_]{0,127}$" },
        "additionalProperties": { "type": "string", "maxLength": max_value_bytes }
    })
}

/// `run_workflow`'s `idempotency_key`.
#[must_use]
pub fn idempotency_key() -> Value {
    json!({
        "description": "Optional caller-side dedup token. A repeat call with the same key and the same \
                        (workflow, task, category, commit) returns the remembered run instead of \
                        starting a second one; same key with a different tuple is idempotency_conflict. \
                        Remembered in memory only, for as long as run state is retained (ADR-0004).",
        "type": "string",
        "minLength": 1,
        "maxLength": 64,
        "pattern": r"^[A-Za-z0-9._:-]+$"
    })
}

/// `$defs/task_id`.
#[must_use]
pub fn task_id() -> Value {
    json!({
        "description": "Job id declared in the resolved workflow file; exactly what act -j receives \
                        (ADR-0001 section 2.3). Same dot rule as workflow_name: '..' is not \
                        expressible.",
        "type": "string",
        "minLength": 1,
        "maxLength": 255,
        "not": { "pattern": "\\.\\." },
        "pattern": r"^[A-Za-z0-9][A-Za-z0-9 ._/-]*$"
    })
}

/// `$defs/category`.
///
/// The pattern and the length limit are the ones `tools::category` enforces in Rust; they are
/// spelled once (see that module for why the Rust check is not optional).
#[must_use]
pub fn category() -> Value {
    json!({
        "description": "OPEN vocabulary, deliberately not a closed enum: benchmark and coverage are \
                        the two known values in v1, any other shape-legal value runs the job and \
                        produces no evidence. ADR-0001 section 2.4 states the reason.",
        "type": "string",
        "minLength": 1,
        "maxLength": super::category::MAX_CATEGORY_BYTES,
        "pattern": super::category::SCHEMA_PATTERN
    })
}

/// `$defs/commit_full`.
#[must_use]
pub fn commit_full() -> Value {
    json!({
        "description": "Resolved commit: full lowercase 40-hex SHA. Every result field named commit \
                        uses this, never the caller's abbreviation.",
        "type": "string",
        "pattern": r"^[0-9a-f]{40}$"
    })
}

/// The `workflow` identity member of `run_view` / `run_summary`.
#[must_use]
pub fn workflow_identity() -> Value {
    json!({
        "description": "Resolved workflow identity: file basename without .yml/.yaml. Never a path, \
                        never re-derived from logs. Same value as the evidence envelope's workflow \
                        field.",
        "type": "string",
        "minLength": 1,
        "maxLength": 255,
        "pattern": r"^[A-Za-z0-9][A-Za-z0-9 ._/-]*$"
    })
}

/// `$defs/warning_code` as a unique, capped array — the `warnings` member everywhere.
#[must_use]
pub fn warnings() -> Value {
    json!({
        "type": "array",
        "maxItems": 64,
        "uniqueItems": true,
        "items": {
            "description": "OPEN vocabulary of machine-readable caveats. Same vocabulary as \
                            meta.json warnings[] in ADR-0002 section 5.1, plus the tool-level \
                            members listed in docs/tool-contract.md. Adding a member is additive; \
                            an unknown code is never fatal.",
            "type": "string",
            "minLength": 1,
            "maxLength": 64,
            "pattern": r"^[a-z][a-z0-9_]*$"
        }
    })
}

/// `$defs/log_tail_bytes`.
#[must_use]
pub fn log_tail_bytes() -> Value {
    json!({
        "description": "How much log tail to return. The server truncates to a UTF-8 boundary at \
                        the last complete line that fits.",
        "type": "integer",
        "minimum": 0,
        "maximum": 65536,
        "default": 4096
    })
}

/// `$defs/log_tail`.
#[must_use]
pub fn log_tail() -> Value {
    json!({
        "description": "Last lines of act's rendered log, redacted, workspace and evidence absolute \
                        paths replaced by <WORKSPACE> and <EVIDENCE>. Length is capped in BYTES by \
                        log_tail_bytes; maxLength here is the maximum allowed request (65536).",
        "type": "string",
        "maxLength": 65536
    })
}

/// `$defs/status`.
#[must_use]
pub fn status() -> Value {
    json!({
        "description": "Run state. pending/running are non-terminal and reachable only through \
                        run_status or a wait:false call; the other five are terminal and are the \
                        SAME names as .evidence run_status (ADR-0002).",
        "enum": ["pending", "running", "success", "failed", "cancelled", "timed_out", "error"]
    })
}

/// `$defs/phase`.
#[must_use]
pub fn phase() -> Value {
    json!({
        "description": "Coarse progress inside the supervisor (ADR-0004 owns transitions). \
                        Advisory: a client may display it, nothing may branch on it. Unknown values \
                        are ignored.",
        "type": "string",
        "enum": [
            "queued",
            "preparing_tree",
            "starting_act",
            "running",
            "extracting_evidence",
            "publishing_evidence",
            "cancelling",
            "done"
        ]
    })
}

/// `$defs/error` — the typed error payload, also the `error` member of a `status: "error"` run.
#[must_use]
pub fn error() -> Value {
    json!({
        "description": "The typed error payload. Present in structuredContent of every isError:true \
                        tool result (as the whole object), and as the 'error' member of a run \
                        result whose terminal status is 'error'.",
        "type": "object",
        "required": ["code", "message", "retryable"],
        "additionalProperties": false,
        "properties": {
            "code": {
                "description": "CLOSED set of machine-readable tool error codes. Every member is \
                                documented in docs/adr/0001-tool-contract.md section 5.3 and \
                                docs/tool-contract.md; x-error-codes in this file carries isError / \
                                retryable / owning tools per member. A client that receives a code \
                                it does not know MUST treat the call as a failed tool call (never \
                                as success) and MUST NOT retry blindly.",
                "enum": [
                    "internal_error",
                    "invalid_arguments",
                    "unknown_argument",
                    "workspace_not_found",
                    "not_a_git_repository",
                    "workflows_dir_missing",
                    "act_missing",
                    "docker_unreachable",
                    "workflow_not_found",
                    "ambiguous_workflow",
                    "task_not_found",
                    "commit_unknown",
                    "commit_ambiguous",
                    "isolation_failed",
                    "workspace_busy",
                    "too_many_runs",
                    "idempotency_conflict",
                    "run_not_found",
                    "act_spawn_failed",
                    "act_crashed",
                    "runner_internal_error",
                    "evidence_write_failed",
                    "evidence_path_exists",
                    "evidence_path_escape"
                ]
            },
            "message": {
                "description": "One human-readable sentence, no trailing newline, secret values \
                                already redacted. Never a stack trace, never a host absolute path.",
                "type": "string",
                "minLength": 1,
                "maxLength": 1024
            },
            "retryable": {
                "description": "Whether calling again unchanged could possibly succeed, taken from \
                                the table in ADR-0001 section 5.3. Callers MUST NOT retry a \
                                retryable:true error in a tight loop.",
                "type": "boolean"
            },
            "detail": {
                "description": "Per-code machine-readable context; shape documented per code in \
                                x-error-codes. Keys listed there are always present for that code, \
                                extra keys are allowed. Must not exceed 4096 bytes when compactly \
                                serialized; lists are truncated rather than truncated mid-element.",
                "type": "object",
                "maxProperties": 16
            }
        }
    })
}

/// The five terminal status words, as the `if` branches of the run shapes use them.
const TERMINAL: &[&str] = &["success", "failed", "cancelled", "timed_out", "error"];
const NON_TERMINAL: &[&str] = &["pending", "running"];

/// The `allOf` block both `run_view` and `run_summary` carry: `terminal` must agree with
/// `status`, `finished_at`/`exit_code`/`error` may only appear where the status allows, and
/// `error` appears exactly when the status is `error` (`run_summary` has no `error` member,
/// so it uses [`status_agreement_only`]).
#[must_use]
pub fn run_view_invariants() -> Value {
    json!([
        {
            "if": { "properties": { "status": { "enum": TERMINAL } }, "required": ["status"] },
            "then": { "required": ["finished_at"] }
        },
        {
            "if": {
                "properties": { "status": { "enum": NON_TERMINAL } },
                "required": ["status"]
            },
            "then": {
                "not": {
                    "anyOf": [
                        { "required": ["finished_at"] },
                        { "required": ["exit_code"] },
                        { "required": ["error"] }
                    ]
                }
            }
        },
        {
            "if": { "properties": { "status": { "const": "error" } }, "required": ["status"] },
            "then": { "required": ["error"] }
        },
        {
            "if": {
                "properties": {
                    "status": { "enum": ["success", "failed", "cancelled", "timed_out"] }
                },
                "required": ["status"]
            },
            "then": { "not": { "required": ["error"] } }
        },
        {
            "if": { "properties": { "status": { "enum": TERMINAL } }, "required": ["status"] },
            "then": { "properties": { "terminal": { "const": true } } }
        },
        {
            "if": { "properties": { "status": { "enum": NON_TERMINAL } }, "required": ["status"] },
            "then": { "properties": { "terminal": { "const": false } } }
        }
    ])
}

/// The `allOf` of `run_summary`: the terminal/non-terminal agreement, without the
/// `finished_at`/`error` members the summary deliberately does not have.
#[must_use]
pub fn status_agreement_only() -> Value {
    json!([
        {
            "if": { "properties": { "status": { "enum": TERMINAL } }, "required": ["status"] },
            "then": { "properties": { "terminal": { "const": true } } }
        },
        {
            "if": { "properties": { "status": { "enum": NON_TERMINAL } }, "required": ["status"] },
            "then": { "properties": { "terminal": { "const": false } } }
        }
    ])
}

/// `$defs/run_view` — the canonical view of one run, in resolved form.
///
/// This is the shape `run_status(run_id)` answers with and the shape `run_workflow` extends
/// with `deduplicated` (mact-f1w.14), so it lives here rather than in one tool's file.
#[must_use]
pub fn run_view() -> Value {
    let mut properties = serde_json::Map::new();
    properties.insert("run_id".to_owned(), run_id());
    properties.insert("status".to_owned(), status());
    properties.insert(
        "terminal".to_owned(),
        json!({
            "description": "Redundant with status by design: a client polling run_status should not \
                            need a status table to decide whether to poll again.",
            "type": "boolean"
        }),
    );
    properties.insert("phase".to_owned(), phase());
    properties.insert("workflow".to_owned(), workflow_identity());
    properties.insert("workflow_file".to_owned(), relpath());
    properties.insert("task".to_owned(), task_id());
    properties.insert("category".to_owned(), category());
    properties.insert("commit".to_owned(), commit_full());
    properties.insert("started_at".to_owned(), timestamp());
    properties.insert("finished_at".to_owned(), timestamp());
    properties.insert(
        "duration_ms".to_owned(),
        json!({
            "description": "Wall duration in ms: total run duration once terminal, elapsed-so-far \
                            while pending/running. Omitted only when the clock is not known (act \
                            never started).",
            "type": "integer",
            "minimum": 0,
            "maximum": 259200000
        }),
    );
    properties.insert(
        "exit_code".to_owned(),
        json!({
            "description": "act's wait(2) code. Absent when act produced no exit status (signalled, \
                            spawn failure) and always absent for pending/running.",
            "type": "integer",
            "minimum": 0,
            "maximum": 255
        }),
    );
    properties.insert(
        "evidence_files".to_owned(),
        json!({
            "description": "The JSONL evidence files this run published, relative to the evidence \
                            root (ADR-0002 section 1.1). Empty array means the run produced none, \
                            which is legitimate for an unknown category and for a run that died \
                            before extraction. meta.json and act.jsonl are NOT listed here: they \
                            are audit paths, not the contract.",
            "type": "array",
            "maxItems": 8,
            "items": relpath()
        }),
    );
    properties.insert(
        "evidence_record_count".to_owned(),
        json!({
            "description": "Total records across evidence_files. Zero is meaningful (ran, found \
                            nothing); it is not an error.",
            "type": "integer",
            "minimum": 0,
            "maximum": 1000000
        }),
    );
    properties.insert("log_tail".to_owned(), log_tail());
    properties.insert("warnings".to_owned(), warnings());
    properties.insert("error".to_owned(), error());
    json!({
        "description": "The canonical view of one run, returned by run_status(run_id) and, with the \
                        extra member 'deduplicated', by run_workflow. Fields the caller asked for \
                        by name in the ticket are all here: run_id, status, workflow_file, job \
                        (named 'task'), commit, started_at, duration_ms, evidence_files, \
                        evidence_record_count, log_tail, error.",
        "type": "object",
        "additionalProperties": false,
        "required": [
            "run_id",
            "status",
            "terminal",
            "workflow",
            "workflow_file",
            "task",
            "category",
            "commit",
            "evidence_files",
            "evidence_record_count",
            "log_tail",
            "warnings"
        ],
        "properties": Value::Object(properties),
        "allOf": run_view_invariants()
    })
}

/// `$defs/run_summary` — the listing entry, deliberately without log or evidence detail.
#[must_use]
pub fn run_summary() -> Value {
    let mut properties = serde_json::Map::new();
    properties.insert("run_id".to_owned(), run_id());
    properties.insert("status".to_owned(), status());
    properties.insert("terminal".to_owned(), json!({ "type": "boolean" }));
    properties.insert("phase".to_owned(), phase());
    properties.insert(
        "workflow".to_owned(),
        json!({
            "type": "string",
            "minLength": 1,
            "maxLength": 255,
            "pattern": r"^[A-Za-z0-9][A-Za-z0-9 ._/-]*$"
        }),
    );
    properties.insert("task".to_owned(), task_id());
    properties.insert("category".to_owned(), category());
    properties.insert("commit".to_owned(), commit_full());
    properties.insert("started_at".to_owned(), timestamp());
    properties.insert(
        "duration_ms".to_owned(),
        json!({ "type": "integer", "minimum": 0, "maximum": 259200000 }),
    );
    json!({
        "description": "Compact entry for the run_status listing (no run_id given). Deliberately \
                        small: the listing is a recovery aid after a lost run_id, not a log viewer.",
        "type": "object",
        "additionalProperties": false,
        "required": [
            "run_id",
            "status",
            "terminal",
            "workflow",
            "task",
            "category",
            "commit"
        ],
        "properties": Value::Object(properties),
        "allOf": status_agreement_only()
    })
}
