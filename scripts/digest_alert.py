#!/usr/bin/env python3
"""Post one Discord ops-channel alert for a missing daily digest (stdlib only).

Fires when production changed but no digest tag was made — the silent-failure
mode that lost tags when the release job skipped with no alert. Reuses the
existing Discord alert line (`synthetic_alert.post`: Discord-only destination
boundary, no redirects, fixed-vocabulary failures).

Usage (called only from the daily-digest workflow alert step):
  SYNTHETIC_ALERT_WEBHOOK_URL=<webhook> RUN_ID=<run> \\
      python3 scripts/digest_alert.py --live-sha <sha> --prev-tag <tag>
"""

import argparse
import importlib
import os
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
# Dynamic sibling import (same pattern as test_synthetic_alert.py): a plain
# `from x import y` after sys.path manipulation would need a lint waiver,
# which the floor guard forbids on new lines.
_synthetic = importlib.import_module("synthetic_alert")
MAX_CONTENT = _synthetic.MAX_CONTENT
RUNBOOK_URL = _synthetic.RUNBOOK_URL
post = _synthetic.post
run_link = _synthetic.run_link

SHA = re.compile(r"[0-9a-f]{40}")
TAG = re.compile(r"v[0-9]{4}\.[0-9]{2}\.[0-9]{2}")


def printable(value):
    return value if isinstance(value, str) and value.isascii() and value.isprintable() else None


def build_content(live_sha, prev_tag, link):
    sha7 = live_sha[:7] if isinstance(live_sha, str) and SHA.fullmatch(live_sha) else "unknown"
    prev = printable(prev_tag) if prev_tag else "none"
    if prev is None:
        prev = "unknown"
    content = (f"two-bot-next ALERT daily-digest: production changed ({sha7}) "
               f"but no digest tag was made (prev {prev}). "
               f"Runbook: {RUNBOOK_URL} | logs {link}")
    return content if len(content) <= MAX_CONTENT else content[:MAX_CONTENT - 3] + "..."


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--live-sha", required=True, help="live production build revision")
    parser.add_argument("--prev-tag", default="", help="previous digest tag (empty when none)")
    args = parser.parse_args(argv)
    webhook_url = os.environ.get("SYNTHETIC_ALERT_WEBHOOK_URL")
    if not webhook_url:
        print("digest_alert: SYNTHETIC_ALERT_WEBHOOK_URL is not set", file=sys.stderr)
        return 1
    if not SHA.fullmatch(args.live_sha or ""):
        print("digest_alert: refusing: live SHA is not 40 hex", file=sys.stderr)
        return 1
    if args.prev_tag and not TAG.fullmatch(args.prev_tag):
        print("digest_alert: refusing: prev tag is not vYYYY.MM.DD", file=sys.stderr)
        return 1
    content = build_content(args.live_sha, args.prev_tag, run_link())
    print(content)
    try:
        post(webhook_url, content)
    except Exception as error:
        print(f"digest_alert: {error}", file=sys.stderr)
        return 1
    print("digest_alert: delivered missing-digest alert", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
