"""Running commands, directly and as the agent user."""

import tempfile
import unittest
from pathlib import Path
from unittest import mock

import helpers  # noqa: F401  (puts the package on sys.path)

from autofix import proc
from autofix.proc import RunAs


class RunAsTest(unittest.TestCase):
    def test_prefix_with_plain_variables_and_named_secrets(self) -> None:
        run_as = RunAs(
            "autofix",
            env={"PATH": "/srv/autofix/bin:/usr/bin", "CARGO_NET_OFFLINE": "true"},
            secrets={"POSTHOG_AUTOFIX_MCP_KEY": "m", "CLAUDE_CODE_OAUTH_TOKEN": "o"},
        )
        self.assertEqual(
            run_as.wrap(["cargo", "test"], {"NO_COLOR": "1"}),
            [
                "sudo", "-n", "-u", "autofix", "-H",
                "--preserve-env=CLAUDE_CODE_OAUTH_TOKEN,POSTHOG_AUTOFIX_MCP_KEY",
                "--", "/usr/bin/env",
                "CARGO_NET_OFFLINE=true", "NO_COLOR=1", "PATH=/srv/autofix/bin:/usr/bin",
                "cargo", "test",
            ],
        )  # fmt: skip

    def test_no_preserve_flag_without_secrets(self) -> None:
        wrapped = RunAs("autofix").wrap(["git", "status"])
        self.assertEqual(
            wrapped, ["sudo", "-n", "-u", "autofix", "-H", "--", "/usr/bin/env", "git", "status"]
        )

    def test_extra_variables_override_the_baseline(self) -> None:
        wrapped = RunAs("autofix", env={"CI": "false"}).wrap(["x"], {"CI": "true"})
        self.assertIn("CI=true", wrapped)
        self.assertNotIn("CI=false", wrapped)

    def test_sudo_env_holds_only_path_and_secrets(self) -> None:
        run_as = RunAs("autofix", env={"CARGO_HOME": "/c"}, secrets={"ANTHROPIC_API_KEY": "k"})
        with mock.patch.dict("os.environ", {"PATH": "/usr/bin", "GH_TOKEN": "nope"}, clear=True):
            self.assertEqual(run_as.sudo_env(), {"PATH": "/usr/bin", "ANTHROPIC_API_KEY": "k"})

    def test_kill_argv(self) -> None:
        self.assertEqual(
            RunAs("autofix").kill_argv(), ["sudo", "-n", "pkill", "-KILL", "-u", "autofix"]
        )


class RunTest(unittest.TestCase):
    def test_env_is_added_to_this_process_environment(self) -> None:
        with mock.patch.dict("os.environ", {"KEEP": "1"}):
            completed = proc.run(
                ["/bin/sh", "-c", 'echo "$KEEP $ADDED"'], Path.cwd(), 10, env={"ADDED": "2"}
            )
        self.assertEqual(completed.stdout.strip(), "1 2")

    def test_merge_output_keeps_the_write_order(self) -> None:
        script = "echo one; echo two >&2; echo three"
        completed = proc.run(["/bin/sh", "-c", script], Path.cwd(), 10, merge_output=True)
        self.assertEqual(completed.stdout.split(), ["one", "two", "three"])
        self.assertEqual(completed.stderr, "")

    def test_timeout_stops_the_whole_group(self) -> None:
        marker = Path(tempfile.mkdtemp()) / "survived"
        script = f"(sleep 2; touch {marker}) & sleep 30"
        completed = proc.run(["/bin/sh", "-c", script], Path.cwd(), timeout=1)
        self.assertTrue(completed.timed_out)
        self.assertFalse(completed.ok)
        proc.run(["sleep", "2"], Path.cwd(), timeout=10)
        self.assertFalse(marker.exists())


if __name__ == "__main__":
    unittest.main()
