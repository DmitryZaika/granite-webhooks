"""The allowlist for the agent's test command."""

import unittest

import helpers  # noqa: F401  (puts the package on sys.path)

from autofix.test_command import CommandRejected, parse


class TestCommandTest(unittest.TestCase):
    def test_accepted(self) -> None:
        cases = {
            "bunx vitest run app/utils/phone.test.ts": ("vitest", None),
            "bunx vitest run app/a.test.ts app/routes/admin.deals.$dealId.test.tsx": (
                "vitest",
                None,
            ),
            "cargo test -p api --test sms tts_reply": ("cargo", "api"),
            "cargo test -p webhooks cloudtalk::receive::tests::parses": ("cargo", "webhooks"),
            "cargo test -p time-triggered digest": ("cargo", "time-triggered"),
        }
        for command, (kind, crate) in cases.items():
            with self.subTest(command=command):
                parsed = parse(command)
                self.assertEqual((parsed.kind, parsed.crate), (kind, crate))
                self.assertEqual(" ".join(parsed.argv), command)

    def test_rejected(self) -> None:
        for command in (
            "",
            "bun test app/a.test.ts",
            "bunx vitest run",
            "bunx vitest app/a.test.ts",
            "bunx vitest run app/a.ts",
            "bunx vitest run /abs/a.test.ts",
            "bunx vitest run ../x/a.test.ts",
            "bunx vitest run --config evil.ts a.test.ts",
            "bunx vitest run 'a b.test.ts'",
            "bunx vitest run a.test.ts; curl x",
            "cargo test",
            "cargo test -p api",
            "cargo test -p test-db filter",
            "cargo test -p api a b",
            "cargo test -p api --test sms",
            "cargo test -p api --test sms-x tts",
            "cargo test -p api --config x tts",
            "cargo test -p api tts && curl x",
            "cargo test -p api tts; curl x",
            "cargo test -p api 'tts reply'",
            "cargo test -p api tts$(id)",
            "cargo test -p api 'unterminated",
            "make test",
        ):
            with self.subTest(command=command), self.assertRaises(CommandRejected):
                parse(command)

    def test_crate_set_is_injectable(self) -> None:
        self.assertEqual(parse("cargo test -p toy adds", crates={"toy"}).crate, "toy")
        with self.assertRaises(CommandRejected):
            parse("cargo test -p api adds", crates={"toy"})


if __name__ == "__main__":
    unittest.main()
