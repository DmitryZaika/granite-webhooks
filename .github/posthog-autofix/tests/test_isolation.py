"""The sandbox's pure parts: pins, PATH, argv, probes, the outbox copy, and
the paths the workflow and isolate.sh share with isolation.py."""

import os
import re
import subprocess
import tempfile
import unittest
from pathlib import Path

from helpers import TOOLS_DIR

from autofix import database, isolation
from autofix.isolation import IsolationError
from autofix.proc import Completed, RunAs

WORKFLOW = TOOLS_DIR.parent / "workflows" / "posthog-autofix.yml"


def resolver(table: dict[str, list[str]]):
    return lambda host: table.get(host, [])


GOOD_DNS = {
    "api.anthropic.com": ["160.79.104.10"],
    "platform.claude.com": ["160.79.104.11"],
    "mcp.posthog.com": ["104.18.1.1", "104.18.2.2"],
}


class PinsTest(unittest.TestCase):
    def test_pins_every_address(self) -> None:
        self.assertEqual(
            isolation.resolve_pins(resolver(GOOD_DNS)),
            [
                ("api.anthropic.com", "160.79.104.10"),
                ("platform.claude.com", "160.79.104.11"),
                ("mcp.posthog.com", "104.18.1.1"),
                ("mcp.posthog.com", "104.18.2.2"),
            ],
        )

    def test_rejects_an_api_address_outside_the_published_range(self) -> None:
        with self.assertRaisesRegex(IsolationError, "outside 160.79.104.0/23"):
            isolation.resolve_pins(resolver({**GOOD_DNS, "api.anthropic.com": ["1.2.3.4"]}))

    def test_rejects_a_missing_or_odd_answer(self) -> None:
        for table in (
            {**GOOD_DNS, "mcp.posthog.com": []},
            {**GOOD_DNS, "platform.claude.com": ["::1"]},
        ):
            with self.subTest(table=table), self.assertRaises(IsolationError):
                isolation.resolve_pins(resolver(table))

    def test_isolate_argv(self) -> None:
        argv = isolation.isolate_argv("/opt/bun", "/home/runner", [("a.example", "1.2.3.4")])
        self.assertEqual(
            argv,
            [
                "sudo", "-n", "bash", "/srv/autofix/tools/isolate.sh", "/srv/autofix",
                "autofix", "/opt/bun", "/home/runner", "a.example=1.2.3.4",
            ],
        )  # fmt: skip


class PathTest(unittest.TestCase):
    SYSROOT = Path("/srv/autofix/rustup/toolchains/stable-x86_64-unknown-linux-gnu")

    def which(self, found: dict[str, str]):
        return lambda tool: found.get(tool)

    def test_agent_path(self) -> None:
        path = isolation.agent_path(
            self.SYSROOT,
            self.which({"node": "/usr/local/bin/node", "claude": "/usr/local/bin/claude"}),
        )
        self.assertEqual(
            path,
            f"/srv/autofix/bin:{self.SYSROOT}/bin:/usr/local/bin:/usr/bin:/bin",
        )

    def test_rejects_tools_the_agent_user_cannot_reach(self) -> None:
        cases = {
            "home": (self.SYSROOT, {"node": "/home/runner/.nvm/node", "claude": "/usr/bin/claude"}),
            "missing": (self.SYSROOT, {"node": "/usr/bin/node"}),
            "toolchain": (
                Path("/home/runner/.rustup/x"),
                {"node": "/usr/bin/node", "claude": "/c"},
            ),
        }
        for name, (sysroot, found) in cases.items():
            with self.subTest(case=name), self.assertRaises(IsolationError):
                isolation.agent_path(sysroot, self.which(found))

    def test_child_env_is_offline(self) -> None:
        env = isolation.child_env("/p")
        self.assertEqual(env["CARGO_NET_OFFLINE"], "true")
        self.assertEqual(
            (env["CARGO_HOME"], env["RUSTUP_HOME"]), ("/srv/autofix/cargo", "/srv/autofix/rustup")
        )
        self.assertEqual(env["PATH"], "/p")


