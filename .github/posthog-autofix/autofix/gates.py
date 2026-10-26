"""Each repository's checks on the fix commit, and what their output means."""

import json
import re
import tomllib
from collections import Counter
from collections.abc import Iterable
from dataclasses import dataclass, field
from pathlib import Path, PurePosixPath
from typing import Any

from . import cargo_tests, proc
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


# --- Results -------------------------------------------------------------------


@dataclass
class GateResult:
    """`errors` make the fix unverified; `notes` are evidence that does not
    (failures `main` already has)."""

    errors: list[str] = field(default_factory=list)
    notes: list[str] = field(default_factory=list)


PREEXISTING = "pre-existing on main, not caused by this fix"
EVIDENCE_LIMIT = 1500


# --- general_datebase ------------------------------------------------------


@dataclass(frozen=True)
class Check:
    name: str
    fix_argv: list[str]
    # The same check for the base; None when there is nothing to compare
    # (biome on files the fix added).
    base_argv: list[str] | None
    cap: float
    env: dict[str, str] | None = None


def _biome(files: list[str]) -> list[str]:
    return [
        "bunx",
        "biome",
        "check",
        "--files-ignore-unknown=true",
        "--no-errors-on-unmatched",
        *files,
    ]


def datebase_checks(lintable_on_fix: list[str], lintable_on_base: list[str]) -> list[Check]:
    checks = []
    if lintable_on_fix:
        base_argv = _biome(lintable_on_base) if lintable_on_base else None
        checks.append(Check("bunx biome check", _biome(lintable_on_fix), base_argv, BIOME_CAP))
    tsc = ["bunx", "tsc", "-b"]
    build = ["bun", "run", "build"]
    checks.append(Check("bunx tsc -b", tsc, tsc, TSC_CAP))
    checks.append(Check("bun run build", build, build, BUILD_CAP, BUILD_ENV))
    return checks


def split_checks(failed: list[tuple[Check, Completed]], failing_on_base: set[str]) -> GateResult:
    """A check that fails on the fix and on the base is pre-existing."""
    result = GateResult()
    for check, completed in failed:
        if check.name in failing_on_base:
            result.notes.append(f"`{check.name}` fails on the base too: {PREEXISTING}.")
        else:
            evidence = proc.tail(completed.output.strip(), EVIDENCE_LIMIT)
            result.errors.append(f"`{check.name}` {_how_it_failed(completed)}:\n{evidence}")
    return result


def datebase_gates(
    session: Session, git: Git, base: str, fix: str, touched: list[str]
) -> GateResult:
    """Biome on the touched files, tsc and the build; a failing one is run on
    the base too. `touched` holds the files that exist on the fix commit.
    Leaves the fix checked out."""
    lintable = [path for path in touched if path.endswith(BIOME_SUFFIXES)]
    checks = datebase_checks(lintable, git.existing_at(base, lintable))
    failed = []
    for check in checks:
        completed = session.execute(check.name, check.fix_argv, git.repo, check.cap, check.env)
        if not completed.ok:
            failed.append((check, completed))
    if not failed:
        return GateResult()
    failing_on_base: set[str] = set()
    git.checkout_detached(base)
    try:
        for check, _ in failed:
            if check.base_argv is None:
                continue
            label = f"{check.name} on the base"
            completed = session.execute(label, check.base_argv, git.repo, check.cap, check.env)
            # A timeout proves nothing about the base: the failure stays new.
            if completed.returncode != 0 and not completed.timed_out:
                failing_on_base.add(check.name)
    finally:
        git.checkout_detached(fix)
    return split_checks(failed, failing_on_base)


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
    """The packages owning the touched files (the deepest member wins). The
    `api` package's integration tests cover the crates in `api/crates/`, so a
    touched `api-*` package brings `api` along. The webhooks crates' tests
    exercise the code of the crates below (`webhooks-core`'s queries in
    every crate's tests), so a touched webhooks package brings them all."""
    found: set[str] = set()
    members = sorted(crates, key=len, reverse=True)
    for path in touched:
        for member in members:
            if PurePosixPath(path).is_relative_to(member):
                found.add(crates[member])
                break
    if any(name.startswith("api-") for name in found) and "api" in crates.values():
        found.add("api")
    if any(name == "webhooks" or name.startswith("webhooks-") for name in found):
        found.update(
            name for name in crates.values() if name == "webhooks" or name.startswith("webhooks-")
        )
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
) -> GateResult:
    """Clippy (no new warnings in touched files) and the touched crates'
    tests, failures `main` already has set apart. Leaves the fix checked out."""
    crates = touched_crates(workspace_crates(git, fix), touched)
    result = GateResult()
    if not crates:
        session.log("No workspace crate touched: no cargo checks.")
        return result
    rust_files = {path for path in touched if path.endswith(".rs")}
    git.checkout_detached(base)
    baseline = clippy(session, git.repo, crates, "clippy on the base")
    git.checkout_detached(fix)
    after = clippy(session, git.repo, crates, "clippy on the fix")
    if not after.ok:
        result.errors.append("cargo clippy failed on the fix commit")
    new = clippy_warnings(after.stdout, rust_files) - clippy_warnings(baseline.stdout, rust_files)
    if new:
        listed = "; ".join(f"{file}: {message}" for (file, _code, message) in list(new)[:10])
        result.errors.append(f"cargo clippy: {sum(new.values())} new warning(s): {listed}")
    failing = _crate_tests(session, git, crates, result)
    if failing:
        _compare_with_base(session, git, base, fix, failing, result)
    return result


