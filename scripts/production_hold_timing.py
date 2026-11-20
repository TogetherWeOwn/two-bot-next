"""Read-only verified timing for pending production work; not a reporter."""

from datetime import datetime, timezone

import incident_issue
from production_hold_receipts import moment, sha


def _clock(value):
    try:
        if isinstance(value, datetime) and value.tzinfo is not None and value.utcoffset() is not None:
            return value.astimezone(timezone.utc)
    except (OverflowError, ValueError):
        pass
    raise incident_issue.IncidentError("production hold clock is unreadable")


def held_since(history, pending, completed_at, now):
    """Return the verified post-baseline interval, or None for no pending SHA.

    The history object supplies the existing bounded pages interface and repo.
    Git commit dates and observation time are not evidence of arrival.
    """
    now, completed_at = _clock(now), _clock(completed_at)
    if completed_at > now:
        raise incident_issue.IncidentError("production hold clock is in the future")
    if pending is None:
        return None
    sha(pending)
    matches = []
    for row in history.pages(f"commits/{pending}/pulls"):
        if not isinstance(row, dict):
            raise incident_issue.IncidentError("production hold merge receipt is malformed")
        base = row.get("base")
        if not isinstance(base, dict) or not isinstance(base.get("repo"), dict):
            raise incident_issue.IncidentError("production hold merge receipt is malformed")
        revision, branch, repo = row.get("merge_commit_sha"), base.get("ref"), base["repo"].get("full_name")
        sha(revision)
        if not all(isinstance(value, str) for value in (branch, repo)):
            raise incident_issue.IncidentError("production hold merge receipt is malformed")
        if revision == pending and branch == "main" and repo == history.repo:
            try:
                matches.append(moment(row.get("merged_at")))
            except (OverflowError, ValueError):
                raise incident_issue.IncidentError("production hold timestamp is unreadable") from None
    if len(matches) != 1:
        raise incident_issue.IncidentError("production hold merge receipt is not unique")
    if matches[0] > now:
        raise incident_issue.IncidentError("production hold clock is in the future")
    return max(completed_at, matches[0])
