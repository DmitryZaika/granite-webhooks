"""JSON over HTTPS with urllib: the one place requests are sent."""

import json
import time
import urllib.error
import urllib.request
from collections.abc import Callable
from dataclasses import dataclass
from typing import Any

TIMEOUT_SECONDS = 30


@dataclass(frozen=True)
class Response:
    status: int
    body: Any


class HttpError(RuntimeError):
    def __init__(self, status: int | None, message: str) -> None:
        super().__init__(message)
        self.status = status


# (method, url, headers, body bytes) -> Response. Raises HttpError(None, …)
# when the request never got an answer.
Transport = Callable[[str, str, dict[str, str], bytes | None], Response]


def urllib_transport(
    method: str, url: str, headers: dict[str, str], body: bytes | None
) -> Response:
    if not url.startswith("https://"):
        raise HttpError(None, "refusing a non-HTTPS URL")
    request = urllib.request.Request(url, data=body, headers=headers, method=method)
    try:
        with urllib.request.urlopen(request, timeout=TIMEOUT_SECONDS) as response:
            return Response(response.status, _decode(response.read()))
    except urllib.error.HTTPError as error:
        return Response(error.code, _decode(error.read()))
    except (urllib.error.URLError, TimeoutError, ConnectionError) as error:
        raise HttpError(None, f"{method} request failed: {error.__class__.__name__}") from None


def _decode(raw: bytes) -> Any:
    if not raw:
        return None
    try:
        return json.loads(raw)
    except ValueError:
        return raw.decode(errors="replace")[:500]


def send_json(
    transport: Transport,
    method: str,
    url: str,
    headers: dict[str, str],
    payload: Any = None,
) -> Response:
    body = None if payload is None else json.dumps(payload).encode()
    all_headers = {"Accept": "application/json", **headers}
    if body is not None:
        all_headers["Content-Type"] = "application/json"
    return transport(method, url, all_headers, body)


def with_retries(
    send: Callable[[], Response],
    attempts: int = 3,
    sleep: Callable[[float], None] = time.sleep,
) -> Response:
    """Retries network errors and 5xx answers with backoff (2s, 4s, …)."""
    last_error: HttpError | None = None
    for attempt in range(attempts):
        if attempt:
            sleep(2.0**attempt)
        try:
            response = send()
        except HttpError as error:
            last_error = error
            continue
        if response.status < 500:
            return response
        last_error = HttpError(response.status, f"server answered {response.status}")
    raise last_error or HttpError(None, "no request was sent")
