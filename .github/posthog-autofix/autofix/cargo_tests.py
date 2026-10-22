"""Reading `cargo test --no-fail-fast` output, and telling the test failures a
fix introduced from the ones `main` already had.

The output must be stdout and stderr merged in order: cargo writes the
`Running …` headers to stderr and libtest writes `test … FAILED` to stdout.
"""

import re
from dataclasses import dataclass, field
from pathlib import PurePosixPath

_RUNNING = re.compile(r"^\s*Running (unittests )?(\S+) \(")
_DOC_TESTS = re.compile(r"^\s*Doc-tests \S+")
_FAILED = re.compile(r"^test (.+) \.\.\. FAILED$")
_RESULT = re.compile(r"^test result: ")
# cargo's errors for code that does not compile or a target it cannot find.
# (Not any `error` line: a test's own output may start with one.)
_BUILD_ERROR = re.compile(
    r"^error(\[E\d+\]: |: could not compile |: no \w+ target named |: failed to )"
)
_SHOULD_PANIC = " - should panic"


@dataclass(frozen=True)
class Target:
    """One test binary of a package; `args` select it for `cargo test`."""

    args: tuple[str, ...]

    @property
    def label(self) -> str:
        return " ".join(self.args)

    @property
    def is_doc(self) -> bool:
        return self.args == ("--doc",)


@dataclass
class TestRun:
    failures: dict[Target, list[str]] = field(default_factory=dict)
    # Started, but never printed `test result:` (a crash or an abort).
    crashed: list[Target] = field(default_factory=list)
    # A compile error, or cargo could not run the tests at all.
    build_failed: bool = False


@dataclass(frozen=True)
class Failure:
    target: Target
    name: str

    def __str__(self) -> str:
        return f"{self.name} ({self.target.label})"


def target_of(line: str) -> Target | None:
    """The target a `Running …` or `Doc-tests …` header starts."""
    if _DOC_TESTS.match(line):
        return Target(("--doc",))
    running = _RUNNING.match(line)
    if running is None:
        return None
    path = PurePosixPath(running.group(2))
    if running.group(1):
        # A binary's file name is the crate name, not always the --bin name
        # (`toy-crate` builds `toy_crate-<hash>`): select every binary.
        is_bin = path.name == "main.rs" or "bin" in path.parts
        return Target(("--bins",) if is_bin else ("--lib",))
    name = path.parent.name if path.name == "main.rs" else path.stem
    kind = "--bench" if path.parts[0] == "benches" else "--test"
    return Target((kind, name))


def parse(output: str) -> TestRun:
    run = TestRun()
    current: Target | None = None
    finished = True
    for line in output.splitlines():
        target = target_of(line)
        if target is not None:
            if current is not None and not finished:
                run.crashed.append(current)
            current, finished = target, False
        elif _RESULT.match(line):
            finished = True
        elif (failed := _FAILED.match(line)) and current is not None:
            name = failed.group(1).removesuffix(_SHOULD_PANIC)
            run.failures.setdefault(current, []).append(name)
        elif _BUILD_ERROR.match(line):
            run.build_failed = True
    if current is not None and not finished:
        run.crashed.append(current)
    return run


def rerun_argv(crate: str, target: Target, names: list[str]) -> list[str]:
    """Runs only `names` of `target`. A doc test's listed name does not
    match `--exact`, so a doc target runs whole and is compared by name."""
    argv = ["cargo", "test", "--no-fail-fast", "-p", crate, *target.args]
    if target.is_doc:
        return argv
    return [*argv, "--", "--exact", *names]


def split(fix: TestRun, base: dict[Target, set[str]]) -> tuple[list[Failure], list[Failure]]:
    """(new, pre-existing) failures of the fix run.

    `base` holds what failed when the same tests ran on the base. A test
    that is missing there, or that did not compile there, is new.
    """
    new: list[Failure] = []
    preexisting: list[Failure] = []
    for target, names in fix.failures.items():
        for name in names:
            failure = Failure(target, name)
            (preexisting if name in base.get(target, set()) else new).append(failure)
    return new, preexisting


def failed_names(run: TestRun, target: Target) -> set[str]:
    """What failed in a base re-run of `target`; nothing when it did not
    build (every fix failure then counts as new)."""
    if run.build_failed:
        return set()
    return set(run.failures.get(target, []))
