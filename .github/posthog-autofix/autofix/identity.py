"""The author of the agent's commits: the GitHub App's bot user.

The noreply address `<id>+<slug>[bot]@users.noreply.github.com` links the
commits to the bot on GitHub; without the id, a plain fallback address.
"""

from pathlib import Path

from .git import Git
from .github import GitHub
from .http import HttpError

DEFAULT_SLUG = "granite-autofix"
FALLBACK_EMAIL = "noreply@granite-manager.com"


def bot_identity(app_slug: str, github: GitHub | None) -> tuple[str, str]:
    slug = app_slug or DEFAULT_SLUG
    name = f"{slug}[bot]"
    if github is None:
        return name, FALLBACK_EMAIL
    try:
        user_id = github.bot_user_id(slug)
    except HttpError:
        return name, FALLBACK_EMAIL
    return name, f"{user_id}+{name}@users.noreply.github.com"


def configure(repos: list[Path], name: str, email: str) -> None:
    for repo in repos:
        git = Git(repo)
        git.run("config", "user.name", name)
        git.run("config", "user.email", email)
