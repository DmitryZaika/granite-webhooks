"""Pre-fetches what the agent needs to start: the issue, one sample event, the
same bug's other events, the server side of a web API error, and breadcrumbs.

Runs in the fix job before isolation (the runner user has the network). It is
best effort: a section that fails is skipped with a one-line note, and when
nothing at all could be fetched no file is written and the step still
succeeds, so the agent falls back to pulling the issue through the PostHog MCP
itself.

PostHog issues are catch-alls (one issue holds many different errors), so the
run is about one sample event (EVENT_ID); the other sections show how that bug
behaves around it. The result is production data, some of it typed by users:
every message and URL is masked here, and `kickoff` in agent.py fences the
whole text as untrusted.
"""

import json
import os
import re
from collections.abc import Callable, Mapping
from datetime import datetime, timedelta
from typing import Any
from urllib.parse import urlsplit

from . import http
from .context import POSTHOG_PROJECT, RunContext

QUERY_URL = f"https://us.posthog.com/api/projects/{POSTHOG_PROJECT}/query/"
REPLAY_URL = f"https://us.posthog.com/project/{POSTHOG_PROJECT}/replay/"
TIMEOUT_SECONDS = 20
MAX_TOTAL = 16_000
# Per section, so the worst case still sums under MAX_TOTAL and a long stack
# cannot push the breadcrumbs out.
MAX_SECTION_ISSUE = 2_500
MAX_SECTION_SAMPLE = 6_000
MAX_SECTION_SAME_BUG = 2_800
MAX_SECTION_SERVER = 2_200
MAX_SECTION_BREADCRUMBS = 2_000
MAX_VALUE = 2_000
MAX_COMPONENT_STACK = 1_500
MAX_PROPERTY = 500
MAX_FRAMES = 25
MAX_MESSAGES = 8
MAX_SAME_BUG = 20
MAX_BREADCRUMBS = 15
SERVER_WINDOW = timedelta(minutes=3)
API_DISTINCT_ID = "granite-api"

# Sample-event properties, in the order they are shown. `exception_list` and
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
_SAFE_ID = re.compile(r"[0-9A-Za-z_-]{8,64}")
_URL_TAIL = re.compile(r"(?<=\S)[?#][^\s\"'<>)\]}]+")
_EMAIL = re.compile(r"[\w.+-]{1,64}@[\w-]{1,63}(?:\.[\w-]{1,63}){1,8}")
_PHONE = re.compile(r"\(?\d{3}\)?[\s.-]\d{3}[\s.-]\d{4}")
# Not inside a word, so release SHAs and uuids keep their digit runs.
_DIGITS = re.compile(r"(?<![0-9A-Za-z])\d{7,}(?![0-9A-Za-z])")
_TIMESTAMP = re.compile(r"(\d{4}-\d\d-\d\d)[T ](\d\d:\d\d:\d\d)(?:\.(\d+))?")
_API_PATH = re.compile(r"[\w\-./{}:~%]+")
_PATH_IN_MESSAGE = re.compile(r"\bon (/[^\s:?]+)")
_GENERIC_BODIES = ("database error", "internal server error", "internal error", "server error")


def quote(value: str) -> str:
    """A HogQL string literal."""
    escaped = value.replace("\\", "\\\\").replace("'", "\\'")
    escaped = escaped.replace("\n", "\\n").replace("\r", "\\r").replace("\t", "\\t")
    return "'" + escaped + "'"


def checked_id(value: str) -> str:
    """An issue, event or session id that is safe to put in a query."""
    if not _SAFE_ID.fullmatch(value):
        raise ValueError("unexpected id shape")
    return value


def timestamp_literal(value: object) -> str:
    """A HogQL UTC datetime literal (millisecond precision) for a returned timestamp."""
    match = _TIMESTAMP.match(str(value))
    if not match:
        raise ValueError("unexpected timestamp shape")
    day, clock, fraction = match.groups()
    millis = (fraction or "0").ljust(3, "0")[:3]
    return f"toDateTime64('{day} {clock}.{millis}', 3, 'UTC')"


