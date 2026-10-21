"""The agent step: the CLI's argv, its environment and the saved output."""

import json
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from helpers import ISSUE_ID, RUN_ID, TOOLS_DIR, run_context

from autofix import agent
from autofix.agent import AgentFiles, ConfigError, Limits
from autofix.proc import Completed, RunAs


def flag_value(argv: list[str], flag: str) -> str:
    return argv[argv.index(flag) + 1]


class ArgvTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp())
        self.run = run_context(self.tmp, self.tmp / "out")
        self.files = AgentFiles.in_dir(TOOLS_DIR)
        self.argv = agent.build_argv(self.run, self.files, Limits("high", 80, 7.5))

    def test_core_flags(self) -> None:
        argv = self.argv
        self.assertEqual(argv[:2], ["claude", "-p"])
        self.assertEqual(
            argv[2],
            f"Investigate PostHog issue {ISSUE_ID} (auto-fix run {RUN_ID}) "
            "and follow your instructions.",
        )
        self.assertIn("--restricted", argv)
        self.assertNotIn("--bare", argv)
        self.assertEqual(flag_value(argv, "--tools"), "Bash,Read,Edit,Write,Grep,Glob")
        self.assertEqual(flag_value(argv, "--permission-mode"), "dontAsk")
        self.assertEqual(flag_value(argv, "--output-format"), "json")
        self.assertEqual(flag_value(argv, "--model"), "claude-opus-5-5")
        self.assertEqual(flag_value(argv, "--effort"), "high")
        self.assertEqual(flag_value(argv, "--max-turns"), "80")
        self.assertEqual(flag_value(argv, "--max-budget-usd"), "7.5")
        for flag in ("--strict-mcp-config", "--no-session-persistence", "--disable-slash-commands"):
            self.assertIn(flag, argv)

    def test_tool_files_and_schema(self) -> None:
        argv = self.argv
        self.assertEqual(flag_value(argv, "--settings"), str(TOOLS_DIR / "settings.json"))
        self.assertEqual(flag_value(argv, "--mcp-config"), str(TOOLS_DIR / "mcp.json"))
        self.assertEqual(
            flag_value(argv, "--append-system-prompt-file"), str(TOOLS_DIR / "fixer-prompt.md")
        )
        schema = json.loads(flag_value(argv, "--json-schema"))
        self.assertEqual(schema["title"], "PostHog auto-fix result")

    def test_allowed_tools_are_one_argument_each_and_end_before_the_next_flag(self) -> None:
        start = self.argv.index("--allowedTools") + 1
        end = self.argv.index("--settings")
        rules = self.argv[start:end]
        self.assertEqual(rules, agent.allowed_tools())
        for rule in ("Read", "Edit", "Write", "Grep", "Glob", "mcp__posthog"):
            self.assertIn(rule, rules)
        for rule in (
            "Bash(cargo test *)",
            "Bash(git status)",
            "Bash(bunx vitest run *)",
            "Bash(mysql -h 127.0.0.1 -P 3307 -uroot -pgranite *)",
        ):
            self.assertIn(rule, rules)
        self.assertFalse(any("push" in rule or "curl" in rule for rule in rules))

    def test_settings_wire_the_guard_fail_closed(self) -> None:
        settings = json.loads((TOOLS_DIR / "settings.json").read_text())
        (entry,) = settings["hooks"]["PreToolUse"]
        self.assertEqual(entry["matcher"], "Bash|Read|Edit|Write|MultiEdit|NotebookEdit|Grep|Glob")
        (hook,) = entry["hooks"]
        self.assertEqual(hook["onFailure"], "block")
        self.assertIn("guard.py", hook["command"])
        deny = settings["permissions"]["deny"]
        for rule in ("Bash(git push *)", "Bash(curl *)", "Bash(gh *)", "WebFetch", "WebSearch"):
            self.assertIn(rule, deny)


class LimitsTest(unittest.TestCase):
    def test_defaults(self) -> None:
        self.assertEqual(Limits.from_env({}), Limits("medium", 150, 10.0))
        self.assertEqual(Limits.from_env({"AUTOFIX_EFFORT": ""}), Limits("medium", 150, 10.0))

    def test_rejects_bad_values(self) -> None:
        for env in (
            {"AUTOFIX_EFFORT": "extreme"},
            {"AUTOFIX_MAX_TURNS": "lots"},
            {"AUTOFIX_MAX_TURNS": "0"},
            {"AUTOFIX_BUDGET_USD": "-1"},
            {"AUTOFIX_BUDGET_USD": "500"},
        ):
            with self.subTest(env=env), self.assertRaises(ConfigError):
                Limits.from_env(env)


