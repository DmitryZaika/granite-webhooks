"""`verify`: proves (or fails to prove) the agent's fix, deterministically.

Nothing the agent wrote is executed as a command: its test command runs only
as the allowlisted argv from `test_command`, and every other check is fixed
here. The result goes to verification.json for the publish job.
"""

import json
from collections.abc import Collection
from dataclasses import asdict, dataclass
from pathlib import Path

from . import agent_result, gates, isolation, proc
from .agent_result import AgentRun
from .context import REPOS, RunContext
from .contract import check_clean, check_commits, check_untouched
from .git import Git, GitError
from .session import OutOfTime, Session
from .test_command import DEFAULT_CRATES, CommandRejected
from .test_command import parse as parse_test_command

VERIFY_BUDGET_SECONDS = 28 * 60
_NOTES_LIMIT = 4000
_TEST_KIND = {"granite-webhooks": "cargo", "general_datebase": "vitest"}


@dataclass(frozen=True)
class Report:
    conclusion: str  # completed | failed | usage_limited
    verification: str  # verified | unverified | not_applicable
    notes: str | None
    publish: bool
    target_repo: str | None = None
    base_sha: str | None = None
    fix_sha: str | None = None

    def write(self, path: Path) -> None:
        path.write_text(json.dumps(asdict(self), indent=2) + "\n")


@dataclass(frozen=True)
class Inputs:
    workspace: Path
    base_shas: dict[str, str]
    agent: AgentRun
    crates: Collection[str] = DEFAULT_CRATES


def run_verify(run: RunContext, base_shas: dict[str, str], agent_user: proc.RunAs) -> Report:
    """Every check (git included) runs as `agent_user`: they execute and
    read what the agent wrote. Whatever they leave running is stopped."""
    agent = agent_result.load(run.agent_output, run.agent_stderr)
    session = Session.start(proc.run, VERIFY_BUDGET_SECONDS, agent_user)
    try:
        report = verify(Inputs(run.workspace, base_shas, agent), session)
    finally:
        isolation.kill_leftovers(agent_user)
    run.out_dir.mkdir(parents=True, exist_ok=True)
    report.write(run.verification_json)
    session.write_log(run.verification_log)
    return report


def verify(inputs: Inputs, session: Session) -> Report:
    agent = inputs.agent
    if agent.kind != "success" or agent.result is None:
        return Report(agent.kind, "not_applicable", agent.notes, publish=False)
    outcome = agent.result["outcome"]
    target = agent.result["target_repo"]
    if outcome not in agent_result.FIX_OUTCOMES:
        return _no_fix(inputs, session)
    return _fix(inputs, session, outcome, target)


def _no_fix(inputs: Inputs, session: Session) -> Report:
    """No fix claimed: any commits are stray and nothing is published."""
    notes = []
    for repo in REPOS:
        notes += check_untouched(
            Git(inputs.workspace / repo, session.run_as), inputs.base_shas[repo], repo
        )
    for note in notes:
        session.log(note)
    return Report("completed", "not_applicable", _join(notes), publish=False)


def _fix(inputs: Inputs, session: Session, outcome: str, target: str) -> Report:
    git = Git(inputs.workspace / target, session.run_as)
    base = inputs.base_shas[target]
    try:
        head = git.rev_parse("HEAD")
    except GitError as error:
        return Report("completed", "unverified", str(error), publish=False, target_repo=target)
    contract = check_commits(git, base, head, outcome, target)
    errors = list(contract.errors) + check_clean(git, target)
    for repo in REPOS:
        if repo != target:
            errors += check_untouched(
                Git(inputs.workspace / repo, session.run_as), inputs.base_shas[repo], repo
            )
    for note in errors:
        session.log(f"contract: {note}")
    if not errors:
        commits = [commit.sha for commit in contract.commits]
        errors = _execute_checks(inputs, session, git, target, outcome, base, commits)
    if errors:
        verification = "unverified"
    else:
        # A fix without a test has nothing that proves it: a person tests it.
        verification = "verified" if outcome == "fixed_with_test" else "not_applicable"
    return Report(
        conclusion="completed",
        verification=verification,
        notes=_join(errors),
        publish=contract.publishable,
        target_repo=target,
        base_sha=base,
        fix_sha=head if contract.commits else None,
    )


def _execute_checks(
    inputs: Inputs,
    session: Session,
    git: Git,
    target: str,
    outcome: str,
    base: str,
    commits: list[str],
) -> list[str]:
    """Red/green and the repo's gates; leaves the fix commit checked out."""
    fix = commits[-1]
    branch = git.current_branch()
    errors: list[str] = []
    try:
        if outcome == "fixed_with_test":
            errors += _red_green(inputs, session, git, target, commits[0], fix)
        errors += _gates(session, git, target, base, fix)
    except OutOfTime as error:
        errors.append(str(error))
    except GitError as error:
        errors.append(f"git: {error}")
    finally:
        try:
            git.checkout(branch or fix)
        except GitError as error:
            errors.append(f"git: could not return to the fix commit: {error}")
    return errors


def _red_green(
    inputs: Inputs,
    session: Session,
    git: Git,
    target: str,
    test_sha: str,
    fix_sha: str,
) -> list[str]:
    command = inputs.agent.result["test"]["command"] if inputs.agent.result else None
    try:
        test = parse_test_command(command or "", inputs.crates)
    except CommandRejected as error:
        session.log(f"test command rejected, not run: {error}")
        return [f"The test command was not run: {error}"]
    if test.kind != _TEST_KIND[target]:
        return [f"The test command was not run: a {test.kind} command cannot test {target}"]
    git.checkout_detached(test_sha)
    red, _, red_output = gates.run_test(
        session, git.repo, test, "test on the test commit (must fail)"
    )
    git.checkout_detached(fix_sha)
    _, green, green_output = gates.run_test(
        session, git.repo, test, "test on the fix commit (must pass)"
    )
    errors = []
    if not red:
        errors.append(
            "The test did not fail on the test commit for the right reason "
            "(it passed, or did not compile or load):\n" + proc.tail(red_output, 1500)
        )
    if not green:
        errors.append("The test did not pass on the fix commit:\n" + proc.tail(green_output, 1500))
    return errors


def _gates(session: Session, git: Git, target: str, base: str, fix: str) -> list[str]:
    git.checkout_detached(fix)
    touched = git.files_between(base, fix, existing_only=True)
    if target == "general_datebase":
        return gates.datebase_gates(session, git.repo, touched)
    return gates.webhooks_gates(session, git, base, fix, touched)


def _join(notes: list[str]) -> str | None:
    if not notes:
        return None
    return proc.tail("\n".join(notes), _NOTES_LIMIT)