def parse_timestamp(value: object) -> datetime:
    match = _TIMESTAMP.match(str(value))
    if not match:
        raise ValueError("unexpected timestamp shape")
    day, clock, fraction = match.groups()
    micros = (fraction or "0").ljust(6, "0")[:6]
    return datetime.fromisoformat(f"{day}T{clock}.{micros}")


def mask(text: str) -> str:
    """Drops what must not reach the agent: query strings, fragments, emails, phones."""
    text = _URL_TAIL.sub("", text)
    text = _EMAIL.sub("<email>", text)
    text = _PHONE.sub("<digits>", text)
    return _DIGITS.sub("<digits>", text)


def url_path(url: object) -> str:
    """The path of a URL, without host, query string or fragment."""
    text = url if isinstance(url, str) else ""
    try:
        return mask(urlsplit(text).path or text.split("?")[0].split("#")[0])
    except ValueError:
        return ""


def issue_query(issue_id: str) -> str:
    return (
        "SELECT id, name, description, status, created_at "
        f"FROM system.error_tracking_issues WHERE id = {quote(checked_id(issue_id))} LIMIT 1"
    )


def messages_query(issue_id: str) -> str:
    """The issue's distinct messages; `kinds` is the count before LIMIT."""
    return (
        "SELECT substringUTF8(JSONExtractString(properties.$exception_values, 1), 1, 200) "
        "AS message, count() AS occurrences, count() OVER () AS kinds "
        "FROM events WHERE event = '$exception' "
        f"AND issue_id = {quote(checked_id(issue_id))} "
        "AND timestamp >= now() - INTERVAL 14 DAY "
        f"GROUP BY message ORDER BY occurrences DESC LIMIT {MAX_MESSAGES}"
    )


# argMax-free columns of the sample query, after the head.
_SAMPLE_HEAD = ("id", "timestamp", "distinct_id", "session_id")
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


def _sample_filter(issue_id: str, event_id: str) -> str:
    base = "event = '$exception' AND timestamp >= now() - INTERVAL 30 DAY"
    if event_id:
        return f"{base} AND uuid = toUUID({quote(checked_id(event_id))})"
    return f"{base} AND issue_id = {quote(checked_id(issue_id))}"


def sample_query(issue_id: str, event_id: str) -> str:
    """One event (the given one, else the latest) and whether its session has a replay."""
    where = _sample_filter(issue_id, event_id)
    columns = ", ".join(f"properties.{name}" for name in ("$session_id", *_EVENT_COLUMNS))
    session = f"SELECT properties.$session_id FROM events WHERE {where} ORDER BY timestamp DESC LIMIT 1"
    replays = (
        "(SELECT count() FROM session_replay_events "
        "WHERE start_time >= now() - INTERVAL 14 DAY "
        f"AND session_id IN ({session}))"
    )
    return (
        f"SELECT toString(uuid), timestamp, distinct_id, {columns}, {replays} "
        f"FROM events WHERE {where} ORDER BY timestamp DESC LIMIT 1"
    )


def _same_bug_filter(bug: Mapping[str, object]) -> str:
    # Not limited to the issue: PostHog splits one bug over several issues.
    clauses = [
        "event = '$exception'",
        "timestamp >= now() - INTERVAL 14 DAY",
    ]
    if bug.get("type"):
        clauses.append(
            f"JSONExtractString(properties.$exception_types, 1) = {quote(str(bug['type']))}"
        )
    if bug.get("api_path") and bug.get("api_status") is not None:
        # Browser events keep the query string in api_path.
        path = str(bug["api_path"])
        clauses.append(
            f"(properties.api_path = {quote(path)} "
            f"OR startsWith(properties.api_path, {quote(path + '?')}))"
        )
        clauses.append(f"properties.api_status = {int(str(bug['api_status']))}")
    else:
        clauses.append(
            "substringUTF8(JSONExtractString(properties.$exception_values, 1), 1, 80) "
            f"= {quote(str(bug.get('message', ''))[:80])}"
        )
    return " AND ".join(clauses)


