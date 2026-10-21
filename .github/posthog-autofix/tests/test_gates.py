"""Reading test and clippy output."""

import json
import unittest

import helpers  # noqa: F401  (puts the package on sys.path)

from autofix.gates import clippy_warnings, failed_for_the_right_reason, passed, touched_crates

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


if __name__ == "__main__":
    unittest.main()
