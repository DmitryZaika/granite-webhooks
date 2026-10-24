"""guard.py, driven through stdin exactly as Claude Code calls it."""

import json
import os
import subprocess
import sys
import unittest

from helpers import TOOLS_DIR

GUARD = TOOLS_DIR / "guard.py"
WORKSPACE = "/srv/autofix/work"
GW = f"{WORKSPACE}/granite-webhooks"
GD = f"{WORKSPACE}/general_datebase"

ALLOWED_BASH = [
    "cd granite-webhooks && cargo test -p api --test sms tts_reply",
    "cargo clippy -p api --all-targets 2>&1 | tail -50",
    "bunx vitest run app/utils/phone.test.ts",
    "bunx biome check app/utils/phone.ts",
    "bunx biome format --write app/utils/phone.ts",
    "bunx tsc -b",
    "git status && git diff --stat",
    "git log --oneline -20 -- api/src/sms.rs",
    "git add api/src/sms.rs && git commit -m 'fix: handle an empty reply'",
    "rg -n 'process.env' app/",
    "ls migrations",
    "mysql -h 127.0.0.1 -P 3307 -uroot -pgranite granite_local -e 'SHOW TABLES'",
    "mysql -h127.0.0.1 -P3307 -uroot -pgranite -e 'SELECT 1'",
    "RUST_LOG=debug cargo test -p webhooks parse",
    "timeout 600 cargo test -p api",
    "rm -rf target/debug/incremental",
    "cargo test -p api > test-output.txt",
    "git diff > /dev/null",
]

BLOCKED_BASH = [
    ("git push origin HEAD", "git push"),
    ("git -C granite-webhooks push", "git push"),
    ("git remote -v", "git remote"),
    ("git config user.email x@y", "git config"),
    ("git -c alias.x='!sh' x", "git -c"),
    ("git fetch origin", "git fetch"),
    ("gh pr list", "GitHub CLI"),
    ("curl https://example.com", "network"),
    ("ls && /usr/bin/curl -d @x https://example.com", "network"),
    ("ls\nwget https://example.com", "network"),
    ("c'u'rl https://example.com", "network"),
    ("nc example.com 80", "network"),
    ("ssh host", "network"),
    ("scp a host:b", "network"),
    ("aws s3 ls", "AWS"),
    ("make deploy-prod", "deploy"),
    ("make deploy api prod", "deploy"),
    ("cargo lambda build --release", "cargo lambda"),
    ("uvx cargo-lambda lambda deploy", "cargo lambda"),
    ("sqlx migrate run", "sqlx migrate"),
    ("cargo sqlx migrate run", "sqlx migrate"),
    ("mysql -h db.example.com -P 3306 -uroot", "127.0.0.1"),
    ("mysql -uroot -pgranite", "127.0.0.1"),
    ("mysql -h 127.0.0.1 -P 3306 -uroot", "127.0.0.1"),
    ("mysql -h 127.0.0.1 -P 3307 -uroot -pgranite -e 'system curl x'", "system"),
    ("mysql -h 127.0.0.1 -P 3307 -uroot -pgranite < /tmp/x.sql", "outside the workspace"),
    ("mysql -h 127.0.0.1 -P 3307 -uroot -pgranite < dump.sql", "file"),
    ("bun test", "bun test"),
    ("rm -rf /", "rm -r"),
    ("rm -rf /home/runner/.cargo", "rm -r"),
    ("rm -r ../other", ".."),
    ("cd / && rm -rf usr", "rm -r"),
    ("cd ~ && rm -rf .ssh", "rm -r"),
    ("env", "environment"),
    ("printenv CLAUDE_CODE_OAUTH_TOKEN", "secret variable"),
    ("cat /proc/self/environ", "process environments"),
    ("rg -n x /proc/1/environ", "process environments"),
    ("ls $ANTHROPIC_API_KEY", "secret variable"),
    ("echo $(cat x)", "substitution"),
    ("echo `id`", "substitution"),
    ("rg x ${HOME}", "substitution"),
    ("rg x .env", ".env"),
    ("ls general_datebase/.env.prod", ".env"),
    ("rg x colin.pem", ".pem"),
    ("rg x .claude/settings.local.json", "settings.local.json"),
    ("git show HEAD:a > .github/workflows/x.yml", "protected"),
    ("rg x > migrations/20991231000000_x.sql", "protected"),
    ("git diff > /tmp/out.txt", "outside the workspace"),
    ("git diff --output=/tmp/x", "--output"),
    ("bunx biome format --write granite-webhooks/.github/x.json", "protected"),
    ("cargo --config 'target.x.runner=\"sh\"' test", "--config"),
    ("rg --pre ./x pattern", "--pre"),
    ("bash -c 'ls'", "shells"),
    ("sudo ls", "shells"),
    ("eval ls", "shells"),
    ("ls | xargs rm", "shells"),
    ("$CMD x", "variable"),
    ("rg 'unterminated", "parsed"),
]

