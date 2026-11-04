"""Moving the fix out of the fix job and into a pull request.

The fix job ran agent-written code, so everything it hands over is data from
an untrusted job: the commits travel as a git bundle, and the publish job
checks them again (contract, base, SHA) before pushing them with the only
write token in the run. Nothing from the bundle is executed.
"""

import base64
import json
import os
from collections.abc import Mapping
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any

from . import agent_result, isolation, pr_body, proc
from .context import OWNER, REPOS, RunContext, is_sha, write_output
from .contract import check_commits
from .git import Git
from .github import GitHub

FIX_REF = "refs/autofix/fix"


class PublishError(RuntimeError):
    pass


# --- fix job ------------------------------------------------------------------


def create_bundle(
    run: RunContext, agent_user: proc.RunAs | None = None, outbox: Path | None = None
) -> bool:
    """Bundles `base..fix` of the target repo when verify says to publish.

    With `agent_user`, git runs as that user and writes into its `outbox`,
    and the bundle is copied from there into the out dir.
    """
    report = _read_json(run.verification_json)
    if not report or report.get("publish") is not True:
        return False
    git = Git(run.repo_dir(report["target_repo"]), agent_user)
    written = (outbox or run.out_dir) / run.bundle.name
    git.run("update-ref", FIX_REF, report["fix_sha"])
    git.run("bundle", "create", str(written), FIX_REF, f"^{report['base_sha']}")
    if written != run.bundle:
        isolation.copy_out(written, run.bundle)
    return True


# --- publish job --------------------------------------------------------------


@dataclass(frozen=True)
class Plan:
    publish: bool
    target_repo: str = ""
    base_sha: str = ""
    fix_sha: str = ""


def plan(run: RunContext, base_shas: Mapping[str, str]) -> Plan:
    """Whether to publish, from verification.json checked against the base
    SHAs the fix job recorded before the agent started."""
    report = _read_json(run.verification_json)
    if not report or report.get("publish") is not True or report.get("conclusion") != "completed":
        return Plan(False)
    target = report.get("target_repo")
    if target not in REPOS:
        raise PublishError("verification.json names an unknown target_repo")
    base = base_shas.get(target, "")
    if not is_sha(base) or report.get("base_sha") != base:
        raise PublishError("verification.json's base_sha is not the recorded base")
    if not is_sha(report.get("fix_sha")):
        raise PublishError("verification.json has no valid fix_sha")
    if not run.bundle.exists():
        raise PublishError("the fix bundle is missing")
    return Plan(True, target, base, report["fix_sha"])


def write_plan(chosen: Plan) -> None:
    write_output("publish", "true" if chosen.publish else "false")
    write_output("target_repo", chosen.target_repo)
    write_output("base_sha", chosen.base_sha)


@dataclass(frozen=True)
class Published:
    pr_url: str
    pr_draft: bool
    verification: str
    notes: str | None


def open_pr(
    run: RunContext,
    chosen: Plan,
    target_dir: Path,
    token: str,
    github: GitHub,
    push_url: str | None = None,
) -> Published:
    agent = agent_result.load(run.agent_output, run.agent_stderr)
    report = _read_json(run.verification_json) or {}
    if agent.result is None or agent.result["outcome"] not in agent_result.FIX_OUTCOMES:
        raise PublishError("the agent's result is not a fix")
    if agent.result["target_repo"] != chosen.target_repo:
        raise PublishError("the agent's target_repo does not match the plan")
    git = Git(target_dir)
    head = _import_bundle(git, run.bundle, chosen.fix_sha)
    contract = check_commits(
        git, chosen.base_sha, head, agent.result["outcome"], chosen.target_repo
    )
    if not contract.publishable:
        raise PublishError("refusing to push: " + "; ".join(contract.errors or ["no commits"]))
    verification, notes = _rechecked(report, contract.errors)
    branch = pr_body.branch_name(run.issue_id, run.run_id, run.bug_id)
    url = push_url or f"https://github.com/{OWNER}/{chosen.target_repo}.git"
    push(git, head, branch, url, token)
    draft = verification == "unverified"
    body = pr_body.render(
        pr_body.BodyInputs(
            run.issue_id,
            agent.result,
            verification,
            notes,
            run.github_run_url,
            agent.cost_usd,
            run.bug_id,
        )
    )
    pull = github.open_pull_request(
        chosen.target_repo, branch, pr_body.title(agent.result), body, draft
    )
    github.add_labels(
        chosen.target_repo, pull.number, pr_body.labels(agent.result["outcome"], verification)
    )
    published = Published(pull.url, pull.draft, verification, notes)
    run.pr_json.write_text(json.dumps(asdict(published), indent=2) + "\n")
    return published


def _import_bundle(git: Git, bundle: Path, fix_sha: str) -> str:
    git.run("fetch", "--quiet", "--no-tags", str(bundle), f"+{FIX_REF}:{FIX_REF}")
    head = git.rev_parse(FIX_REF)
    if head != fix_sha:
        raise PublishError("the bundle's commit is not the verified fix commit")
    return head


def _rechecked(report: dict[str, Any], errors: list[str]) -> tuple[str, str | None]:
    """The fix job's verification, downgraded if this job's own contract
    check disagrees with it."""
    verification = report.get("verification")
    notes = report.get("notes") if isinstance(report.get("notes"), str) else None
    if verification not in ("verified", "unverified", "not_applicable"):
        return "unverified", "verification.json has no valid verification"
    if errors and verification != "unverified":
        return "unverified", "\n".join(errors)
    return verification, notes


def push(git: Git, sha: str, branch: str, url: str, token: str) -> None:
    """Pushes `sha` to `branch`, the token in an http.extraheader.

    The header is passed through GIT_CONFIG_* environment variables: never in
    the remote URL, a file, or the argv (which other processes can read).
    The branch is ours alone, so a re-run may force it.
    """
    credentials = base64.b64encode(f"x-access-token:{token}".encode()).decode()
    env = {
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "GIT_TERMINAL_PROMPT": "0",
        "GIT_CONFIG_NOSYSTEM": "1",
        "GIT_CONFIG_COUNT": "1",
        "GIT_CONFIG_KEY_0": "http.https://github.com/.extraheader",
        "GIT_CONFIG_VALUE_0": f"AUTHORIZATION: basic {credentials}",
    }
    git.run("push", "--quiet", "--no-verify", url, f"+{sha}:refs/heads/{branch}", env=env)


def _read_json(path: Path) -> dict[str, Any] | None:
    try:
        value = json.loads(path.read_text())
    except (OSError, ValueError):
        return None
    return value if isinstance(value, dict) else None
