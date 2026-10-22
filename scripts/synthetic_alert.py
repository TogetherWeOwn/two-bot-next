#!/usr/bin/env python3
"""Post one Discord ops-channel alert for a failed synthetic probe (stdlib only).

Reuses the Worker alert pattern (`wrangler/src/index.ts` postWebhookText):
the message starts `two-bot-next ALERT synthetic-user-flow:<env>`, names the
failing checks, links the observability runbook, and carries the container
build revision plus the GitHub run link. The destination boundary is
Discord-only: anything but a discord.com webhook URL refuses before any
request. The webhook URL arrives only via $SYNTHETIC_ALERT_WEBHOOK_URL and
is never printed, logged, or echoed; failures report the HTTP status only.

Usage (called only from the synthetic-user-flow workflow alert step):
  SYNTHETIC_ALERT_WEBHOOK_URL=<webhook> RUN_ID=<run> GIT_SHA=<sha> \\
      python3 scripts/synthetic_alert.py --env staging --evidence FILE
"""

import argparse
import json
import os
import re
import sys
import urllib.error
import urllib.request

from synthetic_alert_history import HistoryError, previous_probe_outcomes, transition

RUNBOOK_URL = ("https://github.com/TogetherWeOwn/two-bot-next"
               "/blob/main/docs/runbook-observability.md")
# Same Discord-only destination boundary as wrangler/src/index.ts.
WEBHOOK = re.compile(r"https://discord\.com(?::443)?/api/webhooks/[0-9]+/[A-Za-z0-9_-]+")
TIMEOUT_SECONDS = 6
MAX_CONTENT = 1900


class AlertError(Exception):
    """A fixed-vocabulary alert failure (never carries the webhook URL)."""


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


class _NoRedirectHandler(urllib.request.HTTPRedirectHandler):
    """Turn every redirect into an HTTPError without contacting its target."""

    def redirect_request(self, request, response, code, message, headers, new_url):
        return None


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
    # (fail-closed to "probe failed") instead of reaching Discord content.
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
    server = os.environ.get("GITHUB_SERVER_URL", "https://github.com")
    repo = os.environ.get("GITHUB_REPOSITORY", "TogetherWeOwn/two-bot-next")
    run_id = os.environ.get("RUN_ID") or os.environ.get("GITHUB_RUN_ID", "unknown")
    return f"{server}/{repo}/actions/runs/{run_id}"


def post(webhook_url, content):
    if not WEBHOOK.fullmatch(webhook_url or ""):
        raise AlertError("refusing: not a Discord webhook URL")
    payload = json.dumps({"content": content,
                          "allowed_mentions": {"parse": [], "replied_user": False}}).encode()
    request = urllib.request.Request(
        webhook_url, data=payload, method="POST",
        headers={"Content-Type": "application/json", "User-Agent": "two-bot-next-synthetic-alert/1"})
    # urllib's default redirect handler can turn a redirected POST into a GET.
    # Reject redirects at the opener so only the allowlisted URL is contacted.
    opener = urllib.request.build_opener(
        urllib.request.HTTPHandler(),
        urllib.request.HTTPSHandler(),
        urllib.request.HTTPErrorProcessor(),
        _NoRedirectHandler(),
    )
    try:
        with opener.open(request, timeout=TIMEOUT_SECONDS) as response:
            status = response.status
            response.read(1 << 10)
    except urllib.error.HTTPError as error:
        if error.code in (301, 302, 303, 307, 308):
            raise AlertError(f"alert delivery refused redirect with status {error.code}")
        raise AlertError(f"alert delivery failed with status {error.code}")
    except (OSError, ValueError):
        raise AlertError("alert delivery failed (transport)")
    if not 200 <= status < 300:
        raise AlertError(f"alert delivery failed with status {status}")


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
    return transition(current, previous, force_fail), True


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
    webhook_url = env.get("SYNTHETIC_ALERT_WEBHOOK_URL")
    if not webhook_url:
        print("synthetic_alert: SYNTHETIC_ALERT_WEBHOOK_URL is not set", file=sys.stderr)
        return 1
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
        post(webhook_url, content)
    except AlertError as error:
        print(f"synthetic_alert: {error}", file=sys.stderr)
        return 1
    print(f"synthetic_alert: delivered {event} for {environment}", file=sys.stderr)
    return 0


def webhook_validity(webhook_url):
    """Validity label for the dry-run plan; never echoes the URL itself."""
    if not webhook_url:
        return "missing"
    return "valid" if WEBHOOK.fullmatch(webhook_url) else "invalid"


def dry_run_plan(environment, evidence_path):
    """Print the delivery plan without posting or contacting Discord.

    Runs the same decide step as the live path (the read-only GitHub
    history read is included, fail-open as live) so the planned event
    matches what a live run would deliver, then reports the webhook shape
    before any send. A missing webhook or a missing probe sample is a plan
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
    print(f"webhook: {webhook_validity(env.get('SYNTHETIC_ALERT_WEBHOOK_URL'))}")
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
        # Decide step: compute the transition, never post, never need webhook.
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
