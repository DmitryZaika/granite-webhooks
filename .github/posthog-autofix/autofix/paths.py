"""Which files the agent may not read or change.

One source of truth for the PreToolUse guard (what the agent may touch while
it works) and for verification (what its commits may touch).
"""

from pathlib import PurePosixPath

# Directories whose contents only people change: workflows and these tools,
# schema migrations (applied to the shared database by hand), git internals
# (hooks run code), and tool configuration that developers run with.
PROTECTED_DIRS = frozenset(
    {".github", "migrations", "migrations-pending", ".git", ".cargo", ".claude"}
)

# Dependency manifests and lockfiles: a fix never adds or bumps a dependency.
PROTECTED_FILES = frozenset(
    {"Cargo.lock", "Cargo.toml", "bun.lock", "bun.lockb", "package.json", "package-lock.json"}
)


def is_secret_file(path: str) -> bool:
    """`.env*`, `*.pem` and `settings.local.json`, in any directory."""
    name = PurePosixPath(path).name
    return name.startswith(".env") or name.endswith(".pem") or name == "settings.local.json"


def is_write_protected(path: str) -> bool:
    """A path the agent may not create, edit or commit.

    Matches a protected directory at any depth, so it works the same for a
    repo-relative path (`migrations/x.sql`) and a workspace-relative one
    (`granite-webhooks/migrations/x.sql`).
    """
    parts = PurePosixPath(path).parts
    if any(part in PROTECTED_DIRS for part in parts[:-1]):
        return True
    name = parts[-1] if parts else ""
    return name in PROTECTED_DIRS or name in PROTECTED_FILES or is_secret_file(path)


def is_vitest_file(path: str) -> bool:
    return path.endswith(".test.ts") or path.endswith(".test.tsx")