MCP = {"POSTHOG_AUTOFIX_MCP_KEY": "mcp-secret"}


class EnvTest(unittest.TestCase):
    def test_prefers_the_subscription_token(self) -> None:
        secrets = agent.agent_secrets(
            {"CLAUDE_CODE_OAUTH_TOKEN": "oauth", "ANTHROPIC_API_KEY": "key", **MCP, "OTHER": "x"}
        )
        self.assertEqual(secrets, {"CLAUDE_CODE_OAUTH_TOKEN": "oauth", **MCP})

    def test_falls_back_to_the_api_key(self) -> None:
        secrets = agent.agent_secrets(
            {"CLAUDE_CODE_OAUTH_TOKEN": "", "ANTHROPIC_API_KEY": "k", **MCP}
        )
        self.assertEqual(secrets, {"ANTHROPIC_API_KEY": "k", **MCP})

    def test_needs_a_credential_and_the_mcp_key(self) -> None:
        for env in ({}, MCP, {"CLAUDE_CODE_OAUTH_TOKEN": "oauth"}):
            with self.subTest(env=env), self.assertRaises(ConfigError):
                agent.agent_secrets(env)

    def test_plain_variables(self) -> None:
        variables = agent.agent_variables(TOOLS_DIR, Path("/srv/autofix/work"))
        self.assertEqual(
            variables,
            {
                "CLAUDE_CODE_SUBPROCESS_ENV_SCRUB": "1",
                "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
                "DISABLE_TELEMETRY": "1",
                "ENABLE_CLAUDEAI_MCP_SERVERS": "false",
                "AUTOFIX_TOOLS_DIR": str(TOOLS_DIR),
                "AUTOFIX_WORKSPACE": "/srv/autofix/work",
                "NO_COLOR": "1",
            },
        )


AGENT_USER = RunAs("autofix", env={"PATH": "/srv/autofix/bin:/usr/bin"})


class RecordingRunner:
    def __init__(self, completed: Completed) -> None:
        self.completed = completed
        self.calls: list[dict] = []

    def __call__(self, argv, cwd, timeout, env=None, run_as=None) -> Completed:
        self.calls.append({"argv": list(argv), "cwd": cwd, "env": env, "run_as": run_as})
        return self.completed


class RunAgentTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp())
        self.run = run_context(self.tmp, self.tmp / "out")

    def test_runs_as_the_agent_user_with_secrets_out_of_the_argv(self) -> None:
        runner = RecordingRunner(Completed(("claude",), 0, '{"subtype":"success"}', "", False))
        env = {"CLAUDE_CODE_OAUTH_TOKEN": "oauth-secret", **MCP}
        with mock.patch.dict("os.environ", env, clear=True):
            agent.run_agent(self.run, TOOLS_DIR, AGENT_USER, runner)
        cli, kill = runner.calls
        self.assertEqual(cli["cwd"], self.tmp)
        self.assertEqual(cli["run_as"].user, "autofix")
        self.assertEqual(cli["run_as"].secrets, {"CLAUDE_CODE_OAUTH_TOKEN": "oauth-secret", **MCP})
        self.assertEqual(cli["env"]["ENABLE_CLAUDEAI_MCP_SERVERS"], "false")
        wrapped = cli["run_as"].wrap(cli["argv"], cli["env"])
        self.assertFalse(any("secret" in word for word in wrapped))
        self.assertEqual(kill["argv"], ["sudo", "-n", "pkill", "-KILL", "-u", "autofix"])

    def test_saves_output_and_never_raises_on_agent_errors(self) -> None:
        runner = RecordingRunner(
            Completed(("claude",), 1, '{"subtype":"error_max_turns"}', "boom", True)
        )
        with mock.patch.dict("os.environ", {"CLAUDE_CODE_OAUTH_TOKEN": "t", **MCP}, clear=True):
            agent.run_agent(self.run, TOOLS_DIR, AGENT_USER, runner)
        self.assertEqual(self.run.agent_output.read_text(), '{"subtype":"error_max_turns"}')
        self.assertIn("stopped after 55 minutes", self.run.agent_stderr.read_text())

    def test_missing_credentials_are_recorded_not_raised(self) -> None:
        def runner(*args, **kwargs):
            raise AssertionError("the CLI must not start without credentials")

        with mock.patch.dict("os.environ", {}, clear=True):
            agent.run_agent(self.run, TOOLS_DIR, AGENT_USER, runner)
        self.assertIn("CLAUDE_CODE_OAUTH_TOKEN", self.run.agent_stderr.read_text())


if __name__ == "__main__":
    unittest.main()
