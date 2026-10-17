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


def load_evidence(path):
    try:
        with open(path, encoding="utf-8") as handle:
            value = json.load(handle)
    except (OSError, ValueError):
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
    failed = [c for c in checks
              if isinstance(c, dict) and c.get("verdict") == "fail"
              and isinstance(c.get("check"), str)]
    if not failed:
        return "probe failed"
    names = ", ".join(c["check"] for c in failed[:4])
    first_shape = failed[0].get("shape")
    detail = f": {first_shape}" if isinstance(first_shape, str) and first_shape else ""
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
    try:
        with urllib.request.urlopen(request, timeout=TIMEOUT_SECONDS) as response:
            status = response.status
            response.read(1 << 10)
    except urllib.error.HTTPError as error:
        raise AlertError(f"alert delivery failed with status {error.code}")
    except (OSError, ValueError) as error:
        raise AlertError(f"alert delivery failed ({error.__class__.__name__})")
    if not 200 <= status < 300:
        raise AlertError(f"alert delivery failed with status {status}")


def decide_event(env, environment):
    """Compute the alert transition without posting (decide step)."""
    current = env.get("PROBE_OUTCOME")
    if current not in ("success", "failure"):
        return None
    try:
        previous = previous_probe_outcomes(env, environment)
    except HistoryError as error:
        if current == "success":
            print(f"synthetic_alert: warning: {error}; healthy probe remains green",
                  file=sys.stderr)
            return None
        # Fail open on a real probe failure without history: alert now so a
        # missing incident is never silent. If the history error landed on
        # the first failure of an incident this duplicates the alert on the
        # next failure; that duplicate is intentional versus a lost incident.
        print(f"synthetic_alert: warning: {error}; sending failure alert without history",
              file=sys.stderr)
        return "alert"
    force_fail = env.get("FORCE_FAIL") == "true"
    return transition(current, previous, force_fail)


def write_event(event):
    """Record the decide-step event for the post-step gate."""
    value = event or ""
    path = os.environ.get("GITHUB_OUTPUT")
    line = f"event={value}\n"
    if path:
        with open(path, "a", encoding="utf-8") as handle:
            handle.write(line)
    else:
        print(line, end="")
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
    evidence = load_evidence(evidence_path)
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
    return parser.parse_args(argv)


def main(argv=None):
    args = parse_args(argv)
    if args.decide_only and args.post_only:
        print("synthetic_alert: --decide-only and --post-only are exclusive", file=sys.stderr)
        return 2
    if args.decide_only:
        # Decide step: compute the transition, never post, never need webhook.
        # A missing/skipped probe is not a sample and writes an empty event.
        event = decide_event(os.environ, args.env)
        if event is None and os.environ.get("PROBE_OUTCOME") not in ("success", "failure"):
            print("synthetic_alert: no probe sample; no alert transition", file=sys.stderr)
        write_event(event)
        return 0
    if args.post_only:
        # Post step: deliver the decided event without re-reading history, so
        # a no-op is a skipped step (never success) and only success delivery
        # closes an incident in history.
        event = os.environ.get("ALERT_EVENT") or ""
        return post_event(os.environ, args.env, args.evidence, event)
    current = os.environ.get("PROBE_OUTCOME")
    # A missing/skipped probe (for example, an unset optional target binding)
    # is not a sample and must not create an alert or recovery transition.
    if current not in ("success", "failure"):
        print("synthetic_alert: no probe sample; no alert transition", file=sys.stderr)
        return 0
    event = decide_event(os.environ, args.env)
    if event is None:
        print("synthetic_alert: no alert transition", file=sys.stderr)
        return 0
    return post_event(os.environ, args.env, args.evidence, event)


if __name__ == "__main__":
    sys.exit(main())
