"""The PreToolUse decision for one tool call (see guard.py)."""

import os
from collections.abc import Mapping
from typing import Any

from . import bash_policy
from .paths import is_secret_file, is_write_protected

PATH_TOOLS = ("Read", "Edit", "Write", "MultiEdit", "NotebookEdit")
WRITE_TOOLS = ("Edit", "Write", "MultiEdit", "NotebookEdit")
_SECRET_PATTERN_MARKERS = (".env", ".pem", "settings.local.json")


def decide(event: Mapping[str, Any], workspace: str) -> str | None:
    """The reason to block the call, or None to let it through."""
    if not workspace or not os.path.isabs(workspace):
        return "the guard does not know the workspace (AUTOFIX_WORKSPACE)"
    tool = event.get("tool_name")
    tool_input = event.get("tool_input")
    if not isinstance(tool, str) or not isinstance(tool_input, dict):
        return "the hook input has no tool_name or tool_input"
    raw_cwd = event.get("cwd")
    cwd = raw_cwd if isinstance(raw_cwd, str) else workspace
    if tool == "Bash":
        return _bash(tool_input, cwd, workspace)
    if tool in PATH_TOOLS:
        return _file(tool, tool_input, cwd, workspace)
    if tool in ("Grep", "Glob"):
        return _search(tool, tool_input, cwd, workspace)
    return None


def _bash(tool_input: Mapping[str, Any], cwd: str, workspace: str) -> str | None:
    command = tool_input.get("command")
    if not isinstance(command, str):
        return "Bash without a command string"
    return bash_policy.check(command, cwd, workspace)


def _file(tool: str, tool_input: Mapping[str, Any], cwd: str, workspace: str) -> str | None:
    raw = tool_input.get("notebook_path" if tool == "NotebookEdit" else "file_path")
    if not isinstance(raw, str) or not raw:
        return f"{tool} without a file path"
    path = _absolute(raw, cwd)
    if not bash_policy.inside(path, workspace):
        return f"{tool} outside the workspace is blocked"
    if is_secret_file(path):
        return "reading or writing .env, .pem or settings.local.json files is blocked"
    relative = os.path.relpath(path, os.path.realpath(workspace))
    if tool in WRITE_TOOLS and is_write_protected(relative):
        return f"changing {relative} is blocked: it is a protected path"
    return None


def _search(tool: str, tool_input: Mapping[str, Any], cwd: str, workspace: str) -> str | None:
    raw = tool_input.get("path") or cwd
    if not isinstance(raw, str):
        return f"{tool} with an invalid path"
    if not bash_policy.inside(_absolute(raw, cwd), workspace):
        return f"{tool} outside the workspace is blocked"
    # Grep's `pattern` is a regex over file contents (`process.env` is fine);
    # Glob's `pattern` and Grep's `glob` select files.
    selectors = [tool_input.get("glob")] if tool == "Grep" else [tool_input.get("pattern")]
    for selector in selectors:
        if isinstance(selector, str) and any(
            marker in selector for marker in _SECRET_PATTERN_MARKERS
        ):
            return f"{tool} for .env, .pem or settings.local.json files is blocked"
    return None


def _absolute(path: str, cwd: str) -> str:
    return os.path.realpath(path if os.path.isabs(path) else os.path.join(cwd, path))