def same_bug_query(bug: Mapping[str, object]) -> str:
    """Recent events of the same bug in any issue; the window columns cover all of its events."""
    return (
        "SELECT timestamp, properties.release, properties.$current_url, "
        "properties.route_pattern, properties.api_method, properties.api_path, "
        "properties.api_status, count() OVER (), uniq(distinct_id) OVER (), "
        "min(timestamp) OVER (), max(timestamp) OVER (), "
        "count() OVER (PARTITION BY properties.release) "
        f"FROM events WHERE {_same_bug_filter(bug)} "
        f"ORDER BY timestamp DESC LIMIT {MAX_SAME_BUG}"
    )


def merge_windows(stamps: list[datetime]) -> list[tuple[datetime, datetime]]:
    """Overlapping +-3 minute windows around the stamps, merged, oldest first."""
    merged: list[tuple[datetime, datetime]] = []
    for stamp in sorted(stamps):
        low, high = stamp - SERVER_WINDOW, stamp + SERVER_WINDOW
        if merged and low <= merged[-1][1]:
            merged[-1] = (merged[-1][0], max(merged[-1][1], high))
        else:
            merged.append((low, high))
    return merged


def _window_filter(windows: list[tuple[datetime, datetime]]) -> str:
    def literal(moment: datetime) -> str:
        return f"toDateTime64('{moment:%Y-%m-%d %H:%M:%S}.{moment.microsecond // 1000:03d}', 3, 'UTC')"

    parts = [f"(timestamp >= {literal(low)} AND timestamp <= {literal(high)})" for low, high in windows]
    return "(" + " OR ".join(parts) + ")"


def server_query(windows: list[tuple[datetime, datetime]]) -> str:
    """The granite-api's own exceptions in the windows, one row per message and route."""
    full = "JSONExtractString(properties.$exception_values, 1)"
    return (
        f"SELECT substringUTF8({full}, 1, 300) AS message, properties.api_path, "
        "properties.api_route, count(), min(timestamp), max(timestamp), "
        f"argMax(properties.release, timestamp), argMax(substringUTF8({full}, 1, {MAX_VALUE}), timestamp) "
        f"FROM events WHERE event = '$exception' AND distinct_id = {quote(API_DISTINCT_ID)} "
        f"AND {_window_filter(windows)} "
        "GROUP BY message, properties.api_path, properties.api_route "
        "ORDER BY max(timestamp) DESC LIMIT 100"
    )


def web_query(windows: list[tuple[datetime, datetime]]) -> str:
    """The web app's API errors in the windows, one row per event."""
    return (
        "SELECT timestamp, distinct_id, properties.api_path, properties.api_route, "
        "properties.error_source, properties.$current_url "
        f"FROM events WHERE event = '$exception' AND distinct_id != {quote(API_DISTINCT_ID)} "
        "AND properties.api_path IS NOT NULL AND properties.api_path != '' "
        f"AND {_window_filter(windows)} ORDER BY timestamp DESC LIMIT 300"
    )


def breadcrumbs_query(session_id: str, before: object) -> str:
    """The session's last events before `before`; never selects element text."""
    moment = timestamp_literal(before)
    return (
        "SELECT timestamp, event, properties.$current_url, properties.api_path, "
        "properties.api_status, "
        "substringUTF8(JSONExtractString(properties.$exception_types, 1), 1, 80), "
        "substringUTF8(JSONExtractString(properties.$exception_values, 1), 1, 120), "
        "properties.$event_type, extract(elements_chain, '^([a-zA-Z0-9]+)') "
        f"FROM events WHERE properties.$session_id = {quote(checked_id(session_id))} "
        f"AND timestamp >= {moment} - INTERVAL 1 DAY AND timestamp < {moment} "
        f"ORDER BY timestamp DESC LIMIT {MAX_BREADCRUMBS}"
    )


def clean(value: object, limit: int) -> str:
    """Masked printable text of at most `limit` characters; newlines and tabs stay."""
    text = value if isinstance(value, str) else json.dumps(value, default=str)
    # Cut first: masking a huge value would be slow and the rest is dropped anyway.
    text = _CONTROL.sub("", text[: limit + 100])
    text = mask(text)
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
    # The query string goes before the position is appended to it.
    position = source.split("?")[0].split("#")[0] or "?"
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


