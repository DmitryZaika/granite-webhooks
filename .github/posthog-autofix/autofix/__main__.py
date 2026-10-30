"""`python3 -m autofix <command>`: one command per workflow step.

Every input comes from the environment (set by the step's `env:`), never
from the command line, so no `${{ }}` value is ever spliced into a shell.
"""

import os
import sys
from collections.abc import Callable
from pathlib import Path

from . import (
    agent,
    callback,
    database,
    identity,
    isolation,
    issue_context,
    open_prs,
    publish,
    verify,
)
from .context import REPOS, InputError, RunContext, write_output
from .github import GitHub

TOOLS_DIR = Path(__file__).resolve().parent.parent


def _base_shas() -> dict[str, str]:
    return {
        repo: os.environ.get(f"BASE_SHA_{repo.upper().replace('-', '_')}", "") for repo in REPOS
    }


def cmd_validate_inputs() -> None:
    RunContext.from_env()


def cmd_issue_context() -> None:
    # Best effort: a failure only means the agent fetches the issue itself.
    issue_context.write_context(RunContext.from_env())


def cmd_open_prs() -> None:
    # Best effort: a failure only means the agent does not see them.
    open_prs.write_open_prs(RunContext.from_env())


def cmd_layout() -> None:
    workspace = Path(os.environ["GITHUB_WORKSPACE"])
    isolation.lay_out(workspace, TOOLS_DIR.parent.parent)


def cmd_database_start() -> None:
    database.start()


def cmd_database_migrate() -> None:
    database.migrate(isolation.WORK / "granite-webhooks")


def cmd_isolate() -> None:
    isolation.isolate()


def cmd_check_isolation() -> None:
    failures = isolation.check_isolation()
    for failure in failures:
        print(f"::error::{failure}", file=sys.stderr)
    if failures:
        raise isolation.IsolationError(f"{len(failures)} isolation check(s) failed")
    print(f"Isolation holds for {isolation.USER}.")


def cmd_agent() -> None:
    agent.run_agent(RunContext.from_env(), TOOLS_DIR, isolation.agent_user())


def cmd_verify() -> None:
    report = verify.run_verify(RunContext.from_env(), _base_shas(), isolation.agent_user())
    print(
        f"conclusion={report.conclusion} verification={report.verification} "
        f"publish={report.publish}"
    )


def cmd_bundle() -> None:
    created = publish.create_bundle(
        RunContext.from_env(), isolation.agent_user(), isolation.HANDOFF
    )
    print("Bundled the fix commits." if created else "Nothing to publish.")


def cmd_plan() -> None:
    chosen = publish.plan(RunContext.from_env(), _base_shas())
    publish.write_plan(chosen)
    print(f"publish={chosen.publish} target={chosen.target_repo or '-'}")


def cmd_open_pr() -> None:
    run = RunContext.from_env()
    chosen = publish.plan(run, _base_shas())
    if not chosen.publish:
        raise publish.PublishError("open-pr ran without a publishable plan")
    token = os.environ.get("GH_TOKEN", "")
    if not token:
        raise publish.PublishError("GH_TOKEN is not set")
    target_dir = Path(os.environ.get("TARGET_DIR", ""))
    published = publish.open_pr(run, chosen, target_dir, token, GitHub(token))
    print(f"Pull request: {published.pr_url} (draft: {published.pr_draft})")


def cmd_git_identity() -> None:
    run = RunContext.from_env()
    token = os.environ.get("GH_TOKEN", "")
    name, email = identity.bot_identity(
        os.environ.get("APP_SLUG", ""), GitHub(token) if token else None
    )
    identity.configure([run.repo_dir(repo) for repo in REPOS], name, email)
    print(f"Commits are authored by {name} <{email}>")


def cmd_callback_started() -> None:
    run = RunContext.from_env()
    callback.post(callback.Api.from_env(os.environ), run, "started", callback.started_payload(run))
    write_output("started", "true")


def cmd_callback_result() -> None:
    run = RunContext.from_env()
    try:
        payload = callback.result_from_files(run)
    except callback.CallbackError as error:
        # The run's files are unusable: still report, as a failure, so the
        # ledger row does not wait for the watchdog.
        payload = callback.failure_from_files(run, str(error))
    _report(run, payload)


def cmd_callback_failed() -> None:
    run = RunContext.from_env()
    _report(run, callback.failure_from_files(run, _failure_reason()))


def _report(run: RunContext, payload: dict[str, object]) -> None:
    callback.post(callback.Api.from_env(os.environ), run, "result", payload)
    print(f"Reported conclusion={payload['conclusion']} verification={payload['verification']}")


def _failure_reason() -> str:
    fix_job = os.environ.get("FIX_JOB_RESULT", "unknown")
    if fix_job != "success":
        return f"The fix job ended with: {fix_job}."
    return "The report job failed before publishing; see the run's log."


COMMANDS: dict[str, Callable[[], None]] = {
    "layout": cmd_layout,
    "database start": cmd_database_start,
    "database migrate": cmd_database_migrate,
    "validate-inputs": cmd_validate_inputs,
    "issue-context": cmd_issue_context,
    "open-prs": cmd_open_prs,
    "callback started": cmd_callback_started,
    "git-identity": cmd_git_identity,
    "isolate": cmd_isolate,
    "check-isolation": cmd_check_isolation,
    "agent": cmd_agent,
    "verify": cmd_verify,
    "bundle": cmd_bundle,
    "plan": cmd_plan,
    "open-pr": cmd_open_pr,
    "callback result": cmd_callback_result,
    "callback failed": cmd_callback_failed,
}

_ERRORS = (
    InputError,
    agent.ConfigError,
    callback.CallbackError,
    publish.PublishError,
    OSError,
    RuntimeError,
    KeyError,
)


def main(argv: list[str]) -> int:
    command = " ".join(argv)
    if command not in COMMANDS:
        print(f"usage: python3 -m autofix <{' | '.join(COMMANDS)}>", file=sys.stderr)
        return 2
    try:
        COMMANDS[command]()
    except _ERRORS as error:
        # One line: an error can quote git or an API, and a line of its own
        # starting with `::` would be read as a workflow command.
        message = " ".join(str(error).split())
        print(f"::error::{message}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
