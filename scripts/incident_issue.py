#!/usr/bin/env python3
"""File, dedupe and close `incident` GitHub issues for unattended checks.

The one alert sink for the synthetic user-flow, the daily staging rollback
proof and the daily digest (stdlib only). Alerts go to the agents that can
fix the outage, not to a chat channel: Paperclip turns each open issue
labelled `incident` into a DevOps task (docs/synthetic-user-flow.md).

One open issue per check key. The key is a hidden body marker
`<!-- incident-key: <key> -->`, so an ALERT while an issue is open adds a
comment instead of a second issue, and RECOVERY comments then closes it.
Authentication is the workflow GITHUB_TOKEN (issues: write) only; no new
secret. Every failure raises IncidentError with fixed vocabulary (never a
token, remote body or URL) so callers fail closed: an alert that cannot be
written fails its step instead of passing silently.
"""

import http.client
import json
import os
import re
import urllib.error
import urllib.parse
import urllib.request
from datetime import datetime

API = "https://api.github.com"
LABEL = "incident"
KEY = re.compile(r"[a-z0-9][a-z0-9:._-]{0,79}")
MAX_RESPONSE_BYTES = 1 << 20
PAGE_SIZE = 100
# A bounded search: past this many open incident issues, fail closed rather
# than miss the keyed one and open a duplicate.
MAX_PAGES = 10
MAX_TITLE = 200
MAX_BODY = 6000
TIMEOUT_SECONDS = 10


class IncidentError(Exception):
    """A fixed-vocabulary sink failure (never carries the token or a URL)."""


class _NoRedirectHandler(urllib.request.HTTPRedirectHandler):
    """Turn every redirect into an HTTPError so the token never leaves api.github.com."""

    def redirect_request(self, request, response, code, message, headers, new_url):
        return None


def _reject_duplicate_keys(pairs):
    seen = set()
    for key, _value in pairs:
        if key in seen:
            raise ValueError("duplicate JSON object key")
        seen.add(key)
    return dict(pairs)


def marker(key):
    return f"<!-- incident-key: {key} -->"


def _context(env):
    token = env.get("GITHUB_TOKEN")
    repo = env.get("GITHUB_REPOSITORY")
    if not token or not repo or not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repo):
        raise IncidentError("incident sink context is missing (GITHUB_TOKEN/GITHUB_REPOSITORY)")
    return token, urllib.parse.quote(repo, safe="/")


def _request(method, path, token, body=None):
    data = json.dumps(body).encode() if body is not None else None
    request = urllib.request.Request(
        API + path, data=data, method=method,
        headers={"Accept": "application/vnd.github+json",
                 "Authorization": "Bearer " + token,
                 "X-GitHub-Api-Version": "2022-11-28",
                 "Content-Type": "application/json",
                 "User-Agent": "two-bot-next-incident-issue/1"})
    opener = urllib.request.build_opener(
        urllib.request.HTTPHandler(),
        urllib.request.HTTPSHandler(),
        urllib.request.HTTPErrorProcessor(),
        _NoRedirectHandler(),
    )
    try:
        with opener.open(request, timeout=TIMEOUT_SECONDS) as response:
            raw = response.read(MAX_RESPONSE_BYTES + 1)
    except urllib.error.HTTPError as error:
        if error.code in (301, 302, 303, 307, 308):
            raise IncidentError(f"incident issue {method} refused redirect with status {error.code}") from None
        raise IncidentError(f"incident issue {method} failed with status {error.code}") from None
    except (OSError, ValueError, http.client.HTTPException):
        raise IncidentError(f"incident issue {method} failed (transport)") from None
    if len(raw) > MAX_RESPONSE_BYTES:
        raise IncidentError(f"incident issue {method} response exceeds the size cap")
    try:
        return json.loads(raw, object_pairs_hook=_reject_duplicate_keys) if raw else None
    except (ValueError, RecursionError):
        raise IncidentError(f"incident issue {method} response is not valid JSON") from None


def _updated(issue):
    """The issue's last-activity time, or None when absent or unreadable."""
    try:
        value = datetime.fromisoformat(issue["updated_at"].replace("Z", "+00:00"))
    except (KeyError, AttributeError, TypeError, ValueError):
        return None
    return value if value.tzinfo is not None else None


