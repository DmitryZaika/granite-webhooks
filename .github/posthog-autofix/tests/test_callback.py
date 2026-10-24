"""The api callbacks: body shapes pinned to the Rust structs, and retries."""

import json
import re
import tempfile
import unittest
from pathlib import Path

from helpers import ISSUE_ID, TOOLS_DIR, run_context, sample_result

from autofix import callback
from autofix.callback import Api, CallbackError
from autofix.http import HttpError, Response

MODEL_RS = TOOLS_DIR.parent.parent / "api" / "src" / "posthog_autofix" / "model.rs"


def rust_fields(struct: str) -> set[str]:
    source = MODEL_RS.read_text()
    match = re.search(rf"pub struct {struct} \{{(.*?)\n\}}", source, re.DOTALL)
    if match is None:
        raise AssertionError(f"{struct} not found in model.rs")
    return set(re.findall(r"^\s*pub (\w+):", match.group(1), re.MULTILINE))


class FakeTransport:
    def __init__(self, answers: list[Response | HttpError]) -> None:
        self.answers = answers
        self.requests: list[tuple[str, str, dict[str, str], bytes | None]] = []

    def __call__(self, method, url, headers, body) -> Response:
        self.requests.append((method, url, headers, body))
        answer = self.answers.pop(0)
        if isinstance(answer, HttpError):
            raise answer
        return answer


class PayloadTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp())
        self.run = run_context(self.tmp, self.tmp / "out")

    def test_started_matches_started_report(self) -> None:
        self.assertEqual(set(callback.started_payload(self.run)), rust_fields("StartedReport"))

    def test_result_matches_run_report_and_agent_result(self) -> None:
        payload = callback.run_report(
            self.run, "completed", sample_result(), "verified", None, "u", False, 1.0
        )
        self.assertEqual(set(payload), rust_fields("RunReport"))
        self.assertEqual(set(payload["agent"]), rust_fields("AgentResult"))
        self.assertEqual(set(payload["agent"]["test"]), rust_fields("TestReport"))
        self.assertEqual(payload["issue_id"], ISSUE_ID)

    def test_rejects_unknown_enums(self) -> None:
        with self.assertRaises(CallbackError):
            callback.run_report(self.run, "done", None, "verified", None, None, False, None)

    def write_files(self, verification: dict, pr: dict | None = None) -> None:
        output = {"subtype": "success", "total_cost_usd": 2.5, "structured_output": sample_result()}
        self.run.agent_output.write_text(json.dumps(output))
        self.run.verification_json.write_text(json.dumps(verification))
        if pr is not None:
            self.run.pr_json.write_text(json.dumps(pr))

    def test_result_from_files_prefers_the_publish_jobs_recheck(self) -> None:
        verification = {
            "conclusion": "completed",
            "verification": "verified",
            "notes": None,
            "publish": True,
        }
        pr = {
            "pr_url": "https://github.com/o/r/pull/7",
            "pr_draft": True,
            "verification": "unverified",
            "notes": "x",
        }
        self.write_files(verification, pr)
        payload = callback.result_from_files(self.run)
        self.assertEqual(payload["verification"], "unverified")
        self.assertEqual((payload["pr_url"], payload["pr_draft"]), (pr["pr_url"], True))
        self.assertEqual(payload["cost_usd"], 2.5)
        self.assertEqual(payload["agent"]["outcome"], "fixed_with_test")

    def test_usage_limited_result(self) -> None:
        self.run.agent_output.write_text(json.dumps({"subtype": "error_during_execution"}))
        self.run.agent_stderr.write_text("usage limit reached, resets 5pm")
        self.run.verification_json.write_text(
            json.dumps(
                {"conclusion": "usage_limited", "verification": "not_applicable", "notes": "limit"}
            )
        )
        payload = callback.result_from_files(self.run)
        self.assertEqual(
            (payload["conclusion"], payload["agent"], payload["pr_url"]),
            ("usage_limited", None, None),
        )

    def test_failure_without_any_files(self) -> None:
        payload = callback.failure_from_files(self.run, "The fix job ended with: failure.")
        self.assertEqual(payload["conclusion"], "failed")
        self.assertEqual(payload["verification"], "not_applicable")
        self.assertIsNone(payload["agent"])
        self.assertIn("The fix job ended with: failure.", payload["notes"])
        self.assertEqual(set(payload), rust_fields("RunReport"))


class PostTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp())
        self.run = run_context(self.tmp, self.tmp / "out")
        self.api = Api("https://api.example.com", "s3cret")
        self.sleeps: list[float] = []

    def post(self, transport: FakeTransport) -> None:
        callback.post(self.api, self.run, "started", {"a": 1}, transport, sleep=self.sleeps.append)

    def test_url_auth_and_body(self) -> None:
        transport = FakeTransport([Response(204, None)])
        self.post(transport)
        method, url, headers, body = transport.requests[0]
        self.assertEqual(
            (method, url), ("POST", "https://api.example.com/v1/posthog-autofix/runs/42/started")
        )
        self.assertEqual(headers["Authorization"], "Bearer s3cret")
        self.assertEqual(json.loads(body or b""), {"a": 1})

    def test_retries_network_errors_and_5xx(self) -> None:
        transport = FakeTransport(
            [HttpError(None, "reset"), Response(502, None), Response(200, None)]
        )
        self.post(transport)
        self.assertEqual(len(transport.requests), 3)
        self.assertEqual(self.sleeps, [2.0, 4.0])

    def test_gives_up_after_three_attempts(self) -> None:
        transport = FakeTransport([Response(500, None)] * 3)
        with self.assertRaises(CallbackError):
            self.post(transport)
        self.assertEqual(len(transport.requests), 3)

    def test_4xx_fails_without_retrying(self) -> None:
        transport = FakeTransport([Response(409, {"error": "not dispatched"})])
        with self.assertRaises(CallbackError):
            self.post(transport)
        self.assertEqual(len(transport.requests), 1)

    def test_api_config(self) -> None:
        api = Api.from_env(
            {"AUTOFIX_API_URL": "https://api.example.com/", "AUTOFIX_CALLBACK_SECRET": "s"}
        )
        self.assertEqual(api.base_url, "https://api.example.com")
        for env in (
            {"AUTOFIX_API_URL": "http://api.example.com", "AUTOFIX_CALLBACK_SECRET": "s"},
            {"AUTOFIX_API_URL": "https://api.example.com"},
        ):
            with self.subTest(env=env), self.assertRaises(CallbackError):
                Api.from_env(env)


if __name__ == "__main__":
    unittest.main()
