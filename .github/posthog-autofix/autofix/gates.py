"""Each repository's checks on the fix commit, and what their output means."""

import json
import re
import tomllib
from collections import Counter
from collections.abc import Iterable
from pathlib import Path, PurePosixPath
from typing import Any

from .git import Git
from .proc import Completed
from .session import Session
from .test_command import TestCommand

MINUTE = 60
TEST_CAP = 20 * MINUTE
CLIPPY_CAP = 15 * MINUTE
CARGO_TEST_CAP = 25 * MINUTE
BIOME_CAP = 3 * MINUTE
TSC_CAP = 10 * MINUTE
BUILD_CAP = 10 * MINUTE

BIOME_SUFFIXES = (".ts", ".tsx", ".js", ".jsx", ".json", ".jsonc", ".css")
# VITE_* values are inlined at build time; the bundle is thrown away.
BUILD_ENV = {"VITE_GRANITE_API_URL": "https://api.invalid"}


# --- Test outcomes ---------------------------------------------------------


def failed_for_the_right_reason(kind: str, output: str, returncode: int) -> bool:
    """The test compiled, ran and failed (not a build or load error)."""
    if returncode == 0:
        return False
    if kind == "cargo":
        return "test result: FAILED" in output
    return re.search(r"^\s*Tests\s+[1-9]\d* failed", output, re.MULTILINE) is not None


def passed(kind: str, output: str, returncode: int) -> bool:
    """Exit 0 and at least one test ran (a filter matching nothing is no pass)."""
    if returncode != 0:
        return False
    if kind == "cargo":
        return re.search(r"test result: ok\. [1-9]\d* passed", output) is not None
    tests_line = re.search(r"^\s*Tests\s+(.*)$", output, re.MULTILINE)
    if tests_line is None:
        return False
    return re.search(
        r"[1-9]\d* passed", tests_line.group(1)
    ) is not None and "failed" not in tests_line.group(1)


def run_test(
    session: Session, repo_dir: Path, test: TestCommand, label: str
) -> tuple[bool, bool, str]:
    """Runs the test; returns (failed for the right reason, passed, output)."""
    completed = session.execute(label, test.argv, repo_dir, TEST_CAP)
    output = completed.output
    if completed.timed_out:
        return False, False, output
    red = failed_for_the_right_reason(test.kind, output, completed.returncode)
    green = passed(test.kind, output, completed.returncode)
    return red, green, output


# --- general_datebase ------------------------------------------------------


def datebase_gates(session: Session, repo_dir: Path, touched: Iterable[str]) -> list[str]:
    """`touched` holds only files that exist on the fix commit."""
    errors: list[str] = []
    lintable = [path for path in touched if path.endswith(BIOME_SUFFIXES)]
    if lintable:
        argv = [
            "bunx",
            "biome",
            "check",
            "--files-ignore-unknown=true",
            "--no-errors-on-unmatched",
            *lintable,
        ]
        _expect_ok(session.execute("biome", argv, repo_dir, BIOME_CAP), "bunx biome check", errors)
    _expect_ok(
        session.execute("typecheck", ["bunx", "tsc", "-b"], repo_dir, TSC_CAP),
        "bunx tsc -b",
        errors,
    )
    build = session.execute("build", ["bun", "run", "build"], repo_dir, BUILD_CAP, BUILD_ENV)
    _expect_ok(build, "bun run build", errors)
    return errors


# --- granite-webhooks --------------------------------------------------------


def workspace_crates(git: Git, rev: str) -> dict[str, str]:
    """Member directory -> package name, from the Cargo workspace at `rev`."""
    manifest = tomllib.loads(git.show(rev, "Cargo.toml"))
    crates: dict[str, str] = {}
    for member in manifest.get("workspace", {}).get("members", []):
        package = tomllib.loads(git.show(rev, f"{member}/Cargo.toml"))
        crates[member] = package["package"]["name"]
    return crates


def touched_crates(crates: dict[str, str], touched: Iterable[str]) -> list[str]:
    """The packages owning the touched files (the deepest member wins)."""
    found: set[str] = set()
    members = sorted(crates, key=len, reverse=True)
    for path in touched:
        for member in members:
            if PurePosixPath(path).is_relative_to(member):
                found.add(crates[member])
                break
    return sorted(found)


def clippy_warnings(output: str, files: set[str]) -> Counter[tuple[str, str, str]]:
    """Warnings whose primary span is in `files`, keyed without line numbers
    so a moved line still matches its base counterpart."""
    warnings: Counter[tuple[str, str, str]] = Counter()
    for line in output.splitlines():
        message = _compiler_message(line)
        if message is None or message.get("level") != "warning":
            continue
        primary = [span for span in message.get("spans", []) if span.get("is_primary")]
        if not primary or primary[0].get("file_name") not in files:
            continue
        code = (message.get("code") or {}).get("code") or ""
        warnings[(primary[0]["file_name"], code, message.get("message", ""))] += 1
    return warnings


def _compiler_message(line: str) -> dict[str, Any] | None:
    if not line.startswith("{"):
        return None
    try:
        record = json.loads(line)
    except ValueError:
        return None
    if record.get("reason") != "compiler-message":
        return None
    return record.get("message")


def clippy(session: Session, repo_dir: Path, crates: list[str], label: str) -> Completed:
    argv = ["cargo", "clippy", "--message-format=json"]
    for crate in crates:
        argv += ["-p", crate]
    argv.append("--all-targets")
    return session.execute(label, argv, repo_dir, CLIPPY_CAP)


def webhooks_gates(
    session: Session,
    git: Git,
    base: str,
    fix: str,
    touched: list[str],
) -> list[str]:
    """Clippy (no new warnings in touched files) and the touched crates' tests.

    Checks out the base for the clippy baseline and leaves the fix checked out.
    """
    crates = touched_crates(workspace_crates(git, fix), touched)
    if not crates:
        session.log("No workspace crate touched: no cargo checks.")
        return []
    errors: list[str] = []
    rust_files = {path for path in touched if path.endswith(".rs")}
    git.checkout_detached(base)
    baseline = clippy(session, git.repo, crates, "clippy on the base")
    git.checkout_detached(fix)
    after = clippy(session, git.repo, crates, "clippy on the fix")
    if not after.ok:
        errors.append("cargo clippy failed on the fix commit")
    new = clippy_warnings(after.stdout, rust_files) - clippy_warnings(baseline.stdout, rust_files)
    if new:
        listed = "; ".join(f"{file}: {message}" for (file, _code, message) in list(new)[:10])
        errors.append(f"cargo clippy: {sum(new.values())} new warning(s): {listed}")
    argv = ["cargo", "test"]
    for crate in crates:
        argv += ["-p", crate]
    _expect_ok(
        session.execute("crate tests", argv, git.repo, CARGO_TEST_CAP), " ".join(argv), errors
    )
    return errors


def _expect_ok(completed: Completed, name: str, errors: list[str]) -> None:
    if completed.timed_out:
        errors.append(f"`{name}` timed out")
    elif completed.returncode != 0:
        errors.append(f"`{name}` failed (exit {completed.returncode})")
