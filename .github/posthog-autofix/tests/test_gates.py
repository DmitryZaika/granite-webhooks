"""Reading test and clippy output, and the general_datebase checks."""

import json
import shutil
import tempfile
import unittest
from pathlib import Path

from helpers import commit, git, init_repo

from autofix.gates import (
    PREEXISTING,
    GateResult,
    clippy_warnings,
    datebase_gates,
    failed_for_the_right_reason,
    passed,
    touched_crates,
)
from autofix.git import Git
from autofix.proc import Completed
from autofix.session import Session

VITEST_FAILED = """
 FAIL  app/utils/phone.test.ts > canonicalPhone10 > strips a leading 1
AssertionError: expected '15551234567' to be '5551234567'

 Test Files  1 failed (1)
      Tests  1 failed | 4 passed (5)
"""
VITEST_LOAD_ERROR = """
 FAIL  app/utils/phone.test.ts [ app/utils/phone.test.ts ]
Error: Failed to resolve import "./missing" from "app/utils/phone.test.ts"

 Test Files  1 failed (1)
      Tests  no tests
"""
VITEST_PASSED = """
 Test Files  1 passed (1)
      Tests  5 passed (5)
"""
CARGO_FAILED = (
    "running 1 test\ntest tests::adds ... FAILED\n\n"
    "test result: FAILED. 0 passed; 1 failed; 0 ignored\n"
)
CARGO_COMPILE_ERROR = (
    "error[E0425]: cannot find function `sum` in module `super`\nerror: could not compile `toy`\n"
)
CARGO_PASSED = (
    "running 1 test\ntest tests::adds ... ok\n\ntest result: ok. 1 passed; 0 failed; 0 ignored\n"
)
CARGO_NOTHING_RAN = "running 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored\n"


def clippy_line(file_name: str, message: str, level: str = "warning", line: int = 3) -> str:
    return json.dumps(
        {
            "reason": "compiler-message",
            "message": {
                "level": level,
                "message": message,
                "code": {"code": "clippy::needless_return"},
                "spans": [{"file_name": file_name, "is_primary": True, "line_start": line}],
            },
        }
    )


class TestOutcomeTest(unittest.TestCase):
    def test_vitest(self) -> None:
        self.assertTrue(failed_for_the_right_reason("vitest", VITEST_FAILED, 1))
        self.assertFalse(failed_for_the_right_reason("vitest", VITEST_LOAD_ERROR, 1))
        self.assertFalse(failed_for_the_right_reason("vitest", VITEST_PASSED, 0))
        self.assertTrue(passed("vitest", VITEST_PASSED, 0))
        self.assertFalse(passed("vitest", VITEST_FAILED, 1))
        self.assertFalse(passed("vitest", VITEST_LOAD_ERROR, 0))

    def test_cargo(self) -> None:
        self.assertTrue(failed_for_the_right_reason("cargo", CARGO_FAILED, 101))
        self.assertFalse(failed_for_the_right_reason("cargo", CARGO_COMPILE_ERROR, 101))
        self.assertTrue(passed("cargo", CARGO_PASSED, 0))
        self.assertFalse(passed("cargo", CARGO_NOTHING_RAN, 0))
        self.assertFalse(passed("cargo", CARGO_FAILED, 101))


class ClippyTest(unittest.TestCase):
    def test_counts_warnings_in_touched_files_only(self) -> None:
        output = "\n".join(
            [
                clippy_line("api/src/sms.rs", "unneeded `return`"),
                clippy_line("api/src/sms.rs", "unneeded `return`", line=40),
                clippy_line("api/src/other.rs", "unneeded `return`"),
                clippy_line("api/src/sms.rs", "mismatched types", level="error"),
                '{"reason":"build-finished","success":true}',
                "not json",
            ]
        )
        warnings = clippy_warnings(output, {"api/src/sms.rs"})
        self.assertEqual(sum(warnings.values()), 2)

    def test_a_moved_warning_is_not_new(self) -> None:
        base = clippy_warnings(
            clippy_line("api/src/sms.rs", "unneeded `return`", line=3), {"api/src/sms.rs"}
        )
        fix = clippy_warnings(
            clippy_line("api/src/sms.rs", "unneeded `return`", line=9), {"api/src/sms.rs"}
        )
        self.assertFalse(fix - base)


