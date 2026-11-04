"""Temp git repositories and sample agent results for the tests."""

import copy
import subprocess
import sys
from pathlib import Path
from typing import Any

TOOLS_DIR = Path(__file__).resolve().parent.parent
if str(TOOLS_DIR) not in sys.path:
    sys.path.insert(0, str(TOOLS_DIR))

from autofix.context import RunContext  # noqa: E402

ISSUE_ID = "0192f3a4-5b6c-7d8e-9f01-23456789abcd"
RUN_ID = "42"

SAMPLE_RESULT: dict[str, Any] = {
    "outcome": "fixed_with_test",
    "target_repo": "granite-webhooks",
    "summary": "Adding two numbers subtracts them",
    "given": "a toy library with two numbers",
    "when": "a caller adds 2 and 3 with add()",
    "then": "add() returns -1",
    "actual": "Adding two numbers subtracts them.",
    "expected": "add() returns their sum.",
    "impact": "Callers get wrong totals.",
    "root_cause": "toy/src/lib.rs uses `-` instead of `+`.",
    "fix_summary": "Use `+`.",
    "risk": "None.",
    "test": {
        "status": "written",
        "files": ["toy/src/lib.rs"],
        "command": "cargo test -p toy adds",
        "why_not_possible": None,
        "manual_test_steps": None,
    },
    "check": "Repeat Given/When → add() returns 5.",
    "recommendation": None,
    "duplicate_of": None,
    "pr_title": "fix(toy): add instead of subtract",
}


def sample_result(**changes: Any) -> dict[str, Any]:
    result = copy.deepcopy(SAMPLE_RESULT)
    test_changes = changes.pop("test", {})
    result.update(changes)
    result["test"].update(test_changes)
    return result


def git(repo: Path, *args: str) -> str:
    completed = subprocess.run(["git", *args], cwd=repo, capture_output=True, text=True, check=True)
    return completed.stdout.strip()


def init_repo(path: Path) -> Path:
    path.mkdir(parents=True, exist_ok=True)
    git(path, "init", "--quiet", "--initial-branch=main")
    git(path, "config", "user.name", "Test")
    git(path, "config", "user.email", "test@example.com")
    git(path, "config", "commit.gpgsign", "false")
    return path


def commit(repo: Path, message: str, files: dict[str, str]) -> str:
    for name, content in files.items():
        target = repo / name
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content)
        git(repo, "add", name)
    git(repo, "commit", "--quiet", "-m", message)
    return git(repo, "rev-parse", "HEAD")


def run_context(workspace: Path, out_dir: Path) -> RunContext:
    out_dir.mkdir(parents=True, exist_ok=True)
    return RunContext(
        run_id=RUN_ID,
        issue_id=ISSUE_ID,
        workspace=workspace,
        out_dir=out_dir,
        github_run_url="https://github.com/DmitryZaika/granite-webhooks/actions/runs/1",
    )
