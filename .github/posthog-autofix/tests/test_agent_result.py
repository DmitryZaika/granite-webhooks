"""Classifying the CLI's output and validating the agent's structured result."""

import json
import unittest

from helpers import sample_result

from autofix.agent_result import ResultError, classify, mentions_usage_limit, validate_result


def cli_output(**fields: object) -> str:
    return json.dumps({"type": "result", "total_cost_usd": 1.23456, **fields})


class UsageLimitTest(unittest.TestCase):
    def test_limit_messages(self) -> None:
        for text in (
            "Claude AI usage limit reached|1760000000",
            "You've hit your session limit · resets 3pm",
            "5-hour limit reached ∙ resets 7pm",
            "API Error: 429 Too Many Requests",
        ):
            with self.subTest(text=text):
                self.assertTrue(mentions_usage_limit(text))

    def test_other_errors(self) -> None:
        for text in (
            "Reached maximum number of turns (150)",
            "Exceeded USD budget (10)",
            "the line limit of the file",
            "error code 4290",
            "",
        ):
            with self.subTest(text=text):
                self.assertFalse(mentions_usage_limit(text))


class ClassifyTest(unittest.TestCase):
    def test_success(self) -> None:
        run = classify(
            cli_output(subtype="success", result="done", structured_output=sample_result()), ""
        )
        self.assertEqual(run.kind, "success")
        self.assertEqual(run.result, sample_result())
        self.assertEqual(run.cost_usd, 1.2346)

    def test_success_without_structured_output_is_a_failure(self) -> None:
        run = classify(cli_output(subtype="success", result="done"), "")
        self.assertEqual(run.kind, "failed")
        self.assertIn("without structured_output", run.notes or "")

    def test_invalid_result_is_a_failure(self) -> None:
        run = classify(
            cli_output(subtype="success", structured_output={"outcome": "fixed_with_test"}), ""
        )
        self.assertEqual(run.kind, "failed")
        self.assertIn("invalid", run.notes or "")
        self.assertIsNone(run.result)

    def test_usage_limited(self) -> None:
        run = classify(
            cli_output(subtype="error_during_execution", is_error=True),
            "Claude usage limit reached; resets at 5pm",
        )
        self.assertEqual(run.kind, "usage_limited")
        self.assertIsNone(run.result)

    def test_max_turns_is_a_plain_failure(self) -> None:
        run = classify(cli_output(subtype="error_max_turns"), "")
        self.assertEqual(run.kind, "failed")
        self.assertIn("error_max_turns", run.notes or "")

    def test_unparseable_output(self) -> None:
        run = classify("Segmentation fault", "node crashed")
        self.assertEqual(run.kind, "failed")
        self.assertIsNone(run.cost_usd)
        self.assertIn("node crashed", run.notes or "")

    def test_a_cli_crash_keeps_the_error_and_drops_the_bundle(self) -> None:
        # The first live runs (2026-10-25): the CLI exited at startup.
        bundle = 'import{Ne,ws}from"/$bunfs/root/chunk-zaqkg6cw.js";' + "x" * 600
        stderr = "\n".join(
            [
                "…nd-compliance.",
                "10 | ",
                f"11 | {bundle}",
                "",
                "error: bubblewrap is required for subprocess env scrubbing and isolation.",
                "      at tNo (/$bunfs/root/chunk-emvxgzg6.js:11:7202)",
                "      at async <anonymous> (/$bunfs/root/chunk-hjb4qtaj.js:27:3195)",
                "",
                "Bun v1.4.4 (Linux x64)",
            ]
        )
        run = classify("", stderr)
        self.assertEqual(run.kind, "failed")
        notes = run.notes or ""
        self.assertIn("error: bubblewrap is required", notes)
        self.assertNotIn("$bunfs", notes)
        self.assertNotIn("import{", notes)

    def test_reads_the_last_json_line(self) -> None:
        stdout = "warming up\n" + cli_output(subtype="success", structured_output=sample_result())
        self.assertEqual(classify(stdout, "").kind, "success")


class ValidateTest(unittest.TestCase):
    def test_keeps_only_schema_fields(self) -> None:
        data = sample_result()
        data["extra"] = "ignored"
        data["test"]["extra"] = "ignored"
        result = validate_result(data)
        self.assertNotIn("extra", result)
        self.assertNotIn("extra", result["test"])

    def test_rejections(self) -> None:
        cases = {
            "unknown outcome": sample_result(outcome="fixed"),
            "unknown repo": sample_result(target_repo="other"),
            "fix without repo": sample_result(target_repo=None),
            "missing field": {k: v for k, v in sample_result().items() if k != "risk"},
            "too long title": sample_result(pr_title="x" * 101),
            "empty summary": sample_result(summary=""),
            "wrong type": sample_result(root_cause=5),
            "test without command": sample_result(test={"command": None}),
            "files not a list": sample_result(test={"files": "a.rs"}),
            "unknown test status": sample_result(test={"status": "maybe"}),
            "not an object": [],
        }
        for name, data in cases.items():
            with self.subTest(case=name), self.assertRaises(ResultError):
                validate_result(data)

    def test_no_fix_result(self) -> None:
        data = sample_result(
            outcome="not_a_code_bug",
            target_repo=None,
            fix_summary="",
            pr_title=None,
            recommendation="Suppress it in PostHog.",
            test={"status": "not_applicable", "files": [], "command": None},
        )
        self.assertEqual(validate_result(data)["outcome"], "not_a_code_bug")


if __name__ == "__main__":
    unittest.main()
