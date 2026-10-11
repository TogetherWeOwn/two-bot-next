#!/usr/bin/env python3
"""Read-only readback of recent container stdout (stdlib only; no start, no mutation).

The Rust process's stdout/stderr reaches Cloudflare Workers Observability as the
`containers` dataset (docs/startup-diagnostics.md, "Reading container stdout").
This script runs the documented telemetry query for ONE Container application id
and prints the newest lines as sanitized JSON, so the log literals the playbooks
cite (`durable gateway failed`, `gateway reconnect failed; Twilight will retry`,
`gateway ready; checkpoint committed`, ...) can be checked against a live line.

It never touches the Worker or the container: the query goes straight to the
Cloudflare API, so it cannot wake a sleeping container or reach a bot route. Only
two calls are possible, and the transport refuses anything else before it sends:

  * GET  /accounts                (only when no account id is supplied)
  * POST /accounts/<id>/workers/observability/telemetry/query   (a read query)

The credential is $CLOUDFLARE_API_TOKEN (a read-only Workers Observability token);
it is never an argument, never logged, and never reaches stdout or stderr. The
request does not follow redirects, so the token cannot be replayed to another host.

Output (stdout), newest line last:

  {"schema_version": 1,
   "service": "<application id>",
   "window": {"start": "...Z", "end": "...Z"},
   "limit": 100,
   "event_count": <lines kept>,
   "possibly_truncated": <true when the query returned exactly `limit` lines>,
   "redactions": <values replaced>,
   "events": [{"ts": "...Z", "level": "info", "message": "...", "msg": "...", ...}]}

Only a short list of structured fields is kept (FIELD_KEYS); every other field is
dropped. Every kept value is redacted for secret-like content (tokens, URLs,
bearer headers, key=value secrets, long opaque strings, Discord snowflakes) before it
is printed. Zero events means "not observed in this window", not "did not happen".

Telemetry returns the newest 100 events per query: keep windows to a few minutes
around the time you care about (`--end` takes a UTC instant for a past window).

Exit status: 0 read, 1 read failed (one fixed-vocabulary line on stderr), 2 refused
by a fence (nothing was sent).

Usage:
  CLOUDFLARE_API_TOKEN=... python3 scripts/container_stdout_read.py \\
      --service <container-application-id> [--minutes 5] [--end 2026-10-11T12:00:00Z] \\
      [--contains "durable gateway failed"] [--limit 100] [--account-id <32 hex>]
"""

import argparse
from datetime import datetime, timedelta, timezone
import json
import os
import re
import sys
import urllib.error
import urllib.request
import uuid

SCHEMA_VERSION = 1
API_BASE = "https://api.cloudflare.com/client/v4"
TOKEN_ENV = "CLOUDFLARE_API_TOKEN"
ACCOUNT_ENV = "CLOUDFLARE_ACCOUNT_ID"
# api.cloudflare.com rejects urllib's default agent on some edges; name ourselves.
USER_AGENT = "two-bot-next-container-stdout-read/1.0"
DATASET = "containers"
DEFAULT_MINUTES = 5
MAX_MINUTES = 30
DEFAULT_LIMIT = 100
MAX_LIMIT = 100  # the telemetry endpoint returns the newest 100 events per query
MAX_CONTAINS = 120
MAX_RESPONSE_BYTES = 4 * 1024 * 1024
HTTP_TIMEOUT_SECONDS = 30
MAX_VALUE_CHARS = 400