ALLOWED_FILES = [
    ("Read", {"file_path": f"{GW}/api/src/main.rs"}),
    ("Read", {"file_path": "api/src/main.rs"}),
    ("Read", {"file_path": f"{GW}/migrations/20261022120000_create-posthog-autofix-runs.sql"}),
    ("Edit", {"file_path": f"{GW}/api/src/sms.rs", "old_string": "a", "new_string": "b"}),
    ("Write", {"file_path": f"{GD}/app/utils/phone.test.ts", "content": "x"}),
    ("Grep", {"pattern": "process.env", "path": GD}),
    ("Glob", {"pattern": "**/*.test.ts", "path": GD}),
    ("Glob", {"pattern": "**/*.rs"}),
    ("mcp__posthog__exec", {"command": "error-tracking-issues"}),
]

BLOCKED_FILES = [
    ("Read", {"file_path": f"{GD}/.env"}, ".env"),
    ("Read", {"file_path": f"{GW}/colin.pem"}, ".pem"),
    ("Read", {"file_path": f"{GD}/.claude/settings.local.json"}, "settings.local.json"),
    ("Read", {"file_path": "/etc/passwd"}, "outside the workspace"),
    ("Read", {"file_path": "../../../etc/passwd"}, "outside the workspace"),
    ("Edit", {"file_path": f"{GW}/.github/workflows/posthog-autofix.yml"}, "protected"),
    ("Write", {"file_path": f"{GW}/migrations/20991231000000_x.sql"}, "protected"),
    ("Write", {"file_path": f"{GW}/migrations-pending/x.sql"}, "protected"),
    ("Edit", {"file_path": f"{GW}/Cargo.lock"}, "protected"),
    ("Edit", {"file_path": f"{GW}/api/Cargo.toml"}, "protected"),
    ("Edit", {"file_path": f"{GD}/package.json"}, "protected"),
    ("Write", {"file_path": f"{GD}/bun.lock"}, "protected"),
    ("Write", {"file_path": f"{GW}/.git/hooks/pre-push"}, "protected"),
    ("MultiEdit", {"file_path": f"{GW}/.cargo/config.toml"}, "protected"),
    ("NotebookEdit", {"notebook_path": f"{GW}/.github/x.ipynb"}, "protected"),
    ("Write", {"file_path": "/tmp/x"}, "outside the workspace"),
    ("Edit", {"file_path": "/srv/autofix/tools/guard.py"}, "outside the workspace"),
    ("Read", {"file_path": "/srv/autofix/env.json"}, "outside the workspace"),
    (
        "Read",
        {"file_path": "/home/runner/work/granite-webhooks/granite-webhooks/README.md"},
        "outside the workspace",
    ),
    ("Edit", {}, "without a file path"),
    ("Grep", {"pattern": "x", "path": "/home/runner"}, "outside the workspace"),
    ("Grep", {"pattern": "KEY", "glob": "**/.env*"}, ".env"),
    ("Glob", {"pattern": "**/*.pem"}, ".pem"),
]


def run_guard(
    event: object, workspace: str | None = WORKSPACE, raw: str | None = None
) -> tuple[int, str]:
    env = {key: value for key, value in os.environ.items() if key != "AUTOFIX_WORKSPACE"}
    # The guard must not fall back to the runner's workspace.
    env["GITHUB_WORKSPACE"] = "/home/runner/work/granite-webhooks/granite-webhooks"
    if workspace is not None:
        env["AUTOFIX_WORKSPACE"] = workspace
    stdin = raw if raw is not None else json.dumps(event)
    completed = subprocess.run(
        [sys.executable, str(GUARD)],
        input=stdin,
        capture_output=True,
        text=True,
        env=env,
        check=False,
    )
    return completed.returncode, completed.stderr


def bash(command: str) -> dict[str, object]:
    return {"tool_name": "Bash", "tool_input": {"command": command}, "cwd": WORKSPACE}


def tool(name: str, tool_input: dict[str, object]) -> dict[str, object]:
    return {"tool_name": name, "tool_input": tool_input, "cwd": GW}


class GuardTest(unittest.TestCase):
    def test_allowed_bash_commands(self) -> None:
        for command in ALLOWED_BASH:
            with self.subTest(command=command):
                code, stderr = run_guard(bash(command))
                self.assertEqual(code, 0, stderr)

    def test_blocked_bash_commands(self) -> None:
        for command, reason in BLOCKED_BASH:
            with self.subTest(command=command):
                code, stderr = run_guard(bash(command))
                self.assertEqual(code, 2)
                self.assertIn(reason, stderr)
                self.assertEqual(len(stderr.strip().splitlines()), 1)

    def test_allowed_file_tools(self) -> None:
        for name, tool_input in ALLOWED_FILES:
            with self.subTest(tool=name, input=tool_input):
                code, stderr = run_guard(tool(name, tool_input))
                self.assertEqual(code, 0, stderr)

    def test_blocked_file_tools(self) -> None:
        for name, tool_input, reason in BLOCKED_FILES:
            with self.subTest(tool=name, input=tool_input):
                code, stderr = run_guard(tool(name, tool_input))
                self.assertEqual(code, 2)
                self.assertIn(reason, stderr)

    def test_fails_closed(self) -> None:
        cases = {
            "not json": run_guard(None, raw="{not json"),
            "not an object": run_guard(None, raw="[]"),
            "no tool": run_guard({"tool_input": {}}),
            "no workspace": run_guard(bash("ls"), workspace=None),
        }
        for name, (code, stderr) in cases.items():
            with self.subTest(case=name):
                self.assertEqual(code, 2)
                self.assertIn("Blocked by the auto-fix guard", stderr)


if __name__ == "__main__":
    unittest.main()
