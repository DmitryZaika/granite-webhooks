"""The commit contract: the shape the agent's commits must have.

Pure git reads, so it runs twice: in the fix job before anything is
executed, and again in the publish job on the bundle, where the commits are
data from an untrusted job.
"""

from dataclasses import dataclass, field

from .git import Commit, Git, GitError
from .paths import is_vitest_file, is_write_protected

EXPECTED_COMMITS = {"fixed_with_test": 2, "fixed_without_test": 1}


@dataclass
class ContractResult:
    commits: list[Commit] = field(default_factory=list)
    errors: list[str] = field(default_factory=list)
    protected: list[str] = field(default_factory=list)

    @property
    def ok(self) -> bool:
        return not self.errors

    @property
    def publishable(self) -> bool:
        """Commits that may become a pull request, even a draft one.

        A commit touching a protected path is never pushed: the workflow
        token could not push a workflow change anyway, and a migration or
        lockfile change must come from a person.
        """
        return bool(self.commits) and not self.protected


def expected_commits(outcome: str) -> int:
    return EXPECTED_COMMITS.get(outcome, 0)


def check_commits(git: Git, base: str, head: str, outcome: str, repo: str) -> ContractResult:
    result = ContractResult()
    try:
        _check(git, base, head, outcome, repo, result)
    except GitError as error:
        result.errors.append(str(error))
    return result


def _check(git: Git, base: str, head: str, outcome: str, repo: str, result: ContractResult) -> None:
    if not git.is_ancestor(base, head):
        result.errors.append(f"{repo}: HEAD is not on top of the base commit {base[:12]}")
        return
    if git.merge_commits_since(base, head):
        result.errors.append(f"{repo}: the commits include a merge")
        return
    result.commits = [git.commit(sha) for sha in git.commits_since(base, head)]
    expected = expected_commits(outcome)
    if len(result.commits) != expected:
        result.errors.append(
            f"{repo}: {outcome} needs {expected} new commit(s), found {len(result.commits)}"
        )
    for commit in result.commits:
        result.protected += [path for path in commit.files if is_write_protected(path)]
    if result.protected:
        listed = ", ".join(sorted(set(result.protected))[:10])
        result.errors.append(f"{repo}: the commits touch protected paths: {listed}")
    if outcome == "fixed_with_test" and repo == "general_datebase" and result.commits:
        _check_test_commit(result.commits[0], result)


def _check_test_commit(commit: Commit, result: ContractResult) -> None:
    others = [path for path in commit.files if not is_vitest_file(path)]
    if others:
        listed = ", ".join(others[:10])
        result.errors.append(
            f"general_datebase: the test commit may only touch *.test.ts(x) files, not {listed}"
        )


def check_untouched(git: Git, base: str, repo: str) -> list[str]:
    """Errors for a repository the agent was not meant to change."""
    errors: list[str] = []
    try:
        if git.rev_parse("HEAD") != base:
            errors.append(f"{repo}: has commits the run did not ask for")
        if git.dirty_paths():
            errors.append(f"{repo}: the working tree is not clean")
    except GitError as error:
        errors.append(str(error))
    return errors


def check_clean(git: Git, repo: str) -> list[str]:
    try:
        dirty = git.dirty_paths()
    except GitError as error:
        return [str(error)]
    if not dirty:
        return []
    return [f"{repo}: uncommitted changes left behind: {', '.join(dirty[:10])}"]
