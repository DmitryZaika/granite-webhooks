"""`verify` on temp git repos: a toy Cargo workspace stands in for
granite-webhooks (one crate, no dependencies, so it builds in seconds)."""

import os
import shutil
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from helpers import commit, git, init_repo, sample_result

from autofix import proc
from autofix.agent_result import AgentRun
from autofix.session import Session
from autofix.verify import Inputs, verify

HAS_CARGO = (
    shutil.which("cargo") is not None
    and proc.run(["cargo", "clippy", "--version"], cwd=Path.cwd(), timeout=60).ok
)

WORKSPACE_TOML = '[workspace]\nmembers = ["toy"]\nresolver = "2"\n'
CRATE_TOML = '[package]\nname = "toy"\nversion = "0.1.0"\nedition = "2021"\n'
BUGGY = "pub fn add(a: i32, b: i32) -> i32 {\n    a - b\n}\n"
FIXED = "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n"
TEST = """
#[cfg(test)]
mod tests {
    #[test]
    fn adds() {
        assert_eq!(super::{call}(2, 3), 5);
    }
}
"""


def with_test(source: str, call: str = "add") -> str:
    return source + TEST.replace("{call}", call)


class RecordingRunner:
    def __init__(self) -> None:
        self.calls: list[tuple[str, ...]] = []

    def __call__(self, argv, cwd, timeout, **options) -> proc.Completed:
        self.calls.append(tuple(argv))
        return proc.run(argv, cwd=cwd, timeout=timeout, **options)


@unittest.skipUnless(HAS_CARGO, "cargo and clippy are needed")
class VerifyTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        # One target dir for every toy repo, so each test compiles little.
        cls.target_dir = tempfile.mkdtemp()
        cls.env_patch = mock.patch.dict(os.environ, {"CARGO_TARGET_DIR": cls.target_dir})
        cls.env_patch.start()

    @classmethod
    def tearDownClass(cls) -> None:
        cls.env_patch.stop()
        shutil.rmtree(cls.target_dir, ignore_errors=True)

    def setUp(self) -> None:
        self.workspace = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.workspace, ignore_errors=True)
        self.repo = init_repo(self.workspace / "granite-webhooks")
        other = init_repo(self.workspace / "general_datebase")
        self.base_shas = {"general_datebase": commit(other, "init", {"README.md": "spa\n"})}

    def make_base(self, source: str, extra: dict[str, str] | None = None) -> None:
        files = {
            **(extra or {}),
            "Cargo.toml": WORKSPACE_TOML,
            "toy/Cargo.toml": CRATE_TOML,
            "toy/src/lib.rs": source,
        }
        self.base_shas["granite-webhooks"] = commit(self.repo, "base", files)

    def run_verify(self, result: dict, runner: RecordingRunner | None = None):
        runner = runner or RecordingRunner()
        agent = AgentRun("success", result, 0.5, None)
        inputs = Inputs(self.workspace, self.base_shas, agent, crates={"toy"})
        return verify(inputs, Session.start(runner, 600)), runner

    def test_red_then_green_is_verified(self) -> None:
        self.make_base(BUGGY)
        commit(self.repo, "test: reproduce", {"toy/src/lib.rs": with_test(BUGGY)})
        fix = commit(self.repo, "fix: add", {"toy/src/lib.rs": with_test(FIXED)})
        report, runner = self.run_verify(sample_result())
        self.assertEqual(report.verification, "verified", report.notes)
        self.assertTrue(report.publish)
        self.assertEqual((report.target_repo, report.fix_sha), ("granite-webhooks", fix))
        self.assertIn(("cargo", "test", "-p", "toy", "adds"), runner.calls)
        self.assertEqual(git(self.repo, "rev-parse", "HEAD"), fix)
        self.assertEqual(git(self.repo, "symbolic-ref", "--short", "HEAD"), "main")

    def test_a_test_that_already_passes_is_unverified(self) -> None:
        self.make_base(FIXED)
        commit(self.repo, "test: reproduce", {"toy/src/lib.rs": with_test(FIXED)})
        commit(self.repo, "fix: comment", {"toy/src/lib.rs": "// fixed\n" + with_test(FIXED)})
        report, _ = self.run_verify(sample_result())
        self.assertEqual(report.verification, "unverified")
        self.assertIn("did not fail on the test commit", report.notes or "")
        self.assertTrue(report.publish)

    def test_a_compile_error_is_the_wrong_reason(self) -> None:
        self.make_base(BUGGY)
        commit(self.repo, "test: reproduce", {"toy/src/lib.rs": with_test(BUGGY, call="sum")})
        fixed = FIXED + "pub fn sum(a: i32, b: i32) -> i32 {\n    a + b\n}\n"
        commit(self.repo, "fix: sum", {"toy/src/lib.rs": with_test(fixed, call="sum")})
        report, _ = self.run_verify(sample_result())
        self.assertEqual(report.verification, "unverified")
        self.assertIn("for the right reason", report.notes or "")

    def test_a_migration_is_never_published_or_run(self) -> None:
        self.make_base(BUGGY)
        commit(self.repo, "test: reproduce", {"toy/src/lib.rs": with_test(BUGGY)})
        commit(
            self.repo,
            "fix: add",
            {"toy/src/lib.rs": with_test(FIXED), "migrations/20991231_x.sql": "SELECT 1;\n"},
        )
        report, runner = self.run_verify(sample_result())
        self.assertEqual(report.verification, "unverified")
        self.assertFalse(report.publish)
        self.assertIn("protected paths: migrations/20991231_x.sql", report.notes or "")
        self.assertEqual(runner.calls, [])

    def test_a_disallowed_command_is_not_executed(self) -> None:
        self.make_base(BUGGY)
        commit(self.repo, "test: reproduce", {"toy/src/lib.rs": with_test(BUGGY)})
        commit(self.repo, "fix: add", {"toy/src/lib.rs": with_test(FIXED)})
        result = sample_result(test={"command": "cargo test -p toy adds; touch pwned"})
        report, runner = self.run_verify(result)
        self.assertEqual(report.verification, "unverified")
        self.assertIn("The test command was not run", report.notes or "")
        self.assertFalse(any("touch" in call or "adds;" in call for call in runner.calls))
        self.assertFalse((self.repo / "pwned").exists())

    def test_a_new_warning_is_unverified(self) -> None:
        self.make_base(BUGGY)
        commit(self.repo, "test: reproduce", {"toy/src/lib.rs": with_test(BUGGY)})
        noisy = FIXED.replace("    a + b", "    let unused = 1;\n    a + b")
        commit(self.repo, "fix: add", {"toy/src/lib.rs": with_test(noisy)})
        report, _ = self.run_verify(sample_result())
        self.assertEqual(report.verification, "unverified")
        self.assertIn("new warning", report.notes or "")

    def test_a_test_already_failing_on_main_does_not_count(self) -> None:
        legacy = {"toy/tests/legacy.rs": "#[test]\nfn broken_on_main() {\n    assert!(false);\n}\n"}
        self.make_base(BUGGY, legacy)
        commit(self.repo, "test: reproduce", {"toy/src/lib.rs": with_test(BUGGY)})
        commit(self.repo, "fix: add", {"toy/src/lib.rs": with_test(FIXED)})
        report, runner = self.run_verify(sample_result())
        self.assertEqual(report.verification, "verified", report.notes)
        self.assertIn("broken_on_main (--test legacy)", report.notes or "")
        self.assertIn("pre-existing on main", report.notes or "")
        rerun = ("cargo", "test", "--no-fail-fast", "-p", "toy", "--test", "legacy",
                 "--", "--exact", "broken_on_main")  # fmt: skip
        self.assertIn(rerun, runner.calls)

    def test_a_fix_that_breaks_another_test_is_unverified(self) -> None:
        # Passes on the buggy base, where add(2, 2) == 0.
        twice = {
            "toy/tests/twice.rs": "#[test]\nfn doubles() {\n    assert_eq!(toy::add(2, 2), 0);\n}\n"
        }
        self.make_base(BUGGY, twice)
        commit(self.repo, "test: reproduce", {"toy/src/lib.rs": with_test(BUGGY)})
        commit(self.repo, "fix: add", {"toy/src/lib.rs": with_test(FIXED)})
        report, _ = self.run_verify(sample_result())
        self.assertEqual(report.verification, "unverified")
        self.assertIn("1 test(s) fail on the fix: doubles (--test twice)", report.notes or "")
        self.assertNotIn("pre-existing", report.notes or "")

    def test_wrong_commit_count_is_unverified_without_running(self) -> None:
        self.make_base(BUGGY)
        commit(self.repo, "fix: add", {"toy/src/lib.rs": with_test(FIXED)})
        report, runner = self.run_verify(sample_result())
        self.assertEqual(report.verification, "unverified")
        self.assertIn("needs 2 new commit(s), found 1", report.notes or "")
        self.assertEqual(runner.calls, [])


