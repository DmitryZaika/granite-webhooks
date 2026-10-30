"""The open auto-fix pull requests, for the agent's first message.

The agent compares its bug with them and answers `duplicate` instead of
opening a second fix. Best effort: any failure writes nothing, and the agent
works as before. Titles, issue lines and file names come from other runs'
agents, so the text is untrusted data like the issue context.
"""

import json
import os
import re
from collections.abc import Mapping
from dataclasses import dataclass, replace

from . import http
from .context import REPOS, RunContext
from .github import GitHub

LABEL = "ai-fix"
MAX_PRS = 20
MAX_FILES = 30
# Whole entries only, so the first message stays far below the argv limit.
MAX_TOTAL = 10_000
_TEXT_LIMIT = 300
_BUG_ID_LINE = re.compile(r"\*\*Bug ID:\*\* `([0-9a-f]{12})`")
_ISSUE_LINE = re.compile(r"^\*\*Issue:\*\* (.+)$", re.MULTILINE)


@dataclass(frozen=True)
class OpenPr:
    repo: str
    number: int
    url: str
    title: str
    bug_id: str
    issue: str
    files: list[str]


def one_line(text: object, limit: int = _TEXT_LIMIT) -> str:
    collapsed = " ".join(str(text).split())
    return collapsed if len(collapsed) <= limit else collapsed[: limit - 1] + "…"


def parse_body(body: object) -> tuple[str, str]:
    """The Bug ID and the Issue line of a pull request body ("" when absent)."""
    text = body if isinstance(body, str) else ""
    bug = _BUG_ID_LINE.search(text)
    issue = _ISSUE_LINE.search(text)
    return (bug.group(1) if bug else "", one_line(issue.group(1)) if issue else "")


def fetch_open_prs(github: GitHub) -> list[OpenPr]:
    found: list[OpenPr] = []
    for repo in REPOS:
        for item in github.open_labelled(repo, LABEL, MAX_PRS):
            number = item.get("number")
            url = item.get("html_url")
            if not isinstance(number, int) or not isinstance(url, str):
                continue
            bug_id, issue = parse_body(item.get("body"))
            title = one_line(item.get("title", ""))
            found.append(OpenPr(repo, number, url, title, bug_id, issue, []))
    return [
        replace(pr, files=github.changed_files(pr.repo, pr.number, MAX_FILES))
        for pr in found[:MAX_PRS]
    ]


def render(prs: list[OpenPr]) -> str:
    blocks = []
    for pr in prs:
        lines = [f"- {pr.url} ({pr.repo}#{pr.number}): {pr.title}"]
        if pr.bug_id:
            lines.append(f"  Bug ID: {pr.bug_id}")
        if pr.issue:
            lines.append(f"  Issue: {pr.issue}")
        if pr.files:
            lines.append("  Files: " + ", ".join(one_line(name, 200) for name in pr.files))
        block = "\n".join(lines)
        if sum(len(kept) + 1 for kept in blocks) + len(block) > MAX_TOTAL:
            break
        blocks.append(block)
    return "\n".join(blocks)


def write_open_prs(
    run: RunContext,
    env: Mapping[str, str] | None = None,
    transport: http.Transport = http.urllib_transport,
) -> bool:
    """Writes the list file; returns False (writing nothing) on any failure."""
    env = os.environ if env is None else env
    token = env.get("GH_TOKEN", "")
    run.open_prs.unlink(missing_ok=True)
    if not token:
        print("No GitHub token: the agent will not see the open auto-fix pull requests.")
        return False
    try:
        prs = fetch_open_prs(GitHub(token, transport))
        if not prs:
            print("No open auto-fix pull requests.")
            return False
        run.out_dir.mkdir(parents=True, exist_ok=True)
        run.open_prs.write_text(json.dumps({"summary": render(prs)}))
    except (http.HttpError, ValueError, TypeError, KeyError, OSError) as error:
        # Never the token, and no response text.
        print(f"Open pull requests unavailable ({error.__class__.__name__}); continuing without.")
        return False
    print(f"Wrote {len(prs)} open auto-fix pull request(s).")
    return True
