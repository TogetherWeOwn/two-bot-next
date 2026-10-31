#!/usr/bin/env python3
"""File one `incident` GitHub issue for a failed synthetic probe (stdlib only).

The issue title names `synthetic-user-flow:<env>`; the body names the failing
checks, links the observability runbook, and carries the container build
revision plus the GitHub run link. Delivery goes through
scripts/incident_issue.py with the workflow GITHUB_TOKEN (issues: write):
ALERT opens the one issue for the env (or comments on the one already open)
and RECOVERY comments then closes it. Paperclip turns each open `incident`
issue into an agent task. Failures report fixed vocabulary only and fail
the step, so an alert is never silently dropped.

Usage (called only from the synthetic-user-flow workflow decide/post steps):
  GITHUB_TOKEN=<token> RUN_ID=<run> GIT_SHA=<sha> \\
      python3 scripts/synthetic_alert.py --env staging --evidence FILE
"""

import argparse
import json
import os
import re
import sys

import incident_issue
from synthetic_alert_history import HistoryError, previous_probe_outcomes, transition

RUNBOOK_URL = ("https://github.com/TogetherWeOwn/two-bot-next"
               "/blob/main/docs/runbook-observability.md")
MAX_CONTENT = 1900


class AlertError(Exception):
    """A fixed-vocabulary alert failure (never carries a token or remote data)."""


class DuplicateKeyError(ValueError):
    """A JSON object repeats a key; the message never carries key/value data."""


def _reject_duplicate_keys(pairs):
    """object_pairs_hook refusing repeats instead of last-wins.

    The json decoder calls this for every object at any depth, after
    escape decoding, so {"a": 1, "\\u0061": 2} counts as a repeat. The
    raised message is fixed vocabulary: it names no key, value or path.
    """
    seen = set()
    for key, _value in pairs:
        if key in seen:
            raise DuplicateKeyError("duplicate JSON object key")
        seen.add(key)
    return dict(pairs)


def load_evidence(path):
    try:
        with open(path, encoding="utf-8") as handle:
            value = json.load(handle, object_pairs_hook=_reject_duplicate_keys)
    except DuplicateKeyError:
        raise AlertError("evidence refused: duplicate JSON object key") from None
    except (OSError, ValueError, RecursionError):
        return None
    return value if isinstance(value, dict) else None


def printable(value):
    return value if isinstance(value, str) and value.isascii() and value.isprintable() else None


def summarize(evidence):
    """Short failing-check summary from the allowlisted evidence receipt."""
    if not isinstance(evidence, dict):
        return "probe failed with no evidence receipt"
    checks = evidence.get("checks")
    if not isinstance(checks, list) or not checks:
        return "probe failed"
    # Evidence-derived names pass through printable(), like the revision
    # fields below: a crafted check name with control characters is dropped
    # (fail-closed to "probe failed") instead of reaching the issue text.
    failed = [c for c in checks
              if isinstance(c, dict) and c.get("verdict") == "fail"
              and isinstance(c.get("check"), str)
              and printable(c["check"]) is not None]
    if not failed:
        return "probe failed"
    names = ", ".join(c["check"] for c in failed[:4])
    first_shape = failed[0].get("shape")
    detail = f": {first_shape}" if printable(first_shape) else ""
    text = f"{names} failed{detail}"
    return text if len(text) <= 300 else text[:297] + "..."


def build_content(env, evidence, git_sha, run_link, event="alert"):
    summary = summarize(evidence) if event == "alert" else "the probe is healthy again"
    revision = printable((evidence or {}).get("build_revision")) or "unknown"
    build_id = printable((evidence or {}).get("build_id")) or "unknown"
    sha = printable(git_sha) or "unknown"
    kind = "ALERT" if event == "alert" else "RECOVERY"
    content = (f"two-bot-next {kind} synthetic-user-flow:{env}: {summary}. "
               f"Runbook: {RUNBOOK_URL} | "
               f"build {revision} {build_id} (git {sha}) | logs {run_link}")
    return content if len(content) <= MAX_CONTENT else content[:MAX_CONTENT - 3] + "..."


def run_link():
    return incident_issue.run_link()


def incident_key(environment):
    return f"synthetic-user-flow:{environment}"


def post(env, environment, event, content):
    """Deliver one decided event to the `incident` issue sink.

    ALERT opens the env's issue (or comments on the open one); RECOVERY
    comments and closes it. Returns a short receipt such as "opened #12".
    """
    key = incident_key(environment)
    try:
        if event == "alert":
            body = (f"{content}\n\nNext: follow the runbook and fix the cause. "
                    "The first healthy scheduled probe comments and closes this issue.")
            action, number = incident_issue.open_incident(env, key, f"{key} is failing", body)
        else:
            action, number = incident_issue.close_incident(env, key, content)
    except incident_issue.IncidentError as error:
        raise AlertError(str(error)) from None
    return f"{action} #{number}" if number else action


