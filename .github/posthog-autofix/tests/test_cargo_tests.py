"""Parsing `cargo test --no-fail-fast` output (stderr merged into stdout) and
splitting new failures from pre-existing ones."""

import unittest

import helpers  # noqa: F401  (puts the package on sys.path)

from autofix import cargo_tests
from autofix.cargo_tests import Failure, Target, TestRun

LIB = Target(("--lib",))
BINS = Target(("--bins",))
OPENAPI = Target(("--test", "openapi"))
SMS = Target(("--test", "sms"))
DOC = Target(("--doc",))

# Trimmed from a real cargo 1.98 run over a crate with a lib, a main binary,
# two integration tests and a doc test.
CARGO_OUTPUT = """\
   Compiling api v0.1.0 (/srv/autofix/work/granite-webhooks/api)
    Finished `test` profile [unoptimized + debuginfo] target(s) in 41.20s
     Running unittests src/lib.rs (target/debug/deps/api-63d366a05a9bc338)

running 4 tests
test sms::tests::parses_reply ... ok
test sms::tests::empty_reply - should panic ... FAILED
test sms::tests::trims ... FAILED
test util::tests::ok ... ok

failures:

---- sms::tests::trims stdout ----

thread 'sms::tests::trims' panicked at api/src/sms.rs:88:9:
assertion `left == right` failed
error: this line is test output, not a build error

failures:
    sms::tests::empty_reply
    sms::tests::trims

test result: FAILED. 2 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s

error: test failed, to rerun pass `-p api --lib`
     Running unittests src/main.rs (target/debug/deps/api-d3a4ebc02a81eecc)

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

     Running tests/openapi.rs (target/debug/deps/openapi-9e8fe62840bc4892)

running 2 tests
test spec_file_is_current ... FAILED
test every_route_is_documented ... ok

failures:
    spec_file_is_current

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.10s

error: test failed, to rerun pass `-p api --test openapi`
     Running tests/sms.rs (target/debug/deps/sms-0011223344556677)

running 1 test
test tts_reply ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.31s

   Doc-tests api

running 1 test
test api/src/lib.rs - add (line 1) ... FAILED

failures:
    api/src/lib.rs - add (line 1)

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.41s

error: doctest failed, to rerun pass `-p api --doc`
error: 3 targets failed:
    `-p api --lib`
    `-p api --test openapi`
    `-p api --doc`
"""

COMPILE_ERROR = """\
   Compiling api v0.1.0 (/srv/autofix/work/granite-webhooks/api)
error[E0308]: mismatched types
  --> api/tests/sms.rs:12:5
error: could not compile `api` (test "sms") due to 1 previous error
"""

CRASH = """\
     Running tests/sms.rs (target/debug/deps/sms-0011223344556677)

running 2 tests
test tts_reply ... ok
error: test failed, to rerun pass `-p api --test sms`

Caused by:
  process didn't exit successfully: `sms-0011223344556677` (signal: 11, SIGSEGV)
"""


class TargetTest(unittest.TestCase):
    def test_headers(self) -> None:
        cases = {
            "     Running unittests src/lib.rs (target/debug/deps/api-1)": LIB,
            "     Running unittests src/main.rs (target/debug/deps/api-2)": BINS,
            "     Running unittests src/bin/tool.rs (target/debug/deps/tool-3)": BINS,
            "     Running tests/openapi.rs (target/debug/deps/openapi-4)": OPENAPI,
            "     Running tests/it.rs (target/debug/deps/it-7)": Target(("--test", "it")),
            "     Running tests/crm/main.rs (target/debug/deps/crm-8)": Target(("--test", "crm")),
            "     Running tests/flows/main.rs (target/debug/deps/flows-5)": Target(
                ("--test", "flows")
            ),
            "     Running benches/render.rs (target/debug/deps/render-6)": Target(
                ("--bench", "render")
            ),
            "   Doc-tests api": DOC,
            "test tts_reply ... ok": None,
        }
        for line, target in cases.items():
            with self.subTest(line=line):
                self.assertEqual(cargo_tests.target_of(line), target)


class ParseTest(unittest.TestCase):
    def test_failures_per_target(self) -> None:
        run = cargo_tests.parse(CARGO_OUTPUT)
        self.assertEqual(
            run.failures,
            {
                LIB: ["sms::tests::empty_reply", "sms::tests::trims"],
                OPENAPI: ["spec_file_is_current"],
                DOC: ["api/src/lib.rs - add (line 1)"],
            },
        )
        self.assertEqual(run.crashed, [])
        self.assertFalse(
            run.build_failed, "test output starting with `error:` is not a build error"
        )

    def test_compile_error(self) -> None:
        run = cargo_tests.parse(COMPILE_ERROR)
        self.assertTrue(run.build_failed)
        self.assertEqual(run.failures, {})

    def test_crashed_binary(self) -> None:
        run = cargo_tests.parse(CRASH)
        self.assertEqual(run.crashed, [SMS])
        self.assertEqual(run.failures, {})

    def test_missing_target_on_the_base_is_a_build_failure(self) -> None:
        run = cargo_tests.parse("error: no test target named `openapi` in `api` package\n")
        self.assertTrue(run.build_failed)
        self.assertEqual(cargo_tests.failed_names(run, OPENAPI), set())


class RerunTest(unittest.TestCase):
    def test_exact_names(self) -> None:
        self.assertEqual(
            cargo_tests.rerun_argv("api", OPENAPI, ["spec_file_is_current"]),
            ["cargo", "test", "--no-fail-fast", "-p", "api", "--test", "openapi",
             "--", "--exact", "spec_file_is_current"],
        )  # fmt: skip

    def test_doc_target_runs_whole(self) -> None:
        argv = cargo_tests.rerun_argv("api", DOC, ["api/src/lib.rs - add (line 1)"])
        self.assertEqual(argv, ["cargo", "test", "--no-fail-fast", "-p", "api", "--doc"])


class SplitTest(unittest.TestCase):
    def test_new_and_preexisting(self) -> None:
        fix = cargo_tests.parse(CARGO_OUTPUT)
        on_base = {
            OPENAPI: {"spec_file_is_current"},
            LIB: {"sms::tests::empty_reply", "some::other::test"},
            # DOC missing: it did not run (or build) on the base.
        }
        new, preexisting = cargo_tests.split(fix, on_base)
        self.assertEqual(
            new,
            [Failure(LIB, "sms::tests::trims"), Failure(DOC, "api/src/lib.rs - add (line 1)")],
        )
        self.assertEqual(
            preexisting,
            [Failure(LIB, "sms::tests::empty_reply"), Failure(OPENAPI, "spec_file_is_current")],
        )
        self.assertEqual(str(preexisting[1]), "spec_file_is_current (--test openapi)")

    def test_same_name_in_another_target_is_new(self) -> None:
        fix = TestRun(failures={SMS: ["shared_name"]})
        new, preexisting = cargo_tests.split(fix, {OPENAPI: {"shared_name"}})
        self.assertEqual((new, preexisting), ([Failure(SMS, "shared_name")], []))


if __name__ == "__main__":
    unittest.main()
