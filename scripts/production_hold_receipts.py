"""Inert production-hold receipt helpers extracted from PR #2023.

Source: 8ad4550d60210e5022ed1b4a782508d1338936a3,
scripts/production_hold_incident.py. Not wired to a reporter.
Unlike the source, verified_content rejects excessive JSON nesting.
Observation v2 binds metadata and rendered content with an unkeyed digest:
this detects inconsistent edits, not authorship or coherent rewrites. Legacy,
unknown and inconsistent observations remain fenced, not reissued or trusted.
Recovery receipts retain their separate body-only contract. No reporter is wired.
"""

from datetime import datetime, timezone
import hashlib
import json
import re

import incident_issue


KEY = "production-held"


# The guard/recheck skip reason for the non-expiring automatic-promotion
# disable (PRODUCTION_AUTO_PROMOTE_DISABLED, exactly "true"). It is an
# intentional hold with no expiry to outlast: only an operator re-enable or
# a manual promotion ends it. The reporter still treats it as held
# production, but classifies it explicitly instead of a generic reason.
AUTO_PROMOTE_DISABLED = "auto-promote-disabled"


def moment(raw):
    try:
        value = datetime.fromisoformat(raw.replace("Z", "+00:00"))
        if value.tzinfo is not None:
            return value.astimezone(timezone.utc)
    except (AttributeError, TypeError, ValueError, OverflowError):
        pass
    raise incident_issue.IncidentError("production hold timestamp is unreadable")


def sha(value):
    if not isinstance(value, str) or not re.fullmatch(r"[0-9a-f]{40}", value):
        raise incident_issue.IncidentError("production hold revision is unreadable")
    return value


OBSERVATION = re.compile(r"<!-- production-hold-observation: (\{[^\n]*\}) -->")


RECOVERY = re.compile(r"<!-- production-hold-recovery: (\{[^\n]*\}) -->")


GENERATION_FIELDS = ("last_promoted_sha", "last_promoted_at", "promotion_run_id",
                     "promotion_attempt", "promotion_mode")


def content_digest(text):
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


OBSERVATION_FIELDS = (*GENERATION_FIELDS, "main_sha", "observed_at", "held_since")


def observation_digest(content, value):
    return content_digest(json.dumps([content, value], sort_keys=True, separators=(",", ":")))


def observation(report):
    value = {key: report[key] for key in OBSERVATION_FIELDS}
    value["version"] = 2
    content = body(report)
    value["content_sha256"] = content_digest(content)
    value["envelope_sha256"] = observation_digest(content, value)
    return "<!-- production-hold-observation: " + json.dumps(value, sort_keys=True) + " -->"


def verified_content(text, pattern):
    # This checksum detects edits, not hostile authorship. Only the exact sink
    # envelopes are removable; unknown content before/after a marker stays fenced.
    matches = list(pattern.finditer(text or ""))
    if len(matches) != 1:
        return None
    match = matches[0]
    content, suffix = text[:match.start()], text[match.end():]
    if content.startswith("Still failing.\n\n") and pattern is OBSERVATION:
        content = content[len("Still failing.\n\n"):]
        if suffix:
            return None
    elif suffix and (pattern is not OBSERVATION
                     or suffix != "\n\n" + incident_issue.marker(KEY) + "\n"):
        return None
    if not content.endswith("\n"):
        return None
    try:
        value = json.loads(match[1], object_pairs_hook=incident_issue._reject_duplicate_keys)
        content = content[:-1]
        if not isinstance(value, dict) or value.get("content_sha256") != content_digest(content):
            return None
        if pattern is OBSERVATION:
            # Legacy body-only checksums cannot establish metadata consistency.
            if (set(value) != set(OBSERVATION_FIELDS) | {"version", "content_sha256", "envelope_sha256"}
                    or type(value["version"]) is not int or value["version"] != 2):
                return None
            metadata = {key: val for key, val in value.items() if key != "envelope_sha256"}
            if value["envelope_sha256"] != observation_digest(content, metadata):
                return None
            prefix = (f"- Held since: {value['held_since']}\n"
                      f"- Last promoted SHA: `{value['last_promoted_sha']}`\n"
                      f"- Last promotion completed: {value['last_promoted_at']}\n"
                      f"- Current main SHA: `{value['main_sha']}`\n")
            if not content.startswith(prefix):
                return None
        return value
    except (TypeError, ValueError, RecursionError):
        return None