def open_issue_without_incident(env, environment):
    """Whether an `incident` issue is open for a probe whose history is closed.

    A force_fail dispatch is not scheduled history, so its issue would never
    see a recovery transition; a healthy scheduled sample closes it instead.
    An unreadable issue list is a warning, never a red healthy run: the next
    healthy sample retries.
    """
    try:
        return incident_issue.find_open(env, incident_key(environment)) is not None
    except incident_issue.IncidentError as error:
        print(f"synthetic_alert: warning: {error}; open incident issue not reconciled",
              file=sys.stderr)
        return False


def _decide(env, environment):
    """(event, history_known) for the decide step; see decide_event."""
    current = env.get("PROBE_OUTCOME")
    if current not in ("success", "failure"):
        # No sample means no history read: report unknown so the JSON
        # receipt never claims incident history was verified.
        return None, False
    try:
        previous = previous_probe_outcomes(env, environment)
    except HistoryError as error:
        if current == "success":
            print(f"synthetic_alert: warning: {error}; healthy probe remains green",
                  file=sys.stderr)
            return None, False
        # Fail open on a real probe failure without history: alert now so a
        # missing incident is never silent. If the history error landed on
        # the first failure of an incident this duplicates the alert on the
        # next failure; that duplicate is intentional versus a lost incident.
        print(f"synthetic_alert: warning: {error}; sending failure alert without history",
              file=sys.stderr)
        return "alert", False
    force_fail = env.get("FORCE_FAIL") == "true"
    event = transition(current, previous, force_fail)
    if event is None and current == "success" and open_issue_without_incident(env, environment):
        event = "recovery"
    return event, True


def decide_event(env, environment):
    """Compute the alert transition without posting (decide step)."""
    return _decide(env, environment)[0]


def write_decision(environment, event, history_known, no_sample):
    """Bounded machine-readable decision, never a delivery receipt."""
    run_id = os.environ.get("GITHUB_RUN_ID") or os.environ.get("RUN_ID", "")
    if not re.fullmatch(r"[1-9][0-9]{0,18}", run_id):
        run_id = "unknown"
    # A missing probe sample reads no history, so it is unknown even if a
    # caller passes history_known true; its next_action stays none because
    # there is no incident history to inspect, only a missing sample.
    known = history_known and not no_sample
    print(json.dumps({
        "schema_version": 1,
        "check_id": f"synthetic-user-flow:{environment}",
        "run_id": run_id,
        "event": event,
        "history_known": known,
        "code": ("no_probe_sample" if no_sample else
                 "decision_ready" if known else HistoryError.code),
        "next_action": "none" if (known or no_sample) else HistoryError.next_action,
    }, separators=(",", ":")))


def write_event(event, json_mode=False):
    """Record the decide-step event for the post-step gate."""
    value = event or ""
    path = os.environ.get("GITHUB_OUTPUT")
    line = f"event={value}\n"
    if path:
        with open(path, "a", encoding="utf-8") as handle:
            handle.write(line)
    else:
        print(line, end="", file=sys.stderr if json_mode else sys.stdout)
    print(f"synthetic_alert: decided event={value or 'none'}", file=sys.stderr)


def post_event(env, environment, evidence_path, event):
    """Post a decided alert/recovery event without re-reading history."""
    if event not in ("alert", "recovery"):
        print("synthetic_alert: no alert transition", file=sys.stderr)
        return 0
    try:
        evidence = load_evidence(evidence_path)
    except AlertError as error:
        print(f"synthetic_alert: {error}", file=sys.stderr)
        return 1
    content = build_content(environment, evidence,
                            env.get("GIT_SHA") or env.get("GITHUB_SHA"),
                            run_link(), event)
    print(content)
    try:
        receipt = post(env, environment, event, content)
    except AlertError as error:
        print(f"synthetic_alert: {error}", file=sys.stderr)
        return 1
    print(f"synthetic_alert: delivered {event} for {environment}: {receipt}", file=sys.stderr)
    return 0


def sink_readiness(env):
    """Readiness label for the dry-run plan; never echoes the token."""
    if not env.get("GITHUB_TOKEN") or not env.get("GITHUB_REPOSITORY"):
        return "missing"
    return "ready"