class CratesTest(unittest.TestCase):
    def test_deepest_member_wins(self) -> None:
        crates = {
            "api": "api",
            "test-db": "test-db",
            "test-db/macros": "test-db-macros",
            "common": "common",
        }
        touched = ["api/src/sms.rs", "test-db/macros/src/lib.rs", "README.md", "migrations/x.sql"]
        self.assertEqual(touched_crates(crates, touched), ["api", "test-db-macros"])

    def test_an_api_crate_brings_the_api_integration_tests(self) -> None:
        crates = {"api": "api", "api/crates/crm": "api-crm", "common": "common"}
        touched = ["api/crates/crm/src/routes/sms/send.rs"]
        self.assertEqual(touched_crates(crates, touched), ["api", "api-crm"])


class DatebaseGatesTest(unittest.TestCase):
    """The checks run for real git checkouts; the commands are faked."""

    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        self.repo = init_repo(self.tmp / "general_datebase")
        self.base = commit(self.repo, "base", {"app/old.ts": "export const a = 1\n"})
        self.fix = commit(
            self.repo, "fix", {"app/old.ts": "export const a = 2\n", "app/new.ts": "x\n"}
        )
        self.calls: list[tuple[str, tuple[str, ...]]] = []

    def runner(self, failing: dict[str, set[str]]):
        """Fails a command on the commits named in `failing[command]`."""

        def run(argv, cwd, timeout, **options) -> Completed:
            head = git(cwd, "rev-parse", "HEAD")
            commit_name = "base" if head == self.base else "fix"
            self.calls.append((commit_name, tuple(argv)))
            name = " ".join(argv[:3])
            code = 1 if commit_name in failing.get(name, set()) else 0
            return Completed(tuple(argv), code, f"{name} output on {commit_name}", "", False)

        return run

    def gates(self, failing: dict[str, set[str]]) -> GateResult:
        session = Session.start(self.runner(failing), 600)
        touched = ["app/old.ts", "app/new.ts"]
        return datebase_gates(session, Git(self.repo), self.base, self.fix, touched)

    def test_all_pass_without_touching_the_base(self) -> None:
        result = self.gates({})
        self.assertEqual((result.errors, result.notes), ([], []))
        self.assertTrue(all(commit_name == "fix" for commit_name, _ in self.calls))

    def test_failing_on_main_too_is_a_note(self) -> None:
        result = self.gates({"bunx tsc -b": {"fix", "base"}, "bun run build": {"fix"}})
        self.assertEqual(len(result.errors), 1)
        self.assertIn("`bun run build` failed (exit 1)", result.errors[0])
        self.assertIn("bun run build output on fix", result.errors[0])
        self.assertEqual(
            result.notes, ["`bunx tsc -b` fails on the base too: " + PREEXISTING + "."]
        )
        self.assertEqual(git(self.repo, "rev-parse", "HEAD"), self.fix)

    def test_biome_on_the_base_lints_only_files_it_has(self) -> None:
        result = self.gates({"bunx biome check": {"fix", "base"}})
        base_biome = [argv for name, argv in self.calls if name == "base" and argv[1] == "biome"]
        self.assertEqual(base_biome[0][-1], "app/old.ts")
        self.assertNotIn("app/new.ts", base_biome[0])
        self.assertEqual(len(result.notes), 1)

    def test_biome_on_files_new_to_the_fix_has_no_baseline(self) -> None:
        self.fix = commit(self.repo, "only new", {"app/new2.ts": "y\n"})
        session = Session.start(self.runner({"bunx biome check": {"fix", "base"}}), 600)
        result = datebase_gates(session, Git(self.repo), self.base, self.fix, ["app/new2.ts"])
        self.assertEqual(len(result.errors), 1)
        self.assertFalse(any(name == "base" and argv[1] == "biome" for name, argv in self.calls))


if __name__ == "__main__":
    unittest.main()