def _int(value: object) -> int | None:
    """api_status arrives as 500.0."""
    if isinstance(value, bool) or not isinstance(value, (int, float, str)):
        return None
    try:
        return int(float(value))
    except ValueError:
        return None


def _section(title: str, lines: list[str], limit: int) -> str:
    text = "\n".join([f"== {title} ==", *lines])
    return text if len(text) <= limit else text[:limit] + "\n...[truncated]"


def issue_section(issue_row: list[Any] | None, kinds: list[list[Any]]) -> list[str]:
    lines: list[str] = []
    if issue_row and len(issue_row) >= 5:
        _, name, description, status, created = issue_row[:5]
        lines.append(f"title: {clean(name or '', 500)}")
        if _present(description):
            lines.append(f"description: {clean(description, MAX_VALUE)}")
        lines.append(f"status: {clean(status or '', 100)}")
        lines.append(f"issue created: {clean(created or '', 100)}")
    if kinds and len(kinds[0]) >= 3:
        total = _int(kinds[0][2]) or len(kinds)
        lines.append(f"distinct exception messages (last 14 days): {total}")
        if total > 1:
            lines.append(f"top {min(total, len(kinds))} messages:")
            for row in kinds[:MAX_MESSAGES]:
                lines.append(f"  {row[1]}x {clean(row[0], 200)}")
            lines.append(
                "This issue groups several errors; the run is about the sample event below."
            )
    return lines


def parse_sample(row: list[Any]) -> dict[str, Any]:
    """Names the sample row's cells and derives what identifies its bug."""
    names = (*_SAMPLE_HEAD[:3], "$session_id", *_EVENT_COLUMNS, "replays")
    found = dict(zip(names, row, strict=False))
    entries = _as_json(found.get("$exception_list"))
    first = entries[0] if isinstance(entries, list) and entries else None
    first = first if isinstance(first, dict) else {}
    found["type"] = first.get("type") if isinstance(first.get("type"), str) else ""
    found["message"] = first.get("value") if isinstance(first.get("value"), str) else ""
    found["status_code"] = _int(found.get("api_status"))
    return found


def sample_section(found: dict[str, Any]) -> list[str]:
    lines = [f"event id: {clean(found.get('id'), 100)}", f"timestamp: {clean(found.get('timestamp'), 100)}"]
    for name in SIMPLE_PROPERTIES:
        value = found.get(name)
        if name == "api_status":
            value = found["status_code"]
        if _present(value):
            lines.append(f"{name}: {clean(value, MAX_PROPERTY)}")
    session = found.get("$session_id")
    if _present(session) and isinstance(session, str) and _SAFE_ID.fullmatch(session):
        lines.append(f"$session_id: {session}")
        recorded = (_int(found.get("replays")) or 0) > 0
        lines.append(
            f"session replay: {REPLAY_URL}{session} "
            + (
                "(a recording exists)"
                if recorded
                else "(the link only works when recording was on; none was found for this session)"
            )
        )
    if _present(found.get("component_stack")):
        lines.append("component_stack:")
        lines.append(clean(found["component_stack"], MAX_COMPONENT_STACK))
    exceptions = format_exceptions(found.get("$exception_list"))
    if exceptions:
        lines.append("exception (type: message, then the stack):")
        lines += [f"  {line}" for line in exceptions]
    return lines


def bug_key(found: Mapping[str, Any]) -> dict[str, object]:
    """What makes two events the same bug: type plus api path and status, else message."""
    path = found.get("api_path")
    status = found["status_code"]
    usable = isinstance(path, str) and _API_PATH.fullmatch(path.split("?")[0]) is not None
    return {
        "type": found["type"],
        "api_path": path.split("?")[0] if usable and isinstance(path, str) else "",
        "api_status": status if usable else None,
        "message": found["message"],
    }


