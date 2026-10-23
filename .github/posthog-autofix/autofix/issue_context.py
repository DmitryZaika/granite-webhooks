"""Pre-fetches the issue's summary and latest event for the agent's first message.

Runs in the fix job before isolation (the runner user has the network). It is
best effort: on any failure nothing is written and the step still succeeds, so
the agent falls back to pulling the issue through the PostHog MCP itself.

The result is production data, some of it typed by users: `kickoff` in
agent.py fences it as untrusted.
"""

import json
import os
import re
from collections.abc import Mapping
from typing import Any

from . import http
from .context import POSTHOG_PROJECT, RunContext

QUERY_URL = f"https://us.posthog.com/api/projects/{POSTHOG_PROJECT}/query/"
TIMEOUT_SECONDS = 20
MAX_TOTAL = 12_000
MAX_VALUE = 2_000
MAX_COMPONENT_STACK = 1_500
MAX_PROPERTY = 500
MAX_FRAMES = 25

# Latest-event properties, in the order they are shown. `exception_list` and
# `component_stack` get their own formatting.
SIMPLE_PROPERTIES = (
    "distinct_id",
    "$lib",
    "$current_url",
    "release",
    "error_source",
    "error_description",
    "api_method",
    "api_path",
    "api_route",
    "api_status",
    "route_pattern",
    "query_key",
    "job",
    "lambda_function",
    "path",
    "route",
    "status",
)

_CONTROL = re.compile(r"[\x00-\x08\x0b-\x1f\x7f-\x9f]")


def quote(value: str) -> str:
    """A HogQL string literal."""
    return "'" + value.replace("\\", "\\\\").replace("'", "\\'") + "'"


def issue_query(issue_id: str) -> str:
    return (
        "SELECT id, name, description, status, created_at "
        f"FROM system.error_tracking_issues WHERE id = {quote(issue_id)} LIMIT 1"
    )


# argMax columns of the events query, after the five aggregates.
_EVENT_PROPERTIES = (
    "release",
    "error_source",
    "error_description",
    "api_method",
    "api_path",
    "api_route",
    "api_status",
    "route_pattern",
    "query_key",
    "component_stack",
    "job",
    "lambda_function",
    "path",
    "route",
    "status",
)
_EVENT_COLUMNS = ("$lib", "$exception_list", "$current_url", *_EVENT_PROPERTIES)


def events_query(issue_id: str) -> str:
    latest = ", ".join(
        f"argMax(properties.{name}, timestamp)"
        for name in _EVENT_COLUMNS
    )
    return (
        "SELECT count(), uniq(distinct_id), min(timestamp), max(timestamp), "
        f"argMax(distinct_id, timestamp), {latest} "
        "FROM events WHERE event = '$exception' "
        f"AND issue_id = {quote(issue_id)} "
        "AND timestamp >= now() - INTERVAL 30 DAY"
    )




def clean(value: object, limit: int) -> str:
    """Printable text of at most `limit` characters; newlines and tabs stay."""
    text = value if isinstance(value, str) else json.dumps(value, default=str)
    text = _CONTROL.sub("", text)
    if len(text) > limit:
        text = text[:limit] + "...[truncated]"
    return text


def _present(value: object) -> bool:
    return value not in (None, "", [], {}, "null")


def _rows(response: http.Response) -> list[list[Any]]:
    body = response.body
    if response.status != 200 or not isinstance(body, dict):
        raise http.HttpError(response.status, f"query answered {response.status}")
    rows = body.get("results")
    if not isinstance(rows, list):
        raise http.HttpError(response.status, "query answered without results")
    return [row for row in rows if isinstance(row, list)]


def run_query(
    key: str, query: str, transport: http.Transport = http.urllib_transport
) -> list[list[Any]]:
    headers = {"Authorization": f"Bearer {key}"}
    payload = {"query": {"kind": "HogQLQuery", "query": query}}
    return _rows(http.send_json(transport, "POST", QUERY_URL, headers, payload))


def _as_json(value: object) -> object:
    if isinstance(value, str):
        try:
            return json.loads(value)
        except ValueError:
            return value
    return value


def _text(frame: Mapping[str, Any], *keys: str) -> str:
    for key in keys:
        value = frame.get(key)
        if isinstance(value, str) and value:
            return value
    return ""


def format_frame(frame: object) -> str:
    if not isinstance(frame, dict):
        return clean(frame, 300)
    raw = frame.get("raw_frame")
    if not isinstance(raw, dict):
        junk = frame.get("junk_drawer")
        raw = junk.get("raw_frame") if isinstance(junk, dict) else None
    raw = raw if isinstance(raw, dict) else {}
    function = _text(frame, "resolved_name", "function", "name", "mangled_name") or _text(
        raw, "function", "mangled_name"
    )
    source = _text(frame, "source", "filename") or _text(raw, "source", "filename")
    line = frame.get("line", frame.get("lineno", raw.get("line", raw.get("lineno"))))
    column = frame.get("column", frame.get("colno", raw.get("column", raw.get("colno"))))
    position = source or "?"
    for part in (line, column):
        if isinstance(part, int) and not isinstance(part, bool):
            position += f":{part}"
    app = " (in_app)" if frame.get("in_app") is True else ""
    return clean(f"{function or '?'} @ {position}{app}", 400)


