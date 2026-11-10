#!/usr/bin/env python3
"""Generate the ADR-0001 tool-contract fixtures under tests/e2e/testdata/tool-io.

One reviewable source for the whole fixture tree, so the tree is reproducible and diffable
instead of 48 hand-made files with inconsistent formatting. The fixtures are hand-authored
contract examples written FROM the ADR, never emitted by the implementation: the
implementation is measured against them, not the other way round.

Re-running this script must produce byte-identical files. CI runs
scripts/validate_tool_contract.py, which validates every fixture against the matching
$defs entry in schema/tool-contract.schema.json.

Filename grammar (parsed by scripts/validate_tool_contract.py):
    <tool-or-error>.<args|result>.<scenario>.json
where the first token is run_workflow | list_tasks | run_status | cancel_run | error
('error' selects the shared $defs/tool_error shape).
"""
from __future__ import annotations

import json
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent / "tests/e2e/testdata/tool-io"
SHA = "2f7c1a4b9e0d3c6a5b8f1e2d4c6a8b0f3e5d7c91"
BENCH_RUN = "20261110T004203Z-a1b2c3"
COV_RUN = "20261110T011507Z-d4e5f6"


def run_view(**over) -> dict:
    """The canonical run_view, in its most common shape: a successful coverage run."""
    base = {
        "run_id": COV_RUN,
        "status": "success",
        "terminal": True,
        "phase": "done",
        "workflow": "coverage",
        "workflow_file": ".github/workflows/coverage.yml",
        "task": "coverage",
        "category": "coverage",
        "commit": SHA,
        "started_at": "2026-11-10T01:15:07.004Z",
        "finished_at": "2026-11-10T01:16:08.237Z",
        "duration_ms": 61233,
        "exit_code": 0,
        "evidence_files": [f".evidence/coverage/{SHA}/{COV_RUN}.jsonl"],
        "evidence_record_count": 5,
        "log_tail": "exit status 0\n",
        "warnings": [],
    }
    base.update(over)
    for key in [k for k, v in base.items() if v is ABSENT]:
        del base[key]
    return base


class _Absent:
    """Sentinel: this key is omitted, which is not the same as null (ADR-0001 §5.1)."""

    def __repr__(self) -> str:  # pragma: no cover - debugging aid
        return "<absent>"


ABSENT = _Absent()


def rw(**over) -> dict:
    """run_workflow result: run_view plus 'deduplicated'."""
    out = {"deduplicated": False}
    out.update(run_view(**over))
    return out


ARGS_BASE = {
    "workflow": "coverage",
    "task": "coverage",
    "category": "coverage",
    "commit": SHA,
}

