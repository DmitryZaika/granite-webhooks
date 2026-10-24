"""Reporting to the api: `POST /v1/posthog-autofix/runs/{id}/started|result`.

The bodies must deserialize into `StartedReport` and `RunReport`
(api/src/posthog_autofix/model.rs), both `deny_unknown_fields`: the key sets
here are pinned by tests against those structs.
"""

import json
import time
from collections.abc import Callable, Mapping
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from . import agent_result
from .context import RunContext
from .http import HttpError, Response, Transport, send_json, urllib_transport, with_retries

CONCLUSIONS = ("completed", "failed", "usage_limited")
VERIFICATIONS = ("verified", "unverified", "not_applicable")
_NOTES_LIMIT = 4000


class CallbackError(RuntimeError):
    pass


@dataclass(frozen=True)
class Api:
    base_url: str
    secret: str

    @classmethod
    def from_env(cls, env: Mapping[str, str]) -> "Api":
        base_url = env.get("AUTOFIX_API_URL", "").rstrip("/")
        secret = env.get("AUTOFIX_CALLBACK_SECRET", "")
        if not base_url.startswith("https://"):
            raise CallbackError("AUTOFIX_API_URL must be an https:// URL")
        if not secret:
            raise CallbackError("AUTOFIX_CALLBACK_SECRET is not set")
        return cls(base_url, secret)


def started_payload(run: RunContext) -> dict[str, Any]:
    return {"issue_id": run.issue_id, "github_run_url": run.github_run_url}


def run_report(
    run: RunContext,
    conclusion: str,
    agent: dict[str, Any] | None,
    verification: str,
    notes: str | None,
    pr_url: str | None,
    pr_draft: bool,
    cost_usd: float | None,
) -> dict[str, Any]:
    if conclusion not in CONCLUSIONS or verification not in VERIFICATIONS:
        raise CallbackError("unknown conclusion or verification")
    if notes and len(notes) > _NOTES_LIMIT:
        notes = notes[: _NOTES_LIMIT - 1] + "…"
    return {
        "issue_id": run.issue_id,
        "conclusion": conclusion,
        "agent": agent,
        "verification": verification,
        "notes": notes or None,
        "pr_url": pr_url,
        "pr_draft": pr_draft,
        "cost_usd": cost_usd,
        "github_run_url": run.github_run_url,
    }


def post(
    api: Api,
    run: RunContext,
    kind: str,
    payload: dict[str, Any],
    transport: Transport = urllib_transport,
    sleep: Callable[[float], None] = time.sleep,
) -> None:
    """POSTs with retries; a 4xx (or giving up) raises CallbackError."""
    url = f"{api.base_url}/v1/posthog-autofix/runs/{run.run_id}/{kind}"
    headers = {"Authorization": f"Bearer {api.secret}"}

    def send() -> Response:
        return send_json(transport, "POST", url, headers, payload)

    try:
        response = with_retries(send, sleep=sleep)
    except HttpError as error:
        raise CallbackError(f"the {kind} callback failed: {error}") from None
    if not 200 <= response.status < 300:
        raise CallbackError(f"the {kind} callback was refused ({response.status}): {response.body}")


# --- Building the result from the run's files -----------------------------


def _read_json(path: Path) -> dict[str, Any] | None:
    try:
        value = json.loads(path.read_text())
    except (OSError, ValueError):
        return None
    return value if isinstance(value, dict) else None


def result_from_files(run: RunContext) -> dict[str, Any]:
    """The result body for a run whose fix job finished."""
    agent = agent_result.load(run.agent_output, run.agent_stderr)
    verification = _read_json(run.verification_json)
    if verification is None:
        raise CallbackError("verification.json is missing or unreadable")
    # The publish job re-checks the commits and may downgrade the fix job's
    # verification: its pr.json wins.
    pr = _read_json(run.pr_json) or {}
    pr_url = _text(pr.get("pr_url"))
    proved = pr if pr_url else verification
    return run_report(
        run,
        conclusion=str(verification.get("conclusion")),
        agent=agent.result,
        verification=str(proved.get("verification")),
        notes=_text(proved.get("notes")),
        pr_url=pr_url,
        pr_draft=pr.get("pr_draft") is True,
        cost_usd=agent.cost_usd,
    )


def failure_from_files(run: RunContext, reason: str) -> dict[str, Any]:
    """The result body for a run that broke; keeps whatever the agent said."""
    agent = agent_result.load(run.agent_output, run.agent_stderr)
    verification = _read_json(run.verification_json) or {}
    notes = [reason]
    if isinstance(verification.get("notes"), str):
        notes.append(verification["notes"])
    elif agent.notes:
        notes.append(agent.notes)
    proved = verification.get("verification")
    return run_report(
        run,
        conclusion="failed",
        agent=agent.result,
        verification=proved if proved in VERIFICATIONS else "not_applicable",
        notes="\n".join(notes),
        pr_url=None,
        pr_draft=False,
        cost_usd=agent.cost_usd,
    )


def _text(value: object) -> str | None:
    return value if isinstance(value, str) else None
