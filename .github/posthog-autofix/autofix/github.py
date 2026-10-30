"""The few GitHub REST calls the workflow makes."""

from dataclasses import dataclass
from urllib.parse import quote

from .context import OWNER
from .http import HttpError, Transport, send_json, urllib_transport, with_retries

API = "https://api.github.com"


@dataclass(frozen=True)
class PullRequest:
    url: str
    number: int
    draft: bool


class GitHub:
    def __init__(self, token: str, transport: Transport = urllib_transport) -> None:
        self._headers = {
            "Authorization": f"Bearer {token}",
            "X-GitHub-Api-Version": "2022-11-28",
            "User-Agent": "granite-posthog-autofix",
        }
        self._transport = transport

    def _call(self, method: str, path: str, payload: object = None) -> object:
        url = API + path
        response = with_retries(
            lambda: send_json(self._transport, method, url, self._headers, payload)
        )
        if response.status >= 300:
            raise HttpError(
                response.status,
                f"GitHub {method} {path} answered {response.status}: {response.body}",
            )
        return response.body

    def bot_user_id(self, app_slug: str) -> int:
        body = self._call("GET", f"/users/{quote(app_slug + '[bot]')}")
        if not isinstance(body, dict) or not isinstance(body.get("id"), int):
            raise HttpError(None, "GitHub returned no user id")
        return body["id"]

    def open_pull_request(
        self, repo: str, branch: str, title: str, body: str, draft: bool
    ) -> PullRequest:
        """Opens the pull request, or returns the open one for `branch`
        (a re-run of the publish job pushes the same branch again)."""
        existing = self._find_open(repo, branch)
        if existing is not None:
            return existing
        payload = {"title": title, "head": branch, "base": "main", "body": body, "draft": draft}
        created = self._call("POST", f"/repos/{OWNER}/{repo}/pulls", payload)
        return _pull_request(created)

    def _find_open(self, repo: str, branch: str) -> PullRequest | None:
        head = quote(f"{OWNER}:{branch}", safe="")
        found = self._call("GET", f"/repos/{OWNER}/{repo}/pulls?state=open&head={head}")
        if isinstance(found, list) and found:
            return _pull_request(found[0])
        return None

    def open_labelled(self, repo: str, label: str, limit: int) -> list[dict[str, object]]:
        """The newest open pull requests carrying `label`, at most `limit`.

        Read from the pulls API, which the token's pull request permission
        covers (the issues API, which filters by label, needs another one).
        """
        found = self._call("GET", f"/repos/{OWNER}/{repo}/pulls?state=open&per_page=100")
        if not isinstance(found, list):
            raise HttpError(None, "GitHub returned no list of pull requests")
        labelled = [item for item in found if isinstance(item, dict) and _has_label(item, label)]
        return labelled[:limit]

    def changed_files(self, repo: str, number: int, limit: int) -> list[str]:
        found = self._call("GET", f"/repos/{OWNER}/{repo}/pulls/{number}/files?per_page={limit}")
        if not isinstance(found, list):
            raise HttpError(None, "GitHub returned no list of files")
        names = (item.get("filename") for item in found if isinstance(item, dict))
        return [name for name in names if isinstance(name, str)][:limit]

    def add_labels(self, repo: str, number: int, labels: list[str]) -> None:
        # Labels that do not exist yet are created by this call.
        self._call("POST", f"/repos/{OWNER}/{repo}/issues/{number}/labels", {"labels": labels})


def _pull_request(body: object) -> PullRequest:
    if not isinstance(body, dict) or not isinstance(body.get("html_url"), str):
        raise HttpError(None, "GitHub returned no pull request")
    return PullRequest(body["html_url"], int(body["number"]), bool(body.get("draft")))


def _has_label(item: dict[str, object], label: str) -> bool:
    labels = item.get("labels")
    return isinstance(labels, list) and any(
        isinstance(entry, dict) and entry.get("name") == label for entry in labels
    )