class NoFixTest(unittest.TestCase):
    """Outcomes without a fix never run anything."""

    def setUp(self) -> None:
        self.workspace = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.workspace, ignore_errors=True)
        self.base_shas = {}
        for name in ("granite-webhooks", "general_datebase"):
            repo = init_repo(self.workspace / name)
            self.base_shas[name] = commit(repo, "init", {"README.md": name})

    def no_fix_result(self) -> dict:
        return sample_result(
            outcome="not_a_code_bug",
            target_repo=None,
            test={"status": "not_applicable", "files": [], "command": None},
        )

    def test_not_applicable(self) -> None:
        runner = RecordingRunner()
        agent = AgentRun("success", self.no_fix_result(), 0.1, None)
        report = verify(Inputs(self.workspace, self.base_shas, agent), Session.start(runner, 60))
        self.assertEqual(
            (report.conclusion, report.verification, report.publish),
            ("completed", "not_applicable", False),
        )
        self.assertIsNone(report.notes)
        self.assertEqual(runner.calls, [])

    def test_stray_commits_are_noted_not_published(self) -> None:
        commit(self.workspace / "general_datebase", "oops", {"a.ts": "x"})
        agent = AgentRun("success", self.no_fix_result(), 0.1, None)
        report = verify(
            Inputs(self.workspace, self.base_shas, agent), Session.start(RecordingRunner(), 60)
        )
        self.assertEqual(report.verification, "not_applicable")
        self.assertFalse(report.publish)
        self.assertIn("general_datebase: has commits", report.notes or "")

    def test_usage_limited_agent(self) -> None:
        agent = AgentRun("usage_limited", None, None, "limit reached, resets 5pm")
        report = verify(
            Inputs(self.workspace, self.base_shas, agent), Session.start(RecordingRunner(), 60)
        )
        self.assertEqual(
            (report.conclusion, report.verification, report.publish),
            ("usage_limited", "not_applicable", False),
        )


if __name__ == "__main__":
    unittest.main()
