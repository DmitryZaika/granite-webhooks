"""Branch naming, labels, titles and escaping in the pull request body."""

import unittest

from helpers import ISSUE_ID, sample_result

from autofix import pr_body


class NamingTest(unittest.TestCase):
    def test_branch(self) -> None:
        self.assertEqual(pr_body.branch_name(ISSUE_ID, "42"), "ai-fix/posthog-0192f3a4-42")

    def test_labels(self) -> None:
        self.assertEqual(pr_body.labels("fixed_with_test", "verified"), ["ai-fix"])
        self.assertEqual(
            pr_body.labels("fixed_with_test", "unverified"), ["ai-fix", "ai-fix:unverified"]
        )
        self.assertEqual(
            pr_body.labels("fixed_without_test", "not_applicable"), ["ai-fix", "needs-manual-test"]
        )

    def test_title(self) -> None:
        self.assertEqual(pr_body.title(sample_result()), "fix(toy): add instead of subtract")
        fallback = pr_body.title(sample_result(pr_title=None, summary="Saving\na slab fails"))
        self.assertEqual(fallback, "fix: Saving a slab fails")
        long = pr_body.title(sample_result(pr_title=None, summary="x" * 300))
        self.assertEqual(len(long), 100)
        self.assertNotIn("@dima", pr_body.title(sample_result(pr_title="fix: ping @dima")))


class EscapeTest(unittest.TestCase):
    def test_html_links_and_mentions(self) -> None:
        text = pr_body.escape(
            "<img src=x onerror=alert(1)> & ![t](https://evil.example/p.png) cc @DmitryZaika"
        )
        self.assertNotIn("<img", text)
        self.assertIn("&lt;img", text)
        self.assertIn("&amp;", text)
        self.assertIn("!\\[t\\](https", text)
        self.assertNotIn("@DmitryZaika", text)
        self.assertIn("@\u200bDmitryZaika", text)

    def test_caps_length(self) -> None:
        self.assertEqual(len(pr_body.escape("y" * 5000, 100)), 100)

    def test_code_block_cannot_be_closed_from_inside(self) -> None:
        block = pr_body.code_block("before\n````\n<b>after</b>")
        fence = block.split("text", 1)[0]
        self.assertEqual(fence, "`````")
        self.assertTrue(block.endswith("\n`````"))


class RenderTest(unittest.TestCase):
    def render(self, result: dict, verification: str, notes: str | None = None) -> str:
        inputs = pr_body.BodyInputs(
            ISSUE_ID, result, verification, notes, "https://github.com/x/actions/runs/1", 1.5
        )
        return pr_body.render(inputs)

    def test_verified_body(self) -> None:
        body = self.render(sample_result(root_cause="Uses <minus> @someone"), "verified")
        self.assertIn(f"https://us.posthog.com/project/141685/error-tracking/{ISSUE_ID}", body)
        self.assertIn("**Verified.**", body)
        self.assertIn("cargo test -p toy adds", body)
        self.assertIn("&lt;minus&gt;", body)
        self.assertNotIn("@someone", body)
        self.assertIn("Agent cost: $1.50", body)
        self.assertIn("https://github.com/x/actions/runs/1", body)

    def test_a_tested_fix_also_says_how_to_check_it_in_the_app(self) -> None:
        result = sample_result()
        result["test"] = {**result["test"], "manual_test_steps": "1. Open a <slab>\n2. Sell it"}
        body = self.render(result, "verified")
        self.assertIn("**Check it in the app:**", body)
        self.assertIn("1. Open a &lt;slab&gt;", body)
        self.assertNotIn("Check it in the app", self.render(sample_result(), "verified"))

    def test_unverified_body_carries_the_evidence(self) -> None:
        body = self.render(sample_result(), "unverified", "test result: ok. 1 passed")
        self.assertIn("**Unverified**", body)
        self.assertIn("<details><summary>Evidence</summary>", body)
        self.assertIn("test result: ok. 1 passed", body)

    def test_without_a_test(self) -> None:
        result = sample_result(
            outcome="fixed_without_test",
            test={
                "status": "not_possible",
                "files": [],
                "command": None,
                "why_not_possible": "A Safari-only quirk.",
                "manual_test_steps": "1. Open Safari.",
            },
        )
        body = self.render(result, "not_applicable")
        self.assertIn("A Safari-only quirk.", body)
        self.assertIn("1. Open Safari.", body)
        self.assertIn("needs a manual test", body)


if __name__ == "__main__":
    unittest.main()