VALID: dict[str, dict] = {
    # ---- run_workflow -------------------------------------------------------
    "run_workflow.args.minimal.json": {
        "workflow": "coverage",
        "task": "coverage",
        "category": "coverage",
        "commit": "2f7c1a4",
    },
    "run_workflow.args.full.json": {
        "workflow": ".github/workflows/benchmark.yml",
        "task": "hyperfine-bench",
        "category": "benchmark",
        "commit": SHA.upper(),
        "wait": False,
        "timeout_secs": 1800,
        "log_tail_bytes": 8192,
        "inputs": {"BENCH_ITERS": "20", "PROFILE": "release"},
        "secrets": {"NPM_TOKEN": "super-secret-value"},
        "idempotency_key": "sweep-2026-11-10:2f7c1a4:benchmark",
    },
    "run_workflow.result.success.json": rw(),
    "run_workflow.result.failed.json": rw(
        run_id=BENCH_RUN,
        status="failed",
        workflow="benchmark",
        workflow_file=".github/workflows/benchmark.yml",
        task="hyperfine-bench",
        category="benchmark",
        started_at="2026-11-10T00:42:03.417Z",
        finished_at="2026-11-10T00:43:37.539Z",
        duration_ms=94122,
        exit_code=1,
        evidence_files=[f".evidence/benchmark/{SHA}/{BENCH_RUN}.jsonl"],
        evidence_record_count=4,
        log_tail="##[error]Process completed with exit code 1.\n",
        warnings=["exit_nonzero"],
    ),
    "run_workflow.result.running.json": rw(
        status="running",
        terminal=False,
        phase="running",
        finished_at=ABSENT,
        exit_code=ABSENT,
        evidence_files=[],
        evidence_record_count=0,
        duration_ms=12011,
        log_tail="",
    ),
    "run_workflow.result.error.json": rw(
        status="error",
        exit_code=ABSENT,
        evidence_files=[],
        evidence_record_count=0,
        duration_ms=301,
        log_tail="",
        error={
            "code": "runner_internal_error",
            "message": "worktree vanished while act was running",
            "retryable": True,
            "detail": {
                "stage": "publishing_evidence",
                "reason": "ENOENT: <WORKSPACE>/.wt/2f7c1a4",
            },
        },
    ),
    "run_workflow.result.timed_out.json": rw(
        status="timed_out",
        exit_code=ABSENT,
        evidence_files=[],
        evidence_record_count=0,
        duration_ms=900004,
        log_tail="act still running after 900s; process group killed\n",
        warnings=["process_group_killed"],
    ),
    "run_workflow.result.cancelled.json": rw(
        status="cancelled",
        exit_code=ABSENT,
        evidence_files=[],
        evidence_record_count=0,
        duration_ms=4022,
        log_tail="",
        warnings=["partial_evidence_dropped"],
    ),
    "run_workflow.result.deduplicated.json": rw(
        deduplicated=True, warnings=["deduplicated"]
    ),
    # ---- list_tasks ---------------------------------------------------------
    "list_tasks.args.empty.json": {},
    "list_tasks.args.filtered.json": {"workflow": "coverage", "include_steps": True},
    "list_tasks.result.full.json": {
        "workflows_dir": ".github/workflows",
        "workflow_count": 3,
        "workflows": [
            {
                "name": "benchmark",
                "file": ".github/workflows/benchmark.yml",
                "display": "Benchmark",
                "job_count": 2,
                "jobs": [
                    {
                        "id": "hyperfine-bench",
                        "name": "Run hyperfine benchmarks",
                        "needs": ["build"],
                        "runs_on": ["ubuntu-24.04"],
                        "matrix_axes": ["profile"],
                        "uses_composite": True,
                        "category_hint": "benchmark",
                    },
                    {
                        "id": "build",
                        "category_hint": "other",
                        "runs_on": ["ubuntu-24.04"],
                    },
                ],
            },
            {
                "name": "coverage",
                "file": ".github/workflows/coverage.yml",
                "job_count": 1,
                "jobs": [
                    {
                        "id": "coverage",
                        "category_hint": "coverage",
                        "runs_on": ["ubuntu-24.04"],
                        "steps": [{"name": "Run tests", "uses": "nick-fields/retry@v3"}],
                    }
                ],
            },
            {
                "name": "broken",
                "file": ".github/workflows/broken.yml",
                "job_count": 0,
                "jobs": [],
                "parse_error": {
                    "code": "invalid_yaml",
                    "message": "mapping values are not allowed here at line 7 column 3",
                },
            },
        ],
        "warnings": ["parse_error"],
    },
    # ---- run_status ---------------------------------------------------------
    "run_status.args.list.json": {},
    "run_status.args.single.json": {"run_id": COV_RUN, "log_tail_bytes": 2048},
    "run_status.result.running.json": {
        "active_count": 1,
        "truncated": False,
        "run": run_view(
            status="running",
            terminal=False,
            phase="extracting_evidence",
            finished_at=ABSENT,
            exit_code=ABSENT,
            evidence_files=[],
            evidence_record_count=0,
            duration_ms=61002,
        ),
    },
    "run_status.result.list.json": {
        "active_count": 1,
        "truncated": True,
        "runs": [
            {
                "run_id": COV_RUN,
                "status": "running",
                "terminal": False,
                "phase": "running",
                "workflow": "coverage",
                "task": "coverage",
                "category": "coverage",
                "commit": SHA,
                "started_at": "2026-11-10T01:15:07.004Z",
                "duration_ms": 3044,
            },
            {
                "run_id": BENCH_RUN,
                "status": "success",
                "terminal": True,
                "phase": "done",
                "workflow": "benchmark",
                "task": "hyperfine-bench",
                "category": "benchmark",
                "commit": SHA,
                "started_at": "2026-11-10T00:42:03.417Z",
                "duration_ms": 94122,
            },
        ],
    },
    # ---- cancel_run ---------------------------------------------------------
    "cancel_run.args.json": {"run_id": COV_RUN},
    "cancel_run.result.cancelled.json": {
        "run_id": COV_RUN,
        "status": "cancelled",
        "terminal": True,
        "already_terminal": False,
        "cancel_requested_at": "2026-11-10T01:15:31.900Z",
        "kill": {"method": "term", "took_ms": 812},
        "warnings": ["partial_evidence_dropped"],
    },
    "cancel_run.result.already-terminal.json": {
        "run_id": BENCH_RUN,
        "status": "success",
        "terminal": True,
        "already_terminal": True,
        "warnings": ["already_terminal"],
    },
    # ---- the isError:true payload -------------------------------------------
    "error.result.workflow-not-found.json": {
        "error": {
            "code": "workflow_not_found",
            "message": "no workflow named 'bench' in .github/workflows",
            "retryable": False,
            "detail": {
                "workflow": "bench",
                "workflows_dir": ".github/workflows",
                "available": ["benchmark", "coverage"],
            },
        }
    },
    "error.result.workspace-busy.json": {
        "error": {
            "code": "workspace_busy",
            "message": "workspace already has an act run in flight",
            "retryable": True,
            "detail": {
                "run_id": COV_RUN,
                "phase": "running",
                "started_at": "2026-11-10T01:15:07.004Z",
            },
        }
    },
}