def format_exceptions(exception_list: object) -> list[str]:
    parsed = _as_json(exception_list)
    if not isinstance(parsed, list):
        return [clean(parsed, MAX_VALUE)] if _present(parsed) else []
    lines: list[str] = []
    for entry in parsed:
        if not isinstance(entry, dict):
            continue
        kind = clean(entry.get("type") or "Error", 200)
        value = clean(entry.get("value") or "", MAX_VALUE)
        lines.append(f"{kind}: {value}")
        stacktrace = entry.get("stacktrace")
        frames = stacktrace.get("frames") if isinstance(stacktrace, dict) else None
        if not isinstance(frames, list) or not frames:
            continue
        # Frames run outermost first: show the top of the stack first.
        top = list(reversed(frames))[:MAX_FRAMES]
        lines.append(f"  stack, most recent call first ({len(frames)} frames):")
        lines += [f"    {format_frame(frame)}" for frame in top]
        if len(frames) > MAX_FRAMES:
            lines.append(f"    ... {len(frames) - MAX_FRAMES} outer frames omitted")
    return lines


def build_summary(issue_row: list[Any] | None, stats_row: list[Any] | None) -> str:
    lines: list[str] = []
    if issue_row and len(issue_row) >= 5:
        _, name, description, status, created = issue_row[:5]
        lines.append(f"title: {clean(name or '', 500)}")
        if _present(description):
            lines.append(f"description: {clean(description, MAX_VALUE)}")
        lines.append(f"status: {clean(status or '', 100)}")
        lines.append(f"issue created: {clean(created or '', 100)}")
    if stats_row and len(stats_row) >= 5 + len(_EVENT_COLUMNS):
        count, users, first, last, distinct = stats_row[:5]
        found = dict(zip(_EVENT_COLUMNS, stats_row[5:], strict=False))
        lines.append(f"occurrences (last 30 days): {count}")
        lines.append(f"distinct users: {users}")
        lines.append(f"first seen: {clean(first, 100)}")
        lines.append(f"last seen: {clean(last, 100)}")
        found["distinct_id"] = distinct
        lines.append("latest event:")
        for name in SIMPLE_PROPERTIES:
            if _present(found.get(name)):
                lines.append(f"  {name}: {clean(found[name], MAX_PROPERTY)}")
        if _present(found.get("component_stack")):
            lines.append("  component_stack:")
            lines.append(clean(found["component_stack"], MAX_COMPONENT_STACK))
        exceptions = format_exceptions(found.get("$exception_list"))
        if exceptions:
            lines.append("latest exception:")
            lines += [f"  {line}" for line in exceptions]
    text = "\n".join(lines)
    if len(text) > MAX_TOTAL:
        text = text[:MAX_TOTAL] + "\n...[truncated]"
    return text


def fetch_summary(
    key: str, issue_id: str, transport: http.Transport = http.urllib_transport
) -> str:
    issues = run_query(key, issue_query(issue_id), transport)
    stats = run_query(key, events_query(issue_id), transport)
    return build_summary(issues[0] if issues else None, stats[0] if stats else None)


def _post_transport(
    method: str, url: str, headers: dict[str, str], body: bytes | None
) -> http.Response:
    return http.urllib_transport(method, url, headers, body, timeout=TIMEOUT_SECONDS)


def write_context(
    run: RunContext,
    env: Mapping[str, str] | None = None,
    transport: http.Transport = _post_transport,
) -> bool:
    """Writes the summary file; returns False (writing nothing) on any failure."""
    env = os.environ if env is None else env
    key = env.get("POSTHOG_AUTOFIX_MCP_KEY", "")
    run.issue_context.unlink(missing_ok=True)
    if not key:
        print("No PostHog key: the agent will fetch the issue itself.")
        return False
    try:
        summary = fetch_summary(key, run.issue_id, transport)
        if not summary:
            print("PostHog returned nothing for the issue.")
            return False
        run.out_dir.mkdir(parents=True, exist_ok=True)
        run.issue_context.write_text(json.dumps({"summary": summary}))
    except (http.HttpError, ValueError, TypeError, KeyError, OSError) as error:
        # Never the key, and no response text.
        print(f"Issue context unavailable ({error.__class__.__name__}); continuing without.")
        return False
    print(f"Wrote the issue context ({len(summary)} characters).")
    return True