def _crate_tests(
    session: Session, git: Git, crates: list[str], result: GateResult
) -> dict[str, tuple[cargo_tests.TestRun, str]]:
    """Runs each crate's tests on the fix; returns the runs with failing
    tests, with their output. Anything but a plain test failure is an error."""
    failing = {}
    for crate in crates:
        argv = ["cargo", "test", "--no-fail-fast", "-p", crate]
        completed = session.execute(
            f"{crate} tests", argv, git.repo, CARGO_TEST_CAP, merge_output=True
        )
        run = cargo_tests.parse(completed.output)
        evidence = proc.tail(completed.output.strip(), EVIDENCE_LIMIT)
        if completed.timed_out:
            result.errors.append(f"`{' '.join(argv)}` timed out")
        elif run.build_failed:
            result.errors.append(f"`{' '.join(argv)}` does not build on the fix:\n{evidence}")
        elif run.crashed:
            crashed = ", ".join(target.label for target in run.crashed)
            result.errors.append(f"{crate}: test binaries crashed ({crashed}):\n{evidence}")
        elif completed.returncode != 0 and not run.failures:
            result.errors.append(f"`{' '.join(argv)}` failed without a failing test:\n{evidence}")
        if run.failures:
            failing[crate] = (run, completed.output)
    return failing


def _compare_with_base(
    session: Session,
    git: Git,
    base: str,
    fix: str,
    failing: dict[str, tuple[cargo_tests.TestRun, str]],
    result: GateResult,
) -> None:
    """Re-runs the failing tests on the base, one target at a time."""
    git.checkout_detached(base)
    try:
        for crate, (run, fix_output) in failing.items():
            on_base: dict[cargo_tests.Target, set[str]] = {}
            for target, names in run.failures.items():
                argv = cargo_tests.rerun_argv(crate, target, names)
                label = f"{crate} {target.label} on the base"
                completed = session.execute(
                    label, argv, git.repo, CARGO_TEST_CAP, merge_output=True
                )
                if not completed.timed_out:
                    on_base[target] = cargo_tests.failed_names(
                        cargo_tests.parse(completed.output), target
                    )
            new, preexisting = cargo_tests.split(run, on_base)
            if new:
                listed = "; ".join(str(failure) for failure in new[:20])
                evidence = proc.tail(fix_output.strip(), EVIDENCE_LIMIT)
                result.errors.append(
                    f"{crate}: {len(new)} test(s) fail on the fix: {listed}\n{evidence}"
                )
            if preexisting:
                listed = "; ".join(str(failure) for failure in preexisting[:20])
                result.notes.append(
                    f"{crate}: {len(preexisting)} failing test(s) {PREEXISTING}: {listed}"
                )
    finally:
        git.checkout_detached(fix)


def _how_it_failed(completed: Completed) -> str:
    return "timed out" if completed.timed_out else f"failed (exit {completed.returncode})"
