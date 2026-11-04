"""Bundle -> plan -> push -> pull request, on temp repos.

The "GitHub remote" is a local bare repo and the REST API a fake, so
nothing leaves the machine.
"""

import json
import shutil
import tempfile
import unittest
from pathlib import Path

from helpers import commit, git, init_repo, run_context, sample_result

from autofix import publish
from autofix.context import RunContext
from autofix.github import GitHub
from autofix.http import Response
from autofix.publish import Plan, PublishError


class FakeGitHub:
    def __init__(self) -> None:
        self.requests: list[tuple[str, str, object]] = []

    def __call__(self, method, url, headers, body) -> Response:
        payload = json.loads(body) if body else None
        self.requests.append((method, url, payload))
        if method == "GET" and "/pulls?" in url:
            return Response(200, [])
        if method == "POST" and url.endswith("/pulls"):
            return Response(
                201,
                {
                    "html_url": "https://github.com/DmitryZaika/general_datebase/pull/7",
                    "number": 7,
                    "draft": payload["draft"],
                },
            )
        return Response(200, [])


class PublishTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        # The fix job's workspace.
        self.workspace = self.tmp / "work"
        self.repo = init_repo(self.workspace / "general_datebase")
        self.base = commit(self.repo, "base", {"app/a.ts": "export const a = 1\n"})
        # GitHub: holds the base only.
        self.remote = self.tmp / "remote.git"
        git(self.tmp, "init", "--quiet", "--bare", "--initial-branch=main", str(self.remote))
        git(self.repo, "push", "--quiet", str(self.remote), "main")
        self.run = run_context(self.workspace, self.tmp / "out")
        self.bases = {"general_datebase": self.base, "granite-webhooks": "f" * 40}

    def make_fix(self, fix_files: dict[str, str]) -> str:
        commit(self.repo, "test: reproduce", {"app/a.test.ts": "test('a', () => {})\n"})
        return commit(self.repo, "fix: a", fix_files)

    def write_fix_job_files(self, fix: str, verification: str = "verified") -> None:
        result = sample_result(
            target_repo="general_datebase", test={"command": "bunx vitest run app/a.test.ts"}
        )
        output = {"subtype": "success", "total_cost_usd": 3.0, "structured_output": result}
        self.run.agent_output.write_text(json.dumps(output))
        report = {
            "conclusion": "completed",
            "verification": verification,
            "notes": None,
            "publish": True,
            "target_repo": "general_datebase",
            "base_sha": self.base,
            "fix_sha": fix,
        }
        self.run.verification_json.write_text(json.dumps(report))

    def report_job_checkout(self) -> Path:
        """The publish job's view: a depth-1 clone of the base, like actions/checkout."""
        target = self.tmp / "target"
        git(self.tmp, "clone", "--quiet", "--depth", "1", f"file://{self.remote}", str(target))
        return target

    def test_pushes_the_verified_fix_and_opens_a_ready_pull_request(self) -> None:
        fix = self.make_fix({"app/a.ts": "export const a = 2\n"})
        self.write_fix_job_files(fix)
        self.assertTrue(publish.create_bundle(self.run))
        chosen = publish.plan(self.run, self.bases)
        self.assertEqual(chosen, Plan(True, "general_datebase", self.base, fix))
        github = FakeGitHub()
        published = publish.open_pr(
            self.run,
            chosen,
            self.report_job_checkout(),
            "tok",
            GitHub("tok", github),
            push_url=str(self.remote),
        )
        self.assertEqual(
            git(self.remote, "rev-parse", "refs/heads/ai-fix/posthog-0192f3a4-42"), fix
        )
        self.assertEqual((published.pr_draft, published.verification), (False, "verified"))
        create = next(payload for method, url, payload in github.requests if url.endswith("/pulls"))
        self.assertEqual(
            (create["head"], create["base"], create["draft"]),
            ("ai-fix/posthog-0192f3a4-42", "main", False),
        )
        labels = next(
            payload for method, url, payload in github.requests if url.endswith("/labels")
        )
        self.assertEqual(labels, {"labels": ["ai-fix"]})
        self.assertEqual(json.loads(self.run.pr_json.read_text())["pr_url"], published.pr_url)

    def test_the_bug_id_names_the_branch(self) -> None:
        fix = self.make_fix({"app/a.ts": "export const a = 2\n"})
        self.write_fix_job_files(fix)
        publish.create_bundle(self.run)
        run = RunContext(**{**self.run.__dict__, "bug_id": "0123456789ab"})
        github = FakeGitHub()
        publish.open_pr(
            run,
            publish.plan(run, self.bases),
            self.report_job_checkout(),
            "tok",
            GitHub("tok", github),
            push_url=str(self.remote),
        )
        self.assertEqual(
            git(self.remote, "rev-parse", "refs/heads/ai-fix/bug-0123456789ab-42"), fix
        )
        create = next(payload for method, url, payload in github.requests if url.endswith("/pulls"))
        self.assertEqual(create["head"], "ai-fix/bug-0123456789ab-42")
        self.assertIn("**Bug ID:** `0123456789ab`", create["body"])

    def test_unverified_is_a_draft_with_its_label(self) -> None:
        fix = self.make_fix({"app/a.ts": "export const a = 2\n"})
        self.write_fix_job_files(fix, verification="unverified")
        publish.create_bundle(self.run)
        github = FakeGitHub()
        published = publish.open_pr(
            self.run,
            publish.plan(self.run, self.bases),
            self.report_job_checkout(),
            "tok",
            GitHub("tok", github),
            push_url=str(self.remote),
        )
        self.assertTrue(published.pr_draft)
        labels = next(
            payload for method, url, payload in github.requests if url.endswith("/labels")
        )
        self.assertEqual(labels, {"labels": ["ai-fix", "ai-fix:unverified"]})

    def test_a_forged_report_cannot_push_a_protected_path(self) -> None:
        fix = self.make_fix({"app/a.ts": "x\n", ".github/workflows/x.yml": "on: push\n"})
        self.write_fix_job_files(fix)
        publish.create_bundle(self.run)
        github = FakeGitHub()
        with self.assertRaises(PublishError):
            publish.open_pr(
                self.run,
                publish.plan(self.run, self.bases),
                self.report_job_checkout(),
                "tok",
                GitHub("tok", github),
                push_url=str(self.remote),
            )
        self.assertEqual(github.requests, [])
        self.assertEqual(git(self.remote, "branch", "--list", "ai-fix/*"), "")

    def test_a_contract_break_downgrades_a_verified_report(self) -> None:
        commit(self.repo, "fix: only one commit", {"app/a.ts": "export const a = 2\n"})
        fix = git(self.repo, "rev-parse", "HEAD")
        self.write_fix_job_files(fix)
        publish.create_bundle(self.run)
        published = publish.open_pr(
            self.run,
            publish.plan(self.run, self.bases),
            self.report_job_checkout(),
            "tok",
            GitHub("tok", FakeGitHub()),
            push_url=str(self.remote),
        )
        self.assertEqual((published.verification, published.pr_draft), ("unverified", True))
        self.assertIn("needs 2 new commit(s), found 1", published.notes or "")

    def test_plan_rejects_a_base_that_was_not_recorded(self) -> None:
        fix = self.make_fix({"app/a.ts": "export const a = 2\n"})
        self.write_fix_job_files(fix)
        publish.create_bundle(self.run)
        with self.assertRaises(PublishError):
            publish.plan(self.run, {"general_datebase": "a" * 40})

    def test_bundle_through_an_outbox(self) -> None:
        fix = self.make_fix({"app/a.ts": "export const a = 2\n"})
        self.write_fix_job_files(fix)
        outbox = self.tmp / "handoff"
        outbox.mkdir()
        self.assertTrue(publish.create_bundle(self.run, outbox=outbox))
        self.assertEqual(self.run.bundle.read_bytes(), (outbox / "fix.bundle").read_bytes())

    def test_nothing_to_publish(self) -> None:
        self.run.verification_json.write_text(
            json.dumps({"conclusion": "completed", "publish": False})
        )
        self.assertFalse(publish.create_bundle(self.run))
        self.assertEqual(publish.plan(self.run, self.bases), Plan(False))
        self.assertEqual(
            publish.plan(run_context(self.tmp, self.tmp / "empty"), self.bases), Plan(False)
        )


if __name__ == "__main__":
    unittest.main()