def recovery_body(report, repo):
    text = ("Production promotion recovered.\n\n" + body(report)
            + f"- Successful deployment run: https://github.com/"
              f"{repo}/actions/runs/{report['promotion_run_id']}"
              f"/attempts/{report['promotion_attempt']}\n")
    value = {key: report[key] for key in GENERATION_FIELDS}
    value["content_sha256"] = content_digest(text)
    return text + "\n<!-- production-hold-recovery: " + json.dumps(value, sort_keys=True) + " -->"


def recovery_receipt(text, report):
    value = verified_content(text, RECOVERY)
    return (value is not None and report["promotion_mode"] == "deploy"
            and all(value.get(key) == report[key] for key in GENERATION_FIELDS))


def stale_observation(text, report):
    value = verified_content(text, OBSERVATION)
    if value is None:
        return False
    try:
        sha(value["last_promoted_sha"])
        sha(value["main_sha"])
        if (not re.fullmatch(r"[1-9][0-9]*", value["promotion_run_id"])
                or type(value["promotion_attempt"]) is not int or value["promotion_attempt"] <= 0
                or value["promotion_mode"] not in ("deploy", "rollback")):
            return False
        baseline = moment(value["last_promoted_at"])
        held = moment(value["held_since"])
        observed = moment(value["observed_at"])
        return (baseline <= held <= observed
                and baseline < moment(report["last_promoted_at"])
                and held <= moment(report["last_promoted_at"]))
    except (incident_issue.IncidentError, KeyError, TypeError, ValueError):
        return False


def stale_activity(history, issue, report):
    # Reporter writes serialize on the issue-only lock, not with operators.
    # A receipt proves comment delivery, never a failed close: a successful
    # close followed by reopen must remain held against that same generation.
    updated = incident_issue._updated(issue)
    if updated is None:
        return False
    cutoff = moment(report["last_promoted_at"])
    quiet_lifecycle = True
    seen = set()
    for event in history.pages(f"issues/{issue['number']}/events"):
        event_id = event.get("id")
        if (type(event_id) is not int or event_id <= 0 or event_id in seen
                or not isinstance(event.get("event"), str) or not event["event"].strip()):
            raise incident_issue.IncidentError("production hold lifecycle event is malformed")
        seen.add(event_id)
        if moment(event.get("created_at")) >= cutoff:
            # No newer lifecycle kind is exempt, including unknown kinds and
            # equal-cutoff reopens. Exhaust history even after finding a veto.
            quiet_lifecycle = False
    if not quiet_lifecycle:
        return False
    # Preserve the shared sink's original timestamp fence. Only unchanged
    # older-generation content can justify activity newer than the promotion.
    if updated <= cutoff:
        return True
    if not stale_observation(issue.get("body"), report):
        return False
    for comment in history.pages(f"issues/{issue['number']}/comments"):
        updated = incident_issue._updated(comment)
        if updated is None or (updated > cutoff
                               and not stale_observation(comment.get("body"), report)
                               and not recovery_receipt(comment.get("body"), report)):
            return False
    return True


def body(report):
    text = (f"- Held since: {report.get('held_since', 'recovered')}\n"
            f"- Last promoted SHA: `{report['last_promoted_sha']}`\n"
            f"- Last promotion completed: {report['last_promoted_at']}\n"
            f"- Current main SHA: `{report['main_sha']}`\n"
            f"- Skip/failure reason: {report['reason']}\n")
    if report["reason"] == AUTO_PROMOTE_DISABLED:
        # Intentional and non-expiring: name the class so nobody waits for
        # a timed hold to lapse. Only an operator re-enable or a manual
        # promotion ends it, mirroring the guard/recheck semantics.
        text += ("- Hold class: intentional non-expiring auto-promote disable; "
                 "an operator re-enable or manual promotion ends it\n")
    return text + f"- Promotion run: {report['run_url']}\n"