def same_bug_section(rows: list[list[Any]]) -> tuple[list[str], list[datetime]]:
    if not rows or len(rows[0]) < 12:
        return [], []
    total, users, first, last = rows[0][7:11]
    lines = [
        f"total: {total}, distinct users: {users}",
        f"first seen: {clean(first, 100)}, last seen: {clean(last, 100)}",
    ]
    by_release: dict[str, Any] = {}
    stamps: list[datetime] = []
    for row in rows:
        by_release[str(row[1])[:7] if _present(row[1]) else "unknown"] = row[11]
    lines.append(
        "per release: " + ", ".join(f"{name}={count}" for name, count in by_release.items())
    )
    lines.append(f"{len(rows)} most recent (time, release, page, route_pattern, api):")
    for row in rows:
        stamps.append(parse_timestamp(row[0]))
        api = " ".join(
            str(part)
            for part in (row[4], clean(row[5], 120), _int(row[6]))
            if _present(part)
        )
        release = str(row[1])[:7] if _present(row[1]) else "-"
        pattern = clean(row[3], 120) if _present(row[3]) else "-"
        lines.append(
            f"  {clean(row[0], 40)} {release} {url_path(row[2]) or '-'} {pattern} {api or '-'}"
        )
    return lines, stamps


def segments_match(wanted: str, candidate: object) -> bool:
    """Paths match segment by segment; a {param} segment matches any segment."""
    if not isinstance(candidate, str) or not candidate:
        return False
    left = wanted.split("?")[0].strip("/").split("/")
    right = candidate.split("?")[0].strip("/").split("/")
    if len(left) != len(right):
        return False
    return all(
        a == b or (a.startswith("{") and a.endswith("}")) or (b.startswith("{") and b.endswith("}"))
        for a, b in zip(left, right, strict=True)
    )


def is_web_api_failure(found: Mapping[str, Any]) -> bool:
    """A browser-side API 5xx (or a generic body): the cause is in the server's event."""
    if found.get("distinct_id") == API_DISTINCT_ID or not found.get("api_path"):
        return False
    status = found["status_code"]
    if found["type"] == "GraniteApiError" and status is not None and status >= 500:
        return True
    message = str(found["message"]).lower()
    return any(body in message for body in _GENERIC_BODIES)


def server_section(rows: list[list[Any]], path: str) -> list[str]:
    hits = [
        row
        for row in rows
        if len(row) >= 8 and (segments_match(path, row[1]) or segments_match(path, row[2]))
    ]
    if not hits:
        return [f"no granite-api exception matched {clean(path, 200)} within 3 minutes."]
    counts: dict[str, int] = {}
    for row in hits:
        counts[str(row[0])] = counts.get(str(row[0]), 0) + (_int(row[3]) or 0)
    newest = max(hits, key=lambda row: str(row[5]))
    lines = [
        f"{sum(counts.values())} server exceptions, {len(counts)} distinct messages, "
        f"{clean(min(str(row[4]) for row in hits), 40)} to "
        f"{clean(max(str(row[5]) for row in hits), 40)}:"
    ]
    for message, count in sorted(counts.items(), key=lambda item: -item[1])[:MAX_MESSAGES]:
        lines.append(f"  {count}x {clean(message, 250)}")
    lines.append(f"newest message in full: {clean(newest[7], MAX_VALUE)}")
    lines.append(f"newest release: {clean(newest[6], 100)}, route: {clean(newest[2] or newest[1], 200)}")
    return lines


def web_section(rows: list[list[Any]], path: str) -> list[str]:
    hits = [
        row
        for row in rows
        if len(row) >= 6 and (segments_match(path, row[2]) or segments_match(path, row[3]))
    ]
    if not hits:
        return [f"no web exception matched {clean(path, 200)} in the same windows."]
    pages: dict[str, int] = {}
    for row in hits:
        page = url_path(row[5]) or "-"
        pages[page] = pages.get(page, 0) + 1
    toasts = sum(1 for row in hits if row[4] == "toast")
    lines = [
        f"{len(hits)} web exceptions, {len({row[1] for row in hits})} distinct users, "
        f"{toasts} shown as toasts (capped at 300 rows):"
    ]
    for page, count in sorted(pages.items(), key=lambda item: -item[1])[:MAX_MESSAGES]:
        lines.append(f"  {count}x {page}")
    return lines


