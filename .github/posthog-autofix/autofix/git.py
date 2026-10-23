"""Git, read through plumbing commands with hooks and fsmonitor turned off.

Once the agent has run, a repository's `.git` is untrusted: a hook or an
fsmonitor command planted there would otherwise run inside every later git
call, including the push.
"""

from collections.abc import Mapping
from dataclasses import dataclass
from pathlib import Path

from . import proc

SAFE_CONFIG = ("-c", "core.hooksPath=/dev/null", "-c", "core.fsmonitor=false")
TIMEOUT_SECONDS = 600


class GitError(RuntimeError):
    pass


@dataclass(frozen=True)
class Commit:
    sha: str
    files: tuple[str, ...]


class Git:
    """A repository; with `run_as`, every git command runs as that user, so
    the caller never executes anything an agent-written `.git` configures."""

    def __init__(self, repo: Path, run_as: proc.RunAs | None = None) -> None:
        self.repo = repo
        self.run_as = run_as

    def run(self, *args: str, env: Mapping[str, str] | None = None) -> str:
        completed = proc.run(
            ["git", *SAFE_CONFIG, *args],
            cwd=self.repo,
            timeout=TIMEOUT_SECONDS,
            env=env,
            run_as=self.run_as,
        )
        if not completed.ok:
            message = completed.stderr.strip() or completed.stdout.strip() or "timed out"
            raise GitError(f"git {args[0]} failed: {message}")
        return completed.stdout

    def rev_parse(self, rev: str) -> str:
        return self.run("rev-parse", "--verify", f"{rev}^{{commit}}").strip()

    def is_ancestor(self, ancestor: str, descendant: str) -> bool:
        try:
            self.run("merge-base", "--is-ancestor", ancestor, descendant)
        except GitError:
            return False
        return True

    def commits_since(self, base: str, head: str) -> list[str]:
        """Commits in `base..head`, oldest first."""
        out = self.run("rev-list", "--reverse", f"{base}..{head}")
        return out.split()

    def merge_commits_since(self, base: str, head: str) -> list[str]:
        return self.run("rev-list", "--merges", f"{base}..{head}").split()

    def files_of(self, sha: str) -> tuple[str, ...]:
        """Paths a (non-merge) commit adds, changes or deletes."""
        out = self.run(
            "diff-tree", "--no-commit-id", "--name-only", "-r", "-z", "--no-renames", sha
        )
        return tuple(path for path in out.split("\0") if path)

    def commit(self, sha: str) -> Commit:
        return Commit(sha, self.files_of(sha))

    def files_between(self, base: str, head: str, existing_only: bool = False) -> list[str]:
        """Paths changed from `base` to `head`; without the deleted ones when
        `existing_only`."""
        flags = ["--diff-filter=d"] if existing_only else []
        out = self.run("diff", "--name-only", "-z", "--no-renames", *flags, base, head)
        return [path for path in out.split("\0") if path]

    def existing_at(self, rev: str, paths: list[str]) -> list[str]:
        """The `paths` that exist at `rev`, in their given order."""
        if not paths:
            return []
        out = self.run("ls-tree", "-r", "--name-only", "-z", rev, "--", *paths)
        present = set(out.split("\0"))
        return [path for path in paths if path in present]

    def show(self, rev: str, path: str) -> str:
        """A file's content at `rev`, read from git's objects rather than the
        work tree (where it could be a symlink to anything)."""
        return self.run("show", f"{rev}:{path}")

    def dirty_paths(self) -> list[str]:
        out = self.run("status", "--porcelain", "--untracked-files=all", "-z")
        return [entry[3:] for entry in out.split("\0") if entry]

    def current_branch(self) -> str | None:
        try:
            return self.run("symbolic-ref", "-q", "--short", "HEAD").strip() or None
        except GitError:
            return None

    def checkout(self, rev: str) -> None:
        """Switches the work tree to a branch name or (detached) a commit."""
        self.run("checkout", "--quiet", rev)

    def checkout_detached(self, sha: str) -> None:
        self.run("checkout", "--quiet", "--detach", sha)
