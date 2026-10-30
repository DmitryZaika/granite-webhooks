"""The run's identity and file locations, read from the environment.

`RUN_ID`, `ISSUE_ID`, `EVENT_ID` and `BUG_ID` come from `workflow_dispatch` inputs, so
they are validated here before anything uses them (they reach the agent's
prompt, the branch name and the callback URL).
"""

import os
import re
from collections.abc import Mapping
from dataclasses import dataclass
from pathlib import Path

OWNER = "DmitryZaika"
REPOS = ("granite-webhooks", "general_datebase")
POSTHOG_PROJECT = 141685

_RUN_ID = re.compile(r"[0-9]{1,20}")
_ISSUE_ID = re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")
_BUG_ID = re.compile(r"[0-9a-f]{12}")
_SHA = re.compile(r"[0-9a-f]{40}")


class InputError(ValueError):
    pass


def validate_run_id(value: str) -> str:
    if not _RUN_ID.fullmatch(value):
        raise InputError("run_id must be 1-20 digits")
    return value


def validate_issue_id(value: str) -> str:
    if not _ISSUE_ID.fullmatch(value):
        raise InputError("issue_id must be a lowercase UUID")
    return value


def validate_event_id(value: str) -> str:
    """The sample event's uuid; "" is allowed (older dispatches carry none)."""
    if value and not _ISSUE_ID.fullmatch(value):
        raise InputError("event_id must be empty or a lowercase UUID")
    return value


def validate_bug_id(value: str) -> str:
    """The bug's short id (12 hex of its signature); "" is allowed (an older api sends none)."""
    if value and not _BUG_ID.fullmatch(value):
        raise InputError("bug_id must be empty or 12 lowercase hex characters")
    return value


def is_sha(value: object) -> bool:
    return isinstance(value, str) and _SHA.fullmatch(value) is not None


@dataclass(frozen=True)
class RunContext:
    run_id: str
    issue_id: str
    # Holds granite-webhooks/ and general_datebase/ side by side
    # (AUTOFIX_WORKSPACE: /srv/autofix/work in the fix job).
    workspace: Path
    # Files handed from step to step and uploaded as the run's artifact.
    out_dir: Path
    github_run_url: str
    # uuid of a sample `$exception` event of the bug to fix; "" when not given.
    event_id: str = ""
    # Short stable id of the bug (first 12 hex of its signature); "" when not given.
    bug_id: str = ""

    @classmethod
    def from_env(cls, env: Mapping[str, str] | None = None) -> "RunContext":
        env = os.environ if env is None else env
        run_url = "{}/{}/actions/runs/{}".format(
            env.get("GITHUB_SERVER_URL", "https://github.com"),
            env.get("GITHUB_REPOSITORY", f"{OWNER}/granite-webhooks"),
            env.get("GITHUB_RUN_ID", "0"),
        )
        return cls(
            run_id=validate_run_id(env.get("RUN_ID", "")),
            issue_id=validate_issue_id(env.get("ISSUE_ID", "")),
            workspace=Path(
                env.get("AUTOFIX_WORKSPACE") or env.get("GITHUB_WORKSPACE") or os.getcwd()
            ),
            out_dir=Path(env.get("RUNNER_TEMP", "/tmp")) / "autofix-out",
            github_run_url=run_url,
            event_id=validate_event_id(env.get("EVENT_ID", "")),
            bug_id=validate_bug_id(env.get("BUG_ID", "")),
        )

    def repo_dir(self, repo: str) -> Path:
        return self.workspace / repo

    @property
    def agent_output(self) -> Path:
        return self.out_dir / "agent-output.json"

    @property
    def agent_stderr(self) -> Path:
        return self.out_dir / "agent-stderr.log"

    @property
    def issue_context(self) -> Path:
        return self.out_dir / "issue-context.json"

    @property
    def open_prs(self) -> Path:
        return self.out_dir / "open-prs.json"

    @property
    def verification_json(self) -> Path:
        return self.out_dir / "verification.json"

    @property
    def verification_log(self) -> Path:
        return self.out_dir / "verification.log"

    @property
    def bundle(self) -> Path:
        return self.out_dir / "fix.bundle"

    @property
    def pr_json(self) -> Path:
        return self.out_dir / "pr.json"


def write_output(name: str, value: str, env: Mapping[str, str] | None = None) -> None:
    """Sets a step output (`steps.<id>.outputs.<name>`)."""
    env = os.environ if env is None else env
    path = env.get("GITHUB_OUTPUT")
    if not path:
        return
    if "\n" in value or "\n" in name:
        raise ValueError("step outputs must be single-line")
    with open(path, "a", encoding="utf-8") as handle:
        handle.write(f"{name}={value}\n")
