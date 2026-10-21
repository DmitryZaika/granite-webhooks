"""The pull request's branch, title, labels and body.

The agent's text is untrusted (it may carry text from an error message
someone else controls), so it is escaped before it reaches GitHub: no HTML,
no Markdown links or images, no @mentions, and capped lengths.
"""

import html
import re
import string
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from .context import POSTHOG_PROJECT

TITLE_LIMIT = 100
FIELD_LIMIT = 3000
EVIDENCE_LIMIT = 6000
# Zero-width space: `@name` renders unchanged but notifies nobody.
_MENTION_BREAK = "@\u200b"
_TEMPLATE = Path(__file__).with_name("pr_body.md")


def branch_name(issue_id: str, run_id: str) -> str:
    return f"ai-fix/posthog-{issue_id[:8]}-{run_id}"


def labels(outcome: str, verification: str) -> list[str]:
    result = ["ai-fix"]
    if verification == "unverified":
        result.append("ai-fix:unverified")
    if outcome == "fixed_without_test":
        result.append("needs-manual-test")
    return result


def escape(text: str | None, limit: int = FIELD_LIMIT) -> str:
    """Agent prose, safe to drop into Markdown."""
    if not text:
        return "_None given._"
    capped = text if len(text) <= limit else text[: limit - 1] + "…"
    escaped = html.escape(capped, quote=False)
    escaped = escaped.replace("[", "\\[").replace("]", "\\]")
    return escaped.replace("@", _MENTION_BREAK)


def code_block(text: str, limit: int = EVIDENCE_LIMIT) -> str:
    """A fenced block no backtick run inside `text` can close."""
    capped = text if len(text) <= limit else "…" + text[-(limit - 1) :]
    longest = max((len(run) for run in re.findall(r"`+", capped)), default=0)
    fence = "`" * max(3, longest + 1)
    return f"{fence}text\n{capped}\n{fence}"


def title(result: dict[str, Any]) -> str:
    raw = result.get("pr_title") or f"fix: {result.get('summary') or 'PostHog error'}"
    one_line = " ".join(raw.split()).replace("@", _MENTION_BREAK)
    if len(one_line) <= TITLE_LIMIT:
        return one_line
    return one_line[: TITLE_LIMIT - 1] + "…"


@dataclass(frozen=True)
class BodyInputs:
    issue_id: str
    result: dict[str, Any]
    verification: str
    notes: str | None
    run_url: str
    cost_usd: float | None


def render(inputs: BodyInputs) -> str:
    result = inputs.result
    values = {
        "summary": escape(result["summary"], 300),
        "posthog_url": f"https://us.posthog.com/project/{POSTHOG_PROJECT}/error-tracking/{inputs.issue_id}",
        "root_cause": escape(result["root_cause"]),
        "fix_summary": escape(result["fix_summary"]),
        "test_section": _test_section(result["test"]),
        "verification_section": _verification_section(inputs.verification, inputs.notes),
        "risk": escape(result["risk"]),
        "run_url": inputs.run_url,
        "cost": "unknown" if inputs.cost_usd is None else f"${inputs.cost_usd:.2f}",
    }
    template = string.Template(_TEMPLATE.read_text())
    return template.substitute(values)


def _test_section(test: dict[str, Any]) -> str:
    if test["status"] == "written":
        files = "\n".join(test["files"]) or "(none listed)"
        return f"Command:\n\n{code_block(test['command'] or '')}\n\nFiles:\n\n{code_block(files)}"
    return (
        "No automated test. Why:\n\n"
        f"{escape(test['why_not_possible'])}\n\n"
        "**Test it by hand before merging:**\n\n"
        f"{escape(test['manual_test_steps'])}"
    )


def _verification_section(verification: str, notes: str | None) -> str:
    if verification == "verified":
        return (
            "**Verified.** The test failed on the test commit for the right reason, "
            "passed on the fix commit, and the repository's checks passed."
        )
    if verification == "not_applicable":
        return (
            "**Not verified by a test.** The repository's checks passed; "
            "the fix needs a manual test."
        )
    evidence = code_block(notes or "No details recorded.")
    return (
        "**Unverified**: a check failed, so this is a draft.\n\n"
        f"<details><summary>Evidence</summary>\n\n{evidence}\n\n</details>"
    )