INVALID: dict[str, dict] = {
    "run_workflow.args.missing-task.json": {
        k: v for k, v in ARGS_BASE.items() if k != "task"
    },
    "run_workflow.args.unknown-field.json": dict(ARGS_BASE, run_id=COV_RUN),
    "run_workflow.args.short-commit.json": dict(ARGS_BASE, commit="2f7c1a"),
    "run_workflow.args.nonhex-commit.json": dict(ARGS_BASE, commit="zzzzzzz"),
    "run_workflow.args.workflow-traversal.json": dict(ARGS_BASE, workflow="../etc/passwd"),
    "run_workflow.args.category-uppercase.json": dict(ARGS_BASE, category="Coverage"),
    "run_workflow.args.timeout-too-large.json": dict(ARGS_BASE, timeout_secs=30000),
    "run_workflow.args.secret-value-not-string.json": dict(
        ARGS_BASE, secrets={"NPM_TOKEN": 42}
    ),
    "run_workflow.args.bad-idempotency-key.json": dict(
        ARGS_BASE, idempotency_key="has spaces"
    ),
    "run_workflow.result.missing-run-id.json": {
        k: v for k, v in rw().items() if k != "run_id"
    },
    "run_workflow.result.status-succeeded.json": rw(status="succeeded"),
    "run_workflow.result.terminal-without-finished-at.json": {
        k: v for k, v in rw().items() if k != "finished_at"
    },
    "run_workflow.result.running-with-finished-at.json": rw(
        status="running", terminal=False, exit_code=ABSENT
    ),
    "run_workflow.result.failed-with-error.json": rw(
        status="failed",
        exit_code=1,
        error={"code": "internal_error", "message": "sic", "retryable": True},
    ),
    "run_workflow.result.error-status-without-error.json": {
        k: v for k, v in rw(status="error").items() if k != "error"
    },
    "run_workflow.result.explicit-null.json": dict(rw(), duration_ms=None),
    "run_workflow.result.absolute-evidence-path.json": rw(
        evidence_files=["/home/dev/scratch/.evidence/coverage/x.jsonl"]
    ),
    "run_workflow.result.short-commit.json": rw(commit="2f7c1a4"),
    "run_workflow.result.unknown-error-code.json": rw(
        status="error",
        error={"code": "workflow_missing", "message": "sic", "retryable": False},
    ),
    "run_workflow.result.log-tail-too-long.json": rw(log_tail="x" * 65537),
    "run_workflow.result.missing-deduplicated.json": {
        k: v for k, v in rw().items() if k != "deduplicated"
    },
    "run_workflow.result.terminal-flag-disagrees-with-status.json": rw(
        status="success", terminal=False
    ),
    "run_status.args.bad-run-id.json": {"run_id": "2026-11-10T01:15:07Z"},
    "run_status.args.unknown-field.json": {"run_id": COV_RUN, "workflow": "coverage"},
    "run_status.result.neither-run-nor-runs.json": {"active_count": 0, "truncated": False},
    "run_status.result.both-run-and-runs.json": {
        "active_count": 1,
        "truncated": False,
        "run": run_view(),
        "runs": [],
    },
    "cancel_run.result.already-terminal-with-kill.json": {
        "run_id": BENCH_RUN,
        "status": "success",
        "terminal": True,
        "already_terminal": True,
        "kill": {"method": "term"},
    },
    "cancel_run.result.missing-cancel-requested-at.json": {
        "run_id": COV_RUN,
        "status": "cancelled",
        "terminal": True,
        "already_terminal": False,
    },
    "list_tasks.result.unknown-field.json": {
        "workflows_dir": ".github/workflows",
        "workflow_count": 0,
        "workflows": [],
        "warnings": [],
        "workspace": "/home/dev/scratch",
    },
    "list_tasks.result.bad-category-hint.json": {
        "workflows_dir": ".github/workflows",
        "workflow_count": 1,
        "workflows": [
            {
                "name": "coverage",
                "file": ".github/workflows/coverage.yml",
                "job_count": 1,
                "jobs": [{"id": "coverage", "category_hint": "cov"}],
            }
        ],
        "warnings": [],
    },
    "list_tasks.result.absolute-file.json": {
        "workflows_dir": ".github/workflows",
        "workflow_count": 1,
        "workflows": [
            {
                "name": "coverage",
                "file": "/home/dev/scratch/.github/workflows/coverage.yml",
                "job_count": 0,
                "jobs": [],
            }
        ],
        "warnings": [],
    },
    "error.result.missing-retryable.json": {
        "error": {"code": "run_not_found", "message": "no such run"}
    },
    "error.result.detail-not-object.json": {
        "error": {
            "code": "run_not_found",
            "message": "no such run",
            "retryable": False,
            "detail": "20261110T011507Z-d4e5f6",
        }
    },
    "error.result.double-nested.json": {
        "error": {
            "error": {"code": "run_not_found", "message": "sic", "retryable": False}
        }
    },
}

# The violation in this fixture is the presence of finished_at on a non-terminal run;
# rw() set it, so put it back after rw() dropped nothing.
INVALID["run_workflow.result.running-with-finished-at.json"]["finished_at"] = (
    "2026-11-10T01:16:08.237Z"
)


def dump(path: Path, doc) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(doc, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def main() -> None:
    for name, doc in VALID.items():
        dump(ROOT / "valid" / name, doc)
    for name, doc in INVALID.items():
        dump(ROOT / "invalid" / name, doc)
    print(
        "wrote %d valid and %d invalid fixtures under %s"
        % (len(VALID), len(INVALID), ROOT)
    )


if __name__ == "__main__":
    main()
