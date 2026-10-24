"""The agent's `test.command`, turned into an argv only if it is one we allow.

The string is agent output, so it is never handed to a shell: it must be
exactly `bunx vitest run <test files>` or
`cargo test -p <crate> [--test <name>] <filter>`, and it is executed as the
argv parsed here.
"""

import re
import shlex
from collections.abc import Collection
from dataclasses import dataclass
from pathlib import PurePosixPath

from .paths import is_vitest_file

DEFAULT_CRATES = frozenset({"api", "webhooks", "common", "time-triggered"})

_NAME = re.compile(r"[A-Za-z0-9_:]+")
_PATH = re.compile(r"[A-Za-z0-9_.$()/+-]+")


class CommandRejected(ValueError):
    pass


@dataclass(frozen=True)
class TestCommand:
    kind: str  # "vitest" or "cargo"
    argv: tuple[str, ...]
    crate: str | None = None


def parse(command: str, crates: Collection[str] = DEFAULT_CRATES) -> TestCommand:
    try:
        words = shlex.split(command)
    except ValueError:
        raise CommandRejected("the test command does not parse") from None
    if words[:3] == ["bunx", "vitest", "run"]:
        return _vitest(words)
    if words[:2] == ["cargo", "test"]:
        return _cargo(words, crates)
    raise CommandRejected("the test command is neither `bunx vitest run` nor `cargo test`")


def _vitest(words: list[str]) -> TestCommand:
    paths = words[3:]
    if not paths:
        raise CommandRejected("`bunx vitest run` needs at least one test file")
    for path in paths:
        _check_test_path(path)
    return TestCommand("vitest", tuple(words))


def _check_test_path(path: str) -> None:
    pure = PurePosixPath(path)
    if not _PATH.fullmatch(path) or path.startswith("-"):
        raise CommandRejected(f"`{path}` is not a plain relative path")
    if pure.is_absolute() or ".." in pure.parts:
        raise CommandRejected(f"`{path}` must be relative to the repository, without `..`")
    if not is_vitest_file(path):
        raise CommandRejected(f"`{path}` is not a .test.ts or .test.tsx file")


def _cargo(words: list[str], crates: Collection[str]) -> TestCommand:
    rest = words[2:]
    if len(rest) < 3 or rest[0] != "-p":
        raise CommandRejected(
            "`cargo test` must be `cargo test -p <crate> [--test <name>] <filter>`"
        )
    crate = rest[1]
    if crate not in crates:
        raise CommandRejected(f"`{crate}` is not a crate the workflow tests")
    tail = rest[2:]
    if tail[0] == "--test":
        if len(tail) != 3:
            raise CommandRejected("`--test` takes one target name, then one filter")
        names = tail[1:]
    elif len(tail) == 1:
        names = tail
    else:
        raise CommandRejected("`cargo test` takes exactly one filter")
    for name in names:
        if not _NAME.fullmatch(name):
            raise CommandRejected(f"`{name}` is not a plain test name")
    return TestCommand("cargo", tuple(words), crate)