UUID_RE = re.compile(r"[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}")
ACCOUNT_RE = re.compile(r"[0-9a-f]{32}")
TOKEN_RE = re.compile(r"[\x21-\x7e]{20,200}")
INSTANT_RE = re.compile(r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z")
CONTAINS_RE = re.compile(r"[\x20-\x7e]{1,%d}" % MAX_CONTAINS)
RUN_ID_RE = re.compile(r"[0-9]{10,20}-[0-9]{1,6}")
# The two calls the transport will send; anything else is refused before the socket.
ACCOUNTS_PATH_RE = re.compile(r"/accounts")
QUERY_PATH_RE = re.compile(r"/accounts/[0-9a-f]{32}/workers/observability/telemetry/query")

# Structured fields kept from an event's `source` (docs/logging.md). Any other key
# could carry free text, so it is dropped rather than scrubbed.
FIELD_KEYS = (
    "msg", "target", "run_id", "error_class", "startup_phase", "phase", "class",
    "step", "cause", "reason", "signal", "missing", "job", "code", "count", "revision",
)

# Ordered: URLs go first so a token inside one is not split by a later rule.
_URL = re.compile(r"[A-Za-z][A-Za-z0-9+.-]*://\S+")
_BEARER = re.compile(r"\b(?:Bearer|Bot|Basic)\s+\S+", re.IGNORECASE)
_JWT_LIKE = re.compile(r"\b[A-Za-z0-9_-]{20,}\.[A-Za-z0-9_-]{6,}\.[A-Za-z0-9_-]{20,}\b")
_KEYED = re.compile(
    r"""(?ix)
    \b([\w.-]*(?:token|secret|passw(?:or)?d|authorization|api[_-]?key|credential|dsn|cookie)[\w.-]*)
    (\s*[=:]\s*)
    ("[^"]*"|'[^']*'|\S+)
    """
)
_OPAQUE = re.compile(r"(?<![A-Za-z0-9+/_-])[A-Za-z0-9+/_-]{32,}={0,2}(?![A-Za-z0-9+/_-])")
_SNOWFLAKE = re.compile(r"(?<![0-9])[0-9]{15,22}(?![0-9])")
_ANSI = re.compile(r"\x1b\[[0-9;?]*[ -/]*[@-~]")  # lines before the JSON format are colourised
_CONTROL = re.compile(r"[\x00-\x1f\x7f-\x9f]")
REDACTED = "[redacted]"


class Refused(Exception):
    """A fence refused the run before anything was sent."""


class ReadFailed(Exception):
    """The read failed; the message is fixed vocabulary, never a response body."""


def redact(text):
    """Return (text with secret-like values replaced, number of replacements)."""
    count = 0

    def sub(pattern, replacement, value):
        nonlocal count
        value, n = pattern.subn(replacement, value)
        count += n
        return value

    def opaque(match):
        # Only runs mixing letters and digits count; plain snake_case words such as
        # gateway_runtime_failed_after_checkpoint stay readable.
        nonlocal count
        token = match.group(0)
        if any(ch.isdigit() for ch in token) and any(ch.isalpha() for ch in token):
            count += 1
            return REDACTED
        return token

    text = sub(_URL, REDACTED, text)
    text = sub(_BEARER, REDACTED, text)
    text = sub(_JWT_LIKE, REDACTED, text)
    text = sub(_KEYED, lambda m: m.group(1) + m.group(2) + REDACTED, text)
    text = _OPAQUE.sub(opaque, text)
    text = sub(_SNOWFLAKE, "[id]", text)
    return text, count


def clean_value(key, value):
    """Redact one structured value; returns (value, replacements) or None to drop."""
    if isinstance(value, bool) or value is None:
        return (value, 0) if isinstance(value, bool) else None
    if isinstance(value, int):
        return value, 0
    if not isinstance(value, str):
        return None
    if key == "run_id":
        # Boot nanoseconds + PID: a correlation id, not a Discord snowflake.
        if RUN_ID_RE.fullmatch(value):
            return value, 0
        return REDACTED, 1
    # A log line must not be able to drive the reader's terminal or transcript.
    value = _CONTROL.sub(" ", _ANSI.sub("", value))
    text, replaced = redact(value)
    if len(text) > MAX_VALUE_CHARS:
        text = text[:MAX_VALUE_CHARS] + "..."
    return text, replaced


def format_instant(moment):
    return moment.strftime("%Y-%m-%dT%H:%M:%S.") + f"{moment.microsecond // 1000:03d}Z"


def parse_instant(value):
    if not isinstance(value, str) or not INSTANT_RE.fullmatch(value):
        raise Refused("refusing: --end must be a UTC instant like 2026-10-11T12:00:00Z")
    try:
        return datetime.strptime(value, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=timezone.utc)
    except ValueError:
        raise Refused("refusing: --end is not a real UTC instant") from None


def build_query(service, start, end, limit, contains, query_id):
    filters = [{"key": "$metadata.service", "type": "string", "operation": "eq", "value": service}]
    if contains:
        filters.append({"key": "$metadata.message", "type": "string",
                        "operation": "includes", "value": contains})
    return {
        "queryId": query_id,
        "timeframe": {"from": int(start.timestamp() * 1000), "to": int(end.timestamp() * 1000)},
        "view": "events",
        "limit": limit,
        "parameters": {
            "datasets": [DATASET],
            "filters": filters,
            "calculations": [],
            "groupBys": [],
            "orderBy": {"value": "timestamp", "order": "desc"},
        },
    }


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None  # urllib then raises the 3xx as an HTTPError; the token goes nowhere else


class HttpTransport:
    """The only network path. `base` is a constructor argument for loopback tests;
    the command line cannot change it."""

    def __init__(self, token, base=API_BASE, timeout=HTTP_TIMEOUT_SECONDS):
        self._token = token
        self._base = base
        self._timeout = timeout
        self._opener = urllib.request.build_opener(_NoRedirect)

    def __call__(self, method, path, body=None):
        allowed = ((method == "GET" and body is None and ACCOUNTS_PATH_RE.fullmatch(path))
                   or (method == "POST" and QUERY_PATH_RE.fullmatch(path)))
        if not allowed:
            raise Refused("refusing: only the accounts list and the telemetry query are allowed")
        data = None if body is None else json.dumps(body, separators=(",", ":")).encode()
        headers = {"Authorization": f"Bearer {self._token}", "User-Agent": USER_AGENT,
                   "Accept": "application/json"}
        if data is not None:
            headers["Content-Type"] = "application/json"
        request = urllib.request.Request(self._base + path, data=data, method=method, headers=headers)
        try:
            with self._opener.open(request, timeout=self._timeout) as response:
                status, raw = response.status, response.read(MAX_RESPONSE_BYTES + 1)
        except urllib.error.HTTPError as error:
            status, raw = error.code, b""
            error.close()
        except (urllib.error.URLError, OSError):
            raise ReadFailed("network_error") from None
        if len(raw) > MAX_RESPONSE_BYTES:
            raise ReadFailed("bad_response")
        if status != 200:
            raise ReadFailed(status_class(status))
        try:
            return json.loads(raw)
        except (ValueError, RecursionError):
            raise ReadFailed("bad_response") from None


def status_class(status):
    if status in (401, 403):
        return "unauthorized"
    if status == 429:
        return "rate_limited"
    if 400 <= status < 500:
        return "bad_request"
    return "server_error"


def resolve_account(transport, supplied):
    if supplied:
        return supplied
    answer = transport("GET", "/accounts")
    accounts = answer.get("result") if isinstance(answer, dict) else None
    if (not isinstance(accounts, list) or len(accounts) != 1 or not isinstance(accounts[0], dict)
            or not isinstance(accounts[0].get("id"), str) or not ACCOUNT_RE.fullmatch(accounts[0]["id"])):
        # Several visible accounts would make the pick a guess; the caller names one.
        raise ReadFailed("account_not_unique")
    return accounts[0]["id"]


def sanitize_event(raw):
    """One telemetry event -> (kept event, replacements); fail closed on a bad shape."""
    if not isinstance(raw, dict) or not isinstance(raw.get("timestamp"), int) or isinstance(raw["timestamp"], bool):
        raise ReadFailed("bad_response")
    source = raw.get("source") if isinstance(raw.get("source"), dict) else {}
    metadata = raw.get("$metadata") if isinstance(raw.get("$metadata"), dict) else {}
    try:
        stamp = datetime.fromtimestamp(raw["timestamp"] / 1000, tz=timezone.utc)
    except (OverflowError, OSError, ValueError):
        raise ReadFailed("bad_response") from None
    replaced = 0
    event = {"ts": format_instant(stamp)}
    level = metadata.get("level") if isinstance(metadata.get("level"), str) else source.get("level")
    for key, value in (("level", level),
                       ("message", metadata.get("message", source.get("message")))):
        cleaned = clean_value(key, value)
        if cleaned is not None:
            event[key] = cleaned[0]
            replaced += cleaned[1]
    for key in FIELD_KEYS:
        if key in source:
            cleaned = clean_value(key, source[key])
            if cleaned is not None:
                event[key] = cleaned[0]
                replaced += cleaned[1]
    return event, replaced


def read(transport, service, minutes, end, limit, contains, account_id=None, query_id=None):
    start = end - timedelta(minutes=minutes)
    account = resolve_account(transport, account_id)
    body = build_query(service, start, end, limit, contains, query_id or str(uuid.uuid4()))
    answer = transport("POST", f"/accounts/{account}/workers/observability/telemetry/query", body)
    result = answer.get("result") if isinstance(answer, dict) else None
    events = result.get("events", {}).get("events") if isinstance(result, dict) and isinstance(result.get("events"), dict) else None
    if answer.get("success") is not True or not isinstance(events, list):
        raise ReadFailed("bad_response")
    kept, redactions = [], 0
    for raw in events:
        event, replaced = sanitize_event(raw)
        kept.append(event)
        redactions += replaced
    kept.sort(key=lambda event: event["ts"])  # the query is newest-first; print oldest first
    return {
        "schema_version": SCHEMA_VERSION,
        "service": service,
        "window": {"start": format_instant(start), "end": format_instant(end)},
        "limit": limit,
        "event_count": len(kept),
        "possibly_truncated": len(events) >= limit,
        "redactions": redactions,
        "events": kept,
    }


def parse_args(argv):
    parser = argparse.ArgumentParser(description="Read recent container stdout (read-only).")
    parser.add_argument("--service", required=True, help="Container application id (UUID)")
    parser.add_argument("--minutes", type=int, default=DEFAULT_MINUTES,
                        help=f"window length, 1-{MAX_MINUTES} (default {DEFAULT_MINUTES})")
    parser.add_argument("--end", help="window end, UTC like 2026-10-11T12:00:00Z (default now)")
    parser.add_argument("--limit", type=int, default=DEFAULT_LIMIT, help=f"1-{MAX_LIMIT}")
    parser.add_argument("--contains", help="only lines whose message contains this text")
    parser.add_argument("--account-id", help=f"32-hex account id (default ${ACCOUNT_ENV}, else the only visible account)")
    return parser.parse_args(argv)


def main(argv=None, env=None, transport=None, now=None, stdout=None, stderr=None):
    env = os.environ if env is None else env
    stdout = sys.stdout if stdout is None else stdout
    stderr = sys.stderr if stderr is None else stderr
    try:
        args = parse_args(argv)
    except SystemExit:
        return 2
    try:
        if not UUID_RE.fullmatch(args.service):
            raise Refused("refusing: --service must be a Container application id (lowercase UUID)")
        if not 1 <= args.minutes <= MAX_MINUTES:
            raise Refused(f"refusing: --minutes must be 1-{MAX_MINUTES}")
        if not 1 <= args.limit <= MAX_LIMIT:
            raise Refused(f"refusing: --limit must be 1-{MAX_LIMIT}")
        if args.contains is not None and not CONTAINS_RE.fullmatch(args.contains):
            raise Refused(f"refusing: --contains must be 1-{MAX_CONTAINS} printable ASCII characters")
        account = args.account_id or env.get(ACCOUNT_ENV) or None
        if account is not None and not ACCOUNT_RE.fullmatch(account):
            raise Refused("refusing: the account id must be 32 lowercase hex characters")
        current = (now or (lambda: datetime.now(timezone.utc)))().replace(microsecond=0)
        end = parse_instant(args.end) if args.end else current
        if end > current:
            raise Refused("refusing: --end is in the future")
        if transport is None:
            token = env.get(TOKEN_ENV, "")
            if not TOKEN_RE.fullmatch(token):
                raise Refused(f"refusing: ${TOKEN_ENV} is not set to a read-only API token")
            transport = HttpTransport(token)
        packet = read(transport, args.service, args.minutes, end, args.limit, args.contains, account)
    except Refused as refusal:
        print(f"container_stdout_read: {refusal}", file=stderr)
        return 2
    except ReadFailed as failure:
        # One fixed-vocabulary line: never the response body, the URL or the token.
        print(f"container_stdout_read: read failed ({failure})", file=stderr)
        return 1
    print(json.dumps(packet, indent=2), file=stdout)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