def dry_run_plan(environment, evidence_path):
    """Print the delivery plan without writing any issue.

    Runs the same decide step as the live path (the read-only GitHub
    history read is included, fail-open as live) so the planned event
    matches what a live run would deliver, then reports the sink readiness
    before any write. A missing token or a missing probe sample is a plan
    line, never a failure: a preview is not a delivery. Refused evidence
    (for example duplicate JSON keys) fails closed with the same clean
    refusal as the live post path, never a traceback.
    """
    env = os.environ
    current = env.get("PROBE_OUTCOME")
    if current not in ("success", "failure"):
        event = None
    else:
        event = decide_event(env, environment)
    try:
        evidence = load_evidence(evidence_path)
    except AlertError as error:
        print(f"synthetic_alert: {error}", file=sys.stderr)
        return 1
    if event == "alert":
        summary = summarize(evidence)
    elif event == "recovery":
        summary = "the probe is healthy again"
    else:
        summary = "no alert transition"
    print("synthetic_alert dry-run plan (no mutations performed)")
    print(f"env: {environment}")
    print(f"evidence: {summary}")
    print(f"event: {event or 'none'}")
    print(f"sink: incident issue {incident_key(environment)} ({sink_readiness(env)})")
    if event in ("alert", "recovery"):
        content = build_content(environment, evidence,
                                env.get("GIT_SHA") or env.get("GITHUB_SHA"),
                                run_link(), event)
        print(f"content: {content}")
    else:
        print("content: (nothing would be sent)")
    return 0


def parse_args(argv):
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--env", required=True, choices=("staging", "prod"),
                        help="which synthetic failed")
    parser.add_argument("--evidence", required=True,
                        help="probe evidence receipt (missing reads as unknown build)")
    parser.add_argument("--decide-only", action="store_true",
                        help="only compute the transition and write event to GITHUB_OUTPUT")
    parser.add_argument("--post-only", action="store_true",
                        help="only post ALERT_EVENT without reading workflow history")
    parser.add_argument("--dry-run", action="store_true",
                        help="print the delivery plan without posting anything")
    parser.add_argument("--json", action="store_true",
                        help="emit a bounded JSON decision (requires --decide-only)")
    return parser.parse_args(argv)


def main(argv=None):
    args = parse_args(argv)
    if args.decide_only and args.post_only:
        print("synthetic_alert: --decide-only and --post-only are exclusive", file=sys.stderr)
        return 2
    if args.json and (not args.decide_only or args.dry_run):
        print("synthetic_alert: --json requires --decide-only without --dry-run", file=sys.stderr)
        return 2
    if args.dry_run:
        return dry_run_plan(args.env, args.evidence)
    if args.decide_only:
        # Decide step: compute the transition, never post.
        # A missing/skipped probe is not a sample: write the empty event that
        # keeps the post step skipped, then fail the step so a zero-sample
        # run never reads as a healthy pass.
        event, history_known = _decide(os.environ, args.env)
        no_sample = os.environ.get("PROBE_OUTCOME") not in ("success", "failure")
        if event is None and no_sample:
            print("synthetic_alert: no probe sample; failing closed", file=sys.stderr)
        write_event(event, json_mode=args.json)
        if args.json:
            write_decision(args.env, event, history_known, no_sample)
        if no_sample:
            return 1
        if not history_known and os.environ.get("PROBE_OUTCOME") == "success":
            # Record "history unknown" explicitly in this run's step
            # conclusions: a failed decide step keeps later reconstruction
            # from reading this healthy sample's skipped post as a
            # known-closed checkpoint. The empty event above already keeps
            # the post step skipped. On a failed probe the step must stay
            # green so the fail-open alert still posts.
            print("synthetic_alert: history unknown on a healthy probe; "
                  "failing the decide step to record it", file=sys.stderr)
            return 1
        return 0
    if args.post_only:
        # Post step: deliver the decided event without re-reading history, so
        # a no-op is a skipped step (never success) and only success delivery
        # closes an incident in history.
        event = os.environ.get("ALERT_EVENT") or ""
        return post_event(os.environ, args.env, args.evidence, event)
    current = os.environ.get("PROBE_OUTCOME")
    # A missing/skipped probe (for example, an unset optional target binding)
    # is not a sample and must not create an alert or recovery transition;
    # fail closed so a zero-sample run never reads as a healthy pass.
    if current not in ("success", "failure"):
        print("synthetic_alert: no probe sample; failing closed", file=sys.stderr)
        return 1
    event = decide_event(os.environ, args.env)
    if event is None:
        print("synthetic_alert: no alert transition", file=sys.stderr)
        return 0
    return post_event(os.environ, args.env, args.evidence, event)


if __name__ == "__main__":
    sys.exit(main())
