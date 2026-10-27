"""Reading what the agent step left behind.

Claude Code's `--output-format json` result is one JSON object. Its
`structured_output` is the agent's answer (`result.schema.json`); everything
in it is untrusted text, so it is checked field by field here and only the
known fields are passed on (the api's `AgentResult`).
"""

import json
import re
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from .proc import tail

OUTCOMES = (
    "fixed_with_test",
    "fixed_without_test",
    "not_a_code_bug",
    "not_reproducible",
    "duplicate",
    "needs_human",
)
FIX_OUTCOMES = ("fixed_with_test", "fixed_without_test")
TARGET_REPOS = ("granite-webhooks", "general_datebase")
TEST_STATUSES = ("written", "not_possible", "not_applicable")

# Field -> (nullable, max length), from result.schema.json.
_TEXT_FIELDS: dict[str, tuple[bool, int]] = {
    "summary": (False, 300),
    "root_cause": (False, 3000),
    "fix_summary": (False, 3000),
    "risk": (False, 1500),
    "recommendation": (True, 1500),
    "duplicate_of": (True, 300),
    "pr_title": (True, 100),
}
_TEST_TEXT_FIELDS: dict[str, tuple[bool, int]] = {
    "command": (True, 500),
    "why_not_possible": (True, 1500),
    "manual_test_steps": (True, 2000),
}
_NOTE_LIMIT = 1500


class ResultError(ValueError):
    pass


@dataclass(frozen=True)
class AgentRun:
    """How the agent step ended.

    `kind` is `success` (a valid result), `usage_limited` (the subscription's
    limit stopped it: the api queues the run again) or `failed`.
    """

    kind: str
    result: dict[str, Any] | None
    cost_usd: float | None
    notes: str | None


def mentions_usage_limit(text: str) -> bool:
    """Whether an unsuccessful run's text reads like a usage or rate limit.

    Claude Code documents no subtype for it, so this is a heuristic: "limit"
    next to "reset", "usage" or "session", or an HTTP 429.
    """
    lowered = text.lower()
    if re.search(r"\b429\b", lowered):
        return True
    if "limit" not in lowered:
        return False
    return any(word in lowered for word in ("reset", "usage", "session"))


def classify(stdout: str, stderr: str) -> AgentRun:
    """Turns the CLI's stdout and stderr into an [`AgentRun`]."""
    output = parse_cli_output(stdout)
    cost = _cost(output)
    subtype = output.get("subtype") if output else None
    structured = output.get("structured_output") if output else None
    if subtype == "success" and structured is not None:
        try:
            return AgentRun("success", validate_result(structured), cost, None)
        except ResultError as error:
            return AgentRun("failed", None, cost, f"The agent's result is invalid: {error}")
    text = _failure_text(output, stderr)
    if mentions_usage_limit(text):
        return AgentRun("usage_limited", None, cost, tail(text, _NOTE_LIMIT) or None)
    reason = subtype or "no result"
    if subtype == "success":
        reason = "success without structured_output"
    detail = tail(text, _NOTE_LIMIT)
    notes = f"The agent ended with {reason}" + (f": {detail}" if detail else ".")
    return AgentRun("failed", None, cost, notes)


def load(stdout_path: Path, stderr_path: Path) -> AgentRun:
    stdout = stdout_path.read_text(errors="replace") if stdout_path.exists() else ""
    stderr = stderr_path.read_text(errors="replace") if stderr_path.exists() else ""
    return classify(stdout, stderr)


def parse_cli_output(stdout: str) -> dict[str, Any] | None:
    """The CLI's JSON object: the whole stdout, or else its last JSON line."""
    candidates = [stdout.strip(), *reversed(stdout.strip().splitlines())]
    for candidate in candidates:
        try:
            value = json.loads(candidate)
        except ValueError:
            continue
        if isinstance(value, dict):
            return value
    return None


def _cost(output: dict[str, Any] | None) -> float | None:
    value = output.get("total_cost_usd") if output else None
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return round(float(value), 4)


def _failure_text(output: dict[str, Any] | None, stderr: str) -> str:
    parts: list[str] = []
    if output:
        for key in ("result", "error"):
            if isinstance(output.get(key), str):
                parts.append(output[key])
        errors = output.get("errors")
        if isinstance(errors, list):
            parts.extend(str(item) for item in errors)
    parts.append(_readable(stderr))
    return "\n".join(part for part in parts if part)


_CODE_FRAME = re.compile(r"^\s*\d+ \| ")
_LONGEST_LINE = 400


def _readable(stderr: str) -> str:
    """stderr without the CLI's own source: when it crashes, Bun prints the
    minified bundle around the throw and a stack into it, which would bury
    the one `error:` line in the email."""
    lines = (line.rstrip() for line in stderr.splitlines())
    kept = (
        line
        for line in lines
        if "$bunfs" not in line
        and not _CODE_FRAME.match(line)
        and len(line) <= _LONGEST_LINE
    )
    return "\n".join(kept).strip()


def validate_result(value: object) -> dict[str, Any]:
    """Checks `structured_output` against result.schema.json.

    Returns a new dict holding exactly the schema's fields, so nothing the
    agent added travels further.
    """
    data = _require_object(value, "structured_output")
    result: dict[str, Any] = {
        "outcome": _require_enum(data, "outcome", OUTCOMES),
        "target_repo": _require_enum(data, "target_repo", (*TARGET_REPOS, None)),
    }
    for field, (nullable, limit) in _TEXT_FIELDS.items():
        result[field] = _require_text(data, field, nullable, limit)
    if not result["summary"] or not result["root_cause"]:
        raise ResultError("summary and root_cause must not be empty")
    result["test"] = _validate_test(data.get("test"))
    _check_consistency(result)
    return result


def _validate_test(value: object) -> dict[str, Any]:
    data = _require_object(value, "test")
    files = data.get("files")
    if not isinstance(files, list) or len(files) > 10:
        raise ResultError("test.files must be a list of at most 10 paths")
    if not all(isinstance(item, str) and len(item) <= 300 for item in files):
        raise ResultError("test.files must hold strings of at most 300 characters")
    test: dict[str, Any] = {
        "status": _require_enum(data, "status", TEST_STATUSES),
        "files": list(files),
    }
    for field, (nullable, limit) in _TEST_TEXT_FIELDS.items():
        test[field] = _require_text(data, field, nullable, limit)
    return test


def _check_consistency(result: dict[str, Any]) -> None:
    outcome = result["outcome"]
    test = result["test"]
    if outcome in FIX_OUTCOMES and result["target_repo"] is None:
        raise ResultError(f"{outcome} needs a target_repo")
    if outcome == "fixed_with_test" and (test["status"] != "written" or not test["command"]):
        raise ResultError("fixed_with_test needs test.status written and a test.command")


def _require_object(value: object, name: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise ResultError(f"{name} must be an object")
    return value


def _require_enum(data: dict[str, Any], field: str, allowed: tuple[object, ...]) -> Any:
    if field not in data:
        raise ResultError(f"{field} is missing")
    value = data[field]
    if value not in allowed:
        raise ResultError(f"{field} has an unknown value")
    return value


def _require_text(data: dict[str, Any], field: str, nullable: bool, limit: int) -> str | None:
    if field not in data:
        raise ResultError(f"{field} is missing")
    value = data[field]
    if value is None and nullable:
        return None
    if not isinstance(value, str):
        raise ResultError(f"{field} must be a string")
    if len(value) > limit:
        raise ResultError(f"{field} is longer than {limit} characters")
    return value