def _open_keyed(env, key):
    """Every open `incident` issue carrying this key, lowest number first."""
    token, repo = _context(env)
    tag = marker(key)
    matches = []
    for page in range(1, MAX_PAGES + 1):
        query = urllib.parse.urlencode({"state": "open", "labels": LABEL,
                                        "per_page": PAGE_SIZE, "page": page})
        issues = _request("GET", f"/repos/{repo}/issues?{query}", token)
        if not isinstance(issues, list) or len(issues) > PAGE_SIZE or any(
                not isinstance(issue, dict) or type(issue.get("number")) is not int
                or issue["number"] <= 0
                or (issue.get("body") is not None and not isinstance(issue["body"], str))
                for issue in issues):
            raise IncidentError("incident issue list is malformed")
        matches += [issue for issue in issues
                    if "pull_request" not in issue and tag in (issue.get("body") or "")]
        if len(issues) < PAGE_SIZE:
            return sorted(matches, key=lambda issue: issue["number"])
    raise IncidentError("too many open incident issues to search")


def find_open(env, key):
    """Lowest-numbered open `incident` issue carrying this key, or None."""
    matches = _open_keyed(env, key)
    return matches[0]["number"] if matches else None


def _clip(text, limit):
    return text if len(text) <= limit else text[:limit - 3] + "..."


def open_incident(env, key, title, body):
    """ALERT: open one issue for `key`, or comment on the one already open.

    Returns ("opened" | "commented", issue number).
    """
    if not KEY.fullmatch(key or ""):
        raise IncidentError("refusing: invalid incident key")
    token, repo = _context(env)
    number = find_open(env, key)
    if number is not None:
        _request("POST", f"/repos/{repo}/issues/{number}/comments", token,
                 {"body": _clip("Still failing.\n\n" + body, MAX_BODY)})
        return "commented", number
    created = _request("POST", f"/repos/{repo}/issues", token, {
        "title": _clip(title, MAX_TITLE),
        "body": _clip(body, MAX_BODY - 100) + "\n\n" + marker(key) + "\n",
        "labels": [LABEL],
    })
    number = created.get("number") if isinstance(created, dict) else None
    if type(number) is not int or number <= 0:
        raise IncidentError("incident issue create returned no issue number")
    # Two runs can both find no issue and both create one. Each re-reads
    # after creating; whichever is not the lowest closes itself, so racers
    # converge on one open issue without a workflow-level lock.
    first = find_open(env, key)
    if first is not None and first < number:
        _request("POST", f"/repos/{repo}/issues/{number}/comments", token,
                 {"body": f"Duplicate of #{first}, opened by a concurrent run."})
        _request("PATCH", f"/repos/{repo}/issues/{number}", token,
                 {"state": "closed", "state_reason": "duplicate"})
        _request("POST", f"/repos/{repo}/issues/{first}/comments", token,
                 {"body": _clip("Still failing.\n\n" + body, MAX_BODY)})
        return "commented", first
    return "opened", number


def close_incident(env, key, body, quiet_since=None):
    """RECOVERY: comment on and close every open issue for `key`.

    Closing all of them also clears a duplicate that a race left open. With
    `quiet_since` (an aware datetime), an issue with activity after it, or
    no readable activity time, is held open instead: an alert after that
    moment means the check failed again since the evidence of health.
    Returns ("closed", lowest closed), ("held", lowest held) when every open
    issue was held, or ("none-open", None) when no issue is open (for
    example an agent already closed it): nothing is left to resolve.
    """
    if not KEY.fullmatch(key or ""):
        raise IncidentError("refusing: invalid incident key")
    token, repo = _context(env)
    issues = _open_keyed(env, key)
    if not issues:
        return "none-open", None
    closed, held = [], []
    for issue in issues:
        number = issue["number"]
        updated = _updated(issue)
        if quiet_since is not None and (updated is None or updated > quiet_since):
            held.append(number)
            continue
        _request("POST", f"/repos/{repo}/issues/{number}/comments", token,
                 {"body": _clip(body, MAX_BODY)})
        _request("PATCH", f"/repos/{repo}/issues/{number}", token,
                 {"state": "closed", "state_reason": "completed"})
        closed.append(number)
    return ("closed", closed[0]) if closed else ("held", held[0])


def run_link(env=None):
    env = os.environ if env is None else env
    server = env.get("GITHUB_SERVER_URL", "https://github.com")
    repo = env.get("GITHUB_REPOSITORY", "TogetherWeOwn/two-bot-next")
    run_id = env.get("RUN_ID") or env.get("GITHUB_RUN_ID", "unknown")
    return f"{server}/{repo}/actions/runs/{run_id}"