def breadcrumb_line(row: list[Any]) -> str:
    when, event, url, api_path, api_status, kind, message, event_type, tag = (
        [*row, *[None] * 9][:9]
    )
    parts = [clean(when, 40), clean(event, 60), url_path(url) or "-"]
    if event == "api_network_error":
        parts.append(f"{clean(api_path, 120)} {_int(api_status) or ''}".strip())
    elif event == "$exception":
        parts.append(f"{clean(kind, 80)}: {clean(message, 120)}")
    elif event == "$autocapture":
        parts.append(f"{clean(event_type or '', 40)} {clean(tag or '', 40)}".strip())
    return " | ".join(parts)


def breadcrumbs_section(rows: list[list[Any]]) -> list[str]:
    ordered = list(reversed(rows))
    return [
        f"the session's last {len(ordered)} events before the sample, oldest first:",
        *[f"  {breadcrumb_line(row)}" for row in ordered],
    ]


def fetch_summary(
    key: str,
    issue_id: str,
    transport: http.Transport = http.urllib_transport,
    event_id: str = "",
) -> str:
    """The sections that could be built; "" when none could."""

    def ask(query: str) -> list[list[Any]]:
        return run_query(key, query, transport)

    sections: list[str] = []
    notes: list[str] = []
    shared: dict[str, Any] = {}

    def attempt(title: str, limit: int, build: Callable[[], list[str]]) -> None:
        try:
            lines = build()
        except (http.HttpError, ValueError, TypeError, KeyError, IndexError) as error:
            notes.append(f"{title}: unavailable ({error.__class__.__name__})")
            return
        if lines:
            sections.append(_section(title, lines, limit))

    def issue() -> list[str]:
        issues = ask(issue_query(issue_id))
        try:
            kinds = ask(messages_query(issue_id))
        except (http.HttpError, ValueError, TypeError, KeyError, IndexError):
            kinds = []
            notes.append("distinct messages: unavailable")
        return issue_section(issues[0] if issues else None, kinds)

    def sample() -> list[str]:
        rows = ask(sample_query(issue_id, event_id))
        if not rows:
            notes.append("sample event: not found")
            return []
        shared["sample"] = parse_sample(rows[0])
        return sample_section(shared["sample"])

    def same_bug() -> list[str]:
        found = shared.get("sample")
        if not found:
            return []
        key = bug_key(found)
        if not key["type"] and not key["message"]:
            return []
        lines, stamps = same_bug_section(ask(same_bug_query(key)))
        shared["stamps"] = stamps or [parse_timestamp(found["timestamp"])]
        return lines

    def other_side() -> list[str]:
        found = shared.get("sample")
        stamps = shared.get("stamps")
        if not found or not stamps:
            return []
        windows = merge_windows(stamps[:MAX_SAME_BUG])
        path = str(found.get("api_path") or "").split("?")[0]
        if not path:
            match = _PATH_IN_MESSAGE.search(found["message"])
            path = match.group(1) if match else ""
        if not path:
            return []
        if found.get("distinct_id") == API_DISTINCT_ID:
            return web_section(ask(web_query(windows)), path)
        if is_web_api_failure(found):
            return server_section(ask(server_query(windows)), path)
        return []

    def breadcrumbs() -> list[str]:
        found = shared.get("sample")
        session = found.get("$session_id") if found else None
        if not isinstance(session, str) or not session:
            return []
        rows = ask(breadcrumbs_query(session, found["timestamp"]))
        return breadcrumbs_section(rows) if rows else []

    attempt("issue", MAX_SECTION_ISSUE, issue)
    attempt("sample event", MAX_SECTION_SAMPLE, sample)
    attempt("same bug", MAX_SECTION_SAME_BUG, same_bug)
    attempt("other side of the API call", MAX_SECTION_SERVER, other_side)
    attempt("breadcrumbs", MAX_SECTION_BREADCRUMBS, breadcrumbs)
    if not sections:
        return ""
    text = "\n".join([*sections, *notes])
    if len(text) > MAX_TOTAL:
        text = text[:MAX_TOTAL] + "\n...[truncated]"
    return text


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
        summary = fetch_summary(key, run.issue_id, transport, run.event_id)
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