class ProbeTest(unittest.TestCase):
    def test_network_probes_block_the_world_and_allow_mysql_and_the_api(self) -> None:
        probes = isolation.network_probes("140.82.112.3")
        blocked = [args for ok, args in probes if not ok]
        allowed = [args for ok, args in probes if ok]
        self.assertIn(["tcp", "140.82.112.3", "443"], blocked)
        self.assertIn(["tcp", "1.1.1.1", "443"], blocked)
        self.assertIn(["resolve", "example.com"], blocked)
        self.assertIn(["udp-dns", "8.8.8.8"], blocked)
        self.assertEqual(
            allowed, [["tcp", "127.0.0.1", "3307"], ["tcp", "api.anthropic.com", "443"]]
        )

    def test_check_reports_each_probe_that_misbehaves(self) -> None:
        calls = []

        def runner(argv, cwd, timeout, env=None, run_as=None):
            calls.append((list(argv), run_as))
            leaked = argv[-2:] == ["1.1.1.1", "443"]
            return Completed(tuple(argv), 0 if leaked else 1, "", "", False)

        run_as = RunAs("autofix")
        probes = [
            (False, ["tcp", "1.1.1.1", "443"]),
            (False, ["tcp", "10.0.0.1", "443"]),
            (True, ["git", "--version"]),
        ]
        failures = isolation.check(run_as, probes, runner)
        self.assertEqual(len(failures), 2)
        self.assertIn("tcp 1.1.1.1 443` should fail as autofix", failures[0])
        self.assertIn("git --version` should succeed", failures[1])
        self.assertEqual(
            calls[0][0], ["python3", "-I", str(isolation.PROBE), "tcp", "1.1.1.1", "443"]
        )
        self.assertTrue(all(run_as_used is run_as for _, run_as_used in calls))

    def test_probe_script(self) -> None:
        def probe(*args: str) -> int:
            return subprocess.run(
                ["python3", "-I", str(isolation.PROBE), *args], check=False
            ).returncode

        self.assertEqual(probe("resolve", "localhost"), 0)
        self.assertEqual(probe("unix", "/nonexistent.sock"), 1)
        self.assertEqual(probe("tcp", "127.0.0.1", "1"), 1)
        self.assertEqual(probe("bogus", "x"), 2)
        self.assertEqual(probe("tcp", "127.0.0.1"), 2)


class CopyOutTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp())

    def test_copies_a_regular_file(self) -> None:
        (self.tmp / "fix.bundle").write_bytes(b"bundle")
        isolation.copy_out(self.tmp / "fix.bundle", self.tmp / "out.bundle")
        self.assertEqual((self.tmp / "out.bundle").read_bytes(), b"bundle")

    def test_refuses_a_symlink(self) -> None:
        (self.tmp / "secret").write_text("runner-only")
        os.symlink(self.tmp / "secret", self.tmp / "fix.bundle")
        with self.assertRaises(OSError):
            isolation.copy_out(self.tmp / "fix.bundle", self.tmp / "out.bundle")
        self.assertFalse((self.tmp / "out.bundle").exists())


class SharedLayoutTest(unittest.TestCase):
    """The workflow's literal paths must be isolation.py's."""

    def test_workflow_uses_the_layout(self) -> None:
        text = WORKFLOW.read_text()
        for line in (
            f"AUTOFIX_WORKSPACE: {isolation.WORK}",
            f"CARGO_HOME: {isolation.CARGO_HOME}",
            f"RUSTUP_HOME: {isolation.RUSTUP_HOME}",
            f"working-directory: {isolation.TOOLS}",
            f"workspaces: {isolation.WORK}/granite-webhooks",
        ):
            self.assertIn(line, text)
        paths = set(re.findall(r"/srv/autofix[\w/.-]*", text))
        known = {
            str(path)
            for path in (
                isolation.ROOT,
                isolation.TOOLS,
                isolation.WORK,
                isolation.CARGO_HOME,
                isolation.RUSTUP_HOME,
            )
        }
        known.add(f"{isolation.WORK}/granite-webhooks")
        self.assertLessEqual(paths, known)

    def test_fix_job_never_saves_the_cache(self) -> None:
        fix_job = WORKFLOW.read_text().split("\n  fix:\n", 1)[1].split("\n  report:\n", 1)[0]
        self.assertIn("save-if: false", fix_job)

    def test_isolate_script_parses_and_uses_the_user(self) -> None:
        script = TOOLS_DIR / "isolate.sh"
        subprocess.run(["bash", "-n", str(script)], check=True)
        text = script.read_text()
        self.assertIn("--uid-owner", text)
        self.assertIn(str(isolation.ANTHROPIC_API_V4), text)
        self.assertIn("2607:6bc0::/48", text)


class DatabaseTest(unittest.TestCase):
    def test_start_argv(self) -> None:
        argv = database.start_argv()
        self.assertEqual(argv[:6], ["docker", "run", "-d", "--name", "mysql", "-p"])
        self.assertIn("3307:3306", argv)
        self.assertIn("mysql:8.0", argv)
        self.assertEqual(argv[-len(database.SERVER_FLAGS) :], database.SERVER_FLAGS)


if __name__ == "__main__":
    unittest.main()
